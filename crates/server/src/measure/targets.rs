//! The targets that the paper publishes (section 9 and the appendix), and what the logs
//! measure against them. The read log holds the time of each search, so the two targets
//! of the search latency are measured. Every other target is published with the reason
//! that no log here answers it.

use super::table::when;
use crate::{io, reads, Error, Memory};
use serde::Serialize;
use time::Duration;

/// One published target, and what is measured against it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Target {
    /// What the target is of.
    pub name: String,
    /// The target, in the paper's words.
    pub target: String,
    /// What the logs measure, when they do.
    pub measured: Option<String>,
    /// Whether the measured value meets the target.
    pub met: Option<bool>,
    /// Why nothing is measured.
    pub why_not: Option<String>,
}

/// The paper's targets: the name, the target, and why a table without a value has none.
pub const TARGETS: [(&str, &str, &str); 10] = [
    (
        "Search latency, median",
        "100 ms",
        "no timed search in the window",
    ),
    (
        "Search latency, 99th percentile",
        "500 ms",
        "no timed search in the window",
    ),
    (
        "Handoffs and mid-task decisions searchable",
        "99% within 60 s",
        "no log holds the time at which an entry became searchable",
    ),
    (
        "Known-item lookup by id, title or error string",
        "in the top three",
        "amem golden reports the rank of a known item; this table does not read it",
    ),
    (
        "claim_next",
        "p99 1 s",
        "no log holds the time of a claim call",
    ),
    (
        "Projection lag",
        "p99 1 s",
        "no log holds the lag of the projection",
    ),
    (
        "Ordinary admission",
        "within 10 s",
        "the moments of the ledger hold no admission",
    ),
    (
        "Tap to task, and result back",
        "within 5 min",
        "no log holds the time of the event at the tracker",
    ),
    (
        "Triage cycle",
        "5 min",
        "no log holds the times of a triage cycle",
    ),
    (
        "Heartbeat interval",
        "a third of the lease",
        "the moments of the ledger hold no heartbeat",
    ),
];

/// The targets that the read log measures, in the order of [`TARGETS`]: the percentile
/// of the search latency, and the milliseconds that it must not pass.
const SEARCH: [(usize, u64); 2] = [(50, 100), (99, 500)];

/// The nearest-rank percentile of sorted values.
fn percentile(sorted: &[u64], p: usize) -> Option<u64> {
    let rank = (p * sorted.len()).div_ceil(100);
    sorted.get(rank.checked_sub(1)?).copied()
}

impl Memory {
    /// The paper's targets, each with what the logs of the last `days` days measure
    /// against it. The median and the 99th percentile of the search latency come from
    /// the time on each search of the read log. A read without a time is left out.
    ///
    /// # Errors
    ///
    /// A read log that is there and cannot be read.
    pub async fn targets(&self, days: u32) -> Result<Vec<Target>, Error> {
        let since = self.ledger.lock().await.now() - Duration::days(i64::from(days));
        let reads = reads::all(&self.root).map_err(io(&reads::path(&self.root)))?;
        let searches = reads.iter().filter(|r| r.tool == "search");
        let timed = searches.filter(|r| when(&r.at).is_some_and(|at| at >= since));
        let mut ms: Vec<u64> = timed.filter_map(|r| r.ms).collect();
        ms.sort_unstable();
        let target = |(i, (name, target, why)): (usize, &(&str, &str, &str))| {
            let limit = SEARCH.get(i);
            let got = limit.and_then(|(p, _)| percentile(&ms, *p));
            Target {
                name: (*name).to_string(),
                target: (*target).to_string(),
                measured: got.map(|v| format!("{v} ms")),
                met: got.zip(limit).map(|(v, (_, most))| v <= *most),
                why_not: got.is_none().then(|| (*why).to_string()),
            }
        };
        Ok(TARGETS.iter().enumerate().map(target).collect())
    }
}
