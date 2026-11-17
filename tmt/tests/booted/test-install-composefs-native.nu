# number: 60
# tmt:
#   summary: Test that composefs-native images default to the composefs backend
#   duration: 45m
# extra:
#   # A UKI selects the composefs backend by itself, and a derived layer
#   # wouldn't match the composefs digest embedded in it.
#   fixme_skip_if_uki: true
#
# An image that ships /usr/lib/composefs/setup-root-conf.toml and no ostree
# prepare-root.conf must be installed with the composefs backend by
# `bootc install` run from that image without `--composefs-backend`, both
# as a self-install and with --source-imgref (as bootc-image-builder does).
# An image with both files is still installed with ostree. This runs on the
# ostree variant too, where nothing else selects composefs; on the composefs
# variant, every test's install already relies on this default.
#
# TODO: This doesn't depend on the booted host; move it into a dedicated
# install test suite: https://github.com/cgwalters-forge/tracker/issues/249

use std assert
use tap.nu
use install_testlib.nu

const NATIVE = "localhost/bootc-composefs-native"
const BOTH = "localhost/bootc-composefs-both"
const DISK = "/var/tmp/composefs-native.img"
const MNT = "/var/mnt/composefs-native"
const KEY = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITEST test@example.com"
const KEYS = "/var/tmp/composefs-native-keys"

def build [image: string, extra: string] {
    install_testlib build $image $"
RUN rm -rf /usr/lib/bootc/bound-images.d/*
RUN mkdir -p /usr/lib/composefs && touch /usr/lib/composefs/setup-root-conf.toml
($extra)
"
}

# Install `image` from itself and return the backend found on the disk
def install [image: string, ...args: string] {
    install_testlib create_disk $DISK 15G
    install_testlib install_to_disk $image $DISK ([--disable-selinux --root-ssh-authorized-keys $KEYS] | append $args)

    # Inspect the root partition of the installed disk
    let result = install_testlib with_mount $DISK $MNT {|mnt|
        let composefs = ($"($mnt)/composefs" | path exists) and ((ls $"($mnt)/state/deploy" | length) == 1)
        let ostree = ($"($mnt)/ostree/deploy" | path exists)
        let dropin = glob $"($mnt)/state/deploy/*/etc/tmpfiles.d/bootc-root-ssh.conf"
            | append (glob $"($mnt)/ostree/deploy/*/deploy/*/etc/tmpfiles.d/bootc-root-ssh.conf")
            | each {|p| {content: (open --raw $p | decode utf-8), mode: (stat -c %a $p | str trim)}}
        {composefs: $composefs, ostree: $ostree, dropin: $dropin}
    }
    let composefs = $result.composefs
    let ostree = $result.ostree
    let dropin = $result.dropin
    rm -f $DISK
    assert equal $dropin [{content: $"f~ /var/roothome/.ssh/authorized_keys 600 root root - ($KEY | encode base64)\n", mode: "644"}] "root ssh drop-in"
    match [$composefs $ostree] {
        [true false] => "composefs",
        [false true] => "ostree",
        _ => $"unexpected: composefs=($composefs) ostree=($ostree)",
    }
}

def main [] {
    tap begin "composefs-native images default to the composefs backend"

    install_testlib copy_to_storage | ignore
    $KEY | save -f $KEYS
    build $NATIVE "RUN rm -f /usr/lib/ostree/prepare-root.conf /etc/ostree/prepare-root.conf"
    # On the composefs variant, localhost/bootc is itself composefs-native: it
    # has no prepare-root.conf, and configures its composefs bootloader.
    build $BOTH "RUN printf '[composefs]\\nenabled = yes\\n' > /usr/lib/ostree/prepare-root.conf && rm -f /usr/lib/bootc/install/80-composefs-bootloader.toml"

    assert equal (install $NATIVE) "composefs" "composefs-native self-install"
    let src = $"--source-imgref=containers-storage:($NATIVE)"
    assert equal (install $NATIVE $src) "composefs" "composefs-native with --source-imgref"
    # The ostree backend needs bootupd, which images built for systemd-boot drop
    if (which bootupctl | is-not-empty) {
        assert equal (install $BOTH) "ostree" "image with both configurations"
    }

    podman rmi $NATIVE $BOTH
    rm -f $KEYS
    tap ok
}
