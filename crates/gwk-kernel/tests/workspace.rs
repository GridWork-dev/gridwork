//! The durable workspace tree: structure in the kernel, geometry in the host.
//!
//! What the design is FOR: a reattaching client rebuilds its whole arrangement
//! from one query, and nothing about split sizes, focus, or z-order ever
//! reaches a row — those are the host's and die with it. These cases certify
//! the tree's shape rules against a real database: legal parentage (with the
//! trigger proven as the backstop by a seeded illegal row), close as a real
//! DELETE that refuses while children live, the no-cycle rule on moves, and
//! the pane-only session binding.
//!
//! `#[ignore]` because it needs a server — see `tests/common/mod.rs`.

mod common;

use common::{apply, drop_database, fresh_store, maintenance_pool, refuse};
use gwk_domain::command::KernelCommand;
use gwk_domain::entity::WorkspaceNodeKind;
use gwk_domain::ids::{PtySessionId, WorkspaceNodeId};
use gwk_domain::protocol::KernelErrorCode;
use sqlx::Row;

fn create(id: &str, kind: WorkspaceNodeKind, parent: Option<&str>) -> KernelCommand {
    KernelCommand::CreateWorkspaceNode {
        workspace_node_id: WorkspaceNodeId::new(id),
        kind,
        parent_id: parent.map(WorkspaceNodeId::new),
        session_id: None,
    }
}

fn create_pane(id: &str, parent: &str, session: Option<&str>) -> KernelCommand {
    KernelCommand::CreateWorkspaceNode {
        workspace_node_id: WorkspaceNodeId::new(id),
        kind: WorkspaceNodeKind::Pane,
        parent_id: Some(WorkspaceNodeId::new(parent)),
        session_id: session.map(PtySessionId::new),
    }
}

fn close(id: &str, expected_version: u32) -> KernelCommand {
    KernelCommand::CloseWorkspaceNode {
        workspace_node_id: WorkspaceNodeId::new(id),
        expected_version,
    }
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn the_whole_live_tree_returns_in_one_query_and_a_close_leaves_it() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ws_tree", 8).await;

    apply(
        &store,
        "ws",
        create("ws-1", WorkspaceNodeKind::Workspace, None),
    )
    .await;
    apply(
        &store,
        "tab",
        create("tab-1", WorkspaceNodeKind::Tab, Some("ws-1")),
    )
    .await;
    apply(&store, "p1", create_pane("pane-1", "tab-1", Some("pty-1"))).await;
    // A split: a pane under a pane is the legal nesting.
    apply(&store, "p2", create_pane("pane-2", "pane-1", None)).await;

    let rows =
        sqlx::query("SELECT id, kind, parent_id, session_id FROM gwk.workspace_node ORDER BY id")
            .fetch_all(store.pool())
            .await
            .expect("one query returns the tree");
    assert_eq!(rows.len(), 4);
    let parent_of = |wanted: &str| -> Option<String> {
        rows.iter()
            .find(|r| r.get::<String, _>("id") == wanted)
            .expect(wanted)
            .get("parent_id")
    };
    assert_eq!(parent_of("ws-1"), None);
    assert_eq!(parent_of("tab-1"), Some("ws-1".to_owned()));
    assert_eq!(parent_of("pane-1"), Some("tab-1".to_owned()));
    assert_eq!(parent_of("pane-2"), Some("pane-1".to_owned()));

    // A stale close is a CAS conflict, not a delete.
    let (code, _) = refuse(&store, "close-stale", close("pane-2", 7)).await;
    assert_eq!(code, KernelErrorCode::StaleVersion);

    // Rebind moves the version like any other write.
    apply(
        &store,
        "rebind",
        KernelCommand::RebindWorkspacePane {
            workspace_node_id: WorkspaceNodeId::new("pane-1"),
            session_id: PtySessionId::new("pty-2"),
            expected_version: 1,
        },
    )
    .await;

    // Bottom-up, the tree empties; each close is a real DELETE.
    apply(&store, "c2", close("pane-2", 1)).await;
    apply(&store, "c1", close("pane-1", 2)).await;
    apply(&store, "ct", close("tab-1", 1)).await;
    apply(&store, "cw", close("ws-1", 1)).await;
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM gwk.workspace_node")
        .fetch_one(store.pool())
        .await
        .expect("count");
    assert_eq!(left, 0, "a closed node leaves the tree");

    drop(store);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn illegal_parentage_is_refused_and_the_seeded_row_proves_the_trigger() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ws_parentage", 8).await;

    apply(
        &store,
        "ws",
        create("ws-1", WorkspaceNodeKind::Workspace, None),
    )
    .await;
    apply(
        &store,
        "tab",
        create("tab-1", WorkspaceNodeKind::Tab, Some("ws-1")),
    )
    .await;
    apply(&store, "p1", create_pane("pane-1", "tab-1", None)).await;

    // The kernel refuses before the append: a tab under a tab, a pane under a
    // workspace, a parent that does not exist.
    let (code, msg) = refuse(
        &store,
        "tab-under-tab",
        create("tab-2", WorkspaceNodeKind::Tab, Some("tab-1")),
    )
    .await;
    assert_eq!(code, KernelErrorCode::Validation, "{msg}");
    let (code, msg) = refuse(&store, "pane-under-ws", create_pane("pane-2", "ws-1", None)).await;
    assert_eq!(code, KernelErrorCode::Validation, "{msg}");
    let (code, msg) = refuse(
        &store,
        "orphan",
        create("tab-3", WorkspaceNodeKind::Tab, Some("nope")),
    )
    .await;
    assert_eq!(code, KernelErrorCode::Validation, "{msg}");

    // The seeded illegal row: a tab parented by a pane, written past the
    // kernel straight at the table. The trigger — not the submit path — must
    // be what refuses it, or the CHECK the migration promises is prose.
    let seeded = sqlx::query(
        "INSERT INTO gwk.workspace_node (id, kind, parent_id) \
         VALUES ('seeded-tab', 'tab', 'pane-1')",
    )
    .execute(store.pool())
    .await;
    let err = seeded.expect_err("a pane cannot parent a tab").to_string();
    assert!(
        err.contains("must sit under a workspace"),
        "the parentage trigger is the refusal: {err}"
    );

    drop(store);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_close_with_live_children_and_a_cycling_move_are_refused() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ws_shape", 8).await;

    apply(
        &store,
        "ws",
        create("ws-1", WorkspaceNodeKind::Workspace, None),
    )
    .await;
    apply(
        &store,
        "tab",
        create("tab-1", WorkspaceNodeKind::Tab, Some("ws-1")),
    )
    .await;
    apply(&store, "p1", create_pane("pane-1", "tab-1", None)).await;
    apply(&store, "p2", create_pane("pane-2", "pane-1", None)).await;

    // Close is bottom-up by contract: a parent with live children stays.
    let (code, msg) = refuse(&store, "close-parent", close("tab-1", 1)).await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(msg.contains("children"), "{msg}");

    // pane-1 sits above pane-2; moving it under its own descendant would
    // orphan the loop from every root.
    let (code, msg) = refuse(
        &store,
        "cycle",
        KernelCommand::MoveWorkspaceNode {
            workspace_node_id: WorkspaceNodeId::new("pane-1"),
            parent_id: WorkspaceNodeId::new("pane-2"),
            expected_version: 1,
        },
    )
    .await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(msg.contains("ancestor"), "{msg}");

    // A legal re-parent still works: pane-2 moves up beside pane-1.
    apply(
        &store,
        "legal-move",
        KernelCommand::MoveWorkspaceNode {
            workspace_node_id: WorkspaceNodeId::new("pane-2"),
            parent_id: WorkspaceNodeId::new("tab-1"),
            expected_version: 1,
        },
    )
    .await;

    drop(store);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_session_binding_belongs_to_panes_alone() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ws_binding", 8).await;

    apply(
        &store,
        "ws",
        create("ws-1", WorkspaceNodeKind::Workspace, None),
    )
    .await;
    apply(
        &store,
        "tab",
        create("tab-1", WorkspaceNodeKind::Tab, Some("ws-1")),
    )
    .await;

    // Rebinding a non-pane is refused by kind, before any CAS.
    let (code, msg) = refuse(
        &store,
        "rebind-tab",
        KernelCommand::RebindWorkspacePane {
            workspace_node_id: WorkspaceNodeId::new("tab-1"),
            session_id: PtySessionId::new("pty-1"),
            expected_version: 1,
        },
    )
    .await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(msg.contains("only a pane"), "{msg}");

    // The column CHECK is the backstop for a writer that skips the kernel.
    let seeded = sqlx::query(
        "UPDATE gwk.workspace_node SET session_id = 'pty-1', version = 2 WHERE id = 'tab-1'",
    )
    .execute(store.pool())
    .await;
    let err = seeded.expect_err("only a pane binds a session").to_string();
    assert!(
        err.contains("workspace_node_session_only_on_pane"),
        "the CHECK is the refusal: {err}"
    );

    drop(store);
    drop_database(&maintenance, &name).await;
}
