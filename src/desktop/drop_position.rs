//! 将 Explorer 已完成的单项目桌面拖放定位到鼠标释放处。
//!
//! 普通同卷 MOVE 在进入 OLE 前先隐藏源条目，再由 Explorer 完成真实 OLE 落下；
//! Ctrl 复制、跨卷及其他目标保持原链路。定位只在目标、名称和桌面状态都明确时工作，
//! 任何条件不确定都回退 Explorer。

use std::ffi::c_void;
use std::mem::{ManuallyDrop, size_of};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    LockWindowUpdate, RDW_ALLCHILDREN, RDW_ERASE, RDW_FRAME, RDW_INVALIDATE, RDW_UPDATENOW,
    RedrawWindow, ScreenToClient,
};
use windows::Win32::Storage::FileSystem::{GetVolumeNameForVolumeMountPointW, GetVolumePathNameW};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoTaskMemFree, CoUninitialize, IServiceProvider,
};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAllocEx, VirtualFreeEx,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_VM_OPERATION, PROCESS_VM_READ};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_I4};
use windows::Win32::UI::Controls::{
    LVM_GETITEMCOUNT, LVM_GETITEMPOSITION, LVM_GETITEMSPACING, LVS_AUTOARRANGE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CSIDL_DESKTOP, IFolderView, IShellBrowser, IShellFolder, IShellView, IShellWindows,
    SID_STopLevelBrowser, SVSI_POSITIONITEM, SWC_DESKTOP, SWFO_NEEDDISPATCH, ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_STYLE, GetClientRect, GetParent, GetWindowLongPtrW, GetWindowThreadProcessId,
    SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW, SendMessageW, WindowFromPoint,
};
use windows::core::{Interface, PCWSTR};

const POSITION_TIMEOUT: Duration = Duration::from_millis(1500);
const RETRY_DELAY: Duration = Duration::from_millis(25);
static DROP_GENERATION: AtomicU64 = AtomicU64::new(0);
static VISUAL_LOCK_COUNTER: AtomicU64 = AtomicU64::new(0);
static ACTIVE_VISUAL_LOCK: AtomicU64 = AtomicU64::new(0);
static LOCKED_DESKTOP_LIST: AtomicUsize = AtomicUsize::new(0);

/// 结束指定代次的桌面绘制锁。代次不匹配时说明锁已被正常路径或看门狗释放。
pub(crate) fn finish_desktop_visual_lock(generation: u64) {
    if generation == 0
        || ACTIVE_VISUAL_LOCK
            .compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return;
    }
    let list = HWND(LOCKED_DESKTOP_LIST.swap(0, Ordering::AcqRel) as *mut c_void);
    unsafe {
        let _ = LockWindowUpdate(None);
        if !list.is_invalid() {
            let _ = RedrawWindow(
                Some(list),
                None,
                None,
                RDW_INVALIDATE | RDW_ERASE | RDW_FRAME | RDW_ALLCHILDREN | RDW_UPDATENOW,
            );
        }
    }
    crate::dlog(&format!(
        "[desktop-drop] desktop visual lock released generation={generation}"
    ));
}

/// 只在已经确认是桌面目标时短暂锁住 ListView 绘制，避免 Explorer 先画默认位置。
/// 独立看门狗确保任何提前返回或异常路径都在两秒内恢复桌面。
pub(crate) fn begin_desktop_visual_lock(target: HWND) -> u64 {
    let Some(list) = crate::desktop::host::find_desktop_listview() else {
        return 0;
    };
    if target.is_invalid() || !same_or_descendant(list, target) {
        return 0;
    }
    if !unsafe { LockWindowUpdate(Some(list)) }.as_bool() {
        return 0;
    }
    let generation = VISUAL_LOCK_COUNTER.fetch_add(1, Ordering::AcqRel) + 1;
    LOCKED_DESKTOP_LIST.store(list.0 as usize, Ordering::Release);
    ACTIVE_VISUAL_LOCK.store(generation, Ordering::Release);
    let spawned = std::thread::Builder::new()
        .name(format!("feather-desktop-unlock-{generation}"))
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            if ACTIVE_VISUAL_LOCK.load(Ordering::Acquire) == generation {
                crate::dlog(&format!(
                    "[desktop-drop] desktop visual lock watchdog generation={generation}"
                ));
                finish_desktop_visual_lock(generation);
            }
        })
        .is_ok();
    if !spawned {
        finish_desktop_visual_lock(generation);
        return 0;
    }
    crate::dlog(&format!(
        "[desktop-drop] desktop visual lock acquired generation={generation}"
    ));
    generation
}

struct DesktopVisualLockGuard(u64);

impl Drop for DesktopVisualLockGuard {
    fn drop(&mut self) {
        finish_desktop_visual_lock(self.0);
    }
}

fn same_or_descendant(mut hwnd: HWND, ancestor: HWND) -> bool {
    for _ in 0..10 {
        if hwnd.is_invalid() || ancestor.is_invalid() {
            return false;
        }
        if hwnd == ancestor {
            return true;
        }
        let Ok(parent) = (unsafe { GetParent(hwnd) }) else {
            return false;
        };
        if parent.is_invalid() || parent == hwnd {
            return false;
        }
        hwnd = parent;
    }
    false
}

/// OLE 返回后再次按屏幕坐标核对目标，避免把拖到资源管理器、另一栅栏或普通窗口的
/// 项目误判为桌面项目。
pub(crate) fn is_desktop_drop_point(point: POINT) -> bool {
    let Some(list) = crate::desktop::host::find_desktop_listview() else {
        return false;
    };
    same_or_descendant(unsafe { WindowFromPoint(point) }, list)
}

/// 重名时 Explorer 可能自动生成另一文件名。没有可靠目标名就跳过定位，防止移动桌面上
/// 原有的同名图标。
pub(crate) fn desktop_name_was_absent(source: &Path) -> bool {
    let Some(name) = source.file_name() else {
        return false;
    };
    let (Some(user_desktop), Some(public_desktop)) =
        (crate::desktop_dir(), crate::public_desktop_dir())
    else {
        return false;
    };
    [user_desktop, public_desktop].into_iter().all(|root| {
        match std::fs::symlink_metadata(root.join(name)) {
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
            Ok(_) => false,
        }
    })
}

fn volume_name(path: &Path) -> Option<String> {
    let path_w: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut mount = vec![0u16; 32_768];
    unsafe { GetVolumePathNameW(PCWSTR(path_w.as_ptr()), &mut mount).ok()? };
    let mut volume = vec![0u16; 128];
    unsafe {
        GetVolumeNameForVolumeMountPointW(PCWSTR(mount.as_ptr()), &mut volume).ok()?;
    }
    let len = volume.iter().position(|unit| *unit == 0)?;
    String::from_utf16(&volume[..len]).ok()
}

/// 只对同卷、无重名的普通桌面 MOVE 提前隐藏；跨卷复制、目录递归和其他特殊 Shell
/// 语义继续由 Explorer 完整呈现。
pub(crate) fn desktop_move_fast_path_allowed(source: &Path) -> bool {
    let Some(desktop) = crate::desktop_dir() else {
        return false;
    };
    desktop_name_was_absent(source)
        && volume_name(source)
            .zip(volume_name(&desktop))
            .is_some_and(|(source, desktop)| source.eq_ignore_ascii_case(&desktop))
}

/// IDropSourceNotify 给出的目标必须是桌面 ListView 或其父级；随后逐个读取图标坐标，
/// 只在鼠标处为空白单元格时接管。任何超时或跨进程读取失败都回退 Explorer。
pub(crate) fn is_empty_desktop_target(target: HWND, screen_point: POINT) -> bool {
    let Some(list) = crate::desktop::host::find_desktop_listview() else {
        return false;
    };
    if target.is_invalid() || !same_or_descendant(list, target) {
        return false;
    }
    unsafe {
        if GetWindowLongPtrW(list, GWL_STYLE) & LVS_AUTOARRANGE as isize != 0 {
            return false;
        }
        let mut point = screen_point;
        if !ScreenToClient(list, &mut point).as_bool() {
            return false;
        }
        let mut client = RECT::default();
        if GetClientRect(list, &mut client).is_err()
            || point.x < client.left
            || point.x >= client.right
            || point.y < client.top
            || point.y >= client.bottom
        {
            return false;
        }

        let deadline = Instant::now() + Duration::from_millis(75);
        let send = |message, wparam, lparam| -> Option<usize> {
            let timeout = deadline
                .checked_duration_since(Instant::now())?
                .as_millis()
                .clamp(1, 50) as u32;
            let mut result = 0usize;
            (SendMessageTimeoutW(
                list,
                message,
                WPARAM(wparam),
                LPARAM(lparam),
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                timeout,
                Some(&mut result),
            )
            .0 != 0)
                .then_some(result)
        };
        let Some(count) = send(LVM_GETITEMCOUNT, 0, 0).map(|count| count as i32) else {
            return false;
        };
        if count <= 0 {
            return true;
        }
        let Some(packed) = send(LVM_GETITEMSPACING, 0, 0).map(|packed| packed as u32) else {
            return false;
        };
        let cell_w = ((packed & 0xffff) as i32).max(48);
        let cell_h = ((packed >> 16) as i32).max(48);

        let mut process_id = 0u32;
        GetWindowThreadProcessId(list, Some(&mut process_id));
        let Ok(process) = OpenProcess(PROCESS_VM_OPERATION | PROCESS_VM_READ, false, process_id)
        else {
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
            let Some(sent) = send(LVM_GETITEMPOSITION, index as usize, remote as isize) else {
                empty = false;
                break;
            };
            let mut icon = POINT::default();
            if sent == 0
                || ReadProcessMemory(
                    process,
                    remote,
                    &mut icon as *mut POINT as *mut c_void,
                    size_of::<POINT>(),
                    None,
                )
                .is_err()
            {
                empty = false;
                break;
            }
            if point.x >= icon.x
                && point.x < icon.x + cell_w
                && point.y >= icon.y
                && point.y < icon.y + cell_h
            {
                empty = false;
                break;
            }
        }
        let _ = VirtualFreeEx(process, remote, 0, MEM_RELEASE);
        let _ = CloseHandle(process);
        empty
    }
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

unsafe fn desktop_shell_view() -> windows::core::Result<IShellView> {
    let shell_windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER)?;
    let location = variant_i4(CSIDL_DESKTOP as i32);
    let root = VARIANT::default();
    let mut desktop_hwnd = 0i32;
    let dispatch = shell_windows.FindWindowSW(
        &location,
        &root,
        SWC_DESKTOP,
        &mut desktop_hwnd,
        SWFO_NEEDDISPATCH,
    )?;
    let services: IServiceProvider = dispatch.cast()?;
    let browser: IShellBrowser = services.QueryService(&SID_STopLevelBrowser)?;
    browser.QueryActiveShellView()
}

unsafe fn desktop_folder_view() -> windows::core::Result<IFolderView> {
    desktop_shell_view()?.cast()
}

struct OwnedPidl(*mut ITEMIDLIST);

impl Drop for OwnedPidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0 as *const c_void)) };
    }
}

unsafe fn desktop_child_pidl(view: &IFolderView, label_w: &[u16]) -> Option<OwnedPidl> {
    let folder: IShellFolder = view.GetFolder().ok()?;
    let mut raw = std::ptr::null_mut();
    folder
        .ParseDisplayName(
            HWND::default(),
            None,
            PCWSTR(label_w.as_ptr()),
            None,
            &mut raw,
            std::ptr::null_mut(),
        )
        .ok()?;
    (!raw.is_null()).then_some(OwnedPidl(raw))
}

fn centered_icon_position(point: POINT, client: RECT, cell_w: i32, cell_h: i32) -> POINT {
    POINT {
        x: (point.x - cell_w / 2).clamp(client.left, (client.right - cell_w).max(client.left)),
        y: (point.y - cell_h / 2).clamp(client.top, (client.bottom - cell_h).max(client.top)),
    }
}

fn desired_position(list: HWND, screen_point: POINT) -> Option<POINT> {
    unsafe {
        if GetWindowLongPtrW(list, GWL_STYLE) & LVS_AUTOARRANGE as isize != 0 {
            return None;
        }
        let mut point = screen_point;
        if !ScreenToClient(list, &mut point).as_bool() {
            return None;
        }
        let mut client = RECT::default();
        GetClientRect(list, &mut client).ok()?;
        let packed = SendMessageW(list, LVM_GETITEMSPACING, None, None).0 as u32;
        let cell_w = ((packed & 0xffff) as i32).max(48);
        let cell_h = ((packed >> 16) as i32).max(48);
        Some(centered_icon_position(point, client, cell_w, cell_h))
    }
}

unsafe fn position_when_visible(label: &str, screen_point: POINT, generation: u64) -> bool {
    let started = Instant::now();
    let Some(list) = crate::desktop::host::find_desktop_listview() else {
        return false;
    };
    let Some(desired) = desired_position(list, screen_point) else {
        crate::dlog("[desktop-drop] skipped: auto-arrange or desktop geometry unavailable");
        return false;
    };
    let Ok(view) = desktop_folder_view() else {
        return false;
    };
    let label_w: Vec<u16> = Path::new(label)
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let deadline = Instant::now() + POSITION_TIMEOUT;
    while Instant::now() < deadline {
        if DROP_GENERATION.load(Ordering::Acquire) != generation
            || GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0
        {
            return false;
        }
        if crate::desktop::host::find_desktop_listview() != Some(list) {
            return false;
        }
        if let Some(pidl) = desktop_child_pidl(&view, &label_w)
            && view.GetItemPosition(pidl.0).is_ok()
        {
            let child = pidl.0 as *const ITEMIDLIST;
            let positioned = view
                .SelectAndPositionItems(1, &child, Some(&desired), SVSI_POSITIONITEM.0 as u32)
                .is_ok();
            let saved = positioned
                && desktop_shell_view()
                    .and_then(|shell_view| shell_view.SaveViewState())
                    .is_ok();
            crate::dlog(&format!(
                "[desktop-drop] item=\"{label}\" release=({}, {}) requested=({}, {}) positioned={positioned} view_saved={saved} visible_after_ms={}",
                screen_point.x,
                screen_point.y,
                desired.x,
                desired.y,
                started.elapsed().as_millis()
            ));
            return positioned;
        }
        std::thread::sleep(RETRY_DELAY);
    }
    crate::dlog(&format!(
        "[desktop-drop] item=\"{label}\" timed out waiting for Explorer"
    ));
    false
}

/// Explorer 已经执行了文件 MOVE，应用不应再伪造一次 CREATE 通知。直接刷新现有桌面
/// 视图即可让它读取真实目录状态，同时不会向 Shell 的变更队列塞入第二个创建事件。
unsafe fn refresh_desktop_view() {
    let refreshed = desktop_shell_view()
        .and_then(|shell_view| shell_view.Refresh())
        .is_ok();
    crate::dlog(&format!(
        "[desktop-drop] desktop view refreshed={refreshed}"
    ));
}

fn same_path(left: &Path, right: &Path) -> bool {
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

fn is_direct_desktop_child(path: &Path, desktop_roots: &[std::path::PathBuf]) -> bool {
    path.parent()
        .is_some_and(|parent| desktop_roots.iter().any(|root| same_path(parent, root)))
}

/// 从桌面移走另一个项目会促使 Explorer 更新项目列表。更新前先保存当前视图，避免
/// 用户刚刚手动调整的其他图标仍只存在于内存布局中，随后被旧位置覆盖。
pub(crate) fn save_view_state_before_desktop_move(paths: &[String]) {
    let roots: Vec<_> = [crate::desktop_dir(), crate::public_desktop_dir()]
        .into_iter()
        .flatten()
        .collect();
    if roots.is_empty()
        || !paths
            .iter()
            .any(|path| is_direct_desktop_child(Path::new(path), &roots))
    {
        return;
    }
    let saved = unsafe {
        desktop_shell_view()
            .and_then(|shell_view| shell_view.SaveViewState())
            .is_ok()
    };
    crate::dlog(&format!(
        "[desktop-drop] saved desktop view before removing item={saved}"
    ));
}

/// 启动一个有界 STA 任务等待 Explorer 显示新图标。新一次桌面落点会淘汰旧任务；
/// 用户开始下一次左键操作时旧任务也立即停止，避免迟到的写入覆盖手动调整。
pub(crate) fn queue(source: &Path, screen_point: POINT, visual_lock_generation: u64) -> bool {
    let Some(label) = source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return false;
    };
    let generation = DROP_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    std::thread::Builder::new()
        .name(format!("feather-desktop-drop-{generation}"))
        .spawn(move || {
            let _visual_lock = DesktopVisualLockGuard(visual_lock_generation);
            let initialized =
                unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
            if initialized.is_err() {
                crate::dlog("[desktop-drop] skipped: worker COM initialization failed");
                return;
            }
            if visual_lock_generation != 0 {
                unsafe { refresh_desktop_view() };
            }
            let positioned = unsafe { position_when_visible(&label, screen_point, generation) };
            unsafe { CoUninitialize() };
            if !positioned {
                crate::dlog(&format!(
                    "[desktop-drop] item=\"{label}\" was not repositioned"
                ));
            }
        })
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descendant_check_rejects_invalid_handles() {
        assert!(!same_or_descendant(HWND::default(), HWND::default()));
    }

    #[test]
    fn drop_point_is_centered_and_clamped_inside_desktop() {
        let client = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert_eq!(
            centered_icon_position(POINT { x: 500, y: 400 }, client, 100, 80),
            POINT { x: 450, y: 360 }
        );
        assert_eq!(
            centered_icon_position(POINT { x: 10, y: 10 }, client, 100, 80),
            POINT { x: 0, y: 0 }
        );
    }

    #[test]
    fn only_direct_desktop_children_trigger_view_save() {
        let roots = vec![std::path::PathBuf::from(r"C:\Users\Example\Desktop")];
        assert!(is_direct_desktop_child(
            Path::new(r"c:\users\example\desktop\one.txt"),
            &roots
        ));
        assert!(!is_direct_desktop_child(
            Path::new(r"C:\Users\Example\Desktop\folder\nested.txt"),
            &roots
        ));
        assert!(!is_direct_desktop_child(
            Path::new(r"C:\Users\Example\Documents\one.txt"),
            &roots
        ));
    }
}
