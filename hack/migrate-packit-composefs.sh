#!/bin/bash
# Run by provision-packit-composefs.sh inside the image under test, after
# `bootc install to-existing-root` and before the reboot, with the host root
# at $1. Mounts the installed deployment with `bootc install mount` and
# carries over the live state tmt needs to reconnect and continue:
#
# - SSH host keys, so tmt reaches the same host identity;
# - NetworkManager connections and /etc/hostname, as the source system's
#   network configuration is the authoritative one;
# - cloud-init's instance state, so cloud-init does not treat the first boot
#   as a new instance and regenerate the host keys or network configuration;
# - /var/ARTIFACTS and the tmt helpers, which changed since the image build.
#
# Root's authorized keys come from the install's --root-ssh-authorized-keys.
# The copied files include private keys and connection secrets: only their
# paths are ever printed.
set -euo pipefail

HOST=$1
MNT=$(mktemp -d /run/bootc-target.XXXXXX)
carried=()

fail() {
    echo "error: $*" 1>&2
    exit 1
}

cleanup() {
    if mountpoint -q "$MNT"; then
        umount -R "$MNT"
    fi
    rmdir "$MNT"
}
trap cleanup EXIT

# Copy host path $1 to the same path in the deployment, resolving it there
# (e.g. /usr/local -> /var/usrlocal) as the booted system will.
carry() {
    local src=$HOST$1 dest top
    dest=$MNT$(chroot "$MNT" realpath -m "$1")
    echo "carrying $1"
    # Relabel from the first directory this creates.
    top=$dest
    while [[ ! -e $(dirname "$top") ]]; do
        top=$(dirname "$top")
    done
    mkdir -p "$(dirname "$dest")"
    if [[ -d $src ]]; then
        mkdir -p "$dest"
        cp -a "$src/." "$dest/"
    else
        cp -a "$src" "$dest"
    fi
    carried+=("$top")
}

bootc install mount --sysroot "$HOST" --latest "$MNT"

shopt -s nullglob
private_keys=("$HOST"/etc/ssh/ssh_host_*_key)
(( ${#private_keys[@]} > 0 )) || fail "no SSH host keys on the host"
for key in "${private_keys[@]}"; do
    carry "${key#"$HOST"}"
    carry "${key#"$HOST"}.pub"
    # The host's ssh_keys group ID need not match the image's, and sshd
    # rejects a group-readable key whose group is not ssh_keys.
    chown 0:0 "$MNT${key#"$HOST"}"
    chmod 0600 "$MNT${key#"$HOST"}"
done
carry /var/ARTIFACTS
# OSCI keeps the tmt helpers in /var/lib/tmt/scripts, Packit in /usr/local/bin.
if [[ -d $HOST/var/lib/tmt/scripts ]]; then
    carry /var/lib/tmt/scripts
else
    carry /usr/local/bin
fi
for optional in /etc/NetworkManager/system-connections /etc/hostname /var/lib/cloud; do
    if [[ -e $HOST$optional ]]; then
        carry "$optional"
    fi
done

[[ -s $MNT/etc/tmpfiles.d/bootc-root-ssh.conf ]] || fail "install did not provision root's authorized keys"

# Label what was copied as the deployment's own policy would.
selinuxtype=$(sed -n 's/^SELINUXTYPE=//p' "$MNT/etc/selinux/config")
setfiles -F -r "$MNT" "$MNT/etc/selinux/${selinuxtype}/contexts/files/file_contexts" "${carried[@]}"

sync -f "$MNT/etc" "$MNT/var"
trap - EXIT
cleanup
echo "carried ${#carried[@]} paths into the deployment"
