// 系统工具:DPI、桌面宿主窗口(WorkerW)、宽字符串等
use std::mem::size_of;
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, FindWindowW, GetClassNameW, GetSystemMetrics, IsWindow,
    SM_CXSCREEN, SM_CYSCREEN, SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW,
    SetProcessDPIAware,
};
use windows::core::{BOOL, w};

/// UTF-8 -> 以 0 结尾的 UTF-16
pub fn wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn set_dpi_awareness() {
    unsafe {
        // 尽力而为:新 API 失败就退回旧 API
        if SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        )
        .is_err()
        {
            let _ = SetProcessDPIAware();
        }
    }
}

pub fn screen_size() -> (i32, i32) {
    unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
}

pub fn work_area(hwnd: HWND) -> RECT {
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            mi.rcWork
        } else {
            let (w, h) = screen_size();
            RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            }
        }
    }
}

fn should_request_desktop_worker() -> bool {
    static LAST_ATTEMPT: std::sync::OnceLock<std::sync::Mutex<Option<std::time::Instant>>> =
        std::sync::OnceLock::new();
    let mut last = LAST_ATTEMPT
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = std::time::Instant::now();
    if last.is_some_and(|previous| {
        now.saturating_duration_since(previous) < std::time::Duration::from_secs(1)
    }) {
        return false;
    }
    *last = Some(now);
    true
}

/// 桌面层宿主查找:优先持有 SHELLDLL_DefView 的 WorkerW，兼容图标直接挂在
/// Progman 下的系统。只有真正承载桌面视图的窗口才能作为 Z 序锚点。
unsafe extern "system" fn enum_child_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let found = &mut *(lparam.0 as *mut Option<HWND>);
    let mut cls = [0u16; 64];
    let n = GetClassNameW(hwnd, &mut cls);
    if n > 0 {
        let name = String::from_utf16_lossy(&cls[..n as usize]);
        if name == "SHELLDLL_DefView" {
            *found = Some(hwnd);
            return BOOL(0);
        }
    }
    BOOL(1)
}

unsafe extern "system" fn enum_list_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let found = &mut *(lparam.0 as *mut Option<HWND>);
    let mut cls = [0u16; 64];
    let n = GetClassNameW(hwnd, &mut cls);
    if n > 0 && String::from_utf16_lossy(&cls[..n as usize]) == "SysListView32" {
        *found = Some(hwnd);
        return BOOL(0);
    }
    BOOL(1)
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let found = &mut *(lparam.0 as *mut Option<HWND>);
    let mut cls = [0u16; 64];
    let n = GetClassNameW(hwnd, &mut cls);
    if n > 0 {
        let name = String::from_utf16_lossy(&cls[..n as usize]);
        if name == "WorkerW" {
            let mut def_view = None;
            let context = LPARAM(&mut def_view as *mut Option<HWND> as isize);
            let _ = EnumChildWindows(Some(hwnd), Some(enum_child_proc), context);
            if def_view.is_some() {
                *found = Some(hwnd); // 返回持有 SHELLDLL_DefView 的 WorkerW 本身
                return BOOL(0);
            }
        }
    }
    BOOL(1)
}

unsafe fn contains_desktop_view(hwnd: HWND) -> bool {
    let mut def_view = None;
    let context = LPARAM(&mut def_view as *mut Option<HWND> as isize);
    let _ = EnumChildWindows(Some(hwnd), Some(enum_child_proc), context);
    def_view.is_some()
}

/// 缓存的宿主即使 HWND 仍存在，也可能已在 Explorer 重建时失去桌面视图。
/// 同时验证窗口和 SHELLDLL_DefView，避免把栅栏锚到一个“活着但已过期”的 WorkerW。
pub fn is_current_desktop_host(hwnd: HWND) -> bool {
    unsafe { IsWindow(Some(hwnd)).as_bool() && contains_desktop_view(hwnd) }
}

/// 找到桌面图标宿主窗口(WorkerW),栅栏挂到它下面才能随桌面常驻
pub fn find_desktop_host() -> Option<HWND> {
    unsafe {
        let progman = FindWindowW(w!("Progman"), None).ok();
        if let Some(progman) = progman.filter(|h| !h.is_invalid()) {
            // 先直接找 WorkerW(不要每次发 0x052C —— 它会触发 Progman 重建 WorkerW,
            // 把挂在下面的栅栏窗口级联销毁/移动)
            let mut found = None;
            let context = LPARAM(&mut found as *mut Option<HWND> as isize);
            let _ = EnumWindows(Some(enum_proc), context);
            if let Some(h) = found {
                return Some(h);
            }
            // 部分系统把 SHELLDLL_DefView 直接放在 Progman 下，无需创建 WorkerW。
            if contains_desktop_view(progman) {
                return Some(progman);
            }
            // 找不到再节流尝试 0x052C(Win10+ 让 Progman 生成 WorkerW)。必须带超时：
            // Explorer 正忙或挂起时，启动主线程不能无限阻塞。创建可能异步完成，调用方
            // 会保持栅栏隐藏并在后续桌面层 tick 重试。
            if should_request_desktop_worker() {
                let _ = SendMessageTimeoutW(
                    progman,
                    0x052C,
                    WPARAM(0),
                    LPARAM(0),
                    SMTO_ABORTIFHUNG | SMTO_BLOCK,
                    100,
                    None,
                );
            }
            found = None;
            let context = LPARAM(&mut found as *mut Option<HWND> as isize);
            let _ = EnumWindows(Some(enum_proc), context);
            if let Some(h) = found {
                return Some(h);
            }
            if contains_desktop_view(progman) {
                return Some(progman);
            }
            // 不用一个尚未承载桌面视图的 Progman 盲目兜底；错误锚点会把栅栏
            // 短暂提到普通应用之上。让生命周期层保持隐藏并在下一 tick 重试。
            return None;
        }
        None
    }
}

/// 找到 Explorer 中承载桌面原生图标的 SysListView32。
pub fn find_desktop_listview() -> Option<HWND> {
    unsafe {
        let host = find_desktop_host()?;
        let mut found = None;
        let context = LPARAM(&mut found as *mut Option<HWND> as isize);
        let _ = EnumChildWindows(Some(host), Some(enum_list_proc), context);
        if found.is_none() {
            // Progman 作为兜底宿主时，图标列表可能多嵌套一层。
            if let Ok(progman) = FindWindowW(w!("Progman"), None) {
                let context = LPARAM(&mut found as *mut Option<HWND> as isize);
                let _ = EnumChildWindows(Some(progman), Some(enum_list_proc), context);
            }
        }
        found
    }
}
