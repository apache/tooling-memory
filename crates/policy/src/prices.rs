//! The price vector: what each resource class costs, as a file the owner keeps.
//!
//! Prices live in `prices.toml` under the root, in US dollars per million tokens, per
//! model as the provider reports it, with the four classes of token priced apart.
//! `[resources]` in the same table prices CPU time and memory held. The table has a
//! version: the top-level key `version` when the file has one, else the digest of the
//! file's text, so a file with no key is versioned too and a changed rate is a new
//! version. A limit in money and a cost report name the version they were priced by.

use crate::Error;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The version of the empty table: no file, or a file with no text.
pub const NO_VERSION: &str = "none";

/// The template `amem init` writes: every rate left for the owner to fill in.
pub const TEMPLATE: &str = r#"# What a million tokens cost, in US dollars, per model as the provider reports it in
# `amem usage`. A key is matched exactly, else as the longest prefix of the model's
# name, so "claude-opus-5" prices "claude-opus-5-5". Four classes, priced apart:
# input (not from the cache), output, cache reads and cache writes. A model not named
# here is shown unpriced. Calls on a harness's own login are plan quota, never priced.
#
# The version of this table. Left out, the version is the digest of this file's text.
#
# version = "2026-11"
#
# [models."claude-opus-5-5"]
# input = 0.0
# output = 0.0
# cache_read = 0.0
# cache_write = 0.0
#
# The host's own resources, per run of a harness: CPU time of the process tree and the
# launcher, in US dollars per CPU-hour, and peak resident memory held for the run's wall
# clock, in US dollars per GiB-hour. Left out, they are shown unpriced.
#
# [resources]
# cpu_hour = 0.0
# memory_gib_hour = 0.0
"#;

/// The rates of one model, in US dollars per million tokens.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Rate {
    /// Input tokens that did not come from the cache.
    #[serde(default)]
    pub input: f64,
    /// Output tokens.
    #[serde(default)]
    pub output: f64,
    /// Tokens read from the cache.
    #[serde(default)]
    pub cache_read: f64,
    /// Tokens written to the cache.
    #[serde(default)]
    pub cache_write: f64,
}

/// What the host's resources cost, in US dollars.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ResourceRates {
    /// Per hour of CPU time, user and system, the run's tree and the launcher's.
    #[serde(default)]
    pub cpu_hour: f64,
    /// Per GiB of peak resident memory held for an hour of the run's wall clock.
    #[serde(default)]
    pub memory_gib_hour: f64,
}

/// Tokens of one model, by class.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    /// Input tokens that did not come from the cache.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Tokens read from the cache.
    pub cache_read: u64,
    /// Tokens written to the cache.
    pub cache_write: u64,
}

/// The table.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Prices {
    /// The version: the file's `version` key, else the digest of the file's text.
    #[serde(default)]
    pub version: String,
    /// The rates, by model.
    #[serde(default)]
    pub models: BTreeMap<String, Rate>,
    /// The host's resources; absent, they are not priced.
    #[serde(default)]
    pub resources: Option<ResourceRates>,
}

impl Prices {
    /// The table that `text` holds. Empty text is the empty table, version `none`.
    ///
    /// # Errors
    ///
    /// Text that is not valid TOML or does not fit the schema.
    pub fn parse(text: &str) -> Result<Prices, Error> {
        let mut p: Prices = toml::from_str(text)?;
        if p.version.is_empty() {
            p.version = if text.trim().is_empty() {
                NO_VERSION.to_string()
            } else {
                dm_core::digest(text)
            };
        }
        Ok(p)
    }

    /// A model's rate: its exact name, else the longest key it starts with.
    #[must_use]
    pub fn rate(&self, model: &str) -> Option<&Rate> {
        self.models.get(model).or_else(|| {
            self.models
                .iter()
                .filter(|(k, _)| model.starts_with(k.as_str()))
                .max_by_key(|(k, _)| k.len())
                .map(|(_, r)| r)
        })
    }

    /// What tokens of a model cost, in US dollars, when the table prices the model.
    #[must_use]
    pub fn cost(&self, model: &str, t: &Tokens) -> Option<f64> {
        let r = self.rate(model)?;
        #[allow(clippy::cast_precision_loss)] // token counts, priced
        let usd = (t.input as f64 * r.input
            + t.output as f64 * r.output
            + t.cache_read as f64 * r.cache_read
            + t.cache_write as f64 * r.cache_write)
            / 1_000_000.0;
        Some(usd)
    }
}

/// US dollars as whole US cents, rounded down. A sum is rounded once, so many small
/// calls do not each round to nothing.
#[must_use]
pub fn whole_cents(usd: f64) -> u64 {
    // the small term absorbs the error of the binary fraction: 0.01 dollars is one cent
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // floored, not negative
    let cents = (usd * 100.0 + 1e-9).floor().max(0.0) as u64;
    cents
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "[models.m]\ninput = 10.0\noutput = 50.0\n";

    #[test]
    fn a_table_with_no_version_key_has_the_digest_of_its_text() {
        let p = Prices::parse(TABLE).unwrap();
        assert_eq!(p.version, dm_core::digest(TABLE));
        let changed = Prices::parse("[models.m]\ninput = 11.0\noutput = 50.0\n").unwrap();
        assert_ne!(changed.version, p.version, "one changed rate");
    }

    #[test]
    fn an_explicit_version_is_kept_and_no_text_is_version_none() {
        let p = Prices::parse(&format!("version = \"2026-11\"\n{TABLE}")).unwrap();
        assert_eq!(p.version, "2026-11");
        assert_eq!(Prices::parse("").unwrap().version, NO_VERSION);
        assert!(Prices::parse("").unwrap().models.is_empty());
    }

    #[test]
    fn tokens_are_priced_by_class_and_cents_round_down() {
        let p = Prices::parse(TABLE).unwrap();
        let t = Tokens {
            input: 1_000_000,
            output: 1000,
            ..Tokens::default()
        };
        // 1M input at 10 dollars, 1000 output at 50 dollars per million
        assert!((p.cost("m-2", &t).unwrap() - 10.05).abs() < 1e-9);
        assert_eq!(whole_cents(p.cost("m-2", &t).unwrap()), 1005);
        let small = Tokens {
            input: 1999,
            ..Tokens::default()
        };
        let usd = p.cost("m", &small).unwrap();
        assert_eq!(whole_cents(usd), 1, "1.999 cents is one cent");
        assert_eq!(whole_cents(0.01), 1, "one cent is one cent");
        assert!(p.cost("other", &t).is_none(), "not named, not guessed");
    }
}
