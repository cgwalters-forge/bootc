# TMT integration tests

In the bootc CI, integration tests are executed via Packit on the Testing Farm.

See [CONTRIBUTING.md](../CONTRIBUTING.md#running-tmt-integration-tests) for
instructions on running them locally.

## Opt-in aarch64 composefs smoke test

`plans/composefs-smoke-aarch64.fmf` replaces a package-mode CentOS Stream 10
aarch64 guest with a composefs GRUB/BLS image built from the bootc RPM under
test (`hack/provision-packit-composefs.sh`), reboots it, and checks the boot.
The plan and its test are disabled unless the `bootc_smoke=composefs-aarch64`
context is set on CentOS Stream 10 aarch64, so neither Packit nor
`cargo xtask run-tmt` selects them. Before installing, the prepare script fails
unless the guest is UEFI with an EFI system partition, both kernels support
EROFS and overlayfs, and the regenerated initramfs carries the bootc composefs
setup. After installing, `hack/migrate-packit-composefs.sh` mounts the new
deployment with `bootc install mount` and carries over the SSH host keys,
network and cloud-init state and tmt files that the reconnect needs. The
evidence, image IDs and install and migration logs are kept under
`/var/tmp/bootc-composefs-smoke`, which the test copies into its tmt data.

Run it with an authorized Testing Farm token in `TESTING_FARM_API_TOKEN`,
using the commit under test and the Copr build of that commit for
`centos-stream-10-aarch64` (from Packit's `copr_build` on the pull request):

```
testing-farm request \
  --git-url https://github.com/<owner>/bootc --git-ref <commit> \
  --plan '^/tmt/plans/composefs-smoke-aarch64$' \
  --compose CentOS-Stream-10 --arch aarch64 \
  --context distro=centos-stream-10 --context arch=aarch64 \
  --context bootc_smoke=composefs-aarch64 \
  --fedora-copr-build <copr-build-id>:centos-stream-10-aarch64
```

Treat aarch64 composefs as covered only once a linked run of this request
passes with its install, reboot, `bootc status` and mount logs.
