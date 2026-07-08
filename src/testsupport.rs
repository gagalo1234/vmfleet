//! Shared test scaffolding: a recording `Runner` mock and a localhost GitHub
//! fixture server. Lives in one place so systemd/supervisor tests and the offline
//! e2e all drive the code through the same seams (`cmd::Runner`, `api_base`).

use crate::cmd::{CmdOut, Runner};
use anyhow::Result;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// A `Runner` that records every `(program, args)` invocation and returns scripted
/// output. `Mutex` (not `RefCell`) so it satisfies the `Runner: Send + Sync` bound.
#[derive(Default)]
pub struct RecordingRunner {
    calls: Mutex<Vec<(String, Vec<String>)>>,
    /// stdout returned for a `systemctl … list-units … --state=active,activating`
    /// query — one unit name per line. Empty by default (no active workers).
    active_units: String,
}

impl RecordingRunner {
    /// Pre-seed the set of active worker units the mock reports to `list_active`.
    pub fn with_active_units(units: &[&str]) -> Self {
        RecordingRunner {
            active_units: units
                .iter()
                .map(|u| format!("{u} loaded active running\n"))
                .collect(),
            ..Default::default()
        }
    }

    /// Snapshot of every command the code under test issued, in order.
    pub fn calls(&self) -> Vec<(String, Vec<String>)> {
        self.calls.lock().unwrap().clone()
    }

    /// Whether any recorded call matches `program` with `args` containing `needle`.
    pub fn called(&self, program: &str, needle: &str) -> bool {
        self.calls()
            .iter()
            .any(|(p, a)| p == program && a.iter().any(|x| x.contains(needle)))
    }
}

impl Runner for RecordingRunner {
    fn run(&self, program: &str, args: &[&str], _timeout: Option<Duration>) -> Result<CmdOut> {
        self.calls.lock().unwrap().push((
            program.to_string(),
            args.iter().map(|s| s.to_string()).collect(),
        ));
        let joined = args.join(" ");
        let stdout = if program == "systemctl"
            && joined.contains("list-units")
            && joined.contains("active,activating")
        {
            self.active_units.clone()
        } else {
            String::new()
        };
        Ok(CmdOut {
            status: 0,
            stdout,
            stderr: String::new(),
        })
    }
}

/// A throwaway localhost HTTP server that stands in for the GitHub REST API. Point
/// a `github::Client` at `base_url`: it serves `runners_json` for the list-runners
/// endpoint and an empty queued-runs list for everything else. Runs on a detached
/// thread for the lifetime of the test process.
pub struct FixtureGitHub {
    pub base_url: String,
}

impl FixtureGitHub {
    pub fn start(runners_json: impl Into<String>) -> FixtureGitHub {
        let runners_json = runners_json.into();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture github server");
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                // GET requests are small; a single read captures the request line.
                let mut buf = [0u8; 4096];
                let Ok(n) = stream.read(&mut buf) else {
                    continue;
                };
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let body = if path.contains("actions/runners") {
                    runners_json.clone()
                } else if path.contains("actions/runs") {
                    r#"{"workflow_runs":[]}"#.to_string()
                } else {
                    "{}".to_string()
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        FixtureGitHub {
            base_url: format!("http://{addr}"),
        }
    }
}

/// A throwaway localhost server standing in for GitHub's OAuth **device-flow**
/// endpoints (which live on github.com, not the REST `api_base`). Point the `oauth`
/// module at `base_url` via `VMFLEET_OAUTH_BASE`, or pass it straight to
/// `run_device_flow`. Serves `/login/device/code` (a code with `interval: 0` so the
/// poll loop runs without sleeping) and `/login/oauth/access_token`.
pub struct FixtureOAuth {
    pub base_url: String,
}

/// What the fixture should return.
enum OAuthBehavior {
    /// `authorization_pending` for the first `pending` polls, then `access_token`.
    Token { token: String, pending: usize },
    /// Token endpoint always returns this OAuth error code.
    Error(String),
    /// The `device/code` endpoint returns `200` with this error payload (models a
    /// misconfigured app, e.g. `device_flow_disabled`).
    DeviceError { error: String, description: String },
}

impl FixtureOAuth {
    /// Return `token` after `pending` `authorization_pending` responses.
    pub fn start(token: impl Into<String>, pending: usize) -> FixtureOAuth {
        Self::spawn(OAuthBehavior::Token {
            token: token.into(),
            pending,
        })
    }

    /// Always fail the token endpoint with the given OAuth error code.
    pub fn start_error(error: impl Into<String>) -> FixtureOAuth {
        Self::spawn(OAuthBehavior::Error(error.into()))
    }

    /// Fail the `device/code` endpoint with a `200 + {error, error_description}`.
    pub fn start_device_error(
        error: impl Into<String>,
        description: impl Into<String>,
    ) -> FixtureOAuth {
        Self::spawn(OAuthBehavior::DeviceError {
            error: error.into(),
            description: description.into(),
        })
    }

    fn spawn(behavior: OAuthBehavior) -> FixtureOAuth {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture oauth server");
        let addr = listener.local_addr().unwrap();
        let polls = Arc::new(AtomicUsize::new(0));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 4096];
                let Ok(n) = stream.read(&mut buf) else {
                    continue;
                };
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let body = if path.contains("device/code") {
                    match &behavior {
                        OAuthBehavior::DeviceError { error, description } => {
                            format!(r#"{{"error":"{error}","error_description":"{description}"}}"#)
                        }
                        _ => r#"{"device_code":"DC-123","user_code":"WXYZ-1234","verification_uri":"https://github.test/login/device","interval":0}"#.to_string(),
                    }
                } else if path.contains("oauth/access_token") {
                    match &behavior {
                        OAuthBehavior::Error(e) => format!(r#"{{"error":"{e}"}}"#),
                        OAuthBehavior::Token { token, pending } => {
                            let seen = polls.fetch_add(1, Ordering::SeqCst);
                            if seen < *pending {
                                r#"{"error":"authorization_pending"}"#.to_string()
                            } else {
                                format!(r#"{{"access_token":"{token}"}}"#)
                            }
                        }
                        // device flow bails at the device/code step in this mode, so
                        // the token endpoint is never reached — return a filler body.
                        OAuthBehavior::DeviceError { .. } => "{}".to_string(),
                    }
                } else {
                    "{}".to_string()
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        FixtureOAuth {
            base_url: format!("http://{addr}"),
        }
    }
}
