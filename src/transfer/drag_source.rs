// 拖出:把栅栏里的文件/文件夹拖到桌面、资源管理器等目标(移动或复制)。
// 用 OLE DoDragDrop:自定义 IDataObject 提供 CF_HDROP(文件路径列表),
// 自定义 IDropSource 管理拖拽过程(松左键落下 / Esc 取消)。
// 目标端(桌面/文件夹窗口/其他栅栏)负责实际移动文件;拖回本栅栏由现有
// drop target 接住(文件已在自身目录则跳过 → 无操作)。
use std::cell::Cell;
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, E_INVALIDARG, E_NOTIMPL,
    E_UNEXPECTED, GlobalFree, HGLOBAL, HWND, POINT, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{
    DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC,
    IEnumFORMATETC_Impl, IEnumSTATDATA, STGMEDIUM, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GHND, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE, DoDragDrop,
    IDropSource, IDropSource_Impl, IDropSourceNotify, IDropSourceNotify_Impl, ReleaseStgMedium,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MK_SHIFT, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::{CFSTR_PERFORMEDDROPEFFECT, CFSTR_SHELLIDLISTOFFSET, DROPFILES};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows::core::{BOOL, Error, HRESULT, Ref, Result, implement};

/// DATA_E_FORMATETC:请求的格式不是 CF_HDROP
const DATA_E_FORMATETC: HRESULT = HRESULT(0x80040064_u32 as _);

fn final_drop_effect(ole_effect: DROPEFFECT, shell_reported: u32) -> DROPEFFECT {
    let allowed = DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0;
    let bits = if shell_reported & allowed != 0 {
        shell_reported
    } else {
        ole_effect.0
    };
    DROPEFFECT(bits & allowed)
}

fn desktop_move_fast_path_modifiers_allowed(keys: MODIFIERKEYS_FLAGS) -> bool {
    keys.0 == 0 || keys.0 == MK_SHIFT.0
}

#[derive(Clone, Copy, Debug)]
pub struct DragResult {
    pub effect: DROPEFFECT,
    /// 松开左键时的物理屏幕坐标；仅供落点所属目标确认后使用。
    pub release_point: Option<POINT>,
    /// 桌面空白处的安全同卷 MOVE 已成功交给 Explorer；源目录刷新应等待通知。
    pub desktop_drop_defer_refresh: bool,
    /// 非零表示桌面 ListView 绘制已锁定，调用方必须交给定位任务或立即释放。
    pub desktop_visual_lock: u64,
}

/// 拖出指定路径:阻塞到拖拽结束。返回实际拖放效果和准确的鼠标释放坐标。
pub fn start_drag(paths: Vec<String>, allow_desktop_move_fast_path: bool) -> DragResult {
    crate::dlog(&format!("[dragout] start drag: {}", paths.join("; ")));
    unsafe {
        let mut drag_origin = POINT::default();
        let _ = GetCursorPos(&mut drag_origin);
        let performed_effect = Arc::new(AtomicU32::new(DROPEFFECT_NONE.0));
        let release_point = Arc::new(OnceLock::new());
        let shell_offsets_requested = Arc::new(AtomicBool::new(false));
        let dataobj: IDataObject = FileDataObject {
            paths,
            performed_effect: Arc::clone(&performed_effect),
            release_point: Arc::clone(&release_point),
            drag_origin,
            shell_offsets_requested: Arc::clone(&shell_offsets_requested),
        }
        .into();
        let desktop_drop_defer_refresh = Arc::new(AtomicBool::new(false));
        let desktop_visual_lock = Arc::new(AtomicU64::new(0));
        let src: IDropSource = FileDropSource {
            release_point: Arc::clone(&release_point),
            desktop_drop_defer_refresh: Arc::clone(&desktop_drop_defer_refresh),
            allow_desktop_move_fast_path,
            current_target: AtomicUsize::new(0),
            desktop_visual_lock: Arc::clone(&desktop_visual_lock),
        }
        .into();
        let mut effect = DROPEFFECT_NONE;
        let hr = DoDragDrop(
            &dataobj,
            &src,
            DROPEFFECT_COPY | DROPEFFECT_MOVE,
            &mut effect,
        );
        // 诊断:hr 里能看到 E_UNEXPECTED/CO_E_* 等失败原因;effect 为 NONE 表示目标拒绝。
        let shell_reported = performed_effect.load(Ordering::Relaxed);
        let desktop_drop_defer_refresh = desktop_drop_defer_refresh.load(Ordering::Acquire);
        let desktop_visual_lock = desktop_visual_lock.load(Ordering::Acquire);
        let shell_offsets_requested = shell_offsets_requested.load(Ordering::Acquire);
        let final_effect = final_drop_effect(effect, shell_reported);
        let name = if final_effect.0 & DROPEFFECT_COPY.0 != 0 {
            "copy"
        } else if final_effect.0 & DROPEFFECT_MOVE.0 != 0 {
            "move"
        } else {
            "none"
        };
        crate::dlog(&format!(
            "[dragout] DoDragDrop hr=0x{:08x} effect={} performed={} final={} desktop_drop_defer_refresh={} desktop_visual_lock={} shell_offsets_requested={}",
            hr.0 as u32,
            effect.0,
            shell_reported,
            name,
            desktop_drop_defer_refresh,
            desktop_visual_lock,
            shell_offsets_requested
        ));
        DragResult {
            effect: final_effect,
            release_point: release_point.get().copied(),
            desktop_drop_defer_refresh,
            desktop_visual_lock,
        }
    }
}

/// 拖拽数据源:持有拖出文件列表,GetData 按需构造 CF_HDROP。
/// 其余 IDataObject 方法返回 E_NOTIMPL(拖出文件到资源管理器只需要 GetData)。
#[implement(IDataObject)]
pub struct FileDataObject {
    /// 拖出的绝对路径
    pub paths: Vec<String>,
    /// Explorer 可通过 CFSTR_PERFORMEDDROPEFFECT 回写真正执行的 COPY/MOVE。
    performed_effect: Arc<AtomicU32>,
    /// 松开左键后由 IDropSource 写入，确保 CF_HDROP 携带真实屏幕落点。
    release_point: Arc<OnceLock<POINT>>,
    /// OLE 开始时的屏幕坐标，作为 Shell 对象组的原点。
    drag_origin: POINT,
    /// 记录目标是否读取了 Shell 原生对象位置格式。
    shell_offsets_requested: Arc<AtomicBool>,
}

/// 构造 CF_HDROP 全局内存块:DROPFILES 头 + 宽字符路径序列(每段 0 结尾,整体双 0 结尾)。
/// 返回的 HGLOBAL 由接收方 ReleaseStgMedium 释放。
unsafe fn build_hdrop(paths: &[String], drop_point: POINT) -> Result<HGLOBAL> {
    let mut buf: Vec<u16> = Vec::new();
    for p in paths {
        buf.extend(p.encode_utf16());
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
        pt: drop_point,
        // TRUE 表示 pt 是屏幕坐标。此前固定的 (0,0)+客户端坐标会让 Shell
        // 无法从 CF_HDROP 取得本次真实落点。
        fNC: BOOL(1),
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

/// CFSTR_SHELLIDLISTOFFSET:第一个 POINT 是对象组左上角的屏幕坐标，后续 POINT
/// 是各对象相对组原点的位置。栅栏当前一次只拖一个条目，因此相对位置为 (0,0)。
unsafe fn build_shell_offsets(path_count: usize, group_origin: POINT) -> Result<HGLOBAL> {
    if path_count == 0 {
        return Err(Error::from_hresult(E_INVALIDARG));
    }
    let mut points = Vec::with_capacity(path_count + 1);
    points.push(group_origin);
    points.resize(path_count + 1, POINT::default());
    let byte_len = points.len() * size_of::<POINT>();
    let hg = GlobalAlloc(GHND, byte_len)?;
    let ptr = GlobalLock(hg);
    if ptr.is_null() {
        let _ = GlobalFree(Some(hg));
        return Err(Error::from_hresult(E_UNEXPECTED));
    }
    std::ptr::copy_nonoverlapping(points.as_ptr() as *const u8, ptr as *mut u8, byte_len);
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

fn shell_idlist_offset_format() -> FORMATETC {
    FORMATETC {
        cfFormat: unsafe { RegisterClipboardFormatW(CFSTR_SHELLIDLISTOFFSET) as u16 },
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0 as u32,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

fn format_at(direction: u32, pos: u32) -> Option<FORMATETC> {
    match (direction, pos) {
        (1, 0) => Some(hdrop_format()),
        (1, 1) => Some(shell_idlist_offset_format()),
        (2, 0) => Some(performed_drop_effect_format()),
        _ => None,
    }
}

fn format_count(direction: u32) -> u32 {
    match direction {
        1 => 2,
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
    fmt.cfFormat == CF_HDROP.0 || fmt.cfFormat == shell_idlist_offset_format().cfFormat
}

fn is_supported_set_format(fmt: &FORMATETC) -> bool {
    fmt.dwAspect == DVASPECT_CONTENT.0
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
            let hg = if fmt.cfFormat == CF_HDROP.0 {
                let point = self
                    .release_point
                    .get()
                    .copied()
                    .unwrap_or(self.drag_origin);
                build_hdrop(&self.paths, point)?
            } else {
                self.shell_offsets_requested.store(true, Ordering::Release);
                build_shell_offsets(self.paths.len(), self.drag_origin)?
            };
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
    use windows::Win32::System::SystemServices::MK_CONTROL;

    #[test]
    fn advertises_file_paths_and_performed_effect_feedback() {
        let paths = hdrop_format();

        assert!(is_supported_get_format(&paths));
        assert!(format_at(1, 0).is_some());
        let offsets = shell_idlist_offset_format();
        assert!(is_supported_get_format(&offsets));
        assert_eq!(format_at(1, 1).unwrap().cfFormat, offsets.cfFormat);
        assert!(format_at(1, 2).is_none());

        let performed = performed_drop_effect_format();
        assert!(is_supported_set_format(&performed));
        assert!(format_at(2, 0).is_some());
        assert!(format_at(2, 1).is_none());
    }

    #[test]
    fn outgoing_get_formats_include_shell_object_positions() {
        assert_eq!(format_count(1), 2);
        assert_eq!(format_at(1, 0).unwrap().cfFormat, CF_HDROP.0);
        assert_eq!(
            format_at(1, 1).unwrap().cfFormat,
            shell_idlist_offset_format().cfFormat
        );
        assert!(format_at(1, 2).is_none());
    }

    #[test]
    fn hdrop_contains_the_real_screen_drop_point() {
        let point = POINT { x: 1234, y: 567 };
        let paths = vec![r"C:\fence\one.txt".to_string()];
        let hg = unsafe { build_hdrop(&paths, point) }.unwrap();
        let ptr = unsafe { GlobalLock(hg) };
        assert!(!ptr.is_null());
        let header = unsafe { std::ptr::read_unaligned(ptr as *const DROPFILES) };
        let stored_point = unsafe { std::ptr::addr_of!(header.pt).read_unaligned() };
        let stored_fnc = unsafe { std::ptr::addr_of!(header.fNC).read_unaligned() };
        assert_eq!(stored_point, point);
        assert!(stored_fnc.as_bool());
        let _ = unsafe { GlobalUnlock(hg) };
        let _ = unsafe { GlobalFree(Some(hg)) };
    }

    #[test]
    fn shell_offsets_start_with_group_origin_and_keep_item_relative() {
        let origin = POINT { x: 900, y: 400 };
        let hg = unsafe { build_shell_offsets(1, origin) }.unwrap();
        let ptr = unsafe { GlobalLock(hg) } as *const POINT;
        assert!(!ptr.is_null());
        assert_eq!(unsafe { std::ptr::read_unaligned(ptr) }, origin);
        assert_eq!(
            unsafe { std::ptr::read_unaligned(ptr.add(1)) },
            POINT::default()
        );
        let _ = unsafe { GlobalUnlock(hg) };
        let _ = unsafe { GlobalFree(Some(hg)) };
    }

    #[test]
    fn explorer_performed_copy_overrides_the_ole_fallback() {
        assert_eq!(
            final_drop_effect(DROPEFFECT_MOVE, DROPEFFECT_COPY.0),
            DROPEFFECT_COPY
        );
        assert_eq!(
            final_drop_effect(DROPEFFECT_MOVE, DROPEFFECT_NONE.0),
            DROPEFFECT_MOVE
        );
    }

    #[test]
    fn desktop_move_fast_path_keeps_ctrl_on_the_normal_explorer_path() {
        assert!(desktop_move_fast_path_modifiers_allowed(
            MODIFIERKEYS_FLAGS(0)
        ));
        assert!(desktop_move_fast_path_modifiers_allowed(MK_SHIFT));
        assert!(!desktop_move_fast_path_modifiers_allowed(MK_CONTROL));
        assert!(!desktop_move_fast_path_modifiers_allowed(
            MODIFIERKEYS_FLAGS(MK_CONTROL.0 | MK_SHIFT.0)
        ));
    }
}

/// 拖拽过程控制:Esc 取消;松开左键 → 落下;GiveFeedback 用 OLE 默认光标。
#[implement(IDropSource, IDropSourceNotify)]
pub struct FileDropSource {
    release_point: Arc<OnceLock<POINT>>,
    desktop_drop_defer_refresh: Arc<AtomicBool>,
    allow_desktop_move_fast_path: bool,
    current_target: AtomicUsize,
    desktop_visual_lock: Arc<AtomicU64>,
}

impl IDropSource_Impl for FileDropSource_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        if fescapepressed.as_bool() {
            DRAGDROP_S_CANCEL
        } else if !grfkeystate.contains(MK_LBUTTON) {
            let mut point = POINT::default();
            if unsafe { GetCursorPos(&mut point) }.is_ok() {
                let _ = self.release_point.set(point);
                let target = HWND(self.current_target.load(Ordering::Acquire) as *mut c_void);
                if self.allow_desktop_move_fast_path
                    && desktop_move_fast_path_modifiers_allowed(grfkeystate)
                    && crate::desktop::drop_position::is_empty_desktop_target(target, point)
                {
                    self.desktop_drop_defer_refresh
                        .store(true, Ordering::Release);
                    let visual_lock =
                        crate::desktop::drop_position::begin_desktop_visual_lock(target);
                    self.desktop_visual_lock
                        .store(visual_lock, Ordering::Release);
                    // 这是一次真实的成功落下；源条目已在进入 OLE 前暂时隐藏。
                    return DRAGDROP_S_DROP;
                }
            }
            DRAGDROP_S_DROP
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
        Ok(())
    }

    fn DragLeaveTarget(&self) -> Result<()> {
        self.current_target.store(0, Ordering::Release);
        Ok(())
    }
}
