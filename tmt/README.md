# TMT integration tests

In the bootc CI, integration tests are executed via Packit on the Testing Farm.

See [CONTRIBUTING.md](../CONTRIBUTING.md#running-tmt-integration-tests) for
instructions on running them locally.

## Provisioning paths

`cargo xtask run-tmt` boots the image under test in one of two ways, chosen
with `--provision`:

- `bcvk` (the default): install the image to a disk and boot it with bcvk.
- `reinstall`: boot a stock Fedora or CentOS Stream cloud image with tmt's
  `virtual` (testcloud) provisioner, then run `system-reinstall-bootc` in
  place and reboot into the image. It supports only ostree/BLS images
  without custom kernel arguments.

`just test-tmt-reinstall readonly` runs the readonly plan through the
`reinstall` path only. `just test-tmt-readonly` runs it through both, one
after the other, and prints one summary; it fails if either path fails.
With `BOOTC_variant=composefs`, it runs only the `bcvk` path.

## Host prerequisites for the reinstall path

In addition to what the `bcvk` path needs (except bcvk itself), the
`reinstall` path needs:

- tmt 1.79.0 or later with the `provision-virtual` extra, which pulls in
  testcloud, plus libvirt and QEMU. For example
  `pip install --user 'tmt[provision-virtual]==1.79.0'`, the version CI pins.
- `requests-cache` older than 1.3: with 1.3.3, testcloud 0.12.0 fails to look
  up the cloud image URL (`module 'requests_cache' has no attribute
  '__version__'`). For example `pip install --user 'requests-cache<1.3'`.
- `genisoimage`, which testcloud uses to build the cloud-init seed image.
  RHEL 10 doesn't ship it; a `genisoimage` wrapper in `PATH` that runs
  `pycdlib-genisoimage` (from the `pycdlib` Python package) also works, as
  long as it rewrites the `--long` options testcloud passes to the `-long`
  form, the only one `pycdlib-genisoimage` accepts:

  ```sh
  #!/bin/sh
  for a; do
      shift
      case "$a" in --*) a="${a#-}" ;; esac
      set -- "$@" "$a"
  done
  exec pycdlib-genisoimage "$@"
  ```
