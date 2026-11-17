# number: 38
# tmt:
#   summary: Test bootc install with --bootloader=none
#   duration: 30m
# extra:
#   # bootloader=none is not supported for composefs
#   fixme_skip_if_composefs: true

use std assert
use tap.nu
use install_testlib.nu

def main [] {
    tap begin "install with --bootloader=none"

    # Copy the booted image to container storage for use as install source
    let target_image = install_testlib copy_to_storage

    install_testlib create_disk disk.img

    setenforce 0

    tap run_install $"bootc install to-disk --disable-selinux --via-loopback --filesystem xfs --bootloader=none --source-imgref ($target_image) ./disk.img"

    rm -f disk.img

    tap ok
}
