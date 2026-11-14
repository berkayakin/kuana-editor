//! Navigation history for Back / Forward: the places the caret left by
//! navigating (Go to Declaration, a search result, Go to Line, another tab…),
//! not every caret move. The workspace records them (`Workspace::navigate`,
//! tab switches in `activate_in`) and steps through them
//! (`workspace/nav_history.rs`).

use std::path::{Path, PathBuf};

use gpui::EntityId;

/// At most this many places each way; the oldest go first.
const MAX_PLACES: usize = 100;

/// Where the caret was in a file.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub path: PathBuf,
    /// Byte offset, valid for `doc` at `version`.
    pub offset: usize,
    /// The line and byte column, for when the offset can't follow the edits
    /// since (the document was closed and read again, or its patches pruned).
    pub row: usize,
    pub column: usize,
    pub doc: EntityId,
    pub version: u64,
}

impl Place {
    /// The same line of the same file: going there wouldn't move anything.
    pub fn same_line(&self, other: &Place) -> bool {
        self.path == other.path && self.row == other.row
    }
}

#[derive(Default)]
pub struct NavHistory {
    back: Vec<Place>,
    forward: Vec<Place>,
    /// While above 0, tab switches record nothing: a navigation (or Back
    /// itself) is moving the caret and records once, when it's done.
    paused: usize,
}

impl NavHistory {
    pub fn is_paused(&self) -> bool {
        self.paused > 0
    }

    pub fn pause(&mut self) {
        self.paused += 1;
    }

    pub fn resume(&mut self) {
        self.paused = self.paused.saturating_sub(1);
    }

    /// A navigation went from `from` to `to` (None: somewhere without a
    /// place, such as a diff tab): `from` can be gone back to, and what was
    /// ahead (Forward) is dropped, as in a browser.
    pub fn record(&mut self, from: Place, to: Option<&Place>) {
        if to.is_some_and(|to| to.same_line(&from)) {
            return;
        }
        push(&mut self.back, from);
        self.forward.clear();
    }

    /// The place before the current one (`back`), or after it, taken off
    /// its list; the caller puts where it was on the other with `passed`.
    pub fn take(&mut self, back: bool) -> Option<Place> {
        if back { self.back.pop() } else { self.forward.pop() }
    }

    /// Back (`back`) or Forward went from `here`: it goes on the other list.
    pub fn passed(&mut self, back: bool, here: Place) {
        push(if back { &mut self.forward } else { &mut self.back }, here);
    }

    #[cfg(test)]
    fn can_go(&self, back: bool) -> bool {
        !(if back { &self.back } else { &self.forward }).is_empty()
    }

    /// A file or folder moved from `old` to `new`: its places follow.
    pub fn moved(&mut self, old: &Path, new: &Path) {
        for place in self.back.iter_mut().chain(self.forward.iter_mut()) {
            if let Ok(rest) = place.path.strip_prefix(old) {
                place.path = if rest.as_os_str().is_empty() { new.to_path_buf() } else { new.join(rest) };
            }
        }
    }

    pub fn clear(&mut self) {
        self.back.clear();
        self.forward.clear();
    }
}

/// Adds a place, unless it's the line on top already (which it then
/// replaces, keeping the newer column), dropping the oldest past the limit.
fn push(list: &mut Vec<Place>, place: Place) {
    match list.last_mut() {
        Some(top) if top.same_line(&place) => *top = place,
        _ => list.push(place),
    }
    if list.len() > MAX_PLACES {
        list.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(path: &str, row: usize) -> Place {
        Place { path: PathBuf::from(path), offset: row * 10, row, column: 0, doc: EntityId::from(1u64), version: 0 }
    }

    #[test]
    fn back_and_forward_walk_the_places() {
        let mut nav = NavHistory::default();
        nav.record(place("/a", 1), Some(&place("/b", 5)));
        nav.record(place("/b", 5), Some(&place("/c", 9)));
        assert!(nav.can_go(true) && !nav.can_go(false));

        // At /c: Back to /b, then /a.
        let to = nav.take(true).unwrap();
        assert_eq!(to, place("/b", 5));
        nav.passed(true, place("/c", 9));
        let to = nav.take(true).unwrap();
        assert_eq!(to, place("/a", 1));
        nav.passed(true, place("/b", 5));
        assert!(!nav.can_go(true));

        // Forward retraces them.
        assert_eq!(nav.take(false), Some(place("/b", 5)));
        nav.passed(false, place("/a", 1));
        assert_eq!(nav.take(false), Some(place("/c", 9)));
        nav.passed(false, place("/b", 5));
        assert!(!nav.can_go(false));
        assert_eq!(nav.take(true), Some(place("/b", 5)));
    }

    #[test]
    fn a_new_navigation_drops_what_was_ahead() {
        let mut nav = NavHistory::default();
        nav.record(place("/a", 1), Some(&place("/b", 1)));
        let _ = nav.take(true);
        nav.passed(true, place("/b", 1));
        assert!(nav.can_go(false));
        nav.record(place("/a", 1), Some(&place("/c", 1)));
        assert!(!nav.can_go(false));
    }

    #[test]
    fn staying_on_the_line_records_nothing_and_repeats_merge() {
        let mut nav = NavHistory::default();
        let mut moved = place("/a", 3);
        moved.column = 7;
        nav.record(place("/a", 3), Some(&moved));
        assert!(!nav.can_go(true));

        nav.record(place("/a", 3), Some(&place("/a", 40)));
        nav.record(moved.clone(), Some(&place("/b", 1)));
        assert_eq!(nav.take(true), Some(moved));
        assert!(!nav.can_go(true));
    }

    #[test]
    fn keeps_a_bounded_list_and_follows_moves() {
        let mut nav = NavHistory::default();
        for row in 0..MAX_PLACES + 20 {
            nav.record(place("/dir/a", row), None);
        }
        assert_eq!(nav.back.len(), MAX_PLACES);
        assert_eq!(nav.back[0].row, 20);

        nav.moved(Path::new("/dir"), Path::new("/other"));
        assert!(nav.back.iter().all(|p| p.path == Path::new("/other/a")));
        nav.moved(Path::new("/other/a"), Path::new("/other/b"));
        assert_eq!(nav.take(true).unwrap().path, PathBuf::from("/other/b"));
        // A prefix of the name alone isn't the folder.
        nav.record(place("/other/bc", 1), None);
        nav.moved(Path::new("/other/b"), Path::new("/x"));
        assert_eq!(nav.take(true).unwrap().path, PathBuf::from("/other/bc"));
    }

    #[test]
    fn pausing_nests() {
        let mut nav = NavHistory::default();
        nav.pause();
        nav.pause();
        nav.resume();
        assert!(nav.is_paused());
        nav.resume();
        nav.resume();
        assert!(!nav.is_paused());
    }
}
