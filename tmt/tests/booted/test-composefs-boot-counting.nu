# number: 53
# extra:
#   try_bind_storage: true
#   skip_if_ostree: true
# tmt:
#   summary: Test composefs boot counting with systemd-boot
#   duration: 30m
#   adjust:
#     - when: distro == centos-9 and boot_type == uki and seal_state == sealed
#       enabled: false
#       because: CentOS 9 cannot consume the signed host upgrade because shared storage is unavailable and its guest-local UKI builder produces unsigned images
#
# Verify automatic boot assessment for composefs deployments with systemd-boot:
# 1. With /etc/kernel/tries, a staged deployment's boot entry gets a "+3" counter
# 2. After booting it, systemd-bless-boot marks the entry good
# 3. An entry that ran out of boot attempts sorts last, so status reports
#    a rollback as queued, and systemd-boot boots the previous deployment

use std assert
use tap.nu

bootc status
journalctl --list-boots

let st = bootc status --json | from json
let booted = $st.status.booted.image
let bootloader = ($st.status.booted.composefs.bootloader | str downcase)

const ESP = "/var/tmp/efi"
const TRIES = 3

def imgsrc [] {
    $env.BOOTC_upgrade_image? | default "localhost/bootc-derived-local"
}

def mount_esp [] {
    mkdir $ESP
    if (do -i { findmnt $ESP } | complete).exit_code != 0 {
        mount /dev/disk/by-partlabel/EFI-SYSTEM $ESP
    }
}

# The .conf files in an entries directory, with their contents
def entries [dir: string] {
    glob $"($ESP)/loader/($dir)/*.conf" | each { |p|
        { name: ($p | path basename), path: $p, contents: (open --raw $p) }
    }
}

# The entry booting a deployment: its composefs karg (Type #1) or UKI path.
# A Type #1 entry may reuse another deployment's kernel, so ignore
# `/bootc_composefs-<verity>/vmlinuz` paths.
def entry_for [dir: string, verity: string] {
    let found = entries $dir | where { |e| $e.contents =~ $"($verity)\(\\s|$|\\.efi\)" }
    assert equal ($found | length) 1 $"expected one entry in ($dir) for ($verity): ($found)"
    $found | first
}

def first_boot [] {
    tap begin "composefs boot counting"

    let imgsrc = imgsrc
    if ($imgsrc | str ends-with "-local") {
        bootc image copy-to-storage
        (
            tap make_uki_containerfile "
                FROM localhost/bootc as base
                RUN touch /usr/share/testing-bootc-boot-counting
            "
        ) | save Dockerfile
        podman build -t $imgsrc .
    }

    $st.status.booted.composefs.verity | save /var/original-verity

    # Boot counting is opt-in, like with kernel-install
    mkdir /etc/kernel
    $"($TRIES)\n" | save -f /etc/kernel/tries

    # The Fedora and CentOS SELinux policies don't let systemd-bless-boot
    # (init_t) rename entries on the ESP.
    "(allow init_t dosfs_t (file (rename)))" | save -f /var/tmp/bless-boot.cil
    semodule -i /var/tmp/bless-boot.cil

    bootc switch --transport containers-storage $imgsrc
    let staged = (bootc status --json | from json).status.staged.composefs.verity

    mount_esp
    let staged_entry = entry_for "entries.staged" $staged
    print $"staged entry: ($staged_entry.name)"
    assert ($staged_entry.name | str ends-with $"+($TRIES).conf") "new deployment's entry should have a boot counter"

    # The currently booted deployment is known good, so it isn't counted
    let booted_entry = entry_for "entries.staged" $st.status.booted.composefs.verity
    assert (not ($booted_entry.name | str contains "+")) $"booted entry should not be counted: ($booted_entry.name)"

    tmt-reboot
}

def second_boot [] {
    assert equal $booted.image.image (imgsrc)

    # systemd-bless-boot runs after boot-complete.target; wait for it.
    let blessed = (seq 1 60 | each { |_|
        if (systemctl is-active systemd-bless-boot.service | complete).stdout =~ "^active" {
            true
        } else {
            sleep 2sec
            false
        }
    } | any { |x| $x })
    if not $blessed {
        systemctl status systemd-bless-boot.service
        journalctl -b -u systemd-bless-boot.service
        error make { msg: "systemd-bless-boot.service did not succeed" }
    }

    mount_esp
    let verity = $st.status.booted.composefs.verity
    let entry = entry_for "entries" $verity
    print $"booted entry: ($entry.name)"
    assert (not ($entry.name | str contains "+")) $"booted entry should be blessed: ($entry.name)"

    # Simulate a deployment that failed to boot three times
    let bad_name = ($entry.name | str replace ".conf" $"+0-($TRIES).conf")
    mv $entry.path $"($ESP)/loader/entries/($bad_name)"
    sync

    # systemd-boot will skip the bad entry, and status must say so
    let st = bootc status --json | from json
    assert equal $st.status.rollbackQueued true
    assert equal $st.status.rollback.composefs.verity (open /var/original-verity)

    tmt-reboot
}

def third_boot [] {
    let original = open /var/original-verity
    assert equal $st.status.booted.composefs.verity $original "systemd-boot should fall back to the previous deployment"
    assert equal $st.status.rollbackQueued false
    assert equal $st.status.rollback.image.image.image (imgsrc)

    tap ok
}

def main [] {
    if $bootloader != "systemd" {
        print $"Boot counting is only implemented for systemd-boot, not ($bootloader)"
        return
    }

    match $env.TMT_REBOOT_COUNT? {
        null | "0" => first_boot,
        "1" => second_boot,
        "2" => third_boot,
        $o => { error make { msg: $"Invalid TMT_REBOOT_COUNT ($o)" } },
    }
}
