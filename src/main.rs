#![cfg(windows)]
// herdr-done-popup
// 独立于 herdr_right_click.ahk 的任务完成提醒。
//   herdr-done-popup event      <- herdr [[events]] pane.agent_status_changed 调它
//   herdr-done-popup receiver   <- 常驻 GUI，监听回环 TCP，画原生置顶弹窗
// 只在 (session,pane) 观察到 working -> idle/done 时通知；session 由 HERDR_SOCKET_PATH 推导。

use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

mod gui;
use gui::{run_ui_loop, PopupInfo};

const PORT: u16 = 47777;

/* =============================== main / dispatch =============================== */

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [cmd] if cmd == "event" => std::process::exit(run_event_helper()),
        [] => run_receiver(),
        [cmd] if cmd == "receiver" => run_receiver(),
        other => {
            eprintln!(
                "usage: herdr-done-popup <event|receiver>  (got: {:?})",
                other
            );
            std::process::exit(2);
        }
    }
}

/* =============================== event helper =============================== */

fn run_event_helper() -> i32 {
    let Some(ev) = std::env::var("HERDR_PLUGIN_EVENT_JSON").ok() else {
        eprintln!("event: HERDR_PLUGIN_EVENT_JSON not set");
        return 0;
    };
    let sock = std::env::var("HERDR_SOCKET_PATH").unwrap_or_default();
    let session = session_from_socket(&sock);

    let Ok(root) = serde_json::from_str::<Value>(&ev) else {
        eprintln!("event: bad JSON");
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

    let msg = format!(
        "{{\"v\":1,\"session\":{:?},\"pane_id\":{:?},\"agent\":{:?},\"status\":{:?}}}\n",
        session, pane, agent, status
    );

    // 尝试发送；若接收器没起，先拉起 receiver 再重试几次。
    for attempt in 0..6 {
        match TcpStream::connect(("127.0.0.1", PORT)) {
            Ok(mut s) => {
                let _ = s.write_all(msg.as_bytes());
                return 0;
            }
            Err(_) if attempt == 0 => {
                let _ = spawn_receiver();
                thread::sleep(Duration::from_millis(200 + 150 * attempt));
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(200 + 150 * attempt));
            }
        }
    }
    eprintln!("event: could not reach receiver");
    0
}

fn spawn_receiver() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    Command::new(exe).arg("receiver").spawn().map(|_| ())
}

/// default -> "default"；sessions/dse/herdr.sock -> "dse"
fn session_from_socket(socket: &str) -> String {
    let p = Path::new(socket);
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

/// 递归在 Value 中找第一个指定字段名之一（宽松解析，对齐 herdr 非固定 schema）。
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

/* =============================== receiver =============================== */

#[derive(Clone, Copy, PartialEq, Eq)]
enum AgentState {
    Unknown,
    Working,
    Settled,
}

fn run_receiver() {
    // 单实例：夺下回环端口者成为接收器。
    let listener = match TcpListener::bind(("127.0.0.1", PORT)) {
        Ok(l) => l,
        Err(_) => {
            eprintln!("receiver: another instance is running");
            return;
        }
    };

    set_dpi_aware();

    let state: Arc<Mutex<HashMap<(String, String, String), AgentState>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let workspace_cache: Arc<Mutex<HashMap<String, String>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let pane_meta_cache: Arc<Mutex<HashMap<(String, String), PaneMeta>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (tx, rx): (Sender<PopupInfo>, Receiver<PopupInfo>) = mpsc::channel();

    // 接受连接的线程 -> 更新状态机，发现完成则入队 PopupInfo。
    let tst = state.clone();
    let ttx = tx.clone();
    let twc = workspace_cache.clone();
    let tpc = pane_meta_cache.clone();
    thread::spawn(move || accept_loop(listener, tst, ttx, twc, tpc));

    // UI 线程：消息循环 + 排空 channel。
    run_ui_loop(rx);
}

#[derive(Clone, Default)]
struct PaneMeta {
    tab_label: String,
    pane_label: String,
}

fn accept_loop(
    listener: TcpListener,
    state: Arc<Mutex<HashMap<(String, String, String), AgentState>>>,
    tx: Sender<PopupInfo>,
    workspace_cache: Arc<Mutex<HashMap<String, String>>>,
    pane_meta_cache: Arc<Mutex<HashMap<(String, String), PaneMeta>>>,
) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let state = state.clone();
        let tx = tx.clone();
        let wc = workspace_cache.clone();
        let pc = pane_meta_cache.clone();
        thread::spawn(move || handle_conn(stream, state, tx, wc, pc));
    }
}

fn handle_conn(
    stream: TcpStream,
    state: Arc<Mutex<HashMap<(String, String, String), AgentState>>>,
    tx: Sender<PopupInfo>,
    workspace_cache: Arc<Mutex<HashMap<String, String>>>,
    pane_meta_cache: Arc<Mutex<HashMap<(String, String), PaneMeta>>>,
) {
    let reader = BufReader::new(stream);
    for line in reader.lines().flatten() {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (Some(session), Some(pane), Some(agent), Some(status)) = (
            v["session"].as_str(),
            v["pane_id"].as_str(),
            v["agent"].as_str(),
            v["status"].as_str(),
        ) else {
            continue;
        };

        let key = (session.to_string(), pane.to_string(), agent.to_string());
        let new_state = match status {
            "working" => AgentState::Working,
            "idle" | "done" => AgentState::Settled,
            _ => continue, // blocked / unknown 不处理，也不破坏已有状态
        };
        eprintln!("receiver: session={session} pane={pane} agent={agent} status={status}");

        let mut st = state.lock().unwrap();
        let prev = st.get(&key).copied().unwrap_or(AgentState::Unknown);
        if prev == AgentState::Working && new_state == AgentState::Settled {
            let snippet = fetch_snippet(session, pane);
            let workspace_id = pane.split(':').next().unwrap_or("").to_string();
            let workspace_label = resolve_workspace(session, &workspace_id, &workspace_cache);
            let meta = resolve_pane_meta(session, pane, &workspace_id, &pane_meta_cache);
            let workspace = compose_location(&workspace_id, &workspace_label, &meta);
            let _ = tx.send(PopupInfo {
                session: session.to_string(),
                agent: agent.to_string(),
                pane: pane.to_string(),
                workspace,
                snippet,
            });
        }
        st.insert(key, new_state);
    }
}

/// 组装用户可读的“位置”字符串：`tab_label · pane_label`，
/// 缺标签时退回 tab_id/pane_id；两个都没有就只用 workspace label。
fn compose_location(workspace_id: &str, workspace_label: &str, meta: &PaneMeta) -> String {
    let tab = if !meta.tab_label.is_empty() {
        meta.tab_label.clone()
    } else {
        String::new()
    };
    let pane = if !meta.pane_label.is_empty() {
        meta.pane_label.clone()
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
    if !workspace_label.is_empty() && workspace_label != workspace_id {
        workspace_label.to_string()
    } else {
        workspace_id.to_string()
    }
}

/// 通过 `herdr pane list` / `tab list` 取 tab + pane 标签，缓存到 pane_meta_cache。
fn resolve_pane_meta(
    session: &str,
    pane_id: &str,
    workspace_id: &str,
    cache: &Arc<Mutex<HashMap<(String, String), PaneMeta>>>,
) -> PaneMeta {
    let key = (session.to_string(), pane_id.to_string());
    if let Some(m) = cache.lock().unwrap().get(&key).cloned() {
        return m;
    }
    let mut tab_labels: HashMap<String, String> = HashMap::new();
    if let Ok(out) = Command::new("herdr")
        .args(["--session", session, "tab", "list", "--workspace", workspace_id])
        .output()
    {
        if let Ok(v) = serde_json::from_str::<Value>(&String::from_utf8_lossy(&out.stdout)) {
            collect_labels(&v, "tab_id", "label", &mut tab_labels);
        }
    }
    let mut panes: Vec<(String, String, String)> = Vec::new();
    if let Ok(out) = Command::new("herdr")
        .args(["--session", session, "pane", "list", "--workspace", workspace_id])
        .output()
    {
        if let Ok(v) = serde_json::from_str::<Value>(&String::from_utf8_lossy(&out.stdout)) {
            collect_pane_info(&v, &mut panes);
        }
    }
    let mut cache = cache.lock().unwrap();
    for (pid, tab_id, pane_label) in &panes {
        let entry = cache
            .entry((session.to_string(), pid.clone()))
            .or_default();
        entry.pane_label = pane_label.clone();
        if let Some(t) = tab_labels.get(tab_id) {
            entry.tab_label = t.clone();
        }
    }
    cache.get(&key).cloned().unwrap_or_default()
}

fn collect_labels(value: &Value, id_field: &str, label_field: &str, out: &mut HashMap<String, String>) {
    match value {
        Value::Object(map) => {
            if let (Some(id), Some(label)) = (
                map.get(id_field).and_then(|v| v.as_str()),
                map.get(label_field).and_then(|v| v.as_str()),
            ) {
                out.insert(id.to_string(), label.to_string());
            }
            for v in map.values() {
                collect_labels(v, id_field, label_field, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_labels(v, id_field, label_field, out);
            }
        }
        _ => {}
    }
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

/// 通过 `herdr workspace list` 把 workspace_id -> 显示名 缓存起来。
/// 失败则回退到原 id（保证弹窗仍能显示）。
fn resolve_workspace(
    session: &str,
    workspace_id: &str,
    cache: &Arc<Mutex<HashMap<String, String>>>,
) -> String {
    if workspace_id.is_empty() {
        return String::new();
    }
    if let Some(name) = cache.lock().unwrap().get(workspace_id).cloned() {
        return name;
    }
    let out = Command::new("herdr")
        .args(["--session", session, "workspace", "list"])
        .output()
        .ok();
    let Some(out) = out else {
        return workspace_id.to_string();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let Ok(root) = serde_json::from_str::<Value>(&text) else {
        return workspace_id.to_string();
    };
    let mut new_entries: Vec<(String, String)> = Vec::new();
    let mut found = workspace_id.to_string();
    walk_workspaces(&root, &mut new_entries, &mut found, workspace_id);
    let mut cache = cache.lock().unwrap();
    for (k, v) in new_entries {
        cache.entry(k).or_insert(v);
    }
    found
}

fn walk_workspaces(
    value: &Value,
    out: &mut Vec<(String, String)>,
    found: &mut String,
    want_id: &str,
) {
    match value {
        Value::Object(map) => {
            let id = map.get("workspace_id").and_then(|v| v.as_str()).map(|s| s.to_string());
            let label = map.get("label").and_then(|v| v.as_str()).map(|s| s.to_string());
            if let (Some(id), Some(label)) = (id, label) {
                out.push((id.clone(), label.clone()));
                if id == want_id && *found == want_id {
                    *found = label;
                }
            }
            for v in map.values() {
                walk_workspaces(v, out, found, want_id);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                walk_workspaces(v, out, found, want_id);
            }
        }
        _ => {}
    }
}

/// 抓取该 pane 最近输出的第一句话，截断后放到弹窗里。
/// 失败（agent 已退出等）时返回空串，弹窗仍可只显示 agent + workspace。
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

fn is_input_or_done(trimmed: &str) -> bool {
    // claude TUI 输入行（`> xxx`）和完成行（`* Baked for 3s · done 22:07`）。
    trimmed.starts_with('>') || trimmed.starts_with("* ") || trimmed.starts_with("✽ ") || trimmed.starts_with("✻ ")
}

fn strip_ansi(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0x1b {
            // 跳过直到字母结尾的 CSI 序列
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

/// 空白（含多个换行空格）压成单个空格，便于单行展示。
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    out.push_str("…");
    out
}

fn is_prompt(trimmed: &str) -> bool {
    trimmed.ends_with('>')
        || trimmed.ends_with('$')
        || trimmed.ends_with('#')
        || trimmed.starts_with('(')
        || trimmed.len() < 3
}

fn set_dpi_aware() {
    // 失败可忽略（非致命）。
    let _ = unsafe {
        windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        )
    };
}
