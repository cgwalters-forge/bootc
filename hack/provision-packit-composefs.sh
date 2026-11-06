#!/bin/bash
# Replace a package-mode CentOS Stream 10 aarch64 Testing Farm guest with a
# composefs (GRUB/BLS) bootc image. Used only by the opt-in
# tmt/plans/composefs-smoke-aarch64 plan; see tmt/README.md.
#
# Everything the install depends on is checked, and recorded in
# $EVIDENCE_DIR, before the destructive step. The install runs without
# --cleanup, so the old root (and $EVIDENCE_DIR in it) stays readable at
# /sysroot after the reboot for the booted check.
set -exuo pipefail

EVIDENCE_DIR=/var/tmp/bootc-composefs-smoke
IMAGE=localhost/bootc-composefs
ESP_PARTTYPE=c12a7328-f81f-11d2-ba4b-00a0c93ec93b
AUTHORIZED_KEYS=/root/.ssh/authorized_keys
# The guest root filesystem (xfs on the CentOS cloud images) has no fs-verity.
INSTALL_ARGS=(--composefs-backend --allow-missing-verity --bootloader grub)
# What podman run needs to act on the host, as system-reinstall-bootc does.
PODMAN_HOST_ARGS=(--rm --privileged --pid=host --user=root:root
    -v /var/lib/containers:/var/lib/containers -v /dev:/dev
    --security-opt label=type:unconfined_t)

fail() {
    echo "error: $*; not installing" 1>&2
    exit 1
}

# Checks grep saved evidence files rather than pipes: grep -q exiting early
# would fail the writer with SIGPIPE under pipefail.

# Require composefs kernel support in kernel config file $2.
check_kconfig() {
    for opt in CONFIG_EROFS_FS CONFIG_OVERLAY_FS; do
        grep -qE "^${opt}=[ym]$" "$2" || fail "$1 kernel lacks ${opt}"
    done
}

# Require file list $1 to contain each of the remaining arguments.
require_listed() {
    local path=$1
    shift
    for f in "$@"; do
        grep -qF -- "$f" "$path" || fail "$f missing from $(basename "$path")"
    done
}

rm -rf "$EVIDENCE_DIR"
mkdir -p "$EVIDENCE_DIR"
cat /proc/sys/kernel/random/boot_id > "$EVIDENCE_DIR/boot-id"

# Source system: architecture, OS, firmware, ESP, mounts, kernel and SSH
source /etc/os-release
[[ $(uname -m) == aarch64 ]] || fail "expected aarch64, got $(uname -m)"
[[ $ID == centos && $VERSION_ID == 10 ]] || fail "expected CentOS Stream 10, got ${ID}-${VERSION_ID}"
[[ -d /sys/firmware/efi ]] || fail "not booted with UEFI"
efibootmgr -v > "$EVIDENCE_DIR/efibootmgr.txt"
lsblk --json --paths --output NAME,PATH,TYPE,SIZE,FSTYPE,PARTTYPE,LABEL,MOUNTPOINTS > "$EVIDENCE_DIR/source-lsblk.json"
findmnt --json > "$EVIDENCE_DIR/source-findmnt.json"
lsblk -nrpo PARTTYPE > "$EVIDENCE_DIR/source-parttypes.txt"
grep -qx "$ESP_PARTTYPE" "$EVIDENCE_DIR/source-parttypes.txt" || fail "no EFI system partition"
cp "/usr/lib/modules/$(uname -r)/config" "$EVIDENCE_DIR/source-kernel-config"
check_kconfig source "$EVIDENCE_DIR/source-kernel-config"
[[ -s $AUTHORIZED_KEYS ]] || fail "$AUTHORIZED_KEYS is missing or empty"
# migrate-packit-composefs.sh carries each key with its .pub.
for key in /etc/ssh/ssh_host_*_key; do
    [[ -f $key.pub ]] || fail "$key has no public key"
done
# Public fingerprints only, for the booted check to compare against.
for key in /etc/ssh/ssh_host_*_key.pub; do
    ssh-keygen -lf "$key"
done > "$EVIDENCE_DIR/ssh-host-keys.txt"
systemctl is-enabled sshd.service NetworkManager.service

# Build localhost/bootc from Containerfile.packit, which carries the tmt
# helpers, /var/ARTIFACTS and the bootc RPM under test into the image, then
# derive the composefs image from it.
BOOTC_PACKIT_BUILD_ONLY=1 ./provision-packit.sh
podman build -t localhost/bootc-drop-lbis -f Containerfile.drop-lbis .
podman build --from localhost/bootc-drop-lbis -t "$IMAGE" -f Containerfile.packit-composefs .

# Target image: provenance, kernel and regenerated initramfs, inspected with
# the host's tools rather than whatever the image happens to carry.
BASE=$(jq -r --arg v "${ID}-${VERSION_ID}" '.base[$v]' < os-image-map.json)
{
    echo "base ${BASE} $(podman image inspect --format '{{.Digest}}' "$BASE")"
    echo "image ${IMAGE} id $(podman image inspect --format '{{.Id}}' "$IMAGE")"
    podman run --rm "$IMAGE" rpm -q bootc kernel-core
} | tee "$EVIDENCE_DIR/image.txt"
image_root=$(podman image mount "$IMAGE")
trap 'podman image unmount "$IMAGE"' EXIT
kvers=("$image_root"/usr/lib/modules/*)
[[ ${#kvers[@]} == 1 ]] || fail "expected one kernel in $IMAGE, got ${#kvers[@]}"
cp "${kvers[0]}/config" "$EVIDENCE_DIR/target-kernel-config"
lsinitrd --mod "${kvers[0]}/initramfs.img" > "$EVIDENCE_DIR/target-initramfs-modules.txt"
lsinitrd "${kvers[0]}/initramfs.img" > "$EVIDENCE_DIR/target-initramfs.txt"
check_kconfig target "$EVIDENCE_DIR/target-kernel-config"
grep -qx bootc "$EVIDENCE_DIR/target-initramfs-modules.txt" || fail "regenerated initramfs lacks the bootc dracut module"
require_listed "$EVIDENCE_DIR/target-initramfs.txt" usr/lib/bootc/initramfs-setup \
    initrd-root-fs.target.wants/bootc-root-setup.service
if grep -qx CONFIG_EROFS_FS=m "$EVIDENCE_DIR/target-kernel-config"; then
    require_listed "$EVIDENCE_DIR/target-initramfs.txt" /erofs.ko
fi
# What tmt needs to reconnect after the reboot.
systemctl --root="$image_root" is-enabled sshd.service NetworkManager.service ||
    fail "sshd or NetworkManager not enabled in $IMAGE"
for tool in /usr/sbin/sshd /usr/bin/rsync /usr/bin/nu /usr/bin/bootc; do
    [[ -x $image_root$tool ]] || fail "$IMAGE lacks $tool"
done
podman image unmount "$IMAGE"
trap - EXIT

# The destructive step. As system-reinstall-bootc does, but with explicit
# composefs selection and no --cleanup.
podman run "${PODMAN_HOST_ARGS[@]}" -v /:/target \
    -v "$AUTHORIZED_KEYS:/bootc_authorized_ssh_keys/root:ro" \
    "$IMAGE" \
    bootc install to-existing-root --acknowledge-destructive --skip-fetch-check \
    "${INSTALL_ARGS[@]}" \
    --root-ssh-authorized-keys /bootc_authorized_ssh_keys/root \
    2>&1 | tee "$EVIDENCE_DIR/install.log"

# Carry the live state tmt needs to reconnect into the installed deployment.
# Its mounts stay in the container's mount namespace.
podman run "${PODMAN_HOST_ARGS[@]}" -v /:/host \
    -v "$PWD/migrate-packit-composefs.sh:/run/migrate.sh:ro" \
    "$IMAGE" bash /run/migrate.sh /host \
    2>&1 | tee "$EVIDENCE_DIR/migrate.log"
