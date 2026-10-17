# DR-19: Does the wiki stay on MoinMoin?

## Status

Proposed. No decision has been taken: the wiki stays on MoinMoin until the S2 spike says
otherwise.

## Context

The project wiki runs on MoinMoin. Its pages are plain files, which the backups and the
mirror rely on, and its search matches substrings. MoinMoin cannot rank full text without
Xapian beside it, and nobody here runs Xapian. Contributors say they cannot find pages they
know exist, and new contributors ask questions the wiki already answers.

## Options

- **A. Stay on MoinMoin and run Xapian beside it.** Ranked search for one more service to
  operate; the pages stay plain files.
- **B. Move to a wiki on PostgreSQL.** Ranked full-text search comes with the database,
  at the cost of a migration and of pages no longer being files anyone can read.
- **C. Stay on MoinMoin and index the pages elsewhere.** The wiki is untouched; search moves
  to a separate page that can drift from what the wiki holds.

## What settles it

Spike S2 in the work plan: index a copy of the wiki with Xapian and with PostgreSQL's
full-text search, search each page by its own title and by the questions contributors
asked last quarter, and count how often the page they wanted lands in the top three. If
ranking alone closes the gap, A or C; if it does not, B.
