# crates/core

This crate holds the entry format, and all that every other crate must agree on. It has no
I/O. This crate compiles to native code, to wasm for the browser reader, and to WASI for
plugins. Thus nothing here may depend on a filesystem, on a clock other than `time`, or
on a database.

## The frontmatter is the contract

`Frontmatter` in `src/lib.rs` is the on-disk record. Every backend reads every field name.
The bundle manifest reads them too, and so does anyone who opens an entry in a text
editor. Thus:

- **A new field is optional and has a default.** Entries that are already on disk do not
  have the field, and `parse` must still read them. Use the shape
  `#[serde(default, skip_serializing_if = ...)]`.
- **A renamed or removed field is a format change.** It needs a migration path for
  existing records, and it needs a bump to `manifest::FORMAT`. Before you write the
  change, say so in PLAN.md under "Decisions already made".
- **`parse(render(e)) == e` holds for every entry.** The `round_trip` test is the one that
  you may never weaken.
- **The orders have meaning.** `Label` and `TrustTier` derive `Ord`. The filter, the
  export bound and promotion all compare with that order. If you add a variant in the
  middle, you change what every existing entry means.

## Ids

Ids are stable and permanent, and they have no meaning. `id::new_entry_id` makes them.
Nothing parses meaning out of an id, except the `mem-` and `task-` prefixes.

## Digests

`digest` is the SHA-256 of the rendered text, in hex. Manifests and stubs carry it. A
change to the hash is a format change.
