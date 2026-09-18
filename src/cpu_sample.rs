//! 跨平台进程组/树 CPU 时间采样，用于空闲判定。
//!
//! 适配 work-assistant 的 PTY 时间戳空闲检测方案——herdr-freeze 不拥有 PTY
//! （herdr 拥有），拿不到「上次输出/输入」原始时间戳，改用进程 CPU 时间
//! 增量作为活动信号：周期采样每 pane 进程组/树的总 CPU 时间，delta>0 视为
//! 有活动（进程在跑），刷新 last_cpu_active_at。比 work-assistant 的 PTY
//! 输出信号更强——能检测无终端输出的 CPU-bound 进程（如 cargo build）。
//!
//! 平台：
//! - Windows：复刻 work-assistant——用 CreateToolhelp32Snapshot 的进程树
//!   BFS（已在 freezer.rs 实现，复用 tree_pids）+ GetProcessTimes 求和
//!   user/kernel 100ns ticks。
//! - Linux：遍历 /proc/[pid]/stat，按 pgrp 匹配进程组，求和 utime+stime
//!   （jiffies，CLK_TCK 假定 100，即 1 jiffy=10ms）。无 pgid 时用 roots
//!   单 pid 兜底。无新依赖（std::fs 读 /proc）。
//! - macOS：对 roots（herdr 给的 foreground_processes，agent 实体）逐 pid
//!   proc_pidinfo(PROC_PIDTASKINFO) 取 total_user+total_system（ns）。
//!   不遍历整个进程组（需 PROC_PIDTBSDINFO 查 pbi_pgid，struct layout 复杂
//!   且易错；foreground_pids 已是活动主体，MVP 以此为准）。
//!
//! busy_ratio：t0→sleep window→t1，delta 归一化为「一核的占比」
//! （0.0=0 核，1.0=满载一核），阈值 >0.10 即 10% 一核，与 work-assistant
//! 的 FREEZE_CPU_BUSY_RATIO 一致、跨平台可比。

use std::time::Duration;

/// 平台内一致的 CPU 时间累加值（单位随平台：Windows=100ns ticks，
/// Linux=jiffies，macOS=ns）。仅需同平台内 delta>0 判活动 + busy_ratio 归一化。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTime(pub u64);

/// 采样进程组/树的总 CPU 时间。
/// Windows：roots 作为树根 BFS（pgid 忽略）；Linux：pgid 匹配进程组
/// （无 pgid 用 roots 单 pid）；macOS：roots 各 pid 的 taskinfo 求和。
pub fn sample(roots: &[u32], pgid: Option<u32>) -> CpuTime {
    let _ = pgid; // Windows/macOS 分支不用 pgid（仅 Linux 用），此处统一消警告
    #[cfg(windows)]
    {
        CpuTime(sample_windows(roots))
    }
    #[cfg(target_os = "linux")]
    {
        CpuTime(sample_linux(pgid, roots))
    }
    #[cfg(target_os = "macos")]
    {
        CpuTime(sample_macos(roots))
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = roots;
        CpuTime(0)
    }
}

/// 短窗口 CPU 占比（一核的占比：0.0=0 核，1.0=满载一核）。
/// 临冻结前 guard：>0.10 即视为忙，跳过本轮（work-assistant FREEZE_CPU_BUSY_RATIO）。
pub fn busy_ratio(roots: &[u32], pgid: Option<u32>, window_ms: u64) -> f64 {
    let t0 = sample(roots, pgid).0;
    std::thread::sleep(Duration::from_millis(window_ms));
    let t1 = sample(roots, pgid).0;
    let delta = t1.saturating_sub(t0);
    cores_fraction(delta, window_ms)
}

#[cfg(windows)]
fn cores_fraction(delta: u64, window_ms: u64) -> f64 {
    // 1ms = 10_000 个 100ns ticks
    let window = window_ms.saturating_mul(10_000);
    if window == 0 {
        return 0.0;
    }
    delta as f64 / window as f64
}

#[cfg(target_os = "linux")]
fn cores_fraction(delta: u64, window_ms: u64) -> f64 {
    // CLK_TCK 假定 100（几乎所有 Linux）→ 1 jiffy = 10ms
    let window = window_ms / 10;
    if window == 0 {
        return 0.0;
    }
    delta as f64 / window as f64
}

#[cfg(target_os = "macos")]
fn cores_fraction(delta: u64, window_ms: u64) -> f64 {
    let window = window_ms.saturating_mul(1_000_000);
    if window == 0 {
        return 0.0;
    }
    delta as f64 / window as f64
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn cores_fraction(_delta: u64, _window_ms: u64) -> f64 {
    0.0
}

// =============================== Windows ===============================

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FileTime {
    low: u32,
    high: u32,
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> isize;
    fn CloseHandle(handle: isize) -> i32;
    fn GetProcessTimes(
        handle: isize,
        creation_time: *mut FileTime,
        exit_time: *mut FileTime,
        kernel_time: *mut FileTime,
        user_time: *mut FileTime,
    ) -> i32;
}

#[cfg(windows)]
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
#[cfg(windows)]
const INVALID_HANDLE: isize = -1;

#[cfg(windows)]
fn ft_to_u64(ft: &FileTime) -> u64 {
    ((ft.high as u64) << 32) | (ft.low as u64)
}

/// Windows：进程树 BFS（复用 freezer::tree_pids）+ GetProcessTimes 求和。
#[cfg(windows)]
fn sample_windows(roots: &[u32]) -> u64 {
    use crate::freezer;
    let mut total = 0u64;
    for &pid in freezer::tree_pids(roots) {
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if h == 0 || h == INVALID_HANDLE {
            continue;
        }
        let mut creation = FileTime::default();
        let mut exit = FileTime::default();
        let mut kernel = FileTime::default();
        let mut user = FileTime::default();
        let ok = unsafe {
            GetProcessTimes(h, &mut creation, &mut exit, &mut kernel, &mut user)
        };
        if ok != 0 {
            total += ft_to_u64(&user) + ft_to_u64(&kernel);
        }
        unsafe {
            CloseHandle(h);
        }
    }
    total
}

// =============================== Linux ===============================

/// Linux：遍历 /proc/[pid]/stat，按 pgrp 匹配进程组，求和 utime+stime。
/// 无 pgid 时用 roots 单 pid 兜底。
#[cfg(target_os = "linux")]
fn sample_linux(pgid: Option<u32>, roots: &[u32]) -> u64 {
    let target_pgid = pgid.map(|p| p as u64);
    let mut total = 0u64;
    let rd = match std::fs::read_dir("/proc") {
        Ok(r) => r,
        Err(_) => return 0,
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(pid_str) = name.to_str() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        let Some((pgrp, cpu)) = read_proc_stat(pid) else {
            continue;
        };
        let matches = match target_pgid {
            Some(tp) => pgrp == tp,
            None => roots.contains(&pid),
        };
        if matches {
            total = total.saturating_add(cpu);
        }
    }
    total
}

/// 读 /proc/[pid]/stat → (pgrp, utime+stime)。comm 在 (..) 里可能含空格，
/// 故按最后一个 ')' 定界，之后的字段从 state(字段3) 起按 split 索引：
/// [0]=state [1]=ppid [2]=pgrp ... [11]=utime [12]=stime。
#[cfg(target_os = "linux")]
fn read_proc_stat(pid: u32) -> Option<(u64, u64)> {
    let path = format!("/proc/{pid}/stat");
    let content = std::fs::read_to_string(&path).ok()?;
    let close = content.rfind(')')?;
    let after = &content[close + 1..];
    let fields: Vec<&str> = after.split_whitespace().collect();
    let pgrp: u64 = fields.get(2)?.parse().ok()?;
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some((pgrp, utime + stime))
}

// =============================== macOS ===============================

/// macOS：对 roots（foreground_pids）各 pid proc_pidinfo(PROC_PIDTASKINFO)
/// 取 total_user+total_system（ns）求和。不遍历整个进程组（struct layout
/// 风险，foreground_pids 已是活动主体）。
#[cfg(target_os = "macos")]
fn sample_macos(roots: &[u32]) -> u64 {
    // proc_taskinfo 仅取前 4 个 u64（virtual/resident/total_user/total_system，
    // offset 0/8/16/24），buffersize=32 足以拿到 total_user/system。
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct ProcTaskInfo {
        pti_virtual_size: u64,
        pti_resident_size: u64,
        pti_total_user: u64,
        pti_total_system: u64,
    }
    // PROC_PIDTASKINFO = 4
    const PROC_PIDTASKINFO: u32 = 4;
    extern "C" {
        fn proc_pidinfo(
            pid: i32,
            flavor: u32,
            arg: u64,
            buffer: *mut u8,
            buffersize: i32,
        ) -> i32;
    }
    let mut total = 0u64;
    for &pid in roots {
        let mut info = ProcTaskInfo::default();
        let n = unsafe {
            proc_pidinfo(
                pid as i32,
                PROC_PIDTASKINFO,
                0,
                &mut info as *mut _ as *mut u8,
                std::mem::size_of::<ProcTaskInfo>() as i32,
            )
        };
        if n >= std::mem::size_of::<ProcTaskInfo>() as i32 {
            total = total.saturating_add(info.pti_total_user);
            total = total.saturating_add(info.pti_total_system);
        }
    }
    total
}
