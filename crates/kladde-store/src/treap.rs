//! An implicit treap: a sequence of items, each with a length, kept in a
//! randomized balanced binary tree whose nodes know the total length below
//! them. The item at a position is found, a sequence is cut at a position,
//! and two sequences are joined, each in `O(log n)` expected steps.
//!
//! One arena holds any number of trees, each named by its root node; the fold
//! keeps the cuts of a transaction as trees of their own beside the piece
//! table, and the store keeps the rope over the sequence's spans. Nodes know
//! their parent, so an item's position, and its neighbours, are found from
//! its node.

/// The empty tree, and the absence of a node.
pub(crate) const NIL: u32 = 0;

/// What a treap holds: anything with a length.
pub(crate) trait Item: Copy {
    fn len(&self) -> u64;
}

#[derive(Clone, Copy, Debug)]
struct Node<T> {
    item: T,
    left: u32,
    right: u32,
    parent: u32,
    prio: u32,
    /// The total length of the subtree.
    sum: u64,
}

/// An arena of treap nodes, holding any number of trees.
#[derive(Clone, Debug)]
pub(crate) struct Treap<T> {
    nodes: Vec<Node<T>>,
    free: Vec<u32>,
    rng: u64,
}

impl<T: Item + Default> Default for Treap<T> {
    fn default() -> Self {
        Treap::new()
    }
}

impl<T: Item + Default> Treap<T> {
    pub fn new() -> Self {
        Treap {
            nodes: vec![Node {
                item: T::default(),
                left: NIL,
                right: NIL,
                parent: NIL,
                prio: 0,
                sum: 0,
            }],
            free: Vec::new(),
            rng: 0x9E37_79B9_7F4A_7C15,
        }
    }
}

impl<T: Item> Treap<T> {
    fn prio(&mut self) -> u32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 32) as u32
    }

    /// A tree of one node holding `item`.
    pub fn node(&mut self, item: T) -> u32 {
        let prio = self.prio();
        let n = Node {
            item,
            left: NIL,
            right: NIL,
            parent: NIL,
            prio,
            sum: item.len(),
        };
        match self.free.pop() {
            Some(i) => {
                self.nodes[i as usize] = n;
                i
            }
            None => {
                self.nodes.push(n);
                (self.nodes.len() - 1) as u32
            }
        }
    }

    /// Returns every node of the tree at `root` to the arena.
    pub fn free_tree(&mut self, root: u32) {
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            if n == NIL {
                continue;
            }
            let node = self.nodes[n as usize];
            stack.push(node.left);
            stack.push(node.right);
            self.free.push(n);
        }
    }

    /// Nodes in use.
    #[cfg(test)]
    pub fn live_nodes(&self) -> usize {
        self.nodes.len() - 1 - self.free.len()
    }

    pub fn item(&self, n: u32) -> &T {
        &self.nodes[n as usize].item
    }

    /// The total length of the tree at `root`.
    pub fn sum(&self, root: u32) -> u64 {
        if root == NIL {
            0
        } else {
            self.nodes[root as usize].sum
        }
    }

    fn pull(&mut self, n: u32) {
        let (l, r) = (self.nodes[n as usize].left, self.nodes[n as usize].right);
        let s = self.sum(l) + self.sum(r) + self.nodes[n as usize].item.len();
        self.nodes[n as usize].sum = s;
        if l != NIL {
            self.nodes[l as usize].parent = n;
        }
        if r != NIL {
            self.nodes[r as usize].parent = n;
        }
    }

    /// Replaces the item of node `n`, whose length may change, and corrects
    /// the sums of its ancestors.
    pub fn set_item(&mut self, n: u32, item: T) {
        self.nodes[n as usize].item = item;
        let mut m = n;
        while m != NIL {
            let (l, r) = (self.nodes[m as usize].left, self.nodes[m as usize].right);
            self.nodes[m as usize].sum =
                self.sum(l) + self.sum(r) + self.nodes[m as usize].item.len();
            m = self.nodes[m as usize].parent;
        }
    }

    /// The root of the tree that holds node `n`.
    pub fn root_of(&self, n: u32) -> u32 {
        let mut m = n;
        while self.nodes[m as usize].parent != NIL {
            m = self.nodes[m as usize].parent;
        }
        m
    }

    /// Joins the trees at `a` and `b`, `a`'s items first.
    pub fn merge(&mut self, a: u32, b: u32) -> u32 {
        if a == NIL {
            if b != NIL {
                self.nodes[b as usize].parent = NIL;
            }
            return b;
        }
        if b == NIL {
            self.nodes[a as usize].parent = NIL;
            return a;
        }
        if self.nodes[a as usize].prio > self.nodes[b as usize].prio {
            let r = self.nodes[a as usize].right;
            let m = self.merge(r, b);
            self.nodes[a as usize].right = m;
            self.pull(a);
            self.nodes[a as usize].parent = NIL;
            a
        } else {
            let l = self.nodes[b as usize].left;
            let m = self.merge(a, l);
            self.nodes[b as usize].left = m;
            self.pull(b);
            self.nodes[b as usize].parent = NIL;
            b
        }
    }

    /// Splits the tree at `root` into the items before position `pos` and
    /// those from it on. `pos` must fall on a boundary between items.
    pub fn split(&mut self, root: u32, pos: u64) -> (u32, u32) {
        if root == NIL {
            return (NIL, NIL);
        }
        let l = self.nodes[root as usize].left;
        let ls = self.sum(l);
        let own = self.nodes[root as usize].item.len();
        if pos <= ls {
            let (a, b) = self.split(l, pos);
            self.nodes[root as usize].left = b;
            self.pull(root);
            self.nodes[root as usize].parent = NIL;
            if a != NIL {
                self.nodes[a as usize].parent = NIL;
            }
            (a, root)
        } else {
            debug_assert!(pos >= ls + own, "splitting inside an item");
            let r = self.nodes[root as usize].right;
            let (a, b) = self.split(r, pos - ls - own);
            self.nodes[root as usize].right = a;
            self.pull(root);
            self.nodes[root as usize].parent = NIL;
            if b != NIL {
                self.nodes[b as usize].parent = NIL;
            }
            (root, b)
        }
    }

    /// The node whose item holds position `pos` of the tree at `root`, and
    /// how far into the item `pos` lies; `None` past the end.
    pub fn find(&self, root: u32, pos: u64) -> Option<(u32, u64)> {
        let mut n = root;
        let mut pos = pos;
        while n != NIL {
            let node = &self.nodes[n as usize];
            let ls = self.sum(node.left);
            if pos < ls {
                n = node.left;
            } else if pos < ls + node.item.len() {
                return Some((n, pos - ls));
            } else {
                pos -= ls + node.item.len();
                n = node.right;
            }
        }
        None
    }

    /// Where the item of node `n` starts within its tree.
    pub fn pos_of(&self, n: u32) -> u64 {
        let mut pos = self.sum(self.nodes[n as usize].left);
        let mut m = n;
        loop {
            let p = self.nodes[m as usize].parent;
            if p == NIL {
                return pos;
            }
            if self.nodes[p as usize].right == m {
                pos += self.sum(self.nodes[p as usize].left) + self.nodes[p as usize].item.len();
            }
            m = p;
        }
    }

    /// The first node of the tree at `root`, in order.
    pub fn first(&self, root: u32) -> u32 {
        let mut n = root;
        if n == NIL {
            return NIL;
        }
        while self.nodes[n as usize].left != NIL {
            n = self.nodes[n as usize].left;
        }
        n
    }

    /// The last node of the tree at `root`, in order.
    pub fn last(&self, root: u32) -> u32 {
        let mut n = root;
        if n == NIL {
            return NIL;
        }
        while self.nodes[n as usize].right != NIL {
            n = self.nodes[n as usize].right;
        }
        n
    }

    /// The node after `n`, in order, or `NIL`.
    pub fn next(&self, n: u32) -> u32 {
        let r = self.nodes[n as usize].right;
        if r != NIL {
            return self.first(r);
        }
        let mut m = n;
        loop {
            let p = self.nodes[m as usize].parent;
            if p == NIL {
                return NIL;
            }
            if self.nodes[p as usize].left == m {
                return p;
            }
            m = p;
        }
    }

    /// The node before `n`, in order, or `NIL`.
    pub fn prev(&self, n: u32) -> u32 {
        let l = self.nodes[n as usize].left;
        if l != NIL {
            return self.last(l);
        }
        let mut m = n;
        loop {
            let p = self.nodes[m as usize].parent;
            if p == NIL {
                return NIL;
            }
            if self.nodes[p as usize].right == m {
                return p;
            }
            m = p;
        }
    }

    /// Splits the tree at `root` at position `pos`, cutting the item that
    /// holds it with `cut`, which returns the item's two parts; returns the
    /// two trees, and the node of the cut item's right part, if an item was
    /// cut.
    pub fn split_at(
        &mut self,
        root: u32,
        pos: u64,
        cut: impl FnOnce(&T, u64) -> (T, T),
    ) -> (u32, u32) {
        match self.find(root, pos) {
            Some((n, off)) if off > 0 => {
                let (a, b) = cut(&self.nodes[n as usize].item, off);
                self.set_item(n, a);
                let root = self.root_of(n);
                let (l, r) = self.split(root, pos);
                let m = self.node(b);
                (l, self.merge(m, r))
            }
            _ => self.split(root, pos),
        }
    }

    /// The nodes of the tree at `root`, in order.
    pub fn nodes(&self, root: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut stack = Vec::new();
        let mut n = root;
        while n != NIL || !stack.is_empty() {
            while n != NIL {
                stack.push(n);
                n = self.nodes[n as usize].left;
            }
            let m = stack.pop().unwrap();
            out.push(m);
            n = self.nodes[m as usize].right;
        }
        out
    }

    /// Checks the sums and parent links of the tree at `root`, panicking at
    /// the first violation.
    pub fn check(&self, root: u32) {
        if root == NIL {
            return;
        }
        assert_eq!(self.nodes[root as usize].parent, NIL, "a root has a parent");
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            let node = &self.nodes[n as usize];
            assert_eq!(
                node.sum,
                self.sum(node.left) + self.sum(node.right) + node.item.len(),
                "node {n} has a wrong sum"
            );
            for c in [node.left, node.right] {
                if c != NIL {
                    assert_eq!(
                        self.nodes[c as usize].parent, n,
                        "node {c} has a wrong parent"
                    );
                    assert!(self.nodes[c as usize].prio <= node.prio, "heap order");
                    stack.push(c);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    struct Run {
        start: u64,
        len: u64,
    }

    impl Item for Run {
        fn len(&self) -> u64 {
            self.len
        }
    }

    fn cut(r: &Run, off: u64) -> (Run, Run) {
        (
            Run {
                start: r.start,
                len: off,
            },
            Run {
                start: r.start + off,
                len: r.len - off,
            },
        )
    }

    /// The bytes a tree of runs over `0..` describes.
    fn bytes(t: &Treap<Run>, root: u32) -> Vec<u64> {
        t.nodes(root)
            .into_iter()
            .flat_map(|n| {
                let r = *t.item(n);
                r.start..r.start + r.len
            })
            .collect()
    }

    #[test]
    fn splits_moves_and_finds_agree_with_a_vector() {
        let mut rng = 0x2545_F491_4F6C_DD1Du64;
        let mut next = |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n.max(1)
        };
        let mut t: Treap<Run> = Treap::new();
        let mut root = t.node(Run {
            start: 0,
            len: 1000,
        });
        let mut model: Vec<u64> = (0..1000).collect();
        let mut fresh = 1000u64;
        for _ in 0..2000 {
            let len = model.len() as u64;
            match next(3) {
                // A move of a run elsewhere.
                0 if len > 0 => {
                    let from = next(len);
                    let n = next(len - from) + 1;
                    let (a, rest) = t.split_at(root, from, cut);
                    let (m, b) = t.split_at(rest, n, cut);
                    let r = t.merge(a, b);
                    let to = next(len - n + 1);
                    let (x, y) = t.split_at(r, to, cut);
                    let xm = t.merge(x, m);
                    root = t.merge(xm, y);
                    let moved: Vec<u64> = model.drain(from as usize..(from + n) as usize).collect();
                    model.splice(to as usize..to as usize, moved);
                }
                // A removal.
                1 if len > 0 => {
                    let from = next(len);
                    let n = next(len - from) + 1;
                    let (a, rest) = t.split_at(root, from, cut);
                    let (m, b) = t.split_at(rest, n, cut);
                    t.free_tree(m);
                    root = t.merge(a, b);
                    model.drain(from as usize..(from + n) as usize);
                }
                // An insertion.
                _ => {
                    let at = next(len + 1);
                    let n = next(50) + 1;
                    let (a, b) = t.split_at(root, at, cut);
                    let m = t.node(Run {
                        start: fresh,
                        len: n,
                    });
                    let am = t.merge(a, m);
                    root = t.merge(am, b);
                    model.splice(at as usize..at as usize, fresh..fresh + n);
                    fresh += n;
                }
            }
            t.check(root);
            assert_eq!(t.sum(root), model.len() as u64);
            if !model.is_empty() {
                let p = next(model.len() as u64);
                let (n, off) = t.find(root, p).unwrap();
                assert_eq!(t.item(n).start + off, model[p as usize]);
                assert_eq!(t.pos_of(n) + off, p);
            }
        }
        assert_eq!(bytes(&t, root), model);
        // Walking by `next` and `prev` visits the same nodes.
        let order = t.nodes(root);
        let mut n = t.first(root);
        for &m in &order {
            assert_eq!(n, m);
            n = t.next(n);
        }
        assert_eq!(n, NIL);
        let mut n = t.last(root);
        for &m in order.iter().rev() {
            assert_eq!(n, m);
            n = t.prev(n);
        }
        assert_eq!(n, NIL);
    }

    #[test]
    fn nodes_are_reused() {
        let mut t: Treap<Run> = Treap::new();
        let a = t.node(Run { start: 0, len: 3 });
        let b = t.node(Run { start: 3, len: 4 });
        let r = t.merge(a, b);
        assert_eq!(t.live_nodes(), 2);
        t.free_tree(r);
        assert_eq!(t.live_nodes(), 0);
        let c = t.node(Run { start: 9, len: 1 });
        assert!(c == a || c == b);
    }
}
