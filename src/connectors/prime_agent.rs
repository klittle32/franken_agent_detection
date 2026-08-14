//! Prime Agent (`prime_agent`) connector.
//!
//! Reads Prime Agent JSONL sessions from:
//!
//! ```text
//! $PRIME_AGENT_SESSION_DIR/<session-id>.jsonl
//! ```
//!
//! Default root: `~/.prime/agent/sessions`. Prime is Pi-derived but remains a
//! distinct producer: this connector never emits `pi_agent` and never treats
//! `~/.prime` as a Pi root.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Map, Value};

use super::pi_wire::{compact_pi_family_usage, flatten_pi_family_content};
use super::scan::{DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot};
use super::token_extraction::extract_pi_family_tokens;
use super::utils::dedupe_path_key;
use super::{Connector, file_modified_since, franken_detection_for_connector, parse_timestamp};
use crate::types::{
    DetectionResult, NormalizedConversation, NormalizedInvocation, NormalizedMessage,
    reindex_messages,
};
use crate::{nonempty_trimmed, prime_agent_session_dir_from_overrides};

const AGENT_SLUG: &str = "prime_agent";
const SOURCE_MARKER: &str = "prime-agent";
const MAX_STORED_DIAGNOSTICS: usize = 100;
const MAX_DIAGNOSTIC_CHARS: usize = 240;
const PROGRESS_TICK_INTERVAL: usize = 32;
const MAX_WALK_DEPTH: usize = 6;
const EXCLUDED_DIR_NAMES: &[&str] = &[
    "session-artifacts",
    "logs",
    "traces",
    "auth",
    "settings",
    "skills",
    "packages",
    "cron",
    "artifacts",
    "daemon-update-restarts",
    "bin",
    "tools",
    "prompts",
    "themes",
];

/// Connector for Prime Agent JSONL sessions.
pub struct PrimeAgentConnector;

impl Default for PrimeAgentConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl PrimeAgentConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    fn default_sessions_dir() -> PathBuf {
        prime_agent_session_dir_from_overrides(
            std::env::var("PRIME_AGENT_SESSION_DIR").ok().as_deref(),
            std::env::var("PRIME_AGENT_CODING_AGENT_SESSION_DIR")
                .ok()
                .as_deref(),
            std::env::var("PRIME_AGENT_CODING_AGENT_DIR")
                .ok()
                .as_deref(),
            dirs::home_dir().as_deref(),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootKind {
    PrimeShaped,
    ExplicitCustom,
}

#[derive(Debug, Clone)]
struct LocatedSession {
    root: ScanRoot,
    path: PathBuf,
}

struct PrimeDiagnostic {
    code: &'static str,
    message: String,
    input_line: usize,
}

struct ParseState {
    session_id: String,
    session_version: u64,
    cwd: Option<String>,
    parent_session: Option<String>,
    rlm_depth: Option<i64>,
    header_git: Option<Value>,
    latest_name: Option<String>,
    name_was_set: bool,
    session_state: Option<String>,
    latest_model: Option<(String, String)>,
    thinking_level: Option<String>,
    service_tier: Option<String>,
    latest_agent_status: Option<Value>,
    latest_git: Option<Value>,
    latest_labels: BTreeMap<String, String>,
    custom_state_count: u64,
    child_attributions: Vec<Value>,
    future_version: bool,
    diagnostics: Vec<PrimeDiagnostic>,
    diagnostic_counts: BTreeMap<String, u64>,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    first_user_line: Option<String>,
    messages: Vec<NormalizedMessage>,
    source_ordinal: usize,
}

impl ParseState {
    const fn new(session_id: String, version: u64) -> Self {
        Self {
            session_id,
            session_version: version,
            cwd: None,
            parent_session: None,
            rlm_depth: None,
            header_git: None,
            latest_name: None,
            name_was_set: false,
            session_state: None,
            latest_model: None,
            thinking_level: None,
            service_tier: None,
            latest_agent_status: None,
            latest_git: None,
            latest_labels: BTreeMap::new(),
            custom_state_count: 0,
            child_attributions: Vec::new(),
            future_version: version > 3,
            diagnostics: Vec::new(),
            diagnostic_counts: BTreeMap::new(),
            started_at: None,
            ended_at: None,
            first_user_line: None,
            messages: Vec::new(),
            source_ordinal: 0,
        }
    }

    fn note(&mut self, code: &'static str, line: usize, message: impl Into<String>) {
        *self.diagnostic_counts.entry(code.to_string()).or_insert(0) += 1;
        if self.diagnostics.len() < MAX_STORED_DIAGNOSTICS {
            let mut message = message.into();
            if message.len() > MAX_DIAGNOSTIC_CHARS {
                message.truncate(MAX_DIAGNOSTIC_CHARS);
            }
            self.diagnostics.push(PrimeDiagnostic {
                code,
                message,
                input_line: line,
            });
        }
    }

    fn touch_time(&mut self, ts: Option<i64>) {
        if let Some(ts) = ts {
            self.started_at = Some(self.started_at.map_or(ts, |curr| curr.min(ts)));
            self.ended_at = Some(self.ended_at.map_or(ts, |curr| curr.max(ts)));
        }
    }
}

fn file_name_eq(path: &Path, expected: &str) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some(expected)
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn is_prime_shaped_path(path: &Path) -> bool {
    let text = path_text(path);
    text.contains("/.prime/agent")
        || text.ends_with("/.prime")
        || file_name_eq(path, ".prime")
        || (file_name_eq(path, "agent")
            && path
                .parent()
                .is_some_and(|parent| file_name_eq(parent, ".prime")))
        || (file_name_eq(path, "sessions")
            && path.parent().is_some_and(|parent| {
                file_name_eq(parent, "agent")
                    && parent
                        .parent()
                        .is_some_and(|grand| file_name_eq(grand, ".prime"))
            }))
}

fn is_foreign_agent_path(path: &Path) -> bool {
    let text = path_text(path);
    text.contains("/.pi/")
        || text.contains("/.omp/")
        || text.ends_with("/.pi")
        || text.ends_with("/.omp")
        || file_name_eq(path, ".pi")
        || file_name_eq(path, ".omp")
}

fn is_excluded_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| EXCLUDED_DIR_NAMES.contains(&name))
}

fn is_jsonl(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
}

fn is_nonempty_regular_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() > 0)
}

fn classify_target(path: &Path) -> Option<(PathBuf, RootKind)> {
    if is_foreign_agent_path(path) {
        return None;
    }
    if path.is_file() {
        return Some((path.to_path_buf(), RootKind::ExplicitCustom));
    }
    if !path.is_dir() {
        return None;
    }
    if is_prime_shaped_path(path) {
        let sessions = if file_name_eq(path, "sessions") {
            path.to_path_buf()
        } else if file_name_eq(path, "agent") || path_text(path).ends_with("/.prime/agent") {
            path.join("sessions")
        } else if file_name_eq(path, ".prime") {
            path.join("agent").join("sessions")
        } else {
            path.join(".prime").join("agent").join("sessions")
        };
        return Some((sessions, RootKind::PrimeShaped));
    }
    let nested = path.join(".prime").join("agent").join("sessions");
    if nested.is_dir() {
        return Some((nested, RootKind::PrimeShaped));
    }
    Some((path.to_path_buf(), RootKind::ExplicitCustom))
}

fn collect_jsonl(root: &Path, kind: RootKind, out: &mut Vec<PathBuf>) {
    if root.is_file() {
        if is_jsonl(root) && is_nonempty_regular_file(root) {
            out.push(root.to_path_buf());
        }
        return;
    }
    if !root.is_dir() {
        return;
    }
    walk_jsonl(root, kind, 0, out);
}

fn walk_jsonl(dir: &Path, kind: RootKind, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_WALK_DEPTH || is_excluded_dir(dir) {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::debug!(
                path = %dir.display(),
                error = %err,
                "prime_agent: cannot read session directory"
            );
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if kind == RootKind::ExplicitCustom {
                continue;
            }
            if is_excluded_dir(&path) {
                continue;
            }
            walk_jsonl(&path, kind, depth + 1, out);
        } else if is_jsonl(&path) && is_nonempty_regular_file(&path) {
            out.push(path);
        }
    }
}

fn peek_session_header(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line.ok()?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line).ok()?;
        if value.get("type").and_then(Value::as_str) != Some("session") {
            return None;
        }
        return nonempty_trimmed(value.get("id").and_then(Value::as_str)).map(str::to_string);
    }
    None
}

fn admits_file(path: &Path, kind: RootKind) -> bool {
    if is_foreign_agent_path(path) {
        return false;
    }
    let Some(session_id) = peek_session_header(path) else {
        return false;
    };
    match kind {
        RootKind::PrimeShaped => true,
        RootKind::ExplicitCustom => {
            is_prime_shaped_path(path)
                || path.file_stem().and_then(|stem| stem.to_str()) == Some(session_id.as_str())
        }
    }
}

fn source_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
    if ctx.use_default_detection() {
        return vec![ScanRoot::local(PrimeAgentConnector::default_sessions_dir())];
    }
    let mut roots = ctx.scan_roots.clone();
    roots.sort_by(|a, b| a.path.cmp(&b.path));
    roots.dedup_by(|a, b| a.path == b.path);
    roots
}

fn locate_sessions(ctx: &ScanContext) -> Vec<LocatedSession> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for root in source_roots(ctx) {
        let Some((target, kind)) = classify_target(&root.path) else {
            continue;
        };
        let mut files = Vec::new();
        collect_jsonl(&target, kind, &mut files);
        files.sort();
        for path in files {
            if !file_modified_since(&path, ctx.since_ts) {
                continue;
            }
            if !admits_file(&path, kind) {
                continue;
            }
            if !seen.insert(dedupe_path_key(&path)) {
                continue;
            }
            out.push(LocatedSession {
                root: root.with_path(target.clone()),
                path,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn tick(ctx: &ScanContext, index: usize) {
    if index % PROGRESS_TICK_INTERVAL == 0
        && let Some(progress_tick) = &ctx.progress_tick
    {
        progress_tick();
    }
}

fn json_string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    nonempty_trimmed(value.get(key).and_then(Value::as_str))
}

fn compact_git(value: &Value) -> Value {
    let mut out = Map::new();
    for key in ["repoUrl", "commit", "branch"] {
        if let Some(text) = json_string(value, key) {
            out.insert(key.to_string(), Value::String(text.to_string()));
        }
    }
    Value::Object(out)
}

fn normalize_session_state(raw: &str) -> String {
    match raw {
        "sleep" => "archived".to_string(),
        other => other.to_string(),
    }
}

fn first_line(text: &str) -> Option<String> {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .map(str::to_string)
}

fn push_message(
    state: &mut ParseState,
    role: &str,
    author: Option<String>,
    content: String,
    created_at: Option<i64>,
    extra: Value,
    invocations: Vec<NormalizedInvocation>,
) {
    if content.trim().is_empty() && invocations.is_empty() {
        return;
    }
    if role == "user" && state.first_user_line.is_none() {
        state.first_user_line = first_line(&content);
    }
    state.touch_time(created_at);
    state.messages.push(NormalizedMessage {
        idx: 0,
        role: role.to_string(),
        author,
        created_at,
        content,
        extra,
        snippets: Vec::new(),
        invocations,
    });
}

fn provenance(
    state: &ParseState,
    entry: &Value,
    entry_type: &str,
    raw_role: Option<&str>,
    extras: &[(&str, Value)],
) -> Map<String, Value> {
    let mut prime = Map::new();
    prime.insert(
        "entry_type".to_string(),
        Value::String(entry_type.to_string()),
    );
    if let Some(id) = json_string(entry, "id") {
        prime.insert("entry_id".to_string(), Value::String(id.to_string()));
    } else {
        prime.insert(
            "source_offset".to_string(),
            Value::from(state.source_ordinal),
        );
    }
    match entry.get("parentId") {
        Some(Value::String(id)) if !id.is_empty() => {
            prime.insert("parent_id".to_string(), Value::String(id.clone()));
        }
        Some(Value::Null) => {
            prime.insert("parent_id".to_string(), Value::Null);
        }
        _ => {}
    }
    if let Some(ts) = json_string(entry, "timestamp") {
        prime.insert("entry_timestamp".to_string(), Value::String(ts.to_string()));
    }
    if let Some(role) = raw_role {
        prime.insert("raw_role".to_string(), Value::String(role.to_string()));
    }
    prime.insert(
        "session_version".to_string(),
        Value::from(state.session_version),
    );
    for (key, value) in extras {
        prime.insert((*key).to_string(), value.clone());
    }
    prime
}

fn cass_extra(
    model: Option<&str>,
    provider: Option<&str>,
    service_tier: Option<&str>,
    usage: Option<&Value>,
    tool_call_count: u32,
) -> Map<String, Value> {
    let mut cass = Map::new();
    if let Some(model) = model.filter(|text| !text.is_empty()) {
        cass.insert("model".to_string(), Value::String(model.to_string()));
    }
    if let Some(provider) = provider.filter(|text| !text.is_empty()) {
        cass.insert("provider".to_string(), Value::String(provider.to_string()));
    }
    if let Some(tier) = service_tier.filter(|text| !text.is_empty()) {
        cass.insert("service_tier".to_string(), Value::String(tier.to_string()));
    }
    cass.insert("tool_call_count".to_string(), Value::from(tool_call_count));
    if let Some(usage) = usage {
        let extracted = extract_pi_family_tokens(&Value::Object(Map::from_iter([(
            "usage".to_string(),
            usage.clone(),
        )])));
        if extracted.has_token_data() {
            let mut token_usage = Map::new();
            if let Some(v) = extracted.input_tokens {
                token_usage.insert("input_tokens".to_string(), Value::from(v));
            }
            if let Some(v) = extracted.output_tokens {
                token_usage.insert("output_tokens".to_string(), Value::from(v));
            }
            if let Some(v) = extracted.cache_read_tokens {
                token_usage.insert("cache_read_tokens".to_string(), Value::from(v));
            }
            if let Some(v) = extracted.cache_creation_tokens {
                token_usage.insert("cache_creation_tokens".to_string(), Value::from(v));
            }
            token_usage.insert("data_source".to_string(), Value::String("api".to_string()));
            cass.insert("token_usage".to_string(), Value::Object(token_usage));
        }
    }
    cass
}

fn message_extra(prime: Map<String, Value>, cass: Map<String, Value>) -> Value {
    Value::Object(Map::from_iter([
        ("prime".to_string(), Value::Object(prime)),
        ("cass".to_string(), Value::Object(cass)),
    ]))
}

fn created_at(entry: &Value, message: Option<&Value>) -> Option<i64> {
    message
        .and_then(|msg| msg.get("timestamp"))
        .and_then(parse_timestamp)
        .or_else(|| entry.get("timestamp").and_then(parse_timestamp))
}

fn emit_user(state: &mut ParseState, entry: &Value, message: &Value, raw_role: &str) {
    let content = message.get("content").cloned().unwrap_or(Value::Null);
    let flat = flatten_pi_family_content(&content);
    let extra = message_extra(
        provenance(state, entry, "message", Some(raw_role), &[]),
        cass_extra(None, None, None, None, 0),
    );
    push_message(
        state,
        "user",
        Some("user".to_string()),
        flat.searchable_text,
        created_at(entry, Some(message)),
        extra,
        Vec::new(),
    );
}

fn emit_assistant(state: &mut ParseState, entry: &Value, message: &Value) {
    let content = message.get("content").cloned().unwrap_or(Value::Null);
    let flat = flatten_pi_family_content(&content);
    let model = json_string(message, "model")
        .map(str::to_string)
        .or_else(|| state.latest_model.as_ref().map(|(_, model)| model.clone()));
    let provider = json_string(message, "provider")
        .map(str::to_string)
        .or_else(|| {
            state
                .latest_model
                .as_ref()
                .map(|(provider, _)| provider.clone())
        });
    if let (Some(provider), Some(model)) = (provider.as_deref(), model.as_deref()) {
        state.latest_model = Some((provider.to_string(), model.to_string()));
    }
    let mut extras = Vec::new();
    if let Some(api) = json_string(message, "api") {
        extras.push(("api", Value::String(api.to_string())));
    }
    if let Some(stop) = json_string(message, "stopReason") {
        extras.push(("stop_reason", Value::String(stop.to_string())));
    }
    if let Some(error) = json_string(message, "errorMessage") {
        extras.push(("error_message", Value::String(error.to_string())));
    }
    if message.get("usage").is_some() {
        extras.push((
            "usage",
            compact_pi_family_usage(message, state.service_tier.as_deref()),
        ));
    }
    let tool_call_count = u32::try_from(flat.invocations.len()).unwrap_or(u32::MAX);
    extras.push(("tool_call_count", Value::from(tool_call_count)));
    let extra = message_extra(
        provenance(state, entry, "message", Some("assistant"), &extras),
        cass_extra(
            model.as_deref(),
            provider.as_deref(),
            state.service_tier.as_deref(),
            message.get("usage"),
            tool_call_count,
        ),
    );
    push_message(
        state,
        "assistant",
        model,
        flat.searchable_text,
        created_at(entry, Some(message)),
        extra,
        flat.invocations,
    );
}

fn emit_tool_result(state: &mut ParseState, entry: &Value, message: &Value) {
    let tool_name = json_string(message, "toolName").unwrap_or("unknown");
    let is_error = message
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let status = if is_error { "error" } else { "ok" };
    let content = message.get("content").cloned().unwrap_or(Value::Null);
    let flat = flatten_pi_family_content(&content);
    let body = if flat.searchable_text.is_empty() {
        format!("[tool result: {tool_name}; {status}]")
    } else {
        format!(
            "[tool result: {tool_name}; {status}]\n{}",
            flat.searchable_text
        )
    };
    let extras = [
        (
            "tool_call_id",
            json_string(message, "toolCallId")
                .map_or(Value::Null, |id| Value::String(id.to_string())),
        ),
        ("tool_name", Value::String(tool_name.to_string())),
        ("is_error", Value::from(is_error)),
        (
            "details_present",
            Value::from(message.get("details").is_some()),
        ),
    ];
    let extra = message_extra(
        provenance(state, entry, "message", Some("toolResult"), &extras),
        cass_extra(None, None, None, None, 0),
    );
    push_message(
        state,
        "tool",
        Some(tool_name.to_string()),
        body,
        created_at(entry, Some(message)),
        extra,
        Vec::new(),
    );
}

fn emit_shell(state: &mut ParseState, entry: &Value, message: &Value) {
    let command = json_string(message, "command").unwrap_or("");
    let output = message
        .get("output")
        .and_then(Value::as_str)
        .map_or_else(|| "(no output)".to_string(), str::to_string);
    let output = if output.is_empty() {
        "(no output)".to_string()
    } else {
        output
    };
    let exit = message.get("exitCode").and_then(Value::as_i64);
    let cancelled = message
        .get("cancelled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let truncated = message
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let excluded = message
        .get("excludeFromContext")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let exit_text = exit.map_or_else(|| "unknown".to_string(), |code| code.to_string());
    let content = format!(
        "[shell command]\n{command}\n\n[shell output]\n{output}\n\n[status]\nexit={exit_text} cancelled={cancelled} truncated={truncated}"
    );
    let extras = [
        ("exclude_from_context", Value::from(excluded)),
        (
            "full_output_path",
            json_string(message, "fullOutputPath")
                .map_or(Value::Null, |path| Value::String(path.to_string())),
        ),
        ("cancelled", Value::from(cancelled)),
        ("truncated", Value::from(truncated)),
        ("exit_code", exit.map_or(Value::Null, Value::from)),
    ];
    let extra = message_extra(
        provenance(state, entry, "message", Some("bashExecution"), &extras),
        cass_extra(None, None, None, None, 1),
    );
    let invocation = NormalizedInvocation {
        kind: "tool".to_string(),
        name: "bash".to_string(),
        raw_name: Some("bashExecution".to_string()),
        call_id: None,
        arguments: Some(Value::Object(Map::from_iter([(
            "command".to_string(),
            Value::String(command.to_string()),
        )]))),
    };
    push_message(
        state,
        "tool",
        Some("shell".to_string()),
        content,
        created_at(entry, Some(message)),
        extra,
        vec![invocation],
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_custom(
    state: &mut ParseState,
    entry: &Value,
    entry_type: &str,
    raw_role: &str,
    custom_type: Option<&str>,
    display: Option<bool>,
    content: &Value,
    created: Option<i64>,
) {
    let flat = flatten_pi_family_content(content);
    let author = format!("custom:{}", custom_type.unwrap_or("unknown"));
    let extras = [
        (
            "custom_type",
            custom_type.map_or(Value::Null, |text| Value::String(text.to_string())),
        ),
        ("display", display.map_or(Value::Null, Value::from)),
        (
            "details_present",
            Value::from(entry.get("details").is_some()),
        ),
    ];
    let extra = message_extra(
        provenance(state, entry, entry_type, Some(raw_role), &extras),
        cass_extra(None, None, None, None, 0),
    );
    push_message(
        state,
        "system",
        Some(author),
        flat.searchable_text,
        created,
        extra,
        Vec::new(),
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_summary(
    state: &mut ParseState,
    entry: &Value,
    entry_type: &str,
    raw_role: &str,
    author: &str,
    label: &str,
    summary: &str,
    extras: &[(&str, Value)],
    created: Option<i64>,
) {
    let extra = message_extra(
        provenance(state, entry, entry_type, Some(raw_role), extras),
        cass_extra(None, None, None, None, 0),
    );
    push_message(
        state,
        "system",
        Some(author.to_string()),
        format!("[{label}]\n{summary}"),
        created,
        extra,
        Vec::new(),
    );
}

fn handle_message_entry(state: &mut ParseState, entry: &Value, line: usize) {
    let Some(message) = entry.get("message") else {
        state.note(
            "missing_message_object",
            line,
            format!("Skipped message entry without nested message on line {line}."),
        );
        return;
    };
    let raw_role = message.get("role").and_then(Value::as_str).unwrap_or("");
    match raw_role {
        "user" => emit_user(state, entry, message, raw_role),
        "assistant" => emit_assistant(state, entry, message),
        "toolResult" => emit_tool_result(state, entry, message),
        "bashExecution" => emit_shell(state, entry, message),
        "custom" | "hookMessage" => emit_custom(
            state,
            entry,
            "message",
            raw_role,
            json_string(message, "customType"),
            message.get("display").and_then(Value::as_bool),
            message.get("content").unwrap_or(&Value::Null),
            created_at(entry, Some(message)),
        ),
        "compactionSummary" => {
            if let Some(summary) = json_string(message, "summary") {
                emit_summary(
                    state,
                    entry,
                    "message",
                    raw_role,
                    "compaction",
                    "compaction summary",
                    summary,
                    &[],
                    created_at(entry, Some(message)),
                );
            }
        }
        "branchSummary" => {
            if let Some(summary) = json_string(message, "summary") {
                emit_summary(
                    state,
                    entry,
                    "message",
                    raw_role,
                    "branch_summary",
                    "branch summary",
                    summary,
                    &[(
                        "from_id",
                        json_string(message, "fromId")
                            .map_or(Value::Null, |id| Value::String(id.to_string())),
                    )],
                    created_at(entry, Some(message)),
                );
            }
        }
        other => state.note(
            "unknown_role",
            line,
            format!("Skipped unknown Prime message role '{other}' on line {line}."),
        ),
    }
}

fn apply_header(state: &mut ParseState, header: &Value) {
    state.cwd = json_string(header, "cwd").map(str::to_string);
    state.parent_session = json_string(header, "parentSession").map(str::to_string);
    state.rlm_depth = header.get("rlmDepth").and_then(Value::as_i64);
    if let Some(git) = header.get("git") {
        state.header_git = Some(compact_git(git));
        state.latest_git = state.header_git.clone();
    }
    state.touch_time(header.get("timestamp").and_then(parse_timestamp));
    if state.future_version {
        state.note(
            "future_version",
            1,
            format!(
                "Session version {} is newer than 3; known records were parsed best-effort.",
                state.session_version
            ),
        );
    }
}

#[allow(clippy::too_many_lines)]
fn handle_entry(state: &mut ParseState, entry: &Value, line: usize) {
    state.source_ordinal += 1;
    let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("");
    match entry_type {
        "session" => {}
        "message" => handle_message_entry(state, entry, line),
        "custom_message" => emit_custom(
            state,
            entry,
            "custom_message",
            "custom",
            json_string(entry, "customType"),
            entry.get("display").and_then(Value::as_bool),
            entry.get("content").unwrap_or(&Value::Null),
            created_at(entry, None),
        ),
        "compaction" => {
            if let Some(summary) = json_string(entry, "summary") {
                let extras = [
                    (
                        "tokens_before",
                        entry
                            .get("tokensBefore")
                            .and_then(Value::as_i64)
                            .map_or(Value::Null, Value::from),
                    ),
                    (
                        "first_kept_entry_id",
                        json_string(entry, "firstKeptEntryId")
                            .map_or(Value::Null, |id| Value::String(id.to_string())),
                    ),
                    (
                        "custom_instructions",
                        json_string(entry, "customInstructions")
                            .map_or(Value::Null, |text| Value::String(text.to_string())),
                    ),
                    (
                        "from_hook",
                        entry
                            .get("fromHook")
                            .and_then(Value::as_bool)
                            .map_or(Value::Null, Value::from),
                    ),
                ];
                emit_summary(
                    state,
                    entry,
                    "compaction",
                    "compaction",
                    "compaction",
                    "compaction summary",
                    summary,
                    &extras,
                    created_at(entry, None),
                );
            }
        }
        "branch_summary" => {
            if let Some(summary) = json_string(entry, "summary") {
                emit_summary(
                    state,
                    entry,
                    "branch_summary",
                    "branchSummary",
                    "branch_summary",
                    "branch summary",
                    summary,
                    &[(
                        "from_id",
                        json_string(entry, "fromId")
                            .map_or(Value::Null, |id| Value::String(id.to_string())),
                    )],
                    created_at(entry, None),
                );
            }
        }
        "model_change" => {
            if let (Some(provider), Some(model)) = (
                json_string(entry, "provider"),
                json_string(entry, "modelId"),
            ) {
                state.latest_model = Some((provider.to_string(), model.to_string()));
            }
        }
        "thinking_level_change" => {
            state.thinking_level = json_string(entry, "thinkingLevel").map(str::to_string);
        }
        "service_tier_change" => {
            state.service_tier = json_string(entry, "serviceTier").map(str::to_string);
        }
        "custom" => {
            state.custom_state_count += 1;
        }
        "child_usage_attributed" => {
            let mut summary = Map::new();
            if let Some(target) = json_string(entry, "targetId") {
                summary.insert("target_id".to_string(), Value::String(target.to_string()));
            }
            if let Some(origin) = json_string(entry, "origin") {
                summary.insert("origin".to_string(), Value::String(origin.to_string()));
            }
            if let Some(child) = entry.get("childUsage") {
                summary.insert(
                    "child_usage".to_string(),
                    compact_pi_family_usage(
                        &Value::Object(Map::from_iter([("usage".to_string(), child.clone())])),
                        None,
                    ),
                );
            }
            summary.insert(
                "aggregate_present".to_string(),
                Value::from(entry.get("aggregateUsage").is_some()),
            );
            state.child_attributions.push(Value::Object(summary));
        }
        "label" => {
            if let Some(target) = json_string(entry, "targetId") {
                if let Some(label) = json_string(entry, "label") {
                    state
                        .latest_labels
                        .insert(target.to_string(), label.to_string());
                } else {
                    state.latest_labels.remove(target);
                }
            }
        }
        "session_info" => {
            state.name_was_set = true;
            state.latest_name = json_string(entry, "name").map(str::to_string);
        }
        "session_state" => {
            if let Some(status) = entry
                .pointer("/state/status")
                .and_then(Value::as_str)
                .or_else(|| json_string(entry, "status"))
            {
                state.session_state = Some(normalize_session_state(status));
            }
        }
        "agent_status" => {
            if let Some(status) = entry.get("status") {
                let mut compact = Map::new();
                if let Some(summary) = json_string(status, "summary") {
                    compact.insert("summary".to_string(), Value::String(summary.to_string()));
                }
                if let Some(task) = json_string(status, "taskState") {
                    compact.insert("task_state".to_string(), Value::String(task.to_string()));
                }
                if let Some(count) = status.get("basedOnMessageCount").and_then(Value::as_i64) {
                    compact.insert("based_on_message_count".to_string(), Value::from(count));
                }
                state.latest_agent_status = Some(Value::Object(compact));
            }
        }
        "git_state" => {
            if let Some(git) = entry.get("git") {
                state.latest_git = Some(compact_git(git));
            }
        }
        other => state.note(
            "unknown_entry_type",
            line,
            format!("Skipped unknown Prime entry type '{other}' on line {line}."),
        ),
    }
}

#[allow(clippy::too_many_lines)]
fn conversation_from_state(
    path: &Path,
    scan_root: &ScanRoot,
    mut state: ParseState,
) -> Option<NormalizedConversation> {
    if state.messages.is_empty() {
        return None;
    }
    reindex_messages(&mut state.messages);
    let title = state
        .latest_name
        .clone()
        .filter(|name| !name.is_empty())
        .or_else(|| state.first_user_line.clone())
        .or_else(|| Some(state.session_id.clone()));
    let workspace = state
        .cwd
        .as_deref()
        .map(|cwd| PathBuf::from(scan_root.rewrite_workspace(cwd, Some(AGENT_SLUG))));

    let latest_model = if let Some((provider, model)) = &state.latest_model {
        Value::Object(Map::from_iter([
            ("provider".to_string(), Value::String(provider.clone())),
            ("model".to_string(), Value::String(model.clone())),
        ]))
    } else {
        Value::Null
    };

    let stored: Vec<Value> = state
        .diagnostics
        .iter()
        .map(|diag| {
            Value::Object(Map::from_iter([
                ("code".to_string(), Value::String(diag.code.to_string())),
                ("message".to_string(), Value::String(diag.message.clone())),
                ("input_line".to_string(), Value::from(diag.input_line)),
            ]))
        })
        .collect();
    let diagnostic_count: u64 = state.diagnostic_counts.values().sum();

    let metadata = Value::Object(Map::from_iter([
        (
            "source".to_string(),
            Value::String(SOURCE_MARKER.to_string()),
        ),
        (
            "session_id".to_string(),
            Value::String(state.session_id.clone()),
        ),
        (
            "session_version".to_string(),
            Value::from(state.session_version),
        ),
        (
            "projection".to_string(),
            Value::String("append_log".to_string()),
        ),
        ("tree_aware".to_string(), Value::from(true)),
        (
            "parent_session".to_string(),
            state.parent_session.map_or(Value::Null, Value::String),
        ),
        (
            "rlm_depth".to_string(),
            state.rlm_depth.map_or(Value::Null, Value::from),
        ),
        (
            "session_name".to_string(),
            state.latest_name.map_or(Value::Null, Value::String),
        ),
        (
            "session_state".to_string(),
            state.session_state.map_or(Value::Null, Value::String),
        ),
        ("latest_model".to_string(), latest_model),
        (
            "thinking_level".to_string(),
            state.thinking_level.map_or(Value::Null, Value::String),
        ),
        (
            "service_tier".to_string(),
            state.service_tier.map_or(Value::Null, Value::String),
        ),
        (
            "latest_agent_status".to_string(),
            state.latest_agent_status.unwrap_or(Value::Null),
        ),
        ("git".to_string(), state.latest_git.unwrap_or(Value::Null)),
        (
            "child_usage_attribution_count".to_string(),
            Value::from(state.child_attributions.len()),
        ),
        (
            "child_usage_attributions".to_string(),
            Value::Array(state.child_attributions),
        ),
        (
            "custom_state_count".to_string(),
            Value::from(state.custom_state_count),
        ),
        (
            "labels".to_string(),
            Value::Object(
                state
                    .latest_labels
                    .into_iter()
                    .map(|(k, v)| (k, Value::String(v)))
                    .collect(),
            ),
        ),
        ("name_was_set".to_string(), Value::from(state.name_was_set)),
        ("diagnostics".to_string(), Value::Array(stored)),
        (
            "diagnostic_counts".to_string(),
            Value::Object(
                state
                    .diagnostic_counts
                    .into_iter()
                    .map(|(k, v)| (k, Value::from(v)))
                    .collect(),
            ),
        ),
        (
            "diagnostic_count".to_string(),
            Value::from(diagnostic_count),
        ),
        (
            "diagnostics_truncated".to_string(),
            Value::from(
                usize::try_from(diagnostic_count).unwrap_or(usize::MAX) > MAX_STORED_DIAGNOSTICS,
            ),
        ),
    ]));

    Some(NormalizedConversation {
        agent_slug: AGENT_SLUG.to_string(),
        external_id: Some(state.session_id),
        title,
        workspace,
        source_path: path.to_path_buf(),
        started_at: state.started_at,
        ended_at: state.ended_at,
        metadata,
        messages: state.messages,
    })
}

fn parse_session_file(
    path: &Path,
    scan_root: &ScanRoot,
    ctx: &ScanContext,
) -> Option<NormalizedConversation> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) => {
            tracing::debug!(
                path = %path.display(),
                error = %err,
                "prime_agent: skipped unreadable session"
            );
            return None;
        }
    };
    let reader = BufReader::new(file);
    let mut state: Option<ParseState> = None;
    for (index, line) in reader.lines().enumerate() {
        tick(ctx, index);
        let line_no = index + 1;
        let text = match line {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "prime_agent: stopped reading session after an I/O error"
                );
                break;
            }
        };
        if text.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            if let Some(state) = state.as_mut() {
                state.note(
                    "invalid_json_line",
                    line_no,
                    format!("Skipped malformed JSON on line {line_no}."),
                );
            }
            continue;
        };
        if state.is_none() {
            if value.get("type").and_then(Value::as_str) != Some("session") {
                return None;
            }
            let session_id = nonempty_trimmed(value.get("id").and_then(Value::as_str))?;
            let version = value.get("version").and_then(Value::as_u64).unwrap_or(1);
            let mut parsed = ParseState::new(session_id.to_string(), version);
            apply_header(&mut parsed, &value);
            state = Some(parsed);
            continue;
        }
        if let Some(state) = state.as_mut() {
            handle_entry(state, &value, line_no);
        }
    }
    state.and_then(|state| conversation_from_state(path, scan_root, state))
}

fn scan_prime_with_callback(
    ctx: &ScanContext,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
) -> Result<()> {
    for located in locate_sessions(ctx) {
        if let Some(conversation) = parse_session_file(&located.path, &located.root, ctx) {
            on_conversation(conversation)?;
        }
    }
    Ok(())
}

impl Connector for PrimeAgentConnector {
    fn detect(&self) -> DetectionResult {
        franken_detection_for_connector(AGENT_SLUG).unwrap_or_else(DetectionResult::not_found)
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut convs = Vec::new();
        scan_prime_with_callback(ctx, &mut |conv| {
            convs.push(conv);
            Ok(())
        })?;
        Ok(convs)
    }

    fn supports_streaming_scan(&self) -> bool {
        true
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        Ok(locate_sessions(ctx)
            .into_iter()
            .map(|located| {
                DiscoveredSourceFile::new(
                    AGENT_SLUG,
                    &located.root,
                    located.path,
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
        scan_prime_with_callback(ctx, on_conversation)
    }
}

#[cfg(test)]
#[allow(clippy::too_many_lines)]
mod tests {
    use super::*;
    use crate::connectors::assert_discovery_covers_scan_sources;
    use crate::connectors::pi_agent::PiAgentConnector;
    use crate::connectors::token_extraction::extract_tokens_for_agent;
    use crate::types::{Origin, Platform};
    use serde_json::json;
    use std::fmt::Write as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;

    fn fixture_home(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/prime_agent")
            .join(name)
            .join("home")
    }

    fn ctx_for(home: &Path) -> ScanContext {
        ScanContext::with_roots(
            home.join("cass-data"),
            vec![ScanRoot::local(home.to_path_buf())],
            None,
        )
    }

    fn scan_fixture(name: &str) -> Vec<NormalizedConversation> {
        PrimeAgentConnector::new()
            .scan(&ctx_for(&fixture_home(name)))
            .expect("scan fixture")
    }

    fn serialized(conv: &NormalizedConversation) -> String {
        serde_json::to_string(conv).expect("serialize")
    }

    #[test]
    fn new_and_default_exist() {
        let _ = PrimeAgentConnector::new();
        let _ = PrimeAgentConnector;
    }

    #[test]
    fn session_dir_precedence_and_tilde() {
        let home = Path::new("/fabricated/home");
        assert_eq!(
            prime_agent_session_dir_from_overrides(
                Some("  ~/custom-sessions  "),
                Some("/legacy"),
                Some("/agent"),
                Some(home)
            ),
            PathBuf::from("/fabricated/home/custom-sessions")
        );
        assert_eq!(
            prime_agent_session_dir_from_overrides(Some("   "), Some("~/legacy"), None, Some(home)),
            PathBuf::from("/fabricated/home/legacy")
        );
        assert_eq!(
            prime_agent_session_dir_from_overrides(None, None, Some("~/agent-home"), Some(home)),
            PathBuf::from("/fabricated/home/agent-home/sessions")
        );
        assert_eq!(
            prime_agent_session_dir_from_overrides(None, None, None, Some(home)),
            home.join(".prime/agent/sessions")
        );
        assert_eq!(
            crate::expand_tilde_like_prime("~", Some(home)),
            home.to_path_buf()
        );
        assert_eq!(
            crate::expand_tilde_like_prime("~/sessions", Some(home)),
            home.join("sessions")
        );
        assert_eq!(
            crate::expand_tilde_like_prime("/abs/path", Some(home)),
            PathBuf::from("/abs/path")
        );
    }

    #[test]
    fn v3_full_parses_contract() {
        let convs = scan_fixture("v3_full");
        assert_eq!(convs.len(), 1);
        let conv = &convs[0];
        assert_eq!(conv.agent_slug, "prime_agent");
        assert_eq!(
            conv.external_id.as_deref(),
            Some("0198f000-0000-7000-8000-000000000001")
        );
        assert_eq!(conv.title.as_deref(), Some("prime-user-cobalt-orchard"));
        assert_eq!(
            conv.workspace,
            Some(PathBuf::from("/fabricated/prime/project"))
        );
        assert_eq!(conv.metadata["source"], "prime-agent");
        assert_eq!(conv.metadata["projection"], "append_log");
        assert_eq!(conv.metadata["tree_aware"], true);
        assert_eq!(
            conv.metadata["parent_session"],
            "/fabricated/prime/parent.jsonl"
        );
        assert_eq!(conv.metadata["rlm_depth"], 0);
        assert_eq!(conv.metadata["thinking_level"], "high");
        assert_eq!(conv.metadata["service_tier"], "default");
        assert_eq!(conv.metadata["session_state"], "active");
        assert_eq!(conv.metadata["latest_model"]["model"], "claude-opus-4-6");
        assert_eq!(conv.metadata["git"]["branch"], "feature/prime");
        assert_eq!(conv.messages.len(), 6);
        let blob = conv
            .messages
            .iter()
            .map(|msg| msg.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for sentinel in [
            "prime-user-cobalt-orchard",
            "prime-reasoning-amber-lattice",
            "prime-assistant-silver-azimuth",
            "prime-tool-arg-violet-keystone",
            "prime-tool-result-green-circuit",
            "prime-tool-failure-red-marker",
        ] {
            assert!(blob.contains(sentinel), "missing {sentinel}");
        }
        assert!(blob.contains("[image: image/png]"));
        assert!(!serialized(conv).contains("PRIME_BASE64_MUST_NOT_SURVIVE"));
        let assistant = &conv.messages[2];
        assert_eq!(assistant.role, "assistant");
        assert_eq!(assistant.invocations.len(), 2);
        assert_eq!(assistant.invocations[0].name, "read_file");
        assert_eq!(
            assistant.invocations[0].call_id.as_deref(),
            Some("call-read-1")
        );
        assert_eq!(
            assistant.invocations[0].arguments,
            Some(json!({
                "path": "/fabricated/prime/src.rs",
                "needle": "prime-tool-arg-violet-keystone",
                "z": "last",
                "a": "first"
            }))
        );
        assert_eq!(assistant.extra["prime"]["entry_id"], "asst0001");
        assert_eq!(assistant.extra["prime"]["parent_id"], "tier0001");
        assert_eq!(assistant.extra["cass"]["token_usage"]["input_tokens"], 100);
        assert_eq!(assistant.extra["cass"]["token_usage"]["output_tokens"], 250);
        assert_eq!(
            assistant.extra["cass"]["token_usage"]["cache_read_tokens"],
            1000
        );
        assert_eq!(
            assistant.extra["cass"]["token_usage"]["cache_creation_tokens"],
            20
        );
        assert_eq!(assistant.extra["cass"]["token_usage"]["data_source"], "api");
        assert_eq!(conv.messages[3].role, "tool");
        assert!(conv.messages[3].content.contains("ok"));
        assert_eq!(conv.messages[4].extra["prime"]["is_error"], true);
        assert_eq!(conv.messages[5].created_at, None);
        assert!(!serialized(conv).contains("do-not-copy"));
    }

    #[test]
    fn latest_nonempty_session_name_wins_until_cleared() {
        let tmp = TempDir::new().expect("tempdir");
        let sessions = tmp.path().join(".prime/agent/sessions");
        std::fs::create_dir_all(&sessions).expect("mkdir");
        let id = "0198f000-0000-7000-8000-000000000030";
        std::fs::write(
            sessions.join(format!("{id}.jsonl")),
            format!(
                r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-08-13T23:00:00.000Z","cwd":"/x"}}
{{"type":"session_info","id":"n1","parentId":null,"timestamp":"2026-08-13T23:00:01.000Z","name":"First Title"}}
{{"type":"message","id":"u1","parentId":"n1","timestamp":"2026-08-13T23:00:02.000Z","message":{{"role":"user","content":"prime-user-cobalt-orchard"}}}}
{{"type":"session_info","id":"n2","parentId":"u1","timestamp":"2026-08-13T23:00:03.000Z","name":"Latest Title"}}
"#
            ),
        )
        .expect("write named");
        let ctx = ScanContext::with_roots(
            tmp.path().to_path_buf(),
            vec![ScanRoot::local(tmp.path().to_path_buf())],
            None,
        );
        let named = &PrimeAgentConnector::new().scan(&ctx).expect("named")[0];
        assert_eq!(named.title.as_deref(), Some("Latest Title"));

        std::fs::write(
            sessions.join(format!("{id}.jsonl")),
            format!(
                r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-08-13T23:00:00.000Z","cwd":"/x"}}
{{"type":"session_info","id":"n1","parentId":null,"timestamp":"2026-08-13T23:00:01.000Z","name":"Latest Title"}}
{{"type":"message","id":"u1","parentId":"n1","timestamp":"2026-08-13T23:00:02.000Z","message":{{"role":"user","content":"prime-user-cobalt-orchard"}}}}
{{"type":"session_info","id":"n2","parentId":"u1","timestamp":"2026-08-13T23:00:03.000Z","name":""}}
"#
            ),
        )
        .expect("write cleared");
        let cleared = &PrimeAgentConnector::new().scan(&ctx).expect("cleared")[0];
        assert_eq!(cleared.title.as_deref(), Some("prime-user-cobalt-orchard"));
    }

    #[test]
    fn branches_keep_abandoned_and_summary() {
        let conv = &scan_fixture("branches")[0];
        let blob = conv
            .messages
            .iter()
            .map(|msg| msg.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(blob.contains("prime-abandoned-branch-magenta-path"));
        assert!(blob.contains("prime-branch-summary-blue-fork"));
        assert!(blob.contains("prime-append-white-quill"));
        assert_eq!(
            conv.messages
                .iter()
                .filter(|msg| msg.role == "system")
                .count(),
            1
        );
    }

    #[test]
    fn compaction_and_custom_are_searchable() {
        let conv = &scan_fixture("compaction_custom")[0];
        let blob = conv
            .messages
            .iter()
            .map(|msg| msg.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(blob.contains("prime-compaction-gold-archive"));
        assert!(blob.contains("prime-custom-context-teal-signal"));
        assert!(blob.contains("hidden custom context"));
        assert!(blob.contains("nested custom context"));
        assert_eq!(conv.metadata["custom_state_count"], 1);
        assert!(!serialized(conv).contains("do-not-copy-this-blob"));
        let custom = conv
            .messages
            .iter()
            .find(|msg| msg.content.contains("prime-custom-context-teal-signal"))
            .expect("custom");
        assert_eq!(custom.role, "system");
        assert_eq!(custom.author.as_deref(), Some("custom:prime-extension"));
        assert_eq!(custom.extra["prime"]["display"], true);
    }

    #[test]
    fn bash_images_and_large_line() {
        let conv = &scan_fixture("bash_images")[0];
        let blob = conv
            .messages
            .iter()
            .map(|msg| msg.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(blob.contains("prime-shell-command-indigo-rivet"));
        assert!(blob.contains("prime-shell-output-copper-harbor"));
        assert!(blob.contains("cancelled=true"));
        assert!(blob.contains("truncated=true"));
        assert!(blob.contains("!! hidden"));
        assert!(blob.contains("[image: image/jpeg]"));
        assert!(blob.contains("after-large-line"));
        assert!(!serialized(conv).contains("PRIME_BASE64_MUST_NOT_SURVIVE"));
        let hidden = conv
            .messages
            .iter()
            .find(|msg| msg.content.contains("!! hidden"))
            .expect("hidden shell");
        assert_eq!(hidden.extra["prime"]["exclude_from_context"], true);
        assert_eq!(hidden.invocations[0].name, "bash");
        assert_eq!(
            hidden.invocations[0].raw_name.as_deref(),
            Some("bashExecution")
        );
    }

    #[test]
    fn child_usage_is_not_double_counted() {
        let parent = &scan_fixture("rlm_parent")[0];
        let child = &scan_fixture("rlm_child")[0];
        assert_eq!(parent.metadata["rlm_depth"], 0);
        assert_eq!(child.metadata["rlm_depth"], 1);
        assert_eq!(parent.metadata["child_usage_attribution_count"], 1);
        let parent_assistant = parent
            .messages
            .iter()
            .find(|msg| msg.role == "assistant")
            .expect("parent assistant");
        let extracted = extract_tokens_for_agent(
            "prime_agent",
            &parent_assistant.extra,
            &parent_assistant.content,
            "assistant",
        );
        assert_eq!(extracted.input_tokens, Some(80));
        assert_eq!(extracted.output_tokens, Some(40));
        assert_ne!(extracted.input_tokens, Some(110));
        let child_assistant = child
            .messages
            .iter()
            .find(|msg| msg.role == "assistant")
            .expect("child assistant");
        let child_usage = extract_tokens_for_agent(
            "prime_agent",
            &child_assistant.extra,
            &child_assistant.content,
            "assistant",
        );
        let total = extracted.total_tokens().unwrap() + child_usage.total_tokens().unwrap();
        assert_eq!(total, 80 + 40 + 10 + 5 + 30 + 20);
        assert_ne!(total, 110 + 60 + 10 + 5 + 30 + 20);
    }

    #[test]
    fn versions_and_malformed_survive() {
        let v1 = &scan_fixture("legacy_v1")[0];
        assert_eq!(v1.external_id.as_deref(), Some("legacy-v1-session"));
        assert!(
            v1.messages
                .iter()
                .any(|msg| msg.content.contains("legacy v1 user"))
        );
        assert!(v1.messages[0].extra["prime"].get("source_offset").is_some());

        let v2 = &scan_fixture("legacy_v2_hook")[0];
        let hook = v2
            .messages
            .iter()
            .find(|msg| msg.content.contains("legacy hook context"))
            .expect("hook");
        assert_eq!(hook.role, "system");
        assert_eq!(hook.extra["prime"]["raw_role"], "hookMessage");

        let future = &scan_fixture("future_version")[0];
        assert!(
            future
                .messages
                .iter()
                .any(|msg| msg.content.contains("future known user"))
        );
        assert!(
            future
                .messages
                .iter()
                .any(|msg| msg.content.contains("future known assistant"))
        );
        assert!(
            future.metadata["diagnostic_counts"]["future_version"]
                .as_u64()
                .unwrap()
                >= 1
        );
        assert!(
            future.metadata["diagnostic_counts"]["unknown_entry_type"]
                .as_u64()
                .unwrap()
                >= 1
        );

        let malformed = &scan_fixture("malformed_line")[0];
        assert!(
            malformed
                .messages
                .iter()
                .any(|msg| msg.content.contains("before bad line"))
        );
        assert!(
            malformed
                .messages
                .iter()
                .any(|msg| msg.content.contains("after bad line"))
        );
        assert!(
            malformed.metadata["diagnostic_counts"]["invalid_json_line"]
                .as_u64()
                .unwrap()
                >= 1
        );

        assert!(scan_fixture("invalid_header").is_empty());
    }

    #[test]
    fn isolation_from_pi_and_omp() {
        let home = fixture_home("isolation");
        let ctx = ctx_for(&home);
        let prime = PrimeAgentConnector::new().scan(&ctx).expect("prime");
        let pi = PiAgentConnector::new().scan(&ctx).expect("pi");
        assert_eq!(prime.len(), 1);
        assert_eq!(prime[0].agent_slug, "prime_agent");
        assert!(
            prime[0].messages[0]
                .content
                .contains("prime-isolation-sentinel")
        );
        assert!(pi.iter().all(|conv| conv.agent_slug == "pi_agent"));
        assert!(pi.iter().any(|conv| {
            conv.messages
                .iter()
                .any(|msg| msg.content.contains("pi-isolation-sentinel"))
        }));
        assert!(prime.iter().all(|conv| {
            !serialized(conv).contains("pi-isolation-sentinel")
                && !serialized(conv).contains("omp-isolation-sentinel")
        }));
        assert!(pi.iter().all(|conv| {
            !conv
                .messages
                .iter()
                .any(|msg| msg.content.contains("prime-isolation-sentinel"))
        }));
        let discovered = PrimeAgentConnector::new()
            .discover_source_files(&ctx)
            .expect("discover");
        assert_eq!(discovered.len(), 1);
        assert!(
            discovered[0]
                .source_path
                .ends_with("0198f000-0000-7000-8000-000000000009.jsonl")
        );
        assert!(!discovered.iter().any(|src| {
            src.source_path
                .to_string_lossy()
                .contains("session-artifacts")
        }));
        assert!(
            !discovered
                .iter()
                .any(|src| src.source_path.to_string_lossy().contains("/logs/"))
        );
    }

    #[test]
    fn explicit_custom_root_requires_stem_match() {
        let tmp = TempDir::new().expect("tempdir");
        let custom = tmp.path().join("custom-prime-store");
        std::fs::create_dir_all(&custom).expect("mkdir");
        std::fs::write(
            custom.join("0198f000-0000-7000-8000-000000000099.jsonl"),
            r#"{"type":"session","version":3,"id":"0198f000-0000-7000-8000-000000000099","timestamp":"2026-08-13T20:00:00.000Z","cwd":"/fabricated/custom"}
{"type":"message","id":"u1","parentId":null,"timestamp":"2026-08-13T20:00:01.000Z","message":{"role":"user","content":"custom root"}}
"#,
        )
        .expect("write matching");
        std::fs::write(
            custom.join("random-name.jsonl"),
            r#"{"type":"session","version":3,"id":"does-not-match","timestamp":"2026-08-13T20:00:00.000Z","cwd":"/fabricated/custom"}
{"type":"message","id":"u1","parentId":null,"timestamp":"2026-08-13T20:00:01.000Z","message":{"role":"user","content":"should skip"}}
"#,
        )
        .expect("write mismatch");
        let ctx = ScanContext::with_roots(
            tmp.path().to_path_buf(),
            vec![ScanRoot::local(custom)],
            None,
        );
        let convs = PrimeAgentConnector::new().scan(&ctx).expect("scan");
        assert_eq!(convs.len(), 1);
        assert_eq!(
            convs[0].external_id.as_deref(),
            Some("0198f000-0000-7000-8000-000000000099")
        );
    }

    #[test]
    fn workspace_rewrite_and_remote_provenance() {
        let home = fixture_home("v3_full");
        let origin = Origin::remote("laptop");
        let root = ScanRoot::remote(home.clone(), origin.clone(), Some(Platform::Linux))
            .with_rewrite("/fabricated/prime", "/local/prime");
        let ctx = ScanContext::with_roots(home.join("cass-data"), vec![root], None);
        let connector = PrimeAgentConnector::new();
        let convs = connector.scan(&ctx).expect("scan");
        assert_eq!(
            convs[0].workspace,
            Some(PathBuf::from("/local/prime/project"))
        );
        let discovered = connector.discover_source_files(&ctx).expect("discover");
        assert!(discovered[0].origin.is_remote());
        assert_eq!(discovered[0].platform, Some(Platform::Linux));
        assert_eq!(discovered[0].provider_slug, "prime_agent");
        assert_eq!(discovered[0].role, DiscoveredSourceRole::PrimarySessionLog);
        assert!(discovered[0].required_for_reconstruction);
    }

    #[test]
    fn streaming_since_ts_and_sibling_isolation() {
        let home = fixture_home("v3_full");
        let ctx = ctx_for(&home);
        let connector = PrimeAgentConnector::new();
        let scanned = connector.scan(&ctx).expect("scan");
        let mut streamed = Vec::new();
        let mut first_emit_at = None;
        connector
            .scan_with_callback(&ctx, &mut |conv| {
                if first_emit_at.is_none() {
                    first_emit_at = Some(streamed.len());
                }
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
        assert_eq!(first_emit_at, Some(0));

        let tmp = TempDir::new().expect("tempdir");
        let sessions = tmp.path().join(".prime/agent/sessions");
        std::fs::create_dir_all(&sessions).expect("mkdir");
        std::fs::write(
            sessions.join("0198f000-0000-7000-8000-000000000010.jsonl"),
            r#"{"type":"session","version":3,"id":"0198f000-0000-7000-8000-000000000010","timestamp":"2026-08-13T21:00:00.000Z","cwd":"/a"}
{"type":"message","id":"a","parentId":null,"timestamp":"2026-08-13T21:00:01.000Z","message":{"role":"user","content":"old"}}
"#,
        )
        .expect("old");
        std::fs::write(
            sessions.join("0198f000-0000-7000-8000-000000000011.jsonl"),
            r#"{"type":"session","version":3,"id":"0198f000-0000-7000-8000-000000000011","timestamp":"2026-08-13T21:00:00.000Z","cwd":"/b"}
{"type":"message","id":"b","parentId":null,"timestamp":"2026-08-13T21:00:01.000Z","message":{"role":"user","content":"new"}}
"#,
        )
        .expect("new");
        let old = sessions.join("0198f000-0000-7000-8000-000000000010.jsonl");
        let past = SystemTime::now() - Duration::from_secs(4000);
        filetime_set(&old, past);
        let since = i64::try_from(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
            - 1000;
        let ctx = ScanContext::with_roots(
            tmp.path().to_path_buf(),
            vec![ScanRoot::local(tmp.path().to_path_buf())],
            Some(since),
        );
        let convs = connector.scan(&ctx).expect("since");
        assert_eq!(convs.len(), 1);
        assert_eq!(
            convs[0].external_id.as_deref(),
            Some("0198f000-0000-7000-8000-000000000011")
        );

        std::fs::write(sessions.join("broken.jsonl"), "{not-json\n").expect("broken");
        let all_ctx = ScanContext::with_roots(
            tmp.path().to_path_buf(),
            vec![ScanRoot::local(tmp.path().to_path_buf())],
            None,
        );
        let after_broken = connector.scan(&all_ctx).expect("sibling");
        assert!(after_broken.len() >= 2);
    }

    fn filetime_set(path: &Path, time: SystemTime) {
        let file = File::options().write(true).open(path).expect("open");
        file.set_modified(time).expect("set mtime");
    }

    #[test]
    fn progress_tick_and_callback_multi_file() {
        let tmp = TempDir::new().expect("tempdir");
        let sessions = tmp.path().join(".prime/agent/sessions");
        std::fs::create_dir_all(&sessions).expect("mkdir");
        for (idx, id) in [
            "0198f000-0000-7000-8000-000000000021",
            "0198f000-0000-7000-8000-000000000022",
        ]
        .into_iter()
        .enumerate()
        {
            let mut body = format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-08-13T22:00:00.000Z\",\"cwd\":\"/x\"}}\n"
            );
            for i in 0..40 {
                writeln!(
                    body,
                    "{{\"type\":\"message\",\"id\":\"m{i}\",\"parentId\":null,\"timestamp\":\"2026-08-13T22:00:01.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"row-{idx}-{i}\"}}}}"
                )
                .expect("write session line");
            }
            std::fs::write(sessions.join(format!("{id}.jsonl")), body).expect("write");
        }
        let ticks = Arc::new(AtomicUsize::new(0));
        let ticks_clone = ticks.clone();
        let seen = Arc::new(AtomicUsize::new(0));
        let seen_clone = seen.clone();
        let ctx = ScanContext::with_roots(
            tmp.path().to_path_buf(),
            vec![ScanRoot::local(tmp.path().to_path_buf())],
            None,
        )
        .with_progress_tick(Arc::new(move || {
            ticks_clone.fetch_add(1, Ordering::SeqCst);
        }));
        PrimeAgentConnector::new()
            .scan_with_callback(&ctx, &mut |conv| {
                seen_clone.fetch_add(1, Ordering::SeqCst);
                assert_eq!(conv.agent_slug, "prime_agent");
                Ok(())
            })
            .expect("callback");
        assert_eq!(seen.load(Ordering::SeqCst), 2);
        assert!(ticks.load(Ordering::SeqCst) >= 2);
    }
}
