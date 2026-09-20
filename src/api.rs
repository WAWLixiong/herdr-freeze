//! herdr CLI 封装。通过 HERDR_BIN_PATH 调用 herdr（回退 "herdr"），可移植于
//! Unix socket 与 Windows 命名管道之间。所有调用一次性请求/响应（无需常驻 socket）。
//!
//! 输出 JSON 信封：成功 {"id","result":{"type":"...",<字段>}}，失败 {"id","error"}。
//! 本模块用 serde_json::Value 动态取值，避免与 herdr schema 强耦合。

use std::process::Command;

/// herdr 二进制路径（优先 HERDR_BIN_PATH）。
pub fn bin_path() -> String {
    std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string())
}

/// 运行 herdr 子命令，返回 stdout 解析出的 JSON Value（成功响应的 `result` 字段）。
/// 失败（进程退出码非 0 / 非 JSON / 含 error）返回 Err(message)。
fn run_json(args: &[&str]) -> Result<serde_json::Value, String> {
    let bin = bin_path();
    let output = Command::new(&bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("spawn herdr failed ({bin}): {e}"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let out = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "herdr {} exited {}: stderr={err} stdout={out}",
            args.join(" "),
            output.status
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("parse herdr json failed: {e}; raw={stdout}"))?;
    if value.get("error").is_some() {
        return Err(format!("herdr {} error: {}", args.join(" "), value));
    }
    match value.get("result") {
        Some(result) => Ok(result.clone()),
        None => Ok(value.clone()),
    }
}

/// 运行不关心输出的 herdr 调用（report-metadata / rename / pane open 等）。
fn run_ok(args: &[&str]) -> Result<(), String> {
    let bin = bin_path();
    let output = Command::new(&bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("spawn herdr failed ({bin}): {e}"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "herdr {} exited {}: {err}",
            args.join(" "),
            output.status
        ));
    }
    // 即便退出码 0 也确认不是 error 响应
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        if value.get("error").is_some() {
            return Err(format!("herdr {} error: {}", args.join(" "), value));
        }
    }
    Ok(())
}

// =============================== workspace ===============================

#[derive(Debug, Clone)]
pub struct Workspace {
    pub workspace_id: String,
    pub label: String,
    pub tokens: std::collections::HashMap<String, String>,
}

pub fn workspace_list() -> Result<Vec<Workspace>, String> {
    let result = run_json(&["workspace", "list"])?;
    let arr = result
        .get("workspaces")
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("workspace list: missing workspaces array: {result}"))?;
    let mut out = Vec::new();
    for ws in arr {
        out.push(Workspace {
            workspace_id: ws
                .get("workspace_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            label: ws
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            tokens: token_map(ws.get("tokens")),
        });
    }
    Ok(out)
}

/// 写 workspace 元数据 token（配置存储）。source 固定 "herdr-freeze"。
/// ttl 取最大 24h，避免频繁过期；守护进程会定期续期。
pub fn workspace_report_metadata(
    workspace_id: &str,
    tokens: &[(&str, &str)],
    ttl_ms: u64,
) -> Result<(), String> {
    let mut args: Vec<String> = vec![
        "workspace".into(),
        "report-metadata".into(),
        workspace_id.into(),
        "--source".into(),
        "herdr-freeze".into(),
        "--ttl-ms".into(),
        ttl_ms.to_string(),
    ];
    for (k, v) in tokens {
        args.push("--token".into());
        args.push(format!("{k}={v}"));
    }
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_ok(&refs)
}

/// 清除 workspace 的冻结配置 token（恢复默认）。
pub fn workspace_clear_metadata(workspace_id: &str, keys: &[&str]) -> Result<(), String> {
    let mut args: Vec<String> = vec![
        "workspace".into(),
        "report-metadata".into(),
        workspace_id.into(),
        "--source".into(),
        "herdr-freeze".into(),
        "--ttl-ms".into(),
        "86400000".into(),
    ];
    for k in keys {
        args.push("--clear-token".into());
        args.push((*k).into());
    }
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_ok(&refs)
}

// =============================== pane ===============================

#[derive(Debug, Clone)]
pub struct Pane {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub focused: bool,
    pub label: Option<String>,
    #[allow(dead_code)]
    pub title: Option<String>,
    pub revision: u64,
}

pub fn pane_list(workspace_id: &str) -> Result<Vec<Pane>, String> {
    let result = run_json(&["pane", "list", "--workspace", workspace_id])?;
    let arr = result
        .get("panes")
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("pane list: missing panes array: {result}"))?;
    let mut out = Vec::new();
    for p in arr {
        out.push(Pane {
            pane_id: p
                .get("pane_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            workspace_id: p
                .get("workspace_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            tab_id: p
                .get("tab_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            focused: p.get("focused").and_then(|v| v.as_bool()).unwrap_or(false),
            label: p
                .get("label")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            title: p
                .get("title")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            revision: p.get("revision").and_then(|v| v.as_u64()).unwrap_or(0),
        });
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub shell_pid: Option<u32>,
    pub foreground_process_group_id: Option<u32>,
    /// 前台进程 pid 列表（agent 实体）。
    pub foreground_pids: Vec<u32>,
}

pub fn pane_process_info(pane_id: &str) -> Result<ProcessInfo, String> {
    let result = run_json(&["pane", "process-info", "--pane", pane_id])?;
    let info = result
        .get("process_info")
        .ok_or_else(|| format!("process-info: missing process_info: {result}"))?;
    let shell_pid = info
        .get("shell_pid")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);
    let pgid = info
        .get("foreground_process_group_id")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);
    let mut foreground_pids = Vec::new();
    if let Some(procs) = info.get("foreground_processes").and_then(|v| v.as_array()) {
        for proc in procs {
            if let Some(pid) = proc.get("pid").and_then(|v| v.as_u64()) {
                foreground_pids.push(pid as u32);
            }
        }
    }
    Ok(ProcessInfo {
        shell_pid,
        foreground_process_group_id: pgid,
        foreground_pids,
    })
}

/// 设置 pane 标签（冻结时加 ❄ 前缀）。label=None 时清除（--clear）。
pub fn pane_set_label(pane_id: &str, label: Option<&str>) -> Result<(), String> {
    match label {
        Some(text) => run_ok(&["pane", "rename", pane_id, text]),
        None => run_ok(&["pane", "rename", pane_id, "--clear"]),
    }
}

// =============================== plugin pane ===============================

/// 打开插件 pane（config-ui 配置弹窗）。overlay/popup 都「target the active
/// pane」——herdr 拒绝 --workspace/--target-pane（invalid_params），故此处
/// 不传，由 herdr 用当前活动 pane/workspace 打开。返回新建 pane 的 pane_id。
pub fn plugin_pane_open(plugin_id: &str, entrypoint: &str, placement: &str) -> Result<String, String> {
    let refs = &[
        "plugin",
        "pane",
        "open",
        "--plugin",
        plugin_id,
        "--entrypoint",
        entrypoint,
        "--placement",
        placement,
    ];
    let result = run_json(refs)?;
    let pane_id = result
        .get("plugin_pane")
        .and_then(|v| v.get("pane"))
        .and_then(|v| v.get("pane_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("plugin pane open: missing plugin_pane.pane.pane_id: {result}"))?
        .to_string();
    Ok(pane_id)
}

fn token_map(value: Option<&serde_json::Value>) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    if let Some(obj) = value.and_then(|v| v.as_object()) {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                map.insert(k.clone(), s.to_string());
            }
        }
    }
    map
}
