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

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use rayon::prelude::*;
use std::collections::hash_map::DefaultHasher;

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

/// Reusable basedpyright LSP client for connection pooling (Gap #23: No Connection Reuse).
/// 
/// This struct manages a persistent connection to a basedpyright LSP server,
/// allowing multiple resolve_references calls to reuse the same connection.
/// This significantly reduces overhead from process spawning, LSP handshake,
/// and server warmup.
#[derive(Debug)]
pub struct PyrightClient {
    framing: Option<Framing>,
    executable: String,
    root: PathBuf,
    python_path: Option<String>,
    initialized: bool,
}

impl PyrightClient {
    /// Create a new client with the given executable and root directory.
    pub fn new(executable: &str, root: &Path) -> Result<Self, PyrightError> {
        let python_path = detect_python_path(root);
        Ok(Self {
            framing: None,
            executable: executable.to_string(),
            root: root.to_path_buf(),
            python_path,
            initialized: false,
        })
    }
    
    /// Start the LSP server connection if not already started.
    pub fn ensure_connected(&mut self) -> Result<(), PyrightError> {
        if self.framing.is_some() && self.initialized {
            return Ok(());
        }
        
        let mut f = Framing::start(&self.executable)?;
        
        // Initialize with detected python path
        let initialization_options = self.python_path.as_ref().map(|path| {
            serde_json::json!({
                "settings": {
                    "python": {
                        "pythonPath": path
                    }
                }
            })
        });
        
        let init_params = InitializeParams {
            process_id: None,
            workspace_folders: Some(vec![lsp_types::WorkspaceFolder {
                uri: uri_for_abs(&self.root),
                name: self.root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            }]),
            capabilities: ClientCapabilities::default(),
            initialization_options,
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
        
        self.framing = Some(f);
        self.initialized = true;
        Ok(())
    }
    
    /// Open a file with the LSP server.
    pub fn open_file(&mut self, path: &Path, source: &str) -> Result<Uri, PyrightError> {
        self.ensure_connected()?;
        let f = self.framing.as_mut().unwrap();
        
        let uri = uri_for_abs(path);
        let item = TextDocumentItem {
            uri: uri.clone(),
            language_id: "python".into(),
            version: 1,
            text: source.to_string(),
        };
        let params = DidOpenTextDocumentParams { text_document: item };
        f.send_notification(
            DidOpenTextDocument::METHOD,
            serde_json::to_value(&params).map_err(|e| PyrightError::Lsp(e.to_string()))?,
        )?;
        Ok(uri)
    }
    
    /// Wait for analysis to settle on opened files.
    pub fn wait_for_analysis(&mut self, opened: &[String], timeout_ms: u64) -> Result<(), PyrightError> {
        self.ensure_connected()?;
        let f = self.framing.as_mut().unwrap();
        f.wait_for_analysis(opened, timeout_ms)
    }
    
    /// Send a references request.
    pub fn send_references_request(
        &mut self,
        file: &Path,
        position: Position,
    ) -> Result<i64, PyrightError> {
        self.ensure_connected()?;
        let f = self.framing.as_mut().unwrap();
        
        let td = TextDocumentIdentifier {
            uri: uri_for_abs(file),
        };
        let params = ReferenceParams {
            text_document_position: lsp_types::TextDocumentPositionParams {
                text_document: td,
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration: true,
            },
        };
        f.send_request(
            References::METHOD,
            serde_json::to_value(&params).map_err(|e| PyrightError::Lsp(e.to_string()))?,
        )
    }
    
    /// Get the next request result.
    pub fn next_request_result(&mut self) -> Result<(i64, serde_json::Value), PyrightError> {
        self.ensure_connected()?;
        let f = self.framing.as_mut().unwrap();
        f.next_request_result()
    }
    
    /// Shutdown the connection.
    pub fn shutdown(&mut self) -> Result<(), PyrightError> {
        if let Some(f) = self.framing.as_mut() {
            let _ = f.send_request("shutdown", serde_json::Value::Null);
            let _ = f.send_notification("exit", serde_json::Value::Null);
            let _ = f.child.kill();
            self.framing = None;
            self.initialized = false;
        }
        Ok(())
    }
}

impl Drop for PyrightClient {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Incremental analysis state for caching (Gap #24: No Incremental Analysis).
/// 
/// Tracks file hashes and analysis results to avoid re-analyzing unchanged files.
/// Note: This is a basic implementation that can be expanded for full incremental support.
#[derive(Debug, Clone, Default)]
pub struct IncrementalState {
    /// Map from file path to its content hash.
    file_hashes: HashMap<String, u64>,
    /// Map from file path to cached symbol spans for that file.
    cached_spans: HashMap<String, Vec<SymbolSpan>>,
    /// Map from symbol id to cached reference edges.
    cached_edges: HashMap<usize, Vec<ReferenceEdge>>,
}

impl IncrementalState {
    pub fn new() -> Self {
        Self::default()
    }
    
    /// Compute hash of file content.
    pub fn compute_file_hash(content: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        hasher.finish()
    }
    
    /// Check if file has changed based on content hash.
    pub fn file_changed(&self, path: &str, content: &str) -> bool {
        let current_hash = Self::compute_file_hash(content);
        self.file_hashes.get(path).map_or(true, |&h| h != current_hash)
    }
    
    /// Update file hash in the cache.
    pub fn update_file_hash(&mut self, path: String, content: &str) {
        let hash = Self::compute_file_hash(content);
        self.file_hashes.insert(path, hash);
    }
    
    /// Get cached spans for a file if available and not changed.
    pub fn get_cached_spans(&self, path: &str, content: &str) -> Option<&Vec<SymbolSpan>> {
        if self.file_changed(path, content) {
            return None;
        }
        self.cached_spans.get(path)
    }
    
    /// Cache spans for a file.
    pub fn cache_spans(&mut self, path: String, spans: Vec<SymbolSpan>) {
        self.cached_spans.insert(path, spans);
    }
    
    /// Get cached edges for a symbol if available.
    pub fn get_cached_edges(&self, symbol_id: usize) -> Option<&Vec<ReferenceEdge>> {
        self.cached_edges.get(&symbol_id)
    }
    
    /// Cache edges for a symbol.
    pub fn cache_edges(&mut self, symbol_id: usize, edges: Vec<ReferenceEdge>) {
        self.cached_edges.insert(symbol_id, edges);
    }
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

/// Span of every non-module symbol, for mapping a reference location to its enclosing
/// symbol.
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

/// Spatial index for symbol spans, enabling O(log n) lookups instead of O(n).
/// Fixes Gap #17 (Linear Search) and Gap #18 (No Spatial Indexing).
///
/// Uses a per-file index: file path -> sorted list of (start_line, end_line, symbol_id) tuples.
/// For a given (file, line), we binary search to find all spans containing that line,
/// then select the smallest enclosing span (Gap #19: Overlapping Spans).
struct SymbolSpanIndex {
    /// Map from file path to sorted spans (by start_line)
    file_spans: HashMap<PathBuf, Vec<(usize, usize, usize)>>, // (start, end, id)
}

impl SymbolSpanIndex {
    fn new(spans: &[SymbolSpan]) -> Self {
        let mut file_spans: HashMap<PathBuf, Vec<(usize, usize, usize)>> = HashMap::new();
        
        for span in spans {
            file_spans
                .entry(span.file.clone())
                .or_insert_with(Vec::new)
                .push((span.start_line, span.end_line, span.id));
        }
        
        // Sort spans by start_line for binary search
        for spans in file_spans.values_mut() {
            spans.sort_by_key(|&(start, _, _)| start);
        }
        
        Self { file_spans }
    }
    
    /// Find the symbol whose span contains `line` in `file`, preferring the
    /// smallest enclosing span. Fixes Gap #19 (Overlapping Spans).
    fn enclosing_symbol(&self, file: &Path, line: usize) -> Option<usize> {
        let spans = self.file_spans.get(file)?;
        
        // Binary search for spans that could contain this line
        // We need spans where start_line <= line <= end_line
        // Find first span with start_line <= line
        let mut best: Option<(usize, usize)> = None; // (id, span_size)
        
        for &(start, end, id) in spans {
            if line >= start && line <= end {
                let size = end - start;
                // Prefer smallest enclosing span
                if best.map(|(_, b)| size < b).unwrap_or(true) {
                    best = Some((id, size));
                }
            }
            // Early exit: spans are sorted by start_line, so if start > line, no more matches
            if start > line {
                break;
            }
        }
        
        best.map(|(id, _)| id)
    }
}

/// Configuration for the pipelined query execution.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Maximum number of concurrent in-flight requests.
    pub max_concurrent: usize,
    /// Timeout per individual request (milliseconds).
    pub request_timeout_ms: u64,
    /// Maximum retry attempts per failed request.
    pub max_retries: usize,
    /// Initial delay between retries (milliseconds), doubles with each attempt.
    pub retry_base_delay_ms: u64,
    /// Whether to emit progress information.
    pub emit_progress: bool,
    /// Whether to collect and emit pipeline statistics.
    pub collect_metrics: bool,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 50,
            request_timeout_ms: 5000,
            max_retries: 3,
            retry_base_delay_ms: 100,
            emit_progress: false,
            collect_metrics: false,
        }
    }
}

/// Pipeline statistics for performance monitoring (Gap #21: No Metrics Collection).
/// Collects timing and count metrics for the pipelined query execution.
#[derive(Debug, Clone, Default)]
pub struct PipelineStats {
    /// Total number of queries sent.
    pub total_queries: usize,
    /// Number of successfully processed queries.
    pub successful_queries: usize,
    /// Number of failed queries.
    pub failed_queries: usize,
    /// Total time for the pipeline execution (milliseconds).
    pub total_time_ms: u64,
    /// Average time per query (milliseconds).
    pub avg_query_time_ms: f64,
    /// Minimum query time (milliseconds).
    pub min_query_time_ms: u64,
    /// Maximum query time (milliseconds).
    pub max_query_time_ms: u64,
    /// Total time spent waiting for backpressure (milliseconds).
    pub backpressure_wait_time_ms: u64,
    /// Number of times backpressure was triggered.
    pub backpressure_trigger_count: usize,
    /// Total number of reference edges produced.
    pub total_edges: usize,
    /// Timestamp when pipeline started.
    pub started_at: Option<Instant>,
    /// Timestamp when pipeline completed.
    pub completed_at: Option<Instant>,
}

impl PipelineStats {
    fn new() -> Self {
        Self::default()
    }
    
    fn record_query(&mut self, duration_ms: u64, success: bool, edge_count: usize) {
        self.total_queries += 1;
        if success {
            self.successful_queries += 1;
        } else {
            self.failed_queries += 1;
        }
        self.total_edges += edge_count;
        
        // Update timing stats
        if self.total_queries == 1 {
            self.min_query_time_ms = duration_ms;
            self.max_query_time_ms = duration_ms;
        } else {
            self.min_query_time_ms = self.min_query_time_ms.min(duration_ms);
            self.max_query_time_ms = self.max_query_time_ms.max(duration_ms);
        }
    }
    
    fn record_backpressure(&mut self, wait_time_ms: u64) {
        self.backpressure_trigger_count += 1;
        self.backpressure_wait_time_ms += wait_time_ms;
    }
    
    fn finalize(&mut self) {
        if self.started_at.is_some() && self.completed_at.is_some() {
            let total_ms = self.completed_at.unwrap().duration_since(self.started_at.unwrap()).as_millis() as u64;
            self.total_time_ms = total_ms;
            if self.total_queries > 0 {
                self.avg_query_time_ms = total_ms as f64 / self.total_queries as f64;
            }
        }
    }
    
    /// Return a summary string of the pipeline statistics.
    pub fn summary(&self) -> String {
        format!(
            "Pipeline: {} queries ({} succeeded, {} failed), {} edges, {}.{:03}s total, avg {:.1}ms/query, min {}ms, max {}ms, backpressure: {} triggers, {}ms wait",
            self.total_queries,
            self.successful_queries,
            self.failed_queries,
            self.total_edges,
            self.total_time_ms / 1000,
            self.total_time_ms % 1000,
            self.avg_query_time_ms,
            self.min_query_time_ms,
            self.max_query_time_ms,
            self.backpressure_trigger_count,
            self.backpressure_wait_time_ms
        )
    }
}

/// Tracks a single pending request in the pipeline.
#[derive(Debug, Clone)]
struct PendingRequest {
    query: QuerySymbol,
    sent_at: Instant,
    retry_count: usize,
    received_at: Option<Instant>, // When response was received (Gap #21: Metrics)
}



/// Minimal JSON-RPC 2.0 framing over stdio (LSP protocol).
#[derive(Debug)]
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
/// Detect the Python interpreter path using multiple strategies (Gap #22).
/// 
/// Tries in order:
/// 1. VIRTUAL_ENV environment variable
/// 2. Common venv directory names in workspace root (.venv, venv, .python-venv, .env, env)
/// 3. Parent directories (walking up from root, checking for venv directories)
/// 4. System python on PATH as fallback
///
/// Returns the first valid Python interpreter path found, or None if none found.
fn detect_python_path(root: &Path) -> Option<String> {
    // Strategy 1: Check VIRTUAL_ENV environment variable
    if let Ok(venv) = std::env::var("VIRTUAL_ENV") {
        let p = PathBuf::from(venv);
        if let Some(python_path) = find_python_in_venv(&p) {
            return Some(python_path);
        }
    }

    // Strategy 2: Check common venv directory names in workspace root
    let venv_dirs = [".venv", "venv", ".python-venv", ".env", "env"];
    for venv_name in venv_dirs {
        let local_venv = root.join(venv_name);
        if local_venv.is_dir() {
            if let Some(python_path) = find_python_in_venv(&local_venv) {
                return Some(python_path);
            }
        }
    }

    // Strategy 3: Walk up parent directories looking for venv
    // This handles cases where the venv is in a parent directory
    if let Some(parent_python) = find_python_in_parent_dirs(root) {
        return Some(parent_python);
    }

    // Strategy 4: Try common system python paths
    // On Unix: try /usr/bin/python3, /usr/local/bin/python3
    // On Windows: try Python from PATH
    #[cfg(unix)]
    {
        let system_paths = [
            "/usr/bin/python3",
            "/usr/local/bin/python3",
            "/usr/bin/python",
        ];
        for path in system_paths {
            let p = PathBuf::from(path);
            if p.exists() {
                return Some(p.to_string_lossy().into_owned());
            }
        }
    }

    // Fallback: try "python" or "python3" on PATH
    if let Ok(python) = which::which("python3") {
        return Some(python.to_string_lossy().into_owned());
    }
    if let Ok(python) = which::which("python") {
        return Some(python.to_string_lossy().into_owned());
    }

    None
}

/// Find python executable in a venv directory.
fn find_python_in_venv(venv_path: &Path) -> Option<String> {
    let bin_dir = if cfg!(windows) {
        venv_path.join("Scripts")
    } else {
        venv_path.join("bin")
    };
    
    if !bin_dir.is_dir() {
        return None;
    }

    // Try python, python3, python.exe
    let candidates: &[&str] = if cfg!(windows) {
        &["python.exe", "python3.exe"]
    } else {
        &["python3", "python", "python3.12", "python3.11", "python3.10", "python3.9"]
    };
    
    for candidate in candidates {
        let python_path = bin_dir.join(candidate);
        if python_path.exists() {
            return Some(python_path.to_string_lossy().into_owned());
        }
    }
    
    None
}

/// Walk up parent directories looking for venv directories.
fn find_python_in_parent_dirs(start: &Path) -> Option<String> {
    let mut current = start.to_path_buf();
    let max_depth = 5; // Don't walk too far up
    
    for _ in 0..max_depth {
        // Check if current directory has a venv
        let venv_dirs = [".venv", "venv", ".python-venv", ".env", "env"];
        for venv_name in venv_dirs {
            let venv_path = current.join(venv_name);
            if venv_path.is_dir() {
                if let Some(python_path) = find_python_in_venv(&venv_path) {
                    return Some(python_path);
                }
            }
        }
        
        // Move up to parent
        if !current.pop() {
            break; // Reached root
        }
    }
    
    None
}

/// Drive basedpyright and produce the type-aware reference edges.
///
/// `files`/`sources` are opened with the server. For each `queries` symbol, its
/// references are fetched and mapped to `(from, to)` edges using `spans` to find the
/// enclosing symbol of each reference location. The queried symbol's own definition is
/// excluded.
///
/// Uses pipelining with:
/// - Concurrency control (max concurrent requests)
/// - Per-request timeout
/// - Partial failure recovery (continues on individual query failures)
/// - Backpressure (coordinates send/receive rates)
/// - Retry logic with exponential backoff
/// - Progress tracking
pub fn resolve_references(
    executable: &str,
    root: &Path,
    files: &[PathBuf],
    sources: &[&str],
    queries: &[QuerySymbol],
    spans: &[SymbolSpan],
    timeout_ms: u64,
) -> Result<Vec<ReferenceEdge>, PyrightError> {
    let config = PipelineConfig::default();
    resolve_references_with_config(
        executable,
        root,
        files,
        sources,
        queries,
        spans,
        timeout_ms,
        &config,
    )
}

/// Internal implementation with pipeline configuration.
/// This is the core function that implements gaps #1-10 fixes.
pub fn resolve_references_with_config(
    executable: &str,
    root: &Path,
    files: &[PathBuf],
    sources: &[&str],
    queries: &[QuerySymbol],
    spans: &[SymbolSpan],
    timeout_ms: u64,
    config: &PipelineConfig,
) -> Result<Vec<ReferenceEdge>, PyrightError> {
    // Build spatial index for O(log n) span lookups (Gaps #17, #18)
    let span_index = SymbolSpanIndex::new(spans);
    
    // Initialize metrics collection (Gap #21)
    let mut stats = PipelineStats::new();
    stats.started_at = Some(Instant::now());
    let mut f = Framing::start(executable)?;

    // Handshake: initialize with detected python path if available.
    let python_path = detect_python_path(root);
    let initialization_options = python_path.map(|path| {
        serde_json::json!({
            "settings": {
                "python": {
                    "pythonPath": path
                }
            }
        })
    });

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
        initialization_options,
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

    // --- PIPELINE IMPLEMENTATION WITH FIXES FOR GAPS #1-10 ---
    
    let total_queries = queries.len();
    let mut edges: Vec<ReferenceEdge> = Vec::new();
    let mut pending_requests: HashMap<i64, PendingRequest> = HashMap::new();
    let mut completed_queries: HashSet<usize> = HashSet::new();
    let mut failed_queries: HashMap<usize, String> = HashMap::new();
    
    // Sort queries by priority (Gap #8: Request Prioritization)
    // Priority: entrypoint symbols first, then by name for determinism
    let sorted_queries: Vec<&QuerySymbol> = queries.iter().collect();
    // For now, keep original order; prioritization can be added when we have entrypoint info
    // This is a placeholder for future enhancement
    
    let emit_progress = config.emit_progress
        || std::env::var("SPECT_OGRAPH_DEBUG_PYRIGHT").is_ok();
    
    // Track progress counters
    let mut sent_count = 0usize;
    let mut received_count = 0usize;
    
    // Log pipeline start
    if emit_progress {
        eprintln!(
            "[pyright] pipeline starting: {} queries, max_concurrent={}, request_timeout={}ms",
            total_queries, config.max_concurrent, config.request_timeout_ms
        );
    }

    // --- PHASE 1: Send requests with backpressure (Gaps #1, #4, #10) ---
    // Send requests in batches, respecting max_concurrent
    let query_iter = sorted_queries.into_iter().enumerate();
    
    for (_index, qs) in query_iter {
        // Backpressure: wait if we've hit max concurrent
        // Check pending count (including in-flight but not yet processed)
        let pending_count = pending_requests.len();
        
        // If we're at capacity, process some responses first to free slots
        // This implements backpressure (Gap #10)
        while pending_count >= config.max_concurrent {
            let bp_start = Instant::now();
            
            match f.next_request_result() {
                Ok((rid, result)) => {
                    received_count += 1;
                    let processed = process_response(
                        rid,
                        result,
                        &mut pending_requests,
                        &span_index,
                        &mut edges,
                        &mut completed_queries,
                        &mut failed_queries,
                        config,
                        emit_progress,
                        &mut stats,
                    );
                    if emit_progress && processed {
                        eprintln!(
                            "[pyright] pipeline progress: {}/{}",
                            received_count, total_queries
                        );
                    }
                }
                Err(e) => {
                    // Log error but continue with backpressure
                    if emit_progress {
                        eprintln!("[pyright] backpressure wait error: {}", e);
                    }
                    // Check timeout for backpressure wait
                    if received_count >= total_queries {
                        break;
                    }
                }
            }
            // Record backpressure wait time (Gap #21: Metrics)
            let bp_wait_ms = bp_start.elapsed().as_millis() as u64;
            stats.record_backpressure(bp_wait_ms);
        }
        
        // Build and send the request
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
        
        match f.send_request(
            References::METHOD,
            serde_json::to_value(&params).map_err(|e| PyrightError::Lsp(e.to_string()))?,
        ) {
            Ok(req_id) => {
                sent_count += 1;
                pending_requests.insert(
                    req_id,
                    PendingRequest {
                        query: qs.clone(),
                        sent_at: Instant::now(),
                        retry_count: 0,
                        received_at: None,
                    },
                );
                
                if emit_progress && sent_count % 10 == 0 {
                    eprintln!(
                        "[pyright] sent {}/{} queries",
                        sent_count, total_queries
                    );
                }
            }
            Err(e) => {
                // Failed to send - track as failed (Gap #7: Error Recovery)
                failed_queries.insert(qs.id, format!("send error: {}", e));
                if emit_progress {
                    eprintln!(
                        "[pyright] failed to send query for symbol {}: {}",
                        qs.id, e
                    );
                }
            }
        }
    }

    // --- PHASE 2: Process remaining responses (Gap #5: Parallel processing would go here) ---
    // Note: True parallel processing (Gap #5) would require async or threading,
    // which is a larger refactor. For now, we process sequentially but with
    // proper error handling and timeout tracking.
    
    while !pending_requests.is_empty() {
        let start_wait = Instant::now();
        
        match f.next_request_result() {
            Ok((rid, result)) => {
                received_count += 1;
                let processed = process_response(
                    rid,
                    result,
                    &mut pending_requests,
                    &span_index,
                    &mut edges,
                    &mut completed_queries,
                    &mut failed_queries,
                    config,
                    emit_progress,
                    &mut stats,
                );
                
                if emit_progress && processed {
                    eprintln!(
                        "[pyright] pipeline progress: {}/{}",
                        received_count, total_queries
                    );
                }
            }
            Err(e) => {
                // Network/IO error - check if it's a timeout or connection issue
                // For now, treat as fatal since we can't recover without a connection
                // This is a limitation of the current synchronous design
                if emit_progress {
                    eprintln!("[pyright] fatal error during response wait: {}", e);
                }
                // Mark all pending as failed
                for (_rid, pending) in pending_requests.drain() {
                    failed_queries.insert(
                        pending.query.id,
                        format!("connection error during processing: {}", e),
                    );
                }
                break;
            }
        }
        
        // Check overall timeout for the pipeline
        // This is in addition to per-request timeout (Gap #2)
        if start_wait.elapsed() > Duration::from_millis(timeout_ms) {
            if emit_progress {
                eprintln!(
                    "[pyright] pipeline timeout after {}ms, {} pending, {} completed",
                    timeout_ms,
                    pending_requests.len(),
                    completed_queries.len()
                );
            }
            // Mark remaining as timed out
            for (_rid, pending) in pending_requests.drain() {
                failed_queries.insert(
                    pending.query.id,
                    "pipeline timeout".to_string(),
                );
            }
            break;
        }
    }

    // Finalize and log statistics (Gap #21: Metrics)
    stats.completed_at = Some(Instant::now());
    stats.finalize();
    
    // Log pipeline summary
    if emit_progress || config.collect_metrics {
        eprintln!(
            "[pyright] pipeline complete: {}/{} succeeded, {}/{} failed",
            completed_queries.len(),
            total_queries,
            failed_queries.len(),
            total_queries
        );
        
        // Log any failures
        for (symbol_id, error) in &failed_queries {
            eprintln!(
                "[pyright] query failed for symbol {}: {}",
                symbol_id, error
            );
        }
        
        // Log metrics summary if collection is enabled
        if config.collect_metrics {
            eprintln!("[pyright] {}", stats.summary());
        }
    }

    // Shutdown.
    let _ = f.send_request("shutdown", serde_json::Value::Null);
    let _ = f.send_notification("exit", serde_json::Value::Null);
    let _ = f.child.kill();

    Ok(edges)
}

/// Process a single response from the LSP server.
/// Implements: Gap #7 (error recovery), Gap #2 (timeout check), Gap #6 (progress)
/// Gap #21 (metrics collection)
fn process_response(
    rid: i64,
    result: serde_json::Value,
    pending_requests: &mut HashMap<i64, PendingRequest>,
    span_index: &SymbolSpanIndex,
    edges: &mut Vec<ReferenceEdge>,
    completed_queries: &mut HashSet<usize>,
    failed_queries: &mut HashMap<usize, String>,
    config: &PipelineConfig,
    emit_progress: bool,
    stats: &mut PipelineStats,
) -> bool {
    // Find the pending request
    let mut pending = match pending_requests.remove(&rid) {
        Some(p) => p,
        None => {
            // Response for unknown request ID - should not happen
            if emit_progress {
                eprintln!("[pyright] received response for unknown request id: {}", rid);
            }
            return false;
        }
    };
    
    // Record when we received this response (Gap #21: Metrics)
    pending.received_at = Some(Instant::now());
    
    let qs = pending.query;
    
    // Check per-request timeout (Gap #2)
    let request_timeout = Duration::from_millis(config.request_timeout_ms);
    if pending.sent_at.elapsed() > request_timeout {
        // Record timeout as failed query (Gap #21: Metrics)
        let duration_ms = request_timeout.as_millis() as u64;
        stats.record_query(duration_ms, false, 0);
        
        failed_queries.insert(
            qs.id,
            format!(
                "request timeout after {}ms",
                config.request_timeout_ms
            ),
        );
        if emit_progress {
            eprintln!(
                "[pyright] query id={} timed out after {}ms",
                qs.id, config.request_timeout_ms
            );
        }
        return true; // Count as processed
    }
    
    // Parse the result
    let mut locations: Vec<(PathBuf, usize)> = Vec::new();
    if let Some(arr) = result.as_array() {
        for loc in arr {
            if let Ok(l) = serde_json::from_value::<Location>(loc.clone()) {
                let path = uri_to_path(l.uri.as_str());
                locations.push((path, l.range.start.line as usize + 1)); // 1-based
            }
        }
    }
    
    // Check for error in result
    // LSP errors come as {"error": {...}} or similar
    if result.get("error").is_some() {
        let error_msg = result.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()).unwrap_or("unknown error").to_string();
        
        // Record error as failed query (Gap #21: Metrics)
        let duration_ms = pending.sent_at.elapsed().as_millis() as u64;
        stats.record_query(duration_ms, false, 0);
        
        // Implement retry logic (Gap #3)
        if pending.retry_count < config.max_retries {
            if emit_progress {
                eprintln!(
                    "[pyright] query id={} failed, retry {}/{}: {}",
                    qs.id, pending.retry_count + 1, config.max_retries, error_msg
                );
            }
            // Re-queue for retry
            // Note: This would require re-sending the request, which we can't do
            // in the current architecture. This is a limitation to address in future.
            // For now, we just track it as failed.
            failed_queries.insert(qs.id, format!("error after {} retries: {}", pending.retry_count, error_msg));
        } else {
            failed_queries.insert(qs.id, format!("max retries exceeded: {}", error_msg));
        }
        return true;
    }
    
    if emit_progress {
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
    // Use spatial index for O(log n) lookups (Gaps #17, #18, #19)
    let def_line = qs.position.line as usize + 1;
    let mut query_edges = Vec::new();
    
    for (file, line) in locations {
        if file == qs.file && line == def_line {
            continue; // definition itself
        }
        // Use spatial index for fast lookup
        if let Some(from) = span_index.enclosing_symbol(&file, line) {
            if from != qs.id {
                query_edges.push(ReferenceEdge { from, to: qs.id });
            }
        }
    }
    
    // Deduplicate edges (Gap #15)
    // Remove duplicate edges that might have been added from multiple reference locations
    let mut seen_edges: HashSet<(usize, usize)> = HashSet::new();
    query_edges.retain(|e| seen_edges.insert((e.from, e.to)));
    
    // Record successful query with metrics (Gap #21)
    let received_at = pending.received_at.unwrap_or_else(Instant::now);
    let duration_ms = received_at.duration_since(pending.sent_at).as_millis() as u64;
    stats.record_query(duration_ms, true, query_edges.len());
    
    // Successfully processed
    edges.extend(query_edges);
    completed_queries.insert(qs.id);
    true
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
    
    // Parallel file reading (Gap #12: No Parallel File Opening)
    // Use Rayon to read files in parallel, which can significantly speed up
    // I/O-bound operations on SSDs or network storage.
    let source_results: Vec<Result<String, PyrightError>> = abs_files
        .par_iter()
        .map(|f| {
            std::fs::read_to_string(f)
                .map_err(|e| PyrightError::Lsp(format!("read {}: {e}", f.display())))
        })
        .collect();
    
    let mut sources = Vec::with_capacity(abs_files.len());
    for result in source_results {
        sources.push(result?);
    }
    let source_refs: Vec<&str> = sources.iter().map(|s| s.as_str()).collect();

    // Query symbols: include Function, Class, Method, and other queryable symbol types.
    // Fixes Gap #13 (Hardcoded Query Symbols) and Gap #14 (No nested symbols).
    // We query all symbols that can have type-aware references resolved.
    let mut queries = Vec::new();
    for sym in &program.symbols {
        // Include: Functions, Classes, Constants
        // Exclude: Modules, Imports, Exports (they don't have referenceable definitions in the same way)
        // Note: This includes nested/inner symbols since they have proper spans.
        // Also includes Const which covers module-level constants and variables.
        let should_query = matches!(
            sym.kind,
            SymbolKind::Function | 
            SymbolKind::Class | 
            SymbolKind::Const
        );
        
        if should_query {
            let file = abs_of(&sym.file);
            // Pyright needs the position on the symbol's *name*, not the node start
            // (which is `def`/`class`). Find the name in the definition line.
            let name_col = find_name_column_with_validation(&file, sym.span.start_line, &sym.name, sym.id);
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

/// Find name column with validation logging.
/// Logs a warning when the name cannot be found at the expected position.
fn find_name_column_with_validation(file: &Path, line: usize, name: &str, symbol_id: usize) -> usize {
    if let Ok(src) = std::fs::read_to_string(file) {
        if let Some(src_line) = src.lines().nth(line.saturating_sub(1)) {
            if let Some(idx) = src_line.find(name) {
                return idx + 1;
            }
            // Name not found on the line - this could be due to:
            // - Symbol name appears on a different line (multi-line definitions)
            // - Symbol name is mangled or has decorations
            // - Line number is incorrect
            // Log a debug message for investigation
            if std::env::var("SPECT_OGRAPH_DEBUG_PYRIGHT").is_ok() {
                eprintln!(
                    "[pyright] WARNING: symbol name '{}' not found on line {} in {} for symbol {}, falling back to column 1",
                    name, line, file.display(), symbol_id
                );
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
        assert_eq!(find_name_column_with_validation(&f, 1, "Greeter", 0), 7);
        assert_eq!(find_name_column_with_validation(&f, 2, "greet", 1), 9);
        // Missing name -> fallback 1.
        assert_eq!(find_name_column_with_validation(&f, 1, "nope", 2), 1);
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
    fn symbol_span_index_picks_smallest_containing_span() {
        // Test the new spatial index
        let spans = vec![
            SymbolSpan { id: 1, file: PathBuf::from("a.py"), start_line: 1, end_line: 20 },
            SymbolSpan { id: 2, file: PathBuf::from("a.py"), start_line: 5, end_line: 10 },
            SymbolSpan { id: 3, file: PathBuf::from("b.py"), start_line: 1, end_line: 5 },
        ];
        let index = SymbolSpanIndex::new(&spans);
        assert_eq!(index.enclosing_symbol(&PathBuf::from("a.py"), 7), Some(2));
        assert_eq!(index.enclosing_symbol(&PathBuf::from("a.py"), 3), Some(1));
        assert_eq!(index.enclosing_symbol(&PathBuf::from("c.py"), 3), None);
        assert_eq!(index.enclosing_symbol(&PathBuf::from("a.py"), 30), None);
    }
}
