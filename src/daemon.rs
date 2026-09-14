// daemon.rs -- single long-lived host process.
//
// herdr-done-popup used to fork one process per popup, leaving 70+ zombies
// after a busy session. This module owns exactly one daemon at a time:
//
//   * Acquires a named mutex `Local\herdr-done-popup-single`. If the mutex
//     is already held, exits 0 (another daemon is up).
//   * Binds a named pipe `\\.\pipe\herdr-done-popup-v1`. Event senders
//     open it, write one framed request, close, and exit -- they never
//     fork a popup child.
//   * Spawns a listener thread that accepts pipe connections, parses the
//     `[tag][len][payload]` framing, and forwards `DaemonMsg` events to
//     the main loop via an mpsc channel.
//   * The main loop drives a single popup message-pump at a time, with
//     coalescing: same pane -> update content in place; different pane
//     -> dismiss current, show new.

use std::sync::mpsc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

// SAFETY: HANDLE is a raw pointer that we own exclusively for the lifetime
// of the daemon. It is only ever read/written from a single thread at a
// time (the listener thread), and we drop it explicitly before exiting.
// We send the handle to the listener thread as `usize` via this newtype
// so we can satisfy `Send`.
struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, ERROR_ALREADY_EXISTS, ERROR_PIPE_BUSY, HWND,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, NAMED_PIPE_MODE,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, FindWindowW, IsWindow, SendMessageW, WM_CLOSE,
};

use crate::gui::{PopupCmd, PopupHandle, PopupInfo};

pub const PIPE_NAME: &str = r"\\.\pipe\herdr-done-popup-v1";
pub const MUTEX_NAME: &str = "Local\\herdr-done-popup-single";
pub const CLASS_NAME: &str = "HerdrDonePopup";

const TAG_REQUEST: u8 = 0x01;
const TAG_STOP: u8 = 0x02;
const TAG_QUERY: u8 = 0x03;

const PIPE_MAX_INSTANCES: u32 = 8;
const PIPE_OUT_BUF: u32 = 4096;
const PIPE_IN_BUF: u32 = 4096;
const PIPE_CONNECT_TIMEOUT_MS: u32 = 5_000;

const DAEMON_TICK_MS: u64 = 50;

// Generic access (read+write) for a pipe client.
const GENERIC_READ: u32 = 0x80000000;
const GENERIC_WRITE: u32 = 0x40000000;

#[derive(Debug)]
pub enum DaemonMsg {
    Request(PopupInfo),
    Stop,
    Query,
}

pub fn log(msg: &str) {
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
            let _ = writeln!(f, "[{ts}] daemon: {msg}");
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Try to grab the single-instance mutex. Returns Some(handle) on
/// success, None if another daemon already holds it.
unsafe fn ensure_single_instance() -> Option<HANDLE> {
    let name = wide(MUTEX_NAME);
    let h = CreateMutexW(None, true, PCWSTR(name.as_ptr()));
    match h {
        Ok(handle) => {
            let err = GetLastError();
            if err == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(handle);
                None
            } else {
                Some(handle)
            }
        }
        Err(_) => None,
    }
}

/// Opportunistic sweep: if an orphan popup window from a previous
/// crashed daemon is hanging around, close it.
unsafe fn sweep_orphan_popup() {
    let class = wide(CLASS_NAME);
    let hwnd_res = FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null());
    let hwnd = match hwnd_res {
        Ok(h) => h,
        Err(_) => return,
    };
    if hwnd.0.is_null() {
        return;
    }
    log("orphan popup found; sending WM_CLOSE");
    let _ = SendMessageW(hwnd, WM_CLOSE, None, None);
    for _ in 0..50 {
        if !IsWindow(Some(hwnd)).as_bool() {
            log("orphan popup closed cleanly");
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    log("orphan popup still alive after WM_CLOSE; forcing DestroyWindow");
    let _ = DestroyWindow(hwnd);
}

/// Bind a fresh named pipe instance.
unsafe fn bind_pipe_instance() -> HANDLE {
    let name = wide(PIPE_NAME);
    // Pipe mode flags ORed into NAMED_PIPE_MODE.
    // PIPE_TYPE_BYTE = 0, PIPE_READMODE_BYTE = 0, PIPE_WAIT = 0
    // -> all zero in this simplified config.
    let mode = NAMED_PIPE_MODE(0);
    // dwopenmode: PIPE_ACCESS_DUPLEX = 0x00000003 (in|out).
    let dwopenmode: u32 = 0x00000003;
    CreateNamedPipeW(
        PCWSTR(name.as_ptr()),
        FILE_FLAGS_AND_ATTRIBUTES(dwopenmode),
        mode,
        PIPE_MAX_INSTANCES,
        PIPE_OUT_BUF,
        PIPE_IN_BUF,
        PIPE_CONNECT_TIMEOUT_MS,
        None,
    )
}

unsafe fn read_exact(handle: HANDLE, buf: &mut [u8]) -> std::io::Result<()> {
    let mut total = 0;
    while total < buf.len() {
        let mut got = 0u32;
        let ok = ReadFile(handle, Some(&mut buf[total..]), Some(&mut got), None);
        if ok.is_err() || got == 0 {
            return Err(std::io::Error::last_os_error());
        }
        total += got as usize;
    }
    Ok(())
}

unsafe fn write_all(handle: HANDLE, buf: &[u8]) -> std::io::Result<()> {
    let mut written = 0;
    while written < buf.len() {
        let mut put = 0u32;
        let ok = WriteFile(handle, Some(&buf[written..]), Some(&mut put), None);
        if ok.is_err() {
            return Err(std::io::Error::last_os_error());
        }
        if put == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "WriteFile returned 0",
            ));
        }
        written += put as usize;
    }
    let _ = FlushFileBuffers(handle);
    Ok(())
}

unsafe fn handle_one_connection(pipe: HANDLE, tx: &mpsc::Sender<DaemonMsg>) {
    let mut tag = [0u8; 1];
    if read_exact(pipe, &mut tag).is_err() {
        let _ = DisconnectNamedPipe(pipe);
        return;
    }
    let mut lenbuf = [0u8; 4];
    if read_exact(pipe, &mut lenbuf).is_err() {
        let _ = DisconnectNamedPipe(pipe);
        return;
    }
    let len = u32::from_le_bytes(lenbuf) as usize;
    match tag[0] {
        TAG_REQUEST => {
            let mut payload = vec![0u8; len];
            if read_exact(pipe, &mut payload).is_err() {
                let _ = DisconnectNamedPipe(pipe);
                return;
            }
            match serde_json::from_slice::<PopupInfo>(&payload) {
                Ok(info) => {
                    log(&format!("Request received: pane={}", info.pane));
                    let _ = tx.send(DaemonMsg::Request(info));
                }
                Err(e) => log(&format!("Request JSON parse failed: {}", e)),
            }
        }
        TAG_STOP => {
            log("Stop received");
            let _ = tx.send(DaemonMsg::Stop);
        }
        TAG_QUERY => {
            log("Query received");
            let _ = tx.send(DaemonMsg::Query);
            let reply = serde_json::json!({
                "alive": true,
                "version": env!("CARGO_PKG_VERSION"),
                "pid": std::process::id(),
            });
            let bytes = serde_json::to_vec(&reply).unwrap_or_default();
            let lenbuf_reply = (bytes.len() as u32).to_le_bytes();
            let _ = write_all(pipe, &lenbuf_reply);
            let _ = write_all(pipe, &bytes);
        }
        _ => log(&format!("Unknown tag: {}", tag[0])),
    }
    let _ = DisconnectNamedPipe(pipe);
}

unsafe fn listener_thread(pipe: HANDLE, tx: mpsc::Sender<DaemonMsg>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::SeqCst) {
        let conn = ConnectNamedPipe(pipe, None);
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if conn.is_err() {
            let err = GetLastError();
            // ERROR_PIPE_CONNECTED means a client connected before we
            // called ConnectNamedPipe -- treat as connected.
            if err.0 as u32 != windows::Win32::Foundation::ERROR_PIPE_CONNECTED.0 {
                log(&format!("ConnectNamedPipe failed: {:?}", err));
                return;
            }
        }
        handle_one_connection(pipe, &tx);
    }
}

/// Public entry: runs the daemon forever (until Stop or process exit).
/// Returns 0 on graceful shutdown, non-zero on startup failure.
pub fn run_daemon() -> i32 {
    unsafe {
        // 1. Single instance.
        let Some(mutex) = ensure_single_instance() else {
            log("daemon: another instance already running; exiting");
            return 0;
        };

        // 2. Sweep any orphan popup left by a previous daemon.
        sweep_orphan_popup();

        // 3. Bind a fresh pipe instance.
        let pipe = bind_pipe_instance();
        if pipe.is_invalid() {
            log("bind_pipe_instance returned invalid handle");
            let _ = CloseHandle(mutex);
            return 1;
        }

        // 4. Spawn listener thread. Wrap HANDLE in SendHandle so we can
        // move it into the new thread (HANDLE is !Send by default).
        let (tx, rx) = mpsc::channel::<DaemonMsg>();
        let tx_clone = tx.clone();
        let send_pipe = SendHandle(pipe);
        // Pack HANDLE into a usize so the closure body sees `usize`, not
        // the raw pointer. SAFETY: this thread owns the handle exclusively.
        let pipe_usize = send_pipe.0 .0 as usize;
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_clone = stop_flag.clone();
        let listener = thread::Builder::new()
            .name("herdr-popup-listener".into())
            .spawn(move || unsafe {
                listener_thread(HANDLE(pipe_usize as *mut _), tx_clone, stop_flag_clone)
            });
        let listener = match listener {
            Ok(j) => j,
            Err(e) => {
                log(&format!("spawn listener failed: {}", e));
                let _ = CloseHandle(pipe);
                let _ = CloseHandle(mutex);
                return 1;
            }
        };

        log("daemon: up");

        // 5. Main control loop.
        let started = Instant::now();
        let mut current: Option<PopupHandle> = None;
        let mut stop_requested = false;

        loop {
            match rx.recv_timeout(Duration::from_millis(DAEMON_TICK_MS)) {
                Ok(DaemonMsg::Request(new_info)) => {
                    if let Some(h) = current.as_mut() {
                        if h.is_alive() {
                            let same_pane = h
                                .info()
                                .lock()
                                .map(|i| i.pane == new_info.pane)
                                .unwrap_or(false);
                            if same_pane {
                                log("daemon: same pane, updating in place");
                                let _ = h.send(PopupCmd::Update(new_info));
                            } else {
                                log("daemon: different pane, dismissing current");
                                let _ = h.send(PopupCmd::Dismiss);
                                let _ = h.join_timeout(Duration::from_millis(100));
                                current = None;
                                current = Some(PopupHandle::launch(new_info));
                            }
                        } else {
                            current = None;
                            current = Some(PopupHandle::launch(new_info));
                        }
                    } else {
                        current = Some(PopupHandle::launch(new_info));
                    }
                }
                Ok(DaemonMsg::Stop) => {
                    log("daemon: stop requested");
                    stop_requested = true;
                    // Tear down any live popup so we can exit promptly.
                    if let Some(mut h) = current.take() {
                        let _ = h.send(PopupCmd::Dismiss);
                        let _ = h.join_timeout(Duration::from_millis(200));
                    }
                }
                Ok(DaemonMsg::Query) => {
                    // already replied on the pipe; nothing to do here.
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    log("daemon: listener disconnected; exiting");
                    break;
                }
            }

            // Reap finished popups so the JoinHandle is freed promptly.
            if let Some(h) = current.as_ref() {
                if h.is_finished() {
                    current = None;
                }
            }

            if stop_requested && current.is_none() {
                break;
            }
        }

        // 6. Clean shutdown.
        if let Some(mut h) = current.take() {
            let _ = h.send(PopupCmd::Dismiss);
            let _ = h.join_timeout(Duration::from_millis(500));
        }
        log(&format!(
            "daemon: exiting cleanly after {:?}",
            started.elapsed()
        ));
        // Signal listener thread to exit (it might be blocked in
        // ConnectNamedPipe), then join it. Drop tx so the mpsc channel
        // closes and DisconnectNamedPipe errors are recoverable.
        stop_flag.store(true, Ordering::SeqCst);
        drop(tx);
        let _ = CloseHandle(pipe);
        let _ = listener.join();
        let _ = CloseHandle(mutex);
        0
    }
}

/// Open a handle to the named pipe for sending.
unsafe fn open_pipe(dwaccess: u32) -> windows::core::Result<HANDLE> {
    let name = wide(PIPE_NAME);
    CreateFileW(
        PCWSTR(name.as_ptr()),
        dwaccess,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        None,
        OPEN_EXISTING,
        FILE_FLAGS_AND_ATTRIBUTES(0),
        None,
    )
}

/// Check if the daemon is alive by trying to open the named pipe.
pub fn daemon_alive() -> bool {
    unsafe {
        match open_pipe(GENERIC_WRITE) {
            Ok(h) => {
                let _ = CloseHandle(h);
                true
            }
            Err(e) => {
                let code = e.code().0 as u32;
                if code == ERROR_PIPE_BUSY.0 as u32 {
                    true
                } else {
                    false
                }
            }
        }
    }
}

/// Send a Request frame to the daemon. Best-effort.
pub unsafe fn send_request(info: &PopupInfo) -> std::io::Result<()> {
    let handle = open_pipe(GENERIC_WRITE)
        .map_err(|e| std::io::Error::from_raw_os_error(e.code().0 as i32))?;
    let json = serde_json::to_vec(info).map_err(std::io::Error::other)?;
    let mut frame = Vec::with_capacity(5 + json.len());
    frame.push(TAG_REQUEST);
    frame.extend_from_slice(&(json.len() as u32).to_le_bytes());
    frame.extend_from_slice(&json);
    let write_res = write_all(handle, &frame);
    let _ = CloseHandle(handle);
    write_res
}

/// Send a Stop frame to the daemon.
pub unsafe fn send_stop() -> std::io::Result<()> {
    let handle = open_pipe(GENERIC_WRITE)
        .map_err(|e| std::io::Error::from_raw_os_error(e.code().0 as i32))?;
    let frame = [TAG_STOP, 0, 0, 0, 0];
    let res = write_all(handle, &frame);
    let _ = CloseHandle(handle);
    res
}

/// Send a Query frame and read back the JSON reply.
pub unsafe fn send_query() -> std::io::Result<String> {
    let handle = open_pipe(GENERIC_READ | GENERIC_WRITE)
        .map_err(|e| std::io::Error::from_raw_os_error(e.code().0 as i32))?;
    let frame = [TAG_QUERY, 0, 0, 0, 0];
    write_all(handle, &frame)?;
    let mut lenbuf = [0u8; 4];
    read_exact(handle, &mut lenbuf)?;
    let len = u32::from_le_bytes(lenbuf) as usize;
    let mut buf = vec![0u8; len];
    read_exact(handle, &mut buf)?;
    let _ = CloseHandle(handle);
    String::from_utf8(buf).map_err(std::io::Error::other)
}

/// Spawn the daemon as a detached child process. Used by `start`.
pub fn spawn_daemon_detached() -> std::io::Result<u32> {
    let exe = std::env::current_exe()?;
    let child = std::process::Command::new(exe)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(child.id())
}

/// Wait until the daemon is alive or the timeout elapses.
pub fn wait_daemon_alive(timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if daemon_alive() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}