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
make launch                                   # claude, your own login, confined
make launch HARNESS=codex                     # codex, your ChatGPT login
make launch HARNESS=codex AUTH=key            # codex, OPENAI_API_KEY from local/.env
make launch HARNESS=gemini                    # gemini, your Google login
make launch HARNESS=gemini AUTH=key           # gemini, GEMINI_API_KEY
make launch HARNESS=copilot                   # copilot, your GitHub login (GH_TOKEN)
make launch HARNESS=opencode AUTH=key         # opencode, ANTHROPIC_API_KEY
make launch HARNESS=opencode AUTH=key PROVIDER=xai   # the same harness, Grok behind it
make launch AUTH=gateway                      # claude through the local LiteLLM
make launch ARGS='-p "load T0 and summarise"' # arguments go to the harness
make launch PROMPT='load T0 and summarise'    # non-interactive, in the harness's headless form
make work                                     # claim the next task from the ledger and run it
make work LOOP=1                              # keep claiming until interrupted
make usage                                    # tokens by destination and model
```

Three ways to run, one launcher. Interactive is the harness as you know it. `PROMPT` runs the
harness's headless form, prints its answer, and exits with its code; a run that leaves no
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

Five harnesses, one launcher. Each needed the same three things, and the table is the whole
of what differs:

| Harness | Model calls reach the egress by | Memory over MCP by | `AUTH` modes |
|---|---|---|---|
| Claude Code | `ANTHROPIC_BASE_URL` | `--mcp-config` | subscription, key, gateway |
| Codex | `model_providers.<id>.base_url` in `config.toml`; with a ChatGPT login, `chatgpt_base_url` | `mcp_servers.<id>.url` | subscription, key, gateway; `--provider xai` for Grok |
| Gemini CLI | `GOOGLE_GEMINI_BASE_URL`; with a Google login, `CODE_ASSIST_ENDPOINT` | `mcpServers.<id>.httpUrl` | subscription, key |
| Copilot CLI | no override exists; `HTTPS_PROXY` to the egress, which tunnels only to GitHub's Copilot hosts | `mcp-config.json` | subscription (`GH_TOKEN`) |
| OpenCode | `provider.<id>.options.baseURL` | `mcp.<id>.url` | key; `--provider openai` or `xai` |

Copilot is the odd one: with no base-URL override, its HTTPS goes through the egress as a
CONNECT tunnel to an allowlist of hosts. That is pinned egress without metering: TLS runs
end to end, so the egress sees no tokens and checks the ceiling when the tunnel opens.
Grok is not a harness but a destination: any OpenAI-shaped harness reaches it with
`--provider xai` and `XAI_API_KEY`.

| `AUTH` | Model calls go to | Credential | Who holds it |
|---|---|---|---|
| `subscription` | the provider | the harness's own login: Claude Code's credentials file, Codex's `auth.json`, Gemini's `oauth_creds.json`, Copilot's `GH_TOKEN` | the harness, inside its private home or environment |
| `key` | the provider | `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY` or `XAI_API_KEY` from `local/.env` | the egress; the harness gets a placeholder |
| `gateway` | LiteLLM on 127.0.0.1:4000 | `local/.virtual-key` | the egress; the harness gets a placeholder |

A login is made once on the host, in the harness's own way, and the launcher copies that
one file into the private home. For Claude Code, `claude` and its login prompt; for Codex,
`codex login` or, with no browser to hand, `codex login --device-auth`; for Gemini,
`gemini` and `/auth`, Login with Google. Nothing else under your home is read or mounted.
The ChatGPT and Google logins talk to backends that are not the public API, the ChatGPT
backend and Code Assist, so those two get their own destinations, `chatgpt` and `google`,
forwarded and metered like the rest; only the token refresh goes through the egress as a
tunnel to the login host.

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
have, one entry per section, and run a task over it:

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
starts one: harness, auth, an optional prompt, or the next task from the ledger. Each
session is `memory launch` (or `memory work`) in a pseudo-terminal the server keeps
alive whether or not a browser is attached, with the same confinement as from a shell.
Its output goes into a two-megabyte ring on the server and to every attached browser, so
a refresh, a dropped connection or a second browser replays the scrollback and lands in
the same place; resizes reach the harness. Beside the terminal a reading pane renders
Markdown, Mermaid included, for entries from the record: whatever the session writes
opens as it lands, a `mem-…` id in the terminal is a link, and the recent entries are
listed under it. Loopback only; reach it over SSH when remote. The page loads xterm.js,
marked and mermaid from a CDN in this first cut, so the browser needs the internet even
though the harness does not.

## Handoffs the harness did not write

The launcher installs two hooks in Claude Code's settings in the private home: before
context is compacted, and at session end. Each runs the `memory` binary inside the
sandbox, which reads the event and the transcript's tail and leaves a note in an outbox in
the private home. When the run ends, the launcher files the notes as handoffs, marked as
left by the hook rather than the agent and pointing at the full transcript. A note from
before compaction is always filed, since what it holds has left the harness's context. A
note from session end is filed only when the run left no handoff of its own: the handoff
of last resort, which is also why a prompted run that forgot to write one no longer exits
4. Transcripts stay where the harness keeps them, under the private home in the record's
state, so nothing is lost even when no note is filed. Codex and Gemini CLI expose the same
moments; their adapters are next.

## In the harness image, on Linux: `make unpack`

The same image serves the Linux backend. `make unpack` exports it once from whichever
runtime holds it (`wslc` from a WSL distribution, otherwise podman or docker) into the
record's state as a plain directory, about twenty seconds for the two gigabytes, and from
then on `make launch` on Linux pivots into that directory instead of mounting this
machine's `/usr`, at no extra cost per run since it is only bind mounts. Unpack again when
the image changes: the image's harnesses, Node, `/etc`
and `memory` binary, read-only, with only the private home, the working tree and the
egress socket added. A Linux run and a run under WSL containers are then the same
environment. `--backend namespaces` goes back to the host's binaries; `--backend image`
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
