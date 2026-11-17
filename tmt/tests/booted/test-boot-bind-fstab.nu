# number: 55
# extra:
#   bios: true
#   fixme_skip_if_composefs: true
#   fixme_skip_if_uki: true
# tmt:
#   summary: BIOS hybrid install keeps the OSTree boot bind mounted
#   duration: 10m
#   enabled: false
#   adjust:
#     - when: boot_bind_fstab == true
#       enabled: true
use std assert
use tap.nu

# Like readonly/054-test-boot-automount.nu, exercise status after expiry.
# Unlike that symptom test, require no automount and keep the real 120s timer.
def check_mount [] {
    let autofs = (do { ^findmnt -t autofs --mountpoint /boot } | complete)
    assert equal $autofs.exit_code 1
    systemctl is-active boot.mount
}

def main [] {
    assert not ("/sys/firmware/efi" | path exists)
    assert not (tap is_composefs)
    ostree --version
    systemctl --version
    lsblk --output NAME,FSTYPE,PARTTYPE,MOUNTPOINTS
    # Hybrid to-disk layout must retain an unused ESP.
    let partitions = (lsblk --json --list --output NAME,PARTTYPE,MOUNTPOINTS | from json).blockdevices
    let esps = $partitions | where parttype == "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"
    assert equal ($esps | length) 1
    assert ($esps.0.mountpoints | all {|p| $p == null })
    assert equal (stat -c %d /sysroot/boot | str trim) (stat -c %d /sysroot | str trim)
    check_mount

    match $env.TMT_REBOOT_COUNT? {
        null | "0" => {
            # Do not touch /boot during the idle window: 120s expiry plus
            # systemd's polling interval, bounded at 180s.
            sleep 180sec
            check_mount
            bootc status
            # An expired automount cannot be triggered from another mount
            # namespace (opendir(boot): EPERM), so check that path too.
            unshare -m bootc status

            bootc image copy-to-storage
            let td = mktemp -d
            "boot-bind-fstab regression" | save $"($td)/marker"
            "FROM localhost/bootc\nCOPY marker /usr/share/boot-bind-fstab-test\n" | save $"($td)/Containerfile"
            podman build -t localhost/boot-bind-fstab-test $td
            bootc switch --transport containers-storage localhost/boot-bind-fstab-test
            let st = bootc status --json | from json
            assert ($st.status.staged.image.imageDigest != $st.status.booted.image.imageDigest)
            $st.status.staged.image.imageDigest | save /var/tmp/boot-bind-fstab-digest
            tmt-reboot
        }
        "1" => {
            let st = bootc status --json | from json
            assert equal $st.status.booted.image.imageDigest (open --raw /var/tmp/boot-bind-fstab-digest | str trim)
            assert equal (open --raw /usr/share/boot-bind-fstab-test | str trim) "boot-bind-fstab regression"
            tap ok
        }
        $n => { error make {msg: $"Unexpected reboot count ($n)"} }
    }
}
