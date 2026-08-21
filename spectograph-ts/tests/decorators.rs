//! Tests for TypeScript decorator extraction (Part 4): class, method, property,
//! and parameter decorators, captured as structured (name + raw-text arguments)
//! but uninterpreted data.

use tempfile::tempdir;

use spectograph_ts::parse::{DecoratorInfo, DecoratorTarget};
use spectograph_ts::TypeScriptAnalyzer;

fn analyze_decorators(
    files: &[(&str, &str)],
) -> (spectograph_core::ir::Program, Vec<DecoratorInfo>) {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    TypeScriptAnalyzer.analyze_decorators(dir.path()).unwrap()
}

fn symbol_target(target: &DecoratorTarget) -> Option<spectograph_core::ir::SymbolId> {
    match target {
        DecoratorTarget::Symbol(id) => Some(*id),
        DecoratorTarget::Parameter { .. } => None,
    }
}

#[test]
fn class_decorator_with_object_literal_argument() {
    let (program, decorators) = analyze_decorators(&[(
        "app.ts",
        "@Component({ selector: 'app-root' })\nclass AppComponent {}",
    )]);
    let app = program
        .symbols
        .iter()
        .find(|s| s.name == "AppComponent")
        .unwrap();
    let dec = decorators
        .iter()
        .find(|d| d.name == "Component")
        .expect("Component decorator should be captured");
    assert_eq!(symbol_target(&dec.target), Some(app.id));
    assert_eq!(dec.arguments.len(), 1);
    assert!(dec.arguments[0].contains("selector"));
}

#[test]
fn exported_class_decorator_still_attaches_to_the_class() {
    // The decorator grammatically attaches to the outer export_statement, not
    // the class_declaration itself, when the class is exported.
    let (program, decorators) =
        analyze_decorators(&[("app.ts", "@Injectable()\nexport class UserService {}")]);
    let service = program
        .symbols
        .iter()
        .find(|s| s.name == "UserService")
        .unwrap();
    let dec = decorators
        .iter()
        .find(|d| d.name == "Injectable")
        .expect("Injectable decorator should be captured on the exported class");
    assert_eq!(symbol_target(&dec.target), Some(service.id));
}

#[test]
fn multiple_class_decorators_are_all_captured() {
    let (program, decorators) = analyze_decorators(&[(
        "app.ts",
        "@Injectable()\n@Injectable2()\nexport class Service {}",
    )]);
    let service = program
        .symbols
        .iter()
        .find(|s| s.name == "Service")
        .unwrap();
    let names: Vec<&str> = decorators
        .iter()
        .filter(|d| symbol_target(&d.target) == Some(service.id))
        .map(|d| d.name.as_str())
        .collect();
    assert!(names.contains(&"Injectable"));
    assert!(names.contains(&"Injectable2"));
}

#[test]
fn bare_decorator_without_call_has_no_arguments() {
    let (program, decorators) = analyze_decorators(&[("app.ts", "@Injectable\nclass Service {}")]);
    let service = program
        .symbols
        .iter()
        .find(|s| s.name == "Service")
        .unwrap();
    let dec = decorators.iter().find(|d| d.name == "Injectable").unwrap();
    assert_eq!(symbol_target(&dec.target), Some(service.id));
    assert!(dec.arguments.is_empty());
}

#[test]
fn property_decorator_attaches_to_its_own_property_not_the_class() {
    let (program, decorators) = analyze_decorators(&[(
        "app.ts",
        "class AppComponent {\n    @Input() name: string;\n}",
    )]);
    let prop = program
        .symbols
        .iter()
        .find(|s| s.name == "AppComponent.name")
        .expect("the decorated property should be its own symbol");
    let dec = decorators
        .iter()
        .find(|d| d.name == "Input")
        .expect("Input decorator should be captured");
    assert_eq!(symbol_target(&dec.target), Some(prop.id));
}

#[test]
fn method_decorator_attaches_to_the_correct_method_not_a_sibling() {
    // Method decorators attach positionally to class_body, not to the
    // method_definition node itself -- this proves they land on the *right*
    // following member when there's more than one member in the class.
    let (program, decorators) = analyze_decorators(&[(
        "app.ts",
        r#"class Widget {
    setup(): void {}

    @HostListener('click')
    onClick(): void {}

    teardown(): void {}
}"#,
    )]);
    let on_click = program
        .symbols
        .iter()
        .find(|s| s.name == "Widget.onClick")
        .unwrap();
    let setup = program
        .symbols
        .iter()
        .find(|s| s.name == "Widget.setup")
        .unwrap();
    let teardown = program
        .symbols
        .iter()
        .find(|s| s.name == "Widget.teardown")
        .unwrap();

    let dec = decorators
        .iter()
        .find(|d| d.name == "HostListener")
        .unwrap();
    assert_eq!(symbol_target(&dec.target), Some(on_click.id));
    assert_ne!(symbol_target(&dec.target), Some(setup.id));
    assert_ne!(symbol_target(&dec.target), Some(teardown.id));
    assert_eq!(dec.arguments, vec!["'click'".to_string()]);
}

#[test]
fn unrelated_member_between_decorator_and_target_clears_pending_decorator() {
    // A decorator only applies to the *immediately following* member; this is
    // a defensive check that stray/unhandled member kinds don't cause a
    // decorator meant for one member to leak onto a later, unrelated one.
    let (program, decorators) = analyze_decorators(&[(
        "app.ts",
        r#"class Widget {
    @Deprecated()
    oldMethod(): void {}

    newMethod(): void {}
}"#,
    )]);
    let old_method = program
        .symbols
        .iter()
        .find(|s| s.name == "Widget.oldMethod")
        .unwrap();
    let new_method = program
        .symbols
        .iter()
        .find(|s| s.name == "Widget.newMethod")
        .unwrap();
    let dec = decorators.iter().find(|d| d.name == "Deprecated").unwrap();
    assert_eq!(symbol_target(&dec.target), Some(old_method.id));
    assert_ne!(symbol_target(&dec.target), Some(new_method.id));
}

#[test]
fn constructor_parameter_decorators_are_captured_by_position() {
    let (program, decorators) = analyze_decorators(&[(
        "app.ts",
        r#"class UserService {
    constructor(@Inject(TOKEN) private http: Http, private logger: Logger) {}
}"#,
    )]);
    let constructor = program
        .symbols
        .iter()
        .find(|s| s.name == "UserService.constructor")
        .unwrap();
    let dec = decorators
        .iter()
        .find(|d| d.name == "Inject")
        .expect("Inject decorator on the first parameter should be captured");
    match &dec.target {
        DecoratorTarget::Parameter { owner, index } => {
            assert_eq!(*owner, constructor.id);
            assert_eq!(*index, 0);
        }
        DecoratorTarget::Symbol(_) => panic!("parameter decorator should not target a Symbol"),
    }
    assert_eq!(dec.arguments, vec!["TOKEN".to_string()]);

    // The second (undecorated) parameter must not produce a decorator entry.
    assert_eq!(
        decorators
            .iter()
            .filter(|d| matches!(
                &d.target,
                DecoratorTarget::Parameter { owner, .. } if *owner == constructor.id
            ))
            .count(),
        1
    );
}

#[test]
fn method_parameter_decorators_use_correct_parameter_index() {
    let (program, decorators) = analyze_decorators(&[(
        "app.ts",
        "class Widget {\n    handle(a: number, @Decorate() b: number): void {}\n}",
    )]);
    let handle = program
        .symbols
        .iter()
        .find(|s| s.name == "Widget.handle")
        .unwrap();
    let dec = decorators.iter().find(|d| d.name == "Decorate").unwrap();
    match &dec.target {
        DecoratorTarget::Parameter { owner, index } => {
            assert_eq!(*owner, handle.id);
            assert_eq!(*index, 1, "the decorator is on the second parameter");
        }
        DecoratorTarget::Symbol(_) => panic!("parameter decorator should not target a Symbol"),
    }
}

#[test]
fn undecorated_class_produces_no_decorators() {
    let (_program, decorators) = analyze_decorators(&[("app.ts", "class Plain {}")]);
    assert!(decorators.is_empty());
}
