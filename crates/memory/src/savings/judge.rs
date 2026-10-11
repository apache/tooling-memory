//! The paper's judge of "used": a cheap model reads a sample of what sessions opened
//! beside the handoff each session left, and says whether the entry informed the work.
//!
//! The counts in [`super::metrics`] take "used" to be "cited by id in the handoff", which
//! needs no model and undercounts what a session used without naming it. The judge is
//! the paper's measure: optional, on a sample, through a destination the owner names,
//! and reported beside the citation rule with how often the two agree, so the cheap
//! number can be read against the judged one.

use super::metrics::{cites, Metrics};
use dm_server::recall::Deputy;
use dm_server::Memory;
use serde::Serialize;
use std::collections::BTreeMap;

/// The most of an entry's body the judge reads.
const ENTRY_CHARS: usize = 2_000;
/// The most of a session's handoffs the judge reads.
const HANDOFF_CHARS: usize = 6_000;

/// One opened entry, judged.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Judged {
    /// The session that opened it.
    pub session: String,
    /// The entry.
    pub entry: String,
    /// Whether the handoff cites it by id.
    pub cited: bool,
    /// The judge's verdict; none when its answer did not parse.
    pub used: Option<bool>,
    /// The judge's reason, in a sentence.
    pub why: String,
}

/// The judge's section of the report.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Judgement {
    /// The judge's model.
    pub model: String,
    /// Opened entries in sessions that left a handoff: what was sampled from.
    pub population: usize,
    /// How many were sampled.
    pub sampled: usize,
    /// Of those, how many the judge answered in a form that parsed.
    pub judged: usize,
    /// Of those, how many it judged used.
    pub used: usize,
    /// `judged - used` over `judged`: the paper's wasted-read ratio on the sample.
    pub wasted_read_ratio: Option<f64>,
    /// Of the judged, how many the citation rule agrees on.
    pub agrees_with_citation: usize,
    /// Each judged read.
    pub reads: Vec<Judged>,
}

/// Which opened entries to judge: every one in a session that left a handoff, ordered
/// by a digest of session and entry so the sample is the same on every run, and the
/// first `n` of them.
fn sample(metrics: &Metrics, n: usize) -> (usize, Vec<(String, String)>) {
    let mut all: Vec<(String, String, String)> = metrics
        .sessions
        .iter()
        .filter(|s| !s.handoffs.is_empty())
        .flat_map(|s| {
            s.opened.iter().map(|id| {
                let key = dm_core::digest(&format!("{}\n{id}", s.session));
                (key, s.session.clone(), id.clone())
            })
        })
        .collect();
    all.sort();
    let population = all.len();
    let picked = all
        .into_iter()
        .take(n)
        .map(|(_, session, id)| (session, id))
        .collect();
    (population, picked)
}

fn clip(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((at, _)) => &text[..at],
        None => text,
    }
}

/// The question put to the judge.
#[must_use]
pub fn prompt(title: &str, body: &str, handoff: &str) -> String {
    format!(
        "A coding agent opened the memory entry below during a session, and at the end \
         wrote the handoff below. Did the entry inform the work the handoff describes: a \
         fact, decision, step or warning from it used, followed or relied on, whether or \
         not the handoff names it? An entry merely opened and set aside was not used.\n\n\
         Answer with one JSON object and nothing else: \
         {{\"used\": true or false, \"why\": \"one sentence\"}}\n\n\
         <entry title=\"{title}\">\n{}\n</entry>\n\n<handoff>\n{}\n</handoff>\n",
        clip(body, ENTRY_CHARS),
        clip(handoff, HANDOFF_CHARS)
    )
}

/// The verdict in a judge's answer: the first JSON object in it with a boolean `used`.
#[must_use]
pub fn verdict(answer: &str) -> Option<(bool, String)> {
    let mut from = 0;
    while let Some(start) = answer[from..].find('{').map(|i| i + from) {
        let mut stream =
            serde_json::Deserializer::from_str(&answer[start..]).into_iter::<serde_json::Value>();
        if let Some(Ok(v)) = stream.next() {
            if let Some(used) = v.get("used").and_then(serde_json::Value::as_bool) {
                let why = v
                    .get("why")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                return Some((used, why));
            }
        }
        from = start + 1;
    }
    None
}

/// Judge a sample of `n` opened entries against the handoffs of the sessions that opened
/// them. `handoffs` holds each handoff's text by id, as the metrics read them. A model
/// call that fails is a read not judged, not an error: the report says how many were.
pub async fn judge(
    memory: &Memory,
    metrics: &Metrics,
    handoffs: &BTreeMap<String, String>,
    deputy: &dyn Deputy,
    n: usize,
) -> Judgement {
    let (population, picked) = sample(metrics, n);
    let mut j = Judgement {
        model: deputy.model(),
        population,
        sampled: picked.len(),
        ..Judgement::default()
    };
    for (session, id) in picked {
        let Some(s) = metrics.sessions.iter().find(|s| s.session == session) else {
            continue;
        };
        let texts: Vec<&str> = s
            .handoffs
            .iter()
            .filter_map(|h| handoffs.get(h).map(String::as_str))
            .collect();
        let handoff = texts.join("\n\n---\n\n");
        let cited = texts.iter().any(|t| cites(t, &id));
        let (title, body) = match memory.get(&id).await {
            Ok(e) => (e.title(), e.body),
            Err(_) => match memory.archived(&id).await {
                Ok(e) => (e.title(), e.body),
                Err(_) => continue,
            },
        };
        let answer = deputy.complete(&prompt(&title, &body, &handoff)).await;
        let (used, why) = match answer.as_deref().map(verdict) {
            Ok(Some((u, w))) => (Some(u), w),
            Ok(None) => (None, "the judge's answer did not parse".into()),
            Err(e) => (None, format!("the judge failed: {e}")),
        };
        if let Some(u) = used {
            j.judged += 1;
            j.used += usize::from(u);
            j.agrees_with_citation += usize::from(u == cited);
        }
        j.reads.push(Judged {
            session,
            entry: id,
            cited,
            used,
            why,
        });
    }
    j.wasted_read_ratio = (j.judged > 0).then(|| (j.judged - j.used) as f64 / j.judged as f64);
    j
}

/// The judge's lines in the text report.
#[must_use]
pub fn render(j: &Judgement) -> String {
    let mut s = format!(
        "\nJudged: {} of {} opened entries sampled, read by {} beside the handoff\n",
        j.sampled, j.population, j.model
    );
    match j.wasted_read_ratio {
        Some(r) => s.push_str(&format!(
            "  used {}/{}, wasted reads {:.0}%; the citation rule agrees on {}/{}\n",
            j.used,
            j.judged,
            r * 100.0,
            j.agrees_with_citation,
            j.judged
        )),
        None => s.push_str(
            "  nothing judged: no session with a handoff opened anything, or no answer parsed\n",
        ),
    }
    if j.judged < j.sampled {
        s.push_str(&format!(
            "  {} not judged: the call failed or the answer did not parse\n",
            j.sampled - j.judged
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;

    #[test]
    fn a_verdict_is_the_first_object_that_says_used() {
        assert_eq!(
            verdict("{\"used\": true, \"why\": \"step four\"}"),
            Some((true, "step four".into()))
        );
        assert_eq!(
            verdict("Sure.\n```json\n{\"note\": 1}\n{\"used\": false}\n```"),
            Some((false, String::new()))
        );
        assert_eq!(verdict("yes, it was used"), None);
        assert_eq!(
            verdict("{\"used\": \"yes\"}"),
            None,
            "a boolean, not a word"
        );
    }

    /// Says an entry was used when its title is named in the handoff the prompt carries.
    struct Scripted;

    impl Deputy for Scripted {
        fn model(&self) -> String {
            "scripted-judge".into()
        }

        fn complete<'a>(
            &'a self,
            prompt: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async move {
                let title = prompt
                    .split("<entry title=\"")
                    .nth(1)
                    .and_then(|r| r.split('"').next())
                    .unwrap_or_default();
                let handoff = prompt.split("<handoff>").nth(1).unwrap_or_default();
                if title == "Flaky" {
                    return Err("the judge timed out".into());
                }
                Ok(format!(
                    "{{\"used\": {}, \"why\": \"read the handoff\"}}",
                    handoff.contains(title)
                ))
            })
        }
    }

    #[tokio::test]
    async fn the_judge_reads_a_sample_beside_the_handoff_and_says_where_citation_disagrees() {
        let dir = tempfile::tempdir().unwrap();
        let m = Memory::init(dir.path(), "human:test:me", "dom-me")
            .unwrap()
            .with_author("agent:test");
        let mut ids = Vec::new();
        for title in ["Import steps", "Commit convention", "Flaky"] {
            let w = m
                .write(dm_server::WriteRequest {
                    body: format!("# {title}\n\nAbout {title}."),
                    ..dm_server::WriteRequest::default()
                })
                .await
                .unwrap();
            ids.push(w.id);
        }
        for id in &ids {
            m.get(id).await.unwrap();
        }
        // the handoff names one entry by id and relies on another by its title only
        m.write(dm_server::WriteRequest {
            body: format!(
                "# Stopped at step 4\n\nFollowed the Import steps; see {}.",
                ids[1]
            ),
            entry_type: dm_core::EntryType::Handoff,
            ..dm_server::WriteRequest::default()
        })
        .await
        .unwrap();
        drop(m);
        let judge = super::super::Judge {
            deputy: &Scripted,
            sample: 10,
        };
        let r = super::super::measure(dir.path(), Some("import"), Some(judge))
            .await
            .unwrap();
        let j = r.judged.unwrap();
        assert_eq!(j.model, "scripted-judge");
        assert_eq!((j.population, j.sampled), (3, 3));
        assert_eq!(j.judged, 2, "the flaky call is a read not judged: {j:?}");
        let import = j.reads.iter().find(|x| x.entry == ids[0]).unwrap();
        assert_eq!((import.cited, import.used), (false, Some(true)));
        let commit = j.reads.iter().find(|x| x.entry == ids[1]).unwrap();
        assert_eq!((commit.cited, commit.used), (true, Some(false)));
        assert_eq!(j.used, 1);
        assert_eq!(j.agrees_with_citation, 0);
        assert_eq!(j.wasted_read_ratio, Some(0.5));
        let text = render(&j);
        assert!(text.contains("used 1/2, wasted reads 50%"), "{text}");
        assert!(text.contains("1 not judged"), "{text}");
    }

    #[test]
    fn the_sample_is_the_same_every_time_and_only_from_sessions_with_a_handoff() {
        let session = |name: &str, handoffs: &[&str], opened: &[&str]| {
            super::super::metrics::SessionMetrics {
                session: name.into(),
                handoffs: handoffs.iter().map(|s| (*s).to_string()).collect(),
                opened: opened.iter().map(|s| (*s).to_string()).collect(),
                ..Default::default()
            }
        };
        let m = Metrics {
            sessions: vec![
                session("s1", &["h1"], &["a", "b", "c", "d"]),
                session("s2", &[], &["e", "f"]),
            ],
            ..Metrics::default()
        };
        let (population, first) = sample(&m, 2);
        assert_eq!(population, 4);
        assert_eq!(first.len(), 2);
        assert_eq!(sample(&m, 2).1, first);
        assert!(first.iter().all(|(s, _)| s == "s1"));
    }
}
