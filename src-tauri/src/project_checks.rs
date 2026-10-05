//! Checks are configured by the project owner as argv; model text is never executed.
use crate::process_scope::ProcessScope;
use crate::project_store::{CheckCommand, CheckResult};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const OUTPUT_CAP: usize = 16_384;

fn capture(
    mut reader: impl Read + Send + 'static,
    buffer: Arc<Mutex<Vec<u8>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(count) = reader.read(&mut chunk) {
            if count == 0 {
                break;
            }
            let mut output = buffer.lock().unwrap();
            let remaining = OUTPUT_CAP.saturating_sub(output.len());
            output.extend_from_slice(&chunk[..count.min(remaining)]);
            // Keep draining even after the cap so the child never blocks on output.
        }
    })
}

fn safe_output(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut safe = String::new();
    for line in text.lines() {
        let lower = line.to_lowercase();
        let sensitive = [
            "api_key",
            "api-key",
            "authorization",
            "bearer ",
            "access_token",
            "refresh_token",
            "secret",
            "password",
            "sk-",
        ]
        .iter()
        .any(|key| lower.contains(key));
        if sensitive {
            safe.push_str("[凭据相关输出已隐藏]");
        } else {
            // Preserve tab/newline, discard terminal and window control sequences.
            safe.extend(line.chars().filter(|c| *c == '\t' || !c.is_control()));
        }
        safe.push('\n');
    }
    if bytes.len() == OUTPUT_CAP {
        safe.push_str("[输出已截断]\n");
    }
    safe
}

pub fn run_checks(
    commands: &[CheckCommand],
    root: &Path,
    cancelled: impl Fn() -> bool,
) -> Vec<CheckResult> {
    let mut results = Vec::new();
    for check in commands {
        if cancelled() {
            break;
        }
        let started = Instant::now();
        let mut result = CheckResult {
            name: check.name.clone(),
            program: check.program.clone(),
            args: check.args.clone(),
            exit_code: None,
            timed_out: false,
            duration_ms: 0,
            output: String::new(),
        };
        let mut command = Command::new(&check.program);
        command
            .args(&check.args)
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, _) in std::env::vars_os() {
            let upper = name.to_string_lossy().to_ascii_uppercase();
            if ["API_KEY", "TOKEN", "SECRET", "PASSWORD", "AUTHORIZATION"]
                .iter()
                .any(|key| upper.contains(key))
            {
                command.env_remove(name);
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(_) => {
                result.output = "验收程序启动失败，请检查项目配置".into();
                results.push(result);
                break;
            }
        };
        let scope = match ProcessScope::attach(&child) {
            Ok(scope) => scope,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                result.output = "验收进程生命周期绑定失败".into();
                results.push(result);
                break;
            }
        };
        let out = Arc::new(Mutex::new(Vec::new()));
        let err = Arc::new(Mutex::new(Vec::new()));
        let out_thread = capture(child.stdout.take().unwrap(), out.clone());
        let err_thread = capture(child.stderr.take().unwrap(), err.clone());
        let mut interrupted = false;
        loop {
            if cancelled() {
                interrupted = true;
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            if started.elapsed() >= Duration::from_secs(check.timeout_seconds as u64) {
                result.timed_out = true;
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    result.exit_code = status.code();
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(40)),
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
        // Close the job before readers join: descendants may still hold pipe handles.
        drop(scope);
        let _ = out_thread.join();
        let _ = err_thread.join();
        result.duration_ms = started.elapsed().as_millis() as u64;
        result.output = safe_output(&out.lock().unwrap());
        let error = safe_output(&err.lock().unwrap());
        if !error.is_empty() {
            result.output.push_str("\n[stderr]\n");
            result.output.push_str(&error);
        }
        if interrupted {
            result.output.push_str("\n检查因停止请求中断");
        }
        let failed = result.exit_code != Some(0) || result.timed_out || interrupted;
        results.push(result);
        if failed {
            break;
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    fn command(code: &str, timeout: u32) -> CheckCommand {
        CheckCommand {
            name: "真实 Python 检查".into(),
            program: "python".into(),
            args: vec!["-c".into(), code.into()],
            timeout_seconds: timeout,
        }
    }
    #[test]
    fn project_checks_run_real_argv_and_reject_nonzero_evidence() {
        let root = std::env::temp_dir();
        let checks = vec![
            command("print('CHECK_OK')", 10),
            command("import sys; print('FAILED'); sys.exit(7)", 10),
            command("print('MUST_NOT_RUN')", 10),
        ];
        let results = run_checks(&checks, &root, || false);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].exit_code, Some(0));
        assert!(results[0].output.contains("CHECK_OK"));
        assert_eq!(results[1].exit_code, Some(7));
        assert!(!crate::project_store::checks_pass(&checks, &results));
    }
    #[test]
    fn project_check_timeout_and_cancel_terminate_owned_children() {
        let root = std::env::temp_dir();
        let results = run_checks(&[command("import time; time.sleep(30)", 1)], &root, || {
            false
        });
        assert!(results[0].timed_out);
        assert!(results[0].duration_ms < 5000);
        assert_eq!(results[0].exit_code, None);
        let start = Instant::now();
        let results = run_checks(&[command("import time; time.sleep(30)", 10)], &root, || {
            start.elapsed() > Duration::from_millis(200)
        });
        assert_eq!(results.len(), 1);
        assert!(!results[0].timed_out);
        assert_eq!(results[0].exit_code, None);
        assert!(start.elapsed() < Duration::from_secs(5));
    }
    #[test]
    fn project_check_output_is_bounded_and_common_credential_lines_are_hidden() {
        let results = run_checks(
            &[command(
                "print('API_KEY=test-placeholder'); print('x'*50000)",
                10,
            )],
            &std::env::temp_dir(),
            || false,
        );
        assert_eq!(results[0].exit_code, Some(0));
        assert!(!results[0].output.contains("test-placeholder"));
        assert!(results[0].output.contains("输出已截断"));
        assert!(results[0].output.len() < 17000);
    }
    #[test]
    fn project_check_missing_program_and_pre_cancel_cannot_pass() {
        let mut check = command("print('unused')", 5);
        check.program = "agent-hub-nonexistent-program-45b3".into();
        let results = run_checks(&[check], &std::env::temp_dir(), || false);
        assert_eq!(results[0].exit_code, None);
        assert!(run_checks(
            &[command("print('unused')", 5)],
            &std::env::temp_dir(),
            || true
        )
        .is_empty());
    }
}
