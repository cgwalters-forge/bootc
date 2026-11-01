# number: 52
# tmt:
#   summary: Test bootc install to-disk with systemd-repart partitioning
#   duration: 30m

use std assert
use tap.nu
use install_testlib.nu

let st = bootc status --json | from json

let bootloader = if ($st.status.booted.composefs? != null) {
    $st.status.booted.composefs.bootloader | str downcase
} else {
    "grub"
}

# We need this for grub and grub-cc installation as grub-cc
# installation currently is just a frankeinstined version of grub  
# install so we need BIOS partition
let bios = if $bootloader == "grub" or $bootloader == "grub-cc" {
        "
RUN <<EOF cat > /usr/lib/repart.d/00-bios.conf
[Partition]
Type=21686148-6449-6e6f-744e-656564454649
Label=BIOS-BOOT
SizeMinBytes=1M
SizeMaxBytes=1M
EOF
        "
    } else {
        ""
    }

def run_install_to_disk [
    target_image: string
    extra_bootc_args: list<string>
] {
    # Remove usr overlay state for ostree
    # We still need this even though we now have usr-overlay check in ostree branch
    # as well, because ostree checks for a file inside /run/ostree/deployment-state/...
    # for usr-overlay status
    rm -rvf /run/ostree/deployment-state

    let composefs_args = if (tap is_composefs) {
        ["--composefs-backend", "--bootloader", $bootloader]
    } else {
        []
    }

    let args = $composefs_args | append $extra_bootc_args
        | append [--disable-selinux --wipe --source-imgref $"containers-storage:($target_image)"]
    (install_testlib install_to_disk $target_image /disk.img $args
        --env-vars [BOOTC_BOOTLOADER_DEBUG=1]
        --volumes [/run/udev:/run/udev /var/disk.img:/disk.img]
        --runner {|cmd|
            # tap run_install accepts a shell command, not an argument list.
            let quoted = $cmd | each {|arg| "'" + ($arg | str replace --all "'" "'\"'\"'") + "'" }
            tap run_install ($quoted | str join ' ')
        })
}

def test_repart_full [] {
    tap begin "install with systemd-repart (ESP + root defined)"

    install_testlib copy_to_storage | ignore

    let dockerfile = $"
RUN rm -rf /etc/repart.d /usr/lib/repart.d /usr/local/lib/repart.d /run/repart.d
RUN rm -rf /usr/lib/bootc/bound-images.d/*

RUN mkdir -p /usr/lib/repart.d
($bios)
RUN <<'EOF' cat > /usr/lib/repart.d/00-esp.conf
[Partition]
Type=esp
Format=vfat
SizeMinBytes=1024M
SizeMaxBytes=1024M
EOF
RUN <<'EOF' cat > /usr/lib/repart.d/10-root.conf
[Partition]
Type=root
Format=ext4
EOF
"
    install_testlib build localhost/bootc-repart $dockerfile --uki

    install_testlib create_disk /var/disk.img
    setenforce 0

    run_install_to_disk localhost/bootc-repart []

    # since `--no-repart` wasn't passed in, sfdisk should've been used
    install_testlib with_disk /var/disk.img {|loop|
        let parts = (lsblk -J -b -o name,parttype,partuuid,size $loop | from json)
        let children = ($parts.blockdevices.0.children)
        let part_types = ($children | get parttype)

        # ESP GUID
        let esp_guid = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"
        assert ($part_types | any {|t| ($t | str downcase) == $esp_guid }) "ESP partition not found"

        let esp_mb = if (tap is_composefs) { 2048 } else { 512 }
        # Verify ESP size
        let esp_part = ($children | where {|t| ($t.parttype | str downcase) == $esp_guid } | first)
        let esp_size_bytes = ($esp_part.size | into int)
        let esp_expected = ($esp_mb * 1024 * 1024)                                                                                                                                       

        assert ($esp_size_bytes == $esp_expected) $"ESP should be ($esp_mb)M from sfdisk, got ($esp_size_bytes) bytes"                                                                   

        print "PASS: sfdisk created BIOS + ESP + root partitions"
    }

    rm -rvf /var/disk.img
    install_testlib create_disk /var/disk.img

    run_install_to_disk localhost/bootc-repart ["--run-repart"]

    # Verify partition layout
    install_testlib with_disk /var/disk.img {|loop|
        let parts = (lsblk -J -o name,parttype,partuuid $loop | from json)
        let children = ($parts.blockdevices.0.children)
        let part_types = ($children | get parttype)

        # ESP GUID
        let esp_guid = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"
        assert ($part_types | any {|t| ($t | str downcase) == $esp_guid }) "ESP partition not found"

        # Root partition (architecture-specific, just check it exists beyond ESP)
        assert (($children | length) >= 2) "Expected at least 2 partitions (ESP + root)"

        print "PASS: repart created ESP + root partitions"
    }
    rm -rvf /var/disk.img
}

def verify_part_layout_second_boot [] {
    # Verify partition layout
    install_testlib with_disk /var/disk.img {|loop|
        let parts = (lsblk -J --bytes -o name,parttype,partuuid,size $loop | from json)
        print $parts
        let children = ($parts.blockdevices.0.children)
        let part_types = ($children | get parttype)

        # ESP GUID
        let esp_guid = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"
        assert ($part_types | any {|t| ($t | str downcase) == $esp_guid }) "ESP partition not found"

        # Should have ESP + root (and Bios for grub)
        assert (($children | length) >= 2) "Expected at least 2 partitions"

        # Verify root is not using all disk space (--root-size 7G was specified)
        let root_part = ($children | last)
        let root_size_bytes = ($root_part.size | into int)
        let seven_gb = (7 * 1024 * 1024 * 1024)
        assert ($root_size_bytes <= $seven_gb) $"Root partition should be ~7G, got ($root_part.size)"

        print "PASS: repart created ESP, bootc generated root with correct size"
    }
}

def test_repart_no_root [] {
    tap begin "install with systemd-repart (ESP only, root generated by bootc)"

    # Image has ESP + home in repart.d, but no root partition definition
    let dockerfile = $"
RUN rm -rf /etc/repart.d /usr/lib/repart.d /usr/local/lib/repart.d /run/repart.d
RUN rm -rf /usr/lib/bootc/bound-images.d/*

RUN mkdir -p /usr/lib/repart.d
($bios)
RUN <<'EOF' cat > /usr/lib/repart.d/00-esp.conf
[Partition]
Type=esp
Format=vfat
SizeMinBytes=1024M
SizeMaxBytes=1024M
EOF

RUN <<'EOF' cat > /usr/lib/repart.d/20-home.conf
[Partition]
Type=home
Format=ext4
SizeMinBytes=1024M
SizeMaxBytes=1024M
EOF

RUN <<'EOF' cat > /usr/lib/repart.d/30-swap.conf
[Partition]
Type=swap
Format=swap
SizeMinBytes=1024M
SizeMaxBytes=1024M
EOF
"
    install_testlib build localhost/bootc-repart-noroot $dockerfile --uki

    install_testlib create_disk /var/disk.img
    setenforce 0

    run_install_to_disk localhost/bootc-repart-noroot ["--run-repart" "--filesystem" "ext4" "--root-size" "7G"]

    # Verify partition layout
    verify_part_layout_second_boot

    # Install again without passing in root-size make sure it works
    run_install_to_disk localhost/bootc-repart-noroot ["--run-repart" "--filesystem" "ext4"]

    # Verify partition layout (again)
    verify_part_layout_second_boot

    # Now run systemd-repart on the disk again to simulate what would happen on first boot
    print "Running systemd-repart to simulate first boot"
    (
      podman run --privileged --rm
      -v /dev:/dev
      -v /var/disk.img:/output/disk.img
      localhost/bootc-repart-noroot
      systemd-repart --dry-run=no --json=pretty --no-pager /output/disk.img
    )

    install_testlib with_disk /var/disk.img {|loop|
        let parts = (lsblk -J --bytes -o name,parttype,partuuid,size $loop | from json)
        print $parts
        let children = ($parts.blockdevices.0.children)
        let part_types = ($children | get parttype)

        # Should have ESP + root + home + swap (and Bios for grub)
        assert (($children | length) >= 4) "Expected at least 4 partitions"

        let home_guid = "933ac7e1-2eb4-4f13-b844-0e14e2aef915"
        assert ($part_types | any {|t| ($t | str downcase) == $home_guid }) "home partition not found"

        let swap_guid = "0657fd6d-a4ab-43c4-84e5-0933c84b4f4f"
        assert ($part_types | any {|t| ($t | str downcase) == $swap_guid }) "swap partition not found"

        print "PASS: repart first boot verified"
    }
    rm -rvf /var/disk.img
}

def main [] {
    test_repart_full
    test_repart_no_root
    tap ok
}
