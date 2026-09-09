fn audit_async_question(item: &str, questions: Value) -> Value {
    json!({"threadId": "main-thread", "turnId": "live-turn", "item": {
        "id": item, "type": "agentMessage", "delivery": "async", "questions": questions
    }})
}

#[test]
fn esc_approval_declines_independently_of_order_and_selection() {
    for (method, params, expected) in [
        ("item/commandExecution/requestApproval", json!({"claudePermission": true}), json!({"decision": "decline"})),
        ("item/commandExecution/requestApproval", json!({}), json!({"decision": "decline"})),
        ("item/commandExecution/requestApproval", json!({"availableDecisions": ["decline", "acceptForSession", "accept", "cancel"]}), json!({"decision": "decline"})),
        ("item/commandExecution/requestApproval", json!({"availableDecisions": ["cancel", "accept"]}), json!({"decision": "cancel"})),
        ("item/fileChange/requestApproval", json!({}), json!({"decision": "decline"})),
        ("item/permissions/requestApproval", json!({"permissions": {"network": {"enabled": true}}}), json!({"permissions": {}, "scope": "turn"})),
    ] {
        for move_selection in [false, true] {
            let mut state = busy_state_with_live_turn();
            state.begin_server_request(json!(41), method, &params);
            if move_selection { state.handle_key(KeyEvent::from(KeyCode::Down)); }
            assert!(matches!(state.handle_key(KeyEvent::from(KeyCode::Esc)),
                Action::RpcResponse { id, result } if id == json!(41) && result == expected), "{method}: {params}");
            assert!(!state.awaiting_input());
            assert!(!state.turn_interrupted, "거절이 전체 작업 중단으로 바뀜");
        }
    }
    let mut state = busy_state_with_live_turn();
    state.begin_server_request(json!(41), "item/commandExecution/requestApproval", &json!({"availableDecisions": ["accept"]}));
    assert!(matches!(state.handle_key(KeyEvent::from(KeyCode::Esc)), Action::RpcError { .. }));
}

#[test]
fn esc_mcp_approval_never_persists_permission() {
    for (meta, expected) in [
        (json!({"persist": ["session", "always"]}), "decline"),
        (json!({"codex_approval_kind": "mcp_tool_call", "persist": ["session", "always"]}), "cancel"),
    ] {
        let mut state = busy_state_with_live_turn();
        state.begin_server_request(json!(42), "mcpServer/elicitation/request", &json!({
            "message": "도구 실행 허용", "_meta": meta
        }));
        state.handle_key(KeyEvent::from(KeyCode::Down));
        let Action::RpcResponse { id, result } = state.handle_key(KeyEvent::from(KeyCode::Esc)) else { panic!("거절 응답 없음") };
        assert_eq!(id, json!(42));
        assert_eq!(result, mcp_elicitation_response(expected, None));
        assert!(!state.awaiting_input());
    }
}

#[test]
fn esc_queue_waits_for_completion_then_runs_once_in_order() {
    for model in ["claude:opus", "gpt-5.6-sol"] {
        let mut state = AppState::new("thread".into(), "cwd".into(), "account".into(), Vec::new(), model, None);
        state.set_turn_started("turn".into());
        for text in ["다음 요청", "그다음 요청"] {
            state.editor.set_text(text);
            state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        }
        state.editor.set_text("미전송 초안");
        state.handle_notification("item/agentMessage/delta", &json!({"itemId": "answer", "delta": "남겨야 할 응답"}));
        assert!(matches!(state.handle_key(KeyEvent::from(KeyCode::Esc)), Action::Interrupt));
        assert!(state.take_queued_prompt().is_none(), "중단 확인 전에 후속 요청 실행");
        assert_eq!(state.editor.text(), "미전송 초안");
        state.handle_notification("turn/completed", &json!({"turn": {"id": "turn", "status": "interrupted"}}));
        let first = state.take_queued_prompt().expect("ESC 후 대기열이 멈춤");
        assert_eq!(first, "다음 요청");
        assert!(!state.busy);
        assert!(state.drain_committed().iter().any(|block| block.body == "남겨야 할 응답"));
        assert!(matches!(state.start_queued_prompt(first), Action::Submit(_)));
        assert!(state.take_queued_prompt().is_none());
        state.set_turn_started("next".into());
        state.handle_notification("turn/completed", &json!({"turn": {"id": "next", "status": "completed"}}));
        assert_eq!(state.take_queued_prompt().as_deref(), Some("그다음 요청"));
        assert!(state.take_queued_prompt().is_none());
        assert_eq!(state.editor.text(), "미전송 초안");
    }
}

#[test]
fn esc_queue_ignores_old_or_other_thread_completion_after_next_turn_starts() {
    for completed in [
        json!({"threadId": "main-thread", "turn": {"id": "live-turn", "status": "interrupted"}}),
        json!({"threadId": "other-thread", "turn": {"id": "other-turn", "status": "completed"}}),
    ] {
        let mut state = busy_state_with_live_turn();
        state.queued_prompts.extend(["첫 후속 요청".into(), "두 번째 후속 요청".into()]);
        state.handle_key(KeyEvent::from(KeyCode::Esc));
        state.handle_notification("turn/completed", &json!({"turn": {"id": "live-turn", "status": "interrupted"}}));
        let next = state.take_queued_prompt().unwrap();
        assert!(matches!(state.start_queued_prompt(next), Action::Submit(_)));
        state.set_turn_started("next-turn".into());
        // Match the event loop: it tries to drain after every completed notice,
        // including one the state rejected as stale or from another thread.
        state.handle_notification("turn/completed", &completed);
        assert!(state.take_queued_prompt().is_none(), "실행 중인 다음 턴에 후후속 요청이 끼어듦");
        assert_eq!(state.queued_prompts.len(), 1);
        assert_eq!(state.turn_id.as_deref(), Some("next-turn"));
    }
}

#[test]
fn queued_prompt_waits_for_normal_completion_behind_paced_text() {
    let mut state = busy_state_with_live_turn();
    state.queued_prompts.push_back("다음 요청".into());
    state.handle_notification("item/agentMessage/delta", &json!({"itemId": "answer", "delta": "남겨야 할 응답"}));
    assert!(state.take_queued_prompt().is_none());
    state.handle_notification("turn/completed", &json!({"turn": {"id": "live-turn", "status": "completed"}}));
    let next = state.take_queued_prompt().expect("완료 알림이 보류돼 대기열이 멈춤");
    assert!(matches!(state.start_queued_prompt(next), Action::Submit(_)), "끝난 턴으로 추가 입력을 보냄");
}

#[test]
fn esc_queue_does_not_send_or_consume_draft_attachments() {
    let mut state = busy_state_with_live_turn();
    state.queued_prompts.push_back("다음 요청".into());
    state.editor.set_text("아직 보내지 않은 초안");
    state.attach_local_image("C:/private/unsent.png".into());
    let draft = state.editor.text();
    state.handle_key(KeyEvent::from(KeyCode::Esc));
    state.handle_notification("turn/completed", &json!({"turn": {"id": "live-turn", "status": "interrupted"}}));
    let next = state.take_queued_prompt().unwrap();
    let Action::Submit(text) = state.start_queued_prompt(next) else { panic!("후속 요청 시작 실패") };
    let input = state.turn_input(text);
    assert_eq!(input.len(), 1, "대기 요청에 미전송 초안의 이미지가 섞임");
    assert_eq!(state.composer_image_count(), 1);
    assert_eq!(state.editor.text(), draft);
    state.set_turn_started("next-turn".into());
    let Action::Steer(text) = state.submit_editor() else { panic!("초안 전송 실패") };
    assert!(state.turn_input(text).iter().any(|item| item["type"] == "localImage"));
    assert_eq!(state.composer_image_count(), 0);
}

#[test]
fn queued_input_resolves_its_mentions_without_taking_draft_bindings() {
    let mut state = busy_state_with_live_turn();
    state.update_apps(&json!({"data": [{"id": "calendar", "name": "Calendar", "isAccessible": true, "isEnabled": true}]}));
    state.queued_prompts.push_back("$calendar 대기 요청".into());
    state.editor.set_text("$calendar 미전송 초안");
    state.selected_completion_bindings.push(SelectedCompletionBinding {
        sigil: '$', trigger: "calendar".into(), token: "$calendar".into(), range: 0..9,
        kind: CompletionKind::App, name: "초안 전용 선택".into(), path: "app://draft-only".into(),
    });
    state.handle_key(KeyEvent::from(KeyCode::Esc));
    state.handle_notification("turn/completed", &json!({"turn": {"id": "live-turn", "status": "interrupted"}}));
    let next = state.take_queued_prompt().unwrap();
    let Action::Submit(text) = state.start_queued_prompt(next) else { panic!("후속 요청 시작 실패") };
    let input = state.turn_input(text);
    assert!(input.iter().all(|item| item["path"] != "app://draft-only"));
    assert!(input.iter().any(|item| item["type"] == "mention"));
    assert_eq!(state.selected_completion_bindings.len(), 1);
    assert_eq!(state.editor.text(), "$calendar 미전송 초안");
}

#[test]
fn esc_queue_does_not_resume_after_failures() {
    for failure in ["interrupt", "start", "answer", "turn", "malformed", "error"] {
        let mut state = busy_state_with_live_turn();
        state.queued_prompts.push_back("다음 요청".into());
        state.handle_key(KeyEvent::from(KeyCode::Esc));
        match failure {
            "interrupt" => state.set_interrupt_failed("중단 실패"),
            "start" => state.set_request_failed("시작 실패"),
            "answer" => state.restore_failed_question_response(&json!({"answers": {"q": {"answers": ["보관할 답"]}}})),
            "malformed" => { state.begin_server_request(json!(1), "item/tool/requestUserInput", &json!({"questions": []})); }
            "error" => state.handle_notification("error", &json!({"error": {"message": "실행 오류"}, "willRetry": false})),
            _ => {}
        }
        state.handle_notification("turn/completed", &json!({"turn": {
            "id": "live-turn", "status": if failure == "turn" { "failed" } else { "interrupted" }
        }}));
        state.flush_before_question();
        assert!(state.take_queued_prompt().is_none(), "{failure}");
        assert_eq!(state.queued_prompts.len(), 1);
    }
}

#[test]
fn esc_during_start_resumes_queue_only_after_the_deferred_stop() {
    let mut state = test_state();
    state.editor.set_text("시작할 요청");
    state.submit_editor();
    state.queued_prompts.push_back("다음 요청".into());
    assert!(state.note_interrupt_while_sending());
    assert!(state.take_queued_prompt().is_none());
    state.set_turn_started("turn".into());
    assert_eq!(state.take_pending_interrupt().as_deref(), Some("turn"));
    assert!(state.take_queued_prompt().is_none());
    state.handle_notification("turn/completed", &json!({"turn": {"id": "turn", "status": "interrupted"}}));
    assert_eq!(state.take_queued_prompt().as_deref(), Some("다음 요청"));
}

#[test]
fn esc_question_keeps_one_cancellation_record_without_answering() {
    for asynchronous in [false, true] {
        for stopped in [false, true] {
            let mut state = busy_state_with_live_turn();
            if asynchronous {
                state.reject_unanswered_question("item/completed", &audit_async_question("question", json!([
                    {"title": "어느 색인가요?", "options": ["빨강", "파랑"]},
                    {"title": "이유를 적어 주세요"}
                ])));
                if stopped { audit_stop_question_turn(&mut state); }
            } else {
                state.begin_server_request(json!(7), "item/tool/requestUserInput", &json!({"questions": [
                    {"id": "q1", "question": "어느 색인가요?", "options": [{"label": "빨강"}, {"label": "파랑"}]},
                    {"id": "q2", "question": "이유를 적어 주세요"}
                ]}));
            }
            state.drain_committed();
            let action = state.handle_key(KeyEvent::from(KeyCode::Esc));
            assert!(matches!(action, Action::CancelUserInput { .. } | Action::None));
            assert!(!state.awaiting_input());
            let records = state.drain_committed().into_iter()
                .filter(|block| block.title == "질문 답변 취소").collect::<Vec<_>>();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].title, "질문 답변 취소");
            assert_eq!(records[0].body, "어느 색인가요? (빨강 / 파랑)\n이유를 적어 주세요");
            assert!(matches!(records[0].kind, BlockKind::Warning));
            state.handle_key(KeyEvent::from(KeyCode::Esc));
            state.handle_notification("turn/completed", &json!({"turn": {"id": "live-turn", "status": "interrupted"}}));
            state.flush_before_question();
            assert!(state.drain_committed().iter().all(|block| block.title != "질문 답변 취소"));
        }
    }
}

fn audit_stop_question_turn(state: &mut AppState) -> Option<Action> {
    let params = json!({"threadId": "main-thread", "turn": {"id": "live-turn", "status": "interrupted"}});
    let action = state.reject_unanswered_question("turn/completed", &params);
    if action.is_none() { state.handle_notification("turn/completed", &params); }
    state.flush_before_question();
    action
}

#[test]
fn audit_duplicate_async_question_preserves_typed_answer() {
    let mut state = busy_state_with_live_turn();
    let params = audit_async_question("question-1", json!([{"title": "입력하세요"}]));
    state.reject_unanswered_question("item/completed", &params);
    state.handle_paste("작성 중인 답변");
    assert!(matches!(state.reject_unanswered_question("item/completed", &params), Some(Action::None)));
    assert!(state.awaiting_input());
    let Some(PendingInteraction::UserInput { editor, .. }) = &state.pending else { panic!("질문이 사라짐") };
    assert_eq!(editor.text(), "작성 중인 답변");
}

#[test]
fn audit_async_answer_does_not_send_draft_attachments_or_activate_mentions() {
    let mut state = busy_state_with_live_turn();
    state.editor.set_text("전송하지 않은 초안");
    state.attach_local_image("C:/private/draft.png".into());
    state.update_apps(&json!({"data": [{"id": "calendar", "name": "Calendar", "isAccessible": true, "isEnabled": true}]}));
    let draft = state.editor.text();
    let params = audit_async_question("question-1", json!([{"title": "$calendar 관련 선택", "options": ["예", "아니오"]}]));
    state.reject_unanswered_question("item/completed", &params);
    audit_stop_question_turn(&mut state);
    let Action::Submit(text) = state.click_overlay_row(1) else { panic!("답변 제출 실패") };
    let input = state.turn_input(text);
    assert_eq!(input.len(), 1, "질문 답변에 초안 첨부나 도구 호출이 섞임");
    assert_eq!(input[0]["type"], "text");
    assert_eq!(state.composer_image_count(), 1);
    assert_eq!(state.editor.text(), draft);
}

#[test]
fn audit_async_answer_start_failure_preserves_answer_and_pauses_queue() {
    let mut state = busy_state_with_live_turn();
    state.editor.set_text("기존 초안");
    state.queued_prompts.push_back("나중 작업".into());
    state.reject_unanswered_question("item/completed", &audit_async_question("question-1", json!([{"title": "입력하세요"}])));
    audit_stop_question_turn(&mut state);
    state.handle_paste("잃으면 안 되는 답변");
    let Action::Submit(text) = state.handle_key(KeyEvent::from(KeyCode::Enter)) else { panic!("답변 제출 실패") };
    state.turn_input(text);
    state.set_request_failed("시험 전송 실패");
    assert!(state.editor.text().contains("기존 초안"));
    assert!(state.editor.text().contains("잃으면 안 되는 답변"), "전송 실패로 답변 유실");
    assert!(state.take_queued_prompt().is_none(), "답변 전송 실패 뒤 다른 작업 시작");
}

#[test]
fn audit_duplicate_native_request_does_not_cancel_the_question() {
    let mut state = busy_state_with_live_turn();
    let params = blocking_test_question();
    state.begin_server_request(json!(9), "item/tool/requestUserInput", &params);
    assert!(matches!(state.begin_server_request(json!(9), "item/tool/requestUserInput", &params), Action::None));
    assert!(state.awaiting_input());
}

#[test]
fn audit_native_response_failure_keeps_all_answers_and_pauses_queue() {
    let mut state = busy_state_with_live_turn();
    state.editor.set_text("기존 초안");
    state.queued_prompts.push_back("후속 작업".into());
    let result = json!({"answers": {"q1": {"answers": ["첫 답변"]}, "q2": {"answers": ["둘째 답변"]}}});
    state.restore_failed_question_response(&result);
    assert_eq!(state.editor.text(), "기존 초안\n첫 답변\n둘째 답변");
    assert!(state.take_queued_prompt().is_none());
    state.fallback_from_codex("응답 직후 연결 종료");
    assert!(state.take_queued_prompt().is_none());
}

#[test]
fn audit_async_multiple_questions_require_explicit_complete_answers() {
    for mouse in [false, true] {
        let mut state = busy_state_with_live_turn();
        let params = audit_async_question("question-1", json!([
            {"title": "첫 질문", "options": ["첫 답", "다른 답"]},
            {"title": "둘째 질문", "options": null},
            {"title": "셋째 질문", "options": ["예", "아니오"]}
        ]));
        state.reject_unanswered_question("item/completed", &params);
        audit_stop_question_turn(&mut state);
        let action = if mouse { state.click_overlay_row(1) } else { state.handle_key(KeyEvent::from(KeyCode::Enter)) };
        assert!(matches!(action, Action::None));
        assert!(matches!(state.handle_key(KeyEvent::from(KeyCode::Enter)), Action::None));
        state.handle_paste("직접 입력\n둘째 줄");
        assert!(matches!(state.handle_key(KeyEvent::from(KeyCode::Enter)), Action::None));
        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT, KeyModifiers::SHIFT] {
            assert!(matches!(state.handle_key(KeyEvent::new(KeyCode::Enter, modifiers)), Action::None));
            assert!(state.awaiting_input());
        }
        let action = if mouse { state.click_overlay_row(2) } else { state.handle_key(KeyEvent::from(KeyCode::Char('2'))) };
        let Action::Submit(text) = action else { panic!("여러 질문 답변 제출 실패") };
        for answer in ["첫 답", "직접 입력", "둘째 줄", "아니오"] { assert!(text.contains(answer)); }
        assert_eq!(state.turn_input(text).len(), 1);
    }
}

#[test]
fn audit_stale_async_question_cannot_stop_a_new_turn() {
    let mut state = busy_state_with_live_turn();
    state.set_turn_started("new-turn".into());
    assert!(matches!(state.reject_unanswered_question("item/completed", &audit_async_question("old-question", json!([{"title": "이전 질문"}]))), Some(Action::None)));
    assert!(!state.awaiting_input());
    assert!(!state.turn_interrupted);
    assert_eq!(state.turn_id.as_deref(), Some("new-turn"));
}

#[test]
fn audit_different_async_question_cannot_erase_the_existing_answer() {
    let mut state = busy_state_with_live_turn();
    state.reject_unanswered_question("item/completed", &audit_async_question("question-1", json!([{"title": "첫 질문"}])));
    state.handle_paste("남겨야 할 답변");
    let action = state.reject_unanswered_question("item/completed", &audit_async_question("question-2", json!([{"title": "다른 질문"}])));
    assert!(matches!(action, Some(Action::CancelUserInput { .. })));
    assert!(state.awaiting_input());
    let Some(PendingInteraction::UserInput { editor, .. }) = &state.pending else { panic!("첫 질문 유실") };
    assert_eq!(editor.text(), "남겨야 할 답변");
}

#[test]
fn audit_early_async_confirmation_is_sent_once_after_stop() {
    for cancel in [false, true] {
        let mut state = busy_state_with_live_turn();
        state.reject_unanswered_question("item/completed", &audit_async_question("question-1", json!([{"title": "선택", "options": ["선택한 답", "다른 답"]}])));
        assert!(matches!(state.click_overlay_row(1), Action::None));
        assert!(state.awaiting_input());
        state.handle_paste("끼어든 글자");
        assert!(matches!(state.click_overlay_row(2), Action::None));
        if cancel { state.handle_key(KeyEvent::from(KeyCode::Esc)); }
        let action = audit_stop_question_turn(&mut state);
        if cancel {
            assert!(!matches!(action, Some(Action::Submit(_))));
            assert!(!state.awaiting_input());
        } else {
            let Some(Action::Submit(text)) = action else { panic!("확인한 답변이 중단 완료 후에도 전송되지 않음") };
            assert!(text.contains("선택한 답"));
            assert!(!text.contains("다른 답"));
            assert!(!text.contains("끼어든 글자"));
            state.turn_input(text);
            assert!(!state.awaiting_input());
        }
    }
}

#[test]
fn audit_disconnect_after_early_confirmation_preserves_answer_once() {
    let mut state = busy_state_with_live_turn();
    state.reject_unanswered_question("item/completed", &audit_async_question("question-1", json!([{"title": "입력하세요"}])));
    state.handle_paste("보관할 답변");
    state.handle_key(KeyEvent::from(KeyCode::Enter));
    state.fallback_from_codex("연결 종료");
    assert!(!state.awaiting_input());
    assert_eq!(state.editor.text().matches("보관할 답변").count(), 1);
    assert!(state.take_queued_prompt().is_none());
}

#[test]
fn audit_late_duplicate_async_question_cannot_reopen_while_answer_starts() {
    let mut state = busy_state_with_live_turn();
    let params = audit_async_question("question-1", json!([{"title": "선택", "options": ["예", "아니오"]}]));
    state.reject_unanswered_question("item/completed", &params);
    audit_stop_question_turn(&mut state);
    let Action::Submit(text) = state.click_overlay_row(1) else { panic!("답변 제출 실패") };
    assert!(state.turn_id.is_none());
    assert!(matches!(state.reject_unanswered_question("item/completed", &params), Some(Action::None)));
    assert!(!state.awaiting_input());
    assert!(!state.pending_interrupt);
    assert_eq!(state.turn_input(text).len(), 1);
}

#[test]
fn permission_display_tracks_effective_profile_without_lowering_next_preference() {
    let mut state = busy_state_with_live_turn();
    state.handle_notification("devez/permissions/updated", &json!({"threadId": "main-thread", "profile": ":workspace", "lowered": true}));
    assert_eq!(state.permission_mode(), PermissionMode::Workspace);
    assert_eq!(state.permission_profile(), ":danger-full-access");
    state.show_status();
    assert!(state.committed.last().unwrap().body.contains("작업 폴더 수정"));
    state.handle_notification("devez/permissions/updated", &json!({"threadId": "different-thread", "profile": ":read-only", "lowered": true}));
    assert_eq!(state.permission_mode(), PermissionMode::Workspace);
}
