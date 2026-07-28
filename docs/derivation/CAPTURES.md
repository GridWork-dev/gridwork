# Capture registry

Observed-behavior captures cited by clean-room derivation notes (CLEANROOM.md rule 3).
Each capture has a stable ID and the SHA-256 of the raw capture. Scrubbed captures may
live in-repo as test fixtures in a text-safe encoding; unscrubbed ones are retained
privately and can be produced against their hash if a derivation is ever questioned.
Rows carry only the ID, hash, and an observable description — never storage paths.

| ID | sha256 | What it observably shows |
|---|---|---|
| — | — | (none yet — first entries land with the PTY engine) |
