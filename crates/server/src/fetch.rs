//! The fetch of the paper's Appendix A: `get(id | id#anchor | id@version)`.
//!
//! [`Memory::fetch`] serves the current version of an entry, one section of it, or one
//! version of it by the digest of its stored text ([`crate::address`]). Every form is
//! under the read filter of the session, raises the label of the session, and leaves a
//! `read` event in the log of the domain. A section costs the caller the tokens of the
//! section, and the read log counts those. A version lets a reader check what a
//! citation, an approval or a declassified copy named, after the entry moved on.

use crate::address::{self, Address, Part};
use crate::prefetch::Related;
use crate::{reach, withhold, Error, Memory};
use dm_core::{Entry, Status};
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;

/// The most related entries that a fetch of a current version carries.
pub const RELATED_ON_FETCH: usize = 2;

/// What a fetch served.
#[derive(Clone, Debug, PartialEq)]
pub struct Fetched {
    /// The entry served. Its body is what was served: the whole body, or the section
    /// alone.
    pub entry: Entry,
    /// The version served: the digest of its stored text.
    pub version: String,
    /// Whether that version is the current one.
    pub current: bool,
    /// The anchor of the section served, when the address named one.
    pub anchor: Option<String>,
    /// The ids that the fetch was redirected from, in order. A fetch by version follows
    /// no redirect.
    pub redirected_from: Vec<String>,
    /// The entries related to the one served, each with its weight
    /// ([`Memory::related`]). A fetch by version carries none.
    pub related: Vec<Related>,
}

impl Memory {
    /// Serve what `address` names: `<id>`, `<id>#<anchor>` or `<id>@<version>`.
    ///
    /// `<id>` is [`Memory::get_following`]: the current version, with the redirect of a
    /// replaced entry followed, and with the entries related to it.
    ///
    /// `<id>#<anchor>` passes the same checks, and the label of the session rises to the
    /// classification of the entry. The body served is the section alone
    /// ([`address::section`]), and the read log counts the tokens of the section. An
    /// anchor that the entry does not have is refused with the anchors that it has. The
    /// entry was read to find them: the label rose and the log of the domain has the
    /// `read` event, as for a section that was served.
    ///
    /// `<id>@<version>` follows no redirect. The read filter of the session must admit
    /// the entry as it is now, in the active set or out of it, and must admit the
    /// version by the frontmatter of the version. The label of the session rises to the
    /// higher of the two labels. The version of an erased entry is not served, and the
    /// version of a withheld entry is served to the security role alone.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a malformed address. [`Error::NotFound`] for an entry or a
    /// version that is absent or not for this session, and for an unknown anchor.
    /// [`Error::Gone`] for an erased entry, for a withheld entry and an author outside
    /// the security role, and past the redirect limit. Otherwise as [`Memory::get`].
    pub async fn fetch(&self, address: &str) -> Result<Fetched, Error> {
        let at: Address = address.parse().map_err(Error::Denied)?;
        match at.part {
            Part::Whole => self.fetch_current(&at.id, None).await,
            Part::Anchor(anchor) => self.fetch_current(&at.id, Some(anchor)).await,
            Part::Version(version) => self.fetch_version(&at.id, &version).await,
        }
    }

    /// The current version of `id`, or the section of it that `anchor` names.
    async fn fetch_current(&self, id: &str, anchor: Option<String>) -> Result<Fetched, Error> {
        let (mut entry, version, redirected_from) = self.admitted_following(id).await?;
        if let Some(anchor) = &anchor {
            let Some(section) = address::section(&entry.body, anchor) else {
                return Err(unknown_anchor(&entry, anchor));
            };
            self.log_opened(&entry, &section);
            entry.body = section;
        } else {
            self.log_opened(&entry, &entry.body);
        }
        // the entry is served by now: an index that does not answer for the related
        // entries leaves them out, and does not take the entry back
        let related = self.related(&entry.front.id, RELATED_ON_FETCH).await;
        Ok(Fetched {
            entry,
            version,
            current: true,
            anchor,
            redirected_from,
            related: related.unwrap_or_default(),
        })
    }

    /// The version of `id` whose stored text has the digest `version`.
    async fn fetch_version(&self, id: &str, version: &str) -> Result<Fetched, Error> {
        let not_found = || Error::NotFound(id.to_string());
        let own = self.session_filter()?;
        let mut wide = own.clone();
        wide.include_inactive = true;
        // the entry as it is now: a version of an entry that the session may not read
        // now is not served, whatever the version says of itself
        let meta = self.index.lock().await.meta(id, &wide)?;
        let domain = meta.ok_or_else(not_found)?.hit.domain;
        let stored = self.store.get_with_text(&domain, id).await?;
        let (now, text) = stored.ok_or_else(not_found)?;
        match now.front.status {
            Some(Status::Erased) => {
                return Err(Error::Gone(format!(
                    "{id} was erased; a stub and a receipt remain"
                )))
            }
            Some(Status::Withheld) if self.withheld_from_author(&now) => {
                return Err(withhold::gone(id))
            }
            _ => {}
        }
        let current = dm_core::digest(&text) == version;
        let entry = if current {
            now.clone()
        } else {
            let history = self.store.history(&domain, id).await?;
            let old = history.iter().find(|v| dm_core::digest(&v.text) == version);
            let old =
                old.ok_or_else(|| Error::NotFound(format!("{id} has no version {version}")))?;
            Entry::parse(&old.text)?
        };
        // the version by its own frontmatter: its label, its tier, its reach and its
        // time must pass the filter too
        let front = &entry.front;
        let admitted = own.admits_entry(
            &domain,
            front.classification,
            front.trust_tier,
            true,
            front.reach,
            front.task.as_deref(),
        );
        if !admitted || !own.admits_at(front.created) {
            return Err(not_found());
        }
        let label = now.front.classification.max(front.classification);
        let outside = reach::outside_content(&entry) || reach::outside_content(&now);
        if !self.observe_entry(&domain, id, label, outside)? {
            return Err(not_found());
        }
        self.note_get();
        self.reads.opened(id, &entry.body, false);
        Ok(Fetched {
            entry,
            version: version.to_string(),
            current,
            anchor: None,
            redirected_from: Vec::new(),
            related: Vec::new(),
        })
    }
}

impl Fetched {
    /// The text that the fetch served, as a command prints it: the section alone when
    /// the address named an anchor, else the entry with its frontmatter.
    #[must_use]
    pub fn text(&self) -> String {
        if self.anchor.is_some() {
            self.entry.body.clone()
        } else {
            self.entry.render()
        }
    }
}

/// The JSON that a binding returns for a fetch. One shape for the command line and the
/// memory tool. `version` is the address part that a later fetch by version takes.
/// `anchor` is there when the address named one, `current` only when it is false, and
/// `related` only when the fetch carries related entries.
#[must_use]
pub fn to_json(f: &Fetched) -> Value {
    let front = &f.entry.front;
    let mut v = json!({
        "id": front.id,
        "domain": front.domain,
        "type": front.entry_type.to_string(),
        "author": front.author,
        "created": front.created.format(&Rfc3339).unwrap_or_default(),
        "trust_tier": front.trust_tier.to_string(),
        "classification": front.classification.to_string(),
        "cites": front.cites,
        "supersedes": front.supersedes,
        "review_by": front.review_by.map(|d| d.to_string()),
        "title": f.entry.title(),
        "version": f.version,
        "body": f.entry.body,
    });
    // an entry with fleet reach and no task has neither key
    if !front.reach.is_fleet() {
        v["reach"] = json!(front.reach.to_string());
    }
    if let Some(task) = &front.task {
        v["task"] = json!(task);
    }
    if let Some(anchor) = &f.anchor {
        v["anchor"] = json!(anchor);
    }
    if !f.current {
        v["current"] = json!(false);
    }
    if !f.redirected_from.is_empty() {
        // as a client shows the URL it was redirected to: cite this id, not the one asked
        v["redirected_from"] = json!(f.redirected_from);
    }
    if !f.related.is_empty() {
        v["related"] = f.related.iter().map(related_json).collect();
    }
    v
}

/// One related entry in the answer of a fetch: its summary, and its weight to two
/// decimal places.
fn related_json(r: &Related) -> Value {
    json!({
        "id": r.hit.id,
        "title": r.hit.title,
        "summary": r.hit.summary,
        "p": (r.p * 100.0).round() / 100.0,
    })
}

/// The refusal of an anchor that `entry` does not have, with the anchors that it has.
fn unknown_anchor(entry: &Entry, anchor: &str) -> Error {
    let id = &entry.front.id;
    let has = address::anchors(&entry.body);
    if has.is_empty() {
        Error::NotFound(format!(
            "{id} has no section `{anchor}`: its body has no heading"
        ))
    } else {
        Error::NotFound(format!(
            "{id} has no section `{anchor}`; its anchors are: {}",
            has.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{agents_read, read_in_log};
    use crate::{reads, WriteRequest};
    use dm_core::erasure::Reason;
    use dm_core::{Label, TrustTier};
    use std::path::Path;

    const BODY: &str = "# Import\n\nThe import has two steps, and each one is long enough to cost tokens.\n\n## First step\n\nCopy the file to the staging host and check its digest there.\n\n### A detail\n\nThe digest is SHA-256.\n\n## Second step\n\nRun the loader.\n";

    fn mem(dir: &Path) -> Memory {
        Memory::init(dir, "human:test:me", "dom-me").unwrap()
    }

    fn agent(dir: &Path, name: &str) -> Memory {
        Memory::open(dir).unwrap().with_author(name)
    }

    async fn wrote(m: &Memory, body: &str, label: Label) -> String {
        m.write(WriteRequest {
            body: body.into(),
            classification: Some(label),
            ..WriteRequest::default()
        })
        .await
        .unwrap()
        .id
    }

    /// The version of `id` that the record holds now.
    async fn version_now(m: &Memory, id: &str) -> String {
        let text = m.store.get_text("dom-me", id).await.unwrap().unwrap();
        dm_core::digest(&text)
    }

    /// Give `id` a new version at `label`, as a text editor and the feed would.
    async fn relabelled(m: &Memory, id: &str, label: Label) {
        let mut e = m.store.get("dom-me", id).await.unwrap().unwrap();
        e.front.classification = label;
        let put = m.put_logged(&e).await.unwrap();
        m.feed_from("dom-me", put).await.unwrap();
    }

    /// The `get` lines that the session of `m` left in the read log.
    fn gets(dir: &Path, m: &Memory) -> Vec<reads::Read> {
        let all = reads::all(dir).unwrap().into_iter();
        all.filter(|r| r.tool == "get" && r.session == m.session_id())
            .collect()
    }

    #[tokio::test]
    async fn a_malformed_address_is_refused_with_the_three_forms() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let id = wrote(&m, BODY, Label::Public).await;
        for bad in [
            format!("{id}#First-step"),
            format!("{id}@abc"),
            format!("{id}#first-step@{}", version_now(&m, &id).await),
            format!("{id}/first-step"),
        ] {
            let refused = m.fetch(&bad).await;
            assert!(
                matches!(&refused, Err(Error::Denied(why)) if why.contains("`<id>#<anchor>`")),
                "{bad}: {refused:?}"
            );
        }
        assert!(gets(dir.path(), &m).is_empty());
    }

    #[tokio::test]
    async fn a_fetch_of_a_section_serves_that_section_and_the_read_log_counts_its_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let id = wrote(&mem(dir.path()), BODY, Label::Internal).await;
        let reader = agent(dir.path(), "agent:r");
        let whole = reader.fetch(&id).await.unwrap();
        assert_eq!(whole.entry.body, BODY);
        assert!(whole.current && whole.anchor.is_none());
        assert_eq!(whole.version, version_now(&reader, &id).await);
        let other = agent(dir.path(), "agent:s");
        let part = other.fetch(&format!("{id}#first-step")).await.unwrap();
        let section = "## First step\n\nCopy the file to the staging host and check its digest there.\n\n### A detail\n\nThe digest is SHA-256.\n\n";
        assert_eq!(part.entry.body, section);
        assert_eq!(part.anchor.as_deref(), Some("first-step"));
        assert_eq!((part.current, &part.version), (true, &whole.version));
        // the same checks as a fetch of the whole entry: the label and the read event
        assert_eq!(other.label(), Label::Internal);
        assert_eq!(read_in_log(&other, "dom-me"), [id.as_str()]);
        let (of_whole, of_part) = (gets(dir.path(), &reader), gets(dir.path(), &other));
        assert_eq!((of_whole.len(), of_part.len()), (1, 1));
        assert_eq!(of_part[0].ids, [id.as_str()]);
        assert_eq!(of_part[0].tokens, dm_core::approx_tokens(section));
        assert!(
            of_part[0].tokens < of_whole[0].tokens,
            "{} and {}",
            of_part[0].tokens,
            of_whole[0].tokens
        );
    }

    #[tokio::test]
    async fn an_unknown_anchor_is_refused_with_the_anchors_of_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let id = wrote(&mem(dir.path()), BODY, Label::Internal).await;
        let reader = agent(dir.path(), "agent:r");
        let refused = reader.fetch(&format!("{id}#nope")).await;
        let Err(Error::NotFound(why)) = &refused else {
            panic!("{refused:?}");
        };
        assert!(
            why.ends_with("its anchors are: import, first-step, a-detail, second-step"),
            "{why}"
        );
        // the entry was read to find its anchors
        assert_eq!(reader.label(), Label::Internal);
        assert_eq!(read_in_log(&reader, "dom-me"), [id.as_str()]);
        assert!(gets(dir.path(), &reader).is_empty(), "nothing was served");
        let plain = wrote(&reader, "No heading here.\n", Label::Internal).await;
        let refused = reader.fetch(&format!("{plain}#nope")).await;
        assert!(
            matches!(&refused, Err(Error::NotFound(why)) if why.ends_with("has no heading")),
            "{refused:?}"
        );
    }

    #[tokio::test]
    async fn a_fetch_by_version_serves_the_old_text_after_the_entry_got_a_new_version() {
        let dir = tempfile::tempdir().unwrap();
        let owner = mem(dir.path());
        let id = wrote(&owner, BODY, Label::Public).await;
        let first = owner.fetch(&id).await.unwrap();
        assert_eq!(first.entry.front.trust_tier, TrustTier::Raw);
        let curator = agent(dir.path(), "agent:curator");
        curator
            .promote(&id, TrustTier::Curated, "checked")
            .await
            .unwrap();
        let reader = agent(dir.path(), "agent:r");
        let now = reader.fetch(&id).await.unwrap();
        assert_eq!(now.entry.front.trust_tier, TrustTier::Curated);
        assert_ne!(now.version, first.version);
        assert!(now.current);
        let old = reader
            .fetch(&format!("{id}@{}", first.version))
            .await
            .unwrap();
        assert_eq!(old.entry, first.entry, "the text as it was");
        assert_eq!((old.current, &old.version), (false, &first.version));
        assert!(old.related.is_empty() && old.redirected_from.is_empty());
        let same = reader
            .fetch(&format!("{id}@{}", now.version))
            .await
            .unwrap();
        assert_eq!((same.current, &same.entry), (true, &now.entry));
        // a digest that no version has
        let none = reader.fetch(&format!("{id}@{}", "0".repeat(64))).await;
        assert!(
            matches!(&none, Err(Error::NotFound(why)) if why.contains("has no version")),
            "{none:?}"
        );
        assert_eq!(gets(dir.path(), &reader).len(), 3);
    }

    #[tokio::test]
    async fn a_fetch_by_version_follows_no_redirect() {
        let dir = tempfile::tempdir().unwrap();
        let owner = mem(dir.path());
        let old = wrote(&owner, "# Import\n\nFive steps.\n", Label::Public).await;
        let version = version_now(&owner, &old).await;
        let new = owner
            .write(WriteRequest {
                supersedes: Some(old.clone()),
                body: "# Import\n\nSix steps.\n".into(),
                ..WriteRequest::default()
            })
            .await
            .unwrap()
            .id;
        let reader = agent(dir.path(), "agent:r");
        let followed = reader.fetch(&old).await.unwrap();
        assert_eq!(followed.entry.front.id, new);
        assert_eq!(followed.redirected_from, [old.as_str()]);
        let section = reader.fetch(&format!("{old}#import")).await.unwrap();
        assert_eq!(section.entry.front.id, new);
        assert_eq!(section.redirected_from, [old.as_str()]);
        let by_version = reader.fetch(&format!("{old}@{version}")).await.unwrap();
        assert_eq!(by_version.entry.front.id, old);
        assert_eq!(by_version.entry.body, "# Import\n\nFive steps.\n");
        assert!(by_version.redirected_from.is_empty());
    }

    /// A root with two entries of two versions each: the first went from internal to
    /// public, the second from public to internal. The owner, then each id with the
    /// digest of its first version.
    async fn relabelled_both_ways(dir: &Path) -> (Memory, [(String, String); 2]) {
        let owner = mem(dir);
        let lowered = wrote(&owner, "# Lowered\n\nIt was internal.\n", Label::Internal).await;
        let was_internal = version_now(&owner, &lowered).await;
        relabelled(&owner, &lowered, Label::Public).await;
        let raised = wrote(&owner, "# Raised\n\nIt was public.\n", Label::Public).await;
        let was_public = version_now(&owner, &raised).await;
        relabelled(&owner, &raised, Label::Internal).await;
        (owner, [(lowered, was_internal), (raised, was_public)])
    }

    #[tokio::test]
    async fn a_version_above_the_handling_is_not_found_and_so_is_one_of_an_entry_above_it() {
        let dir = tempfile::tempdir().unwrap();
        let (_owner, entries) = relabelled_both_ways(dir.path()).await;
        for (id, version) in &entries {
            let capped = agent(dir.path(), "agent:low").with_handling_cap(Label::Public);
            let refused = capped.fetch(&format!("{id}@{version}")).await;
            assert!(
                matches!(&refused, Err(Error::NotFound(what)) if what == id),
                "{id}: {refused:?}"
            );
            assert_eq!(capped.label(), Label::Public);
            assert!(read_in_log(&capped, "dom-me").is_empty());
            assert!(gets(dir.path(), &capped).is_empty());
        }
        // the entry that is public now is served as it is now
        let capped = agent(dir.path(), "agent:low").with_handling_cap(Label::Public);
        assert!(capped.fetch(&entries[0].0).await.is_ok());
    }

    #[tokio::test]
    async fn a_read_of_a_version_raises_the_label_to_the_higher_of_the_two_and_is_one_read_event() {
        let dir = tempfile::tempdir().unwrap();
        let (_owner, entries) = relabelled_both_ways(dir.path()).await;
        for (id, version) in &entries {
            let reader = agent(dir.path(), "agent:r");
            assert_eq!(reader.label(), Label::Public);
            let served = reader.fetch(&format!("{id}@{version}")).await.unwrap();
            assert!(!served.current);
            assert_eq!(reader.label(), Label::Internal, "{id}");
            assert_eq!(read_in_log(&reader, "dom-me"), [id.as_str()]);
            let lines = gets(dir.path(), &reader);
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].ids, [id.as_str()]);
            assert_eq!(lines[0].tokens, dm_core::approx_tokens(&served.entry.body));
        }
    }

    #[tokio::test]
    async fn a_raw_version_is_not_served_under_a_filter_that_asks_for_curated_entries() {
        let dir = tempfile::tempdir().unwrap();
        let owner = mem(dir.path());
        let writer = agent(dir.path(), "agent:w");
        let id = wrote(&writer, "# Port\n\nIt listens on 8080.\n", Label::Public).await;
        let raw = version_now(&owner, &id).await;
        owner
            .promote(&id, TrustTier::Curated, "Checked.")
            .await
            .unwrap();
        agents_read(dir.path(), "\"min_tier\": \"curated\"");
        let reader = agent(dir.path(), "agent:r");
        assert!(reader.fetch(&id).await.unwrap().current, "curated now");
        let refused = reader.fetch(&format!("{id}@{raw}")).await;
        assert!(
            matches!(&refused, Err(Error::NotFound(what)) if *what == id),
            "{refused:?}"
        );
        assert_eq!(gets(dir.path(), &reader).len(), 1);
        // the owner is no agent, and reads the raw version
        let served = owner.fetch(&format!("{id}@{raw}")).await.unwrap();
        assert_eq!(served.entry.front.trust_tier, TrustTier::Raw);
    }

    #[tokio::test]
    async fn no_version_of_an_erased_entry_is_served() {
        let dir = tempfile::tempdir().unwrap();
        let owner = mem(dir.path());
        let id = wrote(
            &owner,
            "# Key\n\nThe deploy key is on the wiki.\n",
            Label::Public,
        )
        .await;
        let first = version_now(&owner, &id).await;
        let curator = agent(dir.path(), "agent:curator");
        curator
            .promote(&id, TrustTier::Curated, "checked")
            .await
            .unwrap();
        let second = version_now(&owner, &id).await;
        let receipt = owner.erase(&id, Reason::Mistaken).await.unwrap();
        let listed: Vec<&str> = receipt.removed.iter().map(|r| r.digest.as_str()).collect();
        assert!(
            listed.contains(&first.as_str()) && listed.contains(&second.as_str()),
            "{listed:?}"
        );
        let stub = version_now(&owner, &id).await;
        for digest in listed.into_iter().chain([stub.as_str()]) {
            let refused = owner.fetch(&format!("{id}@{digest}")).await;
            assert!(
                matches!(&refused, Err(Error::Gone(why)) if why.contains("was erased")),
                "{digest}: {refused:?}"
            );
        }
        assert!(gets(dir.path(), &owner).is_empty());
    }

    #[tokio::test]
    async fn a_version_of_a_withheld_entry_is_for_the_security_role_alone() {
        let dir = tempfile::tempdir().unwrap();
        let owner = mem(dir.path());
        let gone = wrote(&owner, "Written in error.\n", Label::Public).await;
        let reader = agent(dir.path(), "agent:r");
        reader.get(&gone).await.unwrap();
        let held = wrote(&reader, "Drawn from it.\n", Label::Public).await;
        let before = version_now(&owner, &held).await;
        owner.erase(&gone, Reason::Mistaken).await.unwrap();
        let withheld = version_now(&owner, &held).await;
        assert_ne!(before, withheld, "the entry is withheld as a new version");
        let stranger = agent(dir.path(), "agent:x");
        for version in [&before, &withheld] {
            let refused = stranger.fetch(&format!("{held}@{version}")).await;
            assert!(
                matches!(&refused, Err(Error::Gone(why)) if why.contains("withheld")),
                "{refused:?}"
            );
        }
        assert!(read_in_log(&stranger, "dom-me").is_empty());
        // the owner is of the security role
        let served = owner.fetch(&format!("{held}@{before}")).await.unwrap();
        assert_eq!(served.entry.body, "Drawn from it.\n");
        assert!(!served.current);
    }

    #[tokio::test]
    async fn a_fetch_of_the_current_version_carries_its_related_entries_with_their_weights() {
        let dir = tempfile::tempdir().unwrap();
        let owner = mem(dir.path());
        let a = wrote(
            &owner,
            "# Retry backoff\n\nOperation R retries.\n",
            Label::Public,
        )
        .await;
        let b = wrote(
            &owner,
            "# Jitter\n\nJitter is twenty percent.\n",
            Label::Public,
        )
        .await;
        let log = reads::ReadLog::new(dir.path(), "sess-earlier");
        for _ in 0..3 {
            let ids = [a.clone(), b.clone()];
            log.served_ids("search", Some("q"), 2, 10, false, None, &ids);
        }
        let reader = agent(dir.path(), "agent:r");
        let whole = reader.fetch(&a).await.unwrap();
        assert_eq!(whole.related.len(), 1, "{:?}", whole.related);
        assert_eq!(whole.related[0].hit.id, b);
        assert!((whole.related[0].p - 1.0).abs() < f64::EPSILON);
        let section = reader.fetch(&format!("{a}#retry-backoff")).await.unwrap();
        assert_eq!(section.related, whole.related);
    }
}
