//! Workspace layout contract: durable tree structure, never transient geometry.

use gwk_domain::{
    KernelCommand, PtySessionId, Timestamp, WorkspaceNode, WorkspaceNodeId, WorkspaceNodeKind,
};

#[test]
fn workspace_node_is_a_strict_structural_projection() {
    let pane = WorkspaceNode {
        id: WorkspaceNodeId::new("pane-1"),
        version: 3,
        kind: WorkspaceNodeKind::Pane,
        parent_id: Some(WorkspaceNodeId::new("tab-1")),
        session_id: Some(PtySessionId::new("pty-1")),
        created_at: Timestamp::new("2026-08-09T12:00:00Z"),
        updated_at: Timestamp::new("2026-08-09T12:05:00Z"),
    };

    let json = serde_json::to_value(&pane).expect("serialize workspace node");
    assert_eq!(json["kind"], "pane");
    assert_eq!(json["parent_id"], "tab-1");
    assert_eq!(json["session_id"], "pty-1");

    let mut with_geometry = json;
    with_geometry["split_size"] = serde_json::json!(0.5);
    assert!(
        serde_json::from_value::<WorkspaceNode>(with_geometry).is_err(),
        "transient geometry must be rejected rather than entering the durable contract"
    );
}

#[test]
fn workspace_node_kinds_are_the_closed_three_level_tree() {
    for (kind, wire) in [
        (WorkspaceNodeKind::Workspace, "workspace"),
        (WorkspaceNodeKind::Tab, "tab"),
        (WorkspaceNodeKind::Pane, "pane"),
    ] {
        assert_eq!(serde_json::to_value(kind).expect("serialize kind"), wire);
    }
    assert!(serde_json::from_str::<WorkspaceNodeKind>("\"split\"").is_err());
}

#[test]
fn workspace_commands_are_structural_only() {
    let commands = [
        KernelCommand::CreateWorkspaceNode {
            workspace_node_id: WorkspaceNodeId::new("pane-1"),
            kind: WorkspaceNodeKind::Pane,
            parent_id: Some(WorkspaceNodeId::new("tab-1")),
            session_id: Some(PtySessionId::new("pty-1")),
        },
        KernelCommand::MoveWorkspaceNode {
            workspace_node_id: WorkspaceNodeId::new("pane-1"),
            parent_id: WorkspaceNodeId::new("tab-2"),
            expected_version: 1,
        },
        KernelCommand::RebindWorkspacePane {
            workspace_node_id: WorkspaceNodeId::new("pane-1"),
            session_id: PtySessionId::new("pty-2"),
            expected_version: 2,
        },
        KernelCommand::CloseWorkspaceNode {
            workspace_node_id: WorkspaceNodeId::new("pane-1"),
            expected_version: 3,
        },
    ];

    assert_eq!(
        commands.each_ref().map(|command| command.command_type()),
        [
            "create_workspace_node",
            "move_workspace_node",
            "rebind_workspace_pane",
            "close_workspace_node",
        ]
    );

    for command in commands {
        let mut json = serde_json::to_value(command).expect("serialize workspace command");
        for forbidden in ["split_size", "focus", "z_order", "rows", "cols", "pixels"] {
            assert!(
                json.get(forbidden).is_none(),
                "workspace command admitted transient geometry field {forbidden}"
            );
        }
        json["focus"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<KernelCommand>(json).is_err(),
            "workspace command accepted a transient focus flag"
        );
    }
}
