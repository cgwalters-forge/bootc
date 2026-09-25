# number: 52
# tmt:
#   summary: Test selecting the composefs backend via the install config
#   duration: 30m
# extra:
#   # A UKI selects the composefs backend by itself, and a derived layer
#   # wouldn't match the composefs digest embedded in it.
#   fixme_skip_if_uki: true
#
# An image that sets `composefs-backend = true` in its install config must
# be installed with the composefs backend by `bootc install` run from that
# image (as bootc-image-builder does), without `--composefs-backend`.
# This runs on the ostree variant too, where it's the only thing that
# selects composefs.

use std assert
use tap.nu

const IMAGE = "localhost/bootc-composefs-config"
const DISK = "/var/tmp/composefs-config.img"
const MNT = "/var/mnt/composefs-config"

def main [] {
    tap begin "install config selects the composefs backend"

    bootc image copy-to-storage
    let td = mktemp -d
    $"FROM localhost/bootc
RUN rm -rf /usr/lib/bootc/bound-images.d/*
RUN mkdir -p /usr/lib/bootc/install && printf '[install]\\ncomposefs-backend = true\\n' > /usr/lib/bootc/install/50-composefs.toml
" | save $"($td)/Containerfile"
    # Keep an OCI manifest, see https://github.com/bootc-dev/bootc/issues/1703
    podman build --format oci -t $IMAGE $td
    rm -rf $td

    # This is what tools like bootc-image-builder read
    let config = podman run --rm $IMAGE bootc install print-configuration | from json
    assert equal ($config | get composefs-backend) true

    truncate -s 15G $DISK
    (podman run --rm --privileged --pid=host
        --security-opt label=type:unconfined_t
        -v /dev:/dev -v /var/lib/containers:/var/lib/containers -v /var/tmp:/var/tmp
        $IMAGE
        bootc install to-disk --disable-selinux --via-loopback $DISK)

    # Inspect the root partition of the installed disk
    let parts = sfdisk --json $DISK | from json | get partitiontable
    let root = $parts.partitions | where name == "root" | first
    let offset = $root.start * ($parts.sectorsize? | default 512)
    mkdir $MNT
    mount -o $"ro,loop,offset=($offset)" $DISK $MNT
    let result = try {
        assert ($"($MNT)/composefs" | path exists) "composefs repository must exist"
        let deployments = ls $"($MNT)/state/deploy"
        assert equal ($deployments | length) 1 "there must be one composefs deployment"
        assert (not ($"($MNT)/ostree/deploy" | path exists)) "no ostree deployment expected"
        null
    } catch {|err| $err }
    umount $MNT
    rm -f $DISK
    podman rmi $IMAGE
    if $result != null {
        error make { msg: $"installed disk inspection failed: ($result.msg)" }
    }

    tap ok
}
