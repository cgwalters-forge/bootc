//! Handling of system restarts/reboot

use std::{io::Write, process::Command};

use anyhow::Context;
use bootc_utils::CommandRunExt;
use fn_error_context::context;
use serde::Deserialize;

/// The inhibitor mode which asks for an operation not to happen at all.
const INHIBIT_MODE_BLOCK: &str = "block";
/// The inhibitor "what" for system shutdown and reboot.
const INHIBIT_WHAT_SHUTDOWN: &str = "shutdown";

/// An inhibitor lock as returned by logind's `ListInhibitors`, whose D-Bus
/// signature is `a(ssssuu)`.
#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Inhibitor {
    /// Colon-separated list of what is inhibited, e.g. `shutdown:sleep`.
    what: String,
    who: String,
    why: String,
    /// `block`, `block-weak` or `delay`.
    mode: String,
    uid: u32,
    pid: u32,
}

impl Inhibitor {
    /// Whether this lock asks for reboots not to happen at all.
    fn blocks_shutdown(&self) -> bool {
        self.mode == INHIBIT_MODE_BLOCK && self.what.split(':').any(|w| w == INHIBIT_WHAT_SHUTDOWN)
    }
}

/// The output of `busctl --json=short call` for a method returning `a(ssssuu)`.
#[derive(Debug, Deserialize)]
struct ListInhibitorsReply {
    data: (Vec<Inhibitor>,),
}

/// Parse the JSON output of `busctl call ... ListInhibitors`, returning the
/// locks which block shutdown.
fn blocking_inhibitors(json: &str) -> anyhow::Result<Vec<Inhibitor>> {
    let reply: ListInhibitorsReply =
        serde_json::from_str(json).context("Parsing ListInhibitors reply")?;
    let (inhibitors,) = reply.data;
    Ok(inhibitors
        .into_iter()
        .filter(Inhibitor::blocks_shutdown)
        .collect())
}

/// Ask logind for the locks which block shutdown.
fn query_blocking_inhibitors() -> anyhow::Result<Vec<Inhibitor>> {
    let reply = Command::new("busctl")
        .args([
            "--json=short",
            "call",
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
            "ListInhibitors",
        ])
        .run_get_string()
        .context("Querying logind")?;
    blocking_inhibitors(&reply)
}

/// Fail if a process holds a block mode shutdown inhibitor lock.
///
/// Something that takes such a lock (a long-running job, a package manager,
/// etc.) is explicitly asking for the system not to be rebooted underneath
/// it. This mirrors rpm-ostree, and unlike `systemctl reboot
/// --check-inhibitors=yes` it doesn't also refuse when other users are
/// logged in: that would include the administrator's own session.
///
/// Unlike rpm-ostree, if logind can't be queried (e.g. it isn't running) we
/// only warn and allow the reboot: failing closed would break every
/// `--apply` (and automatic updates) on such systems, and where logind does
/// run, systemd 257 and newer enforce block locks themselves.
#[context("Checking for shutdown inhibitors")]
fn check_inhibitors() -> anyhow::Result<()> {
    let blocking = match query_blocking_inhibitors() {
        Ok(v) => v,
        Err(e) => {
            crate::utils::medium_visibility_warning(&format!(
                "warning: Failed to check for shutdown inhibitors, rebooting anyway: {e:#}"
            ));
            return Ok(());
        }
    };
    if blocking.is_empty() {
        return Ok(());
    }
    let holders = blocking
        .iter()
        .map(|i| format!("\"{}\" (PID {}, UID {}): {}", i.who, i.pid, i.uid, i.why))
        .collect::<Vec<_>>()
        .join("; ");
    anyhow::bail!(
        "Reboot blocked by shutdown inhibitor: {holders}. \
         Pending changes take effect on the next boot; use \
         `systemctl reboot --check-inhibitors=no` to reboot anyway."
    )
}

/// Initiate a system reboot.
/// This function will only return in case of error.
#[context("Initiating reboot")]
pub(crate) fn reboot() -> anyhow::Result<()> {
    check_inhibitors()?;
    // Flush output streams
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // Wait for the transient unit and pass through its stderr, so that if
    // the reboot is refused (e.g. due to an inhibitor) we report an error
    // instead of sleeping forever below. Note --wait/--pipe talk to systemd
    // via the D-Bus system bus rather than /run/systemd/private, so without
    // dbus (e.g. rescue.target) this fails loudly; staged changes are kept.
    Command::new("systemd-run")
        .args([
            "--quiet",
            "--wait",
            "--pipe",
            "--collect",
            "--",
            "systemctl",
            "reboot",
            "--message=Initiated by bootc",
        ])
        .run_capture_stderr()?;
    // We expect to be terminated via SIGTERM here. We sleep
    // instead of exiting an exit would necessarily appear
    // racy to calling processes in that sometimes we'd
    // win the race to exit, other times might get killed
    // via SIGTERM.
    tracing::debug!("Initiated reboot, sleeping");
    loop {
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blocking_inhibitors() {
        // (busctl output, the `who` of each lock expected to block reboot)
        let cases: &[(&str, &[&str])] = &[
            (r#"{"type":"a(ssssuu)","data":[[]]}"#, &[]),
            (
                r#"{"type":"a(ssssuu)","data":[[
                    ["shutdown:sleep","NetworkManager","Waiting","delay",0,901],
                    ["handle-power-key:handle-suspend-key","gdm","GNOME","block",42,1788],
                    ["sleep","backup","Backing up","block",0,2001],
                    ["idle:shutdown","dnf","Installing","block",0,2002],
                    ["shutdown","weak","Maybe","block-weak",0,2003],
                    ["shutdown","bootc-test","testing","block",0,2004]
                ]]}"#,
                &["dnf", "bootc-test"],
            ),
        ];
        for (json, expected) in cases {
            let found = blocking_inhibitors(json).unwrap();
            let found = found.iter().map(|i| i.who.as_str()).collect::<Vec<_>>();
            assert_eq!(&found, expected, "{json}");
        }
    }

    #[test]
    fn test_blocking_inhibitors_malformed() {
        for json in [
            "",
            "not json",
            r#"{"type":"a(ssssuu)"}"#,
            r#"{"type":"a(ssssuu)","data":[]}"#,
            r#"{"type":"a(ssssuu)","data":[[["shutdown","who","why","block"]]]}"#,
            r#"{"type":"a(ssssuu)","data":[[["shutdown","who","why","block","0","1"]]]}"#,
        ] {
            assert!(blocking_inhibitors(json).is_err(), "{json:?}");
        }
    }
}
