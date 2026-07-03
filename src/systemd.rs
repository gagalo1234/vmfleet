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
        // A prior worker that exited non-zero leaves the transient unit in a
        // `failed` state, and systemd-run refuses to reuse the name. Clear it
        // first (idempotent, ignore errors) so the slot never wedges.
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
