//! Angular template-usage scanning.
//!
//! A component's rendered template can reference other components (custom
//! element tags, `<app-foo>`), directives (attribute selectors, `[appHighlight]`
//! or the structural-directive shorthand `*ngIf`), and pipes (`{{ x | upper }}`)
//! -- none of which are an import or a function call, so the normal TS
//! reference graph has no way to see them. Angular's own template compiler is
//! what wires these up, at build time, by matching selector strings against
//! markup. This module does the same match (deliberately with a lightweight
//! regex scanner rather than a full HTML parser -- for finding *usage*, not
//! validating markup, that's sufficient, and building or depending on an HTML
//! parser is a lot more machinery for the same answer) so that usage found
//! this way can become a reachability edge, the same as JSX usage or an
//! `@NgModule` declarations array.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use regex::Regex;
use spectograph_core::ir::SymbolId;

/// A `@Component`/`@Directive`'s selector(s) and (component-only) template
/// source, keyed to the class that declares it. A directive has no template
/// of its own -- it's a usage *target* only, never a scan source.
#[derive(Clone)]
pub struct AngularSelector {
    pub class_id: SymbolId,
    pub names: Vec<String>,
    pub template: Option<String>,
    /// Binding names declared via `@Input()`/`@Output()` (using the alias
    /// when one is given, e.g. `@Input('customName')`, since that's the name
    /// a template binds under -- not the property's own name). Empty means
    /// "nothing statically enumerable", not "declares no bindings": same
    /// convention as `ReactComponent::declared_props`, and for the same
    /// reason `find_binding_mismatches` skips these -- an empty list can't
    /// be told apart from "we didn't look", so treating it as "anything
    /// goes" avoids false positives.
    pub declared_bindings: Vec<String>,
    /// The decorator's own location, used as an approximate usage-site
    /// location for a mismatch found in this component's template (the
    /// template itself, especially an external `templateUrl` file, doesn't
    /// carry per-occurrence line numbers back through the regex scanner).
    pub file: PathBuf,
    pub line: usize,
}

/// A `@Pipe`'s declared name, keyed to the class that declares it.
#[derive(Clone)]
pub struct AngularPipe {
    pub class_id: SymbolId,
    pub name: String,
}

/// Match every component's template against the project-wide selector/pipe
/// maps, producing `(from, to)` reachability edges: the component whose
/// template it is, to the component/directive/pipe class its tag/attribute/
/// pipe name resolves to. Self-edges (a recursive component referencing its
/// own selector in its own template -- a real, valid Angular pattern) are
/// left in; they're harmless for a reachability walk.
pub fn collect_template_edges(
    selectors: &[AngularSelector],
    pipes: &[AngularPipe],
) -> Vec<(SymbolId, SymbolId)> {
    let mut selector_map: HashMap<&str, SymbolId> = HashMap::new();
    for s in selectors {
        for name in &s.names {
            selector_map.entry(name.as_str()).or_insert(s.class_id);
        }
    }
    let mut pipe_map: HashMap<&str, SymbolId> = HashMap::new();
    for p in pipes {
        pipe_map.entry(p.name.as_str()).or_insert(p.class_id);
    }

    let mut edges = Vec::new();
    for s in selectors {
        let Some(template) = &s.template else {
            continue;
        };
        let usages = scan_template(template);
        for name in usages.tags.iter().chain(usages.attrs.iter()) {
            if let Some(&target) = selector_map.get(name.as_str()) {
                edges.push((s.class_id, target));
            }
        }
        for name in &usages.pipes {
            if let Some(&target) = pipe_map.get(name.as_str()) {
                edges.push((s.class_id, target));
            }
        }
    }
    edges.sort_unstable();
    edges.dedup();
    edges
}

/// Angular-framework bindings that can legitimately appear on *any* element
/// -- structural directives, `ngModel`, `ngClass`/`ngStyle` -- and so are
/// never a real component's own `@Input`/`@Output`. `@angular/core` and
/// `@angular/common` ship these as compiled library code, not TS source in
/// the analyzed project, so they'd never show up in a `declared_bindings`
/// list built by scanning decorators; without this exemption, using any of
/// them on a component tag would always look like a mismatch. Mirrors
/// `spectograph_js::react::IMPLICIT_PROPS`'s role for JSX's `key`/`ref`.
const IMPLICIT_BINDINGS: &[&str] = &[
    "ngIf",
    "ngIfElse",
    "ngIfThen",
    "ngFor",
    "ngForOf",
    "ngForTrackBy",
    "ngSwitch",
    "ngSwitchCase",
    "ngSwitchDefault",
    "ngClass",
    "ngStyle",
    "ngModel",
    "ngModelChange",
    "ngNonBindable",
    "ngTemplateOutlet",
    "ngPlural",
    "ngPluralCase",
];

/// One component tag's own name and the attribute-form bindings passed to it
/// specifically (as opposed to [`TemplateUsages`], which flattens every tag
/// and every attribute found anywhere in the whole template into two
/// unassociated lists -- fine for "is X used at all", not enough to check
/// "did usage site Y pass X a binding it doesn't declare").
#[derive(Debug, PartialEq, Eq)]
pub struct TemplateComponentUsage {
    pub tag: String,
    pub attrs: Vec<String>,
}

fn open_tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Group 2 (everything up to the closing `>`) breaks if an attribute
    // *value* contains a literal `>` (`[disabled]="a > b"`) -- a real but
    // rare enough shape in practice that a full tokenizer to handle it isn't
    // worth it here, same "good enough for usage detection" tradeoff as the
    // rest of this module.
    RE.get_or_init(|| Regex::new(r"<\s*([a-zA-Z][\w-]*)([^>]*)>").unwrap())
}

/// Scan `template` for each component tag and the attribute-form bindings
/// passed to it at that specific usage site.
pub fn scan_template_component_usages(template: &str) -> Vec<TemplateComponentUsage> {
    open_tag_re()
        .captures_iter(template)
        .map(|caps| {
            let tag = caps[1].to_string();
            let attrs_blob = &caps[2];
            let mut attrs = Vec::new();
            for re in attr_res() {
                attrs.extend(re.captures_iter(attrs_blob).map(|c| c[1].to_string()));
            }
            TemplateComponentUsage { tag, attrs }
        })
        .collect()
}

/// A binding passed to a component tag that the target doesn't declare as an
/// `@Input()`/`@Output()`.
#[derive(Debug, Clone)]
pub struct BindingMismatch {
    pub target_component: String,
    pub binding: String,
    /// Approximate: the *using* component's own decorator location, not the
    /// exact template line -- see [`AngularSelector::file`]/`line`.
    pub file: PathBuf,
    pub line: usize,
}

/// Flag template usage sites that pass a binding the target component/
/// directive doesn't declare. Only checks targets whose bindings were
/// statically enumerable (at least one `@Input`/`@Output` found); a target
/// with none is skipped entirely, the same "can't tell not-declared from
/// didn't-look" reasoning as [`spectograph_js::react::find_prop_mismatches`].
pub fn find_binding_mismatches(selectors: &[AngularSelector]) -> Vec<BindingMismatch> {
    let mut by_name: HashMap<&str, &AngularSelector> = HashMap::new();
    for s in selectors {
        for name in &s.names {
            by_name.entry(name.as_str()).or_insert(s);
        }
    }

    let mut out = Vec::new();
    for using in selectors {
        let Some(template) = &using.template else {
            continue;
        };
        for usage in scan_template_component_usages(template) {
            let Some(target) = by_name.get(usage.tag.as_str()) else {
                continue;
            };
            if target.declared_bindings.is_empty() {
                continue;
            }
            for attr in &usage.attrs {
                if IMPLICIT_BINDINGS.contains(&attr.as_str()) {
                    continue;
                }
                if !target.declared_bindings.iter().any(|b| b == attr) {
                    out.push(BindingMismatch {
                        target_component: usage.tag.clone(),
                        binding: attr.clone(),
                        file: using.file.clone(),
                        line: using.line,
                    });
                }
            }
        }
    }
    out
}

/// Tag names, attribute-form directive names, and pipe names found in a
/// template, each already reduced to a bare identifier ready to look up in a
/// selector/pipe map.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TemplateUsages {
    pub tags: Vec<String>,
    pub attrs: Vec<String>,
    pub pipes: Vec<String>,
}

fn tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<\s*([a-zA-Z][\w-]*)").unwrap())
}

/// Attribute-form directive usage: structural (`*ngIf`), two-way binding
/// (`[(ngModel)]`), property binding (`[foo]`), event binding (`(click)`).
/// Deliberately doesn't match a bare, unbracketed attribute
/// (`<div appHighlight>`) -- that form is syntactically identical to an
/// ordinary HTML attribute, so matching it would need real tag/attribute
/// parsing (know which token positions in a tag are attribute names at all)
/// rather than a text-level regex; left as a known gap in favor of not
/// producing false matches against plain HTML attributes.
fn attr_res() -> &'static [Regex; 4] {
    static RES: OnceLock<[Regex; 4]> = OnceLock::new();
    RES.get_or_init(|| {
        [
            Regex::new(r"\*([a-zA-Z][\w-]*)").unwrap(),
            Regex::new(r"\[\(([a-zA-Z][\w-]*)\)\]").unwrap(),
            Regex::new(r"\[([a-zA-Z][\w-]*)\]").unwrap(),
            Regex::new(r"\(([a-zA-Z][\w-]*)\)").unwrap(),
        ]
    })
}

/// `{{ name | upper }}` / `[foo]="x | upper"` pipe usage. Requires a
/// non-`|` character (or start of string) immediately before the `|` so a
/// JS-style `||` (valid inside an Angular template expression, e.g.
/// `{{ a || b }}`) is never mistaken for one -- the `regex` crate has no
/// lookaround, so this is expressed as a consuming alternation instead of a
/// zero-width assertion.
fn pipe_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:^|[^|])\|\s*([a-zA-Z][\w-]*)").unwrap())
}

/// Scan `template` for component/directive/pipe usage.
pub fn scan_template(template: &str) -> TemplateUsages {
    let tags = tag_re()
        .captures_iter(template)
        .map(|c| c[1].to_string())
        .collect();

    let mut attrs = Vec::new();
    for re in attr_res() {
        attrs.extend(re.captures_iter(template).map(|c| c[1].to_string()));
    }

    let pipes = pipe_re()
        .captures_iter(template)
        .map(|c| c[1].to_string())
        .collect();

    TemplateUsages { tags, attrs, pipes }
}

/// Split a `@Component`/`@Directive` `selector` string on its (possibly
/// comma-separated) selectors, reducing each to the bare name a template
/// usage would be matched under: `app-foo` (element form) stays as-is,
/// `[app-foo]` (attribute form) has its brackets stripped. A CSS class or
/// attribute-value selector (`.foo`, `[foo=bar]`) isn't something either
/// scanner above can be usefully driven from -- filtered out rather than
/// producing a name that could never match anything.
pub fn parse_selector_names(raw: &str) -> Vec<String> {
    raw.split(',')
        .filter_map(|part| {
            let part = part.trim();
            let bare = part
                .strip_prefix('[')
                .and_then(|p| p.strip_suffix(']'))
                .unwrap_or(part);
            let is_plain_name = !bare.is_empty()
                && bare.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && bare
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_');
            is_plain_name.then(|| bare.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_template_finds_tags_attrs_and_pipes() {
        let usages = scan_template(
            r#"<div><app-bar [x]="1" (click)="go()" *ngIf="cond" [(ngModel)]="y">{{ name | upper }}</app-bar></div>"#,
        );
        // Only the opening tag counts -- `<\s*([a-zA-Z]...)` doesn't match
        // `</app-bar>` (`/` isn't a letter), which is deliberate: a closing
        // tag isn't a second usage site.
        assert_eq!(usages.tags, vec!["div", "app-bar"]);
        assert!(usages.attrs.contains(&"ngIf".to_string()));
        assert!(usages.attrs.contains(&"click".to_string()));
        assert!(usages.attrs.contains(&"x".to_string()));
        assert!(usages.attrs.contains(&"ngModel".to_string()));
        assert_eq!(usages.pipes, vec!["upper"]);
    }

    #[test]
    fn pipe_scan_does_not_match_logical_or() {
        let usages = scan_template("{{ a || b }}");
        assert!(usages.pipes.is_empty());
    }

    #[test]
    fn pipe_scan_matches_pipe_after_logical_or_expression() {
        let usages = scan_template("{{ (a || b) | upper }}");
        assert_eq!(usages.pipes, vec!["upper"]);
    }

    #[test]
    fn selector_names_splits_comma_list_and_strips_attribute_brackets() {
        let names = parse_selector_names("app-foo, [app-foo]");
        assert_eq!(names, vec!["app-foo", "app-foo"]);
    }

    #[test]
    fn selector_names_skips_css_class_and_attribute_value_forms() {
        let names = parse_selector_names(".foo, [bar=baz]");
        assert!(names.is_empty());
    }

    fn test_selector(
        class_id: SymbolId,
        names: &[&str],
        template: Option<&str>,
    ) -> AngularSelector {
        AngularSelector {
            class_id,
            names: names.iter().map(|s| s.to_string()).collect(),
            template: template.map(|s| s.to_string()),
            declared_bindings: Vec::new(),
            file: PathBuf::from("test.ts"),
            line: 1,
        }
    }

    #[test]
    fn collect_template_edges_matches_tag_attr_and_pipe_usage() {
        let selectors = vec![
            test_selector(
                0,
                &["app-root"],
                Some(r#"<app-child></app-child><div appHighlight [foo]="1">{{ x | upper }}</div>"#),
            ),
            test_selector(1, &["app-child"], None),
            test_selector(2, &["foo"], None),
        ];
        let pipes = vec![AngularPipe {
            class_id: 3,
            name: "upper".to_string(),
        }];
        let edges = collect_template_edges(&selectors, &pipes);
        assert!(
            edges.contains(&(0, 1)),
            "app-root should reference app-child"
        );
        assert!(
            edges.contains(&(0, 2)),
            "app-root should reference the [foo] directive"
        );
        assert!(
            edges.contains(&(0, 3)),
            "app-root should reference the upper pipe"
        );
        assert!(
            !edges.contains(&(0, 0)),
            "appHighlight has no matching selector and must not create a self-edge"
        );
    }

    #[test]
    fn collect_template_edges_allows_recursive_self_reference() {
        let selectors = vec![test_selector(
            0,
            &["app-tree"],
            Some("<app-tree></app-tree>"),
        )];
        let edges = collect_template_edges(&selectors, &[]);
        assert_eq!(edges, vec![(0, 0)]);
    }

    #[test]
    fn scan_template_component_usages_associates_attrs_with_their_own_tag() {
        let usages = scan_template_component_usages(
            r#"<app-child [foo]="1"></app-child><app-other (bar)="go()"></app-other>"#,
        );
        assert_eq!(usages.len(), 2);
        assert_eq!(usages[0].tag, "app-child");
        assert_eq!(usages[0].attrs, vec!["foo".to_string()]);
        assert_eq!(usages[1].tag, "app-other");
        assert_eq!(usages[1].attrs, vec!["bar".to_string()]);
    }

    #[test]
    fn find_binding_mismatches_flags_undeclared_binding() {
        let using = test_selector(0, &["app-root"], Some(r#"<app-child [foo]="1" [bar]="2">"#));
        let mut target = test_selector(1, &["app-child"], None);
        target.declared_bindings = vec!["foo".to_string()];
        let mismatches = find_binding_mismatches(&[using, target]);
        assert!(mismatches
            .iter()
            .any(|m| m.target_component == "app-child" && m.binding == "bar"));
        assert!(!mismatches.iter().any(|m| m.binding == "foo"));
    }

    #[test]
    fn find_binding_mismatches_skips_target_with_no_declared_bindings() {
        // Can't tell "declares nothing" from "we didn't look" -- same
        // reasoning as React's bare-`props`-parameter case, so this must not
        // produce false positives.
        let using = test_selector(0, &["app-root"], Some(r#"<app-child [anything]="1">"#));
        let target = test_selector(1, &["app-child"], None);
        let mismatches = find_binding_mismatches(&[using, target]);
        assert!(mismatches.is_empty());
    }

    #[test]
    fn find_binding_mismatches_ignores_implicit_angular_bindings() {
        let using = test_selector(
            0,
            &["app-root"],
            Some(r#"<app-child *ngIf="cond" [ngClass]="x">"#),
        );
        let mut target = test_selector(1, &["app-child"], None);
        target.declared_bindings = vec!["foo".to_string()];
        let mismatches = find_binding_mismatches(&[using, target]);
        assert!(
            mismatches.is_empty(),
            "ngIf/ngClass are framework bindings, not app-child's own: {:?}",
            mismatches
        );
    }
}
