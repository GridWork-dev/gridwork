use gwk_domain::ids::Seq;
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::hall::{
    Agent, AgentId, AgentState, Attention, AttentionId, DensityRung, District, DistrictId, Focus,
    FrameInput, HallTarget, Station, StationId, agent_order, collapse_order, district_stack_order,
    render, solve_density, station_order, target_order,
};
use gwk_tui::input::HitMap;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

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
