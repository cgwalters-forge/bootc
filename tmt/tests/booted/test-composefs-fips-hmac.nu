# number: 64
# tmt:
#   summary: Boot a composefs Type1 deployment with fips=1, checking the installed kernel HMAC
#   duration: 45m
# extra:
#   skip_if_ostree: true
#   fixme_skip_if_uki: true
#
# dracut's fips module finds the HMAC from BOOT_IMAGE, which only GRUB sets,
# and mounts boot= at /boot, so this covers GRUB with /boot on the root
# filesystem; other setups skip.

use std assert
use tap.nu

const hmac_name = ".vmlinuz.hmac"

let kver = (ls /usr/lib/modules | get name | first | path basename)

# Returns the boot directory of the booted kernel, from BOOT_IMAGE
def booted_boot_dir [] {
    let boot_image = (open /proc/cmdline | str trim | split row " "
        | where { $in starts-with "BOOT_IMAGE=" }
        | first
        | str replace "BOOT_IMAGE=" ""
        | str replace --regex '^\(.*\)' "")
    $"/sysroot($boot_image | path dirname)"
}

def assert_hmac_installed [] {
    let hmac = (open --raw $"(booted_boot_dir)/($hmac_name)" | str trim)
    assert ($hmac | str ends-with "  vmlinuz") $"Unexpected HMAC: ($hmac)"
}

def first_boot [] {
    tap begin "Boot with fips=1 using the installed kernel HMAC"

    if not ($"/usr/lib/modules/($kver)/($hmac_name)" | path exists) {
        print "Kernel does not ship an HMAC; skipping"
        tap ok
        return
    }
    # dracut's fips module checks the HMAC with this, if the image has it
    if (which sha512hmac | is-empty) {
        print "No sha512hmac in the image; skipping"
        tap ok
        return
    }
    let bootloader = ((bootc status --json | from json).status.booted.composefs.bootloader | str downcase)
    if $bootloader != "grub" {
        print $"No BOOT_IMAGE with bootloader ($bootloader); skipping"
        tap ok
        return
    }
    let root_uuid = (findmnt -n -o UUID /sysroot | str trim)
    let boot_mount = (do { findmnt -n -o UUID --mountpoint /boot } | complete)
    if $boot_mount.exit_code == 0 and ($boot_mount.stdout | str trim) != $root_uuid {
        print "Separate /boot partition; skipping"
        tap ok
        return
    }
    if not ($"(booted_boot_dir)/vmlinuz" | path exists) {
        print "Kernel is not under /boot on the root filesystem; skipping"
        tap ok
        return
    }

    assert_hmac_installed

    bootc image copy-to-storage
    echo $"
        FROM localhost/bootc
        RUN dracut -vf --add fips /usr/lib/modules/($kver)/initramfs.img ($kver)
        RUN echo 'kargs = [\"fips=1\", \"boot=UUID=($root_uuid)\"]' > /usr/lib/bootc/kargs.d/90-fips.toml
    " | podman build -t localhost/bootc-fips . -f -

    bootc switch --transport containers-storage localhost/bootc-fips
    tmt-reboot
}

# Reaching userspace means dracut's fips module verified the kernel's HMAC
def second_boot [] {
    assert ((open /proc/cmdline | split row " ") | any { $in == "fips=1" })
    assert equal (open /proc/sys/crypto/fips_enabled | str trim) "1"
    assert_hmac_installed
    tap ok
}

def main [] {
    match $env.TMT_REBOOT_COUNT? {
        null | "0" => first_boot,
        "1" => second_boot,
        $o => { error make { msg: $"Invalid TMT_REBOOT_COUNT ($o)" } },
    }
}
