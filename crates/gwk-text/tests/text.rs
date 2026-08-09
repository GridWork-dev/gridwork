use gwk_text::{AmbiguousWidth, WidthProfile, cluster_width, grapheme_boundaries, text_width};

#[test]
fn ambiguous_width_defaults_narrow_and_changes_only_with_context() {
    assert_eq!(
        WidthProfile::default().ambiguous_width(),
        AmbiguousWidth::Narrow
    );
    assert_eq!(cluster_width("·", WidthProfile::default()), 1);
    assert_eq!(cluster_width("·", WidthProfile::east_asian()), 2);
    assert_eq!(cluster_width("界", WidthProfile::default()), 2);
}

#[test]
fn width_is_resolved_per_cluster_not_per_codepoint() {
    let family = "👨‍👧‍👦";
    assert_eq!(grapheme_boundaries(family), [0, family.len()]);
    assert_eq!(cluster_width(family, WidthProfile::default()), 2);
    assert_eq!(
        text_width(&format!("A{family}B"), WidthProfile::default()),
        4
    );

    let decomposed = "e\u{301}";
    assert_eq!(cluster_width(decomposed, WidthProfile::default()), 1);
    assert_eq!(cluster_width("\u{301}", WidthProfile::default()), 0);
}

#[test]
fn emoji_presentation_is_wide_except_for_a_lone_regional_indicator() {
    assert_eq!(cluster_width("⌚", WidthProfile::default()), 2);
    assert_eq!(cluster_width("🇦", WidthProfile::default()), 1);
    assert_eq!(cluster_width("🇦🇧", WidthProfile::default()), 2);
}

#[test]
fn variation_selectors_override_only_when_the_profile_honors_them() {
    let honors = WidthProfile::default();
    let ignores = honors.without_variation_selectors();

    assert_eq!(cluster_width("©\u{fe0f}", honors), 2);
    assert_eq!(cluster_width("©\u{fe0f}", ignores), 1);
    assert_eq!(cluster_width("⌚\u{fe0e}", honors), 1);
    assert_eq!(cluster_width("⌚\u{fe0e}", ignores), 2);
    assert_eq!(cluster_width("A\u{fe0f}", honors), 1);
    assert_eq!(cluster_width("\u{fe0f}", honors), 0);
}

#[test]
fn boundaries_cover_empty_ascii_and_degenerate_input() {
    assert_eq!(grapheme_boundaries(""), [0]);
    assert_eq!(grapheme_boundaries("ab"), [0, 1, 2]);
    assert_eq!(grapheme_boundaries("\0"), [0, 1]);
    assert_eq!(grapheme_boundaries("\u{301}"), [0, 2]);
}

#[test]
fn canonically_equivalent_forms_have_the_same_boundary_topology() {
    let composed = grapheme_boundaries("é");
    let decomposed = grapheme_boundaries("e\u{301}");

    assert_eq!(composed.len(), decomposed.len());
    assert_eq!(composed.len(), 2);
}

#[test]
fn indic_conjunct_runs_remain_one_editing_unit() {
    let conjunct = "\u{915}\u{308}\u{94d}\u{308}\u{915}";
    assert_eq!(grapheme_boundaries(conjunct), [0, conjunct.len()]);
}

#[test]
fn unbounded_emoji_zwj_runs_remain_one_editing_unit() {
    let family = "👨‍👩‍👧‍👦";
    assert_eq!(grapheme_boundaries(family), [0, family.len()]);
}

#[test]
fn regional_indicators_pair_from_the_start_of_each_run() {
    let indicators = "🇦🇧🇨🇩🇪";
    assert_eq!(grapheme_boundaries(indicators), [0, 8, 16, 20]);
}
