# number: 55
# tmt:
#   summary: Test composefs with systemd-boot entries on an XBOOTLDR partition
#   duration: 90m
#   require:
#     - dosfstools
#     - e2fsprogs
# extra:
#   skip_if_ostree: true
#   fixme_skip_if_uki: true
#
# With an XBOOTLDR partition next to the ESP, bootc puts the Type #1 entries
# and kernels on XBOOTLDR, per the Boot Loader Specification, and only
# systemd-boot itself on the ESP.
#
# 1. `bootc install to-filesystem` onto a loop device with an ESP, an
#    XBOOTLDR partition and a root filesystem puts the entries on XBOOTLDR.
#    Booting that disk isn't possible here, so to test a booted system, the
#    VM's own ESP is then split into a smaller ESP and an XBOOTLDR partition.
#    While XBOOTLDR is still unused, bootc keeps using the ESP. Then
#    bootc's entries and kernels are moved to XBOOTLDR, as if installed there.
# 2. After a reboot, bootc uses XBOOTLDR: switch to a derived image with a
#    different initramfs, so it gets its own kernel directory.
# 3. That boots, with its entries on XBOOTLDR too: roll back.
# 4. The rollback worked: switch to another derived image that shares the
#    original kernel.
# 5. That boots, and GC removed the first derived image's kernel from
#    XBOOTLDR. Nothing of bootc's ever went to the ESP.

use std assert
use tap.nu

const XBOOTLDR_TYPE = "BC13C2FF-59E6-4262-A352-B275FD6F7172"
const ESP_TYPE = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
const LINUX_FS_TYPE = "0FC63DAF-8483-4772-8E79-3D69D8477DE4"
# The size the VM's ESP is shrunk to; XBOOTLDR gets the rest of it
const NEW_ESP_SIZE_MIB = 256
const STATE_FILE = "/var/xbootldr-test.json"
const DERIVED_IMAGE = "localhost/bootc-xbootldr-derived"
const DERIVED_IMAGE_2 = "localhost/bootc-xbootldr-derived2"
const MARKER = "/usr/share/bootc-xbootldr-marker"
const MARKER_2 = "/usr/share/bootc-xbootldr-marker2"
const SECTOR_SIZE = 512

def partitions [disk: string] {
    (sfdisk --json $disk | from json).partitiontable.partitions
}

def partition_of_type [disk: string, type: string] {
    partitions $disk | where { |p| ($p.type | str upcase) == $type } | first
}

# The path of an existing mount of `dev`, or a new one.
def find_or_mount [dev: string, name: string] {
    let existing = (findmnt -rn -o TARGET -S $dev | lines)
    if ($existing | is-not-empty) {
        return ($existing | first)
    }
    let path = $"/var/mnt/($name)"
    mkdir $path
    mount $dev $path
    $path
}

# The bootc-owned Type #1 entries and kernel directories on a partition.
def bootc_boot_files [dir: string] {
    glob $"($dir)/loader/entries*/bootc*.conf" | append (glob $"($dir)/EFI/Linux/bootc_composefs-*")
}

# Check that bootc's entries and kernels are on XBOOTLDR, and that the ESP
# only has systemd-boot.
def assert_boot_layout [xbootldr_dir: string, esp_dir: string] {
    let files = (bootc_boot_files $xbootldr_dir)
    print $"bootc files on XBOOTLDR: ($files)"
    assert ($files | any { |f| $f | str ends-with ".conf" }) "No bootc entries on XBOOTLDR"
    assert ($files | any { |f| $f | str contains "bootc_composefs-" }) "No kernels on XBOOTLDR"
    assert equal (bootc_boot_files $esp_dir) [] "bootc files on the ESP"
    assert (glob $"($esp_dir)/EFI/systemd/systemd-boot*.efi" | is-not-empty) "No systemd-boot on the ESP"
}

def kernel_dirs [dir: string] {
    glob $"($dir)/EFI/Linux/bootc_composefs-*" | each { path basename } | sort
}

def assert_booted_layout [] {
    let state = (open $STATE_FILE)
    assert_boot_layout (find_or_mount $state.xbootldr "xbootldr") (find_or_mount $state.esp "esp")
}

# Install to a loop device with an ESP, an XBOOTLDR partition and a root.
def test_install [] {
    let disk_img = "/var/tmp/xbootldr-disk.img"
    let target = "/var/mnt/target"
    truncate -s 12G $disk_img
    let loop = (losetup -f --show -P $disk_img | str trim)
    $"label: gpt\nsize=512M, type=($ESP_TYPE)\nsize=1G, type=($XBOOTLDR_TYPE)\ntype=($LINUX_FS_TYPE)\n" | sfdisk $loop
    partx -u $loop
    udevadm settle
    mkfs.vfat -F 32 -n ESP $"($loop)p1"
    mkfs.vfat -F 32 -n XBOOTLDR $"($loop)p2"
    mkfs.ext4 -q -O verity $"($loop)p3"
    # bootc gets the filesystem types from udev
    udevadm settle

    mkdir $target
    mount $"($loop)p3" $target
    mkdir $"($target)/boot"
    mount $"($loop)p2" $"($target)/boot"

    # Logically bound images would need to be pulled at install time
    bootc image copy-to-storage
    "FROM localhost/bootc\nRUN rm -rf /usr/lib/bootc/bound-images.d/*\n" | podman build -t localhost/bootc-install -f - .
    (podman run --rm --privileged --pid=host
        -v /dev:/dev -v $"($target):/target"
        --security-opt label=type:unconfined_t
        localhost/bootc-install
        bootc install to-filesystem --disable-selinux --composefs-backend --bootloader systemd /target)

    mkdir /var/mnt/target-esp
    mount $"($loop)p1" /var/mnt/target-esp
    assert_boot_layout $"($target)/boot" /var/mnt/target-esp

    umount /var/mnt/target-esp
    umount -R $target
    losetup -d $loop
    rm $disk_img
    podman rmi localhost/bootc-install
}

# Split the booted ESP into a smaller ESP and an XBOOTLDR partition, and
# move bootc's entries and kernels to XBOOTLDR.
def split_esp [] {
    let root_part = (findmnt -n -o SOURCE /sysroot | str trim)
    let disk = $"/dev/(lsblk -n -o PKNAME $root_part | str trim)"
    let esp = (partition_of_type $disk $ESP_TYPE)
    let esp_nr = ($esp.node | str replace -r '.*[^0-9]' '' | into int)
    let esp_uuid = (blkid -s UUID -o value $esp.node | str trim)
    print $"Disk ($disk), ESP ($esp.node) \(($esp.size) sectors\)"

    let backup = "/var/tmp/esp-backup"
    let esp_dir = (find_or_mount $esp.node "esp")
    /usr/bin/cp -a -T $esp_dir $backup
    # Unmount the ESP, including any automounts (which only exist for the
    # mount points the image has)
    for unit in [boot.automount efi.automount boot.mount efi.mount] {
        if (systemctl is-active --quiet $unit | complete).exit_code == 0 {
            systemctl stop $unit
        }
    }
    for t in (findmnt -rn -o TARGET -S $esp.node | lines) {
        umount $t
    }

    let new_esp_sectors = $NEW_ESP_SIZE_MIB * 1024 * 1024 / $SECTOR_SIZE
    $"start=($esp.start), size=($new_esp_sectors), type=($ESP_TYPE)\n" | sfdisk --no-reread -N $esp_nr $disk
    let old_nodes = (partitions $disk | get node)
    $"start=($esp.start + $new_esp_sectors), size=($esp.size - $new_esp_sectors), type=($XBOOTLDR_TYPE)\n" | sfdisk --no-reread --append $disk
    let xbootldr = (partitions $disk | where { |p| $p.node not-in $old_nodes } | first).node
    let xbootldr_nr = ($xbootldr | str replace -r '.*[^0-9]' '')
    partx -u --nr $esp_nr $disk
    partx -a --nr $xbootldr_nr $disk
    udevadm settle
    print $"New ESP ($esp.node), XBOOTLDR ($xbootldr)"

    # Keep the ESP's filesystem UUID, in case something refers to it
    mkfs.vfat -F 32 -n ESP -i ($esp_uuid | str replace "-" "") $esp.node
    mkfs.vfat -F 32 -n XBOOTLDR $xbootldr
    udevadm settle

    let esp_dir = (find_or_mount $esp.node "esp")
    let xbootldr_dir = (find_or_mount $xbootldr "xbootldr")
    /usr/bin/cp -r -T $backup $esp_dir
    mkdir $"($xbootldr_dir)/loader/entries" $"($xbootldr_dir)/EFI/Linux"

    # With no entry for the booted deployment on XBOOTLDR, bootc keeps using
    # the ESP, as on systems that were installed before it supported XBOOTLDR
    let out = (with-env { RUST_LOG: "bootc_lib=debug" } { bootc status | complete })
    assert equal $out.exit_code 0 $out.stderr
    assert ($out.stderr | str contains $"No entry for the booted deployment on ($xbootldr), using the ESP") $out.stderr

    for f in (glob $"($esp_dir)/loader/entries/bootc*.conf") {
        /usr/bin/mv $f $"($xbootldr_dir)/loader/entries/"
    }
    for d in (glob $"($esp_dir)/EFI/Linux/bootc_composefs-*") {
        /usr/bin/mv $d $"($xbootldr_dir)/EFI/Linux/"
    }
    rm -rf $backup
    assert_boot_layout $xbootldr_dir $esp_dir

    { esp: $esp.node, xbootldr: $xbootldr } | to json | save -f $STATE_FILE
    sync
}

def first_boot [] {
    tap begin "composefs with XBOOTLDR"

    let st = bootc status --json | from json
    let bootloader = ($st.status.booted.composefs.bootloader | str downcase)
    if $bootloader != "systemd" {
        print $"XBOOTLDR is only used with systemd-boot, not ($bootloader); skipping"
        tap ok
        return
    }

    test_install
    split_esp
    tmt-reboot
}

def second_boot [] {
    assert_booted_layout
    bootc status

    let state = (open $STATE_FILE)
    let xbootldr_dir = (find_or_mount $state.xbootldr "xbootldr")
    let original_kernels = (kernel_dirs $xbootldr_dir)
    assert equal ($original_kernels | length) 1
    $state | insert kernel $original_kernels.0 | to json | save -f $STATE_FILE

    # Zero padding at the end of the initramfs is harmless, but gives the
    # derived image a kernel directory of its own
    bootc image copy-to-storage
    let containerfile = $"FROM localhost/bootc
RUN echo xbootldr > ($MARKER) && for f in /usr/lib/modules/*/initramfs.img; do head -c 512 /dev/zero >> $f; done
"
    $containerfile | podman build -t $DERIVED_IMAGE -f - .
    bootc switch --transport containers-storage $DERIVED_IMAGE

    # The staged entries and the new kernel go to XBOOTLDR as well
    assert (glob $"($xbootldr_dir)/loader/entries.staged/*.conf" | is-not-empty) "No staged entries on XBOOTLDR"
    assert equal (kernel_dirs $xbootldr_dir | length) 2

    tmt-reboot
}

def third_boot [] {
    assert ($MARKER | path exists) "Not booted into the derived image"
    let booted = (bootc status --json | from json).status.booted.image.image.image
    assert equal $booted $DERIVED_IMAGE
    assert_booted_layout

    bootc rollback
    assert (bootc status --json | from json).status.rollbackQueued
    tmt-reboot
}

def fourth_boot [] {
    assert (not ($MARKER | path exists)) "Still booted into the derived image after rollback"
    assert_booted_layout

    $"FROM localhost/bootc\nRUN echo xbootldr > ($MARKER_2)\n" | podman build -t $DERIVED_IMAGE_2 -f - .
    bootc switch --transport containers-storage $DERIVED_IMAGE_2
    tmt-reboot
}

def fifth_boot [] {
    assert ($MARKER_2 | path exists) "Not booted into the second derived image"
    assert_booted_layout

    # Only the original kernel is still referenced; GC removed the other one
    let state = (open $STATE_FILE)
    let xbootldr_dir = (find_or_mount $state.xbootldr "xbootldr")
    assert equal (kernel_dirs $xbootldr_dir) [$state.kernel]
    tap ok
}

def main [] {
    # See https://tmt.readthedocs.io/en/stable/stories/features.html#reboot-during-test
    match $env.TMT_REBOOT_COUNT? {
        null | "0" => first_boot,
        "1" => second_boot,
        "2" => third_boot,
        "3" => fourth_boot,
        "4" => fifth_boot,
        $o => { error make { msg: $"Invalid TMT_REBOOT_COUNT ($o)" } },
    }
}
