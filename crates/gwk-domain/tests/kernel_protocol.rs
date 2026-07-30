//! Kernel protocol v1 contract tests.

use gwk_domain::{
    BlobAddress, CapabilityName, IngestionKind, KernelCommand, KernelErrorCode, ProtocolVersion,
};

#[test]
fn protocol_v1_is_strict_and_has_no_import_ingestion_kind() {
    assert_eq!(ProtocolVersion::V1.as_u32(), 1);
    assert!(CapabilityName::new("event_subscribe").is_ok());
    assert!(CapabilityName::new("EventSubscribe").is_err());
    assert!(BlobAddress::parse(&format!("sha256:{}", "a".repeat(64))).is_ok());
    assert!(BlobAddress::parse("sha256:ABC").is_err());
    assert!(
        !IngestionKind::ALL
            .iter()
            .any(|kind| kind.as_str() == "import")
    );
}

#[test]
fn command_and_error_variants_are_stable() {
    let command = KernelCommand::ActivateKernel {
        cutover_id: "00000000-0000-4000-8000-000000000001".to_owned(),
        archive_manifest_sha256: "b".repeat(64),
    };
    let json = serde_json::to_value(command).expect("serialize command");
    assert_eq!(json["type"], "activate_kernel");
    assert_eq!(
        KernelErrorCode::IdempotencyConflict.as_str(),
        "idempotency_conflict"
    );
}
