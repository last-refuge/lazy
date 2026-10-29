//! Every `tracing` call in the server's source is reviewed: no `Debug` formatting (the `Debug` of
//! a request struct, a document, a name is content), no field that is not on the list below, and an
//! `error` field only as a [`Describe`](lazy_server::logging::Describe) (kind, never text) outside
//! startup. A new log call with a new field fails here until someone has decided that the field
//! can never hold a credential or content, and adds it.

use std::path::{Path, PathBuf};

/// Fields that may appear in a log call, and why they are safe.
const ALLOWED_FIELDS: &[&str] = &[
    // identifiers (random ids; the person is known by id only)
    "workspace_id",
    "general_id",
    "request_id",
    "user_id",
    // counters, sizes, durations
    "seq",
    "deleted",
    "missed",
    "expired",
    "removed",
    "swept",
    "orphans",
    "stale_uploads",
    "status",
    "latency_ms",
    "bytes_in",
    "bytes_out",
    "slow",
    "migration_version",
    // fixed vocabulary
    "method",
    "route",
    "error_code",
    "place",
    "thread",
    // configuration, never secret: addresses without credentials ([`describe_url`] and
    // [`describe_database_url`]), flags and limits
    "version",
    "protocol_version",
    "listen",
    "addr",
    "database",
    "dir",
    "migrate",
    "seed",
    "open_registration",
    "open_workspace_creation",
    "event_retention_days",
    "max_db_connections",
    "max_attachment_bytes",
    "public_url",
    // what kind of failure: only ever `.describe()`d
    "error",
];

/// Files that report startup and shutdown, where a failure is shown whole (a configuration or
/// migration error holds no request data).
const STARTUP_FILES: &[&str] = &["main.rs", "app.rs"];

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The text between the parentheses that open at `open` (a `(`), and the index after the `)`.
fn balanced(text: &str, open: usize) -> (&str, usize) {
    let bytes = text.as_bytes();
    let (mut depth, mut i, mut in_string) = (0usize, open, false);
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_string => i += 1,
            b'"' => in_string = !in_string,
            b'(' | b'[' | b'{' if !in_string => depth += 1,
            b')' | b']' | b'}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return (&text[open + 1..i], i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    panic!("unbalanced parentheses");
}

/// The arguments of a macro call, split at the commas that are not nested or in a string.
fn arguments(call: &str) -> Vec<String> {
    let bytes = call.as_bytes();
    let (mut depth, mut in_string, mut start) = (0usize, false, 0usize);
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_string => i += 1,
            b'"' => in_string = !in_string,
            b'(' | b'[' | b'{' if !in_string => depth += 1,
            b')' | b']' | b'}' if !in_string => depth -= 1,
            b',' if !in_string && depth == 0 => {
                out.push(call[start..i].trim().to_owned());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    let last = call[start..].trim();
    if !last.is_empty() {
        out.push(last.to_owned());
    }
    out
}

#[test]
fn every_tracing_call_is_reviewed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&root, &mut files);
    let mut calls = 0;
    let mut problems = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        for forbidden in ["println!", "eprintln!", "dbg!"] {
            if text.contains(forbidden) {
                problems.push(format!("{name}: {forbidden} bypasses the log filter"));
            }
        }
        let mut from = 0;
        while let Some(found) = text[from..].find("tracing::") {
            let start = from + found + "tracing::".len();
            from = start;
            let rest = &text[start..];
            let end = rest.find('(').unwrap_or(0);
            let macro_name = &rest[..end];
            let is_log_macro = macro_name.ends_with('!')
                && [
                    "error",
                    "warn",
                    "info",
                    "debug",
                    "trace",
                    "info_span",
                    "debug_span",
                    "$level",
                ]
                .iter()
                .any(|level| macro_name == format!("{level}!"));
            if !is_log_macro {
                continue;
            }
            calls += 1;
            let (call, after) = balanced(&text, start + end);
            from = after;
            for argument in arguments(call) {
                if argument.starts_with('"') {
                    // The message: fixed text, or `{name}` placeholders in a `format_args!` style.
                    if argument.contains(":?}") {
                        problems.push(format!("{name}: a message formats with Debug: {argument}"));
                    }
                    continue;
                }
                let (field, value) = match argument.split_once('=') {
                    Some((field, value)) => (field.trim(), value.trim()),
                    None => (argument.trim_start_matches(['%', '?']), argument.as_str()),
                };
                if value.starts_with('?') || argument.starts_with('?') {
                    problems.push(format!("{name}: `{argument}` formats with Debug"));
                }
                if !ALLOWED_FIELDS.contains(&field) {
                    problems.push(format!(
                        "{name}: the field `{field}` has not been reviewed ({argument})"
                    ));
                }
                if field == "error"
                    && !value.contains(".describe()")
                    && !STARTUP_FILES.contains(&name.as_str())
                {
                    problems.push(format!("{name}: `{argument}` logs an error's text"));
                }
            }
        }
    }
    assert!(
        calls >= 30,
        "the audit found only {calls} tracing calls; has the parser broken?"
    );
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}
