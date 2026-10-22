# Booted tests

These are intended to run via tmt.

## Verified composefs migration coverage

The current migration verification is the explicit `currenttest49` environment
with two independent old-stager modes: `unsealed` and `strict`. The old side is
the `bootc` 1.16.0 UKI fixture; the current side is `bootc` 1.16.13. This UKI
fixture result must not be generalized to all historical versions or to BLS
migration.

The bridge assertions cover old V2 → current V2 fallback → subsequent current
V1 repository handling, preservation of `/etc` and `/var`, rollback, and GC.
They also verify that the unsealed command line uses `?`, while strict mode
does not, and that the policy is preserved. An unsealed UKI is not a strict,
unsigned UKI: `?` permits missing fsverity; strict mode requires fsverity.

The reproducible test environment is:

```text
environment: currenttest49
old-stager mode: unsealed | strict
old UKI fixture: bootc 1.16.0
current bootc: 1.16.13
```

Run the relevant generated tmt plan from the repository root with the normal
project runner, selecting the explicit environment and one mode at a time;
do not combine the modes into one result. The strict plan-23 source-fresh run
and the earlier source-fresh runs that exercised both initial policies are the
recorded verification scope. In the verification artifact tree, the two
runtime logs are:

```text
target/verification-20260917/bridge-old-stager-unsealed-current-sourcefresh
target/verification-20260917/bridge-old-stager-strict-current-sourcefresh-correct-old
```

The final strict plan-23 source-fresh result is recorded at
`target/verification-20260917/plan23-strict-target-policy-sourcefresh`; the
earlier runs also tested both initial policies against the actual target. Test
output is kept in the verification run artifacts rather than copied into
general user documentation.

For scoped provenance, the verified current source diff is `dacff6e…`, the
current bootc binary SHA-256 is
`58ff9b06fc2f782d394ad08a05bca91c99436375156bdbbe9284b00ba438580d`, and the
old 1.16.0 fixture artifact SHA-256 is
`3c50ecb87835fe09e60c8ef49361754c3c590941fd6760ffffd61e2e0d4b2554`.
These are artifact/source provenance values, not OCI manifest digests.
