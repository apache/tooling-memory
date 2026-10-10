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
    /// The domain that owns it.
    #[serde(default)]
    pub owner: Option<String>,
    /// For an exception: the policy id it breaches.
    #[serde(default)]
    pub breaches: Option<String>,
    /// For an exception: the one task it holds for.
    #[serde(default)]
    pub scope_task: Option<String>,
    /// For an exception: when it lapses, RFC 3339.
    #[serde(default)]
    pub expires: Option<String>,
}

impl Meta {
    /// Whether this is an exception rather than a rule.
    #[must_use]
    pub fn is_exception(&self) -> bool {
        self.breaches.is_some()
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
}

impl Entry {
    /// Whether its module is enforced: verified and carrying one.
    #[must_use]
    pub fn enforced(&self) -> bool {
        self.tier == dm_core::TrustTier::Verified
            && self.rego.is_some()
            && !self.meta.is_exception()
    }

    /// Whether, as an exception, it is in force at `now`: verified, not expired.
    #[must_use]
    pub fn in_force(&self, now: time::OffsetDateTime) -> bool {
        self.meta.is_exception()
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
    })
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
    /// `exception-expired`, `inert`, `module-refused`.
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
        if let Some(b) = &m.breaches {
            if by_id(b).is_some_and(|t| t.meta.layer == 0) {
                out.push(Finding {
                    policy: m.id.clone(),
                    kind: "exception-on-core".into(),
                    why: format!(
                        "{} breaches {b}, an L0 rule, which takes no exception: it is ignored",
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
