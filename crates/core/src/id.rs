//! Entry and task identifiers: stable, permanent, meaningless.
//!
//! `mem-<domain-tag>-<8 chars>` for entries, `task-<domain-tag>-<8 chars>` for tasks.
//! The domain tag is the domain id without its `dom-` prefix, so ids read as the
//! paper's examples do (`mem-a-0847`) without carrying meaning the record relies on.

use rand::RngExt;
use sha2::{Digest, Sha256};

const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789"; // no 0/o, 1/l/i

fn tag(domain: &str) -> &str {
    domain.strip_prefix("dom-").unwrap_or(domain)
}

fn suffix(len: usize) -> String {
    let mut rng = rand::rng();
    (0..len)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

/// A fresh entry id for `domain`.
pub fn new_entry_id(domain: &str) -> String {
    format!("mem-{}-{}", tag(domain), suffix(8))
}

/// The entry id for `key` in `domain`: the same key gives the same id, whoever asks. The
/// suffix comes from a hash of the key, so the id has the shape of a fresh id and no more
/// meaning than one. It is for an entry that the record holds once for each key: two
/// writers of that entry name the same id, and the record takes one of them.
#[must_use]
pub fn entry_id_for(domain: &str, key: &str) -> String {
    let suffix: String = Sha256::digest(key.as_bytes())
        .iter()
        .take(8)
        .map(|b| ALPHABET[usize::from(*b) % ALPHABET.len()] as char)
        .collect();
    format!("mem-{}-{suffix}", tag(domain))
}

/// A fresh task id for `domain`.
pub fn new_task_id(domain: &str) -> String {
    format!("task-{}-{}", tag(domain), suffix(8))
}

/// Whether `s` has the shape of an entry id.
#[must_use]
pub fn is_entry_id(s: &str) -> bool {
    s.starts_with("mem-") && s.len() > 6
}

/// Whether `s` has the shape of a task id.
#[must_use]
pub fn is_task_id(s: &str) -> bool {
    s.starts_with("task-") && s.len() > 7
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        let e = new_entry_id("dom-kafka");
        assert!(e.starts_with("mem-kafka-"));
        assert_eq!(e.len(), "mem-kafka-".len() + 8);
        assert!(is_entry_id(&e));
        let t = new_task_id("me");
        assert!(t.starts_with("task-me-"));
        assert!(is_task_id(&t));
        assert_ne!(new_entry_id("dom-a"), new_entry_id("dom-a"));
    }

    #[test]
    fn the_id_for_a_key_is_the_same_each_time_and_has_the_shape_of_a_fresh_one() {
        let id = entry_id_for("dom-kafka", "declassify:internal:mem-kafka-1@abc:human:x:y");
        assert_eq!(
            id,
            entry_id_for("dom-kafka", "declassify:internal:mem-kafka-1@abc:human:x:y")
        );
        assert!(id.starts_with("mem-kafka-") && is_entry_id(&id), "{id}");
        assert_eq!(id.len(), new_entry_id("dom-kafka").len());
        let suffix = id.trim_start_matches("mem-kafka-");
        assert!(suffix.bytes().all(|b| ALPHABET.contains(&b)), "{id}");
        // another key, and the same key in another domain, are other ids
        assert_ne!(
            id,
            entry_id_for("dom-kafka", "declassify:public:mem-kafka-1@abc:human:x:y")
        );
        assert_ne!(
            id,
            entry_id_for("dom-a", "declassify:internal:mem-kafka-1@abc:human:x:y")
        );
    }
}
