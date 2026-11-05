# Check the composefs boot that hack/provision-packit-composefs.sh installed
# over package mode, and copy its evidence into the tmt test data.
use std assert

# provision-packit-composefs.sh wrote this into the old root, which the
# composefs boot mounts at /sysroot.
let evidence = "/sysroot/var/tmp/bootc-composefs-smoke"
let data = $env.TMT_TEST_DATA

cp -r $evidence $"($data)/preinstall"
let pre_boot_id = open $"($data)/preinstall/boot-id" | str trim
let boot_id = open /proc/sys/kernel/random/boot_id | str trim
assert ($pre_boot_id != $boot_id) "boot ID must change across the install"

let arch = uname | get machine
assert equal $arch "aarch64"

bootc status --json | save $"($data)/bootc-status.json"
let composefs = open $"($data)/bootc-status.json" | get status.booted.composefs?
assert ($composefs != null) "booted deployment must use the composefs backend"
assert equal ($composefs.bootType | str downcase) "bls"
assert equal ($composefs.bootloader | str downcase) "grub"
assert $composefs.missingVerityAllowed

let cmdline = open /proc/cmdline | str trim
$cmdline | save $"($data)/cmdline"
assert ($cmdline | split row " " | any { |arg| $arg | str starts-with "composefs=" }) "kernel command line must select composefs"

findmnt --json | save $"($data)/findmnt.json"
let root = findmnt --json / | from json | get filesystems.0
assert equal $root.fstype "overlay" "root must be the composefs overlay"
assert ((findmnt --json /sysroot | from json | get filesystems | length) == 1) "physical root must be mounted at /sysroot"

lsblk --json --paths --output NAME,PATH,TYPE,SIZE,FSTYPE,PARTTYPE,LABEL,MOUNTPOINTS | save $"($data)/lsblk.json"
journalctl -b -u bootc-root-setup.service --no-pager | save $"($data)/bootc-root-setup.log"

# tmt reached this test over SSH, so SSH works; also require a default route.
assert (($env.SSH_CONNECTION? | default "") != "") "test must run over SSH"
let routes = ip --json route show default | from json
assert (($routes | length) > 0) "guest must have a default route"

for f in [preinstall/install.log preinstall/image.txt preinstall/target-initramfs.txt bootc-status.json findmnt.json] {
    assert ($"($data)/($f)" | path exists) $"missing artifact ($f)"
}
print "ok"
