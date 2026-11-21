//! Symbol-id relocation for files parsed in parallel.
//!
//! Each file is parsed with file-local ids (module = 0, members from 1). Once all files are
//! parsed, [`FileResult::shift_ids`] moves one file's ids to its slot in the project-wide id
//! space, giving exactly the ids a sequential parse would have assigned. Every field of
//! [`FileResult`] that holds a [`spectograph_core::ir::SymbolId`] must be handled here; the
//! `tests/parallel.rs` guards that by checking id density and reachability on a project
//! large enough to take the parallel path.

use crate::parse::{FileResult, RefTarget};

impl FileResult {
    /// Add `offset` to every symbol id this result contains.
    pub fn shift_ids(&mut self, offset: usize) {
        if offset == 0 {
            return;
        }
        self.module_id += offset;
        for s in &mut self.symbols {
            s.id += offset;
        }
        for r in &mut self.refs {
            r.from += offset;
            if let RefTarget::Symbol(id) = &mut r.target {
                *id += offset;
            }
        }
        for i in &mut self.imports {
            i.symbol_id += offset;
        }
        for e in &mut self.exported {
            *e += offset;
        }
        for c in &mut self.components {
            c.symbol_id += offset;
        }
        for h in &mut self.hook_calls {
            h.enclosing += offset;
        }
        if let Some(v) = &mut self.vue_component {
            v.symbol_id += offset;
        }
    }
}
