//! Version-matrix install-then-upgrade test in a VM.
//!
//! Builds bootc from two git revisions onto a base image, installs the first
//! with `bcvk libvirt run` (which uses `bcvk to-disk`) on the chosen filesystem
//! and backend, then `bootc switch`es to the second and reboots into it.
//!
//! Like the upgrade tmt tests, the target image reaches the guest through the
//! host container storage mounted with `--bind-storage-ro`, so no registry is
//! needed. To test a pull request, fetch its head first, e.g.
//! `git fetch https://github.com/bootc-dev/bootc pull/2629/head` and pass
//! `--to FETCH_HEAD`. Release tags need `git fetch --tags` in a shallow clone.
//!
//! The base image must already carry what the chosen backend and bootloader
//! need: for example the composefs backend on CentOS Stream may need
//! `--karg=enforcing=0`, and `--bootloader systemd` needs systemd-boot in the
//! base, which `quay.io/fedora/fedora-bootc:44` doesn't have.

use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use camino::Utf8Path;
use fn_error_context::context;
use xshell::{Shell, cmd};

use crate::VmUpgradeTestArgs;
use crate::tmt::{COMMON_INST_ARGS, generate_random_suffix};

/// Repository for the images this command builds; each is tagged with its
/// commit and the base image reference.
const IMAGE_REPO: &str = "localhost/bootc-vm-upgrade";

/// Number of characters of the commit ID used in the image tag.
const TAG_COMMIT_LEN: usize = 12;

/// Maximum length of an image tag.
const MAX_TAG_LEN: usize = 128;

/// How many times to try SSH while waiting for the VM to (re)boot.
const SSH_MAX_ATTEMPTS: u32 = 100;

const SSH_RETRY_DELAY: Duration = Duration::from_secs(3);

/// Builds bootc from `/src` (a `git archive` of the revision) with its own
/// build dependencies and installs it over the base image, regenerating the
/// initramfs so it carries that revision's dracut module, if it has one.
/// The spec is copied first so the build dependencies layer is shared
/// across revisions with the same spec.
const CONTAINERFILE: &str = r#"ARG base
FROM $base AS build
COPY contrib/packaging/bootc.spec /tmp/
RUN if test -x /usr/bin/dnf5; then dnf=dnf5; else dnf=dnf; fi; \
    dnf -y install make "$dnf-command(builddep)" && dnf -y builddep /tmp/bootc.spec
COPY . /src
WORKDIR /src
# /root is a symlink to /var/roothome, which bootc images leave out.
RUN --mount=type=cache,target=/var/roothome make bin && make install DESTDIR=/out

FROM $base
COPY --from=build /out/ /
RUN set -eux; add=; \
    if ls -d /usr/lib/dracut/modules.d/*bootc >/dev/null 2>&1; then add="--add bootc"; fi; \
    for d in /usr/lib/modules/*; do \
        kver=$(basename $d); dracut -f $add $d/initramfs.img $kver; \
    done
"#;

/// Return the `bcvk libvirt run` install and firmware arguments for `args`.
fn bcvk_install_args(args: &VmUpgradeTestArgs) -> Vec<String> {
    // Non-sealed images boot with insecure firmware; see BcvkInstallOpts::firmware_args.
    let mut r = vec![
        "--firmware=uefi-insecure".to_string(),
        format!("--filesystem={}", args.filesystem),
    ];
    if args.composefs_backend {
        r.push("--composefs-backend".into());
        if let Some(b) = &args.bootloader {
            r.push(format!("--bootloader={b}"));
        }
        if args.allow_missing_fsverity {
            r.push("--allow-missing-fsverity".into());
        }
    }
    r.extend(args.karg.iter().map(|k| format!("--karg={k}")));
    r
}

/// Resolve git revision `rev` to a full commit ID.
#[context("Resolving {rev}")]
fn resolve_commit(sh: &Shell, rev: &str) -> Result<String> {
    let spec = format!("{rev}^{{commit}}");
    Ok(cmd!(sh, "git rev-parse --verify {spec}").read()?)
}

/// Return the image name for bootc at `commit` built onto `base`.
fn image_name(commit: &str, base: &str) -> String {
    let commit = commit.get(..TAG_COMMIT_LEN).unwrap_or(commit);
    let base: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut tag = format!("{commit}-{base}");
    tag.truncate(MAX_TAG_LEN);
    format!("{IMAGE_REPO}:{tag}")
}

/// Build bootc at `commit` onto `base` and return the image name.
#[context("Building image for {commit}")]
fn build_image(sh: &Shell, commit: &str, base: &str) -> Result<String> {
    let image = image_name(commit, base);
    let td = tempfile::tempdir()?;
    let td = Utf8Path::from_path(td.path()).context("Non-UTF-8 temporary directory")?;
    let archive = td.join("src.tar");
    let ctx = td.join("src");
    let containerfile = td.join("Containerfile");
    std::fs::create_dir(&ctx)?;
    std::fs::write(&containerfile, CONTAINERFILE)?;
    cmd!(sh, "git archive --output={archive} {commit}").run()?;
    cmd!(sh, "tar -C {ctx} -xf {archive}").run()?;
    // Unchanged sources hit podman's layer cache, so rows sharing a revision
    // only build it once.
    cmd!(
        sh,
        "podman build --build-arg=base={base} -f {containerfile} -t {image} {ctx}"
    )
    .run()?;
    Ok(image)
}

/// Run `command` in the VM over SSH and return its output.
fn vm_ssh(sh: &Shell, vm: &str, command: &[&str]) -> Result<String> {
    Ok(cmd!(sh, "bcvk libvirt ssh {vm} -- {command...}").read()?)
}

/// Wait until the VM answers over SSH with a boot ID other than `old_boot_id`,
/// and return the new one.
#[context("Waiting for VM {vm} to boot")]
fn wait_for_boot(sh: &Shell, vm: &str, old_boot_id: Option<&str>) -> Result<String> {
    const BOOT_ID: &[&str] = &["cat", "/proc/sys/kernel/random/boot_id"];
    for _ in 0..SSH_MAX_ATTEMPTS {
        if let Ok(id) = cmd!(sh, "bcvk libvirt ssh {vm} -- {BOOT_ID...}")
            .quiet()
            .ignore_stderr()
            .read()
            && !id.is_empty()
            && Some(id.as_str()) != old_boot_id
        {
            return Ok(id);
        }
        thread::sleep(SSH_RETRY_DELAY);
    }
    anyhow::bail!("No new boot after {SSH_MAX_ATTEMPTS} SSH attempts")
}

/// Return the image reference of the booted deployment from `bootc status --json`.
fn booted_image(status: &str) -> Result<String> {
    let status: serde_json::Value = serde_json::from_str(status).context("Parsing status")?;
    status
        .pointer("/status/booted/image/image/image")
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned)
        .context("No booted image in bootc status")
}

/// Print `bootc status` in the VM and check it booted `expected`.
fn check_booted(sh: &Shell, vm: &str, expected: &str) -> Result<()> {
    println!("{}", vm_ssh(sh, vm, &["bootc", "status"])?);
    let booted = booted_image(&vm_ssh(sh, vm, &["bootc", "status", "--json"])?)?;
    anyhow::ensure!(booted == expected, "Booted {booted}, expected {expected}");
    Ok(())
}

/// Check the installed image, switch to `to_image`, reboot and check it booted.
fn run_in_vm(sh: &Shell, vm: &str, from_image: &str, to_image: &str) -> Result<()> {
    let boot_id = wait_for_boot(sh, vm, None)?;
    check_booted(sh, vm, from_image).context("After the install")?;
    cmd!(
        sh,
        "bcvk libvirt ssh {vm} -- bootc switch --transport containers-storage {to_image}"
    )
    .run()
    .context("bootc switch")?;
    // The connection drops as the VM goes down.
    let _ = cmd!(sh, "bcvk libvirt ssh {vm} -- systemctl reboot")
        .ignore_status()
        .run();
    wait_for_boot(sh, vm, Some(&boot_id))?;
    check_booted(sh, vm, to_image).context("After the switch")?;
    println!("ok: booted {to_image}");
    Ok(())
}

/// Build both revisions, install the first in a VM, switch to the second and reboot.
#[context("Running VM upgrade test")]
pub(crate) fn vm_upgrade_test(sh: &Shell, args: &VmUpgradeTestArgs) -> Result<()> {
    if args.bootloader.is_some() && !args.composefs_backend {
        anyhow::bail!("--bootloader requires --composefs-backend");
    }
    if args.allow_missing_fsverity && !args.composefs_backend {
        anyhow::bail!("--allow-missing-fsverity requires --composefs-backend");
    }
    let from = resolve_commit(sh, &args.from)?;
    let to = resolve_commit(sh, &args.to)?;
    // Switching to the same image would pass without testing anything.
    anyhow::ensure!(from != to, "--from and --to are both {from}");
    let base = &args.base;
    let from_image = build_image(sh, &from, base)?;
    let to_image = build_image(sh, &to, base)?;

    let vm = args
        .name
        .clone()
        .unwrap_or_else(|| format!("bootc-vm-upgrade-{}", generate_random_suffix()));
    let vm = vm.as_str();
    println!("VM: {vm}");
    let install_args = bcvk_install_args(args);
    // --replace only matters for a VM name given with --name.
    let r = cmd!(
        sh,
        "bcvk libvirt run --name={vm} --replace --detach {COMMON_INST_ARGS...} --bind-storage-ro {install_args...} {from_image}"
    )
    .run()
    .context("Launching VM")
    .and_then(|()| run_in_vm(sh, vm, &from_image, &to_image));
    if args.keep {
        println!("Keeping VM {vm}; remove it with: bcvk libvirt rm --stop --force {vm}");
    } else if let Err(e) = cmd!(sh, "bcvk libvirt rm --stop --force {vm}").run() {
        eprintln!("Warning: Failed to remove VM {vm}: {e}");
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Bootloader;

    #[test]
    fn test_bcvk_install_args() {
        // (composefs_backend, bootloader, allow_missing_fsverity) => expected args
        let cases: &[(bool, Option<Bootloader>, bool, &[&str])] = &[
            (
                false,
                None,
                false,
                &["--firmware=uefi-insecure", "--filesystem=btrfs"],
            ),
            (
                true,
                Some(Bootloader::Grub),
                true,
                &[
                    "--firmware=uefi-insecure",
                    "--filesystem=btrfs",
                    "--composefs-backend",
                    "--bootloader=grub",
                    "--allow-missing-fsverity",
                ],
            ),
        ];
        for (composefs_backend, bootloader, allow_missing_fsverity, expected) in cases {
            let args = VmUpgradeTestArgs {
                from: "v1.16.0".into(),
                to: "HEAD".into(),
                base: "quay.io/fedora/fedora-bootc:44".into(),
                filesystem: "btrfs".into(),
                composefs_backend: *composefs_backend,
                bootloader: bootloader.clone(),
                allow_missing_fsverity: *allow_missing_fsverity,
                karg: Vec::new(),
                name: None,
                keep: false,
            };
            assert_eq!(bcvk_install_args(&args), *expected);
        }
    }

    #[test]
    fn test_image_name() {
        let commit = "0ca885fef4ea0cd658ab495ab39f3a70c14528fb";
        assert_eq!(
            image_name(commit, "quay.io/fedora/fedora-bootc:44"),
            "localhost/bootc-vm-upgrade:0ca885fef4ea-quay.io-fedora-fedora-bootc-44"
        );
        let long = image_name(commit, &"x".repeat(200));
        assert_eq!(long.len(), IMAGE_REPO.len() + 1 + MAX_TAG_LEN);
    }

    #[test]
    fn test_booted_image() {
        let status = r#"{"status":{"booted":{"image":{"image":{"image":"localhost/x:1","transport":"containers-storage"}}}}}"#;
        assert_eq!(booted_image(status).unwrap(), "localhost/x:1");
        assert!(booted_image(r#"{"status":{"booted":null}}"#).is_err());
    }
}
