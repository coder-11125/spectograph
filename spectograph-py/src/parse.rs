//! Single-file Python parsing: extract symbols and references from one source file.

use std::collections::HashMap;
use std::path::Path;

use spectograph_core::ir::{RefKind, SourceSpan, Symbol, SymbolId, SymbolKind};
use tree_sitter::{Node, Parser};

/// The scope of names defined/imported at the top level of one file.
#[derive(Default)]
struct FileScope {
    /// name -> SymbolId of the global symbol it refers to.
    names: HashMap<String, SymbolId>,
}

/// Result of parsing a single file: its symbols and internal references.
pub struct FileResult {
    pub module_id: SymbolId,
    /// Symbols defined in this file (functions, classes, imports).
    pub symbols: Vec<Symbol>,
    /// References whose `from` is a symbol in this file and `to` may be any file
    /// (resolved later once all modules are known).
    pub refs: Vec<(SymbolId, String, RefKind)>,
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

    let mut scope = FileScope::default();
    let mut symbols = Vec::new();
    let mut refs = Vec::new();

    // Module symbol for the file itself.
    let module = Symbol {
        id: module_id,
        name: path.to_string_lossy().into_owned(),
        kind: SymbolKind::Module,
        file: path.to_path_buf(),
        span: span(root),
        reachable: false,
    };
    symbols.push(module);

    collect_globals(root, path, src, module_id, next_id, &mut symbols, &mut scope);
    collect_references(root, src, module_id, &scope, &mut refs);

    FileResult { module_id, symbols, refs }
}

/// Walk top-level statements, defining global symbols (imports, defs, classes).
fn collect_globals(
    root: Node,
    path: &Path,
    src: &str,
    _module_id: SymbolId,
    next_id: &mut usize,
    symbols: &mut Vec<Symbol>,
    scope: &mut FileScope,
) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        let kind = child.kind();
        let _name = node_text(child, src.as_bytes()).to_string();
        match kind {
            "function_definition" | "decorated_definition" => {
                let fn_node = if kind == "decorated_definition" {
                    child.child_by_field_name("definition")
                } else {
                    Some(child)
                };
                if let Some(fn_node) = fn_node {
                    if let Some(name_node) = fn_node.child_by_field_name("name") {
                        let sym_name = node_text(name_node, src.as_bytes()).to_string();
                        let id = *next_id;
                        *next_id += 1;
                        scope.names.insert(sym_name.clone(), id);
                        symbols.push(Symbol {
                            id,
                            name: sym_name,
                            kind: SymbolKind::Function,
                            file: path.to_path_buf(),
                            span: span(fn_node),
                            reachable: false,
                        });
                    }
                }
            }
            "class_definition" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let sym_name = node_text(name_node, src.as_bytes()).to_string();
                    let id = *next_id;
                    *next_id += 1;
                    scope.names.insert(sym_name.clone(), id);
                    symbols.push(Symbol {
                        id,
                        name: sym_name,
                        kind: SymbolKind::Class,
                        file: path.to_path_buf(),
                        span: span(child),
                        reachable: false,
                    });
                }
            }
            "import_statement" => {
                let mut c2 = child.walk();
                for clause in child.named_children(&mut c2) {
                    match clause.kind() {
                        "dotted_name" => {
                            let sym_name = node_text(clause, src.as_bytes()).to_string();
                            let id = *next_id;
                            *next_id += 1;
                            scope.names.insert(sym_name.clone(), id);
                            symbols.push(Symbol {
                                id,
                                name: sym_name,
                                kind: SymbolKind::Import,
                                file: path.to_path_buf(),
                                span: span(clause),
                                reachable: false,
                            });
                        }
                        "aliased_import" => {
                            let sym_name = clause
                                .child_by_field_name("name")
                                .map(|n| node_text(n, src.as_bytes()).to_string())
                                .unwrap_or_default();
                            if sym_name.is_empty() {
                                continue;
                            }
                            let id = *next_id;
                            *next_id += 1;
                            scope.names.insert(sym_name.clone(), id);
                            symbols.push(Symbol {
                                id,
                                name: sym_name,
                                kind: SymbolKind::Import,
                                file: path.to_path_buf(),
                                span: span(clause),
                                reachable: false,
                            });
                        }
                        _ => {}
                    }
                }
            }
            "import_from_statement" => {
                // `from X import a, b`: module is a field, each imported name is a
                // child with field "name".
                let mut c2 = child.walk();
                for clause in child.named_children(&mut c2) {
                    // The module itself.
                    if clause.kind() == "dotted_name" && child.field_name_for_child(0) == Some("module_name") {
                        let sym_name = node_text(clause, src.as_bytes()).to_string();
                        let id = *next_id;
                        *next_id += 1;
                        scope.names.insert(sym_name.clone(), id);
                        symbols.push(Symbol {
                            id,
                            name: sym_name,
                            kind: SymbolKind::Import,
                            file: path.to_path_buf(),
                            span: span(clause),
                            reachable: false,
                        });
                    } else {
                        // Imported names: identifier or aliased_import.
                        let sym_name = match clause.kind() {
                            "aliased_import" => clause
                                .child_by_field_name("name")
                                .map(|n| node_text(n, src.as_bytes()).to_string())
                                .unwrap_or_default(),
                            _ => node_text(clause, src.as_bytes()).to_string(),
                        };
                        if sym_name.is_empty() {
                            continue;
                        }
                        let id = *next_id;
                        *next_id += 1;
                        scope.names.insert(sym_name.clone(), id);
                        symbols.push(Symbol {
                            id,
                            name: sym_name,
                            kind: SymbolKind::Import,
                            file: path.to_path_buf(),
                            span: span(clause),
                            reachable: false,
                        });
                    }
                }
            }
            "module" | "expression_statement" | "comment" => {}
            _ => {}
        }
    }
}

/// Walk all identifiers, emitting references from the enclosing symbol to any
/// global-scope name they resolve to.
///
/// Function and class *bodies* are traversed so that calls count as usage; the
/// definition name and parameters are skipped. References are attributed to the
/// nearest enclosing function/class symbol when one exists.
fn collect_references(
    root: Node,
    src: &str,
    module_id: SymbolId,
    scope: &FileScope,
    refs: &mut Vec<(SymbolId, String, RefKind)>,
) {
    let mut cursor = root.walk();
    for node in root.children(&mut cursor) {
        walk_identifiers(node, src, &scope, module_id, refs);
    }
}

fn walk_identifiers(
    node: Node,
    src: &str,
    scope: &FileScope,
    enclosing: SymbolId,
    refs: &mut Vec<(SymbolId, String, RefKind)>,
) {
    // Import subtrees don't contain uses; skip them wholesale.
    if matches!(node.kind(), "import_statement" | "import_from_statement") {
        return;
    }

    // Inside a function/class definition, switch the enclosing symbol to that
    // definition and recurse into its body (skipping the name/params).
    if matches!(node.kind(), "function_definition" | "class_definition") {
        if let Some(name_node) = node.child_by_field_name("name") {
            let def_name = node_text(name_node, src.as_bytes());
            if let Some(&def_id) = scope.names.get(def_name) {
                let body = node.child_by_field_name("body");
                if let Some(body) = body {
                    walk_identifiers(body, src, scope, def_id, refs);
                }
                return;
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" {
            let name = node_text(child, src.as_bytes()).to_string();
            if scope.names.contains_key(&name) {
                refs.push((enclosing, name, RefKind::Static));
            }
        }
        walk_identifiers(child, src, scope, enclosing, refs);
    }
}
