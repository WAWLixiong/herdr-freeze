//! 持久化状态：冻结记录（守护进程崩溃后按 pid 恢复）+ 手动解冻信号。
//!
//! 状态目录优先 HERDR_PLUGIN_STATE_DIR（herdr 注入），否则回退到插件根下的
//! `.herdr-freeze-state`，再回退到当前工作目录。文件 JSON。

use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FrozenEntry {
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
    /// 进程树根 pid（前台进程 pid；空则用 shell_pid）。
    pub roots: Vec<u32>,
    /// 前台进程组 id（Unix 解挂用；Windows 忽略）。
    pub pgid: Option<u32>,
    /// 冻结前 pane 的原始 label（解冻后恢复；None 表示原无 label，恢复时 --clear）。
    pub orig_label: Option<String>,
}

pub fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("HERDR_PLUGIN_STATE_DIR") {
        let p = PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&p);
        return p;
    }
    let base = std::env::var("HERDR_PLUGIN_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    let p = base.join(".herdr-freeze-state");
    let _ = std::fs::create_dir_all(&p);
    p
}

fn frozen_path() -> PathBuf {
    state_dir().join("frozen.json")
}

fn thaw_path() -> PathBuf {
    state_dir().join("thaw_requests.json")
}

pub fn load_frozen() -> Vec<FrozenEntry> {
    let path = frozen_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

pub fn save_frozen(entries: &[FrozenEntry]) {
    let path = frozen_path();
    let _ = std::fs::write(
        &path,
        serde_json::to_string_pretty(entries).unwrap_or_else(|_| "[]".to_string()),
    );
}

/// 追加一条手动解冻信号（tab_id）。守护进程下一轮 drain_thaw_requests 取走。
pub fn push_thaw_request(tab_id: &str) {
    let mut list = load_thaw_requests();
    if !list.iter().any(|t| t == tab_id) {
        list.push(tab_id.to_string());
    }
    let path = thaw_path();
    let _ = std::fs::write(
        &path,
        serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string()),
    );
}

pub fn load_thaw_requests() -> Vec<String> {
    match std::fs::read_to_string(thaw_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn freeze_now_path() -> PathBuf {
    state_dir().join("freeze_now_requests.json")
}

/// 追加一条「立即冻结当前 tab」信号（手动 freeze-now 动作）。
pub fn push_freeze_now_request(tab_id: &str) {
    let path = freeze_now_path();
    let mut list: Vec<String> = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    if !list.iter().any(|t| t == tab_id) {
        list.push(tab_id.to_string());
    }
    let _ = std::fs::write(
        &path,
        serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string()),
    );
}

pub fn drain_freeze_now_requests() -> Vec<String> {
    let path = freeze_now_path();
    let list: Vec<String> = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let _ = std::fs::write(&path, "[]");
    list
}

/// 读取并清空手动解冻信号。
pub fn drain_thaw_requests() -> Vec<String> {
    let list = load_thaw_requests();
    let _ = std::fs::write(thaw_path(), "[]");
    list
}
