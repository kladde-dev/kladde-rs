//! The fold: the journal's records applied to a piece table over the
//! sequence, which starts as one piece, the committed sequence, and ends as
//! the new sequence described by committed ranges and new bytes.
//!
//! The pieces sit in a counted tree, so each record costs `O(log p)` for the
//! `p` pieces the journal made, however long the sequence; a transaction's
//! cuts are kept as trees of their own until the transaction ends.

use crate::error::{corrupt, Error};
use crate::journal::Record;
use crate::treap::{Item, Treap, NIL};

/// Where a piece's bytes come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Src {
    /// The committed sequence, from this position on.
    Committed(u64),
    /// The journal buffer's arena, from this position on.
    Arena(u64),
}

impl Default for Src {
    fn default() -> Self {
        Src::Committed(0)
    }
}

/// A run of the new sequence and where its bytes come from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Piece {
    pub src: Src,
    pub len: u64,
}

impl Item for Piece {
    fn len(&self) -> u64 {
        self.len
    }
}

impl Piece {
    /// The two parts of the piece, `off` bytes into it.
    fn cut(&self, off: u64) -> (Piece, Piece) {
        let right = match self.src {
            Src::Committed(s) => Src::Committed(s + off),
            Src::Arena(s) => Src::Arena(s + off),
        };
        (
            Piece {
                src: self.src,
                len: off,
            },
            Piece {
                src: right,
                len: self.len - off,
            },
        )
    }

    /// Whether `next` continues this piece's bytes where they end.
    fn continued_by(&self, next: &Piece) -> bool {
        match (self.src, next.src) {
            (Src::Committed(a), Src::Committed(b)) | (Src::Arena(a), Src::Arena(b)) => {
                a + self.len == b
            }
            _ => false,
        }
    }
}

/// The piece table of one fold.
struct PieceTable {
    t: Treap<Piece>,
    root: u32,
}

impl PieceTable {
    fn len(&self) -> u64 {
        self.t.sum(self.root)
    }

    fn split(&mut self, root: u32, pos: u64) -> (u32, u32) {
        self.t.split_at(root, pos, |p, off| p.cut(off))
    }

    /// Takes `[pos, pos + n)` out, and returns it as a tree of its own.
    fn take(&mut self, pos: u64, n: u64) -> u32 {
        let (a, rest) = self.split(self.root, pos);
        let (m, b) = self.split(rest, n);
        self.root = self.t.merge(a, b);
        m
    }

    /// Inserts the tree `m` at `pos`.
    fn insert(&mut self, pos: u64, m: u32) {
        let (a, b) = self.split(self.root, pos);
        let left = self.t.merge(a, m);
        self.root = self.t.merge(left, b);
    }

    fn insert_piece(&mut self, pos: u64, p: Piece) {
        if p.len > 0 {
            let m = self.t.node(p);
            self.insert(pos, m);
        }
    }
}

fn past_end() -> Error {
    corrupt("a journal record reaches past the end of the sequence")
}

/// Applies `records`, each with whether a journal transaction starts at it,
/// to a sequence of `committed` bytes, and returns the new sequence as
/// pieces, adjacent pieces that continue each other merged.
///
/// Fails on a record whose range does not lie inside the sequence, or on a
/// paste of a cut its transaction did not make or already pasted.
pub fn fold<'a>(
    records: impl Iterator<Item = (bool, &'a Record)>,
    committed: u64,
) -> Result<Vec<Piece>, Error> {
    let mut pt = PieceTable {
        t: Treap::new(),
        root: NIL,
    };
    if committed > 0 {
        pt.root = pt.t.node(Piece {
            src: Src::Committed(0),
            len: committed,
        });
    }
    // The cuts of the current transaction, oldest first: their trees, or
    // `NIL` once pasted.
    let mut cuts: Vec<u32> = Vec::new();
    let end_transaction = |pt: &mut PieceTable, cuts: &mut Vec<u32>| {
        for c in cuts.drain(..) {
            pt.t.free_tree(c);
        }
    };
    for (start, r) in records {
        if start {
            end_transaction(&mut pt, &mut cuts);
        }
        let len = pt.len();
        match *r {
            Record::Write { pos, bytes } => {
                let n = bytes.len as u64;
                if pos.checked_add(n).is_none_or(|e| e > len) {
                    return Err(past_end());
                }
                let m = pt.take(pos, n);
                cuts.push(m);
                pt.insert_piece(
                    pos,
                    Piece {
                        src: Src::Arena(bytes.start),
                        len: n,
                    },
                );
            }
            Record::Splice {
                pos,
                removed,
                bytes,
            } => {
                if pos.checked_add(removed).is_none_or(|e| e > len) {
                    return Err(past_end());
                }
                if removed > 0 {
                    let m = pt.take(pos, removed);
                    cuts.push(m);
                }
                pt.insert_piece(
                    pos,
                    Piece {
                        src: Src::Arena(bytes.start),
                        len: bytes.len as u64,
                    },
                );
            }
            Record::Move { from, len: n, to } => {
                if from.checked_add(n).is_none_or(|e| e > len) || to > len - n {
                    return Err(past_end());
                }
                let m = pt.take(from, n);
                pt.insert(to, m);
            }
            Record::Paste {
                pos,
                removed,
                cut,
                len: n,
            } => {
                if pos.checked_add(removed).is_none_or(|e| e > len) {
                    return Err(past_end());
                }
                let Some(i) = cuts.len().checked_sub(1 + cut as usize) else {
                    return Err(corrupt("a paste names a cut its transaction did not make"));
                };
                let m = std::mem::replace(&mut cuts[i], NIL);
                if m == NIL && n > 0 {
                    return Err(corrupt("a paste names a cut already pasted"));
                }
                if pt.t.sum(m) != n {
                    return Err(corrupt("a paste's cut has another length"));
                }
                let dropped = pt.take(pos, removed);
                pt.t.free_tree(dropped);
                pt.insert(pos, m);
            }
        }
    }
    end_transaction(&mut pt, &mut cuts);
    let mut out: Vec<Piece> = Vec::new();
    for n in pt.t.nodes(pt.root) {
        let p = *pt.t.item(n);
        match out.last_mut() {
            Some(last) if last.continued_by(&p) => last.len += p.len,
            _ => out.push(p),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Records;

    /// The bytes the pieces describe, committed bytes read from `old` and
    /// new ones from the arena.
    fn apply(pieces: &[Piece], old: &[u8], recs: &Records) -> Vec<u8> {
        let mut out = Vec::new();
        for p in pieces {
            match p.src {
                Src::Committed(s) => out.extend_from_slice(&old[s as usize..(s + p.len) as usize]),
                Src::Arena(s) => {
                    let mut b = vec![0; p.len as usize];
                    recs.copy_bytes(s, &mut b);
                    out.extend(b);
                }
            }
        }
        out
    }

    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n.max(1)
        }
    }

    #[test]
    fn the_fold_agrees_with_a_vector() {
        for seed in 1..40u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let old: Vec<u8> = (0..rng.below(300)).map(|i| i as u8).collect();
            let mut model = old.clone();
            let mut recs = Records::new();
            let mut cuts: Vec<Option<Vec<u8>>> = Vec::new();
            for _ in 0..200 {
                if rng.below(5) == 0 {
                    recs.mark_start(recs.end().at);
                    cuts.clear();
                }
                let len = model.len() as u64;
                let r = match rng.below(4) {
                    0 if len > 0 => {
                        let pos = rng.below(len);
                        let n = rng.below(len - pos) + 1;
                        let new: Vec<u8> = (0..n).map(|_| rng.below(256) as u8).collect();
                        let cut = model[pos as usize..(pos + n) as usize].to_vec();
                        model[pos as usize..(pos + n) as usize].copy_from_slice(&new);
                        cuts.push(Some(cut));
                        Record::Write {
                            pos,
                            bytes: recs.stash(&new),
                        }
                    }
                    1 => {
                        let pos = rng.below(len + 1);
                        let removed = rng.below(len - pos + 1);
                        let new: Vec<u8> =
                            (0..rng.below(20)).map(|_| rng.below(256) as u8).collect();
                        let cut: Vec<u8> = model
                            .splice(pos as usize..(pos + removed) as usize, new.iter().copied())
                            .collect();
                        if removed > 0 {
                            cuts.push(Some(cut));
                        }
                        Record::Splice {
                            pos,
                            removed,
                            bytes: recs.stash(&new),
                        }
                    }
                    2 if len > 0 => {
                        let from = rng.below(len);
                        let n = rng.below(len - from) + 1;
                        let to = rng.below(len - n + 1);
                        let m: Vec<u8> = model.drain(from as usize..(from + n) as usize).collect();
                        model.splice(to as usize..to as usize, m);
                        Record::Move { from, len: n, to }
                    }
                    _ => match cuts.iter().rposition(|c| c.is_some()) {
                        Some(i) => {
                            let cut = cuts[i].take().unwrap();
                            let pos = rng.below(len + 1);
                            let removed = rng.below(len - pos + 1);
                            let n = cut.len() as u64;
                            model.splice(pos as usize..(pos + removed) as usize, cut);
                            Record::Paste {
                                pos,
                                removed,
                                cut: (cuts.len() - 1 - i) as u32,
                                len: n,
                            }
                        }
                        None => continue,
                    },
                };
                recs.push(r);
            }
            let pieces = fold(recs.range(recs.front(), recs.end()), old.len() as u64).unwrap();
            assert_eq!(apply(&pieces, &old, &recs), model, "seed {seed}");
            // Adjacent pieces never continue each other.
            for w in pieces.windows(2) {
                assert!(!w[0].continued_by(&w[1]));
            }
        }
    }

    #[test]
    fn a_record_past_the_end_is_refused() {
        let mut recs = Records::new();
        let bytes = recs.stash(b"ab");
        recs.push(Record::Write { pos: 9, bytes });
        assert!(fold(recs.range(recs.front(), recs.end()), 10).is_err());
        assert!(fold(recs.range(recs.front(), recs.end()), 11).is_ok());
    }

    #[test]
    fn a_cut_from_another_transaction_is_refused() {
        let mut recs = Records::new();
        recs.mark_start(0);
        let bytes = recs.stash(b"");
        recs.push(Record::Splice {
            pos: 0,
            removed: 2,
            bytes,
        });
        recs.mark_start(1);
        recs.push(Record::Paste {
            pos: 0,
            removed: 0,
            cut: 0,
            len: 2,
        });
        assert!(fold(recs.range(recs.front(), recs.end()), 5).is_err());
    }
}
