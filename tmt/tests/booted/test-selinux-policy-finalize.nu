# number: 58
# tmt:
#   summary: Local SELinux policy modules survive an upgrade with a new policy
#   duration: 30m
#   adjust:
#     - when: running_env != image_mode
#       enabled: false
#       because: these tests require features only available in image mode
# extra:
#   fixme_skip_if_uki: true
#
# A local policy module (`semodule -i`) rebuilds the binary policy in /etc,
# so the /etc merge keeps the booted policy over the new image's. Finalizing
# the deployment has to rebuild the policy from the merged module store, so
# that both the local module and the new image's policy changes are loaded.
# Each module declares a boolean, which shows up in selinuxfs only if the
# loaded policy includes that module.
use std assert
use tap.nu

const LOCAL_MODULE = "bootc_finalize_local"
const IMAGE_MODULE = "bootc_finalize_image"

def loaded_boolean_exists [name: string] {
    $"/sys/fs/selinux/booleans/($name)" | path exists
}

# This code runs on *each* boot.
bootc status

def initial_build [] {
    tap begin "local SELinux module plus image policy change"

    if not ("/sys/fs/selinux/enforce" | path exists) {
        print "Skipping, SELinux is not enabled"
        tap ok
        return
    }

    let td = mktemp -d
    cd $td

    # Install a local module on the running system
    $"\(boolean ($LOCAL_MODULE) false\)\n" | save $"($LOCAL_MODULE).cil"
    semodule -i $"($LOCAL_MODULE).cil"
    assert (loaded_boolean_exists $LOCAL_MODULE)

    # A derived image whose policy differs from the booted one
    bootc image copy-to-storage
    $"\(boolean ($IMAGE_MODULE) false\)\n" | save $"($IMAGE_MODULE).cil"
    (tap make_uki_containerfile $"FROM localhost/bootc
COPY ($IMAGE_MODULE).cil /tmp/
RUN semodule -i /tmp/($IMAGE_MODULE).cil && rm /tmp/($IMAGE_MODULE).cil
") | save Dockerfile
    podman build -t localhost/bootc-derived .

    bootc switch --transport containers-storage localhost/bootc-derived

    tmt-reboot
}

def second_boot [] {
    tap begin "both modules are in the loaded policy"

    if not ("/sys/fs/selinux/enforce" | path exists) {
        print "Skipping, SELinux is not enabled"
        tap ok
        return
    }

    let st = bootc status --json | from json
    assert ($st.status.booted.image.image.image | str contains "bootc-derived")

    # Informational: the finalization log, if the journal is persistent
    try { journalctl -b -1 --no-pager -g "SELinux policy" | print }

    let modules = semodule -l | lines
    assert ($LOCAL_MODULE in $modules) "local module missing from the module store"
    assert ($IMAGE_MODULE in $modules) "image module missing from the module store"

    assert (loaded_boolean_exists $LOCAL_MODULE) "local module missing from the loaded policy"
    assert (loaded_boolean_exists $IMAGE_MODULE) "new image's policy was not loaded"

    # The rebuilt policy files must be labeled as the policy expects. The
    # store's lock files are skipped: with store-root=/etc/selinux they sit
    # where file_contexts says selinux_config_t, but semodule creates them as
    # semanage_trans_lock_t through a type transition, which a plain
    # `semodule -i` does too.
    let mislabeled = restorecon -nvR /etc/selinux | lines | where {|l| not ($l | str contains ".LOCK") }
    assert ($mislabeled | is-empty) $"mislabeled policy files: ($mislabeled | str join "\n")"

    tap ok
}

def main [] {
    # See https://tmt.readthedocs.io/en/stable/stories/features.html#reboot-during-test
    match $env.TMT_REBOOT_COUNT? {
        null | "0" => initial_build,
        "1" => second_boot,
        $o => { error make { msg: $"Invalid TMT_REBOOT_COUNT ($o)" } },
    }
}
