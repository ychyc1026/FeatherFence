# FeatherFence 个人主线架构说明

本文档描述个人仓库 `main` 的当前实现，而不是尚未落地的目标蓝图。同步基线为
`v0.1.4`、PR #30 之后：`cargo test` 共 61 项通过，release 构建成功。

## 1. 当前目标与范围

个人主线当前处于“稳定基线优先”阶段：

- 保留纯 Rust + Win32、低资源占用和单进程桌面工具定位；
- 修复已经确认的数据安全、资源生命周期和窗口层级问题；
- 用小型、单目的、可验证 PR 演进，不做整体重写；
- 让配置、业务状态、窗口状态和平台资源的所有权可追踪；
- 保持个人仓库可继续开发，也保留理解每次改动的清晰历史。

当前明确暂停：

- D；
- E（桌面直接落点实验）；
- 新的桌面整理功能扩张；
- UI 框架迁移、多 crate 工作区和为“现代化”而替换已工作的原生实现。

## 2. 当前模块边界

```text
src/
├─ main.rs                  # 进程入口、UI 消息循环、Global 和模块组合
├─ app/
│  └─ command.rs            # AppCommand 队列、PostMessage 唤醒和防递归 drain
├─ desktop/
│  ├─ host.rs               # 桌面宿主发现、桌面层级和 ListView 查找
│  └─ avoidance.rs          # Explorer 图标禁放区、移动历史和撤销
├─ fence/
│  ├─ mod.rs                # Fence 运行期组合与公共入口
│  ├─ model.rs              # 条目、分页和选择状态
│  ├─ interaction.rs        # 鼠标、拖动、缩放和拖出状态
│  ├─ geometry.rs           # DPI、尺寸和布局参数
│  ├─ grid.rs               # 网格、分页和吸附
│  ├─ render.rs             # GDI/GDI+ 渲染与 RenderCache
│  ├─ window.rs             # HWND 创建和窗口消息处理
│  ├─ menu.rs               # 栅栏菜单及模态输入
│  └─ refresh.rs            # 目录条目刷新与事件合并
├─ transfer/
│  ├─ drag_source.rs        # OLE 拖出和最终效果反馈
│  ├─ drop_target.rs        # OLE 拖入、效果协商和注册所有权
│  └─ file_ops.rs           # 复制、移动、重名、回滚和链接策略
├─ fencelife.rs             # 栅栏创建/删除/重建、可见性及拖入业务流程
├─ config.rs                # 配置模型、迁移、原子保存、备份和恢复
├─ hotkey.rs                # 全局热键注册所有权
├─ icons.rs                 # Shell 图标抽取和 LRU 缓存
├─ watcher.rs               # ReadDirectoryChangesW 监听器
├─ shortcut.rs              # 桌面快捷方式自动收纳
├─ download.rs              # 下载收纳箱候选跟踪
├─ sweep.rs                 # 桌面清扫规则
├─ tray.rs                  # 托盘图标和菜单
└─ utils.rs                 # DPI、屏幕和字符串工具
```

模块拆分遵循“先收窄职责、再考虑类型重组”。`main.rs` 和 `Fence` 仍是较大的组合对象，但
纯模型、交互、渲染、桌面和传输边界已经独立，不再需要把所有实现放在单个文件中。

## 3. UI 状态与可重入模型

### 3.1 `Global` 只属于 UI 线程

`Global` 存放配置、所有栅栏、消息窗口、图标缓存、目录监听器、自动收纳候选、清扫重试、
Zen 状态和热键所有权。它位于 UI 线程的：

```rust
thread_local! {
    static G: RefCell<Option<Global>> = ...;
}
```

`with_global` 使用 `try_borrow_mut` 获取唯一可变访问。若同步消息重入时再次访问，会立即暴露
错误，而不是绕过 Rust 借用规则。当前已经不存在旧架构中的 `Mutex<Global>`、`G_PTR` 或
从裸指针重新构造 `&mut Global` 的路径。

### 3.2 `AppCommand` 处理必须延后的状态变化

后台监听器和可能发生同步重入的窗口消息通过 `AppCommand` 排队：

```text
watcher / Win32 回调
        │ post(AppCommand)
        ▼
Mutex<CommandQueue>
        │ PostMessage(WM_APP_DISPATCH)
        ▼
UI 消息窗口
        │ drain（DispatchGuard 禁止递归 drain）
        ▼
dispatch_app_command → with_global
```

这里的 `Mutex` 只保护短生命周期命令队列，不保护 `Global`。命令只携带 ID、几何值和路径等
普通数据，不把 `&mut Global`、渲染资源或 COM 接口发送到后台线程。

### 3.3 模态调用采用 prepare / execute / complete

菜单、文件夹选择器、`DoDragDrop` 和窗口销毁可能启动嵌套消息处理。相关流程遵循：

1. `prepare`：在 `with_global` 内复制调用所需数据或先从状态中分离对象；
2. `execute`：退出借用后调用可能重入的 Win32 API；
3. `complete`：重新借用状态，或发布 `AppCommand` 应用结果。

窗口销毁、捕获取消和 DPI 变化在检测到已有全局借用时会排队，不会同步构造第二个可变借用。

## 4. 单个栅栏与平台资源所有权

`Fence` 当前组合：

- `FenceCfg`：可序列化配置；
- `HWND`、DPI 和窗口有效状态；
- `FenceModel`：条目、分页和选择；
- `FenceInteraction`：hover、按下、拖动、缩放和拖出；
- 翻页动画状态；
- `RefreshSignal`：监听事件合并；
- `RenderCache`：内存 DC、32bpp DIB 和像素指针；
- `RegisteredDropTarget`：与当前 HWND 一一绑定的 OLE 拖入注册。

关键资源使用 RAII 管理：

| 资源所有者 | 持有内容 | 销毁行为 |
|---|---|---|
| `RenderCache` | HDC、HBITMAP、原选中位图 | 恢复原对象，删除 DIB，再删除 DC |
| `RegisteredDropTarget` | HWND、`IDropTarget` | 先 `RevokeDragDrop`，再释放 COM 引用 |
| `RegisteredHotKey` | 消息 HWND、热键 ID | `UnregisterHotKey` |
| `ManagedWatcher` | 目录监听线程/句柄 | 从所有权容器移除时停止监听 |

尺寸重建、栅栏删除、Explorer 导致的窗口重建、热重载和程序退出都复用这些销毁路径。
平台对象保持在创建和使用它们的 UI/OLE 线程中；当前代码没有 `unsafe impl Send`。

## 5. 桌面宿主与窗口层级

栅栏是独立的非 TOPMOST 顶层分层窗口，不再挂为 Progman 子窗口。显示和定时维护都使用
`desktop/host.rs` 提供的统一边界，把栅栏插在桌面宿主之上、普通应用窗口之下。

桌面宿主规则：

- 显示层级优先使用 Progman，WorkerW 作为兜底；
- 桌面图标访问查找包含 `SHELLDLL_DefView` 的 WorkerW，再查找 `SysListView32`；
- `EnumWindows`/`EnumChildWindows` 的搜索状态位于调用栈，通过 `LPARAM` 同步传入回调；
- 不使用共享 `static mut` 临时结果，因此嵌套枚举不会互相覆盖；
- 仅在 WorkerW 不存在时尝试一次 `0x052C`，通过 `SendMessageTimeoutW` 最多等待 250ms。

启动显示、Win+D 恢复、watchdog 恢复和下载箱显示都经由相同桌面层级函数，避免先显示到
当前前台窗口之上再下沉。

## 6. 拖放与文件操作语义

OLE 层只负责数据格式、允许效果和最终反馈；文件系统语义集中在 `transfer/file_ops.rs`。

### 6.1 拖入

- 普通拖入优先 MOVE；按住 `Ctrl` 优先 COPY；
- 最终效果不会超过拖动源允许的效果；
- 多项目允许部分成功，失败项目会汇总并限制详情数量；
- 目标重名使用 `名称 (1)`、`名称 (2)` 形式，绝不覆盖已有项目。

### 6.2 移动和复制

- 同卷移动首先使用 `rename`，大文件和大目录只修改目录记录；
- 跨卷文件移动采用“创建新目标 → 完整复制并刷新 → 删除源文件”；
- 删除源文件失败时尽量删除本次创建的目标副本并报告回滚结果；
- 只有本次调用确认创建的目标才允许清理，竞态中由其他程序创建的同名目标不会被删除；
- 跨卷目录移动暂不支持，不做可能留下半成品的递归复制。

### 6.3 文件系统链接

目录复制使用 `symlink_metadata` 检查每一级项目。符号链接和 NTFS junction 不会被跟随，避免
复制到源树之外或形成循环；发现后会回滚本次创建的目标目录并指出路径。

同卷 `rename` 可以原样移动链接。跨卷回退会拒绝链接，避免把链接静默转换成目标内容。
断开的链接也会占用其名称。判断依据是 `FileType::is_symlink`，不会仅因为存在普通重解析属性
就拒绝 OneDrive 等非链接型占位项。

## 7. 配置持久化

配置位于 `%APPDATA%\feather-fences\config.json`。保存流程：

1. 在内存中完成 JSON 序列化；
2. 写入同目录 `.tmp` 文件；
3. `sync_all` 并关闭临时文件；
4. 若当前主配置有效，将它原子写入 `.bak`；
5. 使用 `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)` 原子替换主配置；
6. 失败时清理临时文件，不覆盖已有有效配置。

加载时有效主配置优先。主配置缺失或损坏时读取 `.bak`，恢复成功后修复主配置；无有效备份时
才回到默认配置。损坏主配置不会覆盖已有有效备份。

## 8. 线程与子系统边界

| 内容 | 所在线程 | 约束 |
|---|---|---|
| `Global`、HWND、交互和渲染 | UI | 只通过 `with_global` 短借用 |
| GDI/GDI+ 缓存 | UI | 创建、使用和销毁在 UI 生命周期内 |
| OLE DropTarget/DropSource | OLE 初始化的 UI 线程 | 不跨线程移动未封送 COM 接口 |
| 目录监听 | watcher 线程 | 只发送路径事件或 `AppCommand` |
| 命令队列 | 多生产者、UI 消费 | 锁内只做 push/pop，不执行业务 |
| 文件复制和移动 | 当前为 UI 拖放完成路径 | 保证回滚语义；大规模异步化尚未引入 |
| 配置保存 | 当前为 UI 路径 | 使用完整快照和原子替换 |

程序退出顺序先停止 watcher，再保存配置并取出 `Global`，让 OLE、热键、GDI 等所有者在对应
子系统关闭前析构，最后销毁消息窗口并关闭 GDI+/OLE。

## 9. 架构不变量

后续修改继续遵守：

1. `Global` 只由 UI 线程拥有和修改；
2. 可能同步重入的 Win32 调用期间不持有 `Global` 可变借用；
3. 后台线程只发送普通数据，不持有 UI 状态或未封送 COM 接口；
4. Win32 资源必须有明确所有者和成对释放路径；
5. 业务策略优先使用普通 Rust 类型并提供单元测试；
6. 持久配置不包含 HWND、COM、线程句柄或渲染缓存；
7. 文件失败路径不静默覆盖、不删除所有权不明的目标；
8. 纯重构不改变可见行为，行为修改单独提交；
9. 每个 PR 只有一个目的，可独立解释、验证和回滚；
10. D、E 和新功能在重新评估前不进入稳定主线。

## 10. 已完成的演进

| 阶段 | 个人仓库 PR | 结果 |
|---|---:|---|
| A / B / C | #1–#3 | 指针捕获、拖出语义、快捷方式抑制分别独立落地 |
| R0 | #5 | rustfmt 基线 |
| R1–R2 | #6–#7 | 桌面宿主和图标避让模块边界 |
| R3 | #8–#9 | Fence 模型与交互状态边界 |
| R4 | #10–#11 | OLE 与文件操作边界 |
| R5 | #12–#21 | 命令队列、模态调用拆分、删除重入裸指针状态 |
| R6 | #22–#23 | 配置原子保存、有效备份和损坏恢复 |
| F1 | #24 | 启动和恢复统一落到桌面层级 |
| S1 | #25 | 复制失败只清理本次创建的目标 |
| S2 | #26 | `RenderCache` GDI 资源 RAII |
| S3 | #27 | OLE DropTarget 与栅栏生命周期绑定 |
| S4 | #28 | 桌面宿主枚举可重入安全及消息超时 |
| S5 | #29 | Zen 热键注册反馈和自动注销 |
| S6 | #30 | 文件系统链接递归与跨卷回退策略 |

这些 PR 均已合并到个人 `main`，不是仍待实施的目标。

## 11. 当前验证与剩余工作

截至本文档同步：

- `cargo fmt --check` 通过；
- `cargo test`：61/61 通过；
- `cargo build --release` 通过；
- Clippy 基线为普通目标 44 条、测试目标 47 条既有告警，后续 PR 不得增加；
- 启动桌面层级曾完成多轮冷启动人工验证；资源和文件生命周期由真实 Win32/NTFS 测试覆盖。

剩余事项按优先级重新评估，不自动视为下一项功能：

- 清理 Clippy 遗留告警时必须按机械、小批次 PR 处理；
- `main.rs` 和 `Fence` 若继续拆分，应以减少依赖和字段可见性为目标，不机械搬文件；
- 旧分支清理属于仓库管理操作，应在稳定标签或明确保留点之后单独进行；
- 视觉层级、Explorer 重启和多显示器行为仍需在涉及对应代码时做最小人工验证；
- D、E 保持暂停，除非重新提出明确需求、边界和验证方案。

## 12. PR 验证标准

每个后续 PR 至少满足：

- 从最新个人 `main` 创建；
- 一个明确目的，不依赖未合并分支；
- `git diff --check` 和 `cargo fmt --check` 通过；
- 全部单元测试通过，纯重构不减少测试；
- Clippy 不增加 44/47 基线告警；
- release 构建通过；
- 涉及 Win32 可见行为时列出最小人工验证矩阵；
- 新增 `unsafe` 必须局限在平台边界并说明生命周期；
- 无法独立解释、验证或安全回滚的修改不合并。
