# DR-22: The format memory entries travel in

## Status

Decided.

## Decision

Memory entries travel as a directory of Markdown files, one per entry with its metadata in
YAML frontmatter, plus a manifest that lists every file with its digest. Export writes one;
import reads one and refuses an entry whose digest does not match its manifest line before
anything is written.

## Why

A directory of Markdown reads without tools, diffs and reviews like code, and survives the
software that wrote it. The manifest makes every entry in a bundle verifiable, so an altered
copy is caught at the door rather than discovered later in the record.

## Alternatives considered

A single JSON Lines file is easier to stream but needs a tool to read and a tool to review.
A database dump ties the format to one engine and one version of it.
