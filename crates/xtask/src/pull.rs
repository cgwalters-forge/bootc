//! Retry whole pulls: Podman's internal retries miss wrapped mid-blob EOFs.
//! Permanent failures take precedence over transient text anywhere in stderr.

use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};

const RETRIES_ENV: &str = "BOOTC_CI_RETRIES";
const DELAY_ENV: &str = "BOOTC_CI_DELAY";
const DEFAULT_ATTEMPTS: u64 = 1;
const DEFAULT_DELAY_SECS: u64 = 2;
const MAX_ATTEMPTS: u64 = 10;
// Bound retry amplification even when CI supplies a very large delay.
const MAX_DELAY: Duration = Duration::from_secs(30);

const PERMANENT_PATTERNS: &[&str] = &[
    "signature",
    "rejected by policy",
    "source image rejected",
    "policy requirement",
    "unauthorized",
    "authentication",
    "access denied",
    "access to the requested resource",
    "denied:",
    "forbidden",
    "invalid username",
    "invalid password",
    "invalid credentials",
    "manifest unknown",
    "manifest_unknown",
    "name unknown",
    "x509:",
    "certificate",
    "tls verification",
    "tls: failed to verify",
];

const TRANSIENT_PATTERNS: &[&str] = &[
    "unexpected eof",
    ": eof",
    "connection reset by peer",
    "connection refused",
    "i/o timeout",
    "tls handshake timeout",
    "429 too many requests",
    "500 internal server error",
    "502 bad gateway",
    "503 service unavailable",
    "504 gateway timeout",
    // containers/image's registryHTTPResponseToError includes the status
    // code separately from an arbitrary (possibly empty) response body.
    "statuscode: 429,",
    "statuscode: 500,",
    "statuscode: 502,",
    "statuscode: 503,",
    "statuscode: 504,",
    // The token-request path reports 429 without its numeric status.
    "too many requests to registry",
];

struct Attempt {
    success: bool,
    stderr: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Policy {
    attempts: u64,
    delay: Duration,
}

impl Policy {
    // Inject lookup so parsing tests never mutate process-global environment.
    fn from_env(mut lookup: impl FnMut(&str) -> Result<Option<String>>) -> Result<Self> {
        let mut parse = |name, default| -> Result<u64> {
            match lookup(name)?.filter(|v| !v.is_empty()) {
                None => Ok(default),
                Some(v) => v
                    .parse()
                    .with_context(|| format!("Parsing {name}={v:?} as a non-negative integer")),
            }
        };
        Ok(Self {
            attempts: parse(RETRIES_ENV, DEFAULT_ATTEMPTS)?.clamp(1, MAX_ATTEMPTS),
            delay: Duration::from_secs(parse(DELAY_ENV, DEFAULT_DELAY_SECS)?).min(MAX_DELAY),
        })
    }
}

fn is_transient(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    !PERMANENT_PATTERNS.iter().any(|p| stderr.contains(p))
        && TRANSIENT_PATTERNS.iter().any(|p| stderr.contains(p))
}

fn pull_with_retries(
    image: &str,
    policy: Policy,
    run: &mut dyn FnMut(&str) -> Result<Attempt>,
    sleep: &mut dyn FnMut(Duration),
) -> Result<()> {
    let mut delay = policy.delay;
    for attempt in 1..=policy.attempts {
        let result = run(image)?;
        if result.success {
            return Ok(());
        }
        if !is_transient(&result.stderr) || attempt == policy.attempts {
            anyhow::bail!(
                "Pulling {image} failed on attempt {attempt}/{}: {}",
                policy.attempts,
                result.stderr.trim()
            );
        }
        eprintln!(
            "Transient error pulling {image} (attempt {attempt}/{}), retrying in {delay:?}: {}",
            policy.attempts,
            result.stderr.trim()
        );
        sleep(delay);
        delay = (delay * 2).min(MAX_DELAY);
    }
    unreachable!("policy has at least one attempt")
}

// Accept a command for command-boundary tests without changing PATH globally.
fn podman_pull(command: &mut Command, image: &str) -> Result<Attempt> {
    let output = command
        .args(["pull", "-q", "--retry=0", image])
        .stdout(Stdio::inherit())
        .output()
        .context("Running podman pull")?;
    Ok(Attempt {
        success: output.status.success(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

pub(crate) fn pull_images(images: &[String]) -> Result<()> {
    let policy = Policy::from_env(|name| match std::env::var(name) {
        Ok(v) => Ok(Some(v)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("Reading {name}")),
    })?;
    for image in images {
        pull_with_retries(
            image,
            policy,
            &mut |image| podman_pull(&mut Command::new("podman"), image),
            &mut std::thread::sleep,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISSUE_EOF: &str = "Error: copying system image from manifest list: writing blob: storing blob to file: happened during read: unexpected EOF (while reconnecting: Get https://cdn01.quay.io/blob: EOF)";
    const IMAGE: &str = "quay.io/example:latest";

    fn policy(attempts: Option<&str>, delay: Option<&str>) -> Result<Policy> {
        Policy::from_env(|name| {
            Ok(if name == RETRIES_ENV { attempts } else { delay }.map(str::to_owned))
        })
    }

    fn simulate(policy: Policy, results: &[(bool, &str)]) -> (Result<()>, usize, Vec<Duration>) {
        let mut calls = 0;
        let mut sleeps = Vec::new();
        let result = pull_with_retries(
            IMAGE,
            policy,
            &mut |image| {
                assert_eq!(image, IMAGE);
                let (success, stderr) = results[calls];
                calls += 1;
                Ok(Attempt {
                    success,
                    stderr: stderr.into(),
                })
            },
            &mut |d| sleeps.push(d),
        );
        (result, calls, sleeps)
    }

    #[test]
    fn environment_policy() {
        for (attempts, delay, expected_attempts, expected_delay) in [
            (None, None, 1, 2),
            (Some(""), Some(""), 1, 2),
            (Some("3"), Some("2"), 3, 2),
            (Some("0"), Some("0"), 1, 0),
            (Some("10"), Some("30"), 10, 30),
            (
                Some("18446744073709551615"),
                Some("18446744073709551615"),
                10,
                30,
            ),
        ] {
            assert_eq!(
                policy(attempts, delay).unwrap(),
                Policy {
                    attempts: expected_attempts,
                    delay: Duration::from_secs(expected_delay)
                }
            );
        }
        for invalid in ["-1", "abc", "1.5", " 3", "18446744073709551616"] {
            for (attempts, delay, name) in [
                (Some(invalid), None, RETRIES_ENV),
                (None, Some(invalid), DELAY_ENV),
            ] {
                assert!(
                    policy(attempts, delay)
                        .unwrap_err()
                        .to_string()
                        .contains(name)
                );
            }
        }
        assert!(Policy::from_env(|_| anyhow::bail!("non-Unicode environment")).is_err());
    }

    #[test]
    fn transient_then_success_and_exhaustion() {
        let policy = policy(Some("3"), Some("2")).unwrap();
        for message in [
            ISSUE_EOF,
            "Get https://quay.io/v2/: EOF",
            "net/http: TLS handshake timeout",
            "read: connection reset by peer",
            "dial tcp: connection refused",
            "i/o timeout",
            "received unexpected HTTP status: 429 Too Many Requests",
            "500 Internal Server Error",
            "502 Bad Gateway",
            "503 Service Unavailable",
            "504 Gateway Timeout",
            "Error: StatusCode: 429, \"\"",
            "Error: StatusCode: 500, \"upstream unavailable\"",
            "Error: StatusCode: 502, \"\"",
            "Error: StatusCode: 503, \"backend error\"",
            "Error: StatusCode: 504, \"\"",
            "Error: fetching token: too many requests to registry",
        ] {
            let (r, calls, sleeps) = simulate(policy, &[(false, message), (true, "")]);
            r.unwrap();
            assert_eq!(calls, 2);
            assert_eq!(sleeps, [Duration::from_secs(2)]);
        }
        let (r, calls, sleeps) = simulate(
            policy,
            &[
                (false, ISSUE_EOF),
                (false, "502 Bad Gateway"),
                (false, "504 Gateway Timeout: final diagnostic"),
            ],
        );
        let error = r.unwrap_err().to_string();
        assert!(
            error.ends_with("504 Gateway Timeout: final diagnostic"),
            "{error}"
        );
        assert!(!error.contains(ISSUE_EOF));
        assert_eq!(calls, 3);
        assert_eq!(sleeps, [2, 4].map(Duration::from_secs));
    }

    #[test]
    fn permanent_and_mixed_fail_immediately() {
        let assert_immediate = |diagnostic: &str| {
            let (r, calls, sleeps) =
                simulate(policy(Some("3"), None).unwrap(), &[(false, diagnostic)]);
            assert!(r.unwrap_err().to_string().ends_with(diagnostic));
            assert_eq!(calls, 1, "{diagnostic}");
            assert!(sleeps.is_empty(), "{diagnostic}");
        };
        for message in [
            "A signature was required, but no signature exists",
            "rejected by policy",
            "unauthorized",
            "authentication required",
            "invalid credentials",
            "denied: requested access",
            "forbidden",
            "manifest unknown",
            "manifest_unknown",
            "x509: unknown authority",
            "certificate verification failed",
        ] {
            for diagnostic in [
                message.to_owned(),
                format!("{ISSUE_EOF}\n{message}"),
                format!("{message}\n503 Service Unavailable"),
                format!("StatusCode: 503, \"\"\n{message}"),
            ] {
                assert_immediate(&diagnostic);
            }
        }
        for unknown in [
            "unknown failure",
            "TLS handshake failure",
            "",
            "HTTP status: 501 Not Implemented",
            "StatusCode: 501, \"\"",
            "StatusCode: 5030, \"\"",
        ] {
            assert_immediate(unknown);
        }
    }

    #[test]
    fn exact_caps() {
        for (delay, expected) in [
            ("2", vec![2, 4, 8, 16, 30, 30, 30, 30, 30]),
            ("999", vec![30; 9]),
            ("0", vec![0; 9]),
        ] {
            let (r, calls, sleeps) = simulate(
                policy(Some("999"), Some(delay)).unwrap(),
                &[(false, ISSUE_EOF); 10],
            );
            assert!(r.is_err());
            assert_eq!(calls, 10);
            assert_eq!(
                sleeps,
                expected
                    .into_iter()
                    .map(Duration::from_secs)
                    .collect::<Vec<_>>()
            );
        }
        let (r, calls, sleeps) = simulate(policy(None, None).unwrap(), &[(false, ISSUE_EOF)]);
        assert!(r.is_err());
        assert_eq!(calls, 1);
        assert!(sleeps.is_empty());
    }

    #[test]
    fn fake_podman_permanent_failure() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("podman");
        let log = dir.path().join("calls");
        // Invoke via sh: no executable fixture or global PATH mutation needed.
        std::fs::write(&script, "printf '%s\\n' \"$@\" >> \"$CALL_LOG\"\nprintf '%s\\n' 'unauthorized: unexpected EOF' >&2\nexit 1\n").unwrap();
        let mut sleeps = Vec::new();
        let error = pull_with_retries(
            IMAGE,
            policy(Some("10"), None).unwrap(),
            &mut |image| podman_pull(Command::new("sh").arg(&script).env("CALL_LOG", &log), image),
            &mut |d| sleeps.push(d),
        )
        .unwrap_err();
        assert!(error.to_string().contains("unauthorized: unexpected EOF"));
        assert_eq!(
            std::fs::read_to_string(log).unwrap(),
            format!("pull\n-q\n--retry=0\n{IMAGE}\n")
        );
        assert!(sleeps.is_empty());
    }
}
