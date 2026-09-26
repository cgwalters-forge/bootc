# number: 49
# tmt:
#   summary: Test upgrading composefs systems installed by bootc 1.16 to current bootc
#   duration: 45m
#   enabled: false
#   adjust:
#     - when: composefs_bridge == true
#       enabled: true
# extra:
#   skip_if_ostree: true
#   try_bind_storage: true

# This deliberately starts disabled.  The bridge fixtures are large and are
# supplied from the host's read-only containers-storage mount only on request.
#
# The system is installed by a pinned bootc 1.16 release (the "stager"),
# which then stages the current image.  Which EROFS format that stages
# depends on the release: 1.16.0-1.16.2 generate V2 images and only read the
# bare composefs= argument, so they stage the V2 fallback of a dual-digest
# UKI.  1.16.3 already reads composefs.digest= first but still generates V2,
# so it rejects the dual-digest UKI ("The UKI has the wrong composefs=
# parameter") and is not tested here.  1.16.4 generates V1 (composefs-rs
# 080f925) and stages V1 directly, but only on a repository it created
# itself: the format comes from the repository's meta.json and is never
# converted, which is why the fixture is installed by the stager.
# (1.16.4-1.16.14 staging a UKI on a 1.16.0-1.16.3 install fail instead, see
# bootc-dev/bootc#2334.)  BLS installs are covered too: there the stager
# writes the entry for the current image itself, with only a bare composefs=
# argument.  Current bootc then upgrades (to V1) and rolls back.
use std assert
use tap.nu

const COMPOSEFS_MAGIC = 0xd078629a

def required-env [name: string, hint: string] {
    let value = ($env | get -i $name | default "")
    if $value == "" {
        error make { msg: $"($name) is required; ($hint)" }
    }
    $value
}

def bridge-image [] {
    required-env BOOTC_bridge_image "run with --bridge-image and --bind-storage-ro"
}

def upgrade-image [] {
    required-env BOOTC_upgrade_image "run with --upgrade-image"
}

# sealed and unsealed are UKI installs (Secure Boot with fs-verity required,
# and missing fs-verity allowed), bls a Type 1 install.
def bridge-mode [] {
    let mode = (required-env BOOTC_bridge_mode "set it to sealed, unsealed or bls")
    if not ($mode in ["sealed" "unsealed" "bls"]) {
        error make { msg: $"BOOTC_bridge_mode must be sealed, unsealed or bls, not ($mode)" }
    }
    $mode
}

def is-uki [] { (bridge-mode) != "bls" }

def stager-version [] {
    let version = (required-env BOOTC_bridge_stager_version "set it to the pinned bootc 1.16 version")
    if ($version !~ '^1\.16\.[0-9]+$') {
        error make { msg: $"BOOTC_bridge_stager_version must be a 1.16.x release, not ($version)" }
    }
    $version
}

# The EROFS format the stager stages the bridge image as.
def stager-format [] {
    let format = (required-env BOOTC_bridge_stager_format "set it to v1 or v2")
    if not ($format in ["v1" "v2"]) {
        error make { msg: $"BOOTC_bridge_stager_format must be v1 or v2, not ($format)" }
    }
    $format
}

def required-old-bootc-sha256 [] {
    let checksum = (required-env BOOTC_bridge_stager_bootc_sha256 "set it to the pinned stager's /usr/bin/bootc checksum")
    if ($checksum | str length) != 64 {
        error make { msg: "BOOTC_bridge_stager_bootc_sha256 must be a 64-character SHA-256 checksum" }
    }
    $checksum | str downcase
}

def assert-fixture-label [image: string, expected: string] {
    let label = (podman image inspect --format '{{ index .Config.Labels "bootc.test.fixture" }}' $image | str trim)
    assert equal $label $expected $"($image) has the wrong bridge fixture label"
}

def cmdline [] { open /proc/cmdline | str trim | split row " " }

def assert-old-fixture [] {
    let version = (bootc --version | str trim)
    assert equal $version $"bootc (stager-version)"
    let rpm_version = (rpm -q --qf '%{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}\n' bootc | str trim)
    let binary_sha256 = (sha256sum /usr/bin/bootc | split row " " | first | str downcase)
    assert equal $binary_sha256 (required-old-bootc-sha256)
    { bootc_version: $version, rpm_version: $rpm_version, bootc_sha256: $binary_sha256 }
        | to json
        | save --force /var/composefs-1-16-bootc-proof.json
    print $"bootc 1.16 fixture proof: version=($version) rpm=($rpm_version) sha256=($binary_sha256)"
}

def assert-current-bootc [] {
    assert ((bootc --version | str trim) != $"bootc (stager-version)") "booted userspace must be current bootc"
}

def assert-booted-image [expected: string] {
    let st = bootc status --json | from json
    let booted = $st.status.booted.image
    assert equal $booted.image.transport "containers-storage"
    assert equal $booted.image.image $expected
}

# The EROFS format of a repository image, from the composefs_version field
# of its composefs header (four little-endian u32s: magic, version, flags,
# composefs_version): 1 for V1, 2 for V2.
def image-format [digest: string] {
    let path = $"/sysroot/composefs/images/($digest)"
    let header = (^od --endian=little -An -t u4 -N 16 $path
        | split row --regex '\s+'
        | where { |w| $w != "" }
        | each { |w| $w | into int })
    assert equal ($header | length) 4 $"($path) is too short for a composefs header"
    assert equal ($header | first) $COMPOSEFS_MAGIC $"($path) has no composefs header"
    match ($header | get 3) {
        1 => "v1",
        2 => "v2",
        $v => { error make { msg: $"($path) has unexpected composefs_version ($v)" } },
    }
}

# The digest in a composefs= or composefs.digest= value, without the
# optional '?' marker and format prefix.
def karg-digest [value: string] {
    $value | str replace --regex '^\?' '' | split row ":" | last
}

def params-with-prefix [prefix: string] {
    cmdline | where { |p| $p | into string | str starts-with $prefix } | each { |p| $p | str replace $prefix "" }
}

# Check the dual-format UKI arguments and return the digest of the given format.
def uki-digest [format: string] {
    let v2_params = (params-with-prefix "composefs=")
    assert (($v2_params | length) == 1) "UKI must contain one V2 fallback argument"
    let v2_value = ($v2_params | first)
    let v2_is_unsealed = ($v2_value | str starts-with "?")
    assert equal $v2_is_unsealed ((bridge-mode) == "unsealed") "UKI integrity marker must match the fixture mode"
    let v2 = (karg-digest $v2_value)
    let v1_params = (params-with-prefix "composefs.digest=")
    assert (($v1_params | length) == 1) "current automatic UKI must retain one V1 argument"
    # Both arguments carry the same fs-verity policy marker; only the digest
    # is compared here.
    let v1 = (karg-digest ($v1_params | first))
    assert ($v1 != $v2) "dual-format UKI must contain distinct V1 and V2 identities"
    if $format == "v1" { $v1 } else { $v2 }
}

# Verify the identity actually selected by the running initramfs, as well as
# the corresponding repository image and deployment state directory.
def assert-selected-format [format: string] {
    if not ($format in ["v1" "v2"]) {
        error make { msg: $"Unsupported expected composefs format: ($format)" }
    }
    let st = bootc status --json | from json
    let expected_boot_type = if (is-uki) { "uki" } else { "bls" }
    assert equal ($st.status.booted.composefs.bootType | into string | str downcase) $expected_boot_type
    assert equal $st.status.booted.composefs.missingVerityAllowed ((bridge-mode) == "unsealed") "booted composefs policy must match the fixture mode"
    let selected = $st.status.booted.composefs.verity
    assert equal ($selected | str length) 128

    let root = findmnt --json --mountpoint / --output SOURCE | from json
    let root_source = ($root.filesystems | first | get source | into string)
    assert ($root_source | str starts-with "composefs:") "normal bridge boots must mount / directly from composefs"
    assert equal $root_source $"composefs:($selected)"

    if (is-uki) {
        assert equal (uki-digest $format) $selected "selected UKI identity must match bootc status"
    } else {
        # BLS entries are written by whichever bootc staged them, so only
        # check that the booted digest is the one on the command line.
        let digests = (params-with-prefix "composefs=") ++ (params-with-prefix "composefs.digest=")
            | each { |v| karg-digest $v }
        assert ($selected in $digests) $"booted digest must be on the kernel command line: ($digests)"
    }
    assert ($"/sysroot/composefs/images/($selected)" | path exists) "selected composefs image must exist"
    assert equal (image-format $selected) $format "selected composefs image has the wrong EROFS format"
    assert ($"/sysroot/state/deploy/($selected)" | path exists) "selected deployment state must exist"
    $selected
}

def write-sentinels [] {
    "composefs-1-16-bridge-etc" | save --force /etc/bootc-composefs-bridge-sentinel
    "composefs-1-16-bridge-var" | save --force /var/lib/bootc-composefs-bridge-sentinel
}

def assert-sentinels [] {
    assert equal (open /etc/bootc-composefs-bridge-sentinel | str trim) "composefs-1-16-bridge-etc"
    assert equal (open /var/lib/bootc-composefs-bridge-sentinel | str trim) "composefs-1-16-bridge-var"
}

def stage [image: string, save_as: string] {
    bootc switch --transport containers-storage $image
    let staged = (bootc status --json | from json).status.staged
    let staged_image = $staged.image
    assert equal $staged_image.image.transport "containers-storage"
    assert equal $staged_image.image.image $image
    assert (($staged.composefs.verity | str length) == 128)
    assert ("/run/composefs/staged-deployment" | path exists) "staging must create transient composefs deployment state"
    $staged.composefs.verity | save --force $save_as
}

const BRIDGE_IDENTITY = "/var/composefs-bridge-identity"
const UPGRADE_IDENTITY = "/var/composefs-bridge-upgrade-identity"

def old_stager_boot0 [] {
    tap begin $"bootc (stager-version) stager to current bridge \((bridge-mode)\)"
    assert-old-fixture
    let initial = (bootc status --json | from json).status.booted.image.image.image
    assert-fixture-label $initial $"bootc-(stager-version)-stager"
    assert-fixture-label (bridge-image) $"current-(bridge-mode)"
    write-sentinels
    stage (bridge-image) $BRIDGE_IDENTITY
    tmt-reboot
}

def old_stager_boot1 [] {
    assert-booted-image (bridge-image)
    assert-current-bootc
    let selected = assert-selected-format (stager-format)
    assert equal $selected (open $BRIDGE_IDENTITY | str trim)
    assert-sentinels
    assert-fixture-label (upgrade-image) $"current-(bridge-mode)-upgrade"
    stage (upgrade-image) $UPGRADE_IDENTITY
    tmt-reboot
}

def old_stager_boot2 [] {
    assert-booted-image (upgrade-image)
    assert-current-bootc
    let selected = assert-selected-format v1
    assert equal $selected (open $UPGRADE_IDENTITY | str trim)
    assert-sentinels
    bootc rollback
    assert equal ((bootc status --json | from json).status.rollbackQueued) true
    tmt-reboot
}

def old_stager_boot3 [] {
    assert-booted-image (bridge-image)
    let selected = assert-selected-format (stager-format)
    assert equal $selected (open $BRIDGE_IDENTITY | str trim)
    assert-sentinels
    assert equal ((bootc status --json | from json).status.rollbackQueued) false
    bootc internals composefs-gc --assert-no-op
    tap ok
}

def main [] {
    match ($env.TMT_REBOOT_COUNT? | default "0") {
        "0" => old_stager_boot0,
        "1" => old_stager_boot1,
        "2" => old_stager_boot2,
        "3" => old_stager_boot3,
        $count => { error make { msg: $"Invalid bridge reboot count: ($count)" } },
    }
}
