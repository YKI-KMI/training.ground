//! Focus mode: enumerate processes, protect critical ones, close/suspend/trim on request.
use crate::{db, AppState};
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

/// Deep Focus: one aggressive sweep over every non-essential user process —
/// bloatware updaters, background apps, anything not protected — with the
/// user's shell as an explicit extra. The sweep is *reversible*: `suspend`
/// pauses processes (instant restore, RAM kept), `close` ends them (RAM freed,
/// relaunched on restore). Every action lands in the `focus_session` table, so
/// the restore list survives an app restart — no reboot required.

/// Keywords marking background junk: updaters, vendor helpers, agents.
fn is_bloat(name: &str) -> bool {
    let st = stem(name);
    const KEYS: &[&str] = &[
        "update", "updater", "acrotray", "adobe", "onedrive", "officeclicktorun",
        "officec2rclient", "msedgeupdate", "msedgemetadata", "googleupdate",
        "crashreporter", "telemetry", "agent", "helper", "gamingservices",
        "teamviewer", "anydesk", "pipinstall", "dispatcher",
    ];
    KEYS.iter().any(|k| st.contains(k))
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DeepPlanItem {
    pub pid: u32,
    pub name: String,
    pub mem_mb: u64,
    pub exe: String,
    /// "bloat" | "app" | "shell"
    pub kind: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepFocusPlan {
    pub items: Vec<DeepPlanItem>,
    pub total_mb: u64,
    pub bloat_count: usize,
    pub protected_count: usize,
    pub shell: Option<DeepPlanItem>,
    pub available_mb: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepFailItem {
    pub pid: u32,
    pub name: String,
    pub err: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepFocusResult {
    pub closed: u32,
    pub suspended: u32,
    pub failed: Vec<DeepFailItem>,
    pub freed_mb: i64,
    pub available_mb: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FocusSessionEntry {
    pub pid: u32,
    pub name: String,
    pub exe: String,
    /// "suspended" | "closed"
    pub mode: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FocusSession {
    pub active: bool,
    pub entries: Vec<FocusSessionEntry>,
    pub suspended: usize,
    pub closed: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreResult {
    pub resumed: u32,
    pub relaunched: u32,
    pub failed: Vec<DeepFailItem>,
    pub available_mb: u64,
}

fn deep_item(pid: u32, name: &str, exe: &str, mem: u64, kind: &str) -> DeepPlanItem {
    DeepPlanItem {
        pid,
        name: name.to_string(),
        mem_mb: mem / (1024 * 1024),
        exe: exe.to_string(),
        kind: kind.to_string(),
    }
}

#[tauri::command(async)]
pub fn deep_focus_scan(extra: Vec<String>) -> Result<DeepFocusPlan, String> {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    sys.refresh_memory();
    let self_pids = self_and_ancestors(&sys);
    let mut items: Vec<DeepPlanItem> = Vec::new();
    let mut shell: Option<DeepPlanItem> = None;
    let mut protected_count = 0usize;
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
        let mem = p.memory();
        if !reason.is_empty() {
            protected_count += 1;
            // The shell is "always"-protected for the checkbox UI, but Deep
            // Focus may close it at the user's explicit request.
            if reason == "always" && stem(&name) == "explorer" {
                let _ = shell.insert(deep_item(pid_u, &name, &exe, mem, "shell"));
            }
            continue;
        }
        let kind = if is_bloat(&name) { "bloat" } else { "app" };
        items.push(deep_item(pid_u, &name, &exe, mem, kind));
    }
    // Grouped view: user apps with their own windows first, bloat visible, big RAM first.
    items.sort_by(|a, b| {
        (is_bloat(&b.name).cmp(&is_bloat(&a.name))).then(b.mem_mb.cmp(&a.mem_mb))
    });
    let total_mb = items.iter().map(|i| i.mem_mb).sum();
    Ok(DeepFocusPlan {
        bloat_count: items.iter().filter(|i| i.kind == "bloat").count(),
        items,
        total_mb,
        protected_count,
        shell,
        available_mb: sys.available_memory() / (1024 * 1024),
    })
}

fn terminate(pid: u32) -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h.is_null() {
            return Err("OpenProcess failed".into());
        }
        let ok = TerminateProcess(h, 1);
        CloseHandle(h);
        if ok == 0 { Err("TerminateProcess failed".into()) } else { Ok(()) }
    }
}

/// Canonical form for deduping executables across a restore list.
fn norm_exe(exe: &str) -> String {
    exe.trim().replace('/', "\\").to_lowercase()
}

/// Start a closed process again, detached from this app so it outlives it.
fn relaunch(exe: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    std::process::Command::new(exe)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The live sweep. Records every action so `focus_restore` can undo it.
#[tauri::command(async)]
pub fn deep_focus_apply(
    pids: Vec<u32>,
    extra: Vec<String>,
    include_shell: bool,
    mode: String,
    state: tauri::State<'_, AppState>,
) -> Result<DeepFocusResult, String> {
    let suspend_mode = mode.eq_ignore_ascii_case("suspend");
    let mut sys = System::new();
    sys.refresh_memory();
    let before = sys.available_memory() / (1024 * 1024);
    let self_pids = self_and_ancestors(&System::new());
    let mut closed = 0u32;
    let mut suspended = 0u32;
    let mut failed: Vec<DeepFailItem> = Vec::new();
    let mut records: Vec<(u32, String, String, String)> = Vec::new();
    for pid in pids {
        let (name, exe) = match lookup(pid) {
            Ok(v) => v,
            Err(_) => continue, // already exited: nothing to record
        };
        let is_shell = stem(&name) == "explorer";
        let reason = classify(&name, &exe, pid, &self_pids, &extra);
        if !reason.is_empty() && !(is_shell && reason == "always" && include_shell) {
            failed.push(DeepFailItem { pid, name, err: format!("protected ({reason})") });
            continue;
        }
        let done = if suspend_mode { thread_action(pid, true).map(|_| ()) } else { terminate(pid) };
        match done {
            Ok(()) => {
                if suspend_mode {
                    suspended += 1;
                } else {
                    closed += 1;
                }
                records.push((pid, name, exe, mode.clone()));
            }
            Err(err) => failed.push(DeepFailItem { pid, name, err }),
        }
    }
    {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        db::session_clear(&db)?;
        for (pid, name, exe, m) in &records {
            db::session_add(&db, *pid, name, exe, m)?;
        }
    }
    if !suspend_mode {
        std::thread::sleep(std::time::Duration::from_millis(600)); // let Windows reclaim
    }
    let mut sys2 = System::new();
    sys2.refresh_memory();
    let after = sys2.available_memory() / (1024 * 1024);
    Ok(DeepFocusResult {
        closed,
        suspended,
        failed,
        freed_mb: after as i64 - before as i64,
        available_mb: after,
    })
}

#[tauri::command(async)]
pub fn focus_session(state: tauri::State<'_, AppState>) -> Result<FocusSession, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let entries: Vec<FocusSessionEntry> = db::session_entries(&db)?
        .into_iter()
        .map(|(pid, name, exe, mode)| FocusSessionEntry { pid, name, exe, mode })
        .collect();
    let suspended = entries.iter().filter(|e| e.mode == "suspended").count();
    let closed = entries.iter().filter(|e| e.mode == "closed").count();
    Ok(FocusSession { active: !entries.is_empty(), entries, suspended, closed })
}

/// Undo a sweep: resume suspended entries (pid-reuse safe) and relaunch closed
/// ones, once per executable, skipping anything already running.
#[tauri::command(async)]
pub fn focus_restore(state: tauri::State<'_, AppState>) -> Result<RestoreResult, String> {
    let entries: Vec<FocusSessionEntry> = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        db::session_entries(&db)?
            .into_iter()
            .map(|(pid, name, exe, mode)| FocusSessionEntry { pid, name, exe, mode })
            .collect()
    };
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    // Everything alive right now, by lowercased exe path and by name stem.
    let mut running: HashSet<String> = HashSet::new();
    for p in sys.processes().values() {
        running.insert(p.name().to_string_lossy().to_lowercase());
        if let Some(e) = p.exe() {
            running.insert(norm_exe(&e.to_string_lossy()));
        }
    }
    let mut resumed = 0u32;
    let mut relaunched = 0u32;
    let mut failed: Vec<DeepFailItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for e in entries.iter().filter(|e| e.mode == "suspended") {
        let (pid, name) = (e.pid, e.name.clone());
        let still_same = matches!(lookup(pid), Ok((n, _)) if stem(&n) == stem(&name));
        if !still_same {
            continue; // exited or the pid was reused: nothing to resume
        }
        match thread_action(pid, false) {
            Ok(_) => resumed += 1,
            Err(err) => failed.push(DeepFailItem { pid, name, err }),
        }
    }
    for e in entries.iter().filter(|e| e.mode == "closed") {
        let (pid, name) = (e.pid, e.name.clone());
        if e.exe.trim().is_empty() {
            failed.push(DeepFailItem { pid, name, err: "no executable path".into() });
            continue;
        }
        let key = norm_exe(&e.exe);
        if !seen.insert(key.clone()) || running.contains(&key) {
            continue; // duplicate window or already back up
        }
        match relaunch(&e.exe) {
            Ok(()) => {
                relaunched += 1;
                running.insert(key);
            }
            Err(err) => failed.push(DeepFailItem { pid, name, err }),
        }
    }
    {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        db::session_clear(&db)?;
    }
    let mut sys2 = System::new();
    sys2.refresh_memory();
    Ok(RestoreResult {
        resumed,
        relaunched,
        failed,
        available_mb: sys2.available_memory() / (1024 * 1024),
    })
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

    #[test]
    fn bloat_detection_is_conservative() {
        assert!(is_bloat("Adobe_Updater.exe"));
        assert!(is_bloat("GoogleUpdate.exe"));
        assert!(is_bloat("oneDrive-standalone"));
        assert!(is_bloat("OfficeClickToRun.exe"));
        assert!(!is_bloat("chrome.exe"));
        assert!(!is_bloat("code.exe"));
        assert!(!is_bloat("python.exe"));
    }

    #[test]
    fn deep_focus_never_includes_protected() {
        // A protected process must never appear in the deep-focus plan items;
        // only explorer gets the separate opt-in `shell` slot.
        let mut me = HashSet::new();
        me.insert(77u32);
        let extra: Vec<String> = vec![];
        let cases = [
            ("svchost.exe", r"C:\Windows\System32\svchost.exe", "always"),
            ("explorer.exe", r"C:\Windows\explorer.exe", "always"),
            ("chrome.exe", r"C:\x\chrome.exe", ""),
        ];
        for (name, exe, want) in cases {
            assert_eq!(classify(name, exe, 9, &me, &extra), want, "{name}");
        }
        // And the close-side guard: protected names rejected, shell handled.
        let protected_call = classify("explorer.exe", r"C:\Windows\explorer.exe", 9, &me, &extra);
        assert_eq!(protected_call, "always");
        assert_eq!(stem("explorer.exe"), "explorer");
    }
}
