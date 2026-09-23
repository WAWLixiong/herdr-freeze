//! 常驻监控守护进程：CPU 采样判空闲 + socket 订阅聚焦事件即时解冻。
//!
//! 空闲判定（适配 work-assistant 的 PTY 时间戳方案——herdr-freeze 不拥有
//! PTY，用进程 CPU 采样替代 PTY 输出信号）：
//! - 周期（15s）采样每 pane 进程组/树总 CPU 时间；delta>0 → 有活动，刷新
//!   last_cpu。覆盖所有运行的进程（含无终端输出的 CPU-bound 进程）。
//! - revision 变（pane list 已带，零额外 spawn）→ title/agent 变 → 视为活动，
//!   刷新 last_active，跳过 CPU 采样（省 process-info spawn）。
//! - 空闲 = cpu_idle ≥ idle_secs AND active_idle ≥ 60s → 候选 → 400ms CPU
//!   guard（>10% 一核则跳过本轮，下轮重试，复刻 work-assistant）→ freeze_pane。
//!
//! 解冻（单 pane，即时）：订阅 pane.focused + tab.focused（socket 长连接，
//! events.subscribe，0 spawn）；聚焦冻结 pane → 即时 resume + 去 ❄ + 设冷却。
//!
//! 与 herdr 的其余交互（workspace list / pane list / process-info / pane
//! rename / report-metadata）仍走 HERDR_BIN_PATH CLI 一次性调用。配置以 herdr
//! workspace 元数据 token 存储（freeze_enabled / freeze_idle_secs）。

use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::api;
use crate::cpu_sample;
use crate::event_stream::{self, FocusEvent};
use crate::freezer::{self, FreezeTarget};
use crate::state::{self, FrozenEntry};
use crate::{freeze_dbg, freeze_log};

const POLL_INTERVAL: Duration = Duration::from_secs(15);
const COOLDOWN: Duration = Duration::from_secs(90);
const ACTIVE_GRACE: Duration = Duration::from_secs(60);
const METADATA_REFRESH: Duration = Duration::from_secs(3600);
const METADATA_TTL_MS: u64 = 86_400_000;
const DEFAULT_IDLE_SECS: u64 = 180;
const MIN_IDLE_SECS: u64 = 30;
const CPU_GUARD_MS: u64 = 400;
const CPU_BUSY_RATIO: f64 = 0.10;
/// active 判定阈值（一核占比）：两次采样间 delta 折算 > 此值才算活跃。
/// 低于此值的低 CPU 后台活动（agent LSP/心跳/文件监视，典型 0.5-1%）判
/// 空闲可冻；真工作（cargo build 等 >10%）判活跃不冻。调小→更易冻 agent。
const ACTIVE_CPU_THRESHOLD: f64 = 0.01;
const MARKER: &str = "❄ ";

pub struct Monitor {
    frozen: Vec<FrozenEntry>,
    /// pane_id -> (上次 CPU 采样值, 上次活动时刻)
    last_cpu: HashMap<String, (cpu_sample::CpuTime, Instant)>,
    /// pane_id -> 上次聚焦时刻（60s grace）
    last_active: HashMap<String, Instant>,
    /// pane_id -> 上次 revision（便宜初筛，省 process-info spawn）
    last_rev: HashMap<String, u64>,
    cooldown: HashMap<String, Instant>,
    /// 上次元数据续期时刻。None = 尚未续期过（首 tick 即续期）。不能用
    /// `Instant::now() - METADATA_REFRESH` 表达「启动后尽快续期」——开机不满
    /// METADATA_REFRESH 时 Instant 减 Duration 下溢 panic（hook 拉起即死，
    /// stderr 被 herdr 吞掉，无任何痕迹）。
    last_md_refresh: Option<Instant>,
    focus_rx: mpsc::Receiver<FocusEvent>,
}

impl Monitor {
    fn new(focus_rx: mpsc::Receiver<FocusEvent>) -> Self {
        Self {
            frozen: Vec::new(),
            last_cpu: HashMap::new(),
            last_active: HashMap::new(),
            last_rev: HashMap::new(),
            cooldown: HashMap::new(),
            last_md_refresh: None, // 首 tick 即续期（见字段注释）
            focus_rx,
        }
    }

    pub fn run() -> i32 {
        let (tx, rx) = mpsc::channel();
        let _reader = event_stream::spawn_reader(tx);
        let mut m = Monitor::new(rx);
        // 启动恢复：上次崩溃残留的挂起进程，按 pid 全部解挂，清空记录，
        // 并还原 ❄ 标签（否则进程已恢复但 pane 标题仍带 ❄）。
        let leftover = state::load_frozen();
        if !leftover.is_empty() {
            freeze_log!("启动恢复：解挂 {} 个残留冻结记录", leftover.len());
            for entry in &leftover {
                freezer::resume(&FreezeTarget {
                    roots: entry.roots.clone(),
                    pgid: entry.pgid,
                });
                if let Err(e) = api::pane_set_label(&entry.pane_id, entry.orig_label.as_deref()) {
                    freeze_log!("启动恢复还原标签 {} 失败: {e}", entry.pane_id);
                }
                freeze_dbg!(
                    "启动恢复 pane={} roots={:?} pgid={:?} → label={:?}",
                    entry.pane_id,
                    entry.roots,
                    entry.pgid,
                    entry.orig_label
                );
            }
            state::save_frozen(&[]);
        }
        // 启动清理：扫所有 pane，剥掉前导 ❄ 污染标记。frozen.json 已空的
        // pane 不在启动恢复范围内，但上次会话 freeze_pane 的 ❄ 叠加 bug
        // （herdr trim 末尾空格 → "❄ "→"❄"→"❄ ❄"）会遗留污染 label。此处
        // 一次性还原纯净 label。启动恢复已还原的 pane clean==raw，不会误动。
        let mut cleaned = 0u32;
        match api::workspace_list() {
            Ok(workspaces) => {
                for ws in &workspaces {
                    if let Ok(panes) = api::pane_list(&ws.workspace_id) {
                        for p in &panes {
                            let raw = p.label.as_deref().unwrap_or("");
                            let clean = strip_freeze_marker(raw);
                            if clean != raw {
                                freeze_dbg!(
                                    "启动清理 pane={} raw={:?} → clean={:?}",
                                    p.pane_id,
                                    raw,
                                    if clean.is_empty() { "(clear)" } else { clean }
                                );
                                let _ = api::pane_set_label(
                                    &p.pane_id,
                                    if clean.is_empty() { None } else { Some(clean) },
                                );
                                cleaned += 1;
                            }
                        }
                    }
                }
            }
            Err(e) => freeze_log!("启动清理 workspace_list 失败: {e}"),
        }
        if cleaned > 0 {
            freeze_log!("启动清理：还原 {} 个污染 pane 的 label", cleaned);
        }
        freeze_dbg!(
            "DEBUG 已启用（--debug 或 HERDR_FREEZE_DEBUG=1），判定链路日志将打印"
        );
        freeze_log!("监控启动，轮询间隔 {:?}", POLL_INTERVAL);
        loop {
            if let Err(e) = m.tick() {
                freeze_log!("tick 出错: {e}");
            }
            // 事件驱动唤醒：sleep 改 recv_timeout，聚焦事件来时立即醒处理
            // （不等下个 tick 开头），延迟从平均 7.5s 降到 ~0。tick 开头仍
            // try_recv 拿光积压；这里拿到的事件直接处理（不重复进 channel）。
            match m.focus_rx.recv_timeout(POLL_INTERVAL) {
                Ok(ev) => {
                    let now = Instant::now();
                    match ev {
                        FocusEvent::Pane(pid) => m.on_focus(&pid, now),
                        FocusEvent::Tab(tid, wid) => m.on_tab_focus(&tid, &wid, now),
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    freeze_log!("focus event channel 断开，退出");
                    return 1;
                }
            }
        }
    }

    fn tick(&mut self) -> Result<(), String> {
        let now = Instant::now();

        // 1) 处理聚焦事件（socket 订阅）：聚焦冻结 pane → 即时解冻；无论如何
        //    刷新 last_active（60s grace，防冻结用户正在看的 pane）。
        while let Ok(ev) = self.focus_rx.try_recv() {
            match ev {
                FocusEvent::Pane(pid) => self.on_focus(&pid, now),
                FocusEvent::Tab(tid, wid) => self.on_tab_focus(&tid, &wid, now),
            }
        }

        // 2) 处理手动信号
        for tab_id in state::drain_freeze_now_requests() {
            self.force_freeze_tab(&tab_id);
        }
        for tab_id in state::drain_thaw_requests() {
            self.thaw_tab(&tab_id);
        }

        let workspaces = api::workspace_list()?;

        // 3) 元数据续期（配置 token ttl=24h，每小时续一次；首 tick 即续期）
        if self
            .last_md_refresh
            .map_or(true, |t| t.elapsed() >= METADATA_REFRESH)
        {
            for ws in &workspaces {
                let tokens = &ws.tokens;
                if tokens.contains_key("freeze_enabled") || tokens.contains_key("freeze_idle_secs")
                {
                    let enabled = tokens
                        .get("freeze_enabled")
                        .map(|s| s.as_str())
                        .unwrap_or("true");
                    let secs = tokens
                        .get("freeze_idle_secs")
                        .map(|s| s.as_str())
                        .unwrap_or("180");
                    let _ = api::workspace_report_metadata(
                        &ws.workspace_id,
                        &[("freeze_enabled", enabled), ("freeze_idle_secs", secs)],
                        METADATA_TTL_MS,
                    );
                    freeze_log!(
                        "元数据续期 ws={} enabled={} idle_secs={}s",
                        ws.workspace_id,
                        enabled,
                        secs
                    );
                }
            }
            self.last_md_refresh = Some(Instant::now());
        }

        for ws in &workspaces {
            let enabled = ws
                .tokens
                .get("freeze_enabled")
                .map(|s| s.as_str() != "false")
                .unwrap_or(true);
            let idle_secs = ws
                .tokens
                .get("freeze_idle_secs")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(DEFAULT_IDLE_SECS)
                .max(MIN_IDLE_SECS);

            let panes = api::pane_list(&ws.workspace_id)?;

            freeze_dbg!(
                "tick ws={} (label={}) enabled={} idle_secs={}s panes={}",
                ws.workspace_id,
                ws.label,
                enabled,
                idle_secs,
                panes.len()
            );

            // 清理已消失 pane 的追踪状态与冻结记录（best-effort 解挂避免孤儿）
            self.reap_closed_panes(&panes, &ws.workspace_id);

            if !enabled {
                let ws_frozen: Vec<String> = self
                    .frozen
                    .iter()
                    .filter(|f| f.workspace_id == ws.workspace_id)
                    .map(|f| f.tab_id.clone())
                    .collect();
                for tab_id in ws_frozen {
                    self.thaw_tab(&tab_id);
                }
                continue;
            }

            // 冻结空闲 pane
            for p in &panes {
                if self.is_frozen(&p.pane_id) {
                    freeze_dbg!("pane={} → skip(frozen)", p.pane_id);
                    continue;
                }
                // 当前聚焦的 pane 永不冻结（状态判定，对齐 README「未被聚焦达
                // 60s」意图）。聚焦 grace 此前是事件驱动，用户持续聚焦在同一
                // pane（不切换）时不发 pane.focused 事件、last_active 不刷新，
                // 叠加 macOS CPU 采样漏采 vim 等前台进程时会误冻聚焦 pane。
                if p.focused {
                    freeze_dbg!("pane={} focused=true → skip(focused)", p.pane_id);
                    continue;
                }
                if self.in_cooldown(&p.tab_id, now) {
                    freeze_dbg!("pane={} → skip(cooldown tab={})", p.pane_id, p.tab_id);
                    continue;
                }

                // revision 便宜初筛（pane list 已带，零额外 spawn）：变 → 活动
                let prev_rev = self.last_rev.get(&p.pane_id).copied();
                if prev_rev != Some(p.revision) {
                    freeze_dbg!(
                        "pane={} rev {}→{} → skip(rev-changed)",
                        p.pane_id,
                        prev_rev.unwrap_or(0),
                        p.revision
                    );
                    self.last_rev.insert(p.pane_id.clone(), p.revision);
                    self.last_active.insert(p.pane_id.clone(), now);
                    continue;
                }

                // revision 不变 → process-info + CPU 采样确认
                let info = match api::pane_process_info(&p.pane_id) {
                    Ok(i) => i,
                    Err(e) => {
                        freeze_log!("process-info {} 失败: {e}", p.pane_id);
                        continue;
                    }
                };
                let roots = if !info.foreground_pids.is_empty() {
                    info.foreground_pids.clone()
                } else if let Some(shell) = info.shell_pid {
                    vec![shell]
                } else {
                    Vec::new()
                };
                let cur = cpu_sample::sample(&roots, info.foreground_process_group_id);
                let prev = self.last_cpu.get(&p.pane_id).copied();
                // active 判定分两路：
                // - agent pane（agent_status 有值，api.rs 已过滤 unknown）：用 agent_status
                //   判定——idle/done = 空闲（即使后台 CPU 活动也判 inactive 可冻），
                //   working/blocked = 活跃不冻。比 CPU 采样更准（agent 自报状态，
                //   接近 work-assistant 的 PTY 输出判定），解决 opencode 后台 LSP/心跳
                //   CPU 活动导致永远 active 不冻的问题。
                // - 非 agent pane：CPU 采样 delta 归一化 > ACTIVE_CPU_THRESHOLD 算活跃。
                let agent_status = p.agent_status.as_deref();
                let is_agent = agent_status.is_some();
                let agent_idle = matches!(agent_status, Some("idle") | Some("done"));
                let active = if is_agent {
                    !agent_idle
                } else {
                    match prev {
                        None => true, // 首次采样建基线
                        Some((prev_cpu, prev_t)) => {
                            let delta = cur.0.saturating_sub(prev_cpu.0);
                            let elapsed = now.duration_since(prev_t);
                            cpu_sample::delta_cores(delta, elapsed) > ACTIVE_CPU_THRESHOLD
                        }
                    }
                };
                if active {
                    freeze_dbg!(
                        "pane={} {} active prev={:?} cur={} → skip(active)",
                        p.pane_id,
                        if is_agent {
                            format!("agent={}", agent_status.unwrap_or(""))
                        } else {
                            "cpu".to_string()
                        },
                        prev.map(|(c, _)| c.0),
                        cur.0
                    );
                    self.last_cpu.insert(p.pane_id.clone(), (cur, now));
                    self.last_active.insert(p.pane_id.clone(), now);
                    continue;
                }
                // agent pane 首次观察到即为 idle/done：建 idle 基线（对齐
                // work-assistant「会话创建即初始化时间戳」语义）。否则 last_cpu
                // 永远为空，下方 cpu_idle_since 每轮 unwrap_or(now) 恒为 0，
                // 从 monitor 启动起就 idle 的 agent pane（如重启 herdr 后恢复的
                // opencode pane）永远 skip(idle) 不冻结。曾 working 过的 pane
                // 在 working tick 已写入基线，不进此分支，行为不变。
                if is_agent && prev.is_none() {
                    self.last_cpu.insert(p.pane_id.clone(), (cur, now));
                    self.last_active.entry(p.pane_id.clone()).or_insert(now);
                    freeze_dbg!(
                        "pane={} agent={} 首次观察，建 idle 基线",
                        p.pane_id,
                        agent_status.unwrap_or("")
                    );
                    continue;
                }
                // CPU 不变 → 检查空闲时长 + 聚焦 grace
                let cpu_idle_since = self
                    .last_cpu
                    .get(&p.pane_id)
                    .map(|(_, t)| *t)
                    .unwrap_or(now);
                let active_since = self
                    .last_active
                    .get(&p.pane_id)
                    .copied()
                    .unwrap_or(cpu_idle_since);
                let cpu_idle_dur = now.duration_since(cpu_idle_since);
                if cpu_idle_dur < Duration::from_secs(idle_secs) {
                    freeze_dbg!(
                        "pane={} cpu_idle={:?}<{}s → skip(idle)",
                        p.pane_id,
                        cpu_idle_dur,
                        idle_secs
                    );
                    continue;
                }
                let grace_dur = now.duration_since(active_since);
                if grace_dur < ACTIVE_GRACE {
                    freeze_dbg!(
                        "pane={} grace={:?}<60s → skip(grace)",
                        p.pane_id,
                        grace_dur
                    );
                    continue;
                }
                // 400ms CPU guard：>10% 一核 → 跳过本轮（下轮 sample 会捕到 CPU 增长）
                let busy = cpu_sample::busy_ratio(
                    &roots,
                    info.foreground_process_group_id,
                    CPU_GUARD_MS,
                );
                if busy > CPU_BUSY_RATIO {
                    freeze_dbg!(
                        "pane={} guard busy={:.3}>{} → skip(guard)",
                        p.pane_id,
                        busy,
                        CPU_BUSY_RATIO
                    );
                    continue;
                }
                freeze_dbg!(
                    "pane={} cpu_idle={:?} grace={:?} guard={:.3} → FREEZE",
                    p.pane_id,
                    cpu_idle_dur,
                    grace_dur,
                    busy
                );
                self.freeze_pane(p, &info);
            }
        }

        state::save_frozen(&self.frozen);
        Ok(())
    }

    fn is_frozen(&self, pane_id: &str) -> bool {
        self.frozen.iter().any(|f| f.pane_id == pane_id)
    }

    fn in_cooldown(&self, tab_id: &str, now: Instant) -> bool {
        self.cooldown.get(tab_id).is_some_and(|t| *t > now)
    }

    /// 聚焦事件：若该 pane 冻结 → 即时解冻；无论如何刷新 last_active。
    fn on_focus(&mut self, pane_id: &str, now: Instant) {
        let frozen = self.is_frozen(pane_id);
        freeze_dbg!("on_focus pane={} frozen={}", pane_id, frozen);
        if frozen {
            self.thaw_pane(pane_id);
        }
        self.last_active.insert(pane_id.to_string(), now);
    }

    /// tab.focused 兜底（payload 无 pane_id）：查该 tab 当前聚焦 pane → on_focus。
    /// pane.focused 通常已覆盖切 tab 场景，此处防边界未触发。
    fn on_tab_focus(&mut self, tab_id: &str, workspace_id: &str, now: Instant) {
        freeze_dbg!("on_tab_focus tab={} ws={}", tab_id, workspace_id);
        let panes = match api::pane_list(workspace_id) {
            Ok(p) => p,
            Err(e) => {
                freeze_log!("on_tab_focus pane_list ws={} 失败: {e}", workspace_id);
                return;
            }
        };
        if let Some(focused) = panes.iter().find(|p| p.tab_id == tab_id && p.focused) {
            self.on_focus(&focused.pane_id, now);
        } else {
            freeze_dbg!("on_tab_focus tab={} 未找到聚焦 pane", tab_id);
        }
    }

    fn freeze_pane(&mut self, p: &api::Pane, info: &api::ProcessInfo) {
        let roots = if !info.foreground_pids.is_empty() {
            info.foreground_pids.clone()
        } else if let Some(shell) = info.shell_pid {
            vec![shell]
        } else {
            Vec::new()
        };
        let target = FreezeTarget {
            roots: roots.clone(),
            pgid: info.foreground_process_group_id,
        };
        let suspended = freezer::freeze(&target);
        // 剥掉 label 开头所有前导 ❄（及夹的空格）得到纯净 label，再加单层 ❄。
        // herdr pane rename 会 trim 末尾空格，"❄ " 被存成 "❄"（无空格），下次
        // starts_with("❄ ") 失败 → 叠加成 "❄ ❄"。剥前导 ❄ 后再加既防叠加，
        // 又顺手清理上次会话遗留的污染 label。orig_label 存纯净值（空则
        // None），thaw/启动恢复据此还原。
        let clean = strip_freeze_marker(p.label.as_deref().unwrap_or(""));
        let orig_label = if clean.is_empty() {
            None
        } else {
            Some(clean.to_string())
        };
        let new_label = format!("{MARKER}{clean}");
        freeze_dbg!(
            "freeze_pane {} label raw={:?} → clean={:?} → orig={:?} → new={:?}",
            p.pane_id,
            p.label,
            clean,
            orig_label,
            new_label
        );
        if p.label.as_deref() != Some(new_label.as_str()) {
            if let Err(e) = api::pane_set_label(&p.pane_id, Some(&new_label)) {
                freeze_log!("标记 {} 失败: {e}", p.pane_id);
            }
        }
        freeze_log!(
            "冻结 pane={} tab={} roots={:?} pgid={:?} suspended={}",
            p.pane_id,
            p.tab_id,
            roots,
            info.foreground_process_group_id,
            suspended.len()
        );
        self.frozen.push(FrozenEntry {
            workspace_id: p.workspace_id.clone(),
            tab_id: p.tab_id.clone(),
            pane_id: p.pane_id.clone(),
            roots,
            pgid: info.foreground_process_group_id,
            orig_label,
        });
        state::save_frozen(&self.frozen);
    }

    /// 解冻单个 pane（聚焦触发）：恢复进程 + 还原标签 + 重置计时 + 设该 tab 冷却。
    fn thaw_pane(&mut self, pane_id: &str) {
        let entry = match self.frozen.iter().find(|f| f.pane_id == pane_id) {
            Some(e) => e.clone(),
            None => return,
        };
        freezer::resume(&FreezeTarget {
            roots: entry.roots.clone(),
            pgid: entry.pgid,
        });
        freeze_dbg!("thaw_pane {} label → {:?}", entry.pane_id, entry.orig_label);
        if let Err(e) = api::pane_set_label(&entry.pane_id, entry.orig_label.as_deref()) {
            freeze_log!("恢复标签 {} 失败: {e}", entry.pane_id);
        }
        // 重置计时，避免立刻再冻结
        self.last_cpu
            .insert(entry.pane_id.clone(), (cpu_sample::CpuTime(0), Instant::now()));
        self.last_active.insert(entry.pane_id.clone(), Instant::now());
        self.cooldown
            .insert(entry.tab_id.clone(), Instant::now() + COOLDOWN);
        self.frozen.retain(|f| f.pane_id != entry.pane_id);
        state::save_frozen(&self.frozen);
        freeze_log!("解冻 pane={} tab={}", entry.pane_id, entry.tab_id);
    }

    /// 解冻整 tab：恢复进程、还原标签、设冷却（thaw CLI / 信号用）。
    fn thaw_tab(&mut self, tab_id: &str) {
        let entries: Vec<FrozenEntry> = self
            .frozen
            .iter()
            .filter(|f| f.tab_id == tab_id)
            .cloned()
            .collect();
        for e in &entries {
            freezer::resume(&FreezeTarget {
                roots: e.roots.clone(),
                pgid: e.pgid,
            });
            if let Err(err) = api::pane_set_label(&e.pane_id, e.orig_label.as_deref()) {
                freeze_log!("恢复标签 {} 失败: {err}", e.pane_id);
            }
            self.last_cpu
                .insert(e.pane_id.clone(), (cpu_sample::CpuTime(0), Instant::now()));
            self.last_active.insert(e.pane_id.clone(), Instant::now());
        }
        self.frozen.retain(|f| f.tab_id != tab_id);
        self.cooldown
            .insert(tab_id.to_string(), Instant::now() + COOLDOWN);
        state::save_frozen(&self.frozen);
        freeze_log!("解冻 tab={} ({} pane)", tab_id, entries.len());
    }

    fn force_freeze_tab(&mut self, tab_id: &str) {
        let workspaces = match api::workspace_list() {
            Ok(list) => list,
            Err(e) => {
                freeze_log!("freeze-now workspace_list 失败: {e}");
                return;
            }
        };
        for ws in &workspaces {
            let enabled = ws
                .tokens
                .get("freeze_enabled")
                .map(|s| s.as_str() != "false")
                .unwrap_or(true);
            if !enabled {
                continue;
            }
            let panes = match api::pane_list(&ws.workspace_id) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if !panes.iter().any(|p| p.tab_id == tab_id) {
                continue;
            }
            for p in &panes {
                if p.tab_id != tab_id || self.is_frozen(&p.pane_id) {
                    continue;
                }
                let info = match api::pane_process_info(&p.pane_id) {
                    Ok(i) => i,
                    Err(e) => {
                        freeze_log!("freeze-now process-info {} 失败: {e}", p.pane_id);
                        continue;
                    }
                };
                self.freeze_pane(p, &info);
            }
            return;
        }
        freeze_log!(
            "freeze-now: 找不到 tab={} 所属 workspace（或该 workspace 关闭了冻结）",
            tab_id
        );
    }

    fn reap_closed_panes(&mut self, panes: &[api::Pane], workspace_id: &str) {
        let live: HashSet<&str> = panes.iter().map(|p| p.pane_id.as_str()).collect();
        // 只清当前 workspace 的死 pane：panes 是当前 ws 的 pane list，不能拿它
        // 判断其他 ws 的 frozen entry（否则 w5 的 tick 会把 w6 的 frozen 当死清掉，
        // 导致 on_focus frozen=false 不解冻）。
        let dead: Vec<FrozenEntry> = self
            .frozen
            .iter()
            .filter(|f| f.workspace_id == workspace_id && !live.contains(f.pane_id.as_str()))
            .cloned()
            .collect();
        for e in &dead {
            freezer::resume(&FreezeTarget {
                roots: e.roots.clone(),
                pgid: e.pgid,
            });
            self.last_cpu.remove(&e.pane_id);
            self.last_active.remove(&e.pane_id);
            self.last_rev.remove(&e.pane_id);
            freeze_dbg!("reap pane={} roots={:?} pgid={:?}", e.pane_id, e.roots, e.pgid);
        }
        if !dead.is_empty() {
            freeze_log!("reap: 解挂 {} 个已关闭 pane (ws={})", dead.len(), workspace_id);
            // 只移除当前 ws 的死 pane，保留其他 ws 的所有 frozen entry。
            self.frozen
                .retain(|f| f.workspace_id != workspace_id || live.contains(f.pane_id.as_str()));
            state::save_frozen(&self.frozen);
        }
    }
}

/// 剥掉 label 开头所有前导 ❄（及夹的空格），返回纯净 label。
/// herdr pane rename 会 trim 末尾空格，"❄ " 被存成 "❄"（无空格），导致
/// freeze_pane 的 starts_with("❄ ") 防叠加检查失效、叠成 "❄ ❄"。此函数
/// 容错任意前导 ❄ 与空格混排，剥到首个非 ❄ 字符为止。
fn strip_freeze_marker(label: &str) -> &str {
    let mut s = label;
    loop {
        let trimmed = s.trim_start();
        match trimmed.strip_prefix("❄") {
            Some(rest) => s = rest,
            None => return trimmed,
        }
    }
}
