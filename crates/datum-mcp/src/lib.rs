//! datum-mcp: a Model Context Protocol server over the `datum` CLI.
//!
//! An agent (Claude Code, an Inspect agent, anything that speaks MCP) gets
//! datum's verbs as tools and keeps its own timeline in a datum database:
//! what it started and stopped, the states it switched, the moments it
//! noticed. Every write carries the server's actor (`DATUM_ACTOR`, set from
//! `--actor`), so a database shared by a person and the agents working with
//! them says which wrote each row (`extra_metadata.actor` on the entry,
//! `actor` in the op's origin).
//!
//! The server runs the real `datum` binary, one process a call, and never
//! opens the database itself: the CLI's lock, spool, migrations and exit
//! codes stay the one path every write goes through.
//!
//! Transport: JSON-RPC 2.0, one message a line on stdin/stdout (MCP's stdio
//! transport). Nothing but protocol goes to stdout; diagnostics go to
//! stderr.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

/// The protocol versions this server speaks, newest first. A client asking
/// for one of these gets it; any other gets the newest.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// Commands `datum_read` runs: the ones that only read. A command that can
/// write (`check --fix`, `edit`, `undo`, any log) is refused there; logging
/// goes through `datum_log`, which says what it writes.
pub const READ_COMMANDS: &[&str] = &[
    "tail", "head", "now", "get", "grep", "stats", "fields", "history", "why", "count", "ids",
];

/// Flags `datum_read` refuses whatever the command: each makes a read write.
const WRITING_FLAGS: &[&str] = &["--fix"];

/// The verbs `datum_log` takes.
pub const LOG_COMMANDS: &[&str] = &["occur", "start", "stop", "switch", "note"];

/// The `datum` exit code for a duplicate: the same line logged twice is one
/// entry, which is a success for a logger.
const EXIT_DUPLICATE: i32 = 11;

/// How the server runs.
#[derive(Debug, Clone)]
pub struct Config {
    /// The `datum` binary.
    pub bin: PathBuf,
    /// The database, as `DATUM_DB`; none leaves datum's own default.
    pub db: Option<String>,
    /// Who the writes are on behalf of, as `DATUM_ACTOR`.
    pub actor: String,
}

/// What one `datum` run gave back.
#[derive(Debug, Clone, PartialEq)]
pub struct RunOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `datum` with an argv. The real one spawns the binary; tests stand
/// in for it.
pub trait Runner {
    fn run(&self, config: &Config, argv: &[String]) -> RunOutput;
}

/// Spawns the configured `datum` binary.
pub struct ProcessRunner;

impl Runner for ProcessRunner {
    fn run(&self, config: &Config, argv: &[String]) -> RunOutput {
        let mut cmd = Command::new(&config.bin);
        cmd.args(argv)
            .env("DATUM_ACTOR", &config.actor)
            .env("NO_COLOR", "1")
            // A question nobody can answer fails at once (exit 12) instead of
            // waiting on a stdin that is the protocol's.
            .env("DATUM_NO_DEFAULT_ANSWER", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(db) = &config.db {
            cmd.env("DATUM_DB", db);
        }
        match cmd.output() {
            Ok(out) => RunOutput {
                code: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            },
            Err(e) => RunOutput {
                code: -1,
                stdout: String::new(),
                stderr: format!("could not run {}: {e}", config.bin.display()),
            },
        }
    }
}

/// The server's state across a session.
pub struct Server<R: Runner> {
    pub config: Config,
    pub runner: R,
}

impl<R: Runner> Server<R> {
    pub fn new(config: Config, runner: R) -> Self {
        Self { config, runner }
    }

    /// Serve until stdin ends.
    pub fn serve(&self, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(reply) = self.handle_line(&line) {
                writeln!(output, "{reply}")?;
                output.flush()?;
            }
        }
        Ok(())
    }

    /// One incoming line to its reply, if it takes one. A batch (an array)
    /// gets an array of the replies its requests take.
    pub fn handle_line(&self, line: &str) -> Option<Value> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(error_reply(
                    Value::Null,
                    -32700,
                    &format!("parse error: {e}"),
                ))
            }
        };
        match msg {
            Value::Array(batch) => {
                let replies: Vec<Value> =
                    batch.into_iter().filter_map(|m| self.handle(m)).collect();
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            m => self.handle(m),
        }
    }

    /// One message to its reply. A notification (no `id`) gets none, and
    /// neither does a response the client sends.
    pub fn handle(&self, msg: Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            return id.map(|id| error_reply(id, -32600, "invalid request: no method"));
        };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let id = id?;
        let result = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => self.call_tool(&params),
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => error_reply(id, code, &message),
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        let asked = params.get("protocolVersion").and_then(Value::as_str);
        let version = asked
            .filter(|v| PROTOCOL_VERSIONS.contains(v))
            .unwrap_or(PROTOCOL_VERSIONS[0]);
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "datum-mcp", "version": env!("CARGO_PKG_VERSION") },
            "instructions": instructions(&self.config.actor),
        })
    }

    fn call_tool(&self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((-32602, "tools/call needs a `name`".to_string()))?;
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let argv = match name {
            "datum_log" => log_argv(&args),
            "datum_now" => Ok(vec!["now".to_string()]),
            "datum_tail" => tail_argv(&args),
            "datum_read" => read_argv(&args),
            _ => return Err((-32602, format!("unknown tool: {name}"))),
        };
        // A bad argument is the tool's answer, not a protocol error, so the
        // model sees why and can try again.
        let argv = match argv {
            Ok(argv) => argv,
            Err(why) => return Ok(tool_result(&why, true)),
        };
        let out = self.runner.run(&self.config, &argv);
        Ok(match out.code {
            0 => tool_result(&join_output(&out), false),
            EXIT_DUPLICATE => tool_result(
                &format!("already logged (one entry, not two)\n{}", join_output(&out)),
                false,
            ),
            code => tool_result(
                &format!(
                    "datum {} exited {code}\n{}",
                    argv.join(" "),
                    join_output(&out)
                ),
                true,
            ),
        })
    }
}

fn join_output(out: &RunOutput) -> String {
    let mut text = out.stdout.trim_end().to_string();
    let err = out.stderr.trim_end();
    if !err.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(err);
    }
    if text.is_empty() {
        text.push_str("(no output)");
    }
    text
}

fn tool_result(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// What the server tells a client about itself at `initialize`.
pub fn instructions(actor: &str) -> String {
    format!(
        "datum is a timeline of moments, stretches and states. Use it to keep a record of \
your own work as you do it: `start` a stretch when you begin a piece of work and `stop` it \
when you finish, `switch` a state when it changes (e.g. the environment you are working in), \
and `occur` a moment worth noting (a test failing, a decision, an error). Log what you \
actually did, when you did it, including mistakes and things you could not finish. Your \
writes are recorded as actor `{actor}`. Use `datum_now` and `datum_tail` to read the \
timeline back."
    )
}

/// The tools, as `tools/list` gives them.
pub fn tools() -> Value {
    json!([
        {
            "name": "datum_log",
            "title": "Log to datum",
            "description": "Record an entry on the timeline. `occur`: a moment (no duration). \
    `start` / `stop`: begin or end a stretch of activity on a field. `switch`: set a field's state, \
    which holds until changed. `note`: a timeless jot. Extra details go in `data` as key/value pairs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "enum": LOG_COMMANDS },
                    "field": {
                        "type": "string",
                        "description": "What is being tracked, e.g. `refactor`, `tests`, `environment`."
                    },
                    "state": {
                        "type": ["string", "number", "boolean"],
                        "description": "For `switch`: the new state."
                    },
                    "data": {
                        "type": "object",
                        "description": "Details as key/value pairs, e.g. {\"task\": \"auth\", \"files\": 3}.",
                        "additionalProperties": { "type": ["string", "number", "boolean"] }
                    },
                    "time": {
                        "type": "string",
                        "description": "When, if not now: datum's -t, e.g. `-5` (5 min ago), `14:30`, an ISO time."
                    },
                    "duration": {
                        "type": "string",
                        "description": "How long it lasted, ending at `time` (e.g. `30m`, `1:30`)."
                    }
                },
                "required": ["command", "field"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true }
        },
        {
            "name": "datum_now",
            "title": "datum now",
            "description": "The current state of every tracked field: what is ongoing and each field's latest state.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "datum_tail",
            "title": "Recent datum entries",
            "description": "The most recent entries, as JSON, optionally of one field. Each row names the `actor` that logged it, where one did.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "field": { "type": "string" },
                    "n": { "type": "integer", "minimum": 1, "maximum": 500 }
                }
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "datum_read",
            "title": "Run a read-only datum command",
            "description": format!(
                "Run one of datum's read-only commands with its arguments, e.g. [\"stats\", \"tests\"] \
    or [\"history\", \"<hid>\"]. Allowed commands: {}.",
                READ_COMMANDS.join(", ")
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "args": { "type": "array", "items": { "type": "string" }, "minItems": 1 }
                },
                "required": ["args"]
            },
            "annotations": { "readOnlyHint": true }
        }
    ])
}

/// A field or key name: no flag, no `=`, no whitespace.
fn valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_alphanumeric() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '%'))
}

/// A value as datum reads it on a command line.
fn scalar(v: &Value) -> Result<String, String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Null => Err("a value may not be null".into()),
        other => Ok(other.to_string()),
    }
}

/// A positional the CLI would not read as a flag or as data.
fn positional(s: &str, what: &str) -> Result<String, String> {
    if s.is_empty() {
        return Err(format!("{what} may not be empty"));
    }
    if s.starts_with('-') && s.parse::<f64>().is_err() {
        return Err(format!("{what} may not start with `-`: {s:?}"));
    }
    if s.contains('=') {
        return Err(format!("{what} may not contain `=`: {s:?}"));
    }
    Ok(s.to_string())
}

/// `datum_log`'s arguments to the argv it runs.
pub fn log_argv(args: &Value) -> Result<Vec<String>, String> {
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .ok_or("`command` is required")?;
    if !LOG_COMMANDS.contains(&command) {
        return Err(format!(
            "`command` must be one of {}",
            LOG_COMMANDS.join(", ")
        ));
    }
    let field = args
        .get("field")
        .and_then(Value::as_str)
        .ok_or("`field` is required")?;
    if !valid_name(field) {
        return Err(format!("not a field name: {field:?}"));
    }
    let mut argv = vec![command.to_string(), field.to_string()];
    match (command, args.get("state").filter(|v| !v.is_null())) {
        ("switch", Some(state)) => argv.push(positional(&scalar(state)?, "`state`")?),
        ("switch", None) => return Err("`switch` needs a `state`".into()),
        (_, Some(_)) => return Err(format!("`state` is for `switch`, not `{command}`")),
        (_, None) => {}
    }
    if let Some(data) = args.get("data").filter(|v| !v.is_null()) {
        let data = data.as_object().ok_or("`data` must be an object")?;
        // Sorted, so the same call is the same line.
        let sorted: BTreeMap<&String, &Value> = data.iter().collect();
        for (key, value) in sorted {
            if !valid_name(key) {
                return Err(format!("not a key name: {key:?}"));
            }
            if matches!(
                key.as_str(),
                "occur_time" | "dur" | "start_time" | "end_time"
            ) {
                return Err(format!(
                    "`{key}` is set by `time` and `duration`, not `data`"
                ));
            }
            argv.push(format!("{key}={}", scalar(value)?));
        }
    }
    if let Some(dur) = str_arg(args, "duration")? {
        argv.push(format!("dur={dur}"));
    }
    if let Some(time) = str_arg(args, "time")? {
        argv.push("-t".into());
        argv.push(time);
    }
    Ok(argv)
}

fn str_arg(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

/// `datum_tail`'s arguments to the argv it runs.
pub fn tail_argv(args: &Value) -> Result<Vec<String>, String> {
    let mut argv = vec!["tail".to_string()];
    if let Some(field) = str_arg(args, "field")? {
        if !valid_name(&field) {
            return Err(format!("not a field name: {field:?}"));
        }
        argv.push(field);
    }
    let n = match args.get("n") {
        None | Some(Value::Null) => 20,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=500).contains(n))
            .ok_or("`n` must be 1-500")?,
    };
    argv.extend(["-n".to_string(), n.to_string(), "--json".to_string()]);
    Ok(argv)
}

/// `datum_read`'s arguments to the argv it runs: a read-only command and
/// its words, refused otherwise.
pub fn read_argv(args: &Value) -> Result<Vec<String>, String> {
    let words = args
        .get("args")
        .and_then(Value::as_array)
        .ok_or("`args` must be an array of strings")?;
    let argv: Vec<String> = words
        .iter()
        .map(|w| {
            w.as_str()
                .map(str::to_string)
                .ok_or("`args` must be strings")
        })
        .collect::<Result<_, _>>()?;
    let first = argv.first().ok_or("`args` is empty")?;
    if !READ_COMMANDS.contains(&first.as_str()) {
        return Err(format!(
            "`{first}` is not a read-only command here; allowed: {}",
            READ_COMMANDS.join(", ")
        ));
    }
    if let Some(flag) = argv.iter().find(|w| WRITING_FLAGS.contains(&w.as_str())) {
        return Err(format!("`{flag}` writes, and `datum_read` only reads"));
    }
    if argv.iter().any(|w| w == "--db" || w.starts_with("--db=")) {
        return Err("the database is the server's (`--db`), not a call's".into());
    }
    Ok(argv)
}

/// Parse the server's own command line.
pub fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut bin = std::env::var_os("DATUM_BIN").map(PathBuf::from);
    let mut db = std::env::var("DATUM_DB").ok().filter(|d| !d.is_empty());
    let mut actor = std::env::var("DATUM_ACTOR")
        .ok()
        .filter(|a| !a.trim().is_empty());
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--bin" => bin = Some(PathBuf::from(value("--bin")?)),
            "--db" => db = Some(value("--db")?),
            "--actor" => actor = Some(value("--actor")?),
            "-h" | "--help" => return Err(USAGE.into()),
            other => return Err(format!("unknown argument: {other}\n\n{USAGE}")),
        }
    }
    Ok(Config {
        bin: bin.unwrap_or_else(default_bin),
        db,
        actor: actor
            .map(|a| a.trim().to_string())
            .unwrap_or_else(|| "agent".into()),
    })
}

/// `datum` beside this binary when there is one (a workspace build, an
/// install), else whatever `datum` is on the PATH.
fn default_bin() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.parent()
                .map(|dir| dir.join(format!("datum{}", std::env::consts::EXE_SUFFIX)))
        })
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("datum"))
}

pub const USAGE: &str = "\
datum-mcp: a Model Context Protocol server (stdio) over the datum CLI

USAGE:
  datum-mcp [--db PATH] [--actor NAME] [--bin PATH]

  --db PATH      the database (else DATUM_DB, else datum's default)
  --actor NAME   who the writes are on behalf of (else DATUM_ACTOR, else `agent`);
                 entries carry it as extra_metadata.actor
  --bin PATH     the datum binary (else DATUM_BIN, else `datum` beside this one or on PATH)

Claude Code:
  claude mcp add datum -- datum-mcp --actor agent:claude-code
";

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<Vec<String>>>,
        reply: Option<RunOutput>,
    }

    impl Runner for Fake {
        fn run(&self, _: &Config, argv: &[String]) -> RunOutput {
            self.calls.borrow_mut().push(argv.to_vec());
            self.reply.clone().unwrap_or(RunOutput {
                code: 0,
                stdout: "OK".into(),
                stderr: String::new(),
            })
        }
    }

    fn server(reply: Option<RunOutput>) -> Server<Fake> {
        Server::new(
            Config {
                bin: "datum".into(),
                db: None,
                actor: "agent:test".into(),
            },
            Fake {
                calls: RefCell::default(),
                reply,
            },
        )
    }

    fn call(s: &Server<Fake>, name: &str, args: Value) -> Value {
        let reply = s
            .handle(json!({"jsonrpc":"2.0","id":7,"method":"tools/call",
                "params":{"name":name,"arguments":args}}))
            .unwrap();
        reply["result"].clone()
    }

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn initialize_answers_with_a_version_it_speaks() {
        let s = server(None);
        let r = s
            .handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}))
            .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        let r = s
            .handle(json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}))
            .unwrap();
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
        assert!(r["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("agent:test"));
    }

    #[test]
    fn a_notification_gets_no_reply_and_an_unknown_method_an_error() {
        let s = server(None);
        assert!(s
            .handle(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .is_none());
        let r = s
            .handle(json!({"jsonrpc":"2.0","id":3,"method":"nope"}))
            .unwrap();
        assert_eq!(r["error"]["code"], -32601);
        let r = s.handle_line("{not json").unwrap();
        assert_eq!(r["error"]["code"], -32700);
    }

    #[test]
    fn tools_list_names_the_four_tools() {
        let s = server(None);
        let r = s
            .handle(json!({"jsonrpc":"2.0","id":4,"method":"tools/list"}))
            .unwrap();
        let names: Vec<&str> = r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["datum_log", "datum_now", "datum_tail", "datum_read"]
        );
    }

    #[test]
    fn log_builds_the_line_a_person_would_type() {
        assert_eq!(
            log_argv(
                &json!({"command":"start","field":"refactor","data":{"task":"auth","files":3}})
            )
            .unwrap(),
            argv(&["start", "refactor", "files=3", "task=auth"])
        );
        assert_eq!(
            log_argv(
                &json!({"command":"switch","field":"environment","state":"staging","time":"-5"})
            )
            .unwrap(),
            argv(&["switch", "environment", "staging", "-t", "-5"])
        );
        assert_eq!(
            log_argv(&json!({"command":"stop","field":"refactor","duration":"30m"})).unwrap(),
            argv(&["stop", "refactor", "dur=30m"])
        );
        assert_eq!(
            log_argv(&json!({"command":"occur","field":"test_fail","data":{"passed":false}}))
                .unwrap(),
            argv(&["occur", "test_fail", "passed=false"])
        );
    }

    #[test]
    fn log_refuses_what_would_read_as_a_flag_or_another_key() {
        for bad in [
            json!({"command":"delete","field":"x"}),
            json!({"command":"occur","field":"--db"}),
            json!({"command":"occur","field":"a b"}),
            json!({"command":"switch","field":"mood"}),
            json!({"command":"switch","field":"mood","state":"--fix"}),
            json!({"command":"switch","field":"mood","state":"a=b"}),
            json!({"command":"occur","field":"x","state":"y"}),
            json!({"command":"occur","field":"x","data":{"-t":"1"}}),
            json!({"command":"occur","field":"x","data":{"occur_time":"1"}}),
        ] {
            assert!(log_argv(&bad).is_err(), "{bad}");
        }
        // A negative number is a state, not a flag.
        assert!(log_argv(&json!({"command":"switch","field":"temp","state":-3})).is_ok());
    }

    #[test]
    fn a_refused_argument_is_the_tool_s_answer_and_runs_nothing() {
        let s = server(None);
        let r = call(&s, "datum_log", json!({"command":"occur","field":"--db"}));
        assert_eq!(r["isError"], true);
        assert!(s.runner.calls.borrow().is_empty());
    }

    #[test]
    fn read_runs_only_reads() {
        assert_eq!(
            read_argv(&json!({"args":["stats","tests"]})).unwrap(),
            argv(&["stats", "tests"])
        );
        assert!(read_argv(&json!({"args":["delete","x"]})).is_err());
        assert!(read_argv(&json!({"args":["occur","x"]})).is_err());
        assert!(read_argv(&json!({"args":["get","x","--fix"]})).is_err());
        assert!(read_argv(&json!({"args":["tail","--db","/etc/x"]})).is_err());
        assert!(read_argv(&json!({"args":[]})).is_err());
    }

    #[test]
    fn tail_asks_for_json() {
        assert_eq!(
            tail_argv(&json!({})).unwrap(),
            argv(&["tail", "-n", "20", "--json"])
        );
        assert_eq!(
            tail_argv(&json!({"field":"tests","n":5})).unwrap(),
            argv(&["tail", "tests", "-n", "5", "--json"])
        );
        assert!(tail_argv(&json!({"n":0})).is_err());
    }

    #[test]
    fn a_duplicate_is_a_success_and_a_failure_an_error() {
        let dup = server(Some(RunOutput {
            code: 11,
            stdout: "EXISTS".into(),
            stderr: String::new(),
        }));
        let r = call(&dup, "datum_log", json!({"command":"occur","field":"x"}));
        assert_eq!(r["isError"], false);
        let bad = server(Some(RunOutput {
            code: 1,
            stdout: String::new(),
            stderr: "Error: boom".into(),
        }));
        let r = call(&bad, "datum_now", json!({}));
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"].as_str().unwrap().contains("boom"));
    }

    #[test]
    fn the_server_s_flags_override_the_environment() {
        let c = parse_args(&argv(&[
            "--db",
            "/tmp/a.db",
            "--actor",
            "agent:x",
            "--bin",
            "/bin/datum",
        ]))
        .unwrap();
        assert_eq!(c.db.as_deref(), Some("/tmp/a.db"));
        assert_eq!(c.actor, "agent:x");
        assert_eq!(c.bin, PathBuf::from("/bin/datum"));
        assert!(parse_args(&argv(&["--nope"])).is_err());
    }
}
