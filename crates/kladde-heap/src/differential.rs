//! Differential testing of the backends against a pure in-memory model.
//!
//! `journal-semantics.md` §8: the flush path is a small optimizing compiler, so
//! the oracle comes before the optimizer. Every later step of §10 is then a
//! *validated rewrite* rather than a hopeful one.
//!
//! ## What is compared
//!
//! The design note says "compare `heap.iter()` plus every live allocation's
//! bytes". `heap.iter()` turns out to be too strong: **addresses are not
//! observable** through the backend API (`size`, `resolve`, `read_at` all take an
//! id), and two correct implementations legitimately place things differently --
//! the naive replayer claims in log order while an optimizing flush claims
//! first-fit-decreasing. Comparing addresses would reject the optimization it is
//! meant to validate. So the compared surface is the observable one: which
//! allocations are live, how big each is, and what bytes it holds.
//!
//! Uninitialized bytes are excluded, because §2.1 makes them unconstrained: the
//! model tracks content as `Option<u8>` and only the `Some` positions are checked.
//! Without that mask the oracle would reject legal schedules -- a shrink followed
//! by a grow may leave the old bytes in place or not, at the implementation's
//! discretion.
//!
//! ## Why a model rather than one backend against the other
//!
//! A third, obviously-correct implementation catches bugs *both* backends share.
//! It also sidesteps the id-divergence problem: because the journaled backend
//! defers frees, its id counters recycle at different moments than the
//! unjournaled one's, so the same action sequence produces different ids. Actions
//! therefore name model-level handles, and each runner keeps its own
//! `handle -> backend id` map.

use std::collections::BTreeMap;
use std::io::Read;

use crate::backend::{ReadBackend, WriteBackend};
use crate::gain_greedy::GainGreedyHeap;
use crate::journaled::JournaledWriteBackend;
use crate::pointer::{Pointer, UniquePointerFixedSize, UniquePointerResizable};
use crate::storage::InMemoryStorage;
use crate::unjournaled::UnjournaledBackend;

type Id = Pointer<u32>;
type Heap = GainGreedyHeap<Id>;

/// A model-level allocation name. Stable across a sizedness conversion, which the
/// backend id is not.
type Handle = u32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    Alloc {
        handle: Handle,
        size: u32,
        fixed: bool,
    },
    Free {
        handle: Handle,
    },
    Resize {
        handle: Handle,
        size: u32,
    },
    /// `make_resizable` when the allocation is currently fixed, `make_fixed_size`
    /// when it is currently resizable. Which one is implied by the state, so the
    /// action does not carry it and shrinking cannot make it inconsistent.
    ChangeSizedness {
        handle: Handle,
        size: u32,
    },
    Write {
        handle: Handle,
        offset: u32,
        bytes: Vec<u8>,
    },
    Splice {
        handle: Handle,
        offset: u32,
        old_len: u32,
        bytes: Vec<u8>,
    },
}

impl Action {
    fn handle(&self) -> Handle {
        match self {
            Action::Alloc { handle, .. }
            | Action::Free { handle }
            | Action::Resize { handle, .. }
            | Action::ChangeSizedness { handle, .. }
            | Action::Write { handle, .. }
            | Action::Splice { handle, .. } => *handle,
        }
    }
}

/// The reference implementation: a byte vector per live handle, where `None`
/// marks a position whose content the contract does not pin down (§2.1).
#[derive(Debug, Default, Clone)]
pub(crate) struct Model {
    live: BTreeMap<Handle, Alloc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Alloc {
    fixed: bool,
    content: Vec<Option<u8>>,
}

impl Model {
    /// Apply `action` if it is legal in the current state, reporting whether it
    /// was. Illegal actions are *dropped*, not rejected, which is what lets any
    /// `Vec<Action>` be canonicalized into a runnable one -- see [`sanitize`].
    fn apply(&mut self, action: &Action) -> bool {
        match action {
            Action::Alloc {
                handle,
                size,
                fixed,
            } => {
                if self.live.contains_key(handle) {
                    return false;
                }
                self.live.insert(
                    *handle,
                    Alloc {
                        fixed: *fixed,
                        content: vec![None; *size as usize],
                    },
                );
            }
            Action::Free { handle } => {
                if self.live.remove(handle).is_none() {
                    return false;
                }
            }
            Action::Resize { handle, size } => {
                let Some(a) = self.live.get_mut(handle) else {
                    return false;
                };
                // Resizing a fixed-size allocation is not expressible in the API:
                // `resize` takes a `&UniquePointerResizable`.
                if a.fixed {
                    return false;
                }
                a.content.resize(*size as usize, None);
            }
            Action::ChangeSizedness { handle, size } => {
                let Some(a) = self.live.get_mut(handle) else {
                    return false;
                };
                let keep = a.content.len().min(*size as usize);
                a.content.truncate(keep);
                a.content.resize(*size as usize, None);
                a.fixed = !a.fixed;
            }
            Action::Write {
                handle,
                offset,
                bytes,
            } => {
                let Some(a) = self.live.get_mut(handle) else {
                    return false;
                };
                let end = *offset as usize + bytes.len();
                if end > a.content.len() {
                    return false;
                }
                for (slot, b) in a.content[*offset as usize..end].iter_mut().zip(bytes) {
                    *slot = Some(*b);
                }
            }
            Action::Splice {
                handle,
                offset,
                old_len,
                bytes,
            } => {
                let Some(a) = self.live.get_mut(handle) else {
                    return false;
                };
                if a.fixed {
                    return false;
                }
                let tail_start = *offset as usize + *old_len as usize;
                if tail_start > a.content.len() {
                    return false;
                }
                let replacement: Vec<Option<u8>> = bytes.iter().map(|b| Some(*b)).collect();
                a.content
                    .splice(*offset as usize..tail_start, replacement);
            }
        }
        true
    }

    fn snapshot(&self) -> BTreeMap<Handle, (bool, Vec<Option<u8>>)> {
        self.live
            .iter()
            .map(|(h, a)| (*h, (a.fixed, a.content.clone())))
            .collect()
    }
}

/// Drop the actions that are not legal where they sit, yielding a sequence that
/// runs cleanly against every implementation.
///
/// This is what makes shrinking easy: *any* subsequence of a failing case can be
/// handed straight back in, and sanitizing repairs the dangling references that
/// deleting an `Alloc` would otherwise leave behind.
pub(crate) fn sanitize(actions: &[Action]) -> Vec<Action> {
    let mut model = Model::default();
    let mut kept = Vec::with_capacity(actions.len());
    for action in actions {
        if model.apply(action) {
            kept.push(action.clone());
        }
    }
    kept
}

/// The backend-side handle for one model handle. Owned, because `free` and the
/// sizedness conversions consume it.
enum Owned {
    Fixed(UniquePointerFixedSize<Id>),
    Resizable(UniquePointerResizable<Id>),
}

impl Owned {
    fn raw(&self) -> Id {
        match self {
            Owned::Fixed(p) => p.raw(),
            Owned::Resizable(p) => p.raw(),
        }
    }
}

/// Run `actions` against a `WriteBackend`, then read every live allocation back
/// through a `ReadBackend`.
///
/// Split into two closures rather than one generic function because the journaled
/// backend's write and read halves are *different types* -- that is the whole
/// point of the phase split.
fn run<WB, RB>(
    actions: &[Action],
    backend: WB,
    finish: impl FnOnce(WB) -> RB,
) -> BTreeMap<Handle, (bool, Vec<u8>)>
where
    WB: WriteBackend<Pointer = Id, Size = u32>,
    RB: ReadBackend<Pointer = Id, Size = u32>,
{
    let mut owned: BTreeMap<Handle, Owned> = BTreeMap::new();

    for action in actions {
        match action {
            Action::Alloc {
                handle,
                size,
                fixed,
            } => {
                let p = if *fixed {
                    Owned::Fixed(backend.alloc_fixed_size(*size))
                } else {
                    Owned::Resizable(backend.alloc_resizable(*size))
                };
                owned.insert(*handle, p);
            }
            Action::Free { handle } => match owned.remove(handle).expect("sanitized") {
                Owned::Fixed(p) => backend.free_fixed_size(p),
                Owned::Resizable(p) => backend.free_resizable(p),
            },
            Action::Resize { handle, size } => {
                let Owned::Resizable(p) = owned.get(handle).expect("sanitized") else {
                    unreachable!("sanitized: resize only reaches resizable allocations")
                };
                backend.resize(p, *size).expect("resize of a live id");
            }
            Action::ChangeSizedness { handle, size } => {
                let next = match owned.remove(handle).expect("sanitized") {
                    Owned::Fixed(p) => Owned::Resizable(
                        backend.make_resizable(p, *size).expect("live id"),
                    ),
                    Owned::Resizable(p) => Owned::Fixed(
                        backend.make_fixed_size(p, *size).expect("live id"),
                    ),
                };
                owned.insert(*handle, next);
            }
            Action::Write {
                handle,
                offset,
                bytes,
            } => backend.write(owned.get(handle).expect("sanitized").raw(), *offset, bytes),
            Action::Splice {
                handle,
                offset,
                old_len,
                bytes,
            } => {
                let Owned::Resizable(p) = owned.get(handle).expect("sanitized") else {
                    unreachable!("sanitized: splice only reaches resizable allocations")
                };
                backend.splice(p, *offset, *old_len, bytes);
            }
        }
    }

    let ids: Vec<(Handle, Id, bool)> = owned
        .iter()
        .map(|(h, p)| (*h, p.raw(), matches!(p, Owned::Fixed(_))))
        .collect();
    let mut reader = finish(backend);
    ids.into_iter()
        .map(|(h, id, fixed)| {
            let size = reader.size(id).expect("live id has a size") as usize;
            let mut buf = vec![0u8; size];
            reader.read_at(id, 0).read_exact(&mut buf).expect("read back");
            (h, (fixed, buf))
        })
        .collect()
}

/// Compare one implementation's result against the model, ignoring positions the
/// contract leaves undefined.
fn agrees(
    what: &str,
    actual: &BTreeMap<Handle, (bool, Vec<u8>)>,
    model: &BTreeMap<Handle, (bool, Vec<Option<u8>>)>,
    actions: &[Action],
) {
    let live_actual: Vec<Handle> = actual.keys().copied().collect();
    let live_model: Vec<Handle> = model.keys().copied().collect();
    assert_eq!(live_actual, live_model, "{what}: live set\n{actions:#?}");

    for (handle, (fixed, bytes)) in actual {
        let (want_fixed, want) = &model[handle];
        assert_eq!(fixed, want_fixed, "{what}: sizedness of {handle}");
        assert_eq!(
            bytes.len(),
            want.len(),
            "{what}: size of {handle}\n{actions:#?}",
        );
        for (i, expected) in want.iter().enumerate() {
            if let Some(e) = expected {
                assert_eq!(
                    bytes[i], *e,
                    "{what}: byte {i} of {handle}\n{actions:#?}",
                );
            }
        }
    }
}

/// Run one case through the model and both backends. Panics with the offending
/// action sequence on any disagreement.
pub(crate) fn check(actions: &[Action]) {
    let actions = sanitize(actions);

    let mut model = Model::default();
    for a in &actions {
        assert!(model.apply(a), "sanitized sequences always apply");
    }
    let expected = model.snapshot();

    let journaled = run(
        &actions,
        JournaledWriteBackend::<_, Heap>::new(InMemoryStorage::default(), GainGreedyHeap::new()),
        |b| b.flush(),
    );
    agrees("journaled (folded)", &journaled, &expected, &actions);

    // The reference implementation §8 calls for: same log, one `Composed` call
    // per op, no folding and no reordering. Comparing it too is what makes the
    // fold a *validated* rewrite rather than a hopeful one.
    let naive = run(
        &actions,
        JournaledWriteBackend::<_, Heap>::new(InMemoryStorage::default(), GainGreedyHeap::new()),
        |b| b.flush_naively(),
    );
    agrees("journaled (naive)", &naive, &expected, &actions);

    let unjournaled = run(
        &actions,
        UnjournaledBackend::<_, Heap>::new(InMemoryStorage::default(), GainGreedyHeap::new()),
        |b| b,
    );
    agrees("unjournaled", &unjournaled, &expected, &actions);
}

/// Whether `actions` still fails, for the shrinker.
fn fails(actions: &[Action]) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(actions))).is_err()
}

/// Reduce a failing case to a locally minimal one.
///
/// Deliberately hand-rolled rather than delegating to a property-testing crate: an
/// action sequence has internal invariants (a `Write` must reference a live
/// handle, its range must be in bounds), so a generic shrinker would spend its
/// budget proposing sequences that are merely invalid. [`sanitize`] gives those
/// invariants for free, which makes "any subsequence" a legal proposal and the
/// shrinker four obvious reductions long.
pub(crate) fn shrink(actions: &[Action]) -> Vec<Action> {
    let mut best = sanitize(actions);
    loop {
        let mut improved = false;

        // Delete one action at a time; `sanitize` repairs whatever depended on it.
        let mut i = 0;
        while i < best.len() {
            let mut candidate = best.clone();
            candidate.remove(i);
            let candidate = sanitize(&candidate);
            if candidate.len() < best.len() && fails(&candidate) {
                best = candidate;
                improved = true;
            } else {
                i += 1;
            }
        }

        // Then shrink what survives: sizes, offsets, payloads.
        for i in 0..best.len() {
            for candidate in reductions(&best, i) {
                let candidate = sanitize(&candidate);
                if candidate != best && fails(&candidate) {
                    best = candidate;
                    improved = true;
                    break;
                }
            }
        }

        if !improved {
            return best;
        }
    }
}

fn reductions(actions: &[Action], i: usize) -> Vec<Vec<Action>> {
    let mut out = Vec::new();
    let mut push = |a: Action| {
        let mut c = actions.to_vec();
        c[i] = a;
        out.push(c);
    };
    match &actions[i] {
        Action::Alloc {
            handle,
            size,
            fixed,
        } if *size > 1 => push(Action::Alloc {
            handle: *handle,
            size: size / 2,
            fixed: *fixed,
        }),
        Action::Resize { handle, size } if *size > 1 => push(Action::Resize {
            handle: *handle,
            size: size / 2,
        }),
        Action::ChangeSizedness { handle, size } if *size > 1 => push(Action::ChangeSizedness {
            handle: *handle,
            size: size / 2,
        }),
        Action::Write {
            handle,
            offset,
            bytes,
        } => {
            if *offset > 0 {
                push(Action::Write {
                    handle: *handle,
                    offset: 0,
                    bytes: bytes.clone(),
                });
            }
            if bytes.len() > 1 {
                push(Action::Write {
                    handle: *handle,
                    offset: *offset,
                    bytes: bytes[..bytes.len() / 2].to_vec(),
                });
            }
        }
        Action::Splice {
            handle,
            offset,
            old_len,
            bytes,
        } => {
            if *offset > 0 {
                push(Action::Splice {
                    handle: *handle,
                    offset: 0,
                    old_len: *old_len,
                    bytes: bytes.clone(),
                });
            }
            if bytes.len() > 1 {
                push(Action::Splice {
                    handle: *handle,
                    offset: *offset,
                    old_len: *old_len,
                    bytes: bytes[..bytes.len() / 2].to_vec(),
                });
            }
        }
        _ => {}
    }
    out
}

/// Deterministic xorshift, matching what the rest of this crate's tests use.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Generate a random action sequence. Not guaranteed valid -- [`sanitize`] is what
/// makes it runnable, and generating loosely keeps the generator simple.
pub(crate) fn generate(rng: &mut Rng, len: usize, handles: u32) -> Vec<Action> {
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        let handle = rng.below(u64::from(handles)) as u32;
        // Small and lumpy, so size classes and exact fits come up often. Zero is
        // excluded: `GainGreedyHeap` keys allocations by address and assumes a
        // positive extent, so a zero-sized one is outside its model (see
        // `later.md`).
        let size = [1u32, 2, 3, 8, 16, 17, 64][rng.below(7) as usize];
        out.push(match rng.below(10) {
            0..=2 => Action::Alloc {
                handle,
                size,
                fixed: rng.below(2) == 0,
            },
            3 => Action::Free { handle },
            4 => Action::Resize { handle, size },
            5 => Action::ChangeSizedness { handle, size },
            6 => Action::Splice {
                handle,
                offset: rng.below(8) as u32,
                old_len: rng.below(8) as u32,
                bytes: payload(rng),
            },
            _ => Action::Write {
                handle,
                offset: rng.below(8) as u32,
                bytes: payload(rng),
            },
        });
    }
    out
}

fn payload(rng: &mut Rng) -> Vec<u8> {
    let len = rng.below(9) as usize;
    (0..len).map(|_| (rng.next() & 0xff) as u8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The oracle must be able to fail. Without this, a `check` that silently
    /// compared nothing would look like a passing test suite.
    #[test]
    #[should_panic(expected = "byte 0 of 0")]
    fn the_oracle_catches_a_planted_disagreement() {
        let mut model = Model::default();
        let actions = [
            Action::Alloc {
                handle: 0,
                size: 4,
                fixed: true,
            },
            Action::Write {
                handle: 0,
                offset: 0,
                bytes: vec![1, 2, 3, 4],
            },
        ];
        for a in &actions {
            model.apply(a);
        }
        let mut wrong = model.snapshot();
        wrong.get_mut(&0).unwrap().1[0] = Some(0xFF);

        let journaled = run(
            &actions,
            JournaledWriteBackend::<_, Heap>::new(
                InMemoryStorage::default(),
                GainGreedyHeap::new(),
            ),
            |b| b.flush(),
        );
        agrees("planted", &journaled, &wrong, &actions);
    }

    #[test]
    fn randomized_sequences_agree_with_the_model() {
        for seed in 1..200u64 {
            let mut rng = Rng::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let actions = generate(&mut rng, 40, 6);
            if fails(&actions) {
                let minimal = shrink(&actions);
                panic!("seed {seed} disagrees; minimal case:\n{minimal:#?}");
            }
        }
    }

    #[test]
    fn long_sequences_over_few_handles_agree() {
        // Few handles, many actions: maximizes churn on the same ids, which is
        // where the fold's annihilation rules and the id-recycling discipline get
        // stressed.
        for seed in 1..40u64 {
            let mut rng = Rng::new(seed.wrapping_mul(0xD1B5_4A32_D192_ED03));
            let actions = generate(&mut rng, 150, 3);
            if fails(&actions) {
                let minimal = shrink(&actions);
                panic!("seed {seed} disagrees; minimal case:\n{minimal:#?}");
            }
        }
    }

    #[test]
    fn the_shrinker_reduces_a_failing_case() {
        // A sequence that "fails" by construction: the predicate is a stand-in for
        // a real disagreement, so this tests the shrinker itself rather than the
        // backends.
        let actions = sanitize(&generate(&mut Rng::new(7), 60, 4));
        let interesting = |a: &[Action]| a.iter().filter(|x| x.handle() == 2).count() >= 2;
        assert!(interesting(&actions), "the seed must exercise handle 2");

        // Hand-roll the same loop `shrink` runs, against this predicate.
        let mut best = actions;
        loop {
            let mut improved = false;
            let mut i = 0;
            while i < best.len() {
                let mut c = best.clone();
                c.remove(i);
                let c = sanitize(&c);
                if c.len() < best.len() && interesting(&c) {
                    best = c;
                    improved = true;
                } else {
                    i += 1;
                }
            }
            if !improved {
                break;
            }
        }
        assert_eq!(best.len(), 2, "reduced to just the two interesting actions");
    }
}
