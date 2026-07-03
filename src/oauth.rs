//! GitHub OAuth **Device Authorization Flow** — the headless, browser-authorize
//! login `gh auth login` uses. Replaces the "create a PAT by hand, pick scopes,
//! paste it" step of guided install: the user copies a one-time code, authorizes
//! in a browser, and we poll GitHub for the resulting token.
//!
//! The device-flow endpoints live on `github.com` (not `api.github.com`), so this
//! module talks to its own base URL rather than reusing `github::Client`'s scoped
//! `api_base`. The HTTP shape mirrors [`crate::selfupdate`] (plain `ureq`,
//! free-form absolute URLs).

use crate::config::GitHub;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::time::Duration;

/// OAuth App Client ID. Device flow uses a **public** client (no secret), so the
/// id is safe to embed in the binary. Register an OAuth App under the `gagalo1234`
/// account with **Device Flow enabled**, then replace this placeholder — or set
/// `VMFLEET_OAUTH_CLIENT_ID` at runtime to override.
const DEFAULT_OAUTH_CLIENT_ID: &str = "REPLACE_WITH_gagalo1234_OAUTH_APP_CLIENT_ID";

/// Marker prefix identifying the not-yet-configured placeholder above.
const PLACEHOLDER_PREFIX: &str = "REPLACE_WITH_";

const USER_AGENT: &str = "vmfleet";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// GitHub's default minimum poll interval when the device-code response omits one.
const FALLBACK_INTERVAL_SECS: u64 = 5;

/// Base poll interval used in real (non-test) runs. GitHub advertises 5s; matching
/// it keeps us from ever tripping the `slow_down` back-pressure error.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(FALLBACK_INTERVAL_SECS);

/// The OAuth scope vmfleet needs to manage this fleet's self-hosted runners:
/// repo-level fleet → `repo` (grants Administration RW); org-level → `admin:org`
/// (self-hosted runners RW). Mirrors the repo-xor-org invariant in
/// [`GitHub::scope_path`].
pub fn scope_from_repo(is_repo: bool) -> &'static str {
    if is_repo {
        "repo"
    } else {
        "admin:org"
    }
}

/// Scope for a fully-built [`GitHub`] config (used by `vmfleet login`).
pub fn scope_for(gh: &GitHub) -> &'static str {
    scope_from_repo(gh.repo.is_some())
}

/// Resolve the client id from an env override (`VMFLEET_OAUTH_CLIENT_ID`), falling
/// back to the embedded constant. Errors clearly if neither is a real id. Pure
/// (takes the env value as an argument) so it is testable without touching the
/// process environment.
fn resolve_client_id(from_env: Option<&str>) -> Result<String> {
    if let Some(id) = from_env.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(id.to_string());
    }
    if DEFAULT_OAUTH_CLIENT_ID.starts_with(PLACEHOLDER_PREFIX) {
        bail!(
            "no GitHub OAuth App client id configured. Register an OAuth App (with Device \
             Flow enabled) and set its client id, or export VMFLEET_OAUTH_CLIENT_ID. To skip \
             device flow entirely, authenticate with a PAT: `vmfleet login --with-token`."
        );
    }
    Ok(DEFAULT_OAUTH_CLIENT_ID.to_string())
}

/// Resolve the device-flow base URL. `VMFLEET_OAUTH_BASE` overrides (tests point it
/// at a localhost fixture; GHES points it at its own host); defaults to github.com.
fn resolve_base(from_env: Option<&str>) -> String {
    from_env
        .map(|b| b.trim().trim_end_matches('/'))
        .filter(|b| !b.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "https://github.com".to_string())
}

fn client_id() -> Result<String> {
    resolve_client_id(std::env::var("VMFLEET_OAUTH_CLIENT_ID").ok().as_deref())
}

fn base() -> String {
    resolve_base(std::env::var("VMFLEET_OAUTH_BASE").ok().as_deref())
}

#[derive(Deserialize)]
struct DeviceCodeResp {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    interval: u64,
}

#[derive(Deserialize)]
struct TokenResp {
    access_token: Option<String>,
    error: Option<String>,
    #[serde(default)]
    interval: Option<u64>,
}

/// Effective wait between polls: never below the caller's `poll_delay`, and never
/// below the server's advertised interval. Pure so the timing rule is unit-tested.
fn poll_interval(server_interval_secs: u64, poll_delay: Duration) -> Duration {
    poll_delay.max(Duration::from_secs(server_interval_secs))
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build()
}

/// Run the device flow to completion and return a GitHub access token. Prints the
/// user code + verification URL, then polls until the user authorizes.
///
/// `poll_delay` is the base wait between polls; pass [`DEFAULT_POLL_INTERVAL`] in
/// production and `Duration::ZERO` in tests. Resolves the client id and base URL
/// from env/const, then delegates to [`run_device_flow`].
pub fn login(scope: &str, poll_delay: Duration) -> Result<String> {
    run_device_flow(&agent(), &base(), &client_id()?, scope, poll_delay)
}

/// The device-flow state machine, with the base URL and client id passed in
/// explicitly so tests can drive it against a fixture with no env dependence.
fn run_device_flow(
    agent: &ureq::Agent,
    base: &str,
    client_id: &str,
    scope: &str,
    poll_delay: Duration,
) -> Result<String> {
    // 1. request a device + user code
    let dc: DeviceCodeResp = agent
        .post(&format!("{base}/login/device/code"))
        .set("Accept", "application/json")
        .set("User-Agent", USER_AGENT)
        .send_form(&[("client_id", client_id), ("scope", scope)])
        .context("requesting device code")?
        .into_json()
        .context("parsing device-code response")?;

    // 2. tell the user where to authorize
    println!("\n! First copy your one-time code: {}", dc.user_code);
    println!(
        "  Then open {} in a browser and enter it.",
        dc.verification_uri
    );
    println!("  Waiting for authorization (scope: {scope})...");

    // 3. poll for the token
    let mut interval = poll_interval(dc.interval, poll_delay);
    loop {
        std::thread::sleep(interval);
        let resp: TokenResp = agent
            .post(&format!("{base}/login/oauth/access_token"))
            .set("Accept", "application/json")
            .set("User-Agent", USER_AGENT)
            .send_form(&[
                ("client_id", client_id),
                ("device_code", &dc.device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .context("polling for access token")?
            .into_json()
            .context("parsing token response")?;

        if let Some(token) = resp.access_token.filter(|t| !t.is_empty()) {
            println!("\u{2713} authenticated");
            return Ok(token);
        }
        match resp.error.as_deref() {
            // still waiting on the user — keep polling at the current interval
            Some("authorization_pending") => {}
            // GitHub asks us to back off: bump the interval it advertised (or 5s)
            Some("slow_down") => {
                let bump = resp.interval.unwrap_or(FALLBACK_INTERVAL_SECS);
                interval = poll_interval(interval.as_secs() + bump, poll_delay);
            }
            Some("expired_token") => {
                bail!("device code expired before authorization; rerun `vmfleet login`")
            }
            Some("access_denied") => bail!("authorization was denied"),
            Some(other) => bail!("device-flow error: {other}"),
            None => bail!("token response had neither access_token nor error"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::FixtureOAuth;

    #[test]
    fn scope_maps_repo_and_org() {
        assert_eq!(scope_from_repo(true), "repo");
        assert_eq!(scope_from_repo(false), "admin:org");
    }

    #[test]
    fn client_id_prefers_env_then_errors_on_placeholder() {
        assert_eq!(resolve_client_id(Some("gh_abc")).unwrap(), "gh_abc");
        assert_eq!(resolve_client_id(Some("  gh_abc  ")).unwrap(), "gh_abc");
        // the embedded default is still the placeholder, so no env => error
        assert!(resolve_client_id(None).is_err());
        assert!(resolve_client_id(Some("   ")).is_err());
    }

    #[test]
    fn base_defaults_and_overrides() {
        assert_eq!(resolve_base(None), "https://github.com");
        assert_eq!(resolve_base(Some("")), "https://github.com");
        assert_eq!(
            resolve_base(Some("http://127.0.0.1:9/")),
            "http://127.0.0.1:9"
        );
    }

    #[test]
    fn poll_interval_takes_the_larger_floor() {
        // caller floor wins when the server asks for less
        assert_eq!(
            poll_interval(0, Duration::from_secs(5)),
            Duration::from_secs(5)
        );
        // server interval wins when it is larger
        assert_eq!(
            poll_interval(10, Duration::from_secs(5)),
            Duration::from_secs(10)
        );
        // test path: both zero => no sleep
        assert_eq!(poll_interval(0, Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn device_flow_polls_until_authorized() {
        // fixture returns interval 0 and one `authorization_pending` before the token,
        // so with poll_delay ZERO the loop runs instantly and exercises the pending path.
        let fx = FixtureOAuth::start("gho_test_token", 1);
        let token = run_device_flow(
            &agent(),
            &fx.base_url,
            "test-client",
            "repo",
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(token, "gho_test_token");
    }

    #[test]
    fn device_flow_reports_denied() {
        let fx = FixtureOAuth::start_error("access_denied");
        let err = run_device_flow(&agent(), &fx.base_url, "c", "repo", Duration::ZERO)
            .unwrap_err()
            .to_string();
        assert!(err.contains("denied"), "unexpected error: {err}");
    }
}
