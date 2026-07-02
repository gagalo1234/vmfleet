//! Multipass wrapper. All VM operations go through here so the worker/supervisor
//! stay declarative and the command layer is mockable in tests.

use crate::cmd::{checked, CmdOut, Runner};
use anyhow::{anyhow, Result};
use std::time::Duration;

pub struct Multipass<'a> {
    r: &'a dyn Runner,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmState {
    Running,
    Stopped,
    Starting,
    Suspended,
    Unknown(String),
}

impl VmState {
    fn parse(s: &str) -> VmState {
        match s.trim() {
            "Running" => VmState::Running,
            "Stopped" => VmState::Stopped,
            "Starting" => VmState::Starting,
            "Suspended" => VmState::Suspended,
            other => VmState::Unknown(other.to_string()),
        }
    }
}

impl<'a> Multipass<'a> {
    pub fn new(r: &'a dyn Runner) -> Self {
        Multipass { r }
    }

    pub fn available(&self) -> bool {
        self.r
            .run("multipass", &["version"], Some(Duration::from_secs(10)))
            .map(|o| o.ok())
            .unwrap_or(false)
    }

    pub fn exists(&self, name: &str) -> bool {
        self.r
            .run("multipass", &["info", name], Some(Duration::from_secs(20)))
            .map(|o| o.ok())
            .unwrap_or(false)
    }

    /// (name, state) for every instance. Parses `multipass list --format csv`.
    pub fn list(&self) -> Result<Vec<(String, VmState)>> {
        let out = checked(
            self.r,
            "multipass",
            &["list", "--format", "csv"],
            Some(Duration::from_secs(30)),
        )?;
        Ok(parse_list_csv(&out))
    }

    /// Names of instances whose name starts with `prefix`.
    pub fn list_prefixed(&self, prefix: &str) -> Result<Vec<(String, VmState)>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|(n, _)| n.starts_with(prefix))
            .collect())
    }

    pub fn clone_from(&self, base: &str, name: &str) -> Result<()> {
        checked(
            self.r,
            "multipass",
            &["clone", base, "--name", name],
            Some(Duration::from_secs(180)),
        )?;
        Ok(())
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        checked(
            self.r,
            "multipass",
            &["set", &format!("{key}={value}")],
            Some(Duration::from_secs(30)),
        )?;
        Ok(())
    }

    pub fn set_cpus(&self, vm: &str, cpus: u32) -> Result<()> {
        self.set(&format!("local.{vm}.cpus"), &cpus.to_string())
    }
    pub fn set_memory(&self, vm: &str, mem: &str) -> Result<()> {
        self.set(&format!("local.{vm}.memory"), mem)
    }

    pub fn start(&self, name: &str) -> Result<()> {
        checked(
            self.r,
            "multipass",
            &["start", name],
            Some(Duration::from_secs(180)),
        )?;
        Ok(())
    }

    /// Run a shell snippet inside the VM (login shell), with a timeout.
    pub fn exec(&self, name: &str, script: &str, timeout: Duration) -> Result<CmdOut> {
        self.r.run(
            "multipass",
            &["exec", name, "--", "bash", "-lc", script],
            Some(timeout),
        )
    }

    /// Force-teardown with a bounded timeout so a wedged multipassd can't hang us
    /// for the 300s systemd stop window (the failure mode seen in clearbox).
    pub fn purge(&self, name: &str) -> Result<()> {
        let t = Some(Duration::from_secs(60));
        let _ = self.r.run("multipass", &["stop", name, "--cancel"], t);
        let _ = self.r.run("multipass", &["stop", "--force", name], t);
        let o = self.r.run(
            "multipass",
            &["delete", name, "--purge"],
            Some(Duration::from_secs(90)),
        )?;
        if !o.ok() {
            // one retry
            let _ = self.r.run("multipass", &["stop", "--force", name], t);
            let o2 = self.r.run(
                "multipass",
                &["delete", name, "--purge"],
                Some(Duration::from_secs(90)),
            )?;
            if !o2.ok() {
                return Err(anyhow!("failed to purge {name}: {}", o2.stderr.trim()));
            }
        }
        Ok(())
    }

    pub fn stop(&self, name: &str) -> Result<()> {
        checked(
            self.r,
            "multipass",
            &["stop", name],
            Some(Duration::from_secs(120)),
        )?;
        Ok(())
    }

    /// Copy a local file into the VM (used by build-base to stage provision scripts).
    pub fn transfer(&self, local: &str, vm: &str, remote: &str) -> Result<()> {
        checked(
            self.r,
            "multipass",
            &["transfer", local, &format!("{vm}:{remote}")],
            Some(Duration::from_secs(120)),
        )?;
        Ok(())
    }

    /// Launch a fresh VM (used by build-base).
    pub fn launch(
        &self,
        image: &str,
        name: &str,
        cpus: u32,
        memory: &str,
        disk: &str,
    ) -> Result<()> {
        checked(
            self.r,
            "multipass",
            &[
                "launch",
                image,
                "--name",
                name,
                "--cpus",
                &cpus.to_string(),
                "--memory",
                memory,
                "--disk",
                disk,
            ],
            Some(Duration::from_secs(900)),
        )?;
        Ok(())
    }
}

fn parse_list_csv(csv: &str) -> Vec<(String, VmState)> {
    let mut out = Vec::new();
    for (i, line) in csv.lines().enumerate() {
        if i == 0 || line.trim().is_empty() {
            continue; // header
        }
        let mut cols = line.split(',');
        let name = cols.next().unwrap_or("").trim().to_string();
        let state = cols.next().unwrap_or("").trim();
        if !name.is_empty() {
            out.push((name, VmState::parse(state)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_list() {
        let csv = "Name,State,IPv4,IPv6,Release,AllIPv4\n\
                   vmfleet-small-101-x,Running,10.0.0.1,,Ubuntu,\"10.0.0.1\"\n\
                   vmfleet-base,Stopped,,,Ubuntu,\"\"\n";
        let v = parse_list_csv(csv);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].0, "vmfleet-small-101-x");
        assert_eq!(v[0].1, VmState::Running);
        assert_eq!(v[1].1, VmState::Stopped);
    }
}
