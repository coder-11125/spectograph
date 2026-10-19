//! Tests for the constructor-parameter-type -> DI graph (Part 5): Angular's (and
//! most other DI frameworks') primary injection mechanism is a typed constructor
//! parameter, independent of any decorator -- `@Inject()` is only needed for
//! non-class tokens.

use tempfile::tempdir;

use spectograph_analyzer::Analyzer;
use spectograph_core::ir::{Program, RefKind, SymbolKind};
use spectograph_ts::TypeScriptAnalyzer;

fn analyze_ts(files: &[(&str, &str)]) -> Program {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    TypeScriptAnalyzer.analyze(dir.path()).unwrap()
}

fn class_id(program: &Program, name: &str) -> spectograph_core::ir::SymbolId {
    program
        .symbols
        .iter()
        .find(|s| s.name == name && s.kind != SymbolKind::Import)
        .unwrap_or_else(|| panic!("no symbol named {name}"))
        .id
}

/// References from `class` to anything that is not one of its own members. A class always
/// uses its own constructor (it runs on `new`), so `Widget -> Widget.constructor` is not a
/// dependency on anything else.
fn external_edges_from(program: &Program, class: &str) -> Vec<(usize, usize)> {
    let id = class_id(program, class);
    let prefix = format!("{class}.");
    let own: Vec<usize> = program
        .symbols
        .iter()
        .filter(|s| s.name.starts_with(&prefix))
        .map(|s| s.id)
        .collect();
    program
        .references
        .iter()
        .filter(|r| r.from == id && !own.contains(&r.to))
        .map(|r| (r.from, r.to))
        .collect()
}

#[test]
fn undecorated_typed_constructor_param_creates_di_edge() {
    // The idiomatic Angular case: no @Inject() at all, just a typed parameter.
    let program = analyze_ts(&[(
        "app.ts",
        r#"class HttpClient {}
class UserService {
    constructor(private http: HttpClient) {}
}"#,
    )]);
    let service = class_id(&program, "UserService");
    let http = class_id(&program, "HttpClient");
    assert!(
        program
            .references
            .iter()
            .any(|r| r.from == service && r.to == http && r.kind == RefKind::Static),
        "UserService should statically depend on HttpClient"
    );
}

#[test]
fn di_edge_originates_from_the_class_not_the_constructor() {
    // So reachability can flow to injected dependencies as soon as the class
    // itself is reachable, without also requiring the constructor specifically
    // to be referenced (Angular services are rarely `new`'d directly in user
    // code -- the DI container does it).
    let program = analyze_ts(&[(
        "app.ts",
        r#"class HttpClient {}
class UserService {
    constructor(private http: HttpClient) {}
}"#,
    )]);
    let service_class = program
        .symbols
        .iter()
        .find(|s| s.name == "UserService" && s.kind == SymbolKind::Class)
        .unwrap();
    let http = class_id(&program, "HttpClient");
    assert!(program
        .references
        .iter()
        .any(|r| r.from == service_class.id && r.to == http));
}

#[test]
fn multiple_constructor_params_all_create_di_edges() {
    let program = analyze_ts(&[(
        "app.ts",
        r#"class HttpClient {}
class Logger {}
class UserService {
    constructor(private http: HttpClient, private logger: Logger) {}
}"#,
    )]);
    let service = class_id(&program, "UserService");
    let http = class_id(&program, "HttpClient");
    let logger = class_id(&program, "Logger");
    assert!(program
        .references
        .iter()
        .any(|r| r.from == service && r.to == http));
    assert!(program
        .references
        .iter()
        .any(|r| r.from == service && r.to == logger));
}

#[test]
fn inject_decorated_param_still_resolves_by_its_type() {
    // @Inject(TOKEN) changes the *runtime* lookup key but the parameter's own
    // declared type is still what this analyzer's graph tracks.
    let program = analyze_ts(&[(
        "app.ts",
        r#"class Logger {}
class UserService {
    constructor(@Inject(LOGGER_TOKEN) private logger: Logger) {}
}"#,
    )]);
    let service = class_id(&program, "UserService");
    let logger = class_id(&program, "Logger");
    assert!(program
        .references
        .iter()
        .any(|r| r.from == service && r.to == logger));
}

#[test]
fn interface_typed_param_resolves_to_the_interface_symbol() {
    let program = analyze_ts(&[(
        "app.ts",
        r#"interface ConfigService { get(key: string): string; }
class UserService {
    constructor(private config: ConfigService) {}
}"#,
    )]);
    let service = class_id(&program, "UserService");
    let config = program
        .symbols
        .iter()
        .find(|s| s.name == "ConfigService" && s.kind == SymbolKind::Type)
        .unwrap();
    assert!(program
        .references
        .iter()
        .any(|r| r.from == service && r.to == config.id));
}

#[test]
fn cross_file_dependency_resolves_via_global_name_index() {
    let program = analyze_ts(&[
        (
            "user.service.ts",
            "class UserService {\n    constructor(private http: HttpClient) {}\n}",
        ),
        ("http.ts", "export class HttpClient {}"),
    ]);
    let service = class_id(&program, "UserService");
    let http = class_id(&program, "HttpClient");
    assert!(
        program
            .references
            .iter()
            .any(|r| r.from == service && r.to == http && r.kind == RefKind::Dynamic),
        "cross-file DI resolution goes through the deferred name index, like any \
         other cross-file reference in this analyzer, so it's Dynamic not Static"
    );
}

#[test]
fn generic_typed_param_resolves_to_its_base_type() {
    let program = analyze_ts(&[(
        "app.ts",
        r#"class DataSource {}
class UserService {
    constructor(private source: DataSource<User>) {}
}
class User {}"#,
    )]);
    let service = class_id(&program, "UserService");
    let source = program
        .symbols
        .iter()
        .find(|s| s.name == "DataSource" && s.kind == SymbolKind::Class)
        .unwrap();
    assert!(
        program
            .references
            .iter()
            .any(|r| r.from == service && r.to == source.id),
        "DataSource<User> should resolve to DataSource, not User"
    );
}

#[test]
fn primitive_typed_params_create_no_di_edge() {
    let program = analyze_ts(&[(
        "app.ts",
        "class Widget {\n    constructor(private name: string, private count: number) {}\n}",
    )]);
    // Only an edge from Widget to something else would indicate a (bogus) DI
    // resolution attempt on a primitive type; there should be none at all here.
    assert_eq!(
        external_edges_from(&program, "Widget"),
        Vec::new(),
        "primitive-typed constructor params must not produce DI edges"
    );
}

#[test]
fn non_constructor_method_typed_params_create_no_di_edge() {
    let program = analyze_ts(&[(
        "app.ts",
        r#"class HttpClient {}
class Widget {
    setup(http: HttpClient): void {}
}"#,
    )]);
    let widget = class_id(&program, "Widget");
    let http = class_id(&program, "HttpClient");
    assert!(
        !program
            .references
            .iter()
            .any(|r| r.from == widget && r.to == http),
        "DI edges only come from constructors, not arbitrary typed method params"
    );
}

#[test]
fn untyped_constructor_param_creates_no_di_edge() {
    let program = analyze_ts(&[(
        "app.ts",
        "class Widget {\n    constructor(private options) {}\n}",
    )]);
    assert_eq!(external_edges_from(&program, "Widget"), Vec::new());
}
