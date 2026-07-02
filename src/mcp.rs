//! T8 — hand-rolled MCP stdio server: JSON-RPC 2.0, line-delimited over stdin/stdout. No SDK crate
//! (K2) — the surface maple needs is 4 methods and 3 thin tool wrappers, cheaper to hand-roll than
//! to pull in a dependency (bob's `mcp.rs` uses the `rmcp` SDK for a much larger tool surface; not
//! warranted here). One `Store` stays open for the process lifetime; each `tools/call` refreshes
//! first (A1) so a long-lived session self-heals — no daemon semantics beyond the conversation (K1).

use crate::store::Store;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::Path;

const PROTOCOL_VERSION: &str = "2024-11-05";

/// Run the server until stdin hits EOF (client disconnect -> graceful exit).
pub fn serve(repo: &Path) -> anyhow::Result<()> {
    let mut store = Store::open(repo)?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                let resp = json!({"jsonrpc":"2.0","id":Value::Null,"error":{"code":-32700,"message":format!("parse error: {e}")}});
                writeln!(stdout, "{resp}")?;
                stdout.flush()?;
                continue;
            }
        };
        let has_id = req.get("id").is_some();
        let resp = handle_request(&mut store, req);
        if has_id {
            writeln!(stdout, "{resp}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// Dispatch one JSON-RPC request/notification. Always returns a `Value`; `serve()` only writes it
/// when the input carried an `id` (JSON-RPC 2.0: notifications get no response). Factored out of
/// `serve()` so the dispatch logic is unit-testable without a real stdio pipe.
pub fn handle_request(store: &mut Store, req: Value) -> Value {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    match method {
        "initialize" => json!({
            "jsonrpc": "2.0", "id": id,
            "result": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "maple", "version": env!("CARGO_PKG_VERSION")}
            }
        }),
        "notifications/initialized" => Value::Null, // no id, no reply (ignored)
        "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tool_defs()}}),
        "tools/call" => tools_call(store, id, req.get("params").cloned().unwrap_or(Value::Null)),
        _ => json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": format!("method not found: {method}")}
        }),
    }
}

fn tool_defs() -> Value {
    json!([
        {
            "name": "enumerate",
            "description": "Count callers/defs for a symbol: N callers across M files, per-resolution counts.",
            "inputSchema": {"type": "object", "properties": {"symbol": {"type": "string"}}, "required": ["symbol"]}
        },
        {
            "name": "closure",
            "description": "Depth-1 closure of a symbol: target def(s) + direct callers + direct callees.",
            "inputSchema": {
                "type": "object",
                "properties": {"symbol": {"type": "string"}, "depth": {"type": "integer"}},
                "required": ["symbol"]
            }
        },
        {
            "name": "bundle",
            "description": "Context bundle for a symbol: target body, callee signatures, caller call-site snippets, token/omission report.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": {"type": "string"},
                    "budget": {"type": "integer"},
                    "max_callers": {"type": "integer"},
                    "format": {"type": "string", "enum": ["json", "prompt"]}
                },
                "required": ["symbol"]
            }
        }
    ])
}

fn tools_call(store: &mut Store, id: Value, params: Value) -> Value {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    // A1: refresh-first on every call (cheap, ms-scale) so long-lived MCP sessions stay fresh.
    if let Err(e) = store.refresh() {
        return tool_error(id, format!("refresh failed: {e}"));
    }
    match call_tool(store, &name, &args) {
        Ok(v) => {
            let text = match &v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": text}]}})
        }
        Err(e) => tool_error(id, e.to_string()), // symbol-not-found etc. -> tool error, not a crash
    }
}

fn call_tool(store: &mut Store, name: &str, args: &Value) -> anyhow::Result<Value> {
    let symbol = args
        .get("symbol")
        .and_then(|s| s.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required argument: symbol"))?;
    match name {
        "enumerate" => Ok(serde_json::to_value(store.enumerate(symbol)?)?),
        "closure" => {
            let depth = args.get("depth").and_then(|d| d.as_i64()).unwrap_or(1);
            Ok(serde_json::to_value(store.closure(symbol, depth)?)?)
        }
        "bundle" => {
            let budget = args.get("budget").and_then(|b| b.as_i64()).unwrap_or(16000);
            let max_callers = args.get("max_callers").and_then(|m| m.as_u64()).unwrap_or(20) as usize;
            let format = args.get("format").and_then(|f| f.as_str()).unwrap_or("json");
            let bundle = store.bundle(symbol, budget, max_callers, 1, 3, &crate::store::ApproxTokenizer)?;
            match format {
                "json" => Ok(serde_json::to_value(bundle)?),
                "prompt" => Ok(Value::String(crate::render_prompt(&bundle))),
                other => anyhow::bail!("unknown format {other:?} (expected \"json\" or \"prompt\")"),
            }
        }
        other => anyhow::bail!("unknown tool: {other}"),
    }
}

fn tool_error(id: Value, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": message}], "isError": true}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("lib.py"), "def target():\n    return 1\n").unwrap();
        fs::write(root.join("app.py"), "def caller():\n    return target()\n").unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();
        (tmp, s)
    }

    #[test]
    fn initialize_replies_with_protocol_and_capabilities() {
        let (_tmp, mut s) = fixture();
        let resp = handle_request(&mut s, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(resp["result"]["serverInfo"]["name"], "maple");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn notifications_initialized_is_ignored() {
        let (_tmp, mut s) = fixture();
        let resp = handle_request(&mut s, json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        assert!(resp.is_null());
    }

    #[test]
    fn tools_list_has_three_tools() {
        let (_tmp, mut s) = fixture();
        let resp = handle_request(&mut s, json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for want in ["enumerate", "closure", "bundle"] {
            assert!(names.contains(&want), "missing tool {want}");
        }
    }

    #[test]
    fn unknown_method_is_json_rpc_error() {
        let (_tmp, mut s) = fixture();
        let resp = handle_request(&mut s, json!({"jsonrpc":"2.0","id":3,"method":"nope"}));
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn tools_call_enumerate_returns_parseable_result() {
        let (_tmp, mut s) = fixture();
        let resp = handle_request(
            &mut s,
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"enumerate","arguments":{"symbol":"target"}}}),
        );
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["symbol"], "target");
        assert_eq!(parsed["caller_count"], 1);
        assert!(resp["result"]["isError"].is_null());
    }

    /// Symbol-not-found is a tool error, not a crash (spec T8 requirement).
    #[test]
    fn tools_call_symbol_not_found_is_tool_error() {
        let (_tmp, mut s) = fixture();
        let resp = handle_request(
            &mut s,
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"bundle","arguments":{"symbol":"does_not_exist"}}}),
        );
        assert_eq!(resp["result"]["isError"], true);
    }
}
