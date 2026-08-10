//! Single-file Python parsing: extract symbols and references from one source file.
//!
//! Resolution model:
//! - A [`Scopes`] stack tracks name bindings (globals, locals, and dynamic/uncertain).
//! - Phase 1 (`collect_globals`) registers module-level functions, classes, and imports.
//! - Phase 2 (`collect_refs`) walks the tree, pushing scopes for function/class bodies,
//!   binding parameters and assignments as locals, and emitting references from the
//!   enclosing symbol to whatever a name resolves to.
//! - Uncertain constructs (`from x import *`, `getattr`, `__import__`) are marked
//!   [`RefKind::Reflection`]/[`RefKind::Dynamic`] rather than silently assumed.

use std::collections::HashMap;
use std::path::Path;

use spectograph_core::ir::{RefKind, SourceSpan, Symbol, SymbolId, SymbolKind};
use tree_sitter::{Node, Parser};

/// How a name resolves.
#[derive(Clone, Copy, PartialEq)]
enum Binding {
    /// A module-level symbol (function/class/import) this name refers to.
    Global(SymbolId),
    /// A local variable / parameter / assignment, shadowing any outer binding.
    Local,
    /// An uncertain binding (e.g. star import), resolvable only dynamically.
    Dynamic,
}

/// A stack of lexical scopes. Scope 0 is the module/global scope.
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

/// Result of parsing a single file: its symbols and internal references.
pub struct FileResult {
    pub module_id: SymbolId,
    /// Symbols defined in this file (functions, classes, imports).
    pub symbols: Vec<Symbol>,
    /// References whose `from` is a symbol in this file and `to` may be any file
    /// (resolved later once all modules are known).
    pub refs: Vec<UnresolvedRef>,
    /// Names declared public via `__all__`.
    pub exports: Vec<ExportDecl>,
}

/// A single name declared in a module's `__all__`.
#[derive(Debug, Clone)]
pub struct ExportDecl {
    pub name: String,
    pub span: SourceSpan,
}

/// The target of a reference discovered during single-file parsing.
pub enum RefTarget {
    /// A resolved symbol (functions/classes/imports referenced by name).
    Symbol(SymbolId),
    /// An unresolved dynamic/reflection target (e.g. a string from `getattr`/`__import__`),
    /// resolved against the global symbol index after all files are parsed.
    Name(String),
}

/// A reference with its target possibly still unresolved across files.
pub struct UnresolvedRef {
    pub from: SymbolId,
    pub target: RefTarget,
    pub kind: RefKind,
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

fn symbol(id: SymbolId, name: &str, kind: SymbolKind, path: &Path, node: Node) -> Symbol {
    Symbol {
        id,
        name: name.to_string(),
        kind,
        file: path.to_path_buf(),
        span: span(node),
        reachable: false,
    }
}

/// Parse a single Python file. `module_id` is assigned by the caller.
pub fn parse_file(
    path: &Path,
    src: &str,
    module_id: SymbolId,
    next_id: &mut usize,
) -> FileResult {
    let mut parser = Parser::new();
    let lang: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
    parser.set_language(&lang).expect("python grammar");

    let tree = parser.parse(src, None).expect("parse python");
    let root = tree.root_node();

    let mut scopes = Scopes::new();
    let mut symbols = Vec::new();
    let mut refs = Vec::new();
    let mut exports = Vec::new();

    let module = symbol(
        module_id,
        &path.to_string_lossy(),
        SymbolKind::Module,
        path,
        root,
    );
    symbols.push(module);

    collect_globals(root, path, src, module_id, next_id, &mut symbols, &mut scopes, &mut exports);
    collect_refs(root, src, module_id, &mut scopes, &mut refs);

    FileResult { module_id, symbols, refs, exports }
}

/// Extract the imported name and its alias from an import clause node.
///
/// - `import x` / `from x import y`  -> `(x|y, "")`
/// - `import x as y` / `from x import y as z` -> `(x|y, "y"|"z")`
fn import_name_and_alias(node: Node, src: &[u8]) -> (String, String) {
    if node.kind() == "aliased_import" {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, src).to_string())
            .unwrap_or_default();
        let alias = node
            .child_by_field_name("alias")
            .map(|n| node_text(n, src).to_string())
            .unwrap_or_default();
        (name, alias)
    } else {
        (node_text(node, src).to_string(), String::new())
    }
}

/// Walk top-level statements, defining module-level symbols and global bindings.
#[allow(clippy::too_many_arguments)]
fn collect_globals(
    root: Node,
    path: &Path,
    src: &str,
    _module_id: SymbolId,
    next_id: &mut usize,
    symbols: &mut Vec<Symbol>,
    scopes: &mut Scopes,
    exports: &mut Vec<ExportDecl>,
) {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "function_definition" | "decorated_definition" => {
                let fn_node = if child.kind() == "decorated_definition" {
                    child.child_by_field_name("definition")
                } else {
                    Some(child)
                };
                if let Some(fn_node) = fn_node {
                    if let Some(name_node) = fn_node.child_by_field_name("name") {
                        let sym_name = node_text(name_node, src.as_bytes()).to_string();
                        let id = *next_id;
                        *next_id += 1;
                        scopes.declare(&sym_name, Binding::Global(id));
                        symbols.push(symbol(
                            id,
                            &sym_name,
                            SymbolKind::Function,
                            path,
                            fn_node,
                        ));
                    }
                }
            }
            "class_definition" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let sym_name = node_text(name_node, src.as_bytes()).to_string();
                    let id = *next_id;
                    *next_id += 1;
                    scopes.declare(&sym_name, Binding::Global(id));
                    symbols.push(symbol(id, &sym_name, SymbolKind::Class, path, child));
                }
            }
            "import_statement" => {
                let mut c2 = child.walk();
                for clause in child.named_children(&mut c2) {
                    let (sym_name, alias) = import_name_and_alias(clause, src.as_bytes());
                    if sym_name.is_empty() {
                        continue;
                    }
                    let id = *next_id;
                    *next_id += 1;
                    let bound = if alias.is_empty() { &sym_name } else { &alias };
                    scopes.declare(bound, Binding::Global(id));
                    symbols.push(symbol(id, &sym_name, SymbolKind::Import, path, clause));
                }
            }
            "import_from_statement" => {
                let module = child.child_by_field_name("module_name");
                let mut c2 = child.walk();
                for clause in child.named_children(&mut c2) {
                    // Star import: bind the module name as uncertain.
                    if clause.kind() == "wildcard_import" {
                        if let Some(m) = module {
                            let mn = node_text(m, src.as_bytes()).to_string();
                            scopes.declare(&mn, Binding::Dynamic);
                        }
                        continue;
                    }
                    // Skip the module dotted_name itself; only bind imported names.
                    if let Some(m) = module {
                        if clause == m {
                            continue;
                        }
                    }
                    let (sym_name, alias) = import_name_and_alias(clause, src.as_bytes());
                    if sym_name.is_empty() {
                        continue;
                    }
                    let id = *next_id;
                    *next_id += 1;
                    let bound = if alias.is_empty() { &sym_name } else { &alias };
                    scopes.declare(bound, Binding::Global(id));
                    symbols.push(symbol(id, &sym_name, SymbolKind::Import, path, clause));
                }
            }
            // `__all__ = ["a", "b"]` declares the module's public API.
            "expression_statement" => {
                if let Some(assign) = child.named_child(0) {
                    if assign.kind() == "assignment" {
                        let is_all = assign
                            .child_by_field_name("left")
                            .map(|l| node_text(l, src.as_bytes()) == "__all__")
                            .unwrap_or(false);
                        if is_all {
                            if let Some(right) = assign.child_by_field_name("right") {
                                let mut c3 = right.walk();
                                for s in right.named_children(&mut c3) {
                                    if s.kind() == "string" {
                                        let v = node_text(s, src.as_bytes());
                                        let name = v.trim_matches(['\'', '"']).to_string();
                                        if !name.is_empty() {
                                            exports.push(ExportDecl { name, span: span(s) });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Bind every simple identifier in a parameters node / assignment target as a local.
fn bind_local_identifiers(node: Node, src: &[u8], scopes: &mut Scopes) {
    match node.kind() {
        "identifier" => {
            scopes.declare(node_text(node, src), Binding::Local);
        }
        "pattern_list" | "tuple_pattern" | "list_pattern" | "list_splat_pattern"
        | "dictionary_splat_pattern" | "typed_parameter" | "default_parameter"
        | "typed_default_parameter" | "keyword_argument" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                bind_local_identifiers(child, src, scopes);
            }
        }
        _ => {}
    }
}

/// Walk the tree, tracking scopes and emitting references.
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
        // Import statements contain no uses; skip.
        "import_statement" | "import_from_statement" => {}
        // Enter function/class bodies: push a scope, bind the def name + params,
        // and attribute internal references to the definition.
        "function_definition" | "class_definition" => {
            let def_id = node
                .child_by_field_name("name")
                .and_then(|n| scopes.resolve(node_text(n, src.as_bytes())))
                .and_then(|b| match b {
                    Binding::Global(id) => Some(id),
                    _ => None,
                })
                .unwrap_or(enclosing);

            scopes.push();
            if let Some(name_node) = node.child_by_field_name("name") {
                scopes.declare(node_text(name_node, src.as_bytes()), Binding::Local);
            }
            if node.kind() == "class_definition" {
                scopes.declare("self", Binding::Local);
            }
            if let Some(params) = node.child_by_field_name("parameters") {
                bind_local_identifiers(params, src.as_bytes(), scopes);
            }
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.named_children(&mut cursor) {
                    walk_node(child, src, def_id, scopes, refs);
                }
            }
            scopes.pop();
        }
        // Assignments introduce locals on the left; the right side may still reference.
        "assignment" => {
            if let Some(left) = node.child_by_field_name("left") {
                bind_local_identifiers(left, src.as_bytes(), scopes);
            }
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk_node(child, src, enclosing, scopes, refs);
            }
        }
        // Uncertain runtime constructs.
        "call" => {
            handle_call(node, src, enclosing, scopes, refs);
        }
        "identifier" => {
            let name = node_text(node, src.as_bytes()).to_string();
            match scopes.resolve(&name) {
                Some(Binding::Global(id)) => refs.push(UnresolvedRef {
                    from: enclosing,
                    target: RefTarget::Symbol(id),
                    kind: RefKind::Static,
                }),
                Some(Binding::Dynamic) => refs.push(UnresolvedRef {
                    from: enclosing,
                    target: RefTarget::Name(name),
                    kind: RefKind::Dynamic,
                }),
                _ => {}
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk_node(child, src, enclosing, scopes, refs);
            }
        }
    }
}

/// Detect uncertain runtime patterns in a call expression.
fn handle_call(
    node: Node,
    src: &str,
    enclosing: SymbolId,
    scopes: &mut Scopes,
    refs: &mut Vec<UnresolvedRef>,
) {
    // `getattr(obj, "name")` and `__import__("mod")` are reflection/dynamic.
    if let Some(func) = node.child_by_field_name("function") {
        let fname = node_text(func, src.as_bytes());
        if fname == "getattr" {
            if let Some(arg) = node.child_by_field_name("arguments") {
                let mut c = arg.walk();
                let args: Vec<Node> = arg.named_children(&mut c).collect();
                if let Some(second) = args.get(1) {
                    let v = node_text(*second, src.as_bytes());
                    let stripped = v.trim_matches(['\'', '"']);
                    if !stripped.is_empty() {
                        refs.push(UnresolvedRef {
                            from: enclosing,
                            target: RefTarget::Name(stripped.to_string()),
                            kind: RefKind::Reflection,
                        });
                    }
                }
            }
        } else if fname == "__import__" {
            if let Some(arg) = node.child_by_field_name("arguments") {
                let mut c = arg.walk();
                for a in arg.named_children(&mut c) {
                    if a.kind() == "string" {
                        let v = node_text(a, src.as_bytes());
                        let stripped = v.trim_matches(['\'', '"']);
                        if !stripped.is_empty() {
                            refs.push(UnresolvedRef {
                                from: enclosing,
                                target: RefTarget::Name(stripped.to_string()),
                                kind: RefKind::Dynamic,
                            });
                        }
                    }
                }
            }
        }
    }

    // Recurse into the call normally for other references.
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_node(child, src, enclosing, scopes, refs);
    }
}
