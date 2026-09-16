//! herdr-freeze 插件二进制入口。子命令：
//!   monitor     —— 常驻监控守护进程（启动钩子）
//!   frozen      —— 冻结蒙层 UI（插件 pane 入口点）
//!   config      —— 查看/设置当前 workspace 的冻结配置（动作）
//!                  无设置参数时打开交互式配置弹窗 config-ui
//!   config-ui   —— 交互式配置弹窗（插件 pane 入口点）
//!   thaw        —— 手动解冻当前 tab（写解冻信号，守护进程处理）
//!   freeze-now  —— 立即冻结当前 tab（写信号，守护进程处理）

mod api;
mod freezer;
mod monitor;
mod overlay;
mod state;

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(|s| s.as_str()) {
        None => {
            eprintln!(
                "usage: herdr-freeze <monitor|frozen|config|config-ui|thaw|freeze-now> [args]"
            );
            2
        }
        Some("monitor") => monitor::Monitor::run(),
        Some("frozen") => overlay::run(),
        Some("config") => config_cmd(&args[1..]),
        Some("config-ui") => config_ui(),
        Some("thaw") => thaw_cmd(&args[1..]),
        Some("freeze-now") => freeze_now_cmd(&args[1..]),
        Some(other) => {
            eprintln!("unknown subcommand: {other}");
            2
        }
    };
    std::process::exit(code);
}

// =============================== 上下文 ===============================

struct Context {
    workspace_id: Option<String>,
    tab_id: Option<String>,
    #[allow(dead_code)]
    pane_id: Option<String>,
}

fn read_context() -> Context {
    let mut ws = std::env::var("HERDR_WORKSPACE_ID")
        .ok()
        .filter(|s| !s.is_empty());
    let mut tab = std::env::var("HERDR_TAB_ID").ok().filter(|s| !s.is_empty());
    let mut pane = std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|s| !s.is_empty());
    if let Ok(json) = std::env::var("HERDR_PLUGIN_CONTEXT_JSON") {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) {
            if ws.is_none() {
                ws = value
                    .get("workspace_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
            if tab.is_none() {
                tab = value
                    .get("tab_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
            if pane.is_none() {
                pane = value
                    .get("pane_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
        }
    }
    Context {
        workspace_id: ws,
        tab_id: tab,
        pane_id: pane,
    }
}

// =============================== config 命令 ===============================

fn config_cmd(args: &[String]) -> i32 {
    let mut workspace_id = None;
    let mut enabled = None;
    let mut idle_secs: Option<u64> = None;
    let mut clear = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--workspace" => {
                i += 1;
                workspace_id = args.get(i).cloned();
            }
            "--enabled" => {
                i += 1;
                enabled = args
                    .get(i)
                    .and_then(|s| match s.to_ascii_lowercase().as_str() {
                        "true" | "1" | "yes" | "y" | "on" => Some("true"),
                        "false" | "0" | "no" | "n" | "off" => Some("false"),
                        _ => None,
                    });
            }
            "--idle-secs" => {
                i += 1;
                if let Some(v) = args.get(i).and_then(|s| s.parse::<u64>().ok()) {
                    idle_secs = Some(v);
                }
            }
            "--clear" => clear = true,
            other => {
                eprintln!("config: unknown option {other}");
                return 2;
            }
        }
        i += 1;
    }
    let workspace_id = match workspace_id.or_else(|| read_context().workspace_id) {
        Some(w) => w,
        None => {
            eprintln!("config: 缺少 workspace id（用 --workspace 或在 workspace 上下文里调用）");
            return 2;
        }
    };

    if clear {
        let _ =
            api::workspace_clear_metadata(&workspace_id, &["freeze_enabled", "freeze_idle_secs"]);
        println!(
            "[herdr-freeze] 已清除 workspace {} 的冻结配置（恢复默认）",
            workspace_id
        );
        return 0;
    }

    if enabled.is_some() || idle_secs.is_some() {
        let mut tokens: Vec<(&str, String)> = Vec::new();
        let e_val = enabled.unwrap_or("true");
        let secs_val = idle_secs
            .map(|n| n.max(30).to_string())
            .unwrap_or_else(|| "180".to_string());
        // 直接构造字符串以借用
        let e_str = e_val.to_string();
        tokens.push(("freeze_enabled", e_str));
        tokens.push(("freeze_idle_secs", secs_val));
        let refs: Vec<(&str, &str)> = tokens.iter().map(|(k, v)| (*k, v.as_str())).collect();
        if let Err(e) = api::workspace_report_metadata(&workspace_id, &refs, 86_400_000) {
            eprintln!("config: 写入元数据失败: {e}");
            return 1;
        }
        println!(
            "[herdr-freeze] workspace {} 已设冻结 enabled={} idle_secs={}s",
            workspace_id,
            enabled.unwrap_or("(未改)"),
            idle_secs
                .map(|n| n.max(30).to_string())
                .unwrap_or_else(|| "(未改)".to_string())
        );
        return 0;
    }

    // 无设置参数：作为动作调用时打开交互式配置弹窗；否则打印当前配置
    if std::env::var("HERDR_PLUGIN_ACTION_ID").is_ok() {
        // 打开 config-ui 弹窗
        let target = read_context().pane_id.unwrap_or_default();
        match api::plugin_pane_open("herdr-freeze", "config", "popup", &workspace_id, &target) {
            Ok(_) => return 0,
            Err(e) => {
                eprintln!("config: 打开配置弹窗失败: {e}");
                // 回退到打印
            }
        }
    }
    print_current_config(&workspace_id)
}

fn print_current_config(workspace_id: &str) -> i32 {
    match api::workspace_list() {
        Ok(list) => {
            if let Some(ws) = list.iter().find(|w| w.workspace_id == workspace_id) {
                let enabled = ws
                    .tokens
                    .get("freeze_enabled")
                    .map(|s| s.as_str())
                    .unwrap_or("true");
                let secs = ws
                    .tokens
                    .get("freeze_idle_secs")
                    .map(|s| s.as_str())
                    .unwrap_or("180(默认)");
                println!(
                    "[herdr-freeze] workspace {} ({}): freeze_enabled={} freeze_idle_secs={}s",
                    workspace_id, ws.label, enabled, secs
                );
                0
            } else {
                eprintln!("config: 找不到 workspace {}", workspace_id);
                1
            }
        }
        Err(e) => {
            eprintln!("config: 读取 workspace 列表失败: {e}");
            1
        }
    }
}

// =============================== config-ui 交互弹窗 ===============================

fn config_ui() -> i32 {
    let ctx = read_context();
    let workspace_id = match ctx.workspace_id {
        Some(w) => w,
        None => {
            eprintln!("config-ui: 无 workspace 上下文");
            return 2;
        }
    };
    // 当前值
    let (cur_enabled, cur_secs) = match api::workspace_list() {
        Ok(list) => {
            let ws = list.iter().find(|w| w.workspace_id == workspace_id);
            (
                ws.and_then(|w| w.tokens.get("freeze_enabled").cloned())
                    .unwrap_or_else(|| "true".to_string()),
                ws.and_then(|w| w.tokens.get("freeze_idle_secs").cloned())
                    .unwrap_or_else(|| "180".to_string()),
            )
        }
        Err(_) => ("true".to_string(), "180".to_string()),
    };

    let mut out = std::io::stdout();
    let _ = write!(
        out,
        "\x1b[2J\x1b[HHerdr Freeze 配置 — workspace {}\r\n当前: enabled={} idle_secs={}s\r\n\r\n是否开启自动冻结? [Y/n]: ",
        workspace_id, cur_enabled, cur_secs
    );
    let _ = out.flush();

    let enabled = {
        let line = read_line_stdin();
        let t = line.trim().to_ascii_lowercase();
        match t.as_str() {
            "n" | "no" | "false" | "0" => "false",
            _ => "true",
        }
    }
    .to_string();

    let _ = write!(out, "空闲阈值秒 (>=30, 空白=保持 {}): ", cur_secs);
    let _ = out.flush();
    let line2 = read_line_stdin();
    let secs = match line2.trim().parse::<u64>().ok() {
        Some(n) => n.max(30),
        None => cur_secs.parse::<u64>().unwrap_or(180),
    };
    let secs_str = secs.to_string();
    let tokens: Vec<(&str, &str)> = vec![
        ("freeze_enabled", enabled.as_str()),
        ("freeze_idle_secs", secs_str.as_str()),
    ];
    match api::workspace_report_metadata(&workspace_id, &tokens, 86_400_000) {
        Ok(_) => {
            let _ = write!(
                out,
                "\r\n已保存: enabled={} idle_secs={}s\r\n按 Enter 关闭...",
                enabled, secs
            );
            let _ = out.flush();
            let _ = read_line_stdin();
            0
        }
        Err(e) => {
            let _ = write!(out, "\r\n保存失败: {e}\r\n");
            let _ = out.flush();
            1
        }
    }
}

/// 读一行 stdin（读到 \n 为止，整行连同 \r\n 一起消费掉，避免 \r\n 场景下
/// 逐字节读在 \r 处截断、把 \n 留给下一个 prompt 造成“下一项无法输入”）。
fn read_line_stdin() -> String {
    let mut s = String::new();
    match std::io::stdin().read_line(&mut s) {
        Ok(0) => String::new(),
        Ok(_) => s.trim_end_matches(['\r', '\n']).to_string(),
        Err(_) => String::new(),
    }
}

// =============================== thaw / freeze-now ===============================

/// 解析 --tab 或上下文 tab_id。未知选项返回 Err(2)。
fn resolve_tab_id(args: &[String]) -> Result<String, i32> {
    let mut tab_id = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tab" => {
                i += 1;
                tab_id = args.get(i).cloned();
            }
            other => {
                eprintln!("unknown option: {other}");
                return Err(2);
            }
        }
        i += 1;
    }
    tab_id.or_else(|| read_context().tab_id).ok_or_else(|| {
        eprintln!("缺少 tab id（用 --tab 或在 tab 上下文里调用）");
        2
    })
}

/// 解冻整 tab：直接从 frozen.json 读该 tab 的冻结记录 → resume 进程 + 还原标签 +
/// 移除记录。这样即使监控守护进程不在跑也能解冻（守护进程不在时 thaw 信号无人
/// drain）。同时仍推 thaw 信号：若监控在跑，下一轮 thaw_tab 会清内存状态 +
/// 设冷却 + 关蒙层，避免把刚解冻的 pane 又当空闲重新冻结/重开蒙层。
fn thaw_cmd(args: &[String]) -> i32 {
    let tab_id = match resolve_tab_id(args) {
        Ok(t) => t,
        Err(code) => return code,
    };
    let frozen = state::load_frozen();
    let mut remaining: Vec<state::FrozenEntry> = Vec::new();
    let mut resumed = 0usize;
    for entry in &frozen {
        if entry.tab_id == tab_id {
            freezer::resume(&freezer::FreezeTarget {
                roots: entry.roots.clone(),
                pgid: entry.pgid,
            });
            if let Err(e) = api::pane_set_label(&entry.pane_id, entry.orig_label.as_deref()) {
                eprintln!("[herdr-freeze] thaw: 恢复标签 {} 失败: {e}", entry.pane_id);
            }
            resumed += 1;
        } else {
            remaining.push(entry.clone());
        }
    }
    state::save_frozen(&remaining);
    state::push_thaw_request(&tab_id);
    println!(
        "[herdr-freeze] thaw: 已恢复 {} 个 pane (tab={})",
        resumed, tab_id
    );
    0
}

/// 立即冻结当前 tab（信号：由监控守护进程下一轮处理，需监控在跑）。
fn freeze_now_cmd(args: &[String]) -> i32 {
    let tab_id = match resolve_tab_id(args) {
        Ok(t) => t,
        Err(code) => return code,
    };
    state::push_freeze_now_request(&tab_id);
    println!(
        "[herdr-freeze] 已发送 freeze-now 信号 tab={}（监控下一轮处理）",
        tab_id
    );
    0
}
