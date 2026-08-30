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

/// Which role the next turn is sent under.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentMode {
    /// The provider's own general-purpose behavior, with no role text added.
    #[default]
    Standard,
    Planner,
    Advisor,
    Finisher,
}

/// Every role, in the order Tab cycles through them.
pub const CHOICES: [AgentMode; 4] = [
    AgentMode::Standard,
    AgentMode::Planner,
    AgentMode::Advisor,
    AgentMode::Finisher,
];

const PLANNER_PROMPT: &str = include_str!("../prompts/agents/planner.md");
const ADVISOR_PROMPT: &str = include_str!("../prompts/agents/advisor.md");
const FINISHER_PROMPT: &str = include_str!("../prompts/agents/finisher.md");

/// Sent once when the user returns to `Standard`, because the previous role's
/// instruction is still sitting in the conversation history.
const STANDARD_RESET: &str = "Use the provider's normal general-purpose behavior for this and \
following turns. Do not continue a Planner, Advisor, or Finisher role solely because an earlier \
turn selected one.";

impl AgentMode {
    /// The wire and command spelling, e.g. `/agent planner`.
    pub fn id(self) -> &'static str {
        match self {
            Self::Standard => "builder",
            Self::Planner => "planner",
            Self::Advisor => "advisor",
            Self::Finisher => "finisher",
        }
    }

    /// The name shown in the status line and notices.
    pub fn label(self) -> &'static str {
        match self {
            Self::Standard => "Builder",
            Self::Planner => "Planner",
            Self::Advisor => "Advisor",
            Self::Finisher => "Finisher",
        }
    }

    /// One line for the picker.
    pub fn detail(self) -> &'static str {
        match self {
            Self::Standard => "The default agent, tuned for everyday work.",
            Self::Planner => "Clarifies requirements and plans from the repository.",
            Self::Advisor => "Weighs risks and trade-offs, then recommends.",
            Self::Finisher => "Implements, verifies, and reviews until done.",
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
            Self::Planner => Self::Advisor,
            Self::Advisor => Self::Finisher,
            Self::Finisher => Self::Standard,
        }
    }

    /// The role's own instruction. `Standard` adds nothing of its own.
    fn specialized_instruction(self) -> Option<&'static str> {
        match self {
            Self::Standard => None,
            Self::Planner => Some(PLANNER_PROMPT),
            Self::Advisor => Some(ADVISOR_PROMPT),
            Self::Finisher => Some(FINISHER_PROMPT),
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
        // Every role answers under the same Devez response rules; the block
        // changes how the work is approached, never how the answer is formatted.
        format!(
            "<devez-vibe-agent mode=\"{}\" version=\"1\">\nThis block sets the current DevezVibe \
             agent mode. It supersedes every earlier devez-vibe-agent block in this conversation, \
             and stays in effect until another one arrives. The response rules and length caps \
             from the standing DevezVibe instructions apply to this role unchanged.\n\n{}\n</devez-vibe-agent>",
            mode.id(),
            body.trim()
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
                AgentMode::Advisor,
                AgentMode::Finisher,
                AgentMode::Standard,
            ]
        );
    }

    #[test]
    fn parse_ignores_case_and_rejects_unknown_names() {
        assert_eq!(AgentMode::parse("Planner"), Some(AgentMode::Planner));
        assert_eq!(AgentMode::parse(" FINISHER "), Some(AgentMode::Finisher));
        assert_eq!(AgentMode::parse("plan"), None);
        assert_eq!(AgentMode::parse(""), None);
    }

    #[test]
    fn standard_carries_no_instruction_of_its_own() {
        assert!(AgentMode::Standard.specialized_instruction().is_none());
        for mode in [AgentMode::Planner, AgentMode::Advisor, AgentMode::Finisher] {
            let prompt = mode
                .specialized_instruction()
                .expect("specialized roles ship a prompt");
            assert!(!prompt.trim().is_empty(), "{} prompt is empty", mode.id());
        }
    }

    #[test]
    fn every_block_declares_its_mode_and_supersedes_earlier_ones() {
        for mode in [AgentMode::Planner, AgentMode::Advisor, AgentMode::Finisher] {
            let block = AgentTurnContext::Specialized(mode).render();
            assert!(block.starts_with(&format!("<devez-vibe-agent mode=\"{}\"", mode.id())));
            assert!(block.ends_with("</devez-vibe-agent>"));
            assert!(block.contains("supersedes every earlier devez-vibe-agent block"));
        }
        let reset = AgentTurnContext::StandardReset.render();
        assert!(reset.contains("mode=\"builder\""));
        assert!(reset.contains("Do not continue a Planner"));
        // Every role — reset included — answers under the same response rules,
        // with no per-role formatting carve-outs.
        for context in [
            AgentTurnContext::StandardReset,
            AgentTurnContext::Specialized(AgentMode::Planner),
            AgentTurnContext::Specialized(AgentMode::Advisor),
            AgentTurnContext::Specialized(AgentMode::Finisher),
        ] {
            let block = context.render();
            assert!(!block.contains("deliverable"));
            assert!(block.contains("response rules and length caps"));
        }
    }

    /// The role prompts are the product's own text, not a copy of the plugin
    /// they were modelled on, and they must not name its runtime.
    #[test]
    fn role_prompts_avoid_external_runtime_vocabulary() {
        for mode in [AgentMode::Planner, AgentMode::Advisor, AgentMode::Finisher] {
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
