//! Resource-aware admission gate — the port of the clearbox flock+backpressure
//! logic. All workers share one lock file, which serializes VM *launches* (so we
//! never stampede multipassd) and lets each launch wait until the host has
//! headroom (MemAvailable / memory PSI / vault disk). The worker holds the guard
//! across clone+start plus a short settle, then drops it.

use crate::config::Admission;
use crate::resources;
use anyhow::{anyhow, Result};
use nix::fcntl::{Flock, FlockArg};
use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct Gate {
    lock_path: PathBuf,
    cfg: Admission,
    vault: PathBuf,
}

/// Held for the duration of a launch; releasing the flock happens on drop.
pub struct Guard {
    _lock: Flock<File>,
}

pub enum Block {
    LowMem { avail: u64 },
    HighPsi { psi: f64 },
    LowDisk { free: u64 },
}

impl std::fmt::Display for Block {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Block::LowMem { avail } => write!(f, "low-mem (MemAvailable {avail} MiB)"),
            Block::HighPsi { psi } => write!(f, "high-psi (some avg10 {psi}%)"),
            Block::LowDisk { free } => write!(f, "low-disk ({free} GiB free)"),
        }
    }
}

impl Gate {
    pub fn new(lock_path: PathBuf, cfg: Admission, vault: PathBuf) -> Self {
        Gate {
            lock_path,
            cfg,
            vault,
        }
    }

    /// Pure decision: is the host clear to launch another VM right now?
    pub fn blocked(&self) -> Option<Block> {
        decide(
            resources::mem_available_mib(),
            resources::mem_psi_some_avg10(),
            resources::disk_free_gib(&self.vault).unwrap_or(u64::MAX),
            &self.cfg,
        )
    }

    /// Acquire the shared launch lock (serialize), then hold it while waiting for
    /// resources to clear. Returns a guard the caller drops after start+settle.
    pub fn acquire(&self) -> Result<Guard> {
        if let Some(parent) = self.lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)?;

        // Acquire exclusively, non-blocking in a loop so we can log and stay
        // responsive to signals rather than blocking forever in the kernel.
        let deadline = Instant::now() + Duration::from_secs(self.cfg.lock_wait_seconds.max(1) * 20);
        let lock = loop {
            match Flock::lock(file.try_clone()?, FlockArg::LockExclusiveNonblock) {
                Ok(l) => break l,
                Err((_f, nix::errno::Errno::EWOULDBLOCK)) => {
                    if Instant::now() > deadline {
                        return Err(anyhow!("timed out waiting for admission lock"));
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
                Err((_f, e)) => return Err(anyhow!("flock: {e}")),
            }
        };

        // Hold the lock; wait for resource headroom (backpressure).
        let mut waited = 0u64;
        while let Some(b) = self.blocked() {
            tracing::info!("admission: host under pressure ({b}), waited {waited}s");
            std::thread::sleep(Duration::from_secs(self.cfg.poll_seconds.max(1)));
            waited += self.cfg.poll_seconds.max(1);
        }
        Ok(Guard { _lock: lock })
    }

    pub fn settle(&self) {
        std::thread::sleep(Duration::from_secs(self.cfg.settle_seconds));
    }
}

/// Pure admission decision — unit tested.
pub fn decide(avail_mib: u64, psi: f64, disk_gib: u64, cfg: &Admission) -> Option<Block> {
    if avail_mib < cfg.min_avail_mib {
        return Some(Block::LowMem { avail: avail_mib });
    }
    if psi >= cfg.psi_max {
        return Some(Block::HighPsi { psi });
    }
    if disk_gib < cfg.min_disk_gib {
        return Some(Block::LowDisk { free: disk_gib });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Admission {
        Admission {
            min_avail_mib: 4096,
            psi_max: 20.0,
            min_disk_gib: 30,
            ..Default::default()
        }
    }

    #[test]
    fn grants_when_healthy() {
        assert!(decide(8000, 1.0, 100, &cfg()).is_none());
    }
    #[test]
    fn blocks_low_mem() {
        assert!(matches!(
            decide(1000, 1.0, 100, &cfg()),
            Some(Block::LowMem { .. })
        ));
    }
    #[test]
    fn blocks_high_psi() {
        assert!(matches!(
            decide(8000, 25.0, 100, &cfg()),
            Some(Block::HighPsi { .. })
        ));
    }
    #[test]
    fn blocks_low_disk() {
        assert!(matches!(
            decide(8000, 1.0, 10, &cfg()),
            Some(Block::LowDisk { .. })
        ));
    }
}
