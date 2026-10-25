#[test]
fn codex_audit_queued_and_direct_steers_preserve_read_only_roles() {
    let mut state = test_state();
    let input = vec![json!({"type": "text", "text": "추가 요청"})];
    for mode in [AgentMode::Reviewer, AgentMode::Planner, AgentMode::Standard] {
        state.set_agent_mode(mode);
        let params = crate::steer_turn_params(&state, "live-turn", input.clone(), true);
        assert_eq!(params["threadId"], state.thread_id);
        assert_eq!(params["expectedTurnId"], "live-turn");
        assert_eq!(params["input"], json!(input));
        assert_eq!(params.get("toolPolicy"), mode.tool_policy().as_ref());
        assert!(crate::steer_turn_params(&state, "live-turn", input.clone(), false).get("toolPolicy").is_none());
    }
}

#[test]
fn codex_audit_local_model_picker_yields_to_a_real_question() {
    let mut state = busy_state_with_live_turn();
    state.run_slash_command("/model");
    assert!(state.pending.is_some());
    let action = state.begin_server_request(
        json!(41),
        "item/tool/requestUserInput",
        &blocking_test_question(),
    );
    assert!(matches!(action, Action::None));
    assert!(
        matches!(state.pending.as_ref(), Some(PendingInteraction::UserInput { id, .. }) if *id == json!(41))
    );
    assert!(!state.turn_interrupted);
}

#[test]
fn codex_audit_disconnect_follows_the_running_provider_after_a_picker_change() {
    for codex_running in [false, true] {
        let mut state = busy_state_with_live_turn();
        state
            .models
            .push(test_model("claude:sonnet", "Claude Sonnet", true));
        if codex_running {
            state.active_turn_model = Some("gpt-5.6-sol".to_owned());
            state.switch_provider(ModelProvider::Claude);
        } else {
            state.active_turn_model = Some("claude:sonnet".to_owned());
            state.begin_server_request(
                json!({"backend": "claude", "id": "question"}),
                "item/tool/requestUserInput",
                &blocking_test_question(),
            );
        }
        state.fallback_from_codex("시험용 코덱스 연결 종료");
        assert_eq!(
            state.busy, !codex_running,
            "선택한 모델로 실행 중인 제공자를 오판함"
        );
        assert_eq!(state.turn_id.is_some(), !codex_running);
        assert_eq!(state.awaiting_input(), !codex_running);
    }
}

#[test]
fn codex_audit_disconnect_closes_every_unanswerable_approval_card() {
    for method in [
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
        "item/permissions/requestApproval",
        "mcpServer/elicitation/request",
    ] {
        let mut state = busy_state_with_live_turn();
        state.begin_server_request(json!(41), method, &json!({"message": "허용하세요"}));
        assert!(state.awaiting_input(), "{method}");
        state.fallback_from_codex("시험용 코덱스 연결 종료");
        assert!(
            !state.awaiting_input(),
            "답할 수 없는 승인 창이 남음: {method}"
        );
        assert!(!state.busy);
    }
}

#[test]
fn codex_audit_logout_replaces_the_previous_identity() {
    let mut state = test_state();
    state.set_account("old@example.com".into());
    let label = crate::codex_account_label(&json!({"account": null, "requiresOpenaiAuth": true}));
    crate::apply_account(&mut state, (Some(label), None));
    assert_eq!(state.account, "signed out");
    assert_eq!(
        crate::codex_account_label(&json!({"account": null, "requiresOpenaiAuth": false})),
        "Local provider"
    );
    assert_eq!(
        crate::codex_account_label(
            &json!({"account": {"type": "chatgpt", "email": "new@example.com"}})
        ),
        "new@example.com"
    );
}

#[test]
fn codex_audit_resume_restores_the_running_turn_and_escape() {
    let mut state = test_state();
    state.load_history(
        &json!({
            "status": {"type": "active", "activeFlags": []},
            "turns": [{"id": "running", "status": "inProgress", "items": []}]
        }),
        None,
    );
    assert!(state.busy, "실행 중인 세션이 유휴 상태로 복원됨");
    assert_eq!(state.turn_id.as_deref(), Some("running"));
    assert!(matches!(
        state.handle_key(KeyEvent::from(KeyCode::Esc)),
        Action::Interrupt
    ));
}

#[test]
fn codex_audit_resume_does_not_reopen_an_older_running_turn() {
    let mut state = test_state();
    state.load_history(
        &json!({"turns": [
            {"id": "old", "status": "inProgress", "items": []},
            {"id": "last", "status": "completed", "items": []}
        ]}),
        None,
    );
    assert!(!state.busy);
    assert!(state.turn_id.is_none());
}

#[test]
fn codex_audit_current_child_questions_are_still_accepted() {
    let mut state = test_state();
    spawn_codex_subagent(&mut state, "child");
    let action = state.begin_server_request(json!(51), "item/tool/requestUserInput", &json!({
        "threadId": "child", "questions": [{"id": "q", "header": "선택", "question": "선택하세요", "options": []}]
    }));
    assert!(matches!(action, Action::None));
    assert!(state.awaiting_input());
    assert!(!state.turn_interrupted);
}

#[test]
fn codex_audit_foreign_requests_preserve_the_current_question_and_turn() {
    for method in [
        "item/tool/requestUserInput",
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
        "item/permissions/requestApproval",
        "mcpServer/elicitation/request",
    ] {
        let mut state = busy_state_with_live_turn();
        state.begin_server_request(json!(41), "item/tool/requestUserInput", &json!({
            "threadId": state.thread_id, "questions": [{"id": "q", "header": "선택", "question": "선택하세요", "options": []}]
        }));
        let action = state.begin_server_request(json!(42), method, &json!({
            "threadId": "other-thread", "questions": [{"id": "foreign", "header": "선택", "question": "다른 세션 질문", "options": []}]
        }));
        if method == "mcpServer/elicitation/request" {
            assert!(matches!(action, Action::RpcResponse { id, result }
                if id == json!(42) && result == mcp_elicitation_response("cancel", None)));
        } else {
            assert!(matches!(action, Action::RpcError { id, .. } if id == json!(42)), "{method}");
        }
        assert!(
            matches!(state.pending.as_ref(), Some(PendingInteraction::UserInput { id, .. }) if *id == json!(41))
        );
        assert!(
            !state.turn_interrupted,
            "다른 세션의 질문 때문에 현재 작업이 중단됨: {method}"
        );
        assert_eq!(state.turn_id.as_deref(), Some("live-turn"));
    }
}

#[test]
fn codex_audit_failed_file_changes_are_warnings_in_live_and_restored_history() {
    for status in ["failed", "declined"] {
        let item = json!({"id": "patch", "type": "fileChange", "status": status,
            "changes": [{"path": "file.txt", "kind": {"type": "update", "move_path": null}, "diff": "-old\n+new"}]});
        let mut state = test_state();
        state.handle_notification("item/started", &json!({"item": item}));
        state.handle_notification("item/completed", &json!({"item": item}));
        let blocks = state.drain_committed();
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block.kind, BlockKind::Warning)),
            "실패가 변경 성공으로 표시됨: {status}"
        );
        assert!(
            blocks
                .iter()
                .all(|block| !matches!(block.kind, BlockKind::FileChange))
        );
        let mut restored = test_state();
        restored.load_history(
            &json!({"turns": [{"status": "completed", "items": [item]}]}),
            None,
        );
        assert!(
            restored
                .committed
                .iter()
                .any(|block| matches!(block.kind, BlockKind::Warning)),
            "복원한 실패가 변경 성공으로 표시됨: {status}"
        );
    }
}
