//! The agent role a turn is sent under.
//!
//! A role is not a separate runtime: `AppState` owns the selection, and the
//! chosen role's instruction rides along with every turn through the same
//! `additionalContext` path every provider already uses. Claude, Codex and
//! OpenCode therefore see the same role text.
//!
//! Every role, `Standard` included, carries its own block on every turn. Each
//! block declares that it supersedes the earlier ones, so switching roles needs
//! no separate reset. Roles beyond the built-in four come from the app's own
//! agents folder — see [`CustomRole`].

use std::{
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use serde_json::{Value, json};

/// Which role the next turn is sent under.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentMode {
    /// The provider's general-purpose behavior plus the Builder ladder: write
    /// only the code that has to exist.
    #[default]
    Standard,
    Planner,
    GoalRunner,
    Reviewer,
    /// A role defined by a file in the app's own agents folder. The payload is
    /// the role's index in [`custom_roles`].
    Custom(u8),
}

/// The roles compiled into the app, in the order Tab cycles through them.
pub const BUILTIN: [AgentMode; 4] = [
    AgentMode::Standard,
    AgentMode::Planner,
    AgentMode::Reviewer,
    AgentMode::GoalRunner,
];

/// Every role Tab cycles through: the built-in four first, then whatever the
/// agents folder defines, so adding a role never renumbers the built-ins.
pub fn choices() -> Vec<AgentMode> {
    let mut all = BUILTIN.to_vec();
    all.extend((0..custom_roles().len()).map(|index| AgentMode::Custom(index as u8)));
    all
}

const BUILDER_PROMPT: &str = include_str!("../prompts/agents/builder.md");
const PLANNER_PROMPT: &str = include_str!("../prompts/agents/planner.md");
const GOAL_RUNNER_PROMPT: &str = include_str!("../prompts/agents/goal-runner.md");
const REVIEWER_PROMPT: &str = include_str!("../prompts/agents/reviewer.md");

impl AgentMode {
    /// The wire and command spelling, e.g. `/agent planner`.
    pub fn id(self) -> &'static str {
        match self {
            Self::Standard => "builder",
            Self::Planner => "planner",
            Self::GoalRunner => "goal-runner",
            Self::Reviewer => "reviewer",
            Self::Custom(index) => custom_role(index).map_or("custom", |role| role.id.as_str()),
        }
    }

    /// The name shown in the status line and notices.
    pub fn label(self) -> &'static str {
        match self {
            Self::Standard => "Builder",
            Self::Planner => "Planner",
            Self::GoalRunner => "Goal Runner",
            Self::Reviewer => "Reviewer",
            Self::Custom(index) => custom_role(index).map_or("Custom", |role| role.label.as_str()),
        }
    }

    /// One line for the picker.
    pub fn detail(self) -> &'static str {
        match self {
            Self::Standard => "일상적인 개발 작업 전반을 유연하게 처리합니다.",
            Self::Planner => "요구사항을 분석해 체계적인 구현 계획을 수립합니다.",
            Self::GoalRunner => "목표를 정하고 끝까지 완수합니다.",
            Self::Reviewer => "변경 내용과 계획을 근거 기반으로 검토해 심각도와 판정을 냅니다.",
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

    /// What a turn under this role may change on disk; `None` leaves the
    /// provider's own permissions alone. Planner writes only its plan document,
    /// Reviewer writes nothing. The prompt states the same boundary; this makes
    /// it hold where the provider offers a hook — Claude refuses the tool call
    /// before it runs, Codex runs a fully read-only turn in its read-only
    /// sandbox — instead of resting on the model's word.
    pub fn tool_policy(self) -> Option<Value> {
        match self {
            Self::Planner => Some(json!({ "readOnly": true, "writableRoots": ["docs/plans"] })),
            Self::Reviewer => Some(json!({ "readOnly": true, "writableRoots": [] })),
            Self::Standard | Self::GoalRunner | Self::Custom(_) => None,
        }
    }

    /// The role's own instruction, sent on every turn.
    fn instruction(self) -> &'static str {
        match self {
            Self::Standard => BUILDER_PROMPT,
            Self::Planner => PLANNER_PROMPT,
            Self::GoalRunner => GOAL_RUNNER_PROMPT,
            Self::Reviewer => REVIEWER_PROMPT,
            // A `Custom` index only ever comes from `choices()`, so the folder
            // role exists; the fallback keeps the type total without a panic.
            Self::Custom(index) => {
                custom_role(index).map_or(BUILDER_PROMPT, |role| role.prompt.as_str())
            }
        }
    }

    /// The role block, wrapped so the model can tell it apart from user text and
    /// knows it supersedes any earlier block.
    pub fn render_turn_block(self) -> String {
        // Builder owns the length caps in its prompt: it is the everyday seat. A
        // specialized role — built-in or from the agents folder — keeps the
        // language and readability rules but not the caps: a plan or a final
        // report squeezed into a few bullets loses exactly the substance the
        // role exists for.
        let response_rules = match self {
            Self::Standard => {
                "The standing DevezVibe language and formatting rules apply unchanged. \
                 Response-length limits are defined by the Builder role below."
            }
            Self::Planner | Self::GoalRunner | Self::Reviewer | Self::Custom(_) => {
                "The standing DevezVibe language and formatting rules apply to this role \
                 unchanged — answer in Korean, structured and readable. Every response-length \
                 cap is lifted for this role's output: no bullet count, no character count, and \
                 no line count applies. Length follows the work — include every section the task \
                 needs, at the depth needed to be acted on, and stop when the substance is \
                 covered rather than when a budget runs out. Do not pad, and do not drop or \
                 compress a section to stay short."
            }
        };
        format!(
            "<devez-vibe-agent mode=\"{}\" version=\"1\">\nThis block sets the current DevezVibe \
             agent mode. It supersedes every earlier devez-vibe-agent block in this conversation, \
             and stays in effect until another one arrives. {}\n\n{}\n</devez-vibe-agent>",
            self.id(),
            response_rules,
            self.instruction().trim()
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
                AgentMode::Reviewer,
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
        assert_eq!(AgentMode::parse("Reviewer"), Some(AgentMode::Reviewer));
        assert_eq!(AgentMode::parse("plan"), None);
        assert_eq!(AgentMode::parse(""), None);
    }

    #[test]
    fn every_role_ships_a_prompt() {
        for mode in choices() {
            assert!(!mode.instruction().trim().is_empty(), "{} prompt is empty", mode.id());
        }
        assert!(AgentMode::Standard.instruction().contains("Builder role"));
    }

    #[test]
    fn every_block_declares_its_mode_and_supersedes_earlier_ones() {
        for mode in choices() {
            let block = mode.render_turn_block();
            assert!(block.starts_with(&format!("<devez-vibe-agent mode=\"{}\"", mode.id())));
            assert!(block.ends_with("</devez-vibe-agent>"));
            assert!(block.contains("supersedes every earlier devez-vibe-agent block"));
        }
    }

    /// Only Builder carries length caps; every other role, built-in
    /// or from the agents folder, lifts them.
    #[test]
    fn builder_keeps_the_length_caps_and_specialized_roles_lift_them() {
        assert!(AgentMode::Standard
            .render_turn_block()
            .contains("불릿 두세 개, 전체 200자 내외"));
        for mode in choices().into_iter().filter(|mode| *mode != AgentMode::Standard) {
            assert!(mode.render_turn_block().contains("Every response-length cap is lifted"));
        }
    }

    /// The two roles whose prompts forbid edits carry a policy the providers
    /// can enforce; the roles that implement carry none.
    #[test]
    fn only_the_read_only_roles_carry_a_tool_policy() {
        let planner = AgentMode::Planner.tool_policy().unwrap();
        assert_eq!(planner["readOnly"], true);
        assert_eq!(planner["writableRoots"], json!(["docs/plans"]));
        let reviewer = AgentMode::Reviewer.tool_policy().unwrap();
        assert_eq!(reviewer["readOnly"], true);
        assert_eq!(reviewer["writableRoots"], json!([]));
        for mode in choices()
            .into_iter()
            .filter(|mode| !matches!(mode, AgentMode::Planner | AgentMode::Reviewer))
        {
            assert!(mode.tool_policy().is_none(), "{} must keep full tools", mode.id());
        }
    }

    /// The role prompts are the product's own text, not a copy of the tools
    /// they were modelled on, and they must not name those runtimes.
    #[test]
    fn role_prompts_avoid_external_runtime_vocabulary() {
        for mode in [AgentMode::Planner, AgentMode::GoalRunner, AgentMode::Reviewer] {
            let prompt = mode.instruction().to_ascii_lowercase();
            for forbidden in ["hoje", "ultragoal", "ralplan", ".hoje", "superpowers", "gajae"] {
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
            assert!(!mode.instruction().trim().is_empty());
        }
    }
}
