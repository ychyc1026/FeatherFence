//! 避让栅栏：把 Explorer 桌面 ListView 中落在栅栏矩形下的图标移到空闲网格。
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, ManuallyDrop};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 撤销窗口：图标/栅栏被搬移后 1 分钟内可「撤销并关闭避让」,超时记录作废,
/// 防止长期开启避让时 static 历史无限累积。
const ROLLBACK_TTL_SECS: u64 = 60;

/// 避让开启前桌面 ListView 的自动排列状态;关闭避让时按它原样恢复,
/// 避免把用户手动关掉自动排列的自定义布局强制打开。
static AUTOARRANGE_WAS_ON: AtomicBool = AtomicBool::new(false);
/// 避让期间被搬走的图标:index -> (搬移前坐标, 搬移时刻)。撤销时写回原位。
static ICON_HISTORY: Mutex<Option<HashMap<u32, (POINT, u64)>>> = Mutex::new(None);
/// 避让期间被移动/缩放的栅栏:id -> (移动前完整配置, 移动时刻)。撤销时恢复原状。
static FENCE_HISTORY: Mutex<Option<HashMap<u32, (crate::config::FenceCfg, u64)>>> =
    Mutex::new(None);

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IServiceProvider,
    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_VM_OPERATION, PROCESS_VM_READ,
};
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_I4,
};
use windows::Win32::UI::Controls::{
    LVM_GETITEMCOUNT, LVM_GETITEMPOSITION, LVM_GETITEMSPACING, LVM_SETITEMPOSITION,
    LVS_AUTOARRANGE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IFolderView, IShellBrowser, IShellFolder, IShellWindows, ShellWindows, CSIDL_DESKTOP,
    SID_STopLevelBrowser, SVSI_POSITIONITEM, SWC_DESKTOP, SWFO_NEEDDISPATCH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetClientRect, GetParent, GetWindowLongPtrW, GetWindowThreadProcessId,
    MsgWaitForMultipleObjectsEx, PeekMessageW, SendMessageTimeoutW, SendMessageW,
    SetWindowLongPtrW, TranslateMessage, WindowFromPoint, GWL_STYLE, MSG, MWMO_INPUTAVAILABLE,
    PM_NOREMOVE, PM_REMOVE, QS_ALLINPUT, SMTO_ABORTIFHUNG, SMTO_BLOCK,
};
use windows::core::{Interface, PCWSTR};

fn overlaps(a: RECT, b: RECT) -> bool {
    a.left < b.right && a.right > b.left && a.top < b.bottom && a.bottom > b.top
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `reserved_screen` 使用物理屏幕坐标，与顶层栅栏窗口的 cfg 坐标一致。
pub fn reserve(reserved_screen: &[RECT]) {
    if reserved_screen.is_empty() {
        return;
    }
    let Some(list) = crate::utils::find_desktop_listview() else {
        return;
    };
    unsafe {
        // 自动排列会立刻把移开的图标重新填回禁放区，因此启用栅栏避让时关闭该样式；
        // 同时记住原始状态，供关闭避让时原样恢复。
        let style = GetWindowLongPtrW(list, GWL_STYLE);
        if style & LVS_AUTOARRANGE as isize != 0 {
            AUTOARRANGE_WAS_ON.store(true, Ordering::Relaxed);
            SetWindowLongPtrW(list, GWL_STYLE, style & !(LVS_AUTOARRANGE as isize));
        }
        let mut reserved = Vec::with_capacity(reserved_screen.len());
        for r in reserved_screen {
            let mut tl = POINT {
                x: r.left,
                y: r.top,
            };
            let mut br = POINT {
                x: r.right,
                y: r.bottom,
            };
            if ScreenToClient(list, &mut tl).as_bool() && ScreenToClient(list, &mut br).as_bool() {
                reserved.push(RECT {
                    left: tl.x,
                    top: tl.y,
                    right: br.x,
                    bottom: br.y,
                });
            }
        }
        if reserved.is_empty() {
            return;
        }

        let count = SendMessageW(list, LVM_GETITEMCOUNT, Some(WPARAM(0)), Some(LPARAM(0))).0 as i32;
        if count <= 0 {
            return;
        }
        let packed =
            SendMessageW(list, LVM_GETITEMSPACING, Some(WPARAM(0)), Some(LPARAM(0))).0 as u32;
        let cell_w = ((packed & 0xffff) as i32).max(48);
        let cell_h = ((packed >> 16) as i32).max(48);
        let mut client = RECT::default();
        if GetClientRect(list, &mut client).is_err() {
            return;
        }

        let mut pid = 0u32;
        GetWindowThreadProcessId(list, Some(&mut pid));
        let Ok(process) = OpenProcess(PROCESS_VM_OPERATION | PROCESS_VM_READ, false, pid) else {
            return;
        };
        let remote = VirtualAllocEx(
            process,
            None,
            size_of::<POINT>(),
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        if remote.is_null() {
            let _ = CloseHandle(process);
            return;
        }

        let mut positions = Vec::with_capacity(count as usize);
        for i in 0..count {
            let ok = SendMessageW(
                list,
                LVM_GETITEMPOSITION,
                Some(WPARAM(i as usize)),
                Some(LPARAM(remote as isize)),
            )
            .0 != 0;
            let mut p = POINT::default();
            if ok
                && ReadProcessMemory(
                    process,
                    remote,
                    &mut p as *mut POINT as *mut c_void,
                    size_of::<POINT>(),
                    None,
                )
                .is_ok()
            {
                positions.push(p);
            } else {
                positions.push(POINT {
                    x: -10000,
                    y: -10000,
                });
            }
        }

        let item_rect = |p: POINT| RECT {
            left: p.x,
            top: p.y,
            right: p.x + cell_w,
            bottom: p.y + cell_h,
        };
        let blocked = |p: POINT| reserved.iter().any(|r| overlaps(item_rect(p), *r));
        let collides = |p: POINT, skip: usize, all: &[POINT]| {
            all.iter().enumerate().any(|(i, q)| {
                i != skip && (p.x - q.x).abs() < cell_w / 2 && (p.y - q.y).abs() < cell_h / 2
            })
        };

        for idx in 0..positions.len() {
            if !blocked(positions[idx]) {
                continue;
            }
            let mut chosen = None;
            // 就近搬移：以图标当前位置为圆心,找最近的空闲网格(不在栅栏下、
            // 也不与任何图标重叠),尽量不打乱既有布局。仅搬走被栅栏盖住的图标。
            let ox = positions[idx].x;
            let oy = positions[idx].y;
            let mut best_d2 = i64::MAX;
            let max_x = (client.right - cell_w).max(0);
            let max_y = (client.bottom - cell_h).max(0);
            let mut x = 0;
            while x <= max_x {
                let mut y = 0;
                while y <= max_y {
                    let p = POINT { x, y };
                    if !blocked(p) && !collides(p, idx, &positions) {
                        let d2 = (x as i64 - ox as i64).pow(2) + (y as i64 - oy as i64).pow(2);
                        if d2 < best_d2 {
                            best_d2 = d2;
                            chosen = Some(p);
                        }
                    }
                    y += cell_h;
                }
                x += cell_w;
            }
            if let Some(p) = chosen {
                // 记录搬移前的原始位置(每个图标只记一次),供「撤销并关闭避让」写回;
                // 同时按存活时间清理过期记录,防止长期使用无限累积。
                {
                    let mut guard = ICON_HISTORY.lock().unwrap_or_else(|e| e.into_inner());
                    let map = guard.get_or_insert_with(HashMap::new);
                    let now = now_unix();
                    map.retain(|_, (_, t)| now.saturating_sub(*t) < ROLLBACK_TTL_SECS);
                    map.entry(idx as u32).or_insert((positions[idx], now));
                }
                SendMessageW(
                    list,
                    LVM_SETITEMPOSITION,
                    Some(WPARAM(idx)),
                    Some(LPARAM(
                        (((p.y as u32) & 0xffff) << 16 | ((p.x as u32) & 0xffff)) as isize,
                    )),
                );
                positions[idx] = p;
            }
        }

        let _ = VirtualFreeEx(process, remote, 0, MEM_RELEASE);
        let _ = CloseHandle(process);
    }
}

const DROP_POSITION_RETRY_MS: u32 = 30;
const DROP_POSITION_EAGER_RETRY_MS: u32 = 8;
const DROP_POSITION_SETTLE_DELAY: Duration = Duration::from_millis(330);
const DROP_POSITION_TIMEOUT: Duration = Duration::from_millis(2000);
const POINTER_WATCH_POLL: Duration = Duration::from_millis(8);
const MAX_DROP_POSITION_WORKERS: u32 = 2;
static DROP_GENERATION: AtomicU64 = AtomicU64::new(0);
static ACTIVE_DROP_POSITION_WORKERS: AtomicU32 = AtomicU32::new(0);
static DIRECT_POSITION_WARM_LIST: AtomicUsize = AtomicUsize::new(0);
static DIRECT_POSITION_WARMING: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueDropPosition {
    Queued,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DropPositionOrigin {
    /// Explorer completed the file transfer and may still be settling its native drop layout.
    ExplorerDrop,
    /// FeatherFence completed an exact desktop MOVE and should position as soon as the item
    /// enters the folder view, without exposing the default first-empty-cell position for 330ms.
    DirectMove,
}

fn position_timing(origin: DropPositionOrigin) -> (Duration, Duration) {
    match origin {
        DropPositionOrigin::ExplorerDrop => (
            DROP_POSITION_SETTLE_DELAY,
            Duration::from_millis(DROP_POSITION_RETRY_MS as u64),
        ),
        DropPositionOrigin::DirectMove => (
            Duration::ZERO,
            Duration::from_millis(DROP_POSITION_EAGER_RETRY_MS as u64),
        ),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PositionAttemptGate {
    Continue,
    Expired,
    Cancelled,
}

fn position_attempt_gate(release_age: Duration, new_input: bool) -> PositionAttemptGate {
    if release_age >= DROP_POSITION_TIMEOUT {
        PositionAttemptGate::Expired
    } else if new_input {
        PositionAttemptGate::Cancelled
    } else {
        PositionAttemptGate::Continue
    }
}

fn pointer_state_has_press(state: u16) -> bool {
    state & 0x8001 != 0
}

struct DesktopDropJob {
    generation: u64,
    origin: DropPositionOrigin,
    label: String,
    label_w: Vec<u16>,
    screen_point: POINT,
    list_value: usize,
    desired: POINT,
    desired_ready: bool,
    started: Instant,
    released_at: Instant,
    not_before: Instant,
    retry_delay: Duration,
    attempts: u32,
    view_attempts: u32,
    resolve_attempts: u32,
    probe_attempts: u32,
    position_attempts: u32,
    appeared: bool,
    trace: crate::perf::DropTrace,
    total_started: Option<Instant>,
}

fn variant_i4(value: i32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_I4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { lVal: value },
            }),
        },
    }
}

unsafe fn desktop_folder_view() -> windows::core::Result<IFolderView> {
    let shell_windows: IShellWindows =
        unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER)? };
    let location = variant_i4(CSIDL_DESKTOP as i32);
    let root = VARIANT::default();
    let mut desktop_hwnd = 0i32;
    let dispatch = unsafe {
        shell_windows.FindWindowSW(
            &location,
            &root,
            SWC_DESKTOP,
            &mut desktop_hwnd,
            SWFO_NEEDDISPATCH,
        )?
    };
    let services: IServiceProvider = dispatch.cast()?;
    let browser: IShellBrowser = unsafe { services.QueryService(&SID_STopLevelBrowser)? };
    let shell_view = unsafe { browser.QueryActiveShellView()? };
    shell_view.cast()
}

/// Warm the out-of-process Shell view on a short-lived STA thread. The first ShellWindows query
/// can take hundreds of milliseconds; the direct MOVE path must never publish a desktop item
/// while paying that cold-start cost, otherwise Explorer visibly paints its default position.
pub(crate) fn warm_direct_drop_positioner() {
    let Some(list) = crate::utils::find_desktop_listview() else {
        return;
    };
    let list_value = list.0 as usize;
    if DIRECT_POSITION_WARM_LIST.load(Ordering::Acquire) == list_value
        || DIRECT_POSITION_WARMING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return;
    }
    let spawn = thread::Builder::new()
        .name("feather-desktop-position-warmup".into())
        .spawn(move || {
            let started = Instant::now();
            let initialized = unsafe {
                CoInitializeEx(
                    None,
                    COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE,
                )
            };
            let ready = if initialized.is_ok() {
                prepare_sta_message_queue();
                let acquired = unsafe { desktop_folder_view() };
                let ready = acquired.is_ok();
                drop(acquired);
                unsafe { CoUninitialize() };
                ready
            } else {
                false
            };
            if ready
                && crate::utils::find_desktop_listview()
                    .is_some_and(|current| current.0 as usize == list_value)
            {
                DIRECT_POSITION_WARM_LIST.store(list_value, Ordering::Release);
            }
            DIRECT_POSITION_WARMING.store(false, Ordering::Release);
            if crate::perf::enabled() {
                crate::dlog(&format!(
                    "[desktop-drop] positioner_warmup ready={ready} elapsed_us={}",
                    started.elapsed().as_micros()
                ));
            }
        });
    if spawn.is_err() {
        DIRECT_POSITION_WARMING.store(false, Ordering::Release);
    }
}

/// Fail closed when Explorer rebuilt the desktop view or warm-up has not completed. That drag
/// remains on Explorer's native Drop path; a following drag can use the fast path once ready.
pub(crate) fn direct_drop_positioner_ready() -> bool {
    let Some(list) = crate::utils::find_desktop_listview() else {
        return false;
    };
    let ready = DIRECT_POSITION_WARM_LIST.load(Ordering::Acquire) == list.0 as usize;
    if !ready {
        warm_direct_drop_positioner();
    }
    ready
}

struct OwnedPidl(*mut ITEMIDLIST);

impl Drop for OwnedPidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0 as *const c_void)) };
    }
}

unsafe fn desktop_child_pidl(view: &IFolderView, label_w: &[u16]) -> Option<OwnedPidl> {
    let folder: IShellFolder = unsafe { view.GetFolder().ok()? };
    let mut raw = std::ptr::null_mut();
    let mut attributes = 0u32;
    unsafe {
        folder
            .ParseDisplayName(
                HWND::default(),
                None,
                PCWSTR(label_w.as_ptr()),
                None,
                &mut raw,
                &mut attributes,
            )
            .ok()?;
    }
    (!raw.is_null()).then_some(OwnedPidl(raw))
}

fn centered_icon_position(cursor: POINT, client: RECT, cell_w: i32, cell_h: i32) -> POINT {
    let max_x = (client.right - cell_w).max(client.left);
    let max_y = (client.bottom - cell_h).max(client.top);
    POINT {
        x: (cursor.x - cell_w / 2).clamp(client.left, max_x),
        y: (cursor.y - cell_h / 2).clamp(client.top, max_y),
    }
}

unsafe fn point_hits_desktop_list(list: HWND, screen: POINT) -> bool {
    let mut hit = unsafe { WindowFromPoint(screen) };
    for _ in 0..10 {
        if hit == list {
            return true;
        }
        if hit.is_invalid() {
            return false;
        }
        let Ok(parent) = (unsafe { GetParent(hit) }) else {
            return false;
        };
        if parent.is_invalid() || parent == hit {
            return false;
        }
        hit = parent;
    }
    false
}

unsafe fn is_same_or_descendant(mut window: HWND, ancestor: HWND) -> bool {
    for _ in 0..10 {
        if window == ancestor {
            return true;
        }
        if window.is_invalid() {
            return false;
        }
        let Ok(parent) = (unsafe { GetParent(window) }) else {
            return false;
        };
        if parent.is_invalid() || parent == window {
            return false;
        }
        window = parent;
    }
    false
}

unsafe fn target_belongs_to_desktop_list(target: HWND, list: HWND) -> bool {
    // OLE reports a potential drop-target window. Explorer may register the ListView itself or
    // one of its parents (WorkerW on current Windows). Require that exact target to be an
    // ancestor of the one desktop ListView; accepting descendants could swallow a future child
    // control's independent drop behavior.
    unsafe { is_same_or_descendant(list, target) }
}

/// True when the release point belongs to Explorer's desktop ListView rather than a fence or
/// another Explorer window. This is also used to keep our desktop shortcut collector from
/// immediately reclaiming a shortcut that the user just dragged out of a fence.
pub fn is_desktop_drop_point(screen_point: POINT) -> bool {
    let Some(list) = crate::utils::find_desktop_listview() else {
        return false;
    };
    unsafe { point_hits_desktop_list(list, screen_point) }
}

/// Check an empty desktop cell using OLE's current registered target instead of
/// `WindowFromPoint`. During a drag the latter can see the transient drag-image window.
pub fn is_empty_desktop_drop_target(target: HWND, screen_point: POINT) -> bool {
    if !direct_drop_positioner_ready() {
        return false;
    }
    let Some(list) = crate::utils::find_desktop_listview() else {
        return false;
    };
    unsafe {
        // Automatic arrangement would immediately override the captured coordinate. Keep that
        // drag on Explorer's native path instead of cancelling it and failing after release.
        if GetWindowLongPtrW(list, GWL_STYLE) & LVS_AUTOARRANGE as isize != 0 {
            return false;
        }
        if target.is_invalid() || !target_belongs_to_desktop_list(target, list) {
            if crate::perf::enabled() {
                crate::dlog(&format!(
                    "[desktop-fast-path] point=({}, {}) result=wrong_target target=0x{:x} list=0x{:x}",
                    screen_point.x,
                    screen_point.y,
                    target.0 as usize,
                    list.0 as usize,
                ));
            }
            return false;
        }
        is_empty_desktop_list_cell(list, screen_point)
    }
}

fn desktop_hit_test_timeout(deadline: Instant) -> Option<u32> {
    let remaining = deadline.checked_duration_since(Instant::now())?;
    if remaining.is_zero() {
        return None;
    }
    Some(remaining.as_millis().clamp(1, 50) as u32)
}

unsafe fn is_empty_desktop_list_cell(list: HWND, screen_point: POINT) -> bool {
    unsafe {
        let deadline = Instant::now() + Duration::from_millis(75);
        let mut client_point = screen_point;
        if !ScreenToClient(list, &mut client_point).as_bool() {
            return false;
        }
        let mut client = RECT::default();
        if GetClientRect(list, &mut client).is_err()
            || client_point.x < client.left
            || client_point.x >= client.right
            || client_point.y < client.top
            || client_point.y >= client.bottom
        {
            return false;
        }
        let mut count_result = 0usize;
        let Some(timeout_ms) = desktop_hit_test_timeout(deadline) else {
            return false;
        };
        if SendMessageTimeoutW(
            list,
            LVM_GETITEMCOUNT,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            timeout_ms,
            Some(&mut count_result),
        )
        .0
            == 0
        {
            return false;
        }
        let count = count_result as i32;
        if count <= 0 {
            return true;
        }
        let mut spacing_result = 0usize;
        let Some(timeout_ms) = desktop_hit_test_timeout(deadline) else {
            return false;
        };
        if SendMessageTimeoutW(
            list,
            LVM_GETITEMSPACING,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            timeout_ms,
            Some(&mut spacing_result),
        )
        .0
            == 0
        {
            return false;
        }
        let packed = spacing_result as u32;
        let cell_w = ((packed & 0xffff) as i32).max(48);
        let cell_h = ((packed >> 16) as i32).max(48);

        let mut pid = 0u32;
        GetWindowThreadProcessId(list, Some(&mut pid));
        let Ok(process) = OpenProcess(PROCESS_VM_OPERATION | PROCESS_VM_READ, false, pid) else {
            return false;
        };
        let remote = VirtualAllocEx(
            process,
            None,
            size_of::<POINT>(),
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        if remote.is_null() {
            let _ = CloseHandle(process);
            return false;
        }

        let mut empty = true;
        for index in 0..count {
            let Some(timeout_ms) = desktop_hit_test_timeout(deadline) else {
                if crate::perf::enabled() {
                    crate::dlog(&format!(
                        "[desktop-fast-path] point=({}, {}) result=budget_exhausted index={index}",
                        screen_point.x, screen_point.y
                    ));
                }
                empty = false;
                break;
            };
            let mut position_result = 0usize;
            let sent = SendMessageTimeoutW(
                list,
                LVM_GETITEMPOSITION,
                WPARAM(index as usize),
                LPARAM(remote as isize),
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                timeout_ms,
                Some(&mut position_result),
            );
            let mut position = POINT::default();
            if sent.0 == 0
                || position_result == 0
                || ReadProcessMemory(
                    process,
                    remote,
                    &mut position as *mut POINT as *mut c_void,
                    size_of::<POINT>(),
                    None,
                )
                .is_err()
            {
                if crate::perf::enabled() {
                    crate::dlog(&format!(
                        "[desktop-fast-path] point=({}, {}) result=position_read_failed index={index}",
                        screen_point.x, screen_point.y
                    ));
                }
                empty = false;
                break;
            }
            if client_point.x >= position.x
                && client_point.x < position.x + cell_w
                && client_point.y >= position.y
                && client_point.y < position.y + cell_h
            {
                if crate::perf::enabled() {
                    crate::dlog(&format!(
                        "[desktop-fast-path] point=({}, {}) result=occupied index={index} icon=({}, {}) cell={}x{}",
                        screen_point.x,
                        screen_point.y,
                        position.x,
                        position.y,
                        cell_w,
                        cell_h,
                    ));
                }
                empty = false;
                break;
            }
        }
        let _ = VirtualFreeEx(process, remote, 0, MEM_RELEASE);
        let _ = CloseHandle(process);
        if empty && crate::perf::enabled() {
            crate::dlog(&format!(
                "[desktop-fast-path] point=({}, {}) result=empty",
                screen_point.x, screen_point.y
            ));
        }
        empty
    }
}

unsafe fn desired_drop_position(list: HWND, screen_point: POINT) -> Result<POINT, &'static str> {
    let style = unsafe { GetWindowLongPtrW(list, GWL_STYLE) };
    if style & LVS_AUTOARRANGE as isize != 0 {
        return Err("auto_arrange");
    }

    let mut client_point = screen_point;
    if !unsafe { ScreenToClient(list, &mut client_point) }.as_bool() {
        return Err("screen_to_client_failed");
    }
    let mut client = RECT::default();
    if unsafe { GetClientRect(list, &mut client) }.is_err() {
        return Err("client_rect_failed");
    }
    let mut packed = 0usize;
    if unsafe {
        SendMessageTimeoutW(
            list,
            LVM_GETITEMSPACING,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            50,
            Some(&mut packed),
        )
    }
    .0 == 0
    {
        return Err("item_spacing_timeout");
    }
    let packed = packed as u32;
    let cell_w = ((packed & 0xffff) as i32).max(48);
    let cell_h = ((packed >> 16) as i32).max(48);
    Ok(centered_icon_position(
        client_point,
        client,
        cell_w,
        cell_h,
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DesktopNameState {
    Absent,
    Present,
    Unknown,
}

/// Snapshot the ambiguous leaf name immediately before entering OLE. A pre-existing item in
/// either physical desktop folder makes Explorer's merged-view PIDL ambiguous, so positioning is
/// skipped. Errors fail closed. `symlink_metadata` also counts broken reparse points as present.
pub(crate) fn snapshot_desktop_name(source_path: &Path) -> DesktopNameState {
    let Some(name) = source_path.file_name() else {
        return DesktopNameState::Unknown;
    };
    let (Some(desktop), Some(public_desktop)) =
        (crate::desktop_dir(), crate::public_desktop_dir())
    else {
        return DesktopNameState::Unknown;
    };
    let mut roots = vec![desktop];
    if public_desktop != roots[0] {
        roots.push(public_desktop);
    }
    let mut unknown = false;
    for root in roots {
        match std::fs::symlink_metadata(root.join(name)) {
            Ok(_) => return DesktopNameState::Present,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => unknown = true,
        }
    }
    if unknown {
        DesktopNameState::Unknown
    } else {
        DesktopNameState::Absent
    }
}

fn next_drop_generation() -> u64 {
    let next = DROP_GENERATION
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1);
    if next == 0 {
        DROP_GENERATION.fetch_add(1, Ordering::AcqRel).wrapping_add(1)
    } else {
        next
    }
}

fn current_drop_generation() -> u64 {
    DROP_GENERATION.load(Ordering::Acquire)
}

fn finish_drop_job(job: DesktopDropJob, outcome: &'static str, positioned: bool) {
    job.trace
        .finish_stage("desktop_position_total", job.total_started, || {
            format!(
                "scope=aggregate ok={positioned} outcome={outcome} attempts={} view_attempts={} resolve_attempts={} probe_attempts={} position_attempts={} appeared={} wall_us={}",
                job.attempts,
                job.view_attempts,
                job.resolve_attempts,
                job.probe_attempts,
                job.position_attempts,
                job.appeared,
                job.started.elapsed().as_micros(),
            )
        });
    job.trace.finish(outcome, || {
        format!(
            "desktop=true positioned={positioned} item=\"{}\" release_x={} release_y={} requested_ready={} requested_x={} requested_y={} release_age_us={}",
            job.label,
            job.screen_point.x,
            job.screen_point.y,
            job.desired_ready,
            job.desired.x,
            job.desired.y,
            job.released_at.elapsed().as_micros(),
        )
    });
}

#[derive(Clone, Copy)]
struct WorkerOutcome {
    outcome: &'static str,
    positioned: bool,
}

fn worker_gate(job: &DesktopDropJob, new_pointer_press: &AtomicBool) -> Option<WorkerOutcome> {
    if current_drop_generation() != job.generation {
        return Some(WorkerOutcome {
            outcome: "desktop_position_superseded",
            positioned: false,
        });
    }
    match position_attempt_gate(
        job.released_at.elapsed(),
        new_pointer_press.load(Ordering::Acquire),
    ) {
        PositionAttemptGate::Expired => Some(WorkerOutcome {
                outcome: "desktop_position_expired",
                positioned: false,
        }),
        PositionAttemptGate::Cancelled => Some(WorkerOutcome {
                outcome: "desktop_position_cancelled_by_input",
                positioned: false,
        }),
        PositionAttemptGate::Continue => None,
    }
}

fn prepare_sta_message_queue() {
    let mut message = MSG::default();
    unsafe {
        let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
    }
}

fn wait_with_sta_message_pump(duration: Duration) {
    let deadline = Instant::now() + duration;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let timeout_ms = remaining.as_millis().clamp(1, u32::MAX as u128) as u32;
        unsafe {
            let _ = MsgWaitForMultipleObjectsEx(
                None,
                timeout_ms,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
}

fn worker_pause(
    job: &DesktopDropJob,
    new_pointer_press: &AtomicBool,
    duration: Duration,
) -> Option<WorkerOutcome> {
    let remaining = DROP_POSITION_TIMEOUT.saturating_sub(job.released_at.elapsed());
    if !remaining.is_zero() {
        wait_with_sta_message_pump(duration.min(remaining));
    }
    worker_gate(job, new_pointer_press)
}

unsafe fn select_job_position(
    job: &mut DesktopDropJob,
    view: &IFolderView,
    pidl: &OwnedPidl,
) -> bool {
    let child = pidl.0 as *const ITEMIDLIST;
    job.position_attempts += 1;
    let position_started = job.trace.stage_start();
    let positioned = unsafe {
        view.SelectAndPositionItems(
            1,
            &child,
            Some(&job.desired),
            SVSI_POSITIONITEM.0 as u32,
        )
        .is_ok()
    };
    job.trace
        .finish_stage("desktop_position", position_started, || {
            format!(
                "scope=worker attempt={} ok={positioned} requested_x={} requested_y={}",
                job.position_attempts, job.desired.x, job.desired.y
            )
        });
    crate::dlog(&format!(
        "[desktop-drop] item=\"{}\" method=folder-view-worker release=({}, {}) requested=({}, {}) positioned={}",
        job.label,
        job.screen_point.x,
        job.screen_point.y,
        job.desired.x,
        job.desired.y,
        positioned
    ));
    positioned
}

unsafe fn position_on_worker(
    job: &mut DesktopDropJob,
    new_pointer_press: &AtomicBool,
    initial_view: Option<IFolderView>,
) -> WorkerOutcome {
    while Instant::now() < job.not_before {
        if let Some(outcome) = worker_gate(job, new_pointer_press) {
            return outcome;
        }
        let remaining = job.not_before.saturating_duration_since(Instant::now());
        if let Some(outcome) = worker_pause(
            job,
            new_pointer_press,
            job.retry_delay.min(remaining),
        ) {
            return outcome;
        }
    }

    let mut list = HWND(job.list_value as *mut c_void);
    let mut view = initial_view;
    let mut pidl: Option<OwnedPidl> = None;
    let mut consecutive_resolve_failures = 0u8;
    let mut consecutive_probe_failures = 0u8;

    loop {
        if let Some(outcome) = worker_gate(job, new_pointer_press) {
            return outcome;
        }
        job.attempts += 1;

        let Some(current_list) = crate::utils::find_desktop_listview() else {
            pidl.take();
            view.take();
            if let Some(outcome) = worker_pause(
                job,
                new_pointer_press,
                job.retry_delay,
            ) {
                return outcome;
            }
            continue;
        };
        if current_list != list {
            pidl.take();
            view.take();
            job.desired_ready = false;
            consecutive_resolve_failures = 0;
            consecutive_probe_failures = 0;
            list = current_list;
            job.list_value = current_list.0 as usize;
        }
        if unsafe { GetWindowLongPtrW(current_list, GWL_STYLE) } & LVS_AUTOARRANGE as isize != 0 {
            return WorkerOutcome {
                outcome: "desktop_position_auto_arrange",
                positioned: false,
            };
        }

        if view.is_none() {
            job.view_attempts += 1;
            let started = job.trace.stage_start();
            let result = unsafe { desktop_folder_view() };
            job.trace.finish_stage("desktop_view", started, || {
                format!(
                    "scope=worker attempt={} ok={}",
                    job.view_attempts,
                    result.is_ok()
                )
            });
            match result {
                Ok(folder_view) => view = Some(folder_view),
                Err(_) => {
                    if let Some(outcome) = worker_pause(
                        job,
                        new_pointer_press,
                        job.retry_delay,
                    ) {
                        return outcome;
                    }
                    continue;
                }
            }
            if let Some(outcome) = worker_gate(job, new_pointer_press) {
                return outcome;
            }
        }

        if pidl.is_none() {
            job.resolve_attempts += 1;
            let started = job.trace.stage_start();
            let resolved = unsafe {
                desktop_child_pidl(view.as_ref().expect("view acquired"), &job.label_w)
            };
            job.trace
                .finish_stage("desktop_resolve_item", started, || {
                    format!(
                        "scope=worker attempt={} ok={}",
                        job.resolve_attempts,
                        resolved.is_some()
                    )
                });
            if let Some(outcome) = worker_gate(job, new_pointer_press) {
                return outcome;
            }
            match resolved {
                Some(child) => {
                    consecutive_resolve_failures = 0;
                    pidl = Some(child);
                }
                None => {
                    consecutive_resolve_failures = consecutive_resolve_failures.saturating_add(1);
                    if consecutive_resolve_failures >= 3 {
                        view.take();
                        consecutive_resolve_failures = 0;
                    }
                    if let Some(outcome) = worker_pause(
                        job,
                        new_pointer_press,
                        job.retry_delay,
                    ) {
                        return outcome;
                    }
                    continue;
                }
            }
        }

        if job.origin == DropPositionOrigin::DirectMove {
            let Some(final_list) = crate::utils::find_desktop_listview() else {
                return WorkerOutcome {
                    outcome: "desktop_position_view_lost_before_write",
                    positioned: false,
                };
            };
            if final_list != list {
                pidl.take();
                view.take();
                job.desired_ready = false;
                consecutive_resolve_failures = 0;
                consecutive_probe_failures = 0;
                list = final_list;
                job.list_value = final_list.0 as usize;
                continue;
            }
            if !job.desired_ready {
                job.desired = match unsafe { desired_drop_position(final_list, job.screen_point) } {
                    Ok(desired) => desired,
                    Err("auto_arrange") => {
                        return WorkerOutcome {
                            outcome: "desktop_position_auto_arrange",
                            positioned: false,
                        };
                    }
                    Err(_) => {
                        return WorkerOutcome {
                            outcome: "desktop_position_geometry_failed_before_write",
                            positioned: false,
                        };
                    }
                };
                job.desired_ready = true;
            }
            if let Some(outcome) = worker_gate(job, new_pointer_press) {
                return outcome;
            }
            let positioned = unsafe {
                select_job_position(
                    job,
                    view.as_ref().expect("view acquired"),
                    pidl.as_ref().expect("pidl resolved"),
                )
            };
            if positioned {
                job.appeared = true;
                return WorkerOutcome {
                    outcome: "desktop_positioned_eagerly",
                    positioned: true,
                };
            }
            consecutive_probe_failures = consecutive_probe_failures.saturating_add(1);
            if consecutive_probe_failures >= 3 {
                pidl.take();
                view.take();
                consecutive_probe_failures = 0;
                consecutive_resolve_failures = 0;
            }
            if let Some(outcome) = worker_pause(job, new_pointer_press, job.retry_delay) {
                return outcome;
            }
            continue;
        }

        job.probe_attempts += 1;
        let probe_started = job.trace.stage_start();
        let position = unsafe {
            view
                .as_ref()
                .expect("view acquired")
                .GetItemPosition(pidl.as_ref().expect("pidl resolved").0)
        };
        job.trace
            .finish_stage("desktop_position_probe", probe_started, || {
                format!(
                    "scope=worker attempt={} ok={}",
                    job.probe_attempts,
                    position.is_ok()
                )
            });
        if let Some(outcome) = worker_gate(job, new_pointer_press) {
            return outcome;
        }
        let position = match position {
            Ok(position) => {
                job.appeared = true;
                consecutive_probe_failures = 0;
                position
            }
            Err(_) => {
                consecutive_probe_failures = consecutive_probe_failures.saturating_add(1);
                if consecutive_probe_failures >= 3 {
                    pidl.take();
                    view.take();
                    consecutive_probe_failures = 0;
                    consecutive_resolve_failures = 0;
                }
                if let Some(outcome) = worker_pause(
                    job,
                    new_pointer_press,
                    job.retry_delay,
                ) {
                    return outcome;
                }
                continue;
            }
        };

        let Some(final_list) = crate::utils::find_desktop_listview() else {
            return WorkerOutcome {
                outcome: "desktop_position_view_lost_before_write",
                positioned: false,
            };
        };
        if final_list != list {
            pidl.take();
            view.take();
            job.desired_ready = false;
            consecutive_resolve_failures = 0;
            consecutive_probe_failures = 0;
            list = final_list;
            job.list_value = final_list.0 as usize;
            continue;
        }
        job.desired = match unsafe { desired_drop_position(final_list, job.screen_point) } {
            Ok(desired) => desired,
            Err("auto_arrange") => {
                return WorkerOutcome {
                    outcome: "desktop_position_auto_arrange",
                    positioned: false,
                };
            }
            Err(_) => {
                return WorkerOutcome {
                    outcome: "desktop_position_geometry_failed_before_write",
                    positioned: false,
                };
            }
        };
        job.desired_ready = true;
        if crate::utils::find_desktop_listview() != Some(list) {
            pidl.take();
            view.take();
            continue;
        }
        if let Some(outcome) = worker_gate(job, new_pointer_press) {
            return outcome;
        }
        if position.x == job.desired.x && position.y == job.desired.y {
            return WorkerOutcome {
                outcome: "desktop_already_positioned",
                positioned: true,
            };
        }

        let positioned = unsafe {
            select_job_position(
                job,
                view.as_ref().expect("view acquired"),
                pidl.as_ref().expect("pidl resolved"),
            )
        };
        return WorkerOutcome {
            outcome: if positioned {
                "desktop_positioned"
            } else {
                "desktop_position_failed"
            },
            positioned,
        };
    }
}

struct PointerPressWatch {
    pressed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl PointerPressWatch {
    fn start(generation: u64, deadline: Instant) -> std::io::Result<Self> {
        let pressed = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_pressed = Arc::clone(&pressed);
        let thread_stop = Arc::clone(&stop);
        thread::Builder::new()
            .name(format!("feather-pointer-watch-{generation}"))
            .spawn(move || {
                while Instant::now() < deadline && !thread_stop.load(Ordering::Acquire) {
                    let state = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } as u16;
                    if pointer_state_has_press(state) {
                        thread_pressed.store(true, Ordering::Release);
                        break;
                    }
                    thread::sleep(POINTER_WATCH_POLL);
                }
            })?;
        Ok(Self { pressed, stop })
    }
}

impl Drop for PointerPressWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

struct ActiveDropWorkerGuard;

impl Drop for ActiveDropWorkerGuard {
    fn drop(&mut self) {
        ACTIVE_DROP_POSITION_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn run_drop_position_worker(mut job: DesktopDropJob) {
    let _active_guard = ActiveDropWorkerGuard;
    let pointer_watch = match PointerPressWatch::start(
        job.generation,
        job.released_at + DROP_POSITION_TIMEOUT,
    ) {
        Ok(watch) => watch,
        Err(_) => {
            finish_drop_job(
                job,
                "desktop_position_pointer_watch_failed",
                false,
            );
            return;
        }
    };
    let initialized = unsafe {
        CoInitializeEx(
            None,
            COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE,
        )
    };
    let outcome = if initialized.is_ok() {
        prepare_sta_message_queue();
        let outcome = unsafe { position_on_worker(&mut job, &pointer_watch.pressed, None) };
        unsafe { CoUninitialize() };
        outcome
    } else {
        WorkerOutcome {
            outcome: "desktop_position_worker_com_init_failed",
            positioned: false,
        }
    };
    finish_drop_job(job, outcome.outcome, outcome.positioned);
}

/// Prepared on the existing desktop-transfer worker before the file enters the Desktop folder.
/// Keeping the Shell view alive on that same STA lets the worker submit the release coordinate
/// before explicitly publishing the filesystem change to Explorer.
pub(crate) struct PreparedDirectDropPosition {
    job: Option<DesktopDropJob>,
    view: Option<IFolderView>,
    pointer_watch: PointerPressWatch,
    outcome: Option<WorkerOutcome>,
}

impl PreparedDirectDropPosition {
    /// Position the item after the atomic move and before the caller publishes its Shell change.
    pub(crate) fn position_after_move(&mut self) -> bool {
        if self.outcome.is_none() {
            let job = self.job.as_mut().expect("prepared direct-drop job");
            self.outcome = Some(unsafe {
                position_on_worker(
                    job,
                    &self.pointer_watch.pressed,
                    self.view.take(),
                )
            });
        }
        self.outcome.is_some_and(|outcome| outcome.positioned)
    }

    /// Finish the shared drag trace only after the caller has published the matching Shell event.
    pub(crate) fn finish_after_publish(mut self) -> bool {
        let outcome = self.outcome.unwrap_or(WorkerOutcome {
            outcome: "desktop_position_not_attempted",
            positioned: false,
        });
        let job = self.job.take().expect("prepared direct-drop job");
        finish_drop_job(job, outcome.outcome, outcome.positioned);
        outcome.positioned
    }
}

impl Drop for PreparedDirectDropPosition {
    fn drop(&mut self) {
        // IFolderView is apartment-affine. Release it on this worker before balancing COM.
        drop(self.view.take());
        unsafe { CoUninitialize() };
    }
}

/// Acquire the desktop Shell view and calculate the requested cell before moving the file.
/// Preparation failure is returned to the transfer worker so it can leave the source untouched.
pub(crate) fn prepare_direct_drop_position(
    expected_destination: &Path,
    screen_point: POINT,
    released_at: Instant,
    trace: crate::perf::DropTrace,
) -> Result<PreparedDirectDropPosition, String> {
    let total_started = trace.stage_start();
    let started = Instant::now();
    if released_at.elapsed() >= DROP_POSITION_TIMEOUT {
        return Err("desktop release expired before positioning was prepared".to_string());
    }
    let label_os = expected_destination
        .file_name()
        .ok_or_else(|| "desktop destination has no file name".to_string())?;
    let label = label_os.to_string_lossy().into_owned();
    let label_w: Vec<u16> = label_os.encode_wide().chain(std::iter::once(0)).collect();
    let list = crate::utils::find_desktop_listview()
        .ok_or_else(|| "desktop list view is unavailable".to_string())?;
    if DIRECT_POSITION_WARM_LIST.load(Ordering::Acquire) != list.0 as usize {
        warm_direct_drop_positioner();
        return Err("desktop positioner is not ready for the current view".to_string());
    }
    let desired = unsafe { desired_drop_position(list, screen_point) }
        .map_err(|reason| format!("desktop drop geometry is unavailable: {reason}"))?;

    // Clear the completed drag transition before the watcher starts looking for a new click.
    let button_baseline = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } as u16;
    if pointer_state_has_press(button_baseline) {
        return Err("new pointer interaction started before positioning".to_string());
    }
    let generation = next_drop_generation();
    let pointer_watch = PointerPressWatch::start(
        generation,
        released_at + DROP_POSITION_TIMEOUT,
    )
    .map_err(|error| format!("could not start desktop pointer guard: {error}"))?;

    let initialized = unsafe {
        CoInitializeEx(
            None,
            COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE,
        )
    };
    initialized
        .ok()
        .map_err(|error| format!("could not initialize desktop positioning STA: {error}"))?;
    prepare_sta_message_queue();

    let view_started = trace.stage_start();
    let view_result = unsafe { desktop_folder_view() };
    trace.finish_stage("desktop_view", view_started, || {
        format!("scope=prepared attempt=1 ok={}", view_result.is_ok())
    });
    let view = match view_result {
        Ok(view) => view,
        Err(error) => {
            unsafe { CoUninitialize() };
            return Err(format!("could not acquire desktop folder view: {error}"));
        }
    };
    if crate::utils::find_desktop_listview() != Some(list) {
        drop(view);
        unsafe { CoUninitialize() };
        return Err("desktop view changed while positioning was prepared".to_string());
    }

    let job = DesktopDropJob {
        generation,
        origin: DropPositionOrigin::DirectMove,
        label,
        label_w,
        screen_point,
        list_value: list.0 as usize,
        desired,
        desired_ready: true,
        started,
        released_at,
        not_before: released_at,
        retry_delay: Duration::from_millis(DROP_POSITION_EAGER_RETRY_MS as u64),
        attempts: 0,
        view_attempts: 1,
        resolve_attempts: 0,
        probe_attempts: 0,
        position_attempts: 0,
        appeared: false,
        trace,
        total_started,
    };
    if let Some(outcome) = worker_gate(&job, &pointer_watch.pressed) {
        drop(view);
        unsafe { CoUninitialize() };
        return Err(format!("desktop positioning was cancelled: {}", outcome.outcome));
    }
    trace.event("desktop_position_prepared", || {
        format!(
            "scope=transfer-worker requested_x={} requested_y={} release_age_us={}",
            desired.x,
            desired.y,
            released_at.elapsed().as_micros(),
        )
    });
    Ok(PreparedDirectDropPosition {
        job: Some(job),
        view: Some(view),
        pointer_watch,
        outcome: None,
    })
}

/// Queue a bounded background-STA job that waits for Explorer to publish the new desktop item.
/// The UI thread performs only cheap validation and never runs Shell COM or sleeps.
pub(crate) fn queue_file_at_drop_point(
    _owner_hwnd: HWND,
    source_path: &Path,
    name_state: DesktopNameState,
    screen_point: POINT,
    released_at: Instant,
    origin: DropPositionOrigin,
    trace: crate::perf::DropTrace,
) -> QueueDropPosition {
    match name_state {
        DesktopNameState::Present => {
            trace.event("desktop_position_rejected", || {
                "reason=desktop_name_preexisting".to_string()
            });
            return QueueDropPosition::Rejected;
        }
        DesktopNameState::Unknown => {
            trace.event("desktop_position_rejected", || {
                "reason=desktop_name_unknown".to_string()
            });
            return QueueDropPosition::Rejected;
        }
        DesktopNameState::Absent => {}
    }
    let deadline = released_at + DROP_POSITION_TIMEOUT;
    if Instant::now() >= deadline {
        trace.event("desktop_position_rejected", || {
            format!(
                "reason=release_deadline_elapsed release_age_us={}",
                released_at.elapsed().as_micros()
            )
        });
        return QueueDropPosition::Rejected;
    }
    let Some(label_os) = source_path.file_name() else {
        trace.event("desktop_position_rejected", || "reason=missing_label".to_string());
        return QueueDropPosition::Rejected;
    };
    let label = label_os.to_string_lossy().into_owned();
    let Some(list) = crate::utils::find_desktop_listview() else {
        trace.event("desktop_position_rejected", || {
            "reason=desktop_list_missing".to_string()
        });
        return QueueDropPosition::Rejected;
    };
    if origin == DropPositionOrigin::ExplorerDrop
        && !unsafe { point_hits_desktop_list(list, screen_point) }
    {
        trace.event("desktop_position_rejected", || {
            "reason=release_not_on_desktop".to_string()
        });
        return QueueDropPosition::Rejected;
    }
    let generation = next_drop_generation();
    let label_w: Vec<u16> = label_os.encode_wide().chain(std::iter::once(0)).collect();
    let started = Instant::now();
    // Explorer-owned drops retain their publish grace period. A direct MOVE has already been
    // validated and completed by FeatherFence, so waiting here would deliberately expose the
    // default first-empty-cell position before moving the icon to the captured release point.
    let (settle_delay, retry_delay) = position_timing(origin);
    let not_before = released_at + settle_delay;
    let job = DesktopDropJob {
        generation,
        origin,
        label,
        label_w,
        screen_point,
        list_value: list.0 as usize,
        desired: POINT::default(),
        desired_ready: false,
        started,
        released_at,
        not_before,
        retry_delay,
        attempts: 0,
        view_attempts: 0,
        resolve_attempts: 0,
        probe_attempts: 0,
        position_attempts: 0,
        appeared: false,
        trace,
        total_started: trace.stage_start(),
    };

    // Clear the completed drag's transition. The short-lived watcher then observes only a new
    // left press, so keyboard releases and ordinary pointer movement do not cancel positioning.
    let button_baseline = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } as u16;
    if pointer_state_has_press(button_baseline) {
        trace.event("desktop_position_rejected", || {
            "reason=new_pointer_interaction".to_string()
        });
        return QueueDropPosition::Rejected;
    }
    if current_drop_generation() != generation {
        trace.event("desktop_position_rejected", || {
            "reason=superseded_before_queue".to_string()
        });
        return QueueDropPosition::Rejected;
    }
    if ACTIVE_DROP_POSITION_WORKERS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
            (active < MAX_DROP_POSITION_WORKERS).then_some(active + 1)
        })
        .is_err()
    {
        trace.event("desktop_position_rejected", || {
            "reason=worker_limit".to_string()
        });
        return QueueDropPosition::Rejected;
    }
    let spawn = thread::Builder::new()
        .name(format!("feather-desktop-drop-{generation}"))
        .spawn(move || run_drop_position_worker(job));
    if let Err(error) = spawn {
        ACTIVE_DROP_POSITION_WORKERS.fetch_sub(1, Ordering::AcqRel);
        trace.event("desktop_position_rejected", || {
            format!("reason=worker_spawn_failed error={error}")
        });
        return QueueDropPosition::Rejected;
    }
    trace.event("desktop_position_queued", || {
        format!(
            "scope=worker origin={origin:?} timeout_ms={} settle_delay_ms={} retry_ms={} pointer_watch_ms={} requested=pending",
            DROP_POSITION_TIMEOUT.as_millis(), settle_delay.as_millis(), retry_delay.as_millis(),
            POINTER_WATCH_POLL.as_millis(),
        )
    });
    QueueDropPosition::Queued
}

pub(crate) fn cancel_pending_drop_position(_owner_hwnd: HWND, _reason: &'static str) {
    next_drop_generation();
}

pub(crate) fn shutdown_drop_positioner(owner_hwnd: HWND) {
    cancel_pending_drop_position(owner_hwnd, "desktop_position_shutdown");
}

/// 记录栅栏移动/缩放前的配置快照(每个 id 只记一次)。由栅栏 WM_LBUTTONDOWN
/// 进入拖动/缩放时调用——此时窗口还在原位。
pub fn record_fence(cfg: &crate::config::FenceCfg) {
    let now = now_unix();
    let mut guard = FENCE_HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    map.retain(|_, (_, t)| now.saturating_sub(*t) < ROLLBACK_TTL_SECS);
    map.entry(cfg.id).or_insert((cfg.clone(), now));
}

/// 撤销图标搬移:把存活期内被搬走的图标写回原位。返回成功写回的数量。
pub fn rollback_icons() -> usize {
    let Some(list) = crate::utils::find_desktop_listview() else {
        clear_history();
        return 0;
    };
    unsafe {
        let now = now_unix();
        let mut restored = 0;
        let history = ICON_HISTORY.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(map) = history {
            for (idx, (p, t)) in &map {
                if now.saturating_sub(*t) >= ROLLBACK_TTL_SECS {
                    continue; // 已过期,视为在新位置稳定,不回退
                }
                let lp = (((p.y as u32) & 0xffff) << 16 | ((p.x as u32) & 0xffff)) as isize;
                let _ = SendMessageW(
                    list,
                    LVM_SETITEMPOSITION,
                    Some(WPARAM(*idx as usize)),
                    Some(LPARAM(lp)),
                );
                restored += 1;
            }
        }
        restored
    }
}

/// 取出存活期内的栅栏移动前快照(供恢复几何),并清空历史。
pub fn take_fence_history() -> Vec<(u32, crate::config::FenceCfg)> {
    let now = now_unix();
    let map = FENCE_HISTORY.lock().unwrap_or_else(|e| e.into_inner()).take();
    match map {
        Some(m) => m
            .into_iter()
            .filter(|(_, (_, t))| now.saturating_sub(*t) < ROLLBACK_TTL_SECS)
            .map(|(id, (cfg, _))| (id, cfg))
            .collect(),
        None => Vec::new(),
    }
}

/// 清空所有搬移/移动历史(关闭避让但不撤销时调用,图标保持当前位置)。
pub fn clear_history() {
    let _ = ICON_HISTORY.lock().unwrap_or_else(|e| e.into_inner()).take();
    let _ = FENCE_HISTORY.lock().unwrap_or_else(|e| e.into_inner()).take();
}

/// 关闭避让时调用：仅恢复桌面 ListView 在避让开启前的自动排列状态
/// (只当避让开启时由我们关掉自动排列才重新打开;用户原本手动关掉的
/// 自定义布局保持原样,不被强制吸附)。图标位置不做任何改动。
pub fn restore_autoarrange() {
    let Some(list) = crate::utils::find_desktop_listview() else {
        return;
    };
    unsafe {
        let style = GetWindowLongPtrW(list, GWL_STYLE);
        if AUTOARRANGE_WAS_ON.swap(false, Ordering::Relaxed)
            && style & LVS_AUTOARRANGE as isize == 0
        {
            let _ = SetWindowLongPtrW(list, GWL_STYLE, style | (LVS_AUTOARRANGE as isize));
        }
    }
}

#[cfg(test)]
mod drop_position_tests {
    use super::{
        centered_icon_position, desktop_hit_test_timeout, pointer_state_has_press,
        position_attempt_gate, position_timing, DropPositionOrigin, PositionAttemptGate,
    };
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{POINT, RECT};

    #[test]
    fn release_point_centres_and_clamps_the_icon_cell() {
        let client = RECT {
            left: 0,
            top: 0,
            right: 1000,
            bottom: 800,
        };
        assert_eq!(
            centered_icon_position(POINT { x: 500, y: 400 }, client, 100, 80),
            POINT { x: 450, y: 360 }
        );
        assert_eq!(
            centered_icon_position(POINT { x: 5, y: 5 }, client, 100, 80),
            POINT { x: 0, y: 0 }
        );
        assert_eq!(
            centered_icon_position(POINT { x: 995, y: 795 }, client, 100, 80),
            POINT { x: 900, y: 720 }
        );
    }

    #[test]
    fn deadline_is_strict_at_2000_milliseconds() {
        assert_eq!(
            position_attempt_gate(Duration::from_millis(1999), false),
            PositionAttemptGate::Continue
        );
        assert_eq!(
            position_attempt_gate(Duration::from_millis(2000), false),
            PositionAttemptGate::Expired
        );
    }

    #[test]
    fn desktop_hit_test_has_a_bounded_message_timeout() {
        assert_eq!(
            desktop_hit_test_timeout(Instant::now() - Duration::from_millis(1)),
            None
        );
        assert!(desktop_hit_test_timeout(Instant::now() + Duration::from_secs(1))
            .is_some_and(|timeout| (1..=50).contains(&timeout)));
    }

    #[test]
    fn new_input_cancels_before_the_write() {
        assert_eq!(
            position_attempt_gate(Duration::from_millis(100), true),
            PositionAttemptGate::Cancelled
        );
    }

    #[test]
    fn direct_move_does_not_wait_at_the_default_desktop_position() {
        let (direct_settle, direct_retry) = position_timing(DropPositionOrigin::DirectMove);
        let (explorer_settle, explorer_retry) = position_timing(DropPositionOrigin::ExplorerDrop);

        assert_eq!(direct_settle, Duration::ZERO);
        assert_eq!(direct_retry, Duration::from_millis(8));
        assert_eq!(explorer_settle, Duration::from_millis(330));
        assert_eq!(explorer_retry, Duration::from_millis(30));
    }

    #[test]
    fn pointer_press_gate_accepts_current_or_completed_clicks() {
        assert!(!pointer_state_has_press(0));
        assert!(pointer_state_has_press(0x8000));
        assert!(pointer_state_has_press(0x0001));
        assert!(pointer_state_has_press(0x8001));
    }

}
