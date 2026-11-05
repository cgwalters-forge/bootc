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

fail() {
    echo "error: $*; not installing" 1>&2
    exit 1
}

# Require composefs kernel support in a kernel config read from stdin.
check_kconfig() {
    local label=$1 config
    config=$(cat)
    for opt in CONFIG_EROFS_FS CONFIG_OVERLAY_FS; do
        grep -qE "^${opt}=[ym]$" <<< "$config" || fail "${label} kernel lacks ${opt}"
    done
}

rm -rf "$EVIDENCE_DIR"
mkdir -p "$EVIDENCE_DIR"
cat /proc/sys/kernel/random/boot_id > "$EVIDENCE_DIR/boot-id"

# Source system: architecture, OS, firmware, ESP, mounts, kernel and SSH keys
source /etc/os-release
[[ $(uname -m) == aarch64 ]] || fail "expected aarch64, got $(uname -m)"
[[ $ID == centos && $VERSION_ID == 10 ]] || fail "expected CentOS Stream 10, got ${ID}-${VERSION_ID}"
[[ -d /sys/firmware/efi ]] || fail "not booted with UEFI"
efibootmgr -v > "$EVIDENCE_DIR/efibootmgr.txt"
lsblk --json --paths --output NAME,PATH,TYPE,SIZE,FSTYPE,PARTTYPE,LABEL,MOUNTPOINTS > "$EVIDENCE_DIR/source-lsblk.json"
findmnt --json > "$EVIDENCE_DIR/source-findmnt.json"
lsblk -nrpo PARTTYPE | grep -qx "$ESP_PARTTYPE" || fail "no EFI system partition"
cp "/usr/lib/modules/$(uname -r)/config" "$EVIDENCE_DIR/source-kernel-config"
check_kconfig source < "$EVIDENCE_DIR/source-kernel-config"
[[ -s $AUTHORIZED_KEYS ]] || fail "$AUTHORIZED_KEYS is missing or empty"

# Build localhost/bootc from Containerfile.packit, which carries the tmt
# helpers, /var/ARTIFACTS and the bootc RPM under test into the image, then
# derive the composefs image from it.
BOOTC_PACKIT_BUILD_ONLY=1 ./provision-packit.sh
podman build -t "$IMAGE" -f Containerfile.packit-composefs .

# Target image: provenance, kernel and regenerated initramfs
BASE=$(jq -r --arg v "${ID}-${VERSION_ID}" '.base[$v]' < os-image-map.json)
{
    echo "base ${BASE} $(podman image inspect --format '{{.Digest}}' "$BASE")"
    echo "image ${IMAGE} $(podman image inspect --format '{{.Id}}' "$IMAGE")"
    podman run --rm "$IMAGE" rpm -q bootc kernel-core
} | tee "$EVIDENCE_DIR/image.txt"
podman run --rm "$IMAGE" sh -c 'cat /usr/lib/modules/*/config' > "$EVIDENCE_DIR/target-kernel-config"
check_kconfig target < "$EVIDENCE_DIR/target-kernel-config"
podman run --rm "$IMAGE" sh -c 'lsinitrd /usr/lib/modules/*/initramfs.img' > "$EVIDENCE_DIR/target-initramfs.txt"
for f in usr/lib/bootc/initramfs-setup bootc-root-setup.service; do
    grep -qF "$f" "$EVIDENCE_DIR/target-initramfs.txt" || fail "regenerated initramfs lacks $f"
done

# The destructive step. As system-reinstall-bootc does, but with explicit
# composefs selection and no --cleanup.
podman run --rm --privileged --pid=host --user=root:root \
    -v /var/lib/containers:/var/lib/containers -v /dev:/dev -v /:/target \
    --security-opt label=type:unconfined_t \
    -v "$AUTHORIZED_KEYS:/bootc_authorized_ssh_keys/root:ro" \
    "$IMAGE" \
    bootc install to-existing-root --acknowledge-destructive --skip-fetch-check \
    "${INSTALL_ARGS[@]}" \
    --root-ssh-authorized-keys /bootc_authorized_ssh_keys/root \
    2>&1 | tee "$EVIDENCE_DIR/install.log"
