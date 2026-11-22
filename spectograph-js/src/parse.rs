//! Single-file JavaScript parsing.

use std::collections::HashMap;
use std::path::Path;

use spectograph_core::ir::{Environment, RefKind, SourceSpan, Symbol, SymbolId, SymbolKind};
use tree_sitter::{Node, Parser};

use crate::react::{is_component_name, is_hook_name, HookCall, JsxUsage, ReactComponent};
use crate::vue::{self, VueAsyncComponent, VueComponent, VueGlobalRegistration};

/// Environment-indicative globals: identifiers whose mere presence in `typeof X`
/// (a guard) or as a bare reference signals that surrounding code targets one
/// runtime specifically.
fn env_of_global(name: &str) -> Option<Environment> {
    match name {
        "window" | "document" | "navigator" | "localStorage" | "sessionStorage" => {
            Some(Environment::Browser)
        }
        "process" | "global" | "__dirname" | "__filename" | "Buffer" => Some(Environment::Node),
        _ => None,
    }
}

fn opposite_env(env: Environment) -> Environment {
    match env {
        Environment::Browser => Environment::Node,
        Environment::Node => Environment::Browser,
        Environment::Universal => Environment::Universal,
    }
}

/// If `node` is `typeof <identifier>`, return the identifier's name.
fn typeof_target<'a>(node: Node, src: &'a [u8]) -> Option<&'a str> {
    if node.kind() != "unary_expression" {
        return None;
    }
    let op = node.child_by_field_name("operator")?;
    if node_text(op, src) != "typeof" {
        return None;
    }
    let arg = node.child_by_field_name("argument")?;
    if arg.kind() == "identifier" {
        Some(node_text(arg, src))
    } else {
        None
    }
}

/// Detect an environment guard: `typeof X !== 'undefined'` / `=== 'undefined'`,
/// operands in either order, where X is an environment-indicative global.
/// Returns `(consequence_env, alternative_env)`.
fn detect_env_guard(condition: Node, src: &[u8]) -> Option<(Environment, Environment)> {
    let expr = if condition.kind() == "parenthesized_expression" {
        condition.named_child(0)?
    } else {
        condition
    };
    if expr.kind() != "binary_expression" {
        return None;
    }
    let operator = node_text(expr.child_by_field_name("operator")?, src);
    let left = expr.child_by_field_name("left")?;
    let right = expr.child_by_field_name("right")?;

    let (name, literal_side) = if let Some(name) = typeof_target(left, src) {
        (name, right)
    } else {
        (typeof_target(right, src)?, left)
    };
    let literal = node_text(literal_side, src).trim_matches(['\'', '"', '`']);
    if literal != "undefined" {
        return None;
    }
    let env = env_of_global(name)?;
    let opposite = opposite_env(env);
    match operator {
        "!==" | "!=" => Some((env, opposite)),
        "===" | "==" => Some((opposite, env)),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq)]
#[allow(dead_code)]
enum Binding {
    Global(SymbolId),
    Local,
    Dynamic,
}

#[derive(Default)]
struct Scopes {
    stack: Vec<HashMap<String, Binding>>,
    /// Class-method symbols keyed by the method node's start byte. Methods are properties,
    /// not bindings, so they must never be declared by name in a scope (that would shadow
    /// a same-named import or function and misattribute calls to it); this is how
    /// `resolve_def_id` still finds a method's own symbol while walking its body.
    members: HashMap<usize, SymbolId>,
}

impl Scopes {
    fn new() -> Self {
        let mut s = Self {
            stack: Vec::new(),
            members: HashMap::new(),
        };
        s.push();
        s
    }
    fn declare_member(&mut self, start_byte: usize, id: SymbolId) {
        self.members.insert(start_byte, id);
    }
    fn push(&mut self) {
        self.stack.push(HashMap::new());
    }
    fn pop(&mut self) {
        self.stack.pop();
    }
    /// Nesting depth: 1 at module level, more inside functions/classes.
    fn depth(&self) -> usize {
        self.stack.len()
    }
    fn declare(&mut self, name: &str, binding: Binding) {
        if let Some(top) = self.stack.last_mut() {
            top.insert(name.to_string(), binding);
        }
    }
    fn resolve(&self, name: &str) -> Option<Binding> {
        for scope in self.stack.iter().rev() {
            if let Some(b) = scope.get(name) {
                return Some(*b);
            }
        }
        None
    }
}

pub struct FileResult {
    pub module_id: SymbolId,
    pub symbols: Vec<Symbol>,
    pub refs: Vec<UnresolvedRef>,
    pub imports: Vec<ImportDecl>,
    /// Declarations this file exposes to importers: `export function/class/const ...` and
    /// the CommonJS forms (`module.exports = ...`, `exports.x = ...`). Used to make a
    /// module's exports reachable when the module is used as a whole or is a public API
    /// entrypoint.
    pub exported: Vec<SymbolId>,
    pub components: Vec<ReactComponent>,
    pub jsx_usages: Vec<JsxUsage>,
    pub hook_calls: Vec<HookCall>,
    pub vue_component: Option<VueComponent>,
    pub vue_global_registrations: Vec<VueGlobalRegistration>,
    pub vue_async_components: Vec<VueAsyncComponent>,
    /// True if this file calls `createApp(...).mount(...)` -- Vue's
    /// bootstrap convention, used to mark it as an entrypoint independent
    /// of its filename/`package.json` (see [`vue::has_vue_app_mount`]).
    pub vue_app_mount: bool,
    /// `import.meta.glob(...)` / `require.context(...)` bulk imports (see [`crate::globs`]).
    pub module_globs: Vec<crate::globs::ModuleGlob>,
}

pub struct ImportDecl {
    pub symbol_id: SymbolId,
    /// The name to look up in the source module. `"default"` for a default import (or a
    /// plain `require`), `"*"` for a namespace import / `export * from`.
    pub imported_name: String,
    /// The name this file binds it to (`import { a as b }` -> `"b"`).
    pub local_name: String,
    pub source: String,
    pub is_default: bool,
    /// Runtime this import is scoped to, if declared inside an environment guard.
    pub environment: Environment,
}

pub enum RefTarget {
    Symbol(SymbolId),
    Name(String),
    /// A property access (`obj.run`, `this.#cache`): resolved by *member name* to every
    /// class member (`Service.run`) or exported symbol (`run`) with that short name. Without
    /// type information the receiver is unknown, so the conservative reading is that any
    /// access of that name uses all of them.
    Member(String),
    /// Executing another module for its side effects (`import './polyfills'`,
    /// `const x = require('./m')`): the module becomes reachable, its exports are not
    /// thereby used.
    Module(String),
    /// Using another module's namespace as a value (`import('./page')`, a bare
    /// `require('./routes')` expression): the module and all its exports are used.
    ModuleNamespace(String),
}

pub struct UnresolvedRef {
    pub from: SymbolId,
    pub target: RefTarget,
    pub kind: RefKind,
    pub environment: Environment,
}

fn node_text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    let s = node.utf8_text(src).unwrap_or("");
    s.trim()
}

fn span(node: Node) -> SourceSpan {
    let s = node.start_position();
    let e = node.end_position();
    SourceSpan::new(s.row + 1, s.column + 1, e.row + 1, e.column + 1)
}

fn symbol(
    id: SymbolId,
    name: &str,
    kind: SymbolKind,
    path: &Path,
    node: Node,
    environment: Environment,
) -> Symbol {
    Symbol {
        id,
        name: name.to_string(),
        kind,
        file: path.to_path_buf(),
        span: span(node),
        reachable: false,
        environment,
        is_external: false,
    }
}

/// True if `path` has a `node_modules` component: third-party library code
/// rather than the project being analyzed.
fn is_under_node_modules(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "node_modules")
}

fn strip_quotes(s: &str) -> String {
    s.trim_matches(['\'', '"', '`']).to_string()
}

/// Parse a single JavaScript file. `is_vue_script_setup` is true only for
/// the materialized script of a `.vue` file that has a `<script setup>`
/// block -- a signal [`detect_vue_component`] needs but can't recover from
/// the materialized text alone (materialization strips the `<script setup>`
/// tag itself, leaving no marker that the source came from one). Always
/// `false` for a plain (non-`.vue`) file.
pub fn parse_file(
    path: &Path,
    src: &str,
    module_id: SymbolId,
    next_id: &mut usize,
    is_vue_script_setup: bool,
) -> FileResult {
    let mut parser = Parser::new();
    let lang = tree_sitter_javascript::LANGUAGE.into();
    parser.set_language(&lang).expect("javascript grammar");

    let tree = parser.parse(src, None).expect("parse javascript");
    let root = tree.root_node();

    let mut scopes = Scopes::new();
    let mut symbols = Vec::new();
    let mut refs = Vec::new();
    let mut imports = Vec::new();
    let mut exported = Vec::new();

    symbols.push(symbol(
        module_id,
        &path.to_string_lossy(),
        SymbolKind::Module,
        path,
        root,
        Environment::Universal,
    ));

    collect_globals(
        root,
        path,
        src,
        next_id,
        &mut symbols,
        &mut scopes,
        &mut imports,
        &mut refs,
        &mut exported,
        Environment::Universal,
    );
    let mut env_hints: HashMap<SymbolId, Environment> = HashMap::new();
    collect_refs(root, src, module_id, &mut scopes, &mut refs, &mut env_hints);
    refs.extend(collect_react_lifecycle_edges(root, src, &symbols));

    // Symbols that bare-reference an environment-only global (outside any explicit
    // guard) inherit that signal too, so e.g. a function touching `window.foo`
    // is tagged Browser even without a `typeof window` check around it.
    for sym in symbols.iter_mut() {
        if sym.environment == Environment::Universal {
            if let Some(&hint) = env_hints.get(&sym.id) {
                sym.environment = hint;
            }
        }
    }

    // Everything declared in a node_modules file is third-party library code.
    if is_under_node_modules(path) {
        for sym in symbols.iter_mut() {
            sym.is_external = true;
        }
    }

    let (components, jsx_usages, hook_calls) = detect_react(root, path, src, module_id, &symbols);
    let vue_component =
        detect_vue_component(root, path, src, next_id, &mut symbols, is_vue_script_setup);
    let mut vue_global_registrations = Vec::new();
    collect_vue_global_registrations(root, src, &mut vue_global_registrations);
    let vue_async_components = detect_vue_async_components(root, src);
    let vue_app_mount = vue::has_vue_app_mount(root, src);
    if vue_app_mount {
        // The module symbol is always pushed first, above. Mounting to the
        // DOM is inherently a browser-only operation, so the whole file
        // gets tagged -- same "environment inference" vocabulary as
        // `env_of_global`'s `window`/`document` guard detection, just from
        // a Vue-specific signal instead of a bare global reference.
        symbols[0].environment = Environment::Browser;
    }
    // A `.vue` file's component *is* its default export.
    if let Some(component) = &vue_component {
        exported.push(component.symbol_id);
    }

    FileResult {
        module_id,
        symbols,
        refs,
        imports,
        exported,
        components,
        jsx_usages,
        hook_calls,
        vue_component,
        vue_global_registrations,
        vue_async_components,
        vue_app_mount,
        module_globs: crate::globs::collect_module_globs(root, src),
    }
}

/// True if `node`'s subtree contains a JSX element anywhere -- i.e. this is
/// (part of) something that renders.
fn contains_jsx(node: Node) -> bool {
    if matches!(node.kind(), "jsx_element" | "jsx_self_closing_element") {
        return true;
    }
    let mut c = node.walk();
    let found = node.named_children(&mut c).any(contains_jsx);
    found
}

/// Unwrap `forwardRef(...)`/`memo(...)` (bare or namespaced, e.g.
/// `React.memo`) down to the function/arrow-function component definition
/// inside -- or `value` itself, if it's already a bare function. These
/// wrap a component without changing what it *is*: `memo(forwardRef(...))`
/// is a common real combination, so this recurses to unwrap both layers.
/// Returns `None` for anything else (a plain non-component value).
fn unwrap_component_wrapper<'a>(value: Node<'a>, src: &[u8]) -> Option<Node<'a>> {
    match value.kind() {
        "arrow_function" | "function_expression" => Some(value),
        "call_expression" => {
            let func = value.child_by_field_name("function")?;
            let tail = match func.kind() {
                "identifier" => func,
                "member_expression" => func.child_by_field_name("property")?,
                _ => return None,
            };
            if !matches!(node_text(tail, src), "forwardRef" | "memo") {
                return None;
            }
            let args = value.child_by_field_name("arguments")?;
            unwrap_component_wrapper(args.named_child(0)?, src)
        }
        _ => None,
    }
}

/// React lifecycle methods React itself invokes on a class component instance
/// through its own reconciler -- never through a call written in the analyzed
/// codebase. See the `class_declaration` arm of `walk_node`, which uses this
/// to add an explicit reachability edge for each one a class component
/// defines (mirroring the TS analyzer's Angular DI edges: same "framework
/// calls this via reflection, not our code" problem).
const REACT_LIFECYCLE_METHODS: &[&str] = &[
    "render",
    "componentDidMount",
    "componentDidUpdate",
    "componentWillUnmount",
    "shouldComponentUpdate",
    "getSnapshotBeforeUpdate",
    "componentDidCatch",
    "getDerivedStateFromProps",
    "UNSAFE_componentWillMount",
    "UNSAFE_componentWillReceiveProps",
    "UNSAFE_componentWillUpdate",
];

/// True if `class_node`'s heritage clause extends `Component`/`PureComponent`,
/// bare or namespaced (`React.Component`, `Preact.PureComponent`, ...).
/// Matches on the trailing identifier only, since the namespace varies with
/// how React (or a compatible library, e.g. Preact) was imported. `extends`
/// itself is `class_heritage`'s one positional child -- not a named field on
/// `class_declaration` -- confirmed via this grammar's node-types.json.
fn extends_react_component(class_node: Node, src: &[u8]) -> bool {
    let mut c = class_node.walk();
    let Some(heritage) = class_node
        .named_children(&mut c)
        .find(|n| n.kind() == "class_heritage")
    else {
        return false;
    };
    let Some(expr) = heritage.named_child(0) else {
        return false;
    };
    let tail = match expr.kind() {
        "identifier" => expr,
        "member_expression" => expr.child_by_field_name("property").unwrap_or(expr),
        _ => return false,
    };
    matches!(node_text(tail, src), "Component" | "PureComponent")
}

/// Find `class_node`'s `render()` method body, if it has one.
fn class_render_body<'a>(class_node: Node<'a>, src: &[u8]) -> Option<Node<'a>> {
    let body = class_node.child_by_field_name("body")?;
    let mut c = body.walk();
    let render = body.named_children(&mut c).find(|m| {
        m.kind() == "method_definition"
            && m.child_by_field_name("name")
                .map(|n| node_text(n, src) == "render")
                .unwrap_or(false)
    })?;
    render.child_by_field_name("body")
}

/// Extract prop names destructured from a component's first parameter
/// (`{ label, onClick, disabled = false }`). Returns empty if the parameter isn't
/// a destructuring pattern (e.g. a bare `props`), since we then can't statically
/// enumerate what's read off it.
fn destructured_prop_names(params: Node, src: &[u8]) -> Vec<String> {
    let mut c = params.walk();
    let Some(first) = params.named_children(&mut c).next() else {
        return Vec::new();
    };
    if first.kind() != "object_pattern" {
        return Vec::new();
    }
    let mut names = Vec::new();
    let mut pc = first.walk();
    for prop in first.named_children(&mut pc) {
        match prop.kind() {
            "shorthand_property_identifier_pattern" => {
                names.push(node_text(prop, src).to_string());
            }
            "pair_pattern" => {
                if let Some(key) = prop.child_by_field_name("key") {
                    names.push(node_text(key, src).to_string());
                }
            }
            "object_assignment_pattern" => {
                // `{ label = 'default' }`
                if let Some(left) = prop.child_by_field_name("left") {
                    if left.kind() == "shorthand_property_identifier_pattern" {
                        names.push(node_text(left, src).to_string());
                    }
                }
            }
            _ => {}
        }
    }
    names
}

/// Add a reachability edge from a class component to each React lifecycle
/// method it defines (see [`REACT_LIFECYCLE_METHODS`]).
///
/// A separate pass over the raw AST, like [`detect_react`], rather than
/// threaded through [`walk_node`]/[`Scopes`]: doing it there would mean
/// resolving each method by its bare name through the same flat, file-wide
/// scope table [`collect_globals`] declares all class methods into, which is
/// ambiguous the moment two classes in the file share a lifecycle method name
/// (e.g. two components both defining `render`) -- last declaration wins,
/// silently wiring the edge to the wrong class's method. Resolving by the
/// method's exact `"ClassName.methodName"` symbol name instead sidesteps that
/// entirely, since dotted names are always unique.
fn collect_react_lifecycle_edges(root: Node, src: &str, symbols: &[Symbol]) -> Vec<UnresolvedRef> {
    let mut by_full_name: HashMap<&str, SymbolId> = HashMap::new();
    for s in symbols {
        by_full_name.entry(s.name.as_str()).or_insert(s.id);
    }
    let mut edges = Vec::new();
    walk_for_class_components(root, src, &by_full_name, &mut edges);
    edges
}

fn walk_for_class_components(
    node: Node,
    src: &str,
    by_full_name: &HashMap<&str, SymbolId>,
    edges: &mut Vec<UnresolvedRef>,
) {
    if node.kind() == "class_declaration" {
        if let Some(name_node) = node.child_by_field_name("name") {
            let class_name = node_text(name_node, src.as_bytes());
            if extends_react_component(node, src.as_bytes()) {
                if let (Some(&class_id), Some(body)) = (
                    by_full_name.get(class_name),
                    node.child_by_field_name("body"),
                ) {
                    let mut c = body.walk();
                    for method in body.named_children(&mut c) {
                        if method.kind() != "method_definition" {
                            continue;
                        }
                        let Some(method_name_node) = method.child_by_field_name("name") else {
                            continue;
                        };
                        let method_name = node_text(method_name_node, src.as_bytes());
                        if !REACT_LIFECYCLE_METHODS.contains(&method_name) {
                            continue;
                        }
                        let full_name = format!("{class_name}.{method_name}");
                        if let Some(&method_id) = by_full_name.get(full_name.as_str()) {
                            edges.push(UnresolvedRef {
                                from: class_id,
                                target: RefTarget::Symbol(method_id),
                                kind: RefKind::Static,
                                environment: Environment::Universal,
                            });
                        }
                    }
                }
            }
        }
    }
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        walk_for_class_components(child, src, by_full_name, edges);
    }
}

/// Detect React components, JSX usage sites, and hook calls in one file.
///
/// This is deliberately a separate, simpler pass rather than folded into
/// [`collect_globals`]/[`collect_refs`]: it needs no scope/binding tracking, just
/// structural pattern matching plus "which named declaration am I inside", so
/// keeping it isolated avoids adding more parameters to the already-complex
/// scope-resolution recursion.
fn detect_react(
    root: Node,
    path: &Path,
    src: &str,
    module_id: SymbolId,
    symbols: &[Symbol],
) -> (Vec<ReactComponent>, Vec<JsxUsage>, Vec<HookCall>) {
    // Name -> id for this file's top-level declarations, so JSX/hook detection
    // (which doesn't track scope) can still attribute things to the right symbol.
    let mut name_to_id: HashMap<String, SymbolId> = HashMap::new();
    for s in symbols {
        if s.kind != SymbolKind::Module {
            name_to_id.entry(s.name.clone()).or_insert(s.id);
        }
    }

    let mut components = Vec::new();

    // Component detection: top-level (and exported) function/const declarations
    // whose name is PascalCase and whose body renders JSX.
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        let decl = if child.kind() == "export_statement" {
            child.child_by_field_name("declaration").unwrap_or(child)
        } else {
            child
        };
        match decl.kind() {
            "function_declaration" => {
                if let (Some(name_node), Some(body)) = (
                    decl.child_by_field_name("name"),
                    decl.child_by_field_name("body"),
                ) {
                    let name = node_text(name_node, src.as_bytes()).to_string();
                    if is_component_name(&name) && contains_jsx(body) {
                        if let Some(&id) = name_to_id.get(&name) {
                            let declared_props = decl
                                .child_by_field_name("parameters")
                                .map(|p| destructured_prop_names(p, src.as_bytes()))
                                .unwrap_or_default();
                            components.push(ReactComponent {
                                symbol_id: id,
                                name,
                                file: path.to_path_buf(),
                                line: span(decl).start_line,
                                declared_props,
                            });
                        }
                    }
                }
            }
            "class_declaration" => {
                if let Some(name_node) = decl.child_by_field_name("name") {
                    let name = node_text(name_node, src.as_bytes()).to_string();
                    if is_component_name(&name) && extends_react_component(decl, src.as_bytes()) {
                        let has_jsx = class_render_body(decl, src.as_bytes())
                            .map(contains_jsx)
                            .unwrap_or(false);
                        if has_jsx {
                            if let Some(&id) = name_to_id.get(&name) {
                                // Props aren't statically enumerable for class
                                // components the way a destructured parameter
                                // is -- they're read off `this.props.x` inside
                                // the class body, which would need deeper flow
                                // analysis. Same "can't enumerate" case as a
                                // function component taking a bare `props`.
                                components.push(ReactComponent {
                                    symbol_id: id,
                                    name,
                                    file: path.to_path_buf(),
                                    line: span(decl).start_line,
                                    declared_props: Vec::new(),
                                });
                            }
                        }
                    }
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                let mut dc = decl.walk();
                for d in decl.named_children(&mut dc) {
                    if d.kind() != "variable_declarator" {
                        continue;
                    }
                    let (Some(name_node), Some(value)) = (
                        d.child_by_field_name("name"),
                        d.child_by_field_name("value"),
                    ) else {
                        continue;
                    };
                    let Some(func) = unwrap_component_wrapper(value, src.as_bytes()) else {
                        continue;
                    };
                    let name = node_text(name_node, src.as_bytes()).to_string();
                    let body_has_jsx = func
                        .child_by_field_name("body")
                        .map(contains_jsx)
                        .unwrap_or(false);
                    if is_component_name(&name) && body_has_jsx {
                        if let Some(&id) = name_to_id.get(&name) {
                            let declared_props = func
                                .child_by_field_name("parameters")
                                .map(|p| destructured_prop_names(p, src.as_bytes()))
                                .unwrap_or_default();
                            components.push(ReactComponent {
                                symbol_id: id,
                                name,
                                file: path.to_path_buf(),
                                line: span(d).start_line,
                                declared_props,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let mut jsx_usages = Vec::new();
    let mut hook_calls = Vec::new();
    scan_jsx_and_hooks(
        root,
        path,
        src,
        &name_to_id,
        module_id,
        &mut jsx_usages,
        &mut hook_calls,
    );

    (components, jsx_usages, hook_calls)
}

/// Detect this file's Vue component: the Options API's `export default {
/// name, props, emits, ... }` object, and/or `<script setup>`'s
/// `defineProps(...)`/`defineEmits(...)` compiler macros. Gated on `.vue`
/// file extension -- an Options-API-shaped object literal (`{ props: [...],
/// data() {...} }`) is plain JS syntax otherwise, and not a reliable enough
/// signal to detect outside an actual SFC (unlike JSX, which React detection
/// can key off directly). Mints a new `Const` symbol for the component when
/// nothing already names it (the default-export object and the implicit
/// `<script setup>` component are both anonymous), so it participates in the
/// ordinary symbol/reference graph -- the one way this differs from
/// [`detect_react`], where a component is always an already-named
/// declaration and never needs a symbol minted for it.
///
/// One Vue-specific limitation, not shared with `detect_react`: only the
/// plain-JavaScript runtime forms of `defineProps`/`defineEmits` (an array
/// or object literal argument) are handled here. TypeScript's
/// `defineProps<T>()` generic-type-argument form needs the TS grammar's
/// `object_type`/`property_signature` nodes, which don't exist in plain
/// JavaScript source at all -- see `spectograph_ts::parse`'s own
/// `detect_vue_component` for that half.
///
/// `is_vue_script_setup` covers a `<script setup>` block that declares no
/// props/emits at all (a component with none is completely ordinary --
/// e.g. most root `App.vue` files): without this signal, such a file would
/// have neither an Options-API object nor a `defineProps`/`defineEmits` call
/// to anchor a component on, and would wrongly be detected as having none.
fn detect_vue_component(
    root: Node,
    path: &Path,
    src: &str,
    next_id: &mut usize,
    symbols: &mut Vec<Symbol>,
    is_vue_script_setup: bool,
) -> Option<VueComponent> {
    if path.extension().and_then(|e| e.to_str()) != Some("vue") {
        return None;
    }

    let mut declared_props = Vec::new();
    let mut declared_emits = Vec::new();
    let mut registered_components: Vec<(String, String)> = Vec::new();
    let mut inherits_bindings = false;
    let mut model_names: Vec<String> = Vec::new();
    let mut saw_props_macro = false;
    let mut saw_emits_macro = false;
    let mut explicit_name: Option<String> = None;
    let mut anchor: Option<Node> = None;

    // Options API: `export default { name: 'Foo', props: [...], emits: [...], ... }`.
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "export_statement" {
            continue;
        }
        let Some(value) = child
            .child_by_field_name("value")
            .and_then(|v| vue::component_options_object(v, src.as_bytes()))
        else {
            continue;
        };
        anchor = Some(child);
        let mut oc = value.walk();
        for prop in value.named_children(&mut oc) {
            if prop.kind() != "pair" {
                continue;
            }
            let (Some(key), Some(prop_value)) = (
                prop.child_by_field_name("key"),
                prop.child_by_field_name("value"),
            ) else {
                continue;
            };
            match node_text(key, src.as_bytes()) {
                "name" if prop_value.kind() == "string" => {
                    explicit_name = prop_value
                        .named_child(0)
                        .map(|n| node_text(n, src.as_bytes()).to_string());
                }
                "props" => {
                    declared_props =
                        vue::extract_names_from_array_or_object(prop_value, src.as_bytes());
                }
                "emits" => {
                    declared_emits =
                        vue::extract_names_from_array_or_object(prop_value, src.as_bytes());
                }
                "components" => {
                    registered_components.extend(vue::extract_component_registrations(
                        prop_value,
                        src.as_bytes(),
                    ));
                }
                // Props/emits merged in from elsewhere: the component's own lists are only
                // part of what it accepts.
                "mixins" | "extends" => inherits_bindings = true,
                _ => {}
            }
        }
    }

    // Composition API: `<script setup>`'s `defineProps(...)`/`defineEmits(...)`
    // compiler macros -- callable bare (no import) anywhere at the module's
    // top level. Their mere presence, regardless of which original `<script>`
    // block contributed these lines after SFC materialization, is what marks
    // this file as using `<script setup>`: Vue itself only allows them there.
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        let Some(call) = top_level_call(child) else {
            continue;
        };
        let Some(func) = call.child_by_field_name("function") else {
            continue;
        };
        if func.kind() != "identifier" {
            continue;
        }
        // `defineModel()` / `defineModel('name', opts)`: an implicit prop plus its
        // `update:<name>` event (`v-model` / `v-model:name` on the parent side).
        if node_text(func, src.as_bytes()) == "defineModel" {
            anchor.get_or_insert(child);
            let name = call
                .child_by_field_name("arguments")
                .and_then(|a| a.named_child(0))
                .filter(|a| a.kind() == "string")
                .and_then(|a| a.named_child(0))
                .map(|n| node_text(n, src.as_bytes()).to_string())
                .unwrap_or_else(|| "modelValue".to_string());
            model_names.push(name);
            continue;
        }
        let is_props = node_text(func, src.as_bytes()) == "defineProps";
        let is_emits = node_text(func, src.as_bytes()) == "defineEmits";
        if !is_props && !is_emits {
            continue;
        }
        anchor.get_or_insert(child);
        if is_props {
            saw_props_macro = true;
        } else {
            saw_emits_macro = true;
        }
        let Some(names) = call
            .child_by_field_name("arguments")
            .and_then(|a| a.named_child(0))
            .map(|arg| vue::extract_names_from_array_or_object(arg, src.as_bytes()))
        else {
            continue;
        };
        if is_props {
            declared_props = names;
        } else {
            declared_emits = names;
        }
    }

    // A bare `<script setup>` with no macro calls and no Options-API object
    // still declares a component -- Vue's own compiler doesn't need any
    // particular statement inside the block to treat it as one, so this
    // falls back to the whole file as the anchor.
    let anchor = anchor.or(is_vue_script_setup.then_some(root))?;
    // `defineModel` adds a prop and an event. Merge them into a list we already know; with no
    // `defineProps`/`defineEmits` at all they *are* the whole list; but next to a macro whose
    // contents we couldn't enumerate (empty = unknown) they'd make an incomplete list look
    // authoritative, so leave that unknown.
    if !model_names.is_empty() {
        if declared_props.is_empty() && !saw_props_macro || !declared_props.is_empty() {
            declared_props.extend(model_names.iter().cloned());
        }
        if declared_emits.is_empty() && !saw_emits_macro || !declared_emits.is_empty() {
            declared_emits.extend(model_names.iter().map(|n| format!("update:{n}")));
        }
    }
    if inherits_bindings {
        // "Empty means unknown": a mixin/`extends` contributes props and emits we can't see,
        // so don't report bindings as undeclared.
        declared_props.clear();
        declared_emits.clear();
    }
    let name = explicit_name.unwrap_or_else(|| vue::component_name_from_path(path));
    let id = *next_id;
    *next_id += 1;
    symbols.push(symbol(
        id,
        &name,
        SymbolKind::Const,
        path,
        anchor,
        Environment::Universal,
    ));

    Some(VueComponent {
        symbol_id: id,
        name,
        file: path.to_path_buf(),
        line: span(anchor).start_line,
        declared_props,
        declared_emits,
        registered_components,
    })
}

/// If `node` is a top-level statement wrapping a single bare call expression
/// -- `defineProps(...)` as its own statement, or `const props =
/// defineProps(...)` -- return that call. Vue's `<script setup>` macros are
/// used almost exclusively in one of these two shapes.
fn top_level_call(node: Node) -> Option<Node> {
    match node.kind() {
        "expression_statement" => {
            let expr = node.named_child(0)?;
            (expr.kind() == "call_expression").then_some(expr)
        }
        "lexical_declaration" | "variable_declaration" => {
            let mut c = node.walk();
            for d in node.named_children(&mut c) {
                if d.kind() != "variable_declarator" {
                    continue;
                }
                if let Some(value) = d.child_by_field_name("value") {
                    if value.kind() == "call_expression" {
                        return Some(value);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Recursively collect `<expr>.component('Name', LocalIdent)` calls anywhere
/// in the file -- Vue's global component registration API, most often found
/// nested in a bootstrap chain (`createApp(App).component('Foo',
/// Foo).mount('#app')`), not as its own top-level statement, so this walks
/// the whole tree rather than reusing [`top_level_call`]'s shallower search.
/// See [`VueGlobalRegistration`] for the matching caveats.
fn collect_vue_global_registrations(node: Node, src: &str, out: &mut Vec<VueGlobalRegistration>) {
    if node.kind() == "call_expression" {
        if let Some(reg) = vue_global_registration_call(node, src) {
            out.push(reg);
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_vue_global_registrations(child, src, out);
    }
}

/// If `call` is `<expr>.component('Name', LocalIdent)` -- a member call
/// named `component`, taking exactly a string literal and a bare identifier
/// -- extract the registration. `LocalIdent` (not an arbitrary expression)
/// is required since anything else can't be resolved to a component symbol
/// without deeper flow analysis, matching this module's general "can't
/// statically enumerate, don't guess" convention.
fn vue_global_registration_call(call: Node, src: &str) -> Option<VueGlobalRegistration> {
    let func = call.child_by_field_name("function")?;
    if func.kind() != "member_expression" {
        return None;
    }
    let property = func.child_by_field_name("property")?;
    if node_text(property, src.as_bytes()) != "component" {
        return None;
    }
    let args = call.child_by_field_name("arguments")?;
    let mut ac = args.walk();
    let named: Vec<Node> = args.named_children(&mut ac).collect();
    let [name_arg, ident_arg] = named.as_slice() else {
        return None;
    };
    if name_arg.kind() != "string" || ident_arg.kind() != "identifier" {
        return None;
    }
    let tag_name = name_arg
        .named_child(0)
        .map(|n| node_text(n, src.as_bytes()).to_string())?;
    Some(VueGlobalRegistration {
        tag_name,
        local_identifier: node_text(*ident_arg, src.as_bytes()).to_string(),
    })
}

/// Find every top-level `const X = defineAsyncComponent(<factory>)`
/// binding, where `<factory>` is an arrow function or function expression
/// whose body is (or directly returns) a dynamic `import('...')` call.
/// Unlike [`collect_vue_global_registrations`], this only looks at top-level
/// declarations -- `defineAsyncComponent`'s whole purpose is declaring a
/// component binding, so (unlike global registration, commonly buried in a
/// bootstrap chain) it's never meaningfully nested.
fn detect_vue_async_components(root: Node, src: &str) -> Vec<VueAsyncComponent> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if !matches!(child.kind(), "lexical_declaration" | "variable_declaration") {
            continue;
        }
        let mut dc = child.walk();
        for d in child.named_children(&mut dc) {
            if d.kind() != "variable_declarator" {
                continue;
            }
            let (Some(name_node), Some(value)) = (
                d.child_by_field_name("name"),
                d.child_by_field_name("value"),
            ) else {
                continue;
            };
            if name_node.kind() != "identifier" {
                continue;
            }
            if let Some(source) = async_component_import_source(value, src) {
                out.push(VueAsyncComponent {
                    local_name: node_text(name_node, src.as_bytes()).to_string(),
                    source,
                });
            }
        }
    }
    out
}

/// If `value` is `defineAsyncComponent(<factory>)` and `<factory>`'s body
/// is (or directly `return`s) a dynamic `import('...')` call, return the
/// specifier. Doesn't handle `defineAsyncComponent({ loader: () => ...
/// })`'s advanced options-object form -- only the common bare-factory
/// shorthand, same "handle the common shape, not every shape" scoping as
/// the rest of this module's Vue support.
fn async_component_import_source(value: Node, src: &str) -> Option<String> {
    if value.kind() != "call_expression" {
        return None;
    }
    let func = value.child_by_field_name("function")?;
    if func.kind() != "identifier" || node_text(func, src.as_bytes()) != "defineAsyncComponent" {
        return None;
    }
    let factory = value.child_by_field_name("arguments")?.named_child(0)?;
    if !matches!(factory.kind(), "arrow_function" | "function_expression") {
        return None;
    }
    let body = factory.child_by_field_name("body")?;
    let call = if body.kind() == "call_expression" {
        body
    } else if body.kind() == "statement_block" {
        let mut bc = body.walk();
        let mut found = None;
        for n in body.named_children(&mut bc) {
            if n.kind() == "return_statement" {
                found = n.named_child(0).filter(|c| c.kind() == "call_expression");
                break;
            }
        }
        found?
    } else {
        return None;
    };
    if call.child_by_field_name("function")?.kind() != "import" {
        return None;
    }
    let arg = call.child_by_field_name("arguments")?.named_child(0)?;
    if arg.kind() != "string" {
        return None;
    }
    arg.named_child(0)
        .map(|n| node_text(n, src.as_bytes()).to_string())
}

/// Recursively collect JSX usage sites (`<Component prop={x} />`) and hook calls
/// (`useXxx(...)`), tracking which named function declaration each is nested in so
/// hook calls can be attributed to the right component/custom hook.
#[allow(clippy::too_many_arguments)]
fn scan_jsx_and_hooks(
    node: Node,
    path: &Path,
    src: &str,
    name_to_id: &HashMap<String, SymbolId>,
    enclosing: SymbolId,
    jsx_usages: &mut Vec<JsxUsage>,
    hook_calls: &mut Vec<HookCall>,
) {
    match node.kind() {
        "variable_declarator" => {
            if let Some(value) = node.child_by_field_name("value") {
                if let Some(func) = unwrap_component_wrapper(value, src.as_bytes()) {
                    let new_enclosing = node
                        .child_by_field_name("name")
                        .map(|n| node_text(n, src.as_bytes()).to_string())
                        .and_then(|name| name_to_id.get(&name).copied())
                        .unwrap_or(enclosing);
                    scan_jsx_and_hooks(
                        func,
                        path,
                        src,
                        name_to_id,
                        new_enclosing,
                        jsx_usages,
                        hook_calls,
                    );
                    return;
                }
            }
        }
        "function_declaration" | "method_definition" => {
            let new_enclosing = node
                .child_by_field_name("name")
                .map(|n| node_text(n, src.as_bytes()).to_string())
                .and_then(|name| name_to_id.get(&name).copied())
                .unwrap_or(enclosing);
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                scan_jsx_and_hooks(
                    child,
                    path,
                    src,
                    name_to_id,
                    new_enclosing,
                    jsx_usages,
                    hook_calls,
                );
            }
            return;
        }
        "class_declaration" => {
            // Everything in a class component's body -- `render()`, any other
            // method -- is attributed to the class itself (its own
            // `ReactComponent`, if it has one), not to the individual method:
            // there's no separate per-method component to attach to, unlike a
            // top-level function/arrow component. A method's bare name (e.g.
            // "render") isn't registered in `name_to_id` under that bare form
            // (class methods are named "ClassName.method" there), so the
            // nested "method_definition" arm's own lookup will miss and fall
            // back to whatever `enclosing` we set here -- composes correctly
            // without needing a special case there.
            let new_enclosing = node
                .child_by_field_name("name")
                .map(|n| node_text(n, src.as_bytes()).to_string())
                .and_then(|name| name_to_id.get(&name).copied())
                .unwrap_or(enclosing);
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                scan_jsx_and_hooks(
                    child,
                    path,
                    src,
                    name_to_id,
                    new_enclosing,
                    jsx_usages,
                    hook_calls,
                );
            }
            return;
        }
        "jsx_opening_element" | "jsx_self_closing_element" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                // A namespaced tag (`<Menu.Item/>`, `<Nav.Menu.Item/>`) parses
                // its name as a `member_expression`, not a plain `identifier`
                // -- `node_text` on it yields the exact dotted source text
                // ("Menu.Item"), which is already what we want, so no
                // separate reconstruction is needed.
                if matches!(name_node.kind(), "identifier" | "member_expression") {
                    let name = node_text(name_node, src.as_bytes()).to_string();
                    if is_component_name(&name) {
                        let mut passed_props = Vec::new();
                        let mut ac = node.walk();
                        for attr in node.children_by_field_name("attribute", &mut ac) {
                            if attr.kind() == "jsx_attribute" {
                                if let Some(prop_name) = attr.named_child(0) {
                                    passed_props
                                        .push(node_text(prop_name, src.as_bytes()).to_string());
                                }
                            }
                        }
                        jsx_usages.push(JsxUsage {
                            component_name: name,
                            file: path.to_path_buf(),
                            line: span(node).start_line,
                            passed_props,
                        });
                    }
                }
            }
        }
        "call_expression" => {
            if let Some(func) = node.child_by_field_name("function") {
                if func.kind() == "identifier" {
                    let name = node_text(func, src.as_bytes()).to_string();
                    if is_hook_name(&name) {
                        hook_calls.push(HookCall {
                            enclosing,
                            hook_name: name,
                            file: path.to_path_buf(),
                            line: span(node).start_line,
                        });
                    }
                }
            }
        }
        _ => {}
    }
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        scan_jsx_and_hooks(
            child, path, src, name_to_id, enclosing, jsx_usages, hook_calls,
        );
    }
}

/// The module specifier of a `require('x')` call with a string-literal argument.
fn require_literal(call: Node, src: &str) -> Option<String> {
    if call.kind() != "call_expression" {
        return None;
    }
    let func = call.child_by_field_name("function")?;
    if func.kind() != "identifier" || node_text(func, src.as_bytes()) != "require" {
        return None;
    }
    let arg = call.child_by_field_name("arguments")?.named_child(0)?;
    if arg.kind() != "string" {
        return None;
    }
    let spec = strip_quotes(node_text(arg, src.as_bytes()));
    (!spec.is_empty()).then_some(spec)
}

/// Declare one top-level declaration's symbols (`function`, `class` plus its methods,
/// `const`/`let`/`var` bindings including CommonJS `require` imports), plus every
/// ES-module and CommonJS export form. Declarations reached through `export` are recorded
/// in `exported`.
#[allow(clippy::too_many_arguments)]
fn collect_globals(
    root: Node,
    path: &Path,
    src: &str,
    next_id: &mut usize,
    symbols: &mut Vec<Symbol>,
    scopes: &mut Scopes,
    imports: &mut Vec<ImportDecl>,
    refs: &mut Vec<UnresolvedRef>,
    exported: &mut Vec<SymbolId>,
    env: Environment,
) {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        collect_statement(
            child, path, src, next_id, symbols, scopes, imports, refs, exported, env,
        );
    }
}

/// Symbol kinds that are real declarations (as opposed to imports, exports, modules).
fn is_declaration(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Function | SymbolKind::Class | SymbolKind::Const
    )
}

#[allow(clippy::too_many_arguments)]
fn collect_statement(
    statement: Node,
    path: &Path,
    src: &str,
    next_id: &mut usize,
    symbols: &mut Vec<Symbol>,
    scopes: &mut Scopes,
    imports: &mut Vec<ImportDecl>,
    refs: &mut Vec<UnresolvedRef>,
    exported: &mut Vec<SymbolId>,
    env: Environment,
) {
    // `module.exports = x;` and friends are expression statements wrapping the assignment.
    let child = if statement.kind() == "expression_statement" {
        statement.named_child(0).unwrap_or(statement)
    } else {
        statement
    };
    let before = symbols.len();
    let mut is_export = false;

    // CommonJS export symbols (`exports.foo = ...`, `module.exports = { foo }`) are
    // *properties* of the module object, not bindings: they get a symbol but are
    // deliberately not declared in `scopes`. Declaring them would shadow a local variable of
    // the same name (`var Router = require('router'); exports.Router = Router;`), so uses of
    // `Router` would resolve to the export instead of the import.
    match child.kind() {
        "if_statement" => {
            handle_env_if(
                child, path, src, next_id, symbols, scopes, imports, refs, exported,
            );
        }
        "function_declaration" | "generator_function_declaration" => {
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = node_text(name_node, src.as_bytes()).to_string();
                let id = *next_id;
                *next_id += 1;
                scopes.declare(&name, Binding::Global(id));
                symbols.push(symbol(id, &name, SymbolKind::Function, path, child, env));
            }
        }
        "export_statement" => {
            // `export function foo() {}` / `export class Foo {}` / `export const x = ...`
            // declare exactly what the same statement without `export` would.
            if let Some(decl) = child.child_by_field_name("declaration") {
                collect_statement(
                    decl, path, src, next_id, symbols, scopes, imports, refs, exported, env,
                );
                is_export = true;
            } else {
                // `export { a }`, `export default <expr>`, `export { a } from './m'`,
                // `export * from './m'`.
                handle_export_statement(child, path, src, next_id, symbols, imports, refs, env);
            }
        }
        "assignment_expression" => {
            // Handle CommonJS: module.exports = value and exports.* = value
            is_export = true;
            if let Some(left) = child.child_by_field_name("left") {
                if left.kind() == "member_expression" {
                    if let Some(object) = left.child_by_field_name("object") {
                        let object_text = node_text(object, src.as_bytes());
                        if object_text == "module" {
                            if let Some(property) = left.child_by_field_name("property") {
                                let property_text = node_text(property, src.as_bytes());
                                if property_text == "exports" {
                                    // This is module.exports = something
                                    if let Some(right) = child.child_by_field_name("right") {
                                        match right.kind() {
                                            "function" | "function_expression" => {
                                                // module.exports = function() {}
                                                let name = right
                                                    .child_by_field_name("name")
                                                    .map(|n| node_text(n, src.as_bytes()))
                                                    .unwrap_or("default")
                                                    .to_string();
                                                let id = *next_id;
                                                *next_id += 1;
                                                symbols.push(symbol(
                                                    id,
                                                    &name,
                                                    SymbolKind::Function,
                                                    path,
                                                    right,
                                                    env,
                                                ));
                                            }
                                            "class_expression" => {
                                                // module.exports = class {}
                                                let name = right
                                                    .child_by_field_name("name")
                                                    .map(|n| node_text(n, src.as_bytes()))
                                                    .unwrap_or("default")
                                                    .to_string();
                                                let id = *next_id;
                                                *next_id += 1;
                                                symbols.push(symbol(
                                                    id,
                                                    &name,
                                                    SymbolKind::Class,
                                                    path,
                                                    right,
                                                    env,
                                                ));
                                            }
                                            "object_expression" | "object" => {
                                                // module.exports = { key: value }
                                                // Parse object properties as exports
                                                let mut oc = right.walk();
                                                for prop in right.named_children(&mut oc) {
                                                    if prop.kind() == "pair" {
                                                        if let Some(key_node) =
                                                            prop.child_by_field_name("key")
                                                        {
                                                            let key =
                                                                node_text(key_node, src.as_bytes())
                                                                    .to_string();
                                                            let id = *next_id;
                                                            *next_id += 1;
                                                            symbols.push(symbol(
                                                                id,
                                                                &key,
                                                                SymbolKind::Const,
                                                                path,
                                                                prop,
                                                                env,
                                                            ));
                                                        }
                                                    }
                                                }
                                            }
                                            "identifier" => {
                                                // module.exports = existingVariable: the
                                                // symbol already exists from its declaration,
                                                // and the walk adds the reference.
                                            }
                                            _ => {
                                                // Other expressions - mark as default export
                                                let id = *next_id;
                                                *next_id += 1;
                                                symbols.push(symbol(
                                                    id,
                                                    "default",
                                                    SymbolKind::Const,
                                                    path,
                                                    right,
                                                    env,
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                        } else if object_text == "exports" {
                            // This is exports.something = value
                            if let Some(property) = left.child_by_field_name("property") {
                                let property_name = node_text(property, src.as_bytes()).to_string();
                                // Create an export symbol for this property
                                let id = *next_id;
                                *next_id += 1;
                                symbols.push(symbol(
                                    id,
                                    &property_name,
                                    SymbolKind::Const,
                                    path,
                                    child,
                                    env,
                                ));
                            }
                        }
                    }
                }
            }
        }
        "call_expression" => {
            // Handle Object.assign(exports, {...}) pattern
            is_export = true;
            if let Some(func) = child.child_by_field_name("function") {
                let fname = node_text(func, src.as_bytes());
                if fname == "Object.assign" {
                    if let Some(arg) = child.child_by_field_name("arguments") {
                        let mut c = arg.walk();
                        let args: Vec<_> = arg.named_children(&mut c).collect();
                        if args.len() >= 2 {
                            // Check if first argument is exports
                            if let Some(first_arg) = args.first() {
                                if first_arg.kind() == "identifier"
                                    && node_text(*first_arg, src.as_bytes()) == "exports"
                                {
                                    // This is Object.assign(exports, {...})
                                    if let Some(second_arg) = args.get(1) {
                                        if matches!(
                                            second_arg.kind(),
                                            "object_expression" | "object"
                                        ) {
                                            // Parse object properties as exports
                                            let mut oc = second_arg.walk();
                                            for prop in second_arg.named_children(&mut oc) {
                                                if prop.kind() == "pair" {
                                                    if let Some(key_node) =
                                                        prop.child_by_field_name("key")
                                                    {
                                                        let key =
                                                            node_text(key_node, src.as_bytes())
                                                                .to_string();
                                                        let id = *next_id;
                                                        *next_id += 1;
                                                        symbols.push(symbol(
                                                            id,
                                                            &key,
                                                            SymbolKind::Const,
                                                            path,
                                                            prop,
                                                            env,
                                                        ));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        "class_declaration" => {
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = node_text(name_node, src.as_bytes()).to_string();
                let id = *next_id;
                *next_id += 1;
                scopes.declare(&name, Binding::Global(id));
                symbols.push(symbol(id, &name, SymbolKind::Class, path, child, env));

                // Also parse methods inside the class body
                if let Some(body) = child.child_by_field_name("body") {
                    let mut bc = body.walk();
                    for method in body.named_children(&mut bc) {
                        if method.kind() == "method_definition" {
                            if let Some(method_name_node) = method.child_by_field_name("name") {
                                // Use class_name.method_name for the full identifier
                                let method_name =
                                    node_text(method_name_node, src.as_bytes()).to_string();
                                let full_name = format!("{}.{}", name, method_name);
                                let method_id = *next_id;
                                *next_id += 1;
                                scopes.declare_member(method.start_byte(), method_id);
                                symbols.push(symbol(
                                    method_id,
                                    &full_name,
                                    SymbolKind::Function,
                                    path,
                                    method,
                                    env,
                                ));
                            }
                        }
                    }
                }
            }
        }
        "lexical_declaration" | "variable_declaration" => {
            let mut c = child.walk();
            for decl in child.named_children(&mut c) {
                if decl.kind() != "variable_declarator" {
                    continue;
                }
                let Some(name_node) = decl.child_by_field_name("name") else {
                    continue;
                };
                let require_source = decl
                    .child_by_field_name("value")
                    .and_then(|v| require_literal(v, src));

                match name_node.kind() {
                    "identifier" => {
                        let name = node_text(name_node, src.as_bytes()).to_string();
                        let id = *next_id;
                        *next_id += 1;
                        let kind = if let Some(source) = require_source {
                            imports.push(ImportDecl {
                                symbol_id: id,
                                imported_name: "default".to_string(),
                                local_name: name.clone(),
                                source,
                                is_default: true,
                                environment: env,
                            });
                            SymbolKind::Import
                        } else {
                            SymbolKind::Const
                        };
                        scopes.declare(&name, Binding::Global(id));
                        symbols.push(symbol(id, &name, kind, path, name_node, env));
                    }
                    // `const { a, b: c } = require('./m')`: one import per binding.
                    "object_pattern" if require_source.is_some() => {
                        let source = require_source.unwrap_or_default();
                        let mut pc = name_node.walk();
                        for prop in name_node.named_children(&mut pc) {
                            let (imported, local, anchor) = match prop.kind() {
                                "shorthand_property_identifier_pattern" => {
                                    let n = node_text(prop, src.as_bytes()).to_string();
                                    (n.clone(), n, prop)
                                }
                                "pair_pattern" => {
                                    let key = prop.child_by_field_name("key");
                                    let value = prop.child_by_field_name("value");
                                    match (key, value) {
                                        (Some(k), Some(v)) if v.kind() == "identifier" => (
                                            node_text(k, src.as_bytes()).to_string(),
                                            node_text(v, src.as_bytes()).to_string(),
                                            v,
                                        ),
                                        _ => continue,
                                    }
                                }
                                _ => continue,
                            };
                            let id = *next_id;
                            *next_id += 1;
                            scopes.declare(&local, Binding::Global(id));
                            symbols.push(symbol(id, &local, SymbolKind::Import, path, anchor, env));
                            imports.push(ImportDecl {
                                symbol_id: id,
                                imported_name: imported,
                                local_name: local,
                                source: source.clone(),
                                is_default: false,
                                environment: env,
                            });
                        }
                    }
                    // Other destructuring patterns aren't tracked as declarations.
                    _ => {}
                }
            }
        }
        "import_statement" => {
            // `node_text` on the "source" field returns the raw string
            // literal, quote marks included (`'./utils'`, not `./utils`).
            // Left unstripped, every downstream check that expects a bare
            // path (`starts_with('.')` for relative imports, the
            // node_modules directory-name lookup for bare specifiers)
            // silently fails, so *no* ES6 `import ... from` ever resolved
            // to its target file -- only `require(...)` calls did, since
            // that path already stripped quotes.
            let mut source = child
                .child_by_field_name("source")
                .map(|s| strip_quotes(node_text(s, src.as_bytes())))
                .unwrap_or_default();
            if source.is_empty() {
                if let Some(from) = child.child_by_field_name("_from_clause") {
                    if let Some(fs) = from.child_by_field_name("source") {
                        source = strip_quotes(node_text(fs, src.as_bytes()));
                    }
                }
            }
            if source.is_empty() {
                return;
            }

            // `import_clause` is a plain typed child of `import_statement`, not a
            // named field in this grammar version -- `child_by_field_name` always
            // returns None for it, so ES6 `import ... from` bindings (default,
            // named, and namespace) never created symbols. Find it by kind instead.
            let mut import_clause = None;
            let mut ic = child.walk();
            for n in child.named_children(&mut ic) {
                if n.kind() == "import_clause" {
                    import_clause = Some(n);
                    break;
                }
            }
            let Some(clause) = import_clause else {
                // `import './polyfills'`: executed for its side effects.
                refs.push(UnresolvedRef {
                    from: symbols.first().map(|m| m.id).unwrap_or(0),
                    target: RefTarget::Module(source),
                    kind: RefKind::Static,
                    environment: env,
                });
                return;
            };
            let mut c = clause.walk();
            for n in clause.named_children(&mut c) {
                match n.kind() {
                    "identifier" => {
                        let name = node_text(n, src.as_bytes()).to_string();
                        if !name.is_empty() {
                            let id = *next_id;
                            *next_id += 1;
                            scopes.declare(&name, Binding::Global(id));
                            symbols.push(symbol(id, &name, SymbolKind::Import, path, n, env));
                            imports.push(ImportDecl {
                                symbol_id: id,
                                imported_name: "default".to_string(),
                                local_name: name.clone(),
                                source: source.clone(),
                                is_default: true,
                                environment: env,
                            });
                        }
                    }
                    "namespace_import" => {
                        // The bound name is the clause's one identifier child.
                        let name_node = n.child_by_field_name("name").or_else(|| n.named_child(0));
                        if let Some(name_node) = name_node {
                            let name = node_text(name_node, src.as_bytes()).to_string();
                            if !name.is_empty() {
                                let id = *next_id;
                                *next_id += 1;
                                scopes.declare(&name, Binding::Global(id));
                                symbols.push(symbol(
                                    id,
                                    &name,
                                    SymbolKind::Import,
                                    path,
                                    name_node,
                                    env,
                                ));
                                imports.push(ImportDecl {
                                    symbol_id: id,
                                    imported_name: "*".to_string(),
                                    local_name: name.clone(),
                                    source: source.clone(),
                                    is_default: false,
                                    environment: env,
                                });
                            }
                        }
                    }
                    "named_imports" => {
                        let mut nc = n.walk();
                        for imp in n.named_children(&mut nc) {
                            if imp.kind() != "import_specifier" {
                                continue;
                            }
                            let Some(orig_node) = imp.child_by_field_name("name") else {
                                continue;
                            };
                            let imported = node_text(orig_node, src.as_bytes()).to_string();
                            let local = imp
                                .child_by_field_name("alias")
                                .map(|x| node_text(x, src.as_bytes()).to_string())
                                .unwrap_or_else(|| imported.clone());
                            if local.is_empty() {
                                continue;
                            }
                            let id = *next_id;
                            *next_id += 1;
                            scopes.declare(&local, Binding::Global(id));
                            symbols.push(symbol(id, &local, SymbolKind::Import, path, imp, env));
                            imports.push(ImportDecl {
                                symbol_id: id,
                                imported_name: imported,
                                local_name: local,
                                source: source.clone(),
                                is_default: false,
                                environment: env,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    // Everything declared while processing an export form is part of this file's API.
    if is_export {
        exported.extend(
            symbols[before..]
                .iter()
                .filter(|s| is_declaration(s.kind))
                .map(|s| s.id),
        );
    }
}

/// Handle every `export_statement` shape that has no `declaration` field:
/// `export { a, b };` (local re-export), `export default <expr>;`,
/// `export { a } from './mod';`, `export * from './mod';`, and
/// `export * as ns from './mod';`.
#[allow(clippy::too_many_arguments)]
fn handle_export_statement(
    node: Node,
    path: &Path,
    src: &str,
    next_id: &mut usize,
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<ImportDecl>,
    refs: &mut Vec<UnresolvedRef>,
    env: Environment,
) {
    let source = node
        .child_by_field_name("source")
        .map(|s| strip_quotes(node_text(s, src.as_bytes())));

    // `export default <expr>;`
    if let Some(value) = node.child_by_field_name("value") {
        let id = *next_id;
        *next_id += 1;
        symbols.push(symbol(id, "default", SymbolKind::Export, path, node, env));
        if value.kind() == "identifier" {
            refs.push(UnresolvedRef {
                from: id,
                target: RefTarget::Name(node_text(value, src.as_bytes()).to_string()),
                kind: RefKind::ReExport,
                environment: env,
            });
        }
        return;
    }

    // `export * from './mod';` / `export * as ns from './mod';` / `export { a } from './mod';`
    if let Some(source) = &source {
        let mut has_clause = false;
        let mut cursor = node.walk();
        for n in node.named_children(&mut cursor) {
            // Named children include the "source" string itself, so `has_clause` is
            // set only for the clause kinds actually handled below -- otherwise plain
            // `export * from './mod';` (whose only named child IS that string) would
            // look like it had a clause and skip the star re-export entirely.
            match n.kind() {
                "namespace_export" => {
                    has_clause = true;
                    if let Some(name_node) = n.named_child(0) {
                        let name = node_text(name_node, src.as_bytes()).to_string();
                        let import_id = *next_id;
                        *next_id += 1;
                        symbols.push(symbol(
                            import_id,
                            &name,
                            SymbolKind::Import,
                            path,
                            name_node,
                            env,
                        ));
                        imports.push(ImportDecl {
                            symbol_id: import_id,
                            imported_name: "*".to_string(),
                            local_name: name.clone(),
                            source: source.clone(),
                            is_default: false,
                            environment: env,
                        });
                        let export_id = *next_id;
                        *next_id += 1;
                        symbols.push(symbol(
                            export_id,
                            &name,
                            SymbolKind::Export,
                            path,
                            name_node,
                            env,
                        ));
                        refs.push(UnresolvedRef {
                            from: export_id,
                            target: RefTarget::Symbol(import_id),
                            kind: RefKind::ReExport,
                            environment: env,
                        });
                    }
                }
                "export_clause" => {
                    has_clause = true;
                    handle_export_clause(
                        n,
                        path,
                        src,
                        next_id,
                        symbols,
                        Some(source),
                        imports,
                        refs,
                        env,
                    );
                }
                _ => {}
            }
        }
        if !has_clause {
            // `export * from './mod';`: no per-name tracking (the source module's own
            // export list isn't statically enumerated), but keep the module-level
            // dependency so reachability flows through re-exports.
            let import_id = *next_id;
            *next_id += 1;
            symbols.push(symbol(import_id, "*", SymbolKind::Import, path, node, env));
            imports.push(ImportDecl {
                symbol_id: import_id,
                imported_name: "*".to_string(),
                local_name: "*".to_string(),
                source: source.clone(),
                is_default: false,
                environment: env,
            });
            let export_id = *next_id;
            *next_id += 1;
            symbols.push(symbol(export_id, "*", SymbolKind::Export, path, node, env));
            refs.push(UnresolvedRef {
                from: export_id,
                target: RefTarget::Symbol(import_id),
                kind: RefKind::ReExport,
                environment: env,
            });
        }
        return;
    }

    // Plain `export { a, b };`: re-exporting names already declared in this file.
    let mut cursor = node.walk();
    for n in node.named_children(&mut cursor) {
        if n.kind() == "export_clause" {
            handle_export_clause(n, path, src, next_id, symbols, None, imports, refs, env);
        }
    }
}

/// `{ a, b as c }` in either `export { a, b as c };` or
/// `export { a, b as c } from './mod';`.
#[allow(clippy::too_many_arguments)]
fn handle_export_clause(
    clause: Node,
    path: &Path,
    src: &str,
    next_id: &mut usize,
    symbols: &mut Vec<Symbol>,
    source: Option<&String>,
    imports: &mut Vec<ImportDecl>,
    refs: &mut Vec<UnresolvedRef>,
    env: Environment,
) {
    let mut cursor = clause.walk();
    for spec in clause.named_children(&mut cursor) {
        if spec.kind() != "export_specifier" {
            continue;
        }
        let Some(name_node) = spec.child_by_field_name("name") else {
            continue;
        };
        if name_node.kind() != "identifier" {
            continue; // re-exporting a string name (rare) isn't handled yet.
        }
        let orig_name = node_text(name_node, src.as_bytes()).to_string();
        let exported_name = spec
            .child_by_field_name("alias")
            .map(|a| node_text(a, src.as_bytes()).to_string())
            .unwrap_or_else(|| orig_name.clone());

        let export_id = *next_id;
        *next_id += 1;
        symbols.push(symbol(
            export_id,
            &exported_name,
            SymbolKind::Export,
            path,
            spec,
            env,
        ));

        match source {
            Some(source) => {
                // `export { a } from './mod';`: the name is looked up in the *source*
                // module, not this one, so it needs its own Import binding for
                // cross-file resolution (mirroring a plain import).
                let import_id = *next_id;
                *next_id += 1;
                symbols.push(symbol(
                    import_id,
                    &orig_name,
                    SymbolKind::Import,
                    path,
                    name_node,
                    env,
                ));
                imports.push(ImportDecl {
                    symbol_id: import_id,
                    imported_name: orig_name.clone(),
                    local_name: orig_name.clone(),
                    source: source.clone(),
                    // `export { default } from './mod'` / `export { default as X }`.
                    is_default: orig_name == "default",
                    environment: env,
                });
                refs.push(UnresolvedRef {
                    from: export_id,
                    target: RefTarget::Symbol(import_id),
                    kind: RefKind::ReExport,
                    environment: env,
                });
            }
            None => {
                // `export { a };`: re-exporting a name declared in this file. Resolved by
                // name (a deferred lookup against the whole-program index) since the
                // statement may appear before the declaration in source order.
                refs.push(UnresolvedRef {
                    from: export_id,
                    target: RefTarget::Name(orig_name),
                    kind: RefKind::ReExport,
                    environment: env,
                });
            }
        }
    }
}

/// Handle a top-level (or else-if-chained) `if` guarded by an environment check,
/// recursing `collect_globals` into whichever branch(es) matched with the
/// appropriate [`Environment`]. Unguarded top-level `if`s are left alone, matching
/// prior behavior (declarations inside them were never hoisted as globals).
#[allow(clippy::too_many_arguments)]
fn handle_env_if(
    if_node: Node,
    path: &Path,
    src: &str,
    next_id: &mut usize,
    symbols: &mut Vec<Symbol>,
    scopes: &mut Scopes,
    imports: &mut Vec<ImportDecl>,
    refs: &mut Vec<UnresolvedRef>,
    exported: &mut Vec<SymbolId>,
) {
    let Some(condition) = if_node.child_by_field_name("condition") else {
        return;
    };
    let Some((cons_env, alt_env)) = detect_env_guard(condition, src.as_bytes()) else {
        return;
    };
    if let Some(consequence) = if_node.child_by_field_name("consequence") {
        if consequence.kind() == "statement_block" {
            collect_globals(
                consequence,
                path,
                src,
                next_id,
                symbols,
                scopes,
                imports,
                refs,
                exported,
                cons_env,
            );
        }
    }
    if let Some(alternative) = if_node.child_by_field_name("alternative") {
        if let Some(alt_body) = alternative.named_child(0) {
            match alt_body.kind() {
                "statement_block" => {
                    collect_globals(
                        alt_body, path, src, next_id, symbols, scopes, imports, refs, exported,
                        alt_env,
                    );
                }
                "if_statement" => {
                    handle_env_if(
                        alt_body, path, src, next_id, symbols, scopes, imports, refs, exported,
                    );
                }
                _ => {}
            }
        }
    }
}

fn collect_refs(
    root: Node,
    src: &str,
    module_id: SymbolId,
    scopes: &mut Scopes,
    refs: &mut Vec<UnresolvedRef>,
    env_hints: &mut HashMap<SymbolId, Environment>,
) {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        walk_node(
            child,
            src,
            module_id,
            scopes,
            refs,
            Environment::Universal,
            env_hints,
        );
    }
}

/// Record a reference from `enclosing` to whatever `name` resolves to in `scopes`. A name no
/// scope knows retries by project-wide name (a `Dynamic`, uncertain edge), and may hint at
/// the runtime (`window` => browser).
fn push_name_ref(
    name: &str,
    enclosing: SymbolId,
    scopes: &Scopes,
    refs: &mut Vec<UnresolvedRef>,
    env: Environment,
    env_hints: &mut HashMap<SymbolId, Environment>,
) {
    if let Some(detected) = env_of_global(name) {
        env_hints.entry(enclosing).or_insert(detected);
    }
    match scopes.resolve(name) {
        Some(Binding::Global(id)) if id != enclosing => {
            refs.push(UnresolvedRef {
                from: enclosing,
                target: RefTarget::Symbol(id),
                kind: RefKind::Static,
                environment: env,
            });
        }
        Some(Binding::Global(_)) | Some(Binding::Local) => {}
        _ => {
            refs.push(UnresolvedRef {
                from: enclosing,
                target: RefTarget::Name(name.to_string()),
                kind: RefKind::Dynamic,
                environment: env,
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_node(
    node: Node,
    src: &str,
    enclosing: SymbolId,
    scopes: &mut Scopes,
    refs: &mut Vec<UnresolvedRef>,
    env: Environment,
    env_hints: &mut HashMap<SymbolId, Environment>,
) {
    match node.kind() {
        "import_statement" => {
            // Bindings are declared by `collect_globals`; the module-execution edge for a
            // bare `import './x'` is added there too.
        }
        "export_statement" if node.child_by_field_name("source").is_some() => {
            // `export { a } from './mod'`: "a" names something in the *other* module, not
            // this file's scope -- handled by `handle_export_statement`.
        }
        "if_statement" => {
            let condition = node.child_by_field_name("condition");
            let guard = condition.and_then(|c| detect_env_guard(c, src.as_bytes()));
            if let Some(c) = condition {
                walk_node(c, src, enclosing, scopes, refs, env, env_hints);
            }
            let (cons_env, alt_env) = guard.unwrap_or((env, env));
            if let Some(consequence) = node.child_by_field_name("consequence") {
                walk_node(
                    consequence,
                    src,
                    enclosing,
                    scopes,
                    refs,
                    cons_env,
                    env_hints,
                );
            }
            if let Some(alternative) = node.child_by_field_name("alternative") {
                let mut c = alternative.walk();
                for child in alternative.named_children(&mut c) {
                    walk_node(child, src, enclosing, scopes, refs, alt_env, env_hints);
                }
            }
        }
        "class_declaration" => {
            let name_node = node.child_by_field_name("name");
            let def_id = resolve_def_id(node, src, scopes, enclosing);
            scopes.push();
            if let Some(n) = name_node {
                let name = node_text(n, src.as_bytes());
                if !name.is_empty() {
                    scopes.declare(name, Binding::Local);
                }
            }
            scopes.declare("this", Binding::Local);
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                if name_node.is_some_and(|n| n.id() == child.id()) {
                    continue;
                }
                if child.kind() == "class_body" {
                    let mut bc = child.walk();
                    for member in child.named_children(&mut bc) {
                        walk_node(member, src, def_id, scopes, refs, env, env_hints);
                    }
                } else {
                    // `extends Base`: the class depends on its superclass expression.
                    walk_node(child, src, def_id, scopes, refs, env, env_hints);
                }
            }
            scopes.pop();
        }
        "function_declaration"
        | "generator_function_declaration"
        | "function_expression"
        | "generator_function"
        | "arrow_function"
        | "method_definition" => {
            let name_node = node.child_by_field_name("name");
            let def_id = resolve_def_id(node, src, scopes, enclosing);
            scopes.push();
            // A function's own name is visible inside it (recursion); a method's name is
            // a property, not a binding, so it must not shadow an outer function.
            if node.kind() != "method_definition" {
                if let Some(n) = name_node {
                    scopes.declare(node_text(n, src.as_bytes()), Binding::Local);
                }
            }
            if let Some(params) = node.child_by_field_name("parameters") {
                bind_locals(params, src.as_bytes(), scopes);
            }
            // `x => x` has a single bare `parameter` instead of a parameter list.
            if let Some(param) = node.child_by_field_name("parameter") {
                bind_locals(param, src.as_bytes(), scopes);
            }
            // Parameter defaults and the body (a statement block or a bare expression like
            // `() => helper()`).
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                if name_node.is_some_and(|n| n.id() == child.id()) {
                    continue;
                }
                walk_node(child, src, def_id, scopes, refs, env, env_hints);
            }
            scopes.pop();
        }
        "assignment_expression" => {
            // Assigning to a name no scope knows declares an implicit local so later uses
            // don't become project-wide by-name lookups; an existing binding is left alone.
            if let Some(left) = node.child_by_field_name("left") {
                if left.kind() == "identifier"
                    && scopes.resolve(node_text(left, src.as_bytes())).is_none()
                {
                    bind_locals(left, src.as_bytes(), scopes);
                }
            }
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                walk_node(child, src, enclosing, scopes, refs, env, env_hints);
            }
        }
        "variable_declaration" | "lexical_declaration" => {
            // Walk each declarator's initializer only. The declarator's own name is a
            // binding site, not a use -- visiting it as a plain identifier would create
            // a spurious "declaring this counts as using it" reference (e.g. every
            // `const x = require(...)` would self-reference `x`, making unused imports
            // and unused locals impossible to ever flag as dead).
            //
            // Inside a function the declared names shadow outer ones; at module level
            // they're already the file's globals.
            let in_function = scopes.depth() > 1;
            let mut c = node.walk();
            for declarator in node.named_children(&mut c) {
                if declarator.kind() != "variable_declarator" {
                    continue;
                }
                if let Some(value) = declarator.child_by_field_name("value") {
                    if let Some(spec) = require_literal(value, src) {
                        // `const x = require('./m')`: the binding is an import (see
                        // `collect_globals`); here it only makes the module run.
                        refs.push(UnresolvedRef {
                            from: enclosing,
                            target: RefTarget::Module(spec),
                            kind: RefKind::Static,
                            environment: env,
                        });
                    } else {
                        walk_node(value, src, enclosing, scopes, refs, env, env_hints);
                    }
                }
                if let Some(name_node) = declarator.child_by_field_name("name") {
                    walk_pattern_defaults(name_node, src, enclosing, scopes, refs, env, env_hints);
                }
                if in_function {
                    if let Some(name_node) = declarator.child_by_field_name("name") {
                        bind_locals(name_node, src.as_bytes(), scopes);
                    }
                }
            }
        }
        "call_expression" => {
            handle_call(node, src, enclosing, scopes, refs, env, env_hints);
        }
        "member_expression" => {
            if let Some(object) = node.child_by_field_name("object") {
                walk_node(object, src, enclosing, scopes, refs, env, env_hints);
            }
            if let Some(property) = node.child_by_field_name("property") {
                if matches!(
                    property.kind(),
                    "property_identifier" | "private_property_identifier"
                ) {
                    refs.push(UnresolvedRef {
                        from: enclosing,
                        target: RefTarget::Member(node_text(property, src.as_bytes()).to_string()),
                        kind: RefKind::Static,
                        environment: env,
                    });
                }
            }
        }
        "identifier" | "shorthand_property_identifier" => {
            push_name_ref(
                node_text(node, src.as_bytes()),
                enclosing,
                scopes,
                refs,
                env,
                env_hints,
            );
        }
        _ => {
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                walk_node(child, src, enclosing, scopes, refs, env, env_hints);
            }
        }
    }
}

/// Walk the *expressions* inside a declarator's binding pattern: `const { a = DEFAULT } = o`
/// and `const [x, y = DEFAULT] = xs` use `DEFAULT`, and `{ [KEY]: v }` uses `KEY`. The
/// bound names themselves are declarations, not uses, so they are not walked.
fn walk_pattern_defaults(
    pattern: Node,
    src: &str,
    enclosing: SymbolId,
    scopes: &mut Scopes,
    refs: &mut Vec<UnresolvedRef>,
    env: Environment,
    env_hints: &mut HashMap<SymbolId, Environment>,
) {
    match pattern.kind() {
        "assignment_pattern" | "object_assignment_pattern" => {
            if let Some(right) = pattern.child_by_field_name("right") {
                walk_node(right, src, enclosing, scopes, refs, env, env_hints);
            }
            if let Some(left) = pattern.child_by_field_name("left") {
                walk_pattern_defaults(left, src, enclosing, scopes, refs, env, env_hints);
            }
        }
        "pair_pattern" => {
            if let Some(key) = pattern.child_by_field_name("key") {
                if key.kind() == "computed_property_name" {
                    walk_node(key, src, enclosing, scopes, refs, env, env_hints);
                }
            }
            if let Some(value) = pattern.child_by_field_name("value") {
                walk_pattern_defaults(value, src, enclosing, scopes, refs, env, env_hints);
            }
        }
        "object_pattern" | "array_pattern" | "rest_pattern" => {
            let mut c = pattern.walk();
            for child in pattern.named_children(&mut c) {
                walk_pattern_defaults(child, src, enclosing, scopes, refs, env, env_hints);
            }
        }
        _ => {}
    }
}

fn resolve_def_id(node: Node, src: &str, scopes: &Scopes, enclosing: SymbolId) -> SymbolId {
    if node.kind() == "method_definition" {
        return scopes
            .members
            .get(&node.start_byte())
            .copied()
            .unwrap_or(enclosing);
    }
    node.child_by_field_name("name")
        .and_then(|n| scopes.resolve(node_text(n, src.as_bytes())))
        .and_then(|b| match b {
            Binding::Global(id) => Some(id),
            _ => None,
        })
        .unwrap_or(enclosing)
}

/// Declare every name a binding pattern introduces as a local: plain identifiers,
/// destructuring (`{ a, b: c = 1, ...rest }`, `[x, y]`), defaults (`a = 1`), and parameter
/// lists.
fn bind_locals(node: Node, src: &[u8], scopes: &mut Scopes) {
    match node.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            scopes.declare(node_text(node, src), Binding::Local);
        }
        // `a = 1` (parameter default) / `{ a = 1 }` (destructured default): the left side
        // is the binding, the right side is an ordinary expression.
        "assignment_pattern" | "object_assignment_pattern" => {
            if let Some(left) = node.child_by_field_name("left") {
                bind_locals(left, src, scopes);
            }
        }
        // `{ key: local }`: the value side is the binding.
        "pair_pattern" => {
            if let Some(value) = node.child_by_field_name("value") {
                bind_locals(value, src, scopes);
            }
        }
        "formal_parameters" | "object_pattern" | "array_pattern" | "rest_pattern" => {
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                bind_locals(child, src, scopes);
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_call(
    node: Node,
    src: &str,
    enclosing: SymbolId,
    scopes: &mut Scopes,
    refs: &mut Vec<UnresolvedRef>,
    env: Environment,
    env_hints: &mut HashMap<SymbolId, Environment>,
) {
    if let Some(func) = node.child_by_field_name("function") {
        let fname = node_text(func, src.as_bytes());
        let is_import = func.kind() == "import";
        if fname == "require" || fname == "eval" || is_import {
            let first = node
                .child_by_field_name("arguments")
                .and_then(|a| a.named_child(0));
            match first {
                // `require('./m')` / `import('./m')`: statically resolvable, and the result
                // is used as a value, so the whole module is in play.
                Some(arg) if arg.kind() == "string" && fname != "eval" => {
                    let spec = strip_quotes(node_text(arg, src.as_bytes()));
                    if !spec.is_empty() {
                        refs.push(UnresolvedRef {
                            from: enclosing,
                            target: RefTarget::ModuleNamespace(spec),
                            kind: RefKind::Static,
                            environment: env,
                        });
                    }
                }
                // A computed specifier can't be resolved: an uncertain by-name edge.
                Some(arg) => {
                    let s = strip_quotes(node_text(arg, src.as_bytes()));
                    if !s.is_empty() {
                        refs.push(UnresolvedRef {
                            from: enclosing,
                            target: RefTarget::Name(s),
                            kind: RefKind::Dynamic,
                            environment: env,
                        });
                    }
                }
                None => {}
            }
            if fname == "eval" {
                refs.push(UnresolvedRef {
                    from: enclosing,
                    target: RefTarget::Name("eval".into()),
                    kind: RefKind::Dynamic,
                    environment: env,
                });
            }
        }
    }
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        walk_node(child, src, enclosing, scopes, refs, env, env_hints);
    }
}
