//! 避让栅栏：把 Explorer 桌面 ListView 中落在栅栏矩形下的图标移到空闲网格。
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, ManuallyDrop};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    CoCreateInstance, CoTaskMemFree, IServiceProvider, CLSCTX_LOCAL_SERVER,
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
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IFolderView, IShellBrowser, IShellFolder, IShellWindows, ShellWindows, CSIDL_DESKTOP,
    SID_STopLevelBrowser, SVSI_POSITIONITEM, SWC_DESKTOP, SWFO_NEEDDISPATCH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetParent, GetWindowLongPtrW, GetWindowThreadProcessId, SendMessageW,
    SetWindowLongPtrW, WindowFromPoint, GWL_STYLE,
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

/// 记录栅栏移动/缩放前的配置快照(每个 id 只记一次)。由栅栏 WM_LBUTTONDOWN
/// 进入拖动/缩放时调用——此时窗口还在原位。
const DROP_ITEM_RETRIES: usize = 30;
const DROP_ITEM_RETRY_DELAY_MS: u64 = 35;
const DROP_ITEM_STABLE_SAMPLES: usize = 3;

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

struct OwnedPidl(*mut ITEMIDLIST);

impl Drop for OwnedPidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0 as *const c_void)) };
    }
}

unsafe fn desktop_child_pidl(view: &IFolderView, source_path: &Path) -> Option<(OwnedPidl, String)> {
    let label = source_path.file_name()?.to_string_lossy().into_owned();
    let mut label_w: Vec<u16> = label.encode_utf16().collect();
    label_w.push(0);
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
    (!raw.is_null()).then_some((OwnedPidl(raw), label))
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

/// True when the release point belongs to Explorer's desktop ListView rather than a fence or
/// another Explorer window. This is also used to keep our desktop shortcut collector from
/// immediately reclaiming a shortcut that the user just dragged out of a fence.
pub fn is_desktop_drop_point(screen_point: POINT) -> bool {
    let Some(list) = crate::utils::find_desktop_listview() else {
        return false;
    };
    unsafe { point_hits_desktop_list(list, screen_point) }
}

/// If the OLE drop ended on Explorer's desktop, move the newly-created ListView item so its
/// icon cell is centred on the actual mouse release point. Position through `IFolderView` so
/// Explorer's own folder-view state and the visible ListView stay in sync.
pub fn place_file_at_drop_point(source_path: &Path, screen_point: POINT) -> bool {
    let Some(list) = crate::utils::find_desktop_listview() else {
        return false;
    };
    unsafe {
        if !is_desktop_drop_point(screen_point) {
            return false;
        }
        let style = GetWindowLongPtrW(list, GWL_STYLE);
        if style & LVS_AUTOARRANGE as isize != 0 {
            crate::dlog("[desktop-drop] skipped positioning because desktop auto-arrange is on");
            return false;
        }

        let mut client_point = screen_point;
        if !ScreenToClient(list, &mut client_point).as_bool() {
            return false;
        }
        let mut client = RECT::default();
        if GetClientRect(list, &mut client).is_err() {
            return false;
        }
        let packed = SendMessageW(
            list,
            LVM_GETITEMSPACING,
            Some(WPARAM(0)),
            Some(LPARAM(0)),
        )
        .0 as u32;
        let cell_w = ((packed & 0xffff) as i32).max(48);
        let cell_h = ((packed >> 16) as i32).max(48);
        let desired = centered_icon_position(client_point, client, cell_w, cell_h);

        let view = match desktop_folder_view() {
            Ok(view) => view,
            Err(error) => {
                crate::dlog(&format!(
                    "[desktop-drop] could not acquire Explorer folder view: {error:?}"
                ));
                return false;
            }
        };
        let Some((pidl, label)) = desktop_child_pidl(&view, source_path) else {
            crate::dlog(&format!(
                "[desktop-drop] could not resolve desktop item for {}",
                source_path.display()
            ));
            return false;
        };

        // Explorer can return from DoDragDrop before its folder-view transaction has fully
        // published the new item. Wait for a few identical positions, then position through
        // IFolderView so Explorer's model and the visible ListView are updated together.
        let mut last_position: Option<POINT> = None;
        let mut stable_samples = 0usize;
        let mut appeared = false;
        for attempt in 0..DROP_ITEM_RETRIES {
            if let Ok(position) = view.GetItemPosition(pidl.0) {
                appeared = true;
                if last_position
                    .map(|last| last.x == position.x && last.y == position.y)
                    .unwrap_or(false)
                {
                    stable_samples += 1;
                } else {
                    stable_samples = 1;
                    last_position = Some(position);
                }
                if stable_samples >= DROP_ITEM_STABLE_SAMPLES {
                    break;
                }
            }
            if attempt + 1 < DROP_ITEM_RETRIES {
                std::thread::sleep(Duration::from_millis(DROP_ITEM_RETRY_DELAY_MS));
            }
        }
        if !appeared {
            crate::dlog(&format!(
                "[desktop-drop] desktop item did not appear for {}",
                source_path.display()
            ));
            return false;
        }

        let child = pidl.0 as *const ITEMIDLIST;
        let positioned = view
            .SelectAndPositionItems(
                1,
                &child,
                Some(&desired),
                SVSI_POSITIONITEM.0 as u32,
            )
            .is_ok();
        let actual = if positioned {
            view.GetItemPosition(pidl.0)
                .unwrap_or(POINT { x: -1, y: -1 })
        } else {
            POINT { x: -1, y: -1 }
        };
        crate::dlog(&format!(
            "[desktop-drop] item=\"{}\" method=folder-view release=({}, {}) requested=({}, {}) actual=({}, {}) positioned={}",
            label,
            screen_point.x,
            screen_point.y,
            desired.x,
            desired.y,
            actual.x,
            actual.y,
            positioned
        ));
        positioned
    }
}

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
    use super::centered_icon_position;
    use windows::Win32::Foundation::{POINT, RECT};

    #[test]
    fn release_point_centres_and_clamps_the_icon_cell() {
        let client = RECT { left: 0, top: 0, right: 1000, bottom: 800 };
        assert_eq!(
            centered_icon_position(POINT { x: 500, y: 400 }, client, 100, 80),
            POINT { x: 450, y: 360 }
        );
        assert_eq!(
            centered_icon_position(POINT { x: 5, y: 5 }, client, 100, 80),
            POINT { x: 0, y: 0 }
        );
    }
}
