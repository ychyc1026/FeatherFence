#[derive(Clone, Copy, PartialEq)]
pub enum ResizeDir {
    N,
    S,
    E,
    W,
    NW,
    NE,
    SW,
    SE,
}

#[derive(Default)]
pub struct FenceInteraction {
    /// 滚轮增量累加器（1/120 刻度）；小增量满 120 再翻页。
    pub wheel_acc: i32,
    pub hover: Option<usize>,
    pub moving: bool,
    pub move_off: (i32, i32),
    pub resizing: Option<ResizeDir>,
    /// 按下后是否真的拖动或缩放过，用于区分单击标题和拖动。
    pub drag_moved: bool,
    pub hover_visible: bool,
    /// 按下的条目索引；移动超过阈值后启动 OLE 拖拽。
    pub drag_idx: Option<usize>,
    /// 按下时的客户区坐标，用于判断拖拽阈值。
    pub drag_down: (i32, i32),
}
