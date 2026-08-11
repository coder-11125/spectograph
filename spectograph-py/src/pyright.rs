//! basedpyright-backed type-safe reference resolution.
//!
//! Instead of hand-rolling type inference, this module shells out to
//! basedpyright (or pyright) running as a Language Server over stdio, and uses its
//! `textDocument/references` to obtain a type-aware reference graph: `obj.method()`
//! resolves by the object's real type, not by name.
//!
//! The LSP server is driven through a minimal JSON-RPC 2.0 stdio client built on
//! `lsp-types`. If basedpyright is unavailable (not installed, no environment, or
//! unsupported), callers should fall back to the static resolver.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::str::FromStr;

use lsp_types::notification::{DidOpenTextDocument, Initialized, Notification, PublishDiagnostics};
use lsp_types::request::{Initialize, References, Request};
use lsp_types::{
    ClientCapabilities, DidOpenTextDocumentParams, InitializeParams, InitializedParams, Location,
    PartialResultParams, Position, PublishDiagnosticsParams, ReferenceContext, ReferenceParams,
    TextDocumentIdentifier, TextDocumentItem, Uri, WorkDoneProgressParams,
};

/// Errors from the pyright LSP integration. Callers treat these as "fall back to the
/// static resolver."
#[derive(Debug, thiserror::Error)]
pub enum PyrightError {
    #[error("basedpyright executable not found ({0}). Install it with `pip install basedpyright` or use the static resolver.")]
    ExecutableNotFound(String),
    #[error("failed to start basedpyright: {0}")]
    Spawn(std::io::Error),
    #[error("LSP error: {0}")]
    Lsp(String),
    #[error("timed out waiting for analysis")]
    Timeout,
}

/// A top-level symbol we want references for, mapped from our IR.
pub struct QuerySymbol {
    /// Symbol id in the Program.
    pub id: usize,
    /// The file it is defined in (path as it should be opened).
    pub file: PathBuf,
    /// Definition position (0-based line/column, per LSP).
    pub position: Position,
}

/// Span of every non-module symbol, for mapping a reference location to its enclosing
/// symbol.
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
    /// received `textDocument/publishDiagnostics` counts per opened uri.
    diagnostics_seen: HashMap<String, usize>,
}

impl Framing {
    fn start(executable: &str) -> Result<Self, PyrightError> {
        let mut child = Command::new(executable)
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(PyrightError::Spawn)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| PyrightError::Lsp("no stdin".into()))?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| PyrightError::Lsp("no stdout".into()))?,
        );
        Ok(Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            diagnostics_seen: HashMap::new(),
        })
    }

    fn write_frame(&mut self, payload: &[u8]) -> Result<(), PyrightError> {
        write!(self.stdin, "Content-Length: {}\r\n\r\n", payload.len())
            .map_err(|e| PyrightError::Lsp(e.to_string()))?;
        self.stdin
            .write_all(payload)
            .map_err(|e| PyrightError::Lsp(e.to_string()))?;
        self.stdin
            .flush()
            .map_err(|e| PyrightError::Lsp(e.to_string()))
    }

    fn send_request(&mut self, method: &str, params: serde_json::Value) -> Result<i64, PyrightError> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_frame(&serde_json::to_vec(&msg).map_err(|e| PyrightError::Lsp(e.to_string()))?)?;
        Ok(id)
    }

    fn send_notification(&mut self, method: &str, params: serde_json::Value) -> Result<(), PyrightError> {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_frame(&serde_json::to_vec(&msg).map_err(|e| PyrightError::Lsp(e.to_string()))?)
    }

    fn send_response(&mut self, id: i64, result: serde_json::Value) -> Result<(), PyrightError> {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        self.write_frame(&serde_json::to_vec(&msg).map_err(|e| PyrightError::Lsp(e.to_string()))?)
    }

    fn read_message(&mut self) -> Result<serde_json::Value, PyrightError> {
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            let n = self
                .stdout
                .read_line(&mut line)
                .map_err(|e| PyrightError::Lsp(e.to_string()))?;
            if n == 0 {
                return Err(PyrightError::Lsp("server closed stdout".into()));
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
            content_length.ok_or_else(|| PyrightError::Lsp("missing Content-Length".into()))?;
        let mut buf = vec![0u8; len];
        self.stdout
            .read_exact(&mut buf)
            .map_err(|e| PyrightError::Lsp(e.to_string()))?;
        serde_json::from_slice(&buf).map_err(|e| PyrightError::Lsp(format!("bad JSON: {e}")))
    }

    /// Drain until a response to one of our requests arrives. Notifications (including
    /// server->client requests, answered with null) are handled inline.
    fn next_request_result(&mut self) -> Result<(i64, serde_json::Value), PyrightError> {
        loop {
            let msg = self.read_message()?;
            if let Some(method) = msg.get("method").and_then(|m| m.as_str()) {
                if let Some(id) = msg.get("id") {
                    // Server -> client request: answer null.
                    self.send_response(id.as_i64().unwrap_or(0), serde_json::Value::Null)?;
                } else if method == PublishDiagnostics::METHOD {
                    if let Ok(params) = serde_json::from_value::<PublishDiagnosticsParams>(
                        msg["params"].clone(),
                    ) {
                        *self.diagnostics_seen.entry(params.uri.to_string()).or_insert(0) += 1;
                    }
                }
                continue;
            }
            let id = msg
                .get("id")
                .and_then(|i| i.as_i64())
                .ok_or_else(|| PyrightError::Lsp("response missing id".into()))?;
            let result = msg.get("result").cloned().unwrap_or(serde_json::Value::Null);
            return Ok((id, result));
        }
    }

    fn wait_for_analysis(&mut self, opened: &[String], timeout_ms: u64) -> Result<(), PyrightError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        let mut settled = std::collections::HashSet::new();
        let mut iter = 0u32;
        while std::time::Instant::now() < deadline {
            if opened.iter().all(|u| {
                settled.contains(u) || self.diagnostics_seen.get(u).copied().unwrap_or(0) >= 1
            }) {
                if std::env::var("SPECT_OGRAPH_DEBUG_PYRIGHT").is_ok() {
                    eprintln!("[pyright] analysis settled after {iter} reads");
                }
                return Ok(());
            }
            let msg = self.read_message()?;
            iter += 1;
            if msg.get("method").and_then(|m| m.as_str()) == Some(PublishDiagnostics::METHOD) {
                if let Ok(params) = serde_json::from_value::<PublishDiagnosticsParams>(
                    msg["params"].clone(),
                ) {
                    if std::env::var("SPECT_OGRAPH_DEBUG_PYRIGHT").is_ok() {
                        eprintln!(
                            "[pyright] diagnostics for {} ({} findings)",
                            params.uri.as_str(),
                            params.diagnostics.len()
                        );
                    }
                    settled.insert(params.uri.to_string());
                }
            }
        }
        Err(PyrightError::Timeout)
    }
}

fn uri_for_abs(path: &Path) -> Uri {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| path.to_path_buf()).join(path)
    };
    Uri::from_str(&format!("file://{}", abs.display()))
        .unwrap_or_else(|_| Uri::from_str("file:///").unwrap())
}

/// Convert a file `Uri` string back to a path by stripping the `file://` prefix.
fn uri_to_path(uri: &str) -> PathBuf {
    if let Some(rest) = uri.strip_prefix("file://") {
        // fluent_uri keeps the path; percent-encoding is rare for project files.
        PathBuf::from(rest)
    } else {
        PathBuf::from(uri)
    }
}

/// Drive basedpyright and produce the type-aware reference edges.
///
/// `files`/`sources` are opened with the server. For each `queries` symbol, its
/// references are fetched and mapped to `(from, to)` edges using `spans` to find the
/// enclosing symbol of each reference location. The queried symbol's own definition is
/// excluded.
pub fn resolve_references(
    executable: &str,
    root: &Path,
    files: &[PathBuf],
    sources: &[&str],
    queries: &[QuerySymbol],
    spans: &[SymbolSpan],
    timeout_ms: u64,
) -> Result<Vec<ReferenceEdge>, PyrightError> {
    let mut f = Framing::start(executable)?;

    // Handshake: initialize.
    let init_params = InitializeParams {
        process_id: None,
        workspace_folders: Some(vec![lsp_types::WorkspaceFolder {
            uri: uri_for_abs(root),
            name: root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }]),
        capabilities: ClientCapabilities::default(),
        ..Default::default()
    };
    let _ = f.send_request(
        Initialize::METHOD,
        serde_json::to_value(&init_params).map_err(|e| PyrightError::Lsp(e.to_string()))?,
    )?;
    let _ = f.next_request_result()?;
    let _ = f.send_notification(
        Initialized::METHOD,
        serde_json::to_value(InitializedParams {}).map_err(|e| PyrightError::Lsp(e.to_string()))?,
    );

    // Open all source files.
    let mut opened_uris = Vec::new();
    for (path, src) in files.iter().zip(sources) {
        let uri = uri_for_abs(path);
        opened_uris.push(uri.to_string());
        let item = TextDocumentItem {
            uri,
            language_id: "python".into(),
            version: 1,
            text: (*src).to_string(),
        };
        let params = DidOpenTextDocumentParams { text_document: item };
        f.send_notification(
            DidOpenTextDocument::METHOD,
            serde_json::to_value(&params).map_err(|e| PyrightError::Lsp(e.to_string()))?,
        )?;
    }

    // Wait for analysis to settle.
    f.wait_for_analysis(&opened_uris, timeout_ms)?;

    // Query references for each symbol.
    let mut edges: Vec<ReferenceEdge> = Vec::new();
    for qs in queries {
        let td = TextDocumentIdentifier {
            uri: uri_for_abs(&qs.file),
        };
        let params = ReferenceParams {
            text_document_position: lsp_types::TextDocumentPositionParams {
                text_document: td,
                position: qs.position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration: true,
            },
        };
        let req_id = f.send_request(
            References::METHOD,
            serde_json::to_value(&params).map_err(|e| PyrightError::Lsp(e.to_string()))?,
        )?;
        let refs = loop {
            let (rid, result) = f.next_request_result()?;
            if rid == req_id {
                break result;
            }
        };

        let mut locations: Vec<(PathBuf, usize)> = Vec::new();
        if let Some(arr) = refs.as_array() {
            for loc in arr {
                if let Ok(l) = serde_json::from_value::<Location>(loc.clone()) {
                    let path = uri_to_path(l.uri.as_str());
                    locations.push((path, l.range.start.line as usize + 1)); // 1-based
                }
            }
        }
        if std::env::var("SPECT_OGRAPH_DEBUG_PYRIGHT").is_ok() {
            eprintln!(
                "[pyright] query id={} file={} pos={}:{} -> {} raw references",
                qs.id,
                qs.file.display(),
                qs.position.line + 1,
                qs.position.character + 1,
                locations.len()
            );
        }

        // Skip the queried symbol's own definition; map the rest to enclosing symbols.
        let def_line = qs.position.line as usize + 1;
        for (file, line) in locations {
            if file == qs.file && line == def_line {
                continue; // definition itself
            }
            if let Some(from) = enclosing_symbol(&file, line, spans) {
                if from != qs.id {
                    edges.push(ReferenceEdge { from, to: qs.id });
                }
            }
        }
    }

    // Shutdown.
    let _ = f.send_request("shutdown", serde_json::Value::Null);
    let _ = f.send_notification("exit", serde_json::Value::Null);
    let _ = f.child.kill();

    Ok(edges)
}

/// Find the symbol whose span (same file, line within [start,end]) contains `line`,
/// preferring the smallest enclosing span.
fn enclosing_symbol(file: &Path, line: usize, spans: &[SymbolSpan]) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None; // (id, span_size)
    for s in spans {
        if s.file == file && line >= s.start_line && line <= s.end_line {
            let size = s.end_line - s.start_line;
            if best.map(|(_, b)| size < b).unwrap_or(true) {
                best = Some((s.id, size));
            }
        }
    }
    best.map(|(id, _)| id)
}

/// High-level orchestration: given a statically-built [`Program`], query basedpyright
/// for the type-aware reference edges between its function/class symbols.
///
/// Returns `(from, to)` symbol-id pairs. This is meant to be called after the static
/// analysis so the symbol table (with definitions, spans, entrypoints) already exists.
pub fn resolve_program_edges(
    root: &Path,
    program: &spectograph_core::ir::Program,
    executable: &str,
    timeout_ms: u64,
) -> Result<Vec<(usize, usize)>, PyrightError> {
    use spectograph_core::ir::{Language, SymbolKind};
    use lsp_types::Position;

    // Discover and read source files; normalize to absolute paths so that symbol.file,
    // opened uris, and returned reference locations all agree.
    let files: Vec<PathBuf> = spectograph_analyzer::discover::discover_files(root, Language::Python)
        .map_err(|e| PyrightError::Lsp(format!("discover {root:?}: {e}")))?;
    // discover_files returns paths already prefixed with the (possibly relative) root.
    // Make them absolute against the current working directory, without doubling root.
    let abs_of = |p: &Path| -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(p)
        }
    };
    let abs_files: Vec<PathBuf> = files.iter().map(|f| abs_of(f)).collect();
    let mut sources = Vec::new();
    for f in &abs_files {
        let src = std::fs::read_to_string(f)
            .map_err(|e| PyrightError::Lsp(format!("read {}: {e}", f.display())))?;
        sources.push(src);
    }
    let source_refs: Vec<&str> = sources.iter().map(|s| s.as_str()).collect();

    // Query symbols: every top-level function/class.
    let mut queries = Vec::new();
    for sym in &program.symbols {
        if matches!(sym.kind, SymbolKind::Function | SymbolKind::Class) {
            let file = abs_of(&sym.file);
            // Pyright needs the position on the symbol's *name*, not the node start
            // (which is `def`/`class`). Find the name in the definition line.
            let name_col = find_name_column(&file, sym.span.start_line, &sym.name);
            queries.push(QuerySymbol {
                id: sym.id,
                file,
                position: Position {
                    line: sym.span.start_line.saturating_sub(1) as u32,
                    character: name_col.saturating_sub(1) as u32,
                },
            });
        }
    }

    // Spans for all non-module symbols (used to map references to their enclosing symbol).
    let spans: Vec<SymbolSpan> = program
        .symbols
        .iter()
        .filter(|s| s.kind != SymbolKind::Module)
        .map(|s| {
            SymbolSpan {
                id: s.id,
                file: abs_of(&s.file),
                start_line: s.span.start_line,
                end_line: s.span.end_line,
            }
        })
        .collect();

    let edges = resolve_references(executable, root, &abs_files, &source_refs, &queries, &spans, timeout_ms)?;
    Ok(edges.into_iter().map(|e| (e.from, e.to)).collect())
}

/// Find the (1-based) column of `name` on `line` (1-based) of `file`, for use as a
/// pyright query position. Falls back to column 1 if not found.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_name_column_locates_name_in_line() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("m.py");
        std::fs::write(&f, "class Greeter:\n    def greet(self):\n        pass\n").unwrap();
        // "class Greeter:" -> Greeter starts at col 7 (0-based 6).
        assert_eq!(find_name_column(&f, 1, "Greeter"), 7);
        assert_eq!(find_name_column(&f, 2, "greet"), 9);
        // Missing name -> fallback 1.
        assert_eq!(find_name_column(&f, 1, "nope"), 1);
    }

    #[test]
    fn uri_to_path_strips_file_prefix() {
        assert_eq!(
            uri_to_path("file:///Users/me/proj/pkg/core.py"),
            PathBuf::from("/Users/me/proj/pkg/core.py")
        );
        assert_eq!(uri_to_path("file:///x.py"), PathBuf::from("/x.py"));
    }

    #[test]
    fn enclosing_symbol_picks_smallest_containing_span() {
        let spans = vec![
            SymbolSpan { id: 1, file: PathBuf::from("a.py"), start_line: 1, end_line: 20 },
            SymbolSpan { id: 2, file: PathBuf::from("a.py"), start_line: 5, end_line: 10 },
            SymbolSpan { id: 3, file: PathBuf::from("b.py"), start_line: 1, end_line: 5 },
        ];
        assert_eq!(enclosing_symbol(&PathBuf::from("a.py"), 7, &spans), Some(2));
        assert_eq!(enclosing_symbol(&PathBuf::from("a.py"), 3, &spans), Some(1));
        assert_eq!(enclosing_symbol(&PathBuf::from("c.py"), 3, &spans), None);
        assert_eq!(enclosing_symbol(&PathBuf::from("a.py"), 30, &spans), None);
    }
}
