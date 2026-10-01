//! Entry and task identifiers: stable, permanent, meaningless.
//!
//! `mem-<domain-tag>-<8 chars>` for entries, `task-<domain-tag>-<8 chars>` for tasks.
//! The domain tag is the domain id without its `dom-` prefix, so ids read as the
//! paper's examples do (`mem-a-0847`) without carrying meaning the record relies on.

use rand::RngExt;

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

pub fn new_entry_id(domain: &str) -> String {
    format!("mem-{}-{}", tag(domain), suffix(8))
}

pub fn new_task_id(domain: &str) -> String {
    format!("task-{}-{}", tag(domain), suffix(8))
}

pub fn is_entry_id(s: &str) -> bool {
    s.starts_with("mem-") && s.len() > 6
}

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
}
