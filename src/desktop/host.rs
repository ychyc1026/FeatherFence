//! Explorer 桌面宿主窗口发现。

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, FindWindowW, GetClassNameW, SendMessageW,
};
use windows::core::{BOOL, w};

static mut FOUND_HOST: Option<HWND> = None;
static mut FOUND_LIST: Option<HWND> = None;
static SPIF_SENT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// 桌面层宿主查找:优先 Progman(桌面最底),其次 WorkerW。
/// 纯枚举,不发 0x052C(Progman 无响应时会卡死主线程)。
unsafe extern "system" fn enum_host_proc(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    let mut cls = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut cls) };
    if n > 0 {
        let name = String::from_utf16_lossy(&cls[..n as usize]);
        match name.as_str() {
            // 枚举顺序是 Z 序从上到下,Progman 在最底:遇到即停
            "Progman" => {
                unsafe { FOUND_HOST = Some(hwnd) };
                return BOOL(0);
            }
            // WorkerW 兜底:只记第一个(未找到 Progman 时)
            "WorkerW" if (unsafe { FOUND_HOST }).is_none() => {
                unsafe { FOUND_HOST = Some(hwnd) };
            }
            _ => {}
        }
    }
    BOOL(1)
}

/// 桌面宿主窗口(Progman 优先,WorkerW 兜底)。
/// 栅栏插到它之后 = 桌面背景之上、图标层/普通窗口之下。
/// 注意:不能用 HWND_BOTTOM —— 实测会把窗口压到 Progman 之下的
/// DWM 隐藏区域,窗口不可见且 FindWindow/EnumWindows 都枚举不到。
pub(crate) fn desktop_insert_host() -> Option<HWND> {
    unsafe {
        FOUND_HOST = None;
        let _ = EnumWindows(Some(enum_host_proc), LPARAM(0));
        FOUND_HOST
    }
}

unsafe extern "system" fn enum_child_proc(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    let mut cls = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut cls) };
    if n > 0 {
        let name = String::from_utf16_lossy(&cls[..n as usize]);
        if name == "SHELLDLL_DefView" {
            unsafe { FOUND_HOST = Some(hwnd) };
            return BOOL(0);
        }
    }
    BOOL(1)
}

unsafe extern "system" fn enum_list_proc(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    let mut cls = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut cls) };
    if n > 0 && String::from_utf16_lossy(&cls[..n as usize]) == "SysListView32" {
        unsafe { FOUND_LIST = Some(hwnd) };
        return BOOL(0);
    }
    BOOL(1)
}

unsafe extern "system" fn enum_proc(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    let mut cls = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut cls) };
    if n > 0 {
        let name = String::from_utf16_lossy(&cls[..n as usize]);
        if name == "WorkerW" {
            unsafe { FOUND_HOST = None };
            let _ = unsafe { EnumChildWindows(Some(hwnd), Some(enum_child_proc), LPARAM(0)) };
            if (unsafe { FOUND_HOST }).is_some() {
                unsafe { FOUND_HOST = Some(hwnd) }; // 返回持有 SHELLDLL_DefView 的 WorkerW 本身
                return BOOL(0);
            }
        }
    }
    BOOL(1)
}

/// 找到桌面图标宿主窗口(WorkerW),栅栏挂到它下面才能随桌面常驻
pub(crate) fn find_desktop_host() -> Option<HWND> {
    unsafe {
        FOUND_HOST = None;
        let progman = FindWindowW(w!("Progman"), None).ok();
        if let Some(progman) = progman.filter(|h| !h.is_invalid()) {
            // 先直接找 WorkerW(不要每次发 0x052C —— 它会触发 Progman 重建 WorkerW,
            // 把挂在下面的栅栏窗口级联销毁/移动)
            let _ = EnumWindows(Some(enum_proc), LPARAM(0));
            if let Some(h) = FOUND_HOST {
                return Some(h);
            }
            // 找不到再发一次 0x052C(Win10+ 让 Progman 生成 WorkerW);只发一次,
            // 发完 WorkerW 就存在了,后续直接枚举找到
            let first = SPIF_SENT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            if first {
                SendMessageW(progman, 0x052C, Some(WPARAM(0)), Some(LPARAM(0)));
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            FOUND_HOST = None;
            let _ = EnumWindows(Some(enum_proc), LPARAM(0));
            if let Some(h) = FOUND_HOST {
                return Some(h);
            }
            // 兜底:WorkerW 没找到就用 Progman
            return Some(progman);
        }
        None
    }
}

/// 找到 Explorer 中承载桌面原生图标的 SysListView32。
pub(crate) fn find_desktop_listview() -> Option<HWND> {
    unsafe {
        FOUND_LIST = None;
        let host = find_desktop_host()?;
        let _ = EnumChildWindows(Some(host), Some(enum_list_proc), LPARAM(0));
        let first = FOUND_LIST;
        if first.is_none() {
            // Progman 作为兜底宿主时，图标列表可能多嵌套一层。
            if let Ok(progman) = FindWindowW(w!("Progman"), None) {
                let _ = EnumChildWindows(Some(progman), Some(enum_list_proc), LPARAM(0));
            }
        }
        FOUND_LIST
    }
}
