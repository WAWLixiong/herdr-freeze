//! 常驻监控守护进程：周期轮询 pane revision 判定空闲，挂起空闲 pane 的进程树，
//! 整个 tab 全部冻结时弹出蒙层，蒙层关闭后解冻整个 tab。
//!
//! 与 herdr 的交互全部走 HERDR_BIN_PATH CLI（一次性请求），不持有常驻 socket。
//! 空闲判定：pane.read 给出的 content_revision 在阈值秒内未变化即视为空闲
//! （用户选定方案；pane.output_changed 属高频事件，不在插件清单事件钩子内）。
//! 配置：herdr workspace 元数据 token（freeze_enabled / freeze_idle_secs）。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::api;
use crate::freezer::{self, FreezeTarget};
use crate::state::{self, FrozenEntry};

const POLL_INTERVAL: Duration = Duration::from_secs(10);
const COOLDOWN: Duration = Duration::from_secs(90);
const METADATA_REFRESH: Duration = Duration::from_secs(3600);
const METADATA_TTL_MS: u64 = 86_400_000; // 24h（schema 上限）
const DEFAULT_IDLE_SECS: u64 = 180;
const MIN_IDLE_SECS: u64 = 30;
const MARKER: &str = "❄ ";

#[derive(Clone)]
#[allow(dead_code)]
struct Overlay {
    workspace_id: String,
    tab_id: String,
    pane_id: String,
}

pub struct Monitor {
    frozen: Vec<FrozenEntry>,
    overlays: Vec<Overlay>,
    /// pane_id -> (上次 revision, 上次变化时刻)
    last_change: HashMap<String, (u64, Instant)>,
    cooldown: HashMap<String, Instant>,
    last_md_refresh: Instant,
}

impl Default for Monitor {
    fn default() -> Self {
        Self {
            frozen: Vec::new(),
            overlays: Vec::new(),
            last_change: HashMap::new(),
            cooldown: HashMap::new(),
            last_md_refresh: Instant::now() - METADATA_REFRESH, // 启动后尽快续期一次
        }
    }
}

impl Monitor {
    pub fn run() -> i32 {
        let mut m = Monitor::default();
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
        // 1) 处理手动信号
        for tab_id in state::drain_freeze_now_requests() {
            // 立即冻结该 tab 下所有未冻结 pane（受 enable 限制，不受 idle 限制）
            self.force_freeze_tab(&tab_id);
        }
        for tab_id in state::drain_thaw_requests() {
            self.thaw_tab(&tab_id);
        }

        let workspaces = api::workspace_list()?;

        // 2) 元数据续期（配置 token ttl=24h，每小时续一次）
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

        let now = Instant::now();

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

            // 清理已消失 pane 的追踪状态与冻结记录（进程随 pane 关闭已被 herdr 回收，
            // 但若曾被挂起，best-effort 解挂避免孤儿挂起进程）
            self.reap_closed_panes(&panes);

            if !enabled {
                // 该 workspace 关闭冻结：解冻其所有冻结记录
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

            // 更新 last_change：revision 变化即视为有活动
            for p in &panes {
                let prev = self.last_change.get(&p.pane_id);
                match prev {
                    Some((rev, _)) if *rev == p.revision => {
                        // 无变化，保留原时刻
                    }
                    _ => {
                        // 首次见到 或 revision 变化 → 重置为现在
                        self.last_change
                            .insert(p.pane_id.clone(), (p.revision, now));
                    }
                }
            }

            // 冻结空闲 pane
            for p in &panes {
                if self.is_frozen(&p.pane_id) {
                    continue;
                }
                // 排除我们自己打开的蒙层 pane（它本身不应被冻结/计数）
                if self.overlays.iter().any(|o| o.pane_id == p.pane_id) {
                    continue;
                }
                if self.in_cooldown(&p.tab_id, now) {
                    continue;
                }
                let Some((_, changed_at)) = self.last_change.get(&p.pane_id) else {
                    continue;
                };
                if now.duration_since(*changed_at) < Duration::from_secs(idle_secs) {
                    continue;
                }
                self.freeze_pane(p);
            }

            // 全冻结 tab → 弹蒙层；非全冻结但蒙层已开 → 解冻
            self.manage_overlays(ws, &panes);
        }

        // 3) 检测蒙层被关闭（用户点击/按键/手动关）→ 解冻对应 tab
        let closed: Vec<String> = self
            .overlays
            .iter()
            .filter(|o| !api::pane_exists(&o.pane_id))
            .map(|o| o.tab_id.clone())
            .collect();
        for tab_id in closed {
            self.thaw_tab(&tab_id);
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

    fn freeze_pane(&mut self, p: &api::Pane) {
        let info = match api::pane_process_info(&p.pane_id) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("[herdr-freeze] process-info {} 失败: {e}", p.pane_id);
                return;
            }
        };
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
            Some(l) if l.starts_with(MARKER) => None, // 已标记则不动
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

    /// 解冻整 tab：恢复进程、还原标签、关蒙层、设冷却。
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
            // 恢复原始 label（去掉 ❄ 前缀）
            if let Err(err) = api::pane_set_label(&e.pane_id, e.orig_label.as_deref()) {
                eprintln!("[herdr-freeze] 恢复标签 {} 失败: {err}", e.pane_id);
            }
            // 重置该 pane 的空闲计时，避免立刻再冻结
            self.last_change
                .insert(e.pane_id.clone(), (0, Instant::now()));
        }
        // 关闭该 tab 的蒙层
        let to_close: Vec<String> = self
            .overlays
            .iter()
            .filter(|o| o.tab_id == tab_id)
            .map(|o| o.pane_id.clone())
            .collect();
        for pane_id in &to_close {
            let _ = api::plugin_pane_close(pane_id);
        }
        self.overlays.retain(|o| o.tab_id != tab_id);
        self.frozen.retain(|f| f.tab_id != tab_id);
        self.cooldown
            .insert(tab_id.to_string(), Instant::now() + COOLDOWN);
        state::save_frozen(&self.frozen);
        eprintln!(
            "[herdr-freeze] 解冻 tab={} ({} pane)",
            tab_id,
            entries.len()
        );
    }

    fn force_freeze_tab(&mut self, tab_id: &str) {
        // 遍历 workspaces 找到包含该 tab 的 workspace（受 enable 限制）。freeze-now
        // 不受 idle 阈值限制，但尊重 workspace 是否开启冻结。
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
                if self.overlays.iter().any(|o| o.pane_id == p.pane_id) {
                    continue;
                }
                self.freeze_pane(p);
            }
            return;
        }
        eprintln!(
            "[herdr-freeze] freeze-now: 找不到 tab={} 所属 workspace（或该 workspace 关闭了冻结）",
            tab_id
        );
    }

    fn manage_overlays(&mut self, ws: &api::Workspace, panes: &[api::Pane]) {
        use std::collections::HashSet;
        let overlay_panes: HashSet<&str> =
            self.overlays.iter().map(|o| o.pane_id.as_str()).collect();

        // 按 tab 分组（排除蒙层 pane）
        let mut tabs: HashMap<String, Vec<&api::Pane>> = HashMap::new();
        for p in panes {
            if overlay_panes.contains(p.pane_id.as_str()) {
                continue;
            }
            tabs.entry(p.tab_id.clone()).or_default().push(p);
        }

        for (tab_id, tab_panes) in &tabs {
            let total = tab_panes.len();
            let frozen_count = tab_panes
                .iter()
                .filter(|p| self.is_frozen(&p.pane_id))
                .count();
            let all_frozen = total > 0 && frozen_count == total;
            let has_overlay = self.overlays.iter().any(|o| o.tab_id == *tab_id);

            if all_frozen && !has_overlay {
                // 仅在该 tab 是当前聚焦 tab 时弹蒙层，避免抢焦点到非聚焦 tab
                if ws.active_tab_id == *tab_id {
                    let target = tab_panes[0].pane_id.clone();
                    match api::plugin_pane_open(
                        "herdr-freeze",
                        "frozen",
                        "overlay",
                        &ws.workspace_id,
                        &target,
                    ) {
                        Ok(pane_id) => {
                            eprintln!("[herdr-freeze] 弹蒙层 tab={} overlay={}", tab_id, pane_id);
                            self.overlays.push(Overlay {
                                workspace_id: ws.workspace_id.clone(),
                                tab_id: tab_id.clone(),
                                pane_id,
                            });
                        }
                        Err(e) => eprintln!("[herdr-freeze] 打开蒙层失败 tab={}: {e}", tab_id),
                    }
                }
            } else if !all_frozen && has_overlay {
                // tab 不再全冻结（出现新 pane / 某 pane 已不在冻结集）→ 解冻
                eprintln!("[herdr-freeze] tab={} 不再全冻结，收起蒙层并解冻", tab_id);
                self.thaw_tab(tab_id);
            }
        }
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
            // best-effort 解挂（进程可能已退出）
            freezer::resume(&FreezeTarget {
                roots: e.roots.clone(),
                pgid: e.pgid,
            });
            self.last_change.remove(&e.pane_id);
        }
        if !dead.is_empty() {
            self.frozen.retain(|f| live.contains(f.pane_id.as_str()));
            state::save_frozen(&self.frozen);
        }
    }
}
