//! Fixed-model subagents the specialized roles dispatch.
//!
//! Planner and Goal Runner hand work to three lanes — an implementer, a
//! reviewer, and an adversarial tester. Each lane has a model and a tool scope
//! fixed here, so the role prompt only has to name the lane: a dispatch that
//! forgets to pick a model no longer inherits the session's most expensive one.
//!
//! The same definitions reach every provider that has subagents. Claude gets
//! them as SDK agent definitions through the bridge on every session; Codex
//! reads custom agents from `~/.codex/agents/*.toml`, which DevezVibe writes
//! once and never overwrites, so the user can tune a model there.

use std::path::Path;

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
         # 이 파일이 있는 동안 DevezVibe는 다시 덮어쓰지 않습니다.\n\
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

/// Write the Codex agent files that do not exist yet under `<home>/agents/`.
/// Existing files are the user's and stay untouched.
pub fn provision_codex_agents(home: &Path) -> std::io::Result<()> {
    let directory = home.join("agents");
    std::fs::create_dir_all(&directory)?;
    for agent in &SUBAGENTS {
        let path = directory.join(format!("{}.toml", agent.name));
        if path.exists() {
            continue;
        }
        std::fs::write(&path, codex_agent_toml(agent))?;
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
}
