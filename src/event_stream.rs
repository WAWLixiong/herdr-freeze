//! herdr API socket 长连接订阅聚焦事件。
//!
//! 守护进程不再 shell out 到 `herdr` CLI 二进制取聚焦变化（manifest 事件钩子
//! 每次会 spawn 一个新进程），而是直连 herdr 注入的 `HERDR_SOCKET_PATH`
//! （startup hook 注入，已解析好），用 `events.subscribe` 注册持久订阅，
//! 在同一 socket 上持续接收 bare `EventEnvelope`（NDJSON，一行一个 JSON）。
//!
//! 协议要点（见 herdr 仓 src/api）：
//! - 请求：`{"id","method":"events.subscribe","params":{"subscriptions":[...]}}`
//! - ack：`{"id","result":{"type":"subscription_started"}}`
//! - 事件（无 id/result 包装）：`{"event":"pane.focused","data":{...}}`
//! - `events.wait` 只支持 PaneAgentStatusChanged matcher（对 tab/pane.focused
//!   返回 unsupported_event_wait_match），故必须用 events.subscribe 流式。
//! - `Subscription::PaneFocused{}`/`TabFocused{}` 无过滤字段，订阅全量事件，
//!   守护进程在内存按 workspace 过滤。
//!
//! reader 线程阻塞 read_line 收事件，经 mpsc channel 发给主线程。
//!
//! 进程生命周期：herdr 退出时会 cleanup socket 文件（herdr src/server/headless/
//! lifecycle.rs complete_shutdown）。重连失败时检查 socket 文件：
//! - 不存在 → herdr 已退出 → 立即 process::exit(0)（瞬时自杀，避免孤儿）。
//! - 存在但连不上 → herdr 重启中，退避重连。
//! - 累计 60s 仍连不上 → 兜底自杀（防 herdr 异常崩溃未 cleanup socket）。
//!
//! 这样 herdr 退出 → socket 文件清理 → monitor 瞬时自杀；herdr 重启 spawn
//! 新 monitor 时旧已死，无双开。

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::{freeze_dbg, freeze_log};

/// 聚焦事件。pane.focused 直接给 pane_id；tab.focused 给 tab_id+workspace_id
/// （payload 无 pane_id，守护进程查该 tab 当前聚焦 pane）。
#[derive(Debug, Clone)]
pub enum FocusEvent {
    Pane(String),
    Tab(String, String),
}

/// 重连失败分类。SocketGone → herdr 已退出，自杀；Transient → 暂时性，重连。
#[derive(Debug)]
enum StreamError {
    SocketGone,
    Transient(String),
}

/// 启动 reader 线程：连 HERDR_SOCKET_PATH，订阅 pane.focused+tab.focused，
/// 循环读事件经 tx 发出。连接断开 → 检查 socket 文件决定自杀或重连。
/// herdr 已退出（socket 文件消失）或连续重连失败超 60s → process::exit(0)。
pub fn spawn_reader(tx: Sender<FocusEvent>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut first_fail: Option<Instant> = None;
        loop {
            match run_stream(&tx) {
                Ok(()) => {
                    // run_stream 内部无限循环，正常不返回；返回则重置失败计时。
                    first_fail = None;
                }
                Err(StreamError::SocketGone) => {
                    freeze_log!("herdr socket 文件消失，认定 herdr 已退出，退出守护进程");
                    std::process::exit(0);
                }
                Err(StreamError::Transient(e)) => {
                    let now = Instant::now();
                    let first = *first_fail.get_or_insert(now);
                    if now.duration_since(first) >= Duration::from_secs(60) {
                        freeze_log!("event stream 连续重连失败超 60s，兜底退出守护进程: {e}");
                        std::process::exit(0);
                    }
                    freeze_log!("event stream: {e}，2s 后重连");
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
    })
}

fn run_stream(tx: &Sender<FocusEvent>) -> Result<(), StreamError> {
    let path = std::env::var("HERDR_SOCKET_PATH")
        .map_err(|_| StreamError::Transient("HERDR_SOCKET_PATH 未设置".into()))?;
    let mut stream = connect_socket(&path)
        .map_err(|e| classify_connect_err(&path, e))?;

    let req = serde_json::json!({
        "id": "herdr-freeze-sub",
        "method": "events.subscribe",
        "params": {
            "subscriptions": [
                {"type": "pane.focused"},
                {"type": "tab.focused"}
            ]
        }
    });
    let line = format!("{}\n", req);
    stream
        .write_all(line.as_bytes())
        .map_err(|e| StreamError::Transient(format!("write subscribe: {e}")))?;
    stream
        .flush()
        .map_err(|e| StreamError::Transient(format!("flush subscribe: {e}")))?;

    let mut reader = BufReader::new(stream);

    // 读 ack：成功 {"id","result":{"type":"subscription_started"}}，
    // 失败 {"id","error":{"code","message"}}。
    let mut ack = String::new();
    reader
        .read_line(&mut ack)
        .map_err(|e| classify_read_err(&path, e))?;
    freeze_dbg!("event stream ack: {}", ack.trim());
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(ack.trim()) {
        if v.get("error").is_some() {
            freeze_log!("subscribe 被拒: {v}");
            return Err(StreamError::Transient(format!("subscribe 被拒: {v}")));
        }
    } else {
        freeze_dbg!("event stream ack 非 JSON: {:?}", ack);
    }

    freeze_log!("event stream 已连接，订阅 pane.focused+tab.focused");

    // 循环读 bare EventEnvelope（一行一个 JSON）。EOF = herdr 关 socket
    // （退出/重启），交由上层 classify 决定自杀或重连。
    loop {
        let mut buf = String::new();
        let n = reader
            .read_line(&mut buf)
            .map_err(|e| classify_read_err(&path, e))?;
        if n == 0 {
            freeze_dbg!("event stream EOF (herdr 关流)");
            return Err(classify_eof(&path));
        }
        if let Some(ev) = parse_event(&buf) {
            let _ = tx.send(ev);
        }
    }
}

/// 连接失败分类：socket 文件不存在 → SocketGone（herdr 已退出）；否则 Transient。
fn classify_connect_err(path: &str, e: std::io::Error) -> StreamError {
    if !Path::new(path).exists() {
        StreamError::SocketGone
    } else {
        StreamError::Transient(format!("connect {path}: {e}"))
    }
}

/// 读错误分类：socket 文件不存在 → SocketGone；否则 Transient（可能 herdr 重启）。
fn classify_read_err(path: &str, e: std::io::Error) -> StreamError {
    if !Path::new(path).exists() {
        StreamError::SocketGone
    } else {
        StreamError::Transient(format!("read event: {e}"))
    }
}

/// EOF 分类：socket 文件不存在 → SocketGone（herdr 退出已 cleanup）；
/// 存在 → Transient（herdr 可能正在重启）。
fn classify_eof(path: &str) -> StreamError {
    if !Path::new(path).exists() {
        StreamError::SocketGone
    } else {
        StreamError::Transient("EOF（herdr 关闭 socket，可能正在重启）".into())
    }
}

/// 连接 herdr API socket。Unix 用 file-path name（GenericFilePath），
/// Windows 用 namespaced-pipe name（GenericNamespaced）——与 herdr ipc.rs 一致。
fn connect_socket(path: &str) -> std::io::Result<interprocess::local_socket::Stream> {
    use interprocess::local_socket::prelude::*;
    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        let name = std::path::Path::new(path).to_fs_name::<GenericFilePath>()?;
        interprocess::local_socket::Stream::connect(name)
    }
    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        let name = path.to_string().to_ns_name::<GenericNamespaced>()?;
        interprocess::local_socket::Stream::connect(name)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "unsupported platform",
        ))
    }
}

fn parse_event(line: &str) -> Option<FocusEvent> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let event = v.get("event")?.as_str()?;
    let data = v.get("data")?;
    // herdr 的 EventKind 用 serde rename_all="snake_case" 序列化，故 wire 是
    // "pane_focused"/"tab_focused"（下划线），而非 dot_name() 的点分形式。
    match event {
        "pane_focused" => {
            let pane_id = data.get("pane_id")?.as_str()?;
            freeze_dbg!("event pane_focused pane={}", pane_id);
            Some(FocusEvent::Pane(pane_id.to_string()))
        }
        "tab_focused" => {
            let tab_id = data.get("tab_id")?.as_str()?;
            let workspace_id = data.get("workspace_id")?.as_str()?;
            freeze_dbg!("event tab_focused tab={} ws={}", tab_id, workspace_id);
            Some(FocusEvent::Tab(tab_id.to_string(), workspace_id.to_string()))
        }
        _ => {
            freeze_dbg!("event 未知 event={}", event);
            None
        }
    }
}
