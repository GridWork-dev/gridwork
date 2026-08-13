//! Reproducing the arrangement from the kernel's `workspace_node` projection.
//!
//! The kernel owns the durable arrangement — which workspaces, tabs and panes
//! exist, how they contain each other, and what session each pane is bound
//! to. This module rebuilds the client's [`WorkspaceState`] from those rows
//! and from nothing else: hand it the same rows on any machine and it builds
//! the same structure, which is exactly what lets a second client reproduce
//! the arrangement without any client holding authoritative layout.
//!
//! Everything the rows do not carry is transient geometry, and it gets fresh
//! defaults on every rebuild: split orientation and sizes, focus, the active
//! tab and workspace, and container names. Proportions resetting on a host
//! restart is the accepted, stated consequence of keeping geometry out of the
//! ledger — a client that persisted geometry anywhere to smooth that over
//! would be standing up a second layout authority, which is the defect the
//! split of ownership exists to prevent, not a fix.
//!
//! Derivation: none — original client-side mapping over this repository's
//! own projection rows and workspace model; it spawns no process, parses and
//! emits no terminal byte, and asserts no external behavior. The sibling
//! order, the default split orientation, and the placeholder rule for husk
//! containers are this client's own choices, argued below where each is made.

use std::collections::BTreeMap;

use gwk_domain::entity::{WorkspaceNode, WorkspaceNodeKind};
use gwk_domain::ids::{PtySessionId, WorkspaceNodeId};

use super::{Axis, Node, PaneId, Part, Split, Tab, WEIGHT_UNIT, Workspace, WorkspaceState};

/// One reproduced pane: the model leaf, the ledger node it stands for, and
/// the session bound to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneBinding {
    pub pane: PaneId,
    /// The ledger node this pane reproduces — `None` for a placeholder leaf
    /// minted only because the model refuses husk containers (see
    /// [`reproduce`]).
    pub node: Option<WorkspaceNodeId>,
    /// Projection version used by CAS-safe rebind, move and close commands.
    pub version: Option<u32>,
    /// Durable parent, retained so a pane close can reparent surviving
    /// children without deriving structure from screen geometry.
    pub parent: Option<WorkspaceNodeId>,
    /// The session the ledger binds to this pane, when it binds one.
    pub session: Option<PtySessionId>,
}

/// Durable identity for one reproduced tab, aligned with its model position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabBinding {
    pub node: WorkspaceNodeId,
    pub version: u32,
}

/// Durable identity for one reproduced workspace and its tabs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceBinding {
    pub node: WorkspaceNodeId,
    pub version: u32,
    pub tabs: Vec<TabBinding>,
}

/// What a rebuild produced: the model, every pane's ledger identity, and the
/// rows that could not be placed.
#[derive(Debug)]
pub struct Reproduced {
    pub state: WorkspaceState,
    pub bindings: Vec<PaneBinding>,
    pub workspaces: Vec<WorkspaceBinding>,
    /// Rows the walk from the roots never reached, or reached with an illegal
    /// kind for their position — orphans, cycles, and foreign shapes the
    /// kernel itself would have refused. Surfaced rather than dropped so a
    /// caller can say "the ledger holds rows this client did not reproduce".
    pub ignored: Vec<WorkspaceNodeId>,
}

/// Rebuild the workspace surface from projection rows alone.
///
/// Deterministic by construction: siblings order by `(created_at, id)` —
/// the projection carries no order column, so this ordering is the client's
/// own choice, and making it a total order over ledger facts is what makes
/// two independent rebuilds agree. All geometry is defaulted: every rebuilt
/// split lays its parts on one axis with equal shares, focus lands on each
/// tab's first pane, and the first workspace and tab are active.
///
/// Two mapping rules the rows force this client to choose:
///
/// - **A pane's pane children sit beside it in one split, parent first.**
///   The projection records a split as panes parented to a pane; the model's
///   splits are interior nodes and its panes are leaves. The parent row
///   stays a leaf (it can hold a session; a split cannot) and its children
///   join it as siblings — parent first because it is necessarily the older
///   row under the sibling order.
/// - **A husk container is reproduced around a placeholder pane.** The
///   ledger legally holds a tab with no panes yet (its pane arrives as a
///   separate command), but the model's own invariant is that every tab
///   holds a pane and every workspace a tab. A placeholder leaf — bound to
///   no ledger node and no session — keeps the container renderable through
///   the gap, and the next rebuild replaces it the moment the real pane's
///   row lands. Skipping the container instead would drop durable structure
///   a second client is entitled to see.
pub fn reproduce(rows: &[WorkspaceNode]) -> Reproduced {
    // Index by id; a duplicate id is a row the kernel cannot have produced,
    // so the copy that loses the slot is reported, not silently shadowed.
    let mut by_id: BTreeMap<&WorkspaceNodeId, &WorkspaceNode> = BTreeMap::new();
    let mut ignored: Vec<WorkspaceNodeId> = Vec::new();
    for row in rows {
        if by_id.insert(&row.id, row).is_some() {
            ignored.push(row.id.clone());
        }
    }

    // Children grouped under each parent, in the client's sibling order.
    let mut children: BTreeMap<&WorkspaceNodeId, Vec<&WorkspaceNode>> = BTreeMap::new();
    let mut roots: Vec<&WorkspaceNode> = Vec::new();
    for row in by_id.values() {
        match &row.parent_id {
            Some(parent) if by_id.contains_key(parent) => {
                children.entry(parent).or_default().push(row);
            }
            // An orphan points at a row that is not there; the walk can
            // never reach it, and the sweep below reports it.
            Some(_) => {}
            None => roots.push(row),
        }
    }
    let order = |a: &&WorkspaceNode, b: &&WorkspaceNode| {
        (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id))
    };
    roots.sort_by(order);
    for group in children.values_mut() {
        group.sort_by(order);
    }

    let mut builder = Builder {
        children: &children,
        placed: Vec::new(),
        bindings: Vec::new(),
        next_pane: 1,
    };

    let mut workspaces = Vec::new();
    let mut workspace_bindings = Vec::new();
    for root in &roots {
        if root.kind != WorkspaceNodeKind::Workspace {
            // A parentless tab or pane: the kernel refuses these at the
            // append; a foreign row does not get to invent a root here.
            continue;
        }
        builder.placed.push(root.id.clone());
        let (workspace, binding) = builder.workspace(root, workspaces.len() + 1);
        workspaces.push(workspace);
        workspace_bindings.push(binding);
    }

    let state = WorkspaceState {
        next_pane: builder.next_pane,
        next_workspace: u32::try_from(workspaces.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1),
        workspaces,
        active: 0,
    };

    // Everything indexed but never placed was unreachable from a legal root
    // or sat in an illegal position — one sweep reports them all, sorted so
    // the answer is stable regardless of input order.
    let placed: std::collections::BTreeSet<&WorkspaceNodeId> = builder.placed.iter().collect();
    for id in by_id.keys() {
        if !placed.contains(id) {
            ignored.push((*id).clone());
        }
    }
    ignored.sort();
    ignored.dedup();

    Reproduced {
        state,
        bindings: builder.bindings,
        workspaces: workspace_bindings,
        ignored,
    }
}

struct Builder<'a> {
    children: &'a BTreeMap<&'a WorkspaceNodeId, Vec<&'a WorkspaceNode>>,
    placed: Vec<WorkspaceNodeId>,
    bindings: Vec<PaneBinding>,
    next_pane: u64,
}

impl<'a> Builder<'a> {
    fn mint(&mut self, node: Option<&WorkspaceNode>) -> PaneId {
        let pane = PaneId(self.next_pane);
        self.next_pane += 1;
        self.bindings.push(PaneBinding {
            pane,
            node: node.map(|row| row.id.clone()),
            version: node.map(|row| row.version),
            parent: node.and_then(|row| row.parent_id.clone()),
            session: node.and_then(|row| row.session_id.clone()),
        });
        pane
    }

    fn kids(&self, of: &WorkspaceNode, kind: WorkspaceNodeKind) -> Vec<&'a WorkspaceNode> {
        self.children
            .get(&of.id)
            .map(|group| {
                group
                    .iter()
                    .copied()
                    .filter(|row| row.kind == kind)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn workspace(&mut self, row: &WorkspaceNode, position: usize) -> (Workspace, WorkspaceBinding) {
        let mut tabs = Vec::new();
        let mut tab_bindings = Vec::new();
        for tab in self.kids(row, WorkspaceNodeKind::Tab) {
            self.placed.push(tab.id.clone());
            let (model, binding) = self.tab(tab, tabs.len() + 1);
            tabs.push(model);
            tab_bindings.push(binding);
        }
        if tabs.is_empty() {
            // The husk rule: a workspace with no tab row yet still renders,
            // around a placeholder tab and pane.
            let pane = self.mint(None);
            tabs.push(Tab::new("1".to_owned(), pane));
        }
        (
            Workspace {
                name: position.to_string(),
                next_tab: u32::try_from(tabs.len())
                    .unwrap_or(u32::MAX)
                    .saturating_add(1),
                tabs,
                active: 0,
            },
            WorkspaceBinding {
                node: row.id.clone(),
                version: row.version,
                tabs: tab_bindings,
            },
        )
    }

    fn tab(&mut self, row: &WorkspaceNode, position: usize) -> (Tab, TabBinding) {
        let panes = self.kids(row, WorkspaceNodeKind::Pane);
        let root = match panes.as_slice() {
            [] => Node::Pane(self.mint(None)),
            [only] => {
                self.placed.push(only.id.clone());
                self.pane(only)
            }
            many => Node::Split(Split {
                axis: Axis::Columns,
                parts: many
                    .iter()
                    .map(|pane| {
                        self.placed.push(pane.id.clone());
                        Part {
                            weight: WEIGHT_UNIT,
                            node: self.pane(pane),
                        }
                    })
                    .collect(),
            }),
        };
        let focus = super::first_pane(&root);
        (
            Tab {
                title: position.to_string(),
                root,
                focus,
            },
            TabBinding {
                node: row.id.clone(),
                version: row.version,
            },
        )
    }

    fn pane(&mut self, row: &WorkspaceNode) -> Node {
        let leaf = Node::Pane(self.mint(Some(row)));
        let kids = self.kids(row, WorkspaceNodeKind::Pane);
        if kids.is_empty() {
            return leaf;
        }
        // The parent stays a leaf and its children join it as siblings —
        // parent first, being the older row under the sibling order.
        let mut parts = vec![Part {
            weight: WEIGHT_UNIT,
            node: leaf,
        }];
        for kid in kids {
            self.placed.push(kid.id.clone());
            parts.push(Part {
                weight: WEIGHT_UNIT,
                node: self.pane(kid),
            });
        }
        Node::Split(Split {
            axis: Axis::Columns,
            parts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gwk_domain::ids::Timestamp;
    use ratatui::layout::Rect;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 120,
        height: 24,
    };

    fn row(
        id: &str,
        kind: WorkspaceNodeKind,
        parent: Option<&str>,
        session: Option<&str>,
        at: &str,
    ) -> WorkspaceNode {
        WorkspaceNode {
            id: WorkspaceNodeId::new(id),
            version: 1,
            kind,
            parent_id: parent.map(WorkspaceNodeId::new),
            session_id: session.map(PtySessionId::new),
            created_at: Timestamp::new(at),
            updated_at: Timestamp::new(at),
        }
    }

    /// ws1 { tab1 { p1(s1) }, tab2 { p2, p3(s3) } }, ws2 { tab3 { p4 } }
    fn fixture() -> Vec<WorkspaceNode> {
        vec![
            row("ws1", WorkspaceNodeKind::Workspace, None, None, "t1"),
            row("tab1", WorkspaceNodeKind::Tab, Some("ws1"), None, "t2"),
            row(
                "p1",
                WorkspaceNodeKind::Pane,
                Some("tab1"),
                Some("s1"),
                "t3",
            ),
            row("tab2", WorkspaceNodeKind::Tab, Some("ws1"), None, "t4"),
            row("p2", WorkspaceNodeKind::Pane, Some("tab2"), None, "t5"),
            row(
                "p3",
                WorkspaceNodeKind::Pane,
                Some("tab2"),
                Some("s3"),
                "t6",
            ),
            row("ws2", WorkspaceNodeKind::Workspace, None, None, "t7"),
            row("tab3", WorkspaceNodeKind::Tab, Some("ws2"), None, "t8"),
            row("p4", WorkspaceNodeKind::Pane, Some("tab3"), None, "t9"),
        ]
    }

    fn binding<'a>(result: &'a Reproduced, node: &str) -> &'a PaneBinding {
        result
            .bindings
            .iter()
            .find(|b| b.node.as_ref().is_some_and(|n| n.as_str() == node))
            .unwrap_or_else(|| panic!("no binding reproduces {node}"))
    }

    #[test]
    fn arrange_rebuilds_the_containment_tree_from_rows_alone() {
        let result = reproduce(&fixture());
        assert!(result.ignored.is_empty(), "{:?}", result.ignored);
        assert_eq!(result.state.workspace_count(), 2);
        let first = result.state.active_workspace().expect("workspace");
        assert_eq!(first.tab_count(), 2);
        let tabs: Vec<_> = first.tabs().collect();
        assert_eq!(tabs[0].pane_count(), 1);
        assert_eq!(tabs[1].pane_count(), 2);

        assert_eq!(
            binding(&result, "p1").session.as_ref().map(|s| s.as_str()),
            Some("s1")
        );
        assert_eq!(binding(&result, "p2").session, None);
        assert_eq!(
            binding(&result, "p3").session.as_ref().map(|s| s.as_str()),
            Some("s3")
        );
        assert_eq!(result.bindings.len(), 4, "one binding per ledger pane");
    }

    #[test]
    fn arrange_is_deterministic_across_input_orders() {
        let mut shuffled = fixture();
        shuffled.reverse();
        shuffled.swap(0, 4);
        shuffled.swap(2, 7);

        let a = reproduce(&fixture());
        let b = reproduce(&shuffled);
        assert_eq!(a.bindings, b.bindings, "same panes, same mint order");
        assert_eq!(a.ignored, b.ignored);
        assert_eq!(a.state.workspace_count(), b.state.workspace_count());
        let tabs_a: Vec<_> = a.state.active_workspace().expect("ws").tabs().collect();
        let tabs_b: Vec<_> = b.state.active_workspace().expect("ws").tabs().collect();
        for (ta, tb) in tabs_a.iter().zip(&tabs_b) {
            assert_eq!(ta.pane_rects(AREA), tb.pane_rects(AREA));
            assert_eq!(ta.focus(), tb.focus());
        }
    }

    #[test]
    fn arrange_puts_a_split_pane_first_among_its_children() {
        let rows = vec![
            row("ws", WorkspaceNodeKind::Workspace, None, None, "t1"),
            row("tab", WorkspaceNodeKind::Tab, Some("ws"), None, "t2"),
            row("p1", WorkspaceNodeKind::Pane, Some("tab"), None, "t3"),
            row("p2", WorkspaceNodeKind::Pane, Some("p1"), None, "t4"),
            row("p3", WorkspaceNodeKind::Pane, Some("p1"), None, "t5"),
        ];
        let result = reproduce(&rows);
        assert!(result.ignored.is_empty());
        let tab = result.state.active_tab().expect("tab");
        assert_eq!(tab.pane_count(), 3);
        let rects = tab.pane_rects(AREA);
        let p1 = binding(&result, "p1").pane;
        assert_eq!(rects[0].0, p1, "the split pane stays the leftmost leaf");
        assert_eq!(rects[0].1.x, 0);
        assert_eq!(
            rects.iter().map(|(_, r)| r.width).collect::<Vec<_>>(),
            vec![40, 40, 40],
            "fresh equal shares — no geometry survives a rebuild"
        );
        assert_eq!(tab.focus(), p1, "focus defaults to the first pane");
    }

    #[test]
    fn arrange_reproduces_husk_containers_around_placeholders() {
        let rows = vec![
            row("ws", WorkspaceNodeKind::Workspace, None, None, "t1"),
            row("tab", WorkspaceNodeKind::Tab, Some("ws"), None, "t2"),
            row("empty-ws", WorkspaceNodeKind::Workspace, None, None, "t3"),
        ];
        let result = reproduce(&rows);
        assert!(result.ignored.is_empty());
        assert_eq!(result.state.workspace_count(), 2);
        for workspace in [0, 1] {
            let tab = result
                .state
                .workspaces
                .get(workspace)
                .and_then(Workspace::active_tab)
                .expect("tab");
            assert_eq!(tab.pane_count(), 1, "a placeholder keeps it renderable");
        }
        assert_eq!(result.bindings.len(), 2);
        assert!(
            result.bindings.iter().all(|b| b.node.is_none()),
            "placeholders reproduce no ledger node"
        );
        assert!(result.bindings.iter().all(|b| b.session.is_none()));
    }

    #[test]
    fn arrange_ignores_what_a_legal_ledger_cannot_hold() {
        let rows = vec![
            row("ws", WorkspaceNodeKind::Workspace, None, None, "t1"),
            row("tab", WorkspaceNodeKind::Tab, Some("ws"), None, "t2"),
            row("p1", WorkspaceNodeKind::Pane, Some("tab"), None, "t3"),
            // A pane directly under a workspace, a parentless tab, an orphan
            // whose parent is absent, and a two-row cycle: none can reach a
            // legal root, all are reported, and none disturb the rest.
            row("stray", WorkspaceNodeKind::Pane, Some("ws"), None, "t4"),
            row("rootless", WorkspaceNodeKind::Tab, None, None, "t5"),
            row("orphan", WorkspaceNodeKind::Pane, Some("gone"), None, "t6"),
            row("c1", WorkspaceNodeKind::Pane, Some("c2"), None, "t7"),
            row("c2", WorkspaceNodeKind::Pane, Some("c1"), None, "t8"),
        ];
        let result = reproduce(&rows);
        let ignored: Vec<&str> = result.ignored.iter().map(|id| id.as_str()).collect();
        assert_eq!(ignored, vec!["c1", "c2", "orphan", "rootless", "stray"]);
        assert_eq!(result.state.workspace_count(), 1);
        assert_eq!(result.state.active_tab().expect("tab").pane_count(), 1);
    }

    #[test]
    fn arrange_gives_geometry_made_in_a_client_no_way_to_survive() {
        let rows = fixture();
        let mut first = reproduce(&rows);
        // The operator resizes in this client. The ledger rows are untouched
        // — geometry is not a ledger fact — so a rebuild from the same rows
        // must come back to defaults. This is ruling S2 as a test: the reset
        // is the accepted consequence, and its absence would mean geometry
        // leaked into something durable.
        first.state.next_tab();
        first
            .state
            .resize(Axis::Columns, super::super::Resize::Grow);
        first
            .state
            .resize(Axis::Columns, super::super::Resize::Grow);
        let resized = first
            .state
            .active_tab()
            .expect("tab")
            .pane_rects(AREA)
            .iter()
            .map(|(_, r)| r.width)
            .collect::<Vec<_>>();
        assert_ne!(resized, vec![60, 60], "the resize really moved shares");

        let second = reproduce(&rows);
        let tabs: Vec<_> = second
            .state
            .active_workspace()
            .expect("ws")
            .tabs()
            .collect();
        assert_eq!(
            tabs[1]
                .pane_rects(AREA)
                .iter()
                .map(|(_, r)| r.width)
                .collect::<Vec<_>>(),
            vec![60, 60],
            "a rebuild starts from equal shares, always"
        );
    }

    #[test]
    fn arrange_of_no_rows_is_an_empty_surface() {
        let result = reproduce(&[]);
        assert!(result.state.is_empty());
        assert!(result.bindings.is_empty());
        assert!(result.ignored.is_empty());
    }
}
