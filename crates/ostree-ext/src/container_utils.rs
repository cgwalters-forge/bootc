//! Helpers for interacting with containers at runtime.

use std::io;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use ocidir::cap_std::fs::Dir;
use ostree::glib;

use crate::keyfileext::KeyFileExt;

/// The relative path to the stamp file which signals this is an ostree-booted system.
pub const OSTREE_BOOTED: &str = "run/ostree-booted";

// See https://github.com/coreos/rpm-ostree/pull/3285#issuecomment-999101477
// For compatibility with older ostree, we stick this in /sysroot where
// it will be ignored.
const V0_REPO_CONFIG: &str = "/sysroot/config";
const V1_REPO_CONFIG: &str = "/sysroot/ostree/repo/config";

/// Where rpm-ostree based images (Fedora CoreOS, fedora-bootc, ...) keep the
/// rpm database, and where rpm-ostree writes it when installing packages in a
/// container build. Plain Fedora, CentOS and RHEL images keep it in
/// `/usr/lib/sysimage/rpm` or `/var/lib/rpm` instead.
const OSTREE_RPMDB: &str = "/usr/share/rpm";

/// Set by systemd for the processes of a unit.
const SYSTEMD_INVOCATION_ID: &str = "INVOCATION_ID";

/// Attempts to detect if the current process is running inside a container.
/// This looks for the `container` environment variable or the presence
/// of Docker or podman's more generic `/run/.containerenv`.
/// This is a best-effort function, as there is not a 100% reliable way
/// to determine this.
pub fn running_in_container() -> bool {
    if std::env::var_os("container").is_some() {
        return true;
    }
    // https://stackoverflow.com/questions/20010199/how-to-determine-if-a-process-runs-inside-lxc-docker
    for p in ["/run/.containerenv", "/.dockerenv"] {
        if std::path::Path::new(p).exists() {
            return true;
        }
    }
    false
}

// https://docs.rs/openat-ext/0.1.10/openat_ext/trait.OpenatDirExt.html#tymethod.open_file_optional
// https://users.rust-lang.org/t/why-i-use-anyhow-error-even-in-libraries/68592
pub(crate) fn open_optional(path: impl AsRef<Path>) -> std::io::Result<Option<std::fs::File>> {
    match std::fs::File::open(path.as_ref()) {
        Ok(r) => Ok(Some(r)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Returns `true` if the current root filesystem has an ostree repository in `bare-split-xattrs` mode.
/// This will be the case in a running ostree-native container.
pub fn is_bare_split_xattrs() -> Result<bool> {
    if let Some(configf) = open_optional(V1_REPO_CONFIG)
        .transpose()
        .or_else(|| open_optional(V0_REPO_CONFIG).transpose())
    {
        let configf = configf?;
        let mut bufr = std::io::BufReader::new(configf);
        let mut s = String::new();
        bufr.read_to_string(&mut s)?;
        let kf = glib::KeyFile::new();
        kf.load_from_data(&s, glib::KeyFileFlags::NONE)?;
        let r = if let Some(mode) = kf.optional_string("core", "mode")? {
            mode == crate::tar::BARE_SPLIT_XATTRS_MODE
        } else {
            false
        };
        Ok(r)
    } else {
        Ok(false)
    }
}

/// Returns true if the system appears to have been booted via ostree.
/// This accesses global state in /run.
pub fn ostree_booted() -> io::Result<bool> {
    Path::new(&format!("/{OSTREE_BOOTED}")).try_exists()
}

/// Returns true if the target root appears to have been booted via ostree.
pub fn is_ostree_booted_in(rootfs: &Dir) -> io::Result<bool> {
    rootfs.try_exists(OSTREE_BOOTED)
}

/// The facts [`is_ostree_container`] decides on, read from the running system.
#[derive(Debug, Default, Clone, Copy)]
struct ContainerSignals {
    /// See [`ostree_booted`].
    ostree_booted: bool,
    /// The root filesystem has an ostree image layout: an rpm database in
    /// [`OSTREE_RPMDB`], or an ostree repository in `bare-split-xattrs` mode
    /// (see [`is_bare_split_xattrs`]).
    ostree_layout: bool,
    /// See [`running_in_container`].
    container_markers: bool,
    /// We are part of a systemd unit.
    in_systemd_unit: bool,
    /// The system was booted with systemd (`/run/systemd/system` exists).
    systemd_booted: bool,
}

impl ContainerSignals {
    fn read() -> Result<Self> {
        let ostree_layout = Path::new(OSTREE_RPMDB)
            .try_exists()
            .with_context(|| format!("Checking for {OSTREE_RPMDB}"))?
            || is_bare_split_xattrs()?;
        Ok(Self {
            ostree_booted: ostree_booted().context("Checking for ostree-booted")?,
            ostree_layout,
            container_markers: running_in_container(),
            in_systemd_unit: std::env::var_os(SYSTEMD_INVOCATION_ID).is_some(),
            systemd_booted: libsystemd::daemon::booted(),
        })
    }

    fn is_ostree_container(&self) -> bool {
        // A booted ostree host, or a chroot into one, is never a container
        // build, even with a `container` variable leaked into its environment.
        if self.ostree_booted || !self.ostree_layout {
            return false;
        }
        // BuildKit (`docker buildx`) RUN steps set none of the container
        // markers, so otherwise take the absence of systemd as the sign of a
        // build environment.
        self.container_markers || !(self.in_systemd_unit || self.systemd_booted)
    }
}

/// Returns `true` if the current booted filesystem appears to be an ostree-native container.
///
/// That is, the root filesystem has an ostree image layout (an rpm database in
/// `/usr/share/rpm`, or an ostree repository in `bare-split-xattrs` mode), the
/// system was not booted via ostree, and either [`running_in_container`] is
/// true or there is no sign of systemd.
///
/// The ostree repository is not required, as images may omit it.
pub fn is_ostree_container() -> Result<bool> {
    Ok(ContainerSignals::read()?.is_ostree_container())
}

/// Returns an error unless the current filesystem is an ostree-based container
///
/// This just wraps [`is_ostree_container`].
pub fn require_ostree_container() -> Result<()> {
    if !is_ostree_container()? {
        anyhow::bail!("Not in an ostree-based container environment");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_ostree_container() {
        let layout = ContainerSignals {
            ostree_layout: true,
            ..Default::default()
        };
        let cases = [
            (
                "booted host, leaked container=",
                ContainerSignals {
                    ostree_booted: true,
                    container_markers: true,
                    systemd_booted: true,
                    ..layout
                },
                false,
            ),
            (
                "booted host, systemd unit",
                ContainerSignals {
                    ostree_booted: true,
                    in_systemd_unit: true,
                    systemd_booted: true,
                    ..layout
                },
                false,
            ),
            (
                "booted non-ostree host",
                ContainerSignals {
                    systemd_booted: true,
                    ..layout
                },
                false,
            ),
            (
                "ostree container",
                ContainerSignals {
                    container_markers: true,
                    ..layout
                },
                true,
            ),
            (
                "ostree container running systemd",
                ContainerSignals {
                    container_markers: true,
                    systemd_booted: true,
                    ..layout
                },
                true,
            ),
            ("BuildKit build root", layout, true),
            (
                "build root in a systemd unit",
                ContainerSignals {
                    in_systemd_unit: true,
                    ..layout
                },
                false,
            ),
            (
                "plain container",
                ContainerSignals {
                    container_markers: true,
                    ..Default::default()
                },
                false,
            ),
            ("plain build root", ContainerSignals::default(), false),
        ];
        for (name, signals, expected) in cases {
            assert_eq!(
                signals.is_ostree_container(),
                expected,
                "{name}: {signals:?}"
            );
        }
    }
}
