use std::{
    path::{Component, Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command.current_dir(cwd).args(args);
    crate::child_process::isolate_launcher(&mut command);
    let output = command.output().context("Git을 실행할 수 없습니다.")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_creation_preserves_source_and_checks_names() {
        let temp = std::env::temp_dir().join(format!(
            "dvz-worktree-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = temp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        std::fs::write(repo.join("tracked.txt"), "committed").unwrap();
        git(&repo, &["add", "tracked.txt"]).unwrap();
        git(
            &repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        )
        .unwrap();
        std::fs::write(repo.join("uncommitted.txt"), "keep").unwrap();
        let first = prepare(&repo, None).unwrap();
        assert_eq!(first.file_name().unwrap(), "main-worktree-1");
        assert!(first.join(".git").is_file());
        assert!(!first.join("tracked.txt").exists());
        checkout(&first).unwrap();
        assert_eq!(std::fs::read_to_string(first.join("tracked.txt")).unwrap(), "committed");
        assert!(git(&first, &["status", "--porcelain"]).unwrap().is_empty());
        assert!(!first.join("uncommitted.txt").exists());
        assert_eq!(
            git(&first, &["branch", "--show-current"]).unwrap(),
            "main-worktree-1"
        );
        git(&repo, &["branch", "main-worktree-2"]).unwrap();
        let third = create(&repo, None).unwrap();
        assert_eq!(third.file_name().unwrap(), "main-worktree-3");
        assert_eq!(
            git(&third, &["rev-parse", "HEAD"]).unwrap(),
            git(&repo, &["rev-parse", "HEAD"]).unwrap()
        );
        std::fs::create_dir_all(temp.join("repo.worktrees/main-worktree-4")).unwrap();
        let fifth = create(&repo, None).unwrap();
        assert_eq!(fifth.file_name().unwrap(), "main-worktree-5");
        let named = create(&repo, Some("login")).unwrap();
        assert_eq!(named, temp.join("repo.worktrees/main-worktree-login"));
        assert_eq!(
            git(&named, &["branch", "--show-current"]).unwrap(),
            "main-worktree-login"
        );
        assert!(create(&repo, Some("login")).is_err());
        let nested = create(&first, Some("feature/login")).unwrap();
        assert_eq!(
            nested,
            temp.join("repo.worktrees/main-worktree-1-worktree-feature/login")
        );
        let conflict = prepare(&repo, Some("conflict")).unwrap();
        std::fs::write(conflict.join("tracked.txt"), "external edit").unwrap();
        assert!(checkout(&conflict).is_err());
        assert_eq!(std::fs::read_to_string(conflict.join("tracked.txt")).unwrap(), "external edit");
        std::fs::remove_file(conflict.join("tracked.txt")).unwrap();
        checkout(&conflict).unwrap();
        for name in [
            "../escape",
            "/absolute",
            "C:/escape",
            "--force",
            "foo\\bar",
            "foo/../escape",
            "bad name",
        ] {
            assert!(create(&repo, Some(name)).is_err(), "{name}");
        }
        git(&repo, &["checkout", "--detach"]).unwrap();
        assert!(create(&repo, None).is_err());
        assert!(create(&repo, Some("detached-work")).is_err());
        assert_eq!(
            std::fs::read_to_string(repo.join("uncommitted.txt")).unwrap(),
            "keep"
        );
        assert!(create(&temp, None).is_err());
        for path in [&first, &third, &fifth, &named, &nested, &conflict] {
            git(&repo, &["worktree", "remove", path.to_str().unwrap()]).unwrap();
        }
        std::fs::remove_dir_all(temp).unwrap();
    }
}

#[cfg(test)]
fn create(cwd: &Path, requested: Option<&str>) -> Result<PathBuf> {
    let path = prepare(cwd, requested)?;
    checkout(&path)?;
    Ok(path)
}

pub fn checkout(path: &Path) -> Result<()> {
    git(path, &["read-tree", "HEAD"])?;
    // Do not overwrite a file created externally while the new screen is open.
    git(path, &["checkout-index", "--all"])?;
    Ok(())
}

pub fn prepare(cwd: &Path, requested: Option<&str>) -> Result<PathBuf> {
    let common = git(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let root = Path::new(&common)
        .parent()
        .context("저장소 경로가 없습니다.")?;
    let directory = root.with_file_name(format!(
        "{}.worktrees",
        root.file_name()
            .context("저장소 이름이 없습니다.")?
            .to_string_lossy()
    ));
    let branches = git(
        cwd,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
    )?;
    let branch = git(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .context("현재 브랜치가 없습니다. 브랜치를 체크아웃한 뒤 /worktree를 실행하세요.")?;
    let name = match requested {
        Some(name) => {
            git(cwd, &["check-ref-format", "--branch", name])?;
            format!("{branch}-worktree-{name}")
        }
        None => (1_u64..)
            .map(|index| format!("{branch}-worktree-{index}"))
            .find(|name| {
                !branches.lines().any(|branch| branch == name)
                    && directory.join(name).symlink_metadata().is_err()
            })
            .context("사용 가능한 작업 트리 이름이 없습니다.")?,
    };
    // Names become both branch names and relative paths; reject traversal and Windows path syntax.
    if name.is_empty()
        || name.starts_with('-')
        || name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || name
            .chars()
            .any(|ch| ch.is_control() || "\\:<>\"|?*".contains(ch))
        || Path::new(&name)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        bail!("작업 트리 이름에는 상대 브랜치 이름을 사용하세요.");
    }
    git(cwd, &["check-ref-format", "--branch", &name])?;
    let path = directory.join(&name);
    if path.symlink_metadata().is_ok() || branches.lines().any(|branch| branch == name) {
        bail!("이미 존재하는 작업 트리 또는 브랜치 이름입니다: {name}");
    }
    // Never follow an existing symlink/junction in the destination's ancestors.
    for ancestor in path.ancestors().skip(1) {
        if let Ok(metadata) = ancestor.symlink_metadata() {
            if metadata.file_type().is_symlink() {
                bail!(
                    "작업 트리 경로에 연결된 폴더가 있습니다: {}",
                    ancestor.display()
                );
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    bail!(
                        "작업 트리 경로에 연결된 폴더가 있습니다: {}",
                        ancestor.display()
                    );
                }
            }
        }
    }
    git(
        cwd,
        &[
            "worktree",
            "add",
            "--no-checkout",
            "-b",
            &name,
            path.to_str()
                .context("작업 트리 경로를 읽을 수 없습니다.")?,
            "HEAD",
        ],
    )?;
    Ok(path)
}
