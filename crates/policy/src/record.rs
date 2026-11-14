//! Policy as memory: the rules live in the record as entries, layered as the paper
//! layers them, and the ones a machine can check carry a Rego module. An entry's body
//! holds a fenced `policy` block, TOML, with what the rule is and why, and optionally a
//! fenced `rego` block. A module is enforced only once its entry is verified, which is
//! the paper's evolution-loop rule that nothing below verified is loaded as live
//! configuration: an agent may propose a rule, and it is inert until a person who did
//! not propose it approves it. Exceptions are policy entries too, scoped to one task,
//! expiring, and in force once verified; an L0 rule takes none.
//!
//! ````text
//! ```policy
//! id = "pol-L2-031"
//! layer = 2
//! rule = "Decisions are superseded by a person, not an agent."
//! intent = "Incident: an agent rewrote a standing decision mid-task."
//! ```
//!
//! ```rego
//! package amem.rules
//! import rego.v1
//! deny contains {"policy": "pol-L2-031", "reason": "an agent superseded a decision"} if {
//!     input.principal.class == "agent"
//!     input.write.supersedes_type == "decision"
//! }
//! ```
//! ````

use serde::{Deserialize, Serialize};

/// Where a rule is in its life. Each step up is a reviewed promotion: a rule is in a
/// decision, as advisory or as enforced, only once a person has verified the version
/// that says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Written, and not yet in any decision.
    Proposed,
    /// Evaluated with every decision and reported, refusing nothing: where a rule
    /// written after something went wrong starts.
    Advisory,
    /// Refuses.
    Enforced,
    /// No longer in any decision. Kept, as every entry is.
    Retired,
}

impl Status {
    /// The word the block and the data hold.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Proposed => "proposed",
            Status::Advisory => "advisory",
            Status::Enforced => "enforced",
            Status::Retired => "retired",
        }
    }
}

/// What the `policy` block says.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// The stable id, `pol-L<layer>-<n>` or `exc-<year>-<n>`: what citations name.
    pub id: String,
    /// 0 values, 1 strategy, 2 practice, 3 domain convention.
    #[serde(default)]
    pub layer: u8,
    /// The rule, one sentence.
    #[serde(default)]
    pub rule: String,
    /// Why: what an edge case is resolved from.
    #[serde(default)]
    pub intent: String,
    /// The policy ids this one implements; an L3 convention names the L2 it serves.
    #[serde(default)]
    pub implements: Vec<String>,
    /// Who owns it: the person who answers for the rule, as `human:<system>:<id>`, or
    /// the domain that does.
    #[serde(default)]
    pub owner: Option<String>,
    /// Where the rule is in its life. A block that does not say is enforced once its
    /// entry is verified, as before the lifecycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    /// When the rule must be looked at again, a calendar date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_by: Option<String>,
    /// For an L0 rule: whether it is part of the core, which takes no exception. An L0
    /// rule that does not say is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core: Option<bool>,
    /// The person who signed this rule off above the domain's ceiling on enforced rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_off_by: Option<String>,
    /// For an exception: what it covers, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// For an exception: who approved it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approver: Option<String>,
    /// For an exception: why it is needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
    /// For an exception: the policy id it breaches.
    #[serde(default)]
    pub breaches: Option<String>,
    /// For an exception: the one task it holds for.
    #[serde(default)]
    pub scope_task: Option<String>,
    /// For an exception: when it lapses, RFC 3339.
    #[serde(default)]
    pub expires: Option<String>,
    /// For an L1 strategy: the rubric it compiles into, the task-scoring weights.
    #[serde(default)]
    pub rubric: Option<Rubric>,
}

/// The task-scoring weights an L1 strategy compiles into: the paper's rubric. A task's
/// rank starts from the priority it was filed with, plus the weight of the longest
/// origin prefix that matches it, plus its age in days times the age weight; a task
/// that blocks a ranked one rises to that rank plus the unblock weight, and a child to
/// its parent's. Curation has a protected floor: when its share of the completed work
/// falls under it, the best curation task waiting goes to the head of the queue.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rubric {
    /// Weight by origin prefix, e.g. `"curator:" = -1`, `"task:" = 1`.
    #[serde(default)]
    pub origin_weights: std::collections::BTreeMap<String, i64>,
    /// What a blocker gains above the task it blocks.
    #[serde(default = "one")]
    pub unblock_weight: i64,
    /// Rank per day of waiting.
    #[serde(default)]
    pub age_weight_per_day: i64,
    /// The share of completed work that curation is guaranteed, 0 to 1.
    #[serde(default = "floor")]
    pub maintenance_floor: f64,
}

fn one() -> i64 {
    1
}

fn floor() -> f64 {
    0.2
}

impl Eq for Rubric {}

impl Default for Rubric {
    fn default() -> Rubric {
        Rubric {
            origin_weights: std::collections::BTreeMap::new(),
            unblock_weight: one(),
            age_weight_per_day: 0,
            maintenance_floor: floor(),
        }
    }
}

impl Rubric {
    /// The weight of the longest origin prefix that matches.
    #[must_use]
    pub fn origin_weight(&self, origin: Option<&str>) -> i64 {
        let Some(o) = origin else { return 0 };
        self.origin_weights
            .iter()
            .filter(|(prefix, _)| o.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map_or(0, |(_, w)| *w)
    }
}

impl Meta {
    /// Whether it is an L1 strategy that compiles into a rubric.
    #[must_use]
    pub fn has_rubric(&self) -> bool {
        self.layer == 1 && self.rubric.is_some()
    }

    /// Whether this is an exception rather than a rule.
    #[must_use]
    pub fn is_exception(&self) -> bool {
        self.breaches.is_some()
    }

    /// Whether this is an L0 rule of the core, which takes no exception.
    #[must_use]
    pub fn is_core(&self) -> bool {
        self.layer == 0 && self.core.unwrap_or(true)
    }
}

/// A policy entry as the record holds it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    /// The record's entry id.
    pub entry_id: String,
    /// raw, curated or verified.
    pub tier: dm_core::TrustTier,
    /// What its block says.
    pub meta: Meta,
    /// Its Rego module, if it carries one.
    pub rego: Option<String>,
    /// The domain its entry is in; empty for one that came from no record.
    #[serde(default)]
    pub domain: String,
    /// How sensitive its entry is: who may read the rule's text.
    #[serde(default)]
    pub classification: dm_core::Label,
    /// When its entry was written, where the record says.
    #[serde(skip)]
    pub created: Option<time::OffsetDateTime>,
}

impl Entry {
    /// Where the rule is in its life, as its block and its entry's tier say together.
    /// The block names the step the rule's author means it to be at, and the tier
    /// holds it back: below verified a rule is proposed, whatever its block says, so no
    /// module is evaluated that a person has not verified. A module's denial names the
    /// rule it speaks for, and one evaluated unreviewed could name another rule's. A
    /// block that names no status is enforced once verified.
    #[must_use]
    pub fn state(&self) -> Status {
        use dm_core::TrustTier::Verified;
        match (self.meta.status, self.tier) {
            (Some(Status::Retired), _) => Status::Retired,
            (Some(Status::Enforced) | None, Verified) => Status::Enforced,
            (Some(Status::Advisory), Verified) => Status::Advisory,
            _ => Status::Proposed,
        }
    }

    /// Whether its module refuses: an enforced rule carrying one.
    #[must_use]
    pub fn enforced(&self) -> bool {
        self.state() == Status::Enforced && self.rego.is_some() && !self.meta.is_exception()
    }

    /// Whether its module is evaluated and reported without refusing.
    #[must_use]
    pub fn advisory(&self) -> bool {
        self.state() == Status::Advisory && self.rego.is_some() && !self.meta.is_exception()
    }

    /// Whether, as an exception, it is in force at `now`: verified, not expired, and
    /// not retired.
    #[must_use]
    pub fn in_force(&self, now: time::OffsetDateTime) -> bool {
        self.meta.is_exception()
            && self.meta.status != Some(Status::Retired)
            && self.tier == dm_core::TrustTier::Verified
            && self.meta.expires.as_deref().is_some_and(|e| {
                time::OffsetDateTime::parse(e, &time::format_description::well_known::Rfc3339)
                    .is_ok_and(|t| t > now)
            })
    }
}

/// The text of the first fenced block of `lang` in a body.
fn fenced<'a>(body: &'a str, lang: &str) -> Option<&'a str> {
    let open = format!("```{lang}\n");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find("\n```")?;
    Some(&body[start..start + end])
}

/// A policy entry out of an entry's id, tier and body, or why it is not one.
///
/// # Errors
///
/// A body with no `policy` block, a block that is not TOML of the shape, or no id.
pub fn parse(entry_id: &str, tier: dm_core::TrustTier, body: &str) -> Result<Entry, String> {
    let block = fenced(body, "policy").ok_or("no ```policy block")?;
    let meta: Meta = toml::from_str(block).map_err(|e| format!("policy block: {e}"))?;
    if meta.id.trim().is_empty() {
        return Err("policy block: no id".into());
    }
    Ok(Entry {
        entry_id: entry_id.to_string(),
        tier,
        meta,
        rego: fenced(body, "rego").map(str::to_string),
        domain: String::new(),
        classification: dm_core::Label::default(),
        created: None,
    })
}

/// A body with its `policy` block saying `status`: the text a rule is superseded with
/// when it moves in its life. Everything outside the block is kept as it was.
///
/// # Errors
///
/// A body with no `policy` block, or a block that is not TOML.
pub fn with_status(body: &str, status: Status) -> Result<String, String> {
    let block = fenced(body, "policy").ok_or("no ```policy block")?;
    let mut table: toml::Table = toml::from_str(block).map_err(|e| format!("policy block: {e}"))?;
    table.insert("status".into(), toml::Value::String(status.as_str().into()));
    let new = toml::to_string(&table).map_err(|e| format!("policy block: {e}"))?;
    // the block itself, by where it is: the same text can stand earlier in the body
    let at = block.as_ptr() as usize - body.as_ptr() as usize;
    let out = format!(
        "{}{}{}",
        &body[..at],
        new.trim_end(),
        &body[at + block.len()..]
    );
    match parse("", dm_core::TrustTier::Raw, &out) {
        Ok(e) if e.meta.status == Some(status) => Ok(out),
        _ => Err("the policy block does not read back with the new status".into()),
    }
}

/// The body of a policy entry: a heading, what a person reads, and the block.
///
/// # Errors
///
/// A block that will not be written as TOML.
pub fn render(title: &str, text: &str, meta: &Meta) -> Result<String, String> {
    let block = toml::to_string(meta).map_err(|e| format!("policy block: {e}"))?;
    Ok(format!(
        "# {title}\n\n{}\n\n```policy\n{}\n```\n",
        text.trim(),
        block.trim_end()
    ))
}

/// A module may only add denials: it must be in `package amem.rules`, so a record entry
/// cannot redefine the built-in decision, the read filter or the gates.
#[must_use]
pub fn module_is_confined(rego: &str) -> bool {
    rego.lines()
        .map(str::trim)
        .find(|l| l.starts_with("package "))
        .is_some_and(|l| l == "package amem.rules")
}

/// Something the lint found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// The policy id the finding is about.
    pub policy: String,
    /// What kind: `implements-unknown`, `higher-layer-conflict`, `exception-on-core`,
    /// `exception-expired`, `inert`, `module-refused`, `no-owner`, `review-due`; and
    /// from the caller, which has the grants and the decision log, `over-ceiling` and
    /// `unused`.
    pub kind: String,
    /// One sentence.
    pub why: String,
}

/// The paper's conflict rules over a set of policy entries: a lower layer that names a
/// rule it implements which is not there; an exception against an L0 rule, which takes
/// none; an exception past its expiry still on the books; a module outside the rules
/// package; a machine-checkable rule not yet verified, so inert.
#[must_use]
pub fn lint(entries: &[Entry], now: time::OffsetDateTime) -> Vec<Finding> {
    let mut out = Vec::new();
    let by_id = |id: &str| {
        entries
            .iter()
            .find(|e| e.meta.id == id && !e.meta.is_exception())
    };
    for e in entries {
        let m = &e.meta;
        for target in &m.implements {
            match by_id(target) {
                None => out.push(Finding {
                    policy: m.id.clone(),
                    kind: "implements-unknown".into(),
                    why: format!(
                        "{} says it implements {target}, which the record does not hold",
                        m.id
                    ),
                }),
                Some(t) if t.meta.layer > m.layer => out.push(Finding {
                    policy: m.id.clone(),
                    kind: "higher-layer-conflict".into(),
                    why: format!(
                        "{} at L{} implements {target} at L{}, a lower layer: the higher wins",
                        m.id, m.layer, t.meta.layer
                    ),
                }),
                Some(_) => {}
            }
        }
        out.extend(lifecycle_findings(e, now));
        if let Some(b) = &m.breaches {
            if by_id(b).is_some_and(|t| t.meta.is_core()) {
                out.push(Finding {
                    policy: m.id.clone(),
                    kind: "exception-on-core".into(),
                    why: format!(
                        "{} breaches {b}, a rule of the L0 core, which takes no exception: it is ignored",
                        m.id
                    ),
                });
            }
            if e.tier == dm_core::TrustTier::Verified && !e.in_force(now) {
                out.push(Finding {
                    policy: m.id.clone(),
                    kind: "exception-expired".into(),
                    why: format!(
                        "{} has expired; whatever relied on it is no longer covered",
                        m.id
                    ),
                });
            }
        }
        if let Some(r) = &e.rego {
            if !module_is_confined(r) {
                out.push(Finding {
                    policy: m.id.clone(),
                    kind: "module-refused".into(),
                    why: format!(
                        "{}'s module is not in package amem.rules and is never loaded",
                        m.id
                    ),
                });
            } else if e.tier != dm_core::TrustTier::Verified {
                out.push(Finding {
                    policy: m.id.clone(),
                    kind: "inert".into(),
                    why: format!(
                        "{} carries a rule but is {}: it is enforced once verified",
                        m.id, e.tier
                    ),
                });
            }
        }
    }
    out
}

/// What a rule's place in its life asks of it: a rule in a decision has an owner and
/// has been looked at by its review date, and a rule is where its block says it is.
fn lifecycle_findings(e: &Entry, now: time::OffsetDateTime) -> Vec<Finding> {
    let m = &e.meta;
    let mut out = Vec::new();
    if m.is_exception() {
        return out;
    }
    let state = e.state();
    let live = matches!(state, Status::Enforced | Status::Advisory);
    if live && m.owner.as_deref().is_none_or(|o| o.trim().is_empty()) {
        out.push(Finding {
            policy: m.id.clone(),
            kind: "no-owner".into(),
            why: format!(
                "{} is {} and names no owner to answer for it",
                m.id,
                state.as_str()
            ),
        });
    }
    let format = time::macros::format_description!("[year]-[month]-[day]");
    let due = m
        .review_by
        .as_deref()
        .and_then(|d| time::Date::parse(d, format).ok())
        .is_some_and(|d| d < now.date());
    if live && due {
        out.push(Finding {
            policy: m.id.clone(),
            kind: "review-due".into(),
            why: format!(
                "{} was to be reviewed by {}: its owner confirms it, amends it or retires it",
                m.id,
                m.review_by.as_deref().unwrap_or_default()
            ),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use dm_core::TrustTier;

    fn body(block: &str, rego: Option<&str>) -> String {
        let mut b = format!("# A rule\n\n```policy\n{block}\n```\n");
        if let Some(r) = rego {
            b.push_str(&format!("\n```rego\n{r}\n```\n"));
        }
        b
    }

    #[test]
    fn a_policy_entry_is_its_block_and_its_module() {
        let e = parse(
            "mem-p-1",
            TrustTier::Raw,
            &body(
                "id = \"pol-L2-031\"\nlayer = 2\nrule = \"r\"\nintent = \"i\"",
                Some("package amem.rules"),
            ),
        )
        .unwrap();
        assert_eq!(e.meta.id, "pol-L2-031");
        assert_eq!(e.meta.layer, 2);
        assert!(!e.enforced(), "raw is inert");
        let v = Entry {
            tier: TrustTier::Verified,
            ..e
        };
        assert!(v.enforced());
        assert!(parse("x", TrustTier::Raw, "no block").is_err());
        // a new status goes into the block, wherever else its text stands in the body
        let block = "id = \"pol-L2-031\"\nlayer = 2";
        let twice = format!("```toml\n{block}\n```\n\n```policy\n{block}\n```\n");
        let retired = with_status(&twice, Status::Retired).unwrap();
        assert_eq!(
            parse("x", TrustTier::Raw, &retired).unwrap().meta.status,
            Some(Status::Retired)
        );
        assert!(
            retired.starts_with(&format!("```toml\n{block}\n```")),
            "{retired}"
        );
        assert!(
            parse("x", TrustTier::Raw, &body("layer = 2", None)).is_err(),
            "no id"
        );
    }

    #[test]
    fn a_module_must_stay_in_the_rules_package() {
        assert!(module_is_confined("package amem.rules\nimport rego.v1\n"));
        assert!(!module_is_confined("package amem\ndecision := {}\n"));
        assert!(!module_is_confined("deny := 1"));
    }

    #[test]
    fn the_lint_finds_the_papers_conflicts() {
        let now = time::OffsetDateTime::now_utc();
        let mk = |id: &str, layer: u8, extra: &str, tier: TrustTier, rego: Option<&str>| {
            parse(
                id,
                tier,
                &body(&format!("id = \"{id}\"\nlayer = {layer}\n{extra}"), rego),
            )
            .unwrap()
        };
        let entries = vec![
            mk("pol-L0-001", 0, "", TrustTier::Verified, None),
            mk(
                "pol-L2-031",
                2,
                "",
                TrustTier::Raw,
                Some("package amem.rules"),
            ),
            mk(
                "pol-L3-112",
                3,
                "implements = [\"pol-L2-019\"]",
                TrustTier::Raw,
                None,
            ),
            mk(
                "pol-L1-007",
                1,
                "implements = [\"pol-L2-031\"]",
                TrustTier::Raw,
                None,
            ),
            mk(
                "exc-2026-001",
                2,
                "breaches = \"pol-L0-001\"\nexpires = \"2099-01-01T00:00:00Z\"",
                TrustTier::Raw,
                None,
            ),
            mk(
                "exc-2026-002",
                2,
                "breaches = \"pol-L2-031\"\nexpires = \"2000-01-01T00:00:00Z\"",
                TrustTier::Verified,
                None,
            ),
        ];
        let kinds: Vec<(String, String)> = lint(&entries, now)
            .into_iter()
            .map(|f| (f.policy, f.kind))
            .collect();
        for want in [
            ("pol-L2-031", "inert"),
            ("pol-L3-112", "implements-unknown"),
            ("pol-L1-007", "higher-layer-conflict"),
            ("exc-2026-001", "exception-on-core"),
            ("exc-2026-002", "exception-expired"),
        ] {
            assert!(
                kinds.contains(&(want.0.to_string(), want.1.to_string())),
                "{want:?} not in {kinds:?}"
            );
        }
        assert!(entries[5].meta.is_exception() && !entries[5].in_force(now));
    }
}
