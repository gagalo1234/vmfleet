//! vmfleet configuration: a single TOML file replacing the scattered env vars of
//! the original clearbox scripts. Loaded, validated, and shared read-only across
//! all subcommands.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

fn default_true() -> bool {
    true
}

/// Current config schema version.
pub const CURRENT_VERSION: u32 = 1;
fn d_version() -> u32 {
    CURRENT_VERSION
}

/// Apply forward migrations to a parsed config. Returns true if it changed.
/// (Only v1 exists today; this is the framework for future bumps.)
pub fn migrate(cfg: &mut Config) -> bool {
    let mut changed = false;
    // Example future step:
    // if cfg.version < 2 { ...migrate...; cfg.version = 2; changed = true; }
    if cfg.version < CURRENT_VERSION {
        cfg.version = CURRENT_VERSION;
        changed = true;
    }
    changed
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Config schema version (for `install --upgrade` migrations).
    #[serde(default = "d_version")]
    pub version: u32,
    pub github: GitHub,
    #[serde(default)]
    pub storage: Storage,
    #[serde(default)]
    pub admission: Admission,
    /// One or more base images. Pools clone from their referenced base (or the
    /// first one). Written as `[[base]]` tables.
    #[serde(rename = "base")]
    pub bases: Vec<Base>,
    #[serde(rename = "pool")]
    pub pools: Vec<Pool>,
    #[serde(default)]
    pub supervisor: Supervisor,
    #[serde(default)]
    pub worker: Worker,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Worker {
    /// Timeout for in-VM config/setup steps.
    #[serde(default = "d_cfg_timeout")]
    pub config_timeout_seconds: u64,
    /// How long to wait for the runner to register after run.sh starts.
    #[serde(default = "d_reg_timeout")]
    pub registration_timeout_seconds: u64,
    /// Grace before reaping a runner that went offline/absent (zombie).
    #[serde(default = "d_offline_grace")]
    pub offline_grace_seconds: u64,
    /// Health-monitor poll interval.
    #[serde(default = "d_health_poll")]
    pub health_poll_seconds: u64,
    /// cloud-init readiness timeout.
    #[serde(default = "d_ci_timeout")]
    pub cloud_init_timeout_seconds: u64,
}
fn d_cfg_timeout() -> u64 {
    180
}
fn d_reg_timeout() -> u64 {
    180
}
fn d_offline_grace() -> u64 {
    90
}
fn d_health_poll() -> u64 {
    10
}
fn d_ci_timeout() -> u64 {
    180
}
impl Default for Worker {
    fn default() -> Self {
        Worker {
            config_timeout_seconds: d_cfg_timeout(),
            registration_timeout_seconds: d_reg_timeout(),
            offline_grace_seconds: d_offline_grace(),
            health_poll_seconds: d_health_poll(),
            cloud_init_timeout_seconds: d_ci_timeout(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHub {
    /// "owner/name" for a repo-level fleet. Mutually exclusive with `org`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// org login for an org-level fleet. Mutually exclusive with `repo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    /// Path to a file containing a PAT. Env VMFLEET_TOKEN overrides this at runtime.
    pub token_file: PathBuf,
    #[serde(default = "default_runner_group")]
    pub runner_group_id: u64,
    /// GitHub API base; override for GHES.
    #[serde(default = "default_api_base")]
    pub api_base: String,
}

fn default_runner_group() -> u64 {
    1
}
fn default_api_base() -> String {
    "https://api.github.com".to_string()
}

impl GitHub {
    /// The API path segment for registration/listing, e.g. "repos/o/n" or "orgs/o".
    pub fn scope_path(&self) -> Result<String> {
        match (&self.repo, &self.org) {
            (Some(r), None) => Ok(format!("repos/{r}")),
            (None, Some(o)) => Ok(format!("orgs/{o}")),
            (Some(_), Some(_)) => {
                bail!("config [github]: set exactly one of `repo` or `org`, not both")
            }
            (None, None) => bail!("config [github]: one of `repo` or `org` is required"),
        }
    }
    /// URL a runner registers against.
    pub fn runner_url(&self) -> Result<String> {
        match (&self.repo, &self.org) {
            (Some(r), None) => Ok(format!("https://github.com/{r}")),
            (None, Some(o)) => Ok(format!("https://github.com/{o}")),
            _ => self.scope_path().map(|_| unreachable!()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Storage {
    /// Filesystem whose free space gates VM launches (multipass vault location).
    #[serde(default = "default_vault")]
    pub vault_path: PathBuf,
}
fn default_vault() -> PathBuf {
    PathBuf::from("/var/snap/multipass/common")
}
impl Default for Storage {
    fn default() -> Self {
        Storage {
            vault_path: default_vault(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Admission {
    #[serde(default = "d_min_avail")]
    pub min_avail_mib: u64,
    #[serde(default = "d_psi_max")]
    pub psi_max: f64,
    #[serde(default = "d_min_disk")]
    pub min_disk_gib: u64,
    #[serde(default = "d_settle")]
    pub settle_seconds: u64,
    #[serde(default = "d_lock_wait")]
    pub lock_wait_seconds: u64,
    #[serde(default = "d_poll")]
    pub poll_seconds: u64,
}
fn d_min_avail() -> u64 {
    4096
}
fn d_psi_max() -> f64 {
    20.0
}
fn d_min_disk() -> u64 {
    30
}
fn d_settle() -> u64 {
    15
}
fn d_lock_wait() -> u64 {
    60
}
fn d_poll() -> u64 {
    10
}
impl Default for Admission {
    fn default() -> Self {
        Admission {
            min_avail_mib: d_min_avail(),
            psi_max: d_psi_max(),
            min_disk_gib: d_min_disk(),
            settle_seconds: d_settle(),
            lock_wait_seconds: d_lock_wait(),
            poll_seconds: d_poll(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Base {
    /// multipass image alias, e.g. "24.04".
    pub image: String,
    /// Base VM name (also the clone source). Namespaced by convention.
    #[serde(default = "d_base_name")]
    pub name: String,
    #[serde(default = "d_base_disk")]
    pub disk: String,
    #[serde(default = "d_base_cpus")]
    pub cpus: u32,
    #[serde(default = "d_base_mem")]
    pub memory: String,
    /// Provisioning scripts run (in order) inside the base VM by `build-base`.
    /// Paths are resolved relative to the config file's directory.
    #[serde(default)]
    pub setup_scripts: Vec<PathBuf>,
    /// Actions runner release to install ("latest" or a version like "2.317.0").
    #[serde(default = "d_runner_ver")]
    pub runner_version: String,
}
fn d_base_name() -> String {
    "vmfleet-base".to_string()
}
fn d_base_disk() -> String {
    "80G".to_string()
}
fn d_base_cpus() -> u32 {
    4
}
fn d_base_mem() -> String {
    "8G".to_string()
}
fn d_runner_ver() -> String {
    "latest".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pool {
    pub name: String,
    pub labels: Vec<String>,
    pub cpus: u32,
    pub memory: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap: Option<String>,
    #[serde(default)]
    pub min_warm: u32,
    pub max: u32,
    /// Base slot number for this pool (VM/unit slots are slot_base..slot_base+max).
    pub slot_base: u32,
    /// Which `[[base]]` image this pool clones from (by name). Defaults to the
    /// first base. Lets a small pool use a smaller-disk base than a large one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

impl Pool {
    pub fn labels_csv(&self) -> String {
        self.labels.join(",")
    }
    pub fn slots(&self) -> std::ops::Range<u32> {
        self.slot_base..(self.slot_base + self.max)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Supervisor {
    #[serde(default = "d_sup_poll")]
    pub poll_seconds: u64,
    #[serde(default = "d_idle_timeout")]
    pub idle_timeout_seconds: u64,
    #[serde(default = "d_max_launch")]
    pub max_launch_per_poll: u32,
    #[serde(default = "d_busy_reserve")]
    pub busy_reserve_mib: u64,
    #[serde(default = "default_true")]
    pub use_jit: bool,
    /// Query queued jobs only every Nth reconcile cycle (throttles the N+1 API
    /// cost). Between refreshes the last result is reused.
    #[serde(default = "d_queued_every")]
    pub queued_poll_every: u32,
}
fn d_sup_poll() -> u64 {
    15
}
fn d_queued_every() -> u32 {
    4
}
fn d_idle_timeout() -> u64 {
    900
}
fn d_max_launch() -> u32 {
    2
}
fn d_busy_reserve() -> u64 {
    2048
}
impl Default for Supervisor {
    fn default() -> Self {
        Supervisor {
            poll_seconds: d_sup_poll(),
            idle_timeout_seconds: d_idle_timeout(),
            max_launch_per_poll: d_max_launch(),
            busy_reserve_mib: d_busy_reserve(),
            use_jit: default_true(),
            queued_poll_every: d_queued_every(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        // Expand ~ / $HOME in config-supplied paths (TOML keeps them literal).
        cfg.github.token_file = crate::paths::expand(&cfg.github.token_file);
        cfg.storage.vault_path = crate::paths::expand(&cfg.storage.vault_path);
        for b in &mut cfg.bases {
            b.setup_scripts = b
                .setup_scripts
                .iter()
                .map(|p| crate::paths::expand(p))
                .collect();
        }
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        self.github.scope_path()?; // repo xor org
        if self.bases.is_empty() {
            bail!("config: at least one [[base]] is required");
        }
        let mut base_names = std::collections::HashSet::new();
        for b in &self.bases {
            if !base_names.insert(b.name.clone()) {
                bail!("duplicate base name `{}`", b.name);
            }
        }
        if self.pools.is_empty() {
            bail!("config: at least one [[pool]] is required");
        }
        // slot ranges must not overlap; names unique
        let mut seen_names = std::collections::HashSet::new();
        let mut ranges: Vec<(u32, u32, &str)> = Vec::new();
        for p in &self.pools {
            if p.max == 0 {
                bail!("pool `{}`: max must be > 0", p.name);
            }
            if p.min_warm > p.max {
                bail!(
                    "pool `{}`: min_warm ({}) > max ({})",
                    p.name,
                    p.min_warm,
                    p.max
                );
            }
            if p.labels.is_empty() {
                bail!("pool `{}`: at least one label required", p.name);
            }
            if !seen_names.insert(p.name.clone()) {
                bail!("duplicate pool name `{}`", p.name);
            }
            if let Some(bn) = &p.base {
                if !base_names.contains(bn) {
                    bail!(
                        "pool `{}`: base `{}` not defined in any [[base]]",
                        p.name,
                        bn
                    );
                }
            }
            ranges.push((p.slot_base, p.slot_base + p.max, &p.name));
        }
        ranges.sort();
        for w in ranges.windows(2) {
            if w[0].1 > w[1].0 {
                bail!(
                    "pool slot ranges overlap: `{}` [{}..{}) and `{}` [{}..{})",
                    w[0].2,
                    w[0].0,
                    w[0].1,
                    w[1].2,
                    w[1].0,
                    w[1].1
                );
            }
        }
        Ok(())
    }

    pub fn pool(&self, name: &str) -> Result<&Pool> {
        self.pools
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| anyhow!("no pool named `{name}`"))
    }

    /// The base image a pool clones from: its referenced `base`, else the first.
    pub fn base_for(&self, pool: &Pool) -> &Base {
        match &pool.base {
            Some(n) => self
                .bases
                .iter()
                .find(|b| &b.name == n)
                .unwrap_or(&self.bases[0]),
            None => &self.bases[0],
        }
    }

    /// Read the token from VMFLEET_TOKEN env or the configured token_file.
    pub fn token(&self) -> Result<String> {
        if let Ok(t) = std::env::var("VMFLEET_TOKEN") {
            if !t.trim().is_empty() {
                return Ok(t.trim().to_string());
            }
        }
        let t = std::fs::read_to_string(&self.github.token_file)
            .with_context(|| format!("reading token_file {}", self.github.token_file.display()))?;
        Ok(t.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> &'static str {
        r#"
[github]
repo = "owner/name"
token_file = "/tmp/tok"

[[base]]
name = "vmfleet-base"
image = "24.04"

[[base]]
name = "vmfleet-base-small"
image = "24.04"
disk = "30G"

[[pool]]
name = "small"
labels = ["self-hosted-small"]
cpus = 4
memory = "4GiB"
min_warm = 2
max = 10
slot_base = 101
base = "vmfleet-base-small"

[[pool]]
name = "large"
labels = ["self-hosted-large"]
cpus = 20
memory = "12GiB"
min_warm = 1
max = 1
slot_base = 201
"#
    }

    #[test]
    fn parses_and_validates() {
        let cfg: Config = toml::from_str(sample()).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.pools.len(), 2);
        assert_eq!(cfg.github.scope_path().unwrap(), "repos/owner/name");
        assert_eq!(cfg.pool("small").unwrap().slots(), 101..111);
        assert!(cfg.supervisor.use_jit);
        // per-pool base resolution
        assert_eq!(
            cfg.base_for(cfg.pool("small").unwrap()).name,
            "vmfleet-base-small"
        );
        assert_eq!(
            cfg.base_for(cfg.pool("large").unwrap()).name,
            "vmfleet-base"
        );
    }

    #[test]
    fn rejects_unknown_pool_base() {
        let mut cfg: Config = toml::from_str(sample()).unwrap();
        cfg.pools[0].base = Some("nope".into());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_overlapping_slots() {
        let mut cfg: Config = toml::from_str(sample()).unwrap();
        cfg.pools[1].slot_base = 105; // overlaps 101..111
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_repo_and_org() {
        let mut cfg: Config = toml::from_str(sample()).unwrap();
        cfg.github.org = Some("acme".into());
        assert!(cfg.validate().is_err());
    }
}
