use std::{
    collections::BTreeSet,
    env,
    io::{self, BufRead, Write},
    os::unix::net::UnixStream,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use brood_computer_use::{
    COMPUTER_USE_PROTOCOL_VERSION, ComputerUseLimits, ComputerUsePolicy, ComputerUseStartRequest,
};
use clap::Parser;
use serde::Deserialize;
use serde_json::{Value, json};

const SURFACE_SESSION_ENV: &str = "BROOD_SURFACE_SESSION_ID";
const NATIVE_TOOL_SOCKET_ENV: &str = "BROOD_NATIVE_TOOL_SOCKET";
const DEFAULT_LEASE_DURATION_MS: u64 = 300_000;
const DEFAULT_LAUNCH_TIMEOUT_MS: u64 = 10_000;
const MAX_LAUNCH_TIMEOUT_MS: u64 = 30_000;
const MAX_LAUNCH_ARGUMENTS: usize = 32;
const MAX_LAUNCH_ARGUMENT_BYTES: usize = 16 * 1024;

#[derive(Debug, Parser)]
#[command(about = "Session-scoped Brood computer-use MCP adapter")]
struct AdapterArgs {
    /// Restrict every task and action to one Brood surface session.
    #[arg(long)]
    session: Option<u64>,
    /// Stable principal recorded in the computer-use audit trail.
    #[arg(long)]
    principal: Option<String>,
}

#[derive(Deserialize)]
struct JsonRpcRequest {
    #[serde(rename = "jsonrpc")]
    _jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

struct Adapter {
    native_tool_socket: String,
    session: Option<u64>,
    principal: String,
    owned_tasks: BTreeSet<String>,
    next_task: u64,
}

struct ToolOutput {
    text: String,
    images: Vec<String>,
}

#[derive(Deserialize)]
struct NativeToolResponse {
    protocol_version: u32,
    ok: bool,
    output: Option<String>,
    error: Option<String>,
}

impl ToolOutput {
    fn from_response(response: String) -> Self {
        let Ok(mut value) = serde_json::from_str::<Value>(response.trim()) else {
            return Self {
                text: response,
                images: Vec::new(),
            };
        };
        let mut pngs = Vec::new();
        take_pngs(&mut value, &mut pngs);
        let text = serde_json::to_string(&value).unwrap_or(response);
        Self {
            text,
            images: pngs.into_iter().map(|png| BASE64.encode(png)).collect(),
        }
    }

    fn content(self) -> Vec<Value> {
        let mut content = vec![json!({"type":"text","text":self.text})];
        content.extend(
            self.images
                .into_iter()
                .map(|data| json!({"type":"image","data":data,"mimeType":"image/png"})),
        );
        content
    }
}

impl Adapter {
    fn new(args: AdapterArgs) -> Result<Self> {
        let session = args.session.or_else(|| {
            env::var(SURFACE_SESSION_ENV)
                .ok()
                .and_then(|value| value.parse().ok())
        });
        let principal = args
            .principal
            .or_else(|| env::var("BROOD_COMPUTER_USE_PRINCIPAL").ok())
            .unwrap_or_else(|| "local-mcp-agent".to_string());
        ensure!(!principal.trim().is_empty(), "principal must not be empty");
        Ok(Self {
            native_tool_socket: env::var(NATIVE_TOOL_SOCKET_ENV)
                .context("BROOD_NATIVE_TOOL_SOCKET is required for computer use")?,
            session,
            principal,
            owned_tasks: BTreeSet::new(),
            next_task: 1,
        })
    }

    fn instructions(&self) -> String {
        self.session.map_or_else(
            || {
                "Use computer_start before computer actions. Keep the returned lease generation and task sequence current, observe after actions, and always call computer_stop when finished. Every task must explicitly name its authorized Brood session.".to_string()
            },
            |session| {
                format!(
                    "These tools control only server-managed applications in Brood session {session}. Call computer_windows first and reuse a suitable existing window. If the requested application is not already open, call computer_launch before computer_start; it launches through Brood and returns only after a managed window exists. Call computer_start with no session ID, use the returned lease generation and monotonically increasing task sequence for actions or exec_js, observe results, and call computer_stop when finished. The adapter rejects cross-session access; the human presentation is an observer and primary human input pauses automation."
                )
            },
        )
    }

    fn call_tool(&mut self, params: &Value) -> Result<ToolOutput> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .context("MCP tool name is required")?;
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let mut stopped_task = None;
        let mut started_task = None;
        if name == "computer_launch" {
            return self.launch(arguments);
        }
        if name == "computer_windows" {
            let session = self
                .session
                .context("computer_windows requires a session-bound agent pane")?;
            let response = request_native_command(
                &self.native_tool_socket,
                &[
                    "computer-use".to_string(),
                    "windows".to_string(),
                    session.to_string(),
                ],
            )?;
            return Ok(ToolOutput::from_response(response));
        }
        let command = match name {
            "computer_start" => {
                let request = self.start_request(arguments)?;
                started_task = Some(request.task.0.clone());
                vec![
                    "computer-use".to_string(),
                    "start-request".to_string(),
                    serde_json::to_string(&request)?,
                ]
            }
            "computer_action" => {
                let arguments = self.scoped_action(arguments)?;
                vec![
                    "computer-use".to_string(),
                    "mcp-call".to_string(),
                    serde_json::to_string(&arguments)?,
                ]
            }
            "computer_exec_js" => {
                self.ensure_owned_argument(&arguments, "task")?;
                vec![
                    "computer-use".to_string(),
                    "exec-js".to_string(),
                    serde_json::to_string(&arguments)?,
                ]
            }
            "computer_observe" => {
                self.ensure_owned_argument(&arguments, "task")?;
                vec![
                    "computer-use".to_string(),
                    "observe".to_string(),
                    string_argument(&arguments, "task")?,
                    integer_argument(&arguments, "event_cursor")?,
                    integer_argument(&arguments, "observation_sequence")?,
                ]
            }
            "computer_pause" | "computer_stop" => {
                let task = string_argument(&arguments, "task")?;
                self.ensure_owned_task(&task)?;
                if name == "computer_stop" {
                    stopped_task = Some(task.clone());
                }
                vec![
                    "computer-use".to_string(),
                    name.trim_start_matches("computer_").to_string(),
                    task,
                ]
            }
            "computer_resume" => {
                self.ensure_owned_argument(&arguments, "task")?;
                vec![
                    "computer-use".to_string(),
                    "resume".to_string(),
                    string_argument(&arguments, "task")?,
                    integer_argument(&arguments, "lease_duration_ms")?,
                ]
            }
            "computer_renew" => {
                self.ensure_owned_argument(&arguments, "task")?;
                vec![
                    "computer-use".to_string(),
                    "renew".to_string(),
                    string_argument(&arguments, "task")?,
                    integer_argument(&arguments, "lease_generation")?,
                    integer_argument(&arguments, "lease_duration_ms")?,
                ]
            }
            "computer_approve" => {
                self.ensure_owned_argument(&arguments, "task")?;
                vec![
                    "computer-use".to_string(),
                    "approve-for-task".to_string(),
                    string_argument(&arguments, "task")?,
                    string_argument(&arguments, "request_id")?,
                    string_argument(&arguments, "decision")?,
                    self.principal.clone(),
                ]
            }
            _ => anyhow::bail!("unknown MCP computer-use tool {name}"),
        };
        let response = request_native_command(&self.native_tool_socket, &command)?;
        if let Some(task) = started_task {
            self.owned_tasks.insert(task);
        }
        if let Some(task) = stopped_task {
            self.owned_tasks.remove(&task);
        }
        Ok(ToolOutput::from_response(response))
    }

    fn launch(&self, arguments: Value) -> Result<ToolOutput> {
        let session = self
            .session
            .context("computer_launch requires a session-bound agent pane")?;
        let command = string_argument(&arguments, "command")?;
        ensure!(
            !command.trim().is_empty() && command.len() <= 256,
            "launch command must contain 1 to 256 bytes"
        );
        let args = arguments
            .get("args")
            .map(|value| {
                value
                    .as_array()
                    .context("MCP argument args must be an array")?
                    .iter()
                    .map(|value| {
                        value
                            .as_str()
                            .map(ToString::to_string)
                            .context("each launch argument must be a string")
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        ensure!(
            args.len() <= MAX_LAUNCH_ARGUMENTS,
            "launch accepts at most {MAX_LAUNCH_ARGUMENTS} arguments"
        );
        ensure!(
            command.len() + args.iter().map(String::len).sum::<usize>()
                <= MAX_LAUNCH_ARGUMENT_BYTES,
            "launch command and arguments exceed {MAX_LAUNCH_ARGUMENT_BYTES} bytes"
        );
        let timeout_ms = arguments
            .get("window_timeout_ms")
            .map(|value| {
                value
                    .as_u64()
                    .context("MCP argument window_timeout_ms must be an unsigned integer")
            })
            .transpose()?
            .unwrap_or(DEFAULT_LAUNCH_TIMEOUT_MS);
        ensure!(
            (1..=MAX_LAUNCH_TIMEOUT_MS).contains(&timeout_ms),
            "window timeout must be between 1 and {MAX_LAUNCH_TIMEOUT_MS} milliseconds"
        );

        let mut spawn = vec![
            "spawn-in-session".to_string(),
            session.to_string(),
            command.clone(),
        ];
        spawn.extend(args.iter().cloned());
        let response = request_native_command(&self.native_tool_socket, &spawn)?;
        let pane = response
            .split_whitespace()
            .find_map(|field| field.strip_prefix("pane_id="))
            .and_then(|value| value.parse::<u64>().ok())
            .context("Brood launch response did not contain a pane ID")?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let status = request_native_command(
                &self.native_tool_socket,
                &["hosted-child-status".to_string(), pane.to_string()],
            )?;
            let last_status: Value =
                serde_json::from_str(status.trim()).context("decode hosted application status")?;
            if last_status.get("visible").and_then(Value::as_bool) == Some(true) {
                return Ok(ToolOutput::from_response(
                    json!({
                        "session": session,
                        "pane": pane,
                        "command": command,
                        "args": args,
                        "state": last_status.get("state").cloned().unwrap_or(Value::Null),
                        "visible": true,
                    })
                    .to_string(),
                ));
            }
            if last_status.get("state").and_then(Value::as_str) == Some("exited") {
                let detail = last_status
                    .get("detail")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown exit status");
                anyhow::bail!(
                    "{command} exited before creating a managed window in Brood session {session}: {detail}"
                );
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "timed out after {timeout_ms} ms waiting for {command} to create a managed window in Brood session {session}; last status: {last_status}"
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn start_request(&mut self, arguments: Value) -> Result<ComputerUseStartRequest> {
        let Some(session) = self.session else {
            return serde_json::from_value(arguments)
                .context("decode unscoped computer-use start request");
        };
        let object = arguments
            .as_object()
            .context("computer_start arguments must be an object")?;
        if let Some(version) = object.get("protocol_version").and_then(Value::as_u64) {
            ensure!(
                version == u64::from(COMPUTER_USE_PROTOCOL_VERSION),
                "unsupported computer-use protocol version"
            );
        }
        if let Some(requested) = object.get("session").and_then(Value::as_u64) {
            ensure!(
                requested == session,
                "requested session is outside this agent pane"
            );
        }
        let scope = object.get("scope").and_then(Value::as_object);
        if let Some(requested) = scope
            .and_then(|scope| scope.get("session"))
            .and_then(Value::as_u64)
        {
            ensure!(
                requested == session,
                "requested scope is outside this agent pane"
            );
        }
        let windows = object
            .get("windows")
            .cloned()
            .or_else(|| scope.and_then(|scope| scope.get("windows").cloned()))
            .unwrap_or_else(|| json!([]));
        let task = object
            .get("task")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| self.generated_task_id());
        let request = json!({
            "protocol_version": COMPUTER_USE_PROTOCOL_VERSION,
            "task": task,
            "principal": self.principal,
            "scope": {"session": session, "windows": windows},
            "policy": object.get("policy").cloned().unwrap_or_else(|| json!(ComputerUsePolicy::default())),
            "limits": object.get("limits").cloned().unwrap_or_else(|| json!(ComputerUseLimits::default())),
            "lease_duration_ms": object.get("lease_duration_ms").cloned().unwrap_or_else(|| json!(DEFAULT_LEASE_DURATION_MS)),
            "active_window": object.get("active_window").cloned().unwrap_or(Value::Null),
        });
        serde_json::from_value(request).context("decode session-bound computer-use start request")
    }

    fn generated_task_id(&mut self) -> String {
        let principal = self
            .principal
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                    character
                } else {
                    '-'
                }
            })
            .take(48)
            .collect::<String>();
        let task = format!(
            "{principal}-computer-{}-{}",
            std::process::id(),
            self.next_task
        );
        self.next_task = self.next_task.saturating_add(1);
        task
    }

    fn scoped_action(&self, arguments: Value) -> Result<Value> {
        let mut envelope = arguments
            .as_object()
            .cloned()
            .context("computer_action arguments must be an object")?;
        let call = envelope
            .get_mut("arguments")
            .and_then(Value::as_object_mut)
            .context("computer_action.arguments must be an object")?;
        let task = call
            .get("task")
            .and_then(Value::as_str)
            .context("computer_action.arguments.task must be a string")?;
        self.ensure_owned_task(task)?;
        if let Some(session) = self.session {
            if let Some(requested) = call.get("target_session").and_then(Value::as_u64) {
                ensure!(
                    requested == session,
                    "action target is outside this agent pane"
                );
            }
            call.insert("target_session".to_string(), Value::from(session));
        }
        Ok(Value::Object(envelope))
    }

    fn ensure_owned_argument(&self, arguments: &Value, name: &str) -> Result<()> {
        self.ensure_owned_task(&string_argument(arguments, name)?)
    }

    fn ensure_owned_task(&self, task: &str) -> Result<()> {
        if self.session.is_none() {
            return Ok(());
        }
        ensure!(
            self.owned_tasks.contains(task),
            "task is not owned by this agent-pane adapter"
        );
        Ok(())
    }

    fn stop_owned_tasks(&mut self) {
        let tasks = std::mem::take(&mut self.owned_tasks);
        for task in tasks {
            let _result = request_native_command(
                &self.native_tool_socket,
                &["computer-use".to_string(), "stop".to_string(), task],
            );
        }
    }
}

fn request_native_command(socket_path: &str, command: &[String]) -> Result<String> {
    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("connect native computer tool socket {socket_path}"))?;
    serde_json::to_writer(&mut stream, &json!({"command": command}))
        .context("encode native computer tool request")?;
    stream
        .write_all(b"\n")
        .context("write native computer tool request delimiter")?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .context("finish native computer tool request")?;
    let response: NativeToolResponse = serde_json::from_reader(io::BufReader::new(stream))
        .context("decode native computer tool response")?;
    ensure!(
        response.protocol_version == COMPUTER_USE_PROTOCOL_VERSION,
        "native computer tool protocol version mismatch"
    );
    ensure!(
        response.ok,
        "{}",
        response
            .error
            .unwrap_or_else(|| "native computer tool failed".to_string())
    );
    Ok(response.output.unwrap_or_default())
}

fn main() -> Result<()> {
    let mut adapter = Adapter::new(AdapterArgs::parse())?;
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
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
        let Some(id) = request.id.clone() else {
            continue;
        };
        let response = match request.method.as_str() {
            "initialize" => json!({
                "jsonrpc":"2.0",
                "id":id,
                "result":{
                    "protocolVersion":"2025-03-26",
                    "capabilities":{"tools":{}},
                    "serverInfo":{"name":"brood-computer-use","version":env!("CARGO_PKG_VERSION")},
                    "instructions":adapter.instructions()
                }
            }),
            "tools/list" => json!({
                "jsonrpc":"2.0",
                "id":id,
                "result":{"tools":tool_definitions(adapter.session)}
            }),
            "tools/call" => match adapter.call_tool(&request.params) {
                Ok(output) => json!({
                    "jsonrpc":"2.0",
                    "id":id,
                    "result":{"content":output.content(),"isError":false}
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
    adapter.stop_owned_tasks();
    Ok(())
}

fn write_response(output: &mut impl Write, response: &Value) -> Result<()> {
    serde_json::to_writer(&mut *output, response).context("encode MCP response")?;
    output.write_all(b"\n").context("write MCP delimiter")?;
    output.flush().context("flush MCP response")
}

fn take_pngs(value: &mut Value, pngs: &mut Vec<Vec<u8>>) {
    match value {
        Value::Object(object) => {
            if let Some(value) = object.remove("png")
                && let Some(png) = json_byte_array(&value)
            {
                pngs.push(png);
            }
            if let Some(Value::Array(images)) = object.remove("displayed_pngs") {
                pngs.extend(images.iter().filter_map(json_byte_array));
            }
            for child in object.values_mut() {
                take_pngs(child, pngs);
            }
        }
        Value::Array(values) => {
            for child in values {
                take_pngs(child, pngs);
            }
        }
        _ => {}
    }
}

fn json_byte_array(value: &Value) -> Option<Vec<u8>> {
    let bytes = value
        .as_array()?
        .iter()
        .map(|byte| u8::try_from(byte.as_u64()?).ok())
        .collect::<Option<Vec<_>>>()?;
    bytes.starts_with(b"\x89PNG\r\n\x1a\n").then_some(bytes)
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

fn tool_definitions(session: Option<u64>) -> Value {
    let start_description = session.map_or_else(
        || "Start a scoped server-side computer-use task".to_string(),
        |session| {
            format!("Start a computer-use task bound to this agent pane's Brood session {session}")
        },
    );
    let action_schema = action_schema();
    let Value::Array(mut tools) = json!([
        {"name":"computer_start","description":start_description,"inputSchema":{"type":"object","properties":{"task":{"type":"string","description":"Optional unique audit task ID; one is generated when omitted"},"windows":{"type":"array","items":{"type":"integer"},"description":"Optional managed-pane allowlist within this session"},"active_window":{"type":["integer","null"]},"lease_duration_ms":{"type":"integer","minimum":1},"policy":{"type":"object"},"limits":{"type":"object"}}}},
        {"name":"computer_action","description":"Execute validated ordered direct actions and return a causally newer screenshot observation. Inspect that screenshot as authoritative evidence and continue correcting the UI until the requested outcome is visibly present","inputSchema":{"type":"object","required":["tool_call_id","arguments"],"properties":{"tool_call_id":{"type":"string"},"arguments":{"type":"object","required":["protocol_version","task","lease_generation","expected_sequence","actions"],"properties":{"protocol_version":{"const":1},"task":{"type":"string"},"lease_generation":{"type":"integer"},"expected_sequence":{"type":"integer"},"target_window":{"type":["integer","null"]},"actions":{"type":"array","minItems":1,"items":action_schema},"approval_proof":{"type":["string","null"]}}}}}},
        {"name":"computer_exec_js","description":"Evaluate persistent capability-empty JavaScript; computer helpers use the same validated server action path","inputSchema":{"type":"object","required":["protocol_version","task","lease_generation","expected_sequence","source"],"properties":{"protocol_version":{"const":1},"task":{"type":"string"},"lease_generation":{"type":"integer"},"expected_sequence":{"type":"integer"},"source":{"type":"string"},"approval_proof":{"type":["string","null"]}}}},
        {"name":"computer_observe","description":"Read events, pending approvals, and observations newer than the supplied cursors","inputSchema":{"type":"object","required":["task","event_cursor","observation_sequence"],"properties":{"task":{"type":"string"},"event_cursor":{"type":"integer"},"observation_sequence":{"type":"integer"}}}},
        {"name":"computer_pause","description":"Pause a task and release held input","inputSchema":task_schema()},
        {"name":"computer_resume","description":"Resume a task with a new daemon-wide lease","inputSchema":{"type":"object","required":["task","lease_duration_ms"],"properties":{"task":{"type":"string"},"lease_duration_ms":{"type":"integer","minimum":1}}}},
        {"name":"computer_renew","description":"Renew the current lease before it expires","inputSchema":{"type":"object","required":["task","lease_generation","lease_duration_ms"],"properties":{"task":{"type":"string"},"lease_generation":{"type":"integer"},"lease_duration_ms":{"type":"integer","minimum":1}}}},
        {"name":"computer_stop","description":"Stop a task, release input, and destroy its script isolate","inputSchema":task_schema()},
        {"name":"computer_approve","description":"Approve or deny a pending checkpoint owned by this task","inputSchema":{"type":"object","required":["task","request_id","decision"],"properties":{"task":{"type":"string"},"request_id":{"type":"string"},"decision":{"enum":["approve","deny"]}}}}
    ]) else {
        unreachable!("tool definitions are a JSON array")
    };
    if let Some(session) = session {
        tools.insert(0, json!({
            "name":"computer_launch",
            "description":format!("Launch a GUI application inside Brood session {session} and wait for its managed window before starting computer use. Call computer_windows first and launch only when no suitable window already exists"),
            "inputSchema":{
                "type":"object",
                "required":["command"],
                "properties":{
                    "command":{"type":"string","minLength":1,"maxLength":256,"description":"Executable name or path; no shell interpretation is performed"},
                    "args":{"type":"array","maxItems":MAX_LAUNCH_ARGUMENTS,"items":{"type":"string"}},
                    "window_timeout_ms":{"type":"integer","minimum":1,"maximum":MAX_LAUNCH_TIMEOUT_MS}
                }
            }
        }));
        tools.insert(0, json!({
            "name":"computer_windows",
            "description":format!("List existing GUI windows in Brood session {session}. Check these windows first and reuse a suitable one before launching another application"),
            "inputSchema":{"type":"object","properties":{}}
        }));
    }
    Value::Array(tools)
}

fn task_schema() -> Value {
    json!({"type":"object","required":["task"],"properties":{"task":{"type":"string"}}})
}

fn action_schema() -> Value {
    let coordinate = json!({"type":"number","minimum":0});
    let button = json!({"enum":["left","middle","right"]});
    let point = json!({
        "type":"object",
        "required":["x","y"],
        "properties":{"x":coordinate,"y":coordinate}
    });
    json!({"oneOf":[
        {"type":"object","required":["type","x","y","button"],"properties":{"type":{"const":"click"},"x":coordinate,"y":coordinate,"button":button}},
        {"type":"object","required":["type","x","y","button"],"properties":{"type":{"const":"double_click"},"x":coordinate,"y":coordinate,"button":button}},
        {"type":"object","required":["type","x","y"],"properties":{"type":{"const":"move"},"x":coordinate,"y":coordinate}},
        {"type":"object","required":["type","path","button"],"properties":{"type":{"const":"drag"},"path":{"type":"array","minItems":2,"items":point},"button":button}},
        {"type":"object","required":["type","x","y","delta_x","delta_y"],"properties":{"type":{"const":"scroll"},"x":coordinate,"y":coordinate,"delta_x":{"type":"number"},"delta_y":{"type":"number"}}},
        {"type":"object","required":["type","text"],"properties":{"type":{"const":"type"},"text":{"type":"string"}}},
        {"type":"object","required":["type","keys"],"properties":{"type":{"const":"keypress"},"keys":{"type":"array","minItems":1,"items":{"type":"string"}}}},
        {"type":"object","required":["type","duration_ms"],"properties":{"type":{"const":"wait"},"duration_ms":{"type":"integer","minimum":0,"maximum":30000}}},
        {"type":"object","required":["type"],"properties":{"type":{"const":"screenshot"}}}
    ]})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observations_become_mcp_images_without_text_byte_arrays() {
        let output = ToolOutput::from_response(
            json!({
                "sequence": 4,
                "observation": {
                    "sequence": 9,
                    "png": [137, 80, 78, 71, 13, 10, 26, 10, 1, 2, 3]
                }
            })
            .to_string(),
        );
        assert_eq!(output.images.len(), 1);
        assert_eq!(
            BASE64.decode(&output.images[0]).unwrap(),
            b"\x89PNG\r\n\x1a\n\x01\x02\x03"
        );
        assert!(!output.text.contains("137,80,78,71"));
        assert!(output.text.contains("\"sequence\":9"));
    }

    #[test]
    fn displayed_script_images_are_all_extracted() {
        let png = vec![137, 80, 78, 71, 13, 10, 26, 10, 7];
        let output = ToolOutput::from_response(
            json!({"value_json":"ok","displayed_pngs":[png, png]}).to_string(),
        );
        assert_eq!(output.images.len(), 2);
        assert!(!output.text.contains("displayed_pngs"));
    }

    #[test]
    fn launch_tool_is_available_only_to_session_bound_adapters() {
        let bound = tool_definitions(Some(7));
        assert!(bound.as_array().unwrap().iter().any(|tool| {
            tool.get("name").and_then(Value::as_str) == Some("computer_launch")
                && tool
                    .get("description")
                    .and_then(Value::as_str)
                    .is_some_and(|description| description.contains("session 7"))
        }));
        let unbound = tool_definitions(None);
        assert!(
            !unbound.as_array().unwrap().iter().any(|tool| {
                tool.get("name").and_then(Value::as_str) == Some("computer_launch")
            })
        );
    }

    #[test]
    fn window_discovery_is_session_bound_and_recommended_before_launch() {
        let bound = tool_definitions(Some(7));
        let windows = bound
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some("computer_windows"));
        assert!(windows.is_some_and(|tool| {
            tool.get("description")
                .and_then(Value::as_str)
                .is_some_and(|description| {
                    description.contains("session 7") && description.contains("first")
                })
        }));
        assert!(
            !tool_definitions(None)
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| {
                    tool.get("name").and_then(Value::as_str) == Some("computer_windows")
                })
        );
    }
}
