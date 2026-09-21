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
//! - macOS：pgid 给出时用 `proc_listpids(PROC_PGRP_ONLY, pgid)` 列出该
//!   进程组全部 pid，逐 pid `proc_pidinfo(PROC_PIDTASKINFO)` 取
//!   total_user+total_system（ns）求和（与 Linux `/proc` by pgrp 等价，
//!   覆盖 shell 子进程 vim/opencode/cargo build 等同组进程）；
//!   pgid=None 时用 roots 逐 pid 兜底。不走 `PROC_PIDTBSDINFO`——
//!   内核直接按 pgid 过滤，无需逐 pid 取 pbi_pgid（避开 struct layout 风险）。
//!
//! busy_ratio：t0→sleep window→t1，delta 归一化为「一核的占比」
//! （0.0=0 核，1.0=满载一核），阈值 >0.10 即 10% 一核，与 work-assistant
//! 的 FREEZE_CPU_BUSY_RATIO 一致、跨平台可比。

use std::time::Duration;

use crate::freeze_dbg;

/// 平台内一致的 CPU 时间累加值（单位随平台：Windows=100ns ticks，
/// Linux=jiffies，macOS=ns）。仅需同平台内 delta>0 判活动 + busy_ratio 归一化。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTime(pub u64);

/// 采样进程组/树的总 CPU 时间。
/// Windows：roots 作为树根 BFS（pgid 忽略）；Linux：pgid 匹配进程组
/// （无 pgid 用 roots 单 pid）；macOS：pgid 给出时按进程组遍历
/// （proc_listpids PROC_PGRP_ONLY，与 Linux 等价），否则 roots 逐 pid。
pub fn sample(roots: &[u32], pgid: Option<u32>) -> CpuTime {
    let _ = pgid; // Windows 分支不用 pgid（Linux/macOS 用），此处统一消警告
    #[cfg(windows)]
    {
        let total = sample_windows(roots);
        freeze_dbg!("sample(Windows) roots={:?} → {}", roots, total);
        CpuTime(total)
    }
    #[cfg(target_os = "linux")]
    {
        let total = sample_linux(pgid, roots);
        freeze_dbg!("sample(Linux) pgid={:?} roots={:?} → {}", pgid, roots, total);
        CpuTime(total)
    }
    #[cfg(target_os = "macos")]
    {
        let total = sample_macos(roots, pgid);
        freeze_dbg!("sample(macOS) roots={:?} pgid={:?} → {}", roots, pgid, total);
        CpuTime(total)
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

/// 两次采样的 delta 折算成一核占比（0.0=0 核，1.0=满载一核），用于 active 判定。
/// duration 是两次采样的时间间隔。比 `cur != prev`（任何 delta>0 都算活跃）更
/// 合理：低 CPU 后台活动（如 agent LSP/心跳 0.65%）低于阈值判空闲可冻，
/// 真工作（如 cargo build 50%）高于阈值判活跃不冻。
pub(crate) fn delta_cores(delta: u64, duration: Duration) -> f64 {
    cores_fraction(delta, duration.as_millis() as u64)
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
    for pid in freezer::tree_pids(roots) {
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

/// macOS：进程树遍历采样（`proc_listpids(PROC_PPID_ONLY)` 递归 BFS）。
///
/// 覆盖 roots 的所有子孙，**含跨进程组派生的子孙**（如 nvim `--embed`
/// 子进程自己 setpgd，进程组 pgid 遍历采不到它的 CPU，会导致主进程空闲
/// 但 --embed 在跑时误判空闲冻结）。比 pgid 进程组覆盖更全，且与
/// freeze/resume 的 tree_pids 同源，采样与冻结范围一致。
/// 逐 pid `PROC_PIDTASKINFO` 取 total_user+total_system（ns）求和。
#[cfg(target_os = "macos")]
fn sample_macos(roots: &[u32], _pgid: Option<u32>) -> u64 {
    let pids = crate::freezer::tree_pids(roots);
    let mut total = 0u64;
    for &pid in &pids {
        total = total.saturating_add(pid_cpu_time(pid));
    }
    freeze_dbg!(
        "sample_macOS roots={:?} tree_pids={} total={}",
        roots,
        pids.len(),
        total
    );
    total
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ProcTaskInfo {
    pti_virtual_size: u64,
    pti_resident_size: u64,
    pti_total_user: u64,
    pti_total_system: u64,
    pti_threads_user: u64,
    pti_threads_system: u64,
    pti_policy: i32,
    pti_faults: i32,
    pti_pageins: i32,
    pti_cow_faults: i32,
    pti_messages_sent: i32,
    pti_messages_received: i32,
    pti_syscalls_mach: i32,
    pti_syscalls_unix: i32,
    pti_csw: i32,
    pti_threadnum: i32,
    pti_numrunning: i32,
    pti_priority: i32,
}

/// PROC_PIDTASKINFO = 4（proc_info.h）。
#[cfg(target_os = "macos")]
const PROC_PIDTASKINFO: u32 = 4;

#[cfg(target_os = "macos")]
extern "C" {
    fn proc_pidinfo(
        pid: i32,
        flavor: u32,
        arg: u64,
        buffer: *mut u8,
        buffersize: i32,
    ) -> i32;
}

/// 单 pid 的 CPU 时间（pti_total_user + pti_total_system，ns）。
/// proc_pidinfo 失败/缓冲不足返回 0。
#[cfg(target_os = "macos")]
fn pid_cpu_time(pid: u32) -> u64 {
    let mut info = ProcTaskInfo::default();
    let need = std::mem::size_of::<ProcTaskInfo>() as i32;
    let n = unsafe {
        proc_pidinfo(
            pid as i32,
            PROC_PIDTASKINFO,
            0,
            &mut info as *mut _ as *mut u8,
            need,
        )
    };
    if n >= need {
        info.pti_total_user.saturating_add(info.pti_total_system)
    } else {
        freeze_dbg!("pid_cpu_time pid={} 失败 n={} need={}", pid, n, need);
        0
    }
}
