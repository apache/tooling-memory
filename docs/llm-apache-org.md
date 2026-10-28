# Proposals for llm.apache.org

llmao is an Apache project,
[apache/tooling-llmao](https://github.com/apache/tooling-llmao). The proposals below are
to go to it as pull requests, in the same way as [its guide to pointing a harness at the
gateway](https://github.com/apache/tooling-llmao/blob/main/docs/llmao-harnesses.md). Its
guide went to llmao in this way before. Each proposal starts with what goes wrong without
it, and with what the Foundation gets. It does not start with a setting. The settings are
the easy part. A reviewer weighs the motivation. Where an open standard covers the need,
the proposal names the standard. Thus the gateway's surface stays llmao's own, and LiteLLM
stays an implementation behind it.

We wrote this on 11 October 2026. It comes from building this reference implementation
against the gateway, and from llmao at `266122c`. On that day, the gateway gave these
answers to requests with a PAT:

| Route | Answer |
|---|---|
| `/v1/models` | `gemma4-26b`, `qwen3-8b`, `qwen3.8-27b`. The list has `qwen3-8b`, but that model often answers that it does not exist |
| `/v1beta/models/<model>:generateContent` | 200. The Gemini route is deployed. Gemini CLI and Antigravity run on it |
| `/v1beta/models` | 404. The gateway does not route the Gemini-shaped model list |
| `/v1/mcp/server` | 200 with an empty list. A caller can reach the MCP registry of the proxy, but the registry has no servers yet |

Each proposal ends with potential implementation details. Where the proxy has its own
setting for a detail, the detail names that setting. The setting names come from
LiteLLM's documentation. Check them against the version that the gateway runs. llmao
reads that version from the readiness endpoint of the proxy.

These proposals build on what llmao already has:

- Apache identity on every key
- keys per project, user and purpose
- budgets per project
- `drop_params`
- a health roll-up per model on its own pages.

## 1. One trace from a task to a model's answer

**Why.** An agent's session can go wrong, or cost more than it should. Then the question
is always: which task, which session, which model call, which tool call? Today each
system gives only its own part of the answer. Our launcher traces a session up to the
request that it sends. The gateway records spend per key. Nothing joins the two. Three
uses need one trace:

- cost per task
- the audit of what an agent decided
- debugging a failed session across our wrapper, the gateway and a tool server.

When a fleet of agents works at all hours, on any task that it can claim, one trace is how
anyone finds out what happened.

**Standard.**

- W3C Trace Context (`traceparent`).
- W3C Baggage, for the session and the task.
- OpenTelemetry's semantic conventions for generative AI, for the attributes: model,
  tokens, finish reason, `gen_ai.conversation.id`.

**Proposal.** The gateway continues a trace that a caller gives it. It exports its own
spans under its own service name, `llm.apache.org`. The spans are a request, a model call
and, when tool servers are on the gateway, a tool call. The collector of the gateway
keeps the internal spans of the proxy out. Thus what a caller sees is the gateway's
contract. Each response has the gateway's request id in a header. Thus a caller can join
the two by id, also without a shared trace backend. A shared backend, where the owner of
a PAT can query their own traces, can come later.

**Ours.** The launcher's egress sends `traceparent` and `baggage` on every request. It
records the gateway's request id on its span.

**Potential implementation details.**

- LiteLLM's OpenTelemetry callback, `litellm_settings: callbacks: ["otel"]`.
  `OTEL_EXPORTER`, `OTEL_ENDPOINT` and `OTEL_HEADERS` configure it, with
  `OTEL_SERVICE_NAME` set to `llm.apache.org`. The callback continues an incoming
  `traceparent`. Thus the gateway's spans join the caller's trace, with no code in llmao.
- An OpenTelemetry Collector between the proxy and the backend, with a `filter` or
  `transform` processor. The processor drops or renames the internal spans of the proxy.
  It also removes the prompt and response attributes. Thus what leaves is llmao's
  contract.
- The request id that the proxy already returns, `x-litellm-call-id`, documented as
  llmao's request id. If the name should stay hidden, llmao can rename it at its vhost.

## 2. Sessions as a caller the gateway can tell apart

**Why.** A key is per project, user and purpose. A fleet of agents runs on one automation
key. To the gateway, every session on that key looks the same. The gateway cannot limit,
revoke or bill one session apart from the others. An agent that is out of control can
spend the project's budget before anyone notices. The same problem blocks a hosted memory
server. That server must know which session sends each request, because it checks every
read against what that session has already read, and against the ceiling of the
destinations of its model calls.

**Standard.** Now: Baggage for a session id. Later: OAuth 2.0 Token Exchange (RFC 8693)
for a session-scoped token.

**Proposal.** In two steps.

1. A session id in baggage. The gateway records it on the spend row and on the span,
   scoped to the key that sent it. Thus a guessed id can confuse only its owner's own
   sessions. Also limits per key, beside the budget per project, so that an automation
   key has an upper limit of its own.
2. A session token. The caller exchanges its PAT for a short-lived token bound to one
   session. The token can be revoked without a rotation of the PAT. A hosted tool or
   memory server can trust the token. A hosted memory server waits for this token.

**Ours.** The egress already holds the PAT, so that a harness never sees it. The egress
would set the session id, and remove any session id that the harness sends. Later, the
egress would do the exchange.

**Potential implementation details.**

- The proxy's own fields carry the session id and the task. The session id goes in
  `litellm_session_id` in the request; the spend logs and UI of the proxy group by this
  field. The task goes in `tags`, or in the `x-litellm-tags` header. A small pre-call
  hook in llmao maps W3C `baggage` onto these fields. Thus callers use the standard, and
  never the names of the proxy.
- Limits per key are parameters of the key API of the proxy. llmao already calls this
  API to issue PATs. The parameters are `max_budget` with `budget_duration`, and
  `tpm_limit`, `rpm_limit` and `max_parallel_requests`. Automation keys would get them by
  default.
- The session token is llmao code over the same key API. On a token exchange, llmao
  checks the PAT and makes a virtual key with:
  - a short `duration`
  - the PAT's team
  - the session in its `metadata`
  - its own limits.

  To revoke the token, llmao deletes the key. llmao needs no new store, and the proxy
  enforces the token like any key.

## 3. A model list that tells the truth

**Why.** A harness finds models in the list. It fails on the first call to a model that
does not answer. Since 5 October or earlier, the list has had `qwen3-8b`, and in that
time the model often did not answer. Every caller finds this out at the cost of a failed
session. Drift detection watches whether the model behind a name has changed. While the
list has only aliases, drift detection cannot tell a deliberate release from a silent
swap.

**Proposal.** The API's model list follows the health roll-up that llmao already shows
on its pages. Each alias also names the version that it points at, as a versioned name
or as a field. Thus a change of version is visible. The gateway routes the Gemini-shaped
list at `/v1beta/models` like the rest. Logprobs pass through where vLLM returns them.
They are the most sensitive drift signal there is. No other signal shows a change in a
model before its answers change.

**Potential implementation details.**

- llmao's model status already decides "in service". The same loop can remove a
  deployment that fails its probes from the proxy, with `/model/delete`. When the
  deployment recovers, the loop can add it again with `/model/new`, which llmao already
  uses. Between these two events, other settings keep a failing deployment out of
  routing, but not out of the list. These settings are the proxy's
  `general_settings: background_health_checks` with `health_check_interval`, and the
  router's `allowed_fails` and `cooldown_time`.
- Versions in the `model_info` that llmao sends with `/model/new`, such as `version` and
  `base_model`. The proxy serves these on `/v1/model/info`. Or versioned model names,
  with the alias kept as a second name.
- `logprobs` and `top_logprobs` are OpenAI parameters. The proxy passes them to
  `hosted_vllm`, and vLLM honours them. Check that `drop_params` does not drop them for
  these deployments.
- The Gemini-shaped list is a small route in llmao or its vhost. The route answers
  `GET /v1beta/models` from `/v1/models`, in Gemini's shape.

## 4. Compatibility for harnesses, fixed once in the gateway

**Why.** Each harness is written for the API of its vendor. Four differences break the
harnesses on the gateway's models. Our egress rewrites each one, as llmao's harness guide
records. Anyone else who gets a harness working does the same, or gives up. If the
gateway fixes the differences, every harness works with a manual setup. Then every
Apache contributor spends the first hour on the gateway on work, not on debugging.

| Difference | Harness | What breaks |
|---|---|---|
| a system message after the conversation | Claude Code | Qwen's chat template refuses the request |
| a Gemini tool result under the model's role | Gemini CLI, Antigravity | the proxy's Gemini translation drops it. The model answers without its tool's result |
| an output limit of 128k tokens | Muse Code | a 131k-context model refuses when the count includes the prompt |
| a reasoning effort of `high` | Claude Code | vLLM takes low, medium and xhigh. The model supports the parameter, so `drop_params` passes it on, and the value is refused |

**Proposal.** The gateway normalises each difference in a pre-call hook. The hook:

- folds the system message
- moves the tool result to the user's role
- caps the output at the model's room
- maps the effort to the nearest level that the model takes.

Each fix is a few lines with a test, and the upstream fix where there is one. A stale
bot closed the issue for the Gemini translation, and the bug is not fixed.

**Potential implementation details.**

- One `CustomLogger` in llmao with an `async_pre_call_hook`, registered under
  `litellm_settings: callbacks`. It rewrites the request before the proxy routes it:
  - fold a late system message into the first
  - move a Gemini tool result to the user's role
  - clamp `max_tokens` to the model's room
  - map `reasoning_effort` to the nearest level that the model takes.

  Each is a few lines with a test.
- `litellm_settings: modify_params: true` already fixes some message shapes for some
  providers. Before you write the hook, do one test: does it cover these four on
  `hosted_vllm`?
- An upstream fix for the Gemini translation bug is also worth the work, because the
  hook only works around the bug.

## 5. Tool servers: what the registry needs to hold

**Why.** The proxy can manage MCP servers itself. Thus `llm.apache.org` can list the tool
servers that a key may reach, each on its own host. Two things that we learned from
building this make that more urgent.

The vendors' own tools do not go through the gateway. Codex's `web_search` runs inside
OpenAI's API. Claude Code's web search runs inside Anthropic's API. On the gateway's open
models, these tools do nothing, and they give no error. Our egress recorded decision
contexts up to 11 October. In them, Codex offered `web_search` on all 56 requests to the
gateway, and not one search came back. A search server and a fetch server in the
registry fix this for every harness at the same time.

Also, a tool server is an injection surface. The model reads the description of a tool
as instructions. Anyone can write what a tool returns, such as a mail thread or a web
page.

**Proposal.** For each server in the registry:

- **Grants per key and per tool,** private by default. A caller can narrow them more for
  each session.
- **Pinned tool definitions:** a versioned digest of the tool list and the descriptions.
  Thus a changed description is visible before a model reads it.
- **A label on what a tool returns,** a classification such as public, internal or
  restricted. With it, a caller raises the session label on the result, and its ceiling
  holds.
- **Acting as the person:** a tool that changes something acts as the key's owner, with
  their scopes, never as a shared account. It declares which of its tools are
  irreversible.
- **The same trace,** session and limits as model calls, from proposals 1 and 2.

**Ours.** The egress gets a route for each registered server, `/mcp/<name>`. The egress
adds the PAT, so that the harness never holds it. The egress then observes each tool call
through that route, logs it with its policy decision, and labels it. Today, a harness's
tool calls reach the record only through its own transcript, which is a self-report.
What a tool returns is data, never instructions. The memory server's write scan applies
before any of it goes into the record.

**Potential implementation details.**

- Servers registered in the proxy's MCP gateway, in its `mcp_servers` configuration or
  through its API. Each server points at its own host.
- Grants through the proxy's object permissions on keys and teams. These name the MCP
  servers that a key may reach. llmao sets them when it issues keys, private by default.
  Where the deployed version cannot narrow to single tools, llmao's seam filters the
  tool list.
- Pinned definitions are llmao code. At registration, llmao records a digest of the
  server's tool list and descriptions. If the list of a server changes, llmao takes the
  server out of the registry until a person approves the new digest.
- The label is registry metadata that llmao keeps beside each server. llmao returns it
  with the server's results in a header, for a caller's egress to read.
- Acting as the person: the server receives the caller's identity from llmao, never a
  shared credential. The server holds the tokens for each person itself. Check against
  the deployed version how the proxy forwards a caller's credential to a server.

The memory server itself stays beside the harness, per session, until the session token
of proposal 2 exists. Before then, the safe first step is a hosted read-only search over
public entries.

## Where things stand, 12 October 2026

A reviewer at llmao will ask two questions before any of the above. First: does LiteLLM,
the proxy behind the gateway, already offer memory that the Foundation could use instead
of this design? Second: what has llmao itself done since we wrote these proposals? This
section answers both, as of 12 October.

### LiteLLM's own memory, and why it is not the record

LiteLLM ships one feature that it calls memory. That feature is a key-value store, not a
record.

- **`/v1/memory`** stores a text value, with optional JSON metadata, under a globally
  unique key, in a Postgres table. It is scoped to the caller's user and team. The route
  can create, read, update, delete, and list by key prefix
  ([docs](https://docs.litellm.ai/docs/proxy/memory),
  [PR #26218](https://github.com/BerriAI/litellm/pull/26218), merged 25 April 2026, in
  the open-source proxy since about v1.83). Nothing reads it while a model call runs.
  Remembering and recalling are the caller's job. In July it was on a draft deprecation
  list. LiteLLM kept it after the community objected
  ([discussion #32090](https://github.com/BerriAI/litellm/discussions/32090)). The
  gateway runs LiteLLM 1.102.2 (llmao `4493935`, 8 October), so the route exists. With an
  ordinary PAT it answers 401, "Only proxy admin can be used…". Thus no user can reach
  it today.
- **Memory the gateway would keep by itself** was built and dropped. Two PRs by a
  BerriAI engineer made the gateway inject save, search and read tools into requests,
  and run hidden tool rounds. In those PRs, saving and recall were a setting for each user
  ([#40844](https://github.com/BerriAI/litellm/pull/40844), closed 12 September;
  [#40894](https://github.com/BerriAI/litellm/pull/40894), closed 4 October without
  comment). Nobody merged either PR, and nothing has replaced them.
- **Near it, and not memory**:
  - Responses API sessions. These rebuild a conversation from the proxy's spend logs and
    S3 when the caller gives `previous_response_id`
    ([docs](https://docs.litellm.ai/docs/response_api)).
  - Vector stores. Their data stays at the provider, and the managed kind is
    Enterprise-licensed ([docs](https://docs.litellm.ai/docs/completion/knowledgebase)).
  - Prompt management, which is templates
    ([docs](https://docs.litellm.ai/docs/proxy/prompt_management)).
  - Semantic caching. LiteLLM's own docs warn that it "goes badly wrong on agentic
    traffic" ([docs](https://docs.litellm.ai/docs/proxy/caching_semantic)).
  - Anthropic's memory tool, passed through for the caller to execute.

  The A2A and MCP gateways keep no memory of their own.

None of this is the record that the paper describes. `/v1/memory` has none of these:

- trust tiers
- labels or ceilings
- provenance
- search beyond a key prefix
- erasure with a receipt
- a domain to partition by.

Also, the dropped gateway memory went the opposite way from this design. Memory that is
written and recalled where the harness cannot see it is the poisoning surface that the
paper's gates exist for. In this design, every read and write is a tool call that the
harness makes and the policy decides, and its provenance is kept. If that work comes
back upstream, llmao should leave it switched off.

What LiteLLM does offer this design is its MCP registry, with an approval step and a cost
per tool call. That registry is the place, under proposal 5, where the memory server and
the taps would be registered as tool servers. Its pass-through of Anthropic's memory tool
is a second, smaller opportunity. The memory server could serve the commands of that
tool. Then a harness that uses only that tool still writes to the record.

### llmao since these proposals

We wrote these proposals against llmao at `266122c` (10 October). Since then:

| Proposal | In llmao |
|---|---|
| 1. One trace from a task to a model's answer | Nothing yet. `76a6899` (12 October) times each LiteLLM admin request and each page handler, to find why My Keys is slow. This is for llmao's own pages, not for model calls. Nothing in the code reads or sends `traceparent`. |
| 2. Sessions as a caller the gateway can tell apart | Nothing yet. |
| 3. A model list that tells the truth | Only on the side that people see. llmao redesigned the Models page from [issue #46](https://github.com/apache/tooling-llmao/issues/46). The page shows status per deployment, not per model. It computes privacy from every configured deployment (Private, External or Mixed, a down one included). It shows whether a model thinks by default (`266122c`, `689c4ae`, `45da9c2`, `957d607`). The `/v1/models` list that harnesses read did not change. |
| 4. Compatibility for harnesses | In documentation, not in the gateway. This work contributed the guide to pointing a harness at the gateway by hand (`811d775`, 9 October). The guide says what this launcher's egress fixes and a manual setup does not. The system-message fold is one of these fixes. llmao's help now has a page for each harness. [PR #51](https://github.com/apache/tooling-llmao/pull/51) (open) tightens the Claude Code setup. [Issue #44](https://github.com/apache/tooling-llmao/issues/44) asks for configuration snippets to copy. The fixes themselves are not in the gateway. |
| 5. Tool servers | [Issue #25](https://github.com/apache/tooling-llmao/issues/25) is "MCP server registration through llmao" (open since 8 September, no replies). It proposes LiteLLM's MCP registry with its review workflow, project access groups and per-tool cost. Proposal 5 builds on that registry. |

Also, [issue #43](https://github.com/apache/tooling-llmao/issues/43) (open) considers
metrics in Prometheus or OpenMetrics form for Infra's Datadog. It would publish calls,
tokens and time, per key and model. This launcher's egress now keeps the same numbers
per session, from the other side (`amem usage`, `amem cost`). That is the argument for
proposals 1 and 2. With a trace and a session on each call, the two sets of numbers
join. All other changes in llmao since 10 October are to the vLLM hosting fleet, the
help pages and the navigation.
