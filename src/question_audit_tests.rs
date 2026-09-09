fn audit_async_question(item: &str, questions: Value) -> Value {
    json!({"threadId": "main-thread", "turnId": "live-turn", "item": {
        "id": item, "type": "agentMessage", "delivery": "async", "questions": questions
    }})
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
