//! Experimental library interface for installer applications.
//!
//! This is `bootc install to-filesystem` as a function call. An installer (a
//! GUI, Anaconda, or a custom Rust program) partitions, formats and mounts
//! the target itself, and then hands the mounted root to
//! [`Installer::install_to_filesystem`]. Compared to running the CLI:
//!
//! - progress is reported as [`Event`]s to a callback, not parsed from output;
//! - the result is an [`InstallOutcome`] describing what was installed, such as
//!   the deployment's `/etc` and `/var` for injecting configuration before the
//!   first boot;
//! - failures are an [`Error`] carrying an [`ErrorKind`].
//!
//! **This interface is not stable**: it may change in any release, and is
//! only exposed to gather feedback; see
//! <https://github.com/bootc-dev/bootc/issues/542>.
//!
//! # Requirements on the calling process
//!
//! Unlike the CLI, this never re-executes the calling process. The caller
//! must:
//!
//! - run as root with `CAP_SYS_ADMIN`, outside of a container (the source
//!   image is always given explicitly, as with `--source-imgref`);
//! - provide what the CLI reads from the running root with `--source-imgref`:
//!   the install configuration (`/usr/lib/bootc/install`) and, for the ostree
//!   backend, ostree's `prepare-root.conf`;
//! - when installing an image with SELinux policy on a host with SELinux
//!   enabled, be able to write labels unknown to the host policy (e.g. run as
//!   `install_t`, or with SELinux permissive), or set
//!   [`Installer::disable_selinux`];
//! - expect bootc to set up some mounts it needs in the calling process's
//!   mount namespace (e.g. `efivarfs`, and a tmpfs over `/tmp` if it isn't
//!   one); run in a private one (e.g. via `unshare -m`) to keep them from the
//!   rest of the system.
//!
//! Status messages are still printed to stdout for now.

use std::fmt;
use std::sync::Arc;

use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use cap_std_ext::cap_std;
use linux_kernel_cmdline::utf8::CmdlineOwned;
use serde::Serialize;

use super::{
    BoundImagesOpt, Cleanup, InstallComposefsOpts, InstallConfigOpts, InstallSourceOpts,
    InstallTargetFilesystemOpts, InstallTargetOpts, InstallToFilesystemOpts,
};
pub use crate::spec::Bootloader;

/// A coarse step of the installation, in the order they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Step {
    /// Inspecting the source image, the target filesystem and the environment.
    Prepare,
    /// Fetching the image and writing the deployment.
    Deploy,
    /// Installing the bootloader.
    Bootloader,
    /// Copying logically bound images into the target.
    BoundImages,
    /// Labeling, trimming and remounting the target filesystems read-only.
    Finalize,
}

/// A progress event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// A step started; the previous one, if any, is complete.
    StepStarted(Step),
}

/// The storage backend of an installed system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Backend {
    /// The default, ostree-based backend.
    Ostree,
    /// The (experimental) composefs-native backend.
    Composefs,
}

/// What was installed. Paths are relative to the physical root, i.e. the
/// [`FilesystemTarget`]'s root path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct InstallOutcome {
    /// The storage backend used.
    pub backend: Backend,
    /// The stateroot (OS name) the deployment belongs to.
    pub stateroot: String,
    /// The deployment directory.
    pub deployment_path: Utf8PathBuf,
    /// The deployment's persistent `/etc`.
    pub etc_path: Utf8PathBuf,
    /// The stateroot's persistent `/var`, shared by its deployments.
    pub var_path: Utf8PathBuf,
    /// The bootloader that was set up.
    pub bootloader: Bootloader,
}

impl InstallOutcome {
    pub(crate) fn new(
        backend: Backend,
        stateroot: String,
        deployment_path: Utf8PathBuf,
        var_path: Utf8PathBuf,
        bootloader: Bootloader,
    ) -> Self {
        Self {
            backend,
            stateroot,
            etc_path: deployment_path.join("etc"),
            deployment_path,
            var_path,
            bootloader,
        }
    }
}

/// The broad class of an [`Error`], for deciding how to handle it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The options are invalid; the target was not modified.
    InvalidOptions,
    /// The calling process can't perform the install (see the module
    /// documentation); the target was not modified.
    Environment,
    /// The install itself failed; the target may have been partially written.
    Install,
}

/// An installation error.
///
/// Its [`Display`](fmt::Display) is the outermost message only; use
/// [`std::error::Error::source`] (or e.g. `anyhow`'s `{:#}`) for the causes.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    inner: anyhow::Error,
}

impl Error {
    fn new(kind: ErrorKind, inner: anyhow::Error) -> Self {
        Self { kind, inner }
    }

    /// Classify an error from the install code.
    fn from_install(inner: anyhow::Error) -> Self {
        let kind = if inner.chain().any(|e| e.is::<EnvironmentError>()) {
            ErrorKind::Environment
        } else {
            ErrorKind::Install
        };
        Self::new(kind, inner)
    }

    /// The class of this error.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.inner, f)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.inner.source()
    }
}

/// Raised by the install code when the calling process can't install, so
/// that [`Error`] can report [`ErrorKind::Environment`].
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct EnvironmentError(pub(crate) String);

type ProgressFn = Arc<dyn Fn(&Event) + Send + Sync>;

/// Whether the install runs for the `bootc` CLI or for a library caller,
/// which changes what it may do to the process.
#[derive(Clone, Default)]
pub(crate) enum Invocation {
    /// The CLI may re-execute itself and prints to stdout.
    #[default]
    Cli,
    /// A library caller gets progress through the callback, if any.
    Library(Option<ProgressFn>),
}

impl fmt::Debug for Invocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cli => f.write_str("Cli"),
            Self::Library(p) => write!(f, "Library(progress: {})", p.is_some()),
        }
    }
}

impl Invocation {
    pub(crate) fn is_library(&self) -> bool {
        matches!(self, Self::Library(_))
    }

    /// Report that `step` started.
    pub(crate) fn step(&self, step: Step) {
        tracing::debug!("Install step: {step:?}");
        if let Self::Library(Some(progress)) = self {
            progress(&Event::StepStarted(step));
        }
    }
}

/// A mounted target filesystem to install to, set up by the caller.
///
/// Like `bootc install to-filesystem`, the root must be a mount point, and
/// an ESP (and `/boot`, if separate) must be on the same disk, where the
/// bootloader installation finds them.
#[derive(Debug, Clone)]
pub struct FilesystemTarget {
    root_path: Utf8PathBuf,
    root_mount_spec: Option<String>,
    boot_mount_spec: Option<String>,
    skip_finalize: bool,
}

impl FilesystemTarget {
    /// Install to the filesystem mounted at `root_path`.
    pub fn new(root_path: impl Into<Utf8PathBuf>) -> Self {
        Self {
            root_path: root_path.into(),
            root_mount_spec: None,
            boot_mount_spec: None,
            skip_finalize: false,
        }
    }

    /// The `root=` source, e.g. `UUID=...` (by default, the root filesystem's UUID).
    pub fn root_mount_spec(mut self, spec: impl Into<String>) -> Self {
        self.root_mount_spec = Some(spec.into());
        self
    }

    /// The `/boot` source, if `/boot` is a separate filesystem.
    pub fn boot_mount_spec(mut self, spec: impl Into<String>) -> Self {
        self.boot_mount_spec = Some(spec.into());
        self
    }

    /// Leave the filesystems mounted read-write and untrimmed, e.g. to make
    /// further changes before finalizing them yourself.
    pub fn skip_finalize(mut self, skip: bool) -> Self {
        self.skip_finalize = skip;
        self
    }

    /// The root path given to [`FilesystemTarget::new`].
    pub fn root_path(&self) -> &Utf8Path {
        &self.root_path
    }
}

fn check_not_host_root(root: &Utf8Path) -> anyhow::Result<()> {
    let fd = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())
        .with_context(|| format!("Opening target root directory {root}"))?;
    anyhow::ensure!(
        !super::is_host_root(&fd)?,
        "{root} is the running system's root filesystem"
    );
    Ok(())
}

/// Installs a bootc image; see the [module documentation](self).
#[derive(Clone)]
pub struct Installer {
    source_imgref: String,
    target_imgref: Option<String>,
    kargs: Vec<String>,
    stateroot: Option<String>,
    bootloader: Option<Bootloader>,
    composefs_backend: bool,
    disable_selinux: bool,
    generic_image: bool,
    progress: Option<ProgressFn>,
}

impl fmt::Debug for Installer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Installer")
            .field("source_imgref", &self.source_imgref)
            .field("target_imgref", &self.target_imgref)
            .field("kargs", &self.kargs)
            .field("stateroot", &self.stateroot)
            .field("bootloader", &self.bootloader)
            .field("composefs_backend", &self.composefs_backend)
            .field("disable_selinux", &self.disable_selinux)
            .field("generic_image", &self.generic_image)
            .field("progress", &self.progress.is_some())
            .finish()
    }
}

impl Installer {
    /// Install from `source_imgref`, a container image reference with its
    /// transport, e.g. `oci:/path/to/layout` or `docker://quay.io/example/os:latest`.
    pub fn new(source_imgref: impl Into<String>) -> Self {
        Self {
            source_imgref: source_imgref.into(),
            target_imgref: None,
            kargs: Vec::new(),
            stateroot: None,
            bootloader: None,
            composefs_backend: false,
            disable_selinux: false,
            generic_image: false,
            progress: None,
        }
    }

    /// The registry image the installed system updates from, if not the source.
    pub fn target_imgref(mut self, imgref: impl Into<String>) -> Self {
        self.target_imgref = Some(imgref.into());
        self
    }

    /// Add a kernel argument.
    pub fn karg(mut self, karg: impl Into<String>) -> Self {
        self.kargs.push(karg.into());
        self
    }

    /// The stateroot name (default: `default`).
    pub fn stateroot(mut self, stateroot: impl Into<String>) -> Self {
        self.stateroot = Some(stateroot.into());
        self
    }

    /// The bootloader to set up (default: detected from the image).
    pub fn bootloader(mut self, bootloader: Bootloader) -> Self {
        self.bootloader = Some(bootloader);
        self
    }

    /// Use the composefs backend (the default for some images regardless).
    pub fn composefs_backend(mut self, enable: bool) -> Self {
        self.composefs_backend = enable;
        self
    }

    /// Disable SELinux in the installed system.
    pub fn disable_selinux(mut self, disable: bool) -> Self {
        self.disable_selinux = disable;
        self
    }

    /// Set up a disk image for other machines: install all bootloader types,
    /// and leave the firmware of the machine running the install alone.
    pub fn generic_image(mut self, enable: bool) -> Self {
        self.generic_image = enable;
        self
    }

    /// Call `f` with each progress [`Event`]. It runs on the installing
    /// thread, so it should return quickly (e.g. by sending to a channel).
    pub fn on_progress(mut self, f: impl Fn(&Event) + Send + Sync + 'static) -> Self {
        self.progress = Some(Arc::new(f));
        self
    }

    fn to_cli_opts(&self, target: &FilesystemTarget) -> anyhow::Result<InstallToFilesystemOpts> {
        anyhow::ensure!(
            !self.source_imgref.is_empty(),
            "The source image reference is empty"
        );
        let karg = (!self.kargs.is_empty())
            .then(|| self.kargs.iter().cloned().map(CmdlineOwned::from).collect());
        Ok(InstallToFilesystemOpts {
            filesystem_opts: InstallTargetFilesystemOpts {
                root_path: target.root_path.clone(),
                root_mount_spec: target.root_mount_spec.clone(),
                boot_mount_spec: target.boot_mount_spec.clone(),
                replace: None,
                acknowledge_destructive: false,
                skip_finalize: target.skip_finalize,
            },
            source_opts: InstallSourceOpts {
                source_imgref: Some(self.source_imgref.clone()),
            },
            target_opts: InstallTargetOpts {
                target_transport: super::DEFAULT_TARGET_TRANSPORT.into(),
                target_imgref: self.target_imgref.clone(),
                target_no_signature_verification: false,
                enforce_container_sigpolicy: false,
                run_fetch_check: false,
                skip_fetch_check: false,
                unified_storage_exp: false,
            },
            config_opts: InstallConfigOpts {
                disable_selinux: self.disable_selinux,
                karg,
                karg_delete: None,
                root_ssh_authorized_keys: None,
                generic_image: self.generic_image,
                bound_images: BoundImagesOpt::default(),
                stateroot: self.stateroot.clone(),
                bootupd_skip_boot_uuid: false,
                bootloader: self.bootloader,
            },
            composefs_opts: InstallComposefsOpts {
                composefs_backend: self.composefs_backend,
                ..Default::default()
            },
        })
    }

    /// Install to `target`, which the caller has set up and mounted.
    pub async fn install_to_filesystem(
        &self,
        target: &FilesystemTarget,
    ) -> Result<InstallOutcome, Error> {
        let opts = self
            .to_cli_opts(target)
            .map_err(|e| Error::new(ErrorKind::InvalidOptions, e))?;
        crate::cli::require_root(false).map_err(|e| Error::new(ErrorKind::Environment, e))?;
        // The CLI warns and waits for an interrupt when installing over the
        // running system's root; a library can't ask, so refuse it.
        check_not_host_root(&target.root_path)
            .map_err(|e| Error::new(ErrorKind::InvalidOptions, e))?;
        let invocation = Invocation::Library(self.progress.clone());
        invocation.step(Step::Prepare);
        super::install_to_filesystem(opts, false, Cleanup::Skip, invocation)
            .await
            .map_err(Error::from_install)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The builder must map onto `bootc install to-filesystem` with the
    /// CLI's defaults, so that both install the same way.
    #[test]
    fn test_to_cli_opts() {
        use clap::Parser;

        const SOURCE: &str = "oci:/srv/image";
        let cases: Vec<(Installer, FilesystemTarget, &[&str])> = vec![
            (
                Installer::new(SOURCE),
                FilesystemTarget::new("/target"),
                &[],
            ),
            (
                Installer::new(SOURCE)
                    .target_imgref("quay.io/example/os:latest")
                    .karg("console=ttyS0")
                    .karg("quiet")
                    .stateroot("exampleos")
                    .bootloader(Bootloader::Systemd)
                    .composefs_backend(true)
                    .disable_selinux(true)
                    .generic_image(true),
                FilesystemTarget::new("/target")
                    .root_mount_spec("UUID=abc")
                    .boot_mount_spec("UUID=def")
                    .skip_finalize(true),
                &[
                    "--target-imgref=quay.io/example/os:latest",
                    "--karg=console=ttyS0",
                    "--karg=quiet",
                    "--stateroot=exampleos",
                    "--bootloader=systemd",
                    "--composefs-backend",
                    "--disable-selinux",
                    "--generic-image",
                    "--root-mount-spec=UUID=abc",
                    "--boot-mount-spec=UUID=def",
                    "--skip-finalize",
                ],
            ),
        ];
        for (installer, target, args) in cases {
            let base = ["to-filesystem", "--source-imgref", SOURCE, "/target"];
            let cli = InstallToFilesystemOpts::try_parse_from(base.iter().chain(args)).unwrap();
            assert_eq!(installer.to_cli_opts(&target).unwrap(), cli, "{args:?}");
        }

        let target = FilesystemTarget::new("/target");
        let err = Installer::new("").to_cli_opts(&target).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn test_error_kind() {
        let env = anyhow::Error::new(EnvironmentError("no install_t".into())).context("Preparing");
        let err = Error::from_install(env);
        assert_eq!(err.kind(), ErrorKind::Environment);
        assert_eq!(err.to_string(), "Preparing");
        let source = std::error::Error::source(&err).unwrap();
        assert_eq!(source.to_string(), "no install_t");

        let err = Error::from_install(anyhow::anyhow!("bootupd failed"));
        assert_eq!(err.kind(), ErrorKind::Install);
    }

    #[test]
    fn test_progress() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let installer = Installer::new("oci:/srv/image").on_progress({
            let seen = seen.clone();
            move |ev| seen.lock().unwrap().push(ev.clone())
        });
        let invocation = Invocation::Library(installer.progress.clone());
        assert!(invocation.is_library());
        invocation.step(Step::Prepare);
        invocation.step(Step::Deploy);
        Invocation::Cli.step(Step::Finalize);
        assert_eq!(
            *seen.lock().unwrap(),
            [
                Event::StepStarted(Step::Prepare),
                Event::StepStarted(Step::Deploy)
            ]
        );
    }

    #[test]
    fn test_outcome_json() {
        let outcome = InstallOutcome::new(
            Backend::Ostree,
            "default".into(),
            "ostree/deploy/default/deploy/abc.0".into(),
            "ostree/deploy/default/var".into(),
            Bootloader::Grub,
        );
        let v = serde_json::to_value(&outcome).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "backend": "ostree",
                "stateroot": "default",
                "deploymentPath": "ostree/deploy/default/deploy/abc.0",
                "etcPath": "ostree/deploy/default/deploy/abc.0/etc",
                "varPath": "ostree/deploy/default/var",
                "bootloader": "grub",
            })
        );
    }
}
