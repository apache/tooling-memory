# crates/core

The entry format, and everything every other crate must agree on. No I/O: this crate
compiles to native, to wasm for the browser reader, and to WASI for plugins, so nothing
here may depend on a filesystem, a clock beyond `time`, or a database.

## The frontmatter is the contract

`Frontmatter` in `src/lib.rs` is the on-disk record. Every field name is read by every
backend, by the bundle manifest, and by whoever opens an entry in a text editor. So:

- **A new field is optional with a default.** Entries already on disk do not have it, and
  `parse` must still read them. `#[serde(default, skip_serializing_if = ...)]` is the shape.
- **A renamed or removed field is a format change.** It needs a migration path for existing
  records and a bump to `manifest::FORMAT`. Say so in PLAN.md under "Decisions already made"
  before writing it.
- **`parse(render(e)) == e` holds for every entry.** The `round_trip` test is the one that
  may never be weakened.
- **Orders are semantics.** `Label` and `TrustTier` derive `Ord`, and the filter, the
  export bound, and promotion all compare on it. Adding a variant in the middle changes
  what every existing entry means.

## Ids

Ids are stable, permanent, and meaningless. `id::new_entry_id` produces them; nothing
parses meaning out of one beyond the `mem-` and `task-` prefixes.

## Digests

`digest` is SHA-256 over the rendered text, hex. Manifests and stubs carry it. Changing the
hash is a format change.
