#![allow(dead_code)]

//! Session rollout files hold what `thread/resume` leaves out — shell runs above
//! all. This module reads one rollout into timestamped events so the transcript
//! can be rebuilt with those runs back in place.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::pricing::{CostLedger, TokenTotals};

/// One rollout entry worth showing, in file order.
pub struct RolloutEvent {
    /// RFC 3339, always UTC — sorts correctly as a plain string.
    pub ts: String,
    /// The app-server turn this event belongs to, when the payload says so
    /// directly (`custom_tool_call*`'s `internal_chat_message_metadata_passthrough`,
    /// `patch_apply_end`'s own `turn_id`). `None` means the caller has to fall
    /// back to comparing timestamps against the turn's start/end window.
    pub turn_id: Option<String>,
    pub kind: RolloutKind,
}

pub enum RolloutKind {
    /// A shell run. `thread/resume` drops these entirely.
    Exec {
        group_id: String,
        command: String,
        output: String,
        exit_code: Option<i64>,
        duration_ms: Option<u64>,
    },
    /// A reasoning summary. Present-day rollouts leave `summary` empty and keep
    /// only `encrypted_content`, so this rarely fires.
    Reasoning { summary: String },
    /// Anchors an app-server `fileChange` item to a timestamp: its `call_id`
    /// equals the item's id.
    PatchApplied { call_id: String },
    /// Anchors an app-server `agentMessage` item to a timestamp.
    AssistantMessage { text: String },
}

pub struct Rollout {
    pub events: Vec<RolloutEvent>,
    pub last_plan: Option<PlanSnapshot>,
    turn_contexts: Vec<(String, String)>,
}

impl Rollout {
    /// The model effective when a turn started, reconstructed from the latest
    /// local `turn_context` record at or before that timestamp. A turn older
    /// than every recorded context — the shape a resumed session leaves behind,
    /// where the replayed history predates the first context written in this
    /// run — takes the earliest one instead of nothing, so the prompt still
    /// carries a model rather than falling back to the plain accent.
    pub fn model_for_turn(&self, started_at: i64) -> Option<&str> {
        self.turn_contexts
            .iter()
            .filter(|(timestamp, _)| {
                chrono::DateTime::parse_from_rfc3339(timestamp)
                    .ok()
                    .is_some_and(|time| time.timestamp() <= started_at)
            })
            .last()
            .or_else(|| self.turn_contexts.first())
            .map(|(_, model)| model.as_str())
    }
}

/// Most recent `update_plan` payload recorded for a session.
pub struct PlanSnapshot {
    pub explanation: Option<String>,
    pub steps: Vec<PlanStepSnapshot>,
}

pub struct PlanStepSnapshot {
    pub text: String,
    pub status: String,
    pub elapsed_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct RolloutStamp {
    path: PathBuf,
    length: u64,
    modified_nanos: u128,
}

#[derive(Deserialize, Serialize)]
struct CostCache {
    rollout: RolloutStamp,
    ledger: CostLedger,
}

/// The rollout for `thread_id`, or `None` when there is none to read — a session
/// created on another machine, or a schema this parser no longer recognises.
pub fn load(codex_home: &Path, thread_id: &str) -> Option<Rollout> {
    let path = find_rollout(&codex_home.join("sessions"), thread_id)?;
    let text = fs::read_to_string(path).ok()?;
    Some(parse(&text))
}

/// Reads a compact cached cost ledger when it still describes the exact
/// rollout on disk. A cache miss falls back to one local JSONL pass; callers
/// put that work off the UI path.
pub fn load_cost_ledger(codex_home: &Path, thread_id: &str) -> Option<CostLedger> {
    load_matching_cost_cache(codex_home, thread_id).or_else(|| {
        let path = find_rollout(&codex_home.join("sessions"), thread_id)?;
        rebuild_cost_cache(codex_home, thread_id, path)
    })
}

fn load_matching_cost_cache(codex_home: &Path, thread_id: &str) -> Option<CostLedger> {
    let cache = fs::read(cost_cache_path(codex_home, thread_id))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<CostCache>(&bytes).ok())?;
    (rollout_stamp(&cache.rollout.path).as_ref() == Some(&cache.rollout)).then_some(cache.ledger)
}

fn rebuild_cost_cache(codex_home: &Path, thread_id: &str, path: PathBuf) -> Option<CostLedger> {
    let before = rollout_stamp(&path)?;
    let text = fs::read_to_string(&path).ok()?;
    let after = rollout_stamp(&path)?;
    if before != after {
        return None;
    }
    let ledger = cost_ledger_from_text(&text);
    let cache = CostCache {
        rollout: after,
        ledger: ledger.clone(),
    };
    let path = cost_cache_path(codex_home, thread_id);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        let _ = fs::write(path, bytes);
    }
    Some(ledger)
}

fn rollout_stamp(path: &Path) -> Option<RolloutStamp> {
    let metadata = fs::metadata(path).ok()?;
    let modified_nanos = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(RolloutStamp {
        path: path.to_path_buf(),
        length: metadata.len(),
        modified_nanos,
    })
}

fn cost_cache_path(codex_home: &Path, thread_id: &str) -> PathBuf {
    let file_name = thread_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    codex_home
        .join("devez-vibe")
        .join("cost-cache")
        .join(format!("{file_name}.json"))
}

/// Rollout file names end in the thread id, so the whole session tree is walked
/// looking for that suffix rather than guessing the date directory.
fn find_rollout(root: &Path, thread_id: &str) -> Option<PathBuf> {
    let suffix = format!("-{thread_id}.jsonl");
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("rollout-") && name.ends_with(suffix.as_str()) {
                return Some(path);
            }
        }
    }
    None
}

/// One JSONL line per entry. A line that fails to parse — or an entry type this
/// build does not know — is dropped on its own; the rest of the file still reads.
pub fn parse(text: &str) -> Rollout {
    let mut events = Vec::new();
    let mut turn_contexts = Vec::new();
    let mut last_plan = None;
    let mut plan_started_at = HashMap::new();
    let mut plan_elapsed = HashMap::new();
    // Exec calls whose output has not arrived yet: `call_id` → the indices in
    // `events` its output segments fill in, in the order the script's calls
    // ran (a script can run more than one `shell_command` per turn).
    let mut pending: Vec<(String, Vec<usize>)> = Vec::new();
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ts = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some(payload) = entry.get("payload") else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) == Some("turn_context") {
            if let Some(model) = payload.get("model").and_then(Value::as_str) {
                turn_contexts.push((ts, model.to_owned()));
            }
            continue;
        }
        match payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "custom_tool_call" => {
                // `name` is the real discriminant between a shell run and a
                // patch application — not where `tools.shell_command(` happens
                // to show up in the script's text, which a quoted patch body
                // can fake.
                if payload.get("name").and_then(Value::as_str) != Some("exec") {
                    continue;
                }
                let input = payload
                    .get("input")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(plan) = plan_snapshot(input) {
                    let timestamp = chrono::DateTime::parse_from_rfc3339(&ts)
                        .ok()
                        .and_then(|time| u64::try_from(time.timestamp_millis()).ok());
                    last_plan = Some(with_plan_elapsed(
                        plan,
                        timestamp,
                        &mut plan_started_at,
                        &mut plan_elapsed,
                    ));
                    continue;
                }
                let commands = shell_commands(input);
                if commands.is_empty() {
                    continue;
                }
                let call_id = payload
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let turn_id = call_turn_id(payload);
                let indices = commands
                    .into_iter()
                    .map(|command| {
                        let index = events.len();
                        events.push(RolloutEvent {
                            ts: ts.clone(),
                            turn_id: turn_id.clone(),
                            kind: RolloutKind::Exec {
                                group_id: call_id.clone(),
                                command,
                                output: String::new(),
                                exit_code: None,
                                duration_ms: None,
                            },
                        });
                        index
                    })
                    .collect::<Vec<_>>();
                pending.push((call_id, indices));
            }
            "custom_tool_call_output" => {
                let call_id = payload
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let Some(position) = pending.iter().position(|(id, _)| id == call_id) else {
                    continue;
                };
                let (_, indices) = pending.remove(position);
                let text = output_text(payload.get("output"));
                let duration_ms = parse_wall_time_ms(&text);
                let results = command_results(&text, indices.len());
                for (index, (exit_code, body)) in indices.into_iter().zip(results) {
                    if let RolloutKind::Exec {
                        output,
                        exit_code: slot_exit,
                        duration_ms: slot_duration,
                        ..
                    } = &mut events[index].kind
                    {
                        *slot_exit = exit_code;
                        *slot_duration = duration_ms;
                        *output = body;
                    }
                }
            }
            "reasoning" => {
                let summary = summary_text(payload.get("summary"));
                if summary.is_empty() {
                    continue;
                }
                events.push(RolloutEvent {
                    ts,
                    turn_id: None,
                    kind: RolloutKind::Reasoning { summary },
                });
            }
            "patch_apply_end" => {
                let Some(call_id) = payload.get("call_id").and_then(Value::as_str) else {
                    continue;
                };
                let turn_id = payload
                    .get("turn_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                events.push(RolloutEvent {
                    ts,
                    turn_id,
                    kind: RolloutKind::PatchApplied {
                        call_id: call_id.to_owned(),
                    },
                });
            }
            // The `event_msg` form carries `message`; the `response_item` twin has
            // no such field and is skipped, so each message anchors exactly once.
            // Neither form carries a `turn_id` in practice, so this always falls
            // back to the turn's time window.
            "agent_message" => {
                let Some(text) = payload.get("message").and_then(Value::as_str) else {
                    continue;
                };
                events.push(RolloutEvent {
                    ts,
                    turn_id: None,
                    kind: RolloutKind::AssistantMessage {
                        text: text.to_owned(),
                    },
                });
            }
            _ => {}
        }
    }
    Rollout {
        events,
        last_plan,
        turn_contexts,
    }
}

fn plan_snapshot(input: &str) -> Option<PlanSnapshot> {
    let call = input.find("tools.update_plan(")?;
    let input = &input[call..];
    let plan_key = input.find("plan:")? + "plan:".len();
    let after_key = &input[plan_key..];
    let whitespace = after_key.len().saturating_sub(after_key.trim_start().len());
    let plan = plan_key + whitespace;
    input[plan..].strip_prefix('[')?;
    let plan = plan + 1;
    let mut depth = 1usize;
    let mut end = plan;
    let mut quoted = false;
    let mut escaped = false;
    for (offset, ch) in input[plan..].char_indices() {
        if quoted {
            escaped = ch == '\\' && !escaped;
            if ch == '"' && !escaped { quoted = false; }
            continue;
        }
        if ch == '"' { quoted = true; continue; }
        if ch == '[' { depth += 1; }
        if ch == ']' { depth -= 1; if depth == 0 { end = plan + offset; break; } }
    }
    (depth == 0).then_some(())?;
    let body = &input[plan..end];
    let mut steps = Vec::new();
    for item in body.split("step:").skip(1) {
        let step = js_string(item)?;
        let status_at = item.find("status:")? + "status:".len();
        let status = js_string(&item[status_at..])?;
        steps.push(PlanStepSnapshot { text: step, status, elapsed_ms: None });
    }
    (!steps.is_empty()).then_some(PlanSnapshot { explanation: None, steps })
}

fn with_plan_elapsed(
    mut plan: PlanSnapshot,
    timestamp: Option<u64>,
    started_at: &mut HashMap<String, u64>,
    elapsed: &mut HashMap<String, u64>,
) -> PlanSnapshot {
    for step in &mut plan.steps {
        match (step.status.as_str(), timestamp) {
            ("in_progress", Some(now)) => {
                started_at.entry(step.text.clone()).or_insert(now);
            }
            ("completed", Some(now)) => {
                let duration = started_at.remove(&step.text).map(|started| now.saturating_sub(started)).unwrap_or(0);
                elapsed.insert(step.text.clone(), duration);
            }
            _ => {}
        }
        step.elapsed_ms = elapsed.get(&step.text).copied().or_else(|| {
            timestamp.and_then(|now| started_at.get(&step.text).map(|started| now.saturating_sub(*started)))
        });
    }
    plan
}

fn js_string(input: &str) -> Option<String> {
    let input = input.trim_start();
    let rest = input.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].replace("\\\"", "\""))
}

/// Replays the compact accounting records a rollout keeps alongside its
/// transcript. Each token count is cumulative, so only its increase is added
/// to the model named by the latest turn context.
fn cost_ledger_from_text(text: &str) -> CostLedger {
    let mut ledger = CostLedger::default();
    let mut model = None;
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(payload) = entry.get("payload") else {
            continue;
        };
        match entry.get("type").and_then(Value::as_str) {
            Some("turn_context") => {
                model = payload
                    .get("model")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
            }
            Some("event_msg")
                if payload.get("type").and_then(Value::as_str) == Some("token_count") =>
            {
                let Some(total) = payload
                    .get("info")
                    .and_then(|info| info.get("total_token_usage"))
                else {
                    continue;
                };
                ledger.record_cumulative(
                    model.as_deref().unwrap_or("unknown"),
                    TokenTotals::from_breakdown(total),
                );
            }
            _ => {}
        }
    }
    ledger
}

/// A `custom_tool_call`/`custom_tool_call_output` payload's turn id, tucked
/// under `internal_chat_message_metadata_passthrough` rather than sitting on
/// the payload itself the way `patch_apply_end`'s does.
fn call_turn_id(payload: &Value) -> Option<String> {
    payload
        .get("internal_chat_message_metadata_passthrough")
        .and_then(|passthrough| passthrough.get("turn_id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn summary_text(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every `shell_command` call's command in `input`, in the order the calls
/// appear. A script may make more than one in a single `exec` turn —
/// `Promise.all`, a loop over a list of commands — and each becomes its own
/// entry here. A call this parser cannot read (see `call_command`) is simply
/// left out rather than aborting the whole script.
fn shell_commands(input: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut cursor = 0usize;
    while let Some(offset) = input[cursor..].find("tools.shell_command(") {
        let after_call = cursor + offset + "tools.shell_command(".len();
        if let Some(command) = call_command(&input[after_call..]) {
            commands.push(command);
        }
        cursor = after_call;
    }
    commands
}

/// The `command` value of one call's own argument object. The parser anchors
/// on the call's own opening parenthesis — the next non-space character must
/// itself be `{` — instead of searching forward for any `{…}` in the rest of
/// the script, which would happily grab a later, unrelated call's arguments
/// (exactly the failure a bare `find('{')` had).
fn call_command(after_call: &str) -> Option<String> {
    let trimmed = after_call.trim_start();
    if !trimmed.starts_with('{') {
        return None;
    }
    let object = balanced_object(trimmed)?;
    command_field(&object[1..object.len() - 1])
}

/// The first `{…}` run in `text`, counting nesting and ignoring braces inside
/// strings. The call's arguments are hand-written JavaScript, so a plain
/// `find('}')` would stop at the first brace a command happens to contain.
fn balanced_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for (offset, character) in text[start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '{' if !quoted => depth += 1,
            '}' if !quoted => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + offset + character.len_utf8()]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Scans one object literal's top-level fields — skipping over quoted strings
/// and nested brackets so a command's own text never derails it — for
/// `command`, bare or quoted, and reads its value. Model-written scripts use
/// both spellings about as often as each other (`{command: "…"}` and
/// `{"command":"…"}`). A value that isn't a literal string or string array —
/// a variable (`{command: c}`), a shorthand property with no value of its own
/// (`{command}`) — cannot be read without evaluating the script, so it yields
/// `None`.
fn command_field(body: &str) -> Option<String> {
    let mut i = 0usize;
    while i < body.len() {
        let ch = body[i..].chars().next()?;
        if ch == '"' || ch == '\'' || ch == '`' || ch.is_alphabetic() || ch == '_' {
            let (key, after_key) = read_token(body, i);
            let after_gap = skip_whitespace(body, after_key);
            if body[after_gap..].starts_with(':') {
                let value_start = skip_whitespace(body, after_gap + 1);
                if key == "command" {
                    return command_value(body, value_start);
                }
                i = skip_value(body, value_start);
            } else {
                // A shorthand property: the token is both key and value, so
                // there is nothing further to read even when it is the one
                // named `command`.
                if key == "command" {
                    return None;
                }
                i = after_key;
            }
        } else {
            i += ch.len_utf8();
        }
    }
    None
}

fn skip_whitespace(body: &str, mut i: usize) -> usize {
    while i < body.len() {
        let ch = body[i..]
            .chars()
            .next()
            .expect("i is a char boundary within bounds");
        if ch.is_whitespace() {
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    i
}

/// One object key, quoted or bare, starting at `i`. Quotes are stripped;
/// escapes are not decoded because key names never carry any that matter here.
fn read_token(body: &str, i: usize) -> (String, usize) {
    let ch = body[i..]
        .chars()
        .next()
        .expect("i is a char boundary within bounds");
    if ch == '"' || ch == '\'' || ch == '`' {
        let end = skip_string(body, i);
        (body[i + ch.len_utf8()..end - ch.len_utf8()].to_owned(), end)
    } else {
        let mut end = i;
        for c in body[i..].chars() {
            if c.is_alphanumeric() || c == '_' {
                end += c.len_utf8();
            } else {
                break;
            }
        }
        (body[i..end].to_owned(), end)
    }
}

/// Advances past one quoted string (`"`, `'`, or a template literal) to the
/// index just after its closing quote. A backslash escapes the next
/// character, including the quote itself, so an escaped quote never ends the
/// string early.
fn skip_string(body: &str, i: usize) -> usize {
    let quote = body[i..]
        .chars()
        .next()
        .expect("i is a char boundary within bounds");
    let mut j = i + quote.len_utf8();
    let mut escaped = false;
    while j < body.len() {
        let ch = body[j..]
            .chars()
            .next()
            .expect("j is a char boundary within bounds");
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            return j + ch.len_utf8();
        }
        j += ch.len_utf8();
    }
    j
}

/// Advances past one field's value — string, array, nested object, number,
/// boolean — to the delimiter that ends it: a top-level comma or the
/// enclosing `}`/`]`. Used to step over a field this parser does not care
/// about without losing its place in the rest of the object.
fn skip_value(body: &str, mut i: usize) -> usize {
    let mut depth = 0i32;
    while i < body.len() {
        let ch = body[i..]
            .chars()
            .next()
            .expect("i is a char boundary within bounds");
        match ch {
            '"' | '\'' | '`' => {
                i = skip_string(body, i);
                continue;
            }
            '{' | '[' | '(' => depth += 1,
            '}' | ')' | ']' if depth > 0 => depth -= 1,
            ',' if depth == 0 => return i,
            '}' | ']' if depth == 0 => return i,
            _ => {}
        }
        i += ch.len_utf8();
    }
    i
}

/// The `command` field's value once its `:` has been found: a plain string,
/// or an array of strings joined with spaces (the code-mode wrapper's argv
/// form). A number, object, or bare variable is not a literal command, so it
/// yields `None`.
fn command_value(body: &str, i: usize) -> Option<String> {
    let ch = body[i..].chars().next()?;
    match ch {
        '"' | '\'' | '`' => Some(unescape(&read_token(body, i).0)),
        '[' => array_command(body, i),
        _ => None,
    }
}

/// A `command` value written as `["bash", "-lc", "ls"]`, joined with spaces to
/// match the plain-string case. A non-string element is dropped rather than
/// failing the whole call.
fn array_command(body: &str, start: usize) -> Option<String> {
    let mut i = start + 1; // past '['
    let mut parts = Vec::new();
    loop {
        i = skip_whitespace(body, i);
        let ch = body[i..].chars().next()?;
        if ch == ']' {
            break;
        }
        if ch == '"' || ch == '\'' || ch == '`' {
            let (raw, after) = read_token(body, i);
            parts.push(unescape(&raw));
            i = after;
        } else {
            i = skip_value(body, i);
        }
        i = skip_whitespace(body, i);
        match body[i..].chars().next()? {
            ',' => i += 1,
            ']' => break,
            _ => return None,
        }
    }
    Some(parts.join(" "))
}

/// Decodes the backslash escapes a hand-written JS string literal uses (`\\`,
/// `\"`, `\n`, …). An escape this does not recognise passes through as the
/// character itself, which is good enough for a shell command's text.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

fn output_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Splits one `custom_tool_call_output` payload's text into a result per
/// `shell_command` call the script made, in the same order the calls appear
/// in `commands`. A script with exactly one call has no `---label---`
/// framing — the whole (wrapper-stripped) text is that command's own result.
/// A script with several wraps each one as `---label---Exit code: … / Wall
/// time: … / Output: …`, in call order, whether the calls ran through
/// `Promise.all` or a plain loop.
fn command_results(text: &str, calls: usize) -> Vec<(Option<i64>, String)> {
    if calls <= 1 {
        // The command's own header starts at `Exit code:` — found directly
        // rather than by skipping past the wrapper's own `Output:` marker,
        // because not every payload has one ahead of the header (a `Wall
        // time` line with no preceding `Output:` is a real, if degenerate,
        // shape) and guessing wrong would eat the header along with it.
        let header_and_body = match text.find("Exit code:") {
            Some(index) => &text[index..],
            None => text,
        };
        return vec![command_result(header_and_body)];
    }
    let markers = marker_positions(text);
    let mut results: Vec<(Option<i64>, String)> = markers
        .iter()
        .enumerate()
        .map(|(position, &(_marker_start, header_start))| {
            let end = markers
                .get(position + 1)
                .map(|&(next_start, _)| next_start)
                .unwrap_or(text.len());
            command_result(&text[header_start..end])
        })
        .collect();
    while results.len() < calls {
        results.push((None, String::new()));
    }
    results
}

/// One shell run's exit code and (wrapper-stripped) body, read starting
/// exactly at its own `Exit code: N` header. Finding the *first* `Output:`
/// from there — never the last — is what keeps a real body that happens to
/// contain the literal text `Output:` intact instead of truncated.
fn command_result(header_and_body: &str) -> (Option<i64>, String) {
    let exit_code = parse_exit_code(header_and_body);
    let body = match header_and_body.find("Output:") {
        Some(index) => header_and_body[index + "Output:".len()..].trim_start_matches('\n'),
        None => header_and_body,
    };
    (exit_code, body.to_owned())
}

/// Where a multi-call wrapper starts a new command's segment —
/// `---label---Exit code:` — paired with the index right after that marker,
/// where the segment's own header begins. A `---` that turns up inside a
/// command's real output is not immediately followed by `Exit code:`, so it
/// is never mistaken for one.
fn marker_positions(text: &str) -> Vec<(usize, usize)> {
    let mut positions = Vec::new();
    let mut search_from = 0usize;
    while let Some(offset) = text[search_from..].find("---") {
        let start = search_from + offset;
        let after_open = start + 3;
        match text[after_open..].find("---") {
            Some(label_len) => {
                let header_start = after_open + label_len + 3;
                if text[header_start..].starts_with("Exit code:") {
                    positions.push((start, header_start));
                    search_from = header_start;
                } else {
                    search_from = start + 3;
                }
            }
            None => break,
        }
    }
    positions
}

/// The inner `Exit code: N` the code-mode wrapper prints for the command itself.
fn parse_exit_code(output: &str) -> Option<i64> {
    let index = output.find("Exit code:")?;
    let rest = output[index + "Exit code:".len()..].trim_start();
    let digits = rest
        .chars()
        .take_while(|character| character.is_ascii_digit() || *character == '-')
        .collect::<String>();
    digits.parse().ok()
}

/// The wrapper's own `Wall time 1.6 seconds` line — the first of the two, which
/// covers the whole script rather than one command inside it.
fn parse_wall_time_ms(output: &str) -> Option<u64> {
    let index = output.find("Wall time")?;
    let rest = output[index + "Wall time".len()..]
        .trim_start()
        .trim_start_matches(':')
        .trim_start();
    let number = rest
        .chars()
        .take_while(|character| character.is_ascii_digit() || *character == '.')
        .collect::<String>();
    let seconds = number.parse::<f64>().ok()?;
    Some((seconds * 1000.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_plan_call_keeps_the_latest_plan_snapshot() {
        let rollout = parse(
            r#"{"timestamp":"2026-07-28T02:09:00.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"const r = await tools.update_plan({ plan: [{step:\"확인\",status:\"in_progress\"},{step:\"수정\",status:\"pending\"}]});"}}
{"timestamp":"2026-07-28T02:09:06.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"const r = await tools.update_plan({ plan: [{step:\"확인\",status:\"completed\"},{step:\"수정\",status:\"in_progress\"}]});"}}"#,
        );

        let plan = rollout.last_plan.expect("plan snapshot");
        assert_eq!(plan.steps[0].text, "확인");
        assert_eq!(plan.steps[0].status, "completed");
        assert_eq!(plan.steps[0].elapsed_ms, Some(6_000));
        assert_eq!(plan.steps[1].text, "수정");
        assert_eq!(plan.steps[1].status, "in_progress");
    }

    #[test]
    fn assistant_and_patch_events_keep_their_file_order() {
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:08:33.387Z","type":"event_msg","payload":{"type":"agent_message","message":"first"}}
{"timestamp":"2026-07-25T15:09:40.539Z","type":"event_msg","payload":{"type":"patch_apply_end","call_id":"exec-abc","success":true}}
{"timestamp":"2026-07-25T15:09:58.000Z","type":"event_msg","payload":{"type":"agent_message","message":"second"}}"#,
        );

        let summary = rollout
            .events
            .iter()
            .map(|event| match &event.kind {
                RolloutKind::AssistantMessage { text } => format!("assistant:{text}"),
                RolloutKind::PatchApplied { call_id } => format!("patch:{call_id}"),
                _ => "other".to_owned(),
            })
            .collect::<Vec<_>>();

        assert_eq!(
            summary,
            ["assistant:first", "patch:exec-abc", "assistant:second"]
        );
        assert_eq!(rollout.events[1].ts, "2026-07-25T15:09:40.539Z");
    }

    #[test]
    fn a_broken_line_costs_only_that_line() {
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:08:33.387Z","type":"event_msg","payload":{"type":"agent_message","message":"kept"}}
{ this is not json
{"timestamp":"2026-07-25T15:08:34.000Z","type":"event_msg","payload":{"type":"patch_apply_end","call_id":"exec-abc"}}"#,
        );

        assert_eq!(rollout.events.len(), 2);
    }

    #[test]
    fn reasoning_without_a_summary_is_dropped() {
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:08:32.476Z","type":"response_item","payload":{"type":"reasoning","summary":[],"encrypted_content":"gAAA"}}
{"timestamp":"2026-07-25T15:08:33.000Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"둘러본다"}]}}"#,
        );

        assert_eq!(rollout.events.len(), 1);
        assert!(matches!(
            &rollout.events[0].kind,
            RolloutKind::Reasoning { summary } if summary == "둘러본다"
        ));
    }

    #[test]
    fn load_picks_the_file_whose_name_ends_with_the_thread_id() {
        // A unique directory per test run: this test used to share a fixed
        // path (`dvz-rollout-load-test`) across the whole process, which was
        // unsafe under `cargo test`'s parallel runner and left the directory
        // behind whenever an assertion failed before the cleanup ran.
        let home = std::env::temp_dir().join(format!(
            "dvz-rollout-load-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after the epoch")
                .as_nanos()
        ));
        let day = home.join("sessions").join("2026").join("07").join("26");
        fs::create_dir_all(&day).expect("test directory");
        fs::write(
            day.join("rollout-2026-07-26T00-08-28-thread-wanted.jsonl"),
            "{\"timestamp\":\"2026-07-26T00:08:33.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"wanted\"}}\n",
        )
        .expect("wanted rollout");
        fs::write(
            day.join("rollout-2026-07-26T00-09-28-thread-other.jsonl"),
            "{\"timestamp\":\"2026-07-26T00:09:33.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"other\"}}\n",
        )
        .expect("other rollout");

        let rollout = load(&home, "thread-wanted").expect("rollout for the thread");
        let missing = load(&home, "thread-absent");
        fs::remove_dir_all(&home).ok();

        assert!(matches!(
            &rollout.events[0].kind,
            RolloutKind::AssistantMessage { text } if text == "wanted"
        ));
        assert!(missing.is_none());
    }

    #[test]
    fn cost_ledger_assigns_token_deltas_to_the_latest_turn_model() {
        let ledger = cost_ledger_from_text(
            r#"{"timestamp":"1","type":"turn_context","payload":{"model":"gpt-5.6-sol"}}
{"timestamp":"2","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000000,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":0}}}}
{"timestamp":"3","type":"turn_context","payload":{"model":"gpt-5.6-terra"}}
{"timestamp":"4","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2000000,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":0}}}}"#,
        );

        // 1M input on sol ($5) + the 1M delta on terra ($2).
        assert_eq!(ledger.estimate_usd(), Some(7.0));
    }

    #[test]
    fn rollout_restores_the_model_active_when_a_turn_started() {
        let rollout = parse(
            r#"{"timestamp":"1970-01-01T00:00:01.000Z","type":"turn_context","payload":{"model":"gpt-5.6-sol"}}
{"timestamp":"1970-01-01T00:00:02.000Z","type":"turn_context","payload":{"model":"gpt-5.6-terra"}}"#,
        );

        assert_eq!(rollout.model_for_turn(1), Some("gpt-5.6-sol"));
        assert_eq!(rollout.model_for_turn(2), Some("gpt-5.6-terra"));
    }

    #[test]
    fn cost_ledger_cache_is_rebuilt_when_the_rollout_changes() {
        let home = std::env::temp_dir().join(format!(
            "dvz-cost-cache-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after the epoch")
                .as_nanos()
        ));
        let day = home.join("sessions").join("2026").join("07").join("26");
        fs::create_dir_all(&day).expect("session directory");
        let rollout = day.join("rollout-2026-07-26T00-08-28-thread-cost.jsonl");
        fs::write(
            &rollout,
            r#"{"type":"turn_context","payload":{"model":"gpt-5.6-sol"}}
{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000000}}}}"#,
        )
        .expect("first rollout");

        assert_eq!(
            load_cost_ledger(&home, "thread-cost").and_then(|ledger| ledger.estimate_usd()),
            Some(5.0)
        );

        fs::write(
            &rollout,
            r#"{"type":"turn_context","payload":{"model":"gpt-5.6-sol"}}
{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000000}}}}
{"type":"turn_context","payload":{"model":"gpt-5.6-terra"}}
{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2000000}}}}"#,
        )
        .expect("changed rollout");

        assert_eq!(
            load_cost_ledger(&home, "thread-cost").and_then(|ledger| ledger.estimate_usd()),
            Some(7.0)
        );
        fs::remove_dir_all(&home).ok();
    }

    /// Helper for the exec tests: every `Exec` event a rollout produced, in
    /// order.
    fn exec_events(rollout: &Rollout) -> Vec<(&str, &str, Option<i64>, Option<u64>)> {
        rollout
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                RolloutKind::Exec {
                    command,
                    output,
                    exit_code,
                    duration_ms,
                    ..
                } => Some((command.as_str(), output.as_str(), *exit_code, *duration_ms)),
                _ => None,
            })
            .collect()
    }

    /// Helper for the exec tests: the single `Exec` event a rollout produced.
    fn only_exec(rollout: &Rollout) -> (&str, &str, Option<i64>, Option<u64>) {
        let mut events = exec_events(rollout);
        assert_eq!(events.len(), 1, "expected exactly one exec event");
        events.remove(0)
    }

    #[test]
    fn a_string_shell_command_becomes_an_exec_event() {
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:08:36.373Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_one","input":"const r = await tools.shell_command({\"command\":\"cargo test --quiet\",\"workdir\":\"C:\\\\Source\"});\nconsole.log(r);"}}"#,
        );

        let (command, _, _, _) = only_exec(&rollout);
        assert_eq!(command, "cargo test --quiet");
    }

    #[test]
    fn an_array_shell_command_is_joined_with_spaces() {
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:08:36.373Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_one","input":"await tools.shell_command({\"command\":[\"bash\",\"-lc\",\"ls\"]});"}}"#,
        );

        let (command, _, _, _) = only_exec(&rollout);
        assert_eq!(command, "bash -lc ls");
    }

    #[test]
    fn an_apply_patch_exec_call_makes_no_exec_event() {
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:09:40.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_patch","input":"const patch = \"*** Begin Patch\";\nawait tools.apply_patch({\"patch\":patch});"}}"#,
        );

        assert!(rollout.events.is_empty());
    }

    #[test]
    fn an_apply_patch_named_call_is_skipped_without_reading_its_input() {
        // `name` is the real discriminant now (Minor finding #1): a patch body
        // that happens to quote `tools.shell_command(` as literal text must
        // not turn into a phantom exec event.
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:09:40.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","call_id":"call_patch","input":"*** Begin Patch\n*** Add File: note.md\n+tools.shell_command({command: \"rm -rf /\"})\n*** End Patch"}}"#,
        );

        assert!(rollout.events.is_empty());
    }

    #[test]
    fn output_is_paired_by_call_id_with_its_exit_code_and_wall_time() {
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:08:36.373Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_one","input":"await tools.shell_command({\"command\":\"rg TODO\"});"}}
{"timestamp":"2026-07-25T15:08:38.010Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_one","output":[{"type":"input_text","text":"Script completed\nWall time 1.6 seconds\nOutput:\n"},{"type":"input_text","text":"Exit code: 3\nWall time: 0.5 seconds\nOutput:\nsrc/main.rs:12\n"}]}}"#,
        );

        let (command, output, exit_code, duration_ms) = only_exec(&rollout);
        assert_eq!(command, "rg TODO");
        assert!(output.contains("src/main.rs:12"));
        assert_eq!(exit_code, Some(3));
        assert_eq!(duration_ms, Some(1600));
    }

    #[test]
    fn an_exec_without_its_output_still_produces_an_event() {
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:08:36.373Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_one","input":"await tools.shell_command({\"command\":\"rg TODO\"});"}}"#,
        );

        let (_, output, exit_code, duration_ms) = only_exec(&rollout);
        assert!(output.is_empty());
        assert_eq!(exit_code, None);
        assert_eq!(duration_ms, None);
    }

    // --- C1: a real bare-key `tools.shell_command({command: …})` call -----

    #[test]
    fn a_bare_key_shell_command_becomes_an_exec_event() {
        // Real shape (session `019f996a`, one of the sessions the review found
        // at 0/107 quoted-key calls): the model wrote a JS object literal, not
        // JSON, so the key has no quotes around it at all.
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T22:16:03.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_one","input":"const r = await tools.shell_command({command: \"Get-Content -Raw .gitignore\", workdir: \"C:\\\\Source\\\\DevezCLI\"});\ntext(r);"}}"#,
        );

        let (command, _, _, _) = only_exec(&rollout);
        assert_eq!(command, "Get-Content -Raw .gitignore");
    }

    #[test]
    fn a_variable_shell_command_argument_is_skipped_not_misread() {
        // The other shape the review flagged as worse than a plain miss: the
        // argument is a bare identifier (`.map()` over a command list), so
        // there is no literal to read. The parser must not wander forward and
        // grab some later, unrelated call's object instead.
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T20:34:37.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_one","input":"const cmds=[[\"status\",{command:\"git status\"}]];\nconst out = await Promise.all(cmds.map(async ([k,a]) => { return [k, await tools.shell_command(a)]; }));\nconst decoy = {command:\"should not be picked up\"};"}}"#,
        );

        assert!(rollout.events.is_empty());
    }

    // --- I2: a `Promise.all` script running two `shell_command` calls -----

    #[test]
    fn a_promise_all_script_produces_one_exec_event_per_call() {
        // Real shape (session `019f996a`): two commands issued together, their
        // wrapped output framed as `---0---…` / `---1---…` in call order.
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T22:15:16.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_pair","input":"const r = await Promise.all([\n  tools.shell_command({command: \"rg TODO\", workdir: \"C:\\\\Source\\\\DevezCLI\"}),\n  tools.shell_command({command: \"git status --short\", workdir: \"C:\\\\Source\\\\DevezCLI\"})\n]);\nr.forEach((x,i)=>{text(`---${i}---`); text(x)});\n"}}
{"timestamp":"2026-07-25T22:15:20.000Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_pair","output":[{"type":"input_text","text":"Script completed\nWall time 4.1 seconds\nOutput:\n"},{"type":"input_text","text":"---0---Exit code: 0\nWall time: 0.7 seconds\nOutput:\nsrc/main.rs:12: TODO fix this\n---1---Exit code: 1\nWall time: 0.6 seconds\nOutput:\n?? untracked.txt\n"}]}}"#,
        );

        let events = exec_events(&rollout);
        assert_eq!(events.len(), 2);
        let groups = rollout
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                RolloutKind::Exec { group_id, .. } => Some(group_id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(groups, ["call_pair", "call_pair"]);
        assert_eq!(
            events[0],
            (
                "rg TODO",
                "src/main.rs:12: TODO fix this\n",
                Some(0),
                Some(4100)
            )
        );
        assert_eq!(
            events[1],
            (
                "git status --short",
                "?? untracked.txt\n",
                Some(1),
                Some(4100)
            )
        );
    }

    #[test]
    fn a_labelled_multi_command_script_splits_by_its_own_labels() {
        // Sessions also frame multi-command output with the script's own
        // labels (`status`, `log`, …) rather than numeric indices — the
        // framing rule is the same either way, so the split must not assume
        // digits.
        let rollout = parse(
            r#"{"timestamp":"2026-07-10T20:34:37.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_labelled","input":"const v0 = await tools.shell_command({command:\"git status --short\"});\ntext('---status---'); text(v0);\nconst v1 = await tools.shell_command({command:\"git log -1\"});\ntext('---log---'); text(v1);"}}
{"timestamp":"2026-07-10T20:34:39.000Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_labelled","output":[{"type":"input_text","text":"Script completed\nWall time 1.3 seconds\nOutput:\n"},{"type":"input_text","text":"---status---Exit code: 0\nWall time: 0.5 seconds\nOutput:\n## main\n---log---Exit code: 0\nWall time: 0.4 seconds\nOutput:\nabc123 latest commit\n"}]}}"#,
        );

        let events = exec_events(&rollout);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, "git status --short");
        assert!(events[0].1.contains("## main"));
        assert_eq!(events[1].0, "git log -1");
        assert!(events[1].1.contains("abc123 latest commit"));
    }

    // --- I3: a command whose own real output contains the literal `Output:` -

    #[test]
    fn a_body_containing_the_word_output_is_kept_in_full() {
        // Real shape (session `019f4bce`): `rg`/`Select-String` results can
        // legitimately contain the substring `Output:` as part of a matched
        // line. `rfind` used to cut everything before the last such
        // occurrence; the fix keeps the whole body by only skipping the
        // wrapper's own first `Output:` header.
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:08:36.373Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_one","input":"await tools.shell_command({\"command\":\"rg Output\"});"}}
{"timestamp":"2026-07-25T15:08:38.010Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_one","output":[{"type":"input_text","text":"Script completed\nWall time 0.4 seconds\nOutput:\n"},{"type":"input_text","text":"Exit code: 0\nWall time: 0.4 seconds\nOutput:\nsrc/logger.rs:9:fn print(Output: &str) {\nsrc/logger.rs:10:    println!(\"Output: {}\", Output);\n"}]}}"#,
        );

        let (_, output, exit_code, _) = only_exec(&rollout);
        assert_eq!(exit_code, Some(0));
        assert_eq!(
            output,
            "src/logger.rs:9:fn print(Output: &str) {\nsrc/logger.rs:10:    println!(\"Output: {}\", Output);\n"
        );
    }

    // --- I4: turn id attribution (parsing side only; state.rs uses it) -----

    #[test]
    fn exec_events_carry_the_turn_id_from_the_call_payload() {
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:08:36.373Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_one","input":"await tools.shell_command({\"command\":\"rg TODO\"});","internal_chat_message_metadata_passthrough":{"turn_id":"turn-abc"}}}"#,
        );

        assert_eq!(rollout.events[0].turn_id.as_deref(), Some("turn-abc"));
    }

    #[test]
    fn patch_applied_events_carry_their_own_turn_id() {
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:09:40.539Z","type":"event_msg","payload":{"type":"patch_apply_end","call_id":"exec-abc","turn_id":"turn-abc"}}"#,
        );

        assert_eq!(rollout.events[0].turn_id.as_deref(), Some("turn-abc"));
    }
}
