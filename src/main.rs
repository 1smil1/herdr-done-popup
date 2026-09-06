#![cfg(windows)]
// herdr-done-popup
//
// Per-event:  herdr-done-popup event         (one process per agent event; creates popup, exits)
// Daemon:     herdr-done-popup start        (link to all sessions + watch for new ones)
// Inspect:     herdr-done-popup status       (which sessions have us linked)
// Detach:      herdr-done-popup stop         (unlink from all sessions)
//
// Each herdr [[events]] invocation runs `event` once. We fetch the pane's
// first sentence, decide suppression / mode, draw the popup, and run a small
// Win32 message loop until the user dismisses it. No TCP, no shared state.

use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

mod gui;
use gui::{run_popup, PopupInfo, PaneMeta};

const PLUGIN_ID: &str = "herdr-done-popup";

/* =============================== main / dispatch =============================== */

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [cmd] if cmd == "event" => std::process::exit(run_event()),
        [cmd] if cmd == "start" => run_start(),
        [cmd] if cmd == "stop" => run_stop(),
        [cmd] if cmd == "status" => run_status(),
        other => {
            eprintln!("usage: herdr-done-popup <event|start|stop|status>  (got: {:?})", other);
            std::process::exit(2);
        }
    }
}

/* =============================== per-event popup =============================== */

fn run_event() -> i32 {
    let Some(ev) = std::env::var("HERDR_PLUGIN_EVENT_JSON").ok() else {
        return 0;
    };
    let sock = std::env::var("HERDR_SOCKET_PATH").unwrap_or_default();
    let session = session_from_socket(&sock);

    let Ok(root) = serde_json::from_str::<Value>(&ev) else {
        return 0;
    };
    let Some(pane) = find_str(&root, &["pane_id", "paneId"]) else {
        return 0;
    };
    let Some(agent) = find_str(&root, &["agent", "kind"]) else {
        return 0;
    };
    let Some(status) = find_str(&root, &["status", "state", "agent_status", "agentStatus"]) else {
        return 0;
    };
    // idle/done = 完成；blocked = agent 在提问/等批准，也必须提醒。
    if !matches!(status, "idle" | "done" | "blocked") {
        return 0;
    }

    let snippet = fetch_snippet(&session, pane);
    let workspace_id = pane.split(':').next().unwrap_or("").to_string();
    let workspace_label = resolve_workspace_label(&session, &workspace_id);
    let meta = fetch_pane_meta(&session, pane, &workspace_id);
    let workspace = compose_location(&workspace_label, &meta);

    let popup = PopupInfo {
        session,
        agent: agent.to_string(),
        pane: pane.to_string(),
        workspace,
        tab_id: meta.tab_id,
        snippet,
        blocked: status == "blocked",
    };
    run_popup(popup);
    0
}

/* =============================== daemon: start / stop / status =============================== */

fn run_start() {
    let sessions = list_sessions();
    if sessions.is_empty() {
        eprintln!("start: no sessions found (is herdr running?)");
        std::process::exit(1);
    }
    let mut linked: HashSet<String> = HashSet::new();
    for s in &sessions {
        match link_session(s) {
            Ok(_) => {
                linked.insert(s.clone());
                eprintln!("start: linked to session `{}`", s);
            }
            Err(e) => eprintln!("start: failed to link `{}`: {}", s, e),
        }
    }
    eprintln!(
        "start: watching {} session(s); poll every 10s. Press Ctrl+C to stop.",
        sessions.len()
    );
    // Watch for new sessions and link them too.
    loop {
        std::thread::sleep(Duration::from_secs(10));
        let current = list_sessions();
        for s in current {
            if !linked.contains(&s) {
                match link_session(&s) {
                    Ok(_) => {
                        eprintln!("start: linked new session `{}`", s);
                        linked.insert(s);
                    }
                    Err(e) => eprintln!("start: failed to link new `{}`: {}", s, e),
                }
            }
        }
    }
}

fn run_stop() {
    let sessions = list_sessions();
    for s in &sessions {
        match unlink_session(s) {
            Ok(_) => eprintln!("stop: unlinked `{}`", s),
            Err(e) => eprintln!("stop: failed to unlink `{}`: {}", s, e),
        }
    }
    eprintln!("stop: done. (the watcher process needs to be killed separately)");
}

fn run_status() {
    let sessions = list_sessions();
    if sessions.is_empty() {
        println!("no sessions found");
        return;
    }
    for s in sessions {
        let linked = session_has_plugin(&s);
        println!("  {:<12} {}", s, if linked { "linked" } else { "not linked" });
    }
}

/* =============================== session management =============================== */

fn list_sessions() -> Vec<String> {
    let out = match Command::new("herdr").args(["session", "list"]).output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut names = Vec::new();
    // Try JSON first.
    if let Ok(v) = serde_json::from_str::<Value>(&text) {
        collect_session_names(&v, &mut names);
    } else {
        // Human-readable table: parse rows like
        //   "default   running  ..."
        // Take the first whitespace-delimited token of each non-header line.
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // skip header (contains "name" + "status")
            let first = trimmed.split_whitespace().next().unwrap_or("");
            if first.is_empty() || first.eq_ignore_ascii_case("name") {
                continue;
            }
            names.push(first.to_string());
        }
    }
    if !names.iter().any(|n| n == "default") {
        names.push("default".to_string());
    }
    names.sort();
    names.dedup();
    names
}

fn collect_session_names(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            if let Some(name) = map.get("name").and_then(|n| n.as_str()) {
                out.push(name.to_string());
            }
            for val in map.values() {
                collect_session_names(val, out);
            }
        }
        Value::Array(arr) => {
            for val in arr {
                collect_session_names(val, out);
            }
        }
        _ => {}
    }
}

fn link_session(name: &str) -> std::io::Result<()> {
    let root = plugin_root();
    let status = Command::new("herdr")
        .args([
            "--session",
            name,
            "plugin",
            "link",
            &root.to_string_lossy(),
            "--enabled",
        ])
        .status()?;
    if !status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("herdr plugin link exited with {:?}", status.code()),
        ));
    }
    let _ = Command::new("herdr")
        .args(["--session", name, "server", "reload-config"])
        .status();
    Ok(())
}

fn unlink_session(name: &str) -> std::io::Result<()> {
    let status = Command::new("herdr")
        .args(["--session", name, "plugin", "unlink", PLUGIN_ID])
        .status()?;
    if !status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("herdr plugin unlink exited with {:?}", status.code()),
        ));
    }
    let _ = Command::new("herdr")
        .args(["--session", name, "server", "reload-config"])
        .status();
    Ok(())
}

fn session_has_plugin(name: &str) -> bool {
    let out = match Command::new("herdr")
        .args(["--session", name, "plugin", "list"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    let text = String::from_utf8_lossy(&out.stdout);
    text.contains(PLUGIN_ID)
}

fn plugin_root() -> PathBuf {
    if let Some(root) = std::env::var_os("HERDR_PLUGIN_ROOT") {
        return PathBuf::from(root);
    }
    // fallback: assume exe lives at <plugin>/target/release/<exe>
    if let Ok(exe) = std::env::current_exe() {
        if let Some(target) = exe.parent() {
            if let Some(root) = target.parent().and_then(|p| p.parent()) {
                return root.to_path_buf();
            }
        }
    }
    PathBuf::from(".")
}

/* =============================== session from socket =============================== */

/// `C:\Users\...\sessions\dse\herdr.sock` -> "dse"；否则 "default"
fn session_from_socket(socket: &str) -> String {
    let p = std::path::Path::new(socket);
    let components: Vec<_> = p
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    if let Some(idx) = components.iter().position(|c| c == "sessions") {
        if let Some(name) = components.get(idx + 1) {
            if !name.is_empty() {
                return name.clone();
            }
        }
    }
    "default".to_string()
}

/* =============================== JSON helpers =============================== */

fn find_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    match value {
        Value::Object(map) => {
            for k in keys {
                if let Some(v) = map.get(*k) {
                    if let Some(s) = v.as_str() {
                        return Some(s);
                    }
                }
            }
            for v in map.values() {
                if let Some(s) = find_str(v, keys) {
                    return Some(s);
                }
            }
            None
        }
        Value::Array(arr) => {
            for v in arr {
                if let Some(s) = find_str(v, keys) {
                    return Some(s);
                }
            }
            None
        }
        _ => None,
    }
}

/* =============================== content fetching =============================== */

fn fetch_snippet(session: &str, pane: &str) -> String {
    let out = match Command::new("herdr")
        .args([
            "--session", session, "pane", "read", pane,
            "--source", "visible", "--lines", "12",
        ])
        .output()
    {
        Ok(o) => o,
        Err(_) => return String::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut first = String::new();
    for line in text.lines() {
        let clean = strip_ansi(line);
        let trimmed = clean.trim();
        if trimmed.is_empty()
            || is_claude_chrome(trimmed)
            || is_prompt(trimmed)
            || is_input_or_done(trimmed)
        {
            continue;
        }
        first = collapse_ws(trimmed);
        first = first
            .trim_start_matches(|c: char| matches!(c, '●' | '✽' | '·' | '*' | '›' | '▶' | '>'))
            .trim()
            .to_string();
        if first.is_empty() {
            continue;
        }
        if first.chars().count() > 60 {
            first = truncate_chars(&first, 60);
        }
        break;
    }
    first
}

fn fetch_pane_meta(session: &str, pane: &str, workspace_id: &str) -> PaneMeta {
    let mut tab_labels: std::collections::HashMap<String, String> = Default::default();
    if let Ok(out) = Command::new("herdr")
        .args([
            "--session", session, "tab", "list", "--workspace", workspace_id,
        ])
        .output()
    {
        if let Ok(v) = serde_json::from_str::<Value>(&String::from_utf8_lossy(&out.stdout)) {
            collect_labels(&v, "tab_id", "label", &mut tab_labels);
        }
    }
    let mut panes: Vec<(String, String, String)> = Vec::new();
    if let Ok(out) = Command::new("herdr")
        .args([
            "--session", session, "pane", "list", "--workspace", workspace_id,
        ])
        .output()
    {
        if let Ok(v) = serde_json::from_str::<Value>(&String::from_utf8_lossy(&out.stdout)) {
            collect_pane_info(&v, &mut panes);
        }
    }
    for (pid, tab_id, pane_label) in &panes {
        if pid == pane {
            let tab_label = tab_labels.get(tab_id).cloned().unwrap_or_default();
            return PaneMeta {
                tab_id: tab_id.clone(),
                tab_label,
                pane_label: pane_label.clone(),
            };
        }
    }
    PaneMeta::default()
}

fn resolve_workspace_label(session: &str, workspace_id: &str) -> String {
    if workspace_id.is_empty() {
        return String::new();
    }
    let Ok(out) = Command::new("herdr")
        .args(["--session", session, "workspace", "list"])
        .output()
    else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let Ok(root) = serde_json::from_str::<Value>(&text) else {
        return String::new();
    };
    collect_labels(&root, "workspace_id", "label", &mut Default::default())
        .into_iter()
        .find(|(id, _)| id == workspace_id)
        .map(|(_, label)| label)
        .unwrap_or_default()
}

fn compose_location(workspace_label: &str, meta: &PaneMeta) -> String {
    let pane = if !meta.pane_label.is_empty() {
        meta.pane_label.clone()
    } else {
        String::new()
    };
    let tab = if !meta.tab_label.is_empty() {
        meta.tab_label.clone()
    } else {
        String::new()
    };
    if !tab.is_empty() && !pane.is_empty() {
        return format!("{} · {}", tab, pane);
    }
    if !tab.is_empty() {
        return tab;
    }
    if !pane.is_empty() {
        return pane;
    }
    workspace_label.to_string()
}

fn collect_labels(
    value: &Value,
    id_field: &str,
    label_field: &str,
    out: &mut std::collections::HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    match value {
        Value::Object(map) => {
            if let (Some(id), Some(label)) = (
                map.get(id_field).and_then(|v| v.as_str()),
                map.get(label_field).and_then(|v| v.as_str()),
            ) {
                pairs.push((id.to_string(), label.to_string()));
                out.insert(id.to_string(), label.to_string());
            }
            for v in map.values() {
                pairs.extend(collect_labels(v, id_field, label_field, out));
            }
        }
        Value::Array(arr) => {
            for v in arr {
                pairs.extend(collect_labels(v, id_field, label_field, out));
            }
        }
        _ => {}
    }
    pairs
}

fn collect_pane_info(value: &Value, out: &mut Vec<(String, String, String)>) {
    match value {
        Value::Object(map) => {
            let pid = map.get("pane_id").and_then(|v| v.as_str());
            let tab_id = map.get("tab_id").and_then(|v| v.as_str()).unwrap_or("");
            let label = map.get("label").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(pid) = pid {
                out.push((pid.to_string(), tab_id.to_string(), label.to_string()));
            }
            for v in map.values() {
                collect_pane_info(v, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_pane_info(v, out);
            }
        }
        _ => {}
    }
}

fn strip_ansi(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0x1b {
            if i + 1 < b.len() && b[i + 1] == b'[' {
                i += 2;
                while i < b.len() && !(b[i] >= 0x40 && b[i] <= 0x7e) {
                    i += 1;
                }
                i += 1;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

fn is_claude_chrome(trimmed: &str) -> bool {
    let lower = trimmed.to_lowercase();
    trimmed.starts_with('─')
        || trimmed.starts_with('❯')
        || lower.contains("esc to interrupt")
        || lower.contains("shift+tab")
        || lower.contains("bypass permissions")
        || lower.contains("thought for")
        || lower.contains("· ↓")
        || lower.contains("· done")
        || trimmed.contains("Baked for")
        || trimmed.contains("Cooking")
}

fn is_prompt(trimmed: &str) -> bool {
    trimmed.ends_with('>')
        || trimmed.ends_with('$')
        || trimmed.ends_with('#')
        || trimmed.starts_with('(')
        || trimmed.len() < 3
}

fn is_input_or_done(trimmed: &str) -> bool {
    trimmed.starts_with('>')
        || trimmed.starts_with("* ")
        || trimmed.starts_with("✽ ")
        || trimmed.starts_with("✻ ")
}