//! [`Layout`]: where a packed sequence's content lies and where each element
//! starts in it, which the packed and small vectors keep in memory, since
//! nothing on file says.

use std::cell::Cell;

use kladde_persist::{Packed, Persistable, PointerRepr};
use thin_vec::ThinVec;

/// Where a packed sequence's content lies, where each element starts in it,
/// and where it ends.
///
/// `content` is what the vector keeps of where its content lies: a packed
/// vector's content allocation, or the allocation a small vector's content
/// spilled into. It is kept here rather than beside, so that it fills the
/// four bytes the end leaves free and the whole takes 16 bytes.
///
/// Only the starts of the second and later elements take an entry, since the
/// first starts at 0. So a sequence of zero or one element allocates nothing,
/// and one of `n` elements `n - 1` entries, in a `ThinVec`, whose length and
/// capacity sit in its heap block.
///
/// Every element takes at least one byte, since an element type whose
/// encodings take none cannot form a sequence; so a laid-out sequence is
/// empty exactly when its end is 0, and holds one element exactly when it
/// ends later and has no entries. A sequence that holds elements and ends at
/// 0 has not been laid out: its vector was collected from an iterator and is
/// not stored yet.
///
/// The entries and the end are cells, so that the node an element's guard
/// reports to can shift them through a shared reference while the element
/// itself is borrowed mutably. They are `u32`s, since no allocation is
/// larger than that, and a sequence fills one at most.
pub(crate) struct Layout<C> {
    /// Where the content lies, as the vector keeps it.
    pub(crate) content: C,
    /// Where elements `1..len` start.
    starts: ThinVec<Cell<u32>>,
    /// Where the last element ends: the sequence's size.
    end: Cell<u32>,
}

impl<C> Layout<C> {
    /// An empty sequence, or one not laid out yet.
    pub(crate) fn new(content: C) -> Self {
        Layout {
            content,
            starts: ThinVec::new(),
            end: Cell::new(0),
        }
    }

    /// The same starts, with `content` for where the content lies.
    pub(crate) fn with_content<D>(self, content: D) -> Layout<D> {
        Layout {
            content,
            starts: self.starts,
            end: self.end,
        }
    }

    /// Whether the sequence of `len` elements has been laid out.
    pub(crate) fn laid_out(&self, len: usize) -> bool {
        len == 0 || self.end.get() > 0
    }

    /// How many elements the laid-out sequence holds.
    fn len(&self) -> usize {
        if self.end.get() == 0 {
            0
        } else {
            self.starts.len() + 1
        }
    }

    /// Where element `index` starts; `index == len` is where the last ends.
    pub(crate) fn offset(&self, index: usize) -> usize {
        if index == 0 {
            0
        } else if let Some(start) = self.starts.get(index - 1) {
            start.get() as usize
        } else {
            debug_assert_eq!(index, self.len(), "an offset past the end");
            self.end()
        }
    }

    /// How many bytes element `index` takes.
    pub(crate) fn len_of(&self, index: usize) -> usize {
        self.offset(index + 1) - self.offset(index)
    }

    /// The sequence's size.
    pub(crate) fn end(&self) -> usize {
        self.end.get() as usize
    }

    /// Records an element of `len` bytes inserted at `index`.
    pub(crate) fn insert(&mut self, index: usize, len: usize) {
        debug_assert!(len > 0, "an element takes at least one byte");
        if self.len() > 0 {
            // The entry of whichever element now starts where the new one
            // does: the new element's own, or, when it takes index 0, the old
            // first element's, which the shift below moves past it.
            let at = self.offset(index) as u32;
            self.starts.insert(index.saturating_sub(1), Cell::new(at));
        }
        self.shift(index + 1, len as isize);
    }

    /// Records the removal of element `index`.
    pub(crate) fn remove(&mut self, index: usize) {
        let len = self.len_of(index);
        // The removed element's entry, or, for the first, the entry of the
        // second, which becomes the first and starts at 0.
        let entry = index.saturating_sub(1);
        if entry < self.starts.len() {
            self.starts.remove(entry);
        }
        self.shift(index, -(len as isize));
    }

    /// Moves the start of every element from `from` on, and the end, by
    /// `delta` bytes.
    pub(crate) fn shift(&self, from: usize, delta: isize) {
        let moved = |cell: &Cell<u32>| cell.set((cell.get() as isize + delta) as u32);
        for cell in self.starts.iter().skip(from.saturating_sub(1)) {
            moved(cell);
        }
        moved(&self.end);
    }

    /// An empty sequence, laid out; `content` stays as it is.
    pub(crate) fn clear(&mut self) {
        self.starts = ThinVec::new();
        self.end.set(0);
    }
}

impl Layout<()> {
    /// The layout of `items` packed, back to back.
    pub(crate) fn of<T: Persistable<P>, P: PointerRepr>(items: &[T]) -> Self {
        items
            .iter()
            .map(|item| item.encoded_size::<Packed>())
            .collect()
    }
}

impl FromIterator<usize> for Layout<()> {
    /// The layout of elements of these sizes, in order.
    fn from_iter<I: IntoIterator<Item = usize>>(sizes: I) -> Self {
        let sizes = sizes.into_iter();
        let mut starts = ThinVec::with_capacity(sizes.size_hint().0.saturating_sub(1));
        let mut at = 0u32;
        for size in sizes {
            if at > 0 {
                starts.push(Cell::new(at));
            }
            debug_assert!(size > 0, "an element takes at least one byte");
            at += size as u32;
        }
        starts.shrink_to_fit();
        Layout {
            content: (),
            starts,
            end: Cell::new(at),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The starts and the end, as `offset(0..=len)` reports them.
    fn all<C>(layout: &Layout<C>) -> Vec<usize> {
        (0..=layout.len()).map(|i| layout.offset(i)).collect()
    }

    #[test]
    fn zero_and_one_element_allocate_no_entries() {
        let mut l = Layout::new(());
        assert!(l.laid_out(0));
        assert_eq!(all(&l), [0]);
        l.insert(0, 5);
        assert_eq!(all(&l), [0, 5]);
        assert!(l.starts.is_empty());
        l.remove(0);
        assert_eq!(all(&l), [0]);
    }

    #[test]
    fn inserts_removals_and_shifts_keep_the_starts() {
        // The element sizes after each step are in the comments.
        let mut l: Layout<()> = [3, 4].into_iter().collect();
        assert_eq!(all(&l), [0, 3, 7]);
        l.insert(0, 2); // 2 3 4
        assert_eq!(all(&l), [0, 2, 5, 9]);
        l.insert(3, 1); // 2 3 4 1
        assert_eq!(all(&l), [0, 2, 5, 9, 10]);
        l.insert(2, 6); // 2 3 6 4 1
        assert_eq!(all(&l), [0, 2, 5, 11, 15, 16]);
        l.shift(2, -1); // 2 2 6 4 1: element 1 shrank by a byte
        assert_eq!(all(&l), [0, 2, 4, 10, 14, 15]);
        l.remove(0); // 2 6 4 1
        assert_eq!(all(&l), [0, 2, 8, 12, 13]);
        l.remove(3); // 2 6 4
        assert_eq!(all(&l), [0, 2, 8, 12]);
        l.remove(1); // 2 4
        assert_eq!(all(&l), [0, 2, 6]);
        l.remove(1); // 2
        assert!(l.starts.is_empty());
        l.remove(0);
        assert_eq!(all(&l), [0]);
    }

    #[test]
    fn a_collected_sequence_is_not_laid_out_until_it_has_an_end() {
        let l = Layout::new(());
        assert!(!l.laid_out(3));
        let l: Layout<()> = [1, 2, 3].into_iter().collect();
        assert!(l.laid_out(3));
    }

    #[test]
    fn with_a_four_byte_handle_the_value_takes_sixteen_bytes() {
        assert_eq!(
            std::mem::size_of::<Layout<Option<std::num::NonZeroU32>>>(),
            16
        );
        assert_eq!(
            std::mem::size_of::<Layout<Cell<Option<std::num::NonZeroU32>>>>(),
            16
        );
    }
}
