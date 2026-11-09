# number: 40
# tmt:
#   summary: Test bootc install --karg-delete
#   duration: 30m
#   adjust:
#     - when: boot_type == uki or boot_type == aboot
#       enabled: false
# extra:
#   fixme_skip_if_uki: true
#
use std assert
use tap.nu
#
# Keep the selected bootloader's payload and configuration from the test image.
bootc image copy-to-storage
let composefs = (tap is_composefs)
if $composefs {
    let st = bootc status --json | from json
    assert equal ($st.status.booted.composefs.bootType | str downcase) "bls"
}

# Track resources outside the try block so failures during setup or assertions
# take the same cleanup path as a successful install.
let workdir = (mktemp -d | str trim)
let disk = $"($workdir)/disk.img"
let image = $"localhost/bootc-karg-delete:($workdir | path basename | str downcase)"
let config = "/etc/bootc/install/99-test-karg-delete.toml"
assert (not ($config | path exists))
mut loop_device = null
mkdir /var/mnt
let failure = try {
    # Both deletion sources must remove an argument that really exists in the
    # image; a missing fixture would otherwise make the negative checks pass.
    { kargs: ["gapdel=1", "gapclidel=1"] } | to toml | save $"($workdir)/kargs.toml"
    $"FROM localhost/bootc
COPY kargs.toml /usr/lib/bootc/kargs.d/99-install-delete.toml
" | save $"($workdir)/Containerfile"
    podman build -t $image $workdir
    let image_kargs = (podman run --rm $image cat /usr/lib/bootc/kargs.d/99-install-delete.toml | from toml | get kargs)
    assert equal $image_kargs ["gapdel=1", "gapclidel=1"]
    # Host-side containers-storage imports require a writable root. Use a
    # writable OCI layout instead, as in test-install-outside-container.
    skopeo copy $"containers-storage:($image)" $"oci:($workdir)/image"

    # run_install isolates bootupd/ostree mounts. Disable enforcement for its
    # transient systemd unit, as in the other outside-container install tests.
    setenforce 0
    mkdir /etc/bootc/install
    { install: { kargs: ["gapcfg=1", "foo=bar"], karg-deletes: ["gapdel=1"] } } | to toml | save $config

    truncate -s 10G $disk
    let backend = if $composefs { "--composefs-backend" } else { "" }
    let install_args = $"($backend) --disable-selinux --source-imgref oci:($workdir)/image --target-imgref ($image) --skip-fetch-check --karg-delete gapclidel=1 --karg-delete foo"
    if $composefs {
        let st = bootc status --json | from json
        let bootloader = ($st.status.booted.composefs.bootloader | str downcase)
        tap run_install $"bootc install to-disk --via-loopback --filesystem ext4 --bootloader ($bootloader) ($install_args) ($disk)"
        $loop_device = (losetup --find --show --partscan $disk | str trim)
        udevadm settle
        let partitions = (lsblk --json --tree --paths --output PATH,PARTLABEL $loop_device | from json | get blockdevices.0.children)
        let root = ($partitions | where partlabel == "root" | get path | first)
        let esp = ($partitions | where partlabel == "EFI-SYSTEM" | get path | first)
        mount $root /var/mnt
        mkdir /var/mnt/esp
        mount $esp /var/mnt/esp
    } else {
        mkfs.ext4 $disk
        $loop_device = (losetup --find --show $disk | str trim)
        mount $loop_device /var/mnt
        tap run_install $"bootc install to-filesystem --bootloader none ($install_args) /var/mnt"
    }

    let entry_paths = (glob /var/mnt/boot/loader/entries/*.conf | append (glob /var/mnt/esp/loader/entries/*.conf))
    assert (not ($entry_paths | is-empty)) "No bootloader entries found"
    for entry in $entry_paths {
        let options = (open $entry | lines | where { $in | str starts-with "options " })
        assert equal ($options | length) 1
        # Fixture arguments have no quoting or whitespace. Check whole options
        # tokens, never substrings in titles, paths, or other BLS fields.
        let tokens = ($options | first | split row -r '\s+' | skip 1)
        assert ("gapcfg=1" in $tokens) $"Missing install-config karg in ($entry)"
        for deleted in ["gapdel=1", "gapclidel=1", "foo=bar"] {
            assert (not ($deleted in $tokens)) $"Found deleted karg ($deleted) in ($entry)"
        }
        let backend_keys = if $composefs { ["composefs=", "composefs.digest="] } else { ["ostree="] }
        assert ($tokens | any {|token| $backend_keys | any {|key| $token | str starts-with $key } }) $"Wrong install backend in ($entry)"
    }
    null
} catch {|err| $err }

# Unmount children first, including when installation or mounting failed halfway.
# As on upstream main, lazily detach mounts held by transient installer units.
# Attempt independent cleanup even after a teardown error, without masking the
# original failure or deleting a disk that is still attached to a loop device.
def cleanup [action: closure] {
    try { do $action; null } catch {|err| $err }
}
let device = $loop_device
let cleanup_errors = ([
    (cleanup {
        if (mountpoint -q /var/mnt/esp | complete).exit_code == 0 {
            umount -l /var/mnt/esp
        }
    })
    (cleanup {
        if (mountpoint -q /var/mnt | complete).exit_code == 0 {
            umount -l /var/mnt
        }
    })
    (cleanup {
        if $device != null {
            losetup -d $device
        }
    })
    (cleanup {
        if ($config | path exists) {
            rm $config
        }
    })
    (cleanup {
        if (podman image exists $image | complete).exit_code == 0 {
            podman image rm $image
        }
    })
    (cleanup {
        assert (losetup -j $disk | str trim | is-empty) "Disk still has an attached loop device"
        rm -rf $workdir
    })
] | compact)
for err in $cleanup_errors {
    print -e $"Cleanup failed: ($err.msg)"
}
if $failure != null {
    error make $failure
}
if not ($cleanup_errors | is-empty) {
    error make { msg: "Install test cleanup failed" }
}

tap ok
