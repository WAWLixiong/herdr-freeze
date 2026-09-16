//! 冻结蒙层 UI（插件 pane 入口点 `frozen`）。
//!
//! 清屏绘制「❄ 已冻结 · 整个标签页已冻结 · 按任意键恢复整个 tab」，之后阻塞
//! 等待任意 stdin 字节（按键）即退出 → herdr 关闭该 overlay pane → 守护进程
//! 检测到 pane 关闭后解冻整个 tab。
//!
//! 进入 raw 模式，使任意按键字节即时送达 read()（无需按回车）。herdr 0.9 本地
//! shell 模式下不把鼠标事件转发给 pane 程序，故只支持按键解冻，不开鼠标追踪。

use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub fn run() -> i32 {
    // raw 模式：关闭 PTY 规范（行）缓冲，任意按键字节即时送达 read()。
    let _ = enable_raw_mode();

    // 读 stdin 的线程：读到任意字节（按键）或 stdin 关闭即标记退出。
    let got_input = Arc::new(AtomicBool::new(false));
    let g = Arc::clone(&got_input);
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 1];
        let _ = std::io::stdin().read(&mut buf); // 阻塞直到字节/EOF/出错
        g.store(true, Ordering::Relaxed);
    });

    loop {
        draw();
        if got_input.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(600));
    }

    // 复位 raw 模式 + 光标/颜色/清屏
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[?25h\x1b[0m\x1b[2J\x1b[H");
    let _ = out.flush();
    let _ = disable_raw_mode();
    0
}

fn draw() {
    let mut out = std::io::stdout();
    // 清屏 + 光标回原点 + 隐藏光标 + 深蓝背景白字
    let _ = write!(out, "\x1b[2J\x1b[H\x1b[?25l\x1b[48;5;17m\x1b[38;5;255m");
    let (cols, rows) = term_size();
    let width = cols.clamp(40, 64) as usize;
    let height = 7usize.min(rows as usize);
    let left_pad = ((cols as usize).saturating_sub(width)) / 2;
    let top_pad = ((rows as usize).saturating_sub(height)) / 2;

    let lp = " ".repeat(left_pad);
    let border: String = "─".repeat(width.saturating_sub(2));

    let _ = writeln!(out);
    for _ in 0..top_pad {
        let _ = writeln!(out, "{lp}\x1b[0m");
    }
    let _ = writeln!(out, "{lp}╭{border}╮\x1b[0m");
    let _ = writeln!(out, "{lp}│{}│\x1b[0m", center("❄  已冻结", width));
    let _ = writeln!(
        out,
        "{lp}│{}│\x1b[0m",
        center("整个标签页已冻结 · 内存已释放", width)
    );
    let _ = writeln!(
        out,
        "{lp}│{}│\x1b[0m",
        center("按任意键 恢复整个 tab", width)
    );
    let _ = writeln!(out, "{lp}╰{border}╯\x1b[0m");
    let _ = out.flush();
}

/// 居中一行到 width 宽度（width 含两侧边框字符位）。
fn center(text: &str, width: usize) -> String {
    let inner = width.saturating_sub(2);
    let tlen = text.chars().count();
    if tlen >= inner {
        return text.chars().take(inner).collect();
    }
    let pad = inner - tlen;
    let left = pad / 2;
    let right = pad - left;
    format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
}

fn term_size() -> (u16, u16) {
    // 优先环境变量，否则默认 80×24。herdr pane 通常不设 COLUMNS/LINES，
    // 默认值足以居中显示。
    let cols = std::env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|&n| n >= 20)
        .unwrap_or(80);
    let rows = std::env::var("LINES")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|&n| n >= 6)
        .unwrap_or(24);
    (cols, rows)
}
