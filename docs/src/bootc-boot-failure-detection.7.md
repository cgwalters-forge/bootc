# Upgrade/rollback failure detection in bootc

> This text is LLM generated.

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

An unbootable staged composefs deployment would otherwise remain the default
indefinitely. With systemd-boot, composefs supports opt-in
[boot counting](https://uapi-group.org/specifications/specs/boot_loader_specification/#boot-counting)
to fall back to the previous deployment after a configured number of attempts.
Write a positive `u32` attempt count (for example `3`) to `/etc/kernel/tries`;
`0` or an absent file disables counting. Malformed values are warned about and
ignored, not treated as staging failures. The file is read from the **running
host** at staging time, not from the target image. Shipping it in an image does
not count the first deployment of that image.

The target image must include `/usr/lib/systemd/systemd-bless-boot`. Staging
writes a Type-1 `.conf` entry with `+N`; systemd-boot renames it to `+N-M` as
attempts are consumed. This applies both to traditional kernel/initramfs BLS
entries and to Type-1 wrappers referencing UKIs. **The UKI binary is not
renamed.** After successful boot assessment, `systemd-bless-boot.service`
removes the counter. The image must arrange activation of
`boot-complete.target` and the blessing service; including the executable
alone does not establish a useful health assessment.

Entries with zero attempts left sort after non-exhausted entries, independently
of their sort keys. Before fallback, status reports a rollback queued; after
booting the previous deployment, it reports the failed deployment as the
rollback candidate with no rollback queued. Explicit `bootc rollback` rewrites
both entries **without counters**, including an exhausted entry. It does not
grant new counted retries. Initial installations are not counted because
there is nothing to fall back to. GRUB and Android aboot behavior are unchanged;
counting does not alter soft-reboot capability or policy. Use a full reboot to
exercise systemd-boot's attempt accounting.

Boot assessment is distinct from shutdown finalization failure reporting. This
tree does not include a composefs `bootc-boot-complete.service`. An assessment
unit must be both required by and ordered before `boot-complete.target` to
prevent blessing on failure; ordering alone is insufficient. Counting alone
does not detect a failed `bootc-finalize-staged.service` on the next boot. See
[Automatic Boot Assessment](https://systemd.io/AUTOMATIC_BOOT_ASSESSMENT/) for
health-check dependencies.

SELinux policy can prevent `systemd-bless-boot` (`init_t`) from renaming ESP
entries (`dosfs_t`). If blessing is denied, even healthy deployments exhaust
their attempts. The rebooting counting test uses a **test-guest-only** local
CIL module granting that rename. Its results do not establish stock-policy
readiness. Policy publication and default enablement are separate work.

### Rebooting regression test

From a development host with the VM prerequisites in `CONTRIBUTING.md`, run:

```bash
just test-composefs systemd ext4 bls unsealed composefs-boot-counting
just test-composefs systemd ext4 uki sealed composefs-boot-counting
```

These build the current working tree and use the rebooting TMT harness. The
test opts in with `/etc/kernel/tries=3`, checks healthy blessing, simulates
`+2-1` and exhausted `+0-3` renames, reboots to prove fallback, then explicitly
rolls back and checks garbage collection retains referenced boot artifacts.
It does not simulate three real failed boots or prove prevented blessing after
a failed assessment. Combined failed-finalization/prevented-blessing tests with
the separately staged boot-complete implementation remain an integration gate.
Record the tested head, guest configuration, commands and exit codes; historical
results on a different head are not proof for a refreshed build.

## See Also

- [systemd Automatic Boot Assessment](https://systemd.io/AUTOMATIC_BOOT_ASSESSMENT/)
- [OSTree Manual](https://ostreedev.github.io/ostree/)
- [bootc-rollback(8)](man/bootc-rollback.8.md)
- [bootc-status(8)](man/bootc-status.8.md)
