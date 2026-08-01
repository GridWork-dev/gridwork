# Second-reader records

CLEANROOM.md rule 4: every change touching the paths in
[`.github/cleanroom-paths.txt`](../../../.github/cleanroom-paths.txt) gets an
independent fresh-context review before it reaches `main`. This directory is where
those reviews are recorded, and `cleanroom-gate` in CI is what makes skipping one
visible.

## The subject digest

A record is named after its **subject** — a SHA-256 over every gated path the change
touches paired with that file's blob hash at `HEAD`, plus the text of every registry
row those files' `Derivation:` markers cite:

```
docs/derivation/reviews/<subject>.md
```

Get it for the current branch:

```bash
git diff --no-renames --name-only origin/main...HEAD | ./tools/cleanroom-gate.sh --subject
```

(If the branch also edits `SPECS.md` or `CAPTURES.md`, the gate needs a base to diff
their rows against: prepend `CLEANROOM_BASE="$(git merge-base origin/main HEAD)"` to
the `cleanroom-gate.sh` end of the pipe. CI exports it on every run.)

The digest covers gated files and the rows they cite, so committing the record does not
change it — but editing any gated file afterwards does, and the gate goes red again.
That is the point: a review is bound to the content it actually read, and cannot be
carried across a rewrite of that content.

The registry half of a citation gets the same treatment, because a review checks that a
marker and its row *agree* — binding only the file half would leave the row free to move
under a record that vouched for the pair. Editing a row a gated file cites, even in a
change that touches no gated path, re-opens review of the files citing it: the gate
diffs registry rows against `CLEANROOM_BASE` and treats the citing files as touched,
and it refuses a registry edit it has no base to judge. A row nothing cites stays free
to add or fix — pre-seeding is meant to be cheap, and stays so.

Two scope notes, said plainly. The diff feeding the gate must be produced with
`--no-renames` (CI's is): default rename detection folds a modify-plus-rename into one
line naming only the new path, which is exactly how a registry edit would vanish from
the input — the reviewer of this mechanism defeated its first version that way. And the
binding covers table rows, not the prose around them: the sentences that govern how a
row is read stay rule 4's reader's job, like every other claim in this repository.

## Writing one

```bash
subject=$(git diff --no-renames --name-only origin/main...HEAD | ./tools/cleanroom-gate.sh --subject)
cp docs/derivation/reviews/TEMPLATE.md "docs/derivation/reviews/$subject.md"
```

Fill in `subject:`, `reviewer:`, and tick all four checks. Commit it in the same
branch. The gate verifies the file exists, that it declares the matching subject, that
it names a reviewer, and that all four checks are ticked — and, separately, that every
gated source file in the change carries a resolving `Derivation:` marker.

## What the reviewer does

Read the gated diff cold — without the implementing session's context — and check
exactly the four things rule 4 names:

1. **citations** — every non-obvious terminal behavior carries a `Derivation:` marker
   naming a permitted source by ID, from [`../SPECS.md`](../SPECS.md) (public
   specifications) or [`../CAPTURES.md`](../CAPTURES.md) (registered observations).

   The gate already proved every cited ID resolves and that no gated source file is
   unmarked, so do not re-check that by hand. **Check what the gate cannot:** that each
   marker sits on the behavior it describes, and that the cited source actually says what
   the code does. A marker citing a real spec for the wrong behavior passes the gate and
   fails this check — that is the case you are here for.
2. **framing** — no source-derived framing: no comment, name, or structure that reads as
   transcribed from another implementation rather than from a specification.
3. **references** — no other project named as a comparand, and no capture cited by path.
   Captures are referenced by registered ID and SHA-256, never otherwise.
4. **leak** — `./tools/leak-scan.sh` green.

A behavior with no citable permitted source is an escalation, not a guess.

## What this proves, and what it does not

It proves a review ran against this exact content, that its record was written at commit
time rather than reconstructed after a question was raised, and that every citation
resolves.

It does **not** prove the reviewer was independent. This repository has one maintainer;
GitHub does not permit a pull-request author to approve their own pull request, so a
required-review rule would not create a gate here — it would only make `main` unmergeable.
Independence is procedural: the reviewer is a fresh-context session with no exposure to
the implementing session's context. Claiming the narrower thing truthfully is worth more
than claiming a stronger control that a single-maintainer repository cannot deliver.
