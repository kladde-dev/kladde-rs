//! Phase B of the flush optimizer: order the emitted actions.
//!
//! `journal-semantics.md` §5. The vertices are the actions the fold produced, not
//! the log's ops, and the edges are the orderings correctness actually requires.
//! Everything else is a *preference*, expressed as a priority over the ready set
//! rather than as an edge -- which matters, because the obvious preference
//! (release before claim, so the placement pass has more room) would close a
//! cycle if it were an edge.
//!
//! ## How much this does, and when
//!
//! With read hoisting on (§6.5) every cross-id read has already been resolved
//! into a literal, so no `Release` is ever blocked and the graph degenerates to
//! "a write into an allocation follows the action that places it". The schedule
//! is then exactly the phase order plus **first-fit-decreasing claims**, which is
//! the part that survives.
//!
//! The edges earn their keep when hoisting is off -- which
//! [`JournaledWriteBackend::set_hoisting`] allows, since §6.5 notes hoisting's
//! cost is memory proportional to the volume of disturbed reads and a workload
//! may prefer to pay in ordering instead. That switch is also what makes these
//! edges testable rather than vestigial.

use crate::fold::{Folded, Source, Task};
use crate::pointer::Pointer;
use crate::word::Word;

/// Order `tasks` so that every edge points forwards, preferring cheap-to-place
/// orderings among the tasks that are ready at each step.
///
/// Kahn's algorithm with a priority queue. The priority is §5.2's:
///
/// 1. anything that unblocks a `Release`,
/// 2. `Release`,
/// 3. `Claim`, largest first,
/// 4. everything else.
///
/// First-fit-decreasing is therefore *best-effort over the ready set* rather than
/// global: a claim blocked behind a transfer is placed after smaller ones. Only
/// cross-id reads can block one, so with hoisting on it is exactly global.
pub(crate) fn schedule<W: Word, S: Word>(
    folded: &Folded<W, S>,
    survivors: &[(Pointer<W>, S)],
) -> Vec<Task<W, S>> {
    let tasks = folded.tasks(survivors);
    let n = tasks.len();

    // Edge 1: a write into an allocation follows whatever places it.
    // Edge 2: a write that *reads* an allocation precedes its release.
    //
    // Edges 3 and 4 of §5.1 (the WAR anti-dependency, and an earlier write
    // feeding a later read) cannot arise here: the fold has already resolved a
    // read against whatever the table held at that point, so a piece never names
    // storage that a *later* task in this same flush rewrites -- hoisting
    // resolves exactly that set, and without hoisting such a read is left naming
    // the pre-flush bytes, which is what it meant.
    let mut successors: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut indegree = vec![0usize; n];
    let edge = |from: usize, to: usize, succ: &mut Vec<Vec<usize>>, deg: &mut Vec<usize>| {
        if from != to && !succ[from].contains(&to) {
            succ[from].push(to);
            deg[to] += 1;
        }
    };

    for (i, task) in tasks.iter().enumerate() {
        let Task::Write { id, pieces, .. } = task else {
            continue;
        };
        for (j, other) in tasks.iter().enumerate() {
            match other {
                Task::Claim(o, _) | Task::Reshape(o, _) | Task::Relabel { to: o, .. }
                    if o == id =>
                {
                    edge(j, i, &mut successors, &mut indegree);
                }
                Task::Release(o) => {
                    let reads_it = pieces
                        .iter()
                        .any(|(_, src)| matches!(src, Source::Storage(s, _) if s == o));
                    if reads_it {
                        edge(i, j, &mut successors, &mut indegree);
                    }
                }
                _ => {}
            }
        }
    }

    // A task is worth running early if some `Release` is waiting on it. Without
    // this the scheduler would run a claim first, while the allocation it could
    // have reused is still live -- see the worked example in §6.2.
    let mut unblocks_release = vec![false; n];
    for (i, succs) in successors.iter().enumerate() {
        unblocks_release[i] = succs.iter().any(|&j| matches!(tasks[j], Task::Release(_)));
    }

    let rank = |i: usize| -> (u8, u64) {
        if unblocks_release[i] {
            return (0, 0);
        }
        match &tasks[i] {
            Task::Release(_) => (1, 0),
            // Negated so the *largest* sorts first under an ascending key.
            Task::Claim(_, size) => (2, u64::MAX - size.to_usize() as u64),
            Task::Relabel { .. } => (3, 0),
            Task::Reshape(..) => (4, 0),
            Task::Write { .. } => (5, 0),
        }
    };

    let mut ready: Vec<usize> = (0..n).filter(|&i| indegree[i] == 0).collect();
    let mut out = Vec::with_capacity(n);
    while !ready.is_empty() {
        // Ties broken by index, which is the fold's own deterministic order --
        // §9.4 makes that determinism load-bearing.
        let pick = ready
            .iter()
            .copied()
            .min_by_key(|&i| (rank(i), i))
            .expect("non-empty");
        ready.retain(|&i| i != pick);
        out.push(pick);
        for &j in &successors[pick] {
            indegree[j] -= 1;
            if indegree[j] == 0 {
                ready.push(j);
            }
        }
    }
    assert_eq!(
        out.len(),
        n,
        "the action graph is a DAG by construction: every edge runs from a lower \
         log position to a higher one",
    );

    let mut tasks: Vec<Option<Task<W, S>>> = tasks.into_iter().map(Some).collect();
    out.into_iter()
        .map(|i| tasks[i].take().expect("each task scheduled once"))
        .collect()
}
