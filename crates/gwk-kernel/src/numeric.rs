//! Checked conversion between wire `u64` counters and PostgreSQL `numeric(20,0)`.
//!
//! The contract's 64-bit counters — `seq`, `fence_token`, `byte_size` — span
//! the full `u64` range, and PostgreSQL has no unsigned 64-bit type: `bigint`
//! tops out at 2^63-1, exactly half of it. `numeric(20,0)` holds the range, so
//! the schema uses it.
//!
//! Getting the value across is where precision goes to die. A driver that maps
//! `numeric` through `f64` silently rounds above 2^53, and one that maps it
//! through `i64` overflows above 2^63. This module refuses to go through either:
//! values cross as DECIMAL TEXT, which is exact for every `u64` by
//! construction, and parse back under a bounds check. That is also why the
//! queries in this crate say `seq::text` and `$1::numeric` rather than binding
//! a numeric type — the cast is the conversion.

/// A `numeric(20,0)` value that is not a `u64`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumericError {
    pub raw: String,
    pub reason: &'static str,
}

impl std::fmt::Display for NumericError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "numeric {:?} is not a u64: {}", self.raw, self.reason)
    }
}

impl std::error::Error for NumericError {}

/// Render a counter for a `$n::numeric` parameter.
pub fn to_numeric_text(value: u64) -> String {
    value.to_string()
}

/// Parse a `numeric(20,0)::text` result back into a counter.
///
/// Strict on purpose. A scale-0 column renders without a fractional part, so
/// anything else — `4.0`, `+4`, ` 4`, `4e2`, `NaN` — means the value did not
/// come from where this function assumes, and guessing at it would be how a
/// silently wrong sequence enters the log.
pub fn from_numeric_text(raw: &str) -> Result<u64, NumericError> {
    let bad = |reason: &'static str| NumericError {
        raw: raw.to_owned(),
        reason,
    };
    if raw.is_empty() {
        return Err(bad("empty"));
    }
    if !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad(
            "expected only ASCII digits (scale-0 numeric renders bare)",
        ));
    }
    raw.parse::<u64>().map_err(|_| bad("out of u64 range"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_boundary_survives_the_round_trip() {
        // 2^53 is where f64 stops being exact; 2^63 is where i64 overflows.
        // Both are inside the range this column must carry.
        for value in [
            0,
            1,
            9_007_199_254_740_992,     // 2^53
            9_007_199_254_740_993,     // 2^53 + 1, unrepresentable in f64
            9_223_372_036_854_775_807, // i64::MAX
            9_223_372_036_854_775_808, // i64::MAX + 1
            u64::MAX,
        ] {
            let text = to_numeric_text(value);
            assert_eq!(from_numeric_text(&text), Ok(value), "round trip of {value}");
        }
        assert_eq!(to_numeric_text(u64::MAX), "18446744073709551615");
    }

    proptest::proptest! {
        /// The boundary case above proves the five values that were REASONED
        /// about. This one covers the rest of the range, which is the part no
        /// list of examples can reach: the claim in this module's own header is
        /// that decimal text is exact for every `u64`, and "every" is a
        /// quantifier a generator can actually attack.
        #[test]
        fn a_counter_survives_the_round_trip_whatever_it_is(value: u64) {
            proptest::prop_assert_eq!(from_numeric_text(&to_numeric_text(value)), Ok(value));
        }

        /// And nothing outside that shape gets in.
        ///
        /// The generator is weighted, not uniform: an arbitrary Unicode string
        /// almost never looks like `+4` or `4e2`, which are exactly the strings a
        /// lax parser accepts as a sequence number. Two thirds of the cases are
        /// therefore drawn from the confusable alphabet — digits, signs, a
        /// decimal point, an exponent, a space — and the last third from
        /// anywhere, to catch a parser that trusts `char::is_numeric` (which is
        /// true of `٤` and `４`) instead of ASCII.
        #[test]
        fn only_bare_ascii_digits_parse(
            raw in proptest::prop_oneof![
                r"[0-9]{0,24}",
                r"[-+0-9.eE ]{0,12}",
                r"\PC*",
            ],
        ) {
            let bare = !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit());
            let parsed = from_numeric_text(&raw);
            if bare {
                // Still may be refused, but only for being out of range.
                proptest::prop_assert!(parsed.is_ok() || raw.parse::<u64>().is_err());
            } else {
                proptest::prop_assert!(parsed.is_err(), "{raw:?} parsed as a counter");
            }
        }
    }

    #[test]
    fn anything_that_is_not_a_bare_integer_is_refused() {
        for raw in [
            "",
            " 4",
            "4 ",
            "+4",
            "-1",
            "4.0",
            "4e2",
            "NaN",
            "0x10",
            "18446744073709551616", // u64::MAX + 1
            "99999999999999999999", // the numeric(20,0) ceiling, above u64
        ] {
            assert!(
                from_numeric_text(raw).is_err(),
                "{raw:?} must not parse as a counter"
            );
        }
    }
}
