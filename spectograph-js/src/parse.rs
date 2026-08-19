//! Single-file JavaScript parsing.

use std::collections::HashMap;
use std::path::Path;

use spectograph_core::ir::{Environment, RefKind, SourceSpan, Symbol, SymbolId, SymbolKind};
use tree_sitter::{Node, Parser};

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
    /// Runtime this import is scoped to, if declared inside an environment guard.
    pub environment: Environment,
}

pub enum RefTarget {
    Symbol(SymbolId),
    Name(String),
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
        Environment::Universal,
    );
    let mut env_hints: HashMap<SymbolId, Environment> = HashMap::new();
    collect_refs(root, src, module_id, &mut scopes, &mut refs, &mut env_hints);

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

    FileResult {
        module_id,
        symbols,
        refs,
        imports,
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
    env: Environment,
) {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "if_statement" => {
                handle_env_if(child, path, src, next_id, symbols, scopes, imports);
            }
            "function_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(name_node, src.as_bytes()).to_string();
                    let id = *next_id;
                    *next_id += 1;
                    scopes.declare(&name, Binding::Global(id));
                    symbols.push(symbol(id, &name, SymbolKind::Function, path, child, env));
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
                                symbols.push(symbol(
                                    id,
                                    &name,
                                    SymbolKind::Function,
                                    path,
                                    decl,
                                    env,
                                ));
                            }
                        }
                        "class_declaration" => {
                            if let Some(name_node) = decl.child_by_field_name("name") {
                                let name = node_text(name_node, src.as_bytes()).to_string();
                                let id = *next_id;
                                *next_id += 1;
                                scopes.declare(&name, Binding::Global(id));
                                symbols.push(symbol(id, &name, SymbolKind::Class, path, decl, env));
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
                                            env,
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
            "assignment_expression" => {
                // Handle CommonJS: module.exports = value and exports.* = value
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
                                                    if let Some(name_node) =
                                                        right.child_by_field_name("name")
                                                    {
                                                        let name =
                                                            node_text(name_node, src.as_bytes())
                                                                .to_string();
                                                        let id = *next_id;
                                                        *next_id += 1;
                                                        scopes.declare(&name, Binding::Global(id));
                                                        symbols.push(symbol(
                                                            id,
                                                            &name,
                                                            SymbolKind::Function,
                                                            path,
                                                            right,
                                                            env,
                                                        ));
                                                    } else {
                                                        // Anonymous function exported as default
                                                        let name = "default".to_string();
                                                        let id = *next_id;
                                                        *next_id += 1;
                                                        scopes.declare(&name, Binding::Global(id));
                                                        symbols.push(symbol(
                                                            id,
                                                            &name,
                                                            SymbolKind::Function,
                                                            path,
                                                            right,
                                                            env,
                                                        ));
                                                    }
                                                }
                                                "class_expression" => {
                                                    // module.exports = class {}
                                                    if let Some(name_node) =
                                                        right.child_by_field_name("name")
                                                    {
                                                        let name =
                                                            node_text(name_node, src.as_bytes())
                                                                .to_string();
                                                        let id = *next_id;
                                                        *next_id += 1;
                                                        scopes.declare(&name, Binding::Global(id));
                                                        symbols.push(symbol(
                                                            id,
                                                            &name,
                                                            SymbolKind::Class,
                                                            path,
                                                            right,
                                                            env,
                                                        ));
                                                    } else {
                                                        // Anonymous class exported as default
                                                        let name = "default".to_string();
                                                        let id = *next_id;
                                                        *next_id += 1;
                                                        scopes.declare(&name, Binding::Global(id));
                                                        symbols.push(symbol(
                                                            id,
                                                            &name,
                                                            SymbolKind::Class,
                                                            path,
                                                            right,
                                                            env,
                                                        ));
                                                    }
                                                }
                                                "object_expression" => {
                                                    // module.exports = { key: value }
                                                    // Parse object properties as exports
                                                    let mut oc = right.walk();
                                                    for prop in right.named_children(&mut oc) {
                                                        if prop.kind() == "pair" {
                                                            if let Some(key_node) =
                                                                prop.child_by_field_name("key")
                                                            {
                                                                let key = node_text(
                                                                    key_node,
                                                                    src.as_bytes(),
                                                                )
                                                                .to_string();
                                                                let id = *next_id;
                                                                *next_id += 1;
                                                                scopes.declare(
                                                                    &key,
                                                                    Binding::Global(id),
                                                                );
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
                                                    // module.exports = existingVariable
                                                    let _name = node_text(right, src.as_bytes())
                                                        .to_string();
                                                    // Just create a reference, not a new symbol
                                                    // The symbol should already exist from the variable declaration
                                                }
                                                _ => {
                                                    // Other expressions - mark as default export
                                                    let name = "default".to_string();
                                                    let id = *next_id;
                                                    *next_id += 1;
                                                    scopes.declare(&name, Binding::Global(id));
                                                    symbols.push(symbol(
                                                        id,
                                                        &name,
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
                                    let property_name =
                                        node_text(property, src.as_bytes()).to_string();
                                    // Create an export symbol for this property
                                    let id = *next_id;
                                    *next_id += 1;
                                    scopes.declare(&property_name, Binding::Global(id));
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
                if let Some(func) = child.child_by_field_name("function") {
                    let fname = node_text(func, src.as_bytes());
                    if fname == "Object.assign" {
                        if let Some(arg) = child.child_by_field_name("arguments") {
                            let mut c = arg.walk();
                            let args: Vec<_> = arg.named_children(&mut c).collect();
                            if args.len() >= 2 {
                                // Check if first argument is exports
                                if let Some(first_arg) = args.first() {
                                    if first_arg.kind() == "identifier" {
                                        let first_arg_text = node_text(*first_arg, src.as_bytes());
                                        if first_arg_text == "exports" {
                                            // This is Object.assign(exports, {...})
                                            if let Some(second_arg) = args.get(1) {
                                                if second_arg.kind() == "object_expression" {
                                                    // Parse object properties as exports
                                                    let mut oc = second_arg.walk();
                                                    for prop in second_arg.named_children(&mut oc) {
                                                        if prop.kind() == "pair" {
                                                            if let Some(key_node) =
                                                                prop.child_by_field_name("key")
                                                            {
                                                                let key = node_text(
                                                                    key_node,
                                                                    src.as_bytes(),
                                                                )
                                                                .to_string();
                                                                let id = *next_id;
                                                                *next_id += 1;
                                                                scopes.declare(
                                                                    &key,
                                                                    Binding::Global(id),
                                                                );
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
                                    scopes.declare(&method_name, Binding::Global(method_id));
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
                    if decl.kind() == "variable_declarator" {
                        if let Some(name_node) = decl.child_by_field_name("name") {
                            let name = node_text(name_node, src.as_bytes()).to_string();
                            let id = *next_id;
                            *next_id += 1;

                            // Check if this is a require() call
                            let kind = if let Some(value) = decl.child_by_field_name("value") {
                                if value.kind() == "call_expression" {
                                    if let Some(func) = value.child_by_field_name("function") {
                                        let fname = node_text(func, src.as_bytes());
                                        if fname == "require" {
                                            // Extract module path
                                            if let Some(arg) =
                                                value.child_by_field_name("arguments")
                                            {
                                                let mut ac = arg.walk();
                                                let first = arg.named_children(&mut ac).next();
                                                if let Some(first_node) = first {
                                                    let v = node_text(first_node, src.as_bytes());
                                                    let module_path =
                                                        v.trim_matches(['\'', '"', '`']);
                                                    if !module_path.is_empty() {
                                                        imports.push(ImportDecl {
                                                            symbol_id: id,
                                                            local_name: name.clone(),
                                                            source: module_path.to_string(),
                                                            is_default: true,
                                                            environment: env,
                                                        });
                                                    }
                                                }
                                            }
                                            SymbolKind::Import
                                        } else {
                                            SymbolKind::Const
                                        }
                                    } else {
                                        SymbolKind::Const
                                    }
                                } else {
                                    SymbolKind::Const
                                }
                            } else {
                                SymbolKind::Const
                            };

                            scopes.declare(&name, Binding::Global(id));
                            symbols.push(symbol(id, &name, kind, path, name_node, env));
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
                                    symbols.push(symbol(
                                        id,
                                        &name,
                                        SymbolKind::Import,
                                        path,
                                        n,
                                        env,
                                    ));
                                    imports.push(ImportDecl {
                                        symbol_id: id,
                                        local_name: name.clone(),
                                        source: source.clone(),
                                        is_default: true,
                                        environment: env,
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
                                            env,
                                        ));
                                        imports.push(ImportDecl {
                                            symbol_id: id,
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
                                                env,
                                            ));
                                            imports.push(ImportDecl {
                                                symbol_id: id,
                                                local_name: name.clone(),
                                                source: source.clone(),
                                                is_default: false,
                                                environment: env,
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
                cons_env,
            );
        }
    }
    if let Some(alternative) = if_node.child_by_field_name("alternative") {
        if let Some(alt_body) = alternative.named_child(0) {
            match alt_body.kind() {
                "statement_block" => {
                    collect_globals(
                        alt_body, path, src, next_id, symbols, scopes, imports, alt_env,
                    );
                }
                "if_statement" => {
                    handle_env_if(alt_body, path, src, next_id, symbols, scopes, imports);
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
        "import_statement" => {}
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
                        walk_node(child, src, def_id, scopes, refs, env, env_hints);
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
                    walk_node(child, src, def_id, scopes, refs, env, env_hints);
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
                walk_node(child, src, enclosing, scopes, refs, env, env_hints);
            }
        }
        "call_expression" => {
            handle_call(node, src, enclosing, scopes, refs, env, env_hints);
        }
        "identifier" => {
            let name = node_text(node, src.as_bytes()).to_string();
            if let Some(detected) = env_of_global(&name) {
                env_hints.entry(enclosing).or_insert(detected);
            }
            match scopes.resolve(&name) {
                Some(Binding::Global(id)) if id != enclosing => {
                    refs.push(UnresolvedRef {
                        from: enclosing,
                        target: RefTarget::Symbol(id),
                        kind: RefKind::Static,
                        environment: env,
                    });
                }
                Some(Binding::Local) => {}
                _ => {
                    refs.push(UnresolvedRef {
                        from: enclosing,
                        target: RefTarget::Name(name),
                        kind: RefKind::Dynamic,
                        environment: env,
                    });
                }
            }
        }
        _ => {
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                walk_node(child, src, enclosing, scopes, refs, env, env_hints);
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
                            environment: env,
                        });
                    }
                }
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
