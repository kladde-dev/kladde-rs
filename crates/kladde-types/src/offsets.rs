//! [`Offsets`]: where each element of a packed sequence starts, which the
//! packed and small vectors keep in memory, since nothing on file says.

use std::cell::Cell;

use kladde_persist::{Packed, Persistable, PointerRepr};

/// Where each element of a packed sequence starts, relative to the start of
/// the sequence, and after the last, where the sequence ends: one more entry
/// than there are elements, once the sequence is laid out, and none before.
///
/// The entries are cells, so that the node an element's guard reports to can
/// shift them through a shared reference while the element itself is
/// borrowed mutably.
pub(crate) struct Offsets(Vec<Cell<usize>>);

impl Offsets {
    /// Not laid out yet.
    pub(crate) fn new() -> Self {
        Offsets(Vec::new())
    }

    /// The offsets of `items` laid out packed, back to back.
    pub(crate) fn of<T: Persistable<P>, P: PointerRepr>(items: &[T]) -> Self {
        let mut offsets = Vec::with_capacity(items.len() + 1);
        let mut at = 0;
        offsets.push(Cell::new(at));
        for item in items {
            at += item.encoded_size::<Packed>();
            offsets.push(Cell::new(at));
        }
        Offsets(offsets)
    }

    /// Whether the sequence has been laid out.
    pub(crate) fn laid_out(&self) -> bool {
        !self.0.is_empty()
    }

    /// Where element `index` starts; `index == len` is where the last ends.
    pub(crate) fn offset(&self, index: usize) -> usize {
        self.0[index].get()
    }

    /// How many bytes element `index` takes.
    pub(crate) fn len_of(&self, index: usize) -> usize {
        self.offset(index + 1) - self.offset(index)
    }

    /// The sequence's size.
    pub(crate) fn end(&self) -> usize {
        self.0.last().map_or(0, Cell::get)
    }

    /// Records an element of `len` bytes inserted at `index`.
    pub(crate) fn insert(&mut self, index: usize, len: usize) {
        if self.0.is_empty() {
            self.0.push(Cell::new(0));
        }
        let at = self.offset(index);
        self.0.insert(index, Cell::new(at));
        self.shift(index + 1, len as isize);
    }

    /// Records the removal of element `index`.
    pub(crate) fn remove(&mut self, index: usize) {
        let len = self.len_of(index);
        self.0.remove(index);
        self.shift(index, -(len as isize));
    }

    /// Moves every offset from entry `from` on by `delta` bytes.
    pub(crate) fn shift(&self, from: usize, delta: isize) {
        for cell in &self.0[from..] {
            cell.set(cell.get().wrapping_add_signed(delta));
        }
    }

    /// An empty sequence, laid out.
    pub(crate) fn clear(&mut self) {
        self.0 = vec![Cell::new(0)];
    }
}

impl FromIterator<usize> for Offsets {
    /// Offsets from the elements' sizes, in order.
    fn from_iter<I: IntoIterator<Item = usize>>(sizes: I) -> Self {
        let mut at = 0;
        let mut offsets = vec![Cell::new(0)];
        for size in sizes {
            at += size;
            offsets.push(Cell::new(at));
        }
        Offsets(offsets)
    }
}
