//! Host resource probes used by the admission gate and supervisor headroom logic.
//! Pure reads of /proc and statvfs — no external commands, trivially testable via
//! the parse helpers.

use anyhow::Result;
use std::path::Path;

/// Available memory in MiB (from /proc/meminfo MemAvailable).
pub fn mem_available_mib() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| parse_mem_available_kib(&s))
        .map(|kib| kib / 1024)
        .unwrap_or(0)
}

fn parse_mem_available_kib(meminfo: &str) -> Option<u64> {
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            return rest.split_whitespace().next()?.parse::<u64>().ok();
        }
    }
    None
}

/// Memory pressure: "some avg10" percentage from /proc/pressure/memory (0 if absent).
pub fn mem_psi_some_avg10() -> f64 {
    std::fs::read_to_string("/proc/pressure/memory")
        .ok()
        .and_then(|s| parse_psi_some_avg10(&s))
        .unwrap_or(0.0)
}

fn parse_psi_some_avg10(psi: &str) -> Option<f64> {
    for line in psi.lines() {
        if let Some(rest) = line.strip_prefix("some ") {
            for tok in rest.split_whitespace() {
                if let Some(v) = tok.strip_prefix("avg10=") {
                    return v.parse::<f64>().ok();
                }
            }
        }
    }
    None
}

/// Free space in GiB on the filesystem containing `path`.
pub fn disk_free_gib(path: &Path) -> Result<u64> {
    let stat = nix::sys::statvfs::statvfs(path)?;
    let bavail = stat.blocks_available() as u64;
    let frsize = stat.fragment_size() as u64;
    Ok((bavail.saturating_mul(frsize)) / (1024 * 1024 * 1024))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mem_available() {
        let s = "MemTotal:       31629532 kB\nMemFree:  100 kB\nMemAvailable:   13480960 kB\n";
        assert_eq!(parse_mem_available_kib(s), Some(13480960));
    }

    #[test]
    fn parses_psi() {
        let s = "some avg10=0.09 avg60=0.03 avg300=0.07 total=435548281\nfull avg10=0.00 avg60=0.0 avg300=0.0 total=1";
        assert_eq!(parse_psi_some_avg10(s), Some(0.09));
    }

    #[test]
    fn psi_absent_is_zero() {
        assert_eq!(parse_psi_some_avg10("garbage"), None);
    }
}
