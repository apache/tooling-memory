# Brief shared by every research agent

Today is 19 October 2026. Your training data is stale for this space: verify every claim live (GitHub, docs, releases). Do not state a licence, a release date, or a capability you did not just read.

## The project
durable-memory (`amem`), a Rust workspace (read-only for you; do NOT edit, commit, or stash anything there). It launches an AI coding-agent *harness* (Claude Code, Codex CLI, Gemini CLI, Copilot CLI, OpenCode, Qwen Code, Grok, pi, Muse Code; mostly Node CLIs) confined, beside a per-session memory server. We rolled our own confinement and want to know what existing permissively licensed work we could leverage instead.

## Our contract (what any replacement must hold)
1. The harness has NO network. Its only route out is one unix socket to our egress (bind-mounted in), relayed by our own `amem relay` to a loopback TCP port inside the box. The egress is the only process with the network.
2. Filesystem inside: system binaries read-only, a private home read-write, the working tree read-write, our memory binary read-only, a read-only projection of the record, and nothing else from the host. Linux uses fixed paths inside: /home/user, /work, /opt/memory/memory, /run/memory/egress.sock, /memory.
3. Linux backend today: user+mount+net+uts+ipc+pid namespaces, unprivileged, no root, no daemon, no bubblewrap, no container runtime (crates/sandbox/src/linux.rs, ~370 lines, plus a large plan/adapter lib.rs).
4. macOS backend: Seatbelt (`sandbox-exec`) profile generated from the plan, dry-coded (crates/sandbox/src/seatbelt.rs).
5. Windows backend: WSL containers (`wslc`, GA Sept 2026): an egress container with the network, a harness container with `--network none`, sharing a volume that carries the unix socket. Image built from images/harness/Dockerfile (ubuntu:24.04 + Node + the harnesses at pinned versions), pinned by digest. A hand-written binding to the wslc C API lives in crates/wslc-api.
6. Egress (crates/egress): an axum HTTP server on the socket: `/mcp` (memory over MCP), `/<destination>/...` forwarding proxy per model provider with credential injection, a label-vs-ceiling check before anything leaves, token usage metered from every response; a CONNECT tunnel to an allowlist for a harness with no base-URL override (Copilot).
7. Code rules: `unsafe` only in crates/sandbox and crates/wslc-api. Nothing engine-specific outside backends.

Read crates/sandbox/src/lib.rs (top ~120 lines), crates/sandbox/src/linux.rs, images/harness/, and crates/egress/src/lib.rs (top) if you need to check how we do something. PLAN.md rows M5, M6, M8 describe the backends.

## Licence rule
In bounds: Apache-2.0 or less restrictive (MIT, BSD-2/3, ISC, Zlib, BSL-1.0, Unlicense, CC0): ASF Category A. Note but rank out: MPL-2.0, EPL (Category B, weak copyleft). Out: GPL, LGPL, AGPL, SSPL, BUSL/BSL-1.1, Elastic, "fair source", open-core where the part we'd need is proprietary.
Verify each licence from the repository itself: `gh api repos/OWNER/REPO/license --jq '.license.spdx_id'` and, where that says NOASSERTION or the repo is a monorepo with mixed licences, read the LICENSE file(s) and the relevant subdirectory/crate's Cargo.toml/package.json. Record the command and result. Also note maintenance: `gh api repos/OWNER/REPO --jq '{pushed_at,archived,stargazers_count}'` and latest release/tag date.

## Output
Write your findings as Markdown, one section per candidate with exactly these fields:
- **Repo / licence (verified)**: URL; SPDX id; how verified
- **Form**: library (language) / single binary / daemon / hosted service; privileges needed
- **Platforms**: Linux / macOS / Windows, each yes/no/partial
- **Mechanism**: one or two sentences
- **Against our contract**: points 1-6 above that it holds, breaks, or is silent on. Be specific about point 1 (can it give no network except one unix socket we bind in?) and point 2 (can it lay out arbitrary bind mounts at fixed paths?)
- **Maintenance**: last push, latest release, archived?
- **If we adopted it**: what of ours it replaces, what it adds (deps, daemons, attack surface)
- **Verdict**: adopt / borrow (copy an idea or code, with attribution) / learn-from / reject, with one-line reason

End with: a ranked shortlist (top 3-5) for this layer, and for each of the top 2 a concrete spike: the exact commands to install it (no root; into a directory you name, and say whether it needs /dev/kvm, podman, or Node) and what to run to test contract points 1 and 2 on Linux. The machine is Linux on WSL2 kernel 6.18 with podman and /dev/kvm present, no Docker, no Node.

Keep it factual and terse. No emojis. Your final message back should be a ten-line summary and the output path.
