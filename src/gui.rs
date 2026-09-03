use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Receiver;
use serde_json::Value;
use windows::core::{BOOL, PCWSTR};

/// 日志输出到 %TEMP%\herdr-done-popup.log，方便排查。
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
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawTextW,
    EndPaint, FillRect, SetBkMode, SetTextColor, SetWindowRgn, MONITORINFO,
    MonitorFromPoint, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, DT_CENTER, DT_SINGLELINE, DT_VCENTER,
    DT_WORDBREAK,
    HGDIOBJ, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    self, *, GetClientRect, GetCursorPos,
};

const WM_POPUP: u32 = WM_APP + 1;
const POPUP_W: i32 = 300;
const POPUP_H: i32 = 82;
const MARGIN: i32 = 20;
const GAP: i32 = 8;
const ROUND: i32 = 20; // 柔和圆角
const CLOSE_W: i32 = 42; // 右上角关闭热区宽度
const AUTO_DISMISS_MS: u32 = 10000; // 10 秒自动关闭
const FOLLOW_POLL_MS: u32 = 500; // 0.5 秒轮询用户是否已“跟进”到对应 workspace
const RECENT_INPUT_MS: u32 = 5000; // 5 秒内有输入视为“正在操作”
const ID_TIMER_DISMISS: usize = 1;
const ID_TIMER_FOLLOW: usize = 2;

static ACTIVE_COUNT: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy, PartialEq, Eq)]
enum PopupMode {
    /// 5 秒后自动关闭（最近有输入）
    Auto5s,
    /// 一直显示，直到用户主动关闭
    Permanent,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Decision {
    /// 用户正在 herdr 窗口里操作，不弹
    Suppress,
    Auto5s,
    Permanent,
}

#[derive(Clone, Debug)]
pub struct PopupInfo {
    pub session: String,
    pub agent: String,
    pub pane: String,
    pub workspace: String,
    pub tab_id: String,
    pub snippet: String,
}

struct PopupState {
    info: PopupInfo,
    auto_dismiss: bool,
}

pub fn run_ui_loop(rx: Receiver<PopupInfo>) {
    unsafe {
        let _ = register_class("HerdrDonePopup", Some(popup_proc));
        let _ = register_class("HerdrDoneController", Some(controller_proc));
        let instance = Some(HINSTANCE(GetModuleHandleW(None).unwrap_or_default().0));
        let class = wide("HerdrDoneController");
        let controller = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(class.as_ptr()),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            instance,
            None,
        )
        .unwrap_or_default();
        if controller.0.is_null() {
            return;
        }
        // HWND 非 Send：以原始值跨线程传递。
        let controller_raw = controller.0 as isize;
        std::thread::spawn(move || {
            while let Ok(info) = rx.recv() {
                let boxed = Box::new(info);
                let ptr = Box::into_raw(boxed);
                let _ = PostMessageW(
                    Some(HWND(controller_raw as *mut std::ffi::c_void)),
                    WM_POPUP,
                    WPARAM(0),
                    LPARAM(ptr as isize),
                );
            }
        });
        let mut msg = MSG::default();
        loop {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }
}

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

unsafe extern "system" fn controller_proc(hwnd: HWND, msg: u32, _w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_POPUP {
        let ptr = l.0 as *mut PopupInfo;
        if !ptr.is_null() {
            let info = *Box::from_raw(ptr);
            match decide_mode(&info) {
                Decision::Suppress => { /* 不弹 */ }
                Decision::Auto5s => create_popup(hwnd, info, true),
                Decision::Permanent => create_popup(hwnd, info, false),
            }
        }
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, _w, l)
}

/// 用户行为 → 弹窗策略：
/// 1. 前台窗口是 herdr 且 同一 session + 同一 tab → 抑制
///    （不同 herdr / 不同 session / 不同 workspace / 不同 tab → 都要提醒）
/// 2. 5 秒内有键盘/鼠标输入 → 10s 自动关闭（别打断你打字）
/// 3. 否则 → 一直显示（你正在别处干活，强制提醒）
unsafe fn decide_mode(info: &PopupInfo) -> Decision {
    let foreground_herdr = foreground_is_herdr();
    let title = foreground_title();
    let parsed_session = if foreground_herdr {
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
        "decide session={} pane={} tab={} fg_herdr={} fg_title={:?} parsed_session={:?} same_session={} focused_tab={:?} recent_input={}",
        info.session,
        info.pane,
        info.tab_id,
        foreground_herdr,
        title,
        parsed_session,
        same_session,
        focused_tab,
        recent
    ));
    if foreground_herdr
        && same_session
        && !info.tab_id.is_empty()
        && focused_tab.as_deref() == Some(info.tab_id.as_str())
    {
        log("  -> SUPPRESS");
        Decision::Suppress
    } else if recent {
        log("  -> AUTO5s");
        Decision::Auto5s
    } else {
        log("  -> PERMANENT");
        Decision::Permanent
    }
}

unsafe fn should_suppress(info: &PopupInfo) -> bool {
    if !foreground_is_herdr() {
        return false;
    }
    let title = foreground_title();
    let title_lc = title.to_lowercase();
    let parsed_session = parse_session_from_title(&title_lc);
    let parsed = match parsed_session {
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

/// 从前台窗口标题提取 session 名（小写）。`None` 表示看不出是 herdr。
/// 例子：
///   "herdr --session dse"   → Some("dse")
///   "herdr --session default" → Some("default")
///   "herdr"                  → Some("default") （无 --session 标志即默认实例）
///   "Warp - herdr"           → Some("default")
///   "Microsoft Visual Studio" → None
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

/// 轮询：用户是不是已经“跟进”到了这个 popup 对应的 tab？
/// 是的话销毁 popup（说明用户已经看到并切过去处理了）。
unsafe fn user_moved_into_workspace(info: &PopupInfo) -> bool {
    if !foreground_is_herdr() {
        return false;
    }
    let title = foreground_title();
    let parsed = parse_session_from_title(&title.to_lowercase());
    let parsed = match parsed {
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

unsafe fn focused_workspace_in(session: &str) -> Option<String> {
    let out = Command::new("herdr")
        .args(["--session", session, "workspace", "list"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(&text).ok()?;
    find_focused_workspace_id(&v)
}

unsafe fn focused_tab_in(session: &str, workspace_id: &str) -> Option<String> {
    let out = Command::new("herdr")
        .args([
            "--session",
            session,
            "tab",
            "list",
            "--workspace",
            workspace_id,
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(&text).ok()?;
    find_focused_tab_id(&v)
}

fn find_focused_workspace_id(v: &Value) -> Option<String> {
    match v {
        Value::Object(map) => {
            let id = map.get("workspace_id").and_then(|s| s.as_str());
            let focused = map.get("focused").and_then(|s| s.as_bool()).unwrap_or(false);
            if focused && id.is_some() {
                return id.map(|s| s.to_string());
            }
            for val in map.values() {
                if let Some(f) = find_focused_workspace_id(val) {
                    return Some(f);
                }
            }
            None
        }
        Value::Array(arr) => {
            for val in arr {
                if let Some(f) = find_focused_workspace_id(val) {
                    return Some(f);
                }
            }
            None
        }
        _ => None,
    }
}

fn find_focused_tab_id(v: &Value) -> Option<String> {
    match v {
        Value::Object(map) => {
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
        Value::Array(arr) => {
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

/// 文件名常量：窗口不显示文字直到 WM_PAINT。
unsafe fn create_popup(owner: HWND, info: PopupInfo, auto_dismiss: bool) {
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let _ = GetMonitorInfoW(monitor, &mut mi);
    let r = mi.rcWork;
    // 从下往上堆叠（多 pane 时新完成的在最下）
    let slot = ACTIVE_COUNT.fetch_add(1, Ordering::SeqCst);
    let x = r.right - POPUP_W - MARGIN;
    let y = r.bottom - POPUP_H - MARGIN - (POPUP_H + GAP) * slot as i32;
    let class = wide("HerdrDonePopup");
    let data = Box::new(PopupState { info, auto_dismiss });
    let ptr = Box::into_raw(data);
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        PCWSTR(class.as_ptr()),
        PCWSTR(class.as_ptr()),
        WS_POPUP | WS_VISIBLE,
        x,
        y,
        POPUP_W,
        POPUP_H,
        Some(owner),
        None,
        module_instance(),
        Some(ptr as *const std::ffi::c_void),
    );
    let Ok(hwnd) = hwnd else {
        ACTIVE_COUNT.fetch_sub(1, Ordering::SeqCst);
        drop(Box::from_raw(ptr));
        return;
    };
    // 圆角胶囊区域。
    let rgn = CreateRoundRectRgn(0, 0, POPUP_W, POPUP_H, ROUND, ROUND);
    let _ = SetWindowRgn(hwnd, Some(rgn), true);
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        x,
        y,
        POPUP_W,
        POPUP_H,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    if auto_dismiss {
        let _ = SetTimer(Some(hwnd), ID_TIMER_DISMISS, AUTO_DISMISS_MS, None);
    }
    // “跟进”轮询：用户直接在对应 workspace 里打字/操作 → 弹窗自动消失
    let _ = SetTimer(Some(hwnd), ID_TIMER_FOLLOW, FOLLOW_POLL_MS, None);
    log(&format!(
        "popup created auto_dismiss={} hwnd={:?}",
        auto_dismiss, hwnd
    ));
}

/// 按 agent 给一个柔和底色（一点点颜色）。
fn color_for_agent(agent: &str) -> COLORREF {
    let (r, g, b) = match agent.to_lowercase().as_str() {
        "claude" => (0xE0, 0x8A, 0x3C),          // 暖橙
        "codex" => (0x49, 0x86, 0xE8),           // 蓝
        "opencode" => (0x3F, 0xA8, 0x73),        // 绿
        "grok" => (0x5C, 0x5C, 0x5C),            // 灰
        _ => (0x2D, 0x8C, 0x7C),                 // 默认青
    };
    COLORREF(((b as u32) << 16) | ((g as u32) << 8) | (r as u32))
}

unsafe extern "system" fn popup_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = &*(l.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd as _, GWLP_USERDATA, cs.lpCreateParams as isize);
            LRESULT(1)
        }
        WM_ERASEBKGND => LRESULT(1), // 我们自己画，避免默认擦除闪烁
        WM_PAINT => {
            paint_popup(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = (l.0 & 0xFFFF) as i32;
            let y = ((l.0 >> 16) & 0xFFFF) as i32;
            if x >= POPUP_W - CLOSE_W && y < 34 {
                // 右上角 = 关闭提醒
                let _ = DestroyWindow(hwnd);
            } else {
                // 其他任意位置 = 打开对应 Herdr pane
                open_target(hwnd);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            let id = w.0 as usize;
            let ptr = GetWindowLongPtrW(hwnd as _, GWLP_USERDATA) as *mut PopupState;
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
                    // 1. 用户已切到对应 workspace → 立即销毁
                    if user_moved_into_workspace(&state.info) {
                        log("follow: user entered workspace -> dismiss");
                        let _ = DestroyWindow(hwnd);
                        return LRESULT(0);
                    }
                    // 2. PERMANENT 模式 + 用户现在活跃 → 升级为 10s 自动关闭
                    if !state.auto_dismiss && last_input_recent() {
                        state.auto_dismiss = true;
                        let _ = SetTimer(Some(hwnd), ID_TIMER_DISMISS, AUTO_DISMISS_MS, None);
                        log("follow: user became active -> start 10s dismiss");
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
            ACTIVE_COUNT.fetch_sub(1, Ordering::SeqCst);
            let ptr = GetWindowLongPtrW(hwnd as _, GWLP_USERDATA) as *mut PopupState;
            if !ptr.is_null() {
                drop(Box::from_raw(ptr));
            }
            SetWindowLongPtrW(hwnd as _, GWLP_USERDATA, 0);
            DefWindowProcW(hwnd, msg, w, l)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

unsafe fn paint_popup(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    GetClientRect(hwnd, &mut rc);

    let info_ptr = GetWindowLongPtrW(hwnd as _, GWLP_USERDATA) as *mut PopupState;
    let (agent, workspace, snippet) = if !info_ptr.is_null() {
        let info = &(*info_ptr).info;
        (info.agent.as_str(), info.workspace.as_str(), info.snippet.as_str())
    } else {
        ("", "", "")
    };

    // 底色
    let bg = CreateSolidBrush(color_for_agent(agent));
    FillRect(hdc, &rc, bg);

    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(0x00F4F4EE));

    // 第 1 行：agent 名 + workspace
    let mut tr = RECT {
        left: rc.left + 18,
        top: rc.top + 10,
        right: rc.right - CLOSE_W - 6,
        bottom: rc.top + 34,
    };
    let mut titlew = wide(&format!("{} · {}", agent, workspace));
    titlew.pop();
    let _ = DrawTextW(hdc, &mut titlew, &mut tr, DT_VCENTER | DT_SINGLELINE);

    // 第 2 行以上：输出第一句片段（自动换行）
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

    // 右上角关闭符号
    let mut close_rect = RECT {
        left: rc.right - CLOSE_W,
        top: 0,
        right: rc.right,
        bottom: 34,
    };
    let mut closew = wide("×");
    closew.pop();
    let _ = DrawTextW(hdc, &mut closew, &mut close_rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE);

    EndPaint(hwnd, &ps);
    let _ = DeleteObject(HGDIOBJ(bg.0));
}

unsafe fn open_target(hwnd: HWND) {
    let ptr = GetWindowLongPtrW(hwnd as _, GWLP_USERDATA) as *mut PopupState;
    if !ptr.is_null() {
        let info = &(*ptr).info;
        let _ = Command::new("herdr")
            .args(["--session", &info.session, "agent", "focus", &info.pane])
            .status();
        activate_herdr(&info.session);
    }
    let _ = DestroyWindow(hwnd);
}

unsafe fn activate_herdr(session: &str) {
    let mut target: HWND = HWND(std::ptr::null_mut());
    let needle = if session == "default" {
        "herdr".to_lowercase()
    } else {
        format!("herdr --session {}", session).to_lowercase()
    };
    extern "system" fn enum_proc(hwnd: HWND, l: LPARAM) -> BOOL {
        unsafe {
            let data = &mut *(l.0 as *mut (String, HWND));
            let mut buf = [0u16; 512];
            let n = GetWindowTextW(hwnd, &mut buf) as usize;
            let title = String::from_utf16_lossy(&buf[..n]).to_lowercase();
            if IsWindowVisible(hwnd).as_bool() && title.contains(&data.0) {
                data.1 = hwnd;
                return BOOL(0);
            }
            BOOL(1)
        }
    }
    let mut pair = (needle, target);
    let _ = EnumWindows(Some(enum_proc), LPARAM(&mut pair as *mut _ as isize));
    target = pair.1;
    if !target.0.is_null() {
        let _ = ShowWindow(target, SW_RESTORE);
        let _ = BringWindowToTop(target);
        let _ = SetForegroundWindow(target);
    }
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