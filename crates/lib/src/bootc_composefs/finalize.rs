use std::{io::Read, path::Path, process::Command};

use crate::bootc_composefs::boot::{GLOBAL_UKI_ADDONS_DIR, STAGED_GLOBAL_UKI_ADDONS_DIR};
use crate::bootc_composefs::gc::{GCOpts, composefs_gc};
use crate::bootc_composefs::rollback::{rename_exchange_bls_entries, rename_exchange_user_cfg};
use crate::bootc_composefs::status::get_composefs_status;
use crate::bootc_composefs::{aboot, boot::BootType};
use crate::composefs_consts::STATE_DIR_ABS;
use crate::install::BOOT;
use crate::spec::BootloaderKind;
use crate::store::{BootedComposefs, Storage};
use anyhow::{Context, Result};
use bootc_initramfs_setup::mount_composefs_image;
use bootc_mount::tempmount::TempMount;
use cap_std_ext::cap_std::{ambient_authority, fs::Dir};
use cap_std_ext::dirext::CapStdExtDirExt;
use composefs::generic_tree::{FileSystem, Stat};
use composefs_ctl::composefs;
use etc_merge::{Diff, compute_diff, merge, traverse_etc};
use rustix::fs::{Mode, OFlags, RenameFlags, fsync, renameat, renameat_with};

use fn_error_context::context;

// Like ostree's /boot/ostree/finalize-failure.stamp, this must survive shutdown
// and be independent of the deployment whose finalization failed. The hold unit
// keeps /boot available until finalization finishes.
const FINALIZE_FAILURE_STAMP: &str = "bootc-finalize-failure.stamp";
const PREVIOUS_FAILURE_STAMP: &str = "bootc-finalize-failure.stamp.previous";
const MAX_FAILURE_BYTES: usize = 16 * 1024;

/// Record the result even when loading storage failed. Never hide the original
/// finalization error if persisting its diagnostic also fails.
pub(crate) fn record_finalize_result(result: Result<()>) -> Result<()> {
    let recorded = open_boot_for_stamp().and_then(|boot| update_failure_stamp(&boot, &result));
    match result {
        Err(error) => {
            if let Err(stamp_error) = recorded {
                tracing::error!("Could not record finalization failure: {stamp_error:#}");
            }
            Err(error)
        }
        Ok(()) => recorded,
    }
}

pub(crate) fn open_boot_for_stamp() -> Result<Dir> {
    let root = Dir::open_ambient_dir("/", ambient_authority())?;
    let boot_is_mount = root.is_mountpoint(BOOT)? == Some(true);
    open_boot_for_stamp_impl(&root, boot_is_mount)
}

fn open_boot_for_stamp_impl(root: &Dir, boot_is_mount: bool) -> Result<Dir> {
    if boot_is_mount {
        crate::utils::open_dir_remount_rw(root, BOOT.into())
    } else {
        // Match get_boot_dir_for_grub: without a mounted /boot, its persistent
        // contents live under /sysroot/boot, not the read-only image's /boot.
        let sysroot = crate::utils::open_dir_remount_rw(root, "sysroot".into())?;
        sysroot
            .open_dir(BOOT)
            .context("Opening persistent /sysroot/boot")
    }
}

fn update_failure_stamp(boot: &Dir, result: &Result<()>) -> Result<()> {
    if let Err(error) = result {
        let mut message = format!("{error:#}");
        let mut end = message.len().min(MAX_FAILURE_BYTES);
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        // atomic_write syncs both the file and parent directory.
        boot.atomic_write(FINALIZE_FAILURE_STAMP, message)
            .context("Writing finalization failure stamp")?;
    } else {
        boot.remove_file_optional(FINALIZE_FAILURE_STAMP)
            .context("Clearing finalization failure stamp")?;
    }
    boot.open(".")?
        .sync_all()
        .context("Syncing finalization failure stamp directory")?;
    Ok(())
}

/// Consume the failure once, retaining the diagnostic for inspection. Bound the
/// read even if a damaged or externally written stamp is unexpectedly large.
pub(crate) fn boot_complete(boot: &Dir) -> Result<()> {
    let Some(file) = boot.open_optional(FINALIZE_FAILURE_STAMP)? else {
        return Ok(());
    };
    let mut message = Vec::new();
    file.take(MAX_FAILURE_BYTES as u64)
        .read_to_end(&mut message)?;
    boot.rename(FINALIZE_FAILURE_STAMP, boot, PREVIOUS_FAILURE_STAMP)
        .context("Consuming finalization failure stamp")?;
    boot.open(".")?
        .sync_all()
        .context("Syncing consumed finalization failure stamp")?;
    anyhow::bail!(
        "bootc-finalize-staged.service failed on previous boot: {}\nInspect the previous shutdown with: journalctl -b -1 -u bootc-finalize-staged.service",
        String::from_utf8_lossy(&message)
    )
}

pub(crate) async fn get_etc_diff(
    storage: &Storage,
    booted_cfs: &BootedComposefs,
    new_etc: Option<&Dir>,
) -> Result<Diff> {
    let host = get_composefs_status(storage, booted_cfs).await?;
    let booted_composefs = host.require_composefs_booted()?;

    // Mount the booted EROFS image to get pristine etc
    let sysroot_fd = storage.physical_root.reopen_as_ownedfd()?;
    let composefs_fd = mount_composefs_image(
        &sysroot_fd,
        &booted_composefs.verity,
        booted_cfs.cmdline.allow_missing_fsverity,
    )?;

    let erofs_tmp_mnt = TempMount::mount_fd(&composefs_fd)?;

    let pristine_etc =
        Dir::open_ambient_dir(erofs_tmp_mnt.dir.path().join("etc"), ambient_authority())?;
    let current_etc = Dir::open_ambient_dir("/etc", ambient_authority())?;

    let (pristine_files, current_files, new_etc_files) =
        traverse_etc(&pristine_etc, &current_etc, new_etc)?;

    let diff = compute_diff(
        &pristine_files,
        &current_files,
        &new_etc_files
            .as_ref()
            .unwrap_or(&FileSystem::new(Stat::uninitialized())),
    )?;

    Ok(diff)
}

pub(crate) async fn composefs_backend_finalize(
    storage: &Storage,
    booted_cfs: &BootedComposefs,
) -> Result<()> {
    const COMPOSEFS_FINALIZE_JOURNAL_ID: &str = "0e9d8c7b6a5f4e3d2c1b0a9f8e7d6c5b4";

    tracing::info!(
        message_id = COMPOSEFS_FINALIZE_JOURNAL_ID,
        bootc.operation = "finalize",
        bootc.current_deployment = booted_cfs.cmdline.digest,
        "Starting composefs staged deployment finalization"
    );

    let host = get_composefs_status(storage, booted_cfs).await?;

    let booted_composefs = host.require_composefs_booted()?;

    let Some(staged_depl) = host.status.staged.as_ref() else {
        tracing::info!(
            message_id = COMPOSEFS_FINALIZE_JOURNAL_ID,
            bootc.operation = "finalize",
            "No staged deployment found"
        );
        return Ok(());
    };

    if staged_depl.download_only {
        tracing::info!(
            message_id = COMPOSEFS_FINALIZE_JOURNAL_ID,
            bootc.operation = "finalize",
            bootc.download_only = "true",
            "Staged deployment is marked download only. Won't finalize"
        );
        return Ok(());
    }

    let staged_composefs = staged_depl.composefs.as_ref().ok_or(anyhow::anyhow!(
        "Staged deployment is not a composefs deployment"
    ))?;

    let mut aboot_state = (staged_composefs.boot_type == BootType::Aboot)
        .then(|| aboot::AbootState::open(&storage.physical_root))
        .transpose()?;
    if let Some(state) = aboot_state.as_ref()
        && state.read_attempted()?.is_some()
    {
        tracing::warn!("Aboot deployment was already attempted; not flashing it again");
        return Ok(());
    }

    // Mount the booted EROFS image to get pristine etc
    let sysroot_fd = storage.physical_root.reopen_as_ownedfd()?;
    let composefs_fd = mount_composefs_image(
        &sysroot_fd,
        &booted_composefs.verity,
        booted_cfs.cmdline.allow_missing_fsverity,
    )?;

    let erofs_tmp_mnt = TempMount::mount_fd(&composefs_fd)?;

    // Perform the /etc merge
    let pristine_etc =
        Dir::open_ambient_dir(erofs_tmp_mnt.dir.path().join("etc"), ambient_authority())?;
    let current_etc = Dir::open_ambient_dir("/etc", ambient_authority())?;

    let new_etc_path = Path::new(STATE_DIR_ABS)
        .join(&staged_composefs.verity)
        .join("etc");

    let new_etc = Dir::open_ambient_dir(&new_etc_path, ambient_authority()).with_context(|| {
        format!(
            "Opening staged deployment /etc at {}",
            new_etc_path.display()
        )
    })?;

    let (pristine_files, current_files, new_files) =
        traverse_etc(&pristine_etc, &current_etc, Some(&new_etc))?;

    let new_files =
        new_files.ok_or_else(|| anyhow::anyhow!("Failed to get dirtree for new etc"))?;

    let diff = compute_diff(&pristine_files, &current_files, &new_files)?;
    merge(&current_etc, &current_files, &new_etc, &new_files, &diff)?;

    // Remove /etc/.updated from the new deployment so that ConditionNeedsUpdate=|/etc
    // services (systemd-sysusers, systemd-tmpfiles) run on the first boot, mirroring
    // what ostree does in sysroot_finalize_deployment.
    new_etc
        .remove_file_optional(".updated")
        .context("Removing /etc/.updated from staged deployment")?;

    // Unmount EROFS
    drop(erofs_tmp_mnt);

    if staged_composefs.boot_type == BootType::Aboot {
        return finalize_staged_aboot(
            storage,
            aboot_state.as_mut().unwrap(),
            &staged_composefs.verity,
        );
    }

    let esp = storage.require_esp()?;

    // Un-stage Global UKI Addons (if present) as the penultimate step
    if esp.fd.exists(STAGED_GLOBAL_UKI_ADDONS_DIR) {
        if esp.fd.exists(GLOBAL_UKI_ADDONS_DIR) {
            tracing::debug!(
                "Atomically exchanging {STAGED_GLOBAL_UKI_ADDONS_DIR} and {GLOBAL_UKI_ADDONS_DIR}"
            );

            renameat_with(
                &esp.fd,
                STAGED_GLOBAL_UKI_ADDONS_DIR,
                &esp.fd,
                GLOBAL_UKI_ADDONS_DIR,
                RenameFlags::EXCHANGE,
            )
            .context("Atomically exchanging staged Global UKI Addons")?;

            esp.fd
                .remove_dir_all(STAGED_GLOBAL_UKI_ADDONS_DIR)
                .context("Removing staged Global UKI Addons dir")?;
        } else {
            tracing::debug!(
                "No global addons in the current deployment. Finalizing global UKI Addons"
            );

            renameat(
                &esp.fd,
                STAGED_GLOBAL_UKI_ADDONS_DIR,
                &esp.fd,
                GLOBAL_UKI_ADDONS_DIR,
            )
            .context("Finalizing global UKI Addons")?;
        }
    }

    let boot_dir = storage.require_boot_dir()?;

    match booted_composefs.bootloader.kind()? {
        BootloaderKind::GRUBClassic => match staged_composefs.boot_type {
            BootType::Bls => {
                let entries_dir = boot_dir.open_dir("loader")?;
                rename_exchange_bls_entries(&entries_dir)?;
            }
            BootType::Uki => finalize_staged_grub_uki(boot_dir)?,
            BootType::Aboot => unreachable!(),
        },

        BootloaderKind::BLSCompatible => {
            let entries_dir = boot_dir.open_dir("loader")?;
            rename_exchange_bls_entries(&entries_dir)?;
        }
    };

    // Now that we have successfully updated bootloader entries, we can GC the unreferenced ones
    // We do not prune the composefs repository here though
    composefs_gc(
        storage,
        booted_cfs,
        GCOpts {
            dry_run: false,
            prune_repo: false,
        },
    )
    .await?;

    Ok(())
}

/// Keep /boot open until we're killed, which systemd does with SIGTERM
/// when `bootc-finalize-staged-hold.service` is stopped after
/// `bootc-finalize-staged.service`.
///
/// When /boot is an automount (e.g. the ESP set up by
/// systemd-gpt-auto-generator), an idle expire breaks finalization in two
/// ways. If it races with shutdown, it deadlocks: the finalization (which
/// looks up /boot) blocks on the expire, while the unmount is ordered after
/// the finalization. If it completes before shutdown, systemd won't remount
/// /boot once shutdown has begun, so finalization fails to open it (EHOSTDOWN)
/// and the old deployment boots. An open file descriptor makes autofs treat
/// the mount as busy, so it never expires. Note this only works from the
/// root mount namespace.
pub(crate) fn hold_boot() -> Result<()> {
    let path = format!("/{BOOT}");
    let _fd = rustix::fs::open(
        path.as_str(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("Opening {path}"))?;
    tracing::debug!("Holding {path} open until terminated");
    loop {
        std::thread::park();
    }
}

#[context("Finalizing staged aboot deployment {deployment}")]
fn finalize_staged_aboot(
    storage: &Storage,
    state: &mut aboot::AbootState<'_>,
    deployment: &str,
) -> Result<()> {
    let pending = state
        .read_pending()?
        .ok_or_else(|| anyhow::anyhow!("No pending aboot deployment"))?;
    anyhow::ensure!(
        pending.depl_id == deployment,
        "Pending aboot deployment changed"
    );
    anyhow::ensure!(
        !pending.finalization_locked,
        "Pending aboot deployment is download-only"
    );
    let artifacts = state.verify_artifacts(&storage.physical_root_path, &pending)?;
    let kernel_cmdline =
        std::fs::read_to_string("/proc/cmdline").context("Reading kernel command line")?;
    let booted_slot = aboot::Slot::from_cmdline(&linux_kernel_cmdline::utf8::Cmdline::from(
        kernel_cmdline.as_str(),
    ));

    state.record_attempt(deployment)?;
    if let Some(slot) = booted_slot {
        state.invalidate_other(slot)?;
    }

    let mut command = Command::new("aboot-deploy");
    command.args(["--local", "--boot-image"]);
    command.arg(artifacts.boot);
    if let Some(vbmeta) = artifacts.vbmeta {
        command.arg("--vbmeta-image").arg(vbmeta);
    }
    let status = command.status().context("Running aboot-deploy")?;
    anyhow::ensure!(status.success(), "aboot-deploy exited with status {status}");
    Ok(())
}

#[context("Grub: Finalizing staged UKI")]
fn finalize_staged_grub_uki(boot_fd: &Dir) -> Result<()> {
    let entries_dir = boot_fd.open_dir("grub2")?;
    rename_exchange_user_cfg(&entries_dir)?;

    let entries_dir = entries_dir.reopen_as_ownedfd()?;
    fsync(entries_dir).context("fsync")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_stamp_boot_locations() -> Result<()> {
        for boot_is_mount in [false, true] {
            let root = cap_std_ext::cap_tempfile::tempdir(ambient_authority())?;
            root.create_dir(BOOT)?;
            root.create_dir_all("sysroot/boot")?;
            let boot = open_boot_for_stamp_impl(&root, boot_is_mount)?;
            update_failure_stamp(&boot, &Err(anyhow::anyhow!("broken staged etc")))?;
            let expected = if boot_is_mount {
                "boot"
            } else {
                "sysroot/boot"
            };
            assert!(root.try_exists(format!("{expected}/{FINALIZE_FAILURE_STAMP}"))?);
            assert!(boot_complete(&open_boot_for_stamp_impl(&root, boot_is_mount)?).is_err());
            boot_complete(&open_boot_for_stamp_impl(&root, boot_is_mount)?)?;
            update_failure_stamp(&boot, &Err(anyhow::anyhow!("retry failed")))?;
            update_failure_stamp(&boot, &Ok(()))?;
            boot_complete(&open_boot_for_stamp_impl(&root, boot_is_mount)?)?;
        }
        Ok(())
    }

    #[test]
    fn failure_stamp_lifecycle() -> Result<()> {
        let boot = cap_std_ext::cap_tempfile::tempdir(ambient_authority())?;
        // Missing stamps are a no-op, including repeated successful finalization.
        boot_complete(&boot)?;
        update_failure_stamp(&boot, &Ok(()))?;
        update_failure_stamp(&boot, &Ok(()))?;
        let failure = Err(anyhow::anyhow!("broken staged etc").context("Finalizing"));
        update_failure_stamp(&boot, &failure)?;
        assert_eq!(
            boot.read_to_string(FINALIZE_FAILURE_STAMP)?,
            "Finalizing: broken staged etc"
        );
        let error = boot_complete(&boot).unwrap_err().to_string();
        assert!(error.contains("Finalizing: broken staged etc"));
        assert!(error.contains("journalctl -b -1 -u bootc-finalize-staged.service"));
        assert!(!boot.try_exists(FINALIZE_FAILURE_STAMP)?);
        assert_eq!(
            boot.read_to_string(PREVIOUS_FAILURE_STAMP)?,
            "Finalizing: broken staged etc"
        );
        boot_complete(&boot)?;
        // A new failure replaces the previous diagnostic, and success clears it.
        update_failure_stamp(&boot, &failure)?;
        assert!(boot_complete(&boot).is_err());
        update_failure_stamp(&boot, &failure)?;
        update_failure_stamp(&boot, &Ok(()))?;
        assert!(!boot.try_exists(FINALIZE_FAILURE_STAMP)?);
        boot_complete(&boot)?;
        Ok(())
    }

    #[test]
    fn failure_stamp_is_bounded() -> Result<()> {
        let boot = cap_std_ext::cap_tempfile::tempdir(ambient_authority())?;
        update_failure_stamp(&boot, &Err(anyhow::anyhow!("€".repeat(MAX_FAILURE_BYTES))))?;
        let message = boot.read_to_string(FINALIZE_FAILURE_STAMP)?;
        assert!(message.len() <= MAX_FAILURE_BYTES);
        assert!(!message.is_empty());
        boot.atomic_write(FINALIZE_FAILURE_STAMP, "x".repeat(MAX_FAILURE_BYTES * 2))?;
        let error = boot_complete(&boot).unwrap_err().to_string();
        assert!(!error.contains(&"x".repeat(MAX_FAILURE_BYTES + 1)));
        boot_complete(&boot)?;
        Ok(())
    }
}
