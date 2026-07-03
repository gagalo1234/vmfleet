//! systemd --user wrappers: launch workers as transient units, manage the
//! supervisor unit, ensure linger. The worker transient units intentionally have
//! NO Restart= so the supervisor fully controls scaling (a finished worker stays
//! down until reconcile decides to relaunch).

use crate::cmd::{checked, Runner};
use anyhow::Result;
use std::time::Duration;

pub struct Systemd<'a> {
    r: &'a dyn Runner,
}

impl<'a> Systemd<'a> {
    pub fn new(r: &'a dyn Runner) -> Self {
        Systemd { r }
    }

    /// Launch `program args` as a transient user unit named `unit`, with env and
    /// resource properties. No Restart (default) — supervisor owns the lifecycle.
    pub fn run_transient(
        &self,
        unit: &str,
        setenvs: &[(String, String)],
        program: &str,
        prog_args: &[String],
    ) -> Result<()> {
        let mut args: Vec<String> = vec![
            "--user".into(),
            format!("--unit={unit}"),
            "--quiet".into(),
            // Garbage-collect the unit even if it exits non-zero (CollectMode=
            // inactive-or-failed). Without this a failed worker leaves a lingering
            // `failed` unit and systemd-run refuses to reuse the name, wedging the
            // slot. This is the primary, systemd-native defense; reset_failed below
            // is belt-and-suspenders for units left over from an older binary.
            "--collect".into(),
            "-p".into(),
            "KillMode=control-group".into(),
            "-p".into(),
            "TimeoutStopSec=120".into(),
        ];
        for (k, v) in setenvs {
            args.push(format!("--setenv={k}={v}"));
        }
        args.push(program.to_string());
        args.extend(prog_args.iter().cloned());
        let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        // Belt-and-suspenders: clear any pre-existing `failed` state for this unit
        // name (idempotent, ignore errors) so the slot never wedges even if a prior
        // unit was created without --collect.
        self.reset_failed(unit);
        checked(self.r, "systemd-run", &refs, Some(Duration::from_secs(30)))?;
        Ok(())
    }

    /// Clear a unit's failed state (no-op if not failed).
    pub fn reset_failed(&self, unit: &str) {
        let _ = self.r.run(
            "systemctl",
            &["--user", "reset-failed", unit],
            Some(Duration::from_secs(10)),
        );
    }

    /// Names of failed units matching a glob (for the supervisor's periodic sweep).
    pub fn list_failed(&self, pattern: &str) -> Vec<String> {
        self.r
            .run(
                "systemctl",
                &[
                    "--user",
                    "list-units",
                    pattern,
                    "--no-legend",
                    "--plain",
                    "--state=failed",
                ],
                Some(Duration::from_secs(15)),
            )
            .map(|o| {
                o.stdout
                    .lines()
                    .filter_map(|l| l.split_whitespace().next())
                    .filter(|u| u.ends_with(".service"))
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Active/activating transient worker unit names matching a glob (e.g. "vmfleet-worker-*").
    pub fn list_active(&self, pattern: &str) -> Result<Vec<String>> {
        let out = self.r.run(
            "systemctl",
            &[
                "--user",
                "list-units",
                pattern,
                "--no-legend",
                "--plain",
                "--state=active,activating",
            ],
            Some(Duration::from_secs(15)),
        )?;
        Ok(out
            .stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter(|u| u.ends_with(".service"))
            .map(|s| s.to_string())
            .collect())
    }

    pub fn stop(&self, unit: &str) -> Result<()> {
        // bounded: don't block on the 300s TimeoutStopSec window
        let _ = self.r.run(
            "systemctl",
            &["--user", "stop", unit],
            Some(Duration::from_secs(75)),
        );
        let _ = self.r.run(
            "systemctl",
            &["--user", "reset-failed", unit],
            Some(Duration::from_secs(10)),
        );
        Ok(())
    }

    pub fn daemon_reload(&self) -> Result<()> {
        checked(
            self.r,
            "systemctl",
            &["--user", "daemon-reload"],
            Some(Duration::from_secs(20)),
        )?;
        Ok(())
    }

    pub fn enable_now(&self, unit: &str) -> Result<()> {
        checked(
            self.r,
            "systemctl",
            &["--user", "enable", "--now", unit],
            Some(Duration::from_secs(30)),
        )?;
        Ok(())
    }

    pub fn restart(&self, unit: &str) -> Result<()> {
        checked(
            self.r,
            "systemctl",
            &["--user", "restart", unit],
            Some(Duration::from_secs(30)),
        )?;
        Ok(())
    }

    pub fn disable_now(&self, unit: &str) -> Result<()> {
        let _ = self.r.run(
            "systemctl",
            &["--user", "disable", "--now", unit],
            Some(Duration::from_secs(30)),
        );
        Ok(())
    }

    /// loginctl enable-linger <user> so user services run without an active login
    /// and survive reboot.
    pub fn ensure_linger(&self, user: &str) -> Result<()> {
        checked(
            self.r,
            "loginctl",
            &["enable-linger", user],
            Some(Duration::from_secs(15)),
        )?;
        Ok(())
    }

    pub fn linger_enabled(&self, user: &str) -> bool {
        self.r
            .run(
                "loginctl",
                &["show-user", user, "--property=Linger", "--value"],
                Some(Duration::from_secs(10)),
            )
            .map(|o| o.stdout.trim() == "yes")
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdOut, Runner};
    use std::sync::Mutex;

    /// Records every (program, args) invocation so tests can assert the exact
    /// command sequence a `Systemd` method emits. `Mutex` (not `RefCell`) so it
    /// satisfies the `Runner: Send + Sync` bound.
    #[derive(Default)]
    struct MockRunner {
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl Runner for MockRunner {
        fn run(&self, program: &str, args: &[&str], _timeout: Option<Duration>) -> Result<CmdOut> {
            self.calls.lock().unwrap().push((
                program.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
            ));
            Ok(CmdOut {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    // Regression test for the slot-wedge bug: a worker that exited non-zero left a
    // `failed` transient unit, and `systemd-run --unit=<same>` was then rejected,
    // wedging the slot forever. The fix is `--collect` (auto-GC failed units) plus
    // a defensive `reset-failed`. Lock both in so the wedge can't silently return.
    #[test]
    fn run_transient_collects_and_resets_before_launch() {
        let mock = MockRunner::default();
        let sd = Systemd::new(&mock);
        let setenvs = vec![("KEY".to_string(), "VAL".to_string())];
        sd.run_transient(
            "vmfleet-worker-101.service",
            &setenvs,
            "/usr/bin/vmfleet",
            &["worker".to_string(), "101".to_string()],
        )
        .unwrap();

        let calls = mock.calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "expected reset-failed then systemd-run");

        // Assert the exact command + argument vector (not mere presence), so a
        // dropped `--user` (which would target the system manager instead of the
        // user manager) or a reordered flag is caught as a regression.

        // 1) reset-failed clears any stale failed unit first.
        assert_eq!(calls[0].0, "systemctl");
        assert_eq!(
            calls[0].1,
            ["--user", "reset-failed", "vmfleet-worker-101.service"]
        );

        // 2) systemd-run must carry --collect (auto-GC failed units) in exact
        //    order, with the unit name, env, program and its args.
        assert_eq!(calls[1].0, "systemd-run");
        assert_eq!(
            calls[1].1,
            [
                "--user",
                "--unit=vmfleet-worker-101.service",
                "--quiet",
                "--collect",
                "-p",
                "KillMode=control-group",
                "-p",
                "TimeoutStopSec=120",
                "--setenv=KEY=VAL",
                "/usr/bin/vmfleet",
                "worker",
                "101",
            ]
        );
    }

    #[test]
    fn stop_stops_then_resets_failed() {
        let mock = MockRunner::default();
        let sd = Systemd::new(&mock);
        sd.stop("vmfleet-worker-102.service").unwrap();

        let calls = mock.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        // Exact command + args: stop, then reset-failed, both on the user manager.
        assert_eq!(calls[0].0, "systemctl");
        assert_eq!(calls[0].1, ["--user", "stop", "vmfleet-worker-102.service"]);
        assert_eq!(calls[1].0, "systemctl");
        assert_eq!(
            calls[1].1,
            ["--user", "reset-failed", "vmfleet-worker-102.service"]
        );
    }
}
