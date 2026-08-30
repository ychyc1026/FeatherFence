# FeatherFence 个人增强分支说明

## 1. 定位

本仓库是 FeatherFence 的个人 Fork。`main` 保留与已合并上游改动一致的基线；个人使用版本在
`personal/v11-stable` 分支继续维护。个人版以本机日常使用、理解源码和验证改进为目标，不代表
原作者发布的正式版本，也不会把尚未充分验证的功能直接提交给上游。

首个个人预发行版为
[`v0.1.4-personal.1`](https://github.com/ychyc1026/FeatherFence/releases/tag/v0.1.4-personal.1)，仅支持
Windows 10/11 x64。发行附件中的 EXE 未进行代码签名，Windows SmartScreen 可能显示未知发布者提示。

## 2. 基线与独立改动

个人稳定分支从 `c9ab9f1` 开始，当前功能拆为两个独立提交：

- `48761ed`：支持从托盘配置或禁用 Zen 全局快捷键；默认使用 `Ctrl+Shift+Z`，注册新组合失败时
  保留原有可用热键和配置。
- `0f5f834`：改善单项目从栅栏拖到桌面时的鼠标落点、短暂重复图标和桌面布局回弹；在后续桌面
  文件变动前保存 Explorer 当前视图状态。

两部分保持独立提交，便于审查、回退和后续继续拆分。架构与资源所有权见
[`architecture.md`](architecture.md)，未完成问题见 [`remaining-work.md`](remaining-work.md)。

## 3. 桌面拖出链路的适用范围

桌面快速定位只在以下条件同时明确时启用：

- 单个文件或文件夹以普通拖动（或 `Shift`）执行 MOVE。
- 源目录与桌面位于同一卷。
- 桌面不存在同名项目，鼠标目标是空白桌面单元格。
- Explorer 桌面未启用自动排列，且桌面 ListView、COM 视图与坐标均可可靠取得。

按住 `Ctrl` 复制、跨卷移动、重名、非空白目标、自动排列或目标不确定时，程序回退到 Explorer
原生拖放行为，不强行写入桌面坐标。绘制锁和定位任务均有超时或生命周期兜底，失败时优先恢复
桌面显示。

## 4. 已完成验证

`v0.1.4-personal.1` 发布前完成：

- 71 项自动测试全部通过。
- `RUSTFLAGS="-D warnings"` 下的 Release 构建通过。
- `Ctrl+Shift+Z` 可隐藏并恢复所有栅栏。
- 图标从栅栏拖出后可落在鼠标释放位置。
- 图标落到桌面后再手动移动，随后将另一图标移入栅栏，不会回到先前落点。

上述人工验证来自当前开发机器，不等于覆盖所有 Windows 版本、显示器排列、DPI、Explorer 设置、
跨卷设备、重名、重解析点和第三方桌面增强软件组合。

## 5. 构建与附件校验

从发行标签构建：

```powershell
$env:RUSTFLAGS = "-D warnings"
cargo test --all-targets
cargo build --release
```

GitHub Release 会附带：

- `feather-fences.exe`
- `SHA256SUMS.txt`

下载后可在 PowerShell 校验：

```powershell
Get-FileHash -Algorithm SHA256 .\feather-fences.exe
```

## 6. 回退与后续维护

- 个人版出现问题时，可切回 `main`；配置与收纳箱位于 `%APPDATA%\feather-fences\`，切换程序前应先
  退出正在运行的 FeatherFence。
- 新功能先在独立分支实现和验证，不直接堆叠到稳定基线。
- 只有能独立解释、独立测试且不破坏原功能的修改，才考虑拆出上游 PR。
- 预发行阶段以记录真实使用问题为主，不承诺稳定 API 或配置格式长期不变。
