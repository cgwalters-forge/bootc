# Booted tests

> This text is LLM generated.

These are intended to run via tmt.

## Composefs EROFS V1/V2 regression coverage

The existing `BOOTC_erofs_version` configuration selects the format while the
image is built and is forwarded to tmt by `just test-tmt-nobuild`. It is not
an install flag: installation consumes the UKI already present in the image.
Use the same control for the base and synthetic upgrade images:

```console
export BOOTC_variant=composefs BOOTC_bootloader=systemd
export BOOTC_boot_type=uki BOOTC_seal_state=sealed
BOOTC_erofs_version=v1 just test-tmt readonly image-upgrade-reboot
BOOTC_erofs_version=v2 just test-tmt readonly image-upgrade-reboot
```

`readonly/046-test-erofs-version.nu` checks the booted root identity. Its V1
assertion requires the V2 fallback argument, and explicit V2 requires only the
legacy argument. This is a format regression check, not an old-client bridge
test. `image-upgrade-reboot` builds its derived UKI with the same selected
format and verifies current-client-to-current-client upgrade behavior. The
default `v1` case is dual-format (V1 then V2); it is not an initramfs
capability probe.

`composefs-1-16-bridge` is opt-in historical-client coverage, restricted to
the bootc 1.16.0 old-stager fixture. Build and run it on an x86_64 host with
Podman, /dev/fuse, bcvk, libvirt/KVM, virtiofsd and tmt:

```console
just test-composefs-bridge sealed
just test-composefs-bridge unsealed
```

The recipe selects Fedora 44 base/buildroot images, sealed ext4 or unsealed
xfs, and current dual-format UKIs (`erofs_version=v1`), independently of the
normal build defaults. Image names include the mode so the two runs do not
overwrite each other's fixtures. The stager uses the checksum-pinned Fedora
x86_64 bootc 1.16.0 RPM from Koji (built for fc45), removes the current
version-locked subpackages with dependency checks, restores the legacy
`prepare-root.conf`, and regenerates its initramfs before creating the UKI.
The RPM and binary hashes are in the Justfile; if Koji prunes the RPM, mirror
the exact bytes rather than silently changing the historical client.

Test-49 remains disabled by default. The recipe explicitly enables it with
`composefs_bridge=true` and uses the plan's read-only bound image storage,
overriding `BOOTC_skip_bind_storage`. No image copying or SCP is involved.
**Bridge CI integration remains outstanding.** These commands are an
on-demand prerequisite, not evidence of CI coverage or successful VM boots.

The covered path is an old-stager migration where
the old client is followed by current bootc and a newly generated initramfs.
bootc 1.16.0 stages the V2 fallback of the dual-digest UKI; 1.16.4 and later
clients would stage V1 directly, which this test does not cover.
Both sealed and unsealed UKIs have test cases; to use prebuilt fixtures:

```console
cargo xtask run-tmt "$BOOTC_1160_STAGER_IMAGE" composefs-1-16-bridge \
  --skip-bind-storage=false --filesystem ext4 \
  --composefs-backend --bootloader systemd --boot-type uki --seal-state sealed \
  --context composefs_bridge=true \
  --env BOOTC_composefs_bridge_integrity_mode=sealed \
  --env BOOTC_1160_bootc_sha256="$BOOTC_1160_BOOTC_SHA256" \
  --bridge-image "$BOOTC_CURRENT_DUAL_UKI_IMAGE" \
  --upgrade-image "$BOOTC_CURRENT_DUAL_UKI_UPGRADE_IMAGE"

cargo xtask run-tmt "$BOOTC_1160_STAGER_IMAGE" composefs-1-16-bridge \
  --skip-bind-storage=false --filesystem xfs \
  --composefs-backend --bootloader systemd --boot-type uki --seal-state unsealed \
  --context composefs_bridge=true \
  --env BOOTC_composefs_bridge_integrity_mode=unsealed \
  --env BOOTC_1160_bootc_sha256="$BOOTC_1160_BOOTC_SHA256" \
  --bridge-image "$BOOTC_CURRENT_DUAL_UKI_UNSEALED_IMAGE" \
  --upgrade-image "$BOOTC_CURRENT_DUAL_UKI_UPGRADE_UNSEALED_IMAGE"

```

Required fixture labels are `bootc.test.fixture=bootc-1.16.0-stager`,
`bootc.test.fixture=current-dual-uki-sealed`, and
`bootc.test.fixture=current-dual-uki-upgrade-sealed` for the sealed run.
The old-stager label is intentionally unchanged in both runs; its booted
status image is read from `.status.booted.image.image.image`.
The unsealed run requires separately built fixtures labeled
`bootc.test.fixture=current-dual-uki-unsealed` and
`bootc.test.fixture=current-dual-uki-upgrade-unsealed`; it never reuses a
strict fixture. The caller supplies the matching pullspecs through the
variables above.
For the old-stager case it also supplies the required
`BOOTC_1160_BOOTC_SHA256` value from the pinned fixture build; the test records
the exact `bootc --version`, RPM NEVRA, and `/usr/bin/bootc` checksum before it
stages the bridge image.

The bridge and upgrade fixtures must be independently generated with current
userspace and a newly generated dual-format UKI/initramfs for the requested
integrity mode. The sealed run must use Secure Boot firmware; the unsealed run
uses the explicitly insecure firmware context selected by `--seal-state
unsealed`. The test validates the image labels, the `?` marker in the V2 UKI
argument, and `missingVerityAllowed` in bootc status rather than treating
unsealed as a relaxed version of the sealed fixture.

The test checks the public status schema, `/proc/cmdline`, repository image
and deployment-state directories, and `/etc` and `/var` sentinels. There is no
stable public inspection API that labels an on-disk EROFS image as V1 or V2
independently of its UKI argument, so it does not infer that from filenames.
It performs rollback and `composefs-gc --assert-no-op` only after booting a
current client; no old 1.16 rollback or GC command is assumed.

This intentionally does not cover stale initramfs, old-initramfs-mode, or BLS
installs. The test is opt-in through `composefs_bridge=true`, so these large
fixtures are not added to the normal matrix.
