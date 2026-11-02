//! A scriptable stand-in for qemu's QMP socket, for tests: greets, accepts
//! `qmp_capabilities`, and answers every other command with whatever the
//! test's `reply` returns, recording the command names in order.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

type Log = Arc<Mutex<Vec<String>>>;

pub(crate) struct FakeQmp {
    commands: Log,
}

impl FakeQmp {
    /// Serves on `path` from a background thread for the rest of the test.
    /// `reply(command, arguments)` returns the whole response object, e.g.
    /// `{"return": {}}` or `{"error": {...}}`.
    pub(crate) fn serve(
        path: &Path,
        reply: impl Fn(&str, &Value) -> Value + Send + 'static,
    ) -> Self {
        let listener = UnixListener::bind(path).unwrap();
        let commands = Log::default();
        let log = Arc::clone(&commands);
        std::thread::spawn(move || {
            for conn in listener.incoming().map_while(Result::ok) {
                answer(&conn, &reply, &log);
            }
        });
        Self { commands }
    }

    /// The commands received so far, excluding the handshake.
    pub(crate) fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }
}

/// `{"return": value}`.
pub(crate) fn ok(value: Value) -> Value {
    let mut response = json!({});
    response["return"] = value;
    response
}

/// A QMP error response.
pub(crate) fn error(desc: &str) -> Value {
    json!({ "error": { "class": "GenericError", "desc": desc } })
}

fn answer(conn: &UnixStream, reply: &impl Fn(&str, &Value) -> Value, log: &Log) {
    let mut out = conn;
    let greeting = json!({
        "QMP": {
            "version": { "qemu": { "major": 9, "minor": 2, "micro": 0 }, "package": "" },
            "capabilities": []
        }
    });
    writeln!(out, "{greeting}").unwrap();
    for line in BufReader::new(conn).lines().map_while(Result::ok) {
        let message: Value = serde_json::from_str(&line).unwrap();
        let name = message["execute"].as_str().unwrap().to_owned();
        let response = if name == "qmp_capabilities" {
            ok(json!({}))
        } else {
            let response = reply(&name, &message["arguments"]);
            log.lock().unwrap().push(name);
            response
        };
        // The client may hang up right after reading what it needs.
        writeln!(out, "{response}").unwrap_or_default();
    }
}
