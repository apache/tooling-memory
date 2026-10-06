# Running it locally

## Confined: `memory launch`

The way to run a harness. It needs no containers. The harness gets its own user, mount,
network, pid, uts and ipc namespaces, built natively by the `memory` binary with no
privilege: the system's binaries read-only, a private home, the working tree read-write,
the memory binary, and the egress socket. No network interface except loopback. Inside,
a relay forwards one loopback port to the egress, which serves memory over MCP and
forwards model calls to the provider or the gateway, checks the session label against the
destination's ceiling first, and reads token usage out of every response.

```sh
make launch HARNESS=claude                    # claude, your own login, confined
make launch HARNESS=codex                     # codex, your ChatGPT login
make launch HARNESS=codex AUTH=key            # codex, OPENAI_API_KEY from local/.env
make launch HARNESS=gemini                    # gemini, your Google login
make launch HARNESS=gemini AUTH=key           # gemini, GEMINI_API_KEY
make launch HARNESS=copilot                   # copilot, your GitHub login (GH_TOKEN)
make launch HARNESS=opencode AUTH=key         # opencode, ANTHROPIC_API_KEY
make launch HARNESS=opencode AUTH=key PROVIDER=xai   # the same harness, Grok behind it
make launch HARNESS=claude AUTH=gateway       # claude through the local LiteLLM
LLMAO_URL=https://llm.apache.org make launch HARNESS=codex AUTH=gateway MODEL=qwen3.8-27b
                                              # the Foundation gateway, with the PAT in local/.virtual-key
make launch HARNESS=muse AUTH=key             # Muse Code, META_API_KEY
make launch HARNESS=claude MODEL=claude-sonnet-5-5   # the model, in the harness's own flag
make launch HARNESS=claude SUBAGENTS=board    # a subagent the harness would spawn is a task instead
make launch HARNESS=claude ARGS='-p "load T0 and summarise"'  # arguments go to the harness
make launch HARNESS=claude PROMPT='load T0 and summarise'  # non-interactive, in the harness's headless form
make work HARNESS=claude                      # claim the next task from the ledger and run it
make work HARNESS=claude LOOP=1               # keep claiming until interrupted
make usage                                    # tokens by destination and model
```

`HARNESS=` is required, in `make` as in `memory launch --harness`: there is no default
harness, so a run is never the wrong harness by omission.

Three ways to run, one launcher. Interactive is the harness as you know it. `PROMPT` runs the
harness's headless form with the harness's own permission prompts off, since nobody is
there to answer them and a task needs edits and commands; the sandbox is the boundary, and
the working tree is what it can change. It prints the answer and exits with its code; a run that leaves no
handoff exits 4, since a non-interactive run whose outcome is only in its transcript has
left nothing for the next session (`--no-handoff` if that is intended). `make work` claims
the next admitted task the session may take, builds the prompt from T0, the task, where the
last attempt stopped, and the handoffs of the tasks it depends on, and runs the harness on
it. The launcher holds the lease and heartbeats while the harness works, so a crashed or
wedged harness cannot leave a task stuck. On exit, a task the harness completed stays
complete (exit 0); anything else goes back to the pool with the harness's output filed as a
handoff and attached to the task (exit 4), so the next claimant starts where this one
stopped. Nothing claimable is exit 3. The launcher reaches the ledger through the memory
server's own tool surface, as a client like the harness, so the ledger's rules live in one
place and a launcher that is behind still works.

```sh
memory task create "Finish the import" --body "Steps 3 to 5." --priority 5
memory task admit task-me-xxxxxxxx
make work HARNESS=codex AUTH=key
```

Eight harnesses, one launcher. Each needed the same three things, and the table is the
whole of what differs:

| Harness | Model calls reach the egress by | Memory over MCP by | `AUTH` modes |
|---|---|---|---|
| Claude Code | `ANTHROPIC_BASE_URL` | `--mcp-config` | subscription, key, gateway |
| Codex | `model_providers.<id>.base_url` in `config.toml`; with a ChatGPT login, a CONNECT tunnel to the ChatGPT hosts | `mcp_servers.<id>.url` | subscription, key, gateway; `--provider xai` for Grok |
| Gemini CLI | `GOOGLE_GEMINI_BASE_URL`; with a Google login, `CODE_ASSIST_ENDPOINT` | `mcpServers.<id>.httpUrl` | subscription, key |
| Antigravity CLI (`agy`) | `GOOGLE_GEMINI_BASE_URL`, once its settings name the Gemini API as the model provider; with a Google login, `HTTPS_PROXY` to the egress, which tunnels only to Google's login and Cloud Code hosts, pinned without metering | `mcpServers.<id>.serverUrl` in `~/.gemini/config/mcp_config.json` | subscription (the token file `agy` left under `~/.gemini/antigravity-cli`), key. Not in the harness image yet. Written from the docs and the issue tracker, not a run: the first launch on a real machine is the test |
| Qwen Code | `OPENAI_BASE_URL`, once its settings select the OpenAI auth type | `mcpServers.<id>.httpUrl` | key only, since Qwen's free login tier ended in April 2026; `dashscope` by default, `--provider openai` or `xai` |
| Grok Build (`grok`) | `GROK_XAI_API_BASE_URL`, which only API-key inference reads; with a Grok login, `HTTPS_PROXY` to the egress, which tunnels only to Grok's proxy and relay hosts and `auth.x.ai`, pinned without metering | `mcp_servers.<id>.url` in `config.toml` | subscription, key |
| Muse Code (`muse`) | `--base-url` on `exec`, which the launcher puts after the prompt, since only the subcommand takes it; a key is `META_API_KEY`, which outranks a login | `mcp_servers.<id>` as a `streamable_http` entry in `~/.config/muse/settings.json`, which must carry `schema_version` | key, to Meta's Model API at `api.meta.ai`; subscription as the file `muse login` leaves, copied in, forwarded through the same base; gateway, proven on the Foundation gateway with Qwen. Hooks are Claude Code's shape and events under `hooks` in the same file; the session store is binary journals and a hook gets no transcript path, so the headless run is `exec --json`, whose record stream the launcher keeps as the transcript under the private home and prints only the final text of. Written from a real install, run on its offline echo provider, against a local listener, and through the gateway; a Meta key or login is still untried |
| Copilot CLI | no override exists; `HTTPS_PROXY` to the egress, which tunnels only to GitHub's Copilot hosts | `mcp-config.json` | subscription (`GH_TOKEN`) |
| OpenCode | `provider.<id>.options.baseURL`; on a GitHub Copilot login, `HTTPS_PROXY` and the GitHub tunnel | `mcp.<id>.url` | key; `--provider openai` or `xai`; subscription with `--provider github-copilot`, which GitHub supports |
| `command` | the program itself: `OPENAI_BASE_URL`, `ANTHROPIC_BASE_URL`, `GOOGLE_GEMINI_BASE_URL` in its environment | `MEMORY_MCP_URL` in its environment | key, gateway; `--provider` picks the destination |

Copilot is the odd one: with no base-URL override, its HTTPS goes through the egress as a
CONNECT tunnel to an allowlist of hosts. That is pinned egress without metering: TLS runs
end to end, so the egress sees no tokens and checks the ceiling when the tunnel opens. What
a tunnelled run cost is then what the harness says it cost: after the run the launcher reads
the transcript the harness left and records its own token counts, Codex's running total
from its rollout and Claude Code's per-call usage, into the same telemetry marked `harness`
rather than `egress`, so `memory usage` and the savings funnel show them, flagged as
self-reported. Copilot CLI leaves no transcript, so a Copilot run stays uncounted. Under a
subscription a count is a quantity, not a bill: Codex also reports how much of the plan's
window a run used, and the launcher prints that.

The Foundation's gateway, `llm.apache.org`, with the PAT llmao issued in `local/.virtual-key`
and `LLMAO_URL` pointing at it: through the gateway every harness has one destination,
`llmao`, with the gateway's ceiling, and one model, named with `MODEL=`. Tried here on
5 October against the three open models it served then (`qwen3.8-27b`, `gemma4-26b`,
`qwen3-8b`), each a prompted run asking for one word:

| Harness | Through the Foundation gateway | Why |
|---|---|---|
| Codex | works, metered | Responses API under `/v1` |
| Qwen Code | works, metered | chat completions under `/v1` |
| OpenCode | works, metered, as `llmao/<model>`; the launcher declares the model in the provider config, since models.dev is out of reach | chat completions |
| Claude Code | works, metered, after two things the launcher does on the gateway route: Claude Code's default effort, `high`, is Anthropic-only and vLLM takes `low`, `medium` and `xhigh`, so `CLAUDE_CODE_EFFORT_LEVEL=medium` goes in its environment, which `-- --effort <level>` overrides; and Claude Code puts its environment block in `messages` as a `system` role after the user turn, which Anthropic's API takes and the Qwen chat template behind LiteLLM's translation refuses (`System message must be at the beginning`), so the egress folds a `system`-role message into the request's `system` field on that route. Found by capturing the request Claude Code sends and replaying it: as sent, 400; folded, 200. llmao's own notes (`apache/tooling-llmao`, "Connecting your agent") add three things the launcher follows or leaves to you: `CLAUDE_CODE_MAX_CONTEXT_TOKENS=120000`, set on this route, since Claude Code assumes 200k and compacts too late; a model that reasons before it emits can make Claude Code abandon the stream, so `gemma4-26b` is the one they recommend; and tool use through the Anthropic path is broken on their vLLM today, so a one-word answer works and a task with tools may not | Anthropic Messages at the root |
| Grok Build | works, metered: Grok checks a model id against its own catalog, so the launcher declares the model as a `[model.<id>]` block in its config with the egress as `base_url` and `chat_completions` as the backend, and `-m <id>` selects it | custom model block |
| Muse Code | works, metered, after three things: its first call is its own model catalog, `GET /muse-code/models`, which it addresses at the base URL's origin and reads in the OpenAI model-list shape, so at a destination that is not Meta's the egress answers it from that destination's `/v1/models`; it asks for `high` effort by default, so the launcher passes `--reasoning-effort medium` on this route; and it asks for 128k output tokens whatever the catalog says, which a 131k-context model refuses once the prompt is counted, so the egress caps the output tokens a request asks for at 32k on this route. Inference is then the Responses API, streamed. Its reminder observers, background agents of its own, are off in headless runs | catalog, then Responses |
| Gemini CLI, Antigravity | no: the gateway has no `generateContent` route | API shape |
| Copilot CLI | no: no base-URL override | GitHub only |

`--model` passes a model in the harness's own flag, after the prompt, where every harness,
subcommand or not, accepts one; for Claude Code it also sets every role's default, since
Claude Code picks a model per role and a gateway that serves one model answers for that
one only. `--subagents board`, the default in `work`, turns a call that would spawn or
orchestrate subagents into tasks: Claude Code's `Agent` and `Workflow` tools and Codex's
`spawn_agent` are caught by the pre-tool hook, each prompt is filed on the ledger as a
task of its own, born proposed, and the call is refused with the task ids, so the harness
carries on with its own part and the board runs the rest. A `Workflow` script is one task
per `agent()` call whose prompt is a literal, titled by its `label`; a script the hook
cannot read is one task holding the script. The fan-out, the waits and the merges the
script would have done are the board's: a task's result comes back as a handoff.
Grok is not a harness but a destination: any OpenAI-shaped harness reaches it with
`--provider xai` and `XAI_API_KEY`. `command` is not a harness either but a program: the
first argument after `--` runs as given, confined, with the egress and the memory server in
its environment and nothing else configured. That is how a Gofannon agent, ordinary Python
calling a gateway, runs under the same confinement and metering as a harness:

```sh
make launch HARNESS=command AUTH=gateway ARGS='python3 agent.py'
```

GitHub Copilot and the other harnesses: GitHub supports a Copilot subscription in Copilot
CLI and, since January 2026, in OpenCode. Codex is not among the clients GitHub accepts;
its API refuses requests from clients it does not recognise, and the community proxies
that work around that are not something to build on. OpenCode on a Copilot login is
`make launch HARNESS=opencode PROVIDER=github-copilot` after `opencode auth login` on the
host, through the same tunnel Copilot CLI uses.

| `AUTH` | Model calls go to | Credential | Who holds it |
|---|---|---|---|
| `subscription` | the provider | the harness's own login: Claude Code's credentials file, Codex's `auth.json`, Gemini's `oauth_creds.json`, Copilot's `GH_TOKEN` | the harness, inside its private home or environment |
| `key` | the provider | `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY` or `XAI_API_KEY` from `local/.env` | the egress; the harness gets a placeholder |
| `gateway` | LiteLLM on 127.0.0.1:4000, or the institution's at `LLMAO_URL` | `local/.virtual-key`, a virtual key or an llmao PAT | the egress; the harness gets a placeholder |

The memory makes calls of its own: the recall deputy that writes a brief, and mining a
transcript. By default they follow `AUTH`, which on a login means they are not made, since
the login is the harness's alone. `SERVER_AUTH=gateway` gives them a credential of their
own, so a harness on a personal login leaves the memory's calls to the institution:

```sh
make launch HARNESS=claude AUTH=subscription SERVER_AUTH=gateway
make launch HARNESS=grok AUTH=gateway ARGS='-m <a model id the gateway serves>'
```

`SERVER_AUTH=key` with `SERVER_PROVIDER` works the same way with a provider key; the
gateway takes no provider name. The gateway's route is named `llmao` and takes the
`[destinations.llmao]` ceiling. `memory web` does not pass these flags yet. Taps never
use either: a tap's credential is the variable its grant names, `GH_TOKEN` say, which the
host injects as a bearer the tap never sees, under whatever `AUTH` the harness runs.

A login is made once on the host, in the harness's own way, and the launcher copies that
one file into the private home. For Claude Code, `claude` and its login prompt; for Codex,
`codex login` or, with no browser to hand, `codex login --device-auth`; for Gemini,
`gemini` and `/auth`, Login with Google. Nothing else under your home is read or mounted.
The ChatGPT and Google logins talk to backends that are not the public API. Gemini's Code
Assist is forwarded and metered like the rest, with only the token refresh tunnelled. The
ChatGPT backend refuses relayed requests: its bot protection answers a forwarded call with
a challenge page, and Codex speaks websockets first, which a forwarding proxy cannot carry.
So a Codex login is a tunnel, like Copilot's: pinned egress, with the usage taken from
Codex's own rollout afterwards. Proven here on a ChatGPT login: Codex loaded T0 over MCP,
its hooks fired, and it completed a ledger task.
Two things Codex needs that a bare release tarball lacks: the `codex-code-mode-host` helper
beside the binary, which its tools go through (the npm install and the harness image have
it; the launcher warns when it is missing), and, headless, its own approval and hook-trust
gates bypassed, since nobody is there to answer and the sandbox is the boundary.

In every mode the session's handling is capped at the destination's ceiling from
`grants.toml`, so a session whose calls go to a destination with a public ceiling reads only
public entries. Not supported, each refused with the reason: OpenCode with a Claude Pro or
Max login (Anthropic prohibits third-party clients), Copilot with a key (it has no key
mode). A call the upstream refuses is recorded in `make usage` under its HTTP status, so
an expired login or a wrong key shows up there rather than vanishing.

## The savings, measured: `make savings`

The paper's case is that reads dominate cost and that summaries-first retrieval, a T0
under two thousand tokens and recovery from a handoff rather than a transcript cut what an
agent ingests. `make savings` measures that on your own record rather than asserting it.
Recovery: tokens of T0 and the latest handoff against the transcripts the sessions here
left in their private homes, so the comparison is with what a replay of this machine's own
sessions would cost. Retrieval: a page of ten BM25 hits in tokens against the bodies those
hits stand for and against the whole corpus, and every entry searched by its own title to
see whether known items land in the top three. The funnel:
every read the memory server answered is logged with the tokens it served
(`state/reads.jsonl`), shown per session beside what that session's model calls ingested
according to the egress. `QUERY='...'` picks the retrieval query; `--json` gives the numbers.

A record of six entries proves nothing, so seed one of a few hundred from Markdown you
have, one entry per section, and run a task over it. `make demo` does all of it in a
record of its own (`DEMO_ROOT`, seeded from `DEMO_FROM`), measuring before and after; by
hand it is:

```sh
memory --root ~/memory-demo init --owner human:asf:you --domain dom-demo
scripts/seed-corpus.py --root ~/memory-demo --memory target/release/memory docs
memory --root ~/memory-demo task create "Answer: what did DR-19 decide, and why?" \
  --body "Search the record, open only what you need, answer in a handoff with the entry ids you cite, then complete." --priority 5
memory --root ~/memory-demo task admit task-demo-xxxxxxxx
memory --root ~/memory-demo work --harness claude
memory --root ~/memory-demo savings --query "what did DR-19 decide about MoinMoin"
```

On 243 entries and 76k tokens here: T0 was 2.6% of the corpus, 194 of 200 known items
ranked in the top three, the answer took seven reads and 3k tokens served against a
corpus of 76k, and T0 plus the handoff was 58% of replaying that session's transcript.
Search ANDs every word as a phrase, the known-item case; when that finds nothing it
retries with any word, still ranked by BM25, so a question in natural language lands too.

## In the browser: `make web`

`make web` serves a web terminal on 127.0.0.1:4200. The front page lists sessions and
starts one from a form: the harness, named, since there is no default; the auth mode; the
destination, where the mode names one; the model, picked from what the destination lists
or typed, the list fetched through `memory models`, which routes as a launch would with
the destination's credential and asks for its `/v1/models`; where subagents go; a prompt,
or interactive; or the next task from the ledger. The form shows the `memory launch` line
it will run as you fill it in. Each
session is `memory launch` (or `memory work`) in a pseudo-terminal the server keeps
alive whether or not a browser is attached, with the same confinement as from a shell.
Its output goes into a two-megabyte ring on the server and to every attached browser, so
a refresh, a dropped connection or a second browser replays the scrollback and lands in
the same place; resizes reach the harness, and `/api/sessions/<id>/scrollback` returns
the ring as text for a script with no websocket. Beside the terminal a reading pane renders
Markdown, Mermaid included, for entries from the record: whatever the session writes
opens as it lands, a `mem-…` id in the terminal is a link, and the recent entries are
listed under it. Loopback only; reach it over SSH when remote. xterm.js, marked and
mermaid are served from the binary, so the browser needs no internet either.

## Handoffs the harness did not write

The launcher installs three hooks in each harness's settings in the private home: before
context is compacted, at session end, and before a tool runs. Claude Code and Codex call
the first PreCompact and Gemini CLI calls it PreCompress; Codex allows a session-end hook
three seconds, which is plenty for one file read and one file write; the third is
PreToolUse in Claude Code, Codex, Qwen Code and Grok Build and BeforeTool in Gemini CLI, and guards
the projection, the record's read-only files at `/memory`: a read of an entry file
becomes a `get`, and a search over the projection is refused (PLAN.md, M4b). Gemini CLI
and Qwen Code refuse their file tools a path outside the workspace, so the launcher
includes the projection in theirs; otherwise the model would fall back to a shell the
hook does not see. Each runs the `memory` binary inside the
sandbox, which reads the event and the transcript's tail and leaves a note in an outbox in
the private home. When the run ends, the launcher files the notes as handoffs, marked as
left by the hook rather than the agent and pointing at the full transcript. A note from
before compaction is always filed, since what it holds has left the harness's context. A
note from session end is filed only when the run left no handoff of its own: the handoff
of last resort, which is also why a prompted run that forgot to write one no longer exits
4. Transcripts stay where the harness keeps them, under the private home in the record's
state, so nothing is lost even when no note is filed. The hook reads all three transcript
shapes: Claude Code's one turn a line, Codex's rollout items, Gemini's patch log.

## Mining a transcript: `--mine`

The third source of memory the harness did not write, after the hooks and the kept
transcript, and the only one that costs a model call. `--mine` on `launch` or `work`, or
`memory mine <transcript>`, asks a small model through the session's own destination to
read the transcript's tail and propose entries: facts, decisions, conventions, methods.
They land at raw tier with low confidence and the transcript as their source, for a person
to confirm or drop; nothing mined is trusted on arrival. The default model is a small one
per destination (`--mine-model` overrides): mining is a librarian's job, and the paper puts
librarians on the cheap class. Two routes, picked by `--mine-via auto`: on a subscription
login the call is a prompted, confined run of the harness itself on the small model, since
that credential belongs to the harness and may be used by nothing else, and the harness's
printed answer is the proposals; with a key or the gateway it is a direct call through the
egress, metered like any other. Proven here on a Max login: Claude Code on Haiku read a
session's transcript and filed two facts.

## The gateway inside the VM

`--backend wslc --auth gateway` runs LiteLLM over Postgres as two containers on their own
network inside the session VM, since the host's gateway on loopback is not reachable from
there, and the egress container joins that network. They are left running between launches
like the host stack; `wslc remove -f memory-litellm memory-postgres` takes them down. The
key the egress uses is `LITELLM_MASTER_KEY` from `local/.env`, which is right for a laptop
and wrong for the Foundation, where the front desk mints a virtual key per principal.

## The WSL containers C API

`wslcsdk.dll` is what Microsoft supports for production, and it does not ship with WSL: it
comes in the `Microsoft.WSL.Containers` NuGet package. The binding is in
`crates/memory/src/wslc_api.rs`, Windows only, loaded at run time, and `memory
wslc-api-check` proves it as far as creating and terminating a session. Running containers
through it is next, and needs the image provisioned into the API's own session store.

## In the harness image, on Linux: `make unpack`

The same image serves the Linux backend. `make unpack` exports it once from whichever
runtime holds it (`wslc` from a WSL distribution, otherwise podman or docker) into the
record's state as a plain directory, about twenty seconds for the two gigabytes, and from
then on `make launch` on Linux pivots into that directory instead of mounting this
machine's `/usr`, at no extra cost per run since it is only bind mounts. Unpack again when
the image changes: the image's harnesses, Node and `/etc`, read-only, with only the
private home, the working tree and the egress socket added. The `memory` binary inside, the
relay and the hooks, is the launcher's own, mounted over the image's copy, so a change to
the hooks is live on the next Linux launch without a rebuild; under WSL containers there is
no such mount and the image's copy runs, so there a hook change wants `make image` first.
A Linux run and a run under WSL containers are then the same environment. `--backend namespaces` goes back to the host's binaries; `--backend image`
insists on the image.

## In the harness image, under WSL containers: `--backend wslc`

On Windows with WSL 2.9.3 or later, or from inside a WSL distribution, the harness can run
in the harness image under WSL containers instead of from this machine's binaries: the same
harness versions and tools on every machine, in an image we build. `make image` builds it
from `images/harness/Dockerfile`; then

```sh
make launch BACKEND=wslc                      # claude in the image, your login
make launch BACKEND=wslc PROMPT='say pong'    # prompted
make work BACKEND=wslc                        # a task from the ledger
```

Two containers in the session VM. The egress container runs the Linux `memory` with your
record mounted in and the only route outside; it opens the memory, serves the egress on a
socket in a shared volume, and runs the launcher logic, task mode included. The harness
container has no network at all, its private home and the working tree mounted in, and
reaches the egress through the shared volume with the same relay as on Linux. The host side
only turns run requests into `wslc run` and reports the exit code. Your login directories
(`~/.claude`, `~/.codex`, `~/.gemini`) are mounted read-only into the egress container, which
copies the one file the harness needs into the private home; keys and `GH_TOKEN` are passed
as environment. The gateway mode is not wired through yet, since LiteLLM on this machine's
loopback is not reachable from the session VM.

`--unconfined` runs the same configuration without namespaces, with the egress on a TCP
port, for platforms without a backend. macOS gets Seatbelt later.

The rest of this page is the gateway half of the stack.

The local stack is the Foundation's shape at laptop size: LiteLLM as the gateway with
Postgres behind it, the memory server as a sidecar beside the harness, and Claude Code as
the harness. Your provider key sits in LiteLLM and nowhere else. The harness gets a
virtual key for model calls and a stdio connection to the memory server, and that is all
it gets.

```
  claude ──ANTHROPIC_BASE_URL──▶ litellm:4000 ──ANTHROPIC_API_KEY──▶ Anthropic
    │                               └── postgres (keys, spend)
    └──stdio──▶ memory serve --root $MEMORY_ROOT
```

## Once

Podman (or Docker) with compose, Rust stable, and Claude Code on your PATH.

```sh
cp local/.env.example local/.env      # fill in ANTHROPIC_API_KEY and MEMORY_ROOT
cargo build --release
make local-up                         # LiteLLM and Postgres, on 127.0.0.1:4000
make local-key                        # mints a virtual key into local/.virtual-key
make local-memory                     # memory init at MEMORY_ROOT, if not there yet
```

## Every time

```sh
scripts/claude-via-llmao.sh
```

Claude Code starts with every model call going through the gateway and the `memory`
server attached. Ask it to load T0, or let it discover the `t0` tool from the server's
instructions. When you stop, ask it to write a handoff; the next session finds it.

`make local-down` stops the containers and keeps the Postgres volume. `make local-logs`
tails LiteLLM.

## What this is not

Not the asfquart front desk. At the Foundation, llmao issues a PAT against ASF identity
and maps projects to LiteLLM teams; here the master key mints one key and you are the only
principal. The gateway, the sidecar, and the harness are the same pieces in the same
places, which is the point.

## Checks

```sh
curl -s http://127.0.0.1:4000/health/liveliness
curl -s http://127.0.0.1:4000/v1/models -H "Authorization: Bearer $(cat local/.virtual-key)"
```

The second lists what the key may call; with the wildcard route in `local/litellm.yaml` it
is empty, which is fine. A model id LiteLLM cannot resolve returns a 400 with the id in the
message; add an explicit entry for it. To prove the route without spending anything, send
one request with a wrong provider key in `local/.env`: the error that comes back is
Anthropic's own "API key is invalid", relayed by the gateway.

On Podman with the CNI network backend, compose DNS between containers is off, so LiteLLM
shares Postgres's network namespace instead of resolving a service name. That is why the
gateway's port is published on the `postgres` service.
