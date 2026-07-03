//! Autoscaling control plane. Each cycle: read GitHub runners + queued jobs +
//! host resources, then per pool bring the number of workers toward
//! desired = clamp(min_warm, busy + queued + min_warm, max). Launch into free
//! slots (capped by per-cycle limit and memory headroom, gated by admission);
//! stop idle workers that exceed idle_timeout when over-provisioned. Workers are
//! transient units with no Restart, so finished ones simply stay down unless a
//! later cycle relaunches them — giving free scale-down.

use crate::cmd::SystemRunner;
use crate::config::{Config, Pool};
use crate::github::{Client, Runner};
use crate::resources;
use crate::systemd::Systemd;
use crate::{admission, naming, paths};
use anyhow::Result;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

/// Pure: desired worker count for a pool given current demand.
pub fn desired(busy: u32, queued: u32, min_warm: u32, max: u32) -> u32 {
    let want = busy.saturating_add(queued).saturating_add(min_warm);
    want.clamp(min_warm, max)
}

/// Pure: memory headroom expressed as a count of additional busy VMs.
pub fn headroom_vms(avail_mib: u64, min_avail_mib: u64, busy_reserve_mib: u64) -> u32 {
    let free = avail_mib.saturating_sub(min_avail_mib);
    (free / busy_reserve_mib.max(1)) as u32
}

#[derive(Serialize)]
struct PoolReport {
    name: String,
    idle: u32,
    busy: u32,
    queued: u32,
    active_units: u32,
    pending: u32,
    present: u32,
    desired: u32,
    deficit: i64,
    actions: Vec<String>,
}
#[derive(Serialize)]
struct Report {
    ts: u64,
    mem_avail_mib: u64,
    psi: f64,
    disk_free_gib: u64,
    blocked: Option<String>,
    headroom_vms: u32,
    pools: Vec<PoolReport>,
}

pub fn run(cfg: &Config, cfg_path: &Path) -> Result<()> {
    // Single-instance guard: refuse to start a second supervisor (which would
    // double-provision). Held for the process lifetime.
    let _singleton = acquire_singleton()?;
    let runner = SystemRunner::new();
    let sd = Systemd::new(&runner);
    let client = Client::new(&cfg.github, cfg.token()?)?;
    let gate = admission::Gate::new(
        paths::admission_lock(),
        cfg.admission.clone(),
        cfg.storage.vault_path.clone(),
    );
    let exe = std::env::current_exe()?;
    let mut state = SupState::default();

    tracing::info!(
        "supervisor start scope={} poll={}s pools={:?}",
        cfg.github.scope_path()?,
        cfg.supervisor.poll_seconds,
        cfg.pools
            .iter()
            .map(|p| (&p.name, p.min_warm, p.max))
            .collect::<Vec<_>>()
    );

    loop {
        if let Err(e) = reconcile(cfg, cfg_path, &client, &sd, &gate, &exe, &mut state) {
            tracing::warn!("reconcile error: {e:#}");
        }
        std::thread::sleep(Duration::from_secs(cfg.supervisor.poll_seconds.max(1)));
    }
}

fn acquire_singleton() -> Result<nix::fcntl::Flock<std::fs::File>> {
    use nix::fcntl::{Flock, FlockArg};
    std::fs::create_dir_all(paths::state_dir())?;
    let path = paths::state_dir().join("supervisor.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    Flock::lock(file, FlockArg::LockExclusiveNonblock)
        .map_err(|_| anyhow::anyhow!("another vmfleet supervisor is already running"))
}

/// Mutable supervisor state carried across reconcile cycles.
#[derive(Default)]
pub struct SupState {
    idle_since: HashMap<String, Instant>,
    cycle: u64,
    cached_queued: HashMap<String, u32>,
}

/// One reconcile pass. Public-in-crate so `status` can render a live view too.
pub fn reconcile(
    cfg: &Config,
    cfg_path: &Path,
    client: &Client,
    sd: &Systemd,
    gate: &admission::Gate,
    exe: &Path,
    state: &mut SupState,
) -> Result<()> {
    let runners = match client.list_runners() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("github list_runners failed (transient), skipping cycle: {e}");
            return Ok(());
        }
    };
    // Recover any worker units left in `failed` state (else systemd-run can't
    // reuse the slot name). reset_failed is also called just-in-time at launch.
    for u in sd.list_failed(naming::WORKER_UNIT_GLOB) {
        sd.reset_failed(&u);
    }
    // Throttle the expensive (N+1) queued-jobs scan: only every Nth cycle.
    let every = cfg.supervisor.queued_poll_every.max(1) as u64;
    if state.cycle.is_multiple_of(every) {
        state.cached_queued = client.queued_labels();
    }
    state.cycle = state.cycle.wrapping_add(1);
    let queued = state.cached_queued.clone();
    let active_units = sd.list_active(naming::WORKER_UNIT_GLOB).unwrap_or_default();
    let active_slots: HashSet<u32> = active_units
        .iter()
        .filter_map(|u| slot_from_unit(u))
        .collect();

    let avail = resources::mem_available_mib();
    let psi = resources::mem_psi_some_avg10();
    let disk = resources::disk_free_gib(&cfg.storage.vault_path).unwrap_or(u64::MAX);
    let blocked = gate.blocked();
    let mut headroom = headroom_vms(
        avail,
        cfg.admission.min_avail_mib,
        cfg.supervisor.busy_reserve_mib,
    );
    let mut launched_this_cycle = 0u32;

    // refresh idle_since map
    let now_idle: HashSet<String> = runners
        .iter()
        .filter(|r| r.online() && !r.busy)
        .map(|r| r.name.clone())
        .collect();
    state.idle_since.retain(|k, _| now_idle.contains(k));
    for name in &now_idle {
        state
            .idle_since
            .entry(name.clone())
            .or_insert_with(Instant::now);
    }

    let mut reports = Vec::new();
    for pool in &cfg.pools {
        let mine: Vec<&Runner> = runners
            .iter()
            .filter(|r| pool.labels.iter().any(|l| r.has_label(l)))
            .collect();
        let idle: Vec<&&Runner> = mine.iter().filter(|r| r.online() && !r.busy).collect();
        let busy = mine.iter().filter(|r| r.online() && r.busy).count() as u32;
        let pool_slots: HashSet<u32> = pool.slots().collect();
        let active: HashSet<u32> = active_slots.intersection(&pool_slots).copied().collect();
        let online_slots: HashSet<u32> = mine
            .iter()
            .filter(|r| r.online())
            .filter_map(|r| naming::slot_of_vm(&pool.name, &r.name))
            .collect();
        let pending = active.iter().filter(|s| !online_slots.contains(s)).count() as u32;
        let present = idle.len() as u32 + busy + pending;
        let qd = pool
            .labels
            .iter()
            .map(|l| *queued.get(l).unwrap_or(&0))
            .max()
            .unwrap_or(0);
        let want = desired(busy, qd, pool.min_warm, pool.max);
        let deficit = want as i64 - present as i64;

        let mut actions = Vec::new();
        if deficit > 0 && blocked.is_none() {
            let can = (deficit as u32)
                .min(
                    cfg.supervisor
                        .max_launch_per_poll
                        .saturating_sub(launched_this_cycle),
                )
                .min(headroom);
            let mut free: Vec<u32> = pool_slots.difference(&active).copied().collect();
            free.sort_unstable();
            for slot in free.into_iter().take(can as usize) {
                match launch(sd, exe, cfg_path, pool, slot) {
                    Ok(_) => {
                        actions.push(format!("launch:{slot}"));
                        launched_this_cycle += 1;
                        headroom = headroom.saturating_sub(1);
                    }
                    Err(e) => tracing::warn!("launch slot {slot} failed: {e}"),
                }
            }
        } else if deficit < 0 {
            // scale down: stop idle workers over the desired count that have been
            // idle longer than idle_timeout (only our own slots).
            let mut excess = (-deficit) as u32;
            let idle_timeout = Duration::from_secs(cfg.supervisor.idle_timeout_seconds);
            let mut candidates: Vec<(&&Runner, u32)> = idle
                .iter()
                .filter_map(|r| naming::slot_of_vm(&pool.name, &r.name).map(|s| (*r, s)))
                .filter(|(_, s)| active.contains(s))
                .collect();
            // oldest-idle first
            candidates.sort_by_key(|(r, _)| {
                state
                    .idle_since
                    .get(&r.name)
                    .copied()
                    .unwrap_or_else(Instant::now)
            });
            for (r, slot) in candidates {
                if excess == 0 {
                    break;
                }
                let long_idle = state
                    .idle_since
                    .get(&r.name)
                    .map(|t| t.elapsed() >= idle_timeout)
                    .unwrap_or(false);
                if long_idle {
                    let _ = sd.stop(&naming::worker_unit(slot));
                    actions.push(format!("stop:{slot}"));
                    excess -= 1;
                }
            }
        }

        reports.push(PoolReport {
            name: pool.name.clone(),
            idle: idle.len() as u32,
            busy,
            queued: qd,
            active_units: active.len() as u32,
            pending,
            present,
            desired: want,
            deficit,
            actions,
        });
    }

    let report = Report {
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        mem_avail_mib: avail,
        psi,
        disk_free_gib: disk,
        blocked: blocked.map(|b| b.to_string()),
        headroom_vms: headroom_vms(
            avail,
            cfg.admission.min_avail_mib,
            cfg.supervisor.busy_reserve_mib,
        ),
        pools: reports,
    };
    write_status(&report);
    write_metrics(&report);
    Ok(())
}

/// Write a Prometheus textfile-collector file next to status.json.
fn write_metrics(report: &Report) {
    let mut s = String::new();
    s.push_str(&format!(
        "vmfleet_mem_available_mib {}\n",
        report.mem_avail_mib
    ));
    s.push_str(&format!("vmfleet_memory_psi_some_avg10 {}\n", report.psi));
    s.push_str(&format!("vmfleet_disk_free_gib {}\n", report.disk_free_gib));
    s.push_str(&format!("vmfleet_headroom_vms {}\n", report.headroom_vms));
    s.push_str(&format!(
        "vmfleet_blocked {}\n",
        report.blocked.is_some() as u8
    ));
    for p in &report.pools {
        let l = &p.name;
        s.push_str(&format!("vmfleet_pool_idle{{pool=\"{l}\"}} {}\n", p.idle));
        s.push_str(&format!("vmfleet_pool_busy{{pool=\"{l}\"}} {}\n", p.busy));
        s.push_str(&format!(
            "vmfleet_pool_queued{{pool=\"{l}\"}} {}\n",
            p.queued
        ));
        s.push_str(&format!(
            "vmfleet_pool_present{{pool=\"{l}\"}} {}\n",
            p.present
        ));
        s.push_str(&format!(
            "vmfleet_pool_desired{{pool=\"{l}\"}} {}\n",
            p.desired
        ));
    }
    let tmp = paths::metrics_file().with_extension("tmp");
    if std::fs::create_dir_all(paths::state_dir()).is_ok() && std::fs::write(&tmp, &s).is_ok() {
        let _ = std::fs::rename(&tmp, paths::metrics_file());
    }
}

fn launch(sd: &Systemd, exe: &Path, cfg_path: &Path, pool: &Pool, slot: u32) -> Result<()> {
    let unit = naming::worker_unit(slot);
    let args = vec![
        "--config".to_string(),
        cfg_path.to_string_lossy().into_owned(),
        "worker".to_string(),
        pool.name.clone(),
        slot.to_string(),
    ];
    tracing::info!("[{}] launch slot {slot}", pool.name);
    sd.run_transient(&unit, &[], &exe.to_string_lossy(), &args)
}

fn slot_from_unit(unit: &str) -> Option<u32> {
    unit.strip_prefix("vmfleet-worker-")?
        .strip_suffix(".service")?
        .parse()
        .ok()
}

fn write_status(report: &Report) {
    if let Ok(json) = serde_json::to_string_pretty(report) {
        let tmp = paths::status_file().with_extension("tmp");
        if std::fs::create_dir_all(paths::state_dir()).is_ok()
            && std::fs::write(&tmp, &json).is_ok()
        {
            let _ = std::fs::rename(&tmp, paths::status_file());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desired_warm_pool_when_idle() {
        // no load: keep exactly min_warm
        assert_eq!(desired(0, 0, 2, 10), 2);
    }
    #[test]
    fn desired_scales_with_busy_and_queue() {
        // 3 busy + 5 queued + 2 warm = 10 (capped)
        assert_eq!(desired(3, 5, 2, 10), 10);
        assert_eq!(desired(3, 1, 2, 10), 6);
    }
    #[test]
    fn desired_capped_at_max() {
        assert_eq!(desired(20, 20, 2, 10), 10);
    }
    #[test]
    fn headroom_math() {
        assert_eq!(headroom_vms(24000, 4096, 2048), 9);
        assert_eq!(headroom_vms(4000, 4096, 2048), 0);
    }
    #[test]
    fn slot_parse() {
        assert_eq!(slot_from_unit("vmfleet-worker-103.service"), Some(103));
        assert_eq!(slot_from_unit("other.service"), None);
    }
}
