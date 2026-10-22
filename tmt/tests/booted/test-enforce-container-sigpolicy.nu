# number: 61
# tmt:
#   summary: Enforce and persist --enforce-container-sigpolicy on switch and upgrade
#   duration: 30m
#
# `--enforce-container-sigpolicy` must refuse a containers-policy.json whose
# default is `insecureAcceptAnything`, and the choice must be recorded in the
# deployment so a later `bootc upgrade` keeps enforcing it. This runs on both
# the ostree and composefs backends, which must behave the same.
#
# First boot:
#   - permissive policy: `bootc switch --enforce-container-sigpolicy` is refused
#   - restrictive policy that still accepts containers-storage: the same switch
#     is staged, and the staged deployment records `containerPolicy`
# Second boot:
#   - the booted deployment still records `containerPolicy`
#   - restrictive policy: `bootc upgrade` succeeds (nothing to do)
#   - permissive policy: `bootc upgrade` is refused
#
# The restrictive policy stays in /etc across the reboot; the original is
# restored at the end. Each plan runs in its own VM, so a failure midway
# does not leak into other tests.
use std assert
use tap.nu

const POLICY = "/etc/containers/policy.json"
const POLICY_BACKUP = "/var/tmp/policy.json.orig"
const TARGET = "localhost/bootc-sigpolicy"
const REFUSAL = "insecureAcceptAnything"

# The default shipped by containers-common.
const PERMISSIVE_POLICY = {
    default: [{type: "insecureAcceptAnything"}]
}

# Rejects by default, but allows the local test image.
const RESTRICTIVE_POLICY = {
    default: [{type: "reject"}]
    transports: {
        "containers-storage": {"": [{type: "insecureAcceptAnything"}]}
    }
}

def set_policy [policy: record] {
    $policy | to json | save -f $POLICY
}

def assert_refused [result: record, what: string] {
    assert ($result.exit_code != 0) $"Expected ($what) to be refused"
    assert ($result.stderr | str contains $REFUSAL) $"Unexpected error from ($what): ($result.stderr)"
}

def first_boot [] {
    tap begin "enforce-container-sigpolicy on switch and upgrade"

    cp $POLICY $POLICY_BACKUP

    bootc image copy-to-storage
    "sigpolicy" | save -f sigpolicy-marker
    let dockerfile = "FROM localhost/bootc
COPY sigpolicy-marker /usr/share/bootc-test-sigpolicy-marker
"
    (tap make_uki_containerfile $dockerfile) | save -f Dockerfile
    podman build -t $TARGET .

    set_policy $PERMISSIVE_POLICY
    let r = do { bootc switch --enforce-container-sigpolicy --transport containers-storage $TARGET } | complete
    assert_refused $r "switch with a permissive policy"
    let st = bootc status --json | from json
    assert equal $st.status.staged null "Refused switch must not stage a deployment"

    set_policy $RESTRICTIVE_POLICY
    bootc switch --enforce-container-sigpolicy --transport containers-storage $TARGET
    let st = bootc status --json | from json
    assert equal $st.status.staged.image.image.image $TARGET
    assert equal $st.status.staged.image.image.signature containerPolicy

    tmt-reboot
}

def second_boot [] {
    let st = bootc status --json | from json
    assert equal $st.status.booted.image.image.image $TARGET
    assert equal $st.status.booted.image.image.signature containerPolicy
    assert equal $st.spec.image.signature containerPolicy

    # Enforcement must not get in the way of a normal upgrade.
    bootc upgrade

    set_policy $PERMISSIVE_POLICY
    let r = do { bootc upgrade } | complete
    assert_refused $r "upgrade with a permissive policy"

    cp -f $POLICY_BACKUP $POLICY
    tap ok
}

def main [] {
    # See https://tmt.readthedocs.io/en/stable/stories/features.html#reboot-during-test
    match $env.TMT_REBOOT_COUNT? {
        null | "0" => first_boot,
        "1" => second_boot,
        $o => { error make { msg: $"Invalid TMT_REBOOT_COUNT ($o)" } },
    }
}
