# The terminal workspace GRILL — Phase A design record

Scope: the whole terminal surface as one system — Hall/estate, the Board's ten views,
Queue, Config, drilldown/attach, the event tail, and `gw`'s human CLI output. This file
is the design rationale for the mockup rounds that follow; the artifacts of record are
the `mock-*` goldens beside it. Grid contract used throughout: **design at 120×40,
floor at 80×24**; text budgets are character counts.

Evidence base: a full read of `gwk-tui/src/{hall,estate,board,queue,config,drilldown,
input,replay,probe,seven_act,runtime,lib,theme}.rs`, `gwk-theme/src/*`,
`gwk-domain/src/*`, and `gridwork/src/{args,lib,tui,client,admin,pr,exit}.rs`.
(The workspace multiplexer sources are under active development in separate lanes and
were not read; this document designs their IA target from the outside.)

---

## 1. The IA thesis — what the workspace IS

**The workspace is a single ledger console: one process, one estate, many lenses over
the same projections.** Every surface the terminal shows is a reader of the same
event-sourced kernel; the console's job is to make that one estate navigable, honest,
and actionable without ever leaving it.

Today that system exists as three disconnected raw-mode processes (`gw tui`,
`gw event tail`, `gw session attach`) plus twenty-odd one-shot JSON verbs, with no path
between any of them, ~2,500 lines of complete-but-unreachable lens code, and 10 of 19
projection kinds visible only as raw JSON. The redesign's structural move is to
collapse that accident into a deliberate shape:

1. **One process.** `gw tui` starts the workspace. `gw event tail` and `gw term attach
   <id>` become deep links that open the same workspace focused on the named lens
   (piped/non-tty invocations keep the one-shot snapshot behavior). The three event
   loops and their separate kernel subscriptions merge.

2. **Two lens classes.**
   - **Home — the Hall.** The at-rest estate: districts, stations, agents. The only
     ambient/motion surface, and the screen the console opens on.
   - **Lists — everything else.** Queue, fleet, terms, sessions, cost, events, flow,
     tasks/dag, runs, audit, config: each a typed, filterable list sharing ONE row
     grammar (accent · indent · mark · text · tail), one `/` filter, one detail pane,
     one keybar. The Board dissolves: its ten views were this list taxonomy all along,
     trapped behind a single enum with one wired case. A lens's ruled column set and
     the matching CLI verb's table are the same decision, made once per projection
     kind.

3. **Navigation.** `:` is the navigator — `:fleet`, `:cost`, `:term`, `:queue`…, with a
   live-preview picker on bare `:`. `/` filters the current lens. Enter drills
   (row → detail → attach when the row bears a terminal); Esc ascends; a home key
   returns to the Hall. No lens is more than two keystrokes from any other.

4. **Where sessions live.** Terminal lifetimes (`gw term`) are estate objects: rows in
   the term lens, drill targets wherever an agent shows. Attach happens inside the
   workspace, in VIEW mode by default with an explicit INPUT mode for the ruled raw
   send path — every send receipted, receipts rendered in-lens and echoed by the pty.

5. **Keybar honesty.** The bottom row always shows the legal keys for the current
   focus, lazygit-style. Overflow elides with an explicit continuation mark — never the
   current behavior of dropping the whole hint block. Mode and degradation state
   (INPUT/VIEW, motion off, color tier, glyph set) are permanent citizens of the bar:
   silent degradation becomes visible state.

6. **Honest states.** Every lens ships empty / lightly-loaded / dense / degraded
   (`Ascii` + `Mono` + `Ansi16`) goldens. Loading is distinct from empty (today a
   not-yet-connected console and an idle estate render identically). Partial reads say
   "at least N". Open-vocabulary values render an unknown-value fallback, never a
   crash or a blank. Dropped tails, dropped hints, and silent tier downgrades are all
   replaced by explicit marks.

Below the 80×24 floor a lens renders a "too small" card instead of shearing. The
hosted-frame publish ceiling (~13.8k cells in a 2 MiB frame) comfortably admits any
plausible grid (120×40 = 4,800 cells).

## 2. Fork list

Genuine forks put to the operator as pickers (F-numbers continue the input-path SPEC's
F1–F5; that SPEC ruled F1 raw-passthrough, F2 operator+orchestrator grants, F3
one-receipt-per-send, F5 receipt-row-plus-echo):

- **G1 — Lens taxonomy.** Flat `:name` namespace (Board dissolves) vs two-level
  (Board survives with sub-views) vs a curated five-lens grouping.
- **G2 — Attach posture.** Full-screen takeover vs main-region-with-estate-rail vs
  floating pane over the home.
- **G3 — Command surface.** `:` for navigation only + single-key context verbs, vs
  full typed verbs (`:stop attempt-42`), vs no command mode at all.
- **G4 — CLI human output.** Auto-table on TTY / JSON when piped, vs flag-gated
  tables, vs JSON-only forever.
- **F4 — Send-mode UX** (owed to the input-path SPEC): modal INPUT with a mode badge,
  vs a `:send` one-shot verb, vs both from day one.
- **G5 — Hall at-rest direction.** Enriched ambient field vs composite home with
  fixed regions (attention digest + vitals) vs status quo minimalism.
- **G6 — Queue/Config wire-or-shelve.** Recommendation argued in §4.

Designer-decided defaults (taken, vetoable): the 120×40/80×24 grid contract with an
explicit too-small card; keybar elision over wholesale dropping; urgency ordering
applied at every density rung (not only at Paging); real elapsed times replacing the
hardcoded "live" text; a visible loading state; color-tier/glyph-set visibility in the
bar; selection/focus painted with the already-ratified `focus`/`selection` tokens.

## 3. Question storm

Curated per surface; each is a question a mockup round must answer, with the evidence
that raised it.

### Hall / estate
- Focus is data with no paint: `render_frame` never styles `input.focus`, and the live
  path hardcodes `focus: None` (estate.rs:348). What does focused-district and
  selected-agent treatment look like at each tier, given Ansi16/Mono reserve
  reverse-video for exactly this?
- The 6-name identity whitelist plus kind-unscoped `mark()` lookup means real `gw-*`
  roles collapse to a letter and colliding names would render unrelated glyphs
  (hall.rs:1384-1397). After the taxonomy fix, what does WHO actually show — glyph,
  letter, or glyph+short-role on wider rungs?
- `Agent.duration` is the literal string "live" (estate.rs:299-307) — ~7 cells of
  static text on every running agent. Elapsed time, and at which rungs does it survive?
- PairShrink strips WHO from every agent — attention-pinned included — before quiet
  collapsed districts give up their summary lines (hall.rs:702-708 vs :395-414). Is
  losing identity everywhere the right trade against peripheral detail?
- Urgency ordering exists only inside the Paging rung (hall.rs:1184-1244); at calmer
  rungs a NeedsAttention agent can sit after Done agents in ID order. Should
  `attention_rank` order every rung?
- Districts are fixed 3-row horizontal strips that never wrap; a full 7-act project
  triggers the density ladder almost immediately. May busy districts grow vertically?
- Only agents are hit targets (`HallTarget` has one variant); collapsed districts and
  task-level attention items can't be clicked or keyed. What are the missing targets'
  affordances?
- Empty and not-yet-loaded are indistinguishable (hall.rs:1532-1554). What does the
  connecting state show?
- Attempt carries engine, model lane, budget (4 axes), exit code, evidence ref — none
  reach the card. Which of these earn cells at which rung, and what's the budget-
  pressure treatment (the one signal with a deadline attached)?
- District/Attention motion can only Pulse (`admits()`, hall.rs:802-806). Does the
  richer at-rest Hall need any new motion, or does the existing verb set cover it?

### Board (as the list-lens family)
- Ten views share one flat next/previous ring and a 10-name tab strip that shears
  silently at narrow widths (board.rs:3255-3284). What replaces the strip once the
  taxonomy is ruled — and what marks a lens as having attention inside it?
- Only Attempt rows carry actions (stop, budget) out of 14 target kinds. Should
  actionable rows read differently from inert ones?
- Right-aligned tails drop whole rather than truncate (board.rs:3165-3174) — same
  pattern in Queue and Config. What is the ruled narrow-width tail behavior for the
  shared row grammar?
- Estate/Activity hard-cap at 5 summary rows regardless of height; every other view
  scales. Headline or bug?
- Dag/Flow indent saturates at 6 levels — how does depth ≥7 read?
- Message payloads, event correlation/causation ids, lease heartbeats, attempt→lease
  and attempt→evidence cross-refs are all present in the data and absent from every
  row and detail pane. Which cross-references become drill affordances?
- Fleet shows no PTY presence at all — no live-terminal count, no attach churn —
  despite PtySession carrying generation and attach/detach counters. What is the
  fleet↔term join surface?
- Where does "still fetching / load more" live, given Board is a pure function and the
  wire is pull-only with 256-row pages?

### Queue / Config
- Queue's four verbs (ack, mute, unmute, resolve) have no keys; gates can't be decided
  from the Queue at all (queue.rs:99-145 refuses gate targets by design). What is the
  gate-decision surface — and today NOTHING readable shows a raised gate anywhere in
  the shipped binary.
- Mute takes an arbitrary timestamp the lens has no opinion on. Fixed menu
  (15m/1h/til-tomorrow) or duration input?
- Failed/DeadLettered messages are filtered out of the mail section entirely
  (queue.rs:370-376) — where does delivery failure surface, with
  `dead_letter_reason`?
- The authority receipt trail collapses to "{n}x since {t}" with actor and action
  discarded (queue.rs:190-218). Does the row drill into individual receipts?
- Config renders none of a file's content — is the flow select→hand-off-to-form/
  $EDITOR, and what does the generated form screen look like (nothing renders
  `ConfigFormSchema` today)?
- Config's dirty/diverged banner is estate-wide with no per-file blame, and three
  different Editor-route reasons collapse into one label. In scope to split?

### Attach / drilldown / input path
- No cursor concept exists in the frame contract (the wire "cursor" is a resume seq,
  not row/col). With INPUT mode landing, how does the operator know where typed bytes
  will go — and is a real cursor field a domain ask?
- `generation` drives silent mirror wipes and stale-batch discounts but never renders.
  Where does `{id}:{generation}` live on screen, and what does a generation flip look
  like as a transient?
- Stream state (waiting/snapshot/live/closed{code}) renders in undifferentiated
  default styling — 21 error codes share one plain-text slot. Which codes are
  recoverable-styled vs terminal-styled?
- Hosted glyphs are forced through the UI's own 28-codepoint admission gate: real CJK,
  box-drawing, emoji all escape to `\u{XXXX}` (verified drilldown.rs:1264-1283). Does
  PTY content get its own probe-informed escaping policy?
- Hosted colors bypass the tier entirely (`wire_style` takes no tier). Honest
  passthrough or quantize-to-tier?
- In INPUT mode under ruled raw passthrough, Esc is a byte the agent needs. What is
  the leave-INPUT key (leader, F-key, double-Esc), and how does the keybar teach it?
- What does a send receipt look like in-lens (F5 "both" default): a transient row, a
  gutter mark on the echoed line, a counter in the bar?
- What renders when a send is REFUSED (stale generation, authority) — the refusal
  paths are first-class in the SPEC and invisible today?
- Replay: gwk-tui structurally cannot reconstruct GWKREC bytes into a grid
  (no-gwk-pty firewall, lib.rs:26-28). Is replay a scrollable byte-log (honest,
  cheap) or a server-side replay-through-pty pipeline (real playback)?
- Diagnostics counters are lifetime-cumulative with no decay — a glitch ten minutes
  ago reads identically to an active one. Time-window or decay treatment?

### Runtime / shell
- Three loops today, three subscription sets, three inconsistent resize behaviors.
  What is the merged shell's subscription and refresh model, given projections are
  pull-only?
- The `m` motion kill is one-way per session. Toggle or kill?
- `--motion=reduced` and `full` drive identical cadence; Reduced only changes which
  marks animate. Intended contract?
- ColorTier is never shown anywhere; `--color`/`--glyphs` are fully built and
  unreachable from the CLI (no flags in args.rs). Expose both, plus a
  `gw theme swatch`-style preview verb?
- The status bar's frozen `attach {N}ms` reads as live data hours later. Kill or make
  live?

### CLI output
- Which columns for `gw term list` / `gw session list` / cost / fleet tables — ruled
  once with the matching lens columns (one design system)?
- EngineSession has no pty link and PtySession no attempt link; `gw term` cannot join
  to `gw session` even in principle today. Domain ask: a join field (which direction?).
- Pagination: default 256 lives in two unrelated constants (client for `event read`,
  server for lists); no `--all`, no cursor UX. What is the human pagination story
  ("2 rows · watermark 4812 · more: --cursor …")?
- `gw session snapshot` dumps a raw styled-cell grid as JSON — machine fixture or does
  it need a plain-text render option?
- `state.complete`'s "at least N" wording is structurally unreachable (every CLI path
  drains all pages or hard-errors). Keep the honesty machinery or drop it?
- The two non-JSON success paths (blob get raw bytes, pr passthrough) — how do tables
  stay visually distinct from these exceptions?

### Theme / degradation
- `focus` shares `hue`'s hex "until a rendered console proves it must diverge" — this
  design round is that trigger. Diverge or ratify?
- At Mono, `hue` vs `hue_bright` are identical and `fail` carries no color. Per-token,
  what compensates at the bottom tier?
- Elevation tokens are never-a-color by contract; their alternate expressions
  (reverse video, box rules, blank-line grouping) are undemonstrated. The mockups must
  demonstrate them.
- Probe shear/inconclusive detail (exact codepoints) is discarded at every call site.
  Loud diagnostics are being wired by the quick-wins lane; does the workspace ALSO
  surface tier/glyph state permanently (bar) vs only at startup?

## 4. Queue/Config: wire-or-shelve (recommendation)

**Queue: WIRE, as a top-level lens.** It is the workday screen by its own module doc;
it is tested (632 test lines, 2 goldens, adversarial negatives); its four verbs already
build correct kernel commands; and it is the ONLY reader of gates in the entire
codebase — today a raised gate (including relayed permission prompts) is invisible in
every shipped surface. That last fact alone justifies wiring. Cost: a lens mount + key
bindings + a gate-decision affordance (the one genuinely new design, since deciding a
gate is deliberately not an attention verb).

**Config: SHELVE the lens; keep the machinery.** Zero tests, zero goldens, never
visually verified; a closed 4-file list whose real work (ConfigRepository: git
reconcile, schema forms, $EDITOR handoff, lock files) is a workflow, not a glanceable
surface; the lens renders none of the file content it loads; and the failure modes are
filesystem/git side effects — the riskiest kind of code to wake up cold. The estate
banner it would contribute ("config diverged") fits better as an attention item the
Queue already knows how to show. Revisit after the workspace shell exists.

## 5. Corrections + defect ledger (carried into mockup rounds)

Corrections to the audit corpus, verified this round:
- Kernel command union is **50** variants, not 46 (command.rs:642-691).
- No 120×40 default / 80×24 floor exists in code anywhere; the non-tty snapshot is a
  hardcoded **100×30** (three call sites), two of three forcing Ascii. The grid
  contract is a design decision this document makes, not an existing constraint.
- `[`/`]` are always-bound keys (tui.rs:1076-1123); it is their *effect* (per-district
  paging) that only exists at the deepest density rung.

Load-bearing facts for every mockup:
- Wire counters cross as decimal strings, never JSON numbers; `deny_unknown_fields`
  everywhere; absent ≠ null. Open-vocab fields (Command/DispatchNode/WorkflowRun kind
  + state, Receipt.action, AttentionItem.kind…) REQUIRE unknown-value fallbacks;
  closed enums (TaskState/AttemptState/…/IngestionKind) may hard-code glyph/color
  tables.
- CostEntry: 3 real FKs, DB CHECK ≥1 populated, append-only — built for joins nobody
  ever wrote. Cost mockups must decide double-count semantics when 2+ FKs are set.
- Board's runs/receipts/ingested panels are dead-but-compiling (allowlists never
  request those kinds); Gate has NO actor field (a "who decided" column is a domain
  ask); Fleet↔PtySession join needs a domain field.
- 14 of 15 committed TUI goldens are Mono-only; drilldown's golden helper lacks the
  BLESS branch and its render takes no GlyphSet — the mock- suite must add tiered
  goldens and use the board/queue helper shape (the one with the diff-position
  reporter).
- `assert_matches_golden` exists as 4 divergent copies; the mock- suite imports/copies
  the board.rs/queue.rs variant and should not mint a fifth shape.
