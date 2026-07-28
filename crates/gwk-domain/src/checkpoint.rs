//! Projection checkpoints — the recovery shortcut, never the truth.
//!
//! A checkpoint is a hash-verified snapshot of the projection tables at one
//! `global_sequence`. Startup validates the newest one, falls back through
//! older valid ones, and finally replays the whole log; recovery ALWAYS replays
//! the suffix after the checkpoint, so a checkpoint can only save time, never
//! substitute for the log.

use crate::envelope::PayloadRef;
use crate::ids::{Seq, Timestamp};

/// The checkpoint format version this crate reads and writes.
pub const CHECKPOINT_SCHEMA_VERSION: u32 = 1;

/// Events between checkpoints — whichever bound trips first with
/// [`CHECKPOINT_INTERVAL_SECS`].
pub const CHECKPOINT_EVENT_INTERVAL: u64 = 10_000;

/// Seconds between checkpoints — whichever bound trips first with
/// [`CHECKPOINT_EVENT_INTERVAL`].
pub const CHECKPOINT_INTERVAL_SECS: u64 = 300;

/// One checkpoint record.
///
/// `projection_hash` is deterministic over the canonical projection records
/// sorted by table then primary key — that determinism is what lets startup
/// compare a restored checkpoint against live projections and REFUSE readiness
/// on disagreement instead of serving a silently divergent state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub schema_version: u32,
    /// The `global_sequence` this snapshot includes through.
    pub through_sequence: Seq,
    /// Lowercase 64-hex SHA-256 over the canonical records
    /// ([`crate::blob::is_sha256_hex`]).
    pub projection_hash: String,
    /// The encrypted blob holding those canonical records.
    pub records_ref: PayloadRef,
    pub created_at: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ByteCount;

    #[test]
    fn checkpoint_round_trips_with_a_decimal_string_sequence() {
        let cp = Checkpoint {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            through_sequence: Seq::new(9_007_199_254_740_993),
            projection_hash: "c".repeat(64),
            records_ref: PayloadRef {
                digest: format!("sha256:{}", "d".repeat(64)),
                media_type: "application/octet-stream".into(),
                byte_size: ByteCount::new(4096),
                retention_class: None,
                evidence_pin: None,
            },
            created_at: Timestamp::new("2026-07-28T00:00:00Z"),
        };
        let json = serde_json::to_string(&cp).expect("serialize");
        assert!(json.contains("\"through_sequence\":\"9007199254740993\""));
        assert!(crate::blob::is_sha256_hex(&cp.projection_hash));
        let back: Checkpoint = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, cp);
    }
}
