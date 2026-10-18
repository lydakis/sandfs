//! An MCP server that hands an agent a sandfs workspace as file tools.
//!
//! Instead of a bash tool, the agent gets `read`, `list`, `glob`, `grep`, `write`,
//! `edit`, `move`, `delete` and `diff`, shaped like the file tools models already know.
//! Every call works on one copy-on-write [`Workspace`]: mounts stay read-only, writes
//! land in the overlay, and the harness collects the result with
//! [`Server::into_workspace`] and [`Workspace::diff`]. Forking, checkpoints and commits
//! stay with the harness, so there are no tools for them.
//!
//! The `sandfs-mcp` binary serves a workspace built from command-line mounts. For an
//! MCP client such as Claude Code:
//!
//! ```json
//! { "mcpServers": { "sandfs": {
//!     "command": "sandfs-mcp",
//!     "args": ["--git", "/repo=/path/to/clone@main", "--dir", "/data=./data"]
//! } } }
//! ```
//!
//! A harness that builds its own workspace embeds [`Server`] instead and calls
//! [`Server::serve`] on any reader and writer.
//!
//! The transport is MCP's stdio framing: one JSON-RPC 2.0 message per line. Only the
//! tools capability is offered, so the protocol surface is `initialize`, `ping`,
//! `tools/list` and `tools/call`.

use std::io::{self, BufRead, Write};

use sandfs_core::Workspace;
use serde_json::{Value, json};

mod tools;

pub use tools::MAX_OUTPUT;
use tools::Tools;

/// MCP protocol revisions this server speaks, newest first.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// The text a tool call returned, and whether it is an error the agent should see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutput {
    /// Plain text for the model, capped at [`MAX_OUTPUT`] bytes.
    pub text: String,
    /// True when the call failed; the text then says why.
    pub is_error: bool,
}

/// An MCP server bound to one workspace for its whole session.
pub struct Server {
    tools: Tools,
}

impl Server {
    /// Serve `workspace`. Fork it first if the caller wants to keep the original.
    pub fn new(workspace: Workspace) -> Self {
        Server { tools: Tools::new(workspace) }
    }

    /// The workspace as the agent has left it so far.
    pub fn workspace(&self) -> &Workspace {
        &self.tools.workspace
    }

    /// End the session and take the workspace back, for example to diff it.
    pub fn into_workspace(self) -> Workspace {
        self.tools.workspace
    }

    /// Run one tool directly, without JSON-RPC framing. Returns `None` for an unknown
    /// tool name. `arguments` is the JSON object an MCP client would send.
    pub fn call_tool(&mut self, name: &str, arguments: &Value) -> Option<ToolOutput> {
        let result = self.tools.call(name, arguments)?;
        Some(match result {
            Ok(text) => ToolOutput { text, is_error: false },
            Err(text) => ToolOutput { text, is_error: true },
        })
    }

    /// Answer one JSON-RPC message. Notifications and stray responses get `None`.
    pub fn handle(&mut self, message: &str) -> Option<String> {
        let request: Value = match serde_json::from_str(message) {
            Ok(request) => request,
            Err(err) => return Some(error_response(Value::Null, PARSE_ERROR, &format!("parse error: {err}"))),
        };
        if !request.is_object() {
            return Some(error_response(Value::Null, INVALID_REQUEST, "expected a JSON-RPC request object"));
        }
        let id = request.get("id").cloned();
        let Some(method) = request.get("method").and_then(Value::as_str) else {
            // This server sends no requests, so a response addressed to it is ignored.
            if request.get("result").is_some() || request.get("error").is_some() {
                return None;
            }
            return Some(error_response(id.unwrap_or(Value::Null), INVALID_REQUEST, "missing method"));
        };
        // A valid request without an id is a notification (`notifications/initialized`,
        // cancellations), which never gets an answer.
        let id = id?;
        let params = request.get("params").unwrap_or(&Value::Null);
        let reply = match self.dispatch(method, params) {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => return Some(error_response(id, code, &message)),
        };
        Some(reply.to_string())
    }

    /// Read requests line by line from `input` and write answers to `output` until EOF.
    pub fn serve(&mut self, input: impl BufRead, mut output: impl Write) -> io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(reply) = self.handle(&line) {
                writeln!(output, "{reply}")?;
                output.flush()?;
            }
        }
        Ok(())
    }

    fn dispatch(&mut self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        match method {
            "initialize" => {
                let requested = params.get("protocolVersion").and_then(Value::as_str);
                // Echo the client's revision when we speak it; otherwise offer our newest
                // and let the client decide whether to continue.
                let version = requested.filter(|v| PROTOCOL_VERSIONS.contains(v)).unwrap_or(PROTOCOL_VERSIONS[0]);
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "sandfs", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": self.tools.instructions(),
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools::definitions() })),
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or((INVALID_PARAMS, "tools/call needs a tool name".to_owned()))?;
                let arguments = params.get("arguments").unwrap_or(&Value::Null);
                let output =
                    self.call_tool(name, arguments).ok_or((INVALID_PARAMS, format!("unknown tool {name:?}")))?;
                Ok(json!({
                    "content": [{ "type": "text", "text": output.text }],
                    "isError": output.is_error,
                }))
            }
            other => Err((METHOD_NOT_FOUND, format!("method not found: {other}"))),
        }
    }
}

fn error_response(id: Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}
