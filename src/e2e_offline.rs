//! Offline end-to-end test of one `reconcile` pass. The real `github::Client` is
//! pointed at a localhost fixture server (`api_base`); systemd is a `RecordingRunner`;
//! host resources are neutralised through admission config. This exercises the
//! launch, scale-down and admission-blocked branches — plus the dry-run no-op
//! guarantee — without a real Multipass/systemd/GitHub anywhere in sight.

use crate::admission::Gate;
use crate::config::Config;
use crate::github::Client;
use crate::supervisor::{reconcile, Report, SupState};
use crate::systemd::Systemd;
use crate::testsupport::{FixtureGitHub, RecordingRunner};
use std::path::PathBuf;
use std::sync::Once;

static INIT: Once = Once::new();

/// A private temp dir used for XDG_DATA_HOME (so `status.json`/metrics writes land
/// here instead of clobbering a real running supervisor's state) and the token
/// file / disk-free vault path. Set once for the whole test binary.
fn state_home() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vmfleet-e2e-{}", std::process::id()));
    INIT.call_once(|| {
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_DATA_HOME", &dir);
        std::fs::write(dir.join("token"), "x").unwrap();
    });
    dir
}

/// Build a validated config with the given `[[pool]]` TOML, pointed at `fx`.
fn config(fx: &FixtureGitHub, pools_toml: &str) -> Config {
    let dir = state_home();
    let toml = format!(
        r#"
[github]
repo = "owner/name"
token_file = "{token}"

[[base]]
name = "vmfleet-base"
image = "24.04"

{pools_toml}
"#,
        token = dir.join("token").display(),
    );
    let mut cfg: Config = toml::from_str(&toml).unwrap();
    cfg.github.api_base = fx.base_url.clone();
    // Give each test its own vault dir. reconcile only *reads* disk-free from it
    // (it never acquires the admission lock — that's the worker launch path), but
    // isolating keeps the tests independent and future-proof.
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let vault = dir.join(format!("vault-{id}"));
    std::fs::create_dir_all(&vault).unwrap();
    cfg.storage.vault_path = vault;
    // Neutralise the resource gate by default: never blocked, headroom unbounded.
    cfg.admission.min_avail_mib = 0;
    cfg.admission.psi_max = f64::MAX;
    cfg.admission.min_disk_gib = 0;
    cfg.supervisor.busy_reserve_mib = 1;
    cfg.validate().unwrap();
    cfg
}

/// Drive one reconcile pass with a fresh state, returning the decision + the mock's
/// recorded systemd calls.
fn run_once(cfg: &Config, runner: &RecordingRunner, dry_run: bool) -> Report {
    let sd = Systemd::new(runner);
    let client = Client::new(&cfg.github, cfg.token().unwrap()).unwrap();
    let gate = Gate::new(
        cfg.storage.vault_path.join("admission.lock"),
        cfg.admission.clone(),
        cfg.storage.vault_path.clone(),
    );
    let exe = std::env::current_exe().unwrap();
    let cfg_path = PathBuf::from("/test/vmfleet.toml");
    let mut state = SupState::default();
    reconcile(
        cfg, &cfg_path, &client, &sd, &gate, &exe, &mut state, dry_run,
    )
    .unwrap()
}

fn pool<'a>(report: &'a Report, name: &str) -> &'a crate::supervisor::PoolReport {
    report.pools.iter().find(|p| p.name == name).unwrap()
}

const SMALL_UP: &str = r#"
[[pool]]
name = "small"
labels = ["self-hosted-small"]
cpus = 2
memory = "4GiB"
min_warm = 2
max = 10
slot_base = 101
"#;

#[test]
fn scale_up_launches_into_free_slots() {
    let fx = FixtureGitHub::start(r#"{"runners":[]}"#);
    let cfg = config(&fx, SMALL_UP);
    let runner = RecordingRunner::default();

    let report = run_once(&cfg, &runner, false);

    let p = pool(&report, "small");
    assert_eq!(p.desired, 2, "min_warm=2 with no demand => want 2");
    assert_eq!(p.present, 0);
    assert_eq!(p.deficit, 2);
    assert!(p.actions.contains(&"launch:101".to_string()));
    assert!(p.actions.contains(&"launch:102".to_string()));

    // The launch must reach systemd-run with the exact worker subcommand vector.
    let launch = runner
        .calls()
        .into_iter()
        .find(|(prog, args)| {
            prog == "systemd-run"
                && args
                    .iter()
                    .any(|a| a == "--unit=vmfleet-worker-101.service")
        })
        .expect("systemd-run for slot 101");
    let tail = &launch.1[launch.1.len() - 5..];
    assert_eq!(
        tail,
        ["--config", "/test/vmfleet.toml", "worker", "small", "101"]
    );
}

#[test]
fn scale_down_stops_idle_over_desired() {
    // Two online-idle runners, no warm floor => desired 0, both should be stopped.
    let runners = r#"{"runners":[
        {"id":1,"name":"vmfleet-small-101-1000-1","status":"online","busy":false,"labels":[{"name":"self-hosted-small"}]},
        {"id":2,"name":"vmfleet-small-102-1000-1","status":"online","busy":false,"labels":[{"name":"self-hosted-small"}]}
    ]}"#;
    let fx = FixtureGitHub::start(runners);
    let mut cfg = config(
        &fx,
        r#"
[[pool]]
name = "small"
labels = ["self-hosted-small"]
cpus = 2
memory = "4GiB"
min_warm = 0
max = 10
slot_base = 101
"#,
    );
    cfg.supervisor.idle_timeout_seconds = 0; // any idle runner counts as long-idle
                                             // Their worker units are active, so they are eligible scale-down candidates.
    let runner = RecordingRunner::with_active_units(&[
        "vmfleet-worker-101.service",
        "vmfleet-worker-102.service",
    ]);

    let report = run_once(&cfg, &runner, false);

    let p = pool(&report, "small");
    assert_eq!(p.desired, 0);
    assert_eq!(p.present, 2);
    assert_eq!(p.deficit, -2);
    assert!(p.actions.contains(&"stop:101".to_string()));
    assert!(p.actions.contains(&"stop:102".to_string()));
    assert!(runner.called("systemctl", "vmfleet-worker-101.service"));
    assert!(runner.called("systemctl", "vmfleet-worker-102.service"));
}

#[test]
fn admission_block_suppresses_launch() {
    let fx = FixtureGitHub::start(r#"{"runners":[]}"#);
    let mut cfg = config(&fx, SMALL_UP);
    cfg.admission.min_avail_mib = u64::MAX; // MemAvailable can never exceed this => blocked

    let runner = RecordingRunner::default();
    let report = run_once(&cfg, &runner, false);

    assert!(report.blocked.as_deref().unwrap_or("").contains("low-mem"));
    let p = pool(&report, "small");
    assert_eq!(p.deficit, 2, "deficit still computed");
    assert!(p.actions.is_empty(), "but nothing launched while blocked");
    assert!(!runner.called("systemd-run", "--unit="), "no VM launched");
}

#[test]
fn dry_run_makes_no_changes() {
    let fx = FixtureGitHub::start(r#"{"runners":[]}"#);
    let cfg = config(&fx, SMALL_UP);

    let runner = RecordingRunner::default();
    let report = run_once(&cfg, &runner, true);

    // Decision is still computed (would launch 2)...
    assert_eq!(pool(&report, "small").actions.len(), 2);
    // ...but the mock saw no side effects: no launches, no stops, and not even the
    // just-in-time reset-failed the real launch path emits.
    assert!(!runner.called("systemd-run", "--unit="));
    assert!(!runner.called("systemctl", "stop"));
    assert!(!runner.called("systemctl", "reset-failed"));
}
