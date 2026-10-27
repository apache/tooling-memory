# A reference stack for the memory design

Written 1 October 2026. The goal is to build the whole system on one machine, in containers, with the technologies that scale and keep latency low, so that the Foundation can run the system with only a replication of the same containers. After that, this document recommends a language for the parts that are ours.

This stack implements the design in [the paper](durable-memory-for-disposable-agents.md).

## 1. One rule: the domain is the partition key

Each stateful part scales on the same axis. Each part divides its data by domain:

- the record, by domain prefix;
- the event log, by domain partition;
- the index, by domain shard;
- the ledger, by domain schema;
- the analytics tables, by domain partition.

The compiled filter of a session names the domains that the session may read. Thus each hot-path query touches a limited set of shards. More domains give more parallelism, not more contention.

This rule also gives two results with no added work. First, the paper requires one materializer for each domain, so that the record has a single writer. A Kafka consumer group over domain partitions gives this property. Nobody writes a lock for it. Second, the paper handles a reorg as a registry change. A reorg touches no partition.

## 2. The stack

| Component | Technology | Why it scales, and the latency it gives | Local |
|---|---|---|---|
| Record | S3-compatible object store that holds Markdown objects with versioning. Apache OpenDAL gives access to it | Object stores scale by bucket and key. Versioning gives the lineage record (supersession, tombstones, erasure stubs) without a second structure. A GET takes a few ms locally. At scale, a cache serves it | MinIO, one container. Apache Ozone, for a store that is Apache from end to end |
| Event log | Apache Kafka, KRaft mode, one partition for each domain | An append takes some ms, with no lock. Consumer groups give one materializer, one indexer and one ledger projection for each domain. Each one scales by partition count | One broker container |
| Index | Apache Solr, shard key = domain | Lucene BM25 gives ranked text. The compiled filter becomes a query filter. Dense vectors are a derived field in the same engine. A search that the index serves takes single-digit ms. Shard by domain, so that the filter of a session hits one shard | One Solr container. OpenSearch is the non-Apache equivalent, if operations prefer it |
| Ledger and lease cache | PostgreSQL, one schema for each domain | A claim is one statement with row locking and `SKIP LOCKED`. It is atomic and takes a few ms. Domains are independent, so partitions and replicas scale by domain. A rebuild from the log is possible | One container |
| Policy engine | OPA as a sidecar, registry and grants as data | Partial evaluation compiles the session filter one time. Each later check is local. The filter goes into the Solr query, so Solr never scores a denied entry | One sidecar for each memory-server replica |
| Memory server | A stateless service. It has the MCP and HTTP surface, the compiled filter and T0 in a cache for each session, and the read and write checks. In a launch, it limits reads to the ceiling of the model | Replicas behind a load balancer. T0 is a prefix that the harness caches. Thus orientation needs zero tool calls | One container. Scale by count |
| Launcher and sandbox | Ours. Linux namespaces, WSL containers, or Seatbelt on macOS (not yet run on a Mac) confine each harness. The harness has no network other than the egress. `--unconfined` removes the sandbox and keeps the egress, for a platform without a sandbox backend. So far, it runs Claude Code and Codex | It runs where the harness runs, one for each session. Thus it scales with the people and agents that work, not with the stack | The `amem` binary |
| Egress | Ours. It is the only network of the confined harness. It serves the MCP of the memory server and forwards model calls. It checks the ceiling for each call. It also injects credentials, applies spend limits, meters, and records decision contexts. It checks a tunnelled call (Copilot, and some subscription logins) when the tunnel opens, and does not meter it. It also carries the calls of the launcher to hosted tool servers | One egress beside each confined session. It forwards to LiteLLM for models, and to tool servers for hosted taps. A loopback relay and a unix socket connect it to the harness | Part of `amem`. Under WSL, it has its own container, which also runs the memory server |
| Recall deputy, librarians, auditors | Agents on Gofannon, with the cheap tier through llmao | The platform already runs this way. Librarians are a consumer group on the log, as any other consumer group is | The existing Gofannon and LiteLLM containers |
| Taps | MCP servers, one for each tracker or list, each one with a ceiling at registration; as WASI components that the memory server hosts (section 4) | Contributed taps run in a sandbox, with exactly the network and filesystem that the grant allows | Containers, or components that the memory server loads |
| Telemetry and analytics | OpenTelemetry to Kafka. Apache Flink or Spark streams into Apache Iceberg tables on the same object store. Apache DataFusion or Trino queries them | Columnar, append-heavy, cross-corpus. Iceberg snapshots give time travel over telemetry and over the entry metadata table. Cost per task, co-retrieval mining, decay sweeps and lineage queries are in this part | An OTel collector that writes Parquet. DuckDB or DataFusion for queries. Flink when the volume justifies it |
| Interchange | Markdown bundle with a manifest. Apache Arrow for bulk metadata transport | Arrow is the columnar wire format that the analytics part already uses | Same |
| Identity | Authentik for the institution. None for a single person | It is in the serving layer. Thus the local stack runs with it off | Optional container |

No single row is the safety boundary. The sandbox gives the harness no network. The harness sees only these parts of the host, which the launch mounts:

- the binaries and configuration of the system, the harness, the `amem` binary and the projection of the record, all read-only;
- a few device nodes;
- a private home and its working tree;
- the socket of the egress.

The harness can reach only the egress. The egress carries the MCP of the memory server to the harness. It decides where each model call may go. It holds the credentials, unless the harness runs on its own subscription login. The memory server checks each read and each write. The tools run outside the sandbox: in the launcher, on a tool server or in the browser of the owner. They decide each request against the session label. The memory server, the egress and the tools enforce the grants. If a harness reaches the memory server without the launcher, the checks of the memory server still apply. But the calls that the harness makes itself lose the sandbox and the egress.

## 3. Latency on the hot path comes from what is never on it

- **Orientation** is a cached prefix. T0 and the policy digest arrive with the session, not as tool calls.
- **Search and get** never touch the log or the history of the record. Search is Lucene. Get is one object read.
- **Claim** is one Postgres statement.
- **Write** is one Kafka append. The fast lane for handoffs is a Solr soft commit, with a time of some seconds. That number is the freshness SLO.
- **Recall** is the only call with real marginal cost. For that reason, its cost is metered.
- **Policy** compiles one time for each session and goes into the query. Thus there is no post-filtering and no check for each hit.

## 4. Language: Rust, one core library compiled three ways

These are the parts that are ours:

- the memory server;
- the launcher with its sandbox;
- the egress;
- the materializer;
- the indexer;
- the ledger projection;
- taps;
- the personal server;
- the command line;
- a browser UI.

The recommendation is Rust, with a single core library. This library compiles to native for Windows and Linux, to WebAssembly for the browser, and to WASI for taps and skills in a sandbox.

**Why Rust fits this stack.** The Apache libraries in this stack are Rust-native. OpenDAL and DataFusion are written in Rust. Arrow has a first-class Rust implementation. The Iceberg Rust library is not yet mature, but it improves quickly. Tantivy gives embedded BM25 for the personal server, where Solr is too heavy. Solr itself is an HTTP client. Rust uses SQLite with FTS5 through rusqlite, Postgres through sqlx, and Kafka through the librdkafka bindings. There is an official Rust MCP SDK. Rust gives one static binary for each platform, with no runtime to install. Windows is a tier-one target. The cost is velocity. Services take longer to write than in Go, and async Rust takes time to learn.

**Where Go is better, and the one thing to check.** Go services are faster to write and simpler to operate. OPA is a Go program. Thus in Go, the policy engine embeds in the same process. This is important, because the compiled filter for each session depends on the partial evaluation of OPA. In Rust, regorus is an embeddable Rego evaluator that also compiles to wasm. But regorus does not do partial evaluation. Thus the institution keeps OPA as a sidecar, and the laptop uses regorus for its simpler grants file. If OPA in the same process is decisive, that is the argument for Go. It should not decide. The sidecar is one container, and partial evaluation compiles the filter one time for each session.

**Why not split.** Go for services and Rust for the core would work. But every component must agree on the entry schema, the bundle format, the digest manifests and the filter model. To keep them in one crate that compiles everywhere is worth more than the velocity of Go on the services.

**Gofannon stays Python.** It communicates with all of this over MCP and HTTP. It can do this because the surface is the contract.

## 5. WebAssembly, split into three uses

| Use | Does it work | What it buys |
|---|---|---|
| The memory server in a browser tab | No. A tab cannot listen for connections. Thus no harness can reach it over MCP | Nothing as a server |
| A personal reader and curation UI in the browser | Yes. SQLite with FTS5, compiled to wasm, over the origin-private filesystem is proven. An exported bundle is files. Thus you load it and search it | Distribution with a URL. Open a page, load your bundle, read and search it, and admit or refuse proposals. No install is necessary |
| Taps and skills as WASI components | Yes. This is the strongest case. The memory server hosts one component for each tap, and grants capabilities explicitly | Taps that projects contribute run in a sandbox, with exactly the access that the grant allows. A tap with a ceiling and a wasm component with a capability set are the same idea. The runtime enforces it, not trust |

Thus the browser gives a person a view of their own memory and of the public slice of the Foundation. The browser is never the server. The memory server is a native binary on the laptop, or a container in the stack. The boundary of the taps and skills is the place where wasm gives value.

## 6. Two deployments from one codebase

**The Foundation's reference deployment** is the compose file above. It has these parts:

- MinIO;
- Kafka;
- Solr;
- Postgres;
- OPA;
- the memory server;
- Gofannon;
- LiteLLM;
- the OTel collector;
- optionally, Authentik.

The launcher and the egress are not in it. They run where each harness runs, and they connect to the stack as clients. Today the memory server runs in the same process as the egress. This is the process of the launcher, or under WSL, the process of the egress container. The egress reads the session from it. A split of the memory server from the egress will need the session label and the grants carried across that boundary. Replication of it means more brokers, shards, schemas and replicas along the domain key.

**The committer's laptop** is one process from the same crates. It has:

- files or a local object store for the record;
- SQLite with FTS5 for the index and the ledger;
- regorus over a grants file;
- no broker;
- no identity.

It has the same surface and the same bundle format. It is the conformance check for the surface. A person can also run it before their institution builds anything.


## 7. Shape of the codebase

One Rust workspace.

| Crate | Holds | Compiles to |
|---|---|---|
| `core` | Entries, frontmatter, bundles, digest manifests, the filter model, labels and ceilings. No I/O | native, wasm, WASI |
| `store` | The record over OpenDAL: versions, tombstones, erasure | native |
| `index` | Two backends: Tantivy embedded and Solr remote. The results give summaries first | native; Tantivy also wasm for the reader |
| `ledger` | Two backends: SQLite and Postgres. Claims, leases, origin dedup | native |
| `log` | Kafka producer and consumer groups. A file-backed log for the laptop | native |
| `policy` | OPA sidecar client. Regorus for the local grants file | native, wasm |
| `server` | MCP and HTTP surface, session cache, the read filter and write checks, T0 assembly | native |
| `sandbox` | Confinement: the plan, namespaces on Linux, Seatbelt on macOS. The WSL container backend is in `amem` (`crates/memory`) | native |
| `egress` | The one network path out of the confined harness: the MCP of the memory server over HTTP, and model calls with ceilings, credentials, spend and metering | native |
| `plugins` | WASI host for taps and skill bundles | native |
| `ui` | The browser reader and curation page | wasm |
| `memoryd`, `memory-local` | The two binaries | native, Windows and Linux |

## 8. Portability notes

- TCP on localhost, not Unix sockets, for all local connections. Thus Windows and Linux behave the same.
- The container stack runs in WSL2 on Windows. Development already happens there. The native binaries run on each side.
- Paths, line endings and file locking are the usual problems on Windows. The core crate is the one place that handles paths.
- Static binaries, with no system dependencies. Thus distribution is a download or a page.

## 9. What this does not decide

- The choice of Solr or OpenSearch is an operations preference. Both are Lucene.
- Kafka or Pulsar: the tiered storage of Pulsar to the object store is attractive for the old part of the log that is seldom read. But Kafka is the simpler start, with a single container.
- Whether Flink or Spark streams into Iceberg depends on what Infrastructure already runs. A local machine needs neither.
- Whether Apache Atlas becomes the home of the lineage record, if the Foundation contributes it to OMPI. For now, it is heavy. Later, it can be a good story.
