//! The read filter: what one session may see.
//!
//! The policy crate compiles a session's grants into one of these; the index applies it
//! inside every query, so a result, a count, or a summary never reflects an entry the
//! session may not read. Nothing else in the system filters after the fact.

use crate::{Label, TrustTier};
use time::OffsetDateTime;

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
    /// The highest trust tier returned. Verified by default, which leaves nothing out.
    pub max_tier: TrustTier,
    /// The snapshot: an entry created later is not returned. None by default.
    pub not_after: Option<OffsetDateTime>,
}

impl ReadFilter {
    /// Everything in the named domains, at the given handling, active entries only.
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
            max_tier: TrustTier::Verified,
            not_after: None,
        }
    }

    /// Every domain at the given handling, active entries only.
    #[must_use]
    pub fn any_domain(handling: Label) -> Self {
        ReadFilter::for_domains(Vec::<String>::new(), handling)
    }

    /// Whether a single entry's metadata passes this filter.
    #[must_use]
    pub fn admits(
        &self,
        domain: &str,
        classification: Label,
        tier: TrustTier,
        active: bool,
    ) -> bool {
        (self.domains.is_empty() || self.domains.iter().any(|d| d == domain))
            && classification <= self.handling
            && tier >= self.min_tier
            && tier <= self.max_tier
            && (active || self.include_inactive)
    }

    /// Whether an entry created at `created` is inside the snapshot. The instant of the
    /// snapshot itself is inside. [`ReadFilter::admits`] takes no time, so a caller that
    /// filters outside the index asks both.
    #[must_use]
    pub fn admits_at(&self, created: OffsetDateTime) -> bool {
        self.not_after.is_none_or(|at| created <= at)
    }

    /// This filter, closed: the lowest tier is above the highest, so it admits no entry.
    /// The domains stay, so a request that names one of them gets an empty answer where
    /// a filter without the domain would refuse it.
    #[must_use]
    pub fn closed(mut self) -> Self {
        self.min_tier = TrustTier::Verified;
        self.max_tier = TrustTier::Raw;
        self
    }

    /// Whether no tier passes this filter.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.min_tier > self.max_tier
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
    use time::macros::datetime;
    use time::Duration;

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

    #[test]
    fn default_is_the_narrowest() {
        let f = ReadFilter::default();
        assert!(f.domains.is_empty());
        assert_eq!(f.handling, Label::Public);
        assert!(f.admits("anything", Label::Public, TrustTier::Raw, true));
        assert!(!f.admits("anything", Label::Internal, TrustTier::Raw, true));
    }

    const TIERS: [TrustTier; 3] = [TrustTier::Raw, TrustTier::Curated, TrustTier::Verified];
    const LABELS: [Label; 3] = [Label::Public, Label::Internal, Label::Restricted];

    #[test]
    fn a_new_filter_has_no_tier_ceiling_and_no_snapshot() {
        for f in [
            ReadFilter::for_domains(["dom-a"], Label::Restricted),
            ReadFilter::any_domain(Label::Restricted),
        ] {
            assert_eq!(f.max_tier, TrustTier::Verified);
            assert_eq!(f.not_after, None);
            assert!(!f.is_closed());
            for tier in TIERS {
                for label in LABELS {
                    assert!(f.admits("dom-a", label, tier, true), "{tier} {label}");
                }
            }
            assert!(f.admits_at(datetime!(9999-12-31 23:59:59 UTC)));
        }
        let narrowest = ReadFilter::default();
        assert_eq!(narrowest.max_tier, TrustTier::Verified);
        assert_eq!(narrowest.not_after, None);
        for tier in TIERS {
            assert!(narrowest.admits("anything", Label::Public, tier, true));
        }
    }

    #[test]
    fn an_entry_above_the_highest_tier_is_not_admitted() {
        let mut f = ReadFilter::for_domains(["dom-a"], Label::Restricted);
        f.max_tier = TrustTier::Raw;
        assert!(f.admits("dom-a", Label::Public, TrustTier::Raw, true));
        assert!(!f.admits("dom-a", Label::Public, TrustTier::Curated, true));
        assert!(!f.admits("dom-a", Label::Public, TrustTier::Verified, true));
        f.max_tier = TrustTier::Curated;
        assert!(f.admits("dom-a", Label::Public, TrustTier::Curated, true));
        assert!(!f.admits("dom-a", Label::Public, TrustTier::Verified, true));
    }

    #[test]
    fn a_closed_filter_admits_no_entry_and_keeps_its_domains() {
        let mut open = ReadFilter::for_domains(["dom-a", "dom-b"], Label::Restricted);
        open.include_inactive = true;
        let closed = open.clone().closed();
        assert!(closed.is_closed());
        assert_eq!(closed.domains, open.domains);
        assert_eq!(closed.handling, open.handling);
        for domain in ["dom-a", "dom-b", "dom-c"] {
            for label in LABELS {
                for tier in TIERS {
                    for active in [true, false] {
                        assert!(
                            !closed.admits(domain, label, tier, active),
                            "{domain} {label} {tier} {active}"
                        );
                    }
                }
            }
        }
        assert!(ReadFilter::any_domain(Label::Restricted)
            .closed()
            .is_closed());
    }

    #[test]
    fn the_snapshot_admits_its_own_instant_and_nothing_later() {
        let at = datetime!(2026-10-01 10:00:00.5 UTC);
        let mut f = ReadFilter::for_domains(["dom-a"], Label::Internal);
        f.not_after = Some(at);
        assert!(f.admits_at(at), "the instant itself");
        assert!(f.admits_at(at - Duration::nanoseconds(1)));
        assert!(!f.admits_at(at + Duration::nanoseconds(1)));
        // the same instant at another offset is the same instant
        assert!(f.admits_at(at.to_offset(time::macros::offset!(+2))));
    }
}
