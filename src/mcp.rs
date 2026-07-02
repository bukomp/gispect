//! Minimal MCP (Model Context Protocol) server over stdio.
//!
//! Speaks newline-delimited JSON-RPC 2.0 on stdin/stdout with no async
//! runtime: a plain blocking loop reading one request per line and writing
//! one response per line.

use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

use crate::git::GitRepo;
use crate::types::{DiffMode, FileEntry, FileStatus};

/// Run the MCP server, blocking on stdin until EOF.
pub fn run(repo: GitRepo) -> anyhow::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(err) => {
                eprintln!("gispect mcp: failed to parse request: {err}");
                continue;
            }
        };

        let Some(response) = handle_request(&repo, &request) else {
            continue;
        };

        writeln!(out, "{response}")?;
        out.flush()?;
    }

    Ok(())
}

/// Handle a single parsed JSON-RPC request, returning the response line to
/// write (already serialized), or `None` for notifications (no `id`).
fn handle_request(repo: &GitRepo, request: &Value) -> Option<String> {
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    // A request without an "id" member is a notification: handle silently
    // (or ignore) and never respond.
    let id = id?;

    let result = match method {
        "initialize" => Ok(handle_initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(handle_tools_list()),
        "tools/call" => handle_tools_call(repo, &params),
        _ => Err((-32601, format!("method not found: {method}"))),
    };

    let response = match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    };

    Some(response.to_string())
}

fn handle_initialize(params: &Value) -> Value {
    let protocol_version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("2024-11-05");

    json!({
        "protocolVersion": protocol_version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "gispect", "version": env!("CARGO_PKG_VERSION")},
    })
}

/// Shared `mode`/`base` input schema fragment used by all tools.
fn mode_base_properties() -> Value {
    json!({
        "mode": {
            "type": "string",
            "enum": ["branch", "working", "staged", "unstaged"],
            "default": "working",
            "description": "Which diff to inspect: branch vs base, working tree vs HEAD, staged (index vs HEAD), or unstaged (working tree vs index).",
        },
        "base": {
            "type": "string",
            "description": "Base branch to compare against when mode is \"branch\" (defaults to the repo's default base).",
        },
    })
}

fn handle_tools_list() -> Value {
    let list_changed_files_props = mode_base_properties();
    let diff_summary_props = mode_base_properties();
    let mut get_file_diff_props = mode_base_properties();
    get_file_diff_props
        .as_object_mut()
        .expect("object")
        .insert(
            "path".to_string(),
            json!({"type": "string", "description": "Path (relative to the repo root) of the file to diff."}),
        );

    json!({
        "tools": [
            {
                "name": "list_changed_files",
                "description": "List files changed in the selected diff, with status and add/delete line counts.",
                "inputSchema": {
                    "type": "object",
                    "properties": list_changed_files_props,
                },
            },
            {
                "name": "get_file_diff",
                "description": "Get the unified diff of a single file in the selected diff.",
                "inputSchema": {
                    "type": "object",
                    "properties": get_file_diff_props,
                    "required": ["path"],
                },
            },
            {
                "name": "diff_summary",
                "description": "Summarize the selected diff: file count, total additions/deletions, and per-status counts.",
                "inputSchema": {
                    "type": "object",
                    "properties": diff_summary_props,
                },
            },
        ]
    })
}

/// Parse `mode`/`base` arguments into a [`DiffMode`].
fn parse_mode(repo: &GitRepo, arguments: &Value) -> Result<DiffMode, String> {
    let mode = arguments
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("working");

    match mode {
        "branch" => {
            let base = arguments
                .get("base")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| repo.default_base());
            Ok(DiffMode::BranchToBase { base })
        }
        "working" => Ok(DiffMode::WorkingTree),
        "staged" => Ok(DiffMode::Staged),
        "unstaged" => Ok(DiffMode::Unstaged),
        other => Err(format!(
            "invalid mode {other:?}, expected one of: branch, working, staged, unstaged"
        )),
    }
}

fn file_status_marker(status: &FileStatus) -> String {
    status.marker().to_string()
}

fn file_entry_json(entry: &FileEntry) -> Value {
    let mut value = json!({
        "path": entry.path,
        "status": file_status_marker(&entry.status),
        "additions": entry.additions,
        "deletions": entry.deletions,
        "binary": entry.binary,
    });
    if let FileStatus::Renamed { from } = &entry.status {
        value
            .as_object_mut()
            .expect("object")
            .insert("renamed_from".to_string(), json!(from));
    }
    value
}

/// Text-content tool result, e.g. `{"content":[{"type":"text","text":...}],"isError":false}`.
fn tool_text_result(text: String, is_error: bool) -> Value {
    json!({
        "content": [{"type": "text", "text": text}],
        "isError": is_error,
    })
}

fn handle_tools_call(repo: &GitRepo, params: &Value) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| (-32602, "missing required param: name".to_string()))?;
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);

    match name {
        "list_changed_files" => Ok(run_list_changed_files(repo, &arguments)),
        "get_file_diff" => Ok(run_get_file_diff(repo, &arguments)),
        "diff_summary" => Ok(run_diff_summary(repo, &arguments)),
        other => Err((-32602, format!("unknown tool: {other}"))),
    }
}

fn run_list_changed_files(repo: &GitRepo, arguments: &Value) -> Value {
    let mode = match parse_mode(repo, arguments) {
        Ok(mode) => mode,
        Err(err) => return tool_text_result(err, true),
    };

    match repo.changed_files(&mode) {
        Ok(entries) => {
            let files: Vec<Value> = entries.iter().map(file_entry_json).collect();
            let text = serde_json::to_string_pretty(&json!({"files": files}))
                .unwrap_or_else(|e| format!("failed to serialize result: {e}"));
            tool_text_result(text, false)
        }
        Err(err) => tool_text_result(err.to_string(), true),
    }
}

fn run_get_file_diff(repo: &GitRepo, arguments: &Value) -> Value {
    let mode = match parse_mode(repo, arguments) {
        Ok(mode) => mode,
        Err(err) => return tool_text_result(err, true),
    };

    let path = match arguments.get("path").and_then(Value::as_str) {
        Some(path) => path,
        None => return tool_text_result("missing required param: path".to_string(), true),
    };

    match repo.unified_diff(&mode, Some(path)) {
        Ok(diff) => tool_text_result(diff, false),
        Err(err) => tool_text_result(err.to_string(), true),
    }
}

fn run_diff_summary(repo: &GitRepo, arguments: &Value) -> Value {
    let mode = match parse_mode(repo, arguments) {
        Ok(mode) => mode,
        Err(err) => return tool_text_result(err, true),
    };

    match repo.changed_files(&mode) {
        Ok(entries) => {
            let mut additions = 0usize;
            let mut deletions = 0usize;
            let mut by_status: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();

            for entry in &entries {
                additions += entry.additions;
                deletions += entry.deletions;
                *by_status.entry(file_status_marker(&entry.status)).or_insert(0) += 1;
            }

            let summary = json!({
                "file_count": entries.len(),
                "additions": additions,
                "deletions": deletions,
                "by_status": by_status,
            });
            let text = serde_json::to_string_pretty(&summary)
                .unwrap_or_else(|e| format!("failed to serialize result: {e}"));
            tool_text_result(text, false)
        }
        Err(err) => tool_text_result(err.to_string(), true),
    }
}
