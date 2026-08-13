//! Letta Code (`letta_code`) connector.
//!
//! Reads the client-side append-only transcript Letta Code writes at:
//!
//! ```text
//! $LETTA_TRANSCRIPT_ROOT/<agentId>/<conversationId>/transcript.jsonl
//! ```
//!
//! Default root: `~/.letta/transcripts`. Empty or whitespace-only
//! `LETTA_TRANSCRIPT_ROOT` values are ignored. This connector does not ingest
//! Letta backend/API histories, `lc-local-backend` stores, or reflection
//! payload manifests.
//!
//! Row interpretation follows `letta-ai/trajectory` commit
//! `59c0db52cc1521efc7fb5d8c7cccf48ee4afcf32` (`src/adapters/letta-code/`),
//! reimplemented in Rust. The trajectory package is not a runtime dependency.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Map, Value};

use super::scan::{DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot};
use super::utils::{dedupe_path_key, env_path_nonempty};
use super::{Connector, file_modified_since, franken_detection_for_connector, parse_timestamp};
use crate::types::{
    DetectionResult, NormalizedConversation, NormalizedInvocation, NormalizedMessage,
    reindex_messages,
};

const AGENT_SLUG: &str = "letta_code";
const SOURCE_MARKER: &str = "letta-code";
const TRANSCRIPT_FILE_NAME: &str = "transcript.jsonl";
const ENV_TRANSCRIPT_ROOT: &str = "LETTA_TRANSCRIPT_ROOT";
const MAX_STORED_DIAGNOSTICS: usize = 100;
const TITLE_MAX_CHARS: usize = 100;
const PROGRESS_TICK_INTERVAL: usize = 32;

struct LettaCodeDiagnostic {
    code: &'static str,
    message: String,
    input_line: usize,
}

struct SourceIdentity {
    source_message_id: Option<String>,
    source_line_id: Option<String>,
    source_record_id: Option<String>,
    source_offset: Option<usize>,
    source_anchor_kind: &'static str,
    component_index: u8,
}

/// Connector for Letta Code client transcripts.
pub struct LettaCodeConnector;

impl Default for LettaCodeConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl LettaCodeConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Transcript root used when no explicit scan roots are supplied.
    fn transcript_root() -> PathBuf {
        Self::transcript_root_from(env_path_nonempty(ENV_TRANSCRIPT_ROOT), dirs::home_dir())
    }

    /// Pure root derivation so env fallback can be tested without `set_var`.
    fn transcript_root_from(override_root: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
        if let Some(explicit) = override_root {
            return explicit;
        }
        home.unwrap_or_default().join(".letta").join("transcripts")
    }

    fn source_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
        let mut roots = if ctx.use_default_detection() {
            vec![ScanRoot::local(Self::transcript_root())]
        } else {
            ctx.scan_roots.clone()
        };
        roots.sort_by(|a, b| a.path.cmp(&b.path));
        roots.dedup_by(|a, b| a.path == b.path);
        roots
    }
}

fn is_supported_kind(kind: &str) -> bool {
    matches!(
        kind,
        "user" | "assistant" | "reasoning" | "tool_call" | "error"
    )
}

fn nonempty_str(value: Option<&str>) -> Option<&str> {
    value.filter(|text| !text.is_empty())
}

fn nonempty_json_string<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    nonempty_str(row.get(key).and_then(Value::as_str))
}

fn file_name_eq(path: &Path, expected: &str) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some(expected)
}

fn is_nonempty_transcript_file(path: &Path) -> bool {
    if !file_name_eq(path, TRANSCRIPT_FILE_NAME) {
        return false;
    }
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() > 0)
}

fn push_transcript(path: PathBuf, out: &mut Vec<PathBuf>) {
    if is_nonempty_transcript_file(&path) {
        out.push(path);
    }
}

fn collect_from_agent_dir(agent_dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(agent_dir) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::debug!(
                path = %agent_dir.display(),
                error = %err,
                "letta_code: cannot read agent directory"
            );
            return;
        }
    };
    for entry in entries.flatten() {
        let conversation_dir = entry.path();
        if conversation_dir.is_dir() {
            push_transcript(conversation_dir.join(TRANSCRIPT_FILE_NAME), out);
        }
    }
}

fn collect_from_transcript_root(root: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::debug!(
                path = %root.display(),
                error = %err,
                "letta_code: cannot read transcript root"
            );
            return;
        }
    };
    for entry in entries.flatten() {
        let agent_dir = entry.path();
        if agent_dir.is_dir() {
            collect_from_agent_dir(&agent_dir, out);
        }
    }
}

/// Bounded shape-aware discovery. Never walks arbitrary depth.
fn transcripts_under(target: &Path) -> Vec<PathBuf> {
    if is_nonempty_transcript_file(target) {
        return vec![target.to_path_buf()];
    }
    if !target.is_dir() {
        return Vec::new();
    }

    let direct = target.join(TRANSCRIPT_FILE_NAME);
    if is_nonempty_transcript_file(&direct) {
        return vec![direct];
    }

    let mut out = Vec::new();
    if file_name_eq(target, ".letta") {
        collect_from_transcript_root(&target.join("transcripts"), &mut out);
    } else if target.join(".letta").join("transcripts").is_dir() {
        collect_from_transcript_root(&target.join(".letta").join("transcripts"), &mut out);
    } else if file_name_eq(target, "transcripts") {
        collect_from_transcript_root(target, &mut out);
    } else {
        collect_from_agent_dir(target, &mut out);
        collect_from_transcript_root(target, &mut out);
    }

    out.sort();
    out.dedup();
    out
}

fn locate_transcripts(ctx: &ScanContext) -> Vec<(ScanRoot, PathBuf)> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for root in LettaCodeConnector::source_roots(ctx) {
        if !root.path.exists() {
            continue;
        }
        for path in transcripts_under(&root.path) {
            if !file_modified_since(&path, ctx.since_ts) {
                continue;
            }
            if !seen.insert(dedupe_path_key(&path)) {
                continue;
            }
            out.push((root.clone(), path));
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

fn tick(ctx: &ScanContext, index: usize) {
    if index % PROGRESS_TICK_INTERVAL == 0
        && let Some(progress_tick) = &ctx.progress_tick
    {
        progress_tick();
    }
}

fn source_identity(
    row: &Value,
    line: usize,
    reasoning_ids: &HashSet<String>,
    assistant_kind: bool,
) -> SourceIdentity {
    let source_message_id = nonempty_json_string(row, "source_message_id").map(str::to_string);
    let source_line_id = nonempty_json_string(row, "source_line_id").map(str::to_string);
    let source_record_id = source_message_id.clone().or_else(|| source_line_id.clone());
    let component_index = u8::from(
        assistant_kind
            && source_record_id
                .as_ref()
                .is_some_and(|id| reasoning_ids.contains(id)),
    );
    if let Some(source_record_id) = source_record_id {
        SourceIdentity {
            source_message_id,
            source_line_id,
            source_record_id: Some(source_record_id),
            source_offset: None,
            source_anchor_kind: "record_id",
            component_index,
        }
    } else {
        SourceIdentity {
            source_message_id,
            source_line_id,
            source_record_id: None,
            source_offset: Some(line.saturating_sub(1)),
            source_anchor_kind: "ordinal",
            component_index,
        }
    }
}

fn identity_extra(kind: &str, identity: &SourceIdentity, extras: &[(&str, Value)]) -> Value {
    let mut body = Map::new();
    body.insert("kind".to_string(), Value::String(kind.to_string()));
    body.insert(
        "source_message_id".to_string(),
        identity
            .source_message_id
            .as_deref()
            .map_or(Value::Null, |id| Value::String(id.to_string())),
    );
    body.insert(
        "source_line_id".to_string(),
        identity
            .source_line_id
            .as_deref()
            .map_or(Value::Null, |id| Value::String(id.to_string())),
    );
    body.insert(
        "source_record_id".to_string(),
        identity
            .source_record_id
            .as_deref()
            .map_or(Value::Null, |id| Value::String(id.to_string())),
    );
    body.insert(
        "source_offset".to_string(),
        identity.source_offset.map_or(Value::Null, Value::from),
    );
    body.insert(
        "source_anchor_kind".to_string(),
        Value::String(identity.source_anchor_kind.to_string()),
    );
    body.insert(
        "component_index".to_string(),
        Value::from(identity.component_index),
    );
    for (key, value) in extras {
        body.insert((*key).to_string(), value.clone());
    }
    Value::Object(Map::from_iter([(
        SOURCE_MARKER.replace('-', "_"),
        Value::Object(body),
    )]))
}

fn starts_with_error(content: &str) -> bool {
    content
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("error"))
}

fn parse_args(args_text: &str) -> Value {
    serde_json::from_str(args_text).unwrap_or_else(|_| Value::String(args_text.to_string()))
}

fn tool_call_id(identity: &SourceIdentity, line: usize) -> String {
    identity
        .source_line_id
        .clone()
        .or_else(|| identity.source_message_id.clone())
        .unwrap_or_else(|| format!("letta-code-tool-line-{line}"))
}

fn text_message(
    role: &str,
    author: Option<&str>,
    text: &str,
    created_at: Option<i64>,
    extra: Value,
) -> NormalizedMessage {
    NormalizedMessage {
        idx: 0,
        role: role.to_string(),
        author: author.map(str::to_string),
        created_at,
        content: text.to_string(),
        extra,
        snippets: Vec::new(),
        invocations: Vec::new(),
    }
}

fn agent_conversation_ids(path: &Path) -> Option<(String, String)> {
    let conversation_id = path.parent()?.file_name()?.to_str()?.to_string();
    let agent_id = path.parent()?.parent()?.file_name()?.to_str()?.to_string();
    if agent_id.is_empty() || conversation_id.is_empty() {
        return None;
    }
    Some((agent_id, conversation_id))
}

fn diagnostic_value(diag: &LettaCodeDiagnostic) -> Value {
    Value::Object(Map::from_iter([
        ("code".to_string(), Value::String(diag.code.to_string())),
        ("message".to_string(), Value::String(diag.message.clone())),
        ("input_line".to_string(), Value::from(diag.input_line)),
    ]))
}

fn read_raw_lines(path: &Path, ctx: &ScanContext) -> Result<Vec<(usize, String)>> {
    let reader = BufReader::new(File::open(path)?);
    let mut raw_lines = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        tick(ctx, index);
        match line {
            Ok(text) => raw_lines.push((index + 1, text)),
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "letta_code: stopped reading transcript after an I/O error"
                );
                break;
            }
        }
    }
    Ok(raw_lines)
}

fn parse_json_rows(
    raw_lines: &[(usize, String)],
) -> (Vec<(usize, Value)>, Vec<LettaCodeDiagnostic>) {
    let mut diagnostics = Vec::new();
    let mut rows = Vec::new();
    for (line, raw) in raw_lines {
        if raw.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(raw) {
            Ok(value) if value.is_object() => rows.push((*line, value)),
            Ok(_) => diagnostics.push(LettaCodeDiagnostic {
                code: "non_object_json_line",
                message: format!("Skipped non-object JSON on line {line}."),
                input_line: *line,
            }),
            Err(_) => diagnostics.push(LettaCodeDiagnostic {
                code: "invalid_json_line",
                message: format!("Skipped invalid JSON on line {line}."),
                input_line: *line,
            }),
        }
    }
    (rows, diagnostics)
}

fn reasoning_record_ids(rows: &[(usize, Value)]) -> HashSet<String> {
    let mut reasoning_ids = HashSet::new();
    for (_, row) in rows {
        if row.get("kind").and_then(Value::as_str) != Some("reasoning") {
            continue;
        }
        if let Some(id) = nonempty_json_string(row, "source_message_id")
            .or_else(|| nonempty_json_string(row, "source_line_id"))
        {
            reasoning_ids.insert(id.to_string());
        }
    }
    reasoning_ids
}

fn push_noise(diagnostics: &mut Vec<LettaCodeDiagnostic>, line: usize, message: String) {
    diagnostics.push(LettaCodeDiagnostic {
        code: "noise_record_dropped",
        message,
        input_line: line,
    });
}

fn append_tool_messages(
    row: &Value,
    line: usize,
    created_at: Option<i64>,
    reasoning_ids: &HashSet<String>,
    messages: &mut Vec<NormalizedMessage>,
) {
    let identity = source_identity(row, line, reasoning_ids, false);
    let call_id = tool_call_id(&identity, line);
    let name = nonempty_json_string(row, "name").unwrap_or("unknown");
    let args_text = nonempty_json_string(row, "argsText").unwrap_or("{}");
    let arguments = parse_args(args_text);
    messages.push(NormalizedMessage {
        idx: 0,
        role: "assistant".to_string(),
        author: None,
        created_at,
        content: format!("[Tool: {name}]\n{args_text}"),
        extra: identity_extra(
            "tool_call",
            &identity,
            &[("args_text", Value::String(args_text.to_string()))],
        ),
        snippets: Vec::new(),
        invocations: vec![NormalizedInvocation {
            kind: "tool".to_string(),
            name: name.to_string(),
            raw_name: None,
            call_id: Some(call_id.clone()),
            arguments: Some(arguments),
        }],
    });

    let result_text = row.get("resultText").and_then(Value::as_str);
    let result_ok = row.get("resultOk").and_then(Value::as_bool);
    if result_text.is_none() && result_ok.is_none() {
        return;
    }
    let mut content = result_text.unwrap_or("").to_string();
    if result_ok == Some(false) && !starts_with_error(&content) {
        content = format!("Error: {content}");
    }
    let mut extras = vec![("tool_call_id", Value::String(call_id))];
    if let Some(ok) = result_ok {
        extras.push(("ok", Value::from(ok)));
    }
    let mut result_identity = identity;
    result_identity.component_index = 1;
    messages.push(NormalizedMessage {
        idx: 0,
        role: "tool".to_string(),
        author: nonempty_json_string(row, "name").map(str::to_string),
        created_at,
        content,
        extra: identity_extra("tool_result", &result_identity, &extras),
        snippets: Vec::new(),
        invocations: Vec::new(),
    });
}

fn emit_messages(
    rows: &[(usize, Value)],
    reasoning_ids: &HashSet<String>,
    diagnostics: &mut Vec<LettaCodeDiagnostic>,
) -> (Vec<NormalizedMessage>, usize) {
    let mut messages = Vec::new();
    let mut recognized_rows = 0usize;
    for (line, row) in rows {
        let Some(kind) = row.get("kind").and_then(Value::as_str) else {
            push_noise(
                diagnostics,
                *line,
                format!("Skipped unsupported Letta Code transcript row on line {line}."),
            );
            continue;
        };
        if !is_supported_kind(kind) {
            push_noise(
                diagnostics,
                *line,
                format!("Skipped unsupported Letta Code transcript row on line {line}."),
            );
            continue;
        }
        recognized_rows += 1;

        if kind == "error" {
            push_noise(
                diagnostics,
                *line,
                format!("Skipped Letta Code runtime error row on line {line}."),
            );
            continue;
        }

        let created_at = row.get("captured_at").and_then(parse_timestamp);
        if matches!(kind, "user" | "assistant" | "reasoning") {
            let Some(text) = row
                .get("text")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            else {
                push_noise(
                    diagnostics,
                    *line,
                    format!("Skipped empty Letta Code {kind} row on line {line}."),
                );
                continue;
            };
            let identity = source_identity(row, *line, reasoning_ids, kind == "assistant");
            let author = (kind == "reasoning").then_some("reasoning");
            let role = if kind == "user" { "user" } else { "assistant" };
            messages.push(text_message(
                role,
                author,
                text,
                created_at,
                identity_extra(kind, &identity, &[]),
            ));
            continue;
        }

        append_tool_messages(row, *line, created_at, reasoning_ids, &mut messages);
    }
    (messages, recognized_rows)
}

fn conversation_from_messages(
    path: &Path,
    mut messages: Vec<NormalizedMessage>,
    diagnostics: &[LettaCodeDiagnostic],
) -> Option<NormalizedConversation> {
    let (agent_id, conversation_id) = agent_conversation_ids(path)?;
    reindex_messages(&mut messages);
    let title = messages
        .iter()
        .find(|message| message.role == "user")
        .and_then(|message| message.content.lines().find(|line| !line.trim().is_empty()))
        .map(|line| line.chars().take(TITLE_MAX_CHARS).collect::<String>());
    let timestamps: Vec<i64> = messages
        .iter()
        .filter_map(|message| message.created_at)
        .collect();
    let diagnostic_count = diagnostics.len();
    let stored: Vec<Value> = diagnostics
        .iter()
        .take(MAX_STORED_DIAGNOSTICS)
        .map(diagnostic_value)
        .collect();

    let mut letta_meta = Map::new();
    letta_meta.insert("agent_id".to_string(), Value::String(agent_id.clone()));
    letta_meta.insert(
        "conversation_id".to_string(),
        Value::String(conversation_id.clone()),
    );
    letta_meta.insert(
        "diagnostic_count".to_string(),
        Value::from(diagnostic_count),
    );
    letta_meta.insert(
        "diagnostics_truncated".to_string(),
        Value::from(diagnostic_count > MAX_STORED_DIAGNOSTICS),
    );
    letta_meta.insert("diagnostics".to_string(), Value::Array(stored));

    let mut metadata = Map::new();
    metadata.insert(
        "source".to_string(),
        Value::String(SOURCE_MARKER.to_string()),
    );
    metadata.insert(SOURCE_MARKER.replace('-', "_"), Value::Object(letta_meta));

    Some(NormalizedConversation {
        agent_slug: AGENT_SLUG.to_string(),
        external_id: Some(format!("{agent_id}/{conversation_id}")),
        title,
        workspace: None,
        source_path: path.to_path_buf(),
        started_at: timestamps.iter().copied().min(),
        ended_at: timestamps.iter().copied().max(),
        metadata: Value::Object(metadata),
        messages,
    })
}

fn parse_transcript_file(path: &Path, ctx: &ScanContext) -> Result<Option<NormalizedConversation>> {
    let raw_lines = read_raw_lines(path, ctx)?;
    let (rows, mut diagnostics) = parse_json_rows(&raw_lines);
    let reasoning_ids = reasoning_record_ids(&rows);
    let (messages, recognized_rows) = emit_messages(&rows, &reasoning_ids, &mut diagnostics);

    if recognized_rows == 0 {
        tracing::debug!(
            path = %path.display(),
            "letta_code: skipped file with no recognized transcript rows"
        );
        return Ok(None);
    }
    if messages.is_empty() {
        tracing::debug!(
            path = %path.display(),
            "letta_code: skipped recognized transcript that emitted no messages"
        );
        return Ok(None);
    }

    let Some(conversation) = conversation_from_messages(path, messages, &diagnostics) else {
        tracing::warn!(
            path = %path.display(),
            "letta_code: skipped transcript without agent/conversation parents"
        );
        return Ok(None);
    };
    Ok(Some(conversation))
}

fn scan_letta_with_callback(
    ctx: &ScanContext,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
) -> Result<()> {
    for (_, path) in locate_transcripts(ctx) {
        match parse_transcript_file(&path, ctx) {
            Ok(Some(conversation)) => on_conversation(conversation)?,
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "letta_code: skipped unreadable transcript"
                );
            }
        }
    }
    Ok(())
}

impl Connector for LettaCodeConnector {
    fn detect(&self) -> DetectionResult {
        franken_detection_for_connector(AGENT_SLUG).unwrap_or_else(DetectionResult::not_found)
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut convs = Vec::new();
        scan_letta_with_callback(ctx, &mut |conv| {
            convs.push(conv);
            Ok(())
        })?;
        Ok(convs)
    }

    fn supports_streaming_scan(&self) -> bool {
        true
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        Ok(locate_transcripts(ctx)
            .into_iter()
            .map(|(root, path)| {
                DiscoveredSourceFile::new(
                    AGENT_SLUG,
                    &root,
                    path,
                    DiscoveredSourceRole::PrimarySessionLog,
                    true,
                )
                .with_fs_metadata()
            })
            .collect())
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        scan_letta_with_callback(ctx, on_conversation)
    }
}

#[cfg(test)]
#[allow(clippy::too_many_lines)]
mod tests {
    use super::*;
    use crate::connectors::assert_discovery_covers_scan_sources;
    use serde_json::json;
    use std::fmt::Write as _;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;

    fn fixture_transcripts(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/letta_code")
            .join(name)
            .join("transcripts")
    }

    fn ctx_for(root: &Path) -> ScanContext {
        ScanContext::with_roots(
            root.join("cass-data"),
            vec![ScanRoot::local(root.to_path_buf())],
            None,
        )
    }

    fn scan_fixture(name: &str) -> Vec<NormalizedConversation> {
        let root = fixture_transcripts(name);
        LettaCodeConnector::new()
            .scan(&ctx_for(&root))
            .expect("scan fixture")
    }

    fn extra(message: &NormalizedMessage) -> &Value {
        &message.extra["letta_code"]
    }

    fn write_transcript(root: &Path, agent: &str, conversation: &str, body: &str) -> PathBuf {
        let dir = root.join(agent).join(conversation);
        fs::create_dir_all(&dir).expect("mkdir conversation");
        let path = dir.join(TRANSCRIPT_FILE_NAME);
        fs::write(&path, body).expect("write transcript");
        path
    }

    #[test]
    fn transcript_root_uses_override_and_ignores_blank() {
        let home = PathBuf::from("/tmp/home");
        assert_eq!(
            LettaCodeConnector::transcript_root_from(
                Some(PathBuf::from("/custom/root")),
                Some(home.clone())
            ),
            PathBuf::from("/custom/root")
        );
        assert_eq!(
            LettaCodeConnector::transcript_root_from(None, Some(home.clone())),
            home.join(".letta").join("transcripts")
        );
    }

    #[test]
    fn parses_shared_reasoning_and_assistant_source_id() {
        let convs = scan_fixture("valid_text");
        assert_eq!(convs.len(), 1);
        let conv = &convs[0];
        assert_eq!(conv.agent_slug, AGENT_SLUG);
        assert_eq!(
            conv.external_id.as_deref(),
            Some("agent-alpha/conversation-text")
        );
        assert_eq!(conv.title.as_deref(), Some("letta-user-cobalt-lantern"));
        assert_eq!(conv.workspace, None);
        assert_eq!(conv.messages.len(), 3);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].content, "letta-user-cobalt-lantern");
        assert_eq!(conv.messages[1].role, "assistant");
        assert_eq!(conv.messages[1].author.as_deref(), Some("reasoning"));
        assert_eq!(conv.messages[1].content, "letta-reasoning-amber-compass");
        assert_eq!(extra(&conv.messages[1])["component_index"], 0);
        assert_eq!(conv.messages[2].role, "assistant");
        assert_eq!(conv.messages[2].author, None);
        assert_eq!(conv.messages[2].content, "letta-assistant-silver-orbit");
        assert_eq!(extra(&conv.messages[2])["component_index"], 1);
        assert_eq!(
            extra(&conv.messages[2])["source_record_id"],
            "msg-shared-orbit"
        );
        assert!(
            conv.messages
                .iter()
                .enumerate()
                .all(|(i, msg)| msg.idx == i64::try_from(i).unwrap_or(i64::MAX))
        );
    }

    #[test]
    fn parses_completed_and_failed_tools() {
        let convs = scan_fixture("tools");
        assert_eq!(convs.len(), 1);
        let messages = &convs[0].messages;
        assert_eq!(messages[1].role, "assistant");
        assert!(messages[1].content.contains("letta-tool-arg-violet-key"));
        assert_eq!(messages[1].invocations[0].name, "Read");
        assert_eq!(
            messages[1].invocations[0].call_id.as_deref(),
            Some("line-read-1")
        );
        assert_eq!(
            messages[1].invocations[0].arguments,
            Some(json!({"file_path":"src/lib.rs","needle":"letta-tool-arg-violet-key"}))
        );
        assert_eq!(messages[2].role, "tool");
        assert_eq!(messages[2].content, "letta-tool-result-green-beacon");
        assert_eq!(extra(&messages[2])["ok"], true);
        assert_eq!(extra(&messages[2])["component_index"], 1);

        assert_eq!(messages[4].content, "Error: letta-tool-failure-red-signal");
        assert_eq!(extra(&messages[4])["ok"], false);
        assert_eq!(messages[6].content, "error: already prefixed");

        let unknown = &messages[7];
        assert_eq!(unknown.invocations[0].name, "unknown");
        assert_eq!(
            unknown.invocations[0].arguments,
            Some(Value::String("not-json-args".to_string()))
        );
        assert_eq!(
            unknown.invocations[0].call_id.as_deref(),
            Some("msg-unknown-tool")
        );
        assert_eq!(extra(unknown)["args_text"], "not-json-args");
    }

    #[test]
    fn pending_tool_emits_call_only() {
        let convs = scan_fixture("pending_tool");
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].messages[1].role, "assistant");
        assert_eq!(
            convs[0].messages[1].invocations[0].call_id.as_deref(),
            Some("line-pending")
        );
        assert!(convs[0].messages.iter().all(|msg| msg.role != "tool"));
    }

    #[test]
    fn cleanup_keeps_valid_rows_and_records_diagnostics() {
        let convs = scan_fixture("cleanup");
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].messages[1].content, "survived cleanup");
        assert_eq!(convs[0].messages[1].created_at, None);
        let meta = &convs[0].metadata["letta_code"];
        assert!(meta["diagnostic_count"].as_u64().unwrap() >= 5);
        let codes: Vec<&str> = meta["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|diag| diag["code"].as_str().unwrap())
            .collect();
        assert!(codes.contains(&"invalid_json_line"));
        assert!(codes.contains(&"noise_record_dropped"));
        assert_eq!(meta["diagnostics"][0]["input_line"], 2);
    }

    #[test]
    fn identity_uses_line_id_then_ordinal_fallback() {
        let convs = scan_fixture("identity");
        assert_eq!(
            convs[0].external_id.as_deref(),
            Some("agent-gamma/conversation-identity")
        );
        assert_eq!(
            extra(&convs[0].messages[0])["source_record_id"],
            "line-only-7"
        );
        assert_eq!(
            extra(&convs[0].messages[0])["source_anchor_kind"],
            "record_id"
        );
        assert_eq!(
            extra(&convs[0].messages[1])["source_anchor_kind"],
            "ordinal"
        );
        assert_eq!(extra(&convs[0].messages[1])["source_offset"], 1);
        assert_eq!(
            extra(&convs[0].messages[1])["source_record_id"],
            Value::Null
        );
    }

    #[test]
    fn empty_and_unrecognized_files_yield_no_conversation() {
        assert!(scan_fixture("empty").is_empty());
        assert!(scan_fixture("invalid").is_empty());
    }

    #[test]
    fn synthetic_tool_call_id_uses_one_based_line_number() {
        let tmp = TempDir::new().expect("tempdir");
        write_transcript(
            tmp.path(),
            "agent-z",
            "conv-z",
            "{\"kind\":\"tool_call\",\"name\":\"Read\"}\n",
        );
        let convs = LettaCodeConnector::new()
            .scan(&ctx_for(tmp.path()))
            .expect("scan");
        assert_eq!(
            convs[0].messages[0].invocations[0].call_id.as_deref(),
            Some("letta-code-tool-line-1")
        );
    }

    #[test]
    fn discovery_shapes_agree_and_skip_noise() {
        let tmp = TempDir::new().expect("tempdir");
        let transcripts = tmp.path().join("transcripts");
        let path = write_transcript(
            &transcripts,
            "agent-alpha",
            "conversation-text",
            "{\"kind\":\"user\",\"text\":\"hi\",\"captured_at\":\"2026-07-01T12:00:00Z\"}\n",
        );
        fs::write(
            transcripts
                .join("agent-alpha")
                .join("conversation-text")
                .join("notes.jsonl"),
            "x",
        )
        .expect("wrong name");
        fs::write(
            transcripts
                .join("agent-alpha")
                .join("conversation-text")
                .join("empty.jsonl"),
            "",
        )
        .expect("empty sibling");
        let empty_conv = transcripts.join("agent-alpha").join("conversation-empty");
        fs::create_dir_all(&empty_conv).expect("empty conv");
        fs::write(empty_conv.join(TRANSCRIPT_FILE_NAME), "").expect("empty transcript");

        let connector = LettaCodeConnector::new();
        let roots = [
            path.clone(),
            path.parent().unwrap().to_path_buf(),
            transcripts.join("agent-alpha"),
            transcripts.clone(),
        ];
        let mut discovered = Vec::new();
        for root in roots {
            let sources = connector
                .discover_source_files(&ctx_for(&root))
                .expect("discover");
            assert_eq!(sources.len(), 1, "root {}", root.display());
            assert_eq!(sources[0].source_path, path);
            assert_eq!(sources[0].provider_slug, AGENT_SLUG);
            assert_eq!(sources[0].role, DiscoveredSourceRole::PrimarySessionLog);
            assert!(sources[0].required_for_reconstruction);
            discovered.push(sources[0].source_path.clone());
        }
        assert!(discovered.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn dot_letta_and_mirrored_roots_preserve_provenance() {
        let tmp = TempDir::new().expect("tempdir");
        let letta = tmp.path().join(".letta");
        write_transcript(
            &letta.join("transcripts"),
            "agent-a",
            "conv-a",
            "{\"kind\":\"user\",\"text\":\"hi\",\"captured_at\":\"2026-07-01T12:00:00Z\"}\n",
        );
        let connector = LettaCodeConnector::new();
        let dot_letta = connector
            .discover_source_files(&ctx_for(&letta))
            .expect("discover .letta");
        assert_eq!(dot_letta.len(), 1);
        assert_eq!(dot_letta[0].scan_root, letta);

        let mirrored = ScanRoot::remote(
            tmp.path().to_path_buf(),
            crate::types::Origin::remote("mirror"),
            None,
        );
        let ctx = ScanContext::with_roots(tmp.path().join("data"), vec![mirrored], None);
        let sources = connector
            .discover_source_files(&ctx)
            .expect("discover mirror");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].origin.source_id, "mirror");
        assert_eq!(sources[0].scan_root, tmp.path());
    }

    #[test]
    fn since_ts_excludes_old_and_includes_new() {
        let tmp = TempDir::new().expect("tempdir");
        let path = write_transcript(
            tmp.path(),
            "agent-a",
            "conv-a",
            "{\"kind\":\"user\",\"text\":\"hi\",\"captured_at\":\"2026-07-01T12:00:00Z\"}\n",
        );
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        fs::File::options()
            .write(true)
            .open(&path)
            .expect("open")
            .set_modified(old)
            .expect("mtime");
        let connector = LettaCodeConnector::new();
        let old_ctx = ScanContext::with_roots(
            tmp.path().join("data"),
            vec![ScanRoot::local(tmp.path().to_path_buf())],
            Some(1_700_000_000_000),
        );
        assert!(
            connector
                .discover_source_files(&old_ctx)
                .expect("old")
                .is_empty()
        );
        assert!(connector.scan(&old_ctx).expect("scan old").is_empty());

        let now = SystemTime::now();
        fs::File::options()
            .write(true)
            .open(&path)
            .expect("open")
            .set_modified(now)
            .expect("mtime now");
        assert_eq!(
            connector
                .discover_source_files(&old_ctx)
                .expect("new")
                .len(),
            1
        );
    }

    #[test]
    fn malformed_sibling_does_not_hide_valid_transcript() {
        let tmp = TempDir::new().expect("tempdir");
        write_transcript(
            tmp.path(),
            "agent-a",
            "good",
            "{\"kind\":\"user\",\"text\":\"ok\",\"captured_at\":\"2026-07-01T12:00:00Z\"}\n",
        );
        write_transcript(tmp.path(), "agent-a", "bad", "{not json\n");
        let convs = LettaCodeConnector::new()
            .scan(&ctx_for(tmp.path()))
            .expect("scan");
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].external_id.as_deref(), Some("agent-a/good"));
    }

    #[test]
    fn scan_and_callback_match_and_discovery_covers_scan() {
        let root = fixture_transcripts("valid_text");
        let ctx = ctx_for(&root);
        let connector = LettaCodeConnector::new();
        let scanned = connector.scan(&ctx).expect("scan");
        let mut streamed = Vec::new();
        connector
            .scan_with_callback(&ctx, &mut |conv| {
                streamed.push(conv);
                Ok(())
            })
            .expect("callback");
        assert_eq!(
            serde_json::to_value(&scanned).unwrap(),
            serde_json::to_value(&streamed).unwrap()
        );
        assert!(connector.supports_streaming_scan());
        assert_discovery_covers_scan_sources(&connector, &ctx);
        let again = connector.scan(&ctx).expect("rescan");
        assert_eq!(
            serde_json::to_value(&scanned).unwrap(),
            serde_json::to_value(&again).unwrap()
        );
    }

    #[test]
    fn two_conversations_have_stable_external_ids() {
        let tmp = TempDir::new().expect("tempdir");
        write_transcript(
            tmp.path(),
            "agent-alpha",
            "conversation-text",
            "{\"kind\":\"user\",\"text\":\"one\",\"captured_at\":\"2026-07-01T12:00:00Z\"}\n",
        );
        write_transcript(
            tmp.path(),
            "agent-gamma",
            "conversation-identity",
            "{\"kind\":\"user\",\"text\":\"two\",\"captured_at\":\"2026-07-01T12:00:00Z\"}\n",
        );
        let mut ids: Vec<_> = LettaCodeConnector::new()
            .scan(&ctx_for(tmp.path()))
            .expect("scan")
            .into_iter()
            .map(|conv| conv.external_id.unwrap())
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "agent-alpha/conversation-text".to_string(),
                "agent-gamma/conversation-identity".to_string()
            ]
        );
    }

    #[test]
    fn progress_tick_fires_on_large_transcript() {
        let tmp = TempDir::new().expect("tempdir");
        let mut body = String::new();
        for i in 0..80 {
            let _ = writeln!(
                body,
                "{{\"kind\":\"user\",\"text\":\"row-{i}\",\"captured_at\":\"2026-07-01T12:00:00Z\"}}"
            );
        }
        write_transcript(tmp.path(), "agent-a", "conv-a", &body);
        let ticks = Arc::new(AtomicUsize::new(0));
        let ticks_clone = ticks.clone();
        let ctx = ctx_for(tmp.path()).with_progress_tick(Arc::new(move || {
            ticks_clone.fetch_add(1, Ordering::SeqCst);
        }));
        let convs = LettaCodeConnector::new().scan(&ctx).expect("scan");
        assert_eq!(convs.len(), 1);
        assert!(ticks.load(Ordering::SeqCst) >= 2);
    }

    #[test]
    fn title_truncates_to_100_chars() {
        let tmp = TempDir::new().expect("tempdir");
        let long = "x".repeat(150);
        write_transcript(
            tmp.path(),
            "agent-a",
            "conv-a",
            &format!(
                "{{\"kind\":\"user\",\"text\":\"{long}\",\"captured_at\":\"2026-07-01T12:00:00Z\"}}\n"
            ),
        );
        let convs = LettaCodeConnector::new()
            .scan(&ctx_for(tmp.path()))
            .expect("scan");
        assert_eq!(convs[0].title.as_ref().unwrap().len(), 100);
    }

    #[test]
    fn diagnostics_cap_preserves_count_and_truncation() {
        let tmp = TempDir::new().expect("tempdir");
        let mut body = String::from(
            "{\"kind\":\"user\",\"text\":\"keep\",\"captured_at\":\"2026-07-01T12:00:00Z\"}\n",
        );
        for _ in 0..120 {
            body.push_str("not-json\n");
        }
        write_transcript(tmp.path(), "agent-a", "conv-a", &body);
        let convs = LettaCodeConnector::new()
            .scan(&ctx_for(tmp.path()))
            .expect("scan");
        let meta = &convs[0].metadata["letta_code"];
        assert_eq!(meta["diagnostic_count"], 120);
        assert_eq!(meta["diagnostics_truncated"], true);
        assert_eq!(meta["diagnostics"].as_array().unwrap().len(), 100);
    }
}
