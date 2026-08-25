// 栅栏窗口:分层窗口(WS_EX_LAYERED)+ UpdateLayeredWindow 整幅提交(逐像素 alpha)。
// 半透明深色面板 = 真透明(直接透出桌面,无模糊);内容(标题/图标)不透明。
// 圆角由 DWM 裁(DWMWCP_ROUND 对分层窗口同样生效)。
// 注:原生 DWM 亚克力(系统背景)与 GDI 内容不兼容 —— 一画内容整窗就物化成不透明
// 表面盖死磨砂且无法还原,故放弃;分层窗口 + 逐像素 alpha 是唯一可靠方案。
// 本文件是 fence 模块树入口:只留类型/常量 + 对外重导出。
// 职责分放子模块(geometry/render/grid/refresh/window/menu),行为与拆前一致。

mod geometry;
mod grid;
mod interaction;
mod menu;
mod model;
mod refresh;
mod render;
mod window;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::config::FenceCfg;

use refresh::REFRESH_DEBOUNCE_MS;
use render::RenderCache;

// 对外 API 重导出:保持 crate::fence::X 全部调用点不变
pub use geometry::{dpi_scale, min_h, min_w, set_icon_px, set_title_font_px, window_dpi};
pub use grid::{config_snapshot, settle_fence};
pub use interaction::{FenceInteraction, ResizeDir};
pub use model::{Entry, FenceModel};
pub use refresh::refresh_entries;
pub use render::{render_fence, start_perf_animation};
pub use window::{create_window, register_class, schedule_render};

pub const WM_APP_REFRESH: u32 = WM_APP + 1;
pub const WM_APP_DROP: u32 = WM_APP + 2;
/// “显示桌面”会尝试最小化所有独立顶层窗口；异步恢复可避免在 WM_SIZE 内递归。
pub const WM_APP_DESKTOP_RESTORE: u32 = WM_APP + 20;

struct RefreshState {
    queued: bool,
    last_event: Instant,
}

impl Default for RefreshState {
    fn default() -> Self {
        Self {
            queued: false,
            last_event: Instant::now(),
        }
    }
}

impl RefreshState {
    fn record_event(&mut self, now: Instant) -> bool {
        self.last_event = now;
        if self.queued {
            false
        } else {
            self.queued = true;
            true
        }
    }

    fn timer_action(&mut self, now: Instant, delay: Duration) -> RefreshTimerAction {
        if !self.queued {
            return RefreshTimerAction::Idle;
        }
        let elapsed = now.saturating_duration_since(self.last_event);
        if elapsed < delay {
            let remaining = delay - elapsed;
            return RefreshTimerAction::Wait(
                remaining.as_millis().clamp(1, u32::MAX as u128) as u32
            );
        }
        self.queued = false;
        RefreshTimerAction::Refresh
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RefreshTimerAction {
    Idle,
    Wait(u32),
    Refresh,
}

#[derive(Clone, Default)]
pub struct RefreshSignal {
    state: Arc<Mutex<RefreshState>>,
}

impl RefreshSignal {
    /// 同一栅栏最多排队一条刷新消息，避免目录事件风暴淹没 UI 消息队列。
    pub fn post(&self, hwnd: HWND) {
        let should_post = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record_event(Instant::now());
        if !should_post {
            return;
        }
        let posted = unsafe { PostMessageW(Some(hwnd), WM_APP_REFRESH, WPARAM(0), LPARAM(0)) };
        if posted.is_err() {
            self.cancel();
        }
    }

    fn timer_action(&self) -> RefreshTimerAction {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .timer_action(
                Instant::now(),
                Duration::from_millis(REFRESH_DEBOUNCE_MS as u64),
            )
    }

    fn cancel(&self) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .queued = false;
    }
}

unsafe impl Send for Fence {}

pub struct Fence {
    pub cfg: FenceCfg,
    pub hwnd: HWND,
    /// 所在显示器的 DPI 缩放因子(Per-Monitor)。窗口跨屏/缩放变化时由 WM_DPICHANGED 更新。
    pub dpi: f32,
    /// 不依赖 Win32 的条目、分页和选择状态。
    pub model: FenceModel,
    /// 网格顶部行号(浮点):翻页动画中平滑变化,静止时 = page × rows
    pub top_row: f32,
    /// 翻页动画计时器是否在跑
    pub animating: bool,
    /// 翻页动画起始时间 + 起始 top_row(固定时长 ease-out 用)
    pub anim_started: Instant,
    pub anim_from: f32,
    perf_anim_remaining: u32,
    /// 鼠标、滚轮、移动、缩放和条目拖出状态。
    pub interaction: FenceInteraction,
    /// 目录监听线程与窗口消息之间的刷新合并信号。
    pub refresh_signal: RefreshSignal,
    /// 已渲染 DIB 缓存:ULW 整幅提交的源(内容不保留,必须自己存)
    cache: Option<RenderCache>,
    pub valid: bool,
}

impl Fence {
    pub fn new(cfg: FenceCfg, hwnd: HWND) -> Self {
        Fence {
            cfg,
            hwnd,
            dpi: window_dpi(hwnd),
            model: FenceModel::default(),
            top_row: 0.0,
            animating: false,
            anim_started: Instant::now(),
            anim_from: 0.0,
            perf_anim_remaining: 0,
            interaction: FenceInteraction::default(),
            refresh_signal: RefreshSignal::default(),
            cache: None,
            valid: true,
        }
    }
}
