//! [`Sizes`]: a Fenwick tree over the encoded sizes of a container's
//! variable-size elements, in cells, so that an element's guard can report a
//! change of size while its container's guard is borrowed.

use std::cell::Cell;

use kladde_persist::Node;

/// The encoded sizes of a container's elements, as a Fenwick tree: where
/// element `i` starts, and a change of an element's size, both in
/// `O(log m)` for `m` elements; appending in `O(log m)`, and inserting,
/// removing or reordering by a rebuild in `O(m)`.
///
/// Four bytes per element, and the sizes add up to at most `u32::MAX`; a
/// container whose content outgrows that panics, as a bound of this
/// implementation.
#[derive(Default)]
pub(crate) struct Sizes {
    /// Node `k` holds the sum of the sizes in `(k + 1 - lowbit(k + 1), k + 1]`,
    /// counting elements from 1.
    tree: Vec<Cell<u32>>,
    total: Cell<u64>,
    /// Where the first element starts within the container: the count's
    /// varint, for a [`Node::offset_of`] of the container's own start.
    base: Cell<u64>,
}

fn lowbit(k: usize) -> usize {
    k & k.wrapping_neg()
}

fn narrow(n: u64) -> u32 {
    u32::try_from(n).expect("a container's elements take at most 4 GiB")
}

impl Sizes {
    /// The tree over `sizes`, in `O(m)`.
    pub(crate) fn build(sizes: impl Iterator<Item = usize>, base: u64) -> Sizes {
        let mut tree: Vec<u64> = sizes.map(|s| s as u64).collect();
        let total = tree.iter().sum::<u64>();
        narrow(total);
        for k in 1..=tree.len() {
            let parent = k + lowbit(k);
            if parent <= tree.len() {
                tree[parent - 1] += tree[k - 1];
            }
        }
        Sizes {
            tree: tree.into_iter().map(|v| Cell::new(v as u32)).collect(),
            total: Cell::new(total),
            base: Cell::new(base),
        }
    }

    /// The number of elements.
    pub(crate) fn len(&self) -> usize {
        self.tree.len()
    }

    /// The sum of all sizes.
    pub(crate) fn total(&self) -> u64 {
        self.total.get()
    }

    /// Records where the first element starts within the container.
    pub(crate) fn set_base(&self, base: u64) {
        self.base.set(base);
    }

    /// The sum of the sizes of elements `0..i`.
    pub(crate) fn prefix(&self, i: usize) -> u64 {
        let mut sum = 0u64;
        let mut k = i;
        while k > 0 {
            sum += self.tree[k - 1].get() as u64;
            k -= lowbit(k);
        }
        sum
    }

    /// Adds `delta` to element `i`'s size.
    pub(crate) fn add(&self, i: usize, delta: i64) {
        let total = self.total.get().checked_add_signed(delta).unwrap();
        narrow(total);
        self.total.set(total);
        let mut k = i + 1;
        while k <= self.tree.len() {
            let v = &self.tree[k - 1];
            v.set((v.get() as i64 + delta) as u32);
            k += lowbit(k);
        }
    }

    /// Appends an element of `size` bytes.
    pub(crate) fn push(&mut self, size: usize) {
        let k = self.tree.len() + 1;
        // Node `k` covers `(k - lowbit(k), k]`: the new size and the sums of
        // the elements before it in that range.
        let covered = self.prefix(k - 1) - self.prefix(k - lowbit(k));
        let total = self.total.get() + size as u64;
        narrow(total);
        self.total.set(total);
        self.tree.push(Cell::new(narrow(covered + size as u64)));
    }

    /// Removes the last element, whose size is `size`.
    pub(crate) fn pop(&mut self, size: usize) {
        self.tree.pop();
        self.total.set(self.total.get() - size as u64);
    }
}

impl Node for Sizes {
    fn offset_of(&self, index: usize) -> u64 {
        self.base.get() + self.prefix(index)
    }

    fn resized(&self, index: usize, old: u64, new: u64) {
        self.add(index, new as i64 - old as i64);
    }
}

impl Clone for Sizes {
    fn clone(&self) -> Self {
        Sizes {
            tree: self.tree.clone(),
            total: self.total.clone(),
            base: self.base.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_follow_pushes_adds_and_pops() {
        let mut model: Vec<u64> = Vec::new();
        let mut sizes = Sizes::build([3usize, 1, 4, 1, 5].into_iter(), 1);
        model.extend([3, 1, 4, 1, 5]);
        let mut seed = 7u64;
        for step in 0..2000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let r = (seed >> 33) as usize;
            match r % 4 {
                0 => {
                    sizes.push(r % 50);
                    model.push((r % 50) as u64);
                }
                1 if !model.is_empty() => {
                    let i = r % model.len();
                    let delta = (r % 7) as i64 - model[i].min(3) as i64;
                    sizes.add(i, delta);
                    model[i] = (model[i] as i64 + delta) as u64;
                }
                2 if !model.is_empty() && step % 3 == 0 => {
                    let last = model.pop().unwrap();
                    sizes.pop(last as usize);
                }
                _ => {}
            }
            let i = r % (model.len() + 1);
            assert_eq!(sizes.prefix(i), model[..i].iter().sum::<u64>());
            assert_eq!(sizes.total(), model.iter().sum::<u64>());
            assert_eq!(sizes.offset_of(i), 1 + model[..i].iter().sum::<u64>());
        }
        let rebuilt = Sizes::build(model.iter().map(|&s| s as usize), 0);
        for i in 0..=model.len() {
            assert_eq!(rebuilt.prefix(i), sizes.prefix(i));
        }
    }
}
