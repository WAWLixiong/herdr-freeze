# herdr-freeze

自动冻结 Herdr 空闲面板进程树以释放 CPU（及 Windows 上的内存）。**仅 Windows 支持冻结**；
Unix/macOS 暂不可用（详见「限制 - macOS 冻结为何不可用」）。冻结 = 挂起进程树 +
Windows 额外裁剪工作集释放内存 + 给 pane label 加 `❄` 前缀；解冻 = 用户聚焦到冻结 pane 时即时恢复该单 pane。

这是一个 [Herdr](https://herdr.dev) 插件（目录 + `herdr-plugin.toml` 清单 +
一个 Rust 二进制 `herdr-freeze`）。冻结/解冻逻辑参考 `work-assistant` 的
freezer（挂起进程树 + 裁剪工作集），空闲检测适配 work-assistant 的 PTY
时间戳方案——herdr-freeze 不拥有 PTY（herdr 拥有），改用**进程组 CPU 采样**
替代 PTY 输出信号，覆盖所有运行的进程（含无终端输出的 CPU-bound 进程）。

## 行为

- **空闲判定**：守护进程周期（15s）采样每 pane 进程组/树的总 CPU 时间
  （Windows `GetProcessTimes` 树求和；Linux `/proc/[pid]/stat` 遍历进程组
  求和 utime+stime；macOS `proc_pidinfo(PROC_PIDTASKINFO)` 求和
  `total_user+total_system`）。CPU 时间 delta>0 → 进程在跑 → 刷新活动时刻。
  revision 变化（`pane list` 已带，零额外 spawn）→ title/agent 变 → 也视为
  活动，跳过 CPU 采样（省 `pane process-info` spawn）。
- **冻结**（仅 Windows）：CPU 不变达阈值秒（默认 180s）且未被聚焦达 60s（grace）→ 候选
  → 400ms CPU guard（>10% 一核则跳过本轮，复刻 work-assistant）→ 挂起整棵
  进程树并裁剪工作集释放内存。冻结的 pane 标题前加 `❄` 标记。Unix/macOS 不冻结（见「限制」）。
- **解冻**：守护进程常驻订阅 herdr 的 `pane.focused` + `tab.focused` 事件流
  （`events.subscribe`，socket 长连接，0 进程 spawn）。用户聚焦到冻结 pane
  → 即时 resume 该单 pane + 还原 label + 设该 tab 冷却（防立刻再冻结）。
  `tab.focused` 作兜底（payload 无 pane_id，查该 tab 当前聚焦 pane）。
- **配置**：按 Herdr workspace 级别，用 workspace 元数据 token 存储：
  - `freeze_enabled`（`true`/`false`，默认 `true`）
  - `freeze_idle_secs`（秒，默认 `180`，下限 `30`）
  - 通过 `config` 动作（弹窗）或 `herdr-freeze config` CLI 设置。

## 跨平台冻结机制

- **Windows**：复刻 work-assistant freezer —— `NtSuspendProcess`/`NtResumeProcess`
  挂起/恢复整棵进程树（`CreateToolhelp32Snapshot` 遍历父子关系 + BFS，含竞态修复
  轮次），`K32EmptyWorkingSet` 裁剪工作集。CPU 采样用 `GetProcessTimes` 树求和
  （user+kernel 100ns ticks）。
- **Unix / macOS**：冻结**不可用**（详见「限制 - macOS 冻结为何不可用」）。设计上曾用
  对前台进程组发 `SIGSTOP`/`SIGCONT`，但实测会被 shell job control 抢终端、`SIGCONT` 无法
  恢复。CPU 采样逻辑保留（以备 herdr 提供 `pane.suspend` 后启用）：Linux 遍历
  `/proc/[pid]/stat` 按 pgrp 匹配求和 utime+stime（jiffies，假定 CLK_TCK=100）；
  macOS 对 `foreground_processes` 逐 pid `proc_pidinfo(PROC_PIDTASKINFO)` 取
  `total_user+total_system`（ns）。

守护进程只持久化「根 pid + 进程组 id」；恢复时按 pid 重新遍历进程树再开句柄解挂
—— 这样守护进程崩溃重启后也能按 pid 恢复上次残留的挂起进程。

## 安装（从预编译二进制，无需 Rust 工具链）

适合没有 Rust 编辑环境的用户。**二进制靠 PATH 解析、`herdr-plugin.toml`
靠 link 目录——两者缺一不可。**

1. 从 [Releases](../../releases) 页下载对应平台的二进制：
   - Linux：`herdr-freeze-x86_64-unknown-linux-gnu`
   - macOS（Intel）：`herdr-freeze-x86_64-apple-darwin`
   - macOS（Apple Silicon）：`herdr-freeze-aarch64-apple-darwin`
   - Windows：`herdr-freeze-x86_64-pc-windows-msvc.exe`
2. 把二进制重命名为 `herdr-freeze`（Windows 保留 `.exe`），放到 **PATH** 上的目录：
   ```sh
   # Linux/macOS（任选一个在 PATH 上的目录）
   mkdir -p ~/.local/bin
   mv herdr-freeze-* ~/.local/bin/herdr-freeze
   chmod +x ~/.local/bin/herdr-freeze
   # 确认 ~/.local/bin 在 PATH；若不在，加到 ~/.zshrc / ~/.bashrc：
   #   export PATH="$HOME/.local/bin:$PATH"

   # Windows：重命名为 herdr-freeze.exe，放到 PATH 目录
   # （如 %USERPROFILE%\.cargo\bin 或自行添加的目录）
   ```
3. 单独下载 `herdr-plugin.toml`（Releases 同页提供），放到插件目录：
   ```sh
   mkdir -p ~/.config/herdr/plugins/herdr-freeze
   # 把 herdr-plugin.toml 放到该目录
   ```
4. 链接并确认：
   ```sh
   herdr plugin link ~/.config/herdr/plugins/herdr-freeze
   herdr plugin list                          # 确认 herdr-freeze 已注册、enabled
   herdr plugin action list --plugin herdr-freeze
   ```

> 也可克隆本仓库用其 `herdr-plugin.toml`，二进制仍走 PATH。

## 安装（本地开发）

需要 Rust 工具链。herdr 不允许同 id 的 action/pane 按平台重复声明
（`duplicate_plugin_action_id`），故清单用单一命令 + 纯名 `herdr-freeze`
（herdr 在 Windows 上按 PATHEXT 解析 `.exe`）。**需要先把 release 二进制
放到 PATH**：

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
monitor`）。守护进程启动时：连 `HERDR_SOCKET_PATH`（startup hook 注入）订阅
`pane.focused`/`tab.focused` 事件流，并先恢复上次残留的挂起进程。

进程生命周期与 herdr 绑定：
- **herdr 退出 → monitor 自动退出**：monitor 检测 socket 文件消失
  （herdr 退出时 `cleanup_sockets` 删除 socket 文件）→ 立即自杀，不留孤儿。
  累计重连失败超 60s 也兜底自杀（防 herdr 异常崩溃未 cleanup）。
- **herdr 重启 → monitor 重启**：herdr 会话恢复时 startup hook 重新拉起
  monitor（旧 monitor 已随 herdr 退出自杀，无双开）。
- **多个 herdr 客户端 → 1 个 monitor**：startup hook 只在服务器（会话）启动时
  跑一次，不在客户端 attach 时重跑。同一会话的多客户端共享 1 个 monitor；
  不同会话（`HERDR_SESSION`）各自 1 个 monitor，靠 `HERDR_SOCKET_PATH` 隔离。
- 启动钩子**非受监督**：monitor 自身崩溃（非 herdr 退出）时 herdr 不会重启它，
  需重开会话才会再次运行（启动会先恢复残留挂起进程）。

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
- `herdr-freeze thaw`（动作 `thaw`）：手动解冻当前 tab（与聚焦解冻等价，但整 tab；
  直接读 `frozen.json` resume，不依赖守护进程在跑）。
- `herdr-freeze config-ui`：配置弹窗 UI（由 `config` 动作打开）。

## 调试

守护进程的 stderr 由 herdr 捕获到插件日志：

```sh
herdr plugin log list --plugin herdr-freeze
```

手动单跑监控（Ctrl-C 退出；首个 tick 初始化各 pane 的 CPU 采样基线，故
idle_secs 内不会冻结）：

```sh
herdr-freeze monitor   # Ctrl-C 退出
```

## 文件结构

```text
herdr-freeze/
  herdr-plugin.toml     # 清单：startup / actions(config,freeze-now,thaw) / pane(config)
  Cargo.toml
  src/
    main.rs             # 子命令分发：monitor/config/config-ui/thaw/freeze-now
    monitor.rs          # 常驻守护进程：CPU 采样空闲判定 + socket 聚焦解冻 + 冻结/恢复
    event_stream.rs     # herdr socket 长连接订阅 pane.focused/tab.focused（events.subscribe）
    cpu_sample.rs       # 跨平台进程组/树 CPU 采样（GetProcessTimes / /proc / proc_pidinfo）
    api.rs              # herdr CLI 封装（HERDR_BIN_PATH）+ JSON 信封解析
    freezer.rs          # 跨平台进程树挂起/恢复（Windows NtSuspend / Unix SIGSTOP）
    state.rs            # 持久化冻结记录 + thaw/freeze-now 信号
```

## 限制 / 注意

- **解冻靠聚焦事件**：守护进程常驻订阅 `pane.focused`/`tab.focused`，聚焦冻结 pane
  即解冻该单 pane。守护进程不在时（崩溃）聚焦无法解冻——用 `thaw` CLI 兜底（直接读
  `frozen.json` resume，不依赖守护进程），或下次会话启动钩子重启守护进程时恢复残留。
- **CPU 采样覆盖所有运行进程**：无终端输出的 CPU-bound 进程（如 `cargo build`）也能
  被检测为活跃（CPU delta>0），不会被误冻结。但纯 I/O 等待（CPU 低且无输出）的进程
  仍可能被判空闲——这是 work-assistant 方案共有的局限。
- 默认所有 workspace 冻结**开启**（仅 Windows 生效；macOS 冻结不可用，见下条）；不希望的 workspace 用 `config --enabled false` 关闭。
- **macOS/Unix 冻结为何不可用**：herdr 拥有 PTY 并持续读取做 agent 检测/scrollback，
  没有原生 `pane.suspend` 原语、也没有「冻结时让 PTY reader 静默」的钩子。对前台进程组
  发 `SIGSTOP` 会被 shell job control 截胡——实测 `zsh` 在子进程被停后打印
  `suspended (signal)` 并抢占终端前台退回 prompt；随后 `SIGCONT` 无法恢复，因为 agent
  不再是终端前台进程组、一读终端就吃 `SIGTTIN` 再次停住。Mach `task_suspend`（不发信号、
  绕开 job control）受 SIP/taskgated 限制，分布式未签名插件拿不到 task port（pane 进程
  非 same-team），需关 SIP 或跑 root，不可接受。**结论：macOS 冻结需 herdr 核心提供
  `pane.suspend` 原语（冻结时静默自身 PTY reader + 内部管 job control）才能实现**，
  已记为待提 feature request。启用前的实测备忘：即便冻结，macOS 也不主动释放内存——
  无压力时 65s 零回收，仅在系统有内存压力时才被内核压缩/换出（实测 3GB 压力下 512MB
  几乎全量回收）；`purge` 可强制施压但需 sudo，未作为插件功能。
- macOS CPU 采样只采 herdr 给的 `foreground_processes`（agent 实体），不遍历整个进程组
  （需 `PROC_PIDTBSDINFO` 查 `pbi_pgid`，struct layout 复杂易错；foreground_pids 已是
  活动主体）。
- workspace 元数据 token 有 ttl（24h），守护进程每小时续期一次；若守护进程长期不跑，
  配置会回退默认值。
- 启动钩子非受监督；崩溃后需重开会话才会再次运行（启动会先恢复残留挂起进程）。
