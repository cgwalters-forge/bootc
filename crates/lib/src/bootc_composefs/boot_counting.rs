//! Opt-in systemd-boot counting for staged composefs deployments.
//!
//! A counted entry is named `<name>+<tries>.conf`. systemd-boot renames it to
//! `<name>+<left>-<done>.conf` on each attempt; systemd-bless-boot removes the
//! counter after successful boot assessment. Exhausted entries sort last.
//! See <https://systemd.io/AUTOMATIC_BOOT_ASSESSMENT/>.

use std::num::NonZeroU32;

use anyhow::{Context, Result};
use cap_std_ext::cap_std::fs::Dir;
use cap_std_ext::dirext::CapStdExtDirExt;

use crate::spec::Bootloader;

const KERNEL_TRIES_PATH: &str = "etc/kernel/tries";

const BLESS_BOOT_PATH: &str = "usr/lib/systemd/systemd-bless-boot";

const ENTRY_SUFFIX: &str = ".conf";

/// Parse the remaining attempts in a Type #1 entry name, including UKI wrappers.
pub(crate) fn boot_attempts_left(file_name: &str) -> Option<u32> {
    let parse = |s: &str| -> Option<u32> {
        if s.bytes().all(|b| b.is_ascii_digit()) {
            s.parse().ok()
        } else {
            None
        }
    };
    let stem = file_name.strip_suffix(ENTRY_SUFFIX)?;
    let (_, counter) = stem.rsplit_once('+')?;
    let left = match counter.split_once('-') {
        Some((left, done)) => {
            parse(done)?;
            left
        }
        None => counter,
    };
    parse(left)
}

pub(crate) fn entry_is_bad(file_name: &str) -> bool {
    boot_attempts_left(file_name) == Some(0)
}

/// Add attempts to a Type #1 wrapper, never to the referenced UKI binary.
pub(crate) fn with_boot_tries(file_name: String, tries: Option<NonZeroU32>) -> String {
    match (tries, file_name.strip_suffix(ENTRY_SUFFIX)) {
        (Some(tries), Some(stem)) => format!("{stem}+{tries}{ENTRY_SUFFIX}"),
        _ => file_name,
    }
}

/// Prevent an unbootable staged deployment from remaining the default forever.
/// Opt in using the running host's /etc/kernel/tries; never count GRUB or an
/// image without the blessing executable, which would always exhaust attempts.
#[fn_error_context::context("Determining boot counting tries")]
pub(crate) fn boot_tries_for_deployment(
    bootloader: &Bootloader,
    host_root: &Dir,
    new_root: &Dir,
) -> Result<Option<NonZeroU32>> {
    if !matches!(bootloader, Bootloader::Systemd) {
        return Ok(None);
    }
    let Some(contents) = host_root
        .read_to_string_optional(KERNEL_TRIES_PATH)
        .with_context(|| format!("Reading /{KERNEL_TRIES_PATH}"))?
    else {
        return Ok(None);
    };
    // A malformed file should not block updates.
    let Ok(tries) = contents.trim().parse::<u32>() else {
        tracing::warn!(
            "Not counting boots: /{KERNEL_TRIES_PATH} should hold a number of boot attempts, found {:?}",
            contents.trim()
        );
        return Ok(None);
    };
    if !new_root
        .try_exists(BLESS_BOOT_PATH)
        .with_context(|| format!("Querying {BLESS_BOOT_PATH}"))?
    {
        tracing::warn!(
            "Not counting boots despite /{KERNEL_TRIES_PATH}: the new image lacks /{BLESS_BOOT_PATH}"
        );
        return Ok(None);
    }
    Ok(NonZeroU32::new(tries))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cap_std_ext::cap_std;

    #[test]
    fn test_boot_attempts_left() {
        for (name, expected) in [
            ("bootc_fedora-42-1.conf", None),
            ("bootc_fedora-42-1+3.conf", Some(3)),
            ("bootc_fedora-42-1+2-1.conf", Some(2)),
            ("bootc_fedora-42-1+0-3.conf", Some(0)),
            ("bootc_fedora-42-1+0.conf", Some(0)),
            ("entry+4294967295-0.conf", Some(u32::MAX)),
            ("entry+4294967296.conf", None),
            ("entry+0-4294967296.conf", None),
            ("entry+.conf", None),
            ("entry+3-.conf", None),
            ("entry+-3.conf", None),
            ("entry+0-1-2.conf", None),
            ("entry+0-x.conf", None),
            ("entry+ 0.conf", None),
            ("bootc_fedora-1.0+git-1.conf", None),
            ("entry+3.efi", None),
            ("entry+3", None),
        ] {
            assert_eq!(boot_attempts_left(name), expected, "{name}");
            assert_eq!(entry_is_bad(name), expected == Some(0), "{name}");
        }
    }

    #[test]
    fn test_with_boot_tries() {
        assert_eq!(
            with_boot_tries("a-1.conf".into(), NonZeroU32::new(3)),
            "a-1+3.conf"
        );
        assert_eq!(with_boot_tries("a-1.conf".into(), None), "a-1.conf");
        assert_eq!(with_boot_tries("a.efi".into(), NonZeroU32::new(3)), "a.efi");
    }

    #[test]
    fn test_boot_tries_for_deployment() -> Result<()> {
        let host = cap_std_ext::cap_tempfile::tempdir(cap_std::ambient_authority())?;
        let image = cap_std_ext::cap_tempfile::tempdir(cap_std::ambient_authority())?;
        host.create_dir_all("etc/kernel")?;
        host.write(KERNEL_TRIES_PATH, "3\n")?;
        assert_eq!(
            boot_tries_for_deployment(&Bootloader::Systemd, &host, &image)?,
            None
        );
        image.create_dir_all("usr/lib/systemd")?;
        image.write(BLESS_BOOT_PATH, "")?;
        assert_eq!(
            boot_tries_for_deployment(&Bootloader::Systemd, &host, &image)?,
            NonZeroU32::new(3)
        );
        for bootloader in [
            Bootloader::Grub,
            Bootloader::GrubCC,
            Bootloader::None,
            Bootloader::Ukiboot,
        ] {
            assert_eq!(boot_tries_for_deployment(&bootloader, &host, &image)?, None);
        }
        host.remove_file(KERNEL_TRIES_PATH)?;
        assert_eq!(
            boot_tries_for_deployment(&Bootloader::Systemd, &host, &image)?,
            None
        );
        for (contents, expected) in [
            ("5\n", NonZeroU32::new(5)),
            ("0", None),
            ("three", None),
            ("", None),
            ("-1", None),
            ("4294967296", None),
        ] {
            host.write(KERNEL_TRIES_PATH, contents)?;
            assert_eq!(
                boot_tries_for_deployment(&Bootloader::Systemd, &host, &image)?,
                expected,
                "{contents:?}"
            );
        }
        Ok(())
    }
}
