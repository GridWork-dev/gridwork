use gwk_text::{UNICODE_VERSION, grapheme_boundaries};

const BREAK: &str = "\u{00f7}";
const NO_BREAK: &str = "\u{00d7}";
const GRAPHEME_BREAK_TEST: &str = include_str!("../fixtures/ucd-17.0.0/GraphemeBreakTest.txt");

#[test]
fn published_grapheme_break_cases_match_the_pinned_unicode_version() {
    let expected_header = format!(
        "# GraphemeBreakTest-{}.{}.{}.txt",
        UNICODE_VERSION.0, UNICODE_VERSION.1, UNICODE_VERSION.2
    );
    assert_eq!(
        GRAPHEME_BREAK_TEST.lines().next(),
        Some(expected_header.as_str())
    );

    // This file samples property-value pairs; it is not exhaustive, so green is
    // not proof of full conformance. A boundary cannot be decided from only two
    // adjacent characters because rules such as GB9c, GB11, and GB12/13 inspect
    // runs. The file's rule numbering differs from UAX #29, so failures quote
    // the rule ids written in this file rather than substituting annex numbers.
    let checked =
        check_grapheme_break_cases(GRAPHEME_BREAK_TEST).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(checked, 766, "the vendored Unicode corpus changed shape");
}

#[test]
fn seeded_false_boundary_is_rejected_with_the_files_rule_id() {
    let seeded = "\u{00f7} 0061 \u{00f7} 0308 \u{00f7} # \u{00f7} [0.2] LATIN SMALL LETTER A \u{00f7} [999.0] COMBINING DIAERESIS \u{00f7} [0.3]";
    let error = check_grapheme_break_cases(seeded)
        .expect_err("a false break before an extending mark must be rejected");

    assert!(error.contains("[999.0]"), "{error}");
}

fn check_grapheme_break_cases(cases: &str) -> Result<usize, String> {
    let mut checked = 0;

    for (index, raw_line) in cases.lines().enumerate() {
        let line_number = index + 1;
        let (data, comment) = raw_line.split_once('#').unwrap_or((raw_line, ""));
        let data = data.trim();
        if data.is_empty() {
            continue;
        }

        let tokens: Vec<&str> = data.split_whitespace().collect();
        if tokens.len() < 3 || tokens.len().is_multiple_of(2) {
            return Err(format!(
                "GraphemeBreakTest line {line_number} has malformed marker/scalar pairs: {data}"
            ));
        }
        if tokens.first() != Some(&BREAK) || tokens.last() != Some(&BREAK) {
            return Err(format!(
                "GraphemeBreakTest line {line_number} must start and end with a break marker: {data}"
            ));
        }

        let mut text = String::new();
        let mut expected = Vec::new();
        for (token_index, token) in tokens.iter().enumerate() {
            if token_index.is_multiple_of(2) {
                match *token {
                    BREAK => expected.push(text.len()),
                    NO_BREAK => {}
                    marker => {
                        return Err(format!(
                            "GraphemeBreakTest line {line_number} has unknown marker {marker}: {data}"
                        ));
                    }
                }
                continue;
            }

            let scalar = u32::from_str_radix(token, 16).map_err(|error| {
                format!("GraphemeBreakTest line {line_number} has invalid scalar {token}: {error}")
            })?;
            let character = char::from_u32(scalar).ok_or_else(|| {
                format!("GraphemeBreakTest line {line_number} has non-scalar code point {token}")
            })?;
            text.push(character);
        }

        let rule_ids: Vec<&str> = comment
            .split_whitespace()
            .filter(|token| token.starts_with('[') && token.ends_with(']'))
            .collect();
        if rule_ids.is_empty() {
            return Err(format!(
                "GraphemeBreakTest line {line_number} names no file rule id: {raw_line}"
            ));
        }

        let actual = grapheme_boundaries(&text);
        if actual != expected {
            return Err(format!(
                "GraphemeBreakTest line {line_number}, file rules {}: expected {expected:?}, got {actual:?}; {data}",
                rule_ids.join(", ")
            ));
        }
        checked += 1;
    }

    if checked == 0 {
        return Err("GraphemeBreakTest contained no executable cases".to_owned());
    }
    Ok(checked)
}
