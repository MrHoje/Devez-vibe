use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerSource {
    Path,
    VsCodeExtension,
}

impl ServerSource {
    fn label(self) -> &'static str {
        match self {
            Self::Path => "PATH",
            Self::VsCodeExtension => "VS Code extension",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledServer {
    pub command: String,
    pub path: PathBuf,
    pub source: ServerSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerStatus {
    pub language: &'static str,
    pub id: &'static str,
    pub workspace_detected: bool,
    pub installed: Option<InstalledServer>,
}

struct ServerDefinition {
    language: &'static str,
    id: &'static str,
    commands: &'static [&'static str],
    root_markers: &'static [&'static str],
}

const SERVER_DEFINITIONS: &[ServerDefinition] = &[
    ServerDefinition {
        language: "C#",
        id: "csharp",
        commands: &["omnisharp", "roslyn-language-server"],
        root_markers: &["*.sln", "*.csproj"],
    },
    ServerDefinition {
        language: "Rust",
        id: "rust-analyzer",
        commands: &["rust-analyzer"],
        root_markers: &["Cargo.toml", "rust-analyzer.toml"],
    },
    ServerDefinition {
        language: "TypeScript",
        id: "typescript-language-server",
        commands: &["typescript-language-server"],
        root_markers: &["tsconfig.json", "jsconfig.json", "package.json"],
    },
    ServerDefinition {
        language: "Python",
        id: "python",
        commands: &["basedpyright-langserver", "pyright-langserver", "pylsp"],
        root_markers: &[
            "pyproject.toml",
            "pyrightconfig.json",
            "requirements.txt",
            "setup.py",
        ],
    },
    ServerDefinition {
        language: "Go",
        id: "gopls",
        commands: &["gopls"],
        root_markers: &["go.mod", "go.work"],
    },
    ServerDefinition {
        language: "C/C++",
        id: "clangd",
        commands: &["clangd"],
        root_markers: &["compile_commands.json", "CMakeLists.txt"],
    },
];

pub fn detect(cwd: &Path) -> Vec<ServerStatus> {
    SERVER_DEFINITIONS
        .iter()
        .map(|definition| ServerStatus {
            language: definition.language,
            id: definition.id,
            workspace_detected: workspace_matches(cwd, definition.root_markers),
            installed: find_installed_server(definition),
        })
        .collect()
}

pub fn status_report(cwd: &Path) -> String {
    let statuses = detect(cwd);
    let mut lines = Vec::with_capacity(statuses.len() + 4);
    lines.push(format!("workspace: {}", cwd.display()));
    lines.push("discovery: lazy (no language-server process was started)".to_owned());
    lines.push(String::new());

    for status in statuses {
        let scope = if status.workspace_detected {
            "workspace"
        } else {
            "available"
        };
        match status.installed {
            Some(installed) => lines.push(format!(
                "{:<10} installed · {} · {} · {}",
                status.language,
                installed.command,
                installed.source.label(),
                installed.path.display()
            )),
            None if status.workspace_detected => lines.push(format!(
                "{:<10} not installed · project detected",
                status.language
            )),
            None => lines.push(format!("{:<10} not installed · {scope}", status.language)),
        }
    }

    lines.join("\n")
}

fn find_installed_server(definition: &ServerDefinition) -> Option<InstalledServer> {
    for command in definition.commands {
        if let Some(path) = find_on_path(command) {
            return Some(InstalledServer {
                command: (*command).to_owned(),
                path,
                source: ServerSource::Path,
            });
        }
    }

    if definition.id == "csharp"
        && let Some(path) = find_vscode_csharp_server()
    {
        let command = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("csharp-language-server")
            .to_owned();
        return Some(InstalledServer {
            command,
            path,
            source: ServerSource::VsCodeExtension,
        });
    }

    None
}

fn find_on_path(command: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    let extensions = executable_extensions();

    for directory in env::split_paths(&path) {
        for extension in &extensions {
            let candidate = directory.join(format!("{command}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn executable_extensions() -> Vec<String> {
    #[cfg(windows)]
    {
        let mut extensions = vec![String::new()];
        if let Some(path_ext) = env::var_os("PATHEXT") {
            for extension in path_ext.to_string_lossy().split(';') {
                let extension = extension.trim();
                if !extension.is_empty()
                    && !extensions
                        .iter()
                        .any(|existing| existing.eq_ignore_ascii_case(extension))
                {
                    extensions.push(extension.to_ascii_lowercase());
                    extensions.push(extension.to_ascii_uppercase());
                }
            }
        }
        for fallback in [".exe", ".cmd", ".bat", ".com"] {
            if !extensions
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(fallback))
            {
                extensions.push(fallback.to_owned());
            }
        }
        extensions
    }

    #[cfg(not(windows))]
    {
        vec![String::new()]
    }
}

fn workspace_matches(cwd: &Path, markers: &[&str]) -> bool {
    for directory in cwd.ancestors() {
        if markers
            .iter()
            .any(|marker| directory_matches_marker(directory, marker))
        {
            return true;
        }
        if directory.join(".git").exists() {
            break;
        }
    }
    false
}

fn directory_matches_marker(directory: &Path, marker: &str) -> bool {
    if let Some(suffix) = marker.strip_prefix('*') {
        let Ok(entries) = fs::read_dir(directory) else {
            return false;
        };
        return entries.flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with(&suffix.to_ascii_lowercase())
        });
    }

    directory.join(marker).exists()
}

#[cfg(windows)]
fn find_vscode_csharp_server() -> Option<PathBuf> {
    let profile = env::var_os("USERPROFILE").map(PathBuf::from)?;
    for root in [
        profile.join(".vscode").join("extensions"),
        profile.join(".vscode-insiders").join("extensions"),
        profile.join(".cursor").join("extensions"),
    ] {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if !name.starts_with("ms-dotnettools.csharp-")
                && !name.starts_with("ms-dotnettools.csdevkit-")
            {
                continue;
            }
            if let Some(path) = find_csharp_binary(&entry.path(), 0) {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(windows)]
fn find_csharp_binary(directory: &Path, depth: usize) -> Option<PathBuf> {
    if depth > 7 {
        return None;
    }
    let entries = fs::read_dir(directory).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_csharp_binary(&path, depth + 1) {
                return Some(found);
            }
            continue;
        }

        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if name.eq_ignore_ascii_case("omnisharp.exe")
            || name.eq_ignore_ascii_case("Microsoft.CodeAnalysis.LanguageServer.exe")
        {
            return Some(path);
        }
    }
    None
}

#[cfg(not(windows))]
fn find_vscode_csharp_server() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_the_primary_devez_languages() {
        let ids = SERVER_DEFINITIONS
            .iter()
            .map(|server| server.id)
            .collect::<Vec<_>>();
        assert!(ids.contains(&"csharp"));
        assert!(ids.contains(&"rust-analyzer"));
        assert!(ids.contains(&"typescript-language-server"));
        assert!(ids.contains(&"python"));
        assert!(ids.contains(&"gopls"));
    }

    #[test]
    fn wildcard_marker_matching_is_case_insensitive() {
        let unique = format!(
            "devez-lsp-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        let root = env::temp_dir().join(unique);
        fs::create_dir_all(&root).expect("temp dir");
        fs::write(root.join("Clinic.SLN"), "").expect("marker");

        assert!(directory_matches_marker(&root, "*.sln"));

        let _ = fs::remove_dir_all(root);
    }
}
