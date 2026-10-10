#!/bin/bash
set -euxo pipefail
dnf install -y reinstall/packages/bootc-*.rpm reinstall/packages/system-reinstall-bootc-*.rpm
podman load -i reinstall/bootc.tar
podman tag "$BOOTC_REINSTALL_IMAGE" localhost/bootc
# Keep these in sync with hack/lbi and provision-packit.sh.
podman pull -q --retry 5 --retry-delay 5s quay.io/curl/curl:latest quay.io/curl/curl-base:latest registry.access.redhat.com/ubi9/podman:latest
expect reinstall/system-reinstall-bootc.exp
# Expect's existing helper cancels the automatic reboot; tmt performs it next.
test -d /ostree/deploy
# The reinstalled system boots with a fresh /var, but after the reboot tmt
# re-runs the prepare phase that requested it from its workdir there (and
# expects its scripts in TMT_SCRIPTS_DIR). Carry tmt's guest state over,
# minus the payload consumed above.
newvar=/ostree/deploy/default/var
test -d "${newvar}"
mkdir -p "${newvar}/tmp" "${newvar}/lib"
rsync -a --exclude=tree/reinstall /var/tmp/tmt "${newvar}/tmp/"
if test -d /var/lib/tmt; then
    rsync -a /var/lib/tmt "${newvar}/lib/"
fi
