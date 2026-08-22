//! Shared continuation-memory: file-based, no daemon, append-only JSONL threads.
//! Each thread is `<dir>/<thread_id>.jsonl`, one JSON turn per line, append-only.
//! This is maple's independent implementation of a design shared with bob/hector/abe
//! (no shared crate across the four repos — matches their existing per-repo mcp.rs
//! pattern of copy-paste-and-adapt rather than a shared library).

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// ponytail: fixed turn-count cap, not a token-budget calculation; raise if 20 turns
// of real usage proves too coarse. No tokenizer dependency needed for that upgrade.
const MAX_TURNS: usize = 20;

#[derive(Debug, Serialize, Deserialize)]
pub struct Turn {
    pub role: String,
    pub tool: String,
    pub content: String,
    #[serde(default)]
    pub model: Option<String>,
    pub ts: u64,
}

/// Resolve the thread storage directory: $AGENT_THREAD_DIR, else
/// $XDG_CACHE_HOME/agent-thread, else $HOME/.cache/agent-thread.
pub fn thread_dir() -> anyhow::Result<PathBuf> {
    if let Ok(dir) = std::env::var("AGENT_THREAD_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if let Ok(cache) = std::env::var("XDG_CACHE_HOME") {
        return Ok(Path::new(&cache).join("agent-thread"));
    }
    let home = std::env::var("HOME")
        .map_err(|_| anyhow::anyhow!("cannot resolve thread storage dir: $HOME not set"))?;
    Ok(Path::new(&home).join(".cache").join("agent-thread"))
}

fn thread_path(id: &str) -> anyhow::Result<PathBuf> {
    Ok(thread_dir()?.join(format!("{id}.jsonl")))
}

/// 32 lowercase hex chars from 16 bytes read off /dev/urandom. No uuid crate.
pub fn new_thread_id() -> anyhow::Result<String> {
    let mut buf = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Read a thread's turns, capped to the last MAX_TURNS, in chronological order.
/// Missing file -> Err (unknown/expired continuation_id), not a silent empty history.
pub fn read_turns(id: &str) -> anyhow::Result<Vec<Turn>> {
    let path = thread_path(id)?;
    let text = std::fs::read_to_string(&path)
        .map_err(|_| anyhow::anyhow!("unknown or expired continuation_id: {id}"))?;
    let mut turns = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Turn>(line) {
            Ok(t) => turns.push(t),
            Err(e) => eprintln!("maple thread {id}: skipping malformed turn: {e}"),
        }
    }
    if turns.len() > MAX_TURNS {
        let drop = turns.len() - MAX_TURNS;
        turns.drain(0..drop);
    }
    Ok(turns)
}

/// Render a thread's turns as flattened text (not currently used to modify maple's
/// own queries — see NOTE in mcp.rs — but kept as a general-purpose primitive so
/// other tools/consumers of this thread store can render it uniformly).
#[allow(dead_code)]
pub fn render_history(id: &str) -> anyhow::Result<String> {
    let turns = read_turns(id)?;
    let mut out = String::new();
    for t in &turns {
        out.push_str(&format!("[{} / {}]\n{}\n\n", t.tool, t.role, t.content));
    }
    Ok(out)
}

/// Append one turn to a thread (creating the file/dir if needed).
pub fn append_turn(
    id: &str,
    role: &str,
    tool: &str,
    content: &str,
    model: Option<&str>,
) -> anyhow::Result<()> {
    let dir = thread_dir()?;
    std::fs::create_dir_all(&dir)?;
    let path = thread_path(id)?;
    let turn = Turn {
        role: role.to_string(),
        tool: tool.to_string(),
        content: content.to_string(),
        model: model.map(|s| s.to_string()),
        ts: now_ms(),
    };
    let line = serde_json::to_string(&turn)?;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;
    // pub(crate) so mcp.rs's tests can share the SAME lock — both modules mutate
    // the process-global AGENT_THREAD_DIR env var, and Rust runs tests in
    // parallel by default; without a shared lock they'd race each other.
    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Run `f` with AGENT_THREAD_DIR pointed at a fresh temp dir, holding ENV_LOCK
    /// for the duration so no other thread.rs/mcp.rs test observes a torn env var.
    pub(crate) fn with_temp_dir<F: FnOnce(&std::path::Path)>(f: F) {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: serialized by ENV_LOCK above; no other thread reads/writes this
        // var while we hold the guard.
        unsafe {
            std::env::set_var("AGENT_THREAD_DIR", tmp.path());
        }
        f(tmp.path());
        unsafe {
            std::env::remove_var("AGENT_THREAD_DIR");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::with_temp_dir;
    use super::*;

    #[test]
    fn append_then_read_round_trips() {
        with_temp_dir(|_| {
            let id = new_thread_id().unwrap();
            append_turn(&id, "user", "maple.enumerate", "target", None).unwrap();
            append_turn(&id, "assistant", "maple.enumerate", r#"{"caller_count":1}"#, None).unwrap();
            let turns = read_turns(&id).unwrap();
            assert_eq!(turns.len(), 2);
            assert_eq!(turns[0].role, "user");
            assert_eq!(turns[1].role, "assistant");
        });
    }

    #[test]
    fn unknown_thread_is_an_error() {
        with_temp_dir(|_| {
            assert!(read_turns("does-not-exist").is_err());
        });
    }

    #[test]
    fn render_history_includes_tool_and_role() {
        with_temp_dir(|_| {
            let id = new_thread_id().unwrap();
            append_turn(&id, "user", "maple.bundle", "check_text", None).unwrap();
            let rendered = render_history(&id).unwrap();
            assert!(rendered.contains("maple.bundle"));
            assert!(rendered.contains("check_text"));
        });
    }

    #[test]
    fn caps_to_max_turns() {
        with_temp_dir(|_| {
            let id = new_thread_id().unwrap();
            for i in 0..25 {
                append_turn(&id, "user", "maple.enumerate", &format!("sym{i}"), None).unwrap();
            }
            let turns = read_turns(&id).unwrap();
            assert_eq!(turns.len(), MAX_TURNS);
            assert_eq!(turns[0].content, "sym5");
        });
    }
}
