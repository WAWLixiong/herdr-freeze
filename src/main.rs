//! herdr-freeze 插件二进制入口。子命令：
//!   monitor     —— 常驻监控守护进程（启动钩子）
//!   config      —— 查看/设置当前 workspace 的冻结配置（动作）
//!                  无设置参数时打开交互式配置弹窗 config-ui
//!   config-ui   —— 交互式配置弹窗（插件 pane 入口点）
//!   thaw        —— 手动解冻当前 tab（写解冻信号，守护进程处理）
//!   freeze-now  —— 立即冻结当前 tab（写信号，守护进程处理）
//!
//! 冻结/解冻逻辑：守护进程常驻订阅 herdr 的 pane.focused/tab.focused 事件流
//! （events.subscribe，socket 长连接），空闲判定靠进程组 CPU 采样 + 聚焦
//! 60s grace（适配 work-assistant 的 PTY 时间戳方案——herdr-freeze 不拥有
//! PTY，用 CPU 采样替代 PTY 输出信号）。冻结 = 挂起进程树 + label 加 ❄；
//! 解冻 = 聚焦到冻结 pane 即时恢复该单 pane。

mod api;
mod cpu_sample;
mod event_stream;
mod freezer;
mod monitor;
mod state;

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

/// DEBUG 日志总开关。两条开启路径任一即开：
/// - CLI flag `herdr-freeze monitor --debug`/`-v`（main 解析后 enable_debug()）
/// - env var `HERDR_FREEZE_DEBUG`（任意非空值；插件场景 startup hook 继承
///   herdr 进程环境，用户 `HERDR_FREEZE_DEBUG=1 herdr` 即可，无需改清单）
static DEBUG: AtomicBool = AtomicBool::new(false);

/// CLI flag 开启 DEBUG（monitor 子命令解析 --debug 时调用）。
pub(crate) fn enable_debug() {
    DEBUG.store(true, Ordering::Relaxed);
}

/// DEBUG 是否开启。flag 或 env 任一为真即开。
pub(crate) fn debug_enabled() -> bool {
    DEBUG.load(Ordering::Relaxed) || std::env::var("HERDR_FREEZE_DEBUG").is_ok()
}

/// 日志文件路径（固定，便于 startup hook 场景查看——herdr 捕获 stderr 到
/// 内存不暴露内容，改写文件后 `HERDR_FREEZE_DEBUG=1 herdr` 重启会话即可
/// `tail -f` 看 monitor 判定链路 + event stream）。
/// Unix 固定 /tmp/herdr-freeze.log；Windows 上 "/tmp" 会被解析为「当前盘符
/// 根\tmp」，该目录通常不存在 → OpenOptions create 静默失败、日志全丢，
/// 故改用 %TEMP%\herdr-freeze.log。
pub(crate) fn log_file() -> std::path::PathBuf {
    if cfg!(windows) {
        std::env::var("TEMP")
            .map(|d| std::path::PathBuf::from(d).join("herdr-freeze.log"))
            .unwrap_or_else(|_| std::path::PathBuf::from("herdr-freeze.log"))
    } else {
        std::path::PathBuf::from("/tmp/herdr-freeze.log")
    }
}

/// 追加写一行到日志文件（best-effort，失败静默）。带秒级时间戳，便于测延迟。
pub(crate) fn file_log(line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file())
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let _ = writeln!(f, "{ts:.3} {line}");
    }
}

/// INFO 级日志（始终 stderr + 文件）。统一前缀 `[herdr-freeze]`。
macro_rules! freeze_log {
    ($($t:tt)*) => {{
        let line = format!("[herdr-freeze] {}", format_args!($($t)*));
        eprintln!("{}", line);
        crate::file_log(&line);
    }};
}
/// DEBUG 级日志（debug_enabled() 时 stderr + 文件）。前缀 `[herdr-freeze] [dbg]`。
macro_rules! freeze_dbg {
    ($($t:tt)*) => {{
        if crate::debug_enabled() {
            let line = format!("[herdr-freeze] [dbg] {}", format_args!($($t)*));
            eprintln!("{}", line);
            crate::file_log(&line);
        }
    }};
}
pub(crate) use freeze_dbg;
pub(crate) use freeze_log;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(|s| s.as_str()) {
        None | Some("--help") | Some("-h") => {
            print_help();
            0
        }
        Some("monitor") => {
            if wants_help(&args[1..]) {
                print_monitor_help();
                0
            } else {
                // --debug / -v / --verbose：开启 DEBUG 判定链路日志
                if args[1..]
                    .iter()
                    .any(|a| a == "--debug" || a == "-v" || a == "--verbose")
                {
                    enable_debug();
                }
                monitor::Monitor::run()
            }
        }
        Some("config") => {
            if wants_help(&args[1..]) {
                print_config_help();
                0
            } else {
                config_cmd(&args[1..])
            }
        }
        Some("config-ui") => {
            if wants_help(&args[1..]) {
                print_config_ui_help();
                0
            } else {
                config_ui()
            }
        }
        Some("thaw") => {
            if wants_help(&args[1..]) {
                print_thaw_help();
                0
            } else {
                thaw_cmd(&args[1..])
            }
        }
        Some("freeze-now") => {
            if wants_help(&args[1..]) {
                print_freeze_now_help();
                0
            } else {
                freeze_now_cmd(&args[1..])
            }
        }
        Some(other) => {
            eprintln!(
                "unknown subcommand: {other}\n\nRun 'herdr-freeze --help' for usage."
            );
            2
        }
    };
    std::process::exit(code);
}

fn wants_help(args: &[String]) -> bool {
    args.iter().any(|a| a == "--help" || a == "-h")
}

fn print_help() {
    println!("herdr-freeze {} - Auto-freeze idle Herdr panes to release memory\n", env!("CARGO_PKG_VERSION"));
    println!("USAGE:");
    println!("    herdr-freeze <SUBCOMMAND> [OPTIONS]\n");
    println!("SUBCOMMANDS:");
    println!("    monitor       常驻监控守护进程（由 herdr startup hook 拉起，通常不手动运行）");
    println!("    config        查看/设置 workspace 冻结配置");
    println!("    config-ui     交互式配置弹窗（由 config 动作打开，不手动运行）");
    println!("    thaw          手动解冻当前 tab 的所有冻结 pane");
    println!("    freeze-now    立即冻结当前 tab 的所有 pane（不等空闲阈值）\n");
    println!("OPTIONS:");
    println!("    -h, --help    打印本帮助或子命令帮助\n");
    println!("冻结机制：CPU 采样判空闲 + 挂起进程树；聚焦冻结 pane 即时解冻。");
    println!("见 README.md 或 https://herdr.dev 了解详情。");
}

fn print_monitor_help() {
    println!("herdr-freeze monitor - 常驻监控守护进程\n");
    println!("USAGE:");
    println!("    herdr-freeze monitor [--debug]\n");
    println!("由 herdr startup hook 在会话恢复时拉起。常驻：周期（15s）采样每 pane");
    println!("进程组 CPU 时间判空闲，挂起空闲进程树；订阅 pane.focused/tab.focused");
    println!("事件流，聚焦冻结 pane 即时解冻。herdr 退出时（socket 文件消失）");
    println!("自动退出，不留孤儿。\n");
    println!("手动运行（调试）：Ctrl-C 退出。首个 tick 初始化 CPU 采样基线，");
    println!("idle_secs 内不会冻结。\n");
    println!("OPTIONS:");
    println!("    --debug, -v, --verbose   打印判定链路 DEBUG 日志（每 pane 每 tick");
    println!("             的走向、CPU 采样值、label 处理）。等价于设");
    println!("             HERDR_FREEZE_DEBUG=1。插件场景用 env var 更省事：");
    println!("             `HERDR_FREEZE_DEBUG=1 herdr`（startup hook 继承）。");
}

fn print_config_help() {
    println!("herdr-freeze config - 查看/设置 workspace 冻结配置\n");
    println!("USAGE:");
    println!("    herdr-freeze config --workspace <ID> [OPTIONS]");
    println!("    herdr-freeze config --workspace <ID> --clear\n");
    println!("OPTIONS:");
    println!("    --workspace <ID>    workspace id（或在工作区上下文里调用，省略）");
    println!("    --enabled <BOOL>   true/false（默认 true，开启自动冻结）");
    println!("    --idle-secs <N>    空闲阈值秒（默认 180，下限 30）");
    println!("    --clear            清除配置，恢复默认\n");
    println!("无设置参数时：作为 herdr 动作调用打开交互弹窗；CLI 调用打印当前配置。\n");
    println!("示例:");
    println!("    herdr-freeze config --workspace w1 --enabled true --idle-secs 120");
    println!("    herdr-freeze config --workspace w1            # 查看当前配置");
    println!("    herdr-freeze config --workspace w1 --clear     # 恢复默认");
}

fn print_config_ui_help() {
    println!("herdr-freeze config-ui - 交互式配置弹窗\n");
    println!("USAGE:");
    println!("    herdr-freeze config-ui\n");
    println!("由 config 动作（无设置参数时）经 plugin pane open 打开，不手动运行。");
    println!("弹窗内交互设置 enabled / idle-secs。");
}

fn print_thaw_help() {
    println!("herdr-freeze thaw - 手动解冻当前 tab\n");
    println!("USAGE:");
    println!("    herdr-freeze thaw [--tab <ID>]\n");
    println!("OPTIONS:");
    println!("    --tab <ID>    tab id（或在 tab 上下文里调用，省略）\n");
    println!("解冻该 tab 所有冻结 pane（恢复进程 + 还原标签）。直接读 frozen.json");
    println!("resume，不依赖监控守护进程在跑——守护进程崩溃时这是兜底解冻方式。");
    println!("与聚焦解冻等价，但整 tab 粒度。");
}

fn print_freeze_now_help() {
    println!("herdr-freeze freeze-now - 立即冻结当前 tab\n");
    println!("USAGE:");
    println!("    herdr-freeze freeze-now [--tab <ID>]\n");
    println!("OPTIONS:");
    println!("    --tab <ID>    tab id（或在 tab 上下文里调用，省略）\n");
    println!("立即冻结该 tab 所有 pane（不等空闲阈值；仍受 workspace 是否开启冻结");
    println!("限制）。写信号，由监控守护进程下一轮处理（需守护进程在跑）。");
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
        // 打开 config-ui 弹窗（popup 由 herdr 用当前活动 pane/workspace 打开，
        // 不传 --workspace/--target-pane——overlay/popup 均拒绝它们）。
        match api::plugin_pane_open("herdr-freeze", "config", "popup") {
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
    #[cfg(not(windows))]
    {
        let _ = args;
        eprintln!(
            "[herdr-freeze] freeze-now: 本平台不支持冻结（herdr PTY/job-control 限制，详见 README「限制」）。"
        );
        1
    }
    #[cfg(windows)]
    {
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
}
