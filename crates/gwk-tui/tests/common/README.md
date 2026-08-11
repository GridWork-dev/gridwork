# `tests/common/`

Shared snapshot-golden harness for `gwk-tui`'s ratatui `TestBackend` renders
— the Rung-2 mockup pipeline's substrate. The ONE shared copy of the
buffer-flatten + `BLESS=1` convention duplicated across `hall.rs`/`board.rs`/
`queue.rs`'s own private test kits (migrating those four is a follow-up,
not this harness).

- `mod.rs` — `Variant` (a golden name: `seed-<lens>-<scenario>-<WxH>-<tier>-
  <glyphs>`), `dump_frame` (render one `TestBackend` frame to text),
  `assert_matches_golden` (`BLESS=1` rewrites the golden then deliberately
  fails the run — it is not a passing run).
- `estate.rs` — the seeded workday: one coherent fictional GridWork Agent OS
  day, exposed as `estate_frame_input()` (Hall), `estate_board_state(view)`
  (Board), `estate_queue_state()` (Queue), `estate_config_state()` (Config),
  `drilldown_attached()` (Drilldown), plus `empty_*`/`empty_frame_input()`
  minimal variants.

## Adding a scenario

Add a builder to `estate.rs` returning the lens's state type, then in a
`tests/*.rs` suite: `Variant::new(lens, scenario, width, height)`
(`.with_tier(...)`/`.with_glyphs(...)` for non-default tiers)
`.check(|area, buf, tier, glyphs| { lens::render(area, buf, &state, ...) })`.

## Blessing goldens

```
BLESS=1 cargo test -p gwk-tui --test seed_snapshots
cargo test -p gwk-tui --test seed_snapshots   # verify it stuck
```

## Naming

`seed-<lens>-<scenario>-<width>x<height>-<tier>-<glyphs>.txt`, flat in
`crates/gwk-tui/goldens/` alongside the pre-existing ones — the `seed-`
prefix is what keeps this suite's goldens from ever colliding with them.
