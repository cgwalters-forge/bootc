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
# Reuses #25's healthy blessing and simulated exhaustion test for both traditional
# BLS and UKI Type-1 wrappers. This does not inject assessment failures: combined
# failed-finalization/prevented-blessing validation with #122 is a separate gate.

use std assert
use tap.nu

bootc status
journalctl --list-boots

let st = bootc status --json | from json
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

def entries [dir: string] {
    glob $"($ESP)/loader/($dir)/*.conf" | each { |p|
        { name: ($p | path basename), path: $p, contents: (open --raw $p) }
    }
}

# Match the root karg (including format-prefixed composefs.digest=) or UKI
# path, not a possibly shared kernel/initrd directory.
def entry_for [dir: string, verity: string] {
    let found = entries $dir | where { |e| $e.contents =~ $"($verity)\(\\s|$|\\.efi\)" }
    assert equal ($found | length) 1 $"expected one entry in ($dir) for ($verity): ($found)"
    $found | first
}

def assert_status [booted: string, rollback: string, queued: bool] {
    let status = (bootc status --json | from json).status
    assert equal $status.booted.composefs.verity $booted
    assert equal $status.rollback.composefs.verity $rollback
    assert equal $status.rollbackQueued $queued
}

# GC must retain both deployments and the artifacts referenced through renamed
# entries. In the UKI case the .efi binary itself must never be renamed.
def check_gc [booted: string, rollback: string, queued: bool] {
    let before = entries "entries" | sort-by name
    bootc internals composefs-gc
    assert equal (entries "entries" | sort-by name) $before
    assert_status $booted $rollback $queued
    for entry in $before {
        let artifacts = ($entry.contents | lines | where { |line| $line =~ '^(linux|initrd|efi|uki)\s' })
        assert (($artifacts | length) > 0)
        for line in $artifacts {
            let path = ($line | split row --regex '\s+' | get 1 | str trim --left --char '/')
            assert ($"($ESP)/($path)" | path exists) $"GC removed boot artifact ($path)"
        }
    }
}

def first_boot [] {
    tap begin "composefs boot counting"
    let imgsrc = imgsrc
    if ($imgsrc | str ends-with "-local") {
        bootc image copy-to-storage
        (tap make_uki_containerfile "
            FROM localhost/bootc as base
            RUN touch /usr/share/testing-bootc-boot-counting
        ") | save Dockerfile
        podman build -t $imgsrc .
    }
    $st.status.booted.composefs.verity | save /var/original-verity

    # Test-guest-only opt-in and SELinux workaround; not stock-policy proof.
    mkdir /etc/kernel
    $"($TRIES)\n" | save -f /etc/kernel/tries
    "(allow init_t dosfs_t (file (rename)))" | save -f /var/tmp/bless-boot.cil
    semodule -i /var/tmp/bless-boot.cil

    bootc switch --transport containers-storage $imgsrc
    let staged = (bootc status --json | from json).status.staged.composefs.verity
    $staged | save /var/counted-verity
    mount_esp
    let staged_entry = entry_for "entries.staged" $staged
    assert ($staged_entry.name | str ends-with $"+($TRIES).conf")
    let booted_entry = entry_for "entries.staged" $st.status.booted.composefs.verity
    assert (not ($booted_entry.name | str contains "+"))
    tmt-reboot
}

def second_boot [] {
    assert equal $st.status.booted.image.image.image (imgsrc)
    # Bound the wait at 120s and stop immediately after success.
    mut blessed = false
    for attempt in 1..60 {
        if (systemctl is-active systemd-bless-boot.service | complete).stdout =~ '^active' {
            $blessed = true
            break
        }
        sleep 2sec
    }
    if not $blessed {
        journalctl -b -u systemd-bless-boot.service
        error make { msg: "systemd-bless-boot.service did not succeed within 120s" }
    }
    mount_esp
    let verity = open --raw /var/counted-verity
    let original = open --raw /var/original-verity
    let entry = entry_for "entries" $verity
    assert (not ($entry.name | str contains "+")) "healthy entry was not blessed"
    assert_status $verity $original false

    # Exercise the bootloader's renamed +N-M form while attempts remain.
    let pending = ($entry.name | str replace ".conf" "+2-1.conf")
    mv $entry.path $"($ESP)/loader/entries/($pending)"
    check_gc $verity $original false

    # Simulate exhaustion, not three real failed boot attempts. The next reboot
    # must prove systemd-boot's fallback, independently of status ordering.
    let exhausted = ($entry.name | str replace ".conf" $"+0-($TRIES).conf")
    mv $"($ESP)/loader/entries/($pending)" $"($ESP)/loader/entries/($exhausted)"
    sync
    check_gc $verity $original true
    tmt-reboot
}

def third_boot [] {
    mount_esp
    let original = open --raw /var/original-verity
    let counted = open --raw /var/counted-verity
    assert_status $original $counted false
    let failed = entry_for "entries" $counted
    assert ($failed.name | str ends-with $"+0-($TRIES).conf")
    check_gc $original $counted false

    # Existing explicit rollback rewrites both entries without counters. It
    # must select the exhausted deployment, not add a fresh retry policy.
    bootc rollback
    assert_status $original $counted true
    assert (entries "entries" | all { |e| not ($e.name | str contains "+") })
    check_gc $original $counted true
    tmt-reboot
}

def fourth_boot [] {
    mount_esp
    let counted = open --raw /var/counted-verity
    let original = open --raw /var/original-verity
    assert_status $counted $original false
    check_gc $counted $original false
    # Queue and cancel an explicit rollback after the renames.
    bootc rollback
    assert_status $counted $original true
    bootc rollback
    assert_status $counted $original false
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
        "3" => fourth_boot,
        $o => { error make { msg: $"Invalid TMT_REBOOT_COUNT ($o)" } },
    }
}
