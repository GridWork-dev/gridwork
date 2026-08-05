//! Strict JSON decoding: what `serde_json` will not refuse on its own.
//!
//! Three refusals, and only the second needs code here:
//!
//! * **Unknown fields** — `deny_unknown_fields` on every contract type. That
//!   works because the field sets are closed, and it is why the four field-less
//!   requests are empty STRUCT variants rather than unit ones.
//! * **Duplicate keys, at every depth** — serde catches a duplicate of a field
//!   it KNOWS (`{"limit": 1, "limit": 2}` on a typed struct is a
//!   `duplicate field` error). It cannot catch one inside a
//!   `serde_json::Value`: a command's inline payload is free-form, so
//!   `{"cost": 1, "cost": 2}` three levels down silently keeps the last. That
//!   is the hole this module closes, and it matters because the log stores the
//!   payload verbatim — a reader replaying it would see a value the sender
//!   never confirmed.
//! * **Invalid UTF-8 and trailing bytes** — the body is bytes; both are refused
//!   before parsing rather than lossily repaired.
//!
//! The cost is one extra pass. `parse` walks the bytes into a
//! [`serde_json::Value`] rejecting duplicates as it goes, then the target type
//! is deserialized from that value.
//
// ponytail: two passes over a ≤4 MiB body. A wrapping `Deserializer` that
// tracked keys in one pass would avoid the intermediate Value; take that only
// if the phase's latency budget says this shows up, because the two-pass
// version is the one whose correctness is obvious by reading it.

use gwk_domain::protocol::KernelErrorCode;
use serde::Deserialize;
use serde::de::{DeserializeOwned, MapAccess, SeqAccess, Visitor};

use super::WireError;

/// How a duplicate-key refusal announces itself through `serde_json::Error`,
/// which carries no room for a code of our own.
///
/// The producer and the classifier both read this constant, so the one string
/// the two of them agree on cannot drift; `a_duplicate_key_is_refused_at_every_depth`
/// asserts the code, which is what fails if `serde_json` ever reworded around it.
const DUPLICATE_KEY: &str = "duplicate object key";

/// A `serde_json::Value` that refuses a duplicate key at ANY depth.
struct NoDuplicateKeys(serde_json::Value);

impl<'de> Deserialize<'de> for NoDuplicateKeys {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(StrictValue).map(NoDuplicateKeys)
    }
}

struct StrictValue;

impl<'de> Visitor<'de> for StrictValue {
    type Value = serde_json::Value;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON value with no duplicate object key")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Number(v.into()))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Number(v.into()))
    }

    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
        serde_json::Number::from_f64(v)
            .map(serde_json::Value::Number)
            .ok_or_else(|| E::custom("a non-finite number is not JSON"))
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        Ok(serde_json::Value::String(v.to_owned()))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(NoDuplicateKeys(value)) = seq.next_element()? {
            out.push(value);
        }
        Ok(serde_json::Value::Array(out))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut out = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let NoDuplicateKeys(value) = map.next_value()?;
            // The check is on INSERT, so it fires at whatever depth the
            // duplicate lives — the recursion is the visitor's, not a separate
            // walk that could disagree with it.
            if out.insert(key.clone(), value).is_some() {
                return Err(serde::de::Error::custom(format!("{DUPLICATE_KEY} {key:?}")));
            }
        }
        Ok(serde_json::Value::Object(out))
    }
}

/// Decode one frame body into `T`, refusing everything ADR 0001 requires.
///
/// The error code says which wall was hit: [`KernelErrorCode::Schema`] for
/// bytes that are not the shape the contract declares, and
/// [`KernelErrorCode::Validation`] for a value the contract's own types refuse.
/// Both are values a client can branch on, never a dropped connection.
pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, WireError> {
    let text = std::str::from_utf8(body).map_err(|e| {
        WireError::new(
            KernelErrorCode::Schema,
            format!("frame body is not UTF-8: {e}"),
        )
    })?;

    // Pass one: structure, with duplicate keys refused at every depth. A
    // duplicate gets its own contract code rather than the generic schema one —
    // the contract lists `duplicate_key` separately because a client retrying a
    // malformed request and a client sending the same key twice are different
    // bugs on the other end.
    let mut stream = serde_json::Deserializer::from_str(text).into_iter::<NoDuplicateKeys>();
    let NoDuplicateKeys(value) = stream
        .next()
        .transpose()
        .map_err(|e| {
            let text = e.to_string();
            let code = if text.starts_with(DUPLICATE_KEY) {
                KernelErrorCode::DuplicateKey
            } else {
                KernelErrorCode::Schema
            };
            WireError::new(code, format!("frame body: {text}"))
        })?
        .ok_or_else(|| WireError::new(KernelErrorCode::Schema, "frame body holds no JSON value"))?;
    // A second value after the first is trailing garbage, not a second frame.
    // `byte_offset` is past the first value, so anything but whitespace left
    // means the sender framed two things as one.
    let rest = text[stream.byte_offset()..].trim();
    if !rest.is_empty() {
        return Err(WireError::new(
            KernelErrorCode::Schema,
            format!("{} trailing bytes after the JSON value", rest.len()),
        ));
    }

    // Pass two: the contract's own types, where `deny_unknown_fields` and the
    // self-validating newtypes do the rest.
    serde_json::from_value(value)
        .map_err(|e| WireError::new(KernelErrorCode::Validation, format!("frame body: {e}")))
}

#[cfg(test)]
mod tests {
    use gwk_domain::protocol::{ClientControl, KernelRequest};

    use super::*;

    fn control(raw: &str) -> Result<ClientControl, WireError> {
        decode::<ClientControl>(raw.as_bytes())
    }

    #[test]
    fn a_well_formed_request_decodes() {
        let ok = control(r#"{"type":"request","request_id":"r-1","request":{"type":"health"}}"#)
            .expect("decode");
        match ok {
            ClientControl::Request { request, .. } => {
                assert_eq!(request, KernelRequest::Health {});
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_field_less_request_still_refuses_an_extra_field() {
        // The reason `Health` is `Health {}` and not `Health`. As a unit
        // variant serde would accept this and answer as though the caller had
        // asked for plain health — silently ignoring what they said.
        let error = control(
            r#"{"type":"request","request_id":"r-1","request":{"type":"health","limit":500}}"#,
        )
        .expect_err("extra field accepted");
        assert_eq!(error.code, KernelErrorCode::Validation);
        assert!(error.message.contains("limit"), "{error}");
    }

    #[test]
    fn an_unknown_field_on_a_known_request_is_refused() {
        let error = control(
            r#"{"type":"request","request_id":"r-1","request":{"type":"read_events","limit":5,"depth":2}}"#,
        )
        .expect_err("unknown field accepted");
        assert_eq!(error.code, KernelErrorCode::Validation);
        assert!(error.message.contains("depth"), "{error}");
    }

    #[test]
    fn a_duplicate_key_is_refused_at_every_depth() {
        // Top level, and nested inside a free-form payload three levels down —
        // the second is the one serde cannot see, because a `Value` has no
        // field set to compare against.
        for raw in [
            r#"{"type":"request","type":"request","request_id":"r-1","request":{"type":"health"}}"#,
            r#"{"type":"request","request_id":"r-1","request":{"type":"submit_command","envelope":{"command_id":"c","project_id":"p","command_type":"ingest_record","schema_version":1,"issued_at":"t","actor":{"kind":"kernel"},"origin":{"system":"gw"},"idempotency_key":"k","payload":{"deep":{"cost":1,"cost":2}}}}}"#,
        ] {
            let error = control(raw).expect_err("duplicate key accepted");
            assert_eq!(error.code, KernelErrorCode::DuplicateKey);
            assert!(error.message.contains(DUPLICATE_KEY), "{error}");
        }
    }

    #[test]
    fn a_legitimate_repeat_in_two_different_objects_is_not_a_duplicate() {
        // The check is per-object, not per-name: two array elements each having
        // a `kind` key is ordinary JSON, and refusing it would be a bug that
        // only shows up on real payloads.
        control(
            r#"{"type":"request","request_id":"r-1","request":{"type":"submit_command","envelope":{"command_id":"c","project_id":"p","command_type":"ingest_record","schema_version":1,"issued_at":"t","actor":{"kind":"kernel"},"origin":{"system":"gw"},"idempotency_key":"k","payload":{"rows":[{"kind":"a"},{"kind":"b"}]}}}}"#,
        )
        .expect("a repeated name in sibling objects was refused");
    }

    #[test]
    fn bytes_that_are_not_utf8_are_refused_before_parsing() {
        let error = decode::<ClientControl>(&[0x7b, 0xff, 0x7d]).expect_err("invalid UTF-8");
        assert_eq!(error.code, KernelErrorCode::Schema);
        assert!(error.message.contains("UTF-8"), "{error}");
    }

    #[test]
    fn a_second_value_in_one_frame_is_trailing_garbage() {
        let error = control(
            r#"{"type":"request","request_id":"r-1","request":{"type":"health"}}{"type":"request","request_id":"r-2","request":{"type":"status"}}"#,
        )
        .expect_err("two values in one frame accepted");
        assert_eq!(error.code, KernelErrorCode::Schema);
        assert!(error.message.contains("trailing"), "{error}");
    }

    #[test]
    fn an_empty_body_is_a_refusal_and_not_a_default() {
        let error = decode::<ClientControl>(b"  ").expect_err("empty body accepted");
        assert_eq!(error.code, KernelErrorCode::Schema);
    }

    proptest::proptest! {
        /// Each example above refuses ONE malformation. The decoder's promise
        /// is about every body: whatever the bytes, the answer is a decoded
        /// value or a typed refusal on one of the contract's three codes —
        /// never a panic, never a dropped connection. Bounded at 8 KiB for the
        /// sibling `frame.rs` property's reason: the 4 MiB ceiling is a
        /// boundary proved exactly elsewhere, while this is about the space in
        /// between.
        #[test]
        fn any_body_decodes_or_refuses_on_a_contract_code(
            body in proptest::collection::vec(proptest::num::u8::ANY, 0..8192)
        ) {
            if let Err(error) = decode::<ClientControl>(&body) {
                proptest::prop_assert!(
                    matches!(
                        error.code,
                        KernelErrorCode::Schema
                            | KernelErrorCode::Validation
                            | KernelErrorCode::DuplicateKey
                    ),
                    "{error}"
                );
            }
        }
    }
}
