# Proposals for llm.apache.org

llmao is an Apache project, [apache/tooling-llmao](https://github.com/apache/tooling-llmao),
and what follows is meant to go to it as pull requests, as
[its guide to pointing a harness at the gateway](https://github.com/apache/tooling-llmao/blob/main/docs/llmao-harnesses.md)
already did. Each proposal starts from what goes wrong without it, and from what the
Foundation gets, not from a setting: the settings are the easy part, and the motivation
is what a reviewer weighs. Where an open standard covers the need, the proposal names
the standard, so the gateway's surface stays llmao's own and LiteLLM stays an
implementation behind it.

Written 11 October 2026, from building this reference implementation against the gateway
and from llmao at `266122c`. What the gateway answered that day, with a PAT:

| Route | Answer |
|---|---|
| `/v1/models` | `gemma4-26b`, `qwen3-8b`, `qwen3.8-27b`; `qwen3-8b` is listed but often answers that it does not exist |
| `/v1beta/models/<model>:generateContent` | 200: the Gemini route is deployed, and Gemini CLI and Antigravity run on it |
| `/v1beta/models` | 404: the Gemini-shaped model list is not routed |
| `/v1/mcp/server` | 200 with an empty list: the proxy's MCP registry is reachable, with no servers on it yet |

Each proposal ends with potential implementation details, naming the proxy's own
setting where one exists. The setting names are from LiteLLM's documentation and should
be checked against the version the gateway runs, which llmao reads from the proxy's
readiness endpoint.

What llmao already has, and these proposals build on: Apache identity on every key,
keys per project, user and purpose, budgets per project, `drop_params`, and a health
roll-up per model on its own pages.

## 1. One trace from a task to a model's answer

**Why.** When an agent's run goes wrong, or costs more than it should, the question is
always which task, which session, which model call, which tool call. Today the answer
stops at the edge of each system: our launcher traces a session up to the request it
sends, the gateway records spend per key, and nothing joins the two. Cost per task,
the audit of what an agent decided, and debugging a failed run across our wrapper, the
gateway and a tool server all need one trace. For a fleet of agents working around the
clock on any task they can claim, this is how anyone finds out what happened.

**Standard.** W3C Trace Context (`traceparent`), W3C Baggage for the session and task,
and OpenTelemetry's semantic conventions for generative AI for the attributes: model,
tokens, finish reason, `gen_ai.conversation.id`.

**Proposal.** The gateway continues a trace it is handed and exports its own spans under
its own service name, `llm.apache.org`, with a request, a model call and, once tool
servers are on it, a tool call. Its collector keeps the proxy's internal spans out, so
what a caller sees is the gateway's contract. Each response carries the gateway's request
id in a header, so a caller can join the two by id even without a shared trace backend. A
shared backend where a PAT's owner can query their own traces can come later.

**Ours.** The launcher's egress sends `traceparent` and `baggage` on every request and
records the gateway's request id on its span.

**Potential implementation details.**

- LiteLLM's OpenTelemetry callback, `litellm_settings: callbacks: ["otel"]`, configured
  by `OTEL_EXPORTER`, `OTEL_ENDPOINT` and `OTEL_HEADERS`, with `OTEL_SERVICE_NAME` set to
  `llm.apache.org`. It continues an incoming `traceparent`, so the gateway's spans join
  the caller's trace without code in llmao.
- An OpenTelemetry Collector between the proxy and the backend, with a `filter` or
  `transform` processor that drops or renames the proxy's internal spans and strips
  prompt and response attributes, so what leaves is llmao's contract.
- The request id the proxy already returns, `x-litellm-call-id`, documented as llmao's
  request id; llmao can rename it at its vhost if the name should stay hidden.

## 2. Sessions as a caller the gateway can tell apart

**Why.** A key is per project, user and purpose. A fleet of agents runs on one
automation key, and to the gateway every session on it looks the same: one session cannot
be limited, revoked or billed apart from the rest, and a runaway agent can spend the
project's budget before anyone notices. The same gap blocks a hosted memory server, which
must know which session is asking, since every read is checked against what that session
has already read and the ceiling of where its model calls go.

**Standard.** Baggage for a session id now; OAuth 2.0 Token Exchange (RFC 8693) for a
session-scoped token later.

**Proposal.** In two steps.

1. A session id in baggage, recorded on the spend row and the span, scoped to the key
   that sent it, so a guessed id can only confuse its owner's own sessions. Limits per
   key beside the budget per project, so an automation key has a ceiling of its own.
2. A session token: the caller trades its PAT for a short-lived token bound to one
   session, which can be revoked without rotating the PAT, and which a hosted tool or
   memory server can trust. This is what a hosted memory server waits for.

**Ours.** The egress already holds the PAT so that a harness never sees it; it would set
the session id, strip any the harness sends, and later do the exchange.

**Potential implementation details.**

- The session id rides on the proxy's own fields: `litellm_session_id` in the request,
  which its spend logs and UI group by, and `tags`, or the `x-litellm-tags` header, for
  the task. A small pre-call hook in llmao maps W3C `baggage` onto them, so callers
  speak the standard and never the proxy's names.
- Limits per key are parameters of the proxy's key API that llmao already calls to
  issue PATs: `max_budget` with `budget_duration`, and `tpm_limit`, `rpm_limit` and
  `max_parallel_requests`. Automation keys would get them by default.
- The session token is llmao code over the same key API: on a token exchange, llmao
  checks the PAT and mints a virtual key with a short `duration`, the PAT's team, the
  session in its `metadata`, and its own limits; revoking it is a key delete. No new
  store, and the proxy enforces it like any key.

## 3. A model list that tells the truth

**Why.** A harness discovers models from the list and fails on the first call to one
that does not answer. `qwen3-8b` has been listed and often unanswering since at least 5 October, and every
caller pays a failed run to find out. Drift detection, which watches whether the model
behind a name has changed, cannot tell a deliberate release from a silent swap while
only aliases are listed.

**Proposal.** The API's model list follows the health roll-up llmao already shows on its
pages. Each alias also names the version it points at, a versioned name or a field, so a
change of version is visible. The Gemini-shaped list at `/v1beta/models` is routed like
the rest. Logprobs pass through where vLLM returns them: they are the most sensitive
drift signal there is, and nothing else shows a model changing before its answers do.

**Potential implementation details.**

- llmao's model status already decides "in service"; the same loop can remove a
  deployment that fails its probes from the proxy, with `/model/delete`, and push it
  back with `/model/new` when it recovers, which llmao already uses. The proxy's
  `general_settings: background_health_checks` with `health_check_interval`, and the
  router's `allowed_fails` and `cooldown_time`, keep a failing deployment out of
  routing in between, but not out of the list.
- Versions in the `model_info` llmao sends with `/model/new`, such as `version` and
  `base_model`, which the proxy serves on `/v1/model/info`; or versioned model names with
  the alias kept as a second name.
- `logprobs` and `top_logprobs` are OpenAI parameters the proxy passes to `hosted_vllm`
  and vLLM honours; the check is that `drop_params` does not drop them for these
  deployments.
- The Gemini-shaped list is a small route in llmao or its vhost that answers
  `GET /v1beta/models` from `/v1/models`, in Gemini's shape.

## 4. Compatibility for harnesses, fixed once in the gateway

**Why.** The harnesses are written for their vendors' APIs, and four differences break
them on the gateway's models. Our egress rewrites each one, as llmao's harness guide
records, and so does anyone else who gets it working, or they give up. Fixed in the
gateway, every harness works by hand, and every Apache contributor's first hour on it is
spent working rather than debugging.

| Difference | Harness | What breaks |
|---|---|---|
| a system message after the conversation | Claude Code | Qwen's chat template refuses the request |
| a Gemini tool result under the model's role | Gemini CLI, Antigravity | the proxy's Gemini translation drops it, and the model answers without its tool's result |
| an output limit of 128k tokens | Muse Code | a 131k-context model refuses once the prompt is counted |
| a reasoning effort of `high` | Claude Code | vLLM takes low, medium and xhigh; the model supports the parameter, so `drop_params` passes it on and the value is refused |

**Proposal.** The gateway normalises each, in a pre-call hook: fold the system message,
move the tool result to the user's role, cap output at the model's room, map effort to
the nearest level the model takes. Each is a few lines with a test, and the upstream fix
where there is one: the Gemini translation's issue was closed by a stale bot, unfixed.

**Potential implementation details.**

- One `CustomLogger` in llmao with an `async_pre_call_hook`, registered under
  `litellm_settings: callbacks`, that rewrites the request before it is routed: fold a
  late system message into the first, move a Gemini tool result to the user's role, clamp
  `max_tokens` to the model's room, and map `reasoning_effort` to the nearest level the
  model takes. Each is a few lines with a test.
- `litellm_settings: modify_params: true` already fixes some message shapes for some
  providers; whether it covers these four on `hosted_vllm` is worth one test before the
  hook is written.
- The Gemini translation bug is also worth an upstream fix, since the hook only works
  around it.

## 5. Tool servers: what the registry needs to hold

**Why.** The proxy can manage MCP servers itself, so `llm.apache.org` can list the tool
servers a key may reach, each running on its own host. Two things from building this make that more pressing.

The vendors' own tools do not cross the gateway. Codex's `web_search` runs inside OpenAI's
API and Claude Code's web search inside Anthropic's. On the gateway's open models they
silently do nothing: in the decision contexts our egress recorded up to 11 October, Codex
offered `web_search` on all 56 requests to the gateway and not one search came back. A
search server and a fetch server in the registry close that gap for every harness at once.

And a tool server is an injection surface. A tool's description is read by the model as
instructions, and what a tool returns, a mail thread or a web page, is written by anyone.

**Proposal.** For each server in the registry:

- **Grants per key and per tool,** private by default, which a caller
  can narrow further per session.
- **Pinned tool definitions:** a digest of the tool list and descriptions, versioned, so
  a changed description is visible before a model reads it.
- **A label on what a tool returns,** a classification such as public, internal or
  restricted, so a caller raises the session's label on the result and its ceiling holds.
- **Acting as the person:** a tool that changes something acts as the key's owner with
  their scopes, never a shared account, and declares which of its tools are irreversible.
- **The same trace,** session and limits as model calls, from proposals 1 and 2.

**Ours.** The egress gets a route per registered server, `/mcp/<name>`, with the PAT
injected so the harness never holds it. A tool call through it is then observed by the
egress, logged with its policy decision, and labelled, where today a harness's tool calls
reach the record only through its own transcript, a self-report. What a tool returns is
data, never instructions: the memory server's write scan applies before any of it is
written to the record.

**Potential implementation details.**

- Servers registered in the proxy's MCP gateway, in its `mcp_servers` configuration or
  through its API, each pointing at its own host.
- Grants through the proxy's object permissions on keys and teams, which name the MCP
  servers a key may reach; llmao sets them as it issues keys, private by default. Where
  the deployed version cannot narrow to single tools, llmao's seam filters the tool
  list.
- Pinned definitions are llmao code: at registration it records a digest of the
  server's tool list and descriptions, and a server whose list changes is taken out of
  the registry until a person approves the new digest.
- The label is registry metadata llmao keeps beside each server and returns with its
  results in a header, for a caller's egress to read.
- Acting as the person: the server receives the caller's identity from llmao, never a
  shared credential, and holds per-person tokens itself; how the proxy forwards a
  caller's credential to a server is to be checked against the deployed version.

The memory server itself stays beside the harness, per session, until proposal 2's
session token exists; before then, a hosted read-only search over public entries is the
safe first step.

## Where things stand, 12 October 2026

A reviewer at llmao will ask two things before any of the above: whether LiteLLM, the
proxy behind the gateway, already offers memory the Foundation could use instead of
this design, and what llmao itself has done since these proposals were written. Both,
as of 12 October.

### LiteLLM's own memory, and why it is not the record

LiteLLM ships one feature it calls memory, and it is a key-value store, not a record.

- **`/v1/memory`** stores a text value, with optional JSON metadata, under a globally
  unique key, in a Postgres table, scoped to the caller's user and team: create, read,
  update, delete, and list by key prefix
  ([docs](https://docs.litellm.ai/docs/proxy/memory),
  [PR #26218](https://github.com/BerriAI/litellm/pull/26218), merged 25 April 2026, in
  the open-source proxy since about v1.83). Nothing reads it while a model call runs:
  remembering and recalling are the client's job. It was on a draft deprecation list in
  July and kept after the community objected
  ([discussion #32090](https://github.com/BerriAI/litellm/discussions/32090)). The
  gateway runs LiteLLM 1.102.2 (llmao `4493935`, 8 October), so the route exists; with
  an ordinary PAT it answers 401, "Only proxy admin can be used…", so no user can reach
  it today.
- **Memory the gateway would keep by itself** was built and dropped. Two PRs by a
  BerriAI engineer had the gateway inject save, search and read tools into requests and
  run hidden tool rounds, with saving and recall switched on per user
  ([#40844](https://github.com/BerriAI/litellm/pull/40844), closed 12 September;
  [#40894](https://github.com/BerriAI/litellm/pull/40894), closed 4 October without
  comment). Neither was merged and nothing has replaced them.
- **Near it, and not memory**: Responses API sessions, which rebuild a conversation from
  the proxy's spend logs and S3 when `previous_response_id` is given
  ([docs](https://docs.litellm.ai/docs/response_api)); vector stores, whose data stays
  at the provider, the managed kind Enterprise-licensed
  ([docs](https://docs.litellm.ai/docs/completion/knowledgebase)); prompt management,
  which is templates ([docs](https://docs.litellm.ai/docs/proxy/prompt_management));
  semantic caching, which LiteLLM's own docs warn "goes badly wrong on agentic traffic"
  ([docs](https://docs.litellm.ai/docs/proxy/caching_semantic)); and Anthropic's memory
  tool, passed through for the client to execute. The A2A and MCP gateways keep no
  memory of their own.

None of this is the record the paper describes. `/v1/memory` has no trust tiers, no
labels or ceilings, no provenance, no search beyond a key prefix, no erasure with a
receipt, and no domain to partition by. And the dropped gateway memory went the
opposite way from this design: memory written and recalled out of sight of the harness
is the poisoning surface the paper's gates exist for, where here every read and write
is a tool call the harness makes and the policy decides, with its provenance kept. If
that work comes back upstream, llmao should leave it switched off.

What LiteLLM does offer this design is its MCP registry, with an approval step and a
cost per tool call: the place, under proposal 5, where the memory server and the taps
would be registered as tool servers. Its Anthropic memory-tool pass-through is a second,
smaller opening: the tool's commands could be served by the memory server, so a harness
that speaks only that tool still writes to the record.

### llmao since these proposals

These proposals were written against llmao at `266122c` (10 October). Since then:

| Proposal | In llmao |
|---|---|
| 1. One trace from a task to a model's answer | Nothing yet. `76a6899` (12 October) times each LiteLLM admin request and each page handler, to find why My Keys is slow: llmao's own pages, not model calls. Nothing in the code reads or sends `traceparent`. |
| 2. Sessions as a caller the gateway can tell apart | Nothing yet. |
| 3. A model list that tells the truth | On the human side only. The Models page was redesigned from [issue #46](https://github.com/apache/tooling-llmao/issues/46): status per deployment rather than per model, privacy computed from every configured deployment (Private, External or Mixed, a down one included), and whether a model thinks by default (`266122c`, `689c4ae`, `45da9c2`, `957d607`). The `/v1/models` list harnesses read is unchanged. |
| 4. Compatibility for harnesses | By documentation rather than in the gateway. The guide to pointing a harness at the gateway by hand (`811d775`, 9 October, contributed from this work) says what this launcher's egress fixes that a manual setup does not, the system-message fold among them; llmao's help now has a page per harness, [PR #51](https://github.com/apache/tooling-llmao/pull/51) (open) tightens the Claude Code setup, and [issue #44](https://github.com/apache/tooling-llmao/issues/44) asks for configuration snippets to copy. The fixes themselves are not in the gateway. |
| 5. Tool servers | [Issue #25](https://github.com/apache/tooling-llmao/issues/25), "MCP server registration through llmao" (open since 8 September, no replies), proposes LiteLLM's MCP registry with its review workflow, project access groups and per-tool cost: the place proposal 5 builds on. |

Beside them, [issue #43](https://github.com/apache/tooling-llmao/issues/43) (open)
considers metrics in Prometheus or OpenMetrics form for Infra's Datadog. The numbers it
would publish per key and model, calls, tokens and time, are the ones this launcher's
egress now keeps per session from the other side (`amem usage`, `amem cost`), which is
the argument for proposals 1 and 2: with a trace and a session on each call, the two
sets of numbers join. Everything else in llmao since 10 October is the vLLM hosting
fleet, the help pages and the navigation.
