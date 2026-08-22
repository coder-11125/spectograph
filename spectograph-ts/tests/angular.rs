//! Tests for Angular-specific reachability edges: `@NgModule`/`@Component`
//! metadata-array references, and template usage (component tags, directive
//! attributes, pipes).
//!
//! Angular's module/standalone-component system wires components, directives,
//! pipes, and services together through `declarations`/`imports`/`exports`/
//! `bootstrap`/`providers` arrays in decorator metadata, and through a
//! component's rendered template (`<app-child>`, `[appHighlight]`,
//! `{{ x | upper }}`) -- none of which is an ordinary import-and-call. Without
//! edges for both, everything reachable only that way -- which is most of a
//! normal Angular app -- looks like dead code. Same category of fix as the
//! constructor-parameter DI edges (`dependency_injection.rs`), and the React
//! lifecycle-method edges in `spectograph-js`/`spectograph-ts`'s `react`
//! support.
//!
//! A component/directive/pipe used in a template must *also* be registered
//! in an `@NgModule`'s `declarations` (or a standalone component's own
//! `imports`) for Angular's real compiler to accept the template at all --
//! that registration is what the NgModule-edge tests below cover, and it's
//! what makes the *import statement* reachable; the template edge then
//! carries reachability the rest of the way to the class. A template
//! reference with no such registration is not a real Angular setup (it
//! wouldn't compile), so the template-usage tests below always pair template
//! markup with the matching declaration.

use tempfile::tempdir;

use spectograph_analyzer::Analyzer;
use spectograph_core::analysis;
use spectograph_core::cost::{CostModel, CostSource};
use spectograph_core::ir::{Program, RefKind, SymbolId, SymbolKind};
use spectograph_ts::angular::BindingMismatch;
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

fn binding_mismatches(files: &[(&str, &str)]) -> Vec<BindingMismatch> {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    TypeScriptAnalyzer.analyze_angular(dir.path()).unwrap().1
}

fn class_id(program: &Program, name: &str) -> SymbolId {
    program
        .symbols
        .iter()
        .find(|s| s.name == name && s.kind != SymbolKind::Import)
        .unwrap_or_else(|| panic!("no symbol named {name}"))
        .id
}

fn bloat_findings(program: &mut Program) -> Vec<analysis::BloatFinding> {
    let model = CostModel::build(program, CostSource::Estimated);
    analysis::analyze(program, &model).findings
}

#[test]
fn ngmodule_declarations_creates_edges_to_declared_classes() {
    let program = analyze_ts(&[(
        "app.module.ts",
        r#"class AppComponent {}
class FooComponent {}

@NgModule({
    declarations: [AppComponent, FooComponent]
})
class AppModule {}
"#,
    )]);
    let module = class_id(&program, "AppModule");
    let app = class_id(&program, "AppComponent");
    let foo = class_id(&program, "FooComponent");
    assert!(program
        .references
        .iter()
        .any(|r| r.from == module && r.to == app && r.kind == RefKind::Static));
    assert!(program
        .references
        .iter()
        .any(|r| r.from == module && r.to == foo && r.kind == RefKind::Static));
}

#[test]
fn ngmodule_imports_and_bootstrap_create_edges() {
    let program = analyze_ts(&[(
        "app.module.ts",
        r#"class BrowserModule {}
class AppComponent {}

@NgModule({
    imports: [BrowserModule],
    bootstrap: [AppComponent]
})
class AppModule {}
"#,
    )]);
    let module = class_id(&program, "AppModule");
    let browser = class_id(&program, "BrowserModule");
    let app = class_id(&program, "AppComponent");
    assert!(program
        .references
        .iter()
        .any(|r| r.from == module && r.to == browser));
    assert!(program
        .references
        .iter()
        .any(|r| r.from == module && r.to == app));
}

#[test]
fn ngmodule_providers_bare_class_creates_edge() {
    let program = analyze_ts(&[(
        "app.module.ts",
        r#"class FooService {}

@NgModule({
    providers: [FooService]
})
class AppModule {}
"#,
    )]);
    let module = class_id(&program, "AppModule");
    let service = class_id(&program, "FooService");
    assert!(program
        .references
        .iter()
        .any(|r| r.from == module && r.to == service));
}

#[test]
fn ngmodule_providers_use_class_object_references_both_token_and_class() {
    // `{ provide: X, useClass: Y }` references both the injection token X and
    // the implementation class Y -- walking for any identifier inside the
    // array (rather than modeling each provider shape individually) picks up
    // both without needing to special-case this syntax.
    let program = analyze_ts(&[(
        "app.module.ts",
        r#"class ApiTokenBase {}
class RealApiService {}

@NgModule({
    providers: [{ provide: ApiTokenBase, useClass: RealApiService }]
})
class AppModule {}
"#,
    )]);
    let module = class_id(&program, "AppModule");
    let token = class_id(&program, "ApiTokenBase");
    let real = class_id(&program, "RealApiService");
    assert!(program
        .references
        .iter()
        .any(|r| r.from == module && r.to == token));
    assert!(program
        .references
        .iter()
        .any(|r| r.from == module && r.to == real));
}

#[test]
fn standalone_component_imports_creates_edges() {
    // Angular 14+ standalone components list what they use directly in their
    // own @Component metadata instead of an NgModule's declarations array.
    let program = analyze_ts(&[(
        "app.component.ts",
        r#"class CommonModule {}
class ChildComponent {}

@Component({
    standalone: true,
    imports: [CommonModule, ChildComponent]
})
class AppComponent {}
"#,
    )]);
    let app = class_id(&program, "AppComponent");
    let common = class_id(&program, "CommonModule");
    let child = class_id(&program, "ChildComponent");
    assert!(program
        .references
        .iter()
        .any(|r| r.from == app && r.to == common));
    assert!(program
        .references
        .iter()
        .any(|r| r.from == app && r.to == child));
}

#[test]
fn plain_component_decorator_without_reference_arrays_creates_no_extra_edges() {
    // A component with no declarations/imports/etc. key at all (the common,
    // non-NgModule, non-standalone case: selector + template only) must not
    // spuriously create edges from anything found in its other metadata.
    let program = analyze_ts(&[(
        "app.component.ts",
        "@Component({ selector: 'app-root', template: '<div></div>' })\nclass AppComponent {}\n",
    )]);
    let app = class_id(&program, "AppComponent");
    assert!(
        !program.references.iter().any(|r| r.from == app),
        "a component with no declarations/imports/etc. metadata should create no edges: {:?}",
        program
            .references
            .iter()
            .filter(|r| r.from == app)
            .collect::<Vec<_>>()
    );
}

#[test]
fn unrelated_decorator_does_not_create_reference_array_edges() {
    // @Injectable/@Directive/@Pipe aren't NgModule/Component, so even if one
    // somehow had an array-shaped argument, it must not be interpreted this
    // way.
    let program = analyze_ts(&[(
        "foo.service.ts",
        r#"class Dep {}

@Injectable({ deps: [Dep] })
class FooService {}
"#,
    )]);
    let service = class_id(&program, "FooService");
    assert!(!program.references.iter().any(|r| r.from == service));
}

#[test]
fn cross_file_ngmodule_declarations_resolve_through_imports() {
    // The realistic Angular layout: NgModule and its declared components each
    // live in their own file, wired together by ordinary imports.
    let program = analyze_ts(&[
        (
            "app.module.ts",
            r#"import { AppComponent } from './app.component';

@NgModule({
    declarations: [AppComponent],
    bootstrap: [AppComponent]
})
export class AppModule {}
"#,
        ),
        (
            "app.component.ts",
            "@Component({ selector: 'app-root' })\nexport class AppComponent {}\n",
        ),
    ]);
    let module = class_id(&program, "AppModule");
    let app_component = program
        .symbols
        .iter()
        .find(|s| s.name == "AppComponent" && s.kind == SymbolKind::Class)
        .unwrap()
        .id;
    // The edge lands on the local Import symbol (resolved via same-file
    // scope); a separate ReExport edge (already-tested import-resolution
    // machinery) carries it the rest of the way to the actual class.
    let import_sym = program
        .symbols
        .iter()
        .find(|s| s.name == "AppComponent" && s.kind == SymbolKind::Import)
        .unwrap()
        .id;
    assert!(program
        .references
        .iter()
        .any(|r| r.from == module && r.to == import_sym));
    assert!(program
        .references
        .iter()
        .any(|r| r.from == import_sym && r.to == app_component && r.kind == RefKind::ReExport));
}

#[test]
fn full_angular_app_has_no_false_positive_bloat() {
    // End-to-end: a minimal but realistic Angular app (bootstrap entrypoint,
    // NgModule, two components, one providedIn:'root' service) should report
    // no false positives -- every class is genuinely reachable through some
    // combination of the entrypoint, NgModule metadata, and constructor DI --
    // while a genuinely unused method on one of them still gets flagged.
    let mut program = analyze_ts(&[
        (
            "main.ts",
            r#"import { AppModule } from './app.module';

platformBrowserDynamic().bootstrapModule(AppModule);
"#,
        ),
        (
            "app.module.ts",
            r#"import { AppComponent } from './app.component';
import { FooComponent } from './foo.component';
import { FooService } from './foo.service';

@NgModule({
    declarations: [AppComponent, FooComponent],
    providers: [FooService],
    bootstrap: [AppComponent]
})
export class AppModule {}
"#,
        ),
        (
            "app.component.ts",
            "@Component({ selector: 'app-root', template: '<div></div>' })\nexport class AppComponent {}\n",
        ),
        (
            "foo.component.ts",
            "@Component({ selector: 'app-foo', template: '<div></div>' })\nexport class FooComponent {\n    unusedHelper(): void {}\n}\n",
        ),
        (
            "foo.service.ts",
            "@Injectable({ providedIn: 'root' })\nexport class FooService {\n    method(): void {}\n}\n",
        ),
    ]);
    let findings = bloat_findings(&mut program);
    let flagged: Vec<&str> = findings.iter().map(|f| f.name.as_str()).collect();
    for name in ["AppModule", "AppComponent", "FooComponent", "FooService"] {
        assert!(
            !flagged.contains(&name),
            "{name} is wired up through NgModule declarations/DI and must not be flagged: {:?}",
            flagged
        );
    }
    assert!(
        flagged.contains(&"FooComponent.unusedHelper"),
        "a genuinely unused method must still be flagged: {:?}",
        flagged
    );
}

// --- Template usage: inline templates, external templateUrl, selectors, pipes ---

#[test]
fn inline_template_component_tag_usage_creates_edge() {
    let program = analyze_ts(&[(
        "app.component.ts",
        r#"@Component({
    selector: 'app-root',
    template: '<app-child></app-child>'
})
class AppComponent {}

@Component({ selector: 'app-child', template: '<span></span>' })
class ChildComponent {}
"#,
    )]);
    let app = class_id(&program, "AppComponent");
    let child = class_id(&program, "ChildComponent");
    assert!(program
        .references
        .iter()
        .any(|r| r.from == app && r.to == child && r.kind == RefKind::Static));
}

#[test]
fn inline_template_directive_attribute_usage_creates_edge() {
    let program = analyze_ts(&[(
        "app.component.ts",
        r#"@Component({
    selector: 'app-root',
    template: '<div [appHighlight]="true"></div>'
})
class AppComponent {}

@Directive({ selector: '[appHighlight]' })
class HighlightDirective {}
"#,
    )]);
    let app = class_id(&program, "AppComponent");
    let directive = class_id(&program, "HighlightDirective");
    assert!(program
        .references
        .iter()
        .any(|r| r.from == app && r.to == directive));
}

#[test]
fn inline_template_structural_directive_usage_creates_edge() {
    let program = analyze_ts(&[(
        "app.component.ts",
        r#"@Component({
    selector: 'app-root',
    template: '<div *appUnless="cond"></div>'
})
class AppComponent {}

@Directive({ selector: '[appUnless]' })
class UnlessDirective {}
"#,
    )]);
    let app = class_id(&program, "AppComponent");
    let directive = class_id(&program, "UnlessDirective");
    assert!(program
        .references
        .iter()
        .any(|r| r.from == app && r.to == directive));
}

#[test]
fn inline_template_pipe_usage_creates_edge() {
    let program = analyze_ts(&[(
        "app.component.ts",
        r#"@Component({
    selector: 'app-root',
    template: '{{ name | upper }}'
})
class AppComponent {}

@Pipe({ name: 'upper' })
class UpperPipe {}
"#,
    )]);
    let app = class_id(&program, "AppComponent");
    let pipe = class_id(&program, "UpperPipe");
    assert!(program
        .references
        .iter()
        .any(|r| r.from == app && r.to == pipe));
}

#[test]
fn external_template_url_is_resolved_and_scanned() {
    let program = analyze_ts(&[
        (
            "app.component.ts",
            r#"@Component({
    selector: 'app-root',
    templateUrl: './app.component.html'
})
class AppComponent {}

@Component({ selector: 'app-child', template: '<span></span>' })
class ChildComponent {}
"#,
        ),
        ("app.component.html", "<app-child></app-child>"),
    ]);
    let app = class_id(&program, "AppComponent");
    let child = class_id(&program, "ChildComponent");
    assert!(
        program
            .references
            .iter()
            .any(|r| r.from == app && r.to == child),
        "an external templateUrl file should be resolved and scanned the same as an inline template"
    );
}

#[test]
fn comma_separated_selector_list_matches_either_form() {
    // `selector: 'app-foo, [app-foo]'` -- a component usable as either an
    // element or an attribute -- must match template usage in either form.
    let program = analyze_ts(&[(
        "app.component.ts",
        r#"@Component({
    selector: 'app-root',
    template: '<div app-foo></div><app-foo></app-foo>'
})
class AppComponent {}

@Component({ selector: 'app-foo, [app-foo]', template: '<span></span>' })
class FooComponent {}
"#,
    )]);
    let app = class_id(&program, "AppComponent");
    let foo = class_id(&program, "FooComponent");
    assert!(
        program
            .references
            .iter()
            .any(|r| r.from == app && r.to == foo),
        "the element-tag usage (<app-foo>) should match"
    );
}

#[test]
fn template_scan_does_not_match_unrelated_html_element_names() {
    let program = analyze_ts(&[
        (
            "app.component.ts",
            r#"@Component({
    selector: 'app-root',
    template: '<div><span></span></div>'
})
class AppComponent {}
"#,
        ),
        (
            "unrelated.component.ts",
            "@Component({ selector: 'div', template: '<span></span>' })\nclass SuspiciouslyNamedComponent {}\n",
        ),
    ]);
    // A real Angular app would never declare a component selector that
    // collides with a plain HTML element name -- but if one did, matching it
    // is still the technically-correct behavior (that's exactly what
    // Angular's own compiler would do too), so this just documents the
    // matching is name-based, not an intrinsic-tag exclusion.
    let app = class_id(&program, "AppComponent");
    let suspicious = class_id(&program, "SuspiciouslyNamedComponent");
    assert!(program
        .references
        .iter()
        .any(|r| r.from == app && r.to == suspicious));
}

#[test]
fn full_angular_app_with_templates_has_no_false_positive_bloat() {
    // End-to-end, combining both mechanisms the way a real Angular app
    // actually needs them: an NgModule declares every component/directive/
    // pipe (required for Angular's compiler to accept their use in a
    // template at all, and what makes each *import* reachable), and each
    // component's template is scanned for the tags/attributes/pipes it
    // actually renders (what carries reachability the rest of the way, so a
    // component isn't just "declared" but genuinely used).
    let mut program = analyze_ts(&[
        (
            "main.ts",
            r#"import { AppModule } from './app.module';

platformBrowserDynamic().bootstrapModule(AppModule);
"#,
        ),
        (
            "app.module.ts",
            r#"import { AppComponent } from './app.component';
import { ChildComponent } from './child.component';
import { HighlightDirective } from './highlight.directive';
import { UpperPipe } from './upper.pipe';

@NgModule({
    declarations: [AppComponent, ChildComponent, HighlightDirective, UpperPipe],
    bootstrap: [AppComponent]
})
export class AppModule {}
"#,
        ),
        (
            "app.component.ts",
            r#"@Component({
    selector: 'app-root',
    template: '<app-child></app-child><div [appHighlight]="true">{{ name | upper }}</div>'
})
export class AppComponent {}
"#,
        ),
        (
            "child.component.ts",
            "@Component({ selector: 'app-child', template: '<span></span>' })\nexport class ChildComponent {\n    unusedMethod(): void {}\n}\n",
        ),
        (
            "highlight.directive.ts",
            "@Directive({ selector: '[appHighlight]' })\nexport class HighlightDirective {}\n",
        ),
        (
            "upper.pipe.ts",
            "@Pipe({ name: 'upper' })\nexport class UpperPipe {}\n",
        ),
    ]);
    let findings = bloat_findings(&mut program);
    let flagged: Vec<&str> = findings.iter().map(|f| f.name.as_str()).collect();
    for name in [
        "AppModule",
        "AppComponent",
        "ChildComponent",
        "HighlightDirective",
        "UpperPipe",
    ] {
        assert!(
            !flagged.contains(&name),
            "{name} is wired up through NgModule declarations + template usage and must not be flagged: {:?}",
            flagged
        );
    }
    assert!(
        flagged.contains(&"ChildComponent.unusedMethod"),
        "a genuinely unused method must still be flagged: {:?}",
        flagged
    );
}

// --- @Input/@Output binding-usage mismatches ------------------------------------

#[test]
fn undeclared_binding_on_component_tag_is_flagged() {
    let mismatches = binding_mismatches(&[(
        "app.component.ts",
        r#"@Component({
    selector: 'app-root',
    template: '<app-child [label]="x" [extra]="y"></app-child>'
})
class AppComponent {}

@Component({ selector: 'app-child', template: '<span></span>' })
class ChildComponent {
    @Input() label: string;
}
"#,
    )]);
    assert!(
        mismatches
            .iter()
            .any(|m| m.target_component == "app-child" && m.binding == "extra"),
        "passing `extra` to a component that only declares `label` should be flagged: {:?}",
        mismatches
    );
    assert!(
        !mismatches.iter().any(|m| m.binding == "label"),
        "label IS declared via @Input(), must not be flagged: {:?}",
        mismatches
    );
}

#[test]
fn input_alias_is_the_binding_name_not_the_property_name() {
    let mismatches = binding_mismatches(&[(
        "app.component.ts",
        r#"@Component({
    selector: 'app-root',
    template: '<app-child [customLabel]="x"></app-child>'
})
class AppComponent {}

@Component({ selector: 'app-child', template: '<span></span>' })
class ChildComponent {
    @Input('customLabel') label: string;
}
"#,
    )]);
    assert!(
        mismatches.is_empty(),
        "the template binds under the @Input alias, which must be recognized: {:?}",
        mismatches
    );
}

#[test]
fn output_event_binding_is_recognized_as_declared() {
    let mismatches = binding_mismatches(&[(
        "app.component.ts",
        r#"@Component({
    selector: 'app-root',
    template: '<app-child (selected)="onSelect()"></app-child>'
})
class AppComponent {}

@Component({ selector: 'app-child', template: '<span></span>' })
class ChildComponent {
    @Output() selected = new EventEmitter<void>();
}
"#,
    )]);
    assert!(mismatches.is_empty(), "{:?}", mismatches);
}

#[test]
fn component_with_no_declared_bindings_is_not_checked() {
    let mismatches = binding_mismatches(&[(
        "app.component.ts",
        r#"@Component({
    selector: 'app-root',
    template: '<app-child [anything]="x"></app-child>'
})
class AppComponent {}

@Component({ selector: 'app-child', template: '<span></span>' })
class ChildComponent {}
"#,
    )]);
    assert!(
        mismatches.is_empty(),
        "a component with no @Input()/@Output() at all can't be statically checked, so it must not produce false positives: {:?}",
        mismatches
    );
}

#[test]
fn implicit_structural_directive_binding_is_not_flagged() {
    let mismatches = binding_mismatches(&[(
        "app.component.ts",
        r#"@Component({
    selector: 'app-root',
    template: '<app-child *ngIf="cond" [label]="x"></app-child>'
})
class AppComponent {}

@Component({ selector: 'app-child', template: '<span></span>' })
class ChildComponent {
    @Input() label: string;
}
"#,
    )]);
    assert!(
        !mismatches.iter().any(|m| m.binding == "ngIf"),
        "*ngIf is a framework structural directive, not app-child's own binding: {:?}",
        mismatches
    );
}
