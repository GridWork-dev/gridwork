//! The closed ingestion taxonomy.
//!
//! Ingestion is how non-aggregate records (memory, cost, health, …) enter the
//! log. The set is CLOSED in v1 — unlike the open classification strings
//! elsewhere in this contract — because ADR 0026 makes the absence of an
//! import path a load-bearing property, not a convention: a fresh epoch starts
//! empty and stays that way. An open string here would let `import` in as
//! data.

/// What kind of record is being ingested. Closed set; wire values are the
/// snake_case variant names.
///
/// There is deliberately NO `import`, `migrate`, `backfill`, or `legacy` kind.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    specta::Type,
)]
#[serde(rename_all = "snake_case")]
pub enum IngestionKind {
    Memory,
    Knowledge,
    GraphSnapshot,
    EvalVerdict,
    Insight,
    Cost,
    Health,
    Session,
    Config,
    Checkpoint,
    Round,
    Finding,
}

impl IngestionKind {
    /// Every kind, in declaration order.
    pub const ALL: &'static [Self] = &[
        Self::Memory,
        Self::Knowledge,
        Self::GraphSnapshot,
        Self::EvalVerdict,
        Self::Insight,
        Self::Cost,
        Self::Health,
        Self::Session,
        Self::Config,
        Self::Checkpoint,
        Self::Round,
        Self::Finding,
    ];

    /// The wire value — identical to what serde emits.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Knowledge => "knowledge",
            Self::GraphSnapshot => "graph_snapshot",
            Self::EvalVerdict => "eval_verdict",
            Self::Insight => "insight",
            Self::Cost => "cost",
            Self::Health => "health",
            Self::Session => "session",
            Self::Config => "config",
            Self::Checkpoint => "checkpoint",
            Self::Round => "round",
            Self::Finding => "finding",
        }
    }

    /// Parse a wire value (what `gw ingest submit --kind <kind>` receives).
    /// Unknown input is `None`, never a best-effort guess.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == value)
    }
}

impl std::fmt::Display for IngestionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_agrees_with_serde_for_every_kind() {
        for kind in IngestionKind::ALL {
            let wire = serde_json::to_value(kind).expect("serialize");
            assert_eq!(wire, serde_json::json!(kind.as_str()));
            assert_eq!(IngestionKind::parse(kind.as_str()), Some(*kind));
        }
        assert_eq!(IngestionKind::ALL.len(), 12);
    }

    #[test]
    fn there_is_no_import_path_into_a_fresh_epoch() {
        // ADR 0026: no legacy row, history, or outcome enters the new log.
        // A kind named for it would be exactly that door.
        for forbidden in ["import", "migrate", "migration", "backfill", "legacy"] {
            assert!(
                IngestionKind::parse(forbidden).is_none(),
                "ingestion kind {forbidden} exists"
            );
            assert!(
                serde_json::from_value::<IngestionKind>(serde_json::json!(forbidden)).is_err(),
                "ingestion kind {forbidden} decodes off the wire"
            );
        }
    }
}
