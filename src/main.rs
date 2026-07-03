//! vmfleet — single-host autoscaling fleet of ephemeral Multipass-VM GitHub
//! Actions runners. One static binary, one config file, guided install/uninstall.

mod admission;
mod cmd;
mod commands;
mod config;
mod github;
mod multipass;
mod naming;
mod paths;
mod resources;
mod supervisor;
mod systemd;
mod worker;

#[cfg(test)]
mod e2e_offline;
#[cfg(test)]
mod testsupport;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "vmfleet",
    version,
    about = "Autoscaling ephemeral VM runner fleet for a single host"
)]
struct Cli {
    /// Path to config (default: ~/.config/vmfleet/vmfleet.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Guided install: preflight, write config, install & enable the supervisor.
    Install(InstallArgs),
    /// Complete teardown of this fleet (VMs, runners, units; optionally config/base).
    Uninstall(UninstallArgs),
    /// Run the autoscaling control-plane loop (systemd ExecStart).
    Supervisor {
        /// Run a single reconcile pass and exit instead of looping forever.
        #[arg(long)]
        once: bool,
        /// Compute the reconcile decision and print it as JSON, without launching
        /// or stopping anything (and without touching the live status file). Safe to
        /// run alongside a live supervisor — previews "what would it do right now".
        #[arg(long)]
        dry_run: bool,
    },
    /// Run a single ephemeral VM worker for a pool+slot (spawned by the supervisor).
    Worker { pool: String, slot: u32 },
    /// Show fleet status (pools, workers, host resources).
    Status,
    /// Preflight/health checks.
    Doctor,
    /// Build or rebuild the base VM image from the provisioning manifest.
    BuildBase {
        #[arg(long)]
        force: bool,
    },
    /// Garbage-collect orphan VMs and stale runner records.
    Gc,
    /// Adjust a pool's min/max at runtime (writes config).
    Scale {
        pool: String,
        #[arg(long)]
        min: Option<u32>,
        #[arg(long)]
        max: Option<u32>,
    },
    /// Validate the config file.
    ConfigCheck,
}

#[derive(clap::Args)]
struct InstallArgs {
    #[arg(long)]
    non_interactive: bool,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    upgrade: bool,
}

#[derive(clap::Args)]
struct UninstallArgs {
    /// Also remove base VM, config, secrets and state.
    #[arg(long)]
    purge_all: bool,
    /// Skip confirmation prompts.
    #[arg(long)]
    yes: bool,
}

fn config_path(cli: &Cli) -> PathBuf {
    cli.config.clone().unwrap_or_else(paths::config_file)
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("VMFLEET_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .without_time()
        .init();

    if let Err(e) = run() {
        eprintln!("vmfleet: error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let cfg_path = config_path(&cli);

    match &cli.cmd {
        Cmd::ConfigCheck => {
            let cfg = config::Config::load(&cfg_path)?;
            println!(
                "OK: {} — {} pool(s), scope {}",
                cfg_path.display(),
                cfg.pools.len(),
                cfg.github.scope_path()?
            );
            Ok(())
        }
        Cmd::Doctor => commands::doctor(&cfg_path),
        Cmd::Status => commands::status(&cfg_path),
        Cmd::Gc => commands::gc(&cfg_path),
        Cmd::Scale { pool, min, max } => commands::scale(&cfg_path, pool, *min, *max),
        Cmd::BuildBase { force } => commands::build_base(&cfg_path, *force),
        Cmd::Install(a) => commands::install(
            &cfg_path,
            &commands::InstallOpts {
                non_interactive: a.non_interactive,
                dry_run: a.dry_run,
                upgrade: a.upgrade,
            },
        ),
        Cmd::Uninstall(a) => commands::uninstall(
            &cfg_path,
            &commands::UninstallOpts {
                purge_all: a.purge_all,
                yes: a.yes,
            },
        ),
        Cmd::Supervisor { once, dry_run } => {
            let cfg = config::Config::load(&cfg_path)?;
            supervisor::run(&cfg, &cfg_path, *once, *dry_run)
        }
        Cmd::Worker { pool, slot } => {
            let cfg = config::Config::load(&cfg_path)?;
            worker::run(&cfg, pool, *slot)
        }
    }
}
