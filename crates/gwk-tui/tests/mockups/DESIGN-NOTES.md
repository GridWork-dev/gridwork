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

### Rulings (operator pickers, 2026-08-11)

- **G1 — Curated five-lens grouping.** HALL (the estate, home) · WORK (queue ·
  tasks/dag · runs · config) · FLEET (agents · leases · cost) · FLOW (events ·
  messages · audit) · TERM (terminal lifetimes · attach). The Board dissolves into
  these; the quick-wins view-switch wiring is absorbed as sub-tab plumbing. `:` still
  addresses everything one level deep (`:cost` jumps to FLEET›cost).
- **G2 — Estate rail + pty main.** Attach renders a ~30-col estate rail beside the
  pty region; rail auto-collapses to full-screen at ≤100 cols and toggles with a key.
- **G3 — `:` navigation + context keys.** Typed input navigates and filters only;
  every mutation is a single keybar-taught context key with a confirm step.
- **G4 — Auto-table on TTY.** Human table when stdout is a tty, identical JSON when
  piped; `--json` forces the wire shape anywhere.
- **F4 — Both send surfaces from day one.** Modal INPUT (persistent badge; leader
  `ctrl-]` leaves, since Esc is a byte the agent owns) AND a `:send <term> <bytes>`
  one-shot verb from any lens; shared receipt pipeline. This closes the input-path
  SPEC's last open fork.
- **G5 — Enriched ambient Hall.** The calm full-width glyph field stays the identity;
  richness = real elapsed times, painted focus/selection, and a single vitals header
  (running/attention counts, cost tick, tier·glyph state, clock). No composite rails.
- **G6 — Wire both Queue and Config.** Queue mounts as WORK›queue with
  ack/mute/unmute/resolve keys plus a new gate-decide affordance; Config mounts as
  WORK›config, accepting the obligation to add its first golden suite and design the
  schema-form screen this round.

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

---

## 6. Mockup rounds

Artifacts of record: `mock-*` goldens in `crates/gwk-tui/goldens/`, painted by
`crates/gwk-tui/tests/mock_*.rs` over the harness's seeded workday. Shared chrome lives
in `tests/mockups/shared.rs` (not `tests/common/`, which the harness lane owns). Every
round blesses 120×40, 80×24, and at least one degraded tier.

### Round 1 — Hall at-rest · RULED `inline` (picker, 2026-08-11)

Two candidates over the seeded estate: `inline` (2-row districts, identity as role TEXT
beside the state glyph, elapsed inline) vs `stacked` (compact glyph field + a hot-callout
line naming only needs-attention/blocked/failed/unknown agents). **`inline` won.** The
losing frames are in branch history at `f1a858e`; its goldens are retired so the suite
carries no dead maintenance.

**Build spec.** Districts are two rows: heading (`> LABEL  !N  +N done`), then stations
each followed by their agents as `<state-glyph> <role> <elapsed>`. Row 0 is a vitals
header (`GRIDWORK  run N  !N  $X today` … `tier <badge>  as-of <seq>  <clock>`); the last
row is the keybar. Both shorten rather than vanishing. Focus is a `>` prefix, never
colour alone, so it survives Mono. Elapsed drops first under width pressure.

**Ruled details.** Identity is role text, not a WHO glyph — the ratified identity
inventory is closed at 7 marks and cannot name the real `gw-*` taxonomy; `short_role`
strips the `gw-` prefix and shortens the canonical names, passing unknown roles through
unchanged (the vocabulary is open). A roleless agent falls back to its own id with the
namespace stripped.

**Defects the degraded goldens exposed, fixed in-round:** the first pass rendered a
roleless agent as `? -`, and at Ascii that placeholder dash collided with `running`'s own
`-` mark — two different meanings, one character.

**Build requirements this round hands to EXECUTE:** `FrameInput` must carry real start
timestamps (the live path hardcodes the literal string `"live"`, so elapsed here is
hand-assigned); `estate.rs` must populate `Focus` (it hardcodes `focus: None`); and the
resolved `ColorTier`/`GlyphSet` must reach the header.

### Round 2 — FLEET · RULED `work` (picker, 2026-08-11)

Two candidates for `FLEET = agents · leases · cost`, both rendering the joins the audit
flagged as never joined and giving cost its time axis. **`work` won** — one row per
attempt with every join folded on (task, engine, role, state, dispatch-subtree size, live
sessions, lease + flags, rolled tokens, rolled spend, age) and the spend-per-hour chart
beneath. One grain, one comparable column set, and it keeps every load-bearing column at
80×24. The losing `resource` candidate (three stacked resource sections, cross-referenced)
is retired at `8d4cd30`; at 80×24 it lost its chart entirely and its per-agent spend.

**Open follow-up from the losing candidate:** `work` gives no row to a resource no attempt
claims — the expired `ls-release` lease and its released worktree vanish. An "unclaimed"
footer (leases/worktrees with no live attempt, plus any unattributed cost) closes that gap
without giving up the single grain.

**Attribution precedence — ruled by default-and-surface.** A `CostEntry` is counted
exactly ONCE, resolved `attempt_id` → `engine_session_id` → `dispatch_node_id`. The DB
CHECK requires ≥1 FK but permits several; double-counting across two groupings is
forbidden. An entry resolving to no attempt lands in an explicit `unattributed` tally,
never dropped.

**What the join is worth, measured on the seeded day:** two of twelve entries reach an
attempt only through a join — `ce-08` via `d-pty-recon`, `ce-09` via `es-tui-impl`.
Without it $0.43 is invisible at the unit of work and `at-tui-impl` reads as $1.69
instead of its true $2.10 (second-costliest attempt of the day).

**Honesty rules ruled here.** Spend has three distinct readings that must never collapse:
`-` (no cost entry at all), `+Nu` (entries exist but none priced — unknown, not zero),
and `$X +Nu` (priced total carrying an unpriced floor marker). Same for token columns.
The seeded day is 10 priced / 2 unpriced / 0 unattributed, stated in the chart caption.

**Findings from this round:**
- **The ratified mark inventory has no bar or sparkline glyph**, and Unicode block
  elements are East-Asian-Width Ambiguous — inadmissible under the theme's own rule. A
  cost time axis therefore needs ASCII bars (what these mockups use) or a new admissible
  mark. Open ask for the theme.
- **A proportional bar scale hides real spend.** Scaled purely against the peak, four of
  the day's five spending hours render blank behind the 15:00 spike. Ruled: any bucket
  with spend gets at least one row; the axis labels the peak and `> $0`.
- **Rounded rows do not sum to the rounded total** (±1–2¢). Totals fold unrounded micros
  and are the authority; rows round half-up. Recorded rather than papered over.
- **`AttemptState` (10) and `AgentState` (11) are different vocabularies.** The mapping
  is explicit: `Leased`→`queued` (no Hall equivalent), `Succeeded`→`done`.
- **Fixture coherence gap (harness lane):** only three of Hall's fifteen seeded agents
  share an id suffix with a seeded attempt, so "one estate viewed five ways" is looser
  than it reads — Hall's districts are a separate hand-built set rather than a projection
  of the same attempts.

### Round 3 — TERM/attach + the F4 send surfaces · RULED (picker, 2026-08-11)

F4 was already ruled BOTH, so this round designs both surfaces rather than choosing
between them. Four scenarios, with the hosted-session region painted by the REAL
`drilldown::render` over the harness's attached fixture so the pty content is genuine:

- `view` — VIEW mode, live, 28-column estate rail beside the session.
- `input` — INPUT mode: badge, raw-passthrough warning, and a send receipt row.
- `refused` — a send refused for a stale generation, rendered loudly.
- `send` — the `:send` one-shot composed from the TERM list, no attach.

**Ruled by default-and-surface here.** `{id}:{generation}` is the subject line of every
attach frame and every send — a send names the generation it is addressed to, so a
generation flip refuses rather than lands in the wrong life. The receipt row states
`sent <N>B  rcpt <id>  <actor>  <clock>` — **byte count, never content**: raw bytes may be
a password or a control sequence, so the receipt proves delivery without transcribing what
was sent. `ctrl-]` leaves INPUT (telnet's escape; Esc, `^C` and arrows all belong to the
agent under the ruled raw passthrough). The rail collapses entirely below 100 columns
rather than squeezing.

**Ruled (picker):**
- **`ctrl-]` leaves INPUT.** Under raw passthrough Esc, `^C` and the arrows are all bytes
  the agent owns, so the escape must be a chord no TUI wants — telnet's `^]`, the same
  choice `docker attach --detach-keys` makes. Double-Esc was rejected as timing-dependent
  (a vim user leaving insert twice would be ejected mid-edit).
- **The receipt is a transient toast**, not a permanent row.

**Consequence of the toast ruling — no reserved row.** A row that appeared and vanished
with each send would change the session region's height, and the console must resize the
hosted pty to that region: one resize per keystroke batch, a resize storm. The toast
therefore rides the RIGHT END OF THE SESSION'S OWN STATUS ROW, painted after
`drilldown::render` and measured against where that render actually left off (the status
text carries a variable close code — 21 of them exist). It shortens through a fixed
ladder rather than shearing the session identity:
`sent 14B  rcpt 01J9F2C4  operator  17:29:58` → `sent 14B  rcpt 01J9F2C4` → `sent 14B`.

**Refusals are a state, not an event**, so they do not expire on a timer: a refusal holds
the same slot in `fail` styling until the next send or until INPUT is left, and the mode
bar says so.

**Finding — the rail costs the session columns.** The fixture's session is 100 cols; a
28-column rail leaves 90 and a pty cannot reflow, so the frames crop. The console must
RESIZE the hosted session to the region on attach. The crop is left visible in the goldens
deliberately, as the argument for that requirement.

### Round 4 — WORK (queue · gates · config) · RULED (picker, 2026-08-11)

**Ruled:** the gate decision is a **modal confirm** (4a); config editing is a **generated
form for form-routed files with an $EDITOR fallback** (4b); dead-lettered and rejected
mail get **their own row carrying the reason and attempt count** (4c).

4c is the only ruling that needs a change to `queue.rs` itself: its mail filter admits
only `Delivered|Acknowledged|Applied`, so the real renderer cannot draw the ruled row —
the seeded day's dead-lettered alert is filtered out upstream of paint. The `mail` golden
therefore mocks both sections as a before/after so the EXECUTE lane has the exact delta:
one added row kind, carrying `dead_letter_reason` and `delivery_attempts`, both of which
the message already holds. It surfaces a second row the shipped filter also swallows — a
`Rejected` status message, which has no reason field at all and says so.


G6 ruled both lenses get wired, so this round is their first appearance inside a frame:
the bodies are painted by the REAL `queue::render` and `config::render` over the seeded
state, with a mocked lens header, sub-tab strip (`[queue] tasks runs config`), and keybar
around them. Two screens had to be invented outright.

**Gate decision (new).** `queue.rs` refuses ack/mute/resolve on a gate by design — a gate
is DECIDED, never acknowledged — and no decide verb exists anywhere in the crate. It is
drawn as a modal confirm rather than a bare keypress because a gate is the one queue row
whose verb has an irreversible outside effect (the seeded gate restarts the kernel).
Options are rendered from the gate's own open `Vec<String>`, never a hardcoded allow/deny
pair, and the selected option carries a `>` prefix so the choice survives Mono. The frame
states plainly that the decision is receipted but that **the gate aggregate records no
actor** — a domain gap, not a view one.

**Config form (new).** `ConfigFormSchema` validates a submission but nothing has ever
drawn one, and `ConfigState.contents` is loaded for all four files and never painted. The
form's shape follows its validator: every field the incumbent file carries, its type, and
its value — no add, no remove, because `validate_shape` rejects any shape change. A form
offering a field the validator will reject is a form that lies. The keybar states that an
exclusive lock is held and a concurrent edit is refused rather than merged.

**Findings:**
- The queue's mail section admits only `Delivered|Acknowledged|Applied`. The seeded day's
  dead-lettered alert (three delivery attempts, reason "nobody listening") is absent from
  the lens whose whole job is stating what is owed.
- **A header must not restate a count its body owns.** The first pass had the header say
  "2 need attention" while the queue's own verdict line said 4 — the lens counts audible
  attention plus open gates. Header notes now describe, never tally.
- **Goldens cannot verify selection or focus at any tier.** The format keeps symbols and
  drops style, and selection is colour at Truecolor/Xterm256 and reverse-video at
  Ansi16/Mono — both invisible in a symbol dump. This is the mechanical reason every
  ruled screen here marks focus with a `>` character: it is the only selection signal a
  golden can prove, and it is also the only one that survives Mono.

### Round 5 — CLI human output · blessed

Plain-text transcripts, not `TestBackend` frames: the subject is stdout, so rendering it
through a terminal buffer would misrepresent the medium. Every table is folded from the
SAME seeded estate the lens rounds render, which is what makes "one design system"
checkable rather than asserted — `gw attempt list` prints the FLEET lens's ruled columns,
`gw term list` prints the TERM lens's.

**The rule this round adds — one drop order, two surfaces.** Every column carries a
priority; when the width will not take them all, the lowest-priority column goes first, on
both surfaces, ties breaking rightmost-first. Columns drop WHOLE — nothing is ever
truncated mid-value, and no right-aligned tail silently vanishes the way the shipped
Board's does.

**Refinement the round forced.** Each surface applies that one order to its OWN cell
budget, and the budgets differ: the lens spends two cells per row on a state glyph the CLI
has no equivalent for. At 80 columns `gw attempt list` therefore keeps TASK while the
FLEET lens cannot. Same order, different budget — recorded rather than forced into false
agreement.

**Other rulings shown:**
- The trailer states rows, watermark, and how to page: `11 rows · watermark 221`, or
  `at least 5 rows · watermark 221 · more: --cursor c2VxOjIyMQ` when the read is a floor.
  The "at least" wording exists in the summary structs today and no shipped verb can
  reach it.
- `gw cost rollup` prints BY LANE (what the shipped rollup groups) AND BY HOUR (the axis
  it has never had), with the same min-one-mark rule as the lens chart so an hour that
  spent anything is visible.
- `gw term list` renders the impossible ATTEMPT column as an explicit `?` on every row
  rather than omitting it — a missing column hides the gap, a `?` states it — with the
  domain ask spelled out beneath the table.
- The piping transcript pins the G4 contract: a pipe gets today's wire shape byte for
  byte (counters as canonical decimal strings, absent fields omitted rather than null),
  `--json` forces that shape on a terminal too, and only the tty default changed.

### Standing asks for the domain (not view changes)

- `EngineSession` ↔ `PtySession` have no join field in either direction, so the ruled
  `gw term` noun cannot be correlated to `gw session` even in principle.
- `Gate` carries no actor, so "who decided this" cannot be rendered.
- No cursor row/col exists in the frame contract (the wire `cursor` is a resume seq), so
  INPUT mode has nothing to point at.

### Round 6 - shared shell edge rulings (operator picker, 2026-08-11)

- **Attention in an unfocused lens uses a suffix `!` mark, never a count.** The mark is
  compact, survives Mono, and does not repeat a body-owned tally in navigation chrome.
- **A narrow row drops its right-aligned tail as one column and leaves a compact `+`
  omission mark.** The tail is never truncated mid-value or silently discarded.
- **DAG and flow indentation caps at six visual levels, then prints the real depth as
  `+7`, `+8`, and so on.** Deep ancestry remains distinguishable without consuming the
  row's text budget.
- **The merged shell refreshes the active lens plus a warm Hall and Queue on each event
  batch.** Other projection-backed lenses refresh lazily when opened. This keeps the
  navigator's attention mark current without re-paging the full estate on every append.
- **Queue mute asks for an exact `YYYY-MM-DDTHH:MM:SSZ` deadline, then confirms.** The
  operator chose the absolute timestamp over duration parsing or fixed presets: it mirrors
  the CLI and does not invent a mute length, timezone, or product default.

### Round 7 - post-review operator rulings (operator picker, 2026-08-11)

- **Token columns state the floor and mark the gap.** When some cost entries lack a
  token count, the axis renders the summed floor with a `+?` marker
  (`50.0k+?/10.0k+?`) - never a bare `?`, which discards a mostly-known sum, and never
  a silent zero-fill, which reads as complete. One shared helper
  (`console::token_axis`) feeds the lens and the CLI table twin so the two surfaces
  cannot drift. The Round-5 mock painter's zero-fill coincides on the seeded day
  (every entry reports tokens) and is superseded by this ruling.
- **`--view replay` opens the workspace at TERM lifetimes.** Every other `--view`
  value opens the workspace on a TTY; replay follows, focused on the lens that owns
  terminal lifetimes, until a real replay surface ships. Piped and non-tty
  invocations keep the one-shot snapshot, like every other view.
- **`/` filters the estate rail on TERM attach.** The mock-attach keybar is the
  artifact of record and shows `/ filter`; VIEW-mode `/` now filters the rail's
  terminal and queue rows on their painted text (the attached row always stays).
  INPUT mode is unchanged.

### Round 8 - the FLEET lens's unknowns block · RULED `footer` (picker, 2026-08-13)

Closes the two design items the console audit left open: **B5** (the console's FLEET
lens has none of the Board twin's pinned-unknowns machinery, so three facts the twin
names go unsaid) and **B6** (the `SES` column paints `ended_at.is_none()` under a name
that claims liveness). Both are additive rather than corrective - with B1-B4 fixed the
lens stated nothing false; it was silent where the twin speaks, and terse where the
twin is careful.

**Candidates.** `pinned` - the twin's block ported literally, directly beneath the
column header at every rung, always fully worded. `footer` - beside UNCLAIMED, the
lens's existing "what the rows do not cover" block, worded in full while the frame can
spare the rows and collapsed to one subject line when it cannot.

**`footer` won.** At the 80x24 floor `pinned` charged four of eleven attempt rows and
took them off the HEAD of the list - `at-pty-impl` among them, which is the running
attempt the engine-binding note is *about*. A caveat that displaces its own subject is
worse than a terse one. `footer` charges one row there and four at 120x40, where they
are free. Losing frames are in branch history at `c803a6c`; their goldens are retired.
`pinned`'s one real argument - unknowns must never scroll away - does not bind: this
lens is a fixed window with a `+N more` notice, not a scrolling list, so a footer is
as pinned as a header.

**Ruled details.**

- **The block reads the twin, it does not re-derive it.** `render_fleet` calls
  `board::agent_fleet` and paints its `unknowns`. One place decides what the log does
  not carry; a console that recomputed the same three facts would be free to drift from
  the panel that ruled them.
- **Degradation is subject-last.** The `why` clauses drop before the subjects, and a
  subject that will not fit drops WHOLE behind a `+N` mark. A block degraded to a bare
  count would be stating the *number* of things it was declining to name, which is
  worse than either full form.
- **The block is evidence, not chrome.** No unknowns, no block - a standing disclaimer
  on every frame is one the eye learns to skip. The note COUNT decides, never a flag.
- **Words carry it, never colour.** Every token these rows use resolves to the
  terminal's own foreground at Mono, so the mono/ascii goldens read identically to
  truecolor. This is the B4 lesson applied ahead of the defect.
- **`SES` becomes `NOEND`** on the lens and on `gw attempt list`. The cell folds
  `ended_at.is_none()`: the old header read as "the sessions this attempt has" over a
  number that is not that, and any liveness word would claim a reading nothing in the
  log supports. The cell it costs comes out of SUB, whose values are small counts, and
  both are now value cells so an over-budget count drops whole rather than shearing.
- **`?` is not a zero.** A RUNNING attempt with no engine session on the page reads
  `?`; sessions that exist and all ended read `-`. The SESSION count decides, never the
  unended count - once the fold has run, "zero unended of three" and "no session at
  all" are both `0usize`. The `?` set is exactly the set the twin's `engine binding`
  note tallies, so the cell and the note can never describe different rows.
- **`gw session list` stops saying `live`.** It now prints the panel's own words
  through a shared `board::NO_END_RECORDED`, so one field cannot read two ways again.
- **`BoardState::complete` reaches this lens.** A partial read prefixes the header's
  count run with `at least` (the prefix `gw`'s table trailers already use) once, rather
  than repeating on each figure, and an empty cost fold says `no spend on this page`
  instead of `no spend recorded` - "no records at all" is a claim about the ledger and
  only a read that reached the last page can make it.

**Findings from this round:**

- **The FLEET header's chrome tail painted over its own summary.** The summary goes
  down first and the tier/watermark run is right-aligned over it, and the `width < 100`
  threshold picked both long forms at exactly 100 columns: the collision ate the whole
  spend figure and left a header that read as complete. A width threshold cannot decide
  this, because the summary's length moves with the estate and with the `at least`
  qualifier. Fixed in-round: the pair is chosen by measured fit, and the tail gives up
  its low-priority parts (`tier` label, watermark) as one column before the summary
  gives up anything.
- **`agent_fleet`'s `findings` are deliberately NOT surfaced here.** Duplicate ids on a
  projection page are a page-integrity alarm, not an absence: different class, not
  fleet-specific, and they want a louder treatment than a muted footer. Open follow-up,
  recorded rather than swallowed.
- **`complete` is still structurally unreachable on the live console**, exactly as
  §CLI records for the shipped verbs: `refresh_*` drains every projection page or hard
  errors, so `complete` is hardcoded `true` at all three construction sites. The lens
  now honours the field it is handed; making a paged reader that can set it false is a
  separate sitting.
- The Round-2 `mock_fleet` painter still prints `SES` and keeps its own copy of the
  session fold. It is the retired artifact of the Round-2 picker and is superseded by
  this ruling, the same way Round 7 superseded the Round-5 painter's zero-fill. The
  Round-5 CLI painter DID move, because `console_tables` enforces its golden against
  the shipped `tables::attempt_table` and the two cannot disagree.
