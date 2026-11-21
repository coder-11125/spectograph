//! Symbol-id relocation for files parsed in parallel. See `spectograph_js::shift`.

use crate::parse::{DecoratorTarget, FileResult, RefTarget};

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
        for d in &mut self.decorators {
            match &mut d.target {
                DecoratorTarget::Symbol(id) => *id += offset,
                DecoratorTarget::Parameter { owner, .. } => *owner += offset,
            }
        }
        for c in &mut self.components {
            c.symbol_id += offset;
        }
        for h in &mut self.hook_calls {
            h.enclosing += offset;
        }
        for s in &mut self.angular_selectors {
            s.class_id += offset;
        }
        for p in &mut self.angular_pipes {
            p.class_id += offset;
        }
        if let Some(v) = &mut self.vue_component {
            v.symbol_id += offset;
        }
    }
}
