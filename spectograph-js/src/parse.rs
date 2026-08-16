//! Single-file JavaScript parsing.

use std::collections::HashMap;
use std::path::Path;

use spectograph_core::ir::{RefKind, SourceSpan, Symbol, SymbolId, SymbolKind};
use tree_sitter::{Node, Parser};

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
}

pub struct ImportDecl {
    pub symbol_id: SymbolId,
    pub local_name: String,
    pub source: String,
    pub is_default: bool,
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

/// Parse a single JavaScript file.
pub fn parse_file(path: &Path, src: &str, module_id: SymbolId, next_id: &mut usize) -> FileResult {
    let mut parser = Parser::new();
    let lang = tree_sitter_javascript::LANGUAGE.into();
    parser.set_language(&lang).expect("javascript grammar");

    let tree = parser.parse(src, None).expect("parse javascript");
    let root = tree.root_node();

    let mut scopes = Scopes::new();
    let mut symbols = Vec::new();
    let mut refs = Vec::new();
    let mut imports = Vec::new();

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
    );
    collect_refs(root, src, module_id, &mut scopes, &mut refs);

    FileResult {
        module_id,
        symbols,
        refs,
        imports,
    }
}

fn collect_globals(
    root: Node,
    path: &Path,
    src: &str,
    next_id: &mut usize,
    symbols: &mut Vec<Symbol>,
    scopes: &mut Scopes,
    imports: &mut Vec<ImportDecl>,
) {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(name_node, src.as_bytes()).to_string();
                    let id = *next_id;
                    *next_id += 1;
                    scopes.declare(&name, Binding::Global(id));
                    symbols.push(symbol(id, &name, SymbolKind::Function, path, child));
                }
            }
            "export_statement" => {
                // Handle: export function foo() {} or export class Foo {}
                if let Some(decl) = child.child_by_field_name("declaration") {
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
                            }
                        }
                        "lexical_declaration" | "variable_declaration" => {
                            let mut dc = decl.walk();
                            for d in decl.named_children(&mut dc) {
                                if d.kind() == "variable_declarator" {
                                    if let Some(name_node) = d.child_by_field_name("name") {
                                        let name = node_text(name_node, src.as_bytes()).to_string();
                                        let id = *next_id;
                                        *next_id += 1;
                                        scopes.declare(&name, Binding::Global(id));
                                        symbols.push(symbol(
                                            id,
                                            &name,
                                            SymbolKind::Const,
                                            path,
                                            name_node,
                                        ));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                // Handle: export { foo } or export default foo
                // For now, skip these - they'll be handled by import resolution
            }
            "class_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(name_node, src.as_bytes()).to_string();
                    let id = *next_id;
                    *next_id += 1;
                    scopes.declare(&name, Binding::Global(id));
                    symbols.push(symbol(id, &name, SymbolKind::Class, path, child));

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
                                    scopes.declare(&method_name, Binding::Global(method_id));
                                    symbols.push(symbol(
                                        method_id,
                                        &full_name,
                                        SymbolKind::Function,
                                        path,
                                        method,
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
                    if decl.kind() == "variable_declarator" {
                        if let Some(name_node) = decl.child_by_field_name("name") {
                            let name = node_text(name_node, src.as_bytes()).to_string();
                            let id = *next_id;
                            *next_id += 1;
                            scopes.declare(&name, Binding::Global(id));
                            symbols.push(symbol(id, &name, SymbolKind::Const, path, name_node));
                        }
                    }
                }
            }
            "import_statement" => {
                let mut source = child
                    .child_by_field_name("source")
                    .map(|s| node_text(s, src.as_bytes()).to_string())
                    .unwrap_or_default();
                if source.is_empty() {
                    if let Some(from) = child.child_by_field_name("_from_clause") {
                        if let Some(fs) = from.child_by_field_name("source") {
                            source = node_text(fs, src.as_bytes()).to_string();
                        }
                    }
                }
                if source.is_empty() {
                    continue;
                }

                let import_clause = child.child_by_field_name("import_clause");
                if let Some(clause) = import_clause {
                    let mut c = clause.walk();
                    for n in clause.named_children(&mut c) {
                        match n.kind() {
                            "identifier" => {
                                let name = node_text(n, src.as_bytes()).to_string();
                                if !name.is_empty() {
                                    let id = *next_id;
                                    *next_id += 1;
                                    scopes.declare(&name, Binding::Global(id));
                                    symbols.push(symbol(id, &name, SymbolKind::Import, path, n));
                                    imports.push(ImportDecl {
                                        symbol_id: id,
                                        local_name: name.clone(),
                                        source: source.clone(),
                                        is_default: true,
                                    });
                                }
                            }
                            "namespace_import" => {
                                if let Some(name_node) = n.child_by_field_name("name") {
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
                                        ));
                                        imports.push(ImportDecl {
                                            symbol_id: id,
                                            local_name: name.clone(),
                                            source: source.clone(),
                                            is_default: false,
                                        });
                                    }
                                }
                            }
                            "named_imports" => {
                                let mut nc = n.walk();
                                for imp in n.named_children(&mut nc) {
                                    if imp.kind() == "import_specifier" {
                                        let name = imp
                                            .child_by_field_name("name")
                                            .map(|x| node_text(x, src.as_bytes()).to_string())
                                            .or_else(|| {
                                                imp.child_by_field_name("alias").map(|x| {
                                                    node_text(x, src.as_bytes()).to_string()
                                                })
                                            })
                                            .unwrap_or_default();
                                        if !name.is_empty() {
                                            let id = *next_id;
                                            *next_id += 1;
                                            scopes.declare(&name, Binding::Global(id));
                                            symbols.push(symbol(
                                                id,
                                                &name,
                                                SymbolKind::Import,
                                                path,
                                                imp,
                                            ));
                                            imports.push(ImportDecl {
                                                symbol_id: id,
                                                local_name: name.clone(),
                                                source: source.clone(),
                                                is_default: false,
                                            });
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
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
        "import_statement" => {}
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
            if let Some(left) = node.child_by_field_name("left") {
                bind_locals(left, src.as_bytes(), scopes);
            }
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                walk_node(child, src, enclosing, scopes, refs);
            }
        }
        "call_expression" => {
            handle_call(node, src, enclosing, scopes, refs);
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
                Some(Binding::Local) => {}
                _ => {
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
            _ => None,
        })
        .unwrap_or(enclosing)
}

fn bind_locals(node: Node, src: &[u8], scopes: &mut Scopes) {
    match node.kind() {
        "identifier" => {
            scopes.declare(node_text(node, src), Binding::Local);
        }
        "object_pattern" | "array_pattern" => {
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                bind_locals(child, src, scopes);
            }
        }
        _ => {}
    }
}

fn handle_call(
    node: Node,
    src: &str,
    enclosing: SymbolId,
    scopes: &mut Scopes,
    refs: &mut Vec<UnresolvedRef>,
) {
    if let Some(func) = node.child_by_field_name("function") {
        let fname = node_text(func, src.as_bytes());
        if fname == "require" || fname == "eval" {
            let kind = RefKind::Dynamic;
            if let Some(arg) = node.child_by_field_name("arguments") {
                let mut c = arg.walk();
                let first = arg.named_children(&mut c).next();
                if let Some(first_node) = first {
                    let v = node_text(first_node, src.as_bytes());
                    let s = v.trim_matches(['\'', '"', '`']);
                    if !s.is_empty() {
                        refs.push(UnresolvedRef {
                            from: enclosing,
                            target: RefTarget::Name(s.to_string()),
                            kind,
                        });
                    }
                }
            }
            if fname == "eval" {
                refs.push(UnresolvedRef {
                    from: enclosing,
                    target: RefTarget::Name("eval".into()),
                    kind: RefKind::Dynamic,
                });
            }
        }
    }
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        walk_node(child, src, enclosing, scopes, refs);
    }
}
