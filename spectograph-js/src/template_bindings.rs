//! Vue template -> script bindings.
//!
//! In a `<script setup>` component every top-level binding (`const form = reactive(...)`,
//! `function onSubmit() {}`, `import AppFooter from './AppFooter.vue'`) is visible to the
//! `<template>` -- `@submit="onSubmit"`, `{{ form.email }}`, `<AppFooter/>`. The template is
//! not JavaScript, so the ordinary reference walk never sees those uses and every such
//! binding would be reported as dead. Components imported for Options-API `components:`
//! registration are used the same way.
//!
//! The match is by name: a binding counts as used if its name occurs as a word in the
//! template (kebab-case tags are also tried as PascalCase). That over-approximates --
//! `<form>` also "uses" a variable named `form` -- which can only hide bloat, never invent
//! it, matching the tool's precision-first stance.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::OnceLock;

use regex::Regex;
use spectograph_core::ir::{
    Environment, Program, RefKind, Reference, Symbol, SymbolId, SymbolKind,
};

use crate::pathutil::normalize_path;
use crate::vue::{kebab_to_pascal, TemplateUsageSite};

fn word_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-z_$][\w$]*(?:-[A-Za-z0-9_$]+)*").unwrap())
}

/// Every word in a template, plus the PascalCase form of each kebab-case one.
pub fn template_words(template: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for m in word_re().find_iter(template) {
        let word = m.as_str();
        if word.contains('-') {
            out.insert(kebab_to_pascal(word));
        }
        out.insert(word.to_string());
    }
    out
}

/// Add an edge from each template's using symbol (the component) to every top-level
/// binding of the same file that the template names.
pub fn add_template_binding_edges(program: &mut Program, sites: &[TemplateUsageSite]) {
    if sites.is_empty() {
        return;
    }
    let mut by_file: HashMap<PathBuf, Vec<&Symbol>> = HashMap::new();
    for s in &program.symbols {
        by_file.entry(normalize_path(&s.file)).or_default().push(s);
    }

    let mut edges: Vec<(SymbolId, SymbolId)> = Vec::new();
    for site in sites {
        let Some(symbols) = by_file.get(&normalize_path(&site.file)) else {
            continue;
        };
        let words = template_words(&site.template);
        for s in symbols {
            let is_binding = matches!(
                s.kind,
                SymbolKind::Function | SymbolKind::Class | SymbolKind::Const | SymbolKind::Import
            );
            if is_binding
                && s.id != site.using_symbol
                && !s.name.contains('.')
                && words.contains(&s.name)
            {
                edges.push((site.using_symbol, s.id));
            }
        }
    }

    edges.sort_unstable();
    edges.dedup();
    program
        .references
        .extend(edges.into_iter().map(|(from, to)| Reference {
            from,
            to,
            kind: RefKind::Static,
            environment: Environment::Universal,
        }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_identifiers_and_pascal_cased_kebab_tags() {
        let words = template_words(
            r#"<app-footer @submit="onSubmit" :value="form.email">{{ title }}</app-footer>"#,
        );
        for expected in ["onSubmit", "form", "title", "AppFooter", "app-footer"] {
            assert!(words.contains(expected), "missing {expected}: {words:?}");
        }
    }
}
