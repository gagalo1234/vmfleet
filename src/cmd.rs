//! Thin subprocess helper used by the multipass/systemd/github(fallback) wrappers.
//! Centralizes spawning, output capture, and timeouts so the rest of the code can
//! stay declarative and, importantly, so it can be swapped for a mock in tests.

use anyhow::{anyhow, Result};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

/// Result of running an external command.
#[derive(Debug, Clone)]
pub struct CmdOut {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOut {
    pub fn ok(&self) -> bool {
        self.status == 0
    }
}

/// Abstraction over "run a command line and get its output". Real impl shells out;
/// tests provide a scripted mock. Keeps the orchestration logic unit-testable.
pub trait Runner: Send + Sync {
    fn run(&self, program: &str, args: &[&str], timeout: Option<Duration>) -> Result<CmdOut>;
}

/// Production runner: actually spawns processes.
#[derive(Default, Clone)]
pub struct SystemRunner;

impl SystemRunner {
    pub fn new() -> Self {
        SystemRunner
    }
}

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[&str], timeout: Option<Duration>) -> Result<CmdOut> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("spawn {program}: {e}"))?;

        // Simple timeout: poll for completion. Good enough for our second-scale ops.
        if let Some(dur) = timeout {
            let deadline = std::time::Instant::now() + dur;
            loop {
                match child.try_wait()? {
                    Some(_) => break,
                    None => {
                        if std::time::Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            return Err(anyhow!("`{program}` timed out after {dur:?}"));
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        }
        let out: Output = child.wait_with_output()?;
        Ok(CmdOut {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// Convenience: run and require success, returning stdout.
pub fn checked(
    r: &dyn Runner,
    program: &str,
    args: &[&str],
    timeout: Option<Duration>,
) -> Result<String> {
    let o = r.run(program, args, timeout)?;
    if !o.ok() {
        return Err(anyhow!(
            "`{program} {}` exited {}: {}",
            args.join(" "),
            o.status,
            o.stderr.trim()
        ));
    }
    Ok(o.stdout)
}
