use std::borrow::Cow;
use std::collections::HashSet;
#[cfg(feature = "selinux")]
use std::ffi::CString;
use std::io::Write;
use std::ops::ControlFlow;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use bootc_utils::CommandRunExt;
use camino::{Utf8Path, Utf8PathBuf};
use cap_std::fs::Dir;
use cap_std::fs::{DirBuilder, OpenOptions};
use cap_std::io_lifetimes::AsFilelike;
use cap_std_ext::cap_std;
use cap_std_ext::cap_std::fs::{Metadata, MetadataExt};
use cap_std_ext::dirext::CapStdExtDirExt;
use fn_error_context::context;
use ostree_ext::gio;
use ostree_ext::ostree;
use rustix::fd::AsFd;

/// The mount path for selinux
const SELINUXFS: &str = "/sys/fs/selinux";
/// The SELinux xattr
const SELINUX_XATTR: &[u8] = b"security.selinux\0";
/// The kernel initial SID used for objects without a label.
#[cfg(feature = "selinux")]
const SELINUX_INITIAL_SID_UNLABELED: &str = "unlabeled";
const SELF_CURRENT: &str = "/proc/self/attr/current";

/// Whether SELinux is enabled for the current process.
///
/// libselinux caches this state process-wide, so mounting selinuxfs requires a
/// re-exec before this result can change.
#[cfg(feature = "selinux")]
pub(crate) fn selinux_enabled() -> bool {
    selinux::kernel_support() != selinux::KernelSupport::Unsupported
}

/// Without the `selinux` feature, bootc treats SELinux as disabled.
#[cfg(not(feature = "selinux"))]
pub(crate) fn selinux_enabled() -> bool {
    false
}

/// Whether SELinux is enabled on the host, as observed from PID 1's mount namespace.
///
/// This bypasses libselinux's process-cached state and is only appropriate while
/// bootstrapping the selinuxfs mount before re-exec.
#[cfg(feature = "selinux")]
#[context("Querying host SELinux availability")]
pub(crate) fn host_selinux_enabled() -> Result<bool> {
    Path::new("/proc/1/root/sys/fs/selinux/enforce")
        .try_exists()
        .map_err(Into::into)
}

/// Without the `selinux` feature, bootc treats SELinux as disabled.
#[cfg(not(feature = "selinux"))]
pub(crate) fn host_selinux_enabled() -> Result<bool> {
    Ok(false)
}

/// Get the current process SELinux security context
fn get_current_security_context() -> Result<String> {
    std::fs::read_to_string(SELF_CURRENT).with_context(|| format!("Reading {SELF_CURRENT}"))
}

/// Check if the current process has the capability to write SELinux security
/// contexts unknown to the current policy. In SELinux terms this capability is
/// gated under `mac_admin` (admin control over SELinux state), and in the Fedora
/// policy at least it's part of `install_t`.
#[context("Testing install_t")]
fn test_install_t() -> Result<bool> {
    let tmpf = tempfile::NamedTempFile::new()?;
    // Our implementation here writes a label which is always unknown to the current policy
    // to verify that we have the capability to do so.
    let st = Command::new("chcon")
        .args(["-t", "invalid_bootcinstall_testlabel_t"])
        .arg(tmpf.path())
        .stderr(std::process::Stdio::null())
        .status()?;
    Ok(st.success())
}

/// Ensure that the current process has the capability to write SELinux security
/// contexts unknown to the current policy.
///
/// See [`test_install_t`] above for how we check for that capability.
///
/// In the general case of both upgrade or install, we may e.g. jump major versions
/// or even operating systems, and we need the ability to write arbitrary labels.
/// If the current process doesn't already have `mac_admin/install_t` then we
/// make a new temporary copy of our binary, and give it the same label as /usr/bin/ostree,
/// which in Fedora derivatives at least was already historically labeled with
/// the correct install_t label.
///
/// However, if you maintain a bootc operating system with SELinux, you should from
/// the start ensure that /usr/bin/bootc has the correct capabilities.
#[context("Ensuring selinux install_t type")]
pub(crate) fn selinux_ensure_install(reexec_env: &[(&str, &str)]) -> Result<bool> {
    let guardenv = "_bootc_selinuxfs_mounted";
    let current = get_current_security_context()?;
    tracing::debug!("Current security context is {current}");
    if let Some(p) = std::env::var_os(guardenv) {
        let p = Path::new(&p);
        if p.exists() {
            tracing::debug!("Removing temporary file");
            std::fs::remove_file(p).context("Removing {p:?}")?;
        } else {
            tracing::debug!("Assuming we now have a privileged (e.g. install_t) label");
        }
        return test_install_t();
    }
    if test_install_t()? {
        tracing::debug!("We have install_t");
        return Ok(true);
    }
    tracing::debug!("Lacking install_t capabilities; copying self to temporary file for re-exec");
    // OK now, we always copy our binary to a tempfile, set its security context
    // to match that of /usr/bin/ostree, and then re-exec.  This is really a gross
    // hack; we can't always rely on https://github.com/fedora-selinux/selinux-policy/pull/1500/commits/67eb283c46d35a722636d749e5b339615fe5e7f5
    let mut tmpf = tempfile::NamedTempFile::new()?;
    let srcpath = std::env::current_exe()?;
    let mut src = std::fs::File::open(&srcpath)?;
    let meta = src.metadata()?;
    std::io::copy(&mut src, &mut tmpf).context("Copying self to tempfile for selinux re-exec")?;
    tmpf.as_file_mut()
        .set_permissions(meta.permissions())
        .context("Setting permissions of tempfile")?;
    let container_root = Dir::open_ambient_dir("/", cap_std::ambient_authority())?;
    let policy = ostree::SePolicy::new_at(container_root.as_raw_fd(), gio::Cancellable::NONE)?;
    let label = require_label(&policy, "/usr/bin/ostree".into(), libc::S_IFREG | 0o755)?;
    set_security_selinux(tmpf.as_fd(), label.as_bytes())?;
    let tmpf: Utf8PathBuf = tmpf.keep()?.1.try_into().unwrap();
    tracing::debug!("Created {tmpf:?}");

    let mut cmd = Command::new(&tmpf);
    cmd.env(guardenv, tmpf);
    cmd.env(bootc_utils::reexec::ORIG, srcpath);
    bootc_utils::reexec::prepare_reexec(&mut cmd, reexec_env);
    cmd.log_debug();
    Err(anyhow::Error::msg(cmd.exec()).context("execve"))
}

/// Query whether SELinux is apparently enabled in the target root
pub(crate) fn have_selinux_policy(root: &Dir) -> Result<bool> {
    // TODO use ostree::SePolicy and query policy name
    root.try_exists("etc/selinux/config").map_err(Into::into)
}

/// A type which will reset SELinux back to enforcing mode when dropped.
/// This is a workaround for the deep difficulties in trying to reliably
/// gain the `mac_admin` permission (install_t).
#[must_use]
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct SetEnforceGuard(Option<()>);

impl SetEnforceGuard {
    pub(crate) fn new() -> Self {
        SetEnforceGuard(Some(()))
    }

    #[allow(dead_code)]
    pub(crate) fn consume(mut self) -> Result<()> {
        // SAFETY: The option cannot have been consumed until now
        self.0.take().unwrap();
        // This returns errors
        selinux_set_permissive(false)
    }
}

impl Drop for SetEnforceGuard {
    fn drop(&mut self) {
        // A best-effort attempt to re-enable enforcement on drop (installation failure)
        if let Some(()) = self.0.take() {
            let _ = selinux_set_permissive(false);
        }
    }
}

/// Try to enter the install_t domain, but if we can't do that, then
/// just setenforce 0.
#[context("Ensuring selinux install_t type")]
pub(crate) fn selinux_ensure_install_or_setenforce(
    reexec_env: &[(&str, &str)],
) -> Result<Option<SetEnforceGuard>> {
    // If the process already has install_t, exit early
    // Note that this may re-exec the entire process
    if selinux_ensure_install(reexec_env)? {
        return Ok(None);
    }
    let g = if std::env::var_os("BOOTC_SETENFORCE0_FALLBACK").is_some() {
        tracing::warn!("Failed to enter install_t; temporarily setting permissive mode");
        selinux_set_permissive(true)?;
        Some(SetEnforceGuard::new())
    } else {
        let current = get_current_security_context()?;
        anyhow::bail!(
            "Failed to enter install_t (running as {current}) - use BOOTC_SETENFORCE0_FALLBACK=1 to override"
        );
    };
    Ok(g)
}

/// A thin wrapper for loading a SELinux policy that maps "policy nonexistent" to None.
pub(crate) fn new_sepolicy_at(fd: impl AsFd) -> Result<Option<ostree::SePolicy>> {
    let fd = fd.as_fd();
    let cancellable = gio::Cancellable::NONE;
    let sepolicy = ostree::SePolicy::new_at(fd.as_raw_fd(), cancellable)?;
    let r = if sepolicy.csum().is_none() {
        None
    } else {
        Some(sepolicy)
    };
    Ok(r)
}

#[context("Setting SELinux permissive mode")]
#[allow(dead_code)]
pub(crate) fn selinux_set_permissive(permissive: bool) -> Result<()> {
    let enforce_path = &Utf8Path::new(SELINUXFS).join("enforce");
    if !enforce_path.exists() {
        return Ok(());
    }
    let mut f = std::fs::File::options().write(true).open(enforce_path)?;
    f.write_all(if permissive { b"0" } else { b"1" })?;
    tracing::debug!(
        "Set SELinux mode: {}",
        if permissive {
            "permissive"
        } else {
            "enforcing"
        }
    );
    Ok(())
}

/// Check if the ostree-formatted extended attributes include a security.selinux value.
pub(crate) fn xattrs_have_selinux(xattrs: &ostree::glib::Variant) -> bool {
    let n = xattrs.n_children();
    for i in 0..n {
        let child = xattrs.child_value(i);
        let key = child.child_value(0);
        let key = key.data_as_bytes();
        if key == SELINUX_XATTR {
            return true;
        }
    }
    false
}

/// Look up the label for a path in a policy, and error if one is not found.
pub(crate) fn require_label(
    policy: &ostree::SePolicy,
    destname: &Utf8Path,
    mode: u32,
) -> Result<ostree::glib::GString> {
    policy
        .label(destname.as_str(), mode, ostree::gio::Cancellable::NONE)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No label found in policy '{:?}' for {destname})",
                policy.csum()
            )
        })
}

/// Look up the label for a path in a policy, returning None if no match is found.
pub(crate) fn optional_label(
    policy: &ostree::SePolicy,
    destname: &Utf8Path,
    mode: u32,
) -> Result<Option<ostree::glib::GString>> {
    policy
        .label(destname.as_str(), mode, ostree::gio::Cancellable::NONE)
        .map_err(Into::into)
}

/// A thin wrapper for invoking fsetxattr(security.selinux)
pub(crate) fn set_security_selinux(fd: std::os::fd::BorrowedFd, label: &[u8]) -> Result<()> {
    rustix::fs::fsetxattr(
        fd,
        "security.selinux",
        label,
        rustix::fs::XattrFlags::empty(),
    )
    .context("fsetxattr(security.selinux)")
}

/// The labeling state; "unsupported" is distinct as we need to handle
/// cases like the ESP which don't support labeling.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SELinuxLabelState {
    Unlabeled,
    Unsupported,
    Labeled,
}

/// The type of the kernel's `unlabeled` initial context. Files with it (e.g.
/// created while SELinux was disabled) need relabeling just like files without
/// any label.
#[cfg(feature = "selinux")]
#[derive(Debug)]
struct UnlabeledType(CString);

/// Without the `selinux` feature, bootc never parses SELinux contexts, so this
/// can't be constructed and only files without any label count as unlabeled.
#[cfg(not(feature = "selinux"))]
#[derive(Debug)]
enum UnlabeledType {}

impl UnlabeledType {
    /// Look up the unlabeled type if SELinux is enabled for this process.
    #[cfg(feature = "selinux")]
    fn query() -> Result<Option<Self>> {
        if !selinux_enabled() {
            return Ok(None);
        }
        let context = selinux::SecurityContext::of_initial_kernel_context(
            SELINUX_INITIAL_SID_UNLABELED,
            true,
        )
        .context("Querying the SELinux unlabeled initial context")?;
        context_type(context.as_bytes()).map(|t| Some(Self(t)))
    }

    #[cfg(not(feature = "selinux"))]
    fn query() -> Result<Option<Self>> {
        Ok(None)
    }

    /// Whether a `security.selinux` value has this type.
    #[cfg(feature = "selinux")]
    fn matches(&self, context: &[u8]) -> Result<bool> {
        Ok(context_type(context)?.as_c_str() == self.0.as_c_str())
    }

    #[cfg(not(feature = "selinux"))]
    fn matches(&self, _context: &[u8]) -> Result<bool> {
        match *self {}
    }
}

#[cfg(feature = "selinux")]
fn context_type(context: &[u8]) -> Result<CString> {
    // security.selinux xattrs may include a trailing NUL terminator.
    let context = context.strip_suffix(b"\0").unwrap_or(context);
    let context = CString::new(context).context("SELinux context contains an interior NUL")?;
    context
        .to_str()
        .context("SELinux context is not valid UTF-8")?;
    selinux::OpaqueSecurityContext::from_c_str(&context)
        .context("Parsing SELinux context")?
        .the_type()
        .context("Reading SELinux context type")
}

/// Query the SELinux labeling for a particular path
#[cfg(feature = "selinux")]
pub(crate) fn has_security_selinux(root: &Dir, path: &Utf8Path) -> Result<SELinuxLabelState> {
    let unlabeled_type = UnlabeledType::query()?.context("SELinux is not enabled")?;
    has_security_selinux_inner(root, path, Some(&unlabeled_type))
}

fn has_security_selinux_inner(
    root: &Dir,
    path: &Utf8Path,
    unlabeled_type: Option<&UnlabeledType>,
) -> Result<SELinuxLabelState> {
    // TODO: avoid hardcoding a max size here
    let mut buf = [0u8; 2048];
    let fdpath = format!("/proc/self/fd/{}/{path}", root.as_raw_fd());
    match rustix::fs::lgetxattr(fdpath, "security.selinux", &mut buf) {
        Ok(len) => {
            // Installation can relabel a target while its host has SELinux disabled.
            // In that case, retain the conservative xattr-presence behavior.
            let Some(unlabeled_type) = unlabeled_type else {
                return Ok(SELinuxLabelState::Labeled);
            };
            let unlabeled = unlabeled_type
                .matches(&buf[..len])
                .with_context(|| format!("Parsing SELinux context for {path:?}"))?;
            if unlabeled {
                Ok(SELinuxLabelState::Unlabeled)
            } else {
                Ok(SELinuxLabelState::Labeled)
            }
        }
        Err(rustix::io::Errno::OPNOTSUPP) => Ok(SELinuxLabelState::Unsupported),
        Err(rustix::io::Errno::NODATA) => Ok(SELinuxLabelState::Unlabeled),
        Err(e) => Err(e).with_context(|| format!("Failed to look up context for {path:?}")),
    }
}

/// Directly set the `security.selinux` extended attribute on the target
/// path. Symbolic links are not followed for the target.
///
/// Note that this API will work even if SELinux is disabled.
pub(crate) fn set_security_selinux_path(root: &Dir, path: &Utf8Path, label: &[u8]) -> Result<()> {
    let fdpath = format!("/proc/self/fd/{}/", root.as_raw_fd());
    let fdpath = &Path::new(&fdpath).join(path);
    rustix::fs::lsetxattr(
        fdpath,
        "security.selinux",
        label,
        rustix::fs::XattrFlags::empty(),
    )?;
    Ok(())
}

fn ensure_labeled_inner(
    root: &Dir,
    path: &Utf8Path,
    metadata: &Metadata,
    policy: &ostree::SePolicy,
    unlabeled_type: Option<&UnlabeledType>,
) -> Result<SELinuxLabelState> {
    let r = has_security_selinux_inner(root, path, unlabeled_type)?;
    if matches!(r, SELinuxLabelState::Unlabeled) {
        relabel(root, metadata, path, None, policy)?;
    }
    Ok(r)
}

/// Given the policy, relabel the target file or directory.
/// Optionally, an override for the path can be provided
/// to set the label as if the target has that filename.
pub(crate) fn relabel(
    root: &Dir,
    metadata: &Metadata,
    path: &Utf8Path,
    as_path: Option<&Utf8Path>,
    policy: &ostree::SePolicy,
) -> Result<()> {
    assert!(!path.starts_with("/"));
    let as_path = as_path
        .map(Cow::Borrowed)
        .unwrap_or_else(|| Utf8Path::new("/").join(path).into());
    let label = require_label(policy, &as_path, metadata.mode())?;
    tracing::trace!("Setting label for {path} to {label}");
    set_security_selinux_path(root, &path, label.as_bytes())
}

pub(crate) fn relabel_recurse_inner(
    root: &Dir,
    path: &mut Utf8PathBuf,
    mut as_path: Option<&mut Utf8PathBuf>,
    policy: &ostree::SePolicy,
) -> Result<()> {
    // Relabel this directory
    let self_meta = root.dir_metadata()?;
    relabel(
        root,
        &self_meta,
        path,
        as_path.as_ref().map(|p| p.as_path()),
        policy,
    )?;

    // Relabel all children
    for ent in root.read_dir(&path)? {
        let ent = ent?;
        let metadata = ent.metadata()?;
        let name = ent.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid non-UTF-8 filename: {name:?}"))?;
        // Extend both copies of the path
        path.push(name);
        if let Some(p) = as_path.as_mut() {
            p.push(name);
        }

        if metadata.is_dir() {
            let as_path = as_path.as_deref_mut();
            relabel_recurse_inner(root, path, as_path, policy)?;
        } else {
            let as_path = as_path.as_ref().map(|p| p.as_path());
            relabel(root, &metadata, &path, as_path, policy)?
        }
        // Trim what we added to the path
        let r = path.pop();
        assert!(r);
        if let Some(p) = as_path.as_mut() {
            let r = p.pop();
            assert!(r);
        }
    }

    Ok(())
}

/// Recursively relabel the target directory.
pub(crate) fn relabel_recurse(
    root: &Dir,
    path: impl AsRef<Utf8Path>,
    as_path: Option<&Utf8Path>,
    policy: &ostree::SePolicy,
) -> Result<()> {
    let mut path = path.as_ref().to_owned();
    // This path must be relative, as we access via cap-std
    assert!(!path.starts_with("/"));
    let mut as_path = as_path.map(|v| v.to_owned());
    // But the as_path must be absolute, if provided
    if let Some(as_path) = as_path.as_deref() {
        assert!(as_path.starts_with("/"));
    }
    relabel_recurse_inner(root, &mut path, as_path.as_mut(), policy)
}

/// Recursively ensure all files under a directory have SELinux labels.
/// Uses the `walk` API with `noxdev` and `skip_mountpoints` to avoid crossing
/// mount point boundaries
/// (e.g. into sysfs, procfs, etc.).
/// The provided `skip` parameter is a device/inode pair that we will ignore
/// (and not traverse into).
pub(crate) fn ensure_dir_labeled_recurse(
    root: &Dir,
    path: &mut Utf8PathBuf,
    policy: &ostree::SePolicy,
    skip: Option<(libc::dev_t, libc::ino64_t)>,
) -> Result<()> {
    use cap_std_ext::dirext::WalkConfiguration;
    use std::ops::ControlFlow;

    // When SELinux is unavailable to this process, retain the conservative
    // xattr-presence behavior in `has_security_selinux_inner`.
    let unlabeled_type = UnlabeledType::query()?;
    // Juggle the cap-std requirement for relative paths vs the libselinux
    // requirement for absolute paths by special casing the empty string "" as "."
    // just for the initial directory enumeration.
    let path_for_read = if path.as_str().is_empty() {
        Utf8Path::new(".")
    } else {
        &*path
    };

    let mut n = 0u64;

    // Label the starting directory itself; the walk API only visits children.
    let metadata = root.symlink_metadata(path_for_read)?;
    match ensure_labeled_inner(root, path, &metadata, policy, unlabeled_type.as_ref())? {
        SELinuxLabelState::Unlabeled => {
            n += 1;
        }
        SELinuxLabelState::Unsupported => return Ok(()),
        SELinuxLabelState::Labeled => {}
    }
    let config = WalkConfiguration::default()
        .noxdev()
        .skip_mountpoints()
        .path_base(path_for_read.as_std_path());

    root.open_dir(path_for_read)?
        .walk::<_, anyhow::Error>(&config, |component| {
            let metadata = component.entry.metadata()?;

            // Check if this entry should be skipped
            if let Some((skip_dev, skip_ino)) = skip {
                if (metadata.dev(), metadata.ino()) == (skip_dev, skip_ino) {
                    tracing::debug!("Skipping dev={skip_dev} inode={skip_ino}");
                    // For directories, Break skips traversal into the directory
                    // but continues with the next sibling. For non-directories,
                    // Break would skip all remaining siblings, so use Continue
                    // to skip only this entry.
                    if component.file_type.is_dir() {
                        return Ok(ControlFlow::Break(()));
                    } else {
                        return Ok(ControlFlow::Continue(()));
                    }
                }
            }

            let path = Utf8Path::from_path(component.path)
                .ok_or_else(|| anyhow::anyhow!("Invalid non-UTF-8 path: {:?}", component.path))?;

            match ensure_labeled_inner(root, path, &metadata, policy, unlabeled_type.as_ref())? {
                SELinuxLabelState::Unlabeled => {
                    n += 1;
                }
                // We check for Unsupported on the starting directory above,
                // and the walk uses noxdev + skip_mountpoints to stay on
                // the same filesystem, so hitting Unsupported here is
                // unexpected.
                SELinuxLabelState::Unsupported => {
                    anyhow::bail!(
                        "Unexpected SELinuxLabelState::Unsupported during walk at {path}"
                    );
                }
                SELinuxLabelState::Labeled => {}
            }

            Ok(ControlFlow::Continue(()))
        })?;

    if n > 0 {
        tracing::debug!("Relabeled {n} objects in {path}");
    }
    Ok(())
}

/// Depth below the physical root at which the final relabel splits its walk
/// into parallel tasks. When installing alongside existing content
/// (`to-existing-root`), directories at this depth that predate the install
/// are not walked at all: on an ostree host that is e.g.
/// `ostree/deploy/<stateroot>`, with its `/var` and home directories, and on
/// other hosts e.g. `var/lib/containers`. Only a change directly in such a
/// directory makes it count as new; the install writes deeper into existing
/// directories only through ostree, which labels what it writes.
const RELABEL_SPLIT_DEPTH: usize = 3;

/// The maximum number of directories checked concurrently.
const RELABEL_MAX_CONCURRENT: usize = 8;

/// How often to print progress while checking labels.
const RELABEL_PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// A directory's device, inode and change time. The change time tells a
/// directory created at a reused inode, or one the install wrote into, apart
/// from what was there before.
type DirIdentity = (u64, u64, i64, i64);

fn dir_identity(meta: &Metadata) -> DirIdentity {
    (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec())
}

fn relabel_walk_config<'p>() -> cap_std_ext::dirext::WalkConfiguration<'p> {
    cap_std_ext::dirext::WalkConfiguration::default()
        .noxdev()
        .skip_mountpoints()
}

/// The directories at [`RELABEL_SPLIT_DEPTH`] that existed before an install.
#[derive(Debug, Default)]
pub(crate) struct PreexistingDirs(HashSet<DirIdentity>);

impl PreexistingDirs {
    /// Record the directories at [`RELABEL_SPLIT_DEPTH`] below `root`.
    #[context("Recording existing directories")]
    pub(crate) fn snapshot(root: &Dir) -> Result<Self> {
        let mut dirs = HashSet::new();
        root.walk::<_, anyhow::Error>(&relabel_walk_config(), |c| {
            // Break on a non-directory would skip its siblings too.
            if !c.file_type.is_dir() || c.path.components().count() < RELABEL_SPLIT_DEPTH {
                return Ok(ControlFlow::Continue(()));
            }
            dirs.insert(dir_identity(&c.entry.metadata()?));
            Ok(ControlFlow::Break(()))
        })?;
        Ok(Self(dirs))
    }

    fn contains(&self, meta: &Metadata) -> bool {
        self.0.contains(&dir_identity(meta))
    }
}

/// Files found without a label, to relabel once the walk is done.
type Unlabeled = Vec<(Utf8PathBuf, Metadata)>;

/// Check the label of one entry, recording it in `unlabeled` if it has none.
fn check_label(
    root: &Dir,
    path: &Utf8Path,
    metadata: Metadata,
    unlabeled_type: Option<&UnlabeledType>,
    visited: &AtomicU64,
    unlabeled: &mut Unlabeled,
) -> Result<SELinuxLabelState> {
    visited.fetch_add(1, Ordering::Relaxed);
    let r = has_security_selinux_inner(root, path, unlabeled_type)?;
    if r == SELinuxLabelState::Unlabeled {
        unlabeled.push((path.to_owned(), metadata));
    }
    Ok(r)
}

fn walk_component_path<'a>(c: &'a cap_std_ext::dirext::WalkComponent) -> Result<&'a Utf8Path> {
    Utf8Path::from_path(c.path)
        .ok_or_else(|| anyhow::anyhow!("Invalid non-UTF-8 path: {:?}", c.path))
}

/// Find the unlabeled files in the directory `path` (relative to `root`),
/// including the directory itself.
fn find_unlabeled_in(
    root: &Dir,
    path: &Utf8Path,
    unlabeled_type: Option<&UnlabeledType>,
    visited: &AtomicU64,
) -> Result<Unlabeled> {
    let mut unlabeled = Vec::new();
    let metadata = root.symlink_metadata(path)?;
    check_label(
        root,
        path,
        metadata,
        unlabeled_type,
        visited,
        &mut unlabeled,
    )?;
    let config = relabel_walk_config().path_base(path.as_std_path());
    root.open_dir(path)?
        .walk::<_, anyhow::Error>(&config, |c| {
            let path = walk_component_path(c)?;
            let metadata = c.entry.metadata()?;
            match check_label(
                root,
                path,
                metadata,
                unlabeled_type,
                visited,
                &mut unlabeled,
            )? {
                SELinuxLabelState::Unsupported => {
                    anyhow::bail!("Unexpected SELinuxLabelState::Unsupported during walk at {path}")
                }
                _ => Ok(ControlFlow::Continue(())),
            }
        })
        .with_context(|| format!("Checking labels in {path}"))?;
    Ok(unlabeled)
}

/// Walk `root` down to [`RELABEL_SPLIT_DEPTH`], calling `on_entry` for each
/// entry above it, and return the directories at that depth, except
/// `preexisting` ones, for the caller to walk.
fn split_relabel_walk(
    root: &Dir,
    preexisting: Option<&PreexistingDirs>,
    mut on_entry: impl FnMut(&Utf8Path, Metadata) -> Result<()>,
) -> Result<Vec<Utf8PathBuf>> {
    let mut subdirs = Vec::new();
    root.walk::<_, anyhow::Error>(&relabel_walk_config(), |c| {
        let path = walk_component_path(c)?;
        let metadata = c.entry.metadata()?;
        if c.file_type.is_dir() && path.components().count() == RELABEL_SPLIT_DEPTH {
            if preexisting.is_some_and(|p| p.contains(&metadata)) {
                tracing::debug!("Skipping existing {path}");
            } else {
                subdirs.push(path.to_owned());
            }
            return Ok(ControlFlow::Break(()));
        }
        on_entry(path, metadata)?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(subdirs)
}

/// Ensure every file in the physical root of an install has a SELinux label,
/// without walking the `preexisting` directories (see
/// [`RELABEL_SPLIT_DEPTH`]). Labels are checked in parallel; the policy can't
/// be shared between threads, so the files lacking one are relabeled
/// afterwards.
pub(crate) async fn ensure_physical_root_labeled(
    root: &Dir,
    policy: &ostree::SePolicy,
    preexisting: Option<&PreexistingDirs>,
) -> Result<()> {
    // When SELinux is unavailable to this process, retain the conservative
    // xattr-presence behavior in `has_security_selinux_inner`.
    let unlabeled_type = Arc::new(UnlabeledType::query()?);
    let visited = Arc::new(AtomicU64::new(0));
    let mut unlabeled = Vec::new();

    let root_meta = root.dir_metadata()?;
    let root_path = Utf8Path::new("");
    let r = check_label(
        root,
        root_path,
        root_meta,
        unlabeled_type.as_ref().as_ref(),
        &visited,
        &mut unlabeled,
    )?;
    if r == SELinuxLabelState::Unsupported {
        return Ok(());
    }

    // Check the top levels here, and collect the directories below them to
    // check in parallel.
    let subdirs = split_relabel_walk(root, preexisting, |path, metadata| {
        let ut = unlabeled_type.as_ref().as_ref();
        match check_label(root, path, metadata, ut, &visited, &mut unlabeled)? {
            SELinuxLabelState::Unsupported => {
                anyhow::bail!("Unexpected SELinuxLabelState::Unsupported during walk at {path}")
            }
            _ => Ok(()),
        }
    })?;

    let mut subdirs = subdirs.into_iter();
    let mut tasks = tokio::task::JoinSet::new();
    let mut progress = tokio::time::interval(RELABEL_PROGRESS_INTERVAL);
    // The first tick completes immediately.
    progress.tick().await;
    loop {
        while tasks.len() < RELABEL_MAX_CONCURRENT {
            let Some(path) = subdirs.next() else {
                break;
            };
            let root = root.try_clone()?;
            let unlabeled_type = Arc::clone(&unlabeled_type);
            let visited = Arc::clone(&visited);
            tasks.spawn_blocking(move || {
                find_unlabeled_in(&root, &path, unlabeled_type.as_ref().as_ref(), &visited)
            });
        }
        tokio::select! {
            r = tasks.join_next() => match r {
                Some(r) => unlabeled.extend(r??),
                None => break,
            },
            _ = progress.tick() => {
                println!("Checking SELinux labels: {} files", visited.load(Ordering::Relaxed));
            }
        }
    }

    for (path, metadata) in &unlabeled {
        relabel(root, metadata, path, None, policy)?;
    }
    println!(
        "Checked SELinux labels: {} files, {} relabeled",
        visited.load(Ordering::Relaxed),
        unlabeled.len()
    );
    Ok(())
}

/// A wrapper for creating a directory, also optionally setting a SELinux label.
pub(crate) fn ensure_dir_labeled(
    root: &Dir,
    destname: impl AsRef<Utf8Path>,
    as_path: Option<&Utf8Path>,
    mode: rustix::fs::Mode,
    policy: Option<&ostree::SePolicy>,
) -> Result<()> {
    use std::borrow::Cow;

    let destname = destname.as_ref();
    // Special case the empty string
    let local_destname = if destname.as_str().is_empty() {
        ".".into()
    } else {
        destname
    };
    tracing::debug!("Labeling {local_destname}");
    let label = policy
        .map(|policy| {
            let as_path = as_path
                .map(Cow::Borrowed)
                .unwrap_or_else(|| Utf8Path::new("/").join(destname).into());
            require_label(policy, &as_path, libc::S_IFDIR | mode.as_raw_mode())
        })
        .transpose()
        .with_context(|| format!("Labeling {local_destname}"))?;
    tracing::trace!("Label for {local_destname} is {label:?}");

    root.ensure_dir_with(local_destname, &DirBuilder::new())
        .with_context(|| format!("Opening {local_destname}"))?;
    let dirfd = cap_std_ext::cap_primitives::fs::open(
        &root.as_filelike_view(),
        local_destname.as_std_path(),
        OpenOptions::new().read(true),
    )
    .context("opendir")?;
    let dirfd = dirfd.as_fd();
    rustix::fs::fchmod(dirfd, mode).context("fchmod")?;
    if let Some(label) = label {
        set_security_selinux(dirfd, label.as_bytes())?;
    }

    Ok(())
}

/// A wrapper for atomically writing a file, also optionally setting a SELinux label.
/// The label is looked up for `destname` as if `root` were `/`, so pass the
/// directory that stands for the target's root, never a subdirectory of it.
pub(crate) fn atomic_replace_labeled<F>(
    root: &Dir,
    destname: impl AsRef<Utf8Path>,
    mode: rustix::fs::Mode,
    policy: Option<&ostree::SePolicy>,
    f: F,
) -> Result<()>
where
    F: FnOnce(&mut std::io::BufWriter<cap_std_ext::cap_tempfile::TempFile>) -> Result<()>,
{
    let destname = destname.as_ref();
    let label = policy
        .map(|policy| {
            let abs_destname = Utf8Path::new("/").join(destname);
            require_label(policy, &abs_destname, libc::S_IFREG | mode.as_raw_mode())
        })
        .transpose()?;

    root.atomic_replace_with(destname, |w| {
        // Peel through the bufwriter to get the fd
        let fd = w.get_mut();
        let fd = fd.as_file_mut();
        let fd = fd.as_fd();
        // Apply the target mode bits
        rustix::fs::fchmod(fd, mode).context("fchmod")?;
        // If we have a label, apply it
        if let Some(label) = label {
            tracing::debug!("Setting label for {destname} to {label}");
            set_security_selinux(fd, label.as_bytes())?;
        } else {
            tracing::debug!("No label for {destname}");
        }
        // Finally call the underlying writer function
        f(w)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gio::glib::Variant;

    #[test]
    fn test_selinux_xattr() {
        let notfound: &[&[(&[u8], &[u8])]] = &[&[], &[(b"foo", b"bar")]];
        for case in notfound {
            assert!(!xattrs_have_selinux(&Variant::from(case)));
        }
        let found: &[(&[u8], &[u8])] = &[(b"foo", b"bar"), (SELINUX_XATTR, b"foo_t")];
        assert!(xattrs_have_selinux(&Variant::from(found)));
    }

    #[test]
    fn test_split_relabel_walk_skips_preexisting() -> Result<()> {
        let td = cap_std_ext::cap_tempfile::TempDir::new(cap_std::ambient_authority())?;
        // An existing system: a deployment with a large home directory, and
        // an ostree repo.
        td.create_dir_all("ostree/deploy/old/var/home/user")?;
        for i in 0..100 {
            td.write(format!("ostree/deploy/old/var/home/user/f{i}"), "x")?;
        }
        td.create_dir_all("ostree/repo/objects/00")?;
        td.write("ostree/repo/config", "")?;
        let preexisting = PreexistingDirs::snapshot(&td)?;

        // What an install writes: new trees, and a file directly in an
        // existing directory at the split depth, which is then walked too.
        // A change further down, like a new ostree object, isn't seen.
        td.create_dir_all("state/deploy/new/etc")?;
        td.create_dir_all("boot/loader/entries")?;
        td.write("ostree/repo/objects/new", "")?;
        td.write("ostree/repo/objects/00/new", "")?;

        let mut visited = Vec::new();
        let mut subdirs = split_relabel_walk(&td, Some(&preexisting), |p, _| {
            visited.push(p.to_string());
            Ok(())
        })?;
        subdirs.sort();
        assert_eq!(
            subdirs,
            [
                "boot/loader/entries",
                "ostree/repo/objects",
                "state/deploy/new"
            ]
        );
        visited.sort();
        let expected = [
            "boot",
            "boot/loader",
            "ostree",
            "ostree/deploy",
            "ostree/repo",
            "ostree/repo/config",
            "state",
            "state/deploy",
        ];
        assert_eq!(visited, expected);

        // Without a snapshot, every directory at the split depth is walked.
        let subdirs = split_relabel_walk(&td, None, |_, _| Ok(()))?;
        assert_eq!(subdirs.len(), 4);
        Ok(())
    }

    #[test]
    #[cfg(feature = "selinux")]
    fn test_unlabeled_type() {
        // Only meaningful where SELinux is enabled
        let Some(unlabeled_type) = UnlabeledType::query().unwrap() else {
            return;
        };
        assert!(
            unlabeled_type
                .matches(b"system_u:object_r:unlabeled_t:s0\0")
                .unwrap()
        );
        assert!(
            !unlabeled_type
                .matches(b"system_u:object_r:var_t:s0")
                .unwrap()
        );
    }

    #[test]
    #[cfg(feature = "selinux")]
    fn test_context_type() {
        let cases: &[(&[u8], Option<&str>)] = &[
            (b"system_u:object_r:unlabeled_t:s0", Some("unlabeled_t")),
            (b"system_u:object_r:unlabeled_t:s0\0", Some("unlabeled_t")),
            (
                b"system_u:object_r:unlabeled_t:s0:c0.c1023",
                Some("unlabeled_t"),
            ),
            (b"system_u:object_r:var_t:s0", Some("var_t")),
            (b"system_u:object_r", None),
            (b"system_u:object_r:var_t:s0\0\0", None),
            (b"system_u:object_r:var_t\0:s0", None),
            (b"system_u:object_r:unlabeled_t:\xff", None),
        ];

        for (context, expected_type) in cases {
            assert_eq!(
                context_type(context)
                    .ok()
                    .and_then(|context_type| context_type.into_string().ok())
                    .as_deref(),
                *expected_type
            );
        }
    }
}
