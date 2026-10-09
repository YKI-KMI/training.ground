//! Focus mode: enumerate processes, protect critical ones, close/suspend/trim on request.
use serde::Serialize;
use std::collections::HashSet;
use sysinfo::{ProcessesToUpdate, System};

/// Names (without .exe) that are never touchable: Ollama, OS core, and the dev tools
/// this training ground depends on. Browsers are deliberately NOT protected:
/// they are the primary RAM target.
const ALWAYS_PROTECTED: &[&str] = &[
    "ollama", "ollama app", "explorer", "dwm", "csrss", "svchost", "services", "lsass",
    "winlogon", "sihost", "taskhostw", "searchhost", "textinputhost", "startmenuexperiencehost",
    "shellexperiencehost", "runtimebroker", "systemsettings", "conhost", "fontdrvhost", "smss",
    "wininit", "lsm", "spoolsv", "ctfmon", "taskmgr", "msmpeng", "securityhealthservice",
    "code", "cursor", "windowsterminal", "pwsh", "powershell", "bash", "python", "pythonw",
    "node", "git", "docker", "docker desktop", "java", "javaw", "cargo", "rustc",
    "rust-analyzer", "gcc", "g++", "clang",
];

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    pub mem_mb: u64,
    pub exe: String,
    pub protected: bool,
    /// "you" | "always" | "system" | "extra" | ""
    pub reason: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemInfo {
    pub total_mb: u64,
    pub used_mb: u64,
    pub available_mb: u64,
}

fn stem(name: &str) -> String {
    let n = name.trim().to_lowercase();
    n.strip_suffix(".exe").unwrap_or(&n).to_string()
}

/// Pure classification so it can be unit-tested.
fn classify(name: &str, exe: &str, pid: u32, self_pids: &HashSet<u32>, extra: &[String]) -> &'static str {
    if pid == 0 || pid == 4 {
        return "system";
    }
    if self_pids.contains(&pid) {
        return "you";
    }
    let st = stem(name);
    if ALWAYS_PROTECTED.iter().any(|a| *a == st.as_str()) {
        return "always";
    }
    let exe_l = exe.replace('/', "\\").to_lowercase();
    if exe_l.contains(r"\windows\") {
        return "system";
    }
    if extra.iter().filter(|e| !e.trim().is_empty()).any(|e| stem(e) == st) {
        return "extra";
    }
    ""
}

fn self_and_ancestors(sys: &System) -> HashSet<u32> {
    let mut pids = HashSet::new();
    let mut cur = Some(std::process::id());
    for _ in 0..5 {
        match cur.and_then(|id| sys.process(sysinfo::Pid::from_u32(id))) {
            Some(p) => {
                pids.insert(p.pid().as_u32());
                cur = p.parent().map(|pp| pp.as_u32());
            }
            None => break,
        }
    }
    pids
}

#[tauri::command(async)]
pub fn list_processes(extra: Vec<String>) -> Result<Vec<ProcInfo>, String> {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    let self_pids = self_and_ancestors(&sys);
    let mut out: Vec<ProcInfo> = Vec::new();
    for (pid, p) in sys.processes() {
        let pid_u = pid.as_u32();
        if pid_u == 0 || pid_u == 4 {
            continue;
        }
        let name = p.name().to_string_lossy().into_owned();
        if name.is_empty() {
            continue;
        }
        let exe = p
            .exe()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        let reason = classify(&name, &exe, pid_u, &self_pids, &extra);
        out.push(ProcInfo {
            pid: pid_u,
            name,
            mem_mb: p.memory() / (1024 * 1024),
            exe,
            protected: !reason.is_empty(),
            reason: reason.to_string(),
        });
    }
    out.sort_by(|a, b| b.mem_mb.cmp(&a.mem_mb));
    out.truncate(80);
    Ok(out)
}

#[tauri::command(async)]
pub fn memory_snapshot() -> Result<MemInfo, String> {
    let mut sys = System::new();
    sys.refresh_memory();
    Ok(MemInfo {
        total_mb: sys.total_memory() / (1024 * 1024),
        used_mb: sys.used_memory() / (1024 * 1024),
        available_mb: sys.available_memory() / (1024 * 1024),
    })
}

/// Returns (name, exe) for one pid, refreshing only that process.
fn lookup(pid: u32) -> Result<(String, String), String> {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]), true);
    let p = sys
        .process(sysinfo::Pid::from_u32(pid))
        .ok_or_else(|| "process not found (it may have exited)".to_string())?;
    Ok((
        p.name().to_string_lossy().into_owned(),
        p.exe()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default(),
    ))
}

/// Server-side protection re-check before any destructive action.
fn guard(pid: u32, extra: Vec<String>) -> Result<(), String> {
    let (name, exe) = lookup(pid)?;
    let self_pids = self_and_ancestors(&System::new());
    let reason = classify(&name, &exe, pid, &self_pids, &extra);
    if reason.is_empty() {
        Ok(())
    } else {
        Err(format!("protected process ({reason}): {name}"))
    }
}

#[tauri::command(async)]
pub fn close_process(pid: u32, extra: Vec<String>) -> Result<bool, String> {
    guard(pid, extra)?;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h.is_null() {
            return Err("OpenProcess failed (access denied)".into());
        }
        let ok = TerminateProcess(h, 1);
        CloseHandle(h);
        if ok == 0 { Err("TerminateProcess failed".into()) } else { Ok(true) }
    }
}

/// Suspend every thread. NOTE: frees CPU, not RAM.
#[tauri::command(async)]
pub fn suspend_process(pid: u32, extra: Vec<String>) -> Result<u32, String> {
    guard(pid, extra)?;
    thread_action(pid, true)
}

#[tauri::command(async)]
pub fn resume_process(pid: u32, extra: Vec<String>) -> Result<u32, String> {
    guard(pid, extra)?;
    thread_action(pid, false)
}

fn thread_action(pid: u32, suspend: bool) -> Result<u32, String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, SuspendThread, THREAD_SUSPEND_RESUME};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, THREADENTRY32, TH32CS_SNAPTHREAD,
    };
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap.is_null() {
            return Err("thread snapshot failed".into());
        }
        let mut te: THREADENTRY32 = std::mem::zeroed();
        te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut count = 0u32;
        if Thread32First(snap, &mut te) != 0 {
            loop {
                if te.th32OwnerProcessID == pid {
                    let th = OpenThread(THREAD_SUSPEND_RESUME, 0, te.th32ThreadID);
                    if !th.is_null() {
                        let r = if suspend { SuspendThread(th) } else { ResumeThread(th) };
                        if r != u32::MAX {
                            count += 1;
                        }
                        CloseHandle(th);
                    }
                }
                if Thread32Next(snap, &mut te) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        Ok(count)
    }
}

/// Page out the process working set: soft, reversible nudge.
#[tauri::command(async)]
pub fn trim_process(pid: u32, extra: Vec<String>) -> Result<bool, String> {
    guard(pid, extra)?;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_SET_QUOTA};
    #[link(name = "psapi")]
    extern "system" {
        fn EmptyWorkingSet(h: *mut std::ffi::c_void) -> i32;
    }
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_SET_QUOTA, 0, pid);
        if h.is_null() {
            return Err("OpenProcess failed".into());
        }
        let ok = EmptyWorkingSet(h);
        CloseHandle(h);
        if ok == 0 { Err("EmptyWorkingSet failed".into()) } else { Ok(true) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_is_safe() {
        let mut me = HashSet::new();
        me.insert(1234u32);
        let extra = vec!["weird.exe".to_string()];
        let cases = [
            (1234u32, "chrome.exe", r"C:\x\chrome.exe", "you"),
            (9, "ollama app.exe", r"C:\o\ollama app.exe", "always"),
            (9, "chrome.exe", r"C:\x\chrome.exe", ""),
            (9, "svchost.exe", r"C:\Windows\System32\svchost.exe", "always"),
            (4, "System", "", "system"),
            (9, "weird.exe", r"D:\tools\weird.exe", "extra"),
            (9, "notepad.exe", r"C:\Windows\System32\notepad.exe", "system"),
        ];
        for (pid, name, exe, want) in cases {
            assert_eq!(classify(name, exe, pid, &me, &extra), want, "pid={pid} name={name}");
        }
    }
}
