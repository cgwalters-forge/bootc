# number: 54
# tmt:
#   summary: Reject install to-existing-root --composefs-backend on an ostree host
#   duration: 30m
#
# Moving an ostree system to the composefs backend in place is not supported
# yet (https://github.com/bootc-dev/bootc/issues/2079). Until it is, verify that
# `bootc install to-existing-root --composefs-backend` fails with a clear error
# before touching /boot or the ESP, and that the host still boots into the
# same ostree deployment afterwards.
use std assert
use tap.nu

const target_image = "localhost/bootc"
const expected_error = "onto an existing ostree-based system is not supported yet; no changes were made"
const boot_listing = "/var/tmp/composefs-on-ostree-boot-listing"
const booted_digest = "/var/tmp/composefs-on-ostree-booted-digest"

# This covers ostree hosts only; composefs hosts install alongside composefs.
if (tap is_composefs) {
    print "Booted via composefs, skipping"
    exit 0
}

# Everything under /boot (and the ESP, if mounted below it) with sizes and
# mtimes, so that any removal or rewrite by the installer shows up.
def list_boot [] {
    ^find /boot -printf "%p %y %s %T@\n" | lines | sort | str join "\n"
}

def booted_image_digest [] {
    (bootc status --json | from json).status.booted.image.imageDigest
}

def first_boot [] {
    tap begin "install to-existing-root --composefs-backend on ostree"

    assert ("/run/ostree-booted" | path exists) "expected an ostree-booted host"
    bootc image copy-to-storage

    list_boot | save -f $boot_listing
    booted_image_digest | save -f $booted_digest

    let r = (podman run
        --rm
        --privileged
        -v /:/target
        -v /dev:/dev
        -v /var/lib/containers:/var/lib/containers
        -v /usr/share/empty:/usr/lib/bootc/bound-images.d
        --pid=host
        --security-opt label=type:unconfined_t
        $target_image
        bootc install to-existing-root
            --composefs-backend
            --acknowledge-destructive
            --skip-fetch-check
            --disable-selinux
            /target
        | complete)
    print $r.stdout $r.stderr

    assert ($r.exit_code != 0) "install should have failed"
    assert ($r.stderr | str contains $expected_error) $"unexpected error: ($r.stderr)"
    assert ($r.stderr | str contains "https://github.com/bootc-dev/bootc/issues/2079")
    assert equal (list_boot) (open $boot_listing) "/boot must be unchanged"

    tmt-reboot
}

def second_boot [] {
    assert ("/run/ostree-booted" | path exists) "should still boot via ostree"
    assert (not (tap is_composefs)) "should not be booted via composefs"
    assert equal (booted_image_digest) (open $booted_digest) "should boot the same deployment"
    tap ok
}

def main [] {
    match $env.TMT_REBOOT_COUNT? {
        null | "0" => first_boot,
        "1" => second_boot,
        $o => { error make { msg: $"Invalid TMT_REBOOT_COUNT ($o)" } },
    }
}
