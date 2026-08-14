# Working in this repo with an AI agent

Human contributors: [CONTRIBUTING.md](CONTRIBUTING.md) is the document you want. This
one adds only what an agent gets wrong here, and it starts with the rule that is not
negotiable.

---

## Read this before touching anything

**Do not read, search, or fetch the source of any AGPL-licensed terminal multiplexer —
before or during work on this repository.**

This project independently implements terminal multiplexing in a domain where the
established projects are copyleft. That independence is a legal position, not a
preference, and it is enforced two ways: the paths in
[`.github/cleanroom-paths.txt`](.github/cleanroom-paths.txt) require a derivation
citation to a public specification or a recorded capture, and every change to one gets
an independent second review by a reader who has not seen the implementer's reasoning.
The full procedure is [CLEANROOM.md](CLEANROOM.md).

An agent that reads a copyleft multiplexer's source because nobody told it not to is
the worst outcome available in this repository, and it is not undoable — a fresh
implementer cannot be un-exposed. If a task seems to require it, **stop and ask**
rather than reasoning about whether it counts.

Behaviour comes from published specifications and observed wire behaviour. Those are
the two permitted sources, and `docs/derivation/SPECS.md` is the registry of them.

---

## Gates that fail in ways you will misread

**`./tools/cleanroom-gate.sh` reads a diff on stdin.** Run bare, it inspects nothing
and prints a clean verdict for any change at all. Give it the real thing:

```bash
git diff --cached --no-renames | ./tools/cleanroom-gate.sh
```

**Never `--workspace` or `--all-features` on cargo.** Six crates are outside
`default-members` for toolchain reasons — `--workspace` overrides that and pulls in a
crate whose build wants Zig and a ghostty checkout. Each excluded crate has its own CI
job. `Cargo.toml:2-26` explains which and why. (The `package` CI job is the one
deliberate exception, with explicit `--exclude`s.)

**Editing prose can red a gate in a file you did not open.**

- `README.md`, `docs/*.md`, or anything under `docs/derivation/reviews/` reds the docs
  freshness gate until the site mirror is re-curated. Three things move together: the
  mirror prose, the `curated-from` sha256 *inside* the mirror, and the page sha256 in
  `site/content/source-map.json`. Run `cd site && bun scripts/check-doc-freshness.ts`.
- `./tools/check-claims.sh` pins claims that appear in more than one file. Changing the
  stage number in `README.md` alone goes red until the site, `ROADMAP.md`, and the
  threat model agree.

**Editing `gwk-domain` or `gwk-theme` obliges you to regenerate the contract** in the
same commit — `cargo run -p xtask -- contract`. `bun test` in `contracts/` rewriting
`goldens-ts/` is the test working, not a mess to revert.

**One required check.** Branch protection requires `verify`, which aggregates 17 of the
20 jobs in `ci.yml`. If your PR is red, open `verify`'s log — it names the gate. The
other three jobs are `continue-on-error` by design and cannot fail anything;
`advisories` in particular can be red on an unchanged PR.

---

## Commits

[Conventional Commits](https://www.conventionalcommits.org), one logical change each,
and a formatting-only reflow is always its own commit.

Disclose AI assistance with a non-authorship trailer on the commits it applies to:

```
AI-Assisted-By: <tool>
```

**Never** list an AI tool as an author or co-author, and never put session URLs,
absolute home paths, or machine identifiers in a commit message — CI scans commit
messages and every individual commit patch, not just the final tree, and a value added
and later removed does not clear the gate.

## When you add a guard, prove it can fail

Every gate here ships with a mutation check: revert the fix, confirm the test goes red,
restore it. CI does this to its own scanners — the leak scanner is fed seeded
violations and the build fails if it *accepts* one. A guard nobody has watched fail is
a guard nobody has tested.

The related trap, which has bitten this repo more than once: a fold cannot tell "summed
to zero" from "summed over nothing". `all()` over an empty set is true, a `grep -c` over
no input is 0, and a typechecker with no files exits 0. When a check aggregates, assert
the **count** first and let it decide.
