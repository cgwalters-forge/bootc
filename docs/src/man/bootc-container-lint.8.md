# NAME

bootc-container-lint - Perform relatively inexpensive static analysis
checks as part of a container build

# SYNOPSIS

**bootc container lint** \[*OPTIONS...*\]

# DESCRIPTION

Perform relatively inexpensive static analysis checks as part of a
container build.

This is intended to be invoked via e.g. `RUN bootc container lint` as
part of a build process; it will error if any problems are detected.

# OPTIONS

<!-- BEGIN GENERATED OPTIONS -->
**--rootfs**=*ROOTFS*

    Operate on the provided rootfs

    Default: /

**--fatal-warnings**

    Make warnings fatal

**--list**

    Instead of executing the lints, just print all available lints. At the current time, this will output in YAML format because it's reasonably human friendly. However, there is no commitment to maintaining this exact format; do not parse it via code or scripts

**--skip**=*SKIP*

    Skip checking the targeted lints, by name. Use `--list` to discover the set of available lints

**--no-truncate**

    Don't truncate the output. By default, only a limited number of entries are shown for each lint, followed by a count of remaining entries

<!-- END GENERATED OPTIONS -->

# NOTES

When run under QEMU user-mode emulation (e.g. building an image for a
foreign architecture via binfmt_misc) with QEMU older than 9.2, some
lints can fail with errno 38 (ENOSYS). This is because bootc uses the
`openat2` system call (via cap-std-ext), which those QEMU versions do
not implement.
Use QEMU 9.2 or newer, or run the lint natively on the target
architecture. See <https://github.com/bootc-dev/bootc/issues/1481>.

# VERSION

<!-- VERSION PLACEHOLDER -->

