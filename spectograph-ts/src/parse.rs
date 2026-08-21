//! Single-file TypeScript parsing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use spectograph_core::ir::{RefKind, SourceSpan, Symbol, SymbolId, SymbolKind};
use spectograph_js::react::{is_component_name, is_hook_name, HookCall, JsxUsage, ReactComponent};
use tree_sitter::{Node, Parser};

#[derive(Clone, Copy, PartialEq)]
enum Binding {
    Global(SymbolId),
    Local,
}

#[derive(Default)]
struct Scopes {
    stack: Vec<HashMap<String, Binding>>,
}

impl Scopes {
    fn new() -> Self {
        let mut s = Self { stack: Vec::new() };
        s.push();
        s
    }
    fn push(&mut self) {
        self.stack.push(HashMap::new());
    }
    fn pop(&mut self) {
        self.stack.pop();
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
    pub decorators: Vec<DecoratorInfo>,
    pub components: Vec<ReactComponent>,
    pub jsx_usages: Vec<JsxUsage>,
    pub hook_calls: Vec<HookCall>,
}

pub enum RefTarget {
    Symbol(SymbolId),
    Name(String),
}

pub struct UnresolvedRef {
    pub from: SymbolId,
    pub target: RefTarget,
    pub kind: RefKind,
}

/// A binding sourced from another module: `import`, `import x = require(...)`, or
/// the "from" half of a re-export (`export { a } from './mod'`).
pub struct ImportDecl {
    pub symbol_id: SymbolId,
    /// The name to look up in the source module. The reserved value `"*"` means
    /// "the whole module namespace" (namespace imports, `export * from`), which
    /// resolves to the source's own Module symbol rather than a name within it.
    pub imported_name: String,
    /// The local binding name (`import { x as y } from ...` -> "y"; default
    /// imports -> the bound name itself). For default imports, this doubles as
    /// the def_index fallback key: a *named* default export (`export default
    /// function App() {}`, `export default class Greeter {}`) keeps its own
    /// name rather than being called "default", so the only way to match it to
    /// `import Greeter from './greeter'` is by the import's own local name --
    /// same "vertical slice" convention the JS analyzer uses.
    pub local_name: String,
    pub source: String,
    pub is_default: bool,
}

/// What a [`DecoratorInfo`] decorates.
pub enum DecoratorTarget {
    /// A class, method, or property -- all of which have their own Symbol.
    Symbol(SymbolId),
    /// A constructor/method parameter (`constructor(@Inject(TOKEN) http: Http)`),
    /// which doesn't get its own Symbol (parameters aren't independently
    /// reachable/bloat-checkable, so creating one would just pollute the
    /// reachability graph). Identified by which method it belongs to and its
    /// position in that method's parameter list.
    Parameter { owner: SymbolId, index: usize },
}

/// One `@Decorator(...)` application, captured as structured data (name +
/// raw-text arguments) rather than interpreted -- interpretation (e.g. turning
/// `@Component` into React/Angular-specific meaning) is layered on top by later
/// parts, not baked into parsing itself.
pub struct DecoratorInfo {
    pub target: DecoratorTarget,
    /// The decorator's own name: `@Component(...)` -> "Component",
    /// `@ns.Foo` -> "ns.Foo" (whatever text names the callee/reference).
    pub name: String,
    /// Each argument's raw source text, unparsed.
    pub arguments: Vec<String>,
    pub file: PathBuf,
    pub line: usize,
}

fn node_text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("").trim()
}

fn span(node: Node) -> SourceSpan {
    let s = node.start_position();
    let e = node.end_position();
    SourceSpan::new(s.row + 1, s.column + 1, e.row + 1, e.column + 1)
}

fn symbol(id: SymbolId, name: &str, kind: SymbolKind, path: &Path, node: Node) -> Symbol {
    Symbol {
        id,
        name: name.to_string(),
        kind,
        file: path.to_path_buf(),
        span: span(node),
        reachable: false,
        environment: spectograph_core::ir::Environment::Universal,
        is_external: false,
    }
}

/// True if `path` has a `node_modules` component: third-party library code
/// rather than the project being analyzed.
fn is_under_node_modules(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "node_modules")
}

/// Parse a single TypeScript file. `.tsx` files use the TSX grammar dialect
/// (needed to disambiguate JSX from TypeScript's generic/type-assertion syntax);
/// everything else uses the plain TypeScript grammar.
pub fn parse_file(path: &Path, src: &str, module_id: SymbolId, next_id: &mut usize) -> FileResult {
    let mut parser = Parser::new();
    let is_tsx = path.extension().and_then(|e| e.to_str()) == Some("tsx");
    let lang = if is_tsx {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    };
    parser.set_language(&lang).expect("typescript grammar");

    let tree = parser.parse(src, None).expect("parse typescript");
    let root = tree.root_node();

    let mut scopes = Scopes::new();
    let mut symbols = Vec::new();
    let mut refs = Vec::new();
    let mut imports = Vec::new();
    let mut decorators = Vec::new();

    symbols.push(symbol(
        module_id,
        &path.to_string_lossy(),
        SymbolKind::Module,
        path,
        root,
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
        &mut decorators,
    );
    collect_refs(root, src, module_id, &mut scopes, &mut refs);
    refs.extend(collect_react_lifecycle_edges(root, src, &symbols));

    if is_under_node_modules(path) {
        for sym in symbols.iter_mut() {
            sym.is_external = true;
        }
    }

    let (components, jsx_usages, hook_calls) = detect_react(root, path, src, module_id, &symbols);

    FileResult {
        module_id,
        symbols,
        refs,
        imports,
        decorators,
        components,
        jsx_usages,
        hook_calls,
    }
}

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
    decorators: &mut Vec<DecoratorInfo>,
) {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "export_statement" {
            // `export { a } from './mod'`, `export * from './mod'`,
            // `export * as ns from './mod'`, plain `export { a, b };`, and
            // `export default <expr>;` all need module-resolution/local-lookup
            // handling that doesn't fit the declaration-unwrap-and-match path
            // below, since there's no nested declaration to unwrap.
            if child.child_by_field_name("declaration").is_none() {
                handle_export_statement(child, path, src, next_id, symbols, imports, refs);
                continue;
            }
        }

        // `export function foo() {}` / `export class Foo {}` / `export const x = ...`
        // wrap the same declaration shapes one level down. `namespace Foo {}` (bare,
        // non-exported) is wrapped one level down too, but as an `expression_statement`
        // -- the grammar apparently parses it as a labeled-statement-like expression
        // rather than a declaration. Peel through both until neither applies.
        let mut decl = child;
        loop {
            let inner = match decl.kind() {
                "export_statement" => decl.child_by_field_name("declaration"),
                "expression_statement" => decl.named_child(0),
                _ => None,
            };
            match inner {
                Some(inner) => decl = inner,
                None => break,
            }
        }
        if decl.kind() == "import_statement" {
            handle_import_statement(decl, path, src, next_id, symbols, scopes, imports);
            continue;
        }
        match decl.kind() {
            "function_declaration" => {
                if let Some(name_node) = decl.child_by_field_name("name") {
                    let name = node_text(name_node, src.as_bytes()).to_string();
                    let id = *next_id;
                    *next_id += 1;
                    scopes.declare(&name, Binding::Global(id));
                    symbols.push(symbol(id, &name, SymbolKind::Function, path, decl));
                }
            }
            "class_declaration" => {
                if let Some(name_node) = decl.child_by_field_name("name") {
                    let name = node_text(name_node, src.as_bytes()).to_string();
                    let id = *next_id;
                    *next_id += 1;
                    scopes.declare(&name, Binding::Global(id));
                    symbols.push(symbol(id, &name, SymbolKind::Class, path, decl));

                    // Class decorators (`@Component(...) class Foo {}`) attach to
                    // class_declaration's own "decorator" field -- except when the
                    // class is exported, in which case they attach to the *outer*
                    // export_statement instead (`child`, not `decl`, since `decl`
                    // is already the unwrapped class_declaration at this point).
                    let mut dc = decl.walk();
                    for dec in decl.children_by_field_name("decorator", &mut dc) {
                        push_decorator(dec, DecoratorTarget::Symbol(id), path, src, decorators);
                    }
                    if child.kind() == "export_statement" {
                        let mut ec = child.walk();
                        for dec in child.children_by_field_name("decorator", &mut ec) {
                            push_decorator(dec, DecoratorTarget::Symbol(id), path, src, decorators);
                        }
                    }

                    if let Some(body) = decl.child_by_field_name("body") {
                        let mut bc = body.walk();
                        // Method/property decorators attach positionally to
                        // class_body's own "decorator" field -- they are NOT a
                        // field of method_definition/public_field_definition
                        // themselves, so they show up as a preceding sibling in
                        // this same iteration and must be tracked and applied to
                        // whichever member node comes next.
                        let mut pending_decorators: Vec<Node> = Vec::new();
                        for member in body.named_children(&mut bc) {
                            match member.kind() {
                                "decorator" => {
                                    pending_decorators.push(member);
                                }
                                "method_definition" => {
                                    if let Some(method_name_node) =
                                        member.child_by_field_name("name")
                                    {
                                        let method_name =
                                            node_text(method_name_node, src.as_bytes()).to_string();
                                        let full_name = format!("{}.{}", name, method_name);
                                        let method_id = *next_id;
                                        *next_id += 1;
                                        scopes.declare(&method_name, Binding::Global(method_id));
                                        symbols.push(symbol(
                                            method_id,
                                            &full_name,
                                            SymbolKind::Function,
                                            path,
                                            member,
                                        ));
                                        for dec in pending_decorators.drain(..) {
                                            push_decorator(
                                                dec,
                                                DecoratorTarget::Symbol(method_id),
                                                path,
                                                src,
                                                decorators,
                                            );
                                        }
                                        collect_parameter_decorators(
                                            member, method_id, path, src, decorators,
                                        );
                                        if method_name == "constructor" {
                                            // Dependency injection: a typed constructor
                                            // parameter (`private http: HttpClient`) is
                                            // Angular's (and most other DI frameworks')
                                            // primary injection mechanism -- @Inject()
                                            // decorators (Part 4) are only needed for
                                            // non-class tokens, so this can't be limited
                                            // to decorated parameters.
                                            collect_di_edges(member, id, src, scopes, refs);
                                        }
                                    } else {
                                        pending_decorators.clear();
                                    }
                                }
                                "public_field_definition" => {
                                    if let Some(prop_name_node) = member.child_by_field_name("name")
                                    {
                                        let prop_name =
                                            node_text(prop_name_node, src.as_bytes()).to_string();
                                        let full_name = format!("{}.{}", name, prop_name);
                                        let prop_id = *next_id;
                                        *next_id += 1;
                                        symbols.push(symbol(
                                            prop_id,
                                            &full_name,
                                            SymbolKind::Const,
                                            path,
                                            member,
                                        ));
                                        for dec in pending_decorators.drain(..) {
                                            push_decorator(
                                                dec,
                                                DecoratorTarget::Symbol(prop_id),
                                                path,
                                                src,
                                                decorators,
                                            );
                                        }
                                        // Unlike methods, a property's decorators
                                        // (`@Input() name: string;`) are its own
                                        // "decorator" field, not a positional
                                        // sibling -- collect those too.
                                        let mut fc = member.walk();
                                        for dec in
                                            member.children_by_field_name("decorator", &mut fc)
                                        {
                                            push_decorator(
                                                dec,
                                                DecoratorTarget::Symbol(prop_id),
                                                path,
                                                src,
                                                decorators,
                                            );
                                        }
                                    } else {
                                        pending_decorators.clear();
                                    }
                                }
                                _ => {
                                    pending_decorators.clear();
                                }
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
                    if let Some(name_node) = d.child_by_field_name("name") {
                        // Skip destructuring patterns for now (`const { a, b } = ...`):
                        // Part 1 covers simple bindings only.
                        if name_node.kind() != "identifier" {
                            continue;
                        }
                        let name = node_text(name_node, src.as_bytes()).to_string();
                        let id = *next_id;
                        *next_id += 1;
                        scopes.declare(&name, Binding::Global(id));
                        symbols.push(symbol(id, &name, SymbolKind::Const, path, name_node));
                    }
                }
            }
            "interface_declaration" | "type_alias_declaration" => {
                if let Some(name_node) = decl.child_by_field_name("name") {
                    let name = node_text(name_node, src.as_bytes()).to_string();
                    let id = *next_id;
                    *next_id += 1;
                    scopes.declare(&name, Binding::Global(id));
                    symbols.push(symbol(id, &name, SymbolKind::Type, path, decl));
                }
            }
            "enum_declaration" => {
                if let Some(name_node) = decl.child_by_field_name("name") {
                    let name = node_text(name_node, src.as_bytes()).to_string();
                    let id = *next_id;
                    *next_id += 1;
                    scopes.declare(&name, Binding::Global(id));
                    symbols.push(symbol(id, &name, SymbolKind::Type, path, decl));

                    if let Some(body) = decl.child_by_field_name("body") {
                        let mut bc = body.walk();
                        for member in body.named_children(&mut bc) {
                            // Bare members (`Red,`) are plain property_identifiers;
                            // assigned members (`Red = 1,`) wrap one in enum_assignment.
                            let member_name_node = if member.kind() == "enum_assignment" {
                                member.child_by_field_name("name")
                            } else {
                                Some(member)
                            };
                            let Some(member_name_node) = member_name_node else {
                                continue;
                            };
                            if member_name_node.kind() != "property_identifier" {
                                continue;
                            }
                            let member_name =
                                node_text(member_name_node, src.as_bytes()).to_string();
                            let full_name = format!("{}.{}", name, member_name);
                            let member_id = *next_id;
                            *next_id += 1;
                            symbols.push(symbol(
                                member_id,
                                &full_name,
                                SymbolKind::Const,
                                path,
                                member,
                            ));
                        }
                    }
                }
            }
            "internal_module" => {
                // `namespace Foo { ... }` / `module Foo { ... }`. Its own name becomes
                // a symbol; declarations inside it are still collected (so functions,
                // classes, etc. nested in a namespace aren't silently dropped) but
                // aren't dotted-prefixed with the namespace name -- unlike class
                // members, namespace members are full declarations in their own
                // right, and prefixing would need namespace-qualified call resolution
                // this analyzer doesn't do yet.
                if let Some(name_node) = decl.child_by_field_name("name") {
                    if matches!(name_node.kind(), "identifier" | "nested_identifier") {
                        let name = node_text(name_node, src.as_bytes()).to_string();
                        let id = *next_id;
                        *next_id += 1;
                        scopes.declare(&name, Binding::Global(id));
                        symbols.push(symbol(id, &name, SymbolKind::Module, path, decl));
                    }
                }
                if let Some(body) = decl.child_by_field_name("body") {
                    collect_globals(
                        body, path, src, next_id, symbols, scopes, imports, refs, decorators,
                    );
                }
            }
            _ => {}
        }
    }
}

fn strip_quotes(s: &str) -> String {
    s.trim_matches(['\'', '"', '`']).to_string()
}

/// Extract a decorator's name and raw-text arguments: `@Component({...})` ->
/// ("Component", ["{...}"]); bare `@Injectable` -> ("Injectable", []);
/// `@ns.Foo(...)` -> ("ns.Foo", [...]).
fn decorator_call(dec: Node, src: &[u8]) -> (String, Vec<String>) {
    let Some(inner) = dec.named_child(0) else {
        return (String::new(), Vec::new());
    };
    match inner.kind() {
        "call_expression" => {
            let name = inner
                .child_by_field_name("function")
                .map(|f| node_text(f, src).to_string())
                .unwrap_or_default();
            let arguments = inner
                .child_by_field_name("arguments")
                .map(|a| {
                    let mut c = a.walk();
                    a.named_children(&mut c)
                        .map(|arg| node_text(arg, src).to_string())
                        .collect()
                })
                .unwrap_or_default();
            (name, arguments)
        }
        _ => (node_text(inner, src).to_string(), Vec::new()),
    }
}

fn push_decorator(
    dec: Node,
    target: DecoratorTarget,
    path: &Path,
    src: &str,
    decorators: &mut Vec<DecoratorInfo>,
) {
    let (name, arguments) = decorator_call(dec, src.as_bytes());
    if name.is_empty() {
        return;
    }
    decorators.push(DecoratorInfo {
        target,
        name,
        arguments,
        file: path.to_path_buf(),
        line: span(dec).start_line,
    });
}

/// Collect decorators on a method's own parameters (`constructor(@Inject(TOKEN)
/// private http: Http)`), keyed by parameter position rather than a Symbol --
/// parameters don't get their own Symbol in this analyzer.
fn collect_parameter_decorators(
    method: Node,
    method_id: SymbolId,
    path: &Path,
    src: &str,
    decorators: &mut Vec<DecoratorInfo>,
) {
    let Some(params) = method.child_by_field_name("parameters") else {
        return;
    };
    let mut pc = params.walk();
    for (index, param) in params.named_children(&mut pc).enumerate() {
        if !matches!(param.kind(), "required_parameter" | "optional_parameter") {
            continue;
        }
        let mut dc = param.walk();
        for dec in param.children_by_field_name("decorator", &mut dc) {
            push_decorator(
                dec,
                DecoratorTarget::Parameter {
                    owner: method_id,
                    index,
                },
                path,
                src,
                decorators,
            );
        }
    }
}

/// The simple name a parameter's type annotation refers to, if it's a plain
/// named type (`HttpClient`) or a generic instantiation of one (`Observable<T>`
/// -> "Observable"). Primitive/union/tuple/function/etc. types return `None` --
/// they're either not resolvable to a project symbol at all (`string`, `number`)
/// or too structurally ambiguous to pick one name from for this MVP.
fn simple_type_name<'a>(type_node: Node, src: &'a [u8]) -> Option<&'a str> {
    match type_node.kind() {
        "type_identifier" => Some(node_text(type_node, src)),
        "generic_type" => type_node
            .child_by_field_name("name")
            .filter(|n| n.kind() == "type_identifier")
            .map(|n| node_text(n, src)),
        _ => None,
    }
}

/// Dependency-injection edges: for each of a constructor's parameters with a
/// plain named type (`private http: HttpClient`), record that the owning class
/// depends on whatever that type resolves to. This is DI frameworks' (Angular
/// foremost) primary injection mechanism -- the constructor's declared
/// parameter types ARE the dependency list, independent of any decorator.
fn collect_di_edges(
    constructor: Node,
    class_id: SymbolId,
    src: &str,
    scopes: &Scopes,
    refs: &mut Vec<UnresolvedRef>,
) {
    let Some(params) = constructor.child_by_field_name("parameters") else {
        return;
    };
    let mut pc = params.walk();
    for param in params.named_children(&mut pc) {
        if !matches!(param.kind(), "required_parameter" | "optional_parameter") {
            continue;
        }
        let Some(type_annotation) = param.child_by_field_name("type") else {
            continue;
        };
        let Some(type_node) = type_annotation.named_child(0) else {
            continue;
        };
        let Some(type_name) = simple_type_name(type_node, src.as_bytes()) else {
            continue;
        };
        let target = match scopes.resolve(type_name) {
            Some(Binding::Global(id)) => RefTarget::Symbol(id),
            _ => RefTarget::Name(type_name.to_string()),
        };
        let kind = if matches!(target, RefTarget::Symbol(_)) {
            RefKind::Static
        } else {
            RefKind::Dynamic
        };
        refs.push(UnresolvedRef {
            from: class_id,
            target,
            kind,
        });
    }
}

/// Handle `import ... from '...'` (default/named/namespace) and TS's
/// `import x = require('...')` interop form.
fn handle_import_statement(
    node: Node,
    path: &Path,
    src: &str,
    next_id: &mut usize,
    symbols: &mut Vec<Symbol>,
    scopes: &mut Scopes,
    imports: &mut Vec<ImportDecl>,
) {
    let source = node
        .child_by_field_name("source")
        .map(|s| strip_quotes(node_text(s, src.as_bytes())));

    let mut cursor = node.walk();
    for n in node.named_children(&mut cursor) {
        match n.kind() {
            "import_require_clause" => {
                let Some(name_node) = n.named_child(0) else {
                    continue;
                };
                let Some(source_node) = n.child_by_field_name("source") else {
                    continue;
                };
                let name = node_text(name_node, src.as_bytes()).to_string();
                let source = strip_quotes(node_text(source_node, src.as_bytes()));
                let id = *next_id;
                *next_id += 1;
                scopes.declare(&name, Binding::Global(id));
                symbols.push(symbol(id, &name, SymbolKind::Import, path, name_node));
                imports.push(ImportDecl {
                    symbol_id: id,
                    imported_name: "default".to_string(),
                    local_name: name,
                    source,
                    is_default: true,
                });
            }
            "import_clause" => {
                let Some(source) = source.clone() else {
                    continue;
                };
                let mut cc = n.walk();
                for spec in n.named_children(&mut cc) {
                    match spec.kind() {
                        "identifier" => {
                            let name = node_text(spec, src.as_bytes()).to_string();
                            let id = *next_id;
                            *next_id += 1;
                            scopes.declare(&name, Binding::Global(id));
                            symbols.push(symbol(id, &name, SymbolKind::Import, path, spec));
                            imports.push(ImportDecl {
                                symbol_id: id,
                                imported_name: "default".to_string(),
                                local_name: name,
                                source: source.clone(),
                                is_default: true,
                            });
                        }
                        "namespace_import" => {
                            if let Some(name_node) = spec.named_child(0) {
                                let name = node_text(name_node, src.as_bytes()).to_string();
                                let id = *next_id;
                                *next_id += 1;
                                scopes.declare(&name, Binding::Global(id));
                                symbols.push(symbol(
                                    id,
                                    &name,
                                    SymbolKind::Import,
                                    path,
                                    name_node,
                                ));
                                imports.push(ImportDecl {
                                    symbol_id: id,
                                    imported_name: "*".to_string(),
                                    local_name: name,
                                    source: source.clone(),
                                    is_default: false,
                                });
                            }
                        }
                        "named_imports" => {
                            let mut nc = spec.walk();
                            for imp in spec.named_children(&mut nc) {
                                if imp.kind() != "import_specifier" {
                                    continue;
                                }
                                let Some(orig_name_node) = imp.child_by_field_name("name") else {
                                    continue;
                                };
                                let orig_name =
                                    node_text(orig_name_node, src.as_bytes()).to_string();
                                let local_name = imp
                                    .child_by_field_name("alias")
                                    .map(|a| node_text(a, src.as_bytes()).to_string())
                                    .unwrap_or_else(|| orig_name.clone());
                                let id = *next_id;
                                *next_id += 1;
                                scopes.declare(&local_name, Binding::Global(id));
                                symbols.push(symbol(
                                    id,
                                    &local_name,
                                    SymbolKind::Import,
                                    path,
                                    imp,
                                ));
                                imports.push(ImportDecl {
                                    symbol_id: id,
                                    imported_name: orig_name,
                                    local_name,
                                    source: source.clone(),
                                    is_default: false,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
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
) {
    let source = node
        .child_by_field_name("source")
        .map(|s| strip_quotes(node_text(s, src.as_bytes())));

    // `export default <expr>;`
    if let Some(value) = node.child_by_field_name("value") {
        let id = *next_id;
        *next_id += 1;
        symbols.push(symbol(id, "default", SymbolKind::Export, path, node));
        if value.kind() == "identifier" {
            refs.push(UnresolvedRef {
                from: id,
                target: RefTarget::Name(node_text(value, src.as_bytes()).to_string()),
                kind: RefKind::ReExport,
            });
        }
        return;
    }

    // `export * from './mod';` / `export * as ns from './mod';`
    if let Some(source) = &source {
        let mut has_clause = false;
        let mut cursor = node.walk();
        for n in node.named_children(&mut cursor) {
            // Named children here include the "source" string itself (it's a
            // named field, not just an anonymous token), so `has_clause` must be
            // set only for the clause kinds actually handled below -- otherwise
            // plain `export * from './mod';` (whose only named child IS that
            // source string) would wrongly look like it had a clause and skip
            // the star-reexport fallback entirely.
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
                        ));
                        imports.push(ImportDecl {
                            symbol_id: import_id,
                            imported_name: "*".to_string(),
                            local_name: name.clone(),
                            source: source.clone(),
                            is_default: false,
                        });
                        let export_id = *next_id;
                        *next_id += 1;
                        symbols.push(symbol(
                            export_id,
                            &name,
                            SymbolKind::Export,
                            path,
                            name_node,
                        ));
                        refs.push(UnresolvedRef {
                            from: export_id,
                            target: RefTarget::Symbol(import_id),
                            kind: RefKind::ReExport,
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
                    );
                }
                _ => {}
            }
        }
        if !has_clause {
            // `export * from './mod';`: no per-name tracking (the source module's
            // own export list isn't statically enumerated), but at least keep the
            // module-level dependency so reachability flows through re-exports.
            let import_id = *next_id;
            *next_id += 1;
            symbols.push(symbol(import_id, "*", SymbolKind::Import, path, node));
            imports.push(ImportDecl {
                symbol_id: import_id,
                imported_name: "*".to_string(),
                local_name: "*".to_string(),
                source: source.clone(),
                is_default: false,
            });
            let export_id = *next_id;
            *next_id += 1;
            symbols.push(symbol(export_id, "*", SymbolKind::Export, path, node));
            refs.push(UnresolvedRef {
                from: export_id,
                target: RefTarget::Symbol(import_id),
                kind: RefKind::ReExport,
            });
        }
        return;
    }

    // Plain `export { a, b };`: re-exporting names already declared in this file.
    let mut cursor = node.walk();
    for n in node.named_children(&mut cursor) {
        if n.kind() == "export_clause" {
            handle_export_clause(n, path, src, next_id, symbols, None, imports, refs);
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
        ));

        match source {
            Some(source) => {
                // `export { a } from './mod';`: the name is looked up in the
                // *source* module, not this one, so it needs its own Import
                // binding for cross-file resolution (mirroring a plain import).
                let import_id = *next_id;
                *next_id += 1;
                symbols.push(symbol(
                    import_id,
                    &orig_name,
                    SymbolKind::Import,
                    path,
                    name_node,
                ));
                imports.push(ImportDecl {
                    symbol_id: import_id,
                    imported_name: orig_name.clone(),
                    local_name: orig_name,
                    source: source.clone(),
                    is_default: false,
                });
                refs.push(UnresolvedRef {
                    from: export_id,
                    target: RefTarget::Symbol(import_id),
                    kind: RefKind::ReExport,
                });
            }
            None => {
                // `export { a };`: re-exporting a name declared in this file.
                // Resolved by name (a deferred lookup against the whole-program
                // index, same as any other unresolved identifier) rather than via
                // `Scopes`, since this export statement may appear before the
                // declaration in source order.
                refs.push(UnresolvedRef {
                    from: export_id,
                    target: RefTarget::Name(orig_name),
                    kind: RefKind::ReExport,
                });
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
) {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        walk_node(child, src, module_id, scopes, refs);
    }
}

fn walk_node(
    node: Node,
    src: &str,
    enclosing: SymbolId,
    scopes: &mut Scopes,
    refs: &mut Vec<UnresolvedRef>,
) {
    match node.kind() {
        "import_statement" => {
            // Already fully handled by `handle_import_statement` during
            // `collect_globals` (covers both `import ... from` and TS's
            // `import x = require(...)`); nothing left to walk.
        }
        "export_statement" if node.child_by_field_name("source").is_some() => {
            // `export { a } from './mod'` etc: "a" names something in the *other*
            // module, not this file's scope -- already handled by
            // `handle_export_statement`. Walking it here would risk resolving it
            // against an unrelated same-named local symbol.
        }
        "class_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, src.as_bytes()).to_string();
                let def_id = resolve_def_id(node, src, scopes, enclosing);
                scopes.push();
                if !name.is_empty() {
                    scopes.declare(&name, Binding::Local);
                }
                scopes.declare("this", Binding::Local);
                if let Some(body) = node.child_by_field_name("body") {
                    let mut c = body.walk();
                    for child in body.named_children(&mut c) {
                        walk_node(child, src, def_id, scopes, refs);
                    }
                }
                scopes.pop();
            }
        }
        "function_declaration" | "arrow_function" | "method_definition" => {
            let def_id = resolve_def_id(node, src, scopes, enclosing);
            scopes.push();
            if let Some(name_node) = node.child_by_field_name("name") {
                scopes.declare(node_text(name_node, src.as_bytes()), Binding::Local);
            }
            if let Some(params) = node.child_by_field_name("parameters") {
                bind_locals(params, src.as_bytes(), scopes);
            }
            if let Some(body) = node.child_by_field_name("body") {
                let mut c = body.walk();
                for child in body.named_children(&mut c) {
                    walk_node(child, src, def_id, scopes, refs);
                }
            }
            scopes.pop();
        }
        "assignment_expression" | "variable_declaration" | "lexical_declaration" => {
            let mut c = node.walk();
            for declarator in node.named_children(&mut c) {
                if declarator.kind() == "variable_declarator" {
                    if let Some(value) = declarator.child_by_field_name("value") {
                        walk_node(value, src, enclosing, scopes, refs);
                    }
                } else {
                    walk_node(declarator, src, enclosing, scopes, refs);
                }
            }
        }
        "identifier" => {
            let name = node_text(node, src.as_bytes()).to_string();
            match scopes.resolve(&name) {
                Some(Binding::Global(id)) if id != enclosing => {
                    refs.push(UnresolvedRef {
                        from: enclosing,
                        target: RefTarget::Symbol(id),
                        kind: RefKind::Static,
                    });
                }
                Some(Binding::Global(_)) | Some(Binding::Local) => {}
                None => {
                    refs.push(UnresolvedRef {
                        from: enclosing,
                        target: RefTarget::Name(name),
                        kind: RefKind::Dynamic,
                    });
                }
            }
        }
        _ => {
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                walk_node(child, src, enclosing, scopes, refs);
            }
        }
    }
}

fn resolve_def_id(node: Node, src: &str, scopes: &Scopes, enclosing: SymbolId) -> SymbolId {
    node.child_by_field_name("name")
        .and_then(|n| scopes.resolve(node_text(n, src.as_bytes())))
        .and_then(|b| match b {
            Binding::Global(id) => Some(id),
            Binding::Local => None,
        })
        .unwrap_or(enclosing)
}

fn bind_locals(node: Node, src: &[u8], scopes: &mut Scopes) {
    match node.kind() {
        "identifier" | "required_parameter" | "optional_parameter" => {
            // Parameters can carry a type annotation (`x: number`); only the
            // pattern (name) part matters for scoping.
            if let Some(pattern) = node.child_by_field_name("pattern") {
                bind_locals(pattern, src, scopes);
            } else if node.kind() == "identifier" {
                scopes.declare(node_text(node, src), Binding::Local);
            }
        }
        "object_pattern" | "array_pattern" => {
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                bind_locals(child, src, scopes);
            }
        }
        _ => {
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                bind_locals(child, src, scopes);
            }
        }
    }
}

// --- React/TSX support (Part 6) ---------------------------------------------
//
// Reuses spectograph_js::react's data types and pure naming-convention helpers
// (is_component_name/is_hook_name/find_prop_mismatches) rather than duplicating
// them: JSX itself parses identically under the TSX grammar dialect (same node
// kinds -- jsx_element, jsx_opening_element, jsx_attribute, ...), and "what
// counts as a component/hook name" is a React convention, not a JS-vs-TS one.
// The one TS-specific enhancement is typed props: `function Button(props:
// ButtonProps)` resolves `ButtonProps`'s declared members, on top of the
// destructured-parameter style JS supports.

/// Extract property names from an interface body or type-alias object type
/// (`interface Foo { a: string }` / `type Foo = { a: string }` -- both use the
/// same property_signature-bearing body shape).
fn object_member_names(body: Node, src: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut c = body.walk();
    for member in body.named_children(&mut c) {
        if member.kind() != "property_signature" {
            continue;
        }
        if let Some(name_node) = member.child_by_field_name("name") {
            if name_node.kind() == "property_identifier" {
                names.push(node_text(name_node, src).to_string());
            }
        }
    }
    names
}

/// Map every top-level (and exported) interface/type-alias name to its
/// declared property names, for resolving typed (non-destructured) props.
fn collect_interface_props(root: Node, src: &str) -> HashMap<String, Vec<String>> {
    let mut out = HashMap::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        let decl = if child.kind() == "export_statement" {
            child.child_by_field_name("declaration").unwrap_or(child)
        } else {
            child
        };
        match decl.kind() {
            "interface_declaration" => {
                if let (Some(name_node), Some(body)) = (
                    decl.child_by_field_name("name"),
                    decl.child_by_field_name("body"),
                ) {
                    let name = node_text(name_node, src.as_bytes()).to_string();
                    out.insert(name, object_member_names(body, src.as_bytes()));
                }
            }
            "type_alias_declaration" => {
                if let (Some(name_node), Some(value)) = (
                    decl.child_by_field_name("name"),
                    decl.child_by_field_name("value"),
                ) {
                    if value.kind() == "object_type" {
                        let name = node_text(name_node, src.as_bytes()).to_string();
                        out.insert(name, object_member_names(value, src.as_bytes()));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// True if `node`'s subtree contains a JSX element anywhere.
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
/// inside -- or `value` itself, if it's already a bare function.
/// `memo(forwardRef(...))` is a common real combination, so this recurses to
/// unwrap both layers. Returns `None` for anything else. Ported from
/// `spectograph-js`'s identical function -- the wrapper-call shape is the
/// same in both grammars, confirmed via an AST dump.
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

/// React lifecycle methods React itself invokes on a class component
/// instance through its own reconciler -- never through a call written in
/// the analyzed codebase. See [`collect_react_lifecycle_edges`], which uses
/// this to add an explicit reachability edge for each one a class component
/// defines (mirroring this analyzer's own Angular DI edges: same "framework
/// calls this via reflection, not our code" problem). Ported from
/// `spectograph-js`'s identical list.
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
/// how React was imported. TSX's `class_heritage` wraps an `extends_clause`
/// (which has a real "value" field) rather than directly holding the
/// expression the way plain JS's grammar does -- confirmed via this
/// grammar's node-types.json and an AST dump; `class_heritage` itself is
/// still a positional child of `class_declaration`, not a named field.
fn extends_react_component(class_node: Node, src: &[u8]) -> bool {
    let mut c = class_node.walk();
    let Some(heritage) = class_node
        .named_children(&mut c)
        .find(|n| n.kind() == "class_heritage")
    else {
        return false;
    };
    let mut hc = heritage.walk();
    let Some(extends) = heritage
        .named_children(&mut hc)
        .find(|n| n.kind() == "extends_clause")
    else {
        return false;
    };
    let Some(expr) = extends.child_by_field_name("value") else {
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

/// Add a reachability edge from a class component to each React lifecycle
/// method it defines. A separate pass over the raw AST, like
/// [`detect_react`], rather than threaded through [`walk_node`]/[`Scopes`]:
/// doing it there would mean resolving each method by its bare name through
/// the same flat, file-wide scope table [`collect_globals`] declares all
/// class methods into, which is ambiguous the moment two classes in the file
/// share a lifecycle method name (e.g. two components both defining
/// `render`) -- last declaration wins, silently wiring the edge to the wrong
/// class's method. Resolving by the method's exact `"ClassName.methodName"`
/// symbol name instead sidesteps that entirely, since dotted names are
/// always unique. Ported from `spectograph-js`'s identical fix.
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

/// A component's declared props, from either a destructured parameter
/// (`{ label, onClick }`) or a plain parameter typed with a known
/// interface/object-type alias (`props: ButtonProps`). Empty if neither shape
/// matches (e.g. a bare untyped `props` parameter, which can't be statically
/// enumerated).
fn resolve_prop_names(
    params: Node,
    src: &[u8],
    interface_props: &HashMap<String, Vec<String>>,
) -> Vec<String> {
    let mut pc = params.walk();
    let Some(first) = params.named_children(&mut pc).next() else {
        return Vec::new();
    };
    if !matches!(first.kind(), "required_parameter" | "optional_parameter") {
        return Vec::new();
    }
    if let Some(pattern) = first.child_by_field_name("pattern") {
        if pattern.kind() == "object_pattern" {
            let mut names = Vec::new();
            let mut opc = pattern.walk();
            for prop in pattern.named_children(&mut opc) {
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
                        if let Some(left) = prop.child_by_field_name("left") {
                            if left.kind() == "shorthand_property_identifier_pattern" {
                                names.push(node_text(left, src).to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
            return names;
        }
    }
    // Bare typed parameter (`props: ButtonProps`): resolve via the interface
    // map instead of destructuring.
    first
        .child_by_field_name("type")
        .and_then(|t| t.named_child(0))
        .and_then(|t| simple_type_name(t, src))
        .and_then(|name| interface_props.get(name))
        .cloned()
        .unwrap_or_default()
}

/// Detect React components, JSX usage sites, and hook calls in one file. A
/// separate, simpler pass rather than folded into `collect_globals`/
/// `collect_refs`: it needs no scope/binding tracking, just structural pattern
/// matching plus "which named declaration am I inside".
fn detect_react(
    root: Node,
    path: &Path,
    src: &str,
    module_id: SymbolId,
    symbols: &[Symbol],
) -> (Vec<ReactComponent>, Vec<JsxUsage>, Vec<HookCall>) {
    let mut name_to_id: HashMap<String, SymbolId> = HashMap::new();
    for s in symbols {
        if s.kind != SymbolKind::Module {
            name_to_id.entry(s.name.clone()).or_insert(s.id);
        }
    }
    let interface_props = collect_interface_props(root, src);

    let mut components = Vec::new();
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
                                .map(|p| resolve_prop_names(p, src.as_bytes(), &interface_props))
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
                                // Props aren't statically enumerable for a
                                // class component the way a destructured (or
                                // interface-typed) function-component
                                // parameter is -- they're read off
                                // `this.props.x` inside the class body.
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
                                .map(|p| resolve_prop_names(p, src.as_bytes(), &interface_props))
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

/// Recursively collect JSX usage sites and hook calls, tracking which named
/// function declaration each is nested in so hook calls can be attributed to
/// the right component/custom hook. Mirrors spectograph_js's equivalent walk.
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
            // Everything in a class component's body is attributed to the
            // class itself, not to the individual method: there's no
            // separate per-method component to attach to. A method's bare
            // name (e.g. "render") isn't registered in `name_to_id` under
            // that bare form (class methods are named "ClassName.method"
            // there), so the nested "method_definition" arm's own lookup
            // misses and falls back to whatever `enclosing` we set here.
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
                // ("Menu.Item"), which is already what we want. Ported from
                // spectograph-js's identical fix; confirmed via an AST dump
                // that TSX's grammar produces the same shape here.
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
