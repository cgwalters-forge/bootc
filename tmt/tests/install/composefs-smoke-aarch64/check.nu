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
let verity = $composefs.verity
assert equal ($verity | str length) 128

let cmdline = open /proc/cmdline | str trim
$cmdline | save $"($data)/cmdline"
assert ($cmdline | split row " " | any { |arg| ($arg | str starts-with "composefs") and ($arg | str contains $verity) }) "kernel command line must select the booted composefs image"

# An overlay root fstype proves little; the source names the composefs image
# the initramfs actually mounted.
findmnt --json | save $"($data)/findmnt.json"
let root_source = findmnt --json --mountpoint / --output SOURCE | from json | get filesystems.0.source
assert equal $root_source $"composefs:($verity)" "root must be the booted composefs image"
assert ((findmnt --json --mountpoint /sysroot | from json | get filesystems | length) == 1) "physical root must be mounted at /sysroot"

lsblk --json --paths --output NAME,PATH,TYPE,SIZE,FSTYPE,PARTTYPE,LABEL,MOUNTPOINTS | save $"($data)/lsblk.json"
journalctl -b -u bootc-root-setup.service --no-pager | save $"($data)/bootc-root-setup.log"

# Reconnect: tmt got here over SSH, but don't rely on it exporting
# SSH_CONNECTION; look for the connection, the services and the identity.
let ssh_peers = ss -Htn state established "( sport = :22 )" | lines
assert (($ssh_peers | length) > 0) "guest must have an established SSH connection"
for unit in [sshd.service NetworkManager.service] {
    assert equal (systemctl is-active $unit | str trim) "active" $"($unit) must be active"
}
let routes = ip --json route show default | from json
assert (($routes | length) > 0) "guest must have a default route"
let host_keys = glob /etc/ssh/ssh_host_*_key.pub | sort | each { |k| ssh-keygen -lf $k | str trim } | str join "\n"
$host_keys | save $"($data)/ssh-host-keys.txt"
assert equal $host_keys (open $"($data)/preinstall/ssh-host-keys.txt" | str trim) "SSH host keys must survive the install"

# The migrated tmt state.
assert ("/var/ARTIFACTS" | path exists) "/var/ARTIFACTS must be carried over"
assert (["/usr/local/bin/tmt-reboot" "/var/lib/tmt/scripts/tmt-reboot"] | any { |p| $p | path exists }) "tmt helpers must be carried over"

for f in [preinstall/install.log preinstall/migrate.log preinstall/image.txt preinstall/target-initramfs.txt bootc-status.json findmnt.json] {
    assert ($"($data)/($f)" | path exists) $"missing artifact ($f)"
}
print "ok"
