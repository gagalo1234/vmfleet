//! Runtime path layout. Code lives in the repo; config/state/secrets live in the
//! user's XDG dirs so the two are cleanly separated.

use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(fallback))
}

/// ~/.config/vmfleet
pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("vmfleet")
}

/// Default config file path.
pub fn config_file() -> PathBuf {
    config_dir().join("vmfleet.toml")
}

/// ~/.local/share/vmfleet — state (admission lock, per-slot current-vm, status.json).
pub fn state_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("vmfleet")
}

pub fn status_file() -> PathBuf {
    state_dir().join("status.json")
}

pub fn admission_lock() -> PathBuf {
    state_dir().join("admission.lock")
}

/// Per-slot state file recording the current VM name (for orphan sweep).
pub fn slot_state(slot: u32) -> PathBuf {
    state_dir().join("slots").join(format!("{slot}"))
}

/// ~/.config/systemd/user
pub fn systemd_user_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config")
        .join("systemd")
        .join("user")
}
