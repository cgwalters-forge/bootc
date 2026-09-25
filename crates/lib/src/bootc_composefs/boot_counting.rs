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

#[cfg(test)]
mod tests {
    use super::*;

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
}
