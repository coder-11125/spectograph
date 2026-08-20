//! Spectograph shared language-agnostic intermediate representation (IR).
//!
//! Every language analyzer (`spectograph-<lang>`) parses and resolves source into a
//! [`Program`]. All bloat analysis lives in this crate against [`Program`], so adding a
//! language is just another crate that produces one.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A language a `spectograph-<lang>` analyzer understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Language {
    Python,
    JavaScript,
    TypeScript,
    Go,
    Rust,
    Java,
    Kotlin,
    C,
    Cpp,
    Ruby,
}

impl Language {
    /// File extensions this language owns.
    pub fn extensions(&self) -> &'static [&'static str] {
        match self {
            Language::Python => &["py"],
            Language::JavaScript => &["js", "mjs", "cjs", "jsx"],
            Language::TypeScript => &["ts", "tsx", "mts", "cts"],
            Language::Go => &["go"],
            Language::Rust => &["rs"],
            Language::Java => &["java"],
            Language::Kotlin => &["kt", "kts"],
            Language::C => &["c", "h"],
            Language::Cpp => &["cpp", "cc", "cxx", "hpp", "hxx"],
            Language::Ruby => &["rb"],
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Language::Python => "python",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Go => "go",
            Language::Rust => "rust",
            Language::Java => "java",
            Language::Kotlin => "kotlin",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::Ruby => "ruby",
        }
    }
}

/// Unique identifier for a [`Symbol`] within a [`Program`].
pub type SymbolId = usize;

/// A single span of source (1-based line/column, half-open range).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpan {
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
}

impl SourceSpan {
    pub fn new(start_line: usize, start_col: usize, end_line: usize, end_col: usize) -> Self {
        Self {
            start_line,
            start_col,
            end_line,
            end_col,
        }
    }
}

/// The kind of code a [`Symbol`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    /// A function or method.
    Function,
    /// A class (or struct/record/interface).
    Class,
    /// An import / use / require statement binding.
    Import,
    /// An export / public re-export.
    Export,
    /// A module-level constant or variable binding.
    Const,
    /// A type alias or type definition.
    Type,
    /// A package/module (file-level container).
    Module,
}

/// A named, addressable unit of code discovered by an analyzer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub id: SymbolId,
    pub name: String,
    pub kind: SymbolKind,
    pub file: PathBuf,
    pub span: SourceSpan,
    /// True if this symbol is reachable from an explicit entrypoint.
    pub reachable: bool,
    /// The runtime this symbol is scoped to, when an analyzer can tell.
    pub environment: Environment,
    /// True if this symbol belongs to third-party library code (e.g. a
    /// `node_modules` package) rather than the project being analyzed. External
    /// symbols are excluded from bloat findings: their unreachable internals aren't
    /// actionable, and an analyzer's resolution heuristics are lower-fidelity for
    /// arbitrary dependency code than for the project's own source.
    pub is_external: bool,
}

/// The runtime a [`Symbol`] or [`Reference`] is scoped to.
///
/// Inferred from environment guards (`typeof window !== 'undefined'`,
/// `typeof process !== 'undefined'`), direct use of environment-only globals, or
/// bundler remapping (package.json `browser` field). Analyzers with no notion of
/// this distinction (e.g. Python) leave every symbol/reference at the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Environment {
    /// No environment-specific signal found; assumed to run anywhere.
    #[default]
    Universal,
    /// Guarded by, or dependent on, Node.js-only globals (`process`, `global`, `__dirname`, ...).
    Node,
    /// Guarded by, or dependent on, browser-only globals (`window`, `document`, ...).
    Browser,
}

/// How confident we are that a [`Reference`] really points where we think.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RefKind {
    /// Statically resolved, unambiguous.
    Static,
    /// A re-export that forwards usage (e.g. `from x import *`, barrel re-exports).
    ReExport,
    /// Dynamically resolved at runtime (e.g. dynamic `import()`, `__import__`).
    Dynamic,
    /// Resolved through reflection / string lookup.
    Reflection,
    /// Resolved through a string key / registry pattern.
    StringKeyed,
}

impl RefKind {
    /// A reference that is not statically trustworthy needs uncertainty handling.
    pub fn is_uncertain(&self) -> bool {
        !matches!(self, RefKind::Static)
    }
}

/// A directed edge: `from` references `to`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Reference {
    pub from: SymbolId,
    pub to: SymbolId,
    pub kind: RefKind,
    /// The runtime this reference is scoped to, when an analyzer can tell.
    pub environment: Environment,
}

/// A full analysis result for one language.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Program {
    pub language: Language,
    pub root: PathBuf,
    pub symbols: Vec<Symbol>,
    pub references: Vec<Reference>,
    /// Symbols explicitly designated as entrypoints (e.g. `main`, package entry).
    pub entrypoints: Vec<SymbolId>,
    /// Symbols reachable from an entrypoint (filled by analysis).
    pub reachable: Vec<SymbolId>,
}

impl Program {
    pub fn symbol(&self, id: SymbolId) -> Option<&Symbol> {
        self.symbols.get(id)
    }

    /// References *to* `id` (who uses it).
    pub fn incoming(&self, id: SymbolId) -> impl Iterator<Item = &Reference> {
        self.references.iter().filter(move |r| r.to == id)
    }

    /// References *from* `id` (what it uses).
    pub fn outgoing(&self, id: SymbolId) -> impl Iterator<Item = &Reference> {
        self.references.iter().filter(move |r| r.from == id)
    }
}
