# number: 40
# tmt:
#   summary: Reject install-time karg settings for UKI and aboot images
#   duration: 30m
#   adjust:
#     - when: boot_type != uki and boot_type != aboot
#       enabled: false
# extra:
#   skip_if_ostree: true

use std assert
use tap.nu

let st = bootc status --json | from json
assert (($st.status.booted.composefs.bootType | str downcase) in ["uki", "aboot"])

# Install the original image, not a derived layer that would invalidate its
# embedded composefs digest. Bind only the install config into the installer.
bootc image copy-to-storage
let workdir = (mktemp -d | str trim)
let disk = $"($workdir)/disk.img"
let config = $"($workdir)/install.toml"
let failure = try {
    for setting in ["--karg", "--karg-delete", "install.kargs", "install.karg-deletes"] {
        mut install = { kargs: [], karg-deletes: [] }
        mut args = []
        if ($setting | str starts-with "--") {
            $args = [$"($setting)=quiet"]
        } else {
            $install = ($install | upsert ($setting | str replace 'install.' '') ["quiet"])
        }
        { install: $install } | to toml | save -f $config
        truncate -s 15G $disk
        let cli_args = $args
        let result = (do {
            (podman run --rm --privileged --pid=host
                --security-opt label=type:unconfined_t
                -v /dev:/dev -v /var/lib/containers:/var/lib/containers
                -v $"($workdir):/out"
                -v $"($config):/etc/bootc/install/99-test-immutable-kargs.toml:ro"
                localhost/bootc bootc install to-disk --disable-selinux
                --via-loopback --filesystem ext4 --generic-image --wipe
                --composefs-backend --target-transport containers-storage
                ...$cli_args /out/disk.img)
        } | complete)
        # Match the setting-specific error, not an unrelated install failure.
        assert ($result.exit_code != 0) $"Install unexpectedly accepted ($setting)"
        let expected = $"Cannot use ($setting) with a UKI or other prebuilt boot image: its kernel command line is fixed at build time"
        assert ($result.stderr | str contains $expected) $"Wrong error for ($setting): ($result.stderr)"
        assert (losetup -j $disk | str trim | is-empty) "Installer leaked a loop device"
    }
    null
} catch {|err| $err }

# Never unlink an image still attached after a failed installation. Preserve
# the install failure if cleanup also fails, and report both.
let cleanup_error = try {
    assert (losetup -j $disk | str trim | is-empty) "Disk still has an attached loop device"
    rm -rf $workdir
    null
} catch {|err| $err }
if $cleanup_error != null {
    print -e $"Cleanup failed: ($cleanup_error.msg)"
}
if $failure != null {
    error make $failure
}
if $cleanup_error != null {
    error make $cleanup_error
}

tap ok
