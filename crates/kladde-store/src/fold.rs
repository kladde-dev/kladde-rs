//! Phase A of the flush (`impl/flush.md#phase-a--the-fold`): the net effect
//! of a journal segment, per id, as a piece table over the id's final bytes.

use std::collections::BTreeMap;

use crate::journal::{Record, Records};

/// Where a piece's bytes come from, offset-relative so that it can advance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Bytes in the records' arena, from this position.
    Literal(u64),
    /// The committed content of `id`, from this offset.
    Storage(u32, u32),
    /// Zeros.
    Zero,
}

impl Source {
    fn advanced(self, d: u32) -> Source {
        match self {
            Source::Literal(p) => Source::Literal(p + d as u64),
            Source::Storage(id, o) => Source::Storage(id, o + d),
            Source::Zero => Source::Zero,
        }
    }

    fn continues(self, len: u32, next: Source) -> bool {
        match (self, next) {
            (Source::Literal(a), Source::Literal(b)) => a + len as u64 == b,
            (Source::Storage(i, a), Source::Storage(j, b)) => i == j && a + len == b,
            (Source::Zero, Source::Zero) => true,
            _ => false,
        }
    }
}

/// A piece table: the source of every byte of `[0, size)`.
#[derive(Clone, Debug, Default)]
pub struct Table {
    /// Segment start -> source; the first key is 0 whenever `size > 0`.
    pub pieces: BTreeMap<u32, Source>,
    pub size: u32,
}

impl Table {
    fn persistent(id: u32, size: u32) -> Table {
        let mut pieces = BTreeMap::new();
        if size > 0 {
            pieces.insert(0, Source::Storage(id, 0));
        }
        Table { pieces, size }
    }

    /// The pieces covering `[start, end)`, clipped and made relative to
    /// `start`; bytes past the size read as zero.
    fn slice(&self, start: u32, end: u32) -> Vec<(u32, Source)> {
        let mut out = Vec::new();
        if start < self.size && start < end {
            let first = self
                .pieces
                .range(..=start)
                .next_back()
                .map(|(&k, &s)| (k, s))
                .unwrap();
            out.push((0, first.1.advanced(start - first.0)));
            let upper = end.min(self.size);
            if start + 1 < upper {
                for (&k, &s) in self.pieces.range(start + 1..upper) {
                    out.push((k - start, s));
                }
            }
        }
        if end > self.size.max(start) {
            let from = self.size.max(start) - start;
            if out.last().is_none_or(|(_, s)| *s != Source::Zero) {
                out.push((from, Source::Zero));
            }
        }
        out
    }

    /// Splits so that a piece starts at `at`.
    fn split(&mut self, at: u32) {
        if at >= self.size || self.pieces.contains_key(&at) {
            return;
        }
        let (k, s) = self
            .pieces
            .range(..at)
            .next_back()
            .map(|(&k, &s)| (k, s))
            .unwrap();
        self.pieces.insert(at, s.advanced(at - k));
    }

    /// Overwrites `[start, end)`, within the size, with `repl` (relative).
    fn overwrite(&mut self, start: u32, end: u32, repl: &[(u32, Source)]) {
        if start >= end {
            return;
        }
        self.split(start);
        self.split(end);
        let doomed: Vec<u32> = self.pieces.range(start..end).map(|(&k, _)| k).collect();
        for k in doomed {
            self.pieces.remove(&k);
        }
        for &(rel, s) in repl {
            if start + rel < end {
                self.pieces.insert(start + rel, s);
            }
        }
        self.coalesce_at(start);
        self.coalesce_at(end);
        for &(rel, _) in repl.iter().skip(1) {
            self.coalesce_at(start + rel);
        }
    }

    fn coalesce_at(&mut self, at: u32) {
        if at == 0 || at >= self.size {
            return;
        }
        let Some(&s) = self.pieces.get(&at) else {
            return;
        };
        let (k, p) = self
            .pieces
            .range(..at)
            .next_back()
            .map(|(&k, &p)| (k, p))
            .unwrap();
        if p.continues(at - k, s) {
            self.pieces.remove(&at);
        }
    }

    fn grow_to(&mut self, n: u32) {
        if n <= self.size {
            return;
        }
        let old = self.size;
        self.size = n;
        match self.pieces.range(..old).next_back() {
            Some((_, Source::Zero)) => {}
            _ => {
                self.pieces.insert(old, Source::Zero);
            }
        }
    }

    fn truncate(&mut self, n: u32) {
        if n >= self.size {
            return;
        }
        let doomed: Vec<u32> = self.pieces.range(n..).map(|(&k, _)| k).collect();
        for k in doomed {
            self.pieces.remove(&k);
        }
        self.size = n;
    }

    /// Replaces `[off, off + old_len)` with `new_len` bytes from `repl`,
    /// shifting the tail.
    fn splice(&mut self, off: u32, old_len: u32, repl: &[(u32, Source)], new_len: u32) {
        self.grow_to(off + old_len);
        self.split(off);
        self.split(off + old_len);
        let tail: Vec<(u32, Source)> = self
            .pieces
            .range(off + old_len..)
            .map(|(&k, &s)| (k, s))
            .collect();
        let doomed: Vec<u32> = self.pieces.range(off..).map(|(&k, _)| k).collect();
        for k in doomed {
            self.pieces.remove(&k);
        }
        for &(rel, s) in repl {
            self.pieces.insert(off + rel, s);
        }
        let new_size = self.size - old_len + new_len;
        for (k, s) in tail {
            self.pieces.insert(k - old_len + new_len, s);
        }
        self.size = new_size;
        if new_len > 0 {
            self.coalesce_at(off);
            self.coalesce_at(off + new_len);
        } else {
            self.coalesce_at(off);
        }
    }
}

/// The net effect of the segment on one id.
#[derive(Clone, Debug, Default)]
pub struct IdFold {
    /// It existed at the start of the segment.
    pub existed: bool,
    /// Its committed incarnation ended during the segment.
    pub ended: bool,
    /// It exists at the end of the segment.
    pub exists: bool,
    /// Its bytes at the end, when it exists.
    pub table: Table,
    /// The committed incarnation is still the current one.
    pub committed_live: bool,
    /// A record wrote its content or size.
    pub written: bool,
}

/// The fold of one segment.
#[derive(Debug, Default)]
pub struct Fold {
    pub ids: BTreeMap<u32, IdFold>,
}

impl Fold {
    fn entry<'a>(&'a mut self, id: u32, committed: &impl Fn(u32) -> Option<u32>) -> &'a mut IdFold {
        self.ids.entry(id).or_insert_with(|| match committed(id) {
            Some(size) => IdFold {
                existed: true,
                exists: true,
                committed_live: true,
                table: Table::persistent(id, size),
                ..Default::default()
            },
            None => IdFold::default(),
        })
    }

    /// Brings `id` into existence if it is not, as every record but `Free` does.
    fn bring(&mut self, id: u32, committed: &impl Fn(u32) -> Option<u32>) -> &mut IdFold {
        let f = self.entry(id, committed);
        if !f.exists {
            f.exists = true;
            f.table = Table::default();
        }
        f.written = true;
        f
    }

    /// `[start, start + len)` of `id` as it currently reads.
    fn read(
        &mut self,
        id: u32,
        start: u32,
        len: u32,
        committed: &impl Fn(u32) -> Option<u32>,
    ) -> Vec<(u32, Source)> {
        let end = start + len;
        match self.ids.get(&id) {
            Some(f) if f.exists => f.table.slice(start, end),
            Some(_) => vec![(0, Source::Zero)],
            None => match committed(id) {
                Some(size) => Table::persistent(id, size).slice(start, end),
                None => vec![(0, Source::Zero)],
            },
        }
    }
}

/// Folds `records` against the committed state, which `committed` describes:
/// the size of each existing id.
pub fn fold(records: &Records, committed: impl Fn(u32) -> Option<u32>) -> Fold {
    let mut f = Fold::default();
    for r in &records.records {
        match *r {
            Record::Free { id } => {
                let e = f.entry(id, &committed);
                if e.exists {
                    e.exists = false;
                    e.table = Table::default();
                    if e.committed_live {
                        e.committed_live = false;
                        e.ended = true;
                    }
                    e.written = true;
                }
            }
            Record::Resize { id, size } => {
                let e = f.bring(id, &committed);
                if size < e.table.size {
                    e.table.truncate(size);
                } else {
                    e.table.grow_to(size);
                }
            }
            Record::Write { id, offset, bytes } => {
                let e = f.bring(id, &committed);
                let end = offset + bytes.len;
                e.table.grow_to(end);
                if bytes.len > 0 {
                    e.table
                        .overwrite(offset, end, &[(0, Source::Literal(bytes.start))]);
                }
            }
            Record::Splice {
                id,
                offset,
                old_len,
                bytes,
            } => {
                let e = f.bring(id, &committed);
                let repl: Vec<(u32, Source)> = if bytes.len > 0 {
                    vec![(0, Source::Literal(bytes.start))]
                } else {
                    vec![]
                };
                e.table.splice(offset, old_len, &repl, bytes.len);
            }
            Record::Copy {
                src,
                src_offset,
                len,
                dst,
                dst_offset,
            }
            | Record::Move {
                src,
                src_offset,
                len,
                dst,
                dst_offset,
            } => {
                let is_move = matches!(r, Record::Move { .. });
                // The whole source range is read before anything is written.
                let pieces = f.read(src, src_offset, len, &committed);
                let src_size = match f.ids.get(&src) {
                    Some(e) if e.exists => Some(e.table.size),
                    Some(_) => None,
                    None => committed(src),
                };
                let e = f.bring(dst, &committed);
                e.table.grow_to(dst_offset + len);
                if len > 0 {
                    e.table.overwrite(dst_offset, dst_offset + len, &pieces);
                }
                if is_move && len > 0 {
                    if let Some(size) = src_size {
                        let (s, t) = (src_offset.min(size), (src_offset + len).min(size));
                        let d = (dst_offset, dst_offset + len);
                        // Zero what the destination did not just overwrite.
                        let mut zero = Vec::new();
                        if src == dst {
                            if s < d.0.min(t) {
                                zero.push((s, d.0.min(t)));
                            }
                            if d.1.max(s) < t {
                                zero.push((d.1.max(s), t));
                            }
                        } else if s < t {
                            zero.push((s, t));
                        }
                        let es = f.entry(src, &committed);
                        es.written = true;
                        for (a, b) in zero {
                            es.table.overwrite(a, b, &[(0, Source::Zero)]);
                        }
                    }
                }
            }
        }
    }
    f
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::journal::Bytes;
    use std::collections::HashMap;

    /// Replays records naively over byte vectors: the oracle.
    pub fn naive(records: &Records, mut state: HashMap<u32, Vec<u8>>) -> HashMap<u32, Vec<u8>> {
        for r in &records.records {
            match *r {
                Record::Free { id } => {
                    state.remove(&id);
                }
                Record::Resize { id, size } => {
                    state.entry(id).or_default().resize(size as usize, 0)
                }
                Record::Write { id, offset, bytes } => {
                    let v = state.entry(id).or_default();
                    let end = offset as usize + bytes.len as usize;
                    if v.len() < end {
                        v.resize(end, 0);
                    }
                    v[offset as usize..end].copy_from_slice(records.bytes(bytes));
                }
                Record::Splice {
                    id,
                    offset,
                    old_len,
                    bytes,
                } => {
                    let v = state.entry(id).or_default();
                    let end = (offset + old_len) as usize;
                    if v.len() < end {
                        v.resize(end, 0);
                    }
                    v.splice(offset as usize..end, records.bytes(bytes).iter().copied());
                }
                Record::Copy {
                    src,
                    src_offset,
                    len,
                    dst,
                    dst_offset,
                }
                | Record::Move {
                    src,
                    src_offset,
                    len,
                    dst,
                    dst_offset,
                } => {
                    let is_move = matches!(r, Record::Move { .. });
                    let from = state.get(&src).cloned();
                    let bytes: Vec<u8> = (0..len as usize)
                        .map(|i| {
                            from.as_ref()
                                .and_then(|v| v.get(src_offset as usize + i).copied())
                                .unwrap_or(0)
                        })
                        .collect();
                    let v = state.entry(dst).or_default();
                    let end = (dst_offset + len) as usize;
                    if v.len() < end {
                        v.resize(end, 0);
                    }
                    v[dst_offset as usize..end].copy_from_slice(&bytes);
                    if is_move {
                        if let Some(sv) = state.get_mut(&src) {
                            for i in src_offset as usize..(src_offset + len) as usize {
                                let in_dst = src == dst && i >= dst_offset as usize && i < end;
                                if i < sv.len() && !in_dst {
                                    sv[i] = 0;
                                }
                            }
                        }
                    }
                }
            }
        }
        state
    }

    /// Materializes a fold over `committed` bytes.
    pub fn materialize(
        f: &Fold,
        records: &Records,
        committed: &HashMap<u32, Vec<u8>>,
    ) -> HashMap<u32, Vec<u8>> {
        let mut out = committed.clone();
        for (&id, e) in &f.ids {
            if !e.exists {
                out.remove(&id);
                continue;
            }
            let mut v = vec![0u8; e.table.size as usize];
            let keys: Vec<(u32, Source)> = e.table.pieces.iter().map(|(&k, &s)| (k, s)).collect();
            for (i, &(k, s)) in keys.iter().enumerate() {
                let end = keys.get(i + 1).map_or(e.table.size, |n| n.0);
                for j in k..end {
                    v[j as usize] = match s.advanced(j - k) {
                        Source::Literal(p) => records.arena[p as usize],
                        Source::Storage(src, o) => committed
                            .get(&src)
                            .and_then(|b| b.get(o as usize).copied())
                            .unwrap_or(0),
                        Source::Zero => 0,
                    };
                }
            }
            out.insert(id, v);
        }
        out
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
    }

    #[test]
    fn the_fold_agrees_with_naive_replay() {
        for seed in 1..400u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let mut committed: HashMap<u32, Vec<u8>> = HashMap::new();
            for id in 1..4u32 {
                let len = rng.below(40) as usize;
                committed.insert(id, (0..len).map(|i| (id as u8) * 16 + i as u8).collect());
            }
            let mut recs = Records::new();
            for _ in 0..rng.below(12) + 1 {
                let id = 1 + rng.below(5) as u32;
                let other = 1 + rng.below(5) as u32;
                let rec = match rng.below(6) {
                    0 => Record::Free { id },
                    1 => Record::Resize {
                        id,
                        size: rng.below(50) as u32,
                    },
                    2 => {
                        let len = rng.below(10) as usize;
                        let fill = rng.below(200) as u8 + 1;
                        let bytes: Bytes = recs.stash(&vec![fill; len]);
                        Record::Write {
                            id,
                            offset: rng.below(40) as u32,
                            bytes,
                        }
                    }
                    3 => {
                        let len = rng.below(10) as usize;
                        let fill = rng.below(200) as u8 + 1;
                        let bytes = recs.stash(&vec![fill; len]);
                        Record::Splice {
                            id,
                            offset: rng.below(40) as u32,
                            old_len: rng.below(10) as u32,
                            bytes,
                        }
                    }
                    4 => Record::Copy {
                        src: other,
                        src_offset: rng.below(40) as u32,
                        len: rng.below(15) as u32,
                        dst: id,
                        dst_offset: rng.below(40) as u32,
                    },
                    _ => Record::Move {
                        src: other,
                        src_offset: rng.below(40) as u32,
                        len: rng.below(15) as u32,
                        dst: id,
                        dst_offset: rng.below(40) as u32,
                    },
                };
                recs.records.push(rec);
            }
            let want = naive(&recs, committed.clone());
            let f = fold(&recs, |id| committed.get(&id).map(|v| v.len() as u32));
            let got = materialize(&f, &recs, &committed);
            assert_eq!(got, want, "seed {seed}: {:?}", recs.records);
        }
    }
}
