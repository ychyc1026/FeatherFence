# FeatherFences · 轻栅栏

> 超轻量桌面分区整理工具 —— 用 Rust 从零实现的 Fences 轻量版。内存占用极低,风格透明克制,不联网、不上传任何数据。

![FeatherFences 效果图](docs/screenshot.jpg)

## ✨ 特性

- **透明半透明栅栏** —— 分层窗口 (`WS_EX_LAYERED` + `UpdateLayeredWindow` 逐像素 Alpha),背景半透明面板直透桌面。无磨砂、无 DWM 特效,整幅替换每帧提交,不残留拖影。
- **图标网格 + 文字标签** —— 以文件系统目录为内容源,栅栏内按网格排布图标与名称。
- **拖入 / 拖出** —— 通过 OLE `IDropTarget` 把桌面或资源管理器的文件拖进栅栏,也可从栅栏拖出;文件夹门户实时跟随目录内容刷新。
- **文件夹栅栏 & 收纳栅栏** ——「文件夹栅栏」绑定任意目录做门户;「收纳栅栏」自动在配置目录下创建独立收纳箱,不再共享目录。
- **翻页 + 分页圆点** —— 图标超过一页自动分页,滚轮 / 触控板平滑翻页(cubic ease-out 动画),右侧圆点随页平滑缩放。
- **幽灵模式** —— 未悬停时整体淡出至 16% 透明度(逐像素 Alpha 直接透出桌面),鼠标靠近即还原。
- **禅模式 / 一键隐藏** —— 隐藏所有栅栏，可通过托盘菜单或全局快捷键切换；默认 `Ctrl+Shift+Z`，可在托盘中修改或禁用，热键冲突时保留原有可用设置。
- **桌面清扫** —— 按 `sweep_rules` 规则把桌面散落文件自动归类到对应收纳箱。
- **快捷方式自动收纳** —— 程序运行后桌面新增的 `.lnk` 写入稳定后会自动移入用户收纳箱；优先选择快捷方式占比最高的箱子，占比相同则选择快捷方式更多的箱子。
- **下载收纳箱** —— 自动创建专用收纳箱；程序启动后新出现在桌面的文件会在写入稳定后自动移入，浏览器临时下载文件不会被提前截断。
- **桌面图标避让** —— 把栅栏覆盖的桌面区域设为禁放区,Explorer 原生桌面图标自动**就近**搬移到最近空闲网格:以图标当前位置为圆心搜索,不打乱既有布局。开启后关闭系统自动排列(否则搬走的图标会被吸回禁放区),关闭时按开启前的原始状态原样恢复——你手动关过自动排列的自定义布局不会被强制吸附。托盘菜单提供「撤销并关闭避让」一键回退:**被搬走的图标与被移动/缩放的栅栏同时恢复原状**。搬移记录带 1 分钟存活期,超时视为图标已在新位置稳定、不再回退,长期开启不会累积内存。
- **开机自启** —— 写注册表 `HKCU\...\Run`,可选开关。
- **配置持久化** —— JSON 配置采用同目录临时文件原子替换,保存前保留最近一份有效备份；主配置损坏时可自动恢复,并支持热重载。

## 🛠️ 技术栈

| 模块 | 实现 |
| --- | --- |
| 语言 | Rust (edition 2024),纯 Win32 无框架依赖 |
| Windows API 绑定 | [`windows` crate 0.62](https://crates.io/crates/windows)(零安全抽象层之上的原生 FFI) |
| 分层渲染 | `UpdateLayeredWindow` + 32bpp 预乘 Alpha DIB,`WS_EX_LAYERED \| WS_EX_TOOLWINDOW`,`DWMWA_WINDOW_CORNER_PREFERENCE` 圆角 |
| 绘制 | GDI+(`Gdip*`)绘制半透明面板 / 文字 / 网格,GDI `DrawIconEx` 绘制图标(原生掩码/Alpha 处理,透明区正确) |
| 图标抽取 | `SHGetFileInfoW`(`SHGFI_SYSICONINDEX`)+ `SHGetImageList` 取 32bpp Alpha 图标,EXTRALARGE/LARGE 兜底,LRU 缓存(512 个) |
| 拖放 | OLE `IDropTarget` / `DoDragDrop` |
| 目录监听 | `ReadDirectoryChangesW`,文件夹门户实时刷新 |
| 开机自启 | [`winreg`](https://crates.io/crates/winreg) 写注册表 Run 键 |
| 配置 | [`serde`](https://crates.io/crates/serde) / `serde_json`,存于 `%APPDATA%\feather-fences\config.json` |
| 消息循环 | 原生 Win32 消息泵 + 定时器驱动重绘与翻页动画 |

发布配置:`opt-level=3` + LTO(`thin`)+ `codegen-units=1` + `strip` + `panic=abort`,追求极致体积与性能。

## 🚀 构建

```bash
cargo build --release
# 产物:target/release/feather-fences.exe,双击即可运行
```

提交前的完整本地验证：

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets
cargo build --release
```

> 仅支持 Windows 10/11(x64)。运行时会自动在 `%APPDATA%\feather-fences\` 下创建配置与收纳箱目录。

## 🎮 使用

- **创建栅栏** —— 托盘图标右键菜单:「新建文件夹栅栏…」(绑定目录)或「新建收纳栅栏」(独立收纳箱)。
- **拖入文件** —— 从桌面或资源管理器拖入时默认移动,按住 `Ctrl` 请求复制；同卷移动使用文件系统重命名,不会复制大文件内容。
- **跨卷与链接** —— 跨卷文件移动采用“复制完成后删除源文件”并在失败时回滚；跨卷文件夹移动暂不支持。复制目录时不会跟随符号链接或 NTFS junction,避免越界递归或循环。
- **翻页** —— 在栅栏上滚动滚轮(或触控板两指滚动)翻页,右侧圆点显示页码。
- **幽灵 / 禅模式** —— 托盘菜单切换。
- **Zen 快捷键** —— 托盘菜单选择「设置 Zen 快捷键…」，输入如 `Ctrl+Shift+Z` 或 `Alt+F8`；留空可禁用全局快捷键。
- **桌面清扫** —— 托盘菜单「立即整理桌面」,按 `sweep_rules` 自动归类。
- **快捷方式收纳** —— 保持程序运行；安装或更新软件后新增到桌面的 `.lnk` 会自动进入最适合存放快捷方式的用户收纳箱，文件夹栅栏和下载收纳箱不会参与选择。
- **下载接管** —— 无需手工配置；保持程序运行，把下载位置设为桌面，新文件完成写入后会出现在「下载收纳箱」。
- **桌面图标避让** —— 托盘菜单勾选「桌面图标避让」后,栅栏覆盖区域的桌面图标自动就近搬开,拖动/缩放栅栏时实时避让。想回到搬移前布局:点「撤销并关闭避让」,图标与栅栏一起回退并自动关闭该功能(搬移后 1 分钟内有效)。直接取消勾选则只关闭功能,图标保留当前位置。
- **删除栅栏** —— 右键栅栏 → 菜单删除;收纳栅栏仅移除条目,不删除磁盘文件。
- **配置** —— 托盘菜单「打开配置目录」可直达 JSON 配置与收纳箱。

## 📂 仓库结构

```
src/
  main.rs              进程入口、UI 消息循环、全局状态和模块组合
  app/command.rs       可重入安全的 AppCommand 队列与 UI 线程派发
  fence/               栅栏模型、交互、网格、渲染、窗口、菜单和刷新
  fencelife.rs         栅栏创建/删除/重建、可见性和拖入业务流程
  transfer/            OLE 拖入/拖出及文件复制、移动、重名和回滚策略
  desktop/host.rs      Progman/WorkerW/DefView/ListView 探测与桌面层级
  desktop/avoidance.rs Explorer 桌面图标避让和撤销历史
  config.rs            JSON 迁移、原子保存、有效备份和损坏恢复
  hotkey.rs            全局热键注册与自动注销
  icons.rs             系统图标抽取与 LRU 缓存
  watcher.rs           ReadDirectoryChangesW 目录监听
  shortcut.rs          桌面快捷方式自动收纳
  download.rs          下载收纳箱候选跟踪
  sweep.rs             桌面清扫规则
  tray.rs              托盘图标与菜单
```

更完整的线程、消息流和资源所有权说明见 [`docs/architecture.md`](docs/architecture.md)。

## ⚖️ 说明

- 本工具纯本地运行,不联网、不上传任何数据,所有配置与收纳内容均在本机 `%APPDATA%\feather-fences\`。
- 代码中的 `%APPDATA%` 均为运行时环境变量展开,不含任何个人路径或凭据。
