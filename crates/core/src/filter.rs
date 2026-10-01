//! The read filter: what one session may see.
//!
//! The policy crate compiles a session's grants into one of these; the index applies it
//! inside every query, so a result, a count, or a summary never reflects an entry the
//! session may not read. Nothing else in the system filters after the fact.

use crate::{Label, TrustTier};

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
            && (active || self.include_inactive)
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

    #[test]
    fn default_is_the_narrowest() {
        let f = ReadFilter::default();
        assert!(f.domains.is_empty());
        assert_eq!(f.handling, Label::Public);
        assert!(f.admits("anything", Label::Public, TrustTier::Raw, true));
        assert!(!f.admits("anything", Label::Internal, TrustTier::Raw, true));
    }
}
