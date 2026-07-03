//! In-product updater: `vmfleet self-update`.
//!
//! Fetches the latest published GitHub Release (produced by
//! `.github/workflows/release.yml`), downloads the tarball for *this* binary's
//! target triple, verifies its `.sha256`, atomically swaps the running binary in
//! place, then re-runs `install --upgrade` (migrate config, rewrite units,
//! restart the supervisor).
//!
//! Design notes:
//! - Manual trigger by default — a control plane must not silently interrupt
//!   in-flight jobs. The supervisor only *notifies* (see [`latest_version`]).
//! - Lean: reuses `ureq` (no second HTTP/TLS stack). Only the release-metadata
//!   JSON calls carry the auth token (they stay on api.github.com); binary asset
//!   downloads follow `browser_download_url` unauthenticated so the token is never
//!   re-sent across a redirect to object storage.
//! - The target triple is embedded at build time by `build.rs`
//!   (`env!("VMFLEET_TARGET")`), so we always request the exact matching asset.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

const API_VERSION: &str = "2022-11-28";
const USER_AGENT: &str = "vmfleet";
const RETRIES: u32 = 3;

/// Options for `vmfleet self-update`.
pub struct Opts {
    /// Report availability only; do not download or install.
    pub check: bool,
    /// Pin to a specific release tag (e.g. `v0.2.0`) instead of latest.
    pub tag: Option<String>,
    /// Consider prerelease releases when choosing "latest".
    pub allow_prerelease: bool,
    /// Skip the confirmation prompt.
    pub yes: bool,
    /// Swap the binary but do not run `install --upgrade` / restart the supervisor.
    pub no_restart: bool,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    #[serde(default)]
    browser_download_url: String,
    /// api.github.com asset URL — used for private-repo authenticated download.
    #[serde(default)]
    url: String,
}

/// Compile-time version of this binary (from Cargo.toml).
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The target triple this binary was built for (embedded by `build.rs`).
fn target_triple() -> &'static str {
    env!("VMFLEET_TARGET")
}

/// Entry point for the `self-update` subcommand.
pub fn run(cfg_path: &Path, opts: &Opts) -> Result<()> {
    let current = current_version();
    let target = target_triple();
    let (owner, repo) = parse_repo_url(env!("CARGO_PKG_REPOSITORY"))
        .context("could not determine GitHub repo from CARGO_PKG_REPOSITORY")?;
    let token = best_effort_token(cfg_path);
    let agent = build_agent(Duration::from_secs(30));

    let rel = fetch_release(
        &agent,
        &owner,
        &repo,
        token.as_deref(),
        opts.tag.as_deref(),
        opts.allow_prerelease,
    )?;
    let latest = rel.tag_name.trim_start_matches('v').to_string();

    println!("current: {current}   latest: {latest}   target: {target}");

    // For an explicit --tag we always proceed (allows re-install / downgrade);
    // otherwise only when the release is strictly newer.
    let newer = opts.tag.is_some() || is_newer(&latest, current);
    if !newer {
        println!("already up to date.");
        return Ok(());
    }
    if opts.check {
        println!("update available: {current} -> {latest}");
        println!("run `vmfleet self-update` to install it.");
        return Ok(());
    }

    if !opts.yes && !confirm(&format!("update {current} -> {latest}?")) {
        println!("aborted.");
        return Ok(());
    }

    // Locate the tarball asset for this target and its checksum sidecar.
    let want_asset = asset_name(&latest, target);
    let tar_asset = rel
        .assets
        .iter()
        .find(|a| a.name == want_asset)
        .with_context(|| {
            format!(
                "release {} has no asset `{want_asset}` for target {target}",
                rel.tag_name
            )
        })?;
    let sha_asset = rel
        .assets
        .iter()
        .find(|a| a.name == format!("{want_asset}.sha256"));

    // Stage in the same directory as the running binary so the final swap is an
    // atomic same-filesystem rename.
    let exe = std::env::current_exe().context("resolving current executable path")?;
    let dir = exe
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| Path::new(".").to_path_buf());
    let tmp_tar = dir.join(format!(".vmfleet-update-{latest}.tar.gz"));
    let new_bin = dir.join(format!(".vmfleet-update-{latest}.bin"));
    // Best-effort cleanup on any early return.
    let _guard = TempCleanup(vec![tmp_tar.clone(), new_bin.clone()]);

    println!("downloading {want_asset} ...");
    download_asset(&agent, tar_asset, token.as_deref(), &tmp_tar)?;

    match sha_asset {
        Some(sha) => {
            let want = fetch_sha256(&agent, sha, token.as_deref())?;
            let got = sha256_file(&tmp_tar)?;
            if !want.eq_ignore_ascii_case(&got) {
                bail!("checksum mismatch for {want_asset}: expected {want}, got {got}");
            }
            println!("checksum ok ({})", &got[..16.min(got.len())]);
        }
        None => tracing::warn!("no .sha256 asset for {want_asset}; skipping checksum verification"),
    }

    extract_binary(&tmp_tar, "vmfleet", &new_bin)
        .with_context(|| format!("extracting vmfleet from {}", tmp_tar.display()))?;
    make_executable(&new_bin)?;

    self_replace::self_replace(&new_bin)
        .with_context(|| format!("replacing running binary at {}", exe.display()))?;
    println!("installed {latest} at {}", exe.display());

    if opts.no_restart {
        println!(
            "--no-restart: run `vmfleet install --upgrade` to apply units/config and restart."
        );
        return Ok(());
    }
    post_upgrade(&exe, cfg_path);
    Ok(())
}

/// Best-effort: latest published (non-prerelease) version string, no leading `v`.
/// Returns `None` on any error. Uses a short timeout so callers on a hot path
/// (the supervisor loop) never stall. Used only for *notification*.
pub fn latest_version(token: Option<&str>) -> Option<String> {
    let (owner, repo) = parse_repo_url(env!("CARGO_PKG_REPOSITORY"))?;
    let agent = build_agent(Duration::from_secs(10));
    let rel = fetch_release(&agent, &owner, &repo, token, None, false).ok()?;
    Some(rel.tag_name.trim_start_matches('v').to_string())
}

/// Whether `latest` (no leading `v`) is strictly newer than the running binary.
pub fn update_available(latest: &str) -> bool {
    is_newer(latest, current_version())
}

// ===================== release discovery =====================

fn fetch_release(
    agent: &ureq::Agent,
    owner: &str,
    repo: &str,
    token: Option<&str>,
    tag: Option<&str>,
    allow_prerelease: bool,
) -> Result<Release> {
    let base = format!("https://api.github.com/repos/{owner}/{repo}/releases");
    if let Some(tag) = tag {
        return get_json(agent, &format!("{base}/tags/{tag}"), token);
    }
    if allow_prerelease {
        let list: Vec<Release> = get_json(agent, &format!("{base}?per_page=30"), token)?;
        return pick_newest(list).ok_or_else(|| anyhow!("no published releases found"));
    }
    // `/latest` already excludes drafts and prereleases.
    get_json(agent, &format!("{base}/latest"), token)
}

/// Pure: newest non-draft release by semver, prereleases allowed. Used for the
/// `--allow-prerelease` path and unit-tested via [`pick_newest_meta`].
fn pick_newest(releases: Vec<Release>) -> Option<Release> {
    releases
        .into_iter()
        .filter(|r| !r.draft)
        .max_by(|a, b| cmp_versions(&a.tag_name, &b.tag_name))
}

// ===================== download / verify / install =====================

fn download_asset(
    agent: &ureq::Agent,
    asset: &Asset,
    token: Option<&str>,
    dest: &Path,
) -> Result<()> {
    // Prefer the unauthenticated public download URL: it redirects to object
    // storage, and we never want to forward the auth token across that redirect.
    if !asset.browser_download_url.is_empty() {
        if let Ok(()) = stream_to_file(agent, &asset.browser_download_url, None, dest) {
            return Ok(());
        }
    }
    // Fallback (private repos): the api.github.com asset endpoint with an
    // octet-stream Accept header. ureq follows the redirect to signed storage.
    if let Some(t) = token {
        if !asset.url.is_empty() {
            return stream_to_file(agent, &asset.url, Some(t), dest);
        }
    }
    bail!("could not download asset {}", asset.name)
}

fn fetch_sha256(agent: &ureq::Agent, asset: &Asset, token: Option<&str>) -> Result<String> {
    let body = if !asset.browser_download_url.is_empty() {
        get_text(agent, &asset.browser_download_url, None)
            .or_else(|_| get_text(agent, &asset.url, token))?
    } else {
        get_text(agent, &asset.url, token)?
    };
    parse_sha256(&body).ok_or_else(|| anyhow!("could not parse checksum from {}", asset.name))
}

fn extract_binary(tarball: &Path, member: &str, dest: &Path) -> Result<()> {
    let f = std::fs::File::open(tarball)?;
    let gz = flate2::read::GzDecoder::new(f);
    let mut ar = tar::Archive::new(gz);
    for entry in ar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        // Match the `vmfleet` binary anywhere in the archive (the release packs it
        // as `<pkg>/vmfleet`).
        if path.file_name().and_then(|n| n.to_str()) == Some(member) {
            let mut out = std::fs::File::create(dest)?;
            std::io::copy(&mut entry, &mut out)?;
            return Ok(());
        }
    }
    bail!("archive did not contain a `{member}` entry")
}

fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(path)?.permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(path, perm)?;
    Ok(())
}

/// Re-exec the freshly installed binary to migrate config, rewrite units and
/// restart the supervisor. Best-effort: the binary is already swapped, so a
/// failure here only means the operator should run the step manually.
fn post_upgrade(exe: &Path, cfg_path: &Path) {
    if !cfg_path.exists() {
        println!(
            "no config at {} — run `vmfleet install` to set up.",
            cfg_path.display()
        );
        return;
    }
    println!("applying config/units and restarting supervisor ...");
    let status = std::process::Command::new(exe)
        .arg("--config")
        .arg(cfg_path)
        .arg("install")
        .arg("--upgrade")
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => tracing::warn!("`install --upgrade` exited with {s}; run it manually if needed"),
        Err(e) => tracing::warn!("could not run `install --upgrade`: {e}; run it manually"),
    }
}

// ===================== HTTP helpers (ureq) =====================

fn build_agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(timeout)
        // Never forward the Authorization header across a redirect: the asset API
        // 302s to a signed object-storage URL on a foreign host, which must not see
        // our GitHub token. (This is ureq's default; set explicitly for clarity.)
        .redirect_auth_headers(ureq::RedirectAuthHeaders::Never)
        .build()
}

fn apply_headers(mut req: ureq::Request, token: Option<&str>, accept: &str) -> ureq::Request {
    req = req
        .set("Accept", accept)
        .set("X-GitHub-Api-Version", API_VERSION)
        .set("User-Agent", USER_AGENT);
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    req
}

/// GET with a few retries on 5xx / transient I/O (same shape as `github::send`).
fn send_get(
    agent: &ureq::Agent,
    url: &str,
    token: Option<&str>,
    accept: &str,
) -> Result<ureq::Response> {
    let mut last = anyhow!("no attempt");
    for attempt in 1..=RETRIES {
        match apply_headers(agent.get(url), token, accept).call() {
            Ok(resp) => return Ok(resp),
            Err(ureq::Error::Status(code, _)) if code >= 500 || code == 429 => {
                last = anyhow!("GET {url}: HTTP {code}");
                std::thread::sleep(Duration::from_secs(2 * attempt as u64));
            }
            Err(ureq::Error::Status(code, _)) => bail!("GET {url}: HTTP {code}"),
            Err(e) => {
                last = anyhow!("GET {url} attempt {attempt}/{RETRIES}: {e}");
                std::thread::sleep(Duration::from_secs(2 * attempt as u64));
            }
        }
    }
    Err(last)
}

fn get_json<T: for<'de> Deserialize<'de>>(
    agent: &ureq::Agent,
    url: &str,
    token: Option<&str>,
) -> Result<T> {
    Ok(send_get(agent, url, token, "application/vnd.github+json")?.into_json::<T>()?)
}

fn get_text(agent: &ureq::Agent, url: &str, token: Option<&str>) -> Result<String> {
    Ok(send_get(agent, url, token, "application/octet-stream")?.into_string()?)
}

fn stream_to_file(agent: &ureq::Agent, url: &str, token: Option<&str>, dest: &Path) -> Result<()> {
    let resp = send_get(agent, url, token, "application/octet-stream")?;
    let mut reader = resp.into_reader();
    let tmp = dest.with_extension("part");
    {
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        std::io::copy(&mut reader, &mut file)?;
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

// ===================== small pure helpers (unit-tested) =====================

/// Parse `owner`/`repo` from a Cargo `repository` URL like
/// `https://github.com/owner/name` (optionally `.git` / trailing slash).
fn parse_repo_url(url: &str) -> Option<(String, String)> {
    let rest = url
        .trim()
        .strip_prefix("https://github.com/")
        .or_else(|| url.trim().strip_prefix("http://github.com/"))
        .or_else(|| url.trim().strip_prefix("git@github.com:"))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = rest.splitn(2, '/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some((owner, repo))
}

/// Release tarball name for a given version (no leading `v`) and target triple,
/// matching `.github/workflows/release.yml`'s `vmfleet-${version}-${target}`.
fn asset_name(version: &str, target: &str) -> String {
    format!("vmfleet-{version}-{target}.tar.gz")
}

/// Parse into `(MAJOR.MINOR.PATCH, prerelease identifiers)`, tolerating a leading
/// `v` and ignoring any `+build` metadata. An empty identifier list means a stable
/// release. E.g. `v0.2.0-rc.1` -> `((0,2,0), ["rc","1"])`.
fn parse_version_full(s: &str) -> Option<((u64, u64, u64), Vec<String>)> {
    let s = s.trim().trim_start_matches('v');
    let s = s.split('+').next().unwrap_or(s); // drop build metadata
    let mut parts = s.splitn(2, '-');
    let base = parts.next()?;
    let mut it = base.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next().unwrap_or("0").parse().ok()?;
    let pre = match parts.next() {
        Some(p) if !p.is_empty() => p.split('.').map(str::to_string).collect(),
        _ => Vec::new(),
    };
    Some(((major, minor, patch), pre))
}

/// SemVer precedence: compare the base triple, then the prerelease (per §11.3-11.4,
/// a stable release outranks any prerelease of the same base). Unparseable inputs
/// sort below parseable ones.
fn cmp_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (parse_version_full(a), parse_version_full(b)) {
        (Some((av, ap)), Some((bv, bp))) => av.cmp(&bv).then_with(|| cmp_prerelease(&ap, &bp)),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

/// Prerelease-identifier precedence (SemVer §11.4). Empty list = stable = highest.
/// Numeric identifiers compare numerically and rank below alphanumeric ones; when
/// all shared identifiers are equal, the longer list wins.
fn cmp_prerelease(a: &[String], b: &[String]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a.is_empty(), b.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater, // stable > prerelease
        (false, true) => Ordering::Less,
        (false, false) => {
            for (x, y) in a.iter().zip(b.iter()) {
                let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(xn), Ok(yn)) => xn.cmp(&yn),
                    (Ok(_), Err(_)) => Ordering::Less, // numeric < alphanumeric
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => x.as_str().cmp(y.as_str()),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            a.len().cmp(&b.len())
        }
    }
}

/// True when `candidate` has strictly greater SemVer precedence than `current`.
fn is_newer(candidate: &str, current: &str) -> bool {
    cmp_versions(candidate, current) == std::cmp::Ordering::Greater
}

/// Extract the hex digest from `sha256sum` output (`<hex>  <filename>`), or a
/// bare digest on its own line.
fn parse_sha256(body: &str) -> Option<String> {
    let tok = body.split_whitespace().next()?;
    let tok = tok.trim();
    if tok.len() == 64 && tok.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(tok.to_ascii_lowercase())
    } else {
        None
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn best_effort_token(cfg_path: &Path) -> Option<String> {
    if let Ok(t) = std::env::var("VMFLEET_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    if cfg_path.exists() {
        if let Ok(cfg) = crate::config::Config::load(cfg_path) {
            if let Ok(t) = cfg.token() {
                let t = t.trim().to_string();
                if !t.is_empty() {
                    return Some(t);
                }
            }
        }
    }
    None
}

fn confirm(prompt: &str) -> bool {
    use dialoguer::Confirm;
    // Non-interactive (no TTY, e.g. run from a unit) => default to no.
    Confirm::new()
        .with_prompt(prompt)
        .default(false)
        .interact()
        .unwrap_or(false)
}

/// Deletes staged temp files when dropped (covers early returns / errors).
struct TempCleanup(Vec<std::path::PathBuf>);
impl Drop for TempCleanup {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
            let _ = std::fs::remove_file(p.with_extension("part"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repo_url() {
        assert_eq!(
            parse_repo_url("https://github.com/gagalo1234/vmfleet"),
            Some(("gagalo1234".into(), "vmfleet".into()))
        );
        assert_eq!(
            parse_repo_url("https://github.com/o/n.git"),
            Some(("o".into(), "n".into()))
        );
        assert_eq!(
            parse_repo_url("https://github.com/o/n/"),
            Some(("o".into(), "n".into()))
        );
        assert_eq!(
            parse_repo_url("git@github.com:o/n.git"),
            Some(("o".into(), "n".into()))
        );
        assert_eq!(parse_repo_url("https://gitlab.com/o/n"), None);
        assert_eq!(parse_repo_url("https://github.com/o"), None);
    }

    #[test]
    fn builds_asset_name() {
        assert_eq!(
            asset_name("0.2.0", "x86_64-unknown-linux-musl"),
            "vmfleet-0.2.0-x86_64-unknown-linux-musl.tar.gz"
        );
        // prerelease tag flows through unchanged (matches release.yml packaging).
        assert_eq!(
            asset_name("0.2.0-rc.1", "aarch64-unknown-linux-gnu"),
            "vmfleet-0.2.0-rc.1-aarch64-unknown-linux-gnu.tar.gz"
        );
    }

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version_full("v1.2.3"), Some(((1, 2, 3), vec![])));
        assert_eq!(parse_version_full("1.2.3"), Some(((1, 2, 3), vec![])));
        assert_eq!(
            parse_version_full("v0.2.0-rc.1"),
            Some(((0, 2, 0), vec!["rc".to_string(), "1".to_string()]))
        );
        assert_eq!(parse_version_full("1.2"), Some(((1, 2, 0), vec![])));
        assert_eq!(
            parse_version_full("1.2.3+build.5"),
            Some(((1, 2, 3), vec![]))
        );
        assert_eq!(parse_version_full("nope"), None);
    }

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0")); // equal is not newer
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    #[test]
    fn compares_prerelease_versions() {
        // Numeric prerelease identifiers compare numerically, not lexically
        // (11 > 2; lexical string compare would get this wrong).
        assert!(is_newer("0.2.0-rc.2", "0.2.0-rc.1"));
        assert!(is_newer("0.2.0-rc.11", "0.2.0-rc.2"));
        // Stable outranks any prerelease of the same base (SemVer §11.3).
        assert!(is_newer("0.2.0", "0.2.0-rc.2"));
        assert!(!is_newer("0.2.0-rc.1", "0.2.0"));
        // Higher base beats a prerelease of a lower base.
        assert!(is_newer("0.3.0-rc.1", "0.2.0"));
        // A larger set of prerelease fields has higher precedence (§11.4).
        assert!(is_newer("0.2.0-rc.1.1", "0.2.0-rc.1"));
        // Equal prereleases are not newer.
        assert!(!is_newer("0.2.0-rc.1", "0.2.0-rc.1"));
        // Numeric identifiers rank below alphanumeric ones.
        assert!(is_newer("0.2.0-rc", "0.2.0-1"));
    }

    #[test]
    fn parses_sha256() {
        let good = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  vmfleet-0.1.0-x.tar.gz\n";
        assert_eq!(
            parse_sha256(good).as_deref(),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        // bare digest, uppercase -> normalized lowercase
        let bare = "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855";
        assert_eq!(
            parse_sha256(bare).as_deref(),
            Some(bare.to_ascii_lowercase().as_str())
        );
        assert_eq!(parse_sha256("too short  file"), None);
        assert_eq!(parse_sha256(""), None);
    }

    #[test]
    fn sha256_of_file_matches_known_vector() {
        // SHA-256 of the empty input.
        let dir = std::env::temp_dir();
        let p = dir.join("vmfleet-selfupdate-empty-test");
        std::fs::write(&p, b"").unwrap();
        assert_eq!(
            sha256_file(&p).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let _ = std::fs::remove_file(&p);
    }

    // Model of the release-list selection (mirrors `pick_newest`) so we can assert
    // draft-skipping and newest-wins without constructing HTTP fixtures.
    fn pick_newest_meta(items: &[(&str, bool)]) -> Option<String> {
        items
            .iter()
            .filter(|(_, draft)| !draft)
            .max_by(|(a, _), (b, _)| cmp_versions(a, b))
            .map(|(t, _)| t.to_string())
    }

    #[test]
    fn picks_newest_published_not_draft() {
        // (tag, draft): newest published wins, the newer draft is skipped.
        let items = [("v0.3.0", true), ("v0.2.0", false), ("v0.1.0", false)];
        assert_eq!(pick_newest_meta(&items).as_deref(), Some("v0.2.0"));
        // only a draft available => nothing selectable.
        let only_draft: [(&str, bool); 1] = [("v0.3.0", true)];
        assert_eq!(pick_newest_meta(&only_draft), None);
    }
}
