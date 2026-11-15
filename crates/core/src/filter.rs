//! The read filter: what one session may see.
//!
//! The policy crate compiles a session's grants into one of these; the index applies it
//! inside every query, so a result, a count, or a summary never reflects an entry the
//! session may not read. Nothing else in the system filters after the fact.

use crate::{Label, Reach, TrustTier};
use serde::{Deserialize, Serialize};

/// Which raw entries a session reads. Ordered from the narrowest: chain < fleet < all.
/// It decides nothing about a curated or a verified entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RawReach {
    /// Only the raw entries that name a task of the session's chain.
    Chain,
    /// Those, and each raw entry whose reach is not [`Reach::Tap`].
    Fleet,
    /// Every raw entry.
    All,
}

/// What a session may read. Every read path takes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadFilter {
    /// Domains the session may read. Empty means every domain the index holds.
    pub domains: Vec<String>,
    /// The highest classification the session may read: its handling.
    pub handling: Label,
    /// The lowest trust tier worth returning. Raw by default, since a handoff written a
    /// minute ago is raw and the next agent needs it.
    pub min_tier: TrustTier,
    /// Whether demoted, superseded, and erased entries are returned too.
    pub include_inactive: bool,
    /// Which raw entries the session reads.
    pub raw_reach: RawReach,
    /// The ids of the tasks in the session's chain. A raw entry that names one of them
    /// is read whatever its reach and whatever `raw_reach` is.
    pub chain: Vec<String>,
}

impl ReadFilter {
    /// Everything in the named domains, at the given handling, active entries only,
    /// every raw entry among them.
    #[must_use]
    pub fn for_domains(
        domains: impl IntoIterator<Item = impl Into<String>>,
        handling: Label,
    ) -> Self {
        ReadFilter {
            domains: domains.into_iter().map(Into::into).collect(),
            handling,
            min_tier: TrustTier::Raw,
            include_inactive: false,
            raw_reach: RawReach::All,
            chain: Vec::new(),
        }
    }

    /// Every domain at the given handling, active entries only.
    #[must_use]
    pub fn any_domain(handling: Label) -> Self {
        ReadFilter::for_domains(Vec::<String>::new(), handling)
    }

    /// Whether a single entry's metadata passes this filter, for an entry with the
    /// fleet's reach that names no task.
    #[must_use]
    pub fn admits(
        &self,
        domain: &str,
        classification: Label,
        tier: TrustTier,
        active: bool,
    ) -> bool {
        self.admits_entry(domain, classification, tier, active, Reach::Fleet, None)
    }

    /// Whether a single entry's metadata passes this filter, its reach and the task it
    /// names included. `min_tier` is a floor: a filter that asks for curated entries
    /// admits no raw entry, one of the session's own chain included.
    #[must_use]
    pub fn admits_entry(
        &self,
        domain: &str,
        classification: Label,
        tier: TrustTier,
        active: bool,
        reach: Reach,
        task: Option<&str>,
    ) -> bool {
        (self.domains.is_empty() || self.domains.iter().any(|d| d == domain))
            && classification <= self.handling
            && tier >= self.min_tier
            && (active || self.include_inactive)
            && (tier > TrustTier::Raw || self.admits_raw(reach, task))
    }

    /// Whether a raw entry with this reach and this task is within `raw_reach`.
    fn admits_raw(&self, reach: Reach, task: Option<&str>) -> bool {
        match self.raw_reach {
            RawReach::All => true,
            _ if task.is_some_and(|t| self.chain.iter().any(|c| c == t)) => true,
            RawReach::Fleet => reach != Reach::Tap,
            RawReach::Chain => false,
        }
    }
}

impl Default for ReadFilter {
    /// The most restrictive filter: public entries in any domain, active only.
    fn default() -> Self {
        ReadFilter::any_domain(Label::Public)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admits_on_every_axis() {
        let f = ReadFilter::for_domains(["dom-a"], Label::Internal);
        assert!(f.admits("dom-a", Label::Internal, TrustTier::Raw, true));
        assert!(
            !f.admits("dom-b", Label::Public, TrustTier::Raw, true),
            "domain"
        );
        assert!(
            !f.admits("dom-a", Label::Restricted, TrustTier::Raw, true),
            "label"
        );
        assert!(
            !f.admits("dom-a", Label::Public, TrustTier::Raw, false),
            "inactive"
        );
        let mut f = f;
        f.min_tier = TrustTier::Curated;
        assert!(
            !f.admits("dom-a", Label::Public, TrustTier::Raw, true),
            "tier"
        );
        f.include_inactive = true;
        assert!(f.admits("dom-a", Label::Public, TrustTier::Verified, false));
    }

    /// A filter over every domain with this raw reach and this chain.
    fn reaching(raw_reach: RawReach, chain: &[&str]) -> ReadFilter {
        let mut f = ReadFilter::any_domain(Label::Internal);
        f.raw_reach = raw_reach;
        f.chain = chain.iter().map(|t| (*t).to_string()).collect();
        f
    }

    fn raw(f: &ReadFilter, reach: Reach, task: Option<&str>) -> bool {
        f.admits_entry("dom-a", Label::Public, TrustTier::Raw, true, reach, task)
    }

    #[test]
    fn the_constructors_read_every_raw_entry() {
        for f in [
            ReadFilter::for_domains(["dom-a"], Label::Internal),
            ReadFilter::any_domain(Label::Internal),
            ReadFilter::default(),
        ] {
            assert_eq!(f.raw_reach, RawReach::All);
            assert!(f.chain.is_empty());
            for reach in [Reach::Chain, Reach::Fleet, Reach::Tap] {
                assert!(raw(&f, reach, None), "{reach}");
                assert!(raw(&f, reach, Some("task-x")), "{reach}");
            }
        }
        assert!(RawReach::Chain < RawReach::Fleet && RawReach::Fleet < RawReach::All);
    }

    #[test]
    fn a_raw_entry_with_tap_reach_is_read_by_its_chain_and_by_a_reader_of_all() {
        let fleet = reaching(RawReach::Fleet, &["task-other"]);
        assert!(!raw(&fleet, Reach::Tap, Some("task-x")), "not its chain");
        assert!(!raw(&fleet, Reach::Tap, None), "no task");
        let chain = reaching(RawReach::Fleet, &["task-other", "task-x"]);
        assert!(raw(&chain, Reach::Tap, Some("task-x")), "its chain");
        assert!(
            !raw(&chain, Reach::Tap, None),
            "a chain admits no entry without a task"
        );
        let all = reaching(RawReach::All, &[]);
        assert!(raw(&all, Reach::Tap, Some("task-x")));
        assert!(raw(&all, Reach::Tap, None));
    }

    #[test]
    fn a_raw_entry_with_fleet_or_chain_reach_is_read_under_fleet_and_only_by_its_chain_under_chain()
    {
        for reach in [Reach::Fleet, Reach::Chain] {
            let fleet = reaching(RawReach::Fleet, &[]);
            assert!(raw(&fleet, reach, None), "{reach}");
            assert!(raw(&fleet, reach, Some("task-x")), "{reach}");
            let narrow = reaching(RawReach::Chain, &["task-other"]);
            assert!(!raw(&narrow, reach, None), "{reach}");
            assert!(!raw(&narrow, reach, Some("task-x")), "{reach}");
            let own = reaching(RawReach::Chain, &["task-x"]);
            assert!(raw(&own, reach, Some("task-x")), "{reach}");
            assert!(!raw(&own, reach, None), "{reach}");
        }
    }

    #[test]
    fn a_curated_entry_is_read_whatever_its_reach() {
        for raw_reach in [RawReach::Chain, RawReach::Fleet, RawReach::All] {
            let f = reaching(raw_reach, &["task-other"]);
            for reach in [Reach::Chain, Reach::Fleet, Reach::Tap] {
                for tier in [TrustTier::Curated, TrustTier::Verified] {
                    for task in [None, Some("task-x")] {
                        assert!(
                            f.admits_entry("dom-a", Label::Public, tier, true, reach, task),
                            "{raw_reach:?} {reach} {tier}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_tier_floor_refuses_a_raw_entry_of_the_own_chain() {
        for raw_reach in [RawReach::Chain, RawReach::Fleet, RawReach::All] {
            let mut f = reaching(raw_reach, &["task-x"]);
            f.min_tier = TrustTier::Curated;
            for reach in [Reach::Chain, Reach::Fleet, Reach::Tap] {
                assert!(!raw(&f, reach, Some("task-x")), "{raw_reach:?} {reach}");
            }
        }
    }

    #[test]
    fn the_other_axes_hold_for_an_entry_of_the_own_chain() {
        let mut f = reaching(RawReach::Chain, &["task-x"]);
        f.domains = vec!["dom-a".into()];
        let own = |domain, label, active| {
            f.admits_entry(
                domain,
                label,
                TrustTier::Raw,
                active,
                Reach::Tap,
                Some("task-x"),
            )
        };
        assert!(own("dom-a", Label::Internal, true));
        assert!(!own("dom-b", Label::Internal, true), "domain");
        assert!(!own("dom-a", Label::Restricted, true), "label");
        assert!(!own("dom-a", Label::Internal, false), "inactive");
    }

    #[test]
    fn admits_answers_for_fleet_reach_and_no_task() {
        for raw_reach in [RawReach::Chain, RawReach::Fleet, RawReach::All] {
            for chain in [&[][..], &["task-x"][..]] {
                let f = reaching(raw_reach, chain);
                for tier in [TrustTier::Raw, TrustTier::Curated] {
                    assert_eq!(
                        f.admits("dom-a", Label::Public, tier, true),
                        f.admits_entry("dom-a", Label::Public, tier, true, Reach::Fleet, None),
                        "{raw_reach:?} {tier}"
                    );
                }
            }
        }
        let chain = reaching(RawReach::Chain, &["task-x"]);
        assert!(!chain.admits("dom-a", Label::Public, TrustTier::Raw, true));
        assert!(chain.admits("dom-a", Label::Public, TrustTier::Curated, true));
        assert!(reaching(RawReach::Fleet, &[]).admits(
            "dom-a",
            Label::Public,
            TrustTier::Raw,
            true
        ));
    }

    #[test]
    fn default_is_the_narrowest() {
        let f = ReadFilter::default();
        assert!(f.domains.is_empty());
        assert_eq!(f.handling, Label::Public);
        assert!(f.admits("anything", Label::Public, TrustTier::Raw, true));
        assert!(!f.admits("anything", Label::Internal, TrustTier::Raw, true));
    }
}
