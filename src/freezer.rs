//! 跨平台进程树挂起/恢复。
//!
//! Windows：复刻 work-assistant 的 freezer —— 用 NtSuspendProcess 挂起整棵进程树
//! （CreateToolhelp32Snapshot 遍历父子关系 + BFS），再 K32EmptyWorkingSet 把工作集
//! 页换出 RAM。恢复用 NtResumeProcess。我们只持久化「根 pid + 进程组 id」，恢复时
//! 重新遍历进程树按 pid 再打开句柄解挂——这样守护进程崩溃重启后也能按 pid 恢复。
//!
//! Unix：对 herdr 给出的前台进程组 id 发 SIGSTOP/SIGCONT（挂起整个进程组）。
//! 覆盖 agent 及其同组子孙；跨进程组派生的子孙不在范围内（macOS 无 /proc 树遍历，
//! MVP 以进程组为准；Windows 上是完整树遍历）。
//!
//! 不做 CPU 采样：空闲判定由调用方用 pane revision 轮询完成（用户选定方案）。

#![allow(dead_code)]

pub struct FreezeTarget {
    /// 前台进程 pid 列表（herdr pane.process_info.foreground_processes[].pid）。
    /// 若为空则回退到 shell_pid。Windows 上作为进程树遍历的根；Unix 上作为兜底单 pid。
    pub roots: Vec<u32>,
    /// 前台进程组 id（Unix 用 SIGSTOP 整组挂起；Windows 忽略）。
    pub pgid: Option<u32>,
}

/// 挂起目标。成功返回真正被打开并尝试挂起的 pid 列表（仅供日志）。
pub fn freeze(target: &FreezeTarget) -> Vec<u32> {
    #[cfg(windows)]
    {
        freeze_tree_windows(&target.roots)
    }
    #[cfg(not(windows))]
    {
        freeze_unix(target)
    }
}

/// 恢复目标（幂等：对未挂起进程 resume 也无害）。
pub fn resume(target: &FreezeTarget) -> bool {
    #[cfg(windows)]
    {
        resume_tree_windows(&target.roots)
    }
    #[cfg(not(windows))]
    {
        resume_unix(target)
    }
}

// =============================== Windows ===============================

#[cfg(windows)]
mod windows_impl {
    use std::collections::{HashMap, VecDeque};

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[repr(C)]
    struct ProcessEntry32W {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u16; 260],
    }

    const PROCESS_SUSPEND_RESUME: u32 = 0x0800;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const PROCESS_SET_QUOTA: u32 = 0x0100;
    const TH32CS_SNAPPROCESS: u32 = 0x2;
    const INVALID_HANDLE: isize = -1;

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> isize;
        fn CloseHandle(handle: isize) -> i32;
        fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> isize;
        fn Process32FirstW(snapshot: isize, entry: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(snapshot: isize, entry: *mut ProcessEntry32W) -> i32;
        fn K32EmptyWorkingSet(handle: isize) -> i32;
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn NtSuspendProcess(handle: isize) -> i32;
        fn NtResumeProcess(handle: isize) -> i32;
    }

    /// 遍历整棵进程树：以 roots 为起点，BFS 收集所有后代 pid。
    pub(crate) fn tree_pids(roots: &[u32]) -> Vec<u32> {
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == 0 || snap == INVALID_HANDLE {
                return roots.to_vec();
            }
            let mut parent_children: HashMap<u32, Vec<u32>> = HashMap::new();
            let mut entry: ProcessEntry32W = std::mem::zeroed();
            entry.dw_size = std::mem::size_of::<ProcessEntry32W>() as u32;
            if Process32FirstW(snap, &mut entry) != 0 {
                loop {
                    parent_children
                        .entry(entry.th32_parent_process_id)
                        .or_default()
                        .push(entry.th32_process_id);
                    if Process32NextW(snap, &mut entry) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snap);

            let mut seen = std::collections::HashSet::new();
            let mut result: Vec<u32> = Vec::new();
            let mut queue: VecDeque<u32> = VecDeque::new();
            for &root in roots {
                if seen.insert(root) {
                    result.push(root);
                    queue.push_back(root);
                }
            }
            while let Some(pid) = queue.pop_front() {
                if let Some(children) = parent_children.get(&pid) {
                    for &child in children {
                        if seen.insert(child) {
                            result.push(child);
                            queue.push_back(child);
                        }
                    }
                }
            }
            result
        }
    }

    /// 挂起 roots 的整棵进程树并裁剪工作集。返回被挂起的 pid（去重）。
    pub fn freeze_tree(roots: &[u32]) -> Vec<u32> {
        if roots.is_empty() {
            return Vec::new();
        }
        let access = PROCESS_SUSPEND_RESUME | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_QUOTA;
        let mut suspended: Vec<u32> = Vec::new();
        let mut suspended_set: std::collections::HashSet<u32> = std::collections::HashSet::new();

        // 挂起后重新遍历几轮，把采样窗口期间派生的新子孙一并挂起（竞态修复，
        // 复刻 work-assistant）。
        for _ in 0..5 {
            let mut new_any = false;
            for pid in tree_pids(roots) {
                if suspended_set.contains(&pid) {
                    continue;
                }
                let h = unsafe { OpenProcess(access, 0, pid) };
                if h == 0 || h == INVALID_HANDLE {
                    continue;
                }
                let status = unsafe { NtSuspendProcess(h) };
                if status >= 0 {
                    suspended_set.insert(pid);
                    suspended.push(pid);
                    unsafe { K32EmptyWorkingSet(h) };
                    new_any = true;
                }
                unsafe { CloseHandle(h) };
            }
            if !new_any {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        suspended
    }

    /// 恢复 roots 的整棵进程树（重新遍历当前树，对每个 pid 再开句柄解挂）。
    pub fn resume_tree(roots: &[u32]) -> bool {
        let mut any = false;
        for pid in tree_pids(roots) {
            let h = unsafe { OpenProcess(PROCESS_SUSPEND_RESUME, 0, pid) };
            if h == 0 || h == INVALID_HANDLE {
                continue;
            }
            let status = unsafe { NtResumeProcess(h) };
            if status >= 0 {
                any = true;
            }
            unsafe { CloseHandle(h) };
        }
        any
    }
}

#[cfg(windows)]
fn freeze_tree_windows(roots: &[u32]) -> Vec<u32> {
    windows_impl::freeze_tree(roots)
}

#[cfg(windows)]
fn resume_tree_windows(roots: &[u32]) -> bool {
    windows_impl::resume_tree(roots)
}

/// 进程树 pid 列表（Windows BFS；macOS 用 proc_listpids(PROC_PPID_ONLY) 递归
/// BFS 覆盖跨进程组派生的子孙如 nvim --embed 子进程；其他 Unix 返回 roots
/// 本身，靠进程组 SIGSTOP 覆盖同组进程）。供 freezer + cpu_sample 复用。
pub(crate) fn tree_pids(roots: &[u32]) -> Vec<u32> {
    #[cfg(windows)]
    {
        windows_impl::tree_pids(roots)
    }
    #[cfg(target_os = "macos")]
    {
        unix_impl::tree_pids_macos(roots)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        roots.to_vec()
    }
}

// =============================== Unix ===============================

#[cfg(not(windows))]
mod unix_impl {
    // 不引 libc crate：直接声明 kill 与信号常量。
    type CInt = i32;

    extern "C" {
        fn kill(pid: CInt, sig: CInt) -> CInt;
    }

    #[cfg(target_os = "linux")]
    const SIGSTOP: CInt = 19;
    #[cfg(target_os = "linux")]
    const SIGCONT: CInt = 18;
    #[cfg(target_os = "macos")]
    const SIGSTOP: CInt = 17;
    #[cfg(target_os = "macos")]
    const SIGCONT: CInt = 19;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    const SIGSTOP: CInt = 19;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    const SIGCONT: CInt = 18;

    /// 对进程组发 SIGSTOP（pgid 取负），整组挂起。
    pub fn freeze_group(pgid: u32) -> bool {
        let pgid = pgid as CInt;
        if pgid <= 0 {
            return false;
        }
        unsafe { kill(-pgid, SIGSTOP) == 0 }
    }

    /// 对进程组发 SIGCONT，整组恢复。
    pub fn resume_group(pgid: u32) -> bool {
        let pgid = pgid as CInt;
        if pgid <= 0 {
            return false;
        }
        unsafe { kill(-pgid, SIGCONT) == 0 }
    }

    /// 单 pid 兜底（无进程组时）。
    pub fn freeze_pid(pid: u32) -> bool {
        unsafe { kill(pid as CInt, SIGSTOP) == 0 }
    }

    pub fn resume_pid(pid: u32) -> bool {
        unsafe { kill(pid as CInt, SIGCONT) == 0 }
    }

    // ------------------------- macOS 进程树遍历 -------------------------

    #[cfg(target_os = "macos")]
    extern "C" {
        fn proc_listpids(type_: u32, typeinfo: u32, buffer: *mut u8, buffersize: i32) -> i32;
    }

    /// proc_listpids 的 type：PROC_PPID_ONLY=6（typeinfo 传 ppid，列出其直接子进程）。
    #[cfg(target_os = "macos")]
    const PROC_PPID_ONLY: u32 = 6;

    /// macOS：列出 ppid 的直接子进程 pid（proc_listpids PROC_PPID_ONLY）。
    #[cfg(target_os = "macos")]
    fn child_pids(ppid: u32) -> Vec<u32> {
        let needed = unsafe { proc_listpids(PROC_PPID_ONLY, ppid, std::ptr::null_mut(), 0) };
        if needed <= 0 {
            return Vec::new();
        }
        let cap = (needed as usize) + 64;
        let mut buf = vec![0u8; cap];
        let n = unsafe { proc_listpids(PROC_PPID_ONLY, ppid, buf.as_mut_ptr(), buf.len() as i32) };
        if n <= 0 {
            return Vec::new();
        }
        let bytes = (n as usize).min(buf.len());
        let count = bytes / 4;
        (0..count)
            .map(|i| {
                let off = i * 4;
                u32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
            })
            .collect()
    }

    /// macOS 进程树 BFS：从 roots 收集所有子孙 pid（含跨进程组派生的子孙，
    /// 如 nvim --embed 子进程自己 setpgd，进程组 SIGSTOP 停不到）。覆盖 freeze/
    /// resume/采样，避免主进程停了、--embed 子进程还在跑导致状态不同步退出。
    #[cfg(target_os = "macos")]
    pub(crate) fn tree_pids_macos(roots: &[u32]) -> Vec<u32> {
        use std::collections::{HashSet, VecDeque};
        let mut seen = HashSet::new();
        let mut result = Vec::new();
        let mut queue: VecDeque<u32> = VecDeque::new();
        for &r in roots {
            if seen.insert(r) {
                result.push(r);
                queue.push_back(r);
            }
        }
        while let Some(pid) = queue.pop_front() {
            for child in child_pids(pid) {
                if seen.insert(child) {
                    result.push(child);
                    queue.push_back(child);
                }
            }
        }
        result
    }
}

#[cfg(not(windows))]
fn freeze_unix(target: &FreezeTarget) -> Vec<u32> {
    // 进程树遍历（macOS = 完整树含跨进程组子孙；其他 Unix = roots 本身）。
    // 覆盖 nvim --embed 等子进程自己 setpgd 的情况（进程组 SIGSTOP 停不到，
    // 导致主进程停了子进程还在跑、状态不同步退出）。
    let pids = tree_pids(&target.roots);
    let mut out = Vec::new();
    for &pid in &pids {
        if unix_impl::freeze_pid(pid) {
            out.push(pid);
        }
    }
    // 进程组兜底（同组非子孙兄弟，少见但幂等无害）
    if let Some(pgid) = target.pgid {
        let _ = unix_impl::freeze_group(pgid);
    }
    out
}

#[cfg(not(windows))]
fn resume_unix(target: &FreezeTarget) -> bool {
    let pids = tree_pids(&target.roots);
    let mut any = false;
    for &pid in &pids {
        any |= unix_impl::resume_pid(pid);
    }
    if let Some(pgid) = target.pgid {
        any |= unix_impl::resume_group(pgid);
    }
    any
}
