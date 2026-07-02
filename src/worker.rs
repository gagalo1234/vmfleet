//! Single ephemeral VM worker — the port of clearbox `vm-runner-once`.
//! Lifecycle: sweep leftovers -> admission gate -> clone/set/start -> cloud-init
//! -> (JIT|token) register -> run.sh -> health monitor (online/UNKNOWN/ABSENT) ->
//! cleanup (deregister + purge). Cleanup runs on normal exit, error, AND on
//! SIGTERM/SIGINT (a dedicated sigwait thread), matching the bash trap semantics.

use crate::cmd::SystemRunner;
use crate::config::{Config, Pool};
use crate::github::Client;
use crate::multipass::Multipass;
use crate::{admission, naming, paths};
use anyhow::{bail, Context, Result};
use nix::sys::signal::Signal;
use nix::sys::signalfd::SigSet;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

enum RunnerState {
    Online,
    Offline,
    Absent,
    Unknown,
}

struct Ctx {
    runner: SystemRunner,
    github: Client,
    vm_name: String,
    state_file: std::path::PathBuf,
    created: AtomicBool,
    run_child: Mutex<Option<Child>>,
    cleaned: AtomicBool,
}

impl Ctx {
    fn mp(&self) -> Multipass<'_> {
        Multipass::new(&self.runner)
    }

    fn runner_state(&self) -> RunnerState {
        match self.github.list_runners() {
            Err(_) => RunnerState::Unknown,
            Ok(list) => match list.iter().find(|r| r.name == self.vm_name) {
                None => RunnerState::Absent,
                Some(r) if r.online() => RunnerState::Online,
                Some(_) => RunnerState::Offline,
            },
        }
    }

    fn cleanup(&self) {
        if self.cleaned.swap(true, Ordering::SeqCst) {
            return;
        }
        tracing::info!("cleanup: begin {}", self.vm_name);
        // 1. stop the host-side run.sh exec child
        if let Some(mut child) = self.run_child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // 2. deregister the runner record (by name)
        if let Ok(list) = self.github.list_runners() {
            for r in list.iter().filter(|r| r.name == self.vm_name) {
                let _ = self.github.delete_runner(r.id);
            }
        }
        // 3. purge the VM (definitive: also kills any in-VM run.sh)
        if self.created.load(Ordering::SeqCst) {
            if let Err(e) = self.mp().purge(&self.vm_name) {
                tracing::warn!("cleanup: purge {} failed: {e}", self.vm_name);
            }
        }
        let _ = std::fs::remove_file(&self.state_file);
        tracing::info!("cleanup: end {}", self.vm_name);
    }
}

pub fn run(cfg: &Config, pool_name: &str, slot: u32) -> Result<()> {
    let pool = cfg.pool(pool_name)?.clone();
    let token = cfg.token()?;
    let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let vm_name = naming::vm_name(pool_name, slot, ts, std::process::id());

    std::fs::create_dir_all(paths::state_dir().join("slots")).ok();
    let ctx = Arc::new(Ctx {
        runner: SystemRunner::new(),
        github: Client::new(&cfg.github, token.clone())?,
        vm_name: vm_name.clone(),
        state_file: paths::slot_state(slot),
        created: AtomicBool::new(false),
        run_child: Mutex::new(None),
        cleaned: AtomicBool::new(false),
    });

    // --- signal-driven cleanup (bash-trap equivalent) ---------------------
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGTERM);
    mask.add(Signal::SIGINT);
    mask.thread_block().context("blocking signals")?;
    {
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut set = SigSet::empty();
            set.add(Signal::SIGTERM);
            set.add(Signal::SIGINT);
            if let Ok(sig) = set.wait() {
                tracing::info!("received {sig:?}, cleaning up");
                ctx.cleanup();
                std::process::exit(if sig == Signal::SIGINT { 130 } else { 143 });
            }
        });
    }
    // Ensure cleanup on any return path from here on.
    let _guard = CleanupGuard(ctx.clone());

    let result = lifecycle(cfg, &pool, pool_name, slot, &ctx);
    // explicit cleanup happens via _guard drop; surface the result
    result
}

struct CleanupGuard(Arc<Ctx>);
impl Drop for CleanupGuard {
    fn drop(&mut self) {
        self.0.cleanup();
    }
}

fn lifecycle(cfg: &Config, pool: &Pool, pool_name: &str, slot: u32, ctx: &Arc<Ctx>) -> Result<()> {
    let mp = ctx.mp();
    let w = &cfg.worker;

    if !mp.available() {
        bail!("multipass daemon not available");
    }
    if !mp.exists(&cfg.base.name) {
        bail!(
            "base VM does not exist: {} (run `vmfleet build-base`)",
            cfg.base.name
        );
    }
    ctx.github.reachable().context("GitHub not reachable")?;

    sweep_leftovers(ctx, pool_name, slot)?;

    std::fs::write(&ctx.state_file, format!("{}\n", ctx.vm_name)).ok();

    // Token flow needs a registration token up front; JIT is generated just
    // before run.sh to minimize the dangling-registration window.
    let reg_token = if cfg.supervisor.use_jit {
        None
    } else {
        Some(
            ctx.github
                .registration_token()
                .context("registration token")?,
        )
    };

    // --- admission gate: hold across clone+start, release after settle -----
    let gate = admission::Gate::new(
        paths::admission_lock(),
        cfg.admission.clone(),
        cfg.storage.vault_path.clone(),
    );
    let guard = gate.acquire()?;
    tracing::info!("admission granted for {}", ctx.vm_name);

    mp.clone_from(&cfg.base.name, &ctx.vm_name)?;
    ctx.created.store(true, Ordering::SeqCst);
    mp.set_cpus(&ctx.vm_name, pool.cpus)?;
    mp.set_memory(&ctx.vm_name, &pool.memory)?;
    mp.start(&ctx.vm_name)?;

    gate.settle();
    drop(guard); // release the launch lock; next worker may proceed

    wait_cloud_init(&mp, &ctx.vm_name, w.cloud_init_timeout_seconds)?;

    if let Some(swap) = &pool.swap {
        setup_swap(&mp, &ctx.vm_name, swap, w.config_timeout_seconds)?;
    }

    // --- register + run ----------------------------------------------------
    let labels = pool.labels_csv();
    if let Some(tok) = &reg_token {
        let script = format!(
            "set -euo pipefail; cd ~/actions-runner; ./config.sh --unattended \
             --url '{}' --token '{}' --name '{}' --labels '{}' --ephemeral --replace",
            cfg.github.runner_url()?,
            tok,
            ctx.vm_name,
            labels
        );
        let o = mp.exec(
            &ctx.vm_name,
            &script,
            Duration::from_secs(w.config_timeout_seconds),
        )?;
        if !o.ok() {
            bail!("config.sh failed: {}", o.stderr.trim());
        }
    }

    let run_cmd = if cfg.supervisor.use_jit {
        let jit = ctx
            .github
            .generate_jitconfig(&ctx.vm_name, &pool.labels)
            .context("generate-jitconfig")?;
        format!("cd ~/actions-runner && ./run.sh --jitconfig '{jit}'")
    } else {
        "cd ~/actions-runner && ./run.sh".to_string()
    };
    let child = Command::new("multipass")
        .args(["exec", &ctx.vm_name, "--", "bash", "-lc", &run_cmd])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning run.sh")?;
    *ctx.run_child.lock().unwrap() = Some(child);

    wait_registration(ctx, w.registration_timeout_seconds, w.health_poll_seconds)?;
    health_monitor(ctx, w.offline_grace_seconds, w.health_poll_seconds);
    Ok(())
}

fn child_alive(ctx: &Ctx) -> bool {
    let mut g = ctx.run_child.lock().unwrap();
    match g.as_mut() {
        Some(c) => matches!(c.try_wait(), Ok(None)),
        None => false,
    }
}

fn wait_cloud_init(mp: &Multipass, vm: &str, timeout: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        let o = mp.exec(
            vm,
            "cloud-init status --wait",
            Duration::from_secs(timeout.min(120)),
        );
        if let Ok(o) = o {
            if o.ok() {
                return Ok(());
            }
        }
        if Instant::now() > deadline {
            bail!("VM did not become ready: {vm}");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn setup_swap(mp: &Multipass, vm: &str, size: &str, timeout: u64) -> Result<()> {
    let script = format!(
        "set -euo pipefail; f=/swapfile; \
         if swapon --show=NAME --noheadings | grep -qx \"$f\"; then exit 0; fi; \
         sudo swapoff \"$f\" 2>/dev/null || true; sudo rm -f \"$f\"; \
         sudo fallocate -l {size} \"$f\"; sudo chmod 600 \"$f\"; sudo mkswap \"$f\" >/dev/null; \
         sudo swapon \"$f\"; \
         grep -qE '^/swapfile ' /etc/fstab || echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab >/dev/null"
    );
    let _ = mp.exec(vm, &script, Duration::from_secs(timeout));
    Ok(())
}

fn wait_registration(ctx: &Ctx, timeout: u64, poll: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    while child_alive(ctx) {
        if matches!(
            ctx.runner_state(),
            RunnerState::Online | RunnerState::Offline
        ) {
            tracing::info!("{} registered", ctx.vm_name);
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("runner did not register within {timeout}s: {}", ctx.vm_name);
        }
        std::thread::sleep(Duration::from_secs(poll));
    }
    // child exited before registering
    bail!("run.sh exited before registration: {}", ctx.vm_name);
}

fn health_monitor(ctx: &Ctx, offline_grace: u64, poll: u64) {
    let mut unhealthy_since: Option<Instant> = None;
    while child_alive(ctx) {
        match ctx.runner_state() {
            RunnerState::Online => {
                unhealthy_since = None;
            }
            RunnerState::Unknown => {
                // host-side query jitter; don't count it
            }
            RunnerState::Absent | RunnerState::Offline => {
                let since = *unhealthy_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_secs(offline_grace) {
                    tracing::warn!("{} unhealthy for {offline_grace}s; recycling", ctx.vm_name);
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(poll));
    }
}

/// Purge stale VMs / runner records for this slot left by a crash or a killed
/// prior worker. Only ever touches this slot's own `vmfleet-<pool>-<slot>-` VMs.
fn sweep_leftovers(ctx: &Ctx, pool_name: &str, slot: u32) -> Result<()> {
    let mp = ctx.mp();
    let prefix = naming::slot_vm_prefix(pool_name, slot);
    // from prior state file
    if let Ok(old) = std::fs::read_to_string(&ctx.state_file) {
        let old = old.trim();
        if !old.is_empty() && old != ctx.vm_name && old.starts_with(&prefix) {
            reap(ctx, &mp, old);
        }
    }
    // from multipass list
    if let Ok(vms) = mp.list_prefixed(&prefix) {
        for (name, _) in vms {
            if name != ctx.vm_name {
                reap(ctx, &mp, &name);
            }
        }
    }
    Ok(())
}

fn reap(ctx: &Ctx, mp: &Multipass, name: &str) {
    tracing::info!("sweeping stale VM {name}");
    if let Ok(list) = ctx.github.list_runners() {
        for r in list.iter().filter(|r| r.name == name) {
            let _ = ctx.github.delete_runner(r.id);
        }
    }
    let _ = mp.purge(name);
}
