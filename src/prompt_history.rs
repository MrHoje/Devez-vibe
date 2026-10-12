//! Per-project prompt history kept across runs in `prompt-history.jsonl`, one
//! `{"cwd", "text"}` object per line. Several dvz processes may run at once
//! (one per DevezCode room), so entries are only ever appended.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};

use crate::state::{path_eq, vibe_settings_path};

/// Entries the up/down recall keeps, and the line counts that trigger and
/// bound compaction of the file.
pub const RECALL_LIMIT: usize = 100;
const COMPACT_ABOVE: usize = 2000;
const COMPACT_KEEP: usize = 1000;

/// `None` under test so no test touches the maintainer's real history.
pub fn default_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    vibe_settings_path().map(|path| path.with_file_name("prompt-history.jsonl"))
}

/// Best effort: history is a convenience, so a failed write is dropped.
pub fn append(path: &Path, cwd: &str, text: &str) {
    let _ = (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut line = json!({ "cwd": cwd, "text": text }).to_string();
        line.push('\n');
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(line.as_bytes())
    })();
}

/// Keeps only the newest lines once the file has grown past the cap. The kept
/// lines go to a temporary file that then replaces the original.
pub fn compact(path: &Path) {
    let _ = (|| -> std::io::Result<()> {
        // 잘린 멀티바이트 줄 하나가 파일 전체를 읽지 못하게 만들지 않도록 손실 변환한다.
        let content = String::from_utf8_lossy(&fs::read(path)?).into_owned();
        let lines = content.lines().collect::<Vec<_>>();
        if lines.len() <= COMPACT_ABOVE {
            return Ok(());
        }
        let mut kept = lines[lines.len() - COMPACT_KEEP..].join("\n");
        kept.push('\n');
        let temporary = path.with_extension(format!("jsonl.{}.tmp", std::process::id()));
        fs::write(&temporary, kept)?;
        fs::rename(&temporary, path).inspect_err(|_| {
            let _ = fs::remove_file(&temporary);
        })
    })();
}

/// The entries written for `cwd`, newest first, each text once.
pub fn entries(path: &Path, cwd: &str) -> Vec<String> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    let content = String::from_utf8_lossy(&bytes);
    let cwd = normalize(cwd);
    let mut found: Vec<String> = Vec::new();
    for line in content.lines().rev() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let (Some(entry_cwd), Some(text)) = (
            entry.get("cwd").and_then(Value::as_str),
            entry.get("text").and_then(Value::as_str),
        ) else {
            continue;
        };
        if path_eq(&normalize(entry_cwd), &cwd) && !found.iter().any(|seen| seen == text) {
            found.push(text.to_owned());
        }
    }
    found
}

/// The last `RECALL_LIMIT` entries for `cwd`, oldest first, ready for the
/// editor's recall list.
pub fn recent(path: &Path, cwd: &str) -> Vec<String> {
    let mut found = entries(path, cwd);
    found.truncate(RECALL_LIMIT);
    found.reverse();
    found
}

fn normalize(cwd: &str) -> String {
    cwd.replace('/', "\\").trim_end_matches('\\').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "devez-prompt-history-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir.join("prompt-history.jsonl")
    }

    #[test]
    fn round_trip_filters_by_project_and_keeps_the_newest_hundred() {
        let path = temp_file("round-trip");
        for index in 0..130 {
            append(&path, r"C:\work\app", &format!("prompt {index}"));
        }
        append(&path, r"C:\work\other", "elsewhere");
        append(&path, r"C:\work\app", "prompt 5");

        let recent = recent(&path, "c:/WORK/app/");
        assert_eq!(recent.len(), RECALL_LIMIT);
        assert_eq!(recent.last().map(String::as_str), Some("prompt 5"));
        assert_eq!(recent.iter().filter(|text| *text == "prompt 5").count(), 1);
        assert!(!recent.iter().any(|text| text == "elsewhere"));
        assert_eq!(entries(&path, r"C:\work\other"), ["elsewhere"]);
        assert!(entries(&path, r"C:\nowhere").is_empty());
    }

    #[test]
    fn multiline_text_survives_and_bad_lines_are_skipped() {
        let path = temp_file("multiline");
        append(&path, "/proj", "first\nsecond \"quoted\"");
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"not json\n")
            .unwrap();
        assert_eq!(entries(&path, "/proj"), ["first\nsecond \"quoted\""]);
    }

    #[test]
    fn a_line_cut_inside_a_multibyte_character_does_not_hide_the_rest() {
        let path = temp_file("bad-utf8");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{\"cwd\":\"/proj\",\"text\":\"\xED\x95\n").unwrap();
        append(&path, "/proj", "ok");
        assert_eq!(entries(&path, "/proj"), ["ok"]);

        for index in 0..COMPACT_ABOVE {
            append(&path, "/proj", &format!("line {index}"));
        }
        compact(&path);
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), COMPACT_KEEP);
    }

    #[test]
    fn compaction_keeps_the_newest_thousand_only_past_the_cap() {
        let path = temp_file("compact");
        for index in 0..COMPACT_ABOVE {
            append(&path, "/proj", &format!("line {index}"));
        }
        compact(&path);
        assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), COMPACT_ABOVE);

        append(&path, "/proj", "newest");
        compact(&path);
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), COMPACT_KEEP);
        assert_eq!(entries(&path, "/proj")[0], "newest");
        assert!(!content.contains("\"line 0\""));
    }
}
