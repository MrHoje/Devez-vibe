//! 사이드 패널의 변경 섹션이 보여 줄 미커밋 diff를 모은다. 순정 Claude Code의
//! `/diff`와 같은 범위로, 스테이징 여부와 무관하게 마지막 커밋과 작업 트리의
//! 차이를 읽는다. 추적하지 않는 파일은 git이 diff로 내놓지 않으므로 빠진다.

use std::{path::Path, process::Command};

pub struct FileDiff {
    /// 전사의 파일 변경 블록과 같은 표기: `Add`, `Delete`, `Update`.
    pub verb: &'static str,
    pub path: String,
    pub patch: String,
}

/// git이 없거나 저장소가 아니면 빈 목록을 돌려준다. 패널의 한 섹션이 비는 것과
/// 오류를 대화에 밀어 넣는 것 중 전자가 덜 방해된다.
pub fn uncommitted(cwd: &Path) -> Vec<FileDiff> {
    let mut command = Command::new("git");
    // quotepath를 끄지 않으면 한글 경로가 8진 이스케이프로 나와 이름을 읽을 수
    // 없다. 그래도 공백이 든 경로는 따옴표에 싸여 오므로 아래에서 벗긴다.
    command.current_dir(cwd).args([
        "-c",
        "core.quotepath=false",
        "--no-pager",
        "diff",
        "HEAD",
    ]);
    crate::child_process::isolate_launcher(&mut command);
    let Ok(output) = command.output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse(&String::from_utf8_lossy(&output.stdout))
}

fn parse(diff: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            files.push(FileDiff {
                verb: "Update",
                path: header_path(line),
                patch: String::new(),
            });
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if line.starts_with("new file mode") {
            file.verb = "Add";
        } else if line.starts_with("deleted file mode") {
            file.verb = "Delete";
        }
        // 지워진 파일의 `+++`는 /dev/null이라 이름이 없다. 그때는 `---` 쪽이
        // 유일한 이름이고, 새 파일은 반대로 `+++`만 이름을 갖는다.
        if let Some(path) = side_path(line, "+++ ", 'b').or_else(|| {
            file.verb
                .eq("Delete")
                .then(|| side_path(line, "--- ", 'a'))
                .flatten()
        }) {
            file.path = path;
            continue;
        }
        if line.starts_with("@@") || !file.patch.is_empty() {
            file.patch.push_str(line);
            file.patch.push('\n');
        }
    }
    files.retain(|file| !file.patch.is_empty());
    files
}

/// `+++ b/경로` 한 줄에서 이름만. 공백이 든 경로는 `+++ "b/경로"`처럼 따옴표에
/// 싸여 오므로 그것부터 벗긴다. `/dev/null` 쪽은 이름이 아니라 None이다.
fn side_path(line: &str, marker: &str, side: char) -> Option<String> {
    let rest = line.strip_prefix(marker)?.trim_matches('"');
    rest.strip_prefix(side)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(str::to_owned)
}

/// `diff --git a/경로 b/경로`의 뒤쪽 이름. 공백이 든 경로는 앞뒤 이름의 경계를
/// 확정할 수 없어 `+++`/`---` 줄이 곧 덮어쓴다.
fn header_path(line: &str) -> String {
    line.rsplit_once(" b/")
        .map(|(_, path)| path.trim_end_matches('"').to_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_splits_files_and_keeps_only_the_hunks() {
        let diff = concat!(
            "diff --git a/src/a.rs b/src/a.rs\n",
            "index 1111111..2222222 100644\n",
            "--- a/src/a.rs\n",
            "+++ b/src/a.rs\n",
            "@@ -1,2 +1,2 @@\n",
            " keep\n",
            "-old\n",
            "+new\n",
            "diff --git a/notes.md b/notes.md\n",
            "new file mode 100644\n",
            "--- /dev/null\n",
            "+++ b/notes.md\n",
            "@@ -0,0 +1 @@\n",
            "+hello\n",
        );
        let files = parse(diff);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].verb, "Update");
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[0].patch, "@@ -1,2 +1,2 @@\n keep\n-old\n+new\n");
        assert_eq!(files[1].verb, "Add");
        assert_eq!(files[1].path, "notes.md");
        assert_eq!(files[1].patch, "@@ -0,0 +1 @@\n+hello\n");
    }

    #[test]
    fn parse_names_a_deleted_file_from_its_old_side() {
        let diff = concat!(
            "diff --git a/gone.txt b/gone.txt\n",
            "deleted file mode 100644\n",
            "--- a/gone.txt\n",
            "+++ /dev/null\n",
            "@@ -1 +0,0 @@\n",
            "-bye\n",
        );
        let files = parse(diff);
        assert_eq!(files[0].verb, "Delete");
        assert_eq!(files[0].path, "gone.txt");
        assert_eq!(files[0].patch, "@@ -1 +0,0 @@\n-bye\n");
    }

    /// 한글이나 공백이 든 경로는 git이 따옴표에 싸서 내놓는다. 그대로 두면 이름
    /// 자리가 비어 목록에서 어느 파일인지 알 수 없다.
    #[test]
    fn parse_unwraps_a_quoted_path() {
        let diff = concat!(
            "diff --git \"a/.knowledge/배포 노트.md\" \"b/.knowledge/배포 노트.md\"\n",
            "--- \"a/.knowledge/배포 노트.md\"\n",
            "+++ \"b/.knowledge/배포 노트.md\"\n",
            "@@ -1 +1,2 @@\n",
            "+한 줄\n",
        );
        let files = parse(diff);
        assert_eq!(files[0].path, ".knowledge/배포 노트.md");
    }

    /// 내용으로 들어 있는 `--- ` 줄이 헤더로 오해받아 패치에서 사라지면 안 된다.
    #[test]
    fn parse_keeps_dashed_content_rows_inside_a_hunk() {
        let diff = concat!(
            "diff --git a/notes.md b/notes.md\n",
            "--- a/notes.md\n",
            "+++ b/notes.md\n",
            "@@ -1,2 +1,1 @@\n",
            " title\n",
            "---\n",
        );
        let files = parse(diff);
        assert_eq!(files[0].patch, "@@ -1,2 +1,1 @@\n title\n---\n");
    }
}
