# herdr-freeze

自动冻结 Herdr 空闲面板进程树以释放内存，整个标签页全部冻结时弹出
「❄ 已冻结 · 按任意键恢复」蒙层，按键即解冻整个标签页。

这是一个 [Herdr](https://herdr.dev) 插件（目录 + `herdr-plugin.toml` 清单 +
一个 Rust 二进制 `herdr-freeze`）。冻结/解冻逻辑参考了 `work-assistant` 的
freezer（挂起进程树 + 裁剪工作集），并适配到 Herdr 的 pane/tab/workspace 模型。

## 行为

- **空闲判定**：守护进程周期轮询每个 pane 的 `content_revision`（`pane list`
  返回）。某 pane 的 revision 在阈值秒内未变化 → 视为空闲（agent 没在产出）。
  `pane.output_changed` 属高频事件，不在 Herdr 插件清单事件钩子内，故采用轮询。
- **冻结**：空闲 pane 通过 `pane process-info` 取前台进程 pid/进程组，挂起整棵
  进程树并裁剪工作集（释放物理内存）。冻结的 pane 标题前加 `❄` 标记。
- **蒙层**：当某 tab 下**所有** pane 都冻结时，对**当前聚焦的**该 tab 弹出一个
  `overlay` 插件 pane（缩放覆盖整个 tab 区域），进入 raw 模式、绘制「❄ 已冻结 ·
  按任意键恢复整个 tab」。非聚焦的全冻结 tab 暂不弹蒙层，等用户切到它时下一轮再弹
  （避免抢焦点）。部分冻结的 tab 只挂起 + 加 ❄ 标记，不弹蒙层。
- **解冻**：用户在蒙层上**按任意键** → 蒙层进程退出 → herdr 关闭该 overlay pane
  → 守护进程检测到 pane 关闭 → 恢复该 tab 全部冻结进程、还原标签、设冷却期
  （避免立刻再冻结）。
- **配置**：按 Herdr workspace 级别，用 workspace 元数据 token 存储：
  - `freeze_enabled`（`true`/`false`，默认 `true`）
  - `freeze_idle_secs`（秒，默认 `180`，下限 `30`）
  - 通过 `config` 动作（弹窗）或 `herdr-freeze config` CLI 设置。

## 跨平台冻结机制

- **Windows**：复刻 work-assistant freezer —— `NtSuspendProcess`/`NtResumeProcess`
  挂起/恢复整棵进程树（`CreateToolhelp32Snapshot` 遍历父子关系 + BFS，含竞态修复
  轮次），`K32EmptyWorkingSet` 裁剪工作集。完整进程树冻结。
- **Unix / macOS**：对 herdr 给出的前台进程组 id 发 `SIGSTOP`/`SIGCONT`（整组），
  覆盖 agent 及同组子孙；跨进程组派生的子孙不在范围内（macOS 无 `/proc` 树遍历，
  MVP 以进程组为准）。

守护进程只持久化「根 pid + 进程组 id」；恢复时按 pid 重新遍历进程树再开句柄解挂
—— 这样守护进程崩溃重启后也能按 pid 恢复上次残留的挂起进程。

## 安装（本地开发）

herdr 不允许同 id 的 action/pane 按平台重复声明（`duplicate_plugin_action_id`），
故清单用单一命令 + 纯名 `herdr-freeze`（herdr 在 Windows 上按 PATHEXT 解析
`.exe`）。**需要先把 release 二进制放到 PATH**：

```sh
# 方式一：cargo install（装到 ~/.cargo/bin，该目录通常在 PATH 上）
cargo install --path .

# 方式二：手动构建 + 拷贝
cargo build --release
cp target/release/herdr-freeze      ~/.cargo/bin/   # Linux/macOS
cp target/release/herdr-freeze.exe  ~/.cargo/bin/   # Windows
```

然后链接插件（link 不运行 build，故需先完成上一步）：

```sh
herdr plugin link ./herdr-freeze
herdr plugin list                          # 确认 herdr-freeze 已注册、enabled
herdr plugin action list --plugin herdr-freeze
```

链接后，**下次 herdr 会话恢复**时启动钩子会自动拉起监控守护进程（`herdr-freeze
monitor`）。启动钩子是一次性的、**非受监督**守护进程：若它崩溃，herdr 不会重启它，
需重开会话/重启 herdr 服务器才会再次运行（启动时会先恢复上次残留的挂起进程）。

## 配置

默认**所有 workspace 冻结开启**，空闲 180 秒触发。按 workspace 调整：

```sh
# 在某个 workspace 的上下文里调用 config 动作（弹交互式配置窗）
herdr plugin action invoke herdr-freeze.config

# 或直接 CLI（无需 herdr 上下文）
herdr-freeze config --workspace w1 --enabled true --idle-secs 120
herdr-freeze config --workspace w1             # 查看当前配置
herdr-freeze config --workspace w1 --clear      # 恢复默认
```

也可在 herdr 里给 `config` 动作绑快捷键（见 Herdr 插件文档「按键绑定」）。

## 手动动作

- `herdr-freeze freeze-now`（动作 `freeze-now`）：立即冻结当前 tab 的所有 pane
  （不等空闲阈值；仍受该 workspace 是否开启冻结限制）。
- `herdr-freeze thaw`（动作 `thaw`）：手动解冻当前 tab（与在蒙层按键等价）。
- `herdr-freeze frozen`：冻结蒙层 UI（由守护进程通过 `plugin pane open` 打开，
  一般不直接调用）。
- `herdr-freeze config-ui`：配置弹窗 UI（由 `config` 动作打开）。

## 调试

守护进程的 stderr 由 herdr 捕获到插件日志：

```sh
herdr plugin log list --plugin herdr-freeze
```

手动单跑监控（仅做非冻结的一轮校验时可短跑后 Ctrl-C；首个 tick 会把所有 pane 的
last_change 置为现在，故 180s 内不会冻结）：

```sh
herdr-freeze monitor   # Ctrl-C 退出
```

## 文件结构

```text
herdr-freeze/
  herdr-plugin.toml     # 清单：startup / actions(config,freeze-now,thaw) / panes(frozen,config)
  Cargo.toml
  src/
    main.rs             # 子命令分发：monitor/frozen/config/config-ui/thaw/freeze-now
    monitor.rs          # 常驻守护进程：轮询 + 空闲判定 + 冻结 + 蒙层 + 解冻 + 恢复
    api.rs              # herdr CLI 封装（HERDR_BIN_PATH）+ JSON 信封解析
    freezer.rs          # 跨平台进程树挂起/恢复（Windows NtSuspend / Unix SIGSTOP）
    overlay.rs          # 冻结蒙层 UI
    state.rs            # 持久化冻结记录 + thaw/freeze-now 信号
```

## 限制 / 注意

- **只支持按键解冻**：herdr 0.9 本地 shell 模式下不把鼠标事件转发给 pane 程序
  （按键会转发），故蒙层用 raw 模式 + 读 stdin，任意按键即解冻；未开鼠标追踪。
- 默认所有 workspace 冻结**开启**；不希望的 workspace 用 `config --enabled false` 关闭。
- 蒙层只在「tab 下全部 pane 冻结」且「该 tab 为当前聚焦 tab」时弹出；非聚焦的全冻结
  tab 仅显示 ❄ 标题，切到它后下一轮（≤10s）弹蒙层。
- Unix/macOS 用进程组 `SIGSTOP`；跨进程组派生的子孙不会被挂起。
- workspace 元数据 token 有 ttl（24h），守护进程每小时续期一次；若守护进程长期不跑，
  配置会回退默认值。
- 启动钩子非受监督；崩溃后需重开会话才会再次运行（启动会先恢复残留挂起进程）。
