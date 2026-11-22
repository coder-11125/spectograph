//! Implicit uses: code that is used by the language or a framework rather than by any call
//! written in the analyzed project.
//!
//! - **Constructors** run on `new C()`, and a few protocol methods (`toString`, `toJSON`,
//!   `then`, ...) are called by the language runtime.
//! - **Angular lifecycle hooks** (`ngOnInit`, ...) are invoked by Angular.
//! - **React lifecycle methods** (`render`, `componentDidMount`, ...) are invoked by React --
//!   but only on classes that actually extend `Component`/`PureComponent` (directly or
//!   through project classes); on any other class `render` is an ordinary method.
//! - **Members of a class that extends something else outside the project** (`class X
//!   extends ClientRequest`) are overrides the external base class's own code calls
//!   (`_write`, `setNoDelay`, ...).
//! - **`React`** is used by every JSX element under the classic JSX transform.
//! - **Declaration files** (`.d.ts`) are ambient: what they declare (global augmentations
//!   like `interface ImportMeta`, module shims) is consumed by the type checker.
//!
//! Each becomes an edge from the owning class (or module) to the symbol, so it participates
//! in ordinary reachability: if the class is dead, so are its members.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use spectograph_core::ir::{
    Environment, Program, RefKind, Reference, Symbol, SymbolId, SymbolKind,
};

use crate::pathutil::normalize_path;

/// Members the language runtime or Angular calls on any class.
const ALWAYS_IMPLICIT: &[&str] = &[
    // Language protocol.
    "constructor",
    "toString",
    "toJSON",
    "valueOf",
    "then",
    // Angular lifecycle hooks.
    "ngOnInit",
    "ngOnChanges",
    "ngDoCheck",
    "ngOnDestroy",
    "ngAfterContentInit",
    "ngAfterContentChecked",
    "ngAfterViewInit",
    "ngAfterViewChecked",
];

/// `ProxyHandler` traps the runtime calls when a class instance is used as a proxy handler.
/// Only applied in files that mention `ProxyHandler` or `new Proxy`.
const PROXY_TRAPS: &[&str] = &[
    "get",
    "set",
    "has",
    "deleteProperty",
    "defineProperty",
    "ownKeys",
    "getOwnPropertyDescriptor",
    "getPrototypeOf",
    "setPrototypeOf",
    "isExtensible",
    "preventExtensions",
    "apply",
    "construct",
];

/// Members React calls on a class that extends `Component`/`PureComponent`.
const REACT_LIFECYCLE: &[&str] = &[
    "render",
    "componentDidMount",
    "componentDidUpdate",
    "componentWillUnmount",
    "shouldComponentUpdate",
    "getSnapshotBeforeUpdate",
    "componentDidCatch",
    "getDerivedStateFromProps",
    "getDerivedStateFromError",
    "getChildContext",
    "componentWillMount",
    "componentWillReceiveProps",
    "componentWillUpdate",
    "UNSAFE_componentWillMount",
    "UNSAFE_componentWillReceiveProps",
    "UNSAFE_componentWillUpdate",
];

/// What a class extends, as far as the project can tell.
#[derive(Debug, PartialEq, Eq)]
enum Base {
    /// No `extends` clause, or a chain of project classes that ends without one.
    Project,
    /// `React.Component` / `PureComponent`, directly or through project classes.
    ReactComponent,
    /// Anything else not defined in the project.
    External,
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Byte offset of the start of each line in `src`.
fn line_starts(src: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(src.match_indices('\n').map(|(i, _)| i + 1))
        .collect()
}

/// A JSX element: a closing tag (`</div`) or a self-closing one (`<Icon name="x" />`).
/// Generic type arguments (`Array<Foo>()`) and comparisons (`a < b`) don't match.
fn jsx_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"</[A-Za-z]|<[A-Za-z][A-Za-z0-9.]*(\s[^<>]*)?/>").unwrap())
}

/// The identifier path after `extends` in the header of `class <name> ...`, searched from
/// `from` (the symbol's start). Generic parameters on the class (`class A<T extends B>`) are
/// skipped so their constraints aren't mistaken for a base class.
fn base_class(src: &str, from: usize, name: &str) -> Option<String> {
    let window_end = (from + 4000).min(src.len());
    let window = src.get(from..window_end).or_else(|| src.get(from..))?;

    // Find `class <name>` as whole words (decorators and `export`/`abstract` may precede it).
    let mut search = 0;
    let after_name = loop {
        let at = window[search..].find("class")? + search;
        search = at + "class".len();
        let before_ok = window[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident_char(c));
        let rest = &window[search..];
        let trimmed = rest.trim_start();
        if before_ok && trimmed.len() < rest.len() && trimmed.starts_with(name) {
            let end = &trimmed[name.len()..];
            if end.chars().next().is_none_or(|c| !is_ident_char(c)) {
                break end;
            }
        }
    };

    // Skip `<...>` generic parameters (balanced).
    let mut rest = after_name.trim_start();
    if rest.starts_with('<') {
        let mut depth = 0usize;
        let mut cut = rest.len();
        for (i, c) in rest.char_indices() {
            match c {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        cut = i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        rest = rest[cut..].trim_start();
    }

    let header = &rest[..rest.find('{').unwrap_or(rest.len())];
    let after = header.trim_start().strip_prefix("extends")?;
    // `extends` must be a whole word followed by whitespace.
    if !after.starts_with(char::is_whitespace) {
        return None;
    }
    let base: String = after
        .trim_start()
        .chars()
        .take_while(|c| is_ident_char(*c) || *c == '.')
        .collect();
    (!base.is_empty()).then_some(base)
}

/// Follow `start`'s `extends` chain through project classes until it ends, reaches React's
/// `Component`, or leaves the project.
fn classify_base(
    start: SymbolId,
    raw_base: &HashMap<SymbolId, Option<String>>,
    class_by_name: &HashMap<&str, SymbolId>,
) -> Base {
    let mut current = start;
    for _ in 0..16 {
        let Some(Some(base)) = raw_base.get(&current) else {
            return Base::Project;
        };
        let last = base.rsplit('.').next().unwrap_or(base);
        let project_class = (!base.contains('.'))
            .then(|| class_by_name.get(last))
            .flatten();
        match project_class {
            Some(&next) => current = next,
            None if matches!(last, "Component" | "PureComponent") => return Base::ReactComponent,
            None => return Base::External,
        }
    }
    Base::Project
}

fn is_declaration_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".d.ts") || n.ends_with(".d.mts") || n.ends_with(".d.cts"))
}

/// Add the implicit-use edges described in the module docs.
///
/// `sources` are the exact texts that were parsed (so symbol spans line up).
pub fn add_implicit_edges(program: &mut Program, sources: &[(PathBuf, String)]) {
    let mut by_file: HashMap<PathBuf, Vec<&Symbol>> = HashMap::new();
    for s in &program.symbols {
        if !s.is_external {
            by_file.entry(normalize_path(&s.file)).or_default().push(s);
        }
    }
    let mut class_by_name: HashMap<&str, SymbolId> = HashMap::new();
    for s in &program.symbols {
        if s.kind == SymbolKind::Class && !s.is_external && !s.name.contains('.') {
            class_by_name.entry(s.name.as_str()).or_insert(s.id);
        }
    }

    // Pass 1: what each class's header says it extends.
    let mut raw_base: HashMap<SymbolId, Option<String>> = HashMap::new();
    for (path, src) in sources {
        let Some(symbols) = by_file.get(&normalize_path(path)) else {
            continue;
        };
        let starts = line_starts(src);
        for class in symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class && !s.name.contains('.'))
        {
            let offset = starts
                .get(class.span.start_line.saturating_sub(1))
                .map(|line| line + class.span.start_col.saturating_sub(1));
            raw_base.insert(
                class.id,
                offset.and_then(|o| base_class(src, o, &class.name)),
            );
        }
    }

    let mut edges: Vec<(SymbolId, SymbolId)> = Vec::new();

    // Pass 2: members that are used implicitly.
    for (path, src) in sources {
        let Some(symbols) = by_file.get(&normalize_path(path)) else {
            continue;
        };

        // Proxy traps are invoked by the runtime on the proxy, never called by name.
        let proxy_file = src.contains("ProxyHandler") || src.contains("new Proxy");

        for class in symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class && !s.name.contains('.'))
        {
            let prefix = format!("{}.", class.name);
            let base = classify_base(class.id, &raw_base, &class_by_name);
            for member in symbols.iter().filter(|s| {
                s.name.starts_with(&prefix)
                    && (!s.name[prefix.len()..].contains('.')
                        // `[Flags.IS_REF]`: a computed key legitimately contains dots.
                        || s.name[prefix.len()..].starts_with('['))
            }) {
                let short = &member.name[prefix.len()..];
                // A computed member name (`[ReactiveFlags.IS_REF] = true`) is only known at
                // runtime, so any `x[key]` access may reach it: never prove it dead.
                let implicit = short.starts_with('[')
                    || ALWAYS_IMPLICIT.contains(&short)
                    || (proxy_file && PROXY_TRAPS.contains(&short))
                    || match base {
                        Base::External => true,
                        Base::ReactComponent => REACT_LIFECYCLE.contains(&short),
                        Base::Project => false,
                    };
                if implicit {
                    edges.push((class.id, member.id));
                }
            }
        }

        let module = symbols.iter().find(|s| s.kind == SymbolKind::Module);

        // Classic JSX transform: every JSX element is `React.createElement`.
        let is_jsx_file = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("jsx") | Some("tsx")
        ) || jsx_re().is_match(src);
        if is_jsx_file {
            let react = symbols
                .iter()
                .find(|s| s.kind == SymbolKind::Import && s.name == "React");
            if let (Some(module), Some(react)) = (module, react) {
                edges.push((module.id, react.id));
            }
        }

        // Ambient declarations are consumed by the compiler, not by importers.
        if is_declaration_file(path) {
            if let Some(module) = module {
                edges.extend(
                    symbols
                        .iter()
                        .filter(|s| {
                            matches!(
                                s.kind,
                                SymbolKind::Function
                                    | SymbolKind::Class
                                    | SymbolKind::Const
                                    | SymbolKind::Type
                            )
                        })
                        .map(|s| (module.id, s.id)),
                );
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

    fn base(src: &str, name: &str) -> Option<String> {
        base_class(src, 0, name)
    }

    #[test]
    fn finds_plain_and_namespaced_bases() {
        assert_eq!(base("class A extends B {}", "A").as_deref(), Some("B"));
        assert_eq!(
            base(
                "export default class Home extends React.Component {",
                "Home"
            )
            .as_deref(),
            Some("React.Component")
        );
        assert_eq!(base("class A {}", "A"), None);
        assert_eq!(base("class A implements I {}", "A"), None);
    }

    #[test]
    fn generic_constraints_are_not_bases() {
        assert_eq!(base("class A<T extends Foo> {}", "A"), None);
        assert_eq!(
            base("class A<T extends Foo> extends Base<T> {}", "A").as_deref(),
            Some("Base")
        );
    }

    #[test]
    fn decorators_before_the_class_are_skipped() {
        let src = "@Component({ selector: 'x' }) class A extends Base {}";
        assert_eq!(base(src, "A").as_deref(), Some("Base"));
    }

    #[test]
    fn similarly_named_classes_do_not_confuse_the_scan() {
        assert_eq!(
            base("class AB extends X {} class A extends Y {}", "A").as_deref(),
            Some("Y")
        );
    }

    #[test]
    fn base_chains_are_followed_through_project_classes() {
        // 1: ProfileFavorites -> Profile -> React.Component; 2: Plain -> (nothing);
        // 3: Req -> ClientRequest (external); 4: Mid -> Plain.
        let raw: HashMap<SymbolId, Option<String>> = HashMap::from([
            (0, Some("Profile".into())),
            (10, Some("React.Component".into())),
            (2, None),
            (3, Some("ClientRequest".into())),
            (4, Some("Plain".into())),
        ]);
        let classes: HashMap<&str, SymbolId> = HashMap::from([
            ("ProfileFavorites", 0),
            ("Profile", 10),
            ("Plain", 2),
            ("Req", 3),
            ("Mid", 4),
        ]);
        assert_eq!(classify_base(0, &raw, &classes), Base::ReactComponent);
        assert_eq!(classify_base(2, &raw, &classes), Base::Project);
        assert_eq!(classify_base(3, &raw, &classes), Base::External);
        assert_eq!(classify_base(4, &raw, &classes), Base::Project);
    }

    #[test]
    fn a_cyclic_chain_terminates() {
        let raw: HashMap<SymbolId, Option<String>> =
            HashMap::from([(0, Some("B".into())), (1, Some("A".into()))]);
        let classes: HashMap<&str, SymbolId> = HashMap::from([("A", 0), ("B", 1)]);
        assert_eq!(classify_base(0, &raw, &classes), Base::Project);
    }

    #[test]
    fn declaration_files_are_recognized() {
        assert!(is_declaration_file(Path::new("src/types/global.d.ts")));
        assert!(is_declaration_file(Path::new("x.d.mts")));
        assert!(!is_declaration_file(Path::new("src/index.ts")));
    }

    #[test]
    fn jsx_is_detected_including_self_closing_only_files() {
        assert!(jsx_re().is_match("return <i />;"));
        assert!(jsx_re().is_match("<Icon name=\"x\" />"));
        assert!(jsx_re().is_match("<div>hi</div>"));
        assert!(!jsx_re().is_match("const m: Map<string, number> = new Map(); a < b"));
        assert!(!jsx_re().is_match("Array<Foo>()"));
    }
}
