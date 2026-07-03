//! Hermetic CLI smoke tests: drive the *compiled* `vmfleet` binary end-to-end for
//! the surface that needs no network, no systemd and no Multipass — argument
//! parsing, config load+validate, and the presence of the supervisor preview flags.
//! Live behaviour that spins real VMs is out of scope here (that is the manual
//! self-hosted `live-smoke` job); these run for free on any GitHub-hosted runner.

use std::path::PathBuf;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_vmfleet");

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("vmfleet-cli-{}-{name}", std::process::id()))
}

const VALID_CONFIG: &str = r#"
[github]
repo = "owner/name"
token_file = "/tmp/vmfleet-cli-token"

[[base]]
name = "vmfleet-base"
image = "24.04"

[[pool]]
name = "small"
labels = ["self-hosted-small"]
cpus = 2
memory = "4GiB"
min_warm = 1
max = 4
slot_base = 101
"#;

#[test]
fn version_and_help_exit_zero() {
    for args in [["--version"], ["--help"]] {
        let out = Command::new(BIN).args(args).output().unwrap();
        assert!(out.status.success(), "`vmfleet {args:?}` should exit 0");
    }
}

#[test]
fn supervisor_help_advertises_preview_flags() {
    let out = Command::new(BIN)
        .args(["supervisor", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("--once"), "supervisor --help lists --once");
    assert!(
        text.contains("--dry-run"),
        "supervisor --help lists --dry-run"
    );
}

#[test]
fn config_check_accepts_valid_config() {
    let path = tmp("valid.toml");
    std::fs::write(&path, VALID_CONFIG).unwrap();
    let out = Command::new(BIN)
        .args(["config-check", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "config-check on a valid config should exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("OK"));
}

#[test]
fn config_check_rejects_invalid_config() {
    // max = 0 is invalid (validate() requires max > 0).
    let path = tmp("invalid.toml");
    std::fs::write(&path, VALID_CONFIG.replace("max = 4", "max = 0")).unwrap();
    let out = Command::new(BIN)
        .args(["config-check", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "config-check on an invalid config should exit non-zero"
    );
}
