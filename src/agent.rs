//! The agent role a turn is sent under.
//!
//! A role is not a separate runtime: `AppState` owns the selection, and the
//! chosen role's instruction rides along with every turn through the same
//! `additionalContext` path every provider already uses. Claude, Codex and
//! OpenCode therefore see the same role text.
//!
//! Every role, `Standard` included, carries its own block on every turn. Each
//! block declares that it supersedes the earlier ones, so switching roles needs
//! no separate reset.

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
}

/// Every role, in the order Tab cycles through them.
pub const CHOICES: [AgentMode; 4] = [
    AgentMode::Standard,
    AgentMode::Planner,
    AgentMode::GoalRunner,
    AgentMode::Reviewer,
];

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
        }
    }

    /// The name shown in the status line and notices.
    pub fn label(self) -> &'static str {
        match self {
            Self::Standard => "Builder",
            Self::Planner => "Planner",
            Self::GoalRunner => "Goal Runner",
            Self::Reviewer => "Reviewer",
        }
    }

    /// One line for the picker.
    pub fn detail(self) -> &'static str {
        match self {
            Self::Standard => "일상적인 개발 작업 전반을 유연하게 처리합니다.",
            Self::Planner => "요구사항을 분석해 체계적인 구현 계획을 수립합니다.",
            Self::GoalRunner => "목표를 정하고 끝까지 완수합니다.",
            Self::Reviewer => "변경 내용과 계획을 근거 기반으로 검토해 심각도와 판정을 냅니다.",
        }
    }

    /// Case-insensitive, no aliases: the argument to `/agent`.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim().to_ascii_lowercase();
        CHOICES.into_iter().find(|mode| mode.id() == value)
    }

    /// The next role Tab lands on, wrapping back to `Standard`.
    pub fn next(self) -> Self {
        match self {
            Self::Standard => Self::Planner,
            Self::Planner => Self::GoalRunner,
            Self::GoalRunner => Self::Reviewer,
            Self::Reviewer => Self::Standard,
        }
    }

    /// The role's own instruction, sent on every turn.
    fn instruction(self) -> &'static str {
        match self {
            Self::Standard => BUILDER_PROMPT,
            Self::Planner => PLANNER_PROMPT,
            Self::GoalRunner => GOAL_RUNNER_PROMPT,
            Self::Reviewer => REVIEWER_PROMPT,
        }
    }

    /// The role block, wrapped so the model can tell it apart from user text and
    /// knows it supersedes any earlier block.
    pub fn render_turn_block(self) -> String {
        // Builder keeps the standing length caps: it is the everyday seat. A
        // specialized role keeps the language and readability rules but not the
        // caps: a plan or a final report squeezed into a few bullets loses
        // exactly the substance the role exists for.
        let response_rules = match self {
            Self::Standard => {
                "The response rules and length caps from the standing DevezVibe instructions \
                 apply unchanged."
            }
            Self::Planner | Self::GoalRunner | Self::Reviewer => {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_cycles_every_role_and_returns_to_standard() {
        let mut mode = AgentMode::Standard;
        let mut seen = Vec::new();
        for _ in 0..CHOICES.len() {
            mode = mode.next();
            seen.push(mode);
        }
        assert_eq!(
            seen,
            vec![
                AgentMode::Planner,
                AgentMode::GoalRunner,
                AgentMode::Reviewer,
                AgentMode::Standard,
            ]
        );
    }

    #[test]
    fn parse_ignores_case_and_rejects_unknown_names() {
        assert_eq!(AgentMode::parse("Planner"), Some(AgentMode::Planner));
        assert_eq!(AgentMode::parse(" GOAL-RUNNER "), Some(AgentMode::GoalRunner));
        assert_eq!(AgentMode::parse("plan"), None);
        assert_eq!(AgentMode::parse(""), None);
    }

    #[test]
    fn every_role_ships_a_prompt() {
        for mode in CHOICES {
            assert!(!mode.instruction().trim().is_empty(), "{} prompt is empty", mode.id());
        }
        assert!(AgentMode::Standard.instruction().contains("Builder role"));
    }

    #[test]
    fn every_block_declares_its_mode_and_supersedes_earlier_ones() {
        for mode in CHOICES {
            let block = mode.render_turn_block();
            assert!(block.starts_with(&format!("<devez-vibe-agent mode=\"{}\"", mode.id())));
            assert!(block.ends_with("</devez-vibe-agent>"));
            assert!(block.contains("supersedes every earlier devez-vibe-agent block"));
        }
    }

    /// Only Builder keeps the standing length caps; every specialized role
    /// lifts them.
    #[test]
    fn builder_keeps_the_length_caps_and_specialized_roles_lift_them() {
        assert!(AgentMode::Standard
            .render_turn_block()
            .contains("length caps from the standing DevezVibe instructions apply unchanged"));
        for mode in [AgentMode::Planner, AgentMode::GoalRunner, AgentMode::Reviewer] {
            assert!(mode.render_turn_block().contains("Every response-length cap is lifted"));
        }
    }
}
