// 配置:JSON 持久化,位于 %APPDATA%\feather-fences\config.json
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use windows::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::core::PCWSTR;

use crate::utils::wstr;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FenceKind {
    /// 仅表示旧配置中缺少 kind 字段，加载后会立即迁移为明确类型。
    Legacy,
    #[default]
    Collection,
    Portal,
    Download,
}

fn legacy_fence_kind() -> FenceKind {
    FenceKind::Legacy
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FenceCfg {
    pub id: u32,
    pub title: String,
    /// 栅栏的业务类型；旧配置缺少该字段时由 load() 自动推断。
    #[serde(default = "legacy_fence_kind")]
    pub kind: FenceKind,
    /// None = 收纳栅栏(空投区,拖入的文件移动到 vault)
    pub folder: Option<PathBuf>,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// 保存该物理窗口矩形时的窗口 DPI。0 表示旧配置未记录。
    #[serde(default)]
    pub dpi: u32,
    /// 是否已记录过真实位置。None = 旧配置(信任保存的 x/y,即使为 0,0);
    /// Some(true) = 本版本放置过(0,0 也是合法位置)。用于区分"未放置"与"恰好放左上角"。
    #[serde(default)]
    pub pos_set: Option<bool>,
    #[serde(default = "default_opacity")]
    pub opacity: f32,
    /// 图标尺寸(旧版存于栅栏上;现由 Config.icon 全局统一。保留字段仅用于一次性迁移)
    #[serde(default = "default_icon")]
    pub icon: u32,
}

fn default_opacity() -> f32 {
    0.7
}

fn default_icon() -> u32 {
    32
}

fn default_title_font_size() -> u32 {
    12
}

fn default_zen_hotkey() -> Option<String> {
    Some("Ctrl+Alt+Z".into())
}

pub fn normalize_title_font_size(value: u32) -> u32 {
    value.clamp(10, 32)
}

fn default_true() -> bool {
    true
}

impl Default for FenceCfg {
    fn default() -> Self {
        FenceCfg {
            id: 0,
            title: "栅栏".into(),
            kind: FenceKind::Collection,
            folder: None,
            x: 0,
            y: 0,
            w: 260,
            h: 340,
            dpi: 96,
            opacity: default_opacity(),
            icon: default_icon(),
            pos_set: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SweepRule {
    /// 小写带点扩展名,如 ".jpg"
    pub ext: String,
    pub dest: PathBuf,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    #[serde(default)]
    pub fences: Vec<FenceCfg>,
    #[serde(default)]
    pub sweep_rules: Vec<SweepRule>,
    #[serde(default)]
    pub ghost_mode: bool,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub vault_dir: Option<PathBuf>,
    /// 专用“下载收纳箱”的栅栏 id。程序只接管启动后新出现在桌面的文件。
    #[serde(default)]
    pub download_box_id: Option<u32>,
    /// 是否接管程序运行后新出现在桌面的下载文件。
    #[serde(default = "default_true")]
    pub download_enabled: bool,
    /// 下载接管开启时，是否显示专用收纳箱窗口。
    #[serde(default = "default_true")]
    pub download_box_visible: bool,
    /// 全局图标尺寸(逻辑像素,默认 32)
    #[serde(default = "default_icon")]
    pub icon: u32,
    /// 全局栅栏标题字号(逻辑像素,默认 12)
    #[serde(default = "default_title_font_size")]
    pub title_font_size: u32,
    /// Zen 模式全局快捷键。空值表示禁用；旧配置默认使用 Ctrl+Alt+Z。
    #[serde(default = "default_zen_hotkey")]
    pub zen_hotkey: Option<String>,
    /// 配置格式版本:
    /// - 缺省/1:旧版物理 x/y/w/h,未记录 DPI
    /// - 2:逻辑 x/y/w/h,启动时统一乘系统 DPI
    /// - 3:物理 x/y/w/h + 每栅栏保存时 DPI
    /// 桌面图标避让:开启后栅栏覆盖的区域作为禁放区,把被盖住的桌面图标
    /// 就近搬到空闲网格(默认关闭;开启会关闭 Explorer 的自动排列)。
    #[serde(default)]
    pub desktop_avoid: bool,
    #[serde(default)]
    pub version: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            fences: Vec::new(),
            sweep_rules: Vec::new(),
            ghost_mode: false,
            autostart: false,
            vault_dir: None,
            download_box_id: None,
            download_enabled: true,
            download_box_visible: true,
            icon: default_icon(),
            title_font_size: default_title_font_size(),
            zen_hotkey: default_zen_hotkey(),
            desktop_avoid: false,
            version: 3,
        }
    }
}

/// 把磁盘配置迁移为 v3 的物理像素布局:
/// - v1 是物理像素且没有 DPI,保留未知值 0,由窗口创建后用实际 DPI 接管。
/// - v2 的四个字段都是逻辑像素,按旧规则乘系统 DPI 做一次性尽力迁移。
/// - v3 已是物理像素,保持 x/y 不变;窗口创建后再按保存 DPI 调整 w/h。
/// 调用点:进程启动 load() 之后、MENU_RELOAD 之后。
pub fn normalize_dpi(c: &mut Config) {
    let system_dpi = (crate::fence::dpi_scale() * 96.0).round() as u32;
    normalize_dpi_with_system(c, system_dpi);
}

fn normalize_dpi_with_system(c: &mut Config, system_dpi: u32) {
    if c.version == 2 {
        let s = system_dpi as f32 / 96.0;
        for f in &mut c.fences {
            if s != 1.0 {
                f.x = (f.x as f32 * s).round() as i32;
                f.y = (f.y as f32 * s).round() as i32;
                f.w = (f.w as f32 * s).round() as i32;
                f.h = (f.h as f32 * s).round() as i32;
            }
            f.dpi = system_dpi;
        }
    }
    c.version = 3;
}

/// 保持逻辑尺寸不变,把一个物理像素长度从保存 DPI 换算到当前窗口 DPI。
pub fn scale_extent_for_dpi(value: i32, saved_dpi: u32, current_dpi: u32) -> i32 {
    // v1 没有保存 DPI;把未知值视为当前窗口 DPI可原样保留旧物理尺寸。
    let from = if saved_dpi == 0 {
        current_dpi
    } else {
        saved_dpi
    }
    .max(1) as f64;
    ((value as f64 * current_dpi.max(1) as f64) / from).round() as i32
}

#[cfg(test)]
mod dpi_tests {
    use super::*;

    fn fixture(version: u32, x: i32, w: i32, dpi: u32) -> Config {
        Config {
            version,
            fences: vec![FenceCfg {
                x,
                w,
                dpi,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn v1_physical_geometry_stays_physical_and_dpi_remains_unknown() {
        let mut c = fixture(1, 2400, 260, 0);
        normalize_dpi_with_system(&mut c, 192);
        assert_eq!(
            (c.fences[0].x, c.fences[0].w, c.fences[0].dpi),
            (2400, 260, 0)
        );
        assert_eq!(c.version, 3);
    }

    #[test]
    fn v2_logical_geometry_uses_the_legacy_system_dpi_migration() {
        let mut c = fixture(2, 1000, 260, 0);
        normalize_dpi_with_system(&mut c, 192);
        assert_eq!(
            (c.fences[0].x, c.fences[0].w, c.fences[0].dpi),
            (2000, 520, 192)
        );
        assert_eq!(c.version, 3);
    }

    #[test]
    fn v3_physical_geometry_is_not_rescaled_by_system_dpi() {
        let mut c = fixture(3, 2000, 520, 192);
        normalize_dpi_with_system(&mut c, 96);
        assert_eq!(
            (c.fences[0].x, c.fences[0].w, c.fences[0].dpi),
            (2000, 520, 192)
        );
    }

    #[test]
    fn unknown_saved_dpi_preserves_v1_extent() {
        assert_eq!(scale_extent_for_dpi(260, 0, 192), 260);
        assert_eq!(scale_extent_for_dpi(520, 192, 144), 390);
    }

    #[test]
    fn pos_set_legacy_config_is_none_and_roundtrips() {
        // 旧配置没有 pos_set 字段 → None(信任保存的 x/y,含 (0,0))
        let old = r#"{"fences":[{"id":1,"title":"t","folder":null,"x":0,"y":0,"w":260,"h":340}]}"#;
        let c: Config = serde_json::from_str(old).unwrap();
        assert_eq!(c.fences[0].pos_set, None);
        // 新配置保存 Some(true),序列化后原样恢复
        let c2 = Config {
            fences: vec![FenceCfg {
                id: 2,
                title: "t".into(),
                x: 0,
                y: 0,
                pos_set: Some(true),
                ..Default::default()
            }],
            ..Default::default()
        };
        let s = serde_json::to_string(&c2).unwrap();
        let back: Config = serde_json::from_str(&s).unwrap();
        assert_eq!(back.fences[0].pos_set, Some(true));
        assert_eq!(back.fences[0].x, 0);
    }

    #[test]
    fn legacy_config_keeps_download_capture_enabled_and_visible() {
        let c: Config = serde_json::from_str("{}").unwrap();
        assert!(c.download_enabled);
        assert!(c.download_box_visible);
        assert_eq!(c.title_font_size, 12);
        assert_eq!(c.zen_hotkey.as_deref(), Some("Ctrl+Alt+Z"));
    }

    #[test]
    fn title_font_size_is_clamped_to_supported_bounds() {
        assert_eq!(normalize_title_font_size(0), 10);
        assert_eq!(normalize_title_font_size(18), 18);
        assert_eq!(normalize_title_font_size(100), 32);
    }

    #[test]
    fn zen_hotkey_can_be_disabled_and_roundtrips() {
        let config = Config {
            zen_hotkey: None,
            ..Config::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.zen_hotkey, None);
    }

    #[test]
    fn missing_fence_kind_deserializes_as_legacy() {
        let mut value = serde_json::to_value(FenceCfg::default()).unwrap();
        value.as_object_mut().unwrap().remove("kind");

        let fence: FenceCfg = serde_json::from_value(value).unwrap();

        assert_eq!(fence.kind, FenceKind::Legacy);
    }

    #[test]
    fn legacy_fence_kinds_are_migrated_from_existing_configuration() {
        let boxes_root = PathBuf::from(r"C:\Users\test\AppData\Roaming\feather-fences\boxes");
        let legacy = |id, folder| FenceCfg {
            id,
            kind: FenceKind::Legacy,
            folder,
            ..FenceCfg::default()
        };
        let mut c = Config {
            download_box_id: Some(1),
            fences: vec![
                legacy(1, Some(boxes_root.join("下载收纳箱"))),
                legacy(2, None),
                legacy(3, Some(boxes_root.join("收纳箱"))),
                legacy(4, Some(PathBuf::from(r"D:\Documents"))),
                legacy(5, Some(boxes_root.join("nested").join("portal"))),
                FenceCfg {
                    id: 6,
                    kind: FenceKind::Portal,
                    folder: Some(boxes_root.join("explicit")),
                    ..FenceCfg::default()
                },
            ],
            ..Config::default()
        };

        migrate_fence_kinds_with_root(&mut c, &boxes_root);

        assert_eq!(c.fences[0].kind, FenceKind::Download);
        assert_eq!(c.fences[1].kind, FenceKind::Collection);
        assert_eq!(c.fences[2].kind, FenceKind::Collection);
        assert_eq!(c.fences[3].kind, FenceKind::Portal);
        assert_eq!(c.fences[4].kind, FenceKind::Portal);
        assert_eq!(c.fences[5].kind, FenceKind::Portal);
    }
}

pub fn config_dir() -> PathBuf {
    if crate::perf::enabled() {
        if let Some(path) = std::env::var_os("FEATHER_PERF_CONFIG_DIR") {
            return PathBuf::from(path);
        }
    }
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("feather-fences")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

fn migrate_fence_kinds_with_root(c: &mut Config, boxes_root: &Path) {
    for fence in &mut c.fences {
        if fence.kind != FenceKind::Legacy {
            continue;
        }
        fence.kind = if c.download_box_id == Some(fence.id) {
            FenceKind::Download
        } else if fence.folder.is_none()
            || fence
                .folder
                .as_deref()
                .and_then(Path::parent)
                .is_some_and(|parent| parent == boxes_root)
        {
            FenceKind::Collection
        } else {
            FenceKind::Portal
        };
    }
}

pub fn default_vault_dir() -> PathBuf {
    config_dir().join("vault")
}

pub fn download_box_dir() -> PathBuf {
    config_dir().join("boxes").join("下载收纳箱")
}

pub fn vault_dir(c: &Config) -> PathBuf {
    c.vault_dir.clone().unwrap_or_else(default_vault_dir)
}

pub fn load() -> Config {
    load_from_path(&config_path())
}

pub fn save(c: &Config) {
    if let Err(e) = save_to_path(c, &config_path()) {
        eprintln!("[feather] save config failed: {e}");
    }
}

fn load_from_path(path: &Path) -> Config {
    match read_config(path) {
        Ok((mut config, _)) => {
            migrate_fence_kinds_with_root(&mut config, &boxes_root_for(path));
            config
        }
        Err(primary_error) => {
            let backup_path = backup_path_for(path);
            match read_config(&backup_path) {
                Ok((mut config, bytes)) => {
                    eprintln!(
                        "[feather] primary config unavailable ({primary_error}); recovering from {}",
                        backup_path.display()
                    );
                    if let Err(error) = atomic_write(path, &bytes) {
                        eprintln!("[feather] restore primary config failed: {error}");
                    }
                    migrate_fence_kinds_with_root(&mut config, &boxes_root_for(path));
                    config
                }
                Err(backup_error) => {
                    if path.exists() || backup_path.exists() {
                        eprintln!(
                            "[feather] config recovery failed: primary={primary_error}; backup={backup_error}"
                        );
                    }
                    Config::default()
                }
            }
        }
    }
}

fn save_to_path(config: &Config, path: &Path) -> io::Result<()> {
    let serialized = serde_json::to_vec_pretty(config).map_err(io::Error::other)?;
    if let Ok((_, previous)) = read_config(path) {
        let backup_path = backup_path_for(path);
        if let Err(error) = atomic_write(&backup_path, &previous) {
            eprintln!("[feather] backup config failed: {error}");
        }
    }
    atomic_write(path, &serialized)
}

fn read_config(path: &Path) -> io::Result<(Config, Vec<u8>)> {
    let bytes = fs::read(path)?;
    let config = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok((config, bytes))
}

fn boxes_root_for(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("boxes")
}

fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let temp_path = temp_path_for(path);
    let result = (|| {
        let mut temp = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp_path)?;
        temp.write_all(contents)?;
        temp.sync_all()?;
        drop(temp);

        let from = wstr(&temp_path.to_string_lossy());
        let to = wstr(&path.to_string_lossy());
        unsafe {
            MoveFileExW(
                PCWSTR(from.as_ptr()),
                PCWSTR(to.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|error| io::Error::other(error.to_string()))
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn temp_path_for(path: &Path) -> PathBuf {
    sibling_path_with_suffix(path, ".tmp")
}

fn backup_path_for(path: &Path) -> PathBuf {
    sibling_path_with_suffix(path, ".bak")
}

fn sibling_path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("config.json"));
    file_name.push(suffix);
    path.with_file_name(file_name)
}

/// 确保目标目录存在,返回是否成功
pub fn ensure_dir(p: &Path) -> bool {
    if p.exists() {
        return p.is_dir();
    }
    fs::create_dir_all(p).is_ok()
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn test_dir() -> PathBuf {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "feather-fences-config-test-{}-{id}",
            std::process::id()
        ))
    }

    #[test]
    fn atomic_write_creates_parent_and_complete_json() {
        let dir = test_dir();
        let path = dir.join("nested").join("config.json");
        let config = Config {
            ghost_mode: true,
            fences: vec![FenceCfg {
                id: 7,
                title: "测试栅栏".into(),
                ..FenceCfg::default()
            }],
            ..Config::default()
        };
        let serialized = serde_json::to_string_pretty(&config).unwrap();

        atomic_write(&path, serialized.as_bytes()).unwrap();

        let saved: Config = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(saved.ghost_mode);
        assert_eq!(saved.fences[0].title, "测试栅栏");
        assert!(!temp_path_for(&path).exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn atomic_write_replaces_existing_config_without_leaving_temp_file() {
        let dir = test_dir();
        let path = dir.join("config.json");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, br#"{"version":1}"#).unwrap();
        let replacement = serde_json::to_vec_pretty(&Config {
            title_font_size: 18,
            ..Config::default()
        })
        .unwrap();

        atomic_write(&path, &replacement).unwrap();

        let saved: Config = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.title_font_size, 18);
        assert!(!temp_path_for(&path).exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_replace_keeps_destination_and_cleans_temp_file() {
        let dir = test_dir();
        let path = dir.join("config.json");
        fs::create_dir_all(&path).unwrap();

        let error = atomic_write(&path, b"replacement").unwrap_err();

        assert!(
            path.is_dir(),
            "failed replacement must keep its destination"
        );
        assert!(!temp_path_for(&path).exists());
        assert_ne!(error.kind(), io::ErrorKind::NotFound);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn saving_replaces_backup_only_with_previous_valid_config() {
        let dir = test_dir();
        let path = dir.join("config.json");
        let first = Config {
            title_font_size: 14,
            ..Config::default()
        };
        let second = Config {
            title_font_size: 20,
            ..Config::default()
        };

        save_to_path(&first, &path).unwrap();
        save_to_path(&second, &path).unwrap();

        let current = read_config(&path).unwrap().0;
        let backup = read_config(&backup_path_for(&path)).unwrap().0;
        assert_eq!(current.title_font_size, 20);
        assert_eq!(backup.title_font_size, 14);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn saving_over_corrupt_primary_preserves_valid_backup() {
        let dir = test_dir();
        let path = dir.join("config.json");
        let backup_path = backup_path_for(&path);
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, b"broken json").unwrap();
        let backup = serde_json::to_vec_pretty(&Config {
            title_font_size: 16,
            ..Config::default()
        })
        .unwrap();
        fs::write(&backup_path, &backup).unwrap();

        save_to_path(
            &Config {
                title_font_size: 22,
                ..Config::default()
            },
            &path,
        )
        .unwrap();

        assert_eq!(read_config(&path).unwrap().0.title_font_size, 22);
        assert_eq!(read_config(&backup_path).unwrap().0.title_font_size, 16);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_primary_recovers_from_backup_and_repairs_primary() {
        let dir = test_dir();
        let path = dir.join("config.json");
        let backup_path = backup_path_for(&path);
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, b"{ incomplete").unwrap();
        let backup = serde_json::to_vec_pretty(&Config {
            ghost_mode: true,
            title_font_size: 17,
            ..Config::default()
        })
        .unwrap();
        fs::write(&backup_path, backup).unwrap();

        let recovered = load_from_path(&path);

        assert!(recovered.ghost_mode);
        assert_eq!(recovered.title_font_size, 17);
        let repaired = read_config(&path).unwrap().0;
        assert!(repaired.ghost_mode);
        assert_eq!(repaired.title_font_size, 17);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn valid_primary_wins_over_backup() {
        let dir = test_dir();
        let path = dir.join("config.json");
        let backup_path = backup_path_for(&path);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            &path,
            serde_json::to_vec_pretty(&Config {
                title_font_size: 19,
                ..Config::default()
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(
            backup_path,
            serde_json::to_vec_pretty(&Config {
                title_font_size: 13,
                ..Config::default()
            })
            .unwrap(),
        )
        .unwrap();

        assert_eq!(load_from_path(&path).title_font_size, 19);
        fs::remove_dir_all(dir).unwrap();
    }
}
