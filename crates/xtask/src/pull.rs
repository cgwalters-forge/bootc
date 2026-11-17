//! Pull container images with bounded retries on transient transport errors.
//!
//! `podman pull --retry` does not treat errors such as
//! `writing blob ... happened during read: unexpected EOF` as retryable, so
//! CI jobs failed on the first attempt. This retries outside podman, but only
//! when stderr shows a transport error; anything else (signature policy
//! rejections, unknown manifests, ...) fails at once.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};

/// Environment variable overriding the number of attempts per image.
const RETRIES_ENV: &str = "BOOTC_CI_RETRIES";

/// Environment variable overriding the initial delay in seconds.
const DELAY_ENV: &str = "BOOTC_CI_DELAY";

const DEFAULT_ATTEMPTS: u32 = 3;

const DEFAULT_DELAY_SECS: u64 = 2;

/// Upper bound on attempts, whatever the environment says.
const MAX_ATTEMPTS: u32 = 10;

/// Upper bound on a single wait between attempts.
const MAX_DELAY: Duration = Duration::from_secs(120);

/// Lowercased stderr fragments that mark a transient transport error.
const TRANSIENT_PATTERNS: &[&str] = &[
    "unexpected eof",
    ": eof",
    "connection reset",
    "connection refused",
    "i/o timeout",
    "tls handshake",
    "429 too many requests",
    "500 internal server error",
    "502 bad gateway",
    "503 service unavailable",
    "504 gateway timeout",
];

/// The result of one pull attempt.
pub(crate) struct Attempt {
    pub(crate) success: bool,
    pub(crate) stderr: String,
}

/// Retry policy for [`pull_with_retries`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Policy {
    pub(crate) attempts: u32,
    pub(crate) delay: Duration,
}

impl Policy {
    /// Read the policy from [`RETRIES_ENV`] and [`DELAY_ENV`], clamped to
    /// [`MAX_ATTEMPTS`] and [`MAX_DELAY`].
    fn from_env() -> Result<Self> {
        let attempts = parse_env(RETRIES_ENV)?.unwrap_or(DEFAULT_ATTEMPTS.into());
        let delay = parse_env(DELAY_ENV)?.unwrap_or(DEFAULT_DELAY_SECS);
        Ok(Self {
            attempts: u32::try_from(attempts)
                .unwrap_or(MAX_ATTEMPTS)
                .clamp(1, MAX_ATTEMPTS),
            delay: Duration::from_secs(delay).min(MAX_DELAY),
        })
    }
}

fn parse_env(name: &str) -> Result<Option<u64>> {
    match std::env::var(name) {
        Ok(v) if v.is_empty() => Ok(None),
        Ok(v) => v
            .parse()
            .map(Some)
            .with_context(|| format!("Parsing {name}={v:?} as a non-negative integer")),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("Reading {name}")),
    }
}

/// Whether podman's stderr shows a transport error worth retrying.
pub(crate) fn is_transient(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    TRANSIENT_PATTERNS.iter().any(|p| stderr.contains(p))
}

/// Pull `image` via `run`, retrying transient failures with exponential
/// backoff via `sleep`.
pub(crate) fn pull_with_retries(
    image: &str,
    policy: Policy,
    run: &mut dyn FnMut(&str) -> Result<Attempt>,
    sleep: &mut dyn FnMut(Duration),
) -> Result<()> {
    let mut delay = policy.delay;
    for attempt in 1..=policy.attempts {
        let r = run(image)?;
        if r.success {
            return Ok(());
        }
        let last_error = r.stderr.trim();
        if !is_transient(&r.stderr) {
            anyhow::bail!("Pulling {image} failed (not retryable): {last_error}");
        }
        if attempt == policy.attempts {
            anyhow::bail!("Pulling {image} failed after {attempt} attempts: {last_error}");
        }
        eprintln!(
            "--- Transient error pulling {image} (attempt {attempt}/{}), retrying in {delay:?}",
            policy.attempts
        );
        sleep(delay);
        delay = (delay * 2).min(MAX_DELAY);
    }
    unreachable!("policy.attempts is at least 1")
}

/// Run `podman pull -q IMAGE`, echoing stderr while capturing it.
fn podman_pull(image: &str) -> Result<Attempt> {
    let mut child = Command::new("podman")
        .args(["pull", "-q", image])
        .stderr(Stdio::piped())
        .spawn()
        .context("Spawning podman")?;
    let stderr = child.stderr.take().expect("stderr is piped");
    let mut captured = Vec::new();
    let mut out = std::io::stderr().lock();
    for line in BufReader::new(stderr).split(b'\n') {
        let line = line.context("Reading podman stderr")?;
        // Ignore failures to echo; the captured copy is what matters.
        let _ = out.write_all(&line).and_then(|()| out.write_all(b"\n"));
        captured.extend_from_slice(&line);
        captured.push(b'\n');
    }
    drop(out);
    let status = child.wait().context("Waiting for podman")?;
    Ok(Attempt {
        success: status.success(),
        stderr: String::from_utf8_lossy(&captured).into_owned(),
    })
}

/// Entry point for `cargo xtask pull-images`.
pub(crate) fn pull_images(images: &[String]) -> Result<()> {
    let policy = Policy::from_env()?;
    for image in images {
        pull_with_retries(image, policy, &mut podman_pull, &mut std::thread::sleep)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The error from the failed package job cited in the issue.
    const ISSUE_EOF: &str = "Error: copying system image from manifest list: writing blob: storing blob to file \"/var/tmp/container_images_storage1/1\": happened during read: unexpected EOF (while reconnecting: Get \"https://cdn01.quay.io/quayio-production-s3/sha256/ab/abc\": EOF)";

    const TRANSIENT: &[&str] = &[
        ISSUE_EOF,
        "Error: reading blob sha256:abc: Get \"https://quay.io/v2/x/blobs/sha256:abc\": EOF",
        "Error: read tcp 10.0.0.1:1234->1.2.3.4:443: read: connection reset by peer",
        "Error: dial tcp 1.2.3.4:443: connect: connection refused",
        "Error: dial tcp 1.2.3.4:443: i/o timeout",
        "Error: Get \"https://quay.io/v2/\": net/http: TLS handshake timeout",
        "Error: reading manifest latest: received unexpected HTTP status: 429 Too Many Requests",
        "Error: reading manifest latest: received unexpected HTTP status: 500 Internal Server Error",
        "Error: reading blob: fetching blob: received unexpected HTTP status: 502 Bad Gateway",
        "Error: initializing source: received unexpected HTTP status: 503 Service Unavailable",
        "Error: initializing source: received unexpected HTTP status: 504 Gateway Timeout",
    ];

    const PERMANENT: &[&str] = &[
        "Error: Source image rejected: A signature was required, but no signature exists",
        "Error: initializing source docker://quay.io/x:y: reading manifest y in quay.io/x: manifest unknown",
        "Error: initializing source docker://quay.io/x:y: reading manifest y in quay.io/x: unauthorized: access to the requested resource is not authorized",
    ];

    const POLICY: Policy = Policy {
        attempts: 4,
        delay: Duration::from_secs(1),
    };

    /// Run [`pull_with_retries`] against canned results, returning the
    /// outcome, the number of attempts and the waits.
    fn simulate(results: &[(bool, &str)]) -> (Result<()>, usize, Vec<Duration>) {
        let mut calls = 0;
        let mut sleeps = Vec::new();
        let mut run = |image: &str| {
            assert_eq!(image, "quay.io/example:latest");
            let (success, stderr) = results[calls];
            calls += 1;
            Ok(Attempt {
                success,
                stderr: stderr.to_owned(),
            })
        };
        let r = pull_with_retries(
            "quay.io/example:latest",
            POLICY,
            &mut run,
            &mut |d| sleeps.push(d),
        );
        (r, calls, sleeps)
    }

    #[test]
    fn test_transient_retried_up_to_cap() {
        for msg in TRANSIENT {
            let failures = [(false, *msg); POLICY.attempts as usize];
            let (r, calls, sleeps) = simulate(&failures);
            let err = format!("{:#}", r.unwrap_err());
            assert!(err.contains("after 4 attempts"), "{err}");
            assert!(err.contains(msg), "{err}");
            assert_eq!(calls, 4, "{msg}");
            assert_eq!(sleeps, [1, 2, 4].map(Duration::from_secs), "{msg}");
        }
    }

    #[test]
    fn test_transient_then_success() {
        let (r, calls, sleeps) = simulate(&[(false, ISSUE_EOF), (true, "")]);
        r.unwrap();
        assert_eq!(calls, 2);
        assert_eq!(sleeps, [Duration::from_secs(1)]);
    }

    #[test]
    fn test_permanent_fails_at_once() {
        for msg in PERMANENT {
            let (r, calls, sleeps) = simulate(&[(false, *msg)]);
            let err = format!("{:#}", r.unwrap_err());
            assert!(err.contains("not retryable"), "{err}");
            assert!(err.contains(msg), "{err}");
            assert_eq!(calls, 1, "{msg}");
            assert!(sleeps.is_empty(), "{msg}");
        }
    }

    #[test]
    fn test_backoff_is_capped() {
        let policy = Policy {
            attempts: MAX_ATTEMPTS,
            delay: Duration::from_secs(60),
        };
        let mut run = |_: &str| {
            Ok(Attempt {
                success: false,
                stderr: ISSUE_EOF.into(),
            })
        };
        let mut sleeps = Vec::new();
        let r = pull_with_retries("x", policy, &mut run, &mut |d| sleeps.push(d));
        assert!(r.is_err());
        assert_eq!(sleeps.len(), MAX_ATTEMPTS as usize - 1);
        assert!(sleeps.iter().all(|d| *d <= MAX_DELAY));
    }
}
