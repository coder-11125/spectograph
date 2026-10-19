//! A default import binds the target module's default export. When no `default` symbol
//! exists, the importer's binding name is used to guess the declaration -- but the guess
//! must never land on an `import` binding inside the target module that merely shares the
//! name (found in `sindresorhus/got`).

use tempfile::tempdir;

use spectograph_analyzer::Analyzer;
use spectograph_core::analysis::{self, BloatTier};
use spectograph_core::cost::{CostModel, CostSource};
use spectograph_ts::TypeScriptAnalyzer;

fn dead(files: &[(&str, &str)]) -> Vec<String> {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    let mut program = TypeScriptAnalyzer.analyze(dir.path()).unwrap();
    let model = CostModel::build(&program, CostSource::Estimated);
    let mut names: Vec<String> = analysis::analyze(&mut program, &model)
        .findings
        .into_iter()
        .filter(|f| f.tier == BloatTier::Guaranteed)
        .map(|f| f.name)
        .collect();
    names.sort();
    names
}

#[test]
fn default_import_does_not_link_to_a_same_named_import_in_the_target() {
    let found = dead(&[
        ("package.json", r#"{"main": "src/index.ts"}"#),
        (
            "src/index.ts",
            "import decompressResponse from './decompress.js';\nexport const x = decompressResponse;\n",
        ),
        (
            "src/decompress.ts",
            "import decompressResponse from 'decompress-response';\nexport default function decompressResponseBody() { return decompressResponse; }\n",
        ),
    ]);
    assert!(
        !found.contains(&"decompressResponseBody".to_string()),
        "the real default export must stay reachable, got {found:?}"
    );
}
