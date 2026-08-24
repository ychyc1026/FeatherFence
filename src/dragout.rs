// 拖出:把栅栏里的文件/文件夹拖到桌面、资源管理器等目标(移动或复制)。
// 用 OLE DoDragDrop:自定义 IDataObject 提供 CF_HDROP(文件路径列表),
// 自定义 IDropSource 管理拖拽过程(松左键落下 / Esc 取消)。
// 目标端(桌面/文件夹窗口/其他栅栏)负责实际移动文件;拖回本栅栏由现有
// drop target 接住(文件已在自身目录则跳过 → 无操作)。
use std::cell::Cell;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, E_INVALIDARG, E_NOTIMPL,
    E_UNEXPECTED, GlobalFree, HGLOBAL, HWND, POINT, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{
    DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC,
    IEnumFORMATETC_Impl, IEnumSTATDATA, STGMEDIUM, TYMED_HGLOBAL,
};
use windows::Win32::Storage::FileSystem::{
    GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, FILE_ATTRIBUTE_REPARSE_POINT,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GHND, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE, DoDragDrop,
    IDropSource, IDropSourceNotify, IDropSourceNotify_Impl, IDropSource_Impl, ReleaseStgMedium,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MK_SHIFT, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::Shell::{CFSTR_PERFORMEDDROPEFFECT, DROPFILES};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows::core::{BOOL, Error, HRESULT, PCWSTR, Ref, Result, implement};

/// DATA_E_FORMATETC:请求的格式不是 CF_HDROP
const DATA_E_FORMATETC: HRESULT = HRESULT(0x80040064_u32 as _);
/// `DROPEFFECT_NONE` is a meaningful `CFSTR_PERFORMEDDROPEFFECT` value: it means the
/// target completed an optimized move and already removed the source item. Keep a distinct
/// sentinel so an explicit NONE is not confused with a target that never reported an effect.
const PERFORMED_EFFECT_UNREPORTED: u32 = u32::MAX;

fn final_drop_effect(ole_effect: DROPEFFECT, shell_reported: Option<u32>) -> DROPEFFECT {
    let allowed = DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0;
    let bits = match shell_reported {
        // Per the Shell optimized-move protocol, an explicit performed NONE means the target
        // moved the item itself. Expose MOVE to the rest of FeatherFence as the logical result.
        Some(bits) if bits == DROPEFFECT_NONE.0 => DROPEFFECT_MOVE.0,
        Some(bits) if bits & allowed != 0 => bits,
        _ => ole_effect.0,
    };
    DROPEFFECT(bits & allowed)
}

#[derive(Clone, Copy, Debug)]
pub struct DragRelease {
    pub screen_point: Option<POINT>,
    pub at: Instant,
}

pub struct DragResult {
    pub effect: DROPEFFECT,
    pub release: Option<DragRelease>,
    /// A second left press occurred after the original drag began. If it follows `release`, a
    /// delayed desktop-position write must not overwrite that newer user interaction.
    pub newer_left_press: bool,
    /// Desktop releases are cancelled before Explorer enters its synchronous Drop handler. The
    /// caller performs this one-item transfer on a worker and keeps the UI responsive.
    pub desktop_transfer: Option<DesktopTransfer>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesktopTransferMode {
    Copy,
    Move,
}

#[derive(Clone, Copy, Debug)]
pub struct DesktopTransfer {
    pub release: DragRelease,
}

fn volume_name(path: &std::path::Path) -> Option<String> {
    let path_w: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut mount_point = vec![0u16; 32_768];
    unsafe {
        GetVolumePathNameW(PCWSTR(path_w.as_ptr()), &mut mount_point).ok()?;
    }
    let mut volume = vec![0u16; 128];
    unsafe {
        GetVolumeNameForVolumeMountPointW(PCWSTR(mount_point.as_ptr()), &mut volume).ok()?;
    }
    let len = volume.iter().position(|unit| *unit == 0)?;
    String::from_utf16(&volume[..len]).ok()
}

fn desktop_mode_for_volume_names(
    source_volume: Option<&str>,
    desktop_volume: Option<&str>,
) -> DesktopTransferMode {
    if source_volume
        .zip(desktop_volume)
        .is_some_and(|(source, desktop)| source.eq_ignore_ascii_case(desktop))
    {
        DesktopTransferMode::Move
    } else {
        DesktopTransferMode::Copy
    }
}

fn desktop_mode_for_paths(
    source: &std::path::Path,
    desktop: &std::path::Path,
) -> DesktopTransferMode {
    let source_volume = volume_name(source);
    let desktop_volume = volume_name(desktop);
    desktop_mode_for_volume_names(source_volume.as_deref(), desktop_volume.as_deref())
}

fn default_desktop_mode(source: &std::path::Path) -> DesktopTransferMode {
    crate::desktop_dir()
        .map(|desktop| desktop_mode_for_paths(source, &desktop))
        .unwrap_or(DesktopTransferMode::Copy)
}

fn is_safe_desktop_fast_path(source: &std::path::Path) -> bool {
    std::fs::symlink_metadata(source)
        .is_ok_and(|metadata| metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0)
}

fn desktop_fast_path_gate(
    item_count: usize,
    desktop_name_absent: bool,
    desktop_default: DesktopTransferMode,
    source_safe: bool,
) -> bool {
    item_count == 1
        && desktop_name_absent
        && desktop_default == DesktopTransferMode::Move
        && source_safe
}

pub fn start_drag(
    paths: Vec<std::path::PathBuf>,
    desktop_name_absent: bool,
    trace: crate::perf::DropTrace,
) -> DragResult {
    crate::dlog(&format!(
        "[dragout] start drag: {}",
        paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("; ")
    ));
    unsafe {
        let performed_effect = Arc::new(AtomicU32::new(PERFORMED_EFFECT_UNREPORTED));
        let desktop_default = paths
            .first()
            .map(|path| default_desktop_mode(path))
            .unwrap_or(DesktopTransferMode::Copy);
        let source_safe = paths
            .first()
            .is_some_and(|path| is_safe_desktop_fast_path(path));
        let desktop_fast_path_allowed = desktop_fast_path_gate(
            paths.len(),
            desktop_name_absent,
            desktop_default,
            source_safe,
        );
        let dataobj: IDataObject = FileDataObject {
            paths,
            performed_effect: Arc::clone(&performed_effect),
        }
        .into();
        let release = Arc::new(OnceLock::new());
        let desktop_transfer = Arc::new(OnceLock::new());
        let src: IDropSource = FileDropSource {
            release: Arc::clone(&release),
            desktop_transfer: Arc::clone(&desktop_transfer),
            desktop_fast_path_allowed,
            current_target: AtomicUsize::new(0),
        }
        .into();
        let mut effect = DROPEFFECT_NONE;
        // Clear the original press transition. A later low bit then means the user pressed
        // again while Explorer was still completing this operation.
        let _ = GetAsyncKeyState(VK_LBUTTON.0 as i32);
        let hr = DoDragDrop(
            &dataobj,
            &src,
            DROPEFFECT_COPY | DROPEFFECT_MOVE,
            &mut effect,
        );
        // 诊断:hr 里能看到 E_UNEXPECTED/CO_E_* 等失败原因;effect 为 NONE 表示目标拒绝。
        let shell_reported_raw = performed_effect.load(Ordering::Relaxed);
        let shell_reported =
            (shell_reported_raw != PERFORMED_EFFECT_UNREPORTED).then_some(shell_reported_raw);
        let desktop_transfer = desktop_transfer.get().copied();
        let final_effect = if desktop_transfer.is_some() {
            DROPEFFECT_MOVE
        } else {
            final_drop_effect(effect, shell_reported)
        };
        let name = if final_effect.0 & DROPEFFECT_COPY.0 != 0 {
            "copy"
        } else if final_effect.0 & DROPEFFECT_MOVE.0 != 0 {
            "move"
        } else {
            "none"
        };
        let release = release.get().copied();
        if let Some(release) = release {
            trace.record_stage("release_to_ole_return", release.at.elapsed(), || {
                format!(
                    "scope=exclusive hr=0x{:08x} ole_effect={} shell_effect={:?} final={name}",
                    hr.0 as u32, effect.0, shell_reported
                )
            });
        }
        crate::dlog(&format!(
            "[dragout] DoDragDrop hr=0x{:08x} effect={} performed={:?} final={} desktop_intercept={}",
            hr.0 as u32,
            effect.0,
            shell_reported,
            name,
            desktop_transfer.is_some(),
        ));
        let pointer_state = GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16;
        DragResult {
            effect: final_effect,
            release,
            newer_left_press: pointer_state & 0x8001 != 0,
            desktop_transfer,
        }
    }
}

/// 拖拽数据源:持有拖出文件列表,GetData 按需构造 CF_HDROP。
/// 其余 IDataObject 方法返回 E_NOTIMPL(拖出文件到资源管理器只需要 GetData)。
#[implement(IDataObject)]
pub struct FileDataObject {
    /// 拖出的绝对路径
    pub paths: Vec<std::path::PathBuf>,
    /// Explorer 可通过 CFSTR_PERFORMEDDROPEFFECT 回写真正执行的 COPY/MOVE。
    performed_effect: Arc<AtomicU32>,
}

/// 构造 CF_HDROP 全局内存块:DROPFILES 头 + 宽字符路径序列(每段 0 结尾,整体双 0 结尾)。
/// 返回的 HGLOBAL 由接收方 ReleaseStgMedium 释放。
unsafe fn build_hdrop(paths: &[std::path::PathBuf]) -> Result<HGLOBAL> {
    let mut buf: Vec<u16> = Vec::new();
    for path in paths {
        buf.extend(path.as_os_str().encode_wide());
        buf.push(0);
    }
    buf.push(0); // 双空结尾
    let payload = buf.len() * 2;
    let total = size_of::<DROPFILES>() + payload;
    let hg = GlobalAlloc(GHND, total)?;
    let ptr = GlobalLock(hg);
    if ptr.is_null() {
        let _ = GlobalFree(Some(hg));
        return Err(Error::from_hresult(E_UNEXPECTED));
    }
    let df = DROPFILES {
        pFiles: size_of::<DROPFILES>() as u32,
        pt: POINT { x: 0, y: 0 },
        fNC: BOOL(0),
        fWide: BOOL(1), // 宽字符路径
    };
    std::ptr::copy_nonoverlapping(
        &df as *const DROPFILES as *const u8,
        ptr as *mut u8,
        size_of::<DROPFILES>(),
    );
    std::ptr::copy_nonoverlapping(
        buf.as_ptr() as *const u8,
        (ptr as *mut u8).add(size_of::<DROPFILES>()),
        payload,
    );
    let _ = GlobalUnlock(hg);
    Ok(hg)
}

/// 文件路径格式:CF_HDROP / DVASPECT_CONTENT / TYMED_HGLOBAL / lindex=-1。
/// GetData/QueryGetData/EnumFormatEtc 三处必须用同一格式描述,否则目标会格式不匹配而拒绝。
fn hdrop_format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0 as u32,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

fn performed_drop_effect_format() -> FORMATETC {
    FORMATETC {
        cfFormat: unsafe { RegisterClipboardFormatW(CFSTR_PERFORMEDDROPEFFECT) as u16 },
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0 as u32,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

fn format_at(direction: u32, pos: u32) -> Option<FORMATETC> {
    match (direction, pos) {
        (1, 0) => Some(hdrop_format()),
        (2, 0) => Some(performed_drop_effect_format()),
        _ => None,
    }
}

fn format_count(direction: u32) -> u32 {
    match direction {
        1 => 1,
        2 => 1,
        _ => 0,
    }
}

fn is_supported_get_format(fmt: &FORMATETC) -> bool {
    if fmt.dwAspect != DVASPECT_CONTENT.0 as u32
        || fmt.lindex != -1
        || fmt.tymed & TYMED_HGLOBAL.0 as u32 == 0
    {
        return false;
    }
    fmt.cfFormat == CF_HDROP.0
}

fn is_supported_set_format(fmt: &FORMATETC) -> bool {
    fmt.dwAspect == DVASPECT_CONTENT.0 as u32
        && fmt.lindex == -1
        && fmt.tymed & TYMED_HGLOBAL.0 as u32 != 0
        && fmt.cfFormat == performed_drop_effect_format().cfFormat
}

/// 可读格式枚举：CF_HDROP 文件路径 + Preferred DropEffect 首选移动。
/// Explorer 等目标在落点协商前会调用 EnumFormatEtc
/// 枚举数据源支持的格式;若返回 E_NOTIMPL,部分目标直接判定"无可用格式"→ 禁止光标、拒绝落下。
#[implement(IEnumFORMATETC)]
pub struct FileFormatEnum {
    /// 枚举游标(0..=1);Cell 以支持 &self 上推进游标
    pos: Cell<u32>,
    /// DATADIR_GET=1 或 DATADIR_SET=2。
    direction: u32,
}

impl IEnumFORMATETC_Impl for FileFormatEnum_Impl {
    fn Next(&self, celt: u32, rgelt: *mut FORMATETC, pceltfetched: *mut u32) -> HRESULT {
        unsafe {
            if rgelt.is_null() {
                return E_INVALIDARG;
            }
            let mut n = 0u32;
            while n < celt {
                let Some(fmt) = format_at(self.direction, self.pos.get()) else {
                    break;
                };
                *rgelt.add(n as usize) = fmt;
                self.pos.set(self.pos.get() + 1);
                n += 1;
            }
            if !pceltfetched.is_null() {
                *pceltfetched = n;
            }
            // 返回的少于请求的 → S_FALSE(枚举结束)
            if n == celt { S_OK } else { S_FALSE }
        }
    }

    fn Skip(&self, celt: u32) -> Result<()> {
        let remain = format_count(self.direction).saturating_sub(self.pos.get());
        let skipped = remain.min(celt);
        self.pos.set(self.pos.get() + skipped);
        if skipped == celt {
            Ok(())
        } else {
            // 跳过的比请求的少 → S_FALSE
            Err(Error::from_hresult(S_FALSE))
        }
    }

    fn Reset(&self) -> Result<()> {
        self.pos.set(0);
        Ok(())
    }

    fn Clone(&self) -> Result<IEnumFORMATETC> {
        let e: IEnumFORMATETC = FileFormatEnum {
            pos: Cell::new(self.pos.get()),
            direction: self.direction,
        }
        .into();
        Ok(e)
    }
}

impl IDataObject_Impl for FileDataObject_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> Result<STGMEDIUM> {
        unsafe {
            if pformatetcin.is_null() {
                return Err(Error::from_hresult(DATA_E_FORMATETC));
            }
            let fmt = *pformatetcin;
            if !is_supported_get_format(&fmt) {
                return Err(Error::from_hresult(DATA_E_FORMATETC));
            }
            let hg = build_hdrop(&self.paths)?;
            let mut medium = STGMEDIUM::default();
            medium.tymed = TYMED_HGLOBAL.0 as u32;
            medium.u.hGlobal = hg;
            Ok(medium)
        }
    }

    fn GetDataHere(&self, _pformatetc: *const FORMATETC, _pmedium: *mut STGMEDIUM) -> Result<()> {
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        unsafe {
            if pformatetc.is_null() {
                return DATA_E_FORMATETC;
            }
            let fmt = *pformatetc;
            if is_supported_get_format(&fmt) {
                S_OK
            } else {
                DATA_E_FORMATETC
            }
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        _pformatetcin: *const FORMATETC,
        _pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        E_NOTIMPL
    }

    fn SetData(
        &self,
        pformatetc: *const FORMATETC,
        pmedium: *const STGMEDIUM,
        frelease: BOOL,
    ) -> Result<()> {
        unsafe {
            if pformatetc.is_null() || pmedium.is_null() {
                return Err(Error::from_hresult(E_INVALIDARG));
            }
            let fmt = *pformatetc;
            let medium = &*pmedium;
            if !is_supported_set_format(&fmt)
                || medium.tymed & TYMED_HGLOBAL.0 as u32 == 0
                || medium.u.hGlobal.is_invalid()
            {
                return Err(Error::from_hresult(DATA_E_FORMATETC));
            }
            let ptr = GlobalLock(medium.u.hGlobal);
            if ptr.is_null() {
                return Err(Error::from_hresult(E_UNEXPECTED));
            }
            let effect = std::ptr::read_unaligned(ptr as *const u32);
            let _ = GlobalUnlock(medium.u.hGlobal);
            self.performed_effect.store(effect, Ordering::Relaxed);
            if frelease.as_bool() {
                ReleaseStgMedium(pmedium as *mut STGMEDIUM);
            }
            Ok(())
        }
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> Result<IEnumFORMATETC> {
        // DATADIR_GET = 1:文件路径;
        // DATADIR_SET = 2:Explorer 回写真正执行的 COPY/MOVE。
        if format_count(dwdirection) == 0 {
            return Err(Error::from_hresult(E_NOTIMPL));
        }
        let e: IEnumFORMATETC = FileFormatEnum {
            pos: Cell::new(0),
            direction: dwdirection,
        }
        .into();
        Ok(e)
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: Ref<IAdviseSink>,
    ) -> Result<u32> {
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn DUnadvise(&self, _dwconnection: u32) -> Result<()> {
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
        Err(Error::from_hresult(E_NOTIMPL))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_file_paths_and_performed_effect_feedback() {
        let paths = hdrop_format();

        assert!(is_supported_get_format(&paths));
        assert!(format_at(1, 0).is_some());
        assert!(format_at(1, 1).is_none());

        let performed = performed_drop_effect_format();
        assert!(is_supported_set_format(&performed));
        assert!(format_at(2, 0).is_some());
        assert!(format_at(2, 1).is_none());
    }

    #[test]
    fn outgoing_get_formats_leave_effect_choice_to_drop_target() {
        assert_eq!(format_count(1), 1);
        assert_eq!(format_at(1, 0).unwrap().cfFormat, CF_HDROP.0);
        assert!(format_at(1, 1).is_none());
    }

    #[test]
    fn explorer_performed_copy_overrides_the_ole_fallback() {
        assert_eq!(
            final_drop_effect(DROPEFFECT_MOVE, Some(DROPEFFECT_COPY.0)),
            DROPEFFECT_COPY
        );
    }

    #[test]
    fn unreported_effect_uses_the_ole_result() {
        assert_eq!(final_drop_effect(DROPEFFECT_MOVE, None), DROPEFFECT_MOVE);
    }

    #[test]
    fn explicit_none_means_the_target_completed_an_optimized_move() {
        assert_eq!(
            final_drop_effect(DROPEFFECT_COPY, Some(DROPEFFECT_NONE.0)),
            DROPEFFECT_MOVE
        );
    }

    #[test]
    fn explicit_move_overrides_the_ole_fallback() {
        assert_eq!(
            final_drop_effect(DROPEFFECT_COPY, Some(DROPEFFECT_MOVE.0)),
            DROPEFFECT_MOVE
        );
    }

    #[test]
    fn desktop_default_requires_matching_real_volume_names() {
        assert_eq!(
            desktop_mode_for_volume_names(Some(r"\\?\Volume{same}\"), Some(r"\\?\volume{SAME}\")),
            DesktopTransferMode::Move
        );
        assert_eq!(
            desktop_mode_for_volume_names(Some(r"\\?\Volume{one}\"), Some(r"\\?\Volume{two}\")),
            DesktopTransferMode::Copy
        );
        assert_eq!(
            desktop_mode_for_volume_names(None, Some(r"\\?\Volume{two}\")),
            DesktopTransferMode::Copy
        );
    }

    #[test]
    fn desktop_direct_move_accepts_only_plain_or_shift_drop() {
        assert!(direct_move_modifiers_allowed(MODIFIERKEYS_FLAGS(0)));
        assert!(direct_move_modifiers_allowed(MK_SHIFT));
        let ctrl = windows::Win32::System::SystemServices::MK_CONTROL;
        assert!(!direct_move_modifiers_allowed(ctrl));
        assert!(!direct_move_modifiers_allowed(MODIFIERKEYS_FLAGS(0x20))); // MK_ALT
        assert!(!direct_move_modifiers_allowed(MODIFIERKEYS_FLAGS(
            ctrl.0 | MK_SHIFT.0
        )));
    }

    #[test]
    fn desktop_fast_path_only_accepts_one_safe_same_volume_item() {
        assert!(desktop_fast_path_gate(
            1,
            true,
            DesktopTransferMode::Move,
            true
        ));
        assert!(!desktop_fast_path_gate(
            2,
            true,
            DesktopTransferMode::Move,
            true
        ));
        assert!(!desktop_fast_path_gate(
            1,
            true,
            DesktopTransferMode::Copy,
            true
        ));
        assert!(!desktop_fast_path_gate(
            1,
            true,
            DesktopTransferMode::Move,
            false
        ));
    }
}

/// The direct path deliberately recognizes only an unmodified drop or Shift-forced MOVE.
/// Every other flag (Ctrl, Alt, another mouse button, or a future Shell modifier) must fall
/// through to Explorer so FeatherFence cannot accidentally replace special Shell semantics.
fn direct_move_modifiers_allowed(key_state: MODIFIERKEYS_FLAGS) -> bool {
    key_state.0 == 0 || key_state.0 == MK_SHIFT.0
}

/// 拖拽过程控制:Esc 取消;松开左键 → 落下;GiveFeedback 用 OLE 默认光标。
#[implement(IDropSource, IDropSourceNotify)]
pub struct FileDropSource {
    release: Arc<OnceLock<DragRelease>>,
    desktop_transfer: Arc<OnceLock<DesktopTransfer>>,
    desktop_fast_path_allowed: bool,
    current_target: AtomicUsize,
}

impl IDropSource_Impl for FileDropSource_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        if fescapepressed.as_bool() {
            DRAGDROP_S_CANCEL
        } else if !grfkeystate.contains(MK_LBUTTON) {
            let at = Instant::now();
            let mut point = POINT::default();
            let screen_point = unsafe { GetCursorPos(&mut point) }.is_ok().then_some(point);
            let release = DragRelease { screen_point, at };
            let _ = self.release.set(release);
            let direct_move_modifiers = direct_move_modifiers_allowed(grfkeystate);
            let target_value = self.current_target.load(Ordering::Acquire);
            let target = HWND(target_value as *mut std::ffi::c_void);
            if crate::perf::enabled() {
                crate::dlog(&format!(
                    "[desktop-fast-path] release_target hwnd=0x{target_value:x} key_state=0x{:x} modifiers_allowed={direct_move_modifiers} allowed={}",
                    grfkeystate.0,
                    self.desktop_fast_path_allowed
                ));
            }
            let on_desktop = self.desktop_fast_path_allowed
                && screen_point.is_some_and(|point| {
                    crate::desktop_icons::is_empty_desktop_drop_target(target, point)
                });
            if on_desktop && direct_move_modifiers {
                let _ = self.desktop_transfer.set(DesktopTransfer { release });
                // Avoid Explorer's synchronous desktop Drop. The caller now owns the exact
                // one-item transfer and runs it off the UI thread.
                DRAGDROP_S_CANCEL
            } else {
                DRAGDROP_S_DROP
            }
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

impl IDropSourceNotify_Impl for FileDropSource_Impl {
    fn DragEnterTarget(&self, hwndtarget: HWND) -> Result<()> {
        self.current_target
            .store(hwndtarget.0 as usize, Ordering::Release);
        if crate::perf::enabled() {
            crate::dlog(&format!(
                "[desktop-fast-path] enter_target hwnd=0x{:x}",
                hwndtarget.0 as usize
            ));
        }
        Ok(())
    }

    fn DragLeaveTarget(&self) -> Result<()> {
        self.current_target.store(0, Ordering::Release);
        if crate::perf::enabled() {
            crate::dlog("[desktop-fast-path] leave_target");
        }
        Ok(())
    }
}
