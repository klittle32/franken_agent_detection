# Letta Code trajectory contract

Private fork note: **do not open an upstream PR** against
`Dicklesworthstone/franken_agent_detection`. This branch lives on
`klittle32/franken_agent_detection`.

This document pins the behavioral contract for the native Rust `letta_code`
connector. `@letta-ai/trajectory` is a format oracle, not a runtime dependency.

## Source revisions

| Repository | Revision | Role |
|---|---|---|
| `Dicklesworthstone/franken_agent_detection` | `7857b2d740dcdcfb4f834f9e394a873fe1796a4d` | Base library this fork started from |
| `letta-ai/trajectory` | `59c0db52cc1521efc7fb5d8c7cccf48ee4afcf32` | Behavioral specification for listing and row normalization |
| `letta-ai/letta-code` | `64e177d88a2e4dae4e29154c96e9fc172824ff52` | Producer of `~/.letta/transcripts` and `LETTA_TRANSCRIPT_ROOT` |
| `Dicklesworthstone/coding_agent_session_search` | `ecd59e2336c24f169a5c8078c95a1aec2cfcc69d` | Downstream consumer (out of scope for this FAD change) |

Primary trajectory references (not copied into this tree):

- `src/adapters/letta-code/index.ts`
- `src/adapters/letta-code/list.ts`
- `src/adapters/letta-code/README.md`

Fixture files under `tests/fixtures/letta_code/` are independently authored
synthetic transcripts. They exercise the public field contract. They are not
copies of trajectory fixtures or private Letta sessions.

## Identity

| Surface | Value |
|---|---|
| Canonical slug | `letta_code` |
| Accepted aliases | `letta_code`, `letta-code` |
| Display / metadata source marker | `letta-code` |
| Rust type | `LettaCodeConnector` |
| Feature gate | existing `connectors` feature |

Bare `letta` is not an alias. This connector does not ingest Letta backend/API
histories, `lc-local-backend` stores, or reflection payload manifests.

## Input layout

Default root: `~/.letta/transcripts`

Environment override: `LETTA_TRANSCRIPT_ROOT` (empty/whitespace ignored)

Canonical file: `<root>/<agentId>/<conversationId>/transcript.jsonl`

External id: `<agentId>/<conversationId>`

Listing is shape-aware and bounded: exact file name `transcript.jsonl`, regular
nonempty files only, no unrestricted recursive scan.

## Row mapping

Recognized `kind` values: `user`, `assistant`, `reasoning`, `tool_call`, `error`.

- Source identity: `source_message_id` else `source_line_id` else zero-based row ordinal.
- Tool call id: `source_line_id` else `source_message_id` else `letta-code-tool-line-<one-based-line>`.
- Shared reasoning/assistant source id uses component indices `0` / `1`.
- Completed tool rows emit a call plus a result when `resultText` is a string or `resultOk` is a boolean.
- Pending tool rows emit the call only.
- Failed results (`resultOk = false`) gain an `Error: ` prefix unless content already starts with `error` (case-insensitive).
- `error` rows, empty text, unsupported kinds, and invalid JSON are recoverable diagnostics.
- Zero recognized kinds: invalid transcript, no conversation.
- Recognized kinds but zero emitted messages: no empty conversation.
