# TMT integration tests

> This text is LLM generated.

In the bootc CI, integration tests are executed via Packit on the Testing Farm.

See [CONTRIBUTING.md](../CONTRIBUTING.md#running-tmt-integration-tests) for
instructions on running them locally.

## Opt-in boot automount regression lane

`just test-boot-automount-regression IMAGE` reuses the disposable bcvk/libvirt
runner and tmt connect provisioning. The plan is disabled unless the
`boot_automount_regression=true` tmt context is supplied. It consumes the exact
test from PR 106 at `e1f8132bb2719fcc3823859ae6f3c0965ab5e8cc`, without
copying or modifying that test or the generated integration plans.

The false-green risk is the test's successful skip on guests without a
`/boot` automount. This lane instead requires CentOS Stream 10, an ostree boot, an
active real autofs mount, and a `/boot` bind mount backed by the physical
root's `/boot` directory. It does not synthesize an automount on an unsupported
layout. A fresh guest must have no test marker, and the finish step requires
the rebooted `v2` marker, so the test's success/skip cannot pass the lane.
General-purpose integration plans retain their existing skips. Preparation
disables the normal update timer across the first reboot, leaving activation
to the test after its idle assertion. The recipe imposes a 45-minute outer
deadline (30-minute test plus provisioning allowance), since the imported
test's custom reboot command is not bounded by its tmt duration. The timeout
signals the process group, with a 30-second kill escalation. If the outer
deadline fires, inspect and remove any surviving VM with `bcvk libvirt rm`;
this is an infrastructure timeout, not an expected baseline failure.

Supply an already-built **test image** (including Nushell and the normal test
dependencies), not a stock CentOS image. Build the fixed image with
`just build-boot-automount-regression BASELINE_IMAGE FIXED_IMAGE`, then run
`just test-boot-automount-regression` separately against both images.
The focused build layers ostree and ostree-libs `2026.7-1.el10` from CentOS
Stream Koji onto the supplied test image, matching the pending
[c10s rebase MR 92](https://gitlab.com/redhat/centos-stream/rpms/ostree/-/merge_requests/92).
It rejects other distributions and verifies both installed package versions.
Use an affected pre-2026.7 baseline; do not apply the fixed-image build to it.
The test recipe resolves
the input once to a local immutable image ID. The image inspection output
records image IDs, repository digests (if present) and source labels;
builders must label the bootc and ostree source revisions. Preserve that output
along with `/var/tmp/tmt` (tmt results and bcvk journal/console logs). The test
logs idle mount evidence, timer activation, the finalization journal and the
post-reboot assertion; the plan adds initial layout and package evidence.
Inspect the baseline result: only failure at the expected update/finalization
assertion counts as a reproduction, never discovery, provisioning or prepare
failure. The fixed image must pass and boot `v2`.
Source labels are not validated by this recipe; the acceptance caller must
validate them against the selected baseline and reviewed fix revisions.

This is lane scaffolding, not completed regression acceptance. Hosted CI wiring
still needs a caller of the recipe with both pinned images and artifact upload.
A no-idle control also remains required: coordinate it through PR 106 rather
than rewriting that test here. The pinned revision unconditionally waits for
idle, so it cannot provide that control unchanged. Neither static metadata
checks nor historical VM runs establish acceptance of this revision.

For metadata validation with the CI-pinned tmt 1.80.0, use
`tmt -c boot_automount_regression=true plan lint --disable-check P005 boot-automount-regression`
and run the real discover step as well. P005 tries to clone archive URLs as
git repositories; discovery supports archives and must select exactly one test.
