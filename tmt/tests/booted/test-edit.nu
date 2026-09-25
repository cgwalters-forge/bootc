# number: 50
# extra:
#   try_bind_storage: true
# tmt:
#   summary: Test bootc edit for image changes and rollback
#   duration: 30m
#   adjust:
#     - when: distro == centos-9 and boot_type == uki and seal_state == sealed
#       enabled: false
#       because: CentOS 9 cannot consume the signed host upgrade because shared storage is unavailable and its guest-local UKI builder produces unsigned images
#
# This test verifies `bootc edit --filename` on both backends:
# 1. An unchanged spec is a no-op, and an edit changing both the image and
#    the boot order is rejected
# 2. Changing spec.image stages that image, and we boot into it
# 3. Flipping spec.bootOrder queues a rollback, and we boot back; on
#    composefs, editing the image to the rollback one then points at
#    `bootc rollback`
#
# Like test-image-upgrade-reboot, the target image is the host-built
# upgrade image when there is one: on sealed UKI systems only an image
# signed on the host can boot.
use std assert
use tap.nu

# Present in both the host-built upgrade image and the one built here
const marker = "/usr/share/testing-bootc-upgrade-apply"
const initial_state = "/var/bootc-edit-initial-state.json"
const spec_file = "/var/tmp/bootc-edit-host.yaml"

def target_image [] {
    $env.BOOTC_upgrade_image? | default "localhost/bootc-edit-target"
}

def target_host [host: record] {
    $host
        | update spec.image.image (target_image)
        | update spec.image.transport "containers-storage"
}

def status_json [] {
    bootc status --json | from json
}

# Run `bootc edit` with the given host definition
def bootc_edit [host: record] {
    $host | to yaml | save -f $spec_file
    bootc edit --filename $spec_file
}

def first_boot [] {
    tap begin "bootc edit"

    let initial = status_json
    $initial.status.booted.image | to json | save -f $initial_state

    let out = bootc_edit $initial
    assert str contains $out "Edit cancelled"
    assert ((status_json).status.staged | is-empty)

    if $env.BOOTC_upgrade_image? == null {
        bootc image copy-to-storage
        let dockerfile = $"FROM localhost/bootc as base
RUN touch ($marker)
"
        (tap make_uki_containerfile $dockerfile) | podman build -t (target_image) -f - .
    }

    let target = target_host $initial

    # Changing the image and the boot order at once is not a valid transition
    let r = do { bootc_edit ($target | update spec.bootOrder "rollback") } | complete
    assert ($r.exit_code != 0) "Expected image change + rollback to be rejected"
    assert str contains $r.stderr "Invalid state transition"
    assert ((status_json).status.staged | is-empty)

    bootc_edit $target
    let staged = (status_json).status.staged
    assert equal $staged.image.image.image (target_image)
    assert equal $staged.image.image.transport "containers-storage"

    tmt-reboot
}

def second_boot [] {
    let st = status_json
    assert equal $st.status.booted.image.image.image (target_image)
    assert ($marker | path exists)
    assert equal $st.spec.bootOrder "default"

    bootc_edit ($st | update spec.bootOrder "rollback")
    let st = status_json
    assert equal $st.spec.bootOrder "rollback"
    assert equal $st.status.rollbackQueued true

    tmt-reboot
}

def third_boot [] {
    let st = status_json
    let initial = open $initial_state
    assert equal $st.status.booted.image $initial
    assert (not ($marker | path exists))
    assert equal $st.status.rollbackQueued false

    # composefs refuses to stage an image identical to an existing deployment,
    # here the rollback one, and should say how to get back to it
    if (tap is_composefs) {
        let r = do { bootc_edit (target_host $st) } | complete
        assert ($r.exit_code != 0) "Expected edit to the rollback image to be rejected"
        assert str contains $r.stderr "bootc rollback"
        assert ((status_json).status.staged | is-empty)
    }

    tap ok
}

def main [] {
    # See https://tmt.readthedocs.io/en/stable/stories/features.html#reboot-during-test
    match $env.TMT_REBOOT_COUNT? {
        null | "0" => first_boot,
        "1" => second_boot,
        "2" => third_boot,
        $o => { error make { msg: $"Invalid TMT_REBOOT_COUNT ($o)" } },
    }
}
