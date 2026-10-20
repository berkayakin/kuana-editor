//! How the editor area is divided: a tree of splits whose leaves are panes.
//! A split lays its members out side by side (`Axis::Row`) or stacked
//! (`Axis::Column`), each with a share of the room, and any member can be
//! split again either way, so panes make grids.
//!
//! The tree keeps itself tidy: a split has at least two members, and none of
//! them is a split along the same axis (that one's members join it). It's
//! generic over the pane handle, so its rules are tested with numbers; the
//! workspace keeps a `PaneTree<Entity<Pane>>`.

use std::{cell::Cell, rc::Rc};

use gpui::{Bounds, Pixels, point, px};

use super::{GAP, MIN_PANE_HEIGHT, MIN_PANE_WIDTH};
use crate::state::{PaneNode, PaneShare};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Axis {
    /// Side by side.
    Row,
    /// Stacked.
    Column,
}

/// Where a new pane goes beside the one it's split from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Side {
    Left,
    Right,
    Up,
    Down,
}

impl Side {
    pub(super) fn axis(self) -> Axis {
        match self {
            Side::Left | Side::Right => Axis::Row,
            Side::Up | Side::Down => Axis::Column,
        }
    }

    /// Right of it or under it: after it in the split.
    fn after(self) -> bool {
        matches!(self, Side::Right | Side::Down)
    }
}

/// Names a split while it exists (its resize handles say which split they move).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub(super) struct SplitId(u32);

pub(super) enum Member<P> {
    Pane(P),
    Split(Split<P>),
}

pub(super) struct Split<P> {
    pub(super) id: SplitId,
    pub(super) axis: Axis,
    pub(super) members: Vec<Member<P>>,
    /// Each member's part of the room, summing to 1.
    pub(super) shares: Vec<f32>,
    /// The split's bounds last frame, which turn a drag's points into shares.
    pub(super) bounds: Rc<Cell<Bounds<Pixels>>>,
}

pub(super) struct PaneTree<P> {
    root: Member<P>,
    next_id: u32,
}

impl<P: Clone + PartialEq> Member<P> {
    fn first_pane(&self) -> P {
        match self {
            Member::Pane(pane) => pane.clone(),
            Member::Split(split) => split.members[0].first_pane(),
        }
    }

    fn last_pane(&self) -> P {
        match self {
            Member::Pane(pane) => pane.clone(),
            Member::Split(split) => split.members[split.members.len() - 1].last_pane(),
        }
    }

    fn collect(&self, panes: &mut Vec<P>) {
        match self {
            Member::Pane(pane) => panes.push(pane.clone()),
            Member::Split(split) => split.members.iter().for_each(|m| m.collect(panes)),
        }
    }

    /// The least room it needs along `axis`: a pane's minimum; a split's
    /// members' together with the gaps between them (`gap` wide) along its
    /// own axis, the largest of them across it.
    pub(super) fn min_extent(&self, axis: Axis, gap: f32) -> f32 {
        match self {
            Member::Pane(_) => match axis {
                Axis::Row => MIN_PANE_WIDTH,
                Axis::Column => MIN_PANE_HEIGHT,
            },
            Member::Split(split) if split.axis == axis => {
                split.members.iter().map(|m| m.min_extent(axis, gap)).sum::<f32>() + gap * (split.members.len() - 1) as f32
            }
            Member::Split(split) => split.members.iter().map(|m| m.min_extent(axis, gap)).fold(0., f32::max),
        }
    }
}

impl<P> Split<P> {
    /// The room its members share along its axis: its length less the gaps
    /// (`gap` wide) between them.
    pub(super) fn room(&self, gap: f32) -> f32 {
        let size = self.bounds.get().size;
        let length = match self.axis {
            Axis::Row => size.width,
            Axis::Column => size.height,
        };
        f32::from(length) - gap * (self.members.len() - 1) as f32
    }
}

impl<P: Clone + PartialEq> PaneTree<P> {
    pub(super) fn new(pane: P) -> Self {
        Self { root: Member::Pane(pane), next_id: 0 }
    }

    pub(super) fn root(&self) -> &Member<P> {
        &self.root
    }

    /// Every pane in tree order: left to right, top to bottom.
    pub(super) fn panes(&self) -> Vec<P> {
        let mut panes = Vec::new();
        self.root.collect(&mut panes);
        panes
    }

    pub(super) fn len(&self) -> usize {
        self.panes().len()
    }

    /// Puts `new` beside `at`, on `side`: next to it in its split when that
    /// runs the same way (with half of its share), else in a new split of
    /// the two, half each. False if `at` isn't in the tree.
    pub(super) fn split(&mut self, at: &P, new: P, side: Side) -> bool {
        fn split_in<P: Clone + PartialEq>(member: &mut Member<P>, at: &P, new: &P, side: Side, next_id: &mut u32) -> bool {
            match member {
                Member::Pane(pane) if pane == at => {
                    let old = Member::Pane(pane.clone());
                    let new = Member::Pane(new.clone());
                    let members = if side.after() { vec![old, new] } else { vec![new, old] };
                    *next_id += 1;
                    *member = Member::Split(Split {
                        id: SplitId(*next_id),
                        axis: side.axis(),
                        members,
                        shares: vec![0.5, 0.5],
                        bounds: Rc::default(),
                    });
                    true
                }
                Member::Pane(_) => false,
                Member::Split(split) => {
                    if split.axis == side.axis()
                        && let Some(ix) = split.members.iter().position(|m| matches!(m, Member::Pane(p) if p == at))
                    {
                        let half = split.shares[ix] / 2.;
                        split.shares[ix] = half;
                        let at = if side.after() { ix + 1 } else { ix };
                        split.members.insert(at, Member::Pane(new.clone()));
                        split.shares.insert(at, half);
                        return true;
                    }
                    split.members.iter_mut().any(|m| split_in(m, at, new, side, next_id))
                }
            }
        }
        split_in(&mut self.root, at, &new, side, &mut self.next_id)
    }

    /// Takes a pane out: the member before it takes its share (the one after,
    /// for a first one), a split left with one member gives way to it, and a
    /// split that then sits in one along the same axis joins it. Returns the
    /// pane nearest to where it was, in what took its space; None for the
    /// last pane, which stays, or one not in the tree.
    pub(super) fn remove(&mut self, pane: &P) -> Option<P> {
        fn remove_in<P: Clone + PartialEq>(member: &mut Member<P>, pane: &P) -> Option<P> {
            let Member::Split(split) = member else { return None };
            let heir = match split.members.iter().position(|m| matches!(m, Member::Pane(p) if p == pane)) {
                Some(ix) => {
                    split.members.remove(ix);
                    let freed = split.shares.remove(ix);
                    split.shares[ix.saturating_sub(1)] += freed;
                    Some(if ix > 0 { split.members[ix - 1].last_pane() } else { split.members[0].first_pane() })
                }
                None => split.members.iter_mut().find_map(|m| remove_in(m, pane)),
            }?;
            tidy(split);
            Some(heir)
        }
        let heir = remove_in(&mut self.root, pane)?;
        if let Member::Split(split) = &mut self.root
            && split.members.len() == 1
        {
            self.root = split.members.pop().expect("one member");
        }
        Some(heir)
    }

    /// Whether the pane's top is the area's: no split above it stacks it
    /// under another member.
    pub(super) fn at_top(&self, pane: &P) -> bool {
        fn walk<P: PartialEq>(member: &Member<P>, pane: &P, top: bool) -> Option<bool> {
            match member {
                Member::Pane(p) => (p == pane).then_some(top),
                Member::Split(split) => split
                    .members
                    .iter()
                    .enumerate()
                    .find_map(|(ix, m)| walk(m, pane, top && (split.axis == Axis::Row || ix == 0))),
            }
        }
        walk(&self.root, pane, true).unwrap_or(false)
    }

    fn find_split(&mut self, id: SplitId) -> Option<&mut Split<P>> {
        fn find<P>(member: &mut Member<P>, id: SplitId) -> Option<&mut Split<P>> {
            let Member::Split(split) = member else { return None };
            if split.id == id {
                return Some(split);
            }
            split.members.iter_mut().find_map(|m| find(m, id))
        }
        find(&mut self.root, id)
    }

    /// How long member `ix` of split `id` is along the split's axis, in points
    /// (from last frame's bounds, with gaps `gap` wide between members).
    pub(super) fn extent(&mut self, id: SplitId, ix: usize, gap: f32) -> Option<f32> {
        let split = self.find_split(id)?;
        Some(split.room(gap) * *split.shares.get(ix)?)
    }

    /// Moves the boundary after member `ix` of split `id` so that member is
    /// `extent` points long, taking the room from the member after it or
    /// giving it back; both keep their least room when there's enough for
    /// that (else each keeps a tenth of what they share).
    pub(super) fn resize(&mut self, id: SplitId, ix: usize, extent: f32, gap: f32) {
        let Some(split) = self.find_split(id) else { return };
        let room = split.room(gap);
        if ix + 1 >= split.members.len() || room <= 0. {
            return;
        }
        let pair = split.shares[ix] + split.shares[ix + 1];
        let least = |m: &Member<P>| m.min_extent(split.axis, gap) / room;
        let (before, after) = (least(&split.members[ix]), least(&split.members[ix + 1]));
        let (lo, hi) = if before + after <= pair { (before, pair - after) } else { (pair * 0.1, pair * 0.9) };
        let share = (extent / room).clamp(lo, hi);
        split.shares[ix] = share;
        split.shares[ix + 1] = pair - share;
    }

    /// Gives every member of split `id` the same share.
    pub(super) fn even_out(&mut self, id: SplitId) {
        if let Some(split) = self.find_split(id) {
            let n = split.members.len();
            split.shares = vec![1. / n as f32; n];
        }
    }

    /// The tree as the session keeps it, each pane with its current tab.
    pub(super) fn save(&self, active: &mut dyn FnMut(&P) -> usize) -> PaneNode {
        fn node<P>(member: &Member<P>, active: &mut dyn FnMut(&P) -> usize) -> PaneNode {
            match member {
                Member::Pane(pane) => PaneNode::Pane { active: active(pane) },
                Member::Split(split) => {
                    let mut parts = Vec::new();
                    for (member, &share) in split.members.iter().zip(&split.shares) {
                        parts.push(PaneShare { share, node: node(member, active) });
                    }
                    match split.axis {
                        Axis::Row => PaneNode::Row(parts),
                        Axis::Column => PaneNode::Column(parts),
                    }
                }
            }
        }
        node(&self.root, active)
    }

    /// The tree a session saved. Its panes, numbered in tree order, come
    /// from `pane(number, active tab)`; one that doesn't come (no tab of it
    /// came back) is left out as closing it would be. None when no pane is
    /// left. Shares that don't add up (another version's, edited by hand)
    /// are evened out.
    pub(super) fn restore(node: &PaneNode, pane: &mut dyn FnMut(usize, usize) -> Option<P>) -> Option<Self> {
        fn build<P: Clone + PartialEq>(
            node: &PaneNode,
            pane: &mut dyn FnMut(usize, usize) -> Option<P>,
            number: &mut usize,
            next_id: &mut u32,
        ) -> Option<Member<P>> {
            let (axis, parts) = match node {
                PaneNode::Pane { active } => {
                    *number += 1;
                    return pane(*number - 1, *active).map(Member::Pane);
                }
                PaneNode::Row(parts) => (Axis::Row, parts),
                PaneNode::Column(parts) => (Axis::Column, parts),
            };
            let mut members = Vec::new();
            let mut shares = Vec::new();
            for part in parts {
                if let Some(member) = build(&part.node, pane, number, next_id) {
                    members.push(member);
                    shares.push(part.share);
                }
            }
            if members.len() < 2 {
                return members.pop();
            }
            let total: f32 = shares.iter().sum();
            if shares.iter().all(|s| s.is_finite() && *s > 0.) && total.is_finite() {
                shares.iter_mut().for_each(|s| *s /= total);
            } else {
                shares = vec![1. / members.len() as f32; members.len()];
            }
            *next_id += 1;
            let mut split = Split { id: SplitId(*next_id), axis, members, shares, bounds: Rc::default() };
            tidy(&mut split);
            Some(Member::Split(split))
        }
        let mut next_id = 0;
        let root = build(node, pane, &mut 0, &mut next_id)?;
        Some(Self { root, next_id })
    }
}

/// Whether a pane of these bounds (last frame's; empty before the first,
/// when it may) has room to be split on `side`: two least panes and the gap
/// (`gap` wide) between them.
pub(super) fn can_split(bounds: Bounds<Pixels>, side: Side, gap: f32) -> bool {
    let (width, height) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    if width <= 0. {
        return true;
    }
    match side.axis() {
        Axis::Row => width >= 2. * MIN_PANE_WIDTH + gap,
        Axis::Column => height >= 2. * MIN_PANE_HEIGHT + gap,
    }
}

/// The pane past `from`'s edge on `side`, of `panes` and their bounds: the
/// one under a point just across the gap (the widest, a Modern one), level
/// with the middle of `from`.
pub(super) fn pane_toward<P: Clone>(from: Bounds<Pixels>, side: Side, panes: &[(P, Bounds<Pixels>)]) -> Option<P> {
    let (middle, step) = (from.center(), px(GAP + 1.));
    let probe = match side {
        Side::Left => point(from.left() - step, middle.y),
        Side::Right => point(from.right() + step, middle.y),
        Side::Up => point(middle.x, from.top() - step),
        Side::Down => point(middle.x, from.bottom() + step),
    };
    panes.iter().find(|(_, bounds)| bounds.contains(&probe)).map(|(pane, _)| pane.clone())
}

/// Puts a split's members back in order after one changed: a member split
/// with one member gives way to it, and a member split along the same axis
/// joins, its members taking their part of its share.
fn tidy<P>(split: &mut Split<P>) {
    let mut ix = 0;
    while ix < split.members.len() {
        if let Member::Split(inner) = &mut split.members[ix]
            && inner.members.len() == 1
        {
            let only = inner.members.pop().expect("one member");
            split.members[ix] = only;
        }
        match &mut split.members[ix] {
            Member::Split(inner) if inner.axis == split.axis => {
                let share = split.shares[ix];
                let members = std::mem::take(&mut inner.members);
                let shares: Vec<f32> = inner.shares.iter().map(|s| s * share).collect();
                let count = members.len();
                split.members.splice(ix..=ix, members);
                split.shares.splice(ix..=ix, shares);
                ix += count;
            }
            _ => ix += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::{point, px, size};

    use super::*;

    /// The tree as text: panes by number, splits as row(…) / column(…) with
    /// their shares in percent.
    fn shape(tree: &PaneTree<u32>) -> String {
        fn member(m: &Member<u32>) -> String {
            match m {
                Member::Pane(p) => p.to_string(),
                Member::Split(s) => {
                    let name = if s.axis == Axis::Row { "row" } else { "column" };
                    let parts: Vec<String> =
                        s.members.iter().zip(&s.shares).map(|(m, share)| format!("{}:{}", member(m), (share * 100.).round())).collect();
                    format!("{name}({})", parts.join(" "))
                }
            }
        }
        member(tree.root())
    }

    /// Every split has two members or more, one share each summing to 1, and
    /// no member split runs the same way as its split.
    fn check(tree: &PaneTree<u32>) {
        fn walk(m: &Member<u32>, parent: Option<Axis>) {
            if let Member::Split(s) = m {
                assert!(s.members.len() >= 2, "a split of one");
                assert_eq!(s.members.len(), s.shares.len());
                assert!((s.shares.iter().sum::<f32>() - 1.).abs() < 1e-4, "shares {:?}", s.shares);
                assert_ne!(Some(s.axis), parent, "a split inside one along the same axis");
                s.members.iter().for_each(|c| walk(c, Some(s.axis)));
            }
        }
        walk(tree.root(), None);
    }

    fn grid() -> PaneTree<u32> {
        let mut tree = PaneTree::new(1);
        assert!(tree.split(&1, 2, Side::Right));
        assert!(tree.split(&1, 3, Side::Down));
        assert!(tree.split(&2, 4, Side::Down));
        check(&tree);
        tree
    }

    #[test]
    fn splits_make_a_grid() {
        let tree = grid();
        assert_eq!(shape(&tree), "row(column(1:50 3:50):50 column(2:50 4:50):50)");
        assert_eq!(tree.panes(), [1, 3, 2, 4]);
        // Along the window's top: the first of each column.
        assert_eq!([1, 2, 3, 4].map(|p| tree.at_top(&p)), [true, true, false, false]);
    }

    #[test]
    fn a_split_the_same_way_takes_half_of_its_pane() {
        let mut tree = PaneTree::new(1);
        tree.split(&1, 2, Side::Right);
        tree.split(&2, 3, Side::Right);
        assert_eq!(shape(&tree), "row(1:50 2:25 3:25)");
        // Left and up go before it.
        tree.split(&1, 4, Side::Left);
        assert_eq!(shape(&tree), "row(4:25 1:25 2:25 3:25)");
        tree.split(&3, 5, Side::Up);
        assert_eq!(shape(&tree), "row(4:25 1:25 2:25 column(5:50 3:50):25)");
        assert!(!tree.split(&9, 6, Side::Down), "not in the tree");
        check(&tree);
    }

    #[test]
    fn closing_gives_the_space_to_the_member_before() {
        let mut tree = PaneTree::new(1);
        tree.split(&1, 2, Side::Right);
        tree.split(&2, 3, Side::Right);
        assert_eq!(tree.remove(&2), Some(1));
        assert_eq!(shape(&tree), "row(1:75 3:25)");
        // A first one's goes to the one after it.
        assert_eq!(tree.remove(&1), Some(3));
        assert_eq!(shape(&tree), "3");
        // The last pane stays.
        assert_eq!(tree.remove(&3), None);
        assert_eq!(tree.remove(&7), None);
        assert_eq!(shape(&tree), "3");
    }

    #[test]
    fn a_split_left_with_one_member_gives_way_to_it() {
        let mut tree = grid();
        // Column(2, 4) collapses to 4, which takes its share in the row.
        assert_eq!(tree.remove(&2), Some(4));
        assert_eq!(shape(&tree), "row(column(1:50 3:50):50 4:50)");
        check(&tree);
        // The heir of a pane after a split is that split's last pane.
        assert_eq!(tree.remove(&4), Some(3));
        assert_eq!(shape(&tree), "column(1:50 3:50)");
        check(&tree);
    }

    #[test]
    fn a_split_along_its_parents_axis_joins_it() {
        // row(1, column(row(2, 5), 3)): closing 3 leaves row(2, 5) in the row.
        let mut tree = PaneTree::new(1);
        tree.split(&1, 2, Side::Right);
        tree.split(&2, 3, Side::Down);
        tree.split(&2, 5, Side::Right);
        assert_eq!(shape(&tree), "row(1:50 column(row(2:50 5:50):50 3:50):50)");
        tree.remove(&3);
        assert_eq!(shape(&tree), "row(1:50 2:25 5:25)");
        check(&tree);
    }

    #[test]
    fn resizing_keeps_both_sides_their_least_room() {
        let mut tree = PaneTree::new(1);
        tree.split(&1, 2, Side::Right);
        tree.split(&2, 3, Side::Down);
        let Member::Split(row) = tree.root() else { panic!() };
        let id = row.id;
        // 1000 points wide: 994 to share.
        row.bounds.set(Bounds { origin: point(px(0.), px(0.)), size: size(px(1000.), px(800.)) });
        assert_eq!(tree.extent(id, 0, GAP), Some(497.));
        tree.resize(id, 0, 300., GAP);
        assert!((tree.extent(id, 0, GAP).unwrap() - 300.).abs() < 0.01);
        // Not narrower than a pane, nor leaving less than one on the other side.
        tree.resize(id, 0, 10., GAP);
        assert!((tree.extent(id, 0, GAP).unwrap() - MIN_PANE_WIDTH).abs() < 0.01);
        tree.resize(id, 0, 990., GAP);
        assert!((tree.extent(id, 0, GAP).unwrap() - (994. - MIN_PANE_WIDTH)).abs() < 0.01);
        // A column needs the widest of its members.
        let Member::Split(row) = tree.root() else { panic!() };
        assert_eq!(row.members[1].min_extent(Axis::Row, GAP), MIN_PANE_WIDTH);
        assert_eq!(row.members[1].min_extent(Axis::Column, GAP), 2. * MIN_PANE_HEIGHT + GAP);
        tree.even_out(id);
        assert_eq!(shape(&tree), "row(1:50 column(2:50 3:50):50)");
        check(&tree);
    }

    #[test]
    fn a_saved_tree_comes_back_as_it_was() {
        let mut tree = grid();
        let Member::Split(row) = tree.root() else { panic!() };
        let id = row.id;
        row.bounds.set(Bounds { origin: point(px(0.), px(0.)), size: size(px(1006.), px(800.)) });
        tree.resize(id, 0, 400., GAP);
        // Each pane's current tab: its number times ten, to see them come back.
        let saved = tree.save(&mut |p| *p as usize * 10);
        let mut asked = Vec::new();
        let restored = PaneTree::restore(&saved, &mut |n, active| {
            asked.push((n, active));
            Some([1, 3, 2, 4][n])
        })
        .unwrap();
        assert_eq!(asked, [(0, 10), (1, 30), (2, 20), (3, 40)], "in tree order, with their tabs");
        assert_eq!(shape(&restored), "row(column(1:50 3:50):40 column(2:50 4:50):60)");
        check(&restored);
    }

    #[test]
    fn panes_that_dont_come_back_are_left_out() {
        let saved = grid().save(&mut |_| 0);
        // Pane 3 (the bottom left) had no tab left: its column gives way to 1.
        let restored = PaneTree::restore(&saved, &mut |n, _| (n != 1).then_some([1, 3, 2, 4][n])).unwrap();
        assert_eq!(shape(&restored), "row(1:50 column(2:50 4:50):50)");
        check(&restored);
        // A column of a row inside a column left with that row joins the outer
        // row: row(1, column(row(2, 5), 3)) without 3.
        let mut tree = PaneTree::new(1);
        tree.split(&1, 2, Side::Right);
        tree.split(&2, 3, Side::Down);
        tree.split(&2, 5, Side::Right);
        let saved = tree.save(&mut |_| 0);
        let restored = PaneTree::restore(&saved, &mut |n, _| (n != 3).then_some([1, 2, 5, 3][n])).unwrap();
        assert_eq!(shape(&restored), "row(1:50 2:25 5:25)");
        check(&restored);
        // None left: no tree.
        assert!(PaneTree::<u32>::restore(&saved, &mut |_, _| None).is_none());
        // One left: just that pane.
        assert_eq!(shape(&PaneTree::restore(&saved, &mut |n, _| (n == 2).then_some(5)).unwrap()), "5");
    }

    #[test]
    fn shares_that_dont_add_up_are_evened_out() {
        let pane = |share: f32, n: usize| PaneShare { share, node: PaneNode::Pane { active: n } };
        let restore = |node: PaneNode| shape(&PaneTree::restore(&node, &mut |_, n| Some(n as u32)).unwrap());
        // Scaled to add up to 1.
        assert_eq!(restore(PaneNode::Row(vec![pane(1., 1), pane(3., 2)])), "row(1:25 2:75)");
        assert_eq!(restore(PaneNode::Column(vec![pane(f32::NAN, 1), pane(0.5, 2)])), "column(1:50 2:50)");
        assert_eq!(restore(PaneNode::Row(vec![pane(0., 1), pane(0.5, 2), pane(-1., 3)])), "row(1:33 2:33 3:33)");
        // A row in a row joins it, keeping its part.
        let inner = PaneShare { share: 0.5, node: PaneNode::Row(vec![pane(0.5, 2), pane(0.5, 3)]) };
        assert_eq!(restore(PaneNode::Row(vec![pane(0.5, 1), inner])), "row(1:50 2:25 3:25)");
    }

    #[test]
    fn the_pane_beside_is_the_one_across_the_gap() {
        let at = |x: f32, y: f32, w: f32, h: f32| Bounds { origin: point(px(x), px(y)), size: size(px(w), px(h)) };
        // A tall pane on the left; two stacked on the right, the upper one short.
        let (left, upper, lower) = (at(0., 0., 400., 800.), at(406., 0., 600., 200.), at(406., 206., 600., 594.));
        let panes = [(1, left), (2, upper), (3, lower)];
        // Level with the left pane's middle: the lower one.
        assert_eq!(pane_toward(left, Side::Right, &panes), Some(3));
        assert_eq!(pane_toward(upper, Side::Down, &panes), Some(3));
        assert_eq!(pane_toward(lower, Side::Up, &panes), Some(2));
        assert_eq!(pane_toward(upper, Side::Left, &panes), Some(1));
        // At the area's edge there's none.
        assert_eq!(pane_toward(left, Side::Left, &panes), None);
        assert_eq!(pane_toward(lower, Side::Down, &panes), None);
    }

    #[test]
    fn small_panes_dont_split() {
        let bounds = |w: f32, h: f32| Bounds { origin: point(px(0.), px(0.)), size: size(px(w), px(h)) };
        assert!(can_split(bounds(2. * MIN_PANE_WIDTH + GAP, 100.), Side::Right, GAP));
        assert!(!can_split(bounds(2. * MIN_PANE_WIDTH, 100.), Side::Left, GAP));
        assert!(!can_split(bounds(1000., MIN_PANE_HEIGHT), Side::Down, GAP));
        // Before the first frame nothing is known: it may.
        assert!(can_split(Bounds::default(), Side::Down, GAP));
    }
}
