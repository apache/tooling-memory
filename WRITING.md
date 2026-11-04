# Writing the paper

How *Durable Memory for Disposable Agents* and its case study are kept readable. The rules
below were recovered from the commits that shortened the paper in October 2026, from
13,000 words of body to 8,700, and from 27 paragraphs over 100 words to none. Each rule
gives one example from those commits, before and after, so that the next pass can tell
the rule from a taste. The last section is the procedure a pass runs.

The paper keeps this style; the documents written for the repository follow AGENTS.md and
[docs/terms.md](docs/terms.md) instead.

## Structure

**The abstract is three sentences: the problem, the requirement, "This paper proposes
how."** No list of mechanisms.

> *Agent fleets need shared memory, and shared memory spreads mistakes and leaked secrets as easily as knowledge. It has to be cheap to read, safe to share, and outlast every model and tool it runs on. This paper proposes how.*

**Every section and every role opens with why it exists, labelled `**Why.**`** A section
whose first paragraph is a mechanism has lost its reason. Where none can be written, the
section may not belong.

> − Organisations rename, split, merge, and hand names to new owners. ...
> + **Why.** Organisations rename, split, merge, and hand names to new owners. ...

**Overview first, then one section per part, and no second summary.** One "How to read
this paper" paragraph; §2 under two printed pages; no Summary, Conclusion, or reader paths.
The body for a part appears in one section only.

**Example before rule.** When a section's rules take more than one paragraph, the first
thing after `**Why.**` is `**An example.**`, a diagram, or both, and the rules follow. The
example must be readable with no term from the rules after it.

> + **An example first.** An agent reads a thread on a private security list, then tries to post a summary to a public issue tracker. The post is refused, because the session has read restricted material and the tracker takes only public content. ...
> + The perimeter enforces one rule for each direction: ...

**Detail goes to an appendix, and the body keeps the rule and its reason.** A schema in the
body shows only the fields the prose discusses. Every "in detail", "exact", and "full"
lives in an appendix, and one sentence in the body points there.

> + Content moves to a lower label only through `declassify`, and a guard named in policy approves each one. ... Appendix E gives the rules in detail: who may attest handling, tool servers that are also sources, grants, and who guards which crossing.

**Parallel facts go in a table with a Why column.** Three or more items that share the
same two or three attributes are a table. A rules table has a Why column; a milestone has
two lists, "Delivers" and "Proven when".

> | Rule | What it says | Why |
> | **Leases** | A claim expires unless renewed | A crashed agent must never hold work hostage |

**Cut what only repeats something said elsewhere.** For each section, name one thing a
reader learns there and nowhere else. If nothing, cut it and redirect its references.

> Build Order added little the evaluation and survival sections do not already say; remove it and point its few references at §11 instead. (commit 29ff584, 1,200 words)

**A term is explained where it first appears, and the glossary holds the definition.** The
first occurrence carries an appositive ("the harness, the program that runs the model") or
a link to the section that defines it.

**Citations are numbered links; every section reference is a link.** No author-year in
running text. Every `§n` and every "Appendix X" is inside a link that resolves.

**A diagram stays when an example in prose walks through it.** The paper went from
fourteen diagrams to seven in October 2026, and has nine now. Each one tells one story: the
system at a glance, a handoff through the write path, a label crossing, the curation loop,
one server per session, a task's life, one task with three claimants, the agent's life, the
improvement loop.

## Sentences

**No paragraph over about 100 words.** A sentence with three or more parallel items and
more than 25 words becomes a list.

> − Ordinary output carries hazards too: secrets the model has seen (...), instructions planted in something it read, code that runs when loaded, and packages that do not exist, which an attacker can then register. ...
> + - secrets the model has seen (...);
> + - instructions planted in something it read;
> + - code that runs when loaded;
> + - packages that do not exist, which an attacker can then register.

**The actor leads.** The thing that acts is the subject, in the active voice. A vague agent
("the system", "each session") becomes the named part.

> − Confirmed drift is quantified by replay.
> + Replay quantifies confirmed drift.

> − Auditors are checked in turn by immutable history, deterministic tests, and sampled human review.
> + Immutable history, deterministic tests, and sampled human review check the auditors in turn.

**Short sentences in plain words, one idea each.** A term of art is rewritten as what
happens, or defined. Targets from the October paper: mean sentence under 21 words, nine in
ten under 37, none over 45. ASD-STE100 asks for 25 in a descriptive sentence; the November
2026 pass adopted that bound for new text.

> − A provider's cache matches an exact prefix for one model and lasts minutes, so T0 is byte-stable: its slices come in a fixed order, ...
> + A provider reuses its cache only when the start of a request is identical, for the same model, within minutes. So T0 is assembled the same way every time: its slices in a fixed order, ...

**Concrete beats general.** A sentence that names the thing (the laptop, the text editor,
the tracker comment) is kept over one that names the category (stored text, the parts).

> − **Reading costs.** Processing stored text costs tokens and attention.
> + **Reading costs.** Agents pay for every token they read; storage is nearly free.

**A number carries its conditions, or it goes.** Every figure with a citation says "in one
study", the benchmark, or the population in the same sentence, and the source's full text
was opened. See the memory note on verifying against full text.

> − This happened in 48 of 49 tested configurations; storing an authority label with the claim removed the unauthorised actions that followed.
> + Keeping those labels with the claim prevents the misuse that follows.

**A heading names the thing; the argument goes in the text.** No "not", "rather than",
"and why", or semicolon in a heading or a bold lead. A labelled question replaces a
rhetorical one.

> − ## Prices, and why not build in the token
> + ## Cost and throughput

**One bold label per paragraph, at its start, from a fixed set:** `**Why.**`, `**An
example.**`, `**Read:**` / `**Write:**` / `**Send:**`, `**Delivers:**` / `**Proven when:**`,
and one bold phrase per principle or bullet.

## Tells

These are removed on the lines a change adds. They are the marks of text written by a model
or of a discussion restated as prose, and each has an instance from the commits.

**Contrast for effect.** "X, not Y", "rather than", "instead of", "never ... but". Delete
the negative half; if the sentence still says the rule, the contrast was for effect. Keep a
"not" or a "never" only where the negation is the rule: "Trust is not transitive", "Nothing
raw runs", "Nothing leaves by default".

> − The next agent starts from that note, not from a transcript.
> + The next agent starts from that note.

> − Canaries catch bad edits, not a dormant payload, which can wait through a hundred sessions before it acts
> + A dormant payload alters no canary, and it can wait through a hundred sessions before it acts

**Narration of the discussion that produced the text.** A sentence that answers an
objection nobody in the text raised, or restates a decision ("the caller states facts, not a
length"). Cut the clause; keep the rule and the reason.

> − The caller states facts, not a length, and the decision is logged for replay.
> + The decision is logged for replay.

**Lead-ins that only count what follows.** "Three findings shape this section." is cut when
the list has bold leads that carry it.

**The aphorism and the closing flourish.** A short sentence at the end of a paragraph that
restates it; a semicolon joining two clauses that mirror each other.

> − Hard work should be routed, not dropped or half done
> + Hard work should reach someone who can do it

**Nominalisations and vanished actors.** "Judgment is exercised", "is inherited by",
"humans".

> − Limits are enforced by a policy engine; judgment is exercised by agents and leaves a trail.
> + A policy engine enforces the limits; agents use judgment inside them and leave a trail.

**A company or product as the authority.** Named deployments appear in §13 and the
references; the body cites them by kind ("one personal-workspace deployment") and every
rule keeps its own reason.

> − At Meta a personal workspace of this kind ... reached 63,000 installs
> + One industry deployment of a personal workspace of this kind ... reports 63,000 installs

**Idiom and figure.** Read each verb and ask whether the subject can literally do it.
"Follow the lean of" became "go along with"; "(O(n²) messages)" became "which grows with the
square of the fleet".

## Consistency

**One word for one thing, in every section.** A rename touches every occurrence and the
case study in one commit. The role went librarian, archivist, curator; each time everywhere.

**The appendix signature and the body agree.** Every operation the body names appears in
Appendix A with the same parameters and return fields, and the body links to the signature
rather than restating it.

**A rule stated twice says the same thing, and the second place is a pointer.** List every
place a rule appears (body, appendix, failure-mode table, case study) and diff them by
hand. The reader test's "two passages that disagree" is the check.

**A cross-reference points to where the claim is made**, not to a mention of it.

**A statement about the document is true of the document.** After a structural change,
re-read "How to read this paper", the note on reference order, and every "Appendix X
gives".

**No state, part, or term that nothing reaches.** Every state in a schema appears in the
diagram or a rule; every named part is in the §2 roles table or the glossary.

**The case study follows the paper's numbering and terms in the same commit.**

## The pass

One section at a time, in this order: structure first, because moving text changes every
count; tells last, because they are cheapest to reintroduce while rewriting.

1. **Measure.** On the body prose (no code fences, tables, headings, or citation links):
   words; sentences; mean, ninetieth-percentile, and longest sentence; paragraphs over 100
   words; counts of "not", "rather than", "instead of", "ensur", em-dashes, and
   parentheticals over fifteen characters; exact-duplicate sentences; and shared eight-word
   runs between sections. The script below prints them.
2. **Structure.** Abstract; `**Why.**`; example or diagram before rules; tables for
   parallel rules; detail to the appendix with one pointer; cut duplicates and redirect
   their references; terms defined at first use; every reference a link. Record in the
   commit message what moved where.
3. **Sentences.** Split every paragraph over 100 words; lists for comma series; actor
   first; plain words; numbers with conditions; headings that name the thing.
4. **Tells**, on the diff of added lines only.
5. **Consistency**, on the whole file and the case study.
6. **Reader round**, after the push and before the pull request, as
   [`.claude/commands/reader-test.md`](.claude/commands/reader-test.md) describes. For
   a pass that adds no facts, also put the same questions to a reader of the text before
   and after, and record certainty and the number of reads per answer. The after may be
   no worse on any question.
7. **Measure again**, and put the before and after numbers, the questions, and the
   reader's result in the pull request.

```python
#!/usr/bin/env python3 -I
"""Prose metrics for a Markdown paper. Usage: python3 -I metrics.py <file.md>"""
import re, sys, statistics, collections
t = open(sys.argv[1], encoding='utf-8').read()
body = re.sub(r'```.*?```', '', t, flags=re.S).split('\n# References')[0]
paras = [x for x in re.split(r'\n\s*\n', body)
         if x.strip() and not x.lstrip().startswith(('|', '#', '-', '*', '>'))
         and not re.match(r'\s*\d+\. ', x)]
prose = [l for l in body.split('\n') if l.strip() and not l.startswith(('|', '#'))]
text = re.sub(r'\[\[\d+\]\]\(#ref-\d+\)', '', ' '.join(prose))
text = re.sub(r'\([^)]*§[^)]*\)', '', text)
words = len(re.findall(r"[A-Za-z0-9'’-]+", text))
sents = [s for s in re.split(r'(?<=[.!?])\s+(?=[A-Z*"(`])', text) if len(s.split()) > 1]
lens = sorted(len(s.split()) for s in sents)
pw = [len(x.split()) for x in paras]
low = [s.strip().lower() for s in re.split(r'(?<=[.!?])\s+', ' '.join(paras)) if len(s.split()) > 6]
dup = sum(v - 1 for v in collections.Counter(low).values() if v > 1)
c = lambda pat: len(re.findall(pat, text))
print(f"words={words} sentences={len(sents)} mean={statistics.mean(lens):.1f} "
      f"p90={lens[int(len(lens) * 0.9)]} max={max(lens)} over25={sum(1 for l in lens if l > 25)}")
print(f"why_openers={t.count('**Why.**')} paras={len(paras)} paras>100w={sum(1 for w in pw if w > 100)}")
print(f"not={c(r'\bnot\b')} rather_than={c(r'\brather than\b')} instead_of={c(r'\binstead of\b')} "
      f"ensur={len(re.findall(r'\bensur', text, re.I))} emdash={text.count('—')} "
      f"parens15={c(r'\([^)]{15,}\)')} dup_sentences={dup}")
```
