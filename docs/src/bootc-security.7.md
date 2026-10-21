# Security and threat model

This page describes bootc's trust boundaries and how to configure
stronger guarantees than the defaults. If you believe you've found a
valid vulnerability, see [SECURITY.md](https://github.com/bootc-dev/bootc/blob/main/SECURITY.md)
for how to report it.

## Privileges

bootc is a binary that is intended to be run with full privileges.
It does not expose a service (socket, HTTP endpoint) by default;
that's the role of higher level tooling.

All inputs (CLI arguments, etc) are hence considered fully trusted
by default.

## Container image verification

bootc honors the default `/etc/containers/policy.json` when fetching
images. The upstream default at the time of this writing
does not require signatures for generic images.

It is not a vulnerability in bootc that signatures are not required
by default.

It is however a very good idea for bootc users to enable signatures
for their images.

### Requiring signatures with a cosign key pair

One way to do this is to sign each image with a [cosign](https://github.com/sigstore/cosign)
key pair in CI, and ship a policy in the image itself that requires
that key for the image's repository.

It is the policy that does the verification. Every image fetch by
`bootc switch` and `bootc upgrade` goes through `containers-policy.json(5)`,
so once the policy requires a signature for a repository, unsigned or
wrongly signed images from it are refused. This applies to both the
ostree and the [composefs](bootc-composefs.7.md) backends.

With the ostree backend, the `--enforce-container-sigpolicy`
flag of [bootc-switch](man/bootc-switch.8.md) adds one guard on top:
bootc refuses to fetch at all if the policy's top-level `default` is
`insecureAcceptAnything` (which is the stock policy on some
distributions, including Fedora). The flag is recorded in the
origin (`ostree-image-signed:docker://...`), so later `bootc upgrade`
runs keep the guard. The composefs backend does not currently implement
this guard or record the flag, so there the policy file is the only
protection.

#### Files to ship in the image

For an image `quay.io/example/myos`, `/etc/containers/policy.json`
could look like this:

```json
{
  "default": [{ "type": "reject" }],
  "transports": {
    "docker": {
      "quay.io/example/myos": [
        {
          "type": "sigstoreSigned",
          "keyPaths": [
            "/etc/pki/containers/myos.pub",
            "/etc/pki/containers/myos-backup.pub"
          ],
          "signedIdentity": { "type": "matchRepository" }
        }
      ],
      "": [{ "type": "insecureAcceptAnything" }]
    },
    "containers-storage": { "": [{ "type": "insecureAcceptAnything" }] },
    "docker-archive": { "": [{ "type": "insecureAcceptAnything" }] },
    "docker-daemon": { "": [{ "type": "insecureAcceptAnything" }] },
    "oci": { "": [{ "type": "insecureAcceptAnything" }] },
    "oci-archive": { "": [{ "type": "insecureAcceptAnything" }] }
  }
}
```

The top-level `reject` default is what satisfies `--enforce-container-sigpolicy`;
the `""` entries keep ordinary `podman pull` of other images and local
transports working. A second (offline) key in `keyPaths` allows rotating
the signing key without a flag day; older versions of containers/image
only support a single `keyPath`. Cosign signatures carry only the
repository name, so `matchRepository` (or `exactRepository`) is the
`signedIdentity` to use. Note that this accepts any image ever signed
for that repository, including older ones; use a separate repository per
image if one image must not be accepted in place of another.

containers/image looks for sigstore signatures in the registry only when
`containers-registries.d(5)` enables it, so also ship e.g.
`/etc/containers/registries.d/myos.yaml`:

```yaml
docker:
  quay.io/example/myos:
    use-sigstore-attachments: true
```

Finally, put the public keys in `/etc/pki/containers/`. Use
`/etc/containers/policy.json` rather than `/usr/share/containers/policy.json`,
because the former takes precedence and is often present in base images.

#### Signing in CI

Sign by digest after pushing, so that the signature covers exactly the
image that was pushed:

```bash
cosign sign -y --key env://COSIGN_PRIVATE_KEY "quay.io/example/myos@${digest}"
```

As of this writing, containers/image only reads signatures stored in the
legacy `sha256-<digest>.sig` tag, not the sigstore bundle format that
cosign v3 produces by default (a fetch reports "A signature was required,
but no signature exists"). With cosign v3, add
`--new-bundle-format=false --use-signing-config=false`. Those flags are
deprecated, so pin the cosign version used for signing.

Keyless signing is not a good fit for host verification today: a
`fulcio` policy entry can only match the certificate's `subjectEmail`,
while certificates issued to CI workflows (such as GitHub Actions)
identify the workflow by a URI and carry no email.

#### Switching an existing system to the signed image

A system that is not yet running the signed image still has its old
policy, which is often the permissive default: the first fetch of the
new image would not be verified, and with the ostree backend
`bootc switch --enforce-container-sigpolicy` refuses to run at all.
Copy the same `policy.json`, registries.d file and public keys into the
system's `/etc` first, then switch:

```bash
bootc switch --enforce-container-sigpolicy quay.io/example/myos:latest
```

This way the very first fetch is verified. Because the files in `/etc`
are then identical to the ones in the image, they are not treated as
local modifications, and later changes to them in the image apply on
upgrade.

With the ostree backend, always pass `--enforce-container-sigpolicy`
to `bootc switch`. A switch without it writes an origin without the
guard again: the policy itself still applies, but bootc no longer
refuses to fetch if the policy is later changed back to a permissive
default. See also [issue #815](https://github.com/bootc-dev/bootc/issues/815).

For `bootc install`, see `enforce-container-sigpolicy` in
[bootc-install-config](man/bootc-install-config.5.md).

## On-disk integrity vs. pull-time verification

The default backend is ostree (with a composefs mount), but
fsverity is not enabled by default.

The composefs backend enables fsverity by default if available,
but does not verify the composefs image (because there's nothing
to verify it against by default).

The composefs backend with sealed UKIs does verify the composefs
mount. With composefs + sealed UKIs, it is a vulnerability if
e.g. an attacker can mutate a file in the image store, have
that persist across a re-mount without it resulting in an `EIO`
error (default for Linux kernel fsverity).
