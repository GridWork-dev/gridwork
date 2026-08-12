//! Workspace mode's structural floor: the mux object model.
//!
//! Derivation: none — original structural model: workspaces, tabs and split
//! panes as a pure weighted tree with create/navigate/resize/close operations
//! and a deterministic geometry solver. No registry row describes a mux
//! object model, so this file is original work; it spawns no process, parses
//! and emits no terminal byte, and asserts no colour/SGR or
//! keyboard-protocol fact.
//!
//! # The shape
//!
//! A [`WorkspaceState`] holds workspaces; a [`Workspace`] holds tabs; a
//! [`Tab`] holds one tree of split panes. A split lays its children along one
//! [`Axis`] with an integer weight per child — weights are **shares, not
//! cells**: the solver turns them into cell rectangles for whatever area the
//! frame has, so the same structure lays itself out on any terminal size.
//!
//! Three invariants hold everywhere: a split always has at least two parts
//! (a one-part split collapses into its child the moment closing empties it),
//! every live workspace has at least one tab and every tab at least one pane
//! (closing the last member closes the container instead of leaving a husk),
//! and pane identities mint monotonically and are never reused, so a stale
//! [`PaneId`] from a closed pane can never silently select a newer one.
//!
//! # What is deliberately absent
//!
//! **No bindings here.** The input vocabulary — which keys and command names
//! drive these operations, and the lineage every default owes — lives in
//! [`input`]; this file owns the structure, not the verbs. **No wire and no
//! content.** Nothing here talks to the kernel or hosts a session; panes are
//! structural leaves until the elements that bind sessions to them land.
//! **No persistence.** The durable arrangement authority is the kernel's
//! `workspace_node` projection; [`arrange`] rebuilds this model from those
//! rows, and everything the rows do not carry is transient geometry that
//! dies with the client.

pub mod arrange;
pub mod input;
pub mod render;
pub mod runtime;

use std::cmp::Reverse;
use std::fmt;

use ratatui::layout::Rect;

/// Every split starts its parts at this share. Divisible enough that halving
/// on a same-axis split stays integral for a few generations.
const WEIGHT_UNIT: u32 = 120;

/// One resize step transfers this many shares — a tenth of [`WEIGHT_UNIT`],
/// so ten steps roughly double an equal sibling pair.
const RESIZE_STEP: u32 = 12;

/// No resize may push a part below this share. A floor, not a guarantee of
/// visibility: on a small enough terminal the solver can still hand a pane
/// zero cells, and that is the terminal's honest answer.
const MIN_WEIGHT: u32 = 12;

/// One pane's stable identity: minted once per [`WorkspaceState`], never
/// reused within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneId(u64);

impl fmt::Display for PaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "p{}", self.0)
    }
}

/// Which way a split lays its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Children sit side by side, each taking a band of columns.
    Columns,
    /// Children stack, each taking a band of rows.
    Rows,
}

/// A directional focus move, in screen terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Grow or shrink the focused pane along an axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resize {
    Grow,
    Shrink,
}

#[derive(Debug, Clone)]
enum Node {
    Pane(PaneId),
    Split(Split),
}

#[derive(Debug, Clone)]
struct Split {
    axis: Axis,
    parts: Vec<Part>,
}

#[derive(Debug, Clone)]
struct Part {
    weight: u32,
    node: Node,
}

/// One tab: a tree of split panes and the pane taking input.
#[derive(Debug, Clone)]
pub struct Tab {
    title: String,
    root: Node,
    focus: PaneId,
}

impl Tab {
    fn new(title: String, pane: PaneId) -> Self {
        Tab {
            title,
            root: Node::Pane(pane),
            focus: pane,
        }
    }

    /// The label the tab strip shows.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The pane taking input.
    pub fn focus(&self) -> PaneId {
        self.focus
    }

    /// How many panes the tab holds.
    pub fn pane_count(&self) -> usize {
        fn count(node: &Node) -> usize {
            match node {
                Node::Pane(_) => 1,
                Node::Split(split) => split.parts.iter().map(|part| count(&part.node)).sum(),
            }
        }
        count(&self.root)
    }

    /// Every pane identity in paint order.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        fn collect(node: &Node, out: &mut Vec<PaneId>) {
            match node {
                Node::Pane(id) => out.push(*id),
                Node::Split(split) => {
                    for part in &split.parts {
                        collect(&part.node, out);
                    }
                }
            }
        }
        let mut panes = Vec::new();
        collect(&self.root, &mut panes);
        panes
    }

    /// Every pane's rectangle inside `area`, in paint order.
    ///
    /// Total and deterministic: every pane gets exactly one rectangle, the
    /// rectangles tile the area exactly, and a pane the area cannot fit gets
    /// a zero-sized one rather than being dropped — absence and invisibility
    /// are different facts. Boundaries are rounded cumulative shares, so a
    /// one-cell remainder lands where the rounding says, not always on the
    /// last pane.
    pub fn pane_rects(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        fn solve(node: &Node, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
            match node {
                Node::Pane(id) => out.push((*id, area)),
                Node::Split(split) => {
                    let total: u64 = split.parts.iter().map(|part| u64::from(part.weight)).sum();
                    let extent = u64::from(match split.axis {
                        Axis::Columns => area.width,
                        Axis::Rows => area.height,
                    });
                    let mut cumulative: u64 = 0;
                    let mut previous: u16 = 0;
                    for part in &split.parts {
                        cumulative += u64::from(part.weight);
                        // Rounding a monotone sequence keeps it monotone, so
                        // the subtraction below cannot underflow and the last
                        // boundary is exactly the extent.
                        let boundary = ((extent * cumulative + total / 2) / total) as u16;
                        let size = boundary - previous;
                        let rect = match split.axis {
                            Axis::Columns => {
                                Rect::new(area.x + previous, area.y, size, area.height)
                            }
                            Axis::Rows => Rect::new(area.x, area.y + previous, area.width, size),
                        };
                        solve(&part.node, rect, out);
                        previous = boundary;
                    }
                }
            }
        }
        let mut out = Vec::new();
        solve(&self.root, area, &mut out);
        out
    }
}

/// One workspace: a row of tabs, one active.
#[derive(Debug, Clone)]
pub struct Workspace {
    name: String,
    tabs: Vec<Tab>,
    active: usize,
    next_tab: u32,
}

impl Workspace {
    /// The label the header shows.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Every tab, in strip order.
    pub fn tabs(&self) -> impl Iterator<Item = &Tab> {
        self.tabs.iter()
    }

    /// How many tabs the workspace holds.
    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// The active tab's position in the strip.
    pub fn active_index(&self) -> usize {
        self.active
    }

    /// The tab taking input.
    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }
}

/// The whole workspace surface: every workspace, one active, and the mint
/// for pane identities.
#[derive(Debug, Clone)]
pub struct WorkspaceState {
    workspaces: Vec<Workspace>,
    active: usize,
    next_pane: u64,
    next_workspace: u32,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceState {
    /// The opening state: one workspace, one tab, one pane, focused.
    pub fn new() -> Self {
        let mut state = WorkspaceState {
            workspaces: Vec::new(),
            active: 0,
            next_pane: 1,
            next_workspace: 1,
        };
        state.create_workspace();
        state
    }

    /// True once every workspace has been closed. The caller decides what an
    /// empty surface means — this model does not resurrect anything.
    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
    }

    /// How many workspaces exist.
    pub fn workspace_count(&self) -> usize {
        self.workspaces.len()
    }

    /// The active workspace's position.
    pub fn active_index(&self) -> usize {
        self.active
    }

    /// The workspace taking input.
    pub fn active_workspace(&self) -> Option<&Workspace> {
        self.workspaces.get(self.active)
    }

    fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        let workspace = self.workspaces.get_mut(self.active)?;
        workspace.tabs.get_mut(workspace.active)
    }

    /// The tab taking input.
    pub fn active_tab(&self) -> Option<&Tab> {
        self.active_workspace().and_then(Workspace::active_tab)
    }

    fn mint_pane(&mut self) -> PaneId {
        let id = PaneId(self.next_pane);
        self.next_pane += 1;
        id
    }

    /// Create a workspace with one tab and one pane, and switch to it.
    pub fn create_workspace(&mut self) -> PaneId {
        let pane = self.mint_pane();
        let name = self.next_workspace.to_string();
        self.next_workspace += 1;
        self.workspaces.push(Workspace {
            name,
            tabs: vec![Tab::new("1".to_owned(), pane)],
            active: 0,
            next_tab: 2,
        });
        self.active = self.workspaces.len() - 1;
        pane
    }

    /// Create a tab in the active workspace, and switch to it.
    pub fn create_tab(&mut self) -> Option<PaneId> {
        let pane = self.mint_pane();
        let workspace = self.workspaces.get_mut(self.active)?;
        let title = workspace.next_tab.to_string();
        workspace.next_tab += 1;
        workspace.tabs.push(Tab::new(title, pane));
        workspace.active = workspace.tabs.len() - 1;
        Some(pane)
    }

    /// Split the focused pane along `axis`; the new pane takes focus.
    ///
    /// Splitting divides the focused pane's own share between it and the
    /// newcomer — the other siblings keep their sizes. Under a parent already
    /// split on the same axis the new pane joins as a sibling; under the
    /// other axis it nests a fresh two-part split in place.
    pub fn split(&mut self, axis: Axis) -> Option<PaneId> {
        self.active_tab_mut()?;
        let pane = self.mint_pane();
        let tab = self.active_tab_mut()?;
        if split_at(&mut tab.root, tab.focus, axis, pane) {
            tab.focus = pane;
            Some(pane)
        } else {
            None
        }
    }

    /// Close the focused pane. The last pane closes its tab; the last tab
    /// closes its workspace; the last workspace leaves the surface empty.
    pub fn close_pane(&mut self) {
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        if matches!(tab.root, Node::Pane(_)) {
            self.close_tab();
            return;
        }
        if let Some(next) = remove_at(&mut tab.root, tab.focus) {
            tab.focus = next;
        }
    }

    /// Close the active tab and every pane in it.
    pub fn close_tab(&mut self) {
        let Some(workspace) = self.workspaces.get_mut(self.active) else {
            return;
        };
        if workspace.tabs.len() <= 1 {
            self.close_workspace();
            return;
        }
        workspace.tabs.remove(workspace.active);
        if workspace.active >= workspace.tabs.len() {
            workspace.active = workspace.tabs.len() - 1;
        }
    }

    /// Close the active workspace and everything in it.
    pub fn close_workspace(&mut self) {
        if self.workspaces.is_empty() {
            return;
        }
        self.workspaces.remove(self.active);
        if self.active >= self.workspaces.len() && !self.workspaces.is_empty() {
            self.active = self.workspaces.len() - 1;
        }
    }

    /// Switch to the next workspace, wrapping.
    pub fn next_workspace(&mut self) {
        if !self.workspaces.is_empty() {
            self.active = (self.active + 1) % self.workspaces.len();
        }
    }

    /// Switch to the previous workspace, wrapping.
    pub fn previous_workspace(&mut self) {
        if !self.workspaces.is_empty() {
            self.active = (self.active + self.workspaces.len() - 1) % self.workspaces.len();
        }
    }

    /// Select one workspace by position. A stale position is a no-op.
    pub fn select_workspace(&mut self, index: usize) {
        if index < self.workspaces.len() {
            self.active = index;
        }
    }

    /// Switch to the next tab in the active workspace, wrapping.
    pub fn next_tab(&mut self) {
        if let Some(workspace) = self.workspaces.get_mut(self.active)
            && !workspace.tabs.is_empty()
        {
            workspace.active = (workspace.active + 1) % workspace.tabs.len();
        }
    }

    /// Switch to the previous tab in the active workspace, wrapping.
    pub fn previous_tab(&mut self) {
        if let Some(workspace) = self.workspaces.get_mut(self.active)
            && !workspace.tabs.is_empty()
        {
            workspace.active = (workspace.active + workspace.tabs.len() - 1) % workspace.tabs.len();
        }
    }

    /// Switch to tab `index` in the active workspace, if it exists.
    pub fn select_tab(&mut self, index: usize) {
        if let Some(workspace) = self.workspaces.get_mut(self.active)
            && index < workspace.tabs.len()
        {
            workspace.active = index;
        }
    }

    /// Focus pane `id` in the active tab — the click path. False when the
    /// active tab holds no such pane; a stale target must not move focus.
    pub fn focus_pane(&mut self, id: PaneId) -> bool {
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        if contains_pane(&tab.root, id) {
            tab.focus = id;
            true
        } else {
            false
        }
    }

    /// Move focus to the nearest visible pane in `dir`, judged on the
    /// geometry the panes actually have inside `area`.
    ///
    /// Nearest means: smallest gap past the focused pane's `dir` edge, then
    /// the most lateral overlap with the focused pane, then topmost-leftmost
    /// — a total order, so the move is deterministic. Panes the area gave
    /// zero cells are not candidates: focus must not vanish into something
    /// the operator cannot see.
    pub fn move_focus(&mut self, dir: Direction, area: Rect) {
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        let rects = tab.pane_rects(area);
        let Some(&(_, from)) = rects.iter().find(|(id, _)| *id == tab.focus) else {
            return;
        };
        let next = rects
            .iter()
            .filter(|(id, rect)| {
                *id != tab.focus && rect.width > 0 && rect.height > 0 && beyond(from, *rect, dir)
            })
            .map(|&(id, rect)| {
                (
                    (
                        edge_gap(from, rect, dir),
                        Reverse(lateral_overlap(from, rect, dir)),
                        rect.y,
                        rect.x,
                    ),
                    id,
                )
            })
            .min_by_key(|&(key, _)| key)
            .map(|(_, id)| id);
        if let Some(id) = next {
            tab.focus = id;
        }
    }

    /// Resize the focused pane along `axis`: grow takes one step of share
    /// from its nearest sibling on that axis, shrink gives one back.
    ///
    /// The transfer happens at the nearest enclosing split laid on `axis` —
    /// the one whose boundary the operator sees move. A pane with no such
    /// split anywhere above it has nothing to resize against, and the
    /// operation is honestly a no-op rather than a guess.
    pub fn resize(&mut self, axis: Axis, action: Resize) {
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        resize_at(&mut tab.root, tab.focus, axis, action);
    }
}

fn contains_pane(node: &Node, target: PaneId) -> bool {
    match node {
        Node::Pane(id) => *id == target,
        Node::Split(split) => split
            .parts
            .iter()
            .any(|part| contains_pane(&part.node, target)),
    }
}

fn first_pane(node: &Node) -> PaneId {
    match node {
        Node::Pane(id) => *id,
        Node::Split(split) => first_pane(&split.parts[0].node),
    }
}

fn split_at(node: &mut Node, target: PaneId, axis: Axis, new: PaneId) -> bool {
    match node {
        Node::Pane(id) if *id == target => {
            *node = Node::Split(Split {
                axis,
                parts: vec![
                    Part {
                        weight: WEIGHT_UNIT,
                        node: Node::Pane(target),
                    },
                    Part {
                        weight: WEIGHT_UNIT,
                        node: Node::Pane(new),
                    },
                ],
            });
            true
        }
        Node::Pane(_) => false,
        Node::Split(split) => {
            if split.axis == axis
                && let Some(index) = split
                    .parts
                    .iter()
                    .position(|part| matches!(&part.node, Node::Pane(id) if *id == target))
            {
                // Halve the split pane's own share; the floor of one keeps a
                // degenerate weight from minting a permanently zero part.
                let weight = split.parts[index].weight;
                let half = (weight / 2).max(1);
                split.parts[index].weight = weight.saturating_sub(half).max(1);
                split.parts.insert(
                    index + 1,
                    Part {
                        weight: half,
                        node: Node::Pane(new),
                    },
                );
                return true;
            }
            split
                .parts
                .iter_mut()
                .any(|part| split_at(&mut part.node, target, axis, new))
        }
    }
}

/// Remove `target` from the subtree. `Some(next)` names the pane that should
/// take focus — the first pane of the part that absorbed the freed space.
fn remove_at(node: &mut Node, target: PaneId) -> Option<PaneId> {
    let Node::Split(split) = node else {
        return None;
    };
    if let Some(index) = split
        .parts
        .iter()
        .position(|part| matches!(&part.node, Node::Pane(id) if *id == target))
    {
        split.parts.remove(index);
        let neighbour = index.min(split.parts.len() - 1);
        let next = first_pane(&split.parts[neighbour].node);
        if split.parts.len() == 1 {
            // A one-part split is a husk: collapse it into its child so the
            // at-least-two invariant holds everywhere a reader looks.
            let only = split.parts.remove(0);
            *node = only.node;
        }
        return Some(next);
    }
    for part in &mut split.parts {
        if let Some(next) = remove_at(&mut part.node, target) {
            return Some(next);
        }
    }
    None
}

fn resize_at(node: &mut Node, target: PaneId, axis: Axis, action: Resize) -> bool {
    let Node::Split(split) = node else {
        return false;
    };
    let Some(index) = split
        .parts
        .iter()
        .position(|part| contains_pane(&part.node, target))
    else {
        return false;
    };
    // Deeper first: the nearest enclosing split on this axis wins, because
    // its boundary is the one nearest the pane the operator is resizing.
    if resize_at(&mut split.parts[index].node, target, axis, action) {
        return true;
    }
    if split.axis != axis || split.parts.len() < 2 {
        return false;
    }
    let neighbour = if index + 1 < split.parts.len() {
        index + 1
    } else {
        index - 1
    };
    let (donor, taker) = match action {
        Resize::Grow => (neighbour, index),
        Resize::Shrink => (index, neighbour),
    };
    let step = RESIZE_STEP.min(split.parts[donor].weight.saturating_sub(MIN_WEIGHT));
    if step > 0 {
        split.parts[donor].weight -= step;
        split.parts[taker].weight += step;
    }
    // Found the governing split: the operation is spent here even when the
    // floor left nothing to transfer, never retried on a farther ancestor.
    true
}

fn beyond(from: Rect, to: Rect, dir: Direction) -> bool {
    match dir {
        Direction::Left => to.x.saturating_add(to.width) <= from.x,
        Direction::Right => to.x >= from.x.saturating_add(from.width),
        Direction::Up => to.y.saturating_add(to.height) <= from.y,
        Direction::Down => to.y >= from.y.saturating_add(from.height),
    }
}

/// Cells between the focused pane's `dir` edge and the candidate's facing
/// edge. Callers only ask about candidates `beyond` that edge, which is what
/// keeps the subtractions in range.
fn edge_gap(from: Rect, to: Rect, dir: Direction) -> u16 {
    match dir {
        Direction::Left => from.x - (to.x + to.width),
        Direction::Right => to.x - (from.x + from.width),
        Direction::Up => from.y - (to.y + to.height),
        Direction::Down => to.y - (from.y + from.height),
    }
}

/// How many cells the candidate shares with the focused pane on the axis
/// perpendicular to the move — the tie-breaker that keeps a straight-across
/// neighbour ahead of a diagonal one.
fn lateral_overlap(from: Rect, to: Rect, dir: Direction) -> u16 {
    let (from_start, from_end, to_start, to_end) = match dir {
        Direction::Left | Direction::Right => {
            (from.y, from.y + from.height, to.y, to.y + to.height)
        }
        Direction::Up | Direction::Down => (from.x, from.x + from.width, to.x, to.x + to.width),
    };
    from_end
        .min(to_end)
        .saturating_sub(from_start.max(to_start))
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 24,
    };

    fn rect_of(state: &WorkspaceState, id: PaneId) -> Rect {
        let tab = state.active_tab().expect("active tab");
        tab.pane_rects(AREA)
            .into_iter()
            .find(|(pane, _)| *pane == id)
            .map(|(_, rect)| rect)
            .unwrap_or_else(|| panic!("{id} has no rect"))
    }

    #[test]
    fn workspace_the_opening_state_is_one_of_each() {
        let state = WorkspaceState::new();
        assert!(!state.is_empty());
        assert_eq!(state.workspace_count(), 1);
        let workspace = state.active_workspace().expect("workspace");
        assert_eq!(workspace.name(), "1");
        assert_eq!(workspace.tab_count(), 1);
        let tab = workspace.active_tab().expect("tab");
        assert_eq!(tab.title(), "1");
        assert_eq!(tab.pane_count(), 1);
        let rects = tab.pane_rects(AREA);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].1, AREA, "one pane fills the whole area");
        assert_eq!(tab.focus(), rects[0].0);
    }

    #[test]
    fn workspace_create_switches_to_the_new_workspace_and_tab() {
        let mut state = WorkspaceState::new();
        state.create_workspace();
        assert_eq!(state.workspace_count(), 2);
        assert_eq!(state.active_index(), 1);
        assert_eq!(state.active_workspace().expect("workspace").name(), "2");

        state.create_tab();
        let workspace = state.active_workspace().expect("workspace");
        assert_eq!(workspace.tab_count(), 2);
        assert_eq!(workspace.active_index(), 1);
        assert_eq!(workspace.active_tab().expect("tab").title(), "2");
    }

    #[test]
    fn workspace_split_gives_the_new_pane_focus_and_half_the_share() {
        let mut state = WorkspaceState::new();
        let first = state.active_tab().expect("tab").focus();
        state.split(Axis::Columns);
        let tab = state.active_tab().expect("tab");
        assert_eq!(tab.pane_count(), 2);
        let second = tab.focus();
        assert_ne!(second, first, "the new pane takes focus");

        let left = rect_of(&state, first);
        let right = rect_of(&state, second);
        assert_eq!(left.width, 40);
        assert_eq!(right.width, 40);
        assert_eq!(right.x, 40, "the newcomer sits after the pane it split");
        assert_eq!(left.height, AREA.height, "columns split shares every row");
    }

    #[test]
    fn workspace_same_axis_split_joins_as_a_sibling_and_divides_the_split_pane_only() {
        let mut state = WorkspaceState::new();
        let first = state.active_tab().expect("tab").focus();
        state.split(Axis::Columns);
        let second = state.active_tab().expect("tab").focus();
        state.split(Axis::Columns);
        let third = state.active_tab().expect("tab").focus();

        // The first pane kept its half; the second's half divided in two.
        assert_eq!(rect_of(&state, first).width, 40);
        assert_eq!(rect_of(&state, second).width, 20);
        assert_eq!(rect_of(&state, third).width, 20);
        // All three share the full height: the split flattened rather than
        // nesting a second columns split inside the second pane.
        for id in [first, second, third] {
            assert_eq!(rect_of(&state, id).height, AREA.height);
        }
    }

    #[test]
    fn workspace_cross_axis_split_nests_inside_the_focused_pane() {
        let mut state = WorkspaceState::new();
        let first = state.active_tab().expect("tab").focus();
        state.split(Axis::Columns);
        let second = state.active_tab().expect("tab").focus();
        state.split(Axis::Rows);
        let third = state.active_tab().expect("tab").focus();

        let left = rect_of(&state, first);
        let top_right = rect_of(&state, second);
        let bottom_right = rect_of(&state, third);
        assert_eq!(left.height, AREA.height, "the other column is untouched");
        assert_eq!(
            top_right.x, bottom_right.x,
            "the rows split stays in its column"
        );
        assert_eq!(top_right.height, 12);
        assert_eq!(bottom_right.height, 12);
        assert_eq!(bottom_right.y, 12);
    }

    #[test]
    fn workspace_pane_rects_tile_the_area_exactly() {
        let mut state = WorkspaceState::new();
        state.split(Axis::Columns);
        state.split(Axis::Rows);
        state.split(Axis::Columns);
        let tab = state.active_tab().expect("tab");
        // An awkward area: odd width and height so shares cannot divide
        // evenly, which is exactly when tiling has to be proven.
        let area = Rect::new(3, 2, 77, 23);
        let rects = tab.pane_rects(area);
        assert_eq!(rects.len(), tab.pane_count());
        let cells: u32 = rects
            .iter()
            .map(|(_, rect)| u32::from(rect.width) * u32::from(rect.height))
            .sum();
        assert_eq!(
            cells,
            u32::from(area.width) * u32::from(area.height),
            "the panes cover every cell exactly once"
        );
        for (id, rect) in &rects {
            assert!(
                rect.x >= area.x
                    && rect.y >= area.y
                    && rect.x + rect.width <= area.x + area.width
                    && rect.y + rect.height <= area.y + area.height,
                "{id} at {rect:?} escapes {area:?}"
            );
        }
    }

    #[test]
    fn workspace_close_pane_returns_focus_to_the_neighbour_that_absorbed_the_space() {
        let mut state = WorkspaceState::new();
        state.split(Axis::Columns);
        let second = state.active_tab().expect("tab").focus();
        state.split(Axis::Columns);
        state.close_pane();
        let tab = state.active_tab().expect("tab");
        assert_eq!(tab.pane_count(), 2);
        assert_eq!(
            tab.focus(),
            second,
            "closing the last sibling focuses the one before it — the pane \
             whose band absorbed the freed share"
        );
        assert_eq!(state.workspace_count(), 1, "the tab and workspace survive");
    }

    #[test]
    fn workspace_closing_a_nested_pane_collapses_the_husk_split() {
        let mut state = WorkspaceState::new();
        let first = state.active_tab().expect("tab").focus();
        state.split(Axis::Columns);
        let second = state.active_tab().expect("tab").focus();
        state.split(Axis::Rows);
        state.close_pane();
        let tab = state.active_tab().expect("tab");
        assert_eq!(tab.pane_count(), 2);
        assert_eq!(tab.focus(), second, "focus lands on the surviving sibling");
        // The rows split collapsed: the survivor owns its column's full height.
        assert_eq!(rect_of(&state, second).height, AREA.height);
        assert_eq!(rect_of(&state, first).height, AREA.height);
    }

    #[test]
    fn workspace_the_close_cascade_ends_empty_and_stays_empty() {
        let mut state = WorkspaceState::new();
        state.create_tab();
        state.close_pane();
        assert_eq!(
            state.active_workspace().expect("workspace").tab_count(),
            1,
            "the last pane of a tab closes the tab"
        );
        state.close_pane();
        assert!(
            state.is_empty(),
            "the last pane of the last tab of the last workspace empties the surface"
        );
        // Every operation on an empty surface is a no-op, not a panic.
        state.close_pane();
        state.close_tab();
        state.close_workspace();
        state.split(Axis::Rows);
        state.create_tab();
        state.next_workspace();
        state.previous_tab();
        state.resize(Axis::Columns, Resize::Grow);
        state.move_focus(Direction::Left, AREA);
        assert!(state.is_empty());
        // Except the one that creates the container everything else needs.
        state.create_workspace();
        assert!(!state.is_empty());
        assert_eq!(
            state.active_workspace().expect("workspace").name(),
            "2",
            "workspace names mint forward; a closed name is never reissued"
        );
    }

    #[test]
    fn workspace_close_tab_and_close_workspace_keep_a_valid_active_index() {
        let mut state = WorkspaceState::new();
        state.create_tab();
        state.create_tab();
        assert_eq!(
            state.active_workspace().expect("workspace").active_index(),
            2
        );
        state.close_tab();
        let workspace = state.active_workspace().expect("workspace");
        assert_eq!(workspace.tab_count(), 2);
        assert_eq!(
            workspace.active_index(),
            1,
            "closing the last tab activates the one now at the end"
        );

        state.create_workspace();
        state.create_workspace();
        state.close_workspace();
        assert_eq!(state.workspace_count(), 2);
        assert_eq!(state.active_index(), 1);
    }

    #[test]
    fn workspace_navigation_wraps_in_both_directions() {
        let mut state = WorkspaceState::new();
        state.create_workspace();
        state.create_workspace();
        assert_eq!(state.active_index(), 2);
        state.next_workspace();
        assert_eq!(state.active_index(), 0, "next wraps forward");
        state.previous_workspace();
        assert_eq!(state.active_index(), 2, "previous wraps back");

        state.create_tab();
        state.next_tab();
        assert_eq!(
            state.active_workspace().expect("workspace").active_index(),
            0,
            "tab next wraps"
        );
        state.previous_tab();
        assert_eq!(
            state.active_workspace().expect("workspace").active_index(),
            1,
            "tab previous wraps"
        );
        state.select_tab(0);
        assert_eq!(
            state.active_workspace().expect("workspace").active_index(),
            0
        );
        state.select_tab(9);
        assert_eq!(
            state.active_workspace().expect("workspace").active_index(),
            0,
            "selecting a tab that does not exist moves nothing"
        );
    }

    #[test]
    fn workspace_focus_pane_takes_a_live_target_and_refuses_a_stale_one() {
        let mut state = WorkspaceState::new();
        let first = state.active_tab().expect("tab").focus();
        state.split(Axis::Columns);
        let second = state.active_tab().expect("tab").focus();
        assert!(state.focus_pane(first));
        assert_eq!(state.active_tab().expect("tab").focus(), first);

        state.focus_pane(second);
        state.close_pane();
        assert!(
            !state.focus_pane(second),
            "a closed pane's id must not move focus"
        );
        assert_eq!(state.active_tab().expect("tab").focus(), first);
    }

    #[test]
    fn workspace_directional_focus_prefers_the_straight_across_neighbour() {
        // A 2x2 grid: p1 top-left, p4 bottom-left, p2 top-right, p3
        // bottom-right (p4 splits p1's row band after the columns split).
        let mut state = WorkspaceState::new();
        let p1 = state.active_tab().expect("tab").focus();
        state.split(Axis::Columns);
        let p2 = state.active_tab().expect("tab").focus();
        state.split(Axis::Rows);
        let p3 = state.active_tab().expect("tab").focus();
        state.focus_pane(p1);
        state.split(Axis::Rows);
        let p4 = state.active_tab().expect("tab").focus();

        state.move_focus(Direction::Right, AREA);
        assert_eq!(
            state.active_tab().expect("tab").focus(),
            p3,
            "bottom-left moves straight across to bottom-right, not diagonally"
        );
        state.move_focus(Direction::Up, AREA);
        assert_eq!(state.active_tab().expect("tab").focus(), p2);
        state.move_focus(Direction::Left, AREA);
        assert_eq!(state.active_tab().expect("tab").focus(), p1);
        state.move_focus(Direction::Down, AREA);
        assert_eq!(state.active_tab().expect("tab").focus(), p4);
    }

    #[test]
    fn workspace_focus_at_an_edge_stays_put() {
        let mut state = WorkspaceState::new();
        let first = state.active_tab().expect("tab").focus();
        state.split(Axis::Columns);
        state.focus_pane(first);
        state.move_focus(Direction::Left, AREA);
        assert_eq!(
            state.active_tab().expect("tab").focus(),
            first,
            "no pane lies leftward, so focus does not move"
        );
        state.move_focus(Direction::Up, AREA);
        assert_eq!(state.active_tab().expect("tab").focus(), first);
    }

    #[test]
    fn workspace_resize_moves_the_shared_boundary_and_respects_the_floor() {
        let mut state = WorkspaceState::new();
        let first = state.active_tab().expect("tab").focus();
        state.split(Axis::Columns);
        let second = state.active_tab().expect("tab").focus();
        let before = rect_of(&state, second).width;

        state.resize(Axis::Columns, Resize::Grow);
        let grown = rect_of(&state, second).width;
        assert!(
            grown > before,
            "grow widens the focused pane ({before} -> {grown})"
        );
        assert_eq!(
            rect_of(&state, first).width + grown,
            AREA.width,
            "the two panes still tile the row"
        );

        state.resize(Axis::Columns, Resize::Shrink);
        assert_eq!(
            rect_of(&state, second).width,
            before,
            "shrink undoes exactly one grow step"
        );

        // Grow until the floor: the sibling must keep its minimum share.
        for _ in 0..64 {
            state.resize(Axis::Columns, Resize::Grow);
        }
        let floored = rect_of(&state, first).width;
        assert!(
            floored > 0,
            "the floor keeps the donor visible at this size"
        );
        state.resize(Axis::Columns, Resize::Grow);
        assert_eq!(
            rect_of(&state, first).width,
            floored,
            "at the floor a further grow moves nothing"
        );
    }

    #[test]
    fn workspace_resize_on_an_axis_with_no_split_is_a_no_op() {
        let mut state = WorkspaceState::new();
        state.split(Axis::Columns);
        let second = state.active_tab().expect("tab").focus();
        let before = rect_of(&state, second);
        state.resize(Axis::Rows, Resize::Grow);
        assert_eq!(
            rect_of(&state, second),
            before,
            "no rows split exists anywhere above the pane"
        );
    }

    #[test]
    fn workspace_resize_acts_at_the_nearest_enclosing_split_on_the_axis() {
        // Columns split, then a rows split nested right, then a columns split
        // nested inside the bottom-right pane: resizing columns must move the
        // innermost columns boundary, not the outermost.
        let mut state = WorkspaceState::new();
        let p1 = state.active_tab().expect("tab").focus();
        state.split(Axis::Columns);
        state.split(Axis::Rows);
        state.split(Axis::Columns);
        let p4 = state.active_tab().expect("tab").focus();
        let outer_before = rect_of(&state, p1).width;
        let inner_before = rect_of(&state, p4).width;

        state.resize(Axis::Columns, Resize::Grow);
        assert_eq!(
            rect_of(&state, p1).width,
            outer_before,
            "the outer columns boundary does not move"
        );
        assert!(
            rect_of(&state, p4).width > inner_before,
            "the innermost columns split absorbs the step"
        );
    }

    #[test]
    fn workspace_pane_ids_are_never_reused() {
        let mut state = WorkspaceState::new();
        let mut seen = std::collections::BTreeSet::new();
        seen.insert(state.active_tab().expect("tab").focus());
        for _ in 0..8 {
            state.split(Axis::Columns);
            assert!(
                seen.insert(state.active_tab().expect("tab").focus()),
                "a fresh pane must carry a fresh id"
            );
            state.close_pane();
        }
    }

    #[test]
    fn workspace_a_zero_sized_area_still_answers_totally() {
        let mut state = WorkspaceState::new();
        state.split(Axis::Columns);
        let tab = state.active_tab().expect("tab");
        let rects = tab.pane_rects(Rect::new(0, 0, 0, 0));
        assert_eq!(rects.len(), 2, "every pane still gets a rectangle");
        assert!(rects.iter().all(|(_, rect)| rect.width == 0));
        // And focus refuses to move into what cannot be seen.
        state.move_focus(Direction::Right, Rect::new(0, 0, 0, 0));
    }
}
