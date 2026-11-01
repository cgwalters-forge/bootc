# number: 40
# tmt:
#   summary: Test bootc install --karg-delete
#   duration: 30m
# extra:
#   fixme_skip_if_uki: true
#
use std assert
use tap.nu
#
# Use an OS-matched target image to avoid version mismatches
let target_image = (tap get_target_image)

# setup filesystem
mkdir /var/mnt
truncate -s 10G disk.img
let composefs = (tap is_composefs)
if not $composefs {
    mkfs.ext4 disk.img
    mount -o loop disk.img /var/mnt
}

# Mask off the bootupd state to reproduce https://github.com/bootc-dev/bootc/issues/1778
# Also it turns out that installation outside of containers dies due to `error: Multiple commit objects found`
# so we mask off /sysroot/ostree
# And using systemd-run here breaks our install_t so we disable SELinux enforcement
setenforce 0

mkdir /etc/bootc/install
{ install: { kargs: ["foo=bar"] } } | to toml | save /etc/bootc/install/99-test.toml

let install_args = $"--disable-selinux --source-imgref ($target_image) --karg-delete localtestkarg --karg-delete foo"
let loop = if $composefs {
    let st = bootc status --json | from json
    let bootloader = ($st.status.booted.composefs.bootloader | str downcase)
    tap run_install $"bootc install to-disk --via-loopback --filesystem ext4 --bootloader ($bootloader) ($install_args) ./disk.img"
    let device = (losetup --find --show --partscan ./disk.img | str trim)
    udevadm settle
    let partitions = (lsblk --json --tree --paths --output PATH,PARTLABEL $device | from json | get blockdevices.0.children)
    let root = ($partitions | where partlabel == "root" | get path | first)
    let esp = ($partitions | where partlabel == "EFI-SYSTEM" | get path | first)
    mount $root /var/mnt
    mkdir /var/mnt/esp
    mount $esp /var/mnt/esp
    $device
} else {
    tap run_install $"bootc install to-filesystem --bootloader none ($install_args) /var/mnt"
    null
}

# Verify the karg is gone from the bootloader entries
let entry_paths = (glob /var/mnt/boot/loader/entries/*.conf | append (glob /var/mnt/esp/loader/entries/*.conf))
assert (not ($entry_paths | is-empty)) "No bootloader entries found"
let entries = ($entry_paths
    | each { open $in | lines }
    | flatten)

let localtestkarg_found = ($entries | find "localtestkarg" | is-empty)
assert $localtestkarg_found "Found localtestkarg in bootloader entries, but it should have been deleted"

let foo_found = ($entries | find "foo" | is-empty)
assert $foo_found "Found foo in bootloader entries, but it should have been deleted"

# Clean up
if $composefs {
    umount /var/mnt/esp
}
umount /var/mnt
if $loop != null {
    losetup -d $loop
}
rm -rf disk.img

tap ok
