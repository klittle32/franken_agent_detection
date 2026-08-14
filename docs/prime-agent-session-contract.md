# Prime Agent session contract

Private fork note: **do not open an upstream PR** against
`Dicklesworthstone/franken_agent_detection`. This branch lives on
`klittle32/franken_agent_detection`.

This document pins the behavioral contract for the native Rust `prime_agent`
connector. Prime Agent is a Pi-derived harness and a read-only format oracle.
It is **not** a runtime dependency, and Prime sessions must never be emitted
as `pi_agent`.

## Source revisions

| Repository | Revision | Role |
|---|---|---|
| `klittle32/franken_agent_detection` | `394ba2a22773c1f63f701145383d28867797974e` | Writable FAD baseline when this work started (`0.1.11-letta.1`) |
| `PrimeIntellect-ai/prime-agent` | `9bf49d897c22563f3e4483d28149c1aac452a6f9` | Producer and behavior specification (`0.7.2`) |
| `Dicklesworthstone/franken_agent_detection` | `57d2789e8d03f6e4b75b0a1a9a5709fc8b290d19` | Upstream HEAD at drift review; **not merged** |
| `Dicklesworthstone/coding_agent_session_search` | out of scope | Downstream consumer; do not edit during this work |

Primary Prime references (not copied into this tree):

- `packages/coding-agent/docs/session-format.md`
- `packages/coding-agent/docs/usage.md`
- `packages/coding-agent/src/config.ts`
- `packages/coding-agent/src/core/session-manager.ts`
- `packages/coding-agent/src/core/messages.ts`
- `packages/coding-agent/package.json` (`piConfig.name = "prime-agent"`)

Fixture files under `tests/fixtures/prime_agent/` are independently authored
synthetic sessions. They are not copies of Prime tests or private sessions.

## Identity

| Surface | Value |
|---|---|
| Canonical slug | `prime_agent` |
| Factory slug | `prime_agent` |
| Accepted aliases | `prime_agent`, `prime-agent`, `primeagent`, `prime` |
| Human label | Prime Agent |
| Rust type | `PrimeAgentConnector` |
| Metadata source marker | `prime-agent` |
| Feature gate | existing `connectors` feature |

Do not canonicalize `prime_agent` to `pi_agent`. Do not add `~/.prime` as a
`pi_agent` root.

## Material drift reviewed before coding

Verified 2026-08-13 against the pins above. None of these change the
implementation contract in the attached plan:

1. FAD fork HEAD still matches `394ba2a` / `0.1.11-letta.1`. Letta support is
   complete and must stay. Next version is the cumulative prerelease
   `0.1.12-letta-prime.1`.
2. Upstream FAD has moved to `57d2789`. This work stays on the private fork
   and does not merge upstream or open an upstream PR.
3. Prime `getSessionDirEnvOverride()` does not trim. FAD trims surrounding
   whitespace and treats blank values as unset, then expands `~` / `~/`
   exactly as Prime (`expandTildePath`).
4. Prime currently lists only direct `*.jsonl` children of the sessions
   directory. FAD also walks recognized legacy nested directories under an
   admitted Prime sessions root and never enters `session-artifacts`, logs,
   or sibling agent homes.
5. Prime `buildSessionContext()` walks only the current leaf-to-root path.
   FAD indexes the complete append-only history, including abandoned
   branches, and sets `projection=append_log` / `tree_aware=true`.
6. Prime `package.json` at the pinned SHA is `0.7.2` with
   `piConfig.name = "prime-agent"` and `configDir = ".prime/agent"`, which
   yields `PRIME_AGENT_*` environment names. No format-field drift versus
   the plan.

## Input layout

Default session directory:

```text
~/.prime/agent/sessions/<session-id>.jsonl
```

Resolved default-mode session root, in this order:

1. nonempty `PRIME_AGENT_SESSION_DIR`
2. nonempty `PRIME_AGENT_CODING_AGENT_SESSION_DIR`
3. nonempty `PRIME_AGENT_CODING_AGENT_DIR` plus `/sessions`
4. `~/.prime/agent/sessions`

`--session-dir` is a per-invocation Prime CLI option and cannot be
rediscovered later. Durable custom stores should set
`PRIME_AGENT_SESSION_DIR` or an explicit CASS `sources.toml` root.

## Admission

A file is admitted when it is a nonempty regular `.jsonl` whose first valid
parsed record is a `type=session` header with a nonempty string `id`, and at
least one of these is true:

1. it lives under a recognized `~/.prime/agent/sessions`-shaped root;
2. it lives under the resolved documented Prime environment override root;
3. it is a direct file under an explicitly supplied sessions root and its
   file stem equals the header session ID;
4. it is a legacy nested file under a recognized Prime sessions root.

Arbitrary Pi-shaped JSONL outside a Prime-shaped root is not Prime.
`~/.pi` and `~/.omp` files, including OMP `wire.jsonl`, are never admitted.

## Projection

Index every message-bearing record once, in file order, including alternate
branches. Preserve `entry_id` and `parent_id` in compact message provenance.
Do not reduce the file to Prime's current leaf-to-root model context.

Message-bearing records: `message`, `custom_message`, `compaction`,
`branch_summary`, plus nested `custom` / legacy `hookMessage` /
`compactionSummary` / `branchSummary` roles.

## Tokens and RLM

Direct assistant usage maps as:

```text
usage.input       -> input_tokens
usage.output      -> output_tokens
usage.cacheRead   -> cache_read_tokens
usage.cacheWrite  -> cache_creation_tokens
```

`child_usage_attributed` is bookkeeping. Preserve attribution metadata.
Never substitute `aggregateUsage` into the parent assistant's direct usage.
Child sessions are indexed separately; applying the aggregate would
double-count global CASS analytics.

## Images and extra

Never emit base64 image `data` into content or compact `extra`. Use a MIME
placeholder such as `[image: image/png]`. Compact `extra` contains selected
provenance, usage, and safe scalars only — not raw rows or arbitrary
extension `details` / `custom.data`.

## Versions

- absent or `1`: legacy linear sequence
- `2`: tree IDs/parents; legacy `hookMessage` normalizes as `custom`
- `3`: current format
- `>3`: parse known shapes best-effort and record a bounded future-version
  diagnostic

Never rewrite or migrate a source file.

## Maintenance

When syncing FAD upstream changes or a new Prime session format:

1. record the old and new FAD and Prime SHAs;
2. review Prime `session-format.md`, `session-manager.ts`, `messages.ts`,
   config path/environment derivation, and session migrations;
3. review FAD normalized types, `Connector`, `ScanContext`, token
   extraction, and registries;
4. update this document before parser behavior;
5. rerun Prime/Pi isolation, Letta regression, focused connector tests, and
   full all-feature gates;
6. bump the cumulative private prerelease;
7. freeze and report a new immutable FAD SHA.
