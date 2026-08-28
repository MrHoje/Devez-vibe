use std::{ops::Range, path::Path};

use ignore::WalkBuilder;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    Plugin,
    Skill,
    App,
    File,
    Directory,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompletionSource {
    #[default]
    User,
    Plugin,
    Provider,
}

impl CompletionSource {
    pub fn previous(self) -> Self {
        match self {
            Self::User => Self::Provider,
            Self::Plugin => Self::User,
            Self::Provider => Self::Plugin,
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::User => Self::Plugin,
            Self::Plugin => Self::Provider,
            Self::Provider => Self::User,
        }
    }
}

impl CompletionKind {
    pub fn is_filesystem(self) -> bool {
        matches!(self, Self::File | Self::Directory)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Plugin => "Plugin",
            Self::Skill => "Skill",
            Self::App => "App",
            Self::File => "File",
            Self::Directory => "Dir",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompletionMode {
    #[default]
    All,
    Filesystem,
    Tools,
}

impl CompletionMode {
    pub fn previous(self) -> Self {
        match self {
            Self::All => Self::Tools,
            Self::Filesystem => Self::All,
            Self::Tools => Self::Filesystem,
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Filesystem,
            Self::Filesystem => Self::Tools,
            Self::Tools => Self::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All Results",
            Self::Filesystem => "Filesystem Only",
            Self::Tools => "Plugins & Skills",
        }
    }

    fn accepts(self, kind: CompletionKind) -> bool {
        match self {
            Self::All => true,
            Self::Filesystem => kind.is_filesystem(),
            Self::Tools => !kind.is_filesystem() && kind != CompletionKind::App,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionCandidate {
    pub kind: CompletionKind,
    pub label: String,
    pub description: String,
    pub insert_text: String,
    pub binding: Option<CompletionBinding>,
    pub source: CompletionSource,
    search_label: String,
    search_insert: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionBinding {
    pub name: String,
    pub path: String,
}

impl CompletionCandidate {
    pub fn new(
        kind: CompletionKind,
        label: impl Into<String>,
        description: impl Into<String>,
        insert_text: impl Into<String>,
    ) -> Self {
        let label = label.into();
        let insert_text = insert_text.into();
        Self {
            kind,
            search_label: label.to_ascii_lowercase(),
            search_insert: insert_text.to_ascii_lowercase(),
            label,
            description: description.into(),
            insert_text,
            binding: None,
            source: CompletionSource::Provider,
        }
    }

    pub fn with_binding(mut self, name: impl Into<String>, path: impl Into<String>) -> Self {
        self.binding = Some(CompletionBinding {
            name: name.into(),
            path: path.into(),
        });
        self
    }

    pub fn with_source(mut self, source: CompletionSource) -> Self {
        self.source = source;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionTarget {
    pub sigil: char,
    pub range: Range<usize>,
    pub query: String,
}

pub fn completion_target(text: &str, cursor: usize) -> Option<CompletionTarget> {
    let chars = text.chars().collect::<Vec<_>>();
    let cursor = cursor.min(chars.len());
    if cursor > 0 && chars.get(cursor - 1).is_some_and(|ch| ch.is_whitespace()) {
        return None;
    }

    let start = (0..cursor)
        .rev()
        .find(|index| chars[*index].is_whitespace())
        .map_or(0, |index| index + 1);
    let sigil = *chars.get(start)?;
    if !matches!(sigil, '$' | '@') {
        return None;
    }
    if start > 0 && !chars[start - 1].is_whitespace() {
        return None;
    }

    let end = (cursor..chars.len())
        .find(|index| chars[*index].is_whitespace())
        .unwrap_or(chars.len());
    let query = chars[start + 1..end].iter().collect::<String>();
    if sigil == '$' {
        let valid_name = query
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':' | '.'));
        let definite_parameter = !query.is_empty()
            && (query.chars().all(|ch| ch.is_ascii_digit()) || matches!(query.as_str(), "-" | "_"));
        if !valid_name || definite_parameter || is_common_shell_variable(&query) {
            return None;
        }
    }

    Some(CompletionTarget {
        sigil,
        range: start..end,
        query,
    })
}

fn is_common_shell_variable(query: &str) -> bool {
    const VARIABLES: [&str; 22] = [
        "ALLUSERSPROFILE",
        "APPDATA",
        "COMSPEC",
        "HOME",
        "HOMEDRIVE",
        "HOMEPATH",
        "LANG",
        "LOCALAPPDATA",
        "PATH",
        "PATHEXT",
        "PROGRAMDATA",
        "PROGRAMFILES",
        "PROMPT",
        "PWD",
        "SHELL",
        "TEMP",
        "TERM",
        "TMP",
        "TMPDIR",
        "USER",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
    ];
    VARIABLES.contains(&query)
}

pub fn completion_text(kind: CompletionKind, name: &str, display_name: Option<&str>) -> String {
    match kind {
        CompletionKind::Skill | CompletionKind::App => format!("${name}"),
        CompletionKind::Plugin => format!(
            "@{}",
            plugin_mention_name(name, display_name.unwrap_or(name))
        ),
        CompletionKind::File | CompletionKind::Directory => {
            if name.chars().any(char::is_whitespace) && !name.contains('"') {
                format!("\"{name}\"")
            } else {
                name.to_owned()
            }
        }
    }
}

fn plugin_mention_name(name: &str, display_name: &str) -> String {
    let name_parts = name
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let display_parts = display_name
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if name_parts.len() == display_parts.len()
        && name_parts
            .iter()
            .zip(&display_parts)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    {
        let separators = name
            .chars()
            .filter(|ch| matches!(ch, '-' | '_'))
            .collect::<Vec<_>>();
        let mut result = String::new();
        for (index, part) in display_parts.iter().enumerate() {
            result.push_str(part);
            if let Some(separator) = separators.get(index) {
                result.push(*separator);
            }
        }
        result
    } else {
        let mut uppercase = true;
        name.chars()
            .map(|ch| {
                if matches!(ch, '-' | '_') {
                    uppercase = true;
                    ch
                } else if uppercase {
                    uppercase = false;
                    ch.to_ascii_uppercase()
                } else {
                    ch
                }
            })
            .collect()
    }
}

pub fn filter_candidates<'a>(
    candidates: &'a [CompletionCandidate],
    sigil: char,
    query: &str,
    mode: CompletionMode,
) -> Vec<&'a CompletionCandidate> {
    let query = query.trim_start_matches(['$', '@']).to_ascii_lowercase();
    let mut matches = candidates
        .iter()
        .filter(|candidate| candidate_allowed(candidate.kind, sigil, mode))
        .filter(|candidate| !(sigil == '@' && query.is_empty() && candidate.kind.is_filesystem()))
        .filter_map(|candidate| {
            fuzzy_score(&candidate.search_label, &query)
                .or_else(|| fuzzy_score(&candidate.search_insert, &query))
                .map(|score| (candidate, score))
        })
        .collect::<Vec<_>>();
    const MAX_MATCHES: usize = 200;
    if matches.len() > MAX_MATCHES {
        matches.select_nth_unstable_by(MAX_MATCHES, compare_matches);
        matches.truncate(MAX_MATCHES);
    }
    matches.sort_unstable_by(compare_matches);
    matches
        .into_iter()
        .map(|(candidate, _)| candidate)
        .collect()
}

fn compare_matches(
    (left, left_score): &(&CompletionCandidate, (u8, usize)),
    (right, right_score): &(&CompletionCandidate, (u8, usize)),
) -> std::cmp::Ordering {
    category_rank(left.kind)
        .cmp(&category_rank(right.kind))
        .then_with(|| left_score.cmp(right_score))
        .then_with(|| left.search_label.cmp(&right.search_label))
}

fn candidate_allowed(kind: CompletionKind, sigil: char, mode: CompletionMode) -> bool {
    match sigil {
        '$' => matches!(
            kind,
            CompletionKind::Plugin | CompletionKind::Skill | CompletionKind::App
        ),
        '@' => {
            kind != CompletionKind::App
                && matches!(
                    kind,
                    CompletionKind::Plugin
                        | CompletionKind::Skill
                        | CompletionKind::File
                        | CompletionKind::Directory
                )
                && mode.accepts(kind)
        }
        _ => false,
    }
}

fn category_rank(kind: CompletionKind) -> u8 {
    match kind {
        CompletionKind::Plugin => 0,
        CompletionKind::Skill => 1,
        CompletionKind::App => 2,
        CompletionKind::File | CompletionKind::Directory => 3,
    }
}

fn fuzzy_score(value: &str, query: &str) -> Option<(u8, usize)> {
    if query.is_empty() {
        return Some((0, 0));
    }
    if value.starts_with(query) {
        return Some((0, value.len().saturating_sub(query.len())));
    }
    if let Some(index) = value.find(query) {
        return Some((1, index));
    }

    let mut position = 0;
    let mut gaps = 0;
    for query_char in query.chars() {
        let offset = value[position..].find(query_char)?;
        gaps += offset;
        position += offset + query_char.len_utf8();
    }
    Some((2, gaps))
}

pub fn collect_workspace_entries(root: &Path) -> Vec<CompletionCandidate> {
    let mut walker = WalkBuilder::new(root);
    walker
        .require_git(false)
        .hidden(false)
        .filter_entry(|entry| !matches!(entry.file_name().to_str(), Some(".git" | ".hg" | ".svn")));
    let mut entries = walker
        .build()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path == root {
                return None;
            }
            let relative = path.strip_prefix(root).ok()?;
            let label = relative.to_string_lossy().replace('\\', "/");
            let kind = if entry.file_type()?.is_dir() {
                CompletionKind::Directory
            } else if entry.file_type()?.is_file() {
                CompletionKind::File
            } else {
                return None;
            };
            Some(CompletionCandidate::new(
                kind,
                &label,
                "",
                completion_text(kind, &label, None),
            ))
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        category_rank(left.kind)
            .cmp(&category_rank(right.kind))
            .then_with(|| left.label.cmp(&right.label))
    });
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn completion_target_tracks_the_sigiled_token_at_the_cursor() {
        let target = completion_target("before @src/main after", 16).expect("active target");

        assert_eq!(target.sigil, '@');
        assert_eq!(target.range, 7..16);
        assert_eq!(target.query, "src/main");
    }

    #[test]
    fn completion_target_rejects_email_and_shell_variable_syntax() {
        assert_eq!(completion_target("dev@example.com", 15), None);
        assert_eq!(completion_target("$HOME", 5), None);
        assert_eq!(completion_target("$USER", 5), None);
        assert_eq!(completion_target("$LANG", 5), None);
        assert_eq!(completion_target("$TERM", 5), None);
        assert_eq!(completion_target("$XDG_CONFIG_HOME", 16), None);
        assert_eq!(completion_target("$1", 2), None);
        assert_eq!(completion_target("$?", 2), None);
        assert_eq!(completion_target("cost $$", 7), None);
        assert!(completion_target("$review", 7).is_some());
    }

    #[test]
    fn completion_text_uses_codex_sigils_and_quotes_spaced_paths() {
        assert_eq!(
            completion_text(CompletionKind::Skill, "review", None),
            "$review"
        );
        assert_eq!(
            completion_text(CompletionKind::Plugin, "browser-use", Some("Browser Use")),
            "@Browser-Use"
        );
        assert_eq!(
            completion_text(CompletionKind::Plugin, "github", Some("GitHub Tools")),
            "@Github"
        );
        assert_eq!(
            completion_text(CompletionKind::App, "google-calendar", None),
            "$google-calendar"
        );
        assert_eq!(
            completion_text(CompletionKind::File, "docs/user guide.md", None),
            "\"docs/user guide.md\""
        );
    }

    #[test]
    fn modes_keep_the_same_catalog_membership_as_codex() {
        let candidates = vec![
            CompletionCandidate::new(CompletionKind::Plugin, "Browser", "", "@Browser"),
            CompletionCandidate::new(CompletionKind::Skill, "brainstorming", "", "$brainstorming"),
            CompletionCandidate::new(CompletionKind::App, "Calendar", "", "$calendar"),
            CompletionCandidate::new(CompletionKind::File, "src/main.rs", "", "src/main.rs"),
            CompletionCandidate::new(CompletionKind::Directory, "src", "", "src"),
        ];

        let dollar = filter_candidates(&candidates, '$', "", CompletionMode::All);
        assert_eq!(
            dollar
                .iter()
                .map(|candidate| candidate.kind)
                .collect::<Vec<_>>(),
            [
                CompletionKind::Plugin,
                CompletionKind::Skill,
                CompletionKind::App
            ]
        );

        let at = filter_candidates(&candidates, '@', "s", CompletionMode::All);
        assert_eq!(
            at.iter()
                .map(|candidate| candidate.kind)
                .collect::<Vec<_>>(),
            [
                CompletionKind::Plugin,
                CompletionKind::Skill,
                CompletionKind::Directory,
                CompletionKind::File
            ]
        );
        assert!(
            filter_candidates(&candidates, '@', "s", CompletionMode::Filesystem)
                .iter()
                .all(|candidate| candidate.kind.is_filesystem())
        );
        assert!(
            filter_candidates(&candidates, '@', "s", CompletionMode::Tools)
                .iter()
                .all(|candidate| !candidate.kind.is_filesystem())
        );
    }

    #[test]
    fn fuzzy_filter_prefers_prefixes_and_category_order() {
        let candidates = vec![
            CompletionCandidate::new(CompletionKind::Skill, "search-web", "", "$search-web"),
            CompletionCandidate::new(CompletionKind::Plugin, "Web Search", "", "@Web-Search"),
            CompletionCandidate::new(
                CompletionKind::File,
                "src/web_search.rs",
                "",
                "src/web_search.rs",
            ),
        ];

        let matches = filter_candidates(&candidates, '@', "web", CompletionMode::All);
        assert_eq!(
            matches
                .iter()
                .map(|candidate| candidate.label.as_str())
                .collect::<Vec<_>>(),
            ["Web Search", "search-web", "src/web_search.rs"]
        );
    }

    #[test]
    fn fuzzy_filter_bounds_large_result_sets_without_losing_the_best_match() {
        let mut candidates = (0..500)
            .map(|index| {
                CompletionCandidate::new(
                    CompletionKind::File,
                    format!("src/generated-{index:03}-review.rs"),
                    "",
                    format!("src/generated-{index:03}-review.rs"),
                )
            })
            .collect::<Vec<_>>();
        candidates.push(CompletionCandidate::new(
            CompletionKind::File,
            "review.rs",
            "",
            "review.rs",
        ));

        let matches = filter_candidates(&candidates, '@', "review", CompletionMode::All);

        assert_eq!(matches.len(), 200);
        assert_eq!(matches[0].label, "review.rs");
    }

    #[test]
    fn workspace_entries_respect_ignore_files_and_include_directories() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("devez-completion-{}-{unique}", std::process::id()));
        fs::create_dir_all(root.join("src")).expect("create fixture");
        fs::create_dir_all(root.join("target")).expect("create ignored fixture");
        fs::create_dir_all(root.join(".github").join("workflows")).expect("create hidden fixture");
        fs::create_dir_all(root.join(".git")).expect("create vcs fixture");
        fs::write(root.join("src").join("main.rs"), "fn main() {}").expect("write source");
        fs::write(root.join("target").join("hidden.rs"), "").expect("write ignored source");
        fs::write(root.join(".github").join("workflows").join("ci.yml"), "")
            .expect("write hidden source");
        fs::write(root.join(".git").join("config"), "").expect("write vcs source");
        fs::write(root.join(".gitignore"), "target/\n").expect("write ignore rules");

        let entries = collect_workspace_entries(&root);
        let labels = entries
            .iter()
            .map(|entry| (entry.kind, entry.label.as_str()))
            .collect::<Vec<_>>();

        assert!(labels.contains(&(CompletionKind::Directory, "src")));
        assert!(labels.contains(&(CompletionKind::File, "src/main.rs")));
        assert!(labels.contains(&(CompletionKind::File, ".github/workflows/ci.yml")));
        assert!(!labels.iter().any(|(_, label)| label.contains("target")));
        assert!(!labels.iter().any(|(_, label)| label.starts_with(".git/")));

        fs::remove_dir_all(root).expect("remove fixture");
    }
}
