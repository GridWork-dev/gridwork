mod common;

use common::dump_frame;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::board::BoardView;
use gwk_tui::shell::{
    AttachRailItem, AttachRailState, AttachRailTerm, BudgetFormState, Lens, ShellAction, ShellMode,
    ShellState, Surface, TermState, TermTarget, render_attach_rail, render_budget_form,
    render_chrome, render_terms,
};

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn type_text(shell: &mut ShellState, text: &str) {
    for character in text.chars() {
        shell.handle(press(KeyCode::Char(character)));
    }
}

#[test]
fn flat_navigation_previews_live_and_escape_restores_the_origin() {
    let mut shell = ShellState::new(Surface::Hall);
    shell.handle(press(KeyCode::Char('/')));
    type_text(&mut shell, "running");
    shell.handle(press(KeyCode::Enter));
    shell.handle(press(KeyCode::Char(':')));
    assert_eq!(shell.mode(), ShellMode::Navigator);

    type_text(&mut shell, "cost");
    assert_eq!(shell.surface(), Surface::FleetCost);
    assert_eq!(shell.filter(), "");
    assert_eq!(
        shell.handle(press(KeyCode::Esc)),
        ShellAction::NavigatorCanceled(Surface::Hall)
    );
    assert_eq!(shell.surface(), Surface::Hall);
    assert_eq!(shell.filter(), "running");

    shell.handle(press(KeyCode::Char(':')));
    type_text(&mut shell, "messages");
    assert_eq!(
        shell.handle(press(KeyCode::Enter)),
        ShellAction::RouteChanged(Surface::FlowMessages)
    );
    assert_eq!(shell.mode(), ShellMode::View);
    assert_eq!(shell.surface(), Surface::FlowMessages);
}

#[test]
fn context_mutations_never_fire_without_a_confirm_step() {
    let mut shell = ShellState::new(Surface::WorkQueue);
    assert_eq!(
        shell.handle(press(KeyCode::Char('d'))),
        ShellAction::OpenGateDecision
    );
    assert_eq!(shell.mode(), ShellMode::GateDecision);
    assert_eq!(
        shell.handle(press(KeyCode::Char('j'))),
        ShellAction::MoveGateOption(1)
    );
    assert_eq!(
        shell.handle(press(KeyCode::Char('2'))),
        ShellAction::SelectGateOption(1)
    );
    assert_eq!(
        shell.handle(press(KeyCode::Enter)),
        ShellAction::CommitGateDecision
    );
    assert_eq!(shell.mode(), ShellMode::GateDecision);
}

#[test]
fn mute_collects_an_explicit_timestamp_before_the_confirm_step() {
    let mut shell = ShellState::new(Surface::WorkQueue);
    assert_eq!(
        shell.handle(press(KeyCode::Char('m'))),
        ShellAction::PrepareConfirmation(gwk_tui::shell::ContextVerb::MuteAttention)
    );
    shell.set_confirmation_target("attention a-watchdog");
    assert_eq!(
        shell.mode(),
        ShellMode::Prompt(gwk_tui::shell::ContextVerb::MuteAttention)
    );
    type_text(&mut shell, "2026-08-12T10:00:00Z");
    assert_eq!(shell.prompt_value(), "2026-08-12T10:00:00Z");
    assert_eq!(shell.handle(press(KeyCode::Enter)), ShellAction::None);
    assert_eq!(
        shell.mode(),
        ShellMode::Confirm(gwk_tui::shell::ContextVerb::MuteAttention)
    );
    assert_eq!(
        shell.handle(press(KeyCode::Enter)),
        ShellAction::Confirmed(gwk_tui::shell::ContextVerb::MuteAttention)
    );
}

#[test]
fn budget_key_opens_a_typed_form_before_confirming_the_replacement() {
    let mut shell = ShellState::new(Surface::FleetAgents);
    assert_eq!(
        shell.handle(press(KeyCode::Char('b'))),
        ShellAction::OpenBudgetForm
    );

    let board = common::estate::estate_board_state(BoardView::Fleet);
    let mut form = BudgetFormState::new(&board.attempts[0]);
    form.insert('9');
    form.move_selection(1);
    form.backspace();
    let budget = form.budget().expect("typed budget");
    assert_eq!(budget.max_tokens, Some(9));

    form.move_selection(1);
    form.move_selection(1);
    for character in "4294967296".chars() {
        form.insert(character);
    }
    assert_eq!(
        form.budget()
            .expect("cost uses the contract's u64 axis")
            .max_cost_micros
            .expect("cost cap")
            .value(),
        4_294_967_296
    );

    let rendered = dump_frame(
        120,
        24,
        ColorTier::Mono,
        GlyphSet::Ascii,
        |area, buffer, tier, _| render_budget_form(area, buffer, &form, tier),
    );
    assert!(rendered.contains("blank means uncapped"), "{rendered}");
    assert!(rendered.contains("changed"), "{rendered}");
}

#[test]
fn canceling_a_budget_confirmation_returns_to_the_form() {
    let mut shell = ShellState::new(Surface::FleetAgents);
    shell.enter_form();
    shell.confirm(gwk_tui::shell::ContextVerb::EditBudget);
    assert_eq!(
        shell.handle(press(KeyCode::Esc)),
        ShellAction::CancelConfirmation
    );
    assert_eq!(shell.mode(), ShellMode::Form);
}

#[test]
fn input_mode_forwards_every_key_except_ctrl_bracket() {
    let mut shell = ShellState::new(Surface::TermAttach);
    assert_eq!(
        shell.handle(press(KeyCode::Char('i'))),
        ShellAction::EnterInput
    );
    assert_eq!(shell.mode(), ShellMode::Input);
    assert_eq!(shell.handle(press(KeyCode::Esc)), ShellAction::ForwardInput);
    assert_eq!(
        shell.handle(press(KeyCode::Char('x'))),
        ShellAction::ForwardInput
    );
    assert_eq!(
        shell.handle(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL,)),
        ShellAction::LeaveInput
    );
    assert_eq!(shell.mode(), ShellMode::View);
}

#[test]
fn navigator_marks_unfocused_attention_and_keybar_keeps_degradation_state() {
    let mut shell = ShellState::new(Surface::Hall);
    shell.set_attention(Lens::Fleet, true);
    shell.handle(press(KeyCode::Char(':')));
    let rendered = dump_frame(
        80,
        24,
        ColorTier::Mono,
        GlyphSet::Ascii,
        |area, buffer, tier, glyphs| render_chrome(area, buffer, &shell, tier, glyphs, "off"),
    );
    assert!(rendered.contains("fleet !"), "{rendered}");
    assert!(
        rendered
            .lines()
            .last()
            .unwrap_or_default()
            .contains("mono+asc"),
        "{rendered}"
    );
    assert!(
        rendered.lines().last().unwrap_or_default().contains('+'),
        "{rendered}"
    );
}

#[test]
fn attach_header_and_estate_rail_keep_the_generation_and_operating_context_visible() {
    let mut shell = ShellState::new(Surface::TermAttach);
    shell.set_attach_subject("pty-1:gen-3  kernel shell");
    let state = AttachRailState {
        running: 4,
        attention: 2,
        blocked: 1,
        cost: "$5.30".into(),
        terms: vec![AttachRailTerm {
            subject: "pty-1:gen-3".into(),
            state: "running".into(),
            attached: true,
        }],
        queue: vec![AttachRailItem {
            text: "gate deploy".into(),
            attention: true,
        }],
    };
    let rendered = dump_frame(
        120,
        40,
        ColorTier::Mono,
        GlyphSet::Ascii,
        |area, buffer, tier, glyphs| {
            render_attach_rail(
                ratatui::layout::Rect::new(area.x, area.y + 2, 28, area.height - 3),
                buffer,
                &state,
                tier,
            );
            render_chrome(area, buffer, &shell, tier, glyphs, "off");
        },
    );
    assert!(
        rendered.contains("TERM   pty-1:gen-3  kernel shell"),
        "{rendered}"
    );
    assert!(rendered.contains("ESTATE"), "{rendered}");
    assert!(rendered.contains("> pty-1:gen-3"), "{rendered}");
    assert!(rendered.contains("! gate deploy"), "{rendered}");
}

#[test]
fn term_lifetimes_render_the_ruled_identity_and_missing_join() {
    let (running, closed) = common::estate::estate_pty_sessions();
    let state = TermState {
        sessions: vec![running, closed],
        watermark: Some(gwk_domain::ids::Seq::new(221)),
        complete: true,
    };
    let rendered = dump_frame(
        120,
        16,
        ColorTier::Mono,
        GlyphSet::Ascii,
        |area, buffer, tier, _| {
            render_terms(
                area,
                buffer,
                &state,
                Some(&TermTarget::Session(gwk_domain::ids::PtySessionId::new(
                    "pty-1",
                ))),
                tier,
            );
        },
    );
    assert!(rendered.contains("> pty-1"), "{rendered}");
    assert!(rendered.contains("gen-3"), "{rendered}");
    assert!(
        rendered.contains("?"),
        "the unavailable attempt join must be explicit:\n{rendered}"
    );
    assert!(rendered.contains("2 rows"), "{rendered}");
}
