//! T8 — end-to-end spawn smoke test: initialize -> tools/list -> tools/call(enumerate) fed to the
//! real `maple mcp` binary over a stdio pipe, then stdin EOF -> graceful exit. Unit-level dispatch
//! coverage (handle_request) lives in src/mcp.rs; this is the "actually spawns the binary" half the
//! spec asks for (CARGO_BIN_EXE_maple is only set for integration tests, hence this separate file).

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn spawn_smoke_initialize_list_call() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("lib.py"), "def target():\n    return 1\n").unwrap();
    std::fs::write(root.join("app.py"), "def caller():\n    return target()\n").unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_maple"))
        .args(["mcp", root.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#).unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#).unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}"#).unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"enumerate","arguments":{{"symbol":"target"}}}}}}"#
        )
        .unwrap();
    }
    child.stdin.take(); // close stdin -> EOF -> graceful exit

    let stdout = child.stdout.take().unwrap();
    let lines: Vec<String> = BufReader::new(stdout).lines().map(|l| l.unwrap()).collect();
    let status = child.wait().unwrap();
    assert!(status.success(), "server should exit 0 on stdin EOF");
    assert_eq!(lines.len(), 3, "3 replies for 3 requests-with-id (the notification gets none)");

    let init: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "maple");
    let list: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 3);
    let call: serde_json::Value = serde_json::from_str(&lines[2]).unwrap();
    let text = call["result"]["content"][0]["text"].as_str().unwrap();
    let enumerate_result: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(enumerate_result["caller_count"], 1);
}
