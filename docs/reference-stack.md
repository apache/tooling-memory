# A reference stack for the memory design

Written 1 October 2026. The goal: build the whole system on one machine, in containers, using the technologies that scale and keep latency low, so that running it at the Foundation is a matter of replicating the same containers. Then: what language to write the parts that are ours.

The design this implements is [the paper](durable-memory-for-disposable-agents.md).

## 1. One rule: the domain is the partition key

Every stateful piece scales along the same axis. The record is partitioned by domain prefix, the event log by domain partition, the index by domain shard, the ledger by domain schema, the analytics tables by domain partition. A session's compiled filter names the domains it may read, so every hot-path query touches a bounded set of shards, and adding domains adds parallelism rather than contention.

Two things fall out of this for free. One materializer per domain, which the paper requires so the record has a single writer, is a property of a Kafka consumer group over domain partitions rather than a lock anyone writes. And a reorg, which the paper handles as a registry change, touches no partition.

## 2. The stack

| Component | Technology | Why it scales, and the latency it gives | Local |
|---|---|---|---|
| Record | S3-compatible object store holding Markdown objects with versioning, reached through Apache OpenDAL | Object stores scale by bucket and key. Versioning gives the lineage record (supersession, tombstones, erasure stubs) without a second structure. GET is a few ms locally, cached at scale | MinIO, one container. Apache Ozone for an Apache store end to end |
| Event log | Apache Kafka, KRaft mode, one partition per domain | Append is ms with no lock. Consumer groups give one materializer, one indexer, one ledger projection per domain, each scaling by partition count | One broker container |
| Index | Apache Solr, shard key = domain | Lucene BM25 for ranked text, the compiled filter expressed as a query filter, dense vectors as a derived field in the same engine. Index-served searches in single-digit ms. Shard by domain so a session's filter hits one shard | One Solr container. OpenSearch is the non-Apache equivalent if operations prefer it |
| Ledger and lease cache | PostgreSQL, one schema per domain | A claim is one statement with row locking and `SKIP LOCKED`: atomic, a few ms. Domains are independent, so partitions and replicas scale by domain. Rebuildable from the log | One container |
| Policy engine | OPA as a sidecar, registry and grants as data | Partial evaluation compiles the session filter once; every later check is local. The filter is pushed into the Solr query, so denied entries are never scored | One sidecar per memory-server replica |
| Memory server | A stateless service: MCP and HTTP surface, compiled filter and T0 cached per session, the read and write checks | Replicas behind a load balancer. T0 is a prefix the harness caches, so orientation costs zero tool calls | One container, scale by count |
| Launcher and sandbox | Ours: each harness confined with Linux namespaces, WSL containers, or Seatbelt on macOS, with no network but the egress | Runs where the harness runs, one per session, so it scales with the people and agents working rather than with the stack | The `amem` binary |
| Egress | Ours: a forwarding proxy, the confined harness's only network. Ceiling check per outbound call, credentials injected, spend limits, metering, decision contexts recorded | One beside each confined session, forwarding to LiteLLM for models and to tool servers for taps; one local hop on each call | Part of `amem`; its own container under WSL |
| Recall deputy, librarians, auditors | Agents on Gofannon, cheap tier through llmao | Already how the platform runs. Librarians are a consumer group on the log like any other | The existing Gofannon and LiteLLM containers |
| Taps | MCP servers, one per tracker or list, each registered with a ceiling; as WASI components hosted by the memory server (section 4) | Contributed connectors run sandboxed with exactly the network and filesystem the grant allows | Containers, or components loaded by the server |
| Telemetry and analytics | OpenTelemetry to Kafka; Apache Flink or Spark streaming into Apache Iceberg tables on the same object store; queried with Apache DataFusion or Trino | Columnar, append-heavy, cross-corpus. Iceberg snapshots give time travel over telemetry and over the entry metadata table. This is where cost per task, co-retrieval mining, decay sweeps, and lineage queries live | OTel collector writing Parquet; DuckDB or DataFusion to query. Flink when volume justifies it |
| Interchange | Markdown bundle with a manifest; Apache Arrow for bulk metadata transport | Arrow is the columnar wire format the analytics side already speaks | Same |
| Identity | Authentik for the institution; nobody for a single person | Lives in the serving layer, so the local stack runs with it off | Optional container |

No one row is the safety boundary. The memory server checks what is read and written; the sandbox keeps the harness off the network and the host; the egress decides where each call may go and holds the credentials; the memory server and the egress enforce the same grants. A harness that reaches the memory server without the launcher keeps the first and none of the rest.

## 3. Latency on the hot path comes from what is never on it

- **Orientation** is a cached prefix: T0 and the policy digest arrive with the session, not as tool calls.
- **Search and get** never touch the log or the record's history. Search is Lucene; get is one object read.
- **Claim** is one Postgres statement.
- **Write** is one Kafka append. The handoff fast lane is a Solr soft commit, measured in seconds, and that number is the freshness SLO.
- **Recall** is the only call with real marginal cost, and it is metered for that reason.
- **Policy** is compiled once per session and pushed into the query, so there is no post-filtering and no per-hit check.

## 4. Language: Rust, one core library compiled three ways

The parts that are ours: the memory server, the launcher with its sandbox, the egress, the materializer, the indexer, the ledger projection, taps, the personal server, the CLI, and a browser UI. The recommendation is Rust, with a single core library that compiles to native for Windows and Linux, to WebAssembly for the browser, and to WASI for sandboxed plugins.

**Why Rust fits this stack.** The Apache libraries involved are Rust-native: OpenDAL is written in Rust, so is DataFusion, Arrow has a first-class Rust implementation, and the Iceberg Rust library is maturing quickly. Tantivy gives embedded BM25 for the personal server, where Solr is too heavy; Solr itself is an HTTP client. SQLite with FTS5 through rusqlite, Postgres through sqlx, Kafka through the librdkafka bindings. There is an official Rust MCP SDK. One static binary per platform, no runtime to install, Windows a tier-one target. The cost is velocity: services take longer to write than in Go, and async Rust has a learning curve.

**Where Go wins, and the one thing to check.** Go services are faster to write and simpler to operate. OPA is a Go program, so in Go the policy engine embeds in-process, which matters because the compiled per-session filter depends on OPA's partial evaluation. In Rust, regorus is an embeddable Rego evaluator that also compiles to wasm, but it does not do partial evaluation, so the institution keeps OPA as a sidecar and the laptop uses regorus for its simpler grants file. If embedding OPA is decisive, that is the argument for Go. It should not decide: the sidecar is one container and the filter is compiled once per session.

**Why not split.** Go for services and Rust for the core would be workable, but the entry schema, bundle format, digest manifests, and filter model are the things every component must agree on, and keeping them in one crate that compiles everywhere is worth more than Go's velocity on the services.

**Gofannon stays Python.** It talks to all of this over MCP and HTTP, which is the point of the surface being the contract.

## 5. WebAssembly, split into three uses

| Use | Does it work | What it buys |
|---|---|---|
| The memory server in a browser tab | No. A tab cannot listen for connections, so no harness can reach it over MCP | Nothing as a server |
| A personal reader and curation UI in the browser | Yes. SQLite with FTS5 compiled to wasm over the origin-private filesystem is proven; an exported bundle is files, so drop it in and search it | Distribution with a URL: open a page, load your bundle, read and search it, admit or refuse proposals, no install |
| Taps and skills as WASI components | Yes, and it is the strongest case. The server hosts a component per connector and grants capabilities explicitly | Contributed connectors from projects run sandboxed with exactly the access the grant allows. A tap with a ceiling and a wasm component with a capability set are the same idea, enforced by the runtime rather than by trust |

So the browser is the person's window onto their own memory and the Foundation's public slice, never the server. The server is a native binary on the laptop or a container in the stack. The plugin boundary is where wasm earns its keep.

## 6. Two deployments from one codebase

**The Foundation's reference deployment** is the compose file above: MinIO, Kafka, Solr, Postgres, OPA, the memory server, Gofannon, LiteLLM, the OTel collector, optionally Authentik. The launcher and the egress are not in it: they run where each harness runs, and reach the stack as clients. Replicating it is adding brokers, shards, schemas, and replicas along the domain key.

**The committer's laptop** is one process from the same crates: files or a local object store for the record, SQLite with FTS5 for index and ledger, regorus over a grants file, no broker, no identity. Same surface, same bundle format. It is the conformance check for the surface and the thing a person can run before their institution has built anything.


## 7. Shape of the codebase

One Rust workspace.

| Crate | Holds | Compiles to |
|---|---|---|
| `core` | Entries, frontmatter, bundles, digest manifests, the filter model, labels and ceilings. No I/O | native, wasm, WASI |
| `store` | The record over OpenDAL: versions, tombstones, erasure | native |
| `index` | Two backends: Tantivy embedded, Solr remote. Summaries-first results | native; Tantivy also wasm for the reader |
| `ledger` | Two backends: SQLite, Postgres. Claims, leases, origin dedup | native |
| `log` | Kafka producer and consumer groups; a file-backed log for the laptop | native |
| `policy` | OPA sidecar client; regorus for the local grants file | native, wasm |
| `server` | MCP and HTTP surface, session cache, ceiling checks, T0 assembly | native |
| `sandbox` | Confinement: the plan, namespaces on Linux, Seatbelt on macOS | native |
| `egress` | The confined harness's one way out: ceilings, credentials, spend, metering | native |
| `plugins` | WASI host for taps and skill bundles | native |
| `ui` | The browser reader and curation page | wasm |
| `memoryd`, `memory-local` | The two binaries | native, Windows and Linux |

## 8. Portability notes

- TCP on localhost rather than Unix sockets for anything local, so Windows and Linux behave the same.
- The container stack runs in WSL2 on Windows, which is where development already happens; the native binaries run on either side.
- Paths, line endings, and file locking are the usual Windows traps; the core crate owns path handling once.
- Static binaries, no system dependencies, so distribution is a download or a page.

## 9. What this does not decide

- Solr against OpenSearch is an operations preference; both are Lucene.
- Kafka against Pulsar: Pulsar's tiered storage to the object store is attractive for the log's cold tail, but Kafka is the simpler single-container start.
- Whether Flink or Spark streams into Iceberg depends on what Infrastructure already runs. Locally neither is needed.
- Whether Apache Atlas becomes the home of the lineage record if the Foundation contributes it to OMPI. Heavy for now; a good story later.
