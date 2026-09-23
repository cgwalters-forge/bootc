//! Handling of system restarts/reboot

use std::{io::Write, process::Command};

use bootc_utils::CommandRunExt;
use fn_error_context::context;

/// Initiate a system reboot.
/// This function will only return in case of error.
#[context("Initiating reboot")]
pub(crate) fn reboot() -> anyhow::Result<()> {
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
