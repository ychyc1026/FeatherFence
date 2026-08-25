use std::path::PathBuf;

#[derive(Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

#[derive(Default)]
pub struct FenceModel {
    pub entries: Vec<Entry>,
    /// 当前页（从 0 开始）；滚轮按整页切换。
    pub page: usize,
    /// 单击选中的条目；Delete 键对它执行移入回收站。
    pub selected: Option<usize>,
}
