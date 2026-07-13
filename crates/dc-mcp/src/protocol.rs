//! The tiny slice of MCP we need, hand-rolled over JSON-RPC 2.0 (spec: MCP
//! stdio transport). We implement exactly three methods — `initialize`,
//! `tools/list`, `tools/call` — plus the `notifications/initialized` no-op.
//!
//! Rolling this by hand (rather than the async `rmcp` SDK) keeps the crate on the
//! workspace's deliberately lean, blocking-only dependency posture (no tokio):
//! the whole protocol surface Claude Code exercises for a local tool server is
//! small and stable.

use serde_json::{json, Value};

use crate::tools::{self, Tools};

/// The MCP protocol revision we advertise in `initialize`. Claude Code negotiates
/// down if it speaks an older one; this is the version these tool shapes target.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// A parsed JSON-RPC request line. `id` is absent for notifications (to which we
/// send no response).
pub struct Request {
    pub id: Option<Value>,
    pub method: String,
    pub params: Value,
}

impl Request {
    /// Parse one line of stdin into a request, or `None` if it isn't a usable
    /// JSON-RPC object (blank lines, garbage — the loop just skips those).
    pub fn parse(line: &str) -> Option<Request> {
        let v: Value = serde_json::from_str(line).ok()?;
        let method = v.get("method")?.as_str()?.to_string();
        Some(Request {
            id: v.get("id").cloned(),
            method,
            params: v.get("params").cloned().unwrap_or(Value::Null),
        })
    }
}

/// Build a JSON-RPC success response for `id` carrying `result`.
pub fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Build a JSON-RPC error response for `id`.
pub fn err(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Dispatch one request against the tool set, returning the response value to
/// write back — or `None` for a notification (no reply). This is the pure core of
/// the server: no I/O, so the full protocol is unit-testable.
pub fn dispatch(req: &Request, tools: &dyn Tools) -> Option<Value> {
    // Notifications carry no id and get no response.
    let Some(id) = req.id.clone() else {
        return None;
    };

    let result = match req.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "dumb-coder", "version": env!("CARGO_PKG_VERSION") },
        })),
        "tools/list" => Ok(json!({ "tools": tools::tool_manifest() })),
        "tools/call" => call_tool(req, tools),
        // "ping" is a harmless liveness check some clients send.
        "ping" => Ok(json!({})),
        other => Err((-32601, format!("method not found: {other}"))),
    };

    Some(match result {
        Ok(v) => ok(id, v),
        Err((code, msg)) => err(id, code, &msg),
    })
}

/// Handle a `tools/call`: pull the tool name + arguments and route to the store,
/// wrapping the outcome in MCP's `{content:[{type:"text",...}]}` result shape.
fn call_tool(req: &Request, tools: &dyn Tools) -> Result<Value, (i64, String)> {
    let name = req
        .params
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or((-32602, "tools/call missing 'name'".to_string()))?;
    let args = req.params.get("arguments").cloned().unwrap_or(json!({}));

    let outcome = tools.call(name, &args);
    match outcome {
        Ok(text) => Ok(json!({ "content": [ { "type": "text", "text": text } ] })),
        // A tool-level failure is reported as an MCP tool error (isError), not a
        // protocol error, so Claude sees the message and can react.
        Err(text) => Ok(json!({
            "content": [ { "type": "text", "text": text } ],
            "isError": true,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stub tool set that records the last call and returns a canned result.
    struct StubTools {
        reply: Result<String, String>,
        last: std::cell::RefCell<Option<(String, Value)>>,
    }
    impl Tools for StubTools {
        fn call(&self, name: &str, args: &Value) -> Result<String, String> {
            *self.last.borrow_mut() = Some((name.to_string(), args.clone()));
            self.reply.clone()
        }
    }
    fn stub(reply: Result<String, String>) -> StubTools {
        StubTools {
            reply,
            last: std::cell::RefCell::new(None),
        }
    }

    #[test]
    fn initialize_advertises_tools_capability() {
        let req = Request::parse(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .unwrap();
        let resp = dispatch(&req, &stub(Ok(String::new()))).unwrap();
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn tools_list_returns_the_manifest() {
        let req = Request::parse(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).unwrap();
        let resp = dispatch(&req, &stub(Ok(String::new()))).unwrap();
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"dumb_coder_code"));
        assert!(names.contains(&"dumb_coder_status"));
    }

    #[test]
    fn tools_call_routes_name_and_args_and_wraps_text() {
        let tools = stub(Ok("started j1".to_string()));
        let req = Request::parse(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"dumb_coder_code","arguments":{"task":"do it"}}}"#,
        )
        .unwrap();
        let resp = dispatch(&req, &tools).unwrap();
        assert_eq!(tools.last.borrow().as_ref().unwrap().0, "dumb_coder_code");
        assert_eq!(resp["result"]["content"][0]["text"], "started j1");
        assert!(resp["result"].get("isError").is_none());
    }

    #[test]
    fn tool_error_sets_is_error_flag() {
        let req = Request::parse(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"x","arguments":{}}}"#,
        )
        .unwrap();
        let resp = dispatch(&req, &stub(Err("boom".to_string()))).unwrap();
        assert_eq!(resp["result"]["isError"], true);
        assert_eq!(resp["result"]["content"][0]["text"], "boom");
    }

    #[test]
    fn notification_gets_no_response() {
        let req =
            Request::parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert!(dispatch(&req, &stub(Ok(String::new()))).is_none());
    }

    #[test]
    fn unknown_method_is_a_protocol_error() {
        let req = Request::parse(r#"{"jsonrpc":"2.0","id":9,"method":"frobnicate"}"#).unwrap();
        let resp = dispatch(&req, &stub(Ok(String::new()))).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn blank_and_garbage_lines_dont_parse() {
        assert!(Request::parse("").is_none());
        assert!(Request::parse("   ").is_none());
        assert!(Request::parse("not json").is_none());
    }
}
