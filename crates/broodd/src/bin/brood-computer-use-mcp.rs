use std::io::{self, BufRead, Write};

use anyhow::{Context, Result};
use brood_cli::control::{request_dbus_command, resolve_control_target};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
struct JsonRpcRequest {
    #[serde(rename = "jsonrpc")]
    _jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

fn main() -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let target = resolve_control_target(None, None);
    for line in stdin.lock().lines() {
        let line = line.context("read MCP request")?;
        if line.trim().is_empty() {
            continue;
        }
        let request = match serde_json::from_str::<JsonRpcRequest>(&line) {
            Ok(request) => request,
            Err(error) => {
                write_response(
                    &mut stdout,
                    &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":error.to_string()}}),
                )?;
                continue;
            }
        };
        if request.id.is_none() {
            continue;
        }
        let id = request.id.clone().unwrap_or(Value::Null);
        let response = match request.method.as_str() {
            "initialize" => json!({
                "jsonrpc":"2.0",
                "id":id,
                "result":{
                    "protocolVersion":"2025-03-26",
                    "capabilities":{"tools":{}},
                    "serverInfo":{"name":"brood-computer-use","version":env!("CARGO_PKG_VERSION")}
                }
            }),
            "tools/list" => json!({
                "jsonrpc":"2.0",
                "id":id,
                "result":{"tools":tool_definitions()}
            }),
            "tools/call" => match call_tool(&target, &request.params) {
                Ok(text) => json!({
                    "jsonrpc":"2.0",
                    "id":id,
                    "result":{"content":[{"type":"text","text":text}],"isError":false}
                }),
                Err(error) => json!({
                    "jsonrpc":"2.0",
                    "id":id,
                    "result":{"content":[{"type":"text","text":format!("{error:#}")}],"isError":true}
                }),
            },
            _ => json!({
                "jsonrpc":"2.0",
                "id":id,
                "error":{"code":-32601,"message":"method not found"}
            }),
        };
        write_response(&mut stdout, &response)?;
    }
    Ok(())
}

fn write_response(output: &mut impl Write, response: &Value) -> Result<()> {
    serde_json::to_writer(&mut *output, response).context("encode MCP response")?;
    output.write_all(b"\n").context("write MCP delimiter")?;
    output.flush().context("flush MCP response")
}

fn call_tool(target: &brood_cli::control::ControlTarget, params: &Value) -> Result<String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("MCP tool name is required")?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let command = match name {
        "computer_start" => vec![
            "computer-use".to_string(),
            "start-request".to_string(),
            serde_json::to_string(&arguments)?,
        ],
        "computer_action" => vec![
            "computer-use".to_string(),
            "mcp-call".to_string(),
            serde_json::to_string(&arguments)?,
        ],
        "computer_observe" => vec![
            "computer-use".to_string(),
            "observe".to_string(),
            string_argument(&arguments, "task")?,
            integer_argument(&arguments, "event_cursor")?,
            integer_argument(&arguments, "observation_sequence")?,
        ],
        "computer_pause" | "computer_stop" => vec![
            "computer-use".to_string(),
            name.trim_start_matches("computer_").to_string(),
            string_argument(&arguments, "task")?,
        ],
        "computer_resume" => vec![
            "computer-use".to_string(),
            "resume".to_string(),
            string_argument(&arguments, "task")?,
            integer_argument(&arguments, "lease_duration_ms")?,
        ],
        "computer_approve" => vec![
            "computer-use".to_string(),
            "approve".to_string(),
            string_argument(&arguments, "request_id")?,
            string_argument(&arguments, "decision")?,
            string_argument(&arguments, "principal")?,
        ],
        _ => anyhow::bail!("unknown MCP computer-use tool {name}"),
    };
    request_dbus_command(target, &command, false)
}

fn string_argument(arguments: &Value, name: &str) -> Result<String> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .with_context(|| format!("MCP argument {name} must be a string"))
}

fn integer_argument(arguments: &Value, name: &str) -> Result<String> {
    arguments
        .get(name)
        .and_then(Value::as_u64)
        .map(|value| value.to_string())
        .with_context(|| format!("MCP argument {name} must be an unsigned integer"))
}

fn tool_definitions() -> Value {
    json!([
        {"name":"computer_start","description":"Start a scoped server-side computer-use task","inputSchema":{"type":"object"}},
        {"name":"computer_action","description":"Execute a validated ordered direct-action call","inputSchema":{"type":"object","required":["tool_call_id","arguments"]}},
        {"name":"computer_observe","description":"Read cursored task events and observations","inputSchema":{"type":"object","required":["task","event_cursor","observation_sequence"]}},
        {"name":"computer_pause","description":"Pause a computer-use task","inputSchema":{"type":"object","required":["task"]}},
        {"name":"computer_resume","description":"Resume a task with a new daemon-wide lease","inputSchema":{"type":"object","required":["task","lease_duration_ms"]}},
        {"name":"computer_stop","description":"Stop a computer-use task","inputSchema":{"type":"object","required":["task"]}},
        {"name":"computer_approve","description":"Decide a server-owned approval checkpoint","inputSchema":{"type":"object","required":["request_id","decision","principal"]}}
    ])
}
