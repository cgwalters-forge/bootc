use std::io::{Read, Write};

use anyhow::{Context, Result, anyhow};
use cap_std_ext::cap_std::fs::Dir;
use cap_std_ext::dirext::CapStdExtDirExt;
use fn_error_context::context;
use ocidir::cap_std::ambient_authority;
use rustix::fs::{AtFlags, RenameFlags, fsync, renameat_with};

use crate::bootc_composefs::boot::{
    BootType, FILENAME_PRIORITY_PRIMARY, FILENAME_PRIORITY_SECONDARY, primary_sort_key,
    secondary_sort_key, type1_entry_conf_file_name,
};
use crate::bootc_composefs::status::{get_composefs_status, get_sorted_type1_boot_entries};
use crate::composefs_consts::{
    COMPOSEFS_STAGED_DEPLOYMENT_FNAME, COMPOSEFS_TRANSIENT_STATE_DIR, TYPE1_ENT_PATH,
    TYPE1_ENT_PATH_STAGED,
};
use crate::deploy::ROLLBACK_JOURNAL_ID;
use crate::parsers::bls_config::BLSConfig;
use crate::spec::{Bootloader, BootloaderKind, Host};
use crate::store::{BootedComposefs, Storage};
use crate::{
    bootc_composefs::{boot::get_efi_uuid_source, status::get_sorted_grub_uki_boot_entries},
    composefs_consts::{
        BOOT_LOADER_ENTRIES, STAGED_BOOT_LOADER_ENTRIES, USER_CFG, USER_CFG_STAGED,
    },
    spec::BootOrder,
};

/// Atomically rename exchange grub user.cfg with the staged version
/// Performed as the last step in rollback/update/switch operation
#[context("Atomically exchanging user.cfg")]
pub(crate) fn rename_exchange_user_cfg(grub2_dir: &Dir) -> Result<()> {
    tracing::debug!("Atomically exchanging {USER_CFG_STAGED} and {USER_CFG}");
    renameat_with(
        &grub2_dir,
        USER_CFG_STAGED,
        &grub2_dir,
        USER_CFG,
        RenameFlags::EXCHANGE,
    )
    .context("renameat")?;

    tracing::debug!("Removing {USER_CFG_STAGED}");
    rustix::fs::unlinkat(&grub2_dir, USER_CFG_STAGED, AtFlags::empty()).context("unlinkat")?;

    tracing::debug!("Syncing to disk");
    let entries_dir = grub2_dir
        .reopen_as_ownedfd()
        .context("Reopening entries dir as owned fd")?;

    fsync(entries_dir).context("fsync entries dir")?;

    Ok(())
}

/// Atomically rename exchange "entries" <-> "entries.staged"
/// Performed as the last step in rollback/update/switch operation
///
/// `entries_dir` is the directory that contains the BLS entries directories
/// Ex: entries_dir = ESP/loader or boot/loader
#[context("Atomically exchanging BLS entries")]
pub(crate) fn rename_exchange_bls_entries(entries_dir: &Dir) -> Result<()> {
    tracing::debug!("Atomically exchanging {STAGED_BOOT_LOADER_ENTRIES} and {BOOT_LOADER_ENTRIES}");
    renameat_with(
        &entries_dir,
        STAGED_BOOT_LOADER_ENTRIES,
        &entries_dir,
        BOOT_LOADER_ENTRIES,
        RenameFlags::EXCHANGE,
    )
    .context("renameat")?;

    tracing::debug!("Removing {STAGED_BOOT_LOADER_ENTRIES}");
    entries_dir
        .remove_dir_all(STAGED_BOOT_LOADER_ENTRIES)
        .context("Removing staged dir")?;

    tracing::debug!("Syncing to disk");
    let entries_dir = entries_dir
        .reopen_as_ownedfd()
        .context("Reopening as owned fd")?;

    fsync(entries_dir).context("fsync")?;

    Ok(())
}

#[context("Rolling back Grub UKI")]
fn rollback_grub_uki_entries(boot_dir: &Dir) -> Result<()> {
    let mut str = String::new();
    let mut menuentries = get_sorted_grub_uki_boot_entries(&boot_dir, &mut str)
        .context("Getting UKI boot entries")?;

    if menuentries.len() != 2 {
        anyhow::bail!(
            "No rollback deployment available: expected two UKI entries, found {}",
            menuentries.len()
        );
    }

    let (first, second) = menuentries.split_at_mut(1);
    std::mem::swap(&mut first[0], &mut second[0]);

    let entries_dir = boot_dir.open_dir("grub2").context("Opening grub dir")?;

    entries_dir
        .atomic_replace_with(USER_CFG_STAGED, |f| -> std::io::Result<_> {
            f.write_all(get_efi_uuid_source().as_bytes())?;

            for entry in menuentries {
                f.write_all(entry.to_string().as_bytes())?;
            }

            Ok(())
        })
        .with_context(|| format!("Writing to {USER_CFG_STAGED}"))?;

    rename_exchange_user_cfg(&entries_dir)
}

#[derive(Debug)]
struct OriginalType1Entry {
    filename: String,
    contents: Vec<u8>,
    config: Option<BLSConfig>,
}

fn prepare_rollback_type1_entries(
    replacement_dir: &Dir,
    originals: &[OriginalType1Entry],
    managed_configs: &[BLSConfig],
    configs: Vec<BLSConfig>,
    os_id: &str,
) -> Result<()> {
    // Preserve unrelated regular files verbatim.
    for original in originals {
        let managed = original
            .config
            .as_ref()
            .is_some_and(|config| managed_configs.iter().any(|managed| managed == config));
        if !managed {
            replacement_dir
                .atomic_write(&original.filename, &original.contents)
                .with_context(|| format!("Preserving {}", original.filename))?;
        }
    }

    for cfg in configs {
        let priority = if cfg.sort_key == Some(secondary_sort_key(os_id)) {
            FILENAME_PRIORITY_SECONDARY
        } else {
            FILENAME_PRIORITY_PRIMARY
        };
        let file_name = type1_entry_conf_file_name(os_id, &cfg.version(), priority);
        replacement_dir
            .atomic_write(&file_name, cfg.to_string())
            .with_context(|| format!("Writing to {file_name}"))?;
    }
    Ok(())
}

/// Snapshot the entries directory before preparing its replacement.  The
/// replacement must retain files not owned by bootc byte-for-byte; parsing
/// only the `.conf` files is for ownership and selection, not for rewriting
/// unmanaged files.
fn read_original_type1_entries(boot_dir: &Dir) -> Result<Vec<OriginalType1Entry>> {
    let mut entries = Vec::new();
    for entry in boot_dir.read_dir(TYPE1_ENT_PATH)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            anyhow::bail!(
                "Cannot safely preserve non-regular entry {:?} in {}",
                entry.file_name(),
                TYPE1_ENT_PATH
            );
        }
        let filename = entry
            .file_name()
            .to_str()
            .ok_or_else(|| anyhow!("Found non UTF-8 filename in {TYPE1_ENT_PATH}"))?
            .to_owned();
        let mut contents = Vec::new();
        entry.open()?.read_to_end(&mut contents)?;
        let config = if filename.ends_with(".conf") {
            Some(crate::parsers::bls_config::parse_bls_config(
                std::str::from_utf8(&contents).context("BLS entry is not UTF-8")?,
            )?)
        } else {
            None
        };
        entries.push(OriginalType1Entry {
            filename,
            contents,
            config,
        });
    }
    Ok(entries)
}

/// Select the two bootc entries which represent the booted and rollback
/// deployments.  The bootloader directory can also contain entries belonging
/// to another OS, and composefs image IDs are not enough to infer their EROFS
/// format (both v1 and v2 IDs are valid here).
fn select_rollback_type1_entries(
    configs: Vec<BLSConfig>,
    booted_verity: &str,
    rollback_verity: &str,
) -> Result<(Vec<BLSConfig>, String)> {
    // Older composefs installations used numeric sort keys and the original
    // bootc namespace.  Keep that on-disk format readable without expanding
    // rollback into a general multi-OS feature.
    const LEGACY_OS_ID: &str = "bootc";
    if booted_verity == rollback_verity {
        anyhow::bail!("No rollback deployment available: booted and rollback deployments match");
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Role {
        Booted,
        Rollback,
    }

    let mut bootc_entries = Vec::new();
    for config in configs {
        let Some(sort_key) = config.sort_key.clone() else {
            continue;
        };
        let (candidate_os_id, priority) = match sort_key.strip_prefix("bootc-") {
            Some(rest) => {
                let Some((candidate_os_id, priority)) = rest.rsplit_once('-') else {
                    anyhow::bail!("Unsupported bootc sort-key {sort_key:?}");
                };
                if candidate_os_id.is_empty() || !matches!(priority, "0" | "1") {
                    anyhow::bail!("Unsupported bootc sort-key {sort_key:?}");
                }
                (candidate_os_id, priority)
            }
            None if matches!(sort_key.as_str(), "0" | "1") => (LEGACY_OS_ID, sort_key.as_str()),
            None => continue,
        };

        let verity = config.get_verity().with_context(|| {
            format!("Reading verity from boot entry with sort-key {sort_key:?}")
        })?;
        bootc_entries.push((
            config,
            candidate_os_id.to_string(),
            priority.to_string(),
            verity,
        ));
    }

    // The entry naming the running deployment is the authoritative namespace.
    // Do not infer an installation's OS ID from an unrelated second bootc
    // namespace merely because it happens to contain two plausible entries.
    let os_id = bootc_entries
        .iter()
        .find(|(_, _, _, verity)| verity == booted_verity)
        .map(|(_, os_id, _, _)| os_id.clone())
        .ok_or_else(|| anyhow!("No rollback deployment available"))?;

    let mut selected = Vec::new();
    for (config, candidate_os_id, priority, verity) in bootc_entries {
        if candidate_os_id != os_id {
            anyhow::bail!(
                "Unsupported bootc entry with OS ID {candidate_os_id:?}: does not match the running deployment's OS ID {os_id:?}"
            );
        }
        let role = if verity == booted_verity {
            Role::Booted
        } else if verity == rollback_verity {
            Role::Rollback
        } else {
            anyhow::bail!(
                "Unsupported bootc entry with sort-key: does not match a known deployment"
            );
        };
        selected.push((config, priority, role));
    }

    if selected.len() != 2
        || selected
            .iter()
            .filter(|(_, _, role)| *role == Role::Booted)
            .count()
            != 1
        || selected
            .iter()
            .filter(|(_, _, role)| *role == Role::Rollback)
            .count()
            != 1
        || !selected.iter().any(|(_, priority, _)| priority == "0")
        || !selected.iter().any(|(_, priority, _)| priority == "1")
    {
        anyhow::bail!(
            "No rollback deployment available: expected one booted and one rollback entry"
        );
    }

    Ok((
        selected.into_iter().map(|(config, _, _)| config).collect(),
        os_id,
    ))
}

/// Performs rollback for
/// - Grub Type1 boot entries
/// - Systemd Typ1 boot entries
/// - Systemd UKI (Type2) boot entries [since we use BLS entries for systemd boot]
///
/// Cases
/// 1. We're actually booted into the deployment that has it's sort_key as 0
///    a. Just swap the primary and secondary bootloader entries
///    b. If they're already swapped (rollback was queued), re-swap them (unqueue rollback)
///
/// 2. We're booted into the depl with sort_key 1 (choose the rollback deployment on boot screen)
///    a. Here we assume that rollback is queued as there's no way to differentiate between this
///    case and Case 1-b. This is what ostree does as well
#[context("Rolling back {bootloader} entries")]
fn rollback_composefs_entries(host: &Host, boot_dir: &Dir, bootloader: Bootloader) -> Result<()> {
    // Get all boot entries sorted in descending order by sort-key
    let originals = read_original_type1_entries(boot_dir)?;
    let all_configs = get_sorted_type1_boot_entries(&boot_dir, false)?;
    let booted = host
        .status
        .booted
        .as_ref()
        .ok_or_else(|| anyhow!("No booted deployment available"))?
        .require_composefs()?;
    let rollback = host
        .status
        .rollback
        .as_ref()
        .ok_or_else(|| anyhow!("No rollback deployment available"))?
        .require_composefs()?;
    let (mut all_configs, os_id) =
        select_rollback_type1_entries(all_configs, &booted.verity, &rollback.verity)?;
    let managed_configs = all_configs.clone();
    let generated_names = managed_configs
        .iter()
        .map(|cfg| {
            let priority = if cfg.sort_key == Some(secondary_sort_key(&os_id)) {
                FILENAME_PRIORITY_SECONDARY
            } else {
                FILENAME_PRIORITY_PRIMARY
            };
            type1_entry_conf_file_name(&os_id, &cfg.version(), priority)
        })
        .collect::<std::collections::HashSet<_>>();
    if generated_names.len() != managed_configs.len() {
        anyhow::bail!("Cannot safely prepare rollback: duplicate managed filenames");
    }
    for original in &originals {
        let managed = original
            .config
            .as_ref()
            .is_some_and(|config| managed_configs.iter().any(|managed| managed == config));
        if !managed && generated_names.contains(&original.filename) {
            anyhow::bail!(
                "Cannot safely preserve unmanaged boot entry {:?}: filename collision",
                original.filename
            );
        }
    }
    // Open this before removing any staged state.  A missing loader directory
    // is a recoverable disk-state error and must not partially cancel a
    // pending deployment.
    let dir = boot_dir.open_dir("loader").context("Opening loader dir")?;

    // This is the currently booted deployment - it should become secondary
    // OR if rollback was queued, it would become primary
    all_configs[0].sort_key = Some(primary_sort_key(&os_id));
    // This is the previous deployment - it should become primary (rollback target)
    // OR if rollback was queued, it would become secondary
    all_configs[1].sort_key = Some(secondary_sort_key(&os_id));

    // Ostree will drop any staged deployment on rollback
    // We follow the same approach for now
    //
    if let Some(staged) = &host.status.staged {
        let staged = staged.require_composefs()?;
        tracing::info!(
            message_id = ROLLBACK_JOURNAL_ID,
            "Removing currently staged composefs deployment {}",
            // SAFETY: This is a staged composefs entry, so composefs property
            // will always exist
            staged.verity
        );

        let transient_dir =
            Dir::open_ambient_dir(COMPOSEFS_TRANSIENT_STATE_DIR, ambient_authority())
                .context("Opening transient dir")?;

        if !transient_dir.exists(COMPOSEFS_STAGED_DEPLOYMENT_FNAME) {
            anyhow::bail!(
                "Staged deployment state is inconsistent: {} is missing",
                COMPOSEFS_STAGED_DEPLOYMENT_FNAME
            );
        }
    }

    // Do not alter either state representation until all entries and staged
    // state have been validated.
    boot_dir
        .remove_all_optional(TYPE1_ENT_PATH_STAGED)
        .context("Removing staged entries")?;

    if host.status.staged.is_some() {
        let transient_dir =
            Dir::open_ambient_dir(COMPOSEFS_TRANSIENT_STATE_DIR, ambient_authority())
                .context("Opening transient dir")?;
        transient_dir
            .remove_file(COMPOSEFS_STAGED_DEPLOYMENT_FNAME)
            .context("Removing staged deployment file")?;
    }

    // Write these
    boot_dir
        .create_dir_all(TYPE1_ENT_PATH_STAGED)
        .context("Creating staged dir")?;

    let rollback_entries_dir = boot_dir
        .open_dir(TYPE1_ENT_PATH_STAGED)
        .context("Opening staged entries dir")?;

    prepare_rollback_type1_entries(
        &rollback_entries_dir,
        &originals,
        &managed_configs,
        all_configs,
        &os_id,
    )?;

    let rollback_entries_dir = rollback_entries_dir
        .reopen_as_ownedfd()
        .context("Reopening as owned fd")?;

    // Should we sync after every write?
    fsync(rollback_entries_dir).context("fsync")?;

    // Atomically exchange "entries" <-> "entries.rollback"
    rename_exchange_bls_entries(&dir)
}

#[context("Rolling back composefs")]
pub(crate) async fn composefs_rollback(
    storage: &Storage,
    booted_cfs: &BootedComposefs,
) -> Result<()> {
    const COMPOSEFS_ROLLBACK_JOURNAL_ID: &str = "6f5e4d3c2b1a0f9e8d7c6b5a4e3d2c1b0";

    tracing::info!(
        message_id = COMPOSEFS_ROLLBACK_JOURNAL_ID,
        bootc.operation = "rollback",
        "Starting composefs rollback operation"
    );

    let host = get_composefs_status(storage, booted_cfs).await?;

    let new_spec = {
        let mut new_spec = host.spec.clone();
        new_spec.boot_order = new_spec.boot_order.swap();
        new_spec
    };

    // Just to be sure
    host.spec.verify_transition(&new_spec)?;

    let reverting = new_spec.boot_order == BootOrder::Default;
    if reverting {
        println!("notice: Reverting queued rollback state");
    }

    let rollback_status = host
        .status
        .rollback
        .as_ref()
        .ok_or_else(|| anyhow!("No rollback deployment available"))?;

    let Some(rollback_entry) = &rollback_status.composefs else {
        anyhow::bail!("Rollback deployment not a composefs deployment")
    };

    let boot_dir = storage.require_boot_dir()?;

    match &rollback_entry.bootloader.kind()? {
        BootloaderKind::GRUBClassic => match rollback_entry.boot_type {
            BootType::Bls => {
                rollback_composefs_entries(&host, boot_dir, rollback_entry.bootloader)?;
            }
            BootType::Uki => {
                rollback_grub_uki_entries(boot_dir)?;
            }
        },

        BootloaderKind::BLSCompatible => {
            // We use BLS entries for systemd UKI as well
            rollback_composefs_entries(&host, boot_dir, rollback_entry.bootloader)?;
        }
    }

    if reverting {
        println!("Next boot: current deployment");
    } else {
        println!("Next boot: rollback deployment");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsers::bls_config::parse_bls_config;
    use cap_std_ext::{cap_std, dirext::CapStdExtDirExt};

    fn bls(os_id: &str, version: &str, priority: &str, verity: &str) -> BLSConfig {
        parse_bls_config(&format!(
            "title {os_id} {version}\n\
             version {version}\n\
             sort-key bootc-{os_id}-{priority}\n\
             linux /boot/vmlinuz\n\
             options root=UUID=test composefs={verity}\n"
        ))
        .unwrap()
    }

    #[test]
    fn select_rollback_entries_validates_real_entry_shapes() {
        let booted = "a".repeat(128);
        let rollback = "b".repeat(128);
        let foreign = parse_bls_config(
            "title foreign\nversion 1\nsort-key foreign-os-0\nlinux /vmlinuz\noptions root=/dev/vda\n",
        )
        .unwrap();

        let cases = [
            ("zero", vec![], None),
            ("one", vec![bls("fedora", "1", "0", &booted)], None),
            (
                "two plus foreign",
                vec![
                    foreign,
                    bls("fedora", "1", "0", &booted),
                    bls("fedora", "2", "1", &rollback),
                ],
                Some("fedora"),
            ),
            (
                "three bootc entries",
                vec![
                    bls("fedora", "1", "0", &booted),
                    bls("fedora", "2", "1", &rollback),
                    bls("fedora", "3", "1", &booted),
                ],
                None,
            ),
            (
                "duplicate rollback entries",
                vec![
                    bls("fedora", "1", "0", &booted),
                    bls("fedora", "2", "1", &rollback),
                    bls("fedora", "3", "0", &rollback),
                ],
                None,
            ),
        ];

        for (name, configs, expected_os_id) in cases {
            let result = select_rollback_type1_entries(configs, &booted, &rollback);
            if let Some(expected_os_id) = expected_os_id {
                let (configs, os_id) = result.unwrap();
                assert_eq!(configs.len(), 2, "case {name}");
                assert_eq!(os_id, expected_os_id, "case {name}");
            } else {
                assert!(
                    result.unwrap_err().to_string().contains("No rollback"),
                    "case {name}"
                );
            }
        }
    }

    #[test]
    fn select_rollback_entries_rejects_malformed_bootc_sort_key() {
        let booted = "a".repeat(128);
        let rollback = "b".repeat(128);
        let malformed = parse_bls_config(&format!(
            "version 1\nsort-key bootc-fedora\nlinux /boot/vmlinuz\noptions composefs={booted}\n"
        ))
        .unwrap();

        let error = select_rollback_type1_entries(
            vec![malformed, bls("fedora", "2", "1", &rollback)],
            &booted,
            &rollback,
        )
        .unwrap_err();
        assert!(error.to_string().contains("Unsupported bootc sort-key"));
    }

    #[test]
    fn select_rollback_entries_rejects_wrong_bootc_namespace() {
        let booted = "a".repeat(128);
        let rollback = "b".repeat(128);
        let error = select_rollback_type1_entries(
            vec![
                bls("rhel", "1", "0", &booted),
                bls("fedora", "2", "1", &rollback),
            ],
            &booted,
            &rollback,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not match the running deployment")
        );
    }

    #[test]
    fn prepare_and_exchange_preserves_unmanaged_entry_bytes() -> Result<()> {
        let root = cap_std_ext::cap_tempfile::tempdir(cap_std::ambient_authority())?;
        root.create_dir_all("loader/entries")?;
        root.create_dir_all("loader/entries.staged")?;

        let booted = "a".repeat(128);
        let rollback = "b".repeat(128);
        let booted_config = bls("fedora", "1", "0", &booted);
        let rollback_config = bls("fedora", "2", "1", &rollback);
        root.atomic_write(
            "loader/entries/booted.conf",
            booted_config.to_string().as_bytes(),
        )?;
        root.atomic_write(
            "loader/entries/rollback.conf",
            rollback_config.to_string().as_bytes(),
        )?;
        let foreign = b"# Keep this comment and ordering\ntitle Other OS\nversion 1\nlinux /vmlinuz\noptions root=/dev/vda\n";
        root.atomic_write("loader/entries/other-os.conf", foreign)?;

        let originals = read_original_type1_entries(&root)?;
        let mut new_booted = booted_config.clone();
        new_booted.sort_key = Some(primary_sort_key("fedora"));
        let mut new_rollback = rollback_config.clone();
        new_rollback.sort_key = Some(secondary_sort_key("fedora"));
        let staged = root.open_dir("loader/entries.staged")?;
        prepare_rollback_type1_entries(
            &staged,
            &originals,
            &[booted_config, rollback_config],
            vec![new_booted, new_rollback],
            "fedora",
        )?;
        fsync(staged.reopen_as_ownedfd()?)?;
        rename_exchange_bls_entries(&root.open_dir("loader")?)?;

        assert_eq!(root.read("loader/entries/other-os.conf")?, foreign);
        let managed = root.open_dir("loader/entries")?;
        let booted_after = managed.read_to_string("bootc_fedora-1-1.conf")?;
        let rollback_after = managed.read_to_string("bootc_fedora-2-0.conf")?;
        assert!(booted_after.contains("sort-key bootc-fedora-0"));
        assert!(rollback_after.contains("sort-key bootc-fedora-1"));
        Ok(())
    }

    #[test]
    fn non_regular_entry_fails_before_preparation() -> Result<()> {
        let root = cap_std_ext::cap_tempfile::tempdir(cap_std::ambient_authority())?;
        root.create_dir_all("loader/entries")?;
        root.create_dir_all("loader/entries.staged")?;
        root.atomic_write("loader/entries/managed.conf", b"original")?;
        root.create_dir_all("loader/entries/foreign.conf")?;

        let error = read_original_type1_entries(&root).unwrap_err();
        assert!(error.to_string().contains("non-regular"));
        assert_eq!(root.read("loader/entries/managed.conf")?, b"original");
        Ok(())
    }
}
