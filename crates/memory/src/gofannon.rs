//! `amem gofannon`: the bridge to Gofannon's deployed agents, from the ledger's side.
//!
//! The agents are a tool on the memory server, `gofannon`, a WASI tap like fetch and
//! search: every request decided by the policy at the session's label, what comes back
//! scanned and kept as a source. Here the same tool works the ledger: `amem gofannon
//! work <agent>` claims a task, starts the agent on it, keeps the lease while the run
//! goes, and files what the run returned as the task's handoff, completing the task, or
//! releases it with why when the run fails. `amem gofannon agents` lists what is
//! deployed.
//!
//! What Gofannon does with the input is Gofannon's: its model calls go through its own
//! gateway credential and are not metered by this egress, and its agents run as Python
//! in its API process. The grant is the boundary here: the hosts, the methods, the
//! ceiling, and the cookie, which the host puts on the request and the tap never holds.

use crate::Fallible;
use clap::Subcommand;
use dm_client::MemoryClient;
use dm_server::Memory;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// The tool's name in the grants.
const TOOL: &str = "gofannon";

/// What `amem gofannon` does.
#[derive(Subcommand)]
pub enum GofannonOp {
    /// The agents Gofannon has deployed, with their input schemas.
    Agents,
    /// Claim tasks and run a deployed agent on each, filing its result as the handoff.
    Work {
        /// The deployed agent, by its friendly name.
        agent: String,
        /// The field of the agent's input the task goes in.
        #[arg(long, default_value = "prompt")]
        field: String,
        /// Claim from this domain only.
        #[arg(long)]
        domain: Option<String>,
        /// Work this task rather than the next one, and stop after it.
        #[arg(long)]
        task: Option<String>,
        /// The lease, renewed while the run goes.
        #[arg(long, default_value_t = 1800)]
        lease_seconds: i64,
        /// Keep claiming until the queue is empty.
        #[arg(long)]
        keep_going: bool,
        /// How often the run is asked after, in seconds.
        #[arg(long, default_value_t = 10)]
        poll_seconds: u64,
    },
}

/// How `work` runs.
pub struct Plan {
    /// The agent.
    pub agent: String,
    /// The input field the task goes in.
    pub field: String,
    /// The domain to claim from.
    pub domain: Option<String>,
    /// One task, by id.
    pub task: Option<String>,
    /// The lease.
    pub lease_seconds: i64,
    /// Until the queue is empty.
    pub keep_going: bool,
    /// Between asking after the run.
    pub poll: Duration,
}

/// Run `amem gofannon`.
///
/// # Errors
///
/// No gofannon tool granted, the tool's refusal, or the ledger's.
pub async fn cmd(root: &Path, op: GofannonOp) -> Fallible {
    let m = Arc::new(Memory::open(root)?.with_author("agent:gofannon"));
    m.set_tools(crate::taps::TapTools::for_root(root));
    match op {
        GofannonOp::Agents => crate::json(&m.use_tool(TOOL, json!({"op": "agents"})).await?),
        GofannonOp::Work {
            agent,
            field,
            domain,
            task,
            lease_seconds,
            keep_going,
            poll_seconds,
        } => {
            let plan = Plan {
                agent,
                field,
                domain,
                task,
                lease_seconds,
                keep_going,
                poll: Duration::from_secs(poll_seconds.max(1)),
            };
            let done = work(&m, &plan).await?;
            eprintln!("memory: gofannon worked {done} tasks");
            Ok(())
        }
    }
}

/// Claim and run until there is nothing to claim, or once; how many tasks were worked.
///
/// # Errors
///
/// The ledger's refusal, or a write the server refuses.
pub async fn work(m: &Arc<Memory>, plan: &Plan) -> Result<usize, Box<dyn std::error::Error>> {
    let client = Arc::new(crate::work::client(m.clone()).await?);
    let mut n = 0;
    loop {
        let claimed = match &plan.task {
            Some(id) => client.claim(id, plan.lease_seconds).await?,
            None => {
                client
                    .claim_next(plan.domain.as_deref(), plan.lease_seconds)
                    .await?
            }
        };
        let Some(task) = claimed else {
            return Ok(n);
        };
        eprintln!(
            "memory: claimed {}: {} for gofannon {}",
            task.id, task.title, plan.agent
        );
        m.set_held_task(&task.id);
        let heartbeat = keep_lease(client.clone(), task.id.clone(), plan.lease_seconds);
        let ran = run(m, plan, &task).await;
        heartbeat.abort();
        settle(&client, plan, &task, ran).await?;
        n += 1;
        if plan.task.is_some() || !plan.keep_going {
            return Ok(n);
        }
    }
}

fn keep_lease(client: Arc<MemoryClient>, id: String, lease: i64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let every = Duration::from_secs((lease / 3).max(5).unsigned_abs());
        let mut tick = tokio::time::interval(every);
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = client.heartbeat(&id, lease).await {
                eprintln!("memory: heartbeat on {id}: {e}");
                return;
            }
        }
    })
}

/// Start the agent on the task and ask after the run until it is done; the last status.
async fn run(m: &Arc<Memory>, plan: &Plan, task: &dm_client::Task) -> Result<Value, String> {
    let mut input = serde_json::Map::new();
    input.insert(
        plan.field.clone(),
        json!(format!("# {}\n\n{}", task.title, task.body.trim())),
    );
    input.insert("task_id".into(), json!(task.id));
    let started = m
        .use_tool(
            TOOL,
            json!({"op": "start", "agent": plan.agent, "input": input}),
        )
        .await
        .map_err(|e| e.to_string())?;
    let run_id = started["run_id"]
        .as_str()
        .ok_or("the start answered with no run id")?
        .to_string();
    loop {
        let now = m
            .use_tool(TOOL, json!({"op": "status", "run_id": run_id}))
            .await
            .map_err(|e| e.to_string())?;
        if now["status"].as_str() != Some("running") {
            return Ok(now);
        }
        tokio::time::sleep(plan.poll).await;
    }
}

/// File the run's outcome: a completed run's result as the handoff of a completed task,
/// anything else as the handoff of a released one.
async fn settle(
    client: &MemoryClient,
    plan: &Plan,
    task: &dm_client::Task,
    ran: Result<Value, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (done, what) = match &ran {
        Ok(v) if v["status"].as_str() == Some("completed") => (true, v.clone()),
        Ok(v) => (false, v.clone()),
        Err(e) => (false, json!({ "error": e })),
    };
    let source = what["cite_as"].as_str().unwrap_or_default().to_string();
    let body = format!(
        "# {title}\n\nGofannon agent `{agent}` {how} task {id}{run}. What it returned:\n\n```json\n{json}\n```\n{cite}",
        title = task.title,
        agent = plan.agent,
        how = if done { "completed" } else { "did not complete" },
        id = task.id,
        run = what["run_id"]
            .as_str()
            .map(|r| format!(" as run {r}"))
            .unwrap_or_default(),
        json = serde_json::to_string_pretty(&json!({
            "status": what.get("status"),
            "result": what.get("result"),
            "error": what.get("error"),
        }))?,
        cite = if source.is_empty() {
            String::new()
        } else {
            format!("\nSource: {source}\n")
        },
    );
    let title = if done {
        format!("Gofannon {}: {}", plan.agent, task.title)
    } else {
        format!("Task {} stopped in gofannon: {}", task.id, task.title)
    };
    let handoff = client
        .write_handoff(&title, &body, Some(&task.domain))
        .await?;
    if done {
        client.complete(&task.id, &handoff).await?;
        eprintln!("memory: {} completed, handoff {handoff}", task.id);
    } else {
        client.release(&task.id, Some(&handoff)).await?;
        eprintln!("memory: {} released, handoff {handoff}", task.id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dm_server::sources::{Answer, Tools};
    use std::sync::Mutex;

    /// A scripted Gofannon behind the tool: a run that is running once, then done, or a
    /// start that is refused.
    struct Scripted {
        calls: Mutex<Vec<Value>>,
        refuse: bool,
    }

    impl Tools for Scripted {
        fn call(
            &self,
            _m: Arc<Memory>,
            _tool: String,
            _config: String,
            args: String,
        ) -> Answer<'static> {
            let args: Value = serde_json::from_str(&args).unwrap();
            let n = {
                let mut c = self.calls.lock().unwrap();
                c.push(args.clone());
                c.len()
            };
            let refuse = self.refuse;
            Box::pin(async move {
                match args["op"].as_str() {
                    Some("start") if refuse => Err("127.0.0.1: not a host the gofannon tool is granted".into()),
                    Some("start") => Ok(json!({"run_id": "run-1"}).to_string()),
                    _ if n == 2 => Ok(json!({"run_id": "run-1", "status": "running"}).to_string()),
                    _ => Ok(json!({"run_id": "run-1", "status": "completed", "result": {"label": "bug"}}).to_string()),
                }
            })
        }
    }

    fn root(dir: &Path, refuse: bool) -> (Arc<Memory>, Arc<Scripted>) {
        Memory::init(dir, "human:test:me", "dom-me").unwrap();
        let path = dir.join(dm_server::GRANTS_FILE);
        let mut g = dm_policy::Grants::load(&path).unwrap();
        g.taps.insert(
            TOOL.into(),
            dm_policy::TapGrant {
                domain: "dom-me".into(),
                tool: true,
                fresh: true,
                ceiling: dm_core::Label::Public,
                ..dm_policy::TapGrant::default()
            },
        );
        std::fs::write(&path, g.render().unwrap()).unwrap();
        let m = Arc::new(Memory::open(dir).unwrap().with_author("agent:gofannon"));
        let s = Arc::new(Scripted {
            calls: Mutex::new(Vec::new()),
            refuse,
        });
        m.set_tools(s.clone());
        (m, s)
    }

    fn plan() -> Plan {
        Plan {
            agent: "triage".into(),
            field: "issue".into(),
            domain: None,
            task: None,
            lease_seconds: 60,
            keep_going: true,
            poll: Duration::from_millis(10),
        }
    }

    async fn filed(m: &Memory) -> String {
        m.file_own(dm_ledger::TaskSpec {
            domain: "dom-me".into(),
            title: "Import fails on step four".into(),
            body: "The fourth step times out.".into(),
            requirements: Vec::new(),
            depends_on: Vec::new(),
            origin: None,
            upstream_event: None,
            created_by: String::new(),
            classification: dm_core::Label::Public,
            priority: 0,
        })
        .await
        .unwrap()
        .task
        .id
    }

    #[tokio::test]
    async fn a_run_s_result_is_the_completed_task_s_handoff() {
        let dir = tempfile::tempdir().unwrap();
        let (m, s) = root(dir.path(), false);
        let owner = Memory::open(dir.path()).unwrap();
        let id = filed(&owner).await;
        assert_eq!(work(&m, &plan()).await.unwrap(), 1);
        let calls = s.calls.lock().unwrap().clone();
        assert_eq!(calls[0]["agent"], "triage");
        assert!(calls[0]["input"]["issue"]
            .as_str()
            .unwrap()
            .contains("fourth step"));
        assert_eq!(calls[0]["input"]["task_id"], id.as_str());
        assert_eq!(calls.len(), 3, "start, running, completed");
        let t = owner.get_task(&id).await.unwrap();
        assert_eq!(t.status, dm_ledger::Status::Completed);
        let h = owner.get(&t.handoff.unwrap()).await.unwrap();
        assert!(
            h.body.contains("\"label\": \"bug\"") && h.body.contains("run-1"),
            "{}",
            h.body
        );
    }

    #[tokio::test]
    async fn a_refused_start_releases_the_task_with_why() {
        let dir = tempfile::tempdir().unwrap();
        let (m, _) = root(dir.path(), true);
        let owner = Memory::open(dir.path()).unwrap();
        let id = filed(&owner).await;
        let mut p = plan();
        p.task = Some(id.clone());
        assert_eq!(work(&m, &p).await.unwrap(), 1);
        let t = owner.get_task(&id).await.unwrap();
        assert_eq!(t.status, dm_ledger::Status::Admitted);
        let h = owner.get(&t.handoff.unwrap()).await.unwrap();
        assert!(
            h.body.contains("not a host the gofannon tool is granted"),
            "{}",
            h.body
        );
    }
}
