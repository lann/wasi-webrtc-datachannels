# Conformance test suite

A conformance suite for the `lann:webrtc-datachannels` implementations. It runs
the **same wasm conformance guest component** against each target and asserts
**WIT-observable interoperable behavior only** — never SDP contents, candidate
ordering, timing, or exact error strings.

See [`PLAN.md`](PLAN.md) for the full design and the phased implementation plan.
This suite is being built one phase at a time; the sections below describe what
exists today.

## Status

Phase 0 (scaffolding & registry) is in place: the test registry, the manifest
schema, and the `conformance-runner` that aggregates adapter results, applies
expected-fail policy, and renders the matrix. **No targets are enabled yet**, so
the suite runs green over an empty target set. Adapters, the signaling server,
and the conformance guest arrive in later phases.

## Running

From the repository root:

```sh
just conformance
```

This invokes `conformance-runner`, which reads the test registry and any
per-target manifests, aggregates adapter result documents (none yet), applies
the expected-fail / unexpected-pass policy, and writes the markdown matrix to
`conformance/matrix.md`. It exits nonzero on any `fail` or `unexpected-pass`.

Run the runner's own unit tests with the rest of the workspace:

```sh
just test
```

## Layout

```
conformance/
  PLAN.md                  # design + phased plan
  README.md                # this file
  tests.toml               # test registry: id, tags, description
  manifests/               # per-target capability manifests (<target>.toml)
    example.toml.example   #   template (NOT loaded; see below)
  runner/                  # conformance-runner (Rust workspace member)
  signaling/               # signaling server + protocol (Phase 1)
  wit/                     # conformance WIT (Phase 2); deps symlink to root wit/
  guest/                   # conformance guest component(s) (Phase 2)
  adapters/                # per-target adapters (wasmtime / jco / wasip3)
  scenarios/               # ICE lab provisioning (Phase 5+)
```

## Test registry (`tests.toml`)

Every conformance test is declared once in [`tests.toml`](tests.toml) with a
stable `id`, a set of `tags`, and a one-line `description`. The conformance
guest mirrors these ids/tags via its `list-tests` export (Phase 2). See the
comments at the top of the file for the schema. Grow the corpus by adding
`[[test]]` entries; keep tags stable across growth so manifests remain valid.

## Capability manifests (`manifests/<target>.toml`)

Each target gets one manifest declaring:

- `[[unsupported]]` entries referencing **tags** — every matching test is
  reported `skip-unsupported` (visible in the matrix, never a failure). A
  mandatory `reason` explains why.
- `[[expected-fail]]` entries referencing **test ids** — a known divergence that
  keeps the run green while staying visible. A mandatory `tracking` reference
  (e.g. a `TODO.md` item) records the follow-up. An expected-fail that
  **passes** becomes `unexpected-pass` and **fails** the run, forcing the
  manifest to be cleaned up.

See [`manifests/example.toml.example`](manifests/example.toml.example) for the
full schema. That template has a `.toml.example` extension so the runner (which
loads only `*.toml`) does not treat it as an enabled target.

## Reading the matrix

`conformance-runner` renders a markdown table with one row per target and one
column per test. Cell values:

| Symbol | Meaning |
| --- | --- |
| `pass` | Passed and expected to pass. |
| `FAIL` | Failed and not expected to — **fails the run**. |
| `skip` | Skipped: the target's manifest declares the test's tag unsupported. |
| `xfail` | Expected-fail: failed as the manifest predicted (does not fail the run). |
| `UNEXPECTED-PASS` | An expected-fail that passed — **fails the run**; update the manifest. |
| `—` | No adapter reported a result for this (target, test). |

The runner exits nonzero if any cell is `FAIL` or `UNEXPECTED-PASS`.

## Adding a target

A new target = a new adapter (under `adapters/`) that emits a JSON result
document plus a new manifest (under `manifests/`). No change to the runner or
the registry is required. Result document shape:

```json
{ "target": "wasmtime", "environment": "loopback",
  "results": [ { "test_id": "ordering", "status": "pass", "detail": null } ] }
```

Adapters report raw `pass` / `fail` / `skip`; the runner applies manifest policy
and reclassifies.
