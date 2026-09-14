// gui.rs -- one Win32 popup per process, no shared state.
//
// `run_popup` creates a single top-level rounded-pill window at the
// right-bottom of the cursor's monitor (stacked above other popups from
// this same plugin if any are visible). It runs a small message pump until
// the window is destroyed, then returns.

use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;
use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint,
    EnumDisplayMonitors, FillRect, HDC, HMONITOR, HGDIOBJ, MONITORINFO,
    MonitorFromPoint, MonitorFromWindow, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST,
    InvalidateRect, SetBkMode, SetTextColor, SetWindowRgn,
    DT_CENTER, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    EnumWindows, GetAncestor, GetClassNameW, GetClientRect, GetCursorPos, GetForegroundWindow,
    GetWindowLongPtrW, GetWindowTextW, IsIconic, IsWindowVisible, LoadCursorW,
    MSG, PeekMessageW, WindowFromPoint,
    RegisterClassW, SetForegroundWindow, SetTimer, KillTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    TranslateMessage, CREATESTRUCTW, GWLP_USERDATA, HWND_TOPMOST, IDC_ARROW, PM_REMOVE, SW_RESTORE,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_ERASEBKGND,
    GA_ROOT,
    WM_LBUTTONUP, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_QUIT, WM_TIMER, WNDCLASSW, WNDPROC,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

const WM_POPUP: u32 = WM_APP + 1;
const POPUP_W: i32 = 300;
const POPUP_H: i32 = 82;
const MARGIN: i32 = 20;
const GAP: i32 = 8;
const ROUND: i32 = 20;
const CLOSE_W: i32 = 42;
// Ultimate safety-net hard lifetime: even an inactive user gets the popup
// cleaned up eventually (e.g. if the system goes to sleep and the input
// timers never tick). 30 minutes is long enough that a user reading/watching
// video or simply AFK won't notice it; short enough to bound resource use.
const AUTO_DISMISS_MS: u32 = 30 * 60 * 1000;
// After the user has been detected active (any input in the last
// ACTIVE_INPUT_MS), downgrade the dismiss timer to this so the popup
// leaves quickly once the user is around.
const ACTIVE_DISMISS_MS: u32 = 10000;
// And if the user is typing right inside the originating pane, dismiss
// even faster — they can already see the answer inline.
const FAST_DISMISS_MS: u32 = 1000;
const FOLLOW_POLL_MS: u32 = 500;
// Suppress only when the user has been actively typing within this window
// in the event-source herdr. Keep it short so a long agent run that
// finishes while the user is reading the screen still pops up.
const ACTIVE_INPUT_MS: u32 = 2000;
const ID_TIMER_DISMISS: usize = 1;
const ID_TIMER_FOLLOW: usize = 2;
const CLASS_NAME: &str = "HerdrDonePopup";
const CONTROLLER_CLASS: &str = "HerdrDonePopupCtrl";

#[allow(dead_code)]
const ID_OPEN: usize = 1001;
#[allow(dead_code)]
const ID_IGNORE: usize = 1002;

#[derive(Clone, Debug, Default)]
pub struct PaneMeta {
    pub tab_id: String,
    pub tab_label: String,
    pub pane_label: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PopupInfo {
    pub session: String,
    pub agent: String,
    pub pane: String,
    pub workspace: String,
    pub tab_id: String,
    pub snippet: String,
    /// true = agent 处于 blocked（提问/等批准），需要用户回应
    pub blocked: bool,
    /// Pre-computed suppression decision, set by the daemon when fanning
    /// out a popup to multiple monitors so every monitor thread agrees.
    /// Not serialized over the wire.
    #[serde(skip)]
    pub pre_decision: Option<Decision>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    Suppress,
    Permanent,
}

struct PopupState {
    info: PopupInfo,
    /// True once we've observed user activity and downgraded the dismiss
    /// timer to ACTIVE_DISMISS_MS. Prevents flapping between active/inactive
    /// dismissal timelines.
    downgraded: bool,
    /// True once the user has been observed typing inside the originating
    /// pane; downgrade further to FAST_DISMISS_MS.
    fast_path: bool,
}

/// Commands sent from the daemon to a popup message-pump thread.
#[derive(Debug)]
pub enum PopupCmd {
    /// Replace the popup's content with new info. Used when a fresh
    /// event arrives for the same pane -- we update in place instead
    /// of stacking a new window.
    Update(PopupInfo),
    /// Tear down the popup window. Used when a different pane's event
    /// arrives while one is already showing.
    Dismiss,
}

/// Owns popup window's message-pump thread(s). The daemon uses this to
/// issue Update / Dismiss commands and to detect when the thread(s)
/// finish. For single-popup launches, `joins` has one entry; for
/// per-monitor launches it has one per monitor. We track every thread
/// so we can join (or detach) them all -- otherwise the untracked ones
/// keep their handles open and risk leaking resources.
pub struct PopupHandle {
    info: Arc<Mutex<PopupInfo>>,
    cmd_tx: mpsc::Sender<PopupCmd>,
    joins: Vec<Option<thread::JoinHandle<()>>>,
    finished: Arc<AtomicBool>,
}

impl PopupHandle {
    pub fn launch(initial: PopupInfo) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<PopupCmd>();
        let info = Arc::new(Mutex::new(initial.clone()));
        let info_clone = info.clone();
        let finished = Arc::new(AtomicBool::new(false));
        let finished_clone = finished.clone();
        let cmd_rx = Arc::new(std::sync::Mutex::new(cmd_rx));
        let join = thread::Builder::new()
            .name("herdr-popup-pump".into())
            .spawn(move || unsafe {
                run_popup_message_pump(initial, info_clone, cmd_rx, finished_clone, None)
            })
            .expect("spawn popup thread");
        PopupHandle {
            info,
            cmd_tx,
            joins: vec![Some(join)],
            finished,
        }
    }

    /// Spawn one popup on every connected monitor. Returns a handle that
    /// owns all the per-monitor threads. Send to this handle to broadcast
    /// the same command (Update / Dismiss) to every monitor's popup.
    pub fn launch_all_monitors(mut initial: PopupInfo) -> Self {
        // Compute the decision ONCE here, before fanning out, so all
        // monitor threads agree. If we let each thread decide, any one
        // of them seeing "user typing in this pane" would mark the
        // shared `finished` flag and the daemon would treat the whole
        // chain as dead.
        let decision = unsafe { decide_mode(&initial) };
        log(&format!(
            "popup session={} pane={} tab={} -> {:?}",
            initial.session, initial.pane, initial.tab_id, decision
        ));
        if matches!(decision, Decision::Suppress) {
            let finished = Arc::new(AtomicBool::new(true));
            return PopupHandle {
                info: Arc::new(Mutex::new(initial)),
                cmd_tx: mpsc::channel::<PopupCmd>().0,
                joins: Vec::new(),
                finished,
            };
        }
        initial.pre_decision = Some(decision);

        let (cmd_tx, cmd_rx) = mpsc::channel::<PopupCmd>();
        let info = Arc::new(Mutex::new(initial.clone()));
        let info_clone = info.clone();
        let finished = Arc::new(AtomicBool::new(false));
        let finished_clone = finished.clone();
        // Wrap the receiver in Arc<Mutex<_>> so every spawned thread can
        // share the same channel (each thread holds its own MutexGuard).
        let cmd_rx = Arc::new(std::sync::Mutex::new(cmd_rx));
        let monitors = unsafe { all_monitors() };
        let monitor_count = monitors.len();
        log(&format!("launch_all_monitors: spawning on {} monitor(s)", monitor_count));
        let mut joins = Vec::with_capacity(monitor_count);
        for (i, m) in monitors.into_iter().enumerate() {
            let info_t = info_clone.clone();
            let finished_t = finished_clone.clone();
            let cmd_rx_t = cmd_rx.clone();
            let info_init = initial.clone();
            // Pack HMONITOR into a usize for the Send boundary.
            let m_usize = m.0 as usize;
            let join = thread::Builder::new()
                .name("herdr-popup-pump".into())
                .spawn(move || unsafe {
                    run_popup_message_pump(
                        info_init,
                        info_t,
                        cmd_rx_t,
                        finished_t,
                        Some((HMONITOR(m_usize as *mut _), i as i32)),
                    )
                })
                .expect("spawn popup thread");
            joins.push(Some(join));
        }
        PopupHandle {
            info,
            cmd_tx,
            joins,
            finished,
        }
    }

    pub fn send(&self, cmd: PopupCmd) -> Result<(), mpsc::SendError<PopupCmd>> {
        self.cmd_tx.send(cmd)
    }

    pub fn info(&self) -> &Arc<Mutex<PopupInfo>> {
        &self.info
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn is_alive(&self) -> bool {
        !self.is_finished()
    }

    pub fn join_timeout(&mut self, _timeout: std::time::Duration) {
        // std::thread::JoinHandle has no join-with-timeout, and even a
        // tiny unfinished popup thread (e.g. one whose SetWindowPos
        // topmost loop is mid-iteration) would block the daemon's main
        // control loop forever if we called .join() directly.
        //
        // Strategy: if the shared `finished` flag is set we know all
        // threads have already (or are about to) return; take each
        // JoinHandle and call join(), which will return immediately.
        //
        // If `finished` is NOT set (one or more threads are stuck),
        // leak the JoinHandles by calling std::mem::forget. The OS
        // thread is still alive and will exit on its own once it
        // processes a future Dismiss or hits a WatchdogFor. The
        // daemon's main loop is unblocked.
        let finished = self.finished.load(std::sync::atomic::Ordering::SeqCst);
        if finished {
            for slot in self.joins.drain(..) {
                if let Some(j) = slot {
                    let _ = j.join();
                }
            }
        } else {
            // Move the JoinHandles out without joining. They become
            // detached threads -- Rust will not free their stack until
            // the thread itself exits. The daemon's daemon.rs main
            // loop continues immediately.
            for slot in self.joins.drain(..) {
                if let Some(j) = slot {
                    std::mem::forget(j);
                }
            }
        }
    }
}

/// Public entry for one-shot (non-daemon) invocations. Kept for backward
/// compatibility with `popup-stdin` callers (deprecated; the daemon path
/// uses `run_popup_message_pump` directly).
pub fn run_popup(info: PopupInfo) {
    unsafe {
        // Register classes once per process.
        let _ = register_class(CLASS_NAME, Some(popup_proc));
        let _ = register_class(CONTROLLER_CLASS, Some(controller_proc));
        let decision = decide_mode(&info);
        log(&format!(
            "popup session={} pane={} tab={} -> {:?}",
            info.session, info.pane, info.tab_id, decision
        ));
        match decision {
            Decision::Suppress => return,
            Decision::Permanent => create_popup_oneshot(info),
        }
    }
}

/* =============================== decision =============================== */

unsafe fn decide_mode(info: &PopupInfo) -> Decision {
    let fg_herdr = foreground_is_herdr();
    let title = foreground_title();
    let parsed_session = if fg_herdr {
        parse_session_from_title(&title.to_lowercase())
    } else {
        None
    };
    // Where is the user's attention? Three signals:
    //   * cursor_session: top-level herdr window the mouse cursor is hovering.
    //                    Most reliable "what is the user looking at".
    //   * fg_session: z-order foreground herdr's session (= last keyboard
    //                 activity, since typing forces a window to foreground).
    //   * fg_workspace: workspace_id of the foreground herdr's focused pane.
    //                  Needed because the user can be in the same herdr
    //                  session but a different workspace/tab than the event.
    let cursor_session = parse_session_from_title(&cursor_window_title_lc());
    let fg_session = parsed_session.as_deref();
    let fg_workspace = if let Some(s) = fg_session {
        current_focus_workspace_id(s)
    } else {
        None
    };
    let event_workspace = info
        .pane
        .split(':')
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let input_age_ms = last_input_age_ms();
    let active = input_age_ms < ACTIVE_INPUT_MS;
    log(&format!(
        "  inputs: fg_herdr={} fg_title={:?} fg_session={:?} cursor_session={:?} fg_workspace={:?} event_session={} event_workspace={:?} input_age_ms={} active={}",
        fg_herdr, title, fg_session, cursor_session, fg_workspace, info.session, event_workspace, input_age_ms, active
    ));

    // Suppress ONLY when ALL of these hold:
    //   - user is in the event-source session (cursor OR fg session matches)
    //   - user's focused workspace matches the event's workspace
    //   - user is actively typing right now (last 2s)
    // Otherwise the popup shows, so e.g. finishing in workspace wY while the
    // user is working in workspace wX of the same session still notifies.
    let user_in_event_session = cursor_session.as_deref()
        == Some(info.session.to_lowercase().as_str())
        || fg_session == Some(info.session.to_lowercase().as_str());
    let workspace_matches = match (&fg_workspace, &event_workspace) {
        (Some(f), Some(e)) => f == e,
        // Can't determine -> don't suppress just on this axis.
        _ => true,
    };
    if user_in_event_session && workspace_matches && active {
        Decision::Suppress
    } else {
        // Permanent; auto-dismiss as soon as the user starts typing anywhere.
        Decision::Permanent
    }
}

/// Title of the top-level herdr-style window under the mouse cursor, lowercased.
/// Returns "" if the cursor isn't over a herdr-style window or we can't tell.
unsafe fn cursor_window_title_lc() -> String {
    let mut pt = POINT::default();
    if GetCursorPos(&mut pt).is_err() {
        return String::new();
    }
    let hit = WindowFromPoint(pt);
    if hit.0.is_null() {
        return String::new();
    }
    // Walk up to the top-level (so child panes of a terminal all collapse to
    // the same herdr window).
    let top = GetAncestor(hit, GA_ROOT);
    let h = if top.0.is_null() { hit } else { top };
    let mut buf = [0u16; 512];
    let n = GetWindowTextW(h, &mut buf) as usize;
    String::from_utf16_lossy(&buf[..n]).to_string()
}

unsafe fn foreground_is_herdr() -> bool {
    let title = foreground_title();
    title.to_lowercase().contains("herdr")
}

unsafe fn foreground_title() -> String {
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return String::new();
    }
    let mut buf = [0u16; 512];
    let n = GetWindowTextW(fg, &mut buf) as usize;
    String::from_utf16_lossy(&buf[..n]).to_string()
}

fn parse_session_from_title(title_lc: &str) -> Option<String> {
    if !title_lc.contains("herdr") {
        return None;
    }
    if let Some(idx) = title_lc.find("--session") {
        let rest = &title_lc[idx + "--session".len()..];
        let token = rest
            .trim_start()
            .split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .next()
            .unwrap_or("");
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    Some("default".to_string())
}

#[allow(dead_code)]
unsafe fn focused_tab_in(session: &str, workspace_id: &str) -> Option<String> {
    let out = Command::new("herdr")
        .args([
            "--session", session, "tab", "list", "--workspace", workspace_id,
        ])
        .output()
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).ok()?;
    find_focused_tab_id(&v)
}

#[allow(dead_code)]
fn find_focused_tab_id(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Object(map) => {
            let id = map.get("tab_id").and_then(|s| s.as_str());
            let focused = map.get("focused").and_then(|s| s.as_bool()).unwrap_or(false);
            if focused && id.is_some() {
                return id.map(|s| s.to_string());
            }
            for val in map.values() {
                if let Some(f) = find_focused_tab_id(val) {
                    return Some(f);
                }
            }
            None
        }
        serde_json::Value::Array(arr) => {
            for val in arr {
                if let Some(f) = find_focused_tab_id(val) {
                    return Some(f);
                }
            }
            None
        }
        _ => None,
    }
}

/// Returns the pane_id that the foreground herdr UI is currently focused on.
///
/// IMPORTANT: must not use bare `pane current --current`. Without `--session`
/// the herdr CLI returns the pane that owns the calling shell, which is
/// the EVENT-source pane (the one we're showing the popup for). That
/// caused the popup to immediately dismiss itself in the same
/// "session, different workspace" case the user reported: the plugin
/// process is itself the shell for the event pane, so `pane current
/// --current` keeps matching `info.pane` on every follow tick.
///
/// We instead query `pane list --workspace <event_workspace>` and pick
/// the pane whose `focused == true`. That's the user's actual UI focus
/// inside the workspace the completion happened in — and it'll be None
/// if the user is in a different workspace of the same session, exactly
/// the case where we want the popup to keep showing.
unsafe fn current_pane_id(event_workspace: Option<&str>) -> Option<String> {
    let mut cmd = Command::new("herdr");
    cmd.arg("pane").arg("list");
    if let Some(ws) = event_workspace {
        if !ws.is_empty() {
            cmd.arg("--workspace").arg(ws);
        }
    }
    let out = cmd.output().ok()?;
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).ok()?;
    let arr = v.pointer("/result/panes")?.as_array()?;
    for p in arr {
        let focused = p.get("focused").and_then(|f| f.as_bool()).unwrap_or(false);
        if focused {
            return p
                .get("pane_id")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string());
        }
    }
    None
}

/// workspace_id of the foreground herdr's currently focused workspace
/// (e.g. "wX"). Reads `herdr --session <session> workspace list` and
/// picks the workspace whose `focused == true`.
///
/// `pane current --current` is unreliable for this because it returns the
/// pane that owns the calling shell, which is the event-source pane, not
/// the UI-focused pane. `workspace list` reports the UI focus state.
unsafe fn current_focus_workspace_id(session: &str) -> Option<String> {
    let out = Command::new("herdr")
        .args(["--session", session, "workspace", "list"])
        .output()
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).ok()?;
    let arr = v.pointer("/result/workspaces")?.as_array()?;
    for w in arr {
        let focused = w.get("focused").and_then(|f| f.as_bool()).unwrap_or(false);
        if focused {
            return w
                .get("workspace_id")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string());
        }
    }
    None
}

unsafe fn last_input_age_ms() -> u32 {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    if !GetLastInputInfo(&mut info).as_bool() {
        return u32::MAX;
    }
    let now = GetTickCount();
    now.wrapping_sub(info.dwTime)
}

unsafe fn user_moved_into_tab(info: &PopupInfo) -> bool {
    if !foreground_is_herdr() {
        return false;
    }
    let parsed = match parse_session_from_title(&foreground_title().to_lowercase()) {
        Some(s) => s,
        None => return false,
    };
    if parsed != info.session.to_lowercase() {
        return false;
    }
    if info.pane.is_empty() {
        return false;
    }
    // The popup should dismiss the moment the user's UI focus enters the
    // originating pane. Scope the lookup to the event's workspace so a
    // different-workspace focus doesn't accidentally match.
    let event_ws = info.pane.split(':').next().filter(|s| !s.is_empty());
    current_pane_id(event_ws).as_deref() == Some(info.pane.as_str())
}

/* =============================== popup window =============================== */

unsafe fn create_popup_oneshot(info: PopupInfo) {
    let (x, y) = compute_popup_position(&info, count_visible_popups() as i32);
    let state = Box::new(PopupState {
        info,
        downgraded: false,
        fast_path: false,
    });
    let ptr = Box::into_raw(state);
    let hwnd = create_popup_window(ptr as *const std::ffi::c_void, x, y);
    if hwnd.0.is_null() {
        drop(Box::from_raw(ptr));
        return;
    }
    // Run a message loop until the window is destroyed (one-shot path).
    let mut msg = MSG::default();
    loop {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                return;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            if msg.hwnd == hwnd && msg.message == WM_NCDESTROY {
                return;
            }
        }
        thread::sleep(Duration::from_millis(30));
    }
}

/// Compute (x, y) for a popup anchored to the bottom-right of the given
/// monitor's work area. Used when the caller already knows which monitor
/// to target (e.g. spawn one popup per connected monitor).
unsafe fn position_for_monitor(monitor: HMONITOR, slot: i32) -> Option<(i32, i32)> {
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !GetMonitorInfoW(monitor, &mut mi).as_bool() {
        return None;
    }
    let r = mi.rcWork;
    let x = r.right - POPUP_W - MARGIN;
    let y = r.bottom - POPUP_H - MARGIN - (POPUP_H + GAP) * slot;
    log(&format!(
        "popup monitor: src=per-monitor slot={} work=({},{},{},{}) pos=({},{})",
        slot, r.left, r.top, r.right, r.bottom, x, y
    ));
    Some((x, y))
}

/// Pick the monitor and (x, y) for a popup anchored to the bottom-right
/// of the monitor where the cursor currently lives. We use the cursor
/// instead of trying to locate the event-source herdr window because the
/// event may come from a different session whose host window is on a
/// monitor the user is no longer watching. The cursor position always
/// tracks where the user is looking.
unsafe fn compute_popup_position(_info: &PopupInfo, slot: i32) -> (i32, i32) {
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let _ = GetMonitorInfoW(monitor, &mut mi);
    let r = mi.rcWork;
    let x = r.right - POPUP_W - MARGIN;
    let y = r.bottom - POPUP_H - MARGIN - (POPUP_H + GAP) * slot;
    log(&format!(
        "popup monitor: src=cursor work=({},{},{},{}) pos=({},{})",
        r.left, r.top, r.right, r.bottom, x, y
    ));
    (x, y)
}

/// Enumerate every connected monitor and yield its HMONITOR.
/// (kept for possible future per-monitor popup fan-out; unused today)
#[allow(dead_code)]
unsafe fn all_monitors() -> Vec<HMONITOR> {
    let mut out: Vec<HMONITOR> = Vec::new();
    struct Ctx(*mut Vec<HMONITOR>);
    extern "system" fn enum_proc(hmonitor: HMONITOR, _: HDC, _: *mut RECT, l: LPARAM) -> BOOL {
        unsafe {
            let ctx = &mut *(l.0 as *mut Ctx);
            (*ctx.0).push(hmonitor);
            BOOL(1)
        }
    }
    let ctx = Ctx(&mut out as *mut Vec<HMONITOR>);
    let _ = EnumDisplayMonitors(None, None, Some(enum_proc), LPARAM(&ctx as *const _ as isize));
    out
}

/// Create the popup window and return its HWND. Caller is responsible
/// for freeing `create_params` (a `*mut PopupState`) on WM_NCDESTROY.
/// Returns HWND(0) on failure (caller still owns `create_params`).
unsafe fn create_popup_window(
    create_params: *const std::ffi::c_void,
    x: i32,
    y: i32,
) -> HWND {
    let class_name = wide(CLASS_NAME);
    let title_name = wide(CLASS_NAME);
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        PCWSTR(class_name.as_ptr()),
        PCWSTR(title_name.as_ptr()),
        WS_POPUP | WS_VISIBLE,
        x,
        y,
        POPUP_W,
        POPUP_H,
        None,
        None,
        module_instance(),
        Some(create_params),
    );
    let hwnd = match hwnd {
        Ok(h) => h,
        Err(_) => {
            log("popup: CreateWindowExW failed");
            return HWND(std::ptr::null_mut());
        }
    };
    log(&format!(
        "popup window created: hwnd={:?} visible={}",
        hwnd.0,
        IsWindowVisible(hwnd).as_bool()
    ));
    let rgn = CreateRoundRectRgn(0, 0, POPUP_W, POPUP_H, ROUND, ROUND);
    let _ = SetWindowRgn(hwnd, Some(rgn), true);
    // Force popup on top + grab focus. Use NOACTIVATE so we don't steal
    // focus from the user's terminal -- that would hide the popup behind
    // the terminal on the next focus restore.
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW | SWP_NOACTIVATE,
    );
    let _ = BringWindowToTop(hwnd);
    // Skip SetForegroundWindow: it steals focus from the terminal, and
    // when the terminal later regains focus the popup gets pushed behind
    // it. We want the popup to stay visible on top WITHOUT taking focus.
    hwnd
}

/// Run the popup message-pump on the current thread. Initial state comes
/// from the caller's `info`; the popup's HWND_USERDATA points to a
/// heap-allocated `PopupState` that is freed when the window is destroyed.
///
/// `cmd_rx` lets the daemon push `Update` / `Dismiss` commands between
/// PeekMessageW iterations. `finished` is set to true just before the
/// function returns so the daemon's main loop can observe thread
/// completion promptly.
pub unsafe fn run_popup_message_pump(
    info: PopupInfo,
    info_arc: Arc<Mutex<PopupInfo>>,
    cmd_rx: Arc<std::sync::Mutex<mpsc::Receiver<PopupCmd>>>,
    finished: Arc<AtomicBool>,
    on_monitor: Option<(HMONITOR, i32)>,
) {
    // Register classes once per process. Idempotent: returns 0 on the
    // second call inside the same process, which we ignore.
    let _ = register_class(CLASS_NAME, Some(popup_proc));
    let _ = register_class(CONTROLLER_CLASS, Some(controller_proc));
    // If we are part of a multi-monitor launch, honour a pre-computed
    // decision from the daemon. Otherwise decide here.
    let decision = if let Some(d) = info.pre_decision.clone() {
        d
    } else {
        decide_mode(&info)
    };
    log(&format!(
        "popup session={} pane={} tab={} -> {:?}",
        info.session, info.pane, info.tab_id, decision
    ));
    if matches!(decision, Decision::Suppress) {
        finished.store(true, std::sync::atomic::Ordering::SeqCst);
        return;
    }

    let state = Box::new(PopupState {
        info: info.clone(),
        downgraded: false,
        fast_path: false,
    });
    let ptr = Box::into_raw(state);
    // Pick position based on the bound monitor if given, otherwise
    // fall back to cursor monitor.
    let (x, y) = if let Some((m, slot)) = on_monitor {
        position_for_monitor(m, slot).unwrap_or_else(|| compute_popup_position(&info, 0))
    } else {
        compute_popup_position(&info, 0)
    };
    let hwnd = create_popup_window(ptr as *const std::ffi::c_void, x, y);
    if hwnd.0.is_null() {
        drop(Box::from_raw(ptr));
        finished.store(true, std::sync::atomic::Ordering::SeqCst);
        return;
    }
    // Default hard lifetime is AUTO_DISMISS_MS (30 minutes).
    let _ = SetTimer(Some(hwnd), ID_TIMER_DISMISS, AUTO_DISMISS_MS, None);
    let _ = SetTimer(Some(hwnd), ID_TIMER_FOLLOW, FOLLOW_POLL_MS, None);

    let mut msg = MSG::default();
    let mut topmost_tick: u32 = 0;
    loop {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                let _ = DestroyWindow(hwnd);
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            if msg.hwnd == hwnd && msg.message == WM_NCDESTROY {
                finished.store(true, std::sync::atomic::Ordering::SeqCst);
                return;
            }
        }
        // Periodically re-assert topmost so a maximized Warp/Chrome on
        // top of us doesn't permanently bury the popup. Cheap (every 1s)
        // and keeps the popup visible on every monitor we launched it on.
        topmost_tick = topmost_tick.wrapping_add(1);
        if topmost_tick % 33 == 0 {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW | SWP_NOACTIVATE,
            );
        }
        // Drain any pending daemon command non-blockingly.
        let recv_result = cmd_rx.lock().ok().map(|mut r| r.try_recv());
        match recv_result {
            Some(Ok(PopupCmd::Update(new_info))) => {
                if let Some(state) = (ptr as *mut PopupState).as_mut() {
                    state.info = new_info.clone();
                    state.downgraded = false;
                    state.fast_path = false;
                }
                if let Ok(mut guard) = info_arc.lock() {
                    *guard = new_info;
                }
                let _ = KillTimer(Some(hwnd), ID_TIMER_DISMISS);
                let _ = KillTimer(Some(hwnd), ID_TIMER_FOLLOW);
                let _ = SetTimer(Some(hwnd), ID_TIMER_DISMISS, AUTO_DISMISS_MS, None);
                let _ = SetTimer(Some(hwnd), ID_TIMER_FOLLOW, FOLLOW_POLL_MS, None);
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
            Some(Ok(PopupCmd::Dismiss)) => {
                let _ = DestroyWindow(hwnd);
            }
            Some(Err(mpsc::TryRecvError::Empty)) => {}
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                let _ = DestroyWindow(hwnd);
            }
            None => {
                // Lock poisoned; just continue.
            }
        }
        thread::sleep(Duration::from_millis(30));
    }
}

unsafe fn count_visible_popups() -> u32 {
    let mut count: u32 = 0;
    extern "system" fn enum_proc(hwnd: HWND, l: LPARAM) -> BOOL {
        unsafe {
            let counter = &mut *(l.0 as *mut u32);
            let mut buf = [0u16; 64];
            let n = GetClassNameW(hwnd, &mut buf) as usize;
            let class = String::from_utf16_lossy(&buf[..n]);
            if class == CLASS_NAME && IsWindowVisible(hwnd).as_bool() {
                *counter += 1;
            }
            BOOL(1)
        }
    }
    let _ = EnumWindows(Some(enum_proc), LPARAM(&mut count as *mut _ as isize));
    count
}

/// Pick a monitor by finding a top-level herdr window whose title
/// indicates it belongs to the given session, then returning the monitor
/// that window lives on. We do this so a popup for an agent in dse lands
/// on the same screen the user actually has dse open on, even if the
/// cursor is currently on another monitor (e.g. reading Chrome on
/// DISPLAY2 while dse is on DISPLAY1).
///
/// Skip minimized (IsIconic) windows: Windows parks them at (-32000,-32000)
/// and `MonitorFromWindow` on a minimized window returns the monitor the
/// window *was last on*, which can be wrong if the user moved dse between
/// monitors or if the window was minimized from a different display.
#[allow(dead_code)]
unsafe fn event_source_monitor(session: &str) -> Option<HMONITOR> {
    let session_lower = session.to_lowercase();
    let mut found: Option<HWND> = None;
    let mut checked: u32 = 0;
    extern "system" fn enum_proc(hwnd: HWND, l: LPARAM) -> BOOL {
        unsafe {
            let (target, sess, checked) = &mut *(l.0 as *mut (&mut Option<HWND>, String, u32));
            *checked += 1;
            let mut buf = [0u16; 512];
            let n = GetWindowTextW(hwnd, &mut buf) as usize;
            if n == 0 {
                return BOOL(1);
            }
            let title = String::from_utf16_lossy(&buf[..n]).to_lowercase();
            if !title.contains("herdr") {
                return BOOL(1);
            }
            let matches = if sess == "default" {
                !title.contains("--session")
            } else {
                title.contains(&format!("--session {}", sess))
            };
            if matches && IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() {
                *(*target) = Some(hwnd);
                return BOOL(0);
            }
            BOOL(1)
        }
    }
    let mut triple: (&mut Option<HWND>, String, &mut u32) = (&mut found, session_lower, &mut checked);
    let _ = EnumWindows(Some(enum_proc), LPARAM(&mut triple as *mut _ as isize));
    if let Some(h) = found {
        log(&format!("event_source_monitor: matched hwnd={:?}", h.0));
        Some(MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST))
    } else {
        log(&format!(
            "event_source_monitor: no non-minimized herdr window for session (checked={})",
            checked
        ));
        None
    }
}

/* =============================== window proc =============================== */

unsafe extern "system" fn popup_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = &*(l.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            LRESULT(1)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint_popup(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = (l.0 & 0xFFFF) as i32;
            let y = ((l.0 >> 16) & 0xFFFF) as i32;
            if x >= POPUP_W - CLOSE_W && y < 34 {
                let _ = DestroyWindow(hwnd);
            } else {
                open_target(hwnd);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            let id = w.0 as usize;
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PopupState;
            if ptr.is_null() {
                return LRESULT(0);
            }
            match id {
                ID_TIMER_DISMISS => {
                    // Every popup has a hard 10s lifetime, set in create_popup.
                    let _ = DestroyWindow(hwnd);
                }
                ID_TIMER_FOLLOW => {
                    let state = &mut *ptr;
                    // Fast path: user is typing inside the originating
                    // pane — they can see the answer inline, dismiss
                    // quickly. Sticky so we don't re-downgrade.
                    if !state.fast_path && user_moved_into_tab(&state.info) {
                        let active = last_input_age_ms() < ACTIVE_INPUT_MS;
                        if active {
                            log("follow: typing in event pane -> fast dismiss");
                            state.fast_path = true;
                            // Re-arm the dismiss timer at the fast rate.
                            let _ = KillTimer(Some(hwnd), ID_TIMER_DISMISS);
                            let _ = SetTimer(Some(hwnd), ID_TIMER_DISMISS, FAST_DISMISS_MS, None);
                            return LRESULT(0);
                        }
                    }
                    // Active path: user has been doing *something* recently
                    // (any input — even mouse jiggle on a different
                    // monitor). Downgrade the dismiss timer once.
                    if !state.downgraded && last_input_age_ms() < ACTIVE_INPUT_MS {
                        log("follow: user active -> 10s dismiss");
                        state.downgraded = true;
                        let _ = KillTimer(Some(hwnd), ID_TIMER_DISMISS);
                        let _ = SetTimer(Some(hwnd), ID_TIMER_DISMISS, ACTIVE_DISMISS_MS, None);
                        return LRESULT(0);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PopupState;
            if !ptr.is_null() {
                drop(Box::from_raw(ptr));
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DefWindowProcW(hwnd, msg, w, l)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

unsafe extern "system" fn controller_proc(_hwnd: HWND, msg: u32, _w: WPARAM, _l: LPARAM) -> LRESULT {
    if msg == WM_POPUP {
        // unused: we render inline now
    }
    DefWindowProcW(_hwnd, msg, _w, _l)
}

/* =============================== paint =============================== */

unsafe fn paint_popup(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let _ = GetClientRect(hwnd, &mut rc);

    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PopupState;
    let (agent, workspace, snippet, blocked) = if !ptr.is_null() {
        let info = &(*ptr).info;
        (info.agent.as_str(), info.workspace.as_str(), info.snippet.as_str(), info.blocked)
    } else {
        ("", "", "", false)
    };

    // blocked（等输入）用暗红底色 + 标记，和普通完成区分开。
    let bg_color = if blocked {
        COLORREF(0x002E3BA0) // BGR: 砖红 #A03B2E
    } else {
        color_for_agent(agent)
    };
    let bg = CreateSolidBrush(bg_color);
    FillRect(hdc, &rc, bg);

    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(0x00F4F4EE));

    // Title row
    let mut tr = RECT {
        left: rc.left + 18,
        top: rc.top + 10,
        right: rc.right - CLOSE_W - 6,
        bottom: rc.top + 34,
    };
    let title = if blocked {
        format!("{} · {} · 等待输入", agent, workspace)
    } else {
        format!("{} · {}", agent, workspace)
    };
    let mut titlew = wide(&title);
    titlew.pop();
    let _ = DrawTextW(hdc, &mut titlew, &mut tr, DT_VCENTER | DT_SINGLELINE);

    // Snippet (multi-line)
    if !snippet.is_empty() {
        let mut sr = RECT {
            left: rc.left + 18,
            top: rc.top + 34,
            right: rc.right - 12,
            bottom: rc.bottom - 10,
        };
        let mut sw = wide(snippet);
        sw.pop();
        let _ = DrawTextW(hdc, &mut sw, &mut sr, DT_WORDBREAK);
    }

    // Close mark
    let mut close_rect = RECT {
        left: rc.right - CLOSE_W,
        top: 0,
        right: rc.right,
        bottom: 34,
    };
    let mut closew = wide("×");
    closew.pop();
    let _ = DrawTextW(hdc, &mut closew, &mut close_rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE);

    let _ = EndPaint(hwnd, &ps);
    let _ = DeleteObject(HGDIOBJ(bg.0));
}

/* =============================== click =============================== */

unsafe fn open_target(hwnd: HWND) {
    let (session, pane) = {
        let ptr = GetWindowLongPtrW(hwnd as _, GWLP_USERDATA) as *mut PopupState;
        if ptr.is_null() {
            let _ = DestroyWindow(hwnd);
            return;
        }
        let info = &(*ptr).info;
        (info.session.clone(), info.pane.clone())
    };

    // 1. Mark the agent as focused inside herdr. `agent focus` only updates
    //    herdr's internal state; it does not move any host window.
    let _ = std::process::Command::new("herdr")
        .args(["--session", &session, "agent", "focus", &pane])
        .status();

    // 2. Find the host window (warp / Windows Terminal running this herdr
    //    session) and bring it to the foreground without changing its
    //    minimized/maximized state. The herdr client sets the terminal
    //    title to "herdr-..." (Warp) or the tab name; for named sessions
    //    the warp title additionally contains "--session <name>".
    activate_herdr_host(&session);

    let _ = DestroyWindow(hwnd);
}

/// Bring the herdr host window to the foreground for the given session.
/// `default` sessions are identified by titles that contain "herdr" but
/// NOT "--session" (the warp window for a default herdr is titled just
/// "herdr" or "◑ herdr-..."). Named sessions contain both "herdr" and
/// "--session <name>".
unsafe fn activate_herdr_host(session: &str) {
    let must_have_session = session != "default";
    let required_token = if must_have_session {
        format!("--session {}", session)
    } else {
        String::new()
    };
    let mut target: HWND = HWND(std::ptr::null_mut());
    extern "system" fn enum_proc(hwnd: HWND, l: LPARAM) -> BOOL {
        unsafe {
            let data = &mut *(l.0 as *mut (String, bool, HWND));
            let (ref required_token, must_have_session, ref mut target) = *data;
            // Skip our own popup.
            let mut class_buf = [0u16; 64];
            let n = GetClassNameW(hwnd, &mut class_buf) as usize;
            let class = String::from_utf16_lossy(&class_buf[..n]);
            if class == "HerdrDonePopup" {
                return BOOL(1);
            }
            if !IsWindowVisible(hwnd).as_bool() {
                return BOOL(1);
            }
            let mut title_buf = [0u16; 512];
            let m = GetWindowTextW(hwnd, &mut title_buf) as usize;
            let title = String::from_utf16_lossy(&title_buf[..m]);
            let title_lc = title.to_lowercase();
            if title_lc.is_empty() || !title_lc.contains("herdr") {
                return BOOL(1);
            }
            if must_have_session {
                if !title_lc.contains(&required_token.to_lowercase()) {
                    return BOOL(1);
                }
            } else {
                // Default session: must NOT contain any "--session" flag.
                if title_lc.contains("--session") {
                    return BOOL(1);
                }
            }
            *target = hwnd;
            BOOL(0) // stop enum
        }
    }
    let mut pair = (required_token, must_have_session, target);
    let _ = EnumWindows(Some(enum_proc), LPARAM(&mut pair as *mut _ as isize));
    target = pair.2;
    if !target.0.is_null() {
        // 只在最小化时还原，maximized / normal 不动。SW_RESTORE 对
        // maximized 窗口会强行改为 normal 尺寸，这会违反"保持原状"。
        if IsIconic(target).as_bool() {
            let _ = ShowWindow(target, SW_RESTORE);
        }
        let _ = BringWindowToTop(target);
        let _ = SetForegroundWindow(target);
    }
}

/* =============================== helpers =============================== */

unsafe fn register_class(name: &str, proc: WNDPROC) -> u16 {
    let instance = HINSTANCE(GetModuleHandleW(None).unwrap_or_default().0);
    let class = wide(name);
    let wc = WNDCLASSW {
        lpfnWndProc: proc,
        hInstance: instance,
        lpszClassName: PCWSTR(class.as_ptr()),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        ..Default::default()
    };
    RegisterClassW(&wc)
}

fn color_for_agent(agent: &str) -> COLORREF {
    let (r, g, b) = match agent.to_lowercase().as_str() {
        "claude" => (0xE0, 0x8A, 0x3C),
        "codex" => (0x49, 0x86, 0xE8),
        "opencode" => (0x3F, 0xA8, 0x73),
        "grok" => (0x5C, 0x5C, 0x5C),
        _ => (0x2D, 0x8C, 0x7C),
    };
    COLORREF(((b as u32) << 16) | ((g as u32) << 8) | (r as u32))
}

fn module_instance() -> Option<HINSTANCE> {
    unsafe {
        GetModuleHandleW(None)
            .ok()
            .map(|module| HINSTANCE(module.0))
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn log(msg: &str) {
    if let Some(home) = std::env::var_os("USERPROFILE") {
        let path = std::path::PathBuf::from(home)
            .join("AppData")
            .join("Local")
            .join("Temp")
            .join("herdr-done-popup.log");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            use std::io::Write;
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = writeln!(f, "[{ts}] {msg}");
            let _ = f.flush();
        }
    }
}