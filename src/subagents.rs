//! Fixed-model subagents the specialized roles dispatch.
//!
//! Planner and Goal Runner hand work to three lanes — an implementer, a
//! reviewer, and an adversarial tester. Each lane has a model and a tool scope
//! fixed here, so the role prompt only has to name the lane: a dispatch that
//! forgets to pick a model no longer inherits the session's most expensive one.
//!
//! The same definitions reach every provider that has subagents. Claude gets
//! them as SDK agent definitions through the bridge on every session; Codex
//! reads custom agents from `~/.codex/agents/*.toml`. Known shipped instructions
//! are upgraded without changing models, other settings, or customized prompts.

use std::{io::Write, path::Path};

use serde_json::{Value, json};

/// One dispatchable lane.
pub struct Subagent {
    /// The agent type the role prompt names, e.g. `devez-reviewer`.
    pub name: &'static str,
    /// Shown to the dispatching model when it chooses an agent type.
    pub description: &'static str,
    /// The lane's own system prompt.
    pub prompt: &'static str,
    /// Claude model alias.
    pub claude_model: &'static str,
    /// Claude tool allow-list; the implementer edits, the other two read.
    pub claude_tools: &'static [&'static str],
    /// Codex model id written into the agent file.
    pub codex_model: &'static str,
    /// Codex reasoning effort written into the agent file.
    pub codex_effort: &'static str,
}

const IMPLEMENTER_PROMPT: &str = include_str!("../prompts/agents/subagents/implementer.md");
const REVIEWER_PROMPT: &str = include_str!("../prompts/agents/subagents/reviewer.md");
const QA_PROMPT: &str = include_str!("../prompts/agents/subagents/qa.md");

// Exact shipped bodies, used only to recognize defaults safe to upgrade. Every
// body ever shipped stays here, so a file written by any earlier release is
// still recognized.
const LEGACY_IMPLEMENTER: &[&str] = &[
    include_str!("../prompts/agents/subagents/legacy/implementer.md"),
    include_str!("../prompts/agents/subagents/legacy/implementer-v2.md"),
];
const LEGACY_REVIEWER: &[&str] = &[
    include_str!("../prompts/agents/subagents/legacy/reviewer.md"),
    include_str!("../prompts/agents/subagents/legacy/reviewer-v2.md"),
    include_str!("../prompts/agents/subagents/legacy/reviewer-v3.md"),
];
const LEGACY_QA: &[&str] = &[
    include_str!("../prompts/agents/subagents/legacy/qa.md"),
    include_str!("../prompts/agents/subagents/legacy/qa-v2.md"),
    include_str!("../prompts/agents/subagents/legacy/qa-v3.md"),
];

const EDITING_TOOLS: &[&str] = &["Read", "Edit", "Write", "Glob", "Grep", "Bash"];
const READ_ONLY_TOOLS: &[&str] = &["Read", "Glob", "Grep", "Bash"];

pub const SUBAGENTS: [Subagent; 4] = [
    Subagent {
        name: "devez-implementer",
        description: "Implements one plan task exactly as dispatched, test-first where a \
                      harness exists, and reports status, changed files, and verification. \
                      Use for a big task the Goal Runner delegates; it never reviews.",
        prompt: IMPLEMENTER_PROMPT,
        claude_model: "sonnet",
        claude_tools: EDITING_TOOLS,
        codex_model: "gpt-5.6-luna",
        codex_effort: "medium",
    },
    Subagent {
        name: "devez-reviewer",
        description: "Read-only reviewer for a task diff, a fix round, or a bounded plan. \
                      Returns findings with severity and a verdict. Use for Standard-intensity \
                      task reviews, scoped re-reviews, and bounded plan reviews; it never edits \
                      and never spawns subagents.",
        prompt: REVIEWER_PROMPT,
        claude_model: "sonnet",
        claude_tools: READ_ONLY_TOOLS,
        codex_model: "gpt-5.6-terra",
        codex_effort: "high",
    },
    Subagent {
        name: "devez-senior-reviewer",
        description: "Read-only reviewer on the most capable model, reserved for the final \
                      whole-change review, Strict-intensity task reviews, and architectural \
                      plan reviews. Same contract as devez-reviewer; it never edits and never \
                      spawns subagents.",
        prompt: REVIEWER_PROMPT,
        claude_model: "opus",
        claude_tools: READ_ONLY_TOOLS,
        codex_model: "gpt-5.6-sol",
        codex_effort: "high",
    },
    Subagent {
        name: "devez-qa",
        description: "Adversarial tester that drives the real surface to break a change: \
                      boundary, malformed, repeated, and failing inputs, with evidence fit \
                      for the surface. Use for the final adversarial lane; read-only on code.",
        prompt: QA_PROMPT,
        claude_model: "sonnet",
        claude_tools: READ_ONLY_TOOLS,
        codex_model: "gpt-5.6-terra",
        codex_effort: "medium",
    },
];

/// The `agents` option the Claude bridge passes to the SDK on every session.
pub fn claude_agent_definitions() -> Value {
    let mut agents = serde_json::Map::new();
    for agent in &SUBAGENTS {
        agents.insert(
            agent.name.to_owned(),
            json!({
                "description": agent.description,
                "prompt": agent.prompt.trim(),
                "tools": agent.claude_tools,
                "model": agent.claude_model,
            }),
        );
    }
    Value::Object(agents)
}

/// One Codex custom-agent file. The instructions ride in a literal multi-line
/// string so nothing in the prompt needs escaping.
pub fn codex_agent_toml(agent: &Subagent) -> String {
    format!(
        "# DevezVibe가 만든 서브에이전트 정의입니다. 모델이나 지침을 자유롭게 고쳐도 되며,\n\
         # 배포된 기본 지침만 갱신하며, 직접 고친 지침과 나머지 설정은 보존합니다.\n\
         name = \"{name}\"\n\
         description = \"{description}\"\n\
         model = \"{model}\"\n\
         model_reasoning_effort = \"{effort}\"\n\
         developer_instructions = '''\n{prompt}\n'''\n",
        name = agent.name,
        description = agent.description,
        model = agent.codex_model,
        effort = agent.codex_effort,
        prompt = agent.prompt.trim(),
    )
}

/// Replace only a recognized shipped instruction block in our generated format.
/// Unrecognized or customized files are left byte-for-byte intact.
fn upgraded_codex_agent(existing: &str, agent: &Subagent) -> Option<String> {
    if !existing.starts_with("# DevezVibe가 만든 서브에이전트 정의입니다.") {
        return None;
    }
    let legacy = match agent.name {
        "devez-implementer" => LEGACY_IMPLEMENTER,
        "devez-reviewer" | "devez-senior-reviewer" => LEGACY_REVIEWER,
        "devez-qa" => LEGACY_QA,
        _ => return None,
    };
    let newline = if existing.contains("\r\n") { "\r\n" } else { "\n" };
    let block = |prompt: &str| {
        format!("\ndeveloper_instructions = '''\n{}\n'''", prompt.replace("\r\n", "\n").trim())
            .replace('\n', newline)
    };
    let old = legacy
        .iter()
        .map(|body| block(body))
        .find(|old| existing.matches(old.as_str()).count() == 1)?;
    Some(existing.replacen(&old, &block(agent.prompt), 1).replacen(
        "# 이 파일이 있는 동안 DevezVibe는 다시 덮어쓰지 않습니다.",
        "# 배포된 기본 지침만 갱신하며, 직접 고친 지침과 나머지 설정은 보존합니다.",
        1,
    ))
}

/// Keep a recovery copy before replacing the file with a fully written sibling.
fn save_upgraded_agent(path: &Path, existing: &str, updated: &str) -> std::io::Result<()> {
    let backup = path.with_extension("toml.pre-readability.bak");
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&backup) {
        Ok(mut file) => {
            file.write_all(existing.as_bytes())?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::read(&backup)? != existing.as_bytes() {
                return Err(error);
            }
        }
        Err(error) => return Err(error),
    }
    let temporary = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
    let result = (|| {
        file.write_all(updated.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Provision missing files and upgrade known defaults, preserving user settings.
pub fn provision_codex_agents(home: &Path) -> std::io::Result<()> {
    let directory = home.join("agents");
    std::fs::create_dir_all(&directory)?;
    for agent in &SUBAGENTS {
        let path = directory.join(format!("{}.toml", agent.name));
        match std::fs::read_to_string(&path) {
            Ok(existing) => {
                if let Some(updated) = upgraded_codex_agent(&existing, agent) {
                    save_upgraded_agent(&path, &existing, &updated)?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::write(&path, codex_agent_toml(agent))?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_lane_ships_a_prompt_and_a_fixed_model() {
        for agent in &SUBAGENTS {
            assert!(!agent.prompt.trim().is_empty(), "{} prompt is empty", agent.name);
            assert!(!agent.claude_model.is_empty());
            assert!(!agent.codex_model.is_empty());
            assert!(agent.name.starts_with("devez-"));
        }
    }

    #[test]
    fn claude_definitions_pin_model_and_tools_per_lane() {
        let agents = claude_agent_definitions();
        assert_eq!(agents.as_object().map(|map| map.len()), Some(4));
        let reviewer = &agents["devez-reviewer"];
        assert_eq!(reviewer["model"], "sonnet");
        assert!(!reviewer["tools"].as_array().unwrap().iter().any(|tool| tool == "Edit"));
        // The most capable model is reserved for one lane.
        assert_eq!(agents["devez-senior-reviewer"]["model"], "opus");
        assert_eq!(
            SUBAGENTS.iter().filter(|agent| agent.claude_model == "opus").count(),
            1
        );
        let implementer = &agents["devez-implementer"];
        assert_eq!(implementer["model"], "sonnet");
        assert!(implementer["tools"].as_array().unwrap().iter().any(|tool| tool == "Edit"));
    }

    #[test]
    fn codex_agent_file_carries_name_model_and_instructions() {
        let toml = codex_agent_toml(&SUBAGENTS[1]);
        assert!(toml.contains("name = \"devez-reviewer\""));
        assert!(toml.contains("model = \"gpt-5.6-terra\""));
        assert!(toml.contains("model_reasoning_effort = \"high\""));
        assert!(codex_agent_toml(&SUBAGENTS[2]).contains("model = \"gpt-5.6-sol\""));
        assert!(toml.contains("developer_instructions = '''"));
        assert!(toml.contains("DevezVibe reviewer"));
        assert!(!SUBAGENTS.iter().any(|agent| agent.prompt.contains("'''")));
    }

    #[test]
    fn provisioning_writes_missing_files_and_keeps_existing_ones() {
        let home = std::env::temp_dir().join(format!("devez-subagents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let agents = home.join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let custom = agents.join("devez-qa.toml");
        std::fs::write(&custom, "model = \"custom\"\n").unwrap();

        provision_codex_agents(&home).unwrap();

        assert!(agents.join("devez-implementer.toml").exists());
        assert!(agents.join("devez-reviewer.toml").exists());
        assert_eq!(std::fs::read_to_string(&custom).unwrap(), "model = \"custom\"\n");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn upgrades_shipped_prompts_with_backups_and_preserves_settings_on_repeat() {
        for (index, newline) in ["\n", "\r\n"].into_iter().enumerate() {
            let home = std::env::temp_dir().join(format!("devez-upgrade-{}-{index}", std::process::id()));
            let agents = home.join("agents");
            std::fs::create_dir_all(&agents).unwrap();
            for (agent, legacy) in SUBAGENTS.iter().zip([
                LEGACY_IMPLEMENTER, LEGACY_REVIEWER, LEGACY_REVIEWER, LEGACY_QA,
            ]) {
                // Every shipped body, oldest first, must upgrade to the current one.
                for legacy in legacy.iter() {
                    let prefix = format!(
                        "# DevezVibe가 만든 서브에이전트 정의입니다.\nname = \"{}\"\n\
                         model = \"my-model\"\nmodel_reasoning_effort = \"low\"\n\
                         developer_instructions = '''\n", agent.name,
                    );
                    let suffix = "\n'''\nsandbox_mode = \"read-only\"\n# user comment\n";
                    let existing = format!("{prefix}{}{suffix}", legacy.trim()).replace('\n', newline);
                    let path = agents.join(format!("{}.toml", agent.name));
                    std::fs::write(&path, &existing).unwrap();
                    provision_codex_agents(&home).unwrap();
                    let expected = format!("{prefix}{}{suffix}", agent.prompt.trim()).replace('\n', newline);
                    assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
                    let backup = path.with_extension("toml.pre-readability.bak");
                    assert_eq!(std::fs::read_to_string(&backup).unwrap(), existing);
                    provision_codex_agents(&home).unwrap();
                    assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
                    assert_eq!(std::fs::read_to_string(&backup).unwrap(), existing);
                    // A later body in the same directory would reuse the backup name,
                    // so each shipped body gets a clean directory of its own.
                    std::fs::remove_file(&backup).unwrap();
                    std::fs::remove_file(&path).unwrap();
                }
            }
            std::fs::remove_dir_all(home).unwrap();
        }
    }

    #[test]
    fn customized_and_unrecognized_instruction_blocks_are_not_upgraded() {
        let agent = &SUBAGENTS[1];
        let old = format!(
            "# DevezVibe가 만든 서브에이전트 정의입니다.\ndeveloper_instructions = '''\n{}\n'''\n",
            LEGACY_REVIEWER[0].trim(),
        );
        for custom in [
            old.replace("You are a DevezVibe reviewer.", "Custom reviewer instructions."),
            old.replace("\n'''", "\nKeep my custom rule.\n'''"),
            old.replace("\n'''", ""),
            old.replace("# DevezVibe가 만든 서브에이전트 정의입니다.", "# user file"),
            format!("{old}{old}"),
            codex_agent_toml(agent),
        ] {
            assert!(upgraded_codex_agent(&custom, agent).is_none());
        }
    }

    #[test]
    fn a_backup_failure_does_not_replace_the_original_agent() {
        let home = std::env::temp_dir().join(format!("devez-upgrade-failure-{}", std::process::id()));
        let agents = home.join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let path = agents.join("devez-implementer.toml");
        let original = format!(
            "# DevezVibe가 만든 서브에이전트 정의입니다.\ndeveloper_instructions = '''\n{}\n'''\n",
            LEGACY_IMPLEMENTER[0].trim(),
        );
        std::fs::write(&path, &original).unwrap();
        std::fs::create_dir_all(path.with_extension("toml.pre-readability.bak")).unwrap();
        assert!(provision_codex_agents(&home).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn provider_subagent_prompts_have_no_builder_length_limit() {
        let claude = claude_agent_definitions();
        for agent in &SUBAGENTS {
            let codex = codex_agent_toml(agent);
            for prompt in [claude[agent.name]["prompt"].as_str().unwrap(), codex.as_str()] {
                for limit in ["under fifteen lines", "200자", "불릿 두세 개"] {
                    assert!(!prompt.contains(limit), "{}: {limit}", agent.name);
                }
                assert!(prompt.contains("한국어"));
            }
        }
    }
}
