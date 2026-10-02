//! A minimal installer built on [`bootc_lib::install_api`]: partition and
//! format a disk image through a loop device, then install a bootc image to
//! it, reporting progress on stderr and writing the result as JSON to a file.
//!
//! ```text
//! install-to-loopback --source oci:/path/to/layout --disk disk.img --outcome outcome.json
//! ```
//!
//! Run it as root, outside a container, in a private mount namespace
//! (`unshare -m --propagation slave`); see the `install_api` documentation.

use std::process::Command;

use anyhow::{Context, Result};
use bootc_lib::install_api::{Event, FilesystemTarget, Installer};
use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;

/// The GPT partition type of the ESP.
const ESP_TYPE: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
/// The GPT partition type of a BIOS boot partition, for GRUB on BIOS systems.
#[cfg(target_arch = "x86_64")]
const BIOS_BOOT_TYPE: &str = "21686148-6449-6E6F-744E-656564454649";
/// The discoverable partitions specification type of the root partition.
#[cfg(target_arch = "x86_64")]
const ROOT_TYPE: &str = "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709";
#[cfg(target_arch = "aarch64")]
const ROOT_TYPE: &str = "B921B045-1DF0-41C3-AF44-4C6F280D3FAE";
/// Elsewhere, the generic Linux filesystem type, so the example still builds
/// (e.g. in `cargo test` on s390x and ppc64le).
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const ROOT_TYPE: &str = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";

#[derive(Parser)]
struct Opts {
    /// The image to install, with its transport (e.g. `oci:/path/to/layout`).
    #[clap(long)]
    source: String,

    /// The registry image the installed system updates from.
    #[clap(long)]
    target_imgref: Option<String>,

    /// The disk image file to create.
    #[clap(long)]
    disk: Utf8PathBuf,

    /// The disk image size, as accepted by truncate(1).
    #[clap(long, default_value = "10G")]
    size: String,

    /// Add a kernel argument.
    #[clap(long)]
    karg: Vec<String>,

    /// Where to write the install outcome as JSON. (Not stdout, which bootc
    /// still prints status messages to.)
    #[clap(long)]
    outcome: Utf8PathBuf,
}

fn run(cmd: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("Running {cmd}"))?;
    anyhow::ensure!(
        out.status.success(),
        "{cmd} {args:?} failed: {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

/// The loop device and mounts of the target, torn down on drop.
struct Target {
    loopdev: String,
    mountpoint: Option<tempfile::TempDir>,
}

impl Target {
    /// Where the root filesystem is mounted.
    fn root(&self) -> Result<&Utf8Path> {
        let m = self.mountpoint.as_ref().context("Target is not mounted")?;
        Utf8Path::from_path(m.path()).context("Non-UTF-8 mount point")
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        if let Some(m) = self.mountpoint.take() {
            let path = m.path().to_string_lossy().into_owned();
            if let Err(e) = run("umount", &["-R", &path]) {
                eprintln!("warning: {e:#}");
                // Don't let the TempDir delete the target's contents.
                let _ = m.keep();
            }
        }
        if let Err(e) = run("losetup", &["--detach", &self.loopdev]) {
            eprintln!("warning: {e:#}");
        }
    }
}

/// Partition and format `disk`, and mount it on a new temporary directory.
fn prepare_target(disk: &Utf8Path, size: &str) -> Result<Target> {
    run("truncate", &["--size", size, disk.as_str()])?;
    let loopdev = run(
        "losetup",
        &["--find", "--show", "--partscan", disk.as_str()],
    )?;
    let mut target = Target {
        loopdev,
        mountpoint: None,
    };
    let dev = target.loopdev.clone();

    let mut layout = String::from("label: gpt\n");
    #[cfg(target_arch = "x86_64")]
    layout.push_str(&format!("size=1MiB, type={BIOS_BOOT_TYPE}\n"));
    layout.push_str(&format!("size=512MiB, type={ESP_TYPE}\n"));
    layout.push_str(&format!("type={ROOT_TYPE}\n"));
    let mut sfdisk = Command::new("sfdisk")
        .args(["--quiet", "--wipe", "always", &dev])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("Running sfdisk")?;
    let mut stdin = sfdisk.stdin.take().context("sfdisk stdin")?;
    std::io::Write::write_all(&mut stdin, layout.as_bytes())?;
    drop(stdin);
    anyhow::ensure!(sfdisk.wait()?.success(), "sfdisk failed");
    run("udevadm", &["settle"])?;

    let first = if cfg!(target_arch = "x86_64") { 2 } else { 1 };
    let esp = format!("{dev}p{first}");
    let root = format!("{dev}p{}", first + 1);
    run("mkfs.vfat", &["-n", "EFI-SYSTEM", &esp])?;
    run("mkfs.xfs", &["-q", "-f", "-L", "root", &root])?;

    let mountpoint = tempfile::tempdir_in("/var/tmp")?;
    run("mount", &[&root, &mountpoint.path().to_string_lossy()])?;
    target.mountpoint = Some(mountpoint);
    let efi = target.root()?.join("boot/efi");
    std::fs::create_dir_all(&efi)?;
    run("mount", &[&esp, efi.as_str()])?;
    Ok(target)
}

async fn install(opts: Opts) -> Result<()> {
    let target = prepare_target(&opts.disk, &opts.size)?;
    let root = target.root()?;

    // A disk image is installed for some other machine.
    let mut installer = Installer::new(opts.source)
        .generic_image(true)
        .on_progress(|event| match event {
            Event::StepStarted(step) => eprintln!("install-to-loopback: step {step:?}"),
            other => eprintln!("install-to-loopback: {other:?}"),
        });
    if let Some(imgref) = opts.target_imgref {
        installer = installer.target_imgref(imgref);
    }
    for karg in opts.karg {
        installer = installer.karg(karg);
    }
    let outcome = installer
        .install_to_filesystem(&FilesystemTarget::new(root))
        .await
        .map_err(|e| {
            let kind = e.kind();
            anyhow::Error::new(e).context(format!("Install failed ({kind:?})"))
        })?;
    let outcome = serde_json::to_string_pretty(&outcome)?;
    std::fs::write(&opts.outcome, outcome).with_context(|| format!("Writing {}", opts.outcome))?;
    drop(target);
    Ok(())
}

fn main() -> Result<()> {
    let opts = Opts::parse();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(install(opts))
}
