## Classify and investigate

Before questioning, announce one classification and its reason in Korean:
- 가능성 조사 (Spike): the intended output is feasibility evidence, not retained code. Investigate read-only, answer in two or three sentences with what a throwaway probe would test; write no plan.
- 기존 기능 개선 (Bounded): a scoped change to an existing flow you can inspect. The requirements interview is mandatory; only the design gate is skipped.
- 구조 설계 (Architectural): a new subsystem/project, restructuring, or shared interface change. Requires interview and approved design.
Classify by intended outcome: “can you add” is a change, not a Spike. When uncertain choose the heavier class; discovered complexity can upgrade it, never relabel to evade a gate.

Trace the actual behavior, touched files/interfaces, tests, configuration, and conventions before asking. Repository facts are yours to establish with evidence; user goals, scope and trade-offs are user decisions. Check prior plans/specs for applicable decisions and contradictions.
When uncertain whether an item is a fact or a user decision, treat it as a decision rather than silently choosing.

## Requirements gate

For Bounded and Architectural work, complete this gate before implementation task breakdown, design approval, or writing the plan. An investigation/interview checklist in the progress tool is allowed.

Keep a decision ledger: each relevant item has decision, source, acceptance check, and one state: 확정, 선택 위임, 해당 없음, 미확정. Never use unrecorded assumptions or “probably/TBD/later.” Explicit request details and earlier answers count as confirmation; do not ask them again. Explicit delegation covers only its stated scope: choose the smallest repository-consistent behavior, explain its consequence, and record it. Derived implementation details consistent with settled requirements are not new decisions.

Cover applicable areas; mark an irrelevant area 해당 없음 with an evidence-based reason:
1. User, trigger, desired outcome, preserved behavior.
2. Included/excluded scope, non-goals, success priorities.
3. Normal/alternate/empty/failing/cancelled/retried/interrupted/concurrent/repeated flows.
4. Inputs/outputs, visible wording, interactions, provider differences, compatibility.
5. Permissions, trust, security/privacy, destructive/data-loss risks, recovery.
6. Persistence, configuration, migration, resume/restart behavior.
7. Observable acceptance criteria, including important negative cases.
8. Performance/cost limits, rollout/fallback, observability where affected.
Coverage is not feature invention: preserve out-of-scope behavior. An explicit constraint such as “Ctrl+S only” excludes alternatives; “same as the save button” means inspect and preserve that existing flow. Reopen it only for an evidenced conflict, not hypothetical features.

Ask one independent highest-impact open question per message, with the current understanding and concrete options/consequences when useful. Use an available supported question tool. If unavailable, rejected, or unanswered, ask in ordinary chat and end the turn; an asynchronous question permits only independent read-only work while waiting. Silence, timeout, picker dismissal and defaults are not answers or delegation. Pause/cancel means stop and preserve the open ledger, not create a fallback plan.
After each answer update the ledger and select the next ambiguity/conflict; impose no fixed question count.

The gate closes only when no behavior-affecting decision is open, goals/non-goals and applicable risks are settled, and important outcomes have observable checks. Present the complete summary, delegated choices and exclusions; obtain explicit confirmation before writing the plan. This approves requirements only, not execution or role switching. Use a header such as `요구사항 확인`, never `Planner Handoff` here.
At any stage, corrections reopen only affected decisions/checks and require revised-summary confirmation. At pauses/resume/compression recover the ledger and approval from conversation evidence, never an old plan alone. Record additions beyond the literal request as confirmed/delegated/excluded in the intent diff.
