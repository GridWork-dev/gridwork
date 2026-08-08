use gwk_domain::ids::Seq;
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::hall::{
    Agent, AgentId, AgentState, Attention, AttentionId, DECAY_DURATION, DensityRung, District,
    DistrictId, Focus, FrameInput, HallTarget, MotionDriver, MotionEntity, MotionFrame,
    MotionInput, MotionKey, MotionMode, MotionVerb, PULSE_DURATION, Station, StationId, TICK_CYCLE,
    agent_order, collapse_order, district_stack_order, render, render_with_motion, solve_density,
    station_order, target_order,
};
use gwk_tui::input::HitMap;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::time::Duration;

fn district_id(value: &str) -> DistrictId {
    DistrictId::new(value).expect("district id")
}

fn station_id(value: &str) -> StationId {
    StationId::new(value).expect("station id")
}

fn agent_id(value: &str) -> AgentId {
    AgentId::new(value).expect("agent id")
}

fn attention_id(value: &str) -> AttentionId {
    AttentionId::new(value).expect("attention id")
}

fn agent(id: &str, role: Option<&str>, state: AgentState, seq: u64) -> Agent {
    Agent {
        id: agent_id(id),
        role: role.map(str::to_owned),
        state,
        duration: None,
        changed_seq: Seq::new(seq),
    }
}

fn station(id: &str, ordinal: u16, label: &str, agents: Vec<Agent>, seq: u64) -> Station {
    Station {
        id: station_id(id),
        label: label.to_owned(),
        template_ordinal: ordinal,
        agents,
        changed_seq: Seq::new(seq),
    }
}

fn district(id: &str, label: &str, stations: Vec<Station>, seq: u64) -> District {
    District {
        id: district_id(id),
        label: label.to_owned(),
        stations,
        changed_seq: Seq::new(seq),
    }
}

fn empty_input() -> FrameInput {
    FrameInput {
        districts: Vec::new(),
        focus: None,
        attention: Vec::new(),
        watermark: None,
    }
}

fn representative_input() -> FrameInput {
    FrameInput {
        districts: vec![
            district(
                "district-build",
                "Build",
                vec![
                    station(
                        "station-verify",
                        2,
                        "verify",
                        vec![
                            agent(
                                "agent-review",
                                Some("reviewer"),
                                AgentState::NeedsAttention,
                                18,
                            ),
                            agent("agent-test", Some("researcher"), AgentState::Running, 17),
                        ],
                        18,
                    ),
                    station(
                        "station-code",
                        1,
                        "code",
                        vec![agent(
                            "agent-code",
                            Some("implementer"),
                            AgentState::Running,
                            16,
                        )],
                        16,
                    ),
                ],
                18,
            ),
            district(
                "district-research",
                "Research",
                vec![station(
                    "station-discover",
                    1,
                    "discover",
                    vec![agent(
                        "agent-research",
                        Some("researcher"),
                        AgentState::Done,
                        12,
                    )],
                    12,
                )],
                12,
            ),
        ],
        focus: Some(Focus {
            district: district_id("district-build"),
            changed_seq: Seq::new(15),
        }),
        attention: vec![Attention {
            id: attention_id("attention-review"),
            district: district_id("district-build"),
            unresolved: true,
            changed_seq: Seq::new(18),
        }],
        watermark: Some(Seq::new(20)),
    }
}

fn dump_frame(width: u16, height: u16, input: &FrameInput) -> (String, HitMap<HallTarget>, Buffer) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    let mut hits = HitMap::new();
    terminal
        .draw(|frame| {
            render(
                frame.area(),
                frame.buffer_mut(),
                input,
                ColorTier::Mono,
                GlyphSet::Unicode,
                &mut hits,
            );
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut rendered = String::new();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        rendered.push_str(line.trim_end());
        rendered.push('\n');
    }
    (rendered, hits, buffer)
}

fn dump_motion(
    width: u16,
    height: u16,
    input: &FrameInput,
    glyphs: GlyphSet,
    driver: &mut MotionDriver,
    motions: &[MotionInput],
) -> (String, HitMap<HallTarget>, Buffer) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    let mut hits = HitMap::new();
    terminal
        .draw(|frame| {
            render_with_motion(
                frame.area(),
                frame.buffer_mut(),
                input,
                ColorTier::Mono,
                glyphs,
                &mut hits,
                MotionFrame {
                    driver,
                    inputs: motions,
                },
            );
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut rendered = String::new();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        rendered.push_str(line.trim_end());
        rendered.push('\n');
    }
    (rendered, hits, buffer)
}

fn one_agent_input(state: AgentState) -> FrameInput {
    FrameInput {
        districts: vec![district(
            "district-a",
            "A",
            vec![station(
                "station-a",
                1,
                "s",
                vec![agent("agent-a", Some("implementer"), state, 7)],
                7,
            )],
            7,
        )],
        ..empty_input()
    }
}

fn motion(
    entity: MotionEntity,
    changed_seq: u64,
    verb: MotionVerb,
    elapsed: Duration,
    frame_delta: Duration,
    source: Rect,
    target: Rect,
) -> MotionInput {
    MotionInput {
        key: MotionKey {
            entity,
            changed_seq: Seq::new(changed_seq),
        },
        verb,
        elapsed,
        frame_delta,
        source,
        target,
    }
}

fn assert_matches_golden(name: &str, rendered: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("goldens")
        .join(format!("{name}.txt"));
    let bless = std::env::var_os("BLESS").is_some();
    if bless {
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory).expect("create golden directory");
        }
        std::fs::write(&path, rendered).expect("write golden");
    } else {
        let committed = std::fs::read_to_string(&path)
            .unwrap_or_else(|why| panic!("{}: {why} (BLESS=1 to create)", path.display()));
        assert_eq!(committed, rendered, "{} drifted", path.display());
    }
    assert!(
        !bless,
        "BLESS=1 rewrites the goldens; it is not a passing run"
    );
}

#[test]
fn hall_orders_attention_and_collapse_independently() {
    let mut input = empty_input();
    input.districts = vec![
        district(
            "district-a",
            "A",
            vec![station(
                "station-a",
                1,
                "a",
                vec![agent("agent-a", None, AgentState::Idle, 9)],
                9,
            )],
            9,
        ),
        district(
            "district-b",
            "B",
            vec![station(
                "station-b",
                1,
                "b",
                vec![agent("agent-b", None, AgentState::Idle, 1)],
                1,
            )],
            1,
        ),
        district(
            "district-c",
            "C",
            vec![station(
                "station-c",
                1,
                "c",
                vec![agent("agent-c", None, AgentState::Idle, 2)],
                2,
            )],
            2,
        ),
        district(
            "district-d",
            "D",
            vec![station(
                "station-d",
                1,
                "d",
                vec![agent("agent-d", None, AgentState::Idle, 3)],
                3,
            )],
            3,
        ),
    ];
    input.focus = Some(Focus {
        district: district_id("district-b"),
        changed_seq: Seq::new(10),
    });
    input.attention = vec![Attention {
        id: attention_id("attention-c"),
        district: district_id("district-c"),
        unresolved: true,
        changed_seq: Seq::new(11),
    }];

    assert_eq!(
        district_stack_order(&input),
        vec![
            district_id("district-c"),
            district_id("district-a"),
            district_id("district-b"),
            district_id("district-d"),
        ]
    );
    assert_eq!(
        collapse_order(&input),
        vec![district_id("district-d"), district_id("district-a")]
    );
}

#[test]
fn hall_station_and_agent_order_are_stable() {
    let district = district(
        "district-a",
        "A",
        vec![
            station(
                "station-z",
                1,
                "z",
                vec![
                    agent("agent-z", None, AgentState::Idle, 1),
                    agent("agent-a", None, AgentState::Idle, 1),
                ],
                1,
            ),
            station(
                "station-b",
                0,
                "b",
                vec![agent("agent-b", None, AgentState::Idle, 1)],
                1,
            ),
            station(
                "station-a",
                1,
                "a",
                vec![agent("agent-c", None, AgentState::Idle, 1)],
                1,
            ),
        ],
        1,
    );

    assert_eq!(
        station_order(&district),
        vec![
            station_id("station-b"),
            station_id("station-a"),
            station_id("station-z"),
        ]
    );
    assert_eq!(
        agent_order(&district.stations[0]),
        vec![agent_id("agent-a"), agent_id("agent-z")]
    );
}

#[test]
fn hall_view_ids_enforce_their_namespace_and_cell_safe_shape() {
    assert!(DistrictId::new("district-build").is_ok());
    assert!(DistrictId::new("station-build").is_err());
    assert!(DistrictId::new("district-bad id").is_err());
    assert!(DistrictId::new("district-◆").is_err());
    assert!(DistrictId::new(format!("district-{}", "x".repeat(64))).is_err());
}

#[test]
fn hall_density_walk_shrinks_gutter_before_collapse() {
    let input = FrameInput {
        districts: vec![district(
            "district-a",
            "A",
            vec![station(
                "station-a",
                1,
                "s",
                vec![
                    agent("agent-a", None, AgentState::Idle, 1),
                    agent("agent-b", None, AgentState::Idle, 1),
                ],
                1,
            )],
            1,
        )],
        ..empty_input()
    };

    assert_eq!(
        solve_density(Rect::new(0, 0, 5, 3), &input).rung,
        DensityRung::Baseline
    );
    assert_eq!(
        solve_density(Rect::new(0, 0, 4, 3), &input).rung,
        DensityRung::GutterShrink
    );
    assert_eq!(
        solve_density(Rect::new(0, 0, 20, 1), &input).rung,
        DensityRung::DistrictCollapse
    );
}

#[test]
fn hall_empty_epoch_matches_its_golden() {
    let (empty, _, _) = dump_frame(72, 6, &empty_input());
    assert!(empty.contains("No active work yet"), "{empty}");
    assert!(
        empty.contains("Projects and running agents appear when the ledger records them."),
        "{empty}"
    );
    assert_matches_golden("hall-empty", &empty);

    let shells = FrameInput {
        districts: vec![district("district-shell", "Shell", Vec::new(), 1)],
        ..empty_input()
    };
    assert_eq!(dump_frame(72, 6, &shells).0, empty);
}

#[test]
fn hall_attention_only_district_is_visible_without_motion() {
    let input = FrameInput {
        districts: vec![district("district-shell", "Shell", Vec::new(), 1)],
        attention: vec![Attention {
            id: attention_id("attention-shell"),
            district: district_id("district-shell"),
            unresolved: true,
            changed_seq: Seq::new(2),
        }],
        ..empty_input()
    };

    let (rendered, hits, _) = dump_frame(24, 3, &input);
    assert!(rendered.contains("Shell !1"), "{rendered}");
    assert!(!rendered.contains("No active work yet"), "{rendered}");
    assert_eq!(hits.targets().count(), 0);
}

#[test]
fn hall_representative_estate_matches_its_golden() {
    let input = representative_input();
    let (estate, _, _) = dump_frame(72, 8, &input);
    assert_matches_golden("hall-estate", &estate);
}

#[test]
fn hall_input_permutation_does_not_change_frame_or_targets() {
    let input = representative_input();
    let mut permuted = input.clone();
    permuted.districts.reverse();
    for district in &mut permuted.districts {
        district.stations.reverse();
        for station in &mut district.stations {
            station.agents.reverse();
        }
    }
    permuted.attention.reverse();

    let (first, first_hits, _) = dump_frame(72, 8, &input);
    let (second, second_hits, _) = dump_frame(72, 8, &permuted);
    assert_eq!(first, second);
    assert_eq!(
        first_hits.targets().collect::<Vec<_>>(),
        second_hits.targets().collect::<Vec<_>>()
    );
    assert_eq!(target_order(&input), target_order(&permuted));
}

#[test]
fn hall_dynamic_labels_are_escaped_before_canvas_paint() {
    let input = FrameInput {
        districts: vec![district(
            "district-safe",
            "unsafe ◆",
            vec![station(
                "station-safe",
                1,
                "wide 你好",
                vec![agent("agent-safe", None, AgentState::Idle, 1)],
                1,
            )],
            1,
        )],
        ..empty_input()
    };
    let (rendered, _, _) = dump_frame(72, 3, &input);
    assert!(rendered.contains("unsafe \\u{25C6}"), "{rendered}");
    assert!(rendered.contains("wide \\u{4F60}\\u{597D}"), "{rendered}");
    assert!(!rendered.contains('◆'));
    assert!(!rendered.contains('你'));
}

#[test]
fn hall_overflow_names_paging_instead_of_clipping() {
    let input = representative_input();
    let (rendered, hits, _) = dump_frame(28, 1, &input);
    assert!(rendered.contains("district"), "{rendered}");
    assert!(rendered.contains("paging"), "{rendered}");
    assert_eq!(
        hits.targets().count(),
        0,
        "paging result has no hidden hits"
    );
    assert_eq!(
        solve_density(Rect::new(0, 0, 28, 1), &input).rung,
        DensityRung::Paging
    );
}

#[test]
fn hall_canvas_stays_inside_every_supplied_area() {
    let mut input = representative_input();
    input.districts[0].label = "hostile ◆ 你好 ⚠".into();
    let outer = Rect::new(0, 0, 40, 20);

    for origin_x in [0, 3, 7] {
        for origin_y in [0, 2, 5] {
            for width in 0..=24 {
                for height in 0..=10 {
                    let area = Rect::new(origin_x, origin_y, width, height);
                    let mut buffer = Buffer::empty(outer);
                    let mut hits = HitMap::new();
                    render(
                        area,
                        &mut buffer,
                        &input,
                        ColorTier::Mono,
                        GlyphSet::Unicode,
                        &mut hits,
                    );
                    for y in 0..outer.height {
                        for x in 0..outer.width {
                            let inside = x >= area.x
                                && x < area.x.saturating_add(area.width)
                                && y >= area.y
                                && y < area.y.saturating_add(area.height);
                            if !inside {
                                assert_eq!(
                                    buffer[(x, y)].symbol(),
                                    " ",
                                    "paint escaped {area:?} at ({x},{y})"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn hall_agent_hits_are_exactly_the_painted_two_cells() {
    let input = representative_input();
    let (rendered, hits, _) = dump_frame(72, 8, &input);
    let target = HallTarget::Agent(agent_id("agent-code"));
    let mut cells: Vec<(u16, u16)> = Vec::new();
    for y in 0..8 {
        for x in 0..72 {
            if hits.hit(x, y) == Some(&target) {
                cells.push((x, y));
            }
        }
    }
    assert_eq!(
        cells.len(),
        2,
        "target must be exactly two cells:\n{rendered}"
    );
    assert_eq!(cells[0].1, cells[1].1);
    assert_eq!(cells[0].0 + 1, cells[1].0);
    if cells[0].0 > 0 {
        assert_ne!(hits.hit(cells[0].0 - 1, cells[0].1), Some(&target));
    }
    assert_ne!(hits.hit(cells[1].0 + 1, cells[1].1), Some(&target));
}

#[test]
fn hall_gutter_shrink_keeps_adjacent_two_cell_targets_exact() {
    let input = FrameInput {
        districts: vec![district(
            "district-a",
            "A",
            vec![station(
                "station-a",
                1,
                "s",
                vec![
                    agent("agent-a", None, AgentState::Idle, 1),
                    agent("agent-b", None, AgentState::Idle, 1),
                ],
                1,
            )],
            1,
        )],
        ..empty_input()
    };
    let (rendered, hits, _) = dump_frame(4, 3, &input);
    assert_eq!(
        solve_density(Rect::new(0, 0, 4, 3), &input).rung,
        DensityRung::GutterShrink
    );
    let first = HallTarget::Agent(agent_id("agent-a"));
    let second = HallTarget::Agent(agent_id("agent-b"));
    assert_eq!(hits.hit(0, 2), Some(&first), "{rendered}");
    assert_eq!(hits.hit(1, 2), Some(&first), "{rendered}");
    assert_eq!(hits.hit(2, 2), Some(&second), "{rendered}");
    assert_eq!(hits.hit(3, 2), Some(&second), "{rendered}");
}

#[test]
fn hall_motion_constants_pin_all_three_verb_boundaries() {
    assert_eq!(TICK_CYCLE, Duration::from_millis(8 * 33));
    assert_eq!(TICK_CYCLE, gwk_tui::input::TICK.saturating_mul(8));
    assert_eq!(PULSE_DURATION, Duration::from_millis(400));
    assert_eq!(DECAY_DURATION, Duration::from_millis(600));
}

#[test]
fn hall_tick_cycles_only_the_expression_cell_and_wraps_at_eight_frames() {
    let input = one_agent_input(AgentState::Running);
    let base = dump_frame(12, 3, &input).2;
    assert_eq!(base[(0, 2)].symbol(), "▸");
    assert_eq!(base[(1, 2)].symbol(), "⠋");

    for (elapsed, expected) in [(33, "⠙"), (7 * 33, "⠆"), (8 * 33, "⠋")] {
        let mut driver = MotionDriver::new(MotionMode::Full);
        let tick = motion(
            MotionEntity::Agent(agent_id("agent-a")),
            7,
            MotionVerb::Tick,
            Duration::from_millis(elapsed),
            gwk_tui::input::TICK,
            Rect::new(1, 2, 1, 1),
            Rect::new(1, 2, 1, 1),
        );
        let (_, _, animated) = dump_motion(12, 3, &input, GlyphSet::Unicode, &mut driver, &[tick]);
        assert_eq!(animated[(0, 2)].symbol(), "▸", "identity cell moved");
        assert_eq!(animated[(1, 2)].symbol(), expected, "elapsed {elapsed}");
        assert_eq!(animated[(2, 2)].symbol(), " ", "neighbor changed");
    }
}

#[test]
fn hall_motion_off_and_absent_driver_are_frame_zero_equivalent() {
    let mut input = one_agent_input(AgentState::Running);
    input.districts[0].stations[0].agents[0].duration = Some("12m".into());
    let (base, base_hits, _) = dump_frame(12, 3, &input);
    assert!(
        base.contains("12m"),
        "duration must carry liveness:\n{base}"
    );
    let tick = motion(
        MotionEntity::Agent(agent_id("agent-a")),
        7,
        MotionVerb::Tick,
        Duration::from_millis(99),
        gwk_tui::input::TICK,
        Rect::new(1, 2, 1, 1),
        Rect::new(1, 2, 1, 1),
    );
    let mut off = MotionDriver::new(MotionMode::Off);
    let (rendered, hits, _) = dump_motion(12, 3, &input, GlyphSet::Unicode, &mut off, &[tick]);
    assert_eq!(rendered, base);
    assert_eq!(
        hits.targets().collect::<Vec<_>>(),
        base_hits.targets().collect::<Vec<_>>()
    );
}

#[test]
fn hall_reduced_motion_keeps_tick_and_drops_one_shots() {
    let input = one_agent_input(AgentState::Running);
    let tick = motion(
        MotionEntity::Agent(agent_id("agent-a")),
        7,
        MotionVerb::Tick,
        Duration::from_millis(33),
        gwk_tui::input::TICK,
        Rect::new(1, 2, 1, 1),
        Rect::new(1, 2, 1, 1),
    );
    let pulse = motion(
        MotionEntity::Attention(attention_id("attention-a")),
        8,
        MotionVerb::Pulse,
        Duration::ZERO,
        Duration::ZERO,
        Rect::new(0, 3, 12, 3),
        Rect::new(0, 0, 12, 3),
    );
    let mut reduced = MotionDriver::new(MotionMode::Reduced);
    let (_, hits, buffer) = dump_motion(
        12,
        6,
        &input,
        GlyphSet::Unicode,
        &mut reduced,
        &[tick, pulse],
    );
    assert_eq!(buffer[(1, 2)].symbol(), "⠙");
    assert_eq!(
        hits.hit(0, 2),
        Some(&HallTarget::Agent(agent_id("agent-a")))
    );
    assert_eq!(hits.hit(0, 5), None, "reduced mode applied the pulse");
    assert_eq!(reduced.active_one_shots(), 0);
}

#[test]
fn hall_pulse_translates_before_canvas_and_keeps_hits_aligned() {
    let input = one_agent_input(AgentState::Running);
    let key_entity = MotionEntity::Attention(attention_id("attention-a"));

    let mut at_start = MotionDriver::new(MotionMode::Full);
    let start = motion(
        key_entity.clone(),
        8,
        MotionVerb::Pulse,
        Duration::ZERO,
        Duration::ZERO,
        Rect::new(0, 3, 12, 3),
        Rect::new(0, 0, 12, 3),
    );
    let (_, hits, buffer) = dump_motion(12, 6, &input, GlyphSet::Unicode, &mut at_start, &[start]);
    assert_eq!(buffer[(0, 5)].symbol(), "▸");
    assert_eq!(
        hits.hit(0, 5),
        Some(&HallTarget::Agent(agent_id("agent-a")))
    );
    assert_eq!(hits.hit(0, 2), None);

    let mut at_end = MotionDriver::new(MotionMode::Full);
    let end = motion(
        key_entity,
        8,
        MotionVerb::Pulse,
        PULSE_DURATION,
        gwk_tui::input::TICK,
        Rect::new(0, 3, 12, 3),
        Rect::new(0, 0, 12, 3),
    );
    let (_, hits, buffer) = dump_motion(12, 6, &input, GlyphSet::Unicode, &mut at_end, &[end]);
    assert_eq!(buffer[(0, 2)].symbol(), "▸");
    assert_eq!(
        hits.hit(0, 2),
        Some(&HallTarget::Agent(agent_id("agent-a")))
    );
}

#[test]
fn hall_reorder_pulse_keeps_moving_pair_above_an_occupied_source_row() {
    let input = FrameInput {
        districts: vec![
            district(
                "district-a",
                "A",
                vec![station(
                    "station-a",
                    1,
                    "a",
                    vec![agent("agent-a", Some("reviewer"), AgentState::Idle, 1)],
                    1,
                )],
                1,
            ),
            district(
                "district-b",
                "B",
                vec![station(
                    "station-b",
                    1,
                    "b",
                    vec![agent(
                        "agent-b",
                        Some("implementer"),
                        AgentState::Running,
                        2,
                    )],
                    2,
                )],
                2,
            ),
        ],
        attention: vec![Attention {
            id: attention_id("attention-b"),
            district: district_id("district-b"),
            unresolved: true,
            changed_seq: Seq::new(3),
        }],
        ..empty_input()
    };
    let target = HallTarget::Agent(agent_id("agent-b"));

    for elapsed in [Duration::ZERO, PULSE_DURATION / 2, PULSE_DURATION] {
        let pulse = motion(
            MotionEntity::Attention(attention_id("attention-b")),
            3,
            MotionVerb::Pulse,
            elapsed,
            gwk_tui::input::TICK,
            Rect::new(0, 3, 12, 3),
            Rect::new(0, 0, 12, 3),
        );
        let mut driver = MotionDriver::new(MotionMode::Full);
        let (rendered, hits, buffer) =
            dump_motion(12, 6, &input, GlyphSet::Unicode, &mut driver, &[pulse]);
        let cells: Vec<_> = (0..6)
            .flat_map(|y| (0..12).map(move |x| (x, y)))
            .filter(|(x, y)| hits.hit(*x, *y) == Some(&target))
            .collect();

        assert_eq!(cells.len(), 2, "pulse elapsed {elapsed:?}:\n{rendered}");
        assert_eq!(
            buffer[cells[0]].symbol(),
            "▸",
            "moving district lost paint ownership at {elapsed:?}:\n{rendered}"
        );
    }
}

#[test]
fn hall_tick_tracks_a_pulsing_agent_at_start_midpoint_and_end() {
    let input = one_agent_input(AgentState::Running);
    let target = HallTarget::Agent(agent_id("agent-a"));

    for pulse_elapsed in [Duration::ZERO, PULSE_DURATION / 2, PULSE_DURATION] {
        let pulse = motion(
            MotionEntity::Attention(attention_id("attention-a")),
            8,
            MotionVerb::Pulse,
            pulse_elapsed,
            gwk_tui::input::TICK,
            Rect::new(0, 3, 12, 3),
            Rect::new(0, 0, 12, 3),
        );
        let tick = motion(
            MotionEntity::Agent(agent_id("agent-a")),
            7,
            MotionVerb::Tick,
            gwk_tui::input::TICK,
            gwk_tui::input::TICK,
            Rect::new(1, 2, 1, 1),
            Rect::new(1, 2, 1, 1),
        );
        let mut driver = MotionDriver::new(MotionMode::Full);
        let (_, hits, buffer) = dump_motion(
            12,
            6,
            &input,
            GlyphSet::Unicode,
            &mut driver,
            &[pulse, tick],
        );
        let cells: Vec<_> = (0..6)
            .flat_map(|y| (0..12).map(move |x| (x, y)))
            .filter(|(x, y)| hits.hit(*x, *y) == Some(&target))
            .collect();

        assert_eq!(cells.len(), 2, "pulse elapsed {pulse_elapsed:?}");
        assert_eq!(buffer[cells[0]].symbol(), "▸");
        assert_eq!(
            buffer[cells[1]].symbol(),
            "⠙",
            "tick detached at pulse elapsed {pulse_elapsed:?}"
        );
        if cells[0].1 != 2 {
            assert_eq!(
                buffer[(1, 2)].symbol(),
                " ",
                "tick left a ghost at the untransformed target"
            );
        }
    }
}

#[test]
fn hall_decay_tracks_a_pulsing_agent() {
    let input = one_agent_input(AgentState::Running);
    let pulse = motion(
        MotionEntity::Attention(attention_id("attention-a")),
        8,
        MotionVerb::Pulse,
        Duration::ZERO,
        Duration::ZERO,
        Rect::new(0, 3, 12, 3),
        Rect::new(0, 0, 12, 3),
    );
    let decay = motion(
        MotionEntity::Agent(agent_id("agent-a")),
        7,
        MotionVerb::Decay,
        DECAY_DURATION,
        gwk_tui::input::TICK,
        Rect::new(1, 2, 1, 1),
        Rect::new(1, 2, 1, 1),
    );
    let mut driver = MotionDriver::new(MotionMode::Full);
    let (_, hits, buffer) = dump_motion(
        12,
        6,
        &input,
        GlyphSet::Unicode,
        &mut driver,
        &[pulse, decay],
    );

    assert_eq!(
        hits.hit(0, 5),
        Some(&HallTarget::Agent(agent_id("agent-a")))
    );
    assert_eq!(buffer[(0, 5)].symbol(), "▸");
    assert_eq!(
        buffer[(1, 5)].symbol(),
        " ",
        "decay detached from the pulsing expression cell"
    );
}

#[test]
fn hall_pulse_clips_at_the_lens_without_splitting_the_agent_pair() {
    let input = one_agent_input(AgentState::Running);
    let pulse = motion(
        MotionEntity::Attention(attention_id("attention-edge")),
        9,
        MotionVerb::Pulse,
        Duration::ZERO,
        Duration::ZERO,
        Rect::new(11, 0, 12, 3),
        Rect::new(0, 0, 12, 3),
    );
    let mut driver = MotionDriver::new(MotionMode::Full);
    let (_, hits, buffer) = dump_motion(12, 3, &input, GlyphSet::Unicode, &mut driver, &[pulse]);
    assert_eq!(buffer[(11, 2)].symbol(), " ");
    assert_eq!(hits.targets().count(), 0);
}

#[test]
fn hall_clipped_pulse_suppresses_tick_and_decay_with_the_agent_pair() {
    let input = one_agent_input(AgentState::Running);
    let lens = Rect::new(1, 0, 12, 3);

    for (verb, elapsed) in [
        (MotionVerb::Tick, gwk_tui::input::TICK),
        (MotionVerb::Decay, DECAY_DURATION / 2),
    ] {
        let pulse = motion(
            MotionEntity::Attention(attention_id("attention-edge")),
            9,
            MotionVerb::Pulse,
            Duration::ZERO,
            Duration::ZERO,
            Rect::new(0, 0, 12, 3),
            lens,
        );
        let character = motion(
            MotionEntity::Agent(agent_id("agent-a")),
            7,
            verb,
            elapsed,
            gwk_tui::input::TICK,
            Rect::new(2, 2, 1, 1),
            Rect::new(2, 2, 1, 1),
        );
        let mut driver = MotionDriver::new(MotionMode::Full);
        let mut hits = HitMap::new();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 14, 3));
        render_with_motion(
            lens,
            &mut buffer,
            &input,
            ColorTier::Mono,
            GlyphSet::Unicode,
            &mut hits,
            MotionFrame {
                driver: &mut driver,
                inputs: &[pulse, character],
            },
        );

        assert_eq!(hits.targets().count(), 0, "{verb:?} kept a clipped hit");
        assert_eq!(
            buffer[(1, 2)].symbol(),
            " ",
            "{verb:?} survived after its two-cell agent pair was clipped"
        );
    }
}

#[test]
fn hall_decay_is_character_only_and_confined_to_its_cell() {
    let input = one_agent_input(AgentState::Running);
    let decay = motion(
        MotionEntity::Agent(agent_id("agent-a")),
        8,
        MotionVerb::Decay,
        DECAY_DURATION,
        gwk_tui::input::TICK,
        Rect::new(1, 2, 1, 1),
        Rect::new(1, 2, 1, 1),
    );
    let mut driver = MotionDriver::new(MotionMode::Full);
    let (_, hits, buffer) = dump_motion(12, 3, &input, GlyphSet::Unicode, &mut driver, &[decay]);
    assert_eq!(buffer[(0, 2)].symbol(), "▸");
    assert_eq!(buffer[(1, 2)].symbol(), " ");
    assert_eq!(buffer[(2, 2)].symbol(), " ");
    assert_eq!(
        hits.hit(0, 2),
        Some(&HallTarget::Agent(agent_id("agent-a"))),
        "decay must not erase the pair target"
    );
}

#[test]
fn hall_ascii_tick_stays_inside_the_admitted_escape_set() {
    let input = one_agent_input(AgentState::Running);
    let tick = motion(
        MotionEntity::Agent(agent_id("agent-a")),
        7,
        MotionVerb::Tick,
        Duration::from_millis(7 * 33),
        gwk_tui::input::TICK,
        Rect::new(1, 2, 1, 1),
        Rect::new(1, 2, 1, 1),
    );
    let mut driver = MotionDriver::new(MotionMode::Full);
    let (rendered, _, buffer) = dump_motion(12, 3, &input, GlyphSet::Ascii, &mut driver, &[tick]);
    assert_eq!(buffer[(0, 2)].symbol(), "I");
    assert_eq!(buffer[(1, 2)].symbol(), "-");
    assert!(rendered.is_ascii());
}

#[test]
fn hall_same_entity_and_sequence_replaces_the_one_shot() {
    let input = one_agent_input(AgentState::Running);
    let first = motion(
        MotionEntity::Agent(agent_id("agent-a")),
        9,
        MotionVerb::Decay,
        DECAY_DURATION,
        gwk_tui::input::TICK,
        Rect::new(1, 2, 1, 1),
        Rect::new(1, 2, 1, 1),
    );
    let replacement = MotionInput {
        elapsed: Duration::from_millis(100),
        ..first.clone()
    };
    let mut driver = MotionDriver::new(MotionMode::Full);
    let (_, _, buffer) = dump_motion(
        12,
        3,
        &input,
        GlyphSet::Unicode,
        &mut driver,
        &[first, replacement],
    );
    assert_eq!(driver.active_one_shots(), 1);
    assert_eq!(
        buffer[(1, 2)].symbol(),
        "⠋",
        "the replaced completed decay must never touch the frame"
    );
}
