# NAME

bootc-destructive-cleanup.service

# DESCRIPTION

This systemd service runs on first boot after an "alongside" installation
using `bootc install to-existing-root --cleanup`. Its purpose is to clean up
files from the previous operating system.

The service runs as a oneshot unit and executes a distribution-specific cleanup
script located at `/usr/lib/bootc/fedora-bootc-destructive-cleanup` (for Fedora
derivatives).

## How it works

1. During `bootc install to-existing-root --cleanup`, a stamp file is created
   at `/sysroot/etc/bootc-destructive-cleanup`
2. A systemd generator (`bootc-systemd-generator`) detects this stamp file at
   boot time and enables the `bootc-destructive-cleanup.service` unit
3. The service runs the cleanup script on first boot

## What the cleanup script does

On Fedora derivatives, the cleanup script performs the following actions:

- Remounts `/sysroot` as read-write
- If the previous OS's `/etc/fstab` mounts `/var` from a separate block
  device or btrfs subvolume, mounts it at `/sysroot/var`, visible only to
  the service, so that package scriptlets find the previous OS's state
- Removes all RPM packages installed in the physical root (the previous OS)
- Removes all container images from `/sysroot/var/lib/containers` using
  `podman system prune --all -f`, if that directory exists

Whenever the script leaves the previous `/var` unmounted (for example
because the previous `/etc/fstab` has no `/var` entry, its source cannot be
resolved or is not a block device, or the mount fails) or skips the container
image prune, it prints one line saying why. Look for these in the journal of
the unit (`journalctl -u bootc-destructive-cleanup.service`) if storage of the
previous OS was not reclaimed.

**Note:** The cleanup script does not remove stopped containers, so some storage
may remain. This behavior may change in the future.

# CUSTOMIZING THE CLEANUP SCRIPT

The current implementation ships a Fedora-specific cleanup script. Other
distributions can provide their own cleanup script by creating an executable
at `/usr/lib/bootc/fedora-bootc-destructive-cleanup` or by modifying the
systemd unit file to reference a different path.

For an example implementation, see the
[Fedora cleanup script](https://github.com/bootc-dev/bootc/blob/main/contrib/scripts/fedora-bootc-destructive-cleanup).

# PREVIOUS FILESYSTEM DATA

After an alongside installation, the previous root filesystem data is accessible
at `/sysroot` (the "physical root"). Previous mount points or subvolumes will
not be automatically mounted in the new system; for example, a btrfs subvolume
for /home will not be automatically mounted to /sysroot/home. These filesystems
persist and can be handled manually or defined as mount points in the bootc image.
The cleanup service mounts the previous `/var` only for its own duration.

# SEE ALSO

**bootc**(8), **bootc-install-to-existing-root**(8), **system-reinstall-bootc**(8)

# VERSION

<!-- VERSION PLACEHOLDER -->
