// gui.rs -- one Win32 popup per process, no shared state.
//
// `run_popup` creates a single top-level rounded-pill window at the
// right-bottom of the cursor's monitor (stacked above other popups from
// this same plugin if any are visible). It runs a small message pump until
// the window is destroyed, then returns.

use std::process::Command;
use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint,
    FillRect, SetBkMode, SetTextColor, SetWindowRgn, HGDIOBJ, MONITORINFO,
    MonitorFromPoint, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, DT_CENTER, DT_SINGLELINE,
    DT_VCENTER, DT_WORDBREAK, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    EnumWindows, GetClassNameW, GetClientRect, GetCursorPos, GetForegroundWindow,
    GetWindowLongPtrW, GetWindowTextW, IsWindowVisible, LoadCursorW, MSG, PeekMessageW,
    RegisterClassW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    TranslateMessage, CREATESTRUCTW, GWLP_USERDATA, HWND_TOPMOST, IDC_ARROW, PM_REMOVE, SW_RESTORE,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_ERASEBKGND,
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
const AUTO_DISMISS_MS: u32 = 10000;
const FOLLOW_POLL_MS: u32 = 500;
const RECENT_INPUT_MS: u32 = 5000;
const ID_TIMER_DISMISS: usize = 1;
const ID_TIMER_FOLLOW: usize = 2;
const CLASS_NAME: &str = "HerdrDonePopup";
const CONTROLLER_CLASS: &str = "HerdrDonePopupCtrl";

const ID_OPEN: usize = 1001;
const ID_IGNORE: usize = 1002;

#[derive(Clone, Debug, Default)]
pub struct PaneMeta {
    pub tab_id: String,
    pub tab_label: String,
    pub pane_label: String,
}

#[derive(Clone, Debug)]
pub struct PopupInfo {
    pub session: String,
    pub agent: String,
    pub pane: String,
    pub workspace: String,
    pub tab_id: String,
    pub snippet: String,
    /// true = agent 处于 blocked（提问/等批准），需要用户回应
    pub blocked: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Decision {
    Suppress,
    Auto10s,
    Permanent,
}

struct PopupState {
    info: PopupInfo,
    auto_dismiss: bool,
}

/// Public entry: build the popup, run its message loop, return when destroyed.
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
            Decision::Auto10s => create_popup(info, true),
            Decision::Permanent => create_popup(info, false),
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
    let same_session = parsed_session.as_deref() == Some(info.session.to_lowercase().as_str());
    let workspace_id = info.pane.split(':').next().unwrap_or("").to_string();
    let focused_tab = if same_session {
        focused_tab_in(&info.session, &workspace_id)
    } else {
        None
    };
    let recent = last_input_recent();
    log(&format!(
        "  inputs: fg_herdr={} fg_title={:?} parsed_session={:?} same_session={} focused_tab={:?} recent={}",
        fg_herdr, title, parsed_session, same_session, focused_tab, recent
    ));
    if fg_herdr
        && same_session
        && !info.tab_id.is_empty()
        && focused_tab.as_deref() == Some(info.tab_id.as_str())
    {
        Decision::Suppress
    } else if recent {
        Decision::Auto10s
    } else {
        Decision::Permanent
    }
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

unsafe fn last_input_recent() -> bool {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    if !GetLastInputInfo(&mut info).as_bool() {
        return false;
    }
    let now = GetTickCount();
    now.wrapping_sub(info.dwTime) < RECENT_INPUT_MS
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
    if info.tab_id.is_empty() {
        return false;
    }
    let workspace_id = info.pane.split(':').next().unwrap_or("");
    if workspace_id.is_empty() {
        return false;
    }
    match focused_tab_in(&info.session, workspace_id) {
        Some(focused) => focused == info.tab_id,
        None => false,
    }
}

/* =============================== popup window =============================== */

unsafe fn create_popup(info: PopupInfo, auto_dismiss: bool) {
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let _ = GetMonitorInfoW(monitor, &mut mi);
    let r = mi.rcWork;

    // Stack above any other visible popups from this plugin on the same monitor.
    let slot = count_visible_popups();
    let x = r.right - POPUP_W - MARGIN;
    let y = r.bottom - POPUP_H - MARGIN - (POPUP_H + GAP) * slot as i32;

    let class = wide(CLASS_NAME);
    let state = Box::new(PopupState { info, auto_dismiss });
    let ptr = Box::into_raw(state);
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        PCWSTR(class.as_ptr()),
        PCWSTR(class.as_ptr()),
        WS_POPUP | WS_VISIBLE,
        x,
        y,
        POPUP_W,
        POPUP_H,
        None,
        None,
        module_instance(),
        Some(ptr as *const std::ffi::c_void),
    );
    let Ok(hwnd) = hwnd else {
        drop(Box::from_raw(ptr));
        return;
    };
    let rgn = CreateRoundRectRgn(0, 0, POPUP_W, POPUP_H, ROUND, ROUND);
    let _ = SetWindowRgn(hwnd, Some(rgn), true);
    // 强制把 popup 拉到最上层 + 抢焦点，否则浏览器/全屏应用可能盖在它之上。
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
    );
    let _ = BringWindowToTop(hwnd);
    let _ = SetForegroundWindow(hwnd);
    if auto_dismiss {
        let _ = SetTimer(Some(hwnd), ID_TIMER_DISMISS, AUTO_DISMISS_MS, None);
    }
    let _ = SetTimer(Some(hwnd), ID_TIMER_FOLLOW, FOLLOW_POLL_MS, None);

    // Run a message loop until the window is destroyed.
    let mut msg = MSG::default();
    loop {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                return;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            // After dispatching, if our window is gone, exit.
            if msg.hwnd == hwnd && msg.message == WM_NCDESTROY {
                return;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
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
                    if (*ptr).auto_dismiss {
                        let _ = DestroyWindow(hwnd);
                    }
                }
                ID_TIMER_FOLLOW => {
                    let state = &mut *ptr;
                    if user_moved_into_tab(&state.info) {
                        log("follow: user entered tab -> dismiss");
                        let _ = DestroyWindow(hwnd);
                        return LRESULT(0);
                    }
                    if !state.auto_dismiss && last_input_recent() {
                        state.auto_dismiss = true;
                        let _ = SetTimer(Some(hwnd), ID_TIMER_DISMISS, AUTO_DISMISS_MS, None);
                        log("follow: user active -> start 10s dismiss");
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
/// Preserves the window's existing state (no SW_RESTORE).
unsafe fn activate_herdr_host(session: &str) {
    let needle_session = if session == "default" {
        // Default herdr has no "--session" flag in the title, so just look
        // for any visible window whose title starts with "herdr".
        String::new()
    } else {
        format!("--session {}", session)
    };
    let mut target: HWND = HWND(std::ptr::null_mut());
    extern "system" fn enum_proc(hwnd: HWND, l: LPARAM) -> BOOL {
        unsafe {
            let data = &mut *(l.0 as *mut (String, HWND));
            let (ref needle_session, ref mut target) = *data;
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
            if !needle_session.is_empty() && !title_lc.contains(&needle_session.to_lowercase()) {
                return BOOL(1);
            }
            *target = hwnd;
            BOOL(0) // stop enum
        }
    }
    let mut pair = (needle_session, target);
    let _ = EnumWindows(Some(enum_proc), LPARAM(&mut pair as *mut _ as isize));
    target = pair.1;
    if !target.0.is_null() {
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
        }
    }
}