# Research behind docs/harness-containers.md

The reports the survey was written from, one per layer, each produced by a research agent
working to `brief.md` on 19 and 20 October 2026. They carry the per-candidate detail and the
receipts (the `gh api` call behind each licence, the file and line behind each capability)
that the doc only summarises. They are a record of what was read then, not maintained:
licences, versions and stars move.

| Report | Layer |
|---|---|
| `confinement-embeddable.md` | libraries and single binaries in place of `crates/sandbox` |
| `runtimes-external.md` | runtimes, hypervisors and VMMs as the boundary |
| `image-and-egress.md` | the harness image, and the egress beside the box |
| `platforms.md` | whole agent-sandbox platforms |
| `substrate-gaps-metering.md` | Agent Substrate, what the four missed, and who meters tokens (the full matrix with evidence per cell) |

`$SCRATCH` stands for the session's scratch directory, where the agents kept downloads and
probe scripts; it is gone. Where a report says a spike was run, the run that counts is the
one in the directory above, against `contract-check.py`.
