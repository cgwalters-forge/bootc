//! Automatic boot assessment ("boot counting") for systemd-boot entries.
//!
//! See <https://uapi-group.org/specifications/specs/boot_loader_specification/#boot-counting>
//! and <https://systemd.io/AUTOMATIC_BOOT_ASSESSMENT/>.
//!
//! A counted entry is named `<name>+<tries>.conf`. On each boot attempt
//! systemd-boot renames it to `<name>+<left>-<done>.conf`, and once
//! `boot-complete.target` is reached `systemd-bless-boot.service` renames it
//! to plain `<name>.conf`. An entry whose counter reaches zero is "bad", and
//! systemd-boot sorts it after all other entries, so the next boot falls back
//! to the previous deployment.

use std::num::NonZeroU32;

use anyhow::{Context, Result};
use cap_std_ext::cap_std::fs::Dir;
use cap_std_ext::dirext::CapStdExtDirExt;

use crate::spec::Bootloader;

/// The kernel-install(8) configuration file for the number of boot attempts,
/// relative to the root. Boot counting is only enabled if it exists.
const KERNEL_TRIES_PATH: &str = "etc/kernel/tries";

/// The service that marks a booted entry as good; relative to the root.
/// Without it an entry would count down to "bad" on every boot, so boot
/// counting is only enabled for images that ship it.
const BLESS_BOOT_PATH: &str = "usr/lib/systemd/systemd-bless-boot";

const ENTRY_SUFFIX: &str = ".conf";

/// Parse the boot attempts left from a Type #1 entry file name with a boot
/// counter, like `foo+2-1.conf` (2 left, 1 done) or `foo+3.conf`. Returns
/// `None` if the entry isn't being counted.
pub(crate) fn boot_attempts_left(file_name: &str) -> Option<u32> {
    // Unlike u32::from_str, don't accept a leading '+'
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

/// Whether systemd-boot considers the entry with this file name bad, i.e. it
/// has no boot attempts left.
pub(crate) fn entry_is_bad(file_name: &str) -> bool {
    boot_attempts_left(file_name) == Some(0)
}

/// Add a boot counter with `tries` attempts to a Type #1 entry file name.
pub(crate) fn with_boot_tries(file_name: String, tries: Option<NonZeroU32>) -> String {
    match (tries, file_name.strip_suffix(ENTRY_SUFFIX)) {
        (Some(tries), Some(stem)) => format!("{stem}+{tries}{ENTRY_SUFFIX}"),
        _ => file_name,
    }
}

/// Determine how many boot attempts a new deployment's entry gets, or `None`
/// to not count its boots.
///
/// Like kernel-install, this is opt-in: the count is read from
/// `/etc/kernel/tries` of the running system, and without that file (or with
/// `0` in it) boots aren't counted. Counting is also only done for
/// systemd-boot, and only when the new image ships `systemd-bless-boot`.
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

    // Like a missing file, a broken one shouldn't block updates
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
        let cases = [
            ("bootc_fedora-42.0-1.conf", None),
            ("bootc_fedora-42.0-1+3.conf", Some(3)),
            ("bootc_fedora-42.0-1+2-1.conf", Some(2)),
            ("bootc_fedora-42.0-1+0-3.conf", Some(0)),
            ("bootc_fedora-42.0-1+0.conf", Some(0)),
            // Not a counter
            ("bootc_fedora-42.0-1+.conf", None),
            ("bootc_fedora-42.0-1+3-.conf", None),
            ("bootc_fedora-42.0-1+-3.conf", None),
            ("bootc_fedora-1.0+git-1.conf", None),
            ("bootc_fedora-42.0-1+3.efi", None),
            ("bootc_fedora-42.0-1+3", None),
        ];
        for (name, expected) in cases {
            assert_eq!(boot_attempts_left(name), expected, "{name}");
            assert_eq!(entry_is_bad(name), expected == Some(0), "{name}");
        }
    }

    #[test]
    fn test_with_boot_tries() {
        let three = NonZeroU32::new(3);
        let name = with_boot_tries("a-1.conf".into(), three);
        assert_eq!(name, "a-1+3.conf");
        assert_eq!(boot_attempts_left(&name), Some(3));
        assert_eq!(with_boot_tries("a-1.conf".into(), None), "a-1.conf");
    }

    #[test]
    fn test_boot_tries_for_deployment() -> Result<()> {
        let host = cap_std_ext::cap_tempfile::tempdir(cap_std::ambient_authority())?;
        let image = cap_std_ext::cap_tempfile::tempdir(cap_std::ambient_authority())?;

        let three = NonZeroU32::new(3);
        host.create_dir_all("etc/kernel")?;
        host.write(KERNEL_TRIES_PATH, "3\n")?;

        // No systemd-bless-boot in the image
        assert_eq!(
            boot_tries_for_deployment(&Bootloader::Systemd, &host, &image)?,
            None
        );

        image.create_dir_all("usr/lib/systemd")?;
        image.write(BLESS_BOOT_PATH, "")?;
        assert_eq!(
            boot_tries_for_deployment(&Bootloader::Systemd, &host, &image)?,
            three
        );
        // Only systemd-boot does boot counting
        for bootloader in [Bootloader::Grub, Bootloader::GrubCC, Bootloader::None] {
            assert_eq!(boot_tries_for_deployment(&bootloader, &host, &image)?, None);
        }

        // Opt-in via /etc/kernel/tries
        host.remove_file(KERNEL_TRIES_PATH)?;
        assert_eq!(
            boot_tries_for_deployment(&Bootloader::Systemd, &host, &image)?,
            None
        );

        // A malformed file is warned about and ignored
        for (contents, expected) in [("5\n", NonZeroU32::new(5)), ("0", None), ("three", None)] {
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
