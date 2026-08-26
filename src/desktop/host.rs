//! Explorer 桌面宿主窗口发现。

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, FindWindowW, GW_HWNDPREV, GetClassNameW, GetWindow, HWND_TOP,
    IsIconic, SMTO_ABORTIFHUNG, SMTO_BLOCK, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SendMessageTimeoutW, SetWindowPos, ShowWindow,
};
use windows::core::{BOOL, w};

const CREATE_WORKERW_MESSAGE: u32 = 0x052C;
const CREATE_WORKERW_TIMEOUT_MS: u32 = 250;
static CREATE_WORKERW_SENT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn window_class(hwnd: HWND) -> Option<String> {
    let mut cls = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut cls) };
    (n > 0).then(|| String::from_utf16_lossy(&cls[..n as usize]))
}

#[derive(Default)]
struct InsertHostSearch {
    found: Option<HWND>,
}

impl InsertHostSearch {
    fn observe(&mut self, hwnd: HWND, class: &str) -> bool {
        match class {
            "Progman" => {
                self.found = Some(hwnd);
                false
            }
            "WorkerW" if self.found.is_none() => {
                self.found = Some(hwnd);
                true
            }
            _ => true,
        }
    }
}

/// 桌面层宿主查找:优先 Progman(桌面最底),其次 WorkerW。
/// 纯枚举,不发 0x052C(Progman 无响应时会卡死主线程)。
unsafe extern "system" fn enum_host_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = unsafe { &mut *(lparam.0 as *mut InsertHostSearch) };
    let keep_going = window_class(hwnd).is_none_or(|class| search.observe(hwnd, &class));
    BOOL::from(keep_going)
}

/// 桌面宿主窗口(Progman 优先,WorkerW 兜底)。
/// 栅栏插到它之后 = 桌面背景之上、图标层/普通窗口之下。
/// 注意:不能用 HWND_BOTTOM —— 实测会把窗口压到 Progman 之下的
/// DWM 隐藏区域,窗口不可见且 FindWindow/EnumWindows 都枚举不到。
pub(crate) fn desktop_insert_host() -> Option<HWND> {
    let mut search = InsertHostSearch::default();
    let state = LPARAM((&mut search as *mut InsertHostSearch) as isize);
    let _ = unsafe { EnumWindows(Some(enum_host_proc), state) };
    search.found
}

/// 在桌面宿主正上方显示栅栏，不先把它抬到普通应用窗口之上。
/// 这是所有“从隐藏/最小化恢复”路径的唯一显示边界。
pub(crate) fn show_on_desktop_layer(hwnd: HWND) -> bool {
    let Some(host) = desktop_insert_host() else {
        return false;
    };
    show_above_host(hwnd, host)
}

/// 使用调用方已经验证的桌面宿主显示栅栏。
pub(crate) fn show_above_host(hwnd: HWND, host: HWND) -> bool {
    unsafe {
        // 系统“显示桌面”可能真正最小化独立顶层窗口。先恢复，
        // 但在同一次消息处理中立即纠正 Z 序，不等后台定时器。
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        let above = GetWindow(host, GW_HWNDPREV).unwrap_or(HWND_TOP);
        let result = if above == hwnd {
            SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
            )
        } else {
            SetWindowPos(
                hwnd,
                Some(above),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        };
        result.is_ok()
    }
}

struct ChildWindowSearch {
    class: &'static str,
    found: Option<HWND>,
}

unsafe extern "system" fn enum_child_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = unsafe { &mut *(lparam.0 as *mut ChildWindowSearch) };
    if window_class(hwnd).is_some_and(|class| class == search.class) {
        search.found = Some(hwnd);
        return BOOL(0);
    }
    BOOL(1)
}

fn find_child_window(parent: HWND, class: &'static str) -> Option<HWND> {
    let mut search = ChildWindowSearch { class, found: None };
    let state = LPARAM((&mut search as *mut ChildWindowSearch) as isize);
    let _ = unsafe { EnumChildWindows(Some(parent), Some(enum_child_proc), state) };
    search.found
}

#[derive(Default)]
struct DesktopHostSearch {
    found: Option<HWND>,
}

unsafe extern "system" fn enum_desktop_host_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if window_class(hwnd).as_deref() != Some("WorkerW") {
        return BOOL(1);
    }
    if find_child_window(hwnd, "SHELLDLL_DefView").is_some() {
        let search = unsafe { &mut *(lparam.0 as *mut DesktopHostSearch) };
        search.found = Some(hwnd);
        return BOOL(0);
    }
    BOOL(1)
}

fn find_workerw_desktop_host() -> Option<HWND> {
    let mut search = DesktopHostSearch::default();
    let state = LPARAM((&mut search as *mut DesktopHostSearch) as isize);
    let _ = unsafe { EnumWindows(Some(enum_desktop_host_proc), state) };
    search.found
}

fn send_message_with_timeout(hwnd: HWND, message: u32, timeout_ms: u32) -> bool {
    unsafe {
        SendMessageTimeoutW(
            hwnd,
            message,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            timeout_ms,
            None,
        )
        .0 != 0
    }
}

fn request_workerw(progman: HWND) -> bool {
    send_message_with_timeout(progman, CREATE_WORKERW_MESSAGE, CREATE_WORKERW_TIMEOUT_MS)
}

/// 找到桌面图标宿主窗口(WorkerW),栅栏挂到它下面才能随桌面常驻
pub(crate) fn find_desktop_host() -> Option<HWND> {
    let progman = unsafe { FindWindowW(w!("Progman"), None) }
        .ok()
        .filter(|h| !h.is_invalid())?;
    // 先直接找 WorkerW(不要每次发 0x052C —— 它会触发 Progman 重建 WorkerW,
    // 把挂在下面的栅栏窗口级联销毁/移动)
    if let Some(host) = find_workerw_desktop_host() {
        return Some(host);
    }
    // 找不到再请求一次 WorkerW。消息最多等待 250ms，避免 Explorer 卡死 UI 线程。
    let first = !CREATE_WORKERW_SENT.swap(true, std::sync::atomic::Ordering::Relaxed);
    if first && request_workerw(progman) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if let Some(host) = find_workerw_desktop_host() {
        return Some(host);
    }
    // 兜底:WorkerW 没找到就用 Progman
    Some(progman)
}

/// 找到 Explorer 中承载桌面原生图标的 SysListView32。
pub(crate) fn find_desktop_listview() -> Option<HWND> {
    let host = find_desktop_host()?;
    if let Some(list) = find_child_window(host, "SysListView32") {
        return Some(list);
    }
    // Progman 作为兜底宿主时，图标列表可能多嵌套一层。
    let progman = unsafe { FindWindowW(w!("Progman"), None) }.ok()?;
    find_child_window(progman, "SysListView32")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP,
    };
    use windows::core::PCWSTR;

    fn hwnd(value: usize) -> HWND {
        HWND(value as *mut c_void)
    }

    #[test]
    fn insert_host_prefers_progman_and_keeps_the_first_worker_fallback() {
        let mut search = InsertHostSearch::default();
        assert!(search.observe(hwnd(1), "WorkerW"));
        assert!(search.observe(hwnd(2), "WorkerW"));
        assert_eq!(search.found, Some(hwnd(1)));

        assert!(!search.observe(hwnd(3), "Progman"));
        assert_eq!(search.found, Some(hwnd(3)));
    }

    #[test]
    fn message_timeout_returns_while_the_window_thread_is_unresponsive() {
        let (window_tx, window_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let window_thread = std::thread::spawn(move || {
            let window = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("STATIC"),
                    PCWSTR::null(),
                    WS_POPUP,
                    0,
                    0,
                    32,
                    32,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .expect("test window should be created");
            window_tx.send(window.0 as usize).unwrap();
            release_rx.recv().unwrap();
            let _ = unsafe { DestroyWindow(window) };
        });

        let raw = window_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("window thread should publish its handle");
        let started = Instant::now();
        let delivered = send_message_with_timeout(hwnd(raw), 0, 50);
        let elapsed = started.elapsed();

        release_tx.send(()).unwrap();
        window_thread.join().unwrap();

        assert!(!delivered);
        assert!(elapsed < Duration::from_secs(1), "elapsed: {elapsed:?}");
    }
}
