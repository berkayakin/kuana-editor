//! Marks a user puts on lines (breakpoints, bookmarks): kept with the offset
//! their line starts at, sorted, so they stay on their lines as text is
//! edited above them, and a row's mark is a binary search away (drawing asks
//! for every line on screen).

/// Something that sits on a line.
pub trait OnRow: Clone {
    /// 0-based.
    fn row(&self) -> usize;
    fn set_row(&mut self, row: usize);
}

#[derive(Clone, Debug)]
pub struct LineMarks<T> {
    /// (line start, mark), sorted by line start, one per line.
    marks: Vec<(usize, T)>,
}

impl<T> Default for LineMarks<T> {
    fn default() -> Self {
        Self { marks: Vec::new() }
    }
}

impl<T: OnRow> LineMarks<T> {
    /// How many there are (for the tests' work counts).
    #[cfg(test)]
    pub fn count(&self) -> usize {
        self.marks.len()
    }

    /// The line starts, in order.
    pub fn starts(&self) -> impl Iterator<Item = usize> + '_ {
        self.marks.iter().map(|(start, _)| *start)
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.marks.iter().map(|(_, mark)| mark)
    }

    pub fn to_vec(&self) -> Vec<T> {
        self.iter().cloned().collect()
    }

    pub fn at(&self, row: usize) -> Option<&T> {
        // Sorted by line start, so by row.
        let ix = self.marks.partition_point(|(_, m)| m.row() < row);
        self.marks.get(ix).map(|(_, m)| m).filter(|m| m.row() == row)
    }

    /// Replaces them all with `marks` at their line starts; the first of
    /// several on one line stays.
    pub fn replace(&mut self, marks: impl IntoIterator<Item = (usize, T)>) {
        let mut placed: Vec<(usize, T)> = marks.into_iter().collect();
        placed.sort_by_key(|(start, _)| *start);
        placed.dedup_by_key(|(start, _)| *start);
        self.marks = placed;
    }

    /// Puts `mark` on the line starting at `start`, replacing the one there.
    pub fn set(&mut self, start: usize, mark: T) {
        match self.marks.binary_search_by_key(&start, |(s, _)| *s) {
            Ok(ix) => self.marks[ix].1 = mark,
            Err(ix) => self.marks.insert(ix, (start, mark)),
        }
    }

    /// Removes the mark on the line starting at `start`.
    pub fn remove(&mut self, start: usize) -> Option<T> {
        let ix = self.marks.binary_search_by_key(&start, |(s, _)| *s).ok()?;
        Some(self.marks.remove(ix).1)
    }

    /// Moves each mark to where its line starts now (`moved`; None when the
    /// line went, and the mark with it), with its row from `row_of`.
    pub fn shift(&mut self, moved: impl Fn(usize) -> Option<usize>, row_of: impl Fn(usize) -> usize) {
        if self.marks.is_empty() {
            return;
        }
        let mut shifted = Vec::with_capacity(self.marks.len());
        for (offset, mut mark) in std::mem::take(&mut self.marks) {
            if let Some(start) = moved(offset) {
                mark.set_row(row_of(start));
                shifted.push((start, mark));
            }
        }
        // Lines that merged keep the first mark.
        shifted.dedup_by_key(|(start, _)| *start);
        self.marks = shifted;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct Mark(usize);

    impl OnRow for Mark {
        fn row(&self) -> usize {
            self.0
        }
        fn set_row(&mut self, row: usize) {
            self.0 = row;
        }
    }

    #[test]
    fn keeps_one_mark_per_line_in_order() {
        let mut marks = LineMarks::default();
        marks.replace([(20, Mark(2)), (0, Mark(0)), (20, Mark(2))]);
        assert_eq!(marks.to_vec(), vec![Mark(0), Mark(2)]);
        assert_eq!(marks.at(2), Some(&Mark(2)));
        assert_eq!(marks.at(1), None);
        marks.set(10, Mark(1));
        assert_eq!(marks.starts().collect::<Vec<_>>(), vec![0, 10, 20]);
        assert_eq!(marks.remove(10), Some(Mark(1)));
        assert_eq!(marks.remove(10), None);
        // Every line moves down 5 bytes and a row; the first line goes.
        marks.shift(|start| (start > 0).then_some(start + 5), |start| start / 10);
        assert_eq!(marks.to_vec(), vec![Mark(2)]);
        assert_eq!(marks.starts().collect::<Vec<_>>(), vec![25]);
    }
}
