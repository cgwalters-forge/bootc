# Upgrade/rollback failure detection in bootc

This document describes how to detect when a reboot failed to enable the staged image in bootc.

## Overview

bootc uses different mechanisms to detect boot failures depending on the backend (OSTree vs. composefs+UKI) and the specific point of failure. Understanding these mechanisms is crucial for system administrators and automated tooling that needs to detect failed updates.

## OSTree Backend Boot Failure Detection

For systems using the traditional OSTree backend, bootc relies on OSTree's built-in boot failure detection mechanisms.

### Key Services

1. **`ostree-finalize-staged.service`** - Runs during shutdown to finalize staged deployments
2. **`ostree-boot-complete.service`** - Runs early in boot to detect finalization failures

When `ostree-finalize-staged.service` fails during shutdown/reboot, this will create
a stamp file in `/boot`, and then on a subsequent reboot the `ostree-boot-complete.service`
service will detect it, and then itself exit with a failure mode.

You can monitor the success of both services, though for `ostree-finalize-staged.service`
note that the failure occurred during the previous boot's shutdown.


## Composefs Backend Boot Failure Detection

### Key Services

There is a `bootc-finalize-staged.service` which is similar to `ostree-finalize-staged.service`,
but there is not currently a similar `-boot-complete.service`. There is also a `bootc-root-setup.service`
that runs during initramfs to mount the composefs image and set up `/etc` and `/var` - but if this
service fails, the system will not boot at all (emergency mode or hang).

At the current time then, it is recommended to check the journal for failures from the previous boot:

```bash
# Check for finalization failures from previous boot
journalctl -u bootc-finalize-staged.service -b -1
```

### Systemd Boot Assessment Integration

As of a recent OSTree with [this commit](https://github.com/ostreedev/ostree/commit/08487091256b93493f8d692e37ab3d892c758da1)
it is possible to configure the boot loader entry counting.

With systemd-boot, the composefs backend supports
[boot counting](https://uapi-group.org/specifications/specs/boot_loader_specification/#boot-counting).
As with `kernel-install`, it is enabled by writing the number of boot attempts
to `/etc/kernel/tries`; `0` or no file turns it off. The file is read from the
booted system when a deployment is staged, so when it is shipped in the
container image, the first deployment of an image with it isn't counted yet.
The boot entry of a staged deployment then gets that many attempts
(e.g. `bootc_fedora-42-1+3.conf`), and once the new deployment reaches
`boot-complete.target`, `systemd-bless-boot.service` marks it good. If it fails
to get there on every attempt, systemd-boot boots the previous deployment
instead, and `bootc status` shows the failed deployment as the rollback. Units
that must succeed for a boot to count as good can be ordered before
`boot-complete.target`; see
[Automatic Boot Assessment](https://systemd.io/AUTOMATIC_BOOT_ASSESSMENT/).
The target image must ship `systemd-bless-boot`. The initial deployment written
by `bootc install`, and entries rewritten by `bootc rollback`, are not counted.

Note that the SELinux policy in current Fedora and CentOS Stream releases does not
allow `systemd-bless-boot` (running as `init_t`) to rename entries on the ESP
(`dosfs_t`), so without a local policy module granting that, new deployments
are never marked good, and after N boots systemd-boot falls back to the
previous deployment.

GRUB has no support for boot counting, so it is not enabled there.

## See Also

- [systemd Automatic Boot Assessment](https://systemd.io/AUTOMATIC_BOOT_ASSESSMENT/)
- [OSTree Manual](https://ostreedev.github.io/ostree/)
- [bootc-rollback(8)](man/bootc-rollback.8.md)
- [bootc-status(8)](man/bootc-status.8.md)
