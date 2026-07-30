# Second-reader records

CLEANROOM.md rule 4: every change touching the paths in
[`.github/cleanroom-paths.txt`](../../../.github/cleanroom-paths.txt) gets an
independent fresh-context review before it reaches `main`. This directory is where
those reviews are recorded, and `cleanroom-gate` in CI is what makes skipping one
visible.

## The subject digest

A record is named after its **subject** — a SHA-256 over every gated path the change
touches, paired with that file's blob hash at `HEAD`:

```
docs/derivation/reviews/<subject>.md
```

Get it for the current branch:

```bash
git diff --name-only origin/main...HEAD | ./tools/cleanroom-gate.sh --subject
```

The digest covers only gated files, so committing the record does not change it — but
editing any gated file afterwards does, and the gate goes red again. That is the point:
a review is bound to the content it actually read, and cannot be carried across a
rewrite of that content.

## Writing one

```bash
subject=$(git diff --name-only origin/main...HEAD | ./tools/cleanroom-gate.sh --subject)
cp docs/derivation/reviews/TEMPLATE.md "docs/derivation/reviews/$subject.md"
```

Fill in `subject:`, `reviewer:`, and tick the three checks. Commit it in the same
branch. The gate verifies the file exists, that it declares the matching subject, that
it names a reviewer, and that all three checks are ticked.

## What the reviewer does

Read the gated diff cold — without the implementing session's context — and check
exactly the three things rule 4 names:

1. **citations** — every non-obvious terminal behavior carries a derivation citation:
   a public specification (ECMA-48, XTerm ctlseqs, the Kitty keyboard protocol, ACP, …)
   or a capture registered in [`../CAPTURES.md`](../CAPTURES.md) by ID and SHA-256. Each
   citation must resolve — a dead ID is a failed check, not a nit.
2. **framing** — no source-derived framing: no comment, name, or structure that reads as
   transcribed from another implementation rather than from a specification.
3. **leak** — `./tools/leak-scan.sh` green, and no capture cited by an estate path
   instead of its registered ID.

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
