# AGENTS.md

See **[CLAUDE.md](CLAUDE.md)**. Everything in it applies here, unchanged, whatever
engine is reading this.

It is one file rather than two because the first rule in it is a legal constraint —
do not read AGPL-licensed terminal-multiplexer source — and a warning maintained in
two places is a warning that eventually disagrees with itself. A pointer cannot drift.

Human contributors want [CONTRIBUTING.md](CONTRIBUTING.md) instead.

## GridWork fleet graph rails

Code-structure questions: graph first, grep last.

- **Fleet knowledge graph** (pushed origin/main, all 14 repos): the `graphify` MCP —
  `search` with `mode="dense"` (default; right for questions/descriptions) or
  `mode="lexical"` (exact symbol/filename/path only). Never `rerank`/`hybrid`
  (refuted vs dense on the live index, 2026-08-20). Node ids are `<repo>::`-prefixed;
  single-repo questions pass `repo="<repo>"`. The graph is already built (rebuilt every
  15 min from origin/main) — one tool call, no build step.
- **Live branch / uncommitted structure**: the `codebase-memory` MCP (`trace_path`,
  `query_graph`, `search_graph`) — local branches and worktrees are invisible to the
  fleet graph.
- **Grep/Read are for literal text**, not structure (callers, dependents, symbol paths).
