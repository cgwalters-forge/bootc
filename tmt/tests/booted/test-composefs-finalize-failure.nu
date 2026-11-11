# number: 64
# tmt:
#   summary: Report a failed shutdown finalization once on the following composefs boot
#   duration: 30m
#   adjust:
#     - when: boot_type == aboot
#       enabled: false
#       because: Exercises systemd-boot or GRUB finalization
# extra:
#   skip_if_ostree: true

use std assert
use tap.nu
use bootc_testlib.nu

const SERVICE = "bootc-boot-complete.service"

def failure_stamp [] {
    # Match the persistent location used by bootc, including GRUB systems
    # without a separate /boot mount. Do not read the image's unmounted /boot.
    let probe = (mountpoint --quiet /boot | complete)
    let boot = match $probe.exit_code {
        0 => "/boot",
        32 => "/sysroot/boot",
        $code => { error make {msg: $"mountpoint failed with ($code): ($probe.stderr)"} },
    }
    $boot | path join "bootc-finalize-failure.stamp"
}

def assert_old_deployment [] {
    let booted = (bootc status --json | from json).status.booted.composefs.verity
    assert equal $booted (open --raw original-verity | str trim)
    assert (not (failure_stamp | path exists))
}

def main [] {
    match $env.TMT_REBOOT_COUNT? {
        null | "0" => {
            tap begin "composefs finalize failure detection"
            assert (tap is_composefs)
            assert (not (failure_stamp | path exists))
            (bootc status --json | from json).status.booted.composefs.verity | save original-verity
            # GRUB does not necessarily have a blessing consumer. Explicitly pull
            # the passive target into the next boot transaction on both loaders.
            systemctl add-wants multi-user.target boot-complete.target
            bootc image copy-to-storage
            (tap make_uki_containerfile '
FROM localhost/bootc as base
RUN touch /usr/share/test-finalize-failure
') | save Dockerfile
            podman build -t localhost/bootc-finalize-failure .
            bootc switch --transport containers-storage localhost/bootc-finalize-failure
            let staged = (bootc status --json | from json).status.staged.composefs.verity
            # Break the staged /etc from outside bootc, after staging/preflight.
            # Finalization fails opening it, before changing bootloader entries.
            systemd-run -p PrivateMounts=yes -qdPG -- /bin/sh -ec $"
                mount -o remount,rw /sysroot
                mv /sysroot/state/deploy/($staged)/etc /sysroot/state/deploy/($staged)/etc.saved
            "
            bootc_testlib reboot
        },
        "1" => {
            assert_old_deployment
            assert equal (systemctl show -P ActiveState $SERVICE | str trim) "failed"
            assert equal (systemctl show -P Result $SERVICE | str trim) "exit-code"
            assert equal (systemctl show -P ActiveState boot-complete.target | str trim) "inactive"
            let message = (open --raw $"(failure_stamp).previous")
            assert ($message | str contains "Opening staged deployment /etc")
            let journal = (journalctl -b -u $SERVICE --no-pager -o cat)
            assert ($journal | str contains $message)
            assert ($journal | str contains "journalctl -b -1 -u bootc-finalize-staged.service")
            # The transient staged-deployment record is gone after reboot, so
            # this shutdown has nothing to retry and must not recreate the stamp.
            bootc_testlib reboot
        },
        "2" => {
            assert_old_deployment
            assert equal (systemctl show -P ActiveState $SERVICE | str trim) "inactive"
            assert equal (systemctl show -P Result $SERVICE | str trim) "success"
            assert equal (systemctl show -P ActiveState boot-complete.target | str trim) "active"
            tap ok
        },
        $other => { error make {msg: $"Unexpected reboot count ($other)"} },
    }
}
