//! GitHub REST client (native, via ureq) — replaces the `gh` CLI dependency.
//! Only the endpoints the fleet needs: list runners, JIT config, registration
//! token, delete runner, and best-effort queued-job label counts. All calls
//! retry a few times to ride out the fake-ip/proxy i/o timeouts seen in the
//! original clearbox setup.

use crate::config::GitHub;
use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const API_VERSION: &str = "2022-11-28";
const RETRIES: u32 = 3;

pub struct Client {
    agent: ureq::Agent,
    token: String,
    api_base: String,
    scope: String, // "repos/o/n" or "orgs/o"
    runner_group_id: u64,
    is_repo: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Runner {
    pub id: u64,
    pub name: String,
    pub status: String, // "online" | "offline"
    #[serde(default)]
    pub busy: bool,
    #[serde(default)]
    pub labels: Vec<Label>,
}
#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub name: String,
}
impl Runner {
    pub fn has_label(&self, l: &str) -> bool {
        self.labels.iter().any(|x| x.name == l)
    }
    pub fn online(&self) -> bool {
        self.status == "online"
    }
}

#[derive(Deserialize)]
struct RunnersResp {
    #[serde(default)]
    runners: Vec<Runner>,
}
#[derive(Deserialize)]
struct TokenResp {
    token: String,
}
#[derive(Deserialize)]
struct JitResp {
    encoded_jit_config: String,
}

impl Client {
    pub fn new(gh: &GitHub, token: String) -> Result<Client> {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(30))
            .build();
        Ok(Client {
            agent,
            token,
            api_base: gh.api_base.trim_end_matches('/').to_string(),
            scope: gh.scope_path()?,
            runner_group_id: gh.runner_group_id,
            is_repo: gh.repo.is_some(),
        })
    }

    fn url(&self, tail: &str) -> String {
        format!("{}/{}/{}", self.api_base, self.scope, tail)
    }

    fn headers(&self, req: ureq::Request) -> ureq::Request {
        req.set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", API_VERSION)
            .set("User-Agent", "vmfleet")
    }

    /// Send with retry + GitHub rate-limit backoff. `body` None => GET, Some => POST.
    fn send(
        &self,
        method: &str,
        url: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<ureq::Response> {
        let mut last = anyhow!("no attempt");
        for attempt in 1..=RETRIES {
            let req = self.headers(self.agent.request(method, url));
            let res = match body {
                Some(b) => req.send_json(b.clone()),
                None => req.call(),
            };
            match res {
                Ok(resp) => return Ok(resp),
                // primary/secondary rate limit: honor Retry-After / X-RateLimit-Reset
                Err(ureq::Error::Status(code, resp)) if code == 403 || code == 429 => {
                    let wait = rate_limit_wait(&resp).unwrap_or(2 * attempt as u64).min(60);
                    tracing::warn!("github {code} (rate limit) on {url}; waiting {wait}s");
                    last = anyhow!("{method} {url}: {code} rate-limited");
                    std::thread::sleep(Duration::from_secs(wait));
                }
                Err(ureq::Error::Status(code, _)) if code >= 500 => {
                    last = anyhow!("{method} {url}: HTTP {code}");
                    std::thread::sleep(Duration::from_secs(2 * attempt as u64));
                }
                Err(ureq::Error::Status(code, _)) => {
                    return Err(anyhow!("{method} {url}: HTTP {code}"))
                }
                Err(e) => {
                    last = anyhow!("{method} {url} attempt {attempt}/{RETRIES}: {e}");
                    std::thread::sleep(Duration::from_secs(2 * attempt as u64));
                }
            }
        }
        Err(last)
    }

    fn get_json<T: for<'de> Deserialize<'de>>(&self, tail: &str) -> Result<T> {
        Ok(self.send("GET", &self.url(tail), None)?.into_json::<T>()?)
    }

    fn post_json<T: for<'de> Deserialize<'de>>(
        &self,
        tail: &str,
        body: serde_json::Value,
    ) -> Result<T> {
        Ok(self
            .send("POST", &self.url(tail), Some(&body))?
            .into_json::<T>()?)
    }

    /// Liveness/auth probe: list runners once.
    pub fn reachable(&self) -> Result<()> {
        self.list_runners().map(|_| ())
    }

    /// List all runners, following `Link: rel="next"` pagination (>100 runners).
    pub fn list_runners(&self) -> Result<Vec<Runner>> {
        let mut out = Vec::new();
        let mut url = self.url("actions/runners?per_page=100");
        loop {
            let resp = self.send("GET", &url, None)?;
            let next = next_link(resp.header("link"));
            let page: RunnersResp = resp.into_json()?;
            out.extend(page.runners);
            match next {
                Some(n) => url = n,
                None => break,
            }
        }
        Ok(out)
    }

    pub fn registration_token(&self) -> Result<String> {
        let r: TokenResp =
            self.post_json("actions/runners/registration-token", serde_json::json!({}))?;
        Ok(r.token)
    }

    /// Generate a JIT config for an ephemeral runner. Returns the encoded blob to
    /// pass to `run.sh --jitconfig`.
    pub fn generate_jitconfig(&self, name: &str, labels: &[String]) -> Result<String> {
        let body = serde_json::json!({
            "name": name,
            "runner_group_id": self.runner_group_id,
            "labels": labels,
            "work_folder": "_work",
        });
        let r: JitResp = self.post_json("actions/runners/generate-jitconfig", body)?;
        Ok(r.encoded_jit_config)
    }

    pub fn delete_runner(&self, id: u64) -> Result<()> {
        let url = self.url(&format!("actions/runners/{id}"));
        for attempt in 1..=RETRIES {
            match self
                .agent
                .delete(&url)
                .set("Authorization", &format!("Bearer {}", self.token))
                .set("Accept", "application/vnd.github+json")
                .set("X-GitHub-Api-Version", API_VERSION)
                .set("User-Agent", "vmfleet")
                .call()
            {
                Ok(_) => return Ok(()),
                // 404 => already gone; treat as success
                Err(ureq::Error::Status(404, _)) => return Ok(()),
                Err(_) if attempt < RETRIES => std::thread::sleep(Duration::from_secs(2)),
                Err(e) => return Err(anyhow!("DELETE runner {id}: {e}")),
            }
        }
        Ok(())
    }

    /// Best-effort count of queued jobs per label. Repo scope only; org scope
    /// returns empty (org-wide queued enumeration isn't cheap). Never fails the
    /// caller — returns {} on any error.
    pub fn queued_labels(&self) -> HashMap<String, u32> {
        let mut out = HashMap::new();
        if !self.is_repo {
            return out;
        }
        #[derive(Deserialize)]
        struct Runs {
            #[serde(default)]
            workflow_runs: Vec<RunId>,
        }
        #[derive(Deserialize)]
        struct RunId {
            id: u64,
        }
        #[derive(Deserialize)]
        struct Jobs {
            #[serde(default)]
            jobs: Vec<Job>,
        }
        #[derive(Deserialize)]
        struct Job {
            #[serde(default)]
            status: String,
            #[serde(default)]
            labels: Vec<String>,
        }
        let runs: Runs = match self.get_json("actions/runs?status=queued&per_page=30") {
            Ok(r) => r,
            Err(_) => return out,
        };
        for run in runs.workflow_runs {
            if let Ok(j) =
                self.get_json::<Jobs>(&format!("actions/runs/{}/jobs?per_page=50", run.id))
            {
                for job in j.jobs {
                    if job.status == "queued" {
                        for l in job.labels {
                            *out.entry(l).or_insert(0) += 1;
                        }
                    }
                }
            }
        }
        out
    }
}

/// Delete every runner whose name starts with `prefix` (used by uninstall/gc).
pub fn delete_runners_by_prefix(client: &Client, prefix: &str) -> Result<u32> {
    let runners = client.list_runners()?;
    let mut n = 0;
    for r in runners.iter().filter(|r| r.name.starts_with(prefix)) {
        client.delete_runner(r.id)?;
        n += 1;
    }
    Ok(n)
}

/// Validate that a token can at least list runners for the scope.
pub fn check_token(gh: &GitHub, token: &str) -> Result<()> {
    let c = Client::new(gh, token.to_string())?;
    c.reachable()
        .map_err(|e| anyhow!("token/scope check failed: {e}"))?;
    if token.len() < 8 {
        bail!("token looks too short");
    }
    Ok(())
}

/// Seconds to wait given a rate-limited response: Retry-After, else
/// X-RateLimit-Reset (epoch) minus now.
fn rate_limit_wait(resp: &ureq::Response) -> Option<u64> {
    if let Some(ra) = resp
        .header("retry-after")
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        return Some(ra);
    }
    if resp.header("x-ratelimit-remaining").map(|s| s.trim()) == Some("0") {
        let reset = resp
            .header("x-ratelimit-reset")?
            .trim()
            .parse::<u64>()
            .ok()?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        return Some(reset.saturating_sub(now));
    }
    None
}

/// Extract the `rel="next"` URL from a Link header, if present.
fn next_link(link: Option<&str>) -> Option<String> {
    let link = link?;
    for part in link.split(',') {
        if part.contains("rel=\"next\"") {
            let seg = part.split(';').next()?.trim();
            return Some(
                seg.trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string(),
            );
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_next_link() {
        let h = "<https://api.github.com/x?page=2>; rel=\"next\", <https://api.github.com/x?page=5>; rel=\"last\"";
        assert_eq!(
            next_link(Some(h)),
            Some("https://api.github.com/x?page=2".to_string())
        );
        assert_eq!(next_link(Some("<https://x>; rel=\"last\"")), None);
        assert_eq!(next_link(None), None);
    }
}
