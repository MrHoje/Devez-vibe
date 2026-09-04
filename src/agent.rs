//! The agent role a turn is sent under.
//!
//! A role is not a separate runtime: `AppState` owns the selection, and the
//! chosen role's instruction rides along with the next turn through the same
//! `additionalContext` path every provider already uses. Claude, Codex and
//! OpenCode therefore see the same role text.
//!
//! The instruction stays in the conversation after the turn that carried it, so
//! returning to `Standard` sends one reset block rather than simply going quiet
//! — see [`AgentTurnContext`].

use std::{
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// Which role the next turn is sent under.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentMode {
    /// The provider's own general-purpose behavior, with no role text added.
    #[default]
    Standard,
    Planner,
    Advisor,
    GoalRunner,
    /// A role defined by a file in the app's own agents folder. The payload is
    /// the role's index in [`custom_roles`].
    Custom(u8),
}

/// The roles compiled into the app, in the order Tab cycles through them.
pub const BUILTIN: [AgentMode; 4] = [
    AgentMode::Standard,
    AgentMode::Planner,
    AgentMode::Advisor,
    AgentMode::GoalRunner,
];

/// Every role Tab cycles through: the built-in four first, then whatever the
/// agents folder defines, so adding a role never renumbers the built-ins.
pub fn choices() -> Vec<AgentMode> {
    let mut all = BUILTIN.to_vec();
    all.extend((0..custom_roles().len()).map(|index| AgentMode::Custom(index as u8)));
    all
}

const PLANNER_PROMPT: &str = include_str!("../prompts/agents/planner.md");
const ADVISOR_PROMPT: &str = include_str!("../prompts/agents/advisor.md");
const GOAL_RUNNER_PROMPT: &str = include_str!("../prompts/agents/goal-runner.md");

/// Sent once when the user returns to `Standard`, because the previous role's
/// instruction is still sitting in the conversation history.
const STANDARD_RESET: &str = "Use the provider's normal general-purpose behavior for this and \
following turns. Do not continue a Planner, Advisor, Goal Runner, or a user-defined role solely because an earlier \
turn selected one.";

impl AgentMode {
    /// The wire and command spelling, e.g. `/agent planner`.
    pub fn id(self) -> &'static str {
        match self {
            Self::Standard => "builder",
            Self::Planner => "planner",
            Self::Advisor => "advisor",
            Self::GoalRunner => "goal-runner",
            Self::Custom(index) => custom_role(index).map_or("custom", |role| role.id.as_str()),
        }
    }

    /// The name shown in the status line and notices.
    pub fn label(self) -> &'static str {
        match self {
            Self::Standard => "Builder",
            Self::Planner => "Planner",
            Self::Advisor => "Advisor",
            Self::GoalRunner => "Goal Runner",
            Self::Custom(index) => custom_role(index).map_or("Custom", |role| role.label.as_str()),
        }
    }

    /// One line for the picker.
    pub fn detail(self) -> &'static str {
        match self {
            Self::Standard => "일상적인 개발 작업 전반을 유연하게 처리합니다.",
            Self::Planner => "요구사항을 분석해 체계적인 구현 계획을 수립합니다.",
            Self::Advisor => "기술적 선택과 위험 요소를 검토해 최적의 방향을 제시합니다.",
            Self::GoalRunner => "목표를 정하고 끝까지 완수합니다.",
            Self::Custom(index) => custom_role(index)
                .map_or("사용자가 정의한 역할입니다.", |role| role.detail.as_str()),
        }
    }

    /// Case-insensitive, no aliases: the argument to `/agent`.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim().to_ascii_lowercase();
        choices().into_iter().find(|mode| mode.id() == value)
    }

    /// The next role Tab lands on, wrapping back to `Standard`.
    pub fn next(self) -> Self {
        let all = choices();
        let position = all.iter().position(|mode| *mode == self).unwrap_or(0);
        all[(position + 1) % all.len()]
    }

    /// The role's own instruction. `Standard` adds nothing of its own.
    fn specialized_instruction(self) -> Option<&'static str> {
        match self {
            Self::Standard => None,
            Self::Planner => Some(PLANNER_PROMPT),
            Self::Advisor => Some(ADVISOR_PROMPT),
            Self::GoalRunner => Some(GOAL_RUNNER_PROMPT),
            Self::Custom(index) => custom_role(index).map(|role| role.prompt.as_str()),
        }
    }
}

/// What the next turn carries about the role, if anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentTurnContext {
    /// A specialized role is selected; its instruction repeats every turn.
    Specialized(AgentMode),
    /// The user came back to `Standard`, so one block retires the old role.
    StandardReset,
}

impl AgentTurnContext {
    /// The role block, wrapped so the model can tell it apart from user text and
    /// knows it supersedes any earlier block.
    pub fn render(self) -> String {
        let (mode, body) = match self {
            Self::Specialized(mode) => (
                mode,
                mode.specialized_instruction()
                    .expect("a specialized role always carries an instruction"),
            ),
            Self::StandardReset => (AgentMode::Standard, STANDARD_RESET),
        };
        // A specialized role keeps the language and readability rules but not
        // the length caps: a plan or a final report squeezed into a few bullets
        // loses exactly the substance the role exists for.
        let response_rules = match self {
            Self::Specialized(_) => {
                "The standing DevezVibe language and formatting rules apply to this role \
                 unchanged — answer in Korean, structured and readable. The tight \
                 response-length caps are relaxed for this role's output, with a soft bound in \
                 their place: include only the sections that matter for this task, keep the \
                 whole answer around 15 lines, and keep each item to a sentence or two."
            }
            Self::StandardReset => {
                "The response rules and length caps from the standing DevezVibe instructions \
                 apply unchanged."
            }
        };
        format!(
            "<devez-vibe-agent mode=\"{}\" version=\"1\">\nThis block sets the current DevezVibe \
             agent mode. It supersedes every earlier devez-vibe-agent block in this conversation, \
             and stays in effect until another one arrives. {}\n\n{}\n</devez-vibe-agent>",
            mode.id(),
            response_rules,
            body.trim()
        )
    }
}

/// A role defined outside the binary: one Markdown file in the agents folder,
/// front matter for how the role is shown, body for the instruction it sends.
pub struct CustomRole {
    id: String,
    label: String,
    detail: String,
    prompt: String,
}

static CUSTOM_ROLES: OnceLock<Vec<CustomRole>> = OnceLock::new();

/// The agents folder's roles, read once per run and kept in file-name order so
/// the status line, the picker and `/agent` all agree on what an index means.
fn custom_roles() -> &'static [CustomRole] {
    CUSTOM_ROLES.get_or_init(load_custom_roles)
}

fn custom_role(index: u8) -> Option<&'static CustomRole> {
    custom_roles().get(usize::from(index))
}

/// Where role files live: `%APPDATA%/DevezVibe/agents`, one `.md` per role.
fn agents_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("DevezVibe")
        .join("agents")
}

fn load_custom_roles() -> Vec<CustomRole> {
    let Ok(entries) = fs::read_dir(agents_dir()) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        })
        .collect();
    paths.sort();
    paths
        .iter()
        .filter_map(|path| read_custom_role(path))
        // The mode payload is a `u8`, so a folder past that many files is a
        // mistake rather than a request to honour.
        .take(usize::from(u8::MAX))
        .collect()
}

fn read_custom_role(path: &Path) -> Option<CustomRole> {
    let text = fs::read_to_string(path).ok()?;
    let stem = path.file_stem()?.to_str()?.trim();
    let id = stem.to_ascii_lowercase();
    // A file cannot take over a built-in name: Builder stays Builder.
    if id.is_empty() || BUILTIN.iter().any(|mode| mode.id() == id) {
        return None;
    }
    parse_custom_role(&id, stem, &text)
}

/// The body is the role's instruction, so a file without one is not a role.
fn parse_custom_role(id: &str, stem: &str, text: &str) -> Option<CustomRole> {
    let (front, body) = split_front_matter(text);
    let prompt = body.trim();
    if prompt.is_empty() {
        return None;
    }
    Some(CustomRole {
        id: id.to_owned(),
        label: front_value(front, "label").unwrap_or(stem).to_owned(),
        detail: front_value(front, "detail")
            .unwrap_or("사용자가 정의한 역할입니다.")
            .to_owned(),
        prompt: prompt.to_owned(),
    })
}

/// `---` fenced front matter, if the file opens with one. The closing fence's
/// own line ends it, so the body starts on the line after that.
fn split_front_matter(text: &str) -> (&str, &str) {
    let text = text.trim_start_matches('\u{feff}');
    let Some(rest) = text.strip_prefix("---") else {
        return ("", text);
    };
    let Some((front, after)) = rest.split_once("\n---") else {
        return ("", text);
    };
    let body = after.split_once('\n').map(|(_, rest)| rest).unwrap_or("");
    (front, body)
}

fn front_value<'a>(front: &'a str, name: &str) -> Option<&'a str> {
    front.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        let value = value.trim();
        (key.trim().eq_ignore_ascii_case(name) && !value.is_empty()).then_some(value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_cycles_every_role_and_returns_to_standard() {
        let all = choices();
        let mut mode = AgentMode::Standard;
        let mut seen = Vec::new();
        for _ in 0..all.len() {
            mode = mode.next();
            seen.push(mode);
        }
        // The built-in four keep their order and open the cycle, whatever the
        // agents folder adds after them.
        assert_eq!(
            seen[..3],
            [
                AgentMode::Planner,
                AgentMode::Advisor,
                AgentMode::GoalRunner,
            ]
        );
        assert_eq!(seen.last().copied(), Some(AgentMode::Standard));
        assert_eq!(seen.len(), all.len());
    }

    /// The built-ins come first and keep their identity, so a role file can
    /// only ever be appended.
    #[test]
    fn choices_open_with_the_built_in_roles() {
        let all = choices();
        assert_eq!(all[..BUILTIN.len()], BUILTIN);
        assert!(all[BUILTIN.len()..]
            .iter()
            .all(|mode| matches!(mode, AgentMode::Custom(_))));
    }

    #[test]
    fn a_role_file_takes_its_name_from_the_stem_and_its_prompt_from_the_body() {
        let role = parse_custom_role(
            "qa-tester",
            "qa-tester",
            "---\nlabel: QA Tester\ndetail: 검수합니다.\n---\n검수 절차를 따른다.\n",
        )
        .expect("a file with a body is a role");
        assert_eq!(role.label, "QA Tester");
        assert_eq!(role.detail, "검수합니다.");
        assert_eq!(role.prompt, "검수 절차를 따른다.");
    }

    /// Front matter is optional, and a body-less file is not a role.
    #[test]
    fn a_role_file_without_front_matter_or_body_degrades_predictably() {
        let bare = parse_custom_role("qa", "qa", "본문만 있는 역할").expect("body is enough");
        assert_eq!(bare.label, "qa");
        assert_eq!(bare.prompt, "본문만 있는 역할");
        assert!(parse_custom_role("qa", "qa", "---\nlabel: QA\n---\n\n").is_none());
    }

    #[test]
    fn parse_ignores_case_and_rejects_unknown_names() {
        assert_eq!(AgentMode::parse("Planner"), Some(AgentMode::Planner));
        assert_eq!(AgentMode::parse(" GOAL-RUNNER "), Some(AgentMode::GoalRunner));
        assert_eq!(AgentMode::parse("plan"), None);
        assert_eq!(AgentMode::parse(""), None);
    }

    #[test]
    fn standard_carries_no_instruction_of_its_own() {
        assert!(AgentMode::Standard.specialized_instruction().is_none());
        for mode in [AgentMode::Planner, AgentMode::Advisor, AgentMode::GoalRunner] {
            let prompt = mode
                .specialized_instruction()
                .expect("specialized roles ship a prompt");
            assert!(!prompt.trim().is_empty(), "{} prompt is empty", mode.id());
        }
    }

    #[test]
    fn every_block_declares_its_mode_and_supersedes_earlier_ones() {
        for mode in [AgentMode::Planner, AgentMode::Advisor, AgentMode::GoalRunner] {
            let block = AgentTurnContext::Specialized(mode).render();
            assert!(block.starts_with(&format!("<devez-vibe-agent mode=\"{}\"", mode.id())));
            assert!(block.ends_with("</devez-vibe-agent>"));
            assert!(block.contains("supersedes every earlier devez-vibe-agent block"));
        }
        let reset = AgentTurnContext::StandardReset.render();
        assert!(reset.contains("mode=\"builder\""));
        assert!(reset.contains("Do not continue a Planner"));
        // A specialized role keeps the language rules but drops the length
        // caps; the reset restores the full rules, caps included.
        for mode in [AgentMode::Planner, AgentMode::Advisor, AgentMode::GoalRunner] {
            let block = AgentTurnContext::Specialized(mode).render();
            assert!(block.contains("language and formatting rules"));
            assert!(block.contains("caps are relaxed"));
            assert!(block.contains("around 15 lines"));
        }
        assert!(reset.contains("length caps"));
        assert!(reset.contains("apply unchanged"));
    }

    /// The role prompts are the product's own text, not a copy of the plugin
    /// they were modelled on, and they must not name its runtime.
    #[test]
    fn role_prompts_avoid_external_runtime_vocabulary() {
        for mode in [AgentMode::Planner, AgentMode::Advisor, AgentMode::GoalRunner] {
            let prompt = mode
                .specialized_instruction()
                .expect("specialized roles ship a prompt")
                .to_ascii_lowercase();
            for forbidden in ["hoje", "ultragoal", "ralplan", ".hoje"] {
                assert!(
                    !prompt.contains(forbidden),
                    "{} prompt leaks {forbidden}",
                    mode.id()
                );
            }
        }
    }
}

#[cfg(test)]
mod agents_folder_tests {
    use super::*;

    /// The folder is read from the real environment, so this only asserts the
    /// shape: whatever is there is appended after the built-ins and answers
    /// `/agent` by its own id.
    #[test]
    fn a_role_in_the_agents_folder_joins_the_cycle_under_its_own_id() {
        // A folder holding role files must produce roles: a silent zero here
        // is the failure this whole path exists to avoid.
        if fs::read_dir(agents_dir()).into_iter().flatten().flatten().any(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        }) {
            assert!(!custom_roles().is_empty());
        }
        for (index, role) in custom_roles().iter().enumerate() {
            let mode = AgentMode::Custom(index as u8);
            assert_eq!(mode.id(), role.id);
            assert_eq!(AgentMode::parse(&role.id), Some(mode));
            assert!(!mode.label().is_empty());
            assert!(mode.specialized_instruction().is_some_and(|prompt| !prompt.trim().is_empty()));
        }
    }
}
