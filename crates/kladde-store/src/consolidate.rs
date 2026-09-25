//! Consolidation (`impl/consolidation.md`).

use crate::error::Error;
use crate::flush::{DataPage, Output};
use crate::state::*;
use crate::stats::Stats;
use crate::store::Inner;

/// What consolidation carries from flush to flush.
#[derive(Debug, Default)]
pub struct ConsState {
    /// The rotating window's cursor.
    pub cursor: Key,
    /// The per-flush budget in pages, as the controller moved it.
    pub budget: f64,
}

impl Inner {
    pub(crate) fn seed_after_load(&mut self) {
        self.cons.budget = self.opts.budget_pages as f64;
        // Content ages fall back to the youngest page holding a fragment.
        let mut youngest: crate::hash::IdMap<u64> = Default::default();
        for (&k, &f) in &self.state.frags {
            let page = match f {
                Fragment::Bytes { page, .. } => page,
                Fragment::ZeroExplicitly { stmt } => self.state.slab.page(stmt),
                _ => continue,
            };
            let e = self.state.pages[page as usize].epoch;
            let y = youngest.entry(kid(k)).or_default();
            *y = (*y).max(e);
        }
        for (&id, m) in self.state.allocs.iter_mut() {
            m.last_written = youngest.get(&id).copied().unwrap_or(self.epoch);
        }
    }

    pub(crate) fn defrag_share(&mut self, _dirty: &mut Dirty) -> Result<(), Error> {
        Ok(())
    }

    pub(crate) fn budget_loop(
        &mut self,
        _dirty: &mut Dirty,
        _out: &mut Output,
    ) -> Result<(), Error> {
        Ok(())
    }

    pub(crate) fn free_fill(
        &mut self,
        _open: &mut DataPage,
        _dirty: &mut Dirty,
    ) -> Result<(), Error> {
        Ok(())
    }

    pub(crate) fn after_commit(&mut self) -> Result<(), Error> {
        Ok(())
    }

    pub(crate) fn fill_stats(&self, s: &mut Stats) {
        s.file_pages = self.file_pages as u64;
        for (p, info) in self.state.pages.iter().enumerate() {
            if p < 2 {
                continue;
            }
            match info.state {
                PageState::Data => {
                    s.data_pages += 1;
                    s.live_data_bytes += info.coverage as u64;
                }
                PageState::Table | PageState::Interior => {
                    s.table_pages += 1;
                    s.live_table_bytes += info.coverage as u64;
                }
                PageState::Free | PageState::Retiring => s.free_pages += 1,
                _ => {}
            }
        }
        s.allocations = self.state.allocs.len() as u64;
        s.allocation_bytes = self.state.allocs.values().map(|m| m.size as u64).sum();
        s.fragments = self.state.frags.len() as u64;
        s.statements = self.state.slab.live as u64;
        s.budget = self.cons.budget as u64;
    }
}
