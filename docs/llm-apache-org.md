# What we would ask of llm.apache.org

Written 11 October 2026, from building this reference implementation against the
Foundation's gateway: llmao, the control plane with identity and PATs, in front of a
LiteLLM proxy. Most of what follows LiteLLM already does, so the ask is for llmao to
switch it on or let it through. Each ask names what our egress does about it today; that
workaround goes away when the gateway has it, and the gateway then does it for every
caller, not only ours. None of this is filed upstream yet.

What the gateway answered on 11 October 2026, with a PAT:

| Route | Answer |
|---|---|
| `/v1/models` | `gemma4-26b`, `qwen3-8b`, `qwen3.8-27b`; `qwen3-8b` is listed but often answers that it does not exist |
| `/v1beta/models/<model>:generateContent` | 200 on `gemma4-26b` and `qwen3.8-27b`: the Gemini route is deployed, and Gemini CLI and Antigravity run on it |
| `/v1beta/models` | 404: the Gemini-shaped model list is not routed |
| `/v1/mcp/server` | 200 with an empty list: LiteLLM's MCP gateway is reachable, with no servers on it yet |

## Already in LiteLLM: switch it on or pass it through

| Ask | Why | What we do today |
|---|---|---|
| **Drop unsupported parameters** (`drop_params`) | Qwen rejects a reasoning effort it does not know, so a harness's defaults fail | The launcher sets effort itself per harness |
| **Budgets and rate limits per key** | A spend limit belongs where every caller passes, not on one laptop | `[spend.<destination>]` in a root's `grants.toml`, checked by our egress |
| **Health checks and cooldowns** | A model that is listed and does not answer costs every harness a failed run | `LLMAO_MODEL` in `local/.env` pins a model that answers |
| **Request tags** | With a session and a task id on each request's spend record, the gateway's spend joins our telemetry and gives cost per task from the source | `amem cost` prices our own metering from a table the owner keeps |
| **OpenTelemetry, accepting `traceparent`** | Our spans and the gateway's become one trace, from a task's claim to the model's answer | Our spans stop at the egress |
| **Logprobs passed through** | The paper's most sensitive drift detector; vLLM returns them | `amem drift` compares answers, tokens, latency and fingerprints only |

## Configuration or fixes upstream

- **The Gemini-shaped model list.** Generating through `/v1beta` works; listing models
  there answers 404, so a client that discovers models the Gemini way finds none. Ours
  lists them through `/v1/models`, so this is small.
- **LiteLLM's Gemini adapter** drops a tool result sent under the model's role. Our
  egress folds it into the shape the adapter keeps; the upstream issue was closed by a
  stale bot, unfixed.
- **Trailing system messages and output limits.** Claude Code sends a system message
  after the conversation, which vLLM's chat templates refuse, and some harnesses ask for
  128k output tokens whatever the model allows. Our egress folds the one and caps the
  other; the gateway could do both for everyone.
- **Versioned model names.** Only aliases are listed. Drift detection pins a model, and
  an alias that moves underneath it reads as drift when it is a release.

## Tools a vendor runs that an open model does not

A harness's own tools, reading and editing files, a shell, grep, run on the machine and
work on any model. Some tools run at the vendor instead: Codex's `web_search` is a hosted
tool of OpenAI's Responses API, and Claude Code's web search runs inside Anthropic's API.
Pointed at the gateway's open models these silently do nothing: in the decision contexts
our egress recorded up to 11 October, Codex offered `web_search` on all 56 requests to the
gateway, and not one search came back. Claude Code's web fetch runs on the machine, and
under the launcher it has no network to fetch with.

The gap is the same whatever the cause: no search and no fetch. The hosted MCP servers
below are the way to fill it, a search server and a fetch server on the gateway, reached
through the egress, so that what comes back from the web is metered, logged and labelled
like anything else a session reads. LiteLLM also has search integrations of its own; the
MCP shape is the one every harness already speaks.

## Hosted MCP servers

The gateway is to serve tools as well as models. For us that is the same shape as model
calls, with one difference that makes it worth more.

**How a confined harness would reach one.** A harness under the launcher has no network
but the egress, and today the egress serves only the memory server's `/mcp`. A hosted
server becomes one more destination, `/mcp/<name>`, forwarded with the PAT injected, so
the harness never holds the credential, as for models. That is ours to build.

**Why it is worth more.** A tool call that passes through the egress is observed, not
reported. Today a harness's tool calls reach the record only through its own transcript,
labelled `observed_by: harness`. A hosted tool's call would be logged by the egress, under
the policy, with its decision, as a model call is.

**What we would ask of the gateway's MCP side:**

- **Access per key and per tool.** Servers and tools granted to a PAT, which our launcher
  narrows further per session from the policy in force.
- **Pinned tool definitions.** A digest of each server's tool list and descriptions,
  versioned. A tool description is read by the model as instructions, so a changed one is
  an injection, and a session should be able to pin what it was shown.
- **A classification on what a tool returns.** Each server declares the label of its
  results, public, internal or restricted, so the egress raises the session's label on a
  result as a memory `get` does, and the session's ceiling holds.
- **Acting as the person.** A tool that changes something, an issue filed or a comment
  posted, acts as the PAT's owner with that owner's scopes, never a shared account, and
  says which of its tools are irreversible, so our irreversibility gate can escalate them.
- **The same tags and trace.** Tool calls in the gateway's logs with the session and task
  ids and the `traceparent`, as for model calls.
- **Limits per tool,** calls and spend, beside the limits per key.

What a hosted tool returns is data, never instructions: the memory server's write scan
applies before anything from it is written to the record, as for any other source.

**The memory server as a hosted MCP server** is tempting and should wait. The paper keeps
it beside the harness, per session, because every read is checked against what that
session has already read and the ceiling of where its model calls go. A hosted memory
server would need the session's identity on every call, a token the launcher mints per
session and the server honours, not only the PAT. Until the gateway can carry that, a
hosted read-only search over public entries is the safe first step.

## On our side

- The egress sends `traceparent` and the session and task as request tags.
- A route per hosted MCP server in the egress, with the PAT injected, tool calls logged
  and decided, and results labelled.
- Spend limits read from the gateway's per-key budgets once they exist, not only from a
  root's `grants.toml`.
