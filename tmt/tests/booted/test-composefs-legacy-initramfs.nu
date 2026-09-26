# number: 56
# tmt:
#   summary: Boot a BLS entry for an image whose initramfs predates composefs.digest=
#   duration: 30m
# extra:
#   skip_if_ostree: true
#
# In bootc 1.16 initramfs images, bootc-root-setup.service only starts when
# a karg named `composefs` is present.  When a newer bootc writes the BLS
# entry for such an image (e.g. `bootc switch` to an image still shipping
# bootc 1.16), it must emit the legacy `composefs=` karg next to
# `composefs.digest=`, or the target drops to the emergency shell.
# Simulate that initramfs by rebuilding it with the 1.16 unit condition.
use std assert
use tap.nu
use bootc_testlib.nu

const legacy_image = "localhost/bootc-legacy-initramfs"
const unit = "usr/lib/systemd/system/bootc-root-setup.service"
const legacy_condition = "ConditionKernelCommandLine=composefs"

let st = bootc status --json | from json
let booted = $st.status.booted.image

def first_boot [] {
    tap begin "switch to an image whose initramfs only knows composefs="

    bootc image copy-to-storage

    r#'
        FROM localhost/bootc
        RUN sed -i -e '/^ConditionKernelCommandLine=/d' \
            -e '/^\[Unit\]/a ConditionKernelCommandLine=composefs' \
            /usr/lib/systemd/system/bootc-root-setup.service
        RUN set -x; kver=$(cd /usr/lib/modules && echo *); \
            dracut -vf --add bootc /usr/lib/modules/$kver/initramfs.img $kver
    '# | podman build -t $legacy_image . -f -

    # Check the simulation took: the initramfs must carry only the 1.16 condition.
    let conditions = podman run --rm $legacy_image bash -c $"lsinitrd -f ($unit) /usr/lib/modules/*/initramfs.img"
        | lines | where { |l| $l starts-with "ConditionKernelCommandLine=" }
    assert equal $conditions [$legacy_condition]

    bootc switch --transport containers-storage $legacy_image

    tmt-reboot
}

def second_boot [] {
    assert equal $booted.image.image $legacy_image

    let cmdline = bootc_testlib parse_cmdline
    let verity = $st.status.booted.composefs.verity
    assert ($cmdline | any { |k| $k starts-with "composefs.digest=" and ($k | str ends-with $verity) })
    assert ($cmdline | any { |k| $k == $"composefs=($verity)" or $k == $"composefs=?($verity)" })

    let root = findmnt --json --mountpoint / --output SOURCE | from json
    let root_source = ($root.filesystems | first | get source | into string)
    assert ($root_source | str contains $verity) $"root not mounted from composefs: ($root_source)"

    tap ok
}

def main [] {
    # A UKI carries its own kargs, built with the image's bootc; only BLS
    # entries are written by the (possibly newer) host bootc.
    if ($st.status.booted.composefs.bootType | str downcase) == "uki" {
        print "# Skipping: UKI boot"
        return
    }

    match $env.TMT_REBOOT_COUNT? {
        null | "0" => first_boot,
        "1" => second_boot,
        $o => { error make { msg: $"Invalid TMT_REBOOT_COUNT ($o)" } },
    }
}
