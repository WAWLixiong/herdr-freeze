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

const POLL_INTERVAL: Duration = Duration::from_secs(15);
const COOLDOWN: Duration = Duration::from_secs(90);
const ACTIVE_GRACE: Duration = Duration::from_secs(60);
const METADATA_REFRESH: Duration = Duration::from_secs(3600);
const METADATA_TTL_MS: u64 = 86_400_000;
const DEFAULT_IDLE_SECS: u64 = 180;
const MIN_IDLE_SECS: u64 = 30;
const CPU_GUARD_MS: u64 = 400;
const CPU_BUSY_RATIO: f64 = 0.10;
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
    last_md_refresh: Instant,
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
            last_md_refresh: Instant::now() - METADATA_REFRESH, // 启动后尽快续期一次
            focus_rx,
        }
    }

    pub fn run() -> i32 {
        let (tx, rx) = mpsc::channel();
        let _reader = event_stream::spawn_reader(tx);
        let mut m = Monitor::new(rx);
        // 启动恢复：上次崩溃残留的挂起进程，按 pid 全部解挂，清空记录。
        let leftover = state::load_frozen();
        if !leftover.is_empty() {
            eprintln!(
                "[herdr-freeze] 启动恢复：解挂 {} 个残留冻结记录",
                leftover.len()
            );
            for entry in &leftover {
                freezer::resume(&FreezeTarget {
                    roots: entry.roots.clone(),
                    pgid: entry.pgid,
                });
            }
            state::save_frozen(&[]);
        }
        eprintln!("[herdr-freeze] 监控启动，轮询间隔 {:?}", POLL_INTERVAL);
        loop {
            if let Err(e) = m.tick() {
                eprintln!("[herdr-freeze] tick 出错: {e}");
            }
            std::thread::sleep(POLL_INTERVAL);
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

        // 3) 元数据续期（配置 token ttl=24h，每小时续一次）
        if self.last_md_refresh.elapsed() >= METADATA_REFRESH {
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
                }
            }
            self.last_md_refresh = Instant::now();
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

            // 清理已消失 pane 的追踪状态与冻结记录（best-effort 解挂避免孤儿）
            self.reap_closed_panes(&panes);

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
                    continue;
                }
                if self.in_cooldown(&p.tab_id, now) {
                    continue;
                }

                // revision 便宜初筛（pane list 已带，零额外 spawn）：变 → 活动
                if self.last_rev.get(&p.pane_id).copied() != Some(p.revision) {
                    self.last_rev.insert(p.pane_id.clone(), p.revision);
                    self.last_active.insert(p.pane_id.clone(), now);
                    continue;
                }

                // revision 不变 → process-info + CPU 采样确认
                let info = match api::pane_process_info(&p.pane_id) {
                    Ok(i) => i,
                    Err(e) => {
                        eprintln!("[herdr-freeze] process-info {} 失败: {e}", p.pane_id);
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
                let active = prev.is_none() || cur != prev.unwrap().0;
                if active {
                    self.last_cpu.insert(p.pane_id.clone(), (cur, now));
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
                if now.duration_since(cpu_idle_since) < Duration::from_secs(idle_secs) {
                    continue;
                }
                if now.duration_since(active_since) < ACTIVE_GRACE {
                    continue;
                }
                // 400ms CPU guard：>10% 一核 → 跳过本轮（下轮 sample 会捕到 CPU 增长）
                if cpu_sample::busy_ratio(
                    &roots,
                    info.foreground_process_group_id,
                    CPU_GUARD_MS,
                ) > CPU_BUSY_RATIO
                {
                    continue;
                }
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
        if self.is_frozen(pane_id) {
            self.thaw_pane(pane_id);
        }
        self.last_active.insert(pane_id.to_string(), now);
    }

    /// tab.focused 兜底（payload 无 pane_id）：查该 tab 当前聚焦 pane → on_focus。
    /// pane.focused 通常已覆盖切 tab 场景，此处防边界未触发。
    fn on_tab_focus(&mut self, tab_id: &str, workspace_id: &str, now: Instant) {
        let panes = match api::pane_list(workspace_id) {
            Ok(p) => p,
            Err(_) => return,
        };
        if let Some(focused) = panes.iter().find(|p| p.tab_id == tab_id && p.focused) {
            self.on_focus(&focused.pane_id, now);
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
        let orig_label = p.label.clone();
        // 标记 ❄ 前缀（保留原标题以便恢复）
        let new_label = match &orig_label {
            Some(l) if l.starts_with(MARKER) => None,
            Some(l) => Some(format!("{MARKER}{l}")),
            None => Some(format!("{MARKER}{}", p.title.as_deref().unwrap_or(""))),
        };
        if let Some(label) = new_label {
            if let Err(e) = api::pane_set_label(&p.pane_id, Some(&label)) {
                eprintln!("[herdr-freeze] 标记 {} 失败: {e}", p.pane_id);
            }
        }
        eprintln!(
            "[herdr-freeze] 冻结 pane={} tab={} roots={:?} pgid={:?} suspended={}",
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
        if let Err(e) = api::pane_set_label(&entry.pane_id, entry.orig_label.as_deref()) {
            eprintln!("[herdr-freeze] 恢复标签 {} 失败: {e}", entry.pane_id);
        }
        // 重置计时，避免立刻再冻结
        self.last_cpu
            .insert(entry.pane_id.clone(), (cpu_sample::CpuTime(0), Instant::now()));
        self.last_active.insert(entry.pane_id.clone(), Instant::now());
        self.cooldown
            .insert(entry.tab_id.clone(), Instant::now() + COOLDOWN);
        self.frozen.retain(|f| f.pane_id != entry.pane_id);
        state::save_frozen(&self.frozen);
        eprintln!(
            "[herdr-freeze] 解冻 pane={} tab={}",
            entry.pane_id, entry.tab_id
        );
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
                eprintln!("[herdr-freeze] 恢复标签 {} 失败: {err}", e.pane_id);
            }
            self.last_cpu
                .insert(e.pane_id.clone(), (cpu_sample::CpuTime(0), Instant::now()));
            self.last_active.insert(e.pane_id.clone(), Instant::now());
        }
        self.frozen.retain(|f| f.tab_id != tab_id);
        self.cooldown
            .insert(tab_id.to_string(), Instant::now() + COOLDOWN);
        state::save_frozen(&self.frozen);
        eprintln!("[herdr-freeze] 解冻 tab={} ({} pane)", tab_id, entries.len());
    }

    fn force_freeze_tab(&mut self, tab_id: &str) {
        let workspaces = match api::workspace_list() {
            Ok(list) => list,
            Err(e) => {
                eprintln!("[herdr-freeze] freeze-now workspace_list 失败: {e}");
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
                        eprintln!("[herdr-freeze] freeze-now process-info {} 失败: {e}", p.pane_id);
                        continue;
                    }
                };
                self.freeze_pane(p, &info);
            }
            return;
        }
        eprintln!(
            "[herdr-freeze] freeze-now: 找不到 tab={} 所属 workspace（或该 workspace 关闭了冻结）",
            tab_id
        );
    }

    fn reap_closed_panes(&mut self, panes: &[api::Pane]) {
        let live: HashSet<&str> = panes.iter().map(|p| p.pane_id.as_str()).collect();
        let dead: Vec<FrozenEntry> = self
            .frozen
            .iter()
            .filter(|f| !live.contains(f.pane_id.as_str()))
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
        }
        if !dead.is_empty() {
            self.frozen.retain(|f| live.contains(f.pane_id.as_str()));
            state::save_frozen(&self.frozen);
        }
    }
}
