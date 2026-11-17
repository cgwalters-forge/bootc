//! Mount helpers for bootc-initramfs

use std::{
    ffi::OsString,
    fmt::Debug,
    io::{ErrorKind, Read},
    os::fd::{AsFd, AsRawFd, OwnedFd},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::Parser;
use rustix::{
    fs::{CWD, Mode, OFlags, ResolveFlags, major, minor, mkdirat, openat, openat2, stat, symlink},
    io::Errno,
    mount::{
        FsMountFlags, MountAttrFlags, OpenTreeFlags, UnmountFlags, fsconfig_create,
        fsconfig_set_string, fsmount, open_tree, unmount,
    },
    path,
};

use serde::Deserialize;

use composefs::{
    fsverity::{CompareVerityError, FsVerityHashValue, MeasureVerityError, Sha512HashValue},
    mount::{FsHandle, MountOptions},
    mountcompat::{overlayfs_set_fd, overlayfs_set_lower_and_data_fds, prepare_mount},
    repository::{ImageNotFound, IntegrityPolicy, Repository, VerityState},
};
use composefs_boot::cmdline::{ComposefsCmdline, KARG_COMPOSEFS_DIGEST, KARG_V2};
use composefs_ctl::composefs;
use composefs_ctl::composefs_boot;

use fn_error_context::context;

use linux_kernel_cmdline::utf8::Cmdline;

// mount_setattr syscall support
const MOUNT_ATTR_RDONLY: u64 = 0x00000001;

#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

/// Set mount attributes using mount_setattr syscall
#[context("Setting mount attributes")]
#[allow(unsafe_code)]
fn mount_setattr(fd: impl AsFd, flags: libc::c_int, attr: &MountAttr) -> Result<()> {
    let ret = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            fd.as_fd().as_raw_fd(),
            c"".as_ptr(),
            flags,
            attr as *const MountAttr,
            std::mem::size_of::<MountAttr>(),
        )
    };
    if ret == -1 {
        Err(std::io::Error::last_os_error())?;
    }
    Ok(())
}

/// Set a detached mount tree read-only without reopening its path.
#[context("Setting mount readonly")]
pub fn set_mount_readonly(fd: impl AsFd) -> Result<()> {
    let attr = MountAttr {
        attr_set: MOUNT_ATTR_RDONLY,
        attr_clr: 0,
        propagation: 0,
        userns_fd: 0,
    };
    mount_setattr(fd, libc::AT_EMPTY_PATH, &attr)
}

/// Set a mount and every mount below it read-only.
#[context("Setting mount tree readonly")]
pub fn set_mount_tree_readonly(fd: impl AsFd) -> Result<()> {
    let attr = MountAttr {
        attr_set: MOUNT_ATTR_RDONLY,
        attr_clr: 0,
        propagation: 0,
        userns_fd: 0,
    };
    mount_setattr(fd, libc::AT_EMPTY_PATH | libc::AT_RECURSIVE, &attr)
}

/// Types of mounts supported by the configuration
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MountType {
    /// No mount; "root" is an alias meaning this dir is part of the root mount
    #[serde(alias = "root")]
    None,
    /// Bind mount
    Bind,
    /// Overlay mount
    Overlay,
    /// Transient mount; "volatile" is an alias (Unix convention for tmpfs)
    #[serde(alias = "volatile")]
    Transient,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
struct RootConfig {
    #[serde(default)]
    transient: bool,
}

/// Configuration for mount operations
#[derive(Debug, Default, Deserialize, PartialEq)]
pub struct MountConfig {
    /// The type of mount to use
    pub mount: Option<MountType>,
    #[serde(default)]
    /// Whether this mount should be transient (temporary)
    pub transient: bool,
}

/// The setup-root configuration, see `bootc-setup-root-conf(5)`.
#[derive(Debug, Deserialize, Default, PartialEq)]
pub struct Config {
    /// How `/etc` is mounted
    #[serde(default)]
    pub etc: MountConfig,
    /// How `/var` is mounted
    #[serde(default)]
    pub var: MountConfig,
    #[serde(default)]
    root: RootConfig,
}

/// Default path to the setup-root configuration file, relative to the booted root.
pub const SETUP_ROOT_CONF_PATH: &str = "/usr/lib/composefs/setup-root-conf.toml";

/// Read the image's setup-root policy relative to its mounted root.
/// An absent file selects the defaults; other I/O and parse errors are fatal.
pub fn read_config(root: impl AsFd) -> Result<Config> {
    // Image-controlled symlinks must not select policy from the host/initramfs.
    let fd = match openat2(
        root,
        SETUP_ROOT_CONF_PATH.trim_start_matches('/'),
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::IN_ROOT,
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(Config::default()),
        Err(err) => return Err(err).with_context(|| format!("Opening {SETUP_ROOT_CONF_PATH}")),
    };
    let mut text = String::new();
    std::fs::File::from(fd)
        .read_to_string(&mut text)
        .with_context(|| format!("Reading {SETUP_ROOT_CONF_PATH}"))?;
    toml::from_str(&text).with_context(|| format!("Parsing {SETUP_ROOT_CONF_PATH}"))
}

/// Returns `true` if the configuration at `path` requests a transient `/etc`
/// overlay.  Used by the systemd generator to decide whether to emit the
/// SELinux relabel unit *before* those mounts exist (the generator runs before
/// `local-fs.target`).
///
/// Returns `false` if the file is absent or unreadable (safe default: no unit
/// emitted for non-transient systems).
pub fn config_has_transient_submounts(path: &std::path::Path) -> bool {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!("Could not read {}: {e:#}", path.display());
            return false;
        }
    };
    let config: Config = match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("Could not parse {}: {e:#}", path.display());
            return false;
        }
    };
    // Only /etc overlay triggers the relabel unit.
    let is_transient = |mc: &MountConfig| match mc.mount {
        Some(mt) => mt == MountType::Transient,
        None => mc.transient,
    };
    is_transient(&config.etc)
}

/// Command-line arguments
#[derive(Parser, Debug)]
pub struct Args {
    #[arg(help = "Execute this command (for testing)")]
    /// Execute this command (for testing)
    pub cmd: Vec<OsString>,

    #[arg(
        long,
        default_value = "/sysroot",
        help = "sysroot directory in initramfs"
    )]
    /// sysroot directory in initramfs
    pub sysroot: PathBuf,

    #[arg(long, requires = "root_fs", help = "Config path (for testing)")]
    /// Config path (for testing)
    pub config: Option<PathBuf>,

    // we want to test in a userns, but can't mount erofs there
    #[arg(long, help = "Bind mount root-fs from (for testing)")]
    /// Bind mount root-fs from (for testing)
    pub root_fs: Option<PathBuf>,

    #[arg(long, help = "Kernel commandline args (for testing)")]
    /// Kernel commandline args (for testing)
    pub cmdline: Option<Cmdline<'static>>,

    #[arg(long, help = "Mountpoint (don't replace sysroot, for testing)")]
    /// Mountpoint (don't replace sysroot, for testing)
    pub target: Option<PathBuf>,
}

impl Args {
    /// Construct an offline root assembly request (for soft reboot or installation).
    /// `composefs_karg` selects the image and its fs-verity policy, without inheriting
    /// the running system's kernel command line or setup-root configuration.
    pub fn for_deployment(
        sysroot: PathBuf,
        target: Option<PathBuf>,
        composefs_karg: String,
    ) -> Self {
        Self {
            cmd: vec![],
            sysroot,
            config: None,
            root_fs: None,
            cmdline: Some(Cmdline::from(composefs_karg)),
            target,
        }
    }
}

/// Wrapper around [`composefs::mount::mount_at`]
pub fn mount_at_wrapper(
    fs_fd: impl AsFd,
    dirfd: impl AsFd,
    path: impl path::Arg + Debug + Clone,
) -> Result<()> {
    composefs::mount::mount_at(fs_fd, dirfd, path.clone())
        .with_context(|| format!("Mounting at path {path:?}"))
}

/// Wrapper around [`rustix::fs::openat`]
#[context("Opening dir {name:?}")]
pub fn open_dir(dirfd: impl AsFd, name: impl AsRef<Path> + Debug) -> Result<OwnedFd> {
    let res = openat(
        dirfd,
        name.as_ref(),
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    );

    Ok(res?)
}

#[context("Ensure dir")]
fn ensure_dir(dirfd: impl AsFd, name: &str, mode: Option<rustix::fs::Mode>) -> Result<OwnedFd> {
    match mkdirat(dirfd.as_fd(), name, mode.unwrap_or(0o700.into())) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(err) => Err(err).with_context(|| format!("Creating dir {name}"))?,
    }

    open_dir(dirfd, name)
}

#[context("Bind mounting to path {path}")]
fn bind_mount(fd: impl AsFd, path: &str) -> Result<OwnedFd> {
    let res = open_tree(
        fd.as_fd(),
        path,
        OpenTreeFlags::OPEN_TREE_CLONE
            | OpenTreeFlags::OPEN_TREE_CLOEXEC
            | OpenTreeFlags::AT_EMPTY_PATH,
    );

    Ok(res?)
}

/// Mount a tmpfs to use as the upper layer for an overlay.
///
/// TODO: sync these options with systemd's root mounting, there's some tweaks there for default tmpfs
/// and we may want to make this configurable anyways i nthe future
///
/// See <https://github.com/containers/bootc/issues/1992>.
#[context("Mounting tmpfs for overlay")]
fn mount_tmpfs_for_overlay() -> Result<OwnedFd> {
    let tmpfs = FsHandle::open("tmpfs")?;
    fsconfig_create(tmpfs.as_fd())?;
    Ok(fsmount(
        tmpfs.as_fd(),
        FsMountFlags::FSMOUNT_CLOEXEC,
        MountAttrFlags::empty(),
    )?)
}

/// Build an overlayfs fsmount fd from an existing state dir (upper+work).
///
/// upper is 0755: the merged view inherits permissions from upperdir, so 0700
/// would make the mountpoint inaccessible to non-root processes.  work is
/// kernel-internal and never visible; 0700 is fine.
/// See: <https://github.com/composefs/composefs-rs/issues/287>
fn build_overlay_fd(
    base: impl AsFd,
    state: impl AsFd,
    source: &str,
    mount_attr_flags: Option<MountAttrFlags>,
) -> Result<OwnedFd> {
    let upper = ensure_dir(state.as_fd(), "upper", Some(0o755.into()))?;
    let work = ensure_dir(state.as_fd(), "work", Some(0o700.into()))?;

    let overlayfs = FsHandle::open("overlay")?;
    fsconfig_set_string(overlayfs.as_fd(), "source", source)?;
    overlayfs_set_fd(overlayfs.as_fd(), "workdir", work.as_fd())?;
    overlayfs_set_fd(overlayfs.as_fd(), "upperdir", upper.as_fd())?;
    overlayfs_set_lower_and_data_fds(&overlayfs, base.as_fd(), &[])?;
    fsconfig_create(overlayfs.as_fd())?;
    Ok(fsmount(
        overlayfs.as_fd(),
        FsMountFlags::FSMOUNT_CLOEXEC,
        mount_attr_flags.unwrap_or(MountAttrFlags::empty()),
    )?)
}

/// Mount a persistent state directory as an overlay on top of `base`,
/// attaching the result immediately at `.` relative to `base`.
#[context("Mounting state as overlay")]
fn overlay_state(
    base: impl AsFd,
    state: impl AsFd,
    source: &str,
    mount_attr_flags: Option<MountAttrFlags>,
) -> Result<()> {
    let fs = build_overlay_fd(&base, state, source, mount_attr_flags)?;
    mount_at_wrapper(fs, base, ".").context("Moving mount")
}

/// Creates a transient overlayfs with the passed-in fd as the lowerdir.
///
/// Returns a detached (not yet attached) `OwnedFd` for the overlay mount.
/// The caller is responsible for attaching it to the filesystem tree.
///
/// `source` is used verbatim as the overlay's `source` fsconfig option and
/// appears in `/proc/self/mountinfo`.  For the composefs root, pass
/// `"transient:composefs=<digest_hex>"` so that `composefs_booted()` can
/// recover the verity digest from the mount source after switch-root.  For
/// non-root transient mounts (e.g. `/usr`, `/var`) pass `"transient"`.
///
/// The SELinux label on `/` is fixed after boot by
/// `bootc-early-overlay-relabel.service`; no initramfs-side xattr write is
/// needed (kernel `fs_use_trans tmpfs` relabeling at policy-load time would
/// overwrite anything written here).
#[context("Creating transient overlayfs")]
pub fn overlay_transient(
    base: impl AsFd,
    source: &str,
    mount_attr_flags: Option<MountAttrFlags>,
) -> Result<OwnedFd> {
    let tmpfs = mount_tmpfs_for_overlay()?;
    let state = prepare_mount(tmpfs)?;
    build_overlay_fd(base, state, source, mount_attr_flags)
}

#[context("Opening rootfs")]
fn open_root_fs(path: &Path) -> Result<OwnedFd> {
    let rootfs = open_tree(
        CWD,
        path,
        OpenTreeFlags::OPEN_TREE_CLONE | OpenTreeFlags::OPEN_TREE_CLOEXEC,
    )?;

    set_mount_readonly(&rootfs)?;

    Ok(rootfs)
}

/// Shown when a strict `composefs=` boot finds a root filesystem without
/// fs-verity support, e.g. ext4 made without `-O verity` by another tool.
const STRICT_BOOT_WITHOUT_FSVERITY_HELP: &str = "This boot entry requires fs-verity \
(its composefs= argument has no '?'), but the root filesystem does not support it (for \
example ext4 created without '-O verity'). To boot, select the previous deployment in \
the boot menu, or add '?' right after 'composefs=' (or 'composefs.digest=') in this \
entry's kernel command line, which boots without fs-verity enforcement. To enforce \
fs-verity, reinstall onto a filesystem with fs-verity enabled";

/// Whether `error` comes from a filesystem that cannot do fs-verity at all.
fn is_fsverity_unsupported(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<CompareVerityError>(),
        Some(CompareVerityError::Measure(
            MeasureVerityError::FilesystemNotSupported
        ))
    )
}

/// Prepares a floating mount for composefs and returns the fd
///
/// # Arguments
/// * sysroot                - fd for /sysroot
/// * name                   - Expected fs-verity digest of the EROFS image
/// * allow_missing_fsverity - Whether to allow mount without fsverity support
#[context("Mounting composefs image")]
pub fn mount_composefs_image(
    sysroot: &OwnedFd,
    name: &str,
    allow_missing_fsverity: bool,
) -> Result<OwnedFd> {
    let digest = Sha512HashValue::from_hex(name).context("Parsing composefs digest")?;
    // Use open_upgrade to handle upgrades from older composefs-rs versions
    // that lack meta.json: it infers the algorithm and verity mode from
    // existing objects, writes meta.json, and opens normally.
    let (mut repo, _upgraded) = Repository::<Sha512HashValue>::open_upgrade(sysroot, "composefs")?;
    // The cmdline decides the mode, not the repository's mutable metadata:
    // meta.json lives on the same unverified filesystem as the images, and
    // older installs can legitimately have written it without fs-verity.
    // The explicit policy is final for this handle.
    let verity = if allow_missing_fsverity {
        // Missing fs-verity is tolerated, but an image whose digest can be
        // measured still has to match.
        VerityState::Disabled
    } else {
        // fs-verity is required on the image and on every object it serves.
        VerityState::Enabled
    };
    repo.set_integrity_policy(IntegrityPolicy::Explicit(verity))?;
    // mount_digest only ever looks at images/<digest>, never at named refs.
    let rootfs = repo
        .mount_digest(&digest, &MountOptions::default())
        .map_err(|error| {
            if !allow_missing_fsverity && is_fsverity_unsupported(&error) {
                error.context(STRICT_BOOT_WITHOUT_FSVERITY_HELP)
            } else {
                error
            }
        })?;

    set_mount_readonly(&rootfs)?;

    Ok(rootfs)
}

fn parse_composefs_candidates(cmdline: &str) -> Result<Vec<ComposefsCmdline<Sha512HashValue>>> {
    let mut candidates = Vec::new();
    let mut seen_legacy = false;

    // Parse the cmdline with the kernel parser so quoted parameters remain a
    // single parameter. setup_root checks that all candidates agree on policy.
    for parameter in Cmdline::from(cmdline).iter() {
        let key = parameter.key();
        if &*key != KARG_COMPOSEFS_DIGEST && &*key != KARG_V2 {
            continue;
        }

        let value = parameter
            .value()
            .ok_or_else(|| anyhow::anyhow!("{key}= composefs kernel argument has no value"))?;
        if &*key == KARG_V2 {
            anyhow::ensure!(
                !seen_legacy,
                "duplicate {KARG_V2}= composefs kernel argument"
            );
            seen_legacy = true;
        }

        let parameter = format!("{key}={value}");
        if let Some(candidate) = ComposefsCmdline::<Sha512HashValue>::from_cmdline(&parameter)? {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

fn select_composefs_candidate<T>(
    candidates: &[ComposefsCmdline<Sha512HashValue>],
    mut mount: impl FnMut(&ComposefsCmdline<Sha512HashValue>) -> Result<Option<T>>,
) -> Result<(T, ComposefsCmdline<Sha512HashValue>)> {
    for candidate in candidates {
        if let Some(mounted) = mount(candidate)? {
            return Ok((mounted, candidate.clone()));
        }
    }
    let tried = candidates
        .iter()
        .map(|candidate| candidate.digest().to_hex())
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!("no composefs image found (tried: {tried})")
}

fn mount_composefs_candidate(
    sysroot: &OwnedFd,
    candidate: &ComposefsCmdline<Sha512HashValue>,
    allow_missing_fsverity: bool,
) -> Result<Option<OwnedFd>> {
    // A candidate must not relax the fs-verity policy selected for this boot.
    if candidate.is_insecure() && !allow_missing_fsverity {
        anyhow::bail!(
            "composefs candidate requests insecure fs-verity, but the boot policy is strict"
        );
    }

    match mount_composefs_image(
        sysroot,
        &candidate.digest().to_hex(),
        allow_missing_fsverity,
    ) {
        Ok(rootfs) => Ok(Some(rootfs)),
        Err(error) if error.downcast_ref::<ImageNotFound>().is_some() => Ok(None),
        Err(error) => Err(error),
    }
}

/// Mounts a subdirectory with the specified configuration
#[context("Mounting subdirectory")]
pub fn mount_subdir(
    new_root: impl AsFd,
    state: impl AsFd,
    subdir: &str,
    config: MountConfig,
    default: MountType,
) -> Result<()> {
    let mount_type = match config.mount {
        Some(mt) => mt,
        None => match config.transient {
            true => MountType::Transient,
            false => default,
        },
    };

    match mount_type {
        MountType::None => Ok(()),
        MountType::Bind => Ok(mount_at_wrapper(
            bind_mount(&state, subdir)?,
            &new_root,
            subdir,
        )?),
        MountType::Overlay => overlay_state(
            open_dir(&new_root, subdir)?,
            open_dir(&state, subdir)?,
            "overlay",
            None,
        ),
        MountType::Transient => {
            // For subdirectory transient mounts, create the overlay and immediately
            // attach it at the subdirectory path in new_root.
            let subdir_fd = open_dir(&new_root, subdir)?;
            let overlay_fd = overlay_transient(subdir_fd.as_fd(), "transient", None)?;
            mount_at_wrapper(overlay_fd, &new_root, subdir)
        }
    }
}

#[context("GPT workaround")]
/// Workaround for /dev/gpt-auto-root
pub fn gpt_workaround() -> Result<()> {
    // https://github.com/systemd/systemd/issues/35017
    let rootdev = stat("/dev/gpt-auto-root");

    let rootdev = match rootdev {
        Ok(r) => r,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => Err(e)?,
    };

    let target = format!(
        "/dev/block/{}:{}",
        major(rootdev.st_rdev),
        minor(rootdev.st_rdev)
    );
    // Handle the case where the symlink was already created before bootc
    // runs, e.g. by ostree-prepare-root
    // (https://github.com/ostreedev/ostree/pull/3608).
    match symlink(target, "/run/systemd/volatile-root") {
        Ok(()) | Err(Errno::EXIST) => Ok(()),
        Err(err) => Err(err).context("Creating /run/systemd/volatile-root"),
    }
}

/// Sets up /sysroot for switch-root
#[context("Setting up /sysroot")]
pub fn setup_root(args: Args) -> Result<()> {
    anyhow::ensure!(
        args.config.is_none() || args.root_fs.is_some(),
        "--config requires --root-fs (test-only overrides)"
    );

    let sysroot = open_dir(CWD, &args.sysroot)
        .with_context(|| format!("Failed to open sysroot {:?}", args.sysroot))?;

    let cmdline = args
        .cmdline
        .unwrap_or(Cmdline::from_proc().context("Failed to read cmdline")?);

    let candidates =
        parse_composefs_candidates(&cmdline).context("Failed to parse composefs cmdline")?;
    if candidates.is_empty() {
        anyhow::bail!("No composefs image in cmdline");
    }
    let allow_missing_fsverity = candidates[0].is_insecure();
    if candidates
        .iter()
        .any(|candidate| candidate.is_insecure() != allow_missing_fsverity)
    {
        anyhow::bail!("composefs candidates have mixed fs-verity policy");
    }

    let (new_root, composefs_info) = match &args.root_fs {
        Some(path) => (
            open_root_fs(path).context("Failed to clone specified root fs")?,
            candidates[0].clone(),
        ),
        None => select_composefs_candidate(&candidates, |candidate| {
            mount_composefs_candidate(&sysroot, candidate, allow_missing_fsverity)
        })?,
    };
    let config = match args.config {
        Some(path) => {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("Reading test config {}", path.display()))?;
            toml::from_str(&text)
                .with_context(|| format!("Parsing test config {}", path.display()))?
        }
        None => read_config(&new_root)?,
    };

    // Auto-detect systemd.volatile=state: if the kernel cmdline requests a
    // volatile /var via the systemd fstab-generator, skip our initramfs
    // bind-mount of /var from the deployment state directory.  This leaves
    // /var as an empty directory from the composefs image so that
    // systemd-fstab-generator can mount a fresh tmpfs there at local-fs.target.
    // An explicit `[var] mount = "none"` in setup-root-conf.toml has the same
    // effect; the cmdline check is a convenience so users only need the kargs.d
    // entry without also editing setup-root-conf.toml.
    let config = {
        let mut config = config;
        // value_of returns None for a missing key, Some("") for a bare flag,
        // or Some("state") / Some("overlay") / Some("yes") for key=value form.
        let volatile_val = cmdline.value_of("systemd.volatile");
        let var_volatile = matches!(volatile_val, Some("state") | Some("overlay"));
        if var_volatile && config.var.mount.is_none() && !config.var.transient {
            tracing::debug!(
                "systemd.volatile={} detected; skipping /var state bind-mount",
                volatile_val.unwrap_or("")
            );
            config.var.mount = Some(MountType::None);
        }
        config
    };

    // we need to clone this before the next step to make sure we get the old one
    let sysroot_clone = bind_mount(&sysroot, "")?;

    set_mount_readonly(&sysroot_clone)?;

    let mount_target = args.target.unwrap_or(args.sysroot.clone());

    // Ideally we build the new root filesystem together before we mount it, but that only works on
    // 6.15 and later.  Before 6.15 we can't mount into a floating tree, so mount it first.  This
    // will leave an abandoned clone of the sysroot mounted under it, but that's OK for now.
    if cfg!(feature = "pre-6.15") {
        mount_at_wrapper(&new_root, CWD, &mount_target)?;
    }

    // When transient root is enabled, place an overlay on top of the composefs.
    // On pre-6.15, since the composefs is already attached at `mount_target`,
    // the overlay is also immediately attached there.  We then open the overlay
    // via its path so that subsequent mounts target the visible merged tree.
    //
    // On 6.15+, the whole tree is assembled in floating mode; `overlay_transient`
    // returns a detached overlay fd that we can directly mount into.
    //
    // `new_root` always refers to the composefs fd; mounting via it after the
    // overlay is in place would land in the hidden lower layer.
    let transient_overlay_fd: Option<OwnedFd> = if config.root.transient {
        let overlay_fd = overlay_transient(
            &new_root,
            &format!("transient:composefs={}", composefs_info.digest().to_hex()),
            None,
        )?;

        if cfg!(feature = "pre-6.15") {
            // In pre-6.15, the composefs is already attached at `mount_target`.
            // Attach the overlay on top of it, then reopen the path to get a
            // dirfd that resolves through the overlay (not the hidden composefs).
            mount_at_wrapper(&overlay_fd, CWD, &mount_target)
                .context("Moving transient overlay onto sysroot")?;
            Some(open_dir(CWD, &mount_target).context("Opening attached overlay root")?)
        } else {
            // On 6.15+ we assemble in floating mode; use the detached overlay fd
            // directly for subsequent mounts into the tree.
            Some(overlay_fd)
        }
    } else {
        None
    };

    // When transient root is active the overlay sits on top of the composefs.
    // Mounts placed via `new_root` would land in the composefs lower layer and
    // be invisible from the running system.  Use the overlay fd for all
    // post-overlay mounts (sysroot, etc, var) so they appear in the merged view.
    let visible_root: &dyn AsFd = transient_overlay_fd
        .as_ref()
        .map_or(&new_root as &dyn AsFd, |fd| fd as &dyn AsFd);

    // Mount the physical sysroot (with the composefs repo) into the new root
    // so that `bootc status` and other tools can find it after switch-root.
    match composefs::mount::mount_at(&sysroot_clone, visible_root, "sysroot") {
        Ok(()) | Err(Errno::NOENT) => {}
        Err(err) => Err(err)?,
    }

    // etc + var
    let state = open_dir(
        open_dir(&sysroot, "state/deploy")?,
        composefs_info.digest().to_hex(),
    )?;
    mount_subdir(visible_root, &state, "etc", config.etc, MountType::Bind)?;
    // /var is bind-mounted from the deployment state directory by default.
    // The systemd.volatile=state cmdline detection above (or an explicit
    // [var] mount = "none" in setup-root-conf.toml) can change this to
    // MountType::None, which skips the bind-mount entirely and leaves /var
    // as an empty directory from the composefs image for systemd to fill.
    mount_subdir(visible_root, &state, "var", config.var, MountType::Bind)?;

    if cfg!(not(feature = "pre-6.15")) {
        // Replace the /sysroot with the new composed root filesystem.
        // When a transient overlay is active, mount it rather than the bare
        // composefs so the running system sees the writable merged view.
        unmount(&args.sysroot, UnmountFlags::DETACH)?;
        mount_at_wrapper(visible_root, CWD, &mount_target)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, fs::File};

    use composefs::{
        fsverity::{Algorithm, CompareVerityError, MeasureVerityError, compute_verity},
        repository::RepositoryConfig,
    };

    use super::*;
    use cap_std_ext::{cap_std::ambient_authority, cap_tempfile};

    #[test]
    fn test_read_config_from_root() -> Result<()> {
        let root = cap_tempfile::tempdir(ambient_authority())?;
        assert_eq!(read_config(&*root)?, Config::default());
        root.create_dir_all("usr/lib/composefs")?;
        let path = SETUP_ROOT_CONF_PATH.trim_start_matches('/');
        let text = "[etc]\nmount = 'none'\n[var]\nmount = 'none'\n[root]\ntransient = true";
        root.write(path, text)?;
        assert_eq!(read_config(&*root)?, parse(text));
        root.write(path, "[etc]\nmount = 'invalid'")?;
        assert!(
            read_config(&*root)
                .unwrap_err()
                .to_string()
                .contains("Parsing")
        );
        root.remove_file(path)?;
        root.create_dir(path)?;
        assert!(read_config(&*root).is_err());
        Ok(())
    }

    #[test]
    fn test_config_override_requires_root_fs() {
        assert!(Args::try_parse_from(["setup-root", "--config=/test.toml"]).is_err());
        let args = Args::try_parse_from(["setup-root", "--root-fs=/image", "--config=/test.toml"])
            .unwrap();
        assert_eq!(args.config.as_deref(), Some(Path::new("/test.toml")));
        assert!(Args::parse_from(["setup-root"]).config.is_none());
    }

    #[test]
    fn test_config_symlinks_stay_in_image() -> Result<()> {
        let temp = cap_tempfile::tempdir(ambient_authority())?;
        temp.create_dir_all("image/usr/lib/composefs")?;
        let root = temp.open_dir("image")?;
        let path = SETUP_ROOT_CONF_PATH.trim_start_matches('/');
        let policy = "[etc]\nmount = 'none'";
        root.write("usr/lib/composefs/policy.toml", policy)?;
        rustix::fs::symlinkat("/usr/lib/composefs/policy.toml", &root, path)?;
        assert_eq!(read_config(&root)?, parse(policy));
        root.remove_file(path)?;
        // An ordinary openat would follow this out of image into temp.
        temp.write("outside.toml", "[root]\ntransient = true")?;
        rustix::fs::symlinkat("../../../../outside.toml", &root, path)?;
        assert_eq!(read_config(&root)?, Config::default());
        Ok(())
    }

    // Run with: unshare -Ur -m cargo test -p bootc-initramfs-setup -- --ignored
    // Mounts must be isolated from the host and the other unit tests.
    fn check_setup_root_image_policy(offline: bool) -> Result<()> {
        let temp = cap_tempfile::tempdir(ambient_authority())?;
        temp.create_dir_all("image/usr/lib/composefs")?;
        for path in ["image/etc", "image/var", "physical", "target"] {
            temp.create_dir(path)?;
        }
        temp.write("image/etc/origin", "image")?;
        temp.write("image/var/origin", "image")?;
        temp.write(
            "image/usr/lib/composefs/setup-root-conf.toml",
            "[etc]\nmount = 'none'\n[var]\nmount = 'none'",
        )?;
        // Deliberately omit state/etc and state/var: using the defaults would fail.
        temp.create_dir_all(format!(
            "physical/state/deploy/{}",
            Sha512HashValue::EMPTY.to_hex()
        ))?;
        let base = PathBuf::from(format!("/proc/self/fd/{}", temp.as_raw_fd()));
        let sysroot = base.join("physical");
        let target = base.join("target");
        let image = base.join("image");
        let physical = bind_mount(&*temp, "physical")?;
        mount_at_wrapper(&physical, CWD, &sysroot)?;
        let karg = v1(Sha512HashValue::EMPTY, false);
        let args = if offline {
            let mut args = Args::for_deployment(sysroot, Some(target.clone()), karg);
            args.root_fs = Some(image);
            args
        } else {
            Args::try_parse_from([
                OsString::from("setup-root"),
                "--sysroot".into(),
                sysroot.into_os_string(),
                "--target".into(),
                target.clone().into_os_string(),
                "--root-fs".into(),
                image.into_os_string(),
                "--cmdline".into(),
                karg.into(),
            ])?
        };
        let result = setup_root(args);
        if result.is_ok() {
            assert_eq!(std::fs::read_to_string(target.join("etc/origin"))?, "image");
            assert_eq!(std::fs::read_to_string(target.join("var/origin"))?, "image");
        }
        // Release the attached image before TempDir attempts recursive cleanup.
        unmount(&target, UnmountFlags::DETACH)?;
        if cfg!(feature = "pre-6.15") {
            unmount(base.join("physical"), UnmountFlags::DETACH)?;
        }
        result
    }

    #[test]
    #[ignore = "requires a private mount namespace with CAP_SYS_ADMIN"]
    fn test_setup_root_image_policy() -> Result<()> {
        check_setup_root_image_policy(false)
    }

    #[test]
    #[ignore = "requires a private mount namespace with CAP_SYS_ADMIN"]
    fn test_offline_setup_root_image_policy() -> Result<()> {
        check_setup_root_image_policy(true)
    }

    #[test]
    fn test_strict_mount_rejects_unverified_images() -> Result<()> {
        // A strict cmdline must not be overridden by the repository's insecure
        // mode, and an image under images/refs/ must not stand in for the
        // digest whatever the cmdline says. All of these are rejected before
        // anything is mounted, so this runs unprivileged and on tmpfs.
        let tmp = tempfile::tempdir()?;
        let sysroot: OwnedFd = File::open(tmp.path())?.into();
        let (repo, _) = Repository::<Sha512HashValue>::init_path(
            &sysroot,
            "composefs",
            RepositoryConfig::new(Algorithm::SHA512).set_insecure(),
        )?;
        drop(repo);
        let images = tmp.path().join("composefs/images");
        fs::create_dir_all(images.join("refs"))?;
        let data = b"unverified image";
        let digest = compute_verity::<Sha512HashValue>(data).to_hex();

        fs::write(images.join("refs").join(&digest), data)?;
        for allow_missing_fsverity in [false, true] {
            let error =
                mount_composefs_image(&sysroot, &digest, allow_missing_fsverity).unwrap_err();
            assert!(error.downcast_ref::<ImageNotFound>().is_some(), "{error:#}");
        }

        fs::write(images.join(&digest), data)?;
        let error = mount_composefs_image(&sysroot, &digest, false).unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<CompareVerityError>(),
                Some(CompareVerityError::Measure(
                    MeasureVerityError::VerityMissing | MeasureVerityError::FilesystemNotSupported
                ))
            ),
            "{error:#}"
        );
        // Only a filesystem without fs-verity support gets the recovery hint.
        assert_eq!(
            format!("{error:#}").contains(STRICT_BOOT_WITHOUT_FSVERITY_HELP),
            is_fsverity_unsupported(&error),
            "{error:#}"
        );

        let missing = Sha512HashValue::EMPTY.to_hex();
        let error = mount_composefs_image(&sysroot, &missing, false).unwrap_err();
        assert!(error.downcast_ref::<ImageNotFound>().is_some(), "{error:#}");
        Ok(())
    }

    fn parse(toml: &str) -> Config {
        toml::from_str(toml).expect("TOML parse failed")
    }

    #[test]
    fn test_config_defaults() {
        let config = parse("");
        assert_eq!(
            config,
            Config {
                etc: MountConfig {
                    mount: None,
                    transient: false
                },
                var: MountConfig {
                    mount: None,
                    transient: false
                },
                root: RootConfig { transient: false },
            }
        );
    }

    #[test]
    fn test_mounttype_none() {
        let config = parse("[etc]\nmount = \"none\"");
        assert_eq!(config.etc.mount, Some(MountType::None));
    }

    #[test]
    fn test_mounttype_root_alias() {
        let config = parse("[etc]\nmount = \"root\"");
        assert_eq!(config.etc.mount, Some(MountType::None));
    }

    #[test]
    fn test_etc_transient_flag() {
        let config = parse("[etc]\ntransient = true");
        assert_eq!(config.etc.transient, true);
        assert_eq!(config.etc.mount, None);
    }

    #[test]
    fn test_var_none() {
        // mount = "none" skips the state bind-mount; combine with
        // systemd.volatile=state karg to get a fresh tmpfs on every boot.
        let config = parse("[var]\nmount = \"none\"");
        assert_eq!(config.var.mount, Some(MountType::None));
    }

    #[test]
    fn test_root_transient() {
        let config = parse("[root]\ntransient = true");
        assert_eq!(config.root.transient, true);
    }

    #[test]
    fn test_combined_config() {
        let config = parse("[root]\ntransient = true\n[etc]\nmount = \"root\"");
        assert_eq!(config.root.transient, true);
        assert_eq!(config.etc.mount, Some(MountType::None));
    }

    fn v1(digest: Sha512HashValue, insecure: bool) -> String {
        ComposefsCmdline::new_v1(digest, insecure).to_cmdline_arg()
    }

    fn v2(digest: Sha512HashValue, insecure: bool) -> String {
        ComposefsCmdline::new_v2(digest, insecure).to_cmdline_arg()
    }

    #[test]
    fn test_composefs_candidate_priority_and_selected_state() {
        let v1_digest = Sha512HashValue::EMPTY;
        let v2_digest = Sha512HashValue::from_hex("aa".repeat(64)).unwrap();
        let cmdline = format!(
            "quiet {} {}",
            v1(v1_digest.clone(), false),
            v2(v2_digest.clone(), false)
        );
        let candidates = parse_composefs_candidates(&cmdline).unwrap();
        let (selected, selected_cmdline) = select_composefs_candidate(&candidates, |candidate| {
            Ok((candidate.digest() == &v2_digest).then_some(candidate.digest().to_hex()))
        })
        .unwrap();
        assert_eq!(selected, v2_digest.to_hex());
        assert_eq!(selected_cmdline.digest(), &v2_digest);

        let (selected, _) = select_composefs_candidate(&candidates, |candidate| {
            Ok(Some(candidate.digest().to_hex()))
        })
        .unwrap();
        assert_eq!(selected, v1_digest.to_hex());
    }

    #[test]
    fn test_composefs_candidate_parse_and_policy_errors() {
        let malformed = ["composefs=not-a-digest", "composefs.digest=v9-sha512-12:aa"];
        for cmdline in malformed {
            assert!(
                parse_composefs_candidates(cmdline).is_err(),
                "expected parse error for {cmdline:?}"
            );
        }

        let strict = parse_composefs_candidates(&v1(Sha512HashValue::EMPTY, false)).unwrap();
        let insecure = parse_composefs_candidates(&v2(Sha512HashValue::EMPTY, true)).unwrap();
        assert_ne!(strict[0].is_insecure(), insecure[0].is_insecure());
    }

    #[test]
    fn test_composefs_candidate_parser_cases() {
        let first = Sha512HashValue::EMPTY;
        let second = Sha512HashValue::from_hex("aa".repeat(64)).unwrap();
        let cases = [
            ("single v1", v1(first.clone(), false), vec![first.clone()]),
            ("single v2", v2(first.clone(), false), vec![first.clone()]),
            (
                "dual preserves cmdline order",
                format!("{} {}", v2(first.clone(), false), v1(second.clone(), false)),
                vec![first.clone(), second.clone()],
            ),
            (
                "quoted parameter",
                format!("quiet \"{}\" rw", v1(first.clone(), false)),
                vec![first.clone()],
            ),
            (
                "skips sha256 descriptor",
                format!(
                    "composefs.digest=v1-sha256-12:{} {} {}",
                    "aa".repeat(32),
                    v1(first.clone(), false),
                    v2(second.clone(), false)
                ),
                vec![first.clone(), second.clone()],
            ),
        ];

        for (name, cmdline, expected_digests) in cases {
            let candidates = parse_composefs_candidates(&cmdline)
                .unwrap_or_else(|error| panic!("{name} unexpectedly failed: {error:#}"));
            assert_eq!(
                candidates
                    .iter()
                    .map(|candidate| candidate.digest().clone())
                    .collect::<Vec<_>>(),
                expected_digests,
                "case {name}"
            );
        }

        let duplicate_cases = [
            format!("{} {}", v2(first.clone(), false), v2(second.clone(), false)),
            format!("{} {}", v2(first.clone(), false), v2(first.clone(), false)),
        ];
        for cmdline in duplicate_cases {
            assert!(
                parse_composefs_candidates(&cmdline).is_err(),
                "duplicate recognized key accepted: {cmdline}"
            );
        }

        let irrelevant = [
            "quiet splash rw",
            "root=UUID=abc composefs.digest=v1-sha256-12:aa",
        ];
        for cmdline in irrelevant {
            assert!(
                parse_composefs_candidates(cmdline).unwrap().is_empty(),
                "irrelevant argument was treated as a candidate: {cmdline}"
            );
        }
    }
}
