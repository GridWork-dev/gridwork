//! The live workspace wire model: durable-node facts beside transient geometry.
//!
//! The kernel remains the sole durable authority. This module plans typed
//! workspace commands against projection versions, applies a successful plan
//! to the host-local geometry, and rebuilds from projection truth after drift.
//! It also owns plural pane mirrors and exact request-to-pane demultiplexing.
//!
use std::collections::{BTreeMap, BTreeSet};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use gwk_domain::command::KernelCommand;
use gwk_domain::entity::{WorkspaceNode, WorkspaceNodeKind};
use gwk_domain::ids::{PtySessionId, RequestId, WorkspaceNodeId};
use gwk_domain::protocol::{KernelErrorCode, KernelResult, ServerControl};
use gwk_theme::ColorTier;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::arrange::{PaneBinding, WorkspaceBinding, reproduce};
use super::input::{Action, InputState};
use super::{Axis, PaneId, WorkspaceState};
use crate::drilldown::{self, DrilldownState, DrilldownTarget, IngestDisposition};
use crate::input::HitMap;
use crate::theme;

/// One socket's explicit active-tab ceiling. The protocol currently admits
/// eight subscriptions per connection; this socket carries PTY attaches only.
pub const ACTIVE_PANE_LIMIT: usize = gwk_domain::protocol::MAX_SUBSCRIPTIONS_PER_CONNECTION;
/// Durable input is bounded before recursive layout walks so a foreign or
/// future kernel cannot turn a valid projection page into a client stack walk.
pub const WORKSPACE_NODE_LIMIT: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
struct NodeFact {
    id: WorkspaceNodeId,
    version: u32,
    kind: WorkspaceNodeKind,
    parent: Option<WorkspaceNodeId>,
    session: Option<PtySessionId>,
}

impl From<&WorkspaceNode> for NodeFact {
    fn from(row: &WorkspaceNode) -> Self {
        Self {
            id: row.id.clone(),
            version: row.version,
            kind: row.kind,
            parent: row.parent_id.clone(),
            session: row.session_id.clone(),
        }
    }
}

/// The workspace state held by one console process.
#[derive(Debug)]
pub struct WorkspaceRuntime {
    pub state: WorkspaceState,
    pub input: InputState,
    bindings: BTreeMap<PaneId, PaneBinding>,
    containers: Vec<WorkspaceBinding>,
    facts: BTreeMap<WorkspaceNodeId, NodeFact>,
    attachments: BTreeMap<PaneId, DrilldownState>,
    request_panes: BTreeMap<RequestId, PaneId>,
    ignored: Vec<WorkspaceNodeId>,
}

impl WorkspaceRuntime {
    pub fn from_projection(rows: &[WorkspaceNode]) -> Self {
        let (accepted, mut overflow): (Vec<_>, Vec<_>) = if rows.len() > WORKSPACE_NODE_LIMIT {
            (Vec::new(), rows.iter().map(|row| row.id.clone()).collect())
        } else {
            (rows.to_vec(), Vec::new())
        };
        let rebuilt = reproduce(&accepted);
        overflow.extend(rebuilt.ignored);
        overflow.sort();
        overflow.dedup();
        Self {
            state: rebuilt.state,
            input: InputState::new(),
            bindings: rebuilt
                .bindings
                .into_iter()
                .map(|binding| (binding.pane, binding))
                .collect(),
            containers: rebuilt.workspaces,
            facts: accepted
                .iter()
                .map(|row| (row.id.clone(), NodeFact::from(row)))
                .collect(),
            attachments: BTreeMap::new(),
            request_panes: BTreeMap::new(),
            ignored: overflow,
        }
    }

    /// Adopt a fresh projection after startup or a concurrent-editor drift.
    /// Per-pane mirrors survive when the same durable pane still names the
    /// same session; geometry intentionally takes fresh defaults.
    pub fn replace_projection(&mut self, rows: &[WorkspaceNode]) {
        let old_by_node: BTreeMap<WorkspaceNodeId, (PtySessionId, DrilldownState)> = self
            .bindings
            .iter()
            .filter_map(|(pane, binding)| {
                binding.node.as_ref().and_then(|node| {
                    binding.session.as_ref().and_then(|lifetime| {
                        self.attachments
                            .remove(pane)
                            .map(|state| (node.clone(), (lifetime.clone(), state)))
                    })
                })
            })
            .collect();
        let input = std::mem::take(&mut self.input);
        let mut replacement = Self::from_projection(rows);
        replacement.input = input;
        for (pane, binding) in &replacement.bindings {
            if let Some(node) = &binding.node
                && let Some((lifetime, state)) = old_by_node.get(node)
                && binding.session.as_ref() == Some(lifetime)
            {
                replacement.attachments.insert(*pane, state.clone());
            }
        }
        *self = replacement;
    }

    pub fn ignored(&self) -> &[WorkspaceNodeId] {
        &self.ignored
    }

    /// Whether projection truth already matches the durable facts this client
    /// applied after successful commands. A match preserves host-only geometry
    /// when the same commands arrive on the event subscription.
    pub fn matches_projection(&self, rows: &[WorkspaceNode]) -> bool {
        if rows.len() > WORKSPACE_NODE_LIMIT {
            return false;
        }
        let incoming: BTreeMap<_, _> = rows
            .iter()
            .map(|row| (row.id.clone(), NodeFact::from(row)))
            .collect();
        incoming == self.facts
    }

    pub fn focused_pane(&self) -> Option<PaneId> {
        self.state.active_tab().map(super::Tab::focus)
    }

    pub fn focused_session(&self) -> Option<&PtySessionId> {
        self.focused_pane()
            .and_then(|pane| self.bindings.get(&pane))
            .and_then(|binding| binding.session.as_ref())
    }

    pub fn node_for_pane(&self, pane: PaneId) -> Option<WorkspaceNodeId> {
        self.bindings
            .get(&pane)
            .and_then(|binding| binding.node.clone())
    }

    pub fn session_for_pane(&self, pane: PaneId) -> Option<&PtySessionId> {
        self.bindings
            .get(&pane)
            .and_then(|binding| binding.session.as_ref())
    }

    pub fn panes_for_nodes(&self, nodes: &[WorkspaceNodeId]) -> Vec<PaneId> {
        let wanted: BTreeSet<_> = nodes.iter().collect();
        self.bindings
            .iter()
            .filter_map(|(pane, binding)| {
                binding
                    .node
                    .as_ref()
                    .is_some_and(|node| wanted.contains(node))
                    .then_some(*pane)
            })
            .collect()
    }

    pub fn attachment(&self, pane: PaneId) -> Option<&DrilldownState> {
        self.attachments.get(&pane)
    }

    pub fn attachment_mut(&mut self, pane: PaneId) -> Option<&mut DrilldownState> {
        self.attachments.get_mut(&pane)
    }

    pub fn visible_bound_panes(&self) -> Result<Vec<(PaneId, PtySessionId)>, String> {
        let Some(tab) = self.state.active_tab() else {
            return Ok(Vec::new());
        };
        let bound: Vec<_> = tab
            .pane_ids()
            .into_iter()
            .filter_map(|pane| {
                self.bindings
                    .get(&pane)
                    .and_then(|binding| binding.session.clone().map(|session| (pane, session)))
            })
            .collect();
        if bound.len() > ACTIVE_PANE_LIMIT {
            return Err(format!(
                "active tab has {} bound panes; one PTY socket admits {ACTIVE_PANE_LIMIT}",
                bound.len()
            ));
        }
        Ok(bound)
    }

    pub fn ensure_attachment(&mut self, pane: PaneId, session: PtySessionId) {
        let replace = self
            .attachments
            .get(&pane)
            .is_none_or(|state| state.session_id() != &session);
        if replace {
            self.attachments.insert(pane, DrilldownState::new(session));
        }
    }

    pub fn begin_attach(&mut self, pane: PaneId, request_id: RequestId) -> Result<(), String> {
        let state = self
            .attachments
            .get_mut(&pane)
            .ok_or_else(|| format!("pane {pane} has no session mirror"))?;
        state.begin_attach(request_id.clone());
        self.request_panes.insert(request_id, pane);
        Ok(())
    }

    pub fn clear_requests(&mut self) {
        self.request_panes.clear();
        for state in self.attachments.values_mut() {
            state.transport_closed();
        }
    }

    /// Route one PTY control to exactly one pane mirror.
    pub fn ingest(&mut self, control: &ServerControl) -> IngestEffect {
        let request_id = match control {
            ServerControl::Response {
                request_id,
                result: KernelResult::PtyAttached { .. } | KernelResult::Error { .. },
            }
            | ServerControl::PtyDeltaBatch { request_id, .. }
            | ServerControl::PtyStreamClosed { request_id, .. } => Some(request_id),
            _ => None,
        };
        let Some(request_id) = request_id else {
            return IngestEffect::default();
        };
        let Some(pane) = self.request_panes.get(request_id).copied() else {
            return IngestEffect::default();
        };
        let Some(state) = self.attachments.get_mut(&pane) else {
            self.request_panes.remove(request_id);
            return IngestEffect::default();
        };
        if let ServerControl::Response {
            result: KernelResult::PtyAttached { rows, cols, .. },
            ..
        } = control
            && usize::from(*rows)
                .checked_mul(usize::from(*cols))
                .is_none_or(|cells| cells > crate::drilldown::MIRROR_CELL_LIMIT)
        {
            self.request_panes.remove(request_id);
            state.refuse_attach(KernelErrorCode::Overloaded);
            return IngestEffect {
                pane: Some(pane),
                dirty: true,
                refusal: Some(KernelErrorCode::Overloaded),
                ..IngestEffect::default()
            };
        }
        if let ServerControl::Response {
            result: KernelResult::PtyAttached { generation, .. },
            ..
        } = control
            && state
                .generation()
                .is_some_and(|expected| expected != generation)
        {
            self.request_panes.remove(request_id);
            state.refuse_attach(KernelErrorCode::StaleVersion);
            return IngestEffect {
                pane: Some(pane),
                dirty: true,
                refusal: Some(KernelErrorCode::StaleVersion),
                ..IngestEffect::default()
            };
        }
        let disposition = state.ingest(control);
        let mut effect = IngestEffect {
            pane: Some(pane),
            dirty: disposition != IngestDisposition::Unrelated,
            needs_snapshot: disposition != IngestDisposition::Unrelated && state.cells().is_none(),
            ..IngestEffect::default()
        };
        match control {
            ServerControl::PtyStreamClosed { code, .. } => {
                self.request_panes.remove(request_id);
                effect.retired = *code == KernelErrorCode::NotFound;
                effect.reconnect = *code == KernelErrorCode::SlowConsumer;
            }
            ServerControl::Response {
                result: KernelResult::Error { code, .. },
                ..
            } => {
                self.request_panes.remove(request_id);
                effect.retired = *code == KernelErrorCode::NotFound;
                effect.reconnect = matches!(
                    code,
                    KernelErrorCode::SlowConsumer | KernelErrorCode::Overloaded
                );
            }
            _ => {}
        }
        effect
    }

    pub fn apply_host_action(&mut self, action: Action, area: Rect) -> bool {
        match action {
            Action::FocusLeft
            | Action::FocusRight
            | Action::FocusUp
            | Action::FocusDown
            | Action::NextTab
            | Action::PreviousTab
            | Action::NextWorkspace
            | Action::PreviousWorkspace
            | Action::SelectTab(_)
            | Action::GrowColumns
            | Action::ShrinkColumns
            | Action::GrowRows
            | Action::ShrinkRows => {
                super::input::apply(action, &mut self.state, area);
                true
            }
            _ => false,
        }
    }

    pub fn plan_action(&self, action: Action, ids: &[WorkspaceNodeId]) -> Result<Mutation, String> {
        match action {
            Action::NewWorkspace => {
                let [workspace, tab, pane] = ids else {
                    return Err("new workspace needs three node ids".to_owned());
                };
                Ok(Mutation::new(
                    vec![
                        create(workspace, WorkspaceNodeKind::Workspace, None, None),
                        create(tab, WorkspaceNodeKind::Tab, Some(workspace), None),
                        create(pane, WorkspaceNodeKind::Pane, Some(tab), None),
                    ],
                    MutationEffect::NewWorkspace {
                        workspace: workspace.clone(),
                        tab: tab.clone(),
                        pane: pane.clone(),
                        session: None,
                    },
                ))
            }
            Action::NewTab => {
                let [tab, pane] = ids else {
                    return Err("new tab needs two node ids".to_owned());
                };
                let workspace = self.active_workspace_node()?;
                Ok(Mutation::new(
                    vec![
                        create(tab, WorkspaceNodeKind::Tab, Some(&workspace), None),
                        create(pane, WorkspaceNodeKind::Pane, Some(tab), None),
                    ],
                    MutationEffect::NewTab {
                        workspace,
                        tab: tab.clone(),
                        pane: pane.clone(),
                    },
                ))
            }
            Action::SplitColumns | Action::SplitRows => {
                let [node] = ids else {
                    return Err("split needs one node id".to_owned());
                };
                let focused = self.focused_pane().ok_or("no focused pane")?;
                let parent = self
                    .bindings
                    .get(&focused)
                    .and_then(|binding| binding.node.clone())
                    .unwrap_or(self.active_tab_node()?);
                let axis = if action == Action::SplitColumns {
                    Axis::Columns
                } else {
                    Axis::Rows
                };
                Ok(Mutation::new(
                    vec![create(node, WorkspaceNodeKind::Pane, Some(&parent), None)],
                    MutationEffect::Split {
                        node: node.clone(),
                        parent,
                        axis,
                    },
                ))
            }
            Action::ClosePane => self.plan_close_focused(),
            Action::CloseTab => self.plan_close_tab(),
            Action::CloseWorkspace => self.plan_close_workspace(),
            _ => Err("action is host-local and needs no kernel plan".to_owned()),
        }
    }

    /// Bind `session` into the focused leaf, creating the minimum durable husk
    /// when the projection is empty or between container commands.
    pub fn plan_bind(
        &self,
        session: PtySessionId,
        ids: &[WorkspaceNodeId],
    ) -> Result<Option<Mutation>, String> {
        let Some(pane) = self.focused_pane() else {
            let [workspace, tab, node] = ids else {
                return Err("an empty workspace bind needs three node ids".to_owned());
            };
            return Ok(Some(Mutation::new(
                vec![
                    create(workspace, WorkspaceNodeKind::Workspace, None, None),
                    create(tab, WorkspaceNodeKind::Tab, Some(workspace), None),
                    create(node, WorkspaceNodeKind::Pane, Some(tab), Some(&session)),
                ],
                MutationEffect::NewWorkspace {
                    workspace: workspace.clone(),
                    tab: tab.clone(),
                    pane: node.clone(),
                    session: Some(session),
                },
            )));
        };
        let binding = self
            .bindings
            .get(&pane)
            .ok_or("focused pane is unindexed")?;
        if binding.session.as_ref() == Some(&session) {
            return Ok(None);
        }
        if let (Some(node), Some(version)) = (&binding.node, binding.version) {
            return Ok(Some(Mutation::new(
                vec![KernelCommand::RebindWorkspacePane {
                    workspace_node_id: node.clone(),
                    session_id: session.clone(),
                    expected_version: version,
                }],
                MutationEffect::Rebind { pane, session },
            )));
        }
        if let Ok(parent) = self.active_tab_node() {
            let node = ids
                .first()
                .ok_or("a tab placeholder bind needs one node id")?;
            return Ok(Some(Mutation::new(
                vec![create(
                    node,
                    WorkspaceNodeKind::Pane,
                    Some(&parent),
                    Some(&session),
                )],
                MutationEffect::FillPlaceholder {
                    pane,
                    node: node.clone(),
                    parent,
                    session,
                },
            )));
        }
        let workspace = self.active_workspace_node()?;
        let [tab, node, ..] = ids else {
            return Err("a workspace placeholder bind needs two node ids".to_owned());
        };
        Ok(Some(Mutation::new(
            vec![
                create(tab, WorkspaceNodeKind::Tab, Some(&workspace), None),
                create(node, WorkspaceNodeKind::Pane, Some(tab), Some(&session)),
            ],
            MutationEffect::FillWorkspacePlaceholder {
                pane,
                workspace,
                tab: tab.clone(),
                node: node.clone(),
                session,
            },
        )))
    }

    pub fn apply_mutation(&mut self, mutation: Mutation) {
        match mutation.effect {
            MutationEffect::NewWorkspace {
                workspace,
                tab,
                pane,
                session,
            } => {
                let local = self.state.create_workspace();
                self.containers.push(WorkspaceBinding {
                    node: workspace.clone(),
                    version: 1,
                    tabs: vec![super::arrange::TabBinding {
                        node: tab.clone(),
                        version: 1,
                    }],
                });
                self.insert_fact(workspace, 1, WorkspaceNodeKind::Workspace, None, None);
                self.insert_fact(
                    tab.clone(),
                    1,
                    WorkspaceNodeKind::Tab,
                    self.containers.last().map(|binding| binding.node.clone()),
                    None,
                );
                self.insert_binding(local, pane, tab, session);
            }
            MutationEffect::NewTab {
                workspace,
                tab,
                pane,
            } => {
                if let Some(local) = self.state.create_tab() {
                    if let Some(container) = self
                        .containers
                        .iter_mut()
                        .find(|binding| binding.node == workspace)
                    {
                        container.tabs.push(super::arrange::TabBinding {
                            node: tab.clone(),
                            version: 1,
                        });
                    }
                    self.insert_fact(
                        tab.clone(),
                        1,
                        WorkspaceNodeKind::Tab,
                        Some(workspace),
                        None,
                    );
                    self.insert_binding(local, pane, tab, None);
                }
            }
            MutationEffect::Split { node, parent, axis } => {
                if let Some(local) = self.state.split(axis) {
                    self.insert_binding(local, node, parent, None);
                }
            }
            MutationEffect::FillPlaceholder {
                pane,
                node,
                parent,
                session,
            } => self.insert_binding(pane, node, parent, Some(session)),
            MutationEffect::FillWorkspacePlaceholder {
                pane,
                workspace,
                tab,
                node,
                session,
            } => {
                if let Some(container) = self
                    .containers
                    .iter_mut()
                    .find(|binding| binding.node == workspace)
                {
                    container.tabs.push(super::arrange::TabBinding {
                        node: tab.clone(),
                        version: 1,
                    });
                }
                self.insert_fact(
                    tab.clone(),
                    1,
                    WorkspaceNodeKind::Tab,
                    Some(workspace),
                    None,
                );
                self.insert_binding(pane, node, tab, Some(session));
            }
            MutationEffect::Rebind { pane, session } => {
                if let Some(binding) = self.bindings.get_mut(&pane) {
                    binding.session = Some(session.clone());
                    binding.version = binding.version.map(|version| version.saturating_add(1));
                    if let Some(node) = &binding.node
                        && let Some(fact) = self.facts.get_mut(node)
                    {
                        fact.version = fact.version.saturating_add(1);
                        fact.session = Some(session.clone());
                    }
                }
                self.attachments.insert(pane, DrilldownState::new(session));
            }
            MutationEffect::ClosePane {
                pane,
                node,
                moved_children,
            } => {
                let parent = self
                    .facts
                    .get(&node)
                    .and_then(|closed| closed.parent.clone());
                for child in moved_children {
                    if let Some(fact) = self.facts.get_mut(&child) {
                        fact.version = fact.version.saturating_add(1);
                        fact.parent.clone_from(&parent);
                    }
                    if let Some((_, binding)) = self
                        .bindings
                        .iter_mut()
                        .find(|(_, binding)| binding.node.as_ref() == Some(&child))
                    {
                        binding.version = binding.version.map(|version| version.saturating_add(1));
                        binding.parent.clone_from(&parent);
                    }
                }
                self.state.focus_pane(pane);
                let preserve_container_husk = self
                    .state
                    .active_tab()
                    .is_some_and(|tab| tab.pane_count() == 1);
                if !preserve_container_husk {
                    self.state.close_pane();
                }
                self.remove_nodes(&[node]);
            }
            MutationEffect::CloseTab { nodes } => {
                self.state.close_tab();
                self.remove_nodes(&nodes);
            }
            MutationEffect::CloseWorkspace { nodes } => {
                self.state.close_workspace();
                self.remove_nodes(&nodes);
            }
        }
    }

    fn insert_binding(
        &mut self,
        pane: PaneId,
        node: WorkspaceNodeId,
        parent: WorkspaceNodeId,
        session: Option<PtySessionId>,
    ) {
        self.insert_fact(
            node.clone(),
            1,
            WorkspaceNodeKind::Pane,
            Some(parent.clone()),
            session.clone(),
        );
        self.bindings.insert(
            pane,
            PaneBinding {
                pane,
                node: Some(node),
                version: Some(1),
                parent: Some(parent),
                session: session.clone(),
            },
        );
        if let Some(session) = session {
            self.attachments.insert(pane, DrilldownState::new(session));
        }
    }

    fn insert_fact(
        &mut self,
        id: WorkspaceNodeId,
        version: u32,
        kind: WorkspaceNodeKind,
        parent: Option<WorkspaceNodeId>,
        session: Option<PtySessionId>,
    ) {
        self.facts.insert(
            id.clone(),
            NodeFact {
                id,
                version,
                kind,
                parent,
                session,
            },
        );
    }

    fn remove_nodes(&mut self, nodes: &[WorkspaceNodeId]) {
        let removed: BTreeSet<_> = nodes.iter().cloned().collect();
        let panes: Vec<_> = self
            .bindings
            .iter()
            .filter_map(|(pane, binding)| {
                binding
                    .node
                    .as_ref()
                    .is_some_and(|node| removed.contains(node))
                    .then_some(*pane)
            })
            .collect();
        for pane in panes {
            self.bindings.remove(&pane);
            self.attachments.remove(&pane);
            self.request_panes.retain(|_, target| *target != pane);
        }
        for node in &removed {
            self.facts.remove(node);
        }
        self.containers.retain_mut(|workspace| {
            workspace.tabs.retain(|tab| !removed.contains(&tab.node));
            !removed.contains(&workspace.node)
        });
    }

    fn plan_close_focused(&self) -> Result<Mutation, String> {
        let pane = self.focused_pane().ok_or("no focused pane")?;
        self.plan_close_pane(pane)
    }

    pub fn plan_close_pane(&self, pane: PaneId) -> Result<Mutation, String> {
        let binding = self
            .bindings
            .get(&pane)
            .ok_or("focused pane is unindexed")?;
        let node = binding
            .node
            .clone()
            .ok_or("the focused pane is a placeholder")?;
        let fact = self.facts.get(&node).ok_or("focused pane row is absent")?;
        let parent = fact.parent.clone().ok_or("a pane has no durable parent")?;
        let children: Vec<_> = self
            .facts
            .values()
            .filter(|candidate| candidate.parent.as_ref() == Some(&node))
            .map(|candidate| candidate.id.clone())
            .collect();
        let mut commands = Vec::new();
        for child in &children {
            let child_fact = self.facts.get(child).expect("child came from facts");
            commands.push(KernelCommand::MoveWorkspaceNode {
                workspace_node_id: child.clone(),
                parent_id: parent.clone(),
                expected_version: child_fact.version,
            });
        }
        commands.push(KernelCommand::CloseWorkspaceNode {
            workspace_node_id: node.clone(),
            expected_version: fact.version,
        });
        Ok(Mutation::new(
            commands,
            MutationEffect::ClosePane {
                pane,
                node,
                moved_children: children,
            },
        ))
    }

    pub fn panes_for_session(&self, session: &PtySessionId) -> Vec<PaneId> {
        self.bindings
            .iter()
            .filter_map(|(pane, binding)| {
                (binding.session.as_ref() == Some(session)).then_some(*pane)
            })
            .collect()
    }

    fn plan_close_tab(&self) -> Result<Mutation, String> {
        let tab = self.active_tab_node()?;
        self.plan_close_subtree(tab, true)
            .map(|(commands, nodes)| Mutation::new(commands, MutationEffect::CloseTab { nodes }))
    }

    fn plan_close_workspace(&self) -> Result<Mutation, String> {
        let workspace = self.active_workspace_node()?;
        self.plan_close_subtree(workspace, true)
            .map(|(commands, nodes)| {
                Mutation::new(commands, MutationEffect::CloseWorkspace { nodes })
            })
    }

    fn plan_close_subtree(
        &self,
        root: WorkspaceNodeId,
        include_root: bool,
    ) -> Result<(Vec<KernelCommand>, Vec<WorkspaceNodeId>), String> {
        fn visit(
            facts: &BTreeMap<WorkspaceNodeId, NodeFact>,
            parent: &WorkspaceNodeId,
            out: &mut Vec<WorkspaceNodeId>,
        ) {
            let children: Vec<_> = facts
                .values()
                .filter(|fact| fact.parent.as_ref() == Some(parent))
                .map(|fact| fact.id.clone())
                .collect();
            for child in children {
                visit(facts, &child, out);
                out.push(child);
            }
        }
        let mut nodes = Vec::new();
        visit(&self.facts, &root, &mut nodes);
        if include_root {
            nodes.push(root);
        }
        let commands = nodes
            .iter()
            .map(|node| {
                let fact = self
                    .facts
                    .get(node)
                    .ok_or_else(|| format!("workspace node {node} is absent"))?;
                Ok(KernelCommand::CloseWorkspaceNode {
                    workspace_node_id: node.clone(),
                    expected_version: fact.version,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok((commands, nodes))
    }

    fn active_workspace_node(&self) -> Result<WorkspaceNodeId, String> {
        self.containers
            .get(self.state.active_index())
            .map(|binding| binding.node.clone())
            .ok_or_else(|| "the active workspace has no durable node".to_owned())
    }

    fn active_tab_node(&self) -> Result<WorkspaceNodeId, String> {
        let workspace = self
            .containers
            .get(self.state.active_index())
            .ok_or("the active workspace has no durable node")?;
        let tab = self
            .state
            .active_workspace()
            .ok_or("there is no active workspace")?
            .active_index();
        workspace
            .tabs
            .get(tab)
            .map(|binding| binding.node.clone())
            .ok_or_else(|| "the active tab has no durable node".to_owned())
    }
}

#[derive(Debug, Clone)]
pub struct Mutation {
    commands: Vec<KernelCommand>,
    effect: MutationEffect,
}

impl Mutation {
    fn new(commands: Vec<KernelCommand>, effect: MutationEffect) -> Self {
        Self { commands, effect }
    }

    pub fn commands(&self) -> &[KernelCommand] {
        &self.commands
    }

    /// Whether refreshed projection truth proves that this command already
    /// reached its intended postcondition. This makes a multi-command intent
    /// resumable after a lost response or a partial CAS race.
    pub fn command_satisfied(&self, command: &KernelCommand, rows: &[WorkspaceNode]) -> bool {
        let row = |id: &WorkspaceNodeId| rows.iter().find(|row| &row.id == id);
        match command {
            KernelCommand::CreateWorkspaceNode {
                workspace_node_id,
                kind,
                parent_id,
                session_id,
            } => row(workspace_node_id).is_some_and(|existing| {
                existing.kind == *kind
                    && existing.parent_id == *parent_id
                    && existing.session_id == *session_id
            }),
            KernelCommand::MoveWorkspaceNode {
                workspace_node_id,
                parent_id,
                expected_version,
            } => row(workspace_node_id).is_some_and(|existing| {
                existing.parent_id.as_ref() == Some(parent_id)
                    && existing.version > *expected_version
            }),
            KernelCommand::RebindWorkspacePane {
                workspace_node_id,
                session_id,
                expected_version,
            } => row(workspace_node_id).is_some_and(|existing| {
                existing.session_id.as_ref() == Some(session_id)
                    && existing.version > *expected_version
            }),
            KernelCommand::CloseWorkspaceNode {
                workspace_node_id, ..
            } => row(workspace_node_id).is_none(),
            _ => false,
        }
    }

    /// Whether every command's intended postcondition is present in one
    /// coherent projection snapshot.
    pub fn satisfied(&self, rows: &[WorkspaceNode]) -> bool {
        self.commands
            .iter()
            .all(|command| self.command_satisfied(command, rows))
    }
}

#[derive(Debug, Clone)]
enum MutationEffect {
    NewWorkspace {
        workspace: WorkspaceNodeId,
        tab: WorkspaceNodeId,
        pane: WorkspaceNodeId,
        session: Option<PtySessionId>,
    },
    NewTab {
        workspace: WorkspaceNodeId,
        tab: WorkspaceNodeId,
        pane: WorkspaceNodeId,
    },
    Split {
        node: WorkspaceNodeId,
        parent: WorkspaceNodeId,
        axis: Axis,
    },
    FillPlaceholder {
        pane: PaneId,
        node: WorkspaceNodeId,
        parent: WorkspaceNodeId,
        session: PtySessionId,
    },
    FillWorkspacePlaceholder {
        pane: PaneId,
        workspace: WorkspaceNodeId,
        tab: WorkspaceNodeId,
        node: WorkspaceNodeId,
        session: PtySessionId,
    },
    Rebind {
        pane: PaneId,
        session: PtySessionId,
    },
    ClosePane {
        pane: PaneId,
        node: WorkspaceNodeId,
        moved_children: Vec<WorkspaceNodeId>,
    },
    CloseTab {
        nodes: Vec<WorkspaceNodeId>,
    },
    CloseWorkspace {
        nodes: Vec<WorkspaceNodeId>,
    },
}

fn create(
    id: &WorkspaceNodeId,
    kind: WorkspaceNodeKind,
    parent: Option<&WorkspaceNodeId>,
    session: Option<&PtySessionId>,
) -> KernelCommand {
    KernelCommand::CreateWorkspaceNode {
        workspace_node_id: id.clone(),
        kind,
        parent_id: parent.cloned(),
        session_id: session.cloned(),
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IngestEffect {
    pub pane: Option<PaneId>,
    pub dirty: bool,
    pub needs_snapshot: bool,
    pub reconnect: bool,
    pub retired: bool,
    pub refusal: Option<KernelErrorCode>,
}

/// Paint every active pane's content after workspace furniture.
pub fn render_panes(area: Rect, buf: &mut Buffer, runtime: &WorkspaceRuntime, tier: ColorTier) {
    let focused = runtime.focused_pane();
    for (pane, rect) in super::render::pane_content_rects(area, &runtime.state) {
        if let Some(state) = runtime.attachment(pane) {
            let selected = DrilldownTarget::Session(state.session_id().clone());
            drilldown::render(
                rect,
                buf,
                state,
                (focused == Some(pane)).then_some(&selected),
                tier,
                &mut HitMap::new(),
            );
        } else {
            let binding = runtime.bindings.get(&pane);
            let text = binding
                .and_then(|binding| binding.session.as_ref())
                .map_or_else(
                    || "unbound pane".to_owned(),
                    |session| format!("{session}  state ?"),
                );
            let safe = theme::safe_text(&text, rect.width as usize);
            buf.set_stringn(
                rect.x,
                rect.y,
                safe.as_ref(),
                rect.width as usize,
                theme::state_style(theme::binding("idle"), tier),
            );
        }
    }
}

/// Encode one crossterm key using the ruled normal xterm compatibility profile.
//
// Derivation: XTERM-CTLSEQS §Special Keyboard Keys, §PC-Style Function Keys —
// normal-mode cursor/editing keys, F1-F12, and xterm modifier parameters.
// Application cursor mode is deliberately not guessed because the shipped
// frame wire carries no DECCKM state.
pub fn encode_key(key: KeyEvent) -> Result<Vec<u8>, String> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return Ok(Vec::new());
    }
    if key
        .modifiers
        .intersects(KeyModifiers::SUPER | KeyModifiers::HYPER)
    {
        return Err("super/hyper keys have no xterm compatibility encoding".to_owned());
    }

    let modifiers = key.modifiers;
    let modified = modifiers.intersects(
        KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::META,
    );
    let parameter = modifier_parameter(modifiers);
    let special = match key.code {
        KeyCode::Left => cursor('D', modified, parameter),
        KeyCode::Right => cursor('C', modified, parameter),
        KeyCode::Up => cursor('A', modified, parameter),
        KeyCode::Down => cursor('B', modified, parameter),
        KeyCode::Home => cursor('H', modified, parameter),
        KeyCode::End => cursor('F', modified, parameter),
        KeyCode::Insert => tilde(2, modified, parameter),
        KeyCode::Delete => tilde(3, modified, parameter),
        KeyCode::PageUp => tilde(5, modified, parameter),
        KeyCode::PageDown => tilde(6, modified, parameter),
        KeyCode::F(number @ 1..=4) => {
            let final_byte = char::from_u32(u32::from(b'P') + u32::from(number - 1))
                .expect("F1-F4 map to ASCII finals");
            if modified {
                format!("\x1b[1;{parameter}{final_byte}").into_bytes()
            } else {
                format!("\x1bO{final_byte}").into_bytes()
            }
        }
        KeyCode::F(number @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][usize::from(number - 5)];
            tilde(code, modified, parameter)
        }
        KeyCode::F(number) => {
            return Err(format!(
                "F{number} is profile-dependent above F12; no bytes sent"
            ));
        }
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Null => vec![0],
        KeyCode::Char(character) => return encode_character(character, modifiers),
        other => return Err(format!("{other:?} has no xterm compatibility encoding")),
    };
    Ok(special)
}

fn cursor(final_byte: char, modified: bool, parameter: u8) -> Vec<u8> {
    if modified {
        format!("\x1b[1;{parameter}{final_byte}").into_bytes()
    } else {
        format!("\x1b[{final_byte}").into_bytes()
    }
}

fn tilde(code: u8, modified: bool, parameter: u8) -> Vec<u8> {
    if modified {
        format!("\x1b[{code};{parameter}~").into_bytes()
    } else {
        format!("\x1b[{code}~").into_bytes()
    }
}

fn modifier_parameter(modifiers: KeyModifiers) -> u8 {
    1 + u8::from(modifiers.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(modifiers.contains(KeyModifiers::ALT))
        + 4 * u8::from(modifiers.contains(KeyModifiers::CONTROL))
        + 8 * u8::from(modifiers.contains(KeyModifiers::META))
}

fn encode_character(character: char, modifiers: KeyModifiers) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    // Derivation: XTERM-CTLSEQS §Alt and Meta Keys — this compatibility
    // profile uses the documented escape-prefix behavior rather than 8-bit
    // input; no runtime resource setting is exposed by the shipped wire.
    if modifiers.intersects(KeyModifiers::ALT | KeyModifiers::META) {
        bytes.push(0x1b);
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        // Derivation: KITTY-KBD §Legacy key event encoding, Legacy text keys;
        // §Legacy ctrl mapping of ASCII keys — legacy Ctrl-key input maps the
        // listed ASCII keys to their documented C0/DEL bytes, leaves other
        // ASCII keys unchanged, and reserves Ctrl+Shift for CSI-u encoding.
        if modifiers.contains(KeyModifiers::SHIFT) {
            return Err(format!(
                "Ctrl-Shift-{character} requires an unnegotiated CSI-u profile"
            ));
        }
        let code = match character {
            '@' | ' ' | '2' => 0,
            'a'..='z' => character as u8 - b'a' + 1,
            '[' | '3' => 27,
            '\\' | '4' => 28,
            ']' | '5' => 29,
            '^' | '6' | '~' => 30,
            '_' | '7' | '/' => 31,
            '?' | '8' => 127,
            other if other.is_ascii() => other as u8,
            _ => return Err(format!("Ctrl-{character} has no compatibility byte")),
        };
        bytes.push(code);
    } else {
        let mut encoded = [0; 4];
        bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use gwk_domain::ids::Timestamp;

    use super::*;

    fn row(
        id: &str,
        kind: WorkspaceNodeKind,
        parent: Option<&str>,
        session: Option<&str>,
    ) -> WorkspaceNode {
        WorkspaceNode {
            id: WorkspaceNodeId::new(id),
            version: 1,
            kind,
            parent_id: parent.map(WorkspaceNodeId::new),
            session_id: session.map(PtySessionId::new),
            created_at: Timestamp::new(id),
            updated_at: Timestamp::new(id),
        }
    }

    fn fixture() -> WorkspaceRuntime {
        WorkspaceRuntime::from_projection(&[
            row("ws", WorkspaceNodeKind::Workspace, None, None),
            row("tab", WorkspaceNodeKind::Tab, Some("ws"), None),
            row("p1", WorkspaceNodeKind::Pane, Some("tab"), Some("s1")),
            row("p2", WorkspaceNodeKind::Pane, Some("p1"), Some("s2")),
        ])
    }

    #[test]
    fn runtime_preserves_versions_and_plans_a_child_safe_pane_close() {
        let runtime = fixture();
        let mutation = runtime
            .plan_action(Action::ClosePane, &[])
            .expect("close plan");
        assert_eq!(
            mutation.commands().len(),
            2,
            "move child, then close parent"
        );
        assert!(matches!(
            &mutation.commands()[0],
            KernelCommand::MoveWorkspaceNode {
                expected_version: 1,
                ..
            }
        ));
        assert!(matches!(
            &mutation.commands()[1],
            KernelCommand::CloseWorkspaceNode {
                expected_version: 1,
                ..
            }
        ));
    }

    #[test]
    fn mutation_commands_recognize_projection_postconditions_for_resume() {
        let create_command = create(
            &WorkspaceNodeId::new("new-pane"),
            WorkspaceNodeKind::Pane,
            Some(&WorkspaceNodeId::new("tab")),
            None,
        );
        let mut rows = vec![
            row("ws", WorkspaceNodeKind::Workspace, None, None),
            row("tab", WorkspaceNodeKind::Tab, Some("ws"), None),
            row("new-pane", WorkspaceNodeKind::Pane, Some("tab"), None),
        ];
        assert!(
            Mutation::new(vec![], MutationEffect::CloseTab { nodes: vec![] })
                .command_satisfied(&create_command, &rows)
        );

        let close = KernelCommand::CloseWorkspaceNode {
            workspace_node_id: WorkspaceNodeId::new("new-pane"),
            expected_version: 1,
        };
        rows.pop();
        assert!(
            Mutation::new(vec![], MutationEffect::CloseTab { nodes: vec![] })
                .command_satisfied(&close, &rows)
        );
        assert!(
            Mutation::new(
                vec![create(
                    &WorkspaceNodeId::new("tab"),
                    WorkspaceNodeKind::Tab,
                    Some(&WorkspaceNodeId::new("ws")),
                    None,
                )],
                MutationEffect::CloseTab { nodes: vec![] },
            )
            .satisfied(&rows)
        );
    }

    #[test]
    fn oversized_projection_is_rejected_whole_before_recursive_layout() {
        let rows: Vec<_> = (0..=WORKSPACE_NODE_LIMIT)
            .map(|index| {
                row(
                    &format!("ws-{index}"),
                    WorkspaceNodeKind::Workspace,
                    None,
                    None,
                )
            })
            .collect();
        let runtime = WorkspaceRuntime::from_projection(&rows);

        assert_eq!(runtime.state.workspace_count(), 0);
        assert_eq!(runtime.ignored().len(), rows.len());
    }

    #[test]
    fn projection_refresh_preserves_a_wire_id_mirror_for_the_same_lifetime_binding() {
        let rows = [
            row("ws", WorkspaceNodeKind::Workspace, None, None),
            row("tab", WorkspaceNodeKind::Tab, Some("ws"), None),
            row(
                "pane",
                WorkspaceNodeKind::Pane,
                Some("tab"),
                Some("wire:life"),
            ),
        ];
        let mut runtime = WorkspaceRuntime::from_projection(&rows);
        let pane = runtime.focused_pane().unwrap();
        runtime.ensure_attachment(pane, PtySessionId::new("wire"));

        runtime.replace_projection(&rows);

        let pane = runtime.focused_pane().unwrap();
        assert_eq!(
            runtime.attachment(pane).unwrap().session_id().as_str(),
            "wire"
        );
    }

    #[test]
    fn closing_the_last_durable_pane_keeps_its_tab_and_workspace_as_a_placeholder() {
        let rows = [
            row("ws", WorkspaceNodeKind::Workspace, None, None),
            row("tab", WorkspaceNodeKind::Tab, Some("ws"), None),
            row(
                "pane",
                WorkspaceNodeKind::Pane,
                Some("tab"),
                Some("session"),
            ),
        ];
        let mut runtime = WorkspaceRuntime::from_projection(&rows);
        let pane = runtime.focused_pane().expect("focused pane");
        let mutation = runtime.plan_close_pane(pane).expect("close plan");

        runtime.apply_mutation(mutation);

        assert_eq!(runtime.state.workspace_count(), 1);
        assert_eq!(
            runtime
                .state
                .active_workspace()
                .expect("workspace")
                .tab_count(),
            1
        );
        assert_eq!(runtime.state.active_tab().expect("tab").pane_count(), 1);
        assert_eq!(runtime.focused_pane(), Some(pane));
        assert_eq!(runtime.focused_session(), None);
        assert!(runtime.matches_projection(&rows[..2]));
    }

    #[test]
    fn binding_a_tab_placeholder_uses_the_first_supplied_node_id() {
        let rows = [
            row("ws", WorkspaceNodeKind::Workspace, None, None),
            row("tab", WorkspaceNodeKind::Tab, Some("ws"), None),
        ];
        let mut runtime = WorkspaceRuntime::from_projection(&rows);
        let ids = [
            WorkspaceNodeId::new("pane"),
            WorkspaceNodeId::new("unused-a"),
            WorkspaceNodeId::new("unused-b"),
        ];
        let mutation = runtime
            .plan_bind(PtySessionId::new("session"), &ids)
            .expect("bind plan")
            .expect("mutation");

        assert_eq!(mutation.commands().len(), 1);
        runtime.apply_mutation(mutation);
        assert_eq!(
            runtime.focused_session().map(PtySessionId::as_str),
            Some("session")
        );
    }

    #[test]
    fn binding_a_workspace_placeholder_creates_the_missing_tab_and_pane() {
        let rows = [row("ws", WorkspaceNodeKind::Workspace, None, None)];
        let mut runtime = WorkspaceRuntime::from_projection(&rows);
        let ids = [
            WorkspaceNodeId::new("tab"),
            WorkspaceNodeId::new("pane"),
            WorkspaceNodeId::new("unused"),
        ];
        let mutation = runtime
            .plan_bind(PtySessionId::new("session"), &ids)
            .expect("bind plan")
            .expect("mutation");

        assert_eq!(mutation.commands().len(), 2);
        runtime.apply_mutation(mutation);
        assert_eq!(
            runtime.focused_session().map(PtySessionId::as_str),
            Some("session")
        );
        assert_eq!(
            runtime
                .state
                .active_workspace()
                .expect("workspace")
                .tab_count(),
            1
        );
    }

    #[test]
    fn runtime_rejects_a_ninth_bound_pane_on_one_socket() {
        let mut rows = vec![
            row("ws", WorkspaceNodeKind::Workspace, None, None),
            row("tab", WorkspaceNodeKind::Tab, Some("ws"), None),
        ];
        for index in 0..=ACTIVE_PANE_LIMIT {
            rows.push(row(
                &format!("p{index}"),
                WorkspaceNodeKind::Pane,
                Some("tab"),
                Some(&format!("s{index}")),
            ));
        }
        let runtime = WorkspaceRuntime::from_projection(&rows);
        assert!(
            runtime
                .visible_bound_panes()
                .unwrap_err()
                .contains("admits 8")
        );
    }

    #[test]
    fn runtime_routes_interleaved_controls_by_request_not_by_session_broadcast() {
        let mut runtime = fixture();
        let panes = runtime.visible_bound_panes().expect("visible");
        for (index, (pane, session)) in panes.into_iter().enumerate() {
            runtime.ensure_attachment(pane, session);
            runtime
                .begin_attach(pane, RequestId::new(format!("r{index}")))
                .expect("begin");
        }
        let control = ServerControl::Response {
            request_id: RequestId::new("r1"),
            result: KernelResult::Error {
                code: KernelErrorCode::NotFound,
                message: "gone".to_owned(),
                detail: None,
            },
        };
        let effect = runtime.ingest(&control);
        assert!(effect.dirty);
        assert_eq!(
            effect.pane,
            Some(runtime.state.active_tab().unwrap().pane_ids()[1])
        );
        let first = runtime.state.active_tab().unwrap().pane_ids()[0];
        assert_eq!(
            runtime
                .attachment(first)
                .unwrap()
                .diagnostics()
                .foreign_references,
            0
        );
    }

    #[test]
    fn runtime_refuses_an_attach_dimension_past_the_mirror_budget() {
        let mut runtime = fixture();
        let (pane, session) = runtime.visible_bound_panes().unwrap().remove(0);
        runtime.ensure_attachment(pane, session.clone());
        runtime.begin_attach(pane, RequestId::new("large")).unwrap();
        let control = ServerControl::Response {
            request_id: RequestId::new("large"),
            result: KernelResult::PtyAttached {
                session_id: session,
                generation: gwk_domain::ids::PtySessionGeneration::new("life"),
                rows: 200,
                cols: 200,
                cursor: None,
            },
        };

        let effect = runtime.ingest(&control);
        assert_eq!(effect.pane, Some(pane));
        assert!(effect.dirty);
        assert!(runtime.attachment(pane).unwrap().generation().is_none());
    }

    #[test]
    fn xterm_profile_encodes_navigation_functions_modifiers_and_utf8() {
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        assert_eq!(
            encode_key(key(KeyCode::Up, KeyModifiers::NONE)).unwrap(),
            b"\x1b[A"
        );
        assert_eq!(
            encode_key(key(
                KeyCode::Left,
                KeyModifiers::SHIFT | KeyModifiers::CONTROL,
            ))
            .unwrap(),
            b"\x1b[1;6D"
        );
        assert_eq!(
            encode_key(key(KeyCode::F(1), KeyModifiers::NONE)).unwrap(),
            b"\x1bOP"
        );
        assert_eq!(
            encode_key(key(KeyCode::F(5), KeyModifiers::ALT)).unwrap(),
            b"\x1b[15;3~"
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('λ'), KeyModifiers::NONE)).unwrap(),
            "λ".as_bytes()
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)).unwrap(),
            &[3]
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('~'), KeyModifiers::CONTROL)).unwrap(),
            &[30]
        );
        assert_eq!(
            encode_key(key(KeyCode::Char(';'), KeyModifiers::CONTROL)).unwrap(),
            b";"
        );
        assert!(
            encode_key(key(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ))
            .is_err()
        );
        assert!(encode_key(key(KeyCode::F(13), KeyModifiers::NONE)).is_err());
    }
}
