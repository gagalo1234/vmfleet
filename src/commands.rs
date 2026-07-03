//! User-facing lifecycle commands: doctor, status, gc, install, uninstall,
//! build-base, scale. These orchestrate the wrapper modules; the heavy logic
//! lives in worker/supervisor.

use crate::cmd::SystemRunner;
use crate::config::{Admission, Base, Config, GitHub, Pool, Storage, Supervisor, Worker};
use crate::github::{self, Client};
use crate::multipass::{Multipass, VmState};
use crate::systemd::Systemd;
use crate::{naming, paths, resources};
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::time::Duration;

fn current_user() -> String {
    std::env::var("USER").unwrap_or_else(|_| "root".into())
}

// ===================== doctor =====================
pub fn doctor(cfg_path: &Path) -> Result<()> {
    let runner = SystemRunner::new();
    let mp = Multipass::new(&runner);
    let sd = Systemd::new(&runner);
    let mut fails = 0;
    let mut check = |name: &str, ok: bool, detail: String| {
        let tag = if ok {
            "PASS"
        } else {
            fails += 1;
            "FAIL"
        };
        println!("[{tag}] {name}: {detail}");
    };

    check("multipass", mp.available(), "daemon reachable".into());

    let cfg = if cfg_path.exists() {
        match Config::load(cfg_path) {
            Ok(c) => {
                check("config", true, format!("{} valid", cfg_path.display()));
                Some(c)
            }
            Err(e) => {
                check("config", false, format!("{e}"));
                None
            }
        }
    } else {
        check(
            "config",
            false,
            format!("missing: {} (run `vmfleet install`)", cfg_path.display()),
        );
        None
    };

    let avail = resources::mem_available_mib();
    let psi = resources::mem_psi_some_avg10();
    check(
        "memory",
        true,
        format!("MemAvailable {avail} MiB, PSI some avg10 {psi}%"),
    );
    check(
        "linger",
        sd.linger_enabled(&current_user()),
        format!("user {}", current_user()),
    );

    if let Some(cfg) = cfg {
        let disk = resources::disk_free_gib(&cfg.storage.vault_path).unwrap_or(0);
        check(
            "disk",
            disk >= cfg.admission.min_disk_gib,
            format!("{disk} GiB free at {}", cfg.storage.vault_path.display()),
        );
        for b in &cfg.bases {
            check(
                &format!("base VM {}", b.name),
                mp.exists(&b.name),
                b.name.clone(),
            );
        }
        match cfg
            .token()
            .and_then(|t| github::check_token(&cfg.github, &t))
        {
            Ok(_) => check(
                "github token",
                true,
                format!("scope {}", cfg.github.scope_path()?),
            ),
            Err(e) => check("github token", false, format!("{e}")),
        }
        // token file permissions (warn only)
        #[cfg(unix)]
        if let Ok(meta) = std::fs::metadata(&cfg.github.token_file) {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                println!(
                    "[WARN] token perms: {mode:o} on {} (recommend 0600)",
                    cfg.github.token_file.display()
                );
            }
        }
    }

    if fails > 0 {
        bail!("doctor: {fails} check(s) failed");
    }
    println!("doctor: all checks passed");
    Ok(())
}

// ===================== status =====================
pub fn status(cfg_path: &Path) -> Result<()> {
    match std::fs::read_to_string(paths::status_file()) {
        Ok(s) => {
            println!("# supervisor status ({})", paths::status_file().display());
            println!("{s}");
        }
        Err(_) => println!("(no status yet — is the supervisor running?)"),
    }
    let runner = SystemRunner::new();
    let sd = Systemd::new(&runner);
    let units = sd.list_active(naming::WORKER_UNIT_GLOB).unwrap_or_default();
    println!("\nactive worker units: {}", units.len());
    for u in units {
        println!("  {u}");
    }
    if cfg_path.exists() {
        if let Ok(cfg) = Config::load(cfg_path) {
            if let Ok(c) = Client::new(&cfg.github, cfg.token()?) {
                if let Ok(rs) = c.list_runners() {
                    let vmf: Vec<_> = rs
                        .iter()
                        .filter(|r| r.name.starts_with(naming::VM_PREFIX))
                        .collect();
                    println!("\ngithub vmfleet runners: {}", vmf.len());
                    for r in vmf {
                        println!("  {} {} busy={}", r.name, r.status, r.busy);
                    }
                }
            }
        }
    }
    Ok(())
}

// ===================== gc =====================
/// Purge orphan vmfleet VMs (no active worker unit for their slot) and delete
/// offline vmfleet runner records. Never touches non-vmfleet resources.
pub fn gc(cfg_path: &Path) -> Result<()> {
    let cfg = Config::load(cfg_path)?;
    let runner = SystemRunner::new();
    let mp = Multipass::new(&runner);
    let sd = Systemd::new(&runner);

    let active: std::collections::HashSet<u32> = sd
        .list_active(naming::WORKER_UNIT_GLOB)
        .unwrap_or_default()
        .iter()
        .filter_map(|u| {
            u.strip_prefix("vmfleet-worker-")
                .and_then(|s| s.strip_suffix(".service"))
                .and_then(|s| s.parse().ok())
        })
        .collect();

    let mut purged = 0;
    for (name, state) in mp.list_prefixed(naming::VM_PREFIX)? {
        if cfg.bases.iter().any(|b| b.name == name) {
            continue;
        }
        // find slot for this VM across pools
        let slot = cfg
            .pools
            .iter()
            .find_map(|p| naming::slot_of_vm(&p.name, &name));
        let orphan = match slot {
            Some(s) => !active.contains(&s),
            None => true, // unrecognizable vmfleet VM -> orphan
        };
        if orphan {
            println!("gc: purging orphan VM {name} (state {:?})", state);
            let _ = mp.purge(&name);
            purged += 1;
        }
    }

    let mut dereg = 0;
    if let Ok(c) = Client::new(&cfg.github, cfg.token()?) {
        if let Ok(rs) = c.list_runners() {
            for r in rs
                .iter()
                .filter(|r| r.name.starts_with(naming::VM_PREFIX) && !r.online())
            {
                let _ = c.delete_runner(r.id);
                dereg += 1;
            }
        }
    }
    println!("gc: purged {purged} orphan VM(s), deregistered {dereg} offline runner record(s)");
    Ok(())
}

// ===================== scale =====================
pub fn scale(cfg_path: &Path, pool: &str, min: Option<u32>, max: Option<u32>) -> Result<()> {
    let mut cfg = Config::load(cfg_path)?;
    {
        let p = cfg
            .pools
            .iter_mut()
            .find(|p| p.name == pool)
            .with_context(|| format!("no pool `{pool}`"))?;
        if let Some(m) = min {
            p.min_warm = m;
        }
        if let Some(m) = max {
            p.max = m;
        }
    }
    cfg.validate()?;
    write_config(&cfg, cfg_path)?;
    let p = cfg.pool(pool)?;
    println!(
        "pool `{pool}`: min_warm={} max={} (supervisor picks this up next cycle)",
        p.min_warm, p.max
    );
    Ok(())
}

fn write_config(cfg: &Config, path: &Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let toml = toml::to_string_pretty(cfg)?;
    std::fs::write(path, toml).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

// ===================== install =====================
pub struct InstallOpts {
    pub non_interactive: bool,
    pub dry_run: bool,
    pub upgrade: bool,
}

pub fn install(cfg_path: &Path, opts: &InstallOpts) -> Result<()> {
    let runner = SystemRunner::new();
    let sd = Systemd::new(&runner);
    let mp = Multipass::new(&runner);

    if !mp.available() {
        bail!("multipass not available — install it first (snap install multipass)");
    }

    // 1. obtain/validate config
    let cfg = if cfg_path.exists() {
        let mut cfg = Config::load(cfg_path)?;
        if opts.upgrade {
            if crate::config::migrate(&mut cfg) {
                if opts.dry_run {
                    println!(
                        "dry-run: would migrate config to version {}",
                        crate::config::CURRENT_VERSION
                    );
                } else {
                    write_config(&cfg, cfg_path)?;
                    println!(
                        "migrated config to version {}",
                        crate::config::CURRENT_VERSION
                    );
                }
            } else {
                println!("config already at version {}", cfg.version);
            }
        } else {
            println!("using existing config {}", cfg_path.display());
        }
        cfg
    } else if opts.non_interactive {
        bail!(
            "no config at {} and --non-interactive set; provide --config",
            cfg_path.display()
        );
    } else {
        let cfg = prompt_config(cfg_path)?;
        if opts.dry_run {
            println!(
                "--dry-run: would write config:\n{}",
                toml::to_string_pretty(&cfg)?
            );
        } else {
            write_config(&cfg, cfg_path)?;
            println!("wrote {}", cfg_path.display());
        }
        cfg
    };

    // 2. linger
    if opts.dry_run {
        println!(
            "--dry-run: would `loginctl enable-linger {}`",
            current_user()
        );
    } else if !sd.linger_enabled(&current_user()) {
        sd.ensure_linger(&current_user())?;
        println!("enabled linger for {}", current_user());
    }

    // 3. supervisor unit
    let exe = std::env::current_exe()?;
    let unit = supervisor_unit_contents(&exe, cfg_path);
    let unit_path = paths::systemd_user_dir().join(naming::SUPERVISOR_UNIT);
    if opts.dry_run {
        println!(
            "--dry-run: would write {} and enable it",
            unit_path.display()
        );
    } else {
        std::fs::create_dir_all(paths::systemd_user_dir())?;
        std::fs::write(&unit_path, unit)?;
        // 3b. periodic GC timer
        install_gc_timer(&exe, cfg_path)?;
        sd.daemon_reload()?;
        sd.enable_now(naming::SUPERVISOR_UNIT)?;
        sd.enable_now(naming::GC_TIMER)?;
        if opts.upgrade {
            sd.restart(naming::SUPERVISOR_UNIT)?;
            println!("upgraded: reinstalled units and restarted supervisor");
        } else {
            println!(
                "installed and started {} + {}",
                naming::SUPERVISOR_UNIT,
                naming::GC_TIMER
            );
        }
    }

    for b in &cfg.bases {
        if !mp.exists(&b.name) {
            println!(
                "\nNOTE: base VM `{}` not found — run `vmfleet build-base` before runners can start.",
                b.name
            );
        }
    }
    println!("\ninstall complete. Check: vmfleet status   |   vmfleet doctor");
    Ok(())
}

/// Absolute path for embedding in a systemd unit (user services run from $HOME,
/// so relative config paths would not resolve). Does not require the file to exist.
fn abs(p: &Path) -> std::path::PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

fn install_gc_timer(exe: &Path, cfg_path: &Path) -> Result<()> {
    let dir = paths::systemd_user_dir();
    std::fs::create_dir_all(&dir)?;
    // Quote paths so spaces don't split into extra ExecStart arguments.
    let svc = format!(
        "[Unit]\nDescription=vmfleet orphan GC\n\n[Service]\nType=oneshot\nExecStart=\"{}\" --config \"{}\" gc\n",
        exe.display(),
        abs(cfg_path).display()
    );
    let timer = "[Unit]\nDescription=vmfleet orphan GC timer\n\n[Timer]\nOnCalendar=hourly\nPersistent=true\n\n[Install]\nWantedBy=timers.target\n";
    std::fs::write(dir.join(naming::GC_SERVICE), svc)?;
    std::fs::write(dir.join(naming::GC_TIMER), timer)?;
    Ok(())
}

fn supervisor_unit_contents(exe: &Path, cfg_path: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=vmfleet autoscaling runner supervisor\n\
         After=default.target\n\n\
         [Service]\n\
         Type=simple\n\
         ExecStart=\"{}\" --config \"{}\" supervisor\n\
         Restart=always\n\
         RestartSec=10\n\
         KillMode=process\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        exe.display(),
        abs(cfg_path).display()
    )
}

fn prompt_config(cfg_path: &Path) -> Result<Config> {
    use dialoguer::{Confirm, Input};
    println!("== vmfleet guided install ==");
    let scope: String = Input::new()
        .with_prompt("GitHub repo (owner/name) or org (@org)")
        .interact_text()?;
    let (repo, org) = if let Some(o) = scope.strip_prefix('@') {
        (None, Some(o.to_string()))
    } else {
        (Some(scope), None)
    };
    let token_file: String = Input::new()
        .with_prompt("Path to PAT token file")
        .default(
            paths::config_dir()
                .join("token")
                .to_string_lossy()
                .into_owned(),
        )
        .interact_text()?;
    if !Path::new(&token_file).exists() {
        let tok: String = Input::new()
            .with_prompt("Paste PAT (stored 0600)")
            .interact_text()?;
        if let Some(d) = Path::new(&token_file).parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&token_file, tok.trim())?;
        set_mode_600(&token_file);
    }
    let vault: String = Input::new()
        .with_prompt("Multipass vault path (disk gate)")
        .default("/var/snap/multipass/common".into())
        .interact_text()?;
    let use_jit = Confirm::new()
        .with_prompt("Use JIT runner registration?")
        .default(true)
        .interact()?;

    let small_max: u32 = Input::new()
        .with_prompt("small pool: max runners")
        .default(10u32)
        .interact_text()?;
    let small_warm: u32 = Input::new()
        .with_prompt("small pool: warm (min idle)")
        .default(2u32)
        .interact_text()?;
    let small_label: String = Input::new()
        .with_prompt("small pool: runner label")
        .default("self-hosted-small".into())
        .interact_text()?;

    let base_image: String = Input::new()
        .with_prompt("base VM image")
        .default("24.04".into())
        .interact_text()?;

    let cfg = Config {
        version: crate::config::CURRENT_VERSION,
        github: GitHub {
            repo,
            org,
            token_file: token_file.into(),
            runner_group_id: 1,
            api_base: "https://api.github.com".into(),
        },
        storage: Storage {
            vault_path: vault.into(),
        },
        admission: Admission::default(),
        bases: vec![Base {
            image: base_image,
            name: "vmfleet-base".into(),
            disk: "80G".into(),
            cpus: 4,
            memory: "8G".into(),
            setup_scripts: vec![],
            runner_version: "latest".into(),
        }],
        pools: vec![Pool {
            name: "small".into(),
            labels: vec![small_label],
            cpus: 4,
            memory: "4GiB".into(),
            swap: Some("8G".into()),
            min_warm: small_warm,
            max: small_max,
            slot_base: 101,
            base: None,
        }],
        supervisor: Supervisor {
            use_jit,
            ..Default::default()
        },
        worker: Worker::default(),
    };
    cfg.validate()?;
    let _ = cfg_path;
    Ok(cfg)
}

fn set_mode_600(path: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

// ===================== uninstall =====================
pub struct UninstallOpts {
    pub purge_all: bool,
    pub yes: bool,
}

pub fn uninstall(cfg_path: &Path, opts: &UninstallOpts) -> Result<()> {
    let runner = SystemRunner::new();
    let sd = Systemd::new(&runner);
    let mp = Multipass::new(&runner);

    if !opts.yes {
        use dialoguer::Confirm;
        let msg = if opts.purge_all {
            "This will stop the fleet, purge ALL vmfleet VMs/runners, and delete base+config+state. Continue?"
        } else {
            "This will stop the fleet and purge all vmfleet VMs/runners (config kept). Continue?"
        };
        if !Confirm::new().with_prompt(msg).default(false).interact()? {
            println!("aborted");
            return Ok(());
        }
    }

    // 1. stop supervisor + gc timer + workers
    let _ = sd.disable_now(naming::SUPERVISOR_UNIT);
    let _ = sd.disable_now(naming::GC_TIMER);
    for u in [
        naming::SUPERVISOR_UNIT,
        naming::GC_TIMER,
        naming::GC_SERVICE,
    ] {
        let _ = std::fs::remove_file(paths::systemd_user_dir().join(u));
    }
    for unit in sd.list_active(naming::WORKER_UNIT_GLOB).unwrap_or_default() {
        println!("stopping {unit}");
        let _ = sd.stop(&unit);
    }
    let _ = sd.daemon_reload();

    // 2. deregister vmfleet runners + purge vmfleet VMs
    let cfg = Config::load(cfg_path).ok();
    if let Some(cfg) = &cfg {
        if let Ok(c) = Client::new(&cfg.github, cfg.token()?) {
            match github::delete_runners_by_prefix(&c, naming::VM_PREFIX) {
                Ok(n) => println!("deregistered {n} runner record(s)"),
                Err(e) => println!("runner deregister warning: {e}"),
            }
        }
    }
    for (name, _) in mp.list_prefixed(naming::VM_PREFIX)? {
        if let Some(cfg) = &cfg {
            if cfg.bases.iter().any(|b| b.name == name) && !opts.purge_all {
                continue; // keep base(s) unless purge_all
            }
        }
        println!("purging VM {name}");
        let _ = mp.purge(&name);
    }

    // 3. purge-all: config/secrets/state
    if opts.purge_all {
        if let Some(cfg) = &cfg {
            let _ = std::fs::remove_file(&cfg.github.token_file);
        }
        let _ = std::fs::remove_dir_all(paths::state_dir());
        let _ = std::fs::remove_file(cfg_path);
        println!("removed config, secrets, and state");
    }
    println!("uninstall complete");
    Ok(())
}

// ===================== build-base =====================
pub fn build_base(cfg_path: &Path, force: bool) -> Result<()> {
    let cfg = Config::load(cfg_path)?;
    let runner = SystemRunner::new();
    let mp = Multipass::new(&runner);
    let base_dir = cfg_path.parent().unwrap_or(Path::new("."));
    for base in &cfg.bases {
        build_one_base(&mp, base, base_dir, force)?;
    }
    Ok(())
}

fn build_one_base(mp: &Multipass, base: &Base, base_dir: &Path, force: bool) -> Result<()> {
    if mp.exists(&base.name) {
        if !force {
            bail!(
                "base VM `{}` already exists; pass --force to rebuild",
                base.name
            );
        }
        println!("rebuilding: purging existing {}", base.name);
        mp.purge(&base.name)?;
    }

    println!(
        "launching base {} ({} {} cpus {} mem {} disk)",
        base.name, base.image, base.cpus, base.memory, base.disk
    );
    mp.launch(&base.image, &base.name, base.cpus, &base.memory, &base.disk)?;
    let _ = mp.exec(
        &base.name,
        "cloud-init status --wait",
        Duration::from_secs(300),
    );

    // run provisioning scripts (paths relative to config dir)
    for (i, script) in base.setup_scripts.iter().enumerate() {
        let local = if script.is_absolute() {
            script.clone()
        } else {
            base_dir.join(script)
        };
        if !local.exists() {
            bail!("setup script not found: {}", local.display());
        }
        let remote = format!("/tmp/vmfleet-provision-{i}.sh");
        println!(
            "provisioning {} [{}/{}]: {}",
            base.name,
            i + 1,
            base.setup_scripts.len(),
            script.display()
        );
        mp.transfer(&local.to_string_lossy(), &base.name, &remote)?;
        let run = format!(
            "chmod +x {remote} && RUNNER_VERSION='{}' sudo -E bash {remote}",
            base.runner_version
        );
        let o = mp.exec(&base.name, &run, Duration::from_secs(1800))?;
        if !o.ok() {
            bail!(
                "provision script {} failed: {}",
                script.display(),
                o.stderr.trim()
            );
        }
    }

    // clone source must be Stopped
    println!("stopping base (clone source must be stopped)");
    let _ = mp.stop(&base.name);
    if let Ok(list) = mp.list() {
        if list
            .iter()
            .any(|(n, s)| n == &base.name && *s == VmState::Running)
        {
            println!(
                "warning: base still Running; run `multipass stop {}`",
                base.name
            );
        }
    }
    println!("base image `{}` ready", base.name);
    Ok(())
}
