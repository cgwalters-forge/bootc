# composefs backend

Experimental features are subject to change or removal. Please
do provide feedback on them.

## Overview

The composefs backend is an experimental alternative storage backend that uses [composefs-rs](https://github.com/composefs/composefs-rs) instead of ostree for storing and managing bootc system deployments.

**Status**: Experimental, but close to stabilization. The migration described
below is verified for the tested `bootc` 1.16.0 UKI fixture. It is not a claim
about every historical bootc release or every BLS layout.

The composefs backend has two independent integrity properties:

- **Secure Boot provenance**: A signed UKI lets firmware authenticate the
  kernel, initramfs, and command line it contains. This is what is meant by a
  sealed UKI. A traditional BLS entry is not cryptographically sealed and does
  not provide equivalent provenance or root authenticity, even when it carries
  a composefs digest.
- **fsverity strictness**: composefs can require fsverity on filesystems that
  support it. The composefs digest is checked against the mounted root in this
  mode. The `?` marker in the composefs command line (selected by
  `--allow-missing-verity`) permits missing fsverity; it does not add Secure
  Boot provenance. This setting applies to BLS boots as well as UKI boots.

Thus, a BLS deployment can enforce fsverity on a supported filesystem without
being a cryptographically sealed image. Conversely, putting an image in a UKI
does not by itself make fsverity strict: Secure Boot provenance and fsverity
strictness must be evaluated separately.

## Storage and repository structure

Unlike the ostree backend, which keeps its repository at `/ostree/repo`, the composefs backend splits its on-disk state across two top-level directories in the physical sysroot:

- `/composefs`: The [composefs-rs repository](https://github.com/composefs/composefs-rs/blob/main/crates/composefs/src/repository_format.rs) (mode `0700`), containing:
  - `objects/`: content-addressed file storage, keyed by SHA-512 fsverity digest and shared via reflink (`FICLONE`) where the filesystem supports it
  - `images/`: EROFS images describing each deployment's root filesystem metadata
  - `streams/`: OCI manifest, config, and layer splitstreams captured during image pulls
  - `bootc/storage/`: the `containers-storage:` instance backing logically bound images, reflink-shared with the composefs object store
- `/state/deploy/<deployment-id>/`: Persistent per-deployment state, one directory per deployment (named after its composefs digest):
  - `etc/`: a writable copy of the deployment's `/etc`, bind-mounted onto the booted root's `/etc`
  - `var`: a symlink to the shared `/state/os/default/var`, bind-mounted onto the booted root's `/var`
  - `<deployment-id>.origin`: an INI file recording the image reference, boot type (BLS or UKI) and digest, and the OCI manifest digest (the latter is what keeps a deployment's objects alive across garbage collection)

Although composefs-rs supports other fsverity hash algorithms, bootc currently hardcodes `SHA-512` for the repository (see `Algorithm::SHA512` at every repository init/open call site, and the `ComposefsRepository` type alias in `crates/lib/src/store/mod.rs`). This is why deployment and object identifiers throughout this document (and in `bootc status`) are 128-character hex strings.

There is no `/ostree/repo`; the composefs backend doesn't use the ostree repository at all. A minimal `/ostree` directory is still created, but only to hold a compatibility symlink (`ostree/bootc -> ../composefs/bootc`) so that existing tooling expecting `/usr/lib/bootc/storage` to resolve through `ostree/bootc` keeps working.

Transient, not-yet-finalized deployment state (used while staging an update before reboot) lives under `/run/composefs/staged-deployment` and is never persisted to disk.

## Migrating an existing deployment

The supported migration procedure is to update the image builder first. Add
the current `bootc` to the image, regenerate its initramfs, and, for a sealed
image, regenerate the UKI and sign it again. Do not deploy a new image while
still relying on an initramfs produced by an older image builder; that is not a
supported migration mode.

On the machine, use the existing (older) `bootc` to stage the new image, then
reboot into it. For example, replace the placeholders with the image reference
you built and the registry transport appropriate to your deployment:

```console
# Run on the existing deployment, before rebooting:
bootc switch --transport <TRANSPORT> <NEW-IMAGE-REFERENCE>
systemctl reboot
```

After the first boot into the new image, perform a normal subsequent update:

```console
bootc upgrade
systemctl reboot
```

The verified bridge preserved `/etc` and `/var`, allowed rollback, and allowed
garbage collection. It migrated an old EROFS V2 repository while the current
boot used V2, fell back to the current V2 reader, and then handled a subsequent
current V1 repository. Both the unsealed (`?`) and strict fsverity policies
were tested independently. An unsealed UKI (`?`) is not the same thing as a
strict, unsigned UKI: `?` permits missing fsverity, while strict mode requires
it; neither statement supplies Secure Boot provenance.

The exact UKI migration test covered the `bootc` 1.16.0 fixture only. BLS
initial-policy behavior has been tested and is documented by the independent
fsverity policy description above, but historical BLS migration remains
pending additional evidence.

## How Sealed Images Work

A sealed image is a cryptographically signed and verified bootc image that provides end-to-end integrity protection. This is achieved through:

- **Unified Kernel Images (UKIs)**: Combining kernel, initramfs, and boot parameters into a single signed binary
- **Composefs integration**: Using composefs with fsverity for content-addressed filesystem verification
- **Secure Boot**: Cryptographic signatures on both the UKI and systemd-boot loader

The Secure Boot signature authenticates the UKI and the command line it carries;
fsverity strictness separately determines whether the mounted root must have
the expected verity metadata. Both checks are needed for the full sealed-UKI
chain described below, but neither check should be used as a substitute for
the other.

A sealed image includes:

1. **composefs digest**: A SHA-512 hash of the entire root filesystem, computed at build time
2. **Unified Kernel Image (UKI)**: A single EFI binary containing the kernel, initramfs, and kernel command line with the composefs digest embedded
3. **Secure Boot signature**: The UKI is signed with your private key

At boot time, the composefs digest in the kernel command line (e.g., `composefs=<sha512-hash>`) is verified against the mounted root filesystem. This creates a chain of trust from firmware to userspace, ensuring the system will only boot if the root filesystem matches exactly what was signed.

## Building Sealed Images

### Prerequisites

For sealed images, the container must:

- Include a kernel and initramfs in `/usr/lib/modules/<kver>/`
- Have systemd-boot available (and NOT have `bootupd`)
- Not include a pre-built UKI (the build process generates one)

Sealed images also require:

- Secure Boot support in the target system firmware
- A filesystem with fsverity support (e.g., ext4, btrfs) for the root partition

#### Using without Secure Boot

You can use a sealed UKI without Secure Boot enabled. The composefs and mounting
code is fully orthogonal to Secure Boot - the fsverity digest of the root filesystem
and all of its contents will still be validated at runtime, which does provide
an increased level of integrity.

However: nothing validates that root digest itself, meaning any locally running
code can replace the UKI (e.g. after a container breakout) and fully control
the next boot.

It is intentional to support booting with Secure Boot disabled, because a
valid use case is to temporarily disable it in order to test a change locally
on e.g. one machine, then re-enable it later. However at the current time it
is not yet streamlined to regenerate the UKI locally.

Note this is a different, independent weakening from
`--allow-missing-verity` (see [Overview](#overview) above): disabling Secure
Boot removes firmware verification of the UKI's signature, while the fsverity
digest of the root filesystem can remain enforced. Building with
`--allow-missing-verity` instead permits missing fsverity, including when
Secure Boot remains enabled.

### Build Pattern: Split the Kernel, Then Generate the UKI in a Separate Stage

Building a sealed image involves three stages: build the rootfs, split the kernel and initramfs out of it, and generate the signed UKI from the split rootfs in a tools stage:

```dockerfile
# Build your rootfs with all packages and configuration
FROM <base-image> as rootfs
RUN apt|dnf|zypper install ... && bootc container lint --fatal-warnings

# Split the kernel and initramfs out of the rootfs. This moves
# /usr/lib/modules/<kver>/{vmlinuz,initramfs.img} into /kernel/<kver>/,
# since for a sealed image they end up embedded in the UKI instead.
FROM rootfs as split
RUN mkdir /kernel && bootc container split-kernel-and-rootfs --rootfs / --output /kernel

# Generate the sealed UKI in a tools stage
FROM <tools-image> as sealed-uki
RUN --mount=type=bind,from=split,target=/target \
    --mount=type=bind,from=split,source=/kernel,target=/kernel \
    --mount=type=secret,id=secureboot_key \
    --mount=type=secret,id=secureboot_cert <<EORUN
set -euo pipefail

mkdir -p /out
kver=$(ls /kernel)

# `bootc container ukify` computes the composefs digest of /target, reads
# extra kernel arguments from /target/usr/lib/bootc/kargs.d, and invokes the
# real `ukify` binary with the digest embedded in the cmdline. Everything
# after `--` is passed straight through to ukify.
bootc container ukify \
  --rootfs /target \
  --kernel-dir "/kernel/${kver}" \
  -- \
  --output "/out/${kver}.efi" \
  --signtool sbsign \
  --secureboot-private-key /run/secrets/secureboot_key \
  --secureboot-certificate /run/secrets/secureboot_cert
EORUN

# Final image: the split rootfs (kernel/initramfs already removed) plus the signed UKI
FROM split
COPY --from=sealed-uki /out/*.efi /boot/EFI/Linux/
```

This pattern works because:

1. `bootc container split-kernel-and-rootfs` removes the raw kernel and initramfs from the rootfs ahead of time, so the final image never carries a duplicate copy of them (they end up embedded in the UKI instead)
2. `bootc container ukify` handles computing the composefs digest and assembling the kernel command line, so you only need to pass `ukify`-specific options (like signing) after `--`
3. The final stage copies the signed UKI into the already-split rootfs

### The `bootc container ukify` Command

```bash
bootc container ukify --rootfs <PATH> [OPTIONS] -- [UKIFY_ARGS...]
```

This is the recommended way to build a UKI for a bootc image. It computes the composefs digest of `--rootfs` (using the lower-level `compute-composefs-digest` primitive described below), reads extra kernel arguments from `/usr/lib/bootc/kargs.d`, and invokes the system `ukify` binary with the resulting cmdline. Anything after `--` is forwarded to `ukify` unchanged (e.g. `--output`, `--signtool`, signing key/cert options).

**Options:**

- `--rootfs <PATH>`: Root filesystem to operate on (default: `/`)
- `--kernel-dir <PATH>`: Directory containing `vmlinuz`/`initramfs.img`, named `/parent/<kernel-version>`. Needed when the kernel has already been split out of `--rootfs`, e.g. via `split-kernel-and-rootfs`
- `--allow-missing-verity`: Make fsverity validation optional, for filesystems that don't support it (e.g. XFS)
- `--write-dumpfile-to <PATH>`: Write a composefs dumpfile for debugging

### The `bootc container compute-composefs-digest` Command

```bash
bootc container compute-composefs-digest [PATH]
```

A lower-level primitive, used internally by `ukify` above, that computes just the composefs digest for a filesystem without building a UKI. The digest is a 128-character SHA-512 hex string that uniquely identifies the filesystem contents. Useful for scripting or debugging outside of the UKI build flow.

**Options:**

- `PATH`: Path to the filesystem root (default: `/target`)
- `--write-dumpfile-to <PATH>`: Generate a dumpfile for debugging

> **Note**: This command is currently hidden from `--help` output as it's part of the experimental composefs feature set.

### Final Image Structure

The sealed image should have:

- The signed UKI at `/boot/EFI/Linux/<kver>.efi`
- A signed systemd-boot at `/boot/EFI/BOOT/BOOTX64.EFI` and `/boot/EFI/systemd/systemd-bootx64.efi`
- The raw `vmlinuz` and `initramfs.img` removed from `/usr/lib/modules/<kver>/` (they're now embedded in the UKI)

### External Signing Workflow

For production environments with dedicated signing infrastructure:

1. **Build unsigned UKI**: Compute digest and create an unsigned UKI (omit `--signtool` from ukify)
2. **Sign externally**: Take the unsigned UKI to your signing infrastructure
3. **Complete the seal**: Inject the signed UKI into the final image

This workflow is planned for streamlining in future releases (see [#1498](https://github.com/bootc-dev/bootc/issues/1498)).

## Developing and Testing bootc with composefs

See [CONTRIBUTING.md](https://github.com/bootc-dev/bootc/blob/main/CONTRIBUTING.md) for information on building and testing bootc itself with composefs support.

## Bootloader Support

Whenever the container image has a UKI, bootc automatically selects the composefs backend during installation (see [Prerequisites](#prerequisites) above for the currently-supported UKI + systemd-boot configuration). A UKI can provide Secure Boot provenance, but having one does not by itself determine fsverity strictness; see [Overview](#overview).

Composefs installs using a traditional `vmlinuz`/`initramfs.img` layout use a
BLS entry and are not cryptographically sealed like signed UKIs. They can
nevertheless enforce fsverity on a supported filesystem; `?` explicitly
permits missing fsverity. Such installs can use either `bootupd` (GRUB) or
systemd-boot, the same as the ostree backend. See [bootloaders.md](bootloaders.md)
for the general bootloader selection rules. Under the hood, bootc writes
standard BLS boot entries for both UKI and traditional kernels; see the
[composefs boot module documentation](https://github.com/bootc-dev/bootc/blob/main/crates/lib/src/bootc_composefs/boot.rs)
for details on how entry filenames and sort-keys are chosen to sort correctly
on both GRUB and systemd-boot.

## Installation

There is a `--composefs-backend` option for `bootc install` to explicitly select a composefs backend apart from sealed images; this is not as heavily tested yet.

## Known Issues

The composefs backend is experimental; on-disk formats are subject to change.

### Stability blockers

- [Dual EROFS v1/v2 generation](https://github.com/bootc-dev/bootc/pull/2248) and https://github.com/bootc-dev/bootc/pull/2353

### Important

- Extended install APIs: Ability to cleanly implement anaconda %post and osbuild post mutations and general post-install pre-reboot; right now some tools just mount the deployment directory (note this one also relates to [APIs in general](https://github.com/bootc-dev/bootc/issues/522))
- [zstd:chunked pull failures](https://github.com/bootc-dev/bootc/issues/2408): Images pushed with `--compression-format zstd:chunked` currently fail to pull on the composefs backend ("unexpected EOF reading tar entry"). A [decode fix](https://github.com/composefs/composefs-rs/pull/381) is in flight in composefs-rs and reaches bootc with the next composefs-rs update; until then publishers should use plain zstd (or gzip).

## Related issues

- [Unified storage](https://github.com/bootc-dev/bootc/issues/20): Not strictly a blocker but a really nice to have
- [Sealed image build UX](https://github.com/bootc-dev/bootc/issues/1498): Streamlined tooling for building sealed images
- In place transitions: 
  - First: support [factory reset](https://github.com/bootc-dev/bootc/issues/404) from ostree to composefs
  - Next: Support copying /etc and /var

## Additional Resources

- See [filesystem.md](filesystem.md) for information about composefs in the standard ostree backend
- See [bootloaders.md](bootloaders.md) for bootloader configuration details
- [composefs-rs](https://github.com/composefs/composefs-rs) - The underlying composefs implementation
- [composefs-rs repository format](https://github.com/composefs/composefs-rs/blob/main/crates/composefs/src/repository_format.rs) - Detailed on-disk layout of the `/composefs` repository
- [Unified Kernel Images specification](https://uapi-group.org/specifications/specs/unified_kernel_image/)
- [ukify documentation](https://www.freedesktop.org/software/systemd/man/latest/ukify.html) - Tool for building UKIs
