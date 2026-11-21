//! Unused CSS classes in Vue `<style scoped>` blocks.
//!
//! A scoped style only ever matches elements of its own component's template (plus the root
//! element of child components), so a class selector no template names is dead. This is the
//! one place the tool can say "unused CSS" without knowing the rest of the page.
//!
//! Precision comes first -- a wrong "unused" report on a class that is in fact used is worse
//! than missing a dead one -- so everything ambiguous makes the check skip rather than guess:
//!
//! - only `<style scoped>` blocks in plain CSS/SCSS/Less are inspected (global styles can be
//!   used from anywhere; `module` styles are referenced as `$style.x`);
//! - a template with a class binding that can't be enumerated (`:class="cls"`, a ternary, a
//!   computed key, `v-bind="obj"`) is skipped entirely;
//! - a style using SCSS/Less class concatenation (`&-suffix`, `&__elem`) or interpolation
//!   (`#{...}`, `@{...}`) is skipped entirely;
//! - selectors using `:deep()`, `::v-deep`, `>>>`, `/deep/`, `:global()`, `:slotted()` or
//!   escapes (`.md\:flex`) are not inspected;
//! - a class counts as used if *any* `.vue` template in the project names it (a parent can pass
//!   a class down to a child's root, where the child's scoped rule applies) or its name, or a
//!   prefix such as `'btn-'`, appears as a string in the component's own script.

use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use spectograph_core::analysis::display_path;
use spectograph_core::diagnostic::{Diagnostic, VUE_UNUSED_CSS_CLASS};

use crate::vue::{discover_vue_files, materialize_script, parse_sfc, ScriptLang, SfcFile};

/// A class selector defined in a scoped style that nothing uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnusedCssClass {
    pub class: String,
    /// 1-based line in the `.vue` file.
    pub line: usize,
}

/// Class names (exact) and prefixes (`'btn-'` from `'btn-' + kind`) a piece of code or markup
/// names.
#[derive(Debug, Default, Clone)]
pub struct ClassUse {
    exact: HashSet<String>,
    prefixes: Vec<String>,
}

impl ClassUse {
    fn add_token(&mut self, token: &str) {
        if token.is_empty() {
            return;
        }
        self.exact.insert(token.to_string());
        if token.ends_with('-') || token.ends_with('_') {
            self.prefixes.push(token.to_string());
        }
    }

    fn contains(&self, class: &str) -> bool {
        self.exact.contains(class) || self.prefixes.iter().any(|p| class.starts_with(p.as_str()))
    }

    pub fn extend(&mut self, other: &ClassUse) {
        self.exact.extend(other.exact.iter().cloned());
        self.prefixes.extend(other.prefixes.iter().cloned());
    }
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).unwrap())
}

fn static_class_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `class="a b"`; the preceding character excludes `:class` and `v-bind:class`.
    re(&RE, r#"(?:^|[\s"'])class\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
}

fn bound_class_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(
        &RE,
        r#"(?:^|\s)(?::|v-bind:)class\s*=\s*(?:"([^"]*)"|'([^']*)')"#,
    )
}

fn spread_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `v-bind="obj"` -- a whole attribute object, which can carry `class`.
    re(&RE, r#"(?:^|\s)v-bind\s*="#)
}

fn string_literal_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r#"'([^']*)'|"([^"]*)"|`([^`$]*)`"#)
}

fn object_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r#"[{,]\s*([A-Za-z_$][\w$]*)\s*:"#)
}

fn computed_or_spread_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r#"[{,]\s*\[|\.\.\."#)
}

fn template_prefix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // The literal part before a `${...}` in a template string: `is-${state}`.
    re(&RE, r#"([\w-]+)\$\{"#)
}

/// Add every string literal in `expr` (split on whitespace) to `uses`; returns the expression
/// with the literals removed.
fn collect_strings(expr: &str, uses: &mut ClassUse) -> String {
    for caps in string_literal_re().captures_iter(expr) {
        let s = caps
            .get(1)
            .or_else(|| caps.get(2))
            .or_else(|| caps.get(3))
            .map_or("", |m| m.as_str());
        for token in s.split_whitespace() {
            uses.add_token(token);
        }
    }
    for caps in template_prefix_re().captures_iter(expr) {
        uses.add_token(&caps[1]);
        // A trailing-dash style prefix even without the dash (`btn${kind}`).
        uses.prefixes.push(caps[1].to_string());
    }
    string_literal_re().replace_all(expr, "").into_owned()
}

/// Remove balanced `{ ... }` groups.
fn strip_braces(s: &str) -> String {
    let mut depth = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Scan one `:class="..."` expression. Adds what it names to `uses`; returns `true` when it
/// is *dynamic* -- some class name could come from a variable, so the full set is unknowable.
fn scan_class_expression(expr: &str, uses: &mut ClassUse) -> bool {
    let stripped = collect_strings(expr, uses);
    for caps in object_key_re().captures_iter(&stripped) {
        uses.add_token(&caps[1]);
    }
    let t = stripped.trim();
    if t.is_empty() {
        return false; // a bare string literal
    }
    if computed_or_spread_re().is_match(t) {
        return true;
    }
    if t.starts_with('{') {
        return false; // `{ active: isActive, 'is-open': open }`
    }
    if t.starts_with('[') {
        // Elements must be only strings (already removed), objects, commas and brackets.
        let rest = strip_braces(t);
        return !rest
            .chars()
            .all(|c| c.is_whitespace() || matches!(c, ',' | '[' | ']'));
    }
    true // an identifier, ternary, call, ...
}

/// Class names a template uses, plus whether any binding was too dynamic to enumerate.
pub fn template_class_use(template: &str) -> (ClassUse, bool) {
    let mut uses = ClassUse::default();
    let mut dynamic = spread_re().is_match(template);
    for caps in static_class_re().captures_iter(template) {
        let value = caps
            .get(1)
            .or_else(|| caps.get(2))
            .map_or("", |m| m.as_str());
        for token in value.split_whitespace() {
            uses.add_token(token);
        }
    }
    for caps in bound_class_re().captures_iter(template) {
        let expr = caps
            .get(1)
            .or_else(|| caps.get(2))
            .map_or("", |m| m.as_str());
        dynamic |= scan_class_expression(expr, &mut uses);
    }
    (uses, dynamic)
}

/// Every name-like token and string fragment in a script, for the "mentioned in script" check.
fn script_class_use(script: &str) -> ClassUse {
    static WORD: OnceLock<Regex> = OnceLock::new();
    let mut uses = ClassUse::default();
    collect_strings(script, &mut uses);
    for m in re(&WORD, r"[A-Za-z_][\w-]*").find_iter(script) {
        uses.exact.insert(m.as_str().to_string());
    }
    uses
}

fn style_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r"(?s)<style((?:\s+[^>]*)?)>(.*?)</style>")
}

fn word_attr(attrs: &str, name: &str) -> bool {
    attrs
        .split_whitespace()
        .any(|a| a == name || a.starts_with(&format!("{name}=")))
}

fn lang_of(attrs: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r#"\blang\s*=\s*["']([\w-]+)["']"#)
        .captures(attrs)
        .map(|c| c[1].to_ascii_lowercase())
}

/// Replace comments with spaces, keeping newlines so line numbers survive. `//` comments only
/// exist in SCSS/Less, and are not recognised right after a `:` (`url(http://...)`).
fn strip_comments(css: &str, line_comments: bool) -> String {
    let b: Vec<char> = css.chars().collect();
    let mut out = String::with_capacity(css.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == '/' && b.get(i + 1) == Some(&'*') {
            while i < b.len() && !(b[i] == '*' && b.get(i + 1) == Some(&'/')) {
                out.push(if b[i] == '\n' { '\n' } else { ' ' });
                i += 1;
            }
            out.push_str("  ");
            i += 2;
        } else if line_comments
            && b[i] == '/'
            && b.get(i + 1) == Some(&'/')
            && !(i > 0 && b[i - 1] == ':')
        {
            while i < b.len() && b[i] != '\n' {
                out.push(' ');
                i += 1;
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// Remove balanced `( ... )` groups (`:not(.a)`, `:nth-child(2n+1)`), so classes in
/// functional pseudo-classes aren't treated as defined.
fn strip_parens(s: &str) -> String {
    let mut depth = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

fn class_in_selector_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r"\.(-?[_a-zA-Z][\w-]*)")
}

/// Class selectors a style block defines, as `(class, 0-based line within the block)`.
/// `None` when the block can't be reasoned about (see the module docs).
fn defined_classes(css: &str, lang: Option<&str>) -> Option<Vec<(String, usize)>> {
    let line_comments = matches!(lang, Some("scss") | Some("less"));
    let css = strip_comments(css, line_comments);
    static CONCAT: OnceLock<Regex> = OnceLock::new();
    if css.contains("#{") || css.contains("@{") || re(&CONCAT, r"&[\w-]").is_match(&css) {
        return None;
    }

    let skip_markers = [
        ":deep(",
        "::v-deep",
        ">>>",
        "/deep/",
        ":global(",
        ":slotted(",
        "::deep",
        "\\",
    ];
    let mut out: Vec<(String, usize)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut start = 0usize;
    for (i, ch) in css.char_indices() {
        match ch {
            '{' => {
                let prelude = &css[start..i];
                let trimmed = prelude.trim_start();
                if !trimmed.starts_with('@') && !skip_markers.iter().any(|m| prelude.contains(m)) {
                    let cleaned = strip_parens(prelude);
                    for m in class_in_selector_re().captures_iter(&cleaned) {
                        let class = m[1].to_string();
                        if seen.insert(class.clone()) {
                            // Line of the `.class` text inside the original prelude.
                            let offset = start + prelude.find(&format!(".{class}")).unwrap_or(0);
                            let line = css[..offset].matches('\n').count();
                            out.push((class, line));
                        }
                    }
                }
                start = i + 1;
            }
            '}' | ';' => start = i + 1,
            _ => {}
        }
    }
    Some(out)
}

/// Unused classes in `sfc_source`'s scoped styles, given every class name the project's
/// templates use (`project_use`, which should already include this file's own template).
/// Empty when the file has no template, a dynamic class binding, or nothing scoped.
pub fn find_unused_scoped_classes(sfc_source: &str, project_use: &ClassUse) -> Vec<UnusedCssClass> {
    let sfc: SfcFile = parse_sfc(sfc_source);
    let Some(template) = &sfc.template else {
        return Vec::new();
    };
    let (_own, dynamic) = template_class_use(&template.content);
    if dynamic {
        return Vec::new();
    }
    let mut script_text = String::new();
    for block in [&sfc.script, &sfc.script_setup].into_iter().flatten() {
        script_text.push_str(&block.content);
        script_text.push('\n');
    }
    let script_use = script_class_use(&script_text);

    let mut out = Vec::new();
    for caps in style_re().captures_iter(sfc_source) {
        let attrs = &caps[1];
        if !word_attr(attrs, "scoped") || word_attr(attrs, "module") {
            continue;
        }
        let lang = lang_of(attrs);
        if !matches!(
            lang.as_deref(),
            None | Some("css") | Some("scss") | Some("less") | Some("postcss") | Some("pcss")
        ) {
            continue;
        }
        let body = caps.get(2).unwrap();
        let Some(defined) = defined_classes(body.as_str(), lang.as_deref()) else {
            continue;
        };
        // 1-based line of the `<style>` tag; the body starts on that same line.
        let tag_line = 1 + sfc_source.as_bytes()[..body.start()]
            .iter()
            .filter(|&&c| c == b'\n')
            .count();
        for (class, rel_line) in defined {
            if project_use.contains(&class) || script_use.contains(&class) {
                continue;
            }
            out.push(UnusedCssClass {
                class,
                line: tag_line + rel_line,
            });
        }
    }
    out
}

/// `check` diagnostics for unused scoped-style classes across every `.vue` file under `root`
/// whose script language `include` accepts (`None` = the file has no script block). Templates
/// of *all* `.vue` files feed the "used anywhere" set regardless of language.
pub fn unused_css_diagnostics(
    root: &Path,
    include: impl Fn(Option<ScriptLang>) -> bool,
) -> std::io::Result<Vec<Diagnostic>> {
    let mut sources = Vec::new();
    for path in discover_vue_files(root)? {
        let bytes = std::fs::read(&path)?;
        sources.push((path, String::from_utf8_lossy(&bytes).into_owned()));
    }

    let mut project_use = ClassUse::default();
    for (_path, src) in &sources {
        if let Some(t) = parse_sfc(src).template {
            let (uses, _dynamic) = template_class_use(&t.content);
            project_use.extend(&uses);
        }
    }

    let mut out = Vec::new();
    for (path, src) in &sources {
        let lang = materialize_script(&parse_sfc(src)).map(|(_text, lang)| lang);
        if !include(lang) {
            continue;
        }
        for unused in find_unused_scoped_classes(src, &project_use) {
            out.push(Diagnostic {
                code: VUE_UNUSED_CSS_CLASS,
                message: format!(
                    "scoped style defines `.{}` but no template uses it",
                    unused.class
                ),
                file: display_path(root, path),
                line: unused.line,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unused(sfc: &str) -> Vec<String> {
        let (own, _) = parse_sfc(sfc)
            .template
            .map(|t| template_class_use(&t.content))
            .unwrap_or_default();
        find_unused_scoped_classes(sfc, &own)
            .into_iter()
            .map(|u| u.class)
            .collect()
    }

    #[test]
    fn flags_scoped_classes_no_template_uses() {
        let sfc = "<template><div class=\"card\"><p :class=\"{ active: on }\">x</p></div></template>\n<style scoped>\n.card { color: red }\n.active { color: blue }\n.gone { color: green }\n</style>\n";
        assert_eq!(unused(sfc), vec!["gone"]);
    }

    #[test]
    fn reports_the_line_of_the_selector() {
        let sfc =
            "<template><div/></template>\n<style scoped>\n.a { x: 1 }\n\n.b { x: 2 }\n</style>\n";
        let (own, _) = template_class_use("<div/>");
        let found = find_unused_scoped_classes(sfc, &own);
        assert_eq!(
            found[0],
            UnusedCssClass {
                class: "a".into(),
                line: 3
            }
        );
        assert_eq!(
            found[1],
            UnusedCssClass {
                class: "b".into(),
                line: 5
            }
        );
    }

    #[test]
    fn only_scoped_plain_styles_are_inspected() {
        let global = "<template><div/></template>\n<style>\n.gone { x: 1 }\n</style>\n";
        let module =
            "<template><div/></template>\n<style scoped module>\n.gone { x: 1 }\n</style>\n";
        let sass =
            "<template><div/></template>\n<style scoped lang=\"sass\">\n.gone\n  x: 1\n</style>\n";
        assert!(unused(global).is_empty());
        assert!(unused(module).is_empty());
        assert!(unused(sass).is_empty());
    }

    #[test]
    fn dynamic_class_bindings_skip_the_file() {
        for binding in [
            ":class=\"cls\"",
            ":class=\"on ? a : b\"",
            ":class=\"{ [name]: on }\"",
            ":class=\"[cls, { x: y }]\"",
            "v-bind=\"attrs\"",
        ] {
            let sfc = format!(
                "<template><div {binding}/></template>\n<style scoped>\n.gone {{ x: 1 }}\n</style>\n"
            );
            assert!(unused(&sfc).is_empty(), "{binding}");
        }
    }

    #[test]
    fn enumerable_class_bindings_are_understood() {
        let sfc = "<template><div :class=\"['a', { b: x, 'c-d': y }]\" class=\"e f\"/></template>\n<style scoped>\n.a,.b,.c-d,.e,.f { x: 1 }\n.g { x: 1 }\n</style>\n";
        assert_eq!(unused(sfc), vec!["g"]);
    }

    #[test]
    fn script_mentions_and_prefixes_count_as_use() {
        let sfc = "<template><div ref=\"el\"/></template>\n<script>\nexport default { mounted() { this.$refs.el.classList.add('open'); this.k = 'btn-' + this.kind } }\n</script>\n<style scoped>\n.open { x: 1 }\n.btn-primary { x: 1 }\n.gone { x: 1 }\n</style>\n";
        assert_eq!(unused(sfc), vec!["gone"]);
    }

    #[test]
    fn project_wide_template_use_covers_parent_passed_classes() {
        let child = "<template><div/></template>\n<style scoped>\n.highlight { x: 1 }\n</style>\n";
        let mut project = ClassUse::default();
        project.extend(&template_class_use("<Child class=\"highlight\"/>").0);
        assert!(find_unused_scoped_classes(child, &project).is_empty());
    }

    #[test]
    fn ambiguous_styles_are_skipped() {
        for css in [
            ".btn { &-primary { x: 1 } }",
            ".#{$prefix}-a { x: 1 }",
            ".a :deep(.inner) { x: 1 }",
            ".a ::v-deep .inner { x: 1 }",
            ":global(.g) { x: 1 }",
            ".md\\:flex { x: 1 }",
        ] {
            let sfc = format!(
                "<template><div/></template>\n<style scoped lang=\"scss\">\n{css}\n</style>\n"
            );
            // `.btn`/`.a` in the first/third/fourth could still be flagged on their own only
            // if the whole block were trusted; none of these blocks is.
            let found = unused(&sfc);
            assert!(
                !found
                    .iter()
                    .any(|c| matches!(c.as_str(), "inner" | "g" | "md" | "flex" | "primary")),
                "{css}: {found:?}"
            );
        }
        // Whole-block bails (concatenation / interpolation).
        for css in [".btn { &-primary { x: 1 } }", ".#{$prefix}-a { x: 1 }"] {
            let sfc = format!(
                "<template><div/></template>\n<style scoped lang=\"scss\">\n{css}\n</style>\n"
            );
            assert!(unused(&sfc).is_empty(), "{css}");
        }
    }

    #[test]
    fn comments_pseudo_functions_at_rules_and_numbers_are_not_classes() {
        let sfc = "<template><div class=\"real\"/></template>\n<style scoped lang=\"scss\">\n/* .commented { } */\n// .line { }\n.real:not(.excluded) { margin: .5rem; background: url(http://x.test/a.png) }\n@media (min-width: 10px) { .real { x: 1 } }\n@keyframes spin { 50% { x: 1 } }\n</style>\n";
        assert!(unused(sfc).is_empty(), "{:?}", unused(sfc));
    }

    #[test]
    fn no_template_means_no_report() {
        assert!(unused(
            "<script>export default {}</script>\n<style scoped>\n.a { x: 1 }\n</style>\n"
        )
        .is_empty());
    }
}
