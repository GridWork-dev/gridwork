# Capture registry

Observed-behavior captures cited by clean-room derivation notes (CLEANROOM.md rule 3).
Each capture has a stable ID and the SHA-256 of the raw capture. Scrubbed captures may
live in-repo as test fixtures in a text-safe encoding; unscrubbed ones are retained
privately and can be produced against their hash if a derivation is ever questioned.

Rows carry only the ID, hash, and an observable description. A storage path may be named
elsewhere in-tree for a capture of **public source**, and only while all of these hold:
the repository, revision and path are already a build input, the license is named, and
the hash — never the path — remains the citation of record. For anything else, and for
private recordings above all, the location is not written down. Two reasons, and the
second survives even where the first does not: a citation must not disclose where a
private capture is kept, and a path is not a stable identifier, so a claim resting on one
can come to point at different bytes without saying so.

IDs are `CAP-<nnn>`, allocated in order and never reused: a `Derivation:` marker in
shipped code cites one, so a recycled ID would silently re-point an existing citation at
a different observation. `cleanroom-gate` resolves every cited ID against this table or
`SPECS.md` and fails the build on one that is in neither.

| ID | sha256 | What it observably shows |
|---|---|---|
| `CAP-001` | `efb1138c4730af0cea8a0aa8e9a558c8c642227fa20ef529346c777cb4f2a043` | A public third-party VT fuzz harness. Its first input byte selects a parser code path and is not terminal input, and it drives a terminal built at 80×24 with 100 lines of scrollback. Both facts decide how the conformance corpus has to be replayed for its frames to describe the same terminal upstream tests. |

`CAP-001` takes the public-source clause: `crates/gwk-pty/fixtures/PROVENANCE.md` names
its repository, revision, license and path. The repository, revision and path are already
build inputs — `pins.env` pins them and the soak test reads the corpus through them — and
the license is named there because the clause requires it, not because the build consults
it. The hash above stays the citation of record, so "the harness says this" remains
falsifiable against bytes rather than against a location.
