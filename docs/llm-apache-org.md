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

The memory server itself stays beside the harness, per session, until proposal 2's
session token exists; before then, a hosted read-only search over public entries is the
safe first step.
