//! Sandbox runner: executes learner code with a Job Object memory cap, timeout,
//! capped output capture, and interpreter auto-detection. The sandbox result —
//! never the model — is the ground truth for pass/fail.
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::PathBuf;
use std::os::windows::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const COMPILE_TIMEOUT_MS: u64 = 30_000;
const MAX_OUTPUT: usize = 128 * 1024; // keep the last 128 KB of each stream
const MEM_CAP: usize = 256 * 1024 * 1024; // 256 MB job limit
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRequest {
    pub language: String,
    pub code: String,
    #[serde(default)]
    pub tests: Option<String>,
    #[serde(default)]
    pub stdin: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub duration_ms: u64,
    pub interpreter: String,
}

/// A Windows Job Object that caps process memory and kills the child if we die.
struct Job(*mut std::ffi::c_void);

impl Drop for Job {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        unsafe { CloseHandle(self.0) };
    }
}

fn make_job() -> Option<Job> {
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, SetInformationJobObject, JobObjectExtendedLimitInformation,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        info.ProcessMemoryLimit = MEM_CAP;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            use windows_sys::Win32::Foundation::CloseHandle;
            CloseHandle(job);
            return None;
        }
        Some(Job(job))
    }
}

/// Put a spawned pid into our job. Failure is non-fatal (timeout still protects us).
fn assign_to_job(job: &Job, pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};
    unsafe {
        let h = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if h.is_null() {
            return false;
        }
        let ok = AssignProcessToJobObject(job.0, h);
        CloseHandle(h);
        ok != 0
    }
}

struct Plan {
    src_name: &'static str,
    compile: Option<Vec<String>>,
    runs: Vec<Vec<String>>,
}

fn build_plan(lang: &str, dir: &std::path::Path) -> Result<Plan, String> {
    let f = |n: &str| dir.join(n).to_string_lossy().into_owned();
    Ok(match lang {
        "python" | "py" => Plan {
            src_name: "main.py",
            compile: None,
            runs: vec![
                vec!["python".into(), f("main.py")],
                vec!["py".into(), "-3".into(), f("main.py")],
                vec!["python3".into(), f("main.py")],
            ],
        },
        "javascript" | "js" => Plan {
            src_name: "main.js",
            compile: None,
            runs: vec![vec!["node".into(), f("main.js")]],
        },
        "c" => Plan {
            src_name: "main.c",
            compile: Some(vec!["gcc".into(), f("main.c"), "-O0".into(), "-o".into(), f("main.exe")]),
            runs: vec![vec![f("main.exe")]],
        },
        "cpp" | "c++" => Plan {
            src_name: "main.cpp",
            compile: Some(vec!["g++".into(), f("main.cpp"), "-O0".into(), "-o".into(), f("main.exe")]),
            runs: vec![vec![f("main.exe")]],
        },
        "rust" => Plan {
            src_name: "main.rs",
            compile: Some(vec![
                "rustc".into(), "--edition".into(), "2021".into(), "-o".into(), f("main.exe"), f("main.rs"),
            ]),
            runs: vec![vec![f("main.exe")]],
        },
        "java" => Plan {
            src_name: "Main.java",
            compile: Some(vec!["javac".into(), f("Main.java")]),
            runs: vec![vec!["java".into(), "-cp".into(), dir.to_string_lossy().into_owned(), "Main".into()]],
        },
        other => return Err(format!("unsupported language: {other}")),
    })
}

/// Insert tests at the model-provided marker; otherwise append. The marker rule
/// lets compiled languages (java/c/rust) place tests inside main().
fn inject(program: &str, tests: &str) -> String {
    for marker in ["//TESTS", "#TESTS", "// TESTS", "# TESTS"] {
        if let Some(idx) = program.find(marker) {
            return format!("{}{}{}", &program[..idx], tests, &program[idx + marker.len()..]);
        }
    }
    format!("{program}\n\n{tests}")
}

struct Exec {
    status: Option<ExitStatus>,
    stdout: String,
    stderr: String,
    timed_out: bool,
    ms: u64,
}

fn pump<R: Read + Send + 'static>(mut r: R) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut stored: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    stored.extend_from_slice(&chunk[..n]);
                    if stored.len() > MAX_OUTPUT {
                        let excess = stored.len() - MAX_OUTPUT;
                        stored.drain(..excess);
                    }
                }
            }
        }
        String::from_utf8_lossy(&stored).into_owned()
    })
}

fn exec(
    argv: &[String],
    cwd: &std::path::Path,
    stdin_data: Option<&str>,
    timeout_ms: u64,
    cap_mem: bool,
) -> std::io::Result<Exec> {
    use std::io::Write;
    let (prog, args) = argv
        .split_first()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty argv"))?;
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(if stdin_data.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);
    let mut child = cmd.spawn()?;
    let job = if cap_mem { make_job() } else { None };
    if let Some(j) = job.as_ref() {
        assign_to_job(j, child.id());
    }
    if let Some(data) = stdin_data {
        if let Some(mut si) = child.stdin.take() {
            let _ = si.write_all(data.as_bytes());
        }
    }
    let out_t = child.stdout.take().map(pump);
    let err_t = child.stderr.take().map(pump);
    let start = Instant::now();
    let deadline = start + Duration::from_millis(timeout_ms);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break child.wait().ok(),
        }
    };
    Ok(Exec {
        status,
        stdout: out_t.and_then(|t| t.join().ok()).unwrap_or_default(),
        stderr: err_t.and_then(|t| t.join().ok()).unwrap_or_default(),
        timed_out,
        ms: start.elapsed().as_millis() as u64,
    })
}

fn run_blocking(req: RunRequest) -> Result<RunResult, String> {
    let lang = req.language.trim().to_lowercase();
    static RUN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let seq = RUN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir: PathBuf =
        std::env::temp_dir().join(format!("tg-run-{}-{}-{}", std::process::id(), secs, seq));
    std::fs::create_dir_all(&dir).map_err(|e| format!("temp dir: {e}"))?;
    let plan = build_plan(&lang, &dir)?;
    let mut program = req.code.clone();
    if let Some(t) = req.tests.as_ref().filter(|t| !t.trim().is_empty()) {
        program = inject(&program, t);
    }
    let src_path = dir.join(plan.src_name);
    if let Err(e) = std::fs::write(&src_path, &program) {
        return Err(format!("write source: {e}"));
    }
    let interp = plan
        .compile
        .as_ref()
        .map(|c| c[0].clone())
        .or_else(|| plan.runs.first().map(|r| r[0].clone()))
        .unwrap_or_default();

    if let Some(argv) = plan.compile.as_ref() {
        let ex = exec(argv, &dir, None, COMPILE_TIMEOUT_MS, false).map_err(|e| format!("spawn {}: {e}", argv[0]))?;
        let code = ex.status.and_then(|s| s.code());
        if ex.timed_out || code != Some(0) {
            std::fs::remove_dir_all(&dir).ok();
            return Ok(RunResult {
                ok: false,
                exit_code: code,
                stdout: ex.stdout,
                stderr: if ex.timed_out { format!("compilation timed out\n{}", ex.stderr) } else { ex.stderr },
                timed_out: ex.timed_out,
                duration_ms: ex.ms,
                interpreter: interp,
            });
        }
    }

    let timeout = req.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let mut last_err = "no runnable command".to_string();
    for argv in plan.runs.iter() {
        match exec(argv, &dir, req.stdin.as_deref(), timeout, true) {
            Ok(ex) => {
                let code = ex.status.and_then(|s| s.code());
                let ok = !ex.timed_out && code == Some(0);
                let result = RunResult {
                    ok,
                    exit_code: code,
                    stdout: ex.stdout,
                    stderr: ex.stderr,
                    timed_out: ex.timed_out,
                    duration_ms: ex.ms,
                    interpreter: argv[0].clone(),
                };
                std::fs::remove_dir_all(&dir).ok();
                return Ok(result);
            }
            Err(e) => last_err = format!("spawn {}: {e}", argv[0]),
        }
    }
    std::fs::remove_dir_all(&dir).ok();
    Err(format!(
        "{last_err}. Is the {lang} toolchain installed and on PATH?"
    ))
}

#[tauri::command(async)]
pub async fn run_code(req: RunRequest) -> Result<RunResult, String> {
    tauri::async_runtime::spawn_blocking(move || run_blocking(req))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn py(code: &str, tests: Option<&str>, timeout_ms: Option<u64>) -> RunResult {
        run_blocking(RunRequest {
            language: "python".into(),
            code: code.into(),
            tests: tests.map(String::from),
            stdin: None,
            timeout_ms,
        })
        .expect("run failed")
    }

    #[test]
    fn injects_at_marker_or_appends() {
        assert_eq!(inject("def f():\n    return 1\n#TESTS\n", "assert f()==1"), "def f():\n    return 1\nassert f()==1\n");
        assert_eq!(inject("x=1", "assert x==1"), "x=1\n\nassert x==1");
    }

    #[test]
    fn python_pass_and_fail() {
        let good = py("print('hello')", Some("assert 1+1==2"), None);
        assert!(good.ok, "stdout={} stderr={}", good.stdout, good.stderr);
        assert!(good.stdout.contains("hello"));
        let bad = py("x=1", Some("assert x==2"), None);
        assert!(!bad.ok);
        assert!(bad.stderr.contains("AssertionError"), "stderr={}", bad.stderr);
    }

    #[test]
    fn python_syntax_error_fails() {
        let r = py("def broken(:", None, None);
        assert!(!r.ok);
        assert!(!r.stderr.is_empty());
    }

    #[test]
    fn infinite_loop_times_out() {
        let r = py("while True:\n    pass", None, Some(700));
        assert!(r.timed_out);
        assert!(!r.ok);
    }

    #[test]
    fn stdin_is_forwarded() {
        run_blocking(RunRequest {
            language: "python".into(),
            code: "print(input())".into(),
            tests: None,
            stdin: Some("ping\n".into()),
            timeout_ms: None,
        })
        .map(|r| {
            assert!(r.ok, "stderr={}", r.stderr);
            assert_eq!(r.stdout.trim(), "ping");
        })
        .unwrap();
    }

    #[test]
    fn unsupported_language_is_rejected() {
        let r = run_blocking(RunRequest {
            language: "cobol".into(),
            code: "".into(),
            tests: None,
            stdin: None,
            timeout_ms: None,
        });
        assert!(r.is_err());
    }

    #[test]
    fn output_is_capped() {
        let r = py("print('y'*10_000_000)", None, Some(8000));
        assert!(r.stdout.len() <= MAX_OUTPUT + 8192, "got {}", r.stdout.len());
    }
}
