//! Machine-readable result of `bootc install`.
//!
//! An installer wrapping `bootc install` usually needs to know where the new
//! deployment ended up, e.g. to inject configuration into its `/etc` before
//! the first boot.  The `--output-{json,pairs}-{path,fd}` options write an
//! [`InstallResult`] once the installation has succeeded, either as JSON or
//! as shell-quoted `KEY="value"` lines in the style of `lsblk --pairs --shell`.

use std::io::Write as _;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std_ext::cap_std::fs::{Dir, Permissions, PermissionsExt};
use cap_std_ext::dirext::CapStdExtDirExt;
use fn_error_context::context;
use ostree_ext::container::OstreeImageReference;
use rustix::fs::{Access, AtFlags, Mode, OFlags};
use rustix::io::FdFlags;
use serde::Serialize;

use crate::spec::{Bootloader, ImageReference};

/// Carries the directory of an `--output-*-path` across re-executions of
/// bootc; see [`InstallOutputOpts::open`].
const OUTPUT_DIRFD_ENV: &str = "_BOOTC_INSTALL_OUTPUT_DIRFD";

/// Mode of a file written by `--output-*-path`.
const OUTPUT_FILE_MODE: u32 = 0o644;

/// Options to write the result of the installation in a machine-readable form.
#[derive(Debug, Clone, Default, clap::Args, PartialEq, Eq)]
#[group(id = "install-output", multiple = false)]
pub(crate) struct InstallOutputOpts {
    /// Write the result of the installation as JSON to this path, replacing it
    /// atomically.
    ///
    /// Nothing is written unless the installation succeeds.  At most one of
    /// the --output-* options may be given.
    #[clap(long, value_name = "PATH")]
    pub(crate) output_json_path: Option<Utf8PathBuf>,

    /// Write the result of the installation as JSON to this inherited file
    /// descriptor, which must be open for writing, then close it.
    #[clap(long, value_name = "FD")]
    pub(crate) output_json_fd: Option<RawFd>,

    /// Write the result of the installation as shell-quoted KEY="value" lines,
    /// like `lsblk --pairs --shell`, to this path, replacing it atomically.
    ///
    /// The output is suitable for `eval` or `.` in a shell script.
    #[clap(long, value_name = "PATH")]
    pub(crate) output_pairs_path: Option<Utf8PathBuf>,

    /// Write the result of the installation as shell-quoted KEY="value" lines
    /// to this inherited file descriptor, which must be open for writing, then
    /// close it.
    #[clap(long, value_name = "FD")]
    pub(crate) output_pairs_fd: Option<RawFd>,
}

impl InstallOutputOpts {
    /// Validate and open the requested destination, if any.
    ///
    /// Call this before any installation work, so that a bad destination
    /// fails early, and before `prepare_install`: that may mount a tmpfs on
    /// `/tmp` and re-execute bootc.  The destination directory is opened here
    /// and handed down to re-executed processes (see
    /// [`InstallOutput::reexec_env`]), so that a path under `/tmp` still
    /// refers to the caller's directory.
    pub(crate) fn open(&self) -> Result<Option<InstallOutput>> {
        let candidates = [
            (
                OutputFormat::Json,
                &self.output_json_path,
                self.output_json_fd,
            ),
            (
                OutputFormat::Pairs,
                &self.output_pairs_path,
                self.output_pairs_fd,
            ),
        ];
        // clap ensures that at most one of these is set
        for (format, path, fd) in candidates {
            let dest = match (path, fd) {
                (Some(path), _) => Destination::open_path(path)?,
                (None, Some(fd)) => Destination::from_fd(fd)?,
                (None, None) => continue,
            };
            return Ok(Some(InstallOutput { format, dest }));
        }
        Ok(None)
    }
}

/// The storage backend of an installed system.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Backend {
    Ostree,
    Composefs,
}

/// What `bootc install` installed.
///
/// This is a stable interface, documented in bootc-install-to-filesystem(8):
/// keys may be added, but existing ones keep their meaning.  Paths are
/// relative to the root of the target filesystem.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstallResult {
    /// The storage backend.
    pub(crate) backend: Backend,
    /// The stateroot of the deployment.
    pub(crate) stateroot: String,
    /// The deployment's root directory.
    pub(crate) deployment_path: Utf8PathBuf,
    /// The deployment's persistent `/etc`.
    pub(crate) etc_path: Utf8PathBuf,
    /// The persistent `/var`, shared by the deployments of the stateroot.
    pub(crate) var_path: Utf8PathBuf,
    /// The bootloader that was set up.
    pub(crate) bootloader: Bootloader,
    /// The image the system will update from.
    pub(crate) image: String,
    /// The transport of `image`, e.g. `registry`.
    pub(crate) image_transport: String,
    /// The manifest digest of the installed image.
    pub(crate) image_digest: String,
}

impl InstallResult {
    pub(crate) fn new(
        backend: Backend,
        stateroot: String,
        deployment_path: Utf8PathBuf,
        var_path: Utf8PathBuf,
        bootloader: Bootloader,
        target_imgref: &OstreeImageReference,
        image_digest: String,
    ) -> Self {
        let ImageReference {
            image, transport, ..
        } = ImageReference::from(target_imgref.clone());
        Self {
            backend,
            stateroot,
            etc_path: deployment_path.join("etc"),
            deployment_path,
            var_path,
            bootloader,
            image,
            image_transport: transport,
            image_digest,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Json,
    Pairs,
}

impl OutputFormat {
    fn render(self, result: &InstallResult) -> Result<Vec<u8>> {
        match self {
            OutputFormat::Json => {
                let mut buf = serde_json::to_vec_pretty(result)?;
                buf.push(b'\n');
                Ok(buf)
            }
            OutputFormat::Pairs => Ok(to_pairs(result)?.into_bytes()),
        }
    }
}

#[derive(Debug)]
enum Destination {
    /// An inherited file descriptor.
    Fd(OwnedFd),
    /// A file in `dir`, replaced atomically.
    Path {
        dir: OwnedFd,
        name: String,
        path: Utf8PathBuf,
    },
}

impl Destination {
    #[context("Validating output fd {fd}")]
    fn from_fd(fd: RawFd) -> Result<Self> {
        // bootc itself writes to stdout and stderr.
        if (0..=2).contains(&fd) {
            anyhow::bail!("Cannot use stdin, stdout or stderr; use a path or another inherited fd");
        }
        let fd = adopt_inherited_fd(fd)?;
        let flags = rustix::fs::fcntl_getfl(&fd)?;
        if !flags.intersects(OFlags::WRONLY | OFlags::RDWR) {
            anyhow::bail!("Not open for writing");
        }
        Ok(Self::Fd(fd))
    }

    #[context("Validating output path {path}")]
    fn open_path(path: &Utf8Path) -> Result<Self> {
        let name = path
            .file_name()
            .ok_or_else(|| anyhow!("Not a file path"))?
            .to_owned();
        let dir = match std::env::var(OUTPUT_DIRFD_ENV) {
            // Set by the process that re-executed us
            Ok(fd) => {
                let fd = fd
                    .parse()
                    .with_context(|| format!("Parsing {OUTPUT_DIRFD_ENV}"))?;
                adopt_inherited_fd(fd)?
            }
            Err(std::env::VarError::NotPresent) => {
                let parent = path
                    .parent()
                    .filter(|p| !p.as_str().is_empty())
                    .unwrap_or(Utf8Path::new("."));
                // Deliberately without O_CLOEXEC, so it survives a re-exec.
                rustix::fs::open(
                    parent.as_std_path(),
                    OFlags::DIRECTORY | OFlags::RDONLY,
                    Mode::empty(),
                )
                .with_context(|| format!("Opening directory {parent}"))?
            }
            Err(e) => return Err(e).with_context(|| format!("Reading {OUTPUT_DIRFD_ENV}")),
        };
        rustix::fs::accessat(&dir, ".", Access::WRITE_OK, AtFlags::EACCESS)
            .context("Checking that the directory is writable")?;
        match rustix::fs::statat(&dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) if rustix::fs::FileType::from_raw_mode(st.st_mode).is_dir() => {
                anyhow::bail!("Is a directory")
            }
            Ok(_) | Err(rustix::io::Errno::NOENT) => {}
            Err(e) => return Err(e).context("Querying the existing file"),
        }
        Ok(Self::Path {
            dir,
            name,
            path: path.to_owned(),
        })
    }
}

/// Take ownership of a file descriptor inherited from our parent process.
///
/// This checks that it is open and not close-on-exec: an inherited fd can't be,
/// while everything bootc opens itself is.  That rejects an fd number the
/// caller didn't actually pass, which may belong to e.g. the async runtime.
#[allow(unsafe_code)]
fn adopt_inherited_fd(fd: RawFd) -> Result<OwnedFd> {
    // SAFETY: The borrow is only used for fcntl(), which is fine for any fd number.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let fdflags = rustix::io::fcntl_getfd(borrowed).with_context(|| format!("fd {fd}"))?;
    if fdflags.contains(FdFlags::CLOEXEC) {
        anyhow::bail!("fd {fd} was not inherited from the calling process");
    }
    // SAFETY: The fd is open, and as it isn't close-on-exec, nothing in this
    // process owns it already (see above).
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A validated output destination and format.
#[derive(Debug)]
pub(crate) struct InstallOutput {
    format: OutputFormat,
    dest: Destination,
}

impl InstallOutput {
    /// Environment for a re-executed bootc to find the destination again.
    pub(crate) fn reexec_env(&self) -> Option<(&'static str, String)> {
        match &self.dest {
            Destination::Path { dir, .. } => Some((OUTPUT_DIRFD_ENV, dir.as_raw_fd().to_string())),
            Destination::Fd(_) => None,
        }
    }

    /// Write the result; an fd is closed afterwards.
    pub(crate) fn write(self, result: &InstallResult) -> Result<()> {
        let buf = self.format.render(result)?;
        match self.dest {
            Destination::Fd(fd) => {
                let fdnum = fd.as_raw_fd();
                std::fs::File::from(fd)
                    .write_all(&buf)
                    .with_context(|| format!("Writing install result to fd {fdnum}"))
            }
            Destination::Path { dir, name, path } => {
                let dir = Dir::from_std_file(std::fs::File::from(dir));
                dir.atomic_write_with_perms(&name, &buf, Permissions::from_mode(OUTPUT_FILE_MODE))
                    .with_context(|| format!("Writing install result to {path}"))
            }
        }
    }
}

/// Convert a camelCase key to a shell variable name, e.g. `etcPath` to
/// `ETC_PATH`.  Like `lsblk --shell`, any other character that isn't valid in
/// a variable name becomes `_`.
fn shell_key(key: &str) -> String {
    let mut r = String::with_capacity(key.len() + 4);
    let mut prev_lower = false;
    for c in key.chars() {
        if c.is_ascii_uppercase() && prev_lower {
            r.push('_');
        }
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        r.push(if c.is_ascii_alphanumeric() {
            c.to_ascii_uppercase()
        } else {
            '_'
        });
    }
    r
}

/// Double-quote a value for a POSIX shell, keeping it on one line.
///
/// `"`, `\`, `$` and `` ` `` are escaped with a backslash, as in os-release(5),
/// so that `eval` yields the original value.  Control characters such as a
/// newline are written as `\xNN` like `lsblk` does: that can't run anything
/// and keeps one line per key, but doesn't round-trip (bootc never emits
/// them in practice).  Everything else, including non-ASCII, is kept as is.
fn shell_quote(value: &str) -> String {
    let mut r = String::with_capacity(value.len() + 2);
    r.push('"');
    for c in value.chars() {
        match c {
            '"' | '\\' | '$' | '`' => {
                r.push('\\');
                r.push(c);
            }
            c if c.is_control() => r.push_str(&format!("\\x{:02x}", u32::from(c))),
            c => r.push(c),
        }
    }
    r.push('"');
    r
}

/// Render the fields of `v`, a struct of scalars, as sorted `KEY="value"` lines.
fn to_pairs<T: Serialize>(v: &T) -> Result<String> {
    let serde_json::Value::Object(map) = serde_json::to_value(v)? else {
        anyhow::bail!("Expected an object");
    };
    let mut pairs = map
        .into_iter()
        .map(|(k, v)| {
            let v = match v {
                serde_json::Value::String(s) => s,
                serde_json::Value::Null => String::new(),
                serde_json::Value::Bool(_) | serde_json::Value::Number(_) => v.to_string(),
                _ => anyhow::bail!("Unsupported nested value for {k}"),
            };
            Ok((shell_key(&k), v))
        })
        .collect::<Result<Vec<_>>>()?;
    pairs.sort();
    Ok(pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={}\n", shell_quote(&v)))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::os::fd::IntoRawFd as _;

    use cap_std_ext::cap_std;

    use super::*;

    fn sample_result() -> InstallResult {
        InstallResult::new(
            Backend::Ostree,
            "default".into(),
            "ostree/deploy/default/deploy/abc.0".into(),
            "ostree/deploy/default/var".into(),
            Bootloader::Grub,
            &"ostree-unverified-registry:quay.io/example/os:latest"
                .parse()
                .unwrap(),
            "sha256:0123".into(),
        )
    }

    #[test]
    fn test_shell_key() {
        for (input, expected) in [
            ("backend", "BACKEND"),
            ("etcPath", "ETC_PATH"),
            ("imageDigest", "IMAGE_DIGEST"),
            ("v2Thing", "V2_THING"),
            ("MAJ:MIN", "MAJ_MIN"),
            ("kebab-case", "KEBAB_CASE"),
        ] {
            assert_eq!(shell_key(input), expected, "{input}");
        }
    }

    // (input, quoted, whether `eval` yields the input again)
    const QUOTE_CASES: &[(&str, &str, bool)] = &[
        ("", r#""""#, true),
        ("plain", r#""plain""#, true),
        ("with spaces", r#""with spaces""#, true),
        (r#"a"quote"#, r#""a\"quote""#, true),
        ("single'quote", r#""single'quote""#, true),
        ("$HOME ${x}", r#""\$HOME \${x}""#, true),
        ("`id`", r#""\`id\`""#, true),
        ("$(id)", r#""\$(id)""#, true),
        (r"back\slash", r#""back\\slash""#, true),
        ("semi;colon & | < >", r#""semi;colon & | < >""#, true),
        ("ünïcødé ☃", r#""ünïcødé ☃""#, true),
        ("new\nline", r#""new\x0aline""#, false),
        ("tab\tdel\x7f", r#""tab\x09del\x7f""#, false),
    ];

    #[test]
    fn test_shell_quote() {
        for &(input, expected, _) in QUOTE_CASES {
            assert_eq!(shell_quote(input), expected, "{input:?}");
        }
    }

    /// Check what a real shell makes of the quoted values.
    #[test]
    fn test_shell_quote_eval() {
        for &(input, quoted, roundtrips) in QUOTE_CASES {
            let out = std::process::Command::new("sh")
                .args(["-c", r#"eval "V=$1"; printf %s "$V""#, "sh", quoted])
                .output()
                .unwrap();
            assert!(out.status.success(), "{input:?}: {out:?}");
            let out = String::from_utf8(out.stdout).unwrap();
            if roundtrips {
                assert_eq!(out, input, "{input:?}");
            } else {
                // Escaped control characters stay literal, and run nothing.
                assert_eq!(out, &quoted[1..quoted.len() - 1], "{input:?}");
            }
        }
    }

    #[test]
    fn test_render() {
        let r = sample_result();
        let json: serde_json::Value =
            serde_json::from_slice(&OutputFormat::Json.render(&r).unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "backend": "ostree",
                "stateroot": "default",
                "deploymentPath": "ostree/deploy/default/deploy/abc.0",
                "etcPath": "ostree/deploy/default/deploy/abc.0/etc",
                "varPath": "ostree/deploy/default/var",
                "bootloader": "grub",
                "image": "quay.io/example/os:latest",
                "imageTransport": "registry",
                "imageDigest": "sha256:0123",
            })
        );
        let pairs = String::from_utf8(OutputFormat::Pairs.render(&r).unwrap()).unwrap();
        assert_eq!(
            pairs,
            indoc::indoc! {r#"
                BACKEND="ostree"
                BOOTLOADER="grub"
                DEPLOYMENT_PATH="ostree/deploy/default/deploy/abc.0"
                ETC_PATH="ostree/deploy/default/deploy/abc.0/etc"
                IMAGE="quay.io/example/os:latest"
                IMAGE_DIGEST="sha256:0123"
                IMAGE_TRANSPORT="registry"
                STATEROOT="default"
                VAR_PATH="ostree/deploy/default/var"
            "#}
        );
    }

    #[test]
    fn test_fd_destination() -> Result<()> {
        let invalid = [
            (0, "stdin, stdout or stderr"),
            (1, "stdin, stdout or stderr"),
            (i32::MAX, "fd 2147483647"),
        ];
        for (fd, msg) in invalid {
            let e = Destination::from_fd(fd).unwrap_err();
            assert!(format!("{e:#}").contains(msg), "{fd}: {e:#}");
        }

        // A close-on-exec fd was not inherited
        let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
        let e = Destination::from_fd(w.as_raw_fd()).unwrap_err();
        assert!(format!("{e:#}").contains("not inherited"), "{e:#}");
        drop((r, w));

        let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::empty())?;
        // The read end is not writable
        let r = r.into_raw_fd();
        let e = Destination::from_fd(r).unwrap_err();
        assert!(format!("{e:#}").contains("Not open for writing"), "{e:#}");
        drop(w);

        let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::empty())?;
        let output = InstallOutput {
            format: OutputFormat::Pairs,
            dest: Destination::from_fd(w.into_raw_fd())?,
        };
        assert_eq!(output.reexec_env(), None);
        output.write(&sample_result())?;
        // The write end is closed now, so this reads to EOF.
        let mut buf = String::new();
        std::fs::File::from(r).read_to_string(&mut buf)?;
        assert!(buf.starts_with("BACKEND=\"ostree\"\n"), "{buf}");
        Ok(())
    }

    #[test]
    fn test_path_destination() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let tdpath = Utf8Path::from_path(tmp.path()).unwrap();
        let td = Dir::open_ambient_dir(tdpath, cap_std::ambient_authority())?;
        td.create_dir("subdir")?;

        for (path, msg) in [
            (tdpath.join("nonexistent/result.json"), "Opening directory"),
            (tdpath.join("subdir"), "Is a directory"),
            (tdpath.join(".."), "Not a file path"),
        ] {
            let e = Destination::open_path(&path).unwrap_err();
            assert!(format!("{e:#}").contains(msg), "{path}: {e:#}");
        }

        let path = tdpath.join("result.json");
        td.write("result.json", "old contents")?;
        let opts = InstallOutputOpts {
            output_json_path: Some(path),
            ..Default::default()
        };
        let output = opts.open()?.unwrap();
        let (k, _) = output.reexec_env().unwrap();
        assert_eq!(k, OUTPUT_DIRFD_ENV);
        output.write(&sample_result())?;
        let written: serde_json::Value = serde_json::from_str(&td.read_to_string("result.json")?)?;
        assert_eq!(written, serde_json::to_value(sample_result())?);
        Ok(())
    }

    #[test]
    fn test_no_output() {
        assert!(InstallOutputOpts::default().open().unwrap().is_none());
    }
}
