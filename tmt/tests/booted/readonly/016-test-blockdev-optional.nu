use std assert
use tap.nu

tap begin "blockdev ls-filesystem --optional"

def ls_optional [path: string] {
    bootc internals blockdev ls-filesystem --optional $path | from json
}

# /sysroot is on the real disk: --optional must find the same device as
# the plain invocation.
let sysroot = (ls_optional /sysroot)
assert ($sysroot != null) "expected a block device for /sysroot"
assert equal $sysroot (bootc internals blockdev ls-filesystem /sysroot | from json)

# / is on the disk too, unless it is a composefs overlay.
let rootfs = (findmnt -no FSTYPE / | str trim)
let root = (ls_optional /)
if $rootfs == "overlay" {
    assert equal $root null "expected no block device for an overlay /"
} else {
    assert ($root != null) $"expected a block device for / with fstype ($rootfs)"
}

# Virtual filesystems have none, where the plain invocation fails.
assert equal (ls_optional /proc) null "expected no block device for /proc"
let plain = (do { bootc internals blockdev ls-filesystem /proc } | complete)
assert ($plain.exit_code != 0) "expected ls-filesystem /proc to fail without --optional"

# tmpfs and overlayfs, mounted in a private mount namespace so the host's
# mounts are untouched.
let tmp = (mktemp -d)
let script = $"
set -euo pipefail
mount -t tmpfs tmpfs ($tmp)
mkdir ($tmp)/lower ($tmp)/upper ($tmp)/work ($tmp)/merged
mount -t overlay overlay -o lowerdir=($tmp)/lower,upperdir=($tmp)/upper,workdir=($tmp)/work ($tmp)/merged
bootc internals blockdev ls-filesystem --optional ($tmp)
bootc internals blockdev ls-filesystem --optional ($tmp)/merged
"
let results = (unshare -m --propagation private -- bash -c $script | lines)
rmdir $tmp
assert equal $results ["null" "null"] "expected no block device for tmpfs and overlayfs"

tap ok
