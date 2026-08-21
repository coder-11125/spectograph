//! `typescript-language-server`-backed type-aware reference resolution.
//!
//! Mirrors `spectograph-py::pyright`: instead of hand-rolling type inference,
//! this shells out to `typescript-language-server` (a wrapper around
//! Microsoft's `tsserver`) running as a Language Server over stdio, and uses
//! its `textDocument/references` to obtain a type-aware reference graph --
//! `obj.method()` resolves by `obj`'s real type rather than by name, and
//! generic/overloaded call sites resolve correctly.
//!
//! Lives in its own crate (rather than in `spectograph-ts`, alongside
//! `spectograph-py`'s `pyright.rs`) because the same server and client drive
//! both TypeScript *and* plain JavaScript (`allowJs`/`checkJs`):
//! `spectograph-js` can't depend on `spectograph-ts` (which already depends
//! on `spectograph-js` for its React support) without a cycle, and this isn't
//! generic enough to belong in `spectograph-analyzer` -- it's JS/TS-ecosystem
//! specific (npm, node's `tsserver.js`), not something a Python or Go
//! analyzer would ever call.
//!
//! If the server is unavailable (not installed, spawn failure, timeout),
//! callers should fall back to each language's static resolver.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::str::FromStr;

use lsp_types::notification::{DidOpenTextDocument, Initialized, Notification};
use lsp_types::request::{Initialize, References, Request};
use lsp_types::{
    ClientCapabilities, DidOpenTextDocumentParams, InitializeParams, InitializedParams, Location,
    PartialResultParams, Position, ReferenceContext, ReferenceParams, TextDocumentIdentifier,
    TextDocumentItem, Uri, WorkDoneProgressParams,
};

use spectograph_core::ir::Language;

/// Errors from the tsserver LSP integration. Callers treat these as "fall back
/// to the static resolver."
#[derive(Debug, thiserror::Error)]
pub enum TsServerError {
    #[error("failed to start {0}: {1}. Install it with `npm install -g typescript-language-server typescript`, or use the static resolver.")]
    Spawn(String, std::io::Error),
    #[error("LSP error: {0}")]
    Lsp(String),
    #[error("no TypeScript installation found under {0}/node_modules or globally; install with `npm install typescript` (locally) or `npm install -g typescript`")]
    NoTypeScriptInstall(PathBuf),
}

/// A top-level symbol we want references for, mapped from our IR.
#[derive(Debug, Clone)]
pub struct QuerySymbol {
    /// Symbol id in the Program.
    pub id: usize,
    /// The file it is defined in (path as it should be opened).
    pub file: PathBuf,
    /// Definition position (0-based line/column, per LSP).
    pub position: Position,
}

/// Span of every non-module symbol, for mapping a reference location to its
/// enclosing symbol.
#[derive(Debug, Clone)]
pub struct SymbolSpan {
    pub id: usize,
    pub file: PathBuf,
    /// 1-based inclusive start line.
    pub start_line: usize,
    /// 1-based inclusive end line.
    pub end_line: usize,
}

/// A discovered edge: `from` references `to` (both symbol ids in the Program).
#[derive(Debug, Clone)]
pub struct ReferenceEdge {
    pub from: usize,
    pub to: usize,
}

/// Minimal JSON-RPC 2.0 framing over stdio (LSP protocol).
struct Framing {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Framing {
    fn start(executable: &str, args: &[&str]) -> Result<Self, TsServerError> {
        let mut child = Command::new(executable)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| TsServerError::Spawn(executable.to_string(), e))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TsServerError::Lsp("no stdin".into()))?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| TsServerError::Lsp("no stdout".into()))?,
        );
        Ok(Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        })
    }

    fn write_frame(&mut self, payload: &[u8]) -> Result<(), TsServerError> {
        write!(self.stdin, "Content-Length: {}\r\n\r\n", payload.len())
            .map_err(|e| TsServerError::Lsp(e.to_string()))?;
        self.stdin
            .write_all(payload)
            .map_err(|e| TsServerError::Lsp(e.to_string()))?;
        self.stdin
            .flush()
            .map_err(|e| TsServerError::Lsp(e.to_string()))
    }

    fn send_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<i64, TsServerError> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_frame(
            &serde_json::to_vec(&msg).map_err(|e| TsServerError::Lsp(e.to_string()))?,
        )?;
        Ok(id)
    }

    fn send_notification(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(), TsServerError> {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_frame(&serde_json::to_vec(&msg).map_err(|e| TsServerError::Lsp(e.to_string()))?)
    }

    fn send_response(&mut self, id: i64, result: serde_json::Value) -> Result<(), TsServerError> {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        self.write_frame(&serde_json::to_vec(&msg).map_err(|e| TsServerError::Lsp(e.to_string()))?)
    }

    fn read_message(&mut self) -> Result<serde_json::Value, TsServerError> {
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            let n = self
                .stdout
                .read_line(&mut line)
                .map_err(|e| TsServerError::Lsp(e.to_string()))?;
            if n == 0 {
                return Err(TsServerError::Lsp("server closed stdout".into()));
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            if let Some(len) = trimmed.strip_prefix("Content-Length:") {
                content_length = len.trim().parse().ok();
            }
        }
        let len =
            content_length.ok_or_else(|| TsServerError::Lsp("missing Content-Length".into()))?;
        let mut buf = vec![0u8; len];
        self.stdout
            .read_exact(&mut buf)
            .map_err(|e| TsServerError::Lsp(e.to_string()))?;
        serde_json::from_slice(&buf).map_err(|e| TsServerError::Lsp(format!("bad JSON: {e}")))
    }

    /// Drain until a response to one of our requests arrives. Notifications
    /// (server->client requests are answered with null; anything else,
    /// including `textDocument/publishDiagnostics` -- which a clean file may
    /// never receive at all -- is simply skipped) are handled inline. We
    /// don't wait on diagnostics to know when the server is "ready": tsserver
    /// serializes requests after `didOpen` internally, so the next response
    /// we're actually waiting for is reliable without a separate settle step.
    fn next_request_result(&mut self) -> Result<(i64, serde_json::Value), TsServerError> {
        loop {
            let msg = self.read_message()?;
            if let Some(_method) = msg.get("method").and_then(|m| m.as_str()) {
                if let Some(id) = msg.get("id") {
                    // Server -> client request: answer null.
                    self.send_response(id.as_i64().unwrap_or(0), serde_json::Value::Null)?;
                }
                continue;
            }
            let id = msg
                .get("id")
                .and_then(|i| i.as_i64())
                .ok_or_else(|| TsServerError::Lsp("response missing id".into()))?;
            if let Some(error) = msg.get("error") {
                let message = error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error");
                return Err(TsServerError::Lsp(format!(
                    "request {id} failed: {message}"
                )));
            }
            let result = msg
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            return Ok((id, result));
        }
    }
}

fn uri_for_abs(path: &Path) -> Uri {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| path.to_path_buf())
            .join(path)
    };
    Uri::from_str(&format!("file://{}", abs.display()))
        .unwrap_or_else(|_| Uri::from_str("file:///").unwrap())
}

/// Convert a file `Uri` string back to a path by stripping the `file://` prefix.
fn uri_to_path(uri: &str) -> PathBuf {
    if let Some(rest) = uri.strip_prefix("file://") {
        PathBuf::from(rest)
    } else {
        PathBuf::from(uri)
    }
}

/// LSP `languageId` for a source file, based on its extension. `typescript-language-server`
/// uses this both to pick the right grammar (JSX-aware `*react` dialects for
/// `.tsx`/`.jsx`) and, for `.js`/`.jsx`, to run its JavaScript (as opposed to
/// TypeScript) language service -- which still gets type-aware resolution via
/// `allowJs`/`checkJs` and inferred/JSDoc types, just without a `.ts` file's
/// explicit annotations.
fn language_id_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("tsx") => "typescriptreact",
        Some("jsx") => "javascriptreact",
        Some("js" | "mjs" | "cjs") => "javascript",
        _ => "typescript",
    }
}

/// Find the column (1-based) of `name` on `line` (1-based) of `file`, falling
/// back to column 1 if not found (e.g. a multi-line signature).
fn find_name_column(file: &Path, line: usize, name: &str) -> usize {
    if let Ok(src) = std::fs::read_to_string(file) {
        if let Some(src_line) = src.lines().nth(line.saturating_sub(1)) {
            if let Some(idx) = src_line.find(name) {
                return idx + 1;
            }
        }
    }
    1
}

/// Find the symbol whose span contains `line` in `file`, preferring the
/// smallest enclosing span (methods nested inside a class, etc.).
fn enclosing_symbol(spans: &[SymbolSpan], file: &Path, line: usize) -> Option<usize> {
    spans
        .iter()
        .filter(|s| s.file == file && line >= s.start_line && line <= s.end_line)
        .min_by_key(|s| s.end_line - s.start_line)
        .map(|s| s.id)
}

/// Locate a real TypeScript install to hand to `typescript-language-server`
/// as `initializationOptions.tsserver.path`. Without this, the server falls
/// back to a non-functional stub ("bundled" version) that can't type-check
/// anything and every query silently returns no results -- so this is
/// required, not an optimization. Needed for plain JavaScript projects too:
/// `typescript-language-server` always runs on top of a real `tsserver`, even
/// when every opened file is `.js`.
///
/// Tries `node_modules/typescript/lib/tsserver.js`, walking up from `root`
/// the same way Node module resolution would, then falls back to a globally
/// installed `typescript` package (`npm root -g`).
fn detect_typescript_path(root: &Path) -> Option<PathBuf> {
    let mut current = root;
    loop {
        let candidate = current
            .join("node_modules")
            .join("typescript")
            .join("lib")
            .join("tsserver.js");
        if candidate.is_file() {
            return Some(candidate);
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }

    let output = Command::new("npm").args(["root", "-g"]).output().ok()?;
    let global_root = String::from_utf8(output.stdout).ok()?;
    let candidate = PathBuf::from(global_root.trim())
        .join("typescript")
        .join("lib")
        .join("tsserver.js");
    candidate.is_file().then_some(candidate)
}

/// Drive `typescript-language-server` and produce the type-aware reference
/// edges.
///
/// `files`/`sources` are opened with the server. For each `queries` symbol,
/// its references are fetched and mapped to `(from, to)` edges using `spans`
/// to find the enclosing symbol of each reference location. The queried
/// symbol's own definition is excluded.
///
/// Bounded by `timeout_ms` for the whole exchange (not per-request): a
/// watchdog thread kills the server process if it hasn't finished by then, so
/// a stalled server can't hang the caller forever.
pub fn resolve_references(
    executable: &str,
    root: &Path,
    files: &[PathBuf],
    sources: &[&str],
    queries: &[QuerySymbol],
    spans: &[SymbolSpan],
    timeout_ms: u64,
) -> Result<Vec<ReferenceEdge>, TsServerError> {
    let ts_path = detect_typescript_path(root)
        .ok_or_else(|| TsServerError::NoTypeScriptInstall(root.to_path_buf()))?;

    let mut f = Framing::start(executable, &["--stdio"])?;

    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pid = f.child.id();
    {
        let done = done.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(timeout_ms));
            if !done.load(std::sync::atomic::Ordering::SeqCst) {
                #[cfg(unix)]
                let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
                #[cfg(windows)]
                let _ = Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/F"])
                    .status();
            }
        });
    }

    let outcome = drive_session(&mut f, root, files, sources, queries, spans, &ts_path);
    done.store(true, std::sync::atomic::Ordering::SeqCst);

    let _ = f.send_request("shutdown", serde_json::Value::Null);
    let _ = f.send_notification("exit", serde_json::Value::Null);
    let _ = f.child.kill();

    outcome
}

#[allow(clippy::too_many_arguments)]
fn drive_session(
    f: &mut Framing,
    root: &Path,
    files: &[PathBuf],
    sources: &[&str],
    queries: &[QuerySymbol],
    spans: &[SymbolSpan],
    ts_path: &Path,
) -> Result<Vec<ReferenceEdge>, TsServerError> {
    let init_params = InitializeParams {
        process_id: None,
        // `root_uri` is deprecated in favor of `workspace_folders`, but
        // `typescript-language-server` needs it anyway: without it, cross-file
        // `textDocument/references` silently degrades to same-file-only
        // results (confirmed empirically -- `workspace_folders` alone is not
        // enough for it to build the project's file graph before answering).
        #[allow(deprecated)]
        root_uri: Some(uri_for_abs(root)),
        workspace_folders: Some(vec![lsp_types::WorkspaceFolder {
            uri: uri_for_abs(root),
            name: root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }]),
        capabilities: ClientCapabilities::default(),
        initialization_options: Some(serde_json::json!({
            "tsserver": { "path": ts_path.to_string_lossy() }
        })),
        ..Default::default()
    };
    f.send_request(
        Initialize::METHOD,
        serde_json::to_value(&init_params).map_err(|e| TsServerError::Lsp(e.to_string()))?,
    )?;
    f.next_request_result()?;
    f.send_notification(
        Initialized::METHOD,
        serde_json::to_value(InitializedParams {})
            .map_err(|e| TsServerError::Lsp(e.to_string()))?,
    )?;

    // Open all source files. tsserver serializes requests after `didOpen`
    // internally, so the references queries below don't need to wait for a
    // separate "analysis settled" signal (and diagnostics -- the only such
    // signal LSP offers -- may never arrive at all for files with no errors).
    for (path, src) in files.iter().zip(sources) {
        let item = TextDocumentItem {
            uri: uri_for_abs(path),
            language_id: language_id_for(path).into(),
            version: 1,
            text: (*src).to_string(),
        };
        f.send_notification(
            DidOpenTextDocument::METHOD,
            serde_json::to_value(&DidOpenTextDocumentParams {
                text_document: item,
            })
            .map_err(|e| TsServerError::Lsp(e.to_string()))?,
        )?;
    }

    let mut edges: Vec<ReferenceEdge> = Vec::new();
    for qs in queries {
        let params = ReferenceParams {
            text_document_position: lsp_types::TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: uri_for_abs(&qs.file),
                },
                position: qs.position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration: true,
            },
        };
        f.send_request(
            References::METHOD,
            serde_json::to_value(&params).map_err(|e| TsServerError::Lsp(e.to_string()))?,
        )?;
        // Tolerate an individual query failing (e.g. a symbol tsserver can't
        // place a cursor on) without losing every edge already found.
        let Ok((_id, result)) = f.next_request_result() else {
            continue;
        };

        let mut locations: Vec<(PathBuf, usize)> = Vec::new();
        if let Some(arr) = result.as_array() {
            for loc in arr {
                if let Ok(l) = serde_json::from_value::<Location>(loc.clone()) {
                    let path = uri_to_path(l.uri.as_str());
                    locations.push((path, l.range.start.line as usize + 1)); // 1-based
                }
            }
        }

        let def_line = qs.position.line as usize + 1;
        for (file, line) in locations {
            if file == qs.file && line == def_line {
                continue; // definition itself
            }
            if let Some(from) = enclosing_symbol(spans, &file, line) {
                if from != qs.id {
                    edges.push(ReferenceEdge { from, to: qs.id });
                }
            }
        }
    }

    let mut seen = std::collections::HashSet::new();
    edges.retain(|e| seen.insert((e.from, e.to)));
    Ok(edges)
}

/// High-level orchestration: given a statically-built [`spectograph_core::ir::Program`]
/// for `language` (currently `TypeScript` or `JavaScript`), query
/// `typescript-language-server` for the type-aware reference edges between its
/// function/class/const symbols.
///
/// Returns `(from, to)` symbol-id pairs. Meant to be called after the static
/// analysis so the symbol table (with definitions and spans) already exists.
pub fn resolve_program_edges(
    root: &Path,
    program: &spectograph_core::ir::Program,
    language: Language,
    executable: &str,
    timeout_ms: u64,
) -> Result<Vec<(usize, usize)>, TsServerError> {
    use spectograph_core::ir::SymbolKind;

    let files: Vec<PathBuf> = spectograph_analyzer::discover::discover_files(root, language)
        .map_err(|e| TsServerError::Lsp(format!("discover {root:?}: {e}")))?;
    let abs_of = |p: &Path| -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(p)
        }
    };
    let abs_files: Vec<PathBuf> = files.iter().map(|f| abs_of(f)).collect();

    let mut sources = Vec::with_capacity(abs_files.len());
    for f in &abs_files {
        sources.push(
            std::fs::read_to_string(f)
                .map_err(|e| TsServerError::Lsp(format!("read {}: {e}", f.display())))?,
        );
    }
    let source_refs: Vec<&str> = sources.iter().map(|s| s.as_str()).collect();

    // Query symbols: same shape as Python's resolver -- functions, classes,
    // and top-level consts have referenceable definitions with proper spans.
    let mut queries = Vec::new();
    for sym in &program.symbols {
        if !matches!(
            sym.kind,
            SymbolKind::Function | SymbolKind::Class | SymbolKind::Const
        ) {
            continue;
        }
        let file = abs_of(&sym.file);
        // tsserver needs the position on the symbol's *name*, not the node
        // start (which is `function`/`class`/`const`).
        let short_name = sym.name.rsplit('.').next().unwrap_or(&sym.name);
        let name_col = find_name_column(&file, sym.span.start_line, short_name);
        queries.push(QuerySymbol {
            id: sym.id,
            file,
            position: Position {
                line: sym.span.start_line.saturating_sub(1) as u32,
                character: name_col.saturating_sub(1) as u32,
            },
        });
    }

    let spans: Vec<SymbolSpan> = program
        .symbols
        .iter()
        .filter(|s| s.kind != SymbolKind::Module)
        .map(|s| SymbolSpan {
            id: s.id,
            file: abs_of(&s.file),
            start_line: s.span.start_line,
            end_line: s.span.end_line,
        })
        .collect();

    let edges = resolve_references(
        executable,
        root,
        &abs_files,
        &source_refs,
        &queries,
        &spans,
        timeout_ms,
    )?;
    Ok(edges.into_iter().map(|e| (e.from, e.to)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_name_column_locates_name_in_line() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("m.ts");
        std::fs::write(
            &f,
            "class Greeter {\n    greet(): void {}\n}\nfunction plain(): void {}\n",
        )
        .unwrap();
        // "class Greeter {" -> Greeter starts at col 7 (0-based 6).
        assert_eq!(find_name_column(&f, 1, "Greeter"), 7);
        assert_eq!(find_name_column(&f, 2, "greet"), 5);
        assert_eq!(find_name_column(&f, 4, "plain"), 10);
        // Missing name -> fallback 1.
        assert_eq!(find_name_column(&f, 1, "nope"), 1);
    }

    #[test]
    fn uri_to_path_strips_file_prefix() {
        assert_eq!(
            uri_to_path("file:///Users/me/proj/src/index.ts"),
            PathBuf::from("/Users/me/proj/src/index.ts")
        );
        assert_eq!(uri_to_path("file:///x.ts"), PathBuf::from("/x.ts"));
    }

    #[test]
    fn language_id_distinguishes_ts_tsx_js_jsx() {
        assert_eq!(language_id_for(Path::new("App.tsx")), "typescriptreact");
        assert_eq!(language_id_for(Path::new("index.ts")), "typescript");
        assert_eq!(language_id_for(Path::new("types.d.ts")), "typescript");
        assert_eq!(language_id_for(Path::new("App.jsx")), "javascriptreact");
        assert_eq!(language_id_for(Path::new("index.js")), "javascript");
        assert_eq!(language_id_for(Path::new("index.mjs")), "javascript");
        assert_eq!(language_id_for(Path::new("index.cjs")), "javascript");
    }

    #[test]
    fn enclosing_symbol_picks_smallest_containing_span() {
        let spans = vec![
            SymbolSpan {
                id: 1,
                file: PathBuf::from("a.ts"),
                start_line: 1,
                end_line: 20,
            },
            SymbolSpan {
                id: 2,
                file: PathBuf::from("a.ts"),
                start_line: 5,
                end_line: 10,
            },
            SymbolSpan {
                id: 3,
                file: PathBuf::from("b.ts"),
                start_line: 1,
                end_line: 5,
            },
        ];
        assert_eq!(enclosing_symbol(&spans, &PathBuf::from("a.ts"), 7), Some(2));
        assert_eq!(enclosing_symbol(&spans, &PathBuf::from("a.ts"), 3), Some(1));
        assert_eq!(enclosing_symbol(&spans, &PathBuf::from("c.ts"), 3), None);
        assert_eq!(enclosing_symbol(&spans, &PathBuf::from("a.ts"), 30), None);
    }
}
