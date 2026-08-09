//! Identifier and scalar newtypes.
//!
//! Two families:
//!
//! - **Opaque string identifiers** — the contract does not prescribe an id scheme
//!   (UUID, ULID, …); ids are opaque tokens minted by the kernel and compared
//!   byte-for-byte. On the wire they are plain JSON strings.
//! - **64-bit counters as decimal strings** — any value that can exceed
//!   JavaScript's safe-integer range (2^53 − 1) crosses the wire as a canonical
//!   decimal string, never a JSON number. Canonical means: ASCII digits only, no
//!   sign, no leading zero (except `"0"` itself). Non-canonical input is rejected
//!   at deserialization so that decode → encode is byte-identical.

/// Declares an opaque string identifier newtype.
macro_rules! string_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(
            Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash,
            serde::Serialize, serde::Deserialize, specta::Type,
        )]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

/// Declares a u64 counter newtype carried on the wire as a canonical decimal string.
macro_rules! u64_decimal_string {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub u64);

        impl $name {
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            pub const fn value(self) -> u64 {
                self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(&self.0)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = <std::borrow::Cow<'de, str>>::deserialize(d)?;
                let canonical = !raw.is_empty()
                    && raw.len() <= 20
                    && raw.bytes().all(|b| b.is_ascii_digit())
                    && (raw.len() == 1 || !raw.starts_with('0'));
                if !canonical {
                    return Err(serde::de::Error::custom(concat!(
                        stringify!($name),
                        " must be a canonical decimal u64 string"
                    )));
                }
                raw.parse::<u64>().map(Self).map_err(serde::de::Error::custom)
            }
        }

        // The WIRE form is a string, so the exported TS type is `string`.
        impl specta::Type for $name {
            fn definition(types: &mut specta::Types) -> specta::datatype::DataType {
                <String as specta::Type>::definition(types)
            }
        }
    };
}

string_id!(
    /// An appended event's identity.
    EventId
);
string_id!(
    /// A command envelope's identity.
    CommandId
);
string_id!(
    /// The project (tenant / workspace) an aggregate belongs to.
    ProjectId
);
string_id!(
    /// An aggregate instance's identity, unique within its `aggregate_type`.
    AggregateId
);
string_id!(TaskId);
string_id!(AttemptId);
string_id!(MessageId);
string_id!(LeaseId);
string_id!(GateId);
string_id!(ReceiptId);
string_id!(WorktreeId);
string_id!(DispatchNodeId);
string_id!(AttentionItemId);
string_id!(AuthorityGrantId);
string_id!(EvidenceId);
string_id!(CostEntryId);
string_id!(WorkspaceNodeId);
string_id!(
    /// One ingested record. Kernel-derived rather than caller-named: the
    /// command carries no id, so the identity is minted from the
    /// `(project_id, idempotency_key)` pair the envelope already had to
    /// supply — which is what makes a retried ingest the same record.
    IngestedRecordId
);
string_id!(EngineSessionId);
string_id!(
    /// Correlates every event/command in one logical flow.
    CorrelationId
);
string_id!(
    /// Caller-chosen key that makes a command or transition retry-stable.
    IdempotencyKey
);
string_id!(
    /// An execution engine, as an OPEN identifier (new engines are additive,
    /// never a breaking enum change).
    EngineId
);
string_id!(
    /// Correlates one wire request with its response, event batch, or stream
    /// close. Client-minted and echoed back unchanged.
    RequestId
);
string_id!(
    /// One in-flight blob upload. Kernel-minted: a caller never names a
    /// storage location, and an uncommitted upload expires after an hour.
    BlobUploadId
);
string_id!(
    /// A hosted PTY session's identity, as the wire session registry names
    /// it. Host-minted: a caller never invents one, only echoes an id an
    /// earlier spawn handed back. Distinct from [`EngineSessionId`] — that is
    /// the provider-level engine conversation, this is the terminal I/O
    /// session a TUI attaches to, and the two do not share a lifecycle (an
    /// attempt may hold one while resuming across several engine sessions,
    /// or none at all).
    PtySessionId
);
string_id!(
    /// One lifetime of a hosted PTY session id. A reclaimed [`PtySessionId`]
    /// gets a new opaque generation so frame revisions from separate lives
    /// are never mistaken for one continuous cursor axis.
    PtySessionGeneration
);

u64_decimal_string!(
    /// A position in the global event log. Assigned by the kernel append actor
    /// in COMMIT order — unique and strictly increasing, but NOT gapless.
    Seq
);
u64_decimal_string!(
    /// A fencing token: strictly increasing per lease scope; a holder presenting
    /// a stale token is rejected by the storage layer.
    FenceToken
);
u64_decimal_string!(
    /// A size in bytes.
    ByteCount
);
u64_decimal_string!(
    /// The durable writer epoch: incremented under row lock at every kernel
    /// boot and compared by every mutating transaction, so a resurrected old
    /// writer cannot commit. NOT a log position — [`Seq`] is.
    WriterEpoch
);
u64_decimal_string!(
    /// A count of events. Distinct from [`Seq`]: the fresh-epoch proof asserts
    /// the log holds exactly ONE event, which says nothing about the sequence
    /// the database assigned it.
    EventCount
);
u64_decimal_string!(
    /// A count of tokens, as an engine reported it. Never converted to money:
    /// the ledger records counts and currency as separate engine-reported facts.
    TokenCount
);
u64_decimal_string!(
    /// A hosted PTY session's frame revision. Assigned by the session's own
    /// producer, strictly increasing within that session — NOT a [`Seq`], and
    /// not comparable across sessions. A snapshot's revision is the point a
    /// client applies later deltas after; an attach's `cursor` names the same
    /// axis to resume from on a reconnect, mirroring how [`Seq`] threads
    /// through [`crate::protocol::KernelRequest::SubscribeEvents`].
    PtyFrameSeq
);
u64_decimal_string!(
    /// A cost amount in micro-USD (1_000_000 = $1).
    CostMicros
);

/// An RFC 3339 timestamp carried opaquely (the contract pins the format, the
/// kernel validates it; a plain JSON string on the wire).
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    specta::Type,
)]
#[serde(transparent)]
pub struct Timestamp(pub String);

impl Timestamp {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Timestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_round_trips_as_decimal_string() {
        let seq = Seq::new(u64::MAX);
        let json = serde_json::to_string(&seq).expect("serialize");
        assert_eq!(json, "\"18446744073709551615\"");
        let back: Seq = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, seq);
    }

    #[test]
    fn seq_rejects_non_canonical_input() {
        for bad in [
            "\"\"",
            "\"01\"",
            "\"1e3\"",
            "\"-1\"",
            "\" 1\"",
            "\"18446744073709551616\"",
            "7",
        ] {
            assert!(
                serde_json::from_str::<Seq>(bad).is_err(),
                "accepted non-canonical {bad}"
            );
        }
        let zero: Seq = serde_json::from_str("\"0\"").expect("bare zero is canonical");
        assert_eq!(zero, Seq::new(0));
    }
}
