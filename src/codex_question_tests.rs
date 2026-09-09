//! Explicit, opt-in checks against the installed Codex and the real model.
//! cargo test live_codex_question -- --ignored --nocapture --test-threads=1
use super::apply_codex_question_mode;
use crate::{
    app_server::{AppServer, ServerEvent},
    state::{Action, AppState},
};
use crossterm::event::{KeyCode, KeyEvent};
use futures_util::FutureExt;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::{Instant, timeout};

#[tokio::test]
#[ignore = "실제 Codex 로그인과 네 모델 호출 필요"]
async fn live_codex_natural_question_delivery() {
    let results = futures_util::stream::iter(["gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"])
        .map(|model| async move {
            let mut server = AppServer::spawn(Path::new("codex"), None).await.unwrap();
            let result = std::panic::AssertUnwindSafe(async {
                server.initialize().await.unwrap();
                let response = server.request("thread/start", json!({
                    "model": model, "ephemeral": true, "cwd": std::env::temp_dir(),
                    "approvalPolicy": "never", "permissions": ":read-only",
                    "developerInstructions": crate::DEVEZ_INSTRUCTIONS
                })).await.unwrap();
                assert_eq!(response["model"], model);
                let mut state = AppState::new(response["thread"]["id"].as_str().unwrap().into(),
                    std::env::temp_dir().to_string_lossy().into(), "시험".into(), Vec::new(), model, Some("low"));
                start(&server, &mut state, model,
                    "화면 색상을 밝게 또는 어둡게 중 제가 고르게 질문해 주세요. 제가 답하기 전에는 최종 답변을 하지 말고, 답변을 받은 뒤 그 값만 말하세요. 목록 이외의 문자열을 직접 입력해도 그 문자열을 바꾸지 말고 그대로 최종 답변하세요. 파일이나 셸 작업은 필요 없습니다.").await;
                let mut native = false;
                let mut async_question_seen = false;
                loop {
                    match timeout(Duration::from_secs(120), server.next_event()).await.unwrap().unwrap() {
                        ServerEvent::Request { id, method, params } => {
                            assert_eq!(method, "item/tool/requestUserInput");
                            assert!(matches!(state.begin_server_request(id, &method, &params), Action::None));
                            native = true;
                            break;
                        }
                        ServerEvent::Notification { method, params } => {
                            match state.reject_unanswered_question(&method, &params) {
                                Some(Action::Interrupt) => {
                                    async_question_seen = true;
                                    server.request("turn/interrupt", json!({"threadId": state.thread_id, "turnId": state.turn_id})).await.unwrap();
                                }
                                Some(Action::None) => {}
                                Some(_) => panic!("질문 연결 실패: {model}"),
                                None => state.handle_notification(&method, &params),
                            }
                            if method == "turn/completed" {
                                assert!(async_question_seen, "질문 없이 종료됨: {model}");
                                break;
                            }
                        }
                        ServerEvent::Closed(message) => panic!("{message}"),
                        _ => {}
                    }
                }
                assert!(state.awaiting_input());
                hold_question(&mut server, &mut state, &std::env::temp_dir().join("devez-natural-question-no-file"), 5).await;
                for _ in 0..20 {
                    if state.pending_text_input_target().is_some() { break; }
                    state.handle_key(KeyEvent::from(KeyCode::Down));
                }
                assert!(state.pending_text_input_target().is_some(), "직접 입력 칸으로 이동하지 못함: {model}");
                let answer = format!("답변_{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos());
                state.handle_paste(&answer);
                match state.handle_key(KeyEvent::from(KeyCode::Enter)) {
                    Action::RpcResponse { id, result } => {
                        assert!(native);
                        assert!(result.to_string().contains(&answer), "실제 전송할 답변이 다름: {model} {result}");
                        server.respond(id, result).unwrap();
                    }
                    Action::Submit(text) => { assert!(!native); start(&server, &mut state, model, &text).await; }
                    _ => panic!("실제 질문 답변 제출 실패: {model}"),
                }
                let response = finish(&mut server, &mut state, false).await;
                assert!(response.contains(&answer), "답변이 최종 응답에 없음: {model} {response}");
                println!("검증 통과: {model} 실제 지침·자연어 질문·직접 입력·최종 응답");
            }).catch_unwind().await;
            server.shutdown().await;
            (model, result)
        }).buffer_unordered(4).collect::<Vec<_>>().await;
    let failed = results.iter().filter(|(_, result)| result.is_err()).map(|(model, _)| *model).collect::<Vec<_>>();
    assert!(failed.is_empty(), "자연어 질문 검증 실패: {failed:?}");
}

#[tokio::test]
#[ignore = "실제 Codex 로그인과 모델 호출 필요"]
async fn live_codex_async_question_recovery() {
    let model = "gpt-6-astra";
    let mut server = AppServer::spawn(Path::new("codex"), None).await.unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        server.initialize().await.unwrap();
        let response = server.request("thread/start", json!({
            "model": model, "cwd": std::env::temp_dir(),
            "approvalPolicy": "never", "permissions": ":read-only",
            "developerInstructions": "질문 연결 시험입니다. 파일·셸·검색·하위 에이전트를 사용하지 마세요."
        })).await.unwrap();
        let mut state = AppState::new(
            response["thread"]["id"].as_str().unwrap().into(),
            std::env::temp_dir().to_string_lossy().into(), "시험".into(),
            Vec::new(), model, Some("low"),
        );
        server.request("turn/start", json!({
            "threadId": state.thread_id, "model": model, "effort": "low",
            "collaborationMode": {"mode": "default", "settings": {
                "model": model, "reasoning_effort": "low",
                "developer_instructions": "비동기 질문 복구를 검증합니다. 반드시 request_user_input_async로 첫째와 둘째 중 고르는 질문 하나를 보내세요. 다른 도구를 쓰지 마세요."
            }},
            "input": [{"type": "text", "text": "비동기 질문을 보내고 사용자 답변을 기다리세요. 답변을 받으면 그 값만 말하세요."}]
        })).await.unwrap();
        let mut question_seen = false;
        let mut free_text = false;
        loop {
            let event = timeout(Duration::from_secs(120), server.next_event()).await.unwrap().unwrap();
            match event {
                ServerEvent::Notification { method, params } => {
                    if let Some(action) = state.reject_unanswered_question(&method, &params) {
                        if matches!(action, Action::None) {
                            assert_eq!(method, "turn/completed");
                            break;
                        }
                        assert!(matches!(action, Action::Interrupt), "예상하지 않은 복구 동작");
                        assert!(state.awaiting_input());
                        free_text = params.pointer("/item/questions/0/options")
                            .and_then(Value::as_array).is_none_or(Vec::is_empty);
                        question_seen = true;
                        server.request("turn/interrupt", json!({"threadId": state.thread_id, "turnId": state.turn_id})).await.unwrap();
                    } else {
                        state.handle_notification(&method, &params);
                    }
                    if method == "turn/completed" {
                        assert!(question_seen, "비동기 질문 없이 종료됨");
                        break;
                    }
                }
                ServerEvent::Request { method, .. } => panic!("예상하지 않은 요청: {method}"),
                ServerEvent::Closed(message) => panic!("{message}"),
                _ => {}
            }
        }
        // The interrupted runtime remains idle; the local question stays open.
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            state.render_tick();
            assert!(state.awaiting_input());
            assert!(state.take_queued_prompt().is_none());
            if let Ok(Some(event)) = timeout(Duration::from_millis(100), server.next_event()).await {
                match event {
                    ServerEvent::Notification { method, params } => {
                        assert_ne!(method, "turn/started");
                        assert_ne!(method, "item/started");
                        state.handle_notification(&method, &params);
                    }
                    ServerEvent::Request { method, .. } => panic!("대기 중 요청: {method}"),
                    _ => {}
                }
            }
        }
        if !free_text {
            state.handle_key(KeyEvent::from(KeyCode::Char('3')));
        }
        state.handle_paste("첫째\n직접 입력한 둘째 줄".into());
        let Action::Submit(answer) = state.handle_key(KeyEvent::from(KeyCode::Enter)) else {
            panic!("비동기 질문 답변이 새 작업으로 전달되지 않음");
        };
        assert!(answer.contains("첫째"));
        start(&server, &mut state, model, &answer).await;
        let response = finish(&mut server, &mut state, false).await;
        assert!(response.contains("첫째"), "실제 최종 응답에 사용자 답변이 없음: {response}");
        let resumed = AppServer::spawn(Path::new("codex"), None).await.unwrap();
        std::mem::replace(&mut server, resumed).shutdown().await;
        server.initialize().await.unwrap();
        let history = server.request("thread/resume", json!({
            "threadId": state.thread_id
        })).await.unwrap();
        let mut restored = AppState::new(state.thread_id.clone(), state.cwd.clone(),
            "시험".into(), Vec::new(), model, Some("low"));
        restored.load_history(&history["thread"], None);
        let blocks = restored.drain_committed();
        assert!(blocks.iter().any(|block| block.body.contains("↳ 첫째\n직접 입력한 둘째 줄")),
            "저장된 대화에서 직접 입력 답변 상자를 복원하지 못함: {}", history["thread"]);
        server.request("thread/archive", json!({"threadId": state.thread_id})).await.unwrap();
    }).catch_unwind().await;
    server.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn event(server: &mut AppServer, state: &mut AppState) -> ServerEvent {
    let event = timeout(Duration::from_secs(120), server.next_event())
        .await
        .expect("실제 모델 응답 시간 초과")
        .expect("연결 종료");
    if let ServerEvent::Notification { method, params } = &event {
        assert!(
            state.reject_unanswered_question(method, params).is_none(),
            "예상하지 않은 질문 상태 변경: {method} {params}"
        );
        state.handle_notification(method, params);
    }
    event
}

async fn start(server: &AppServer, state: &mut AppState, model: &str, prompt: &str) {
    start_with_effort(server, state, model, "low", prompt).await;
}

async fn start_with_effort(
    server: &AppServer,
    state: &mut AppState,
    model: &str,
    effort: &str,
    prompt: &str,
) {
    let mut params = json!({
        "threadId": state.thread_id, "model": model, "effort": effort,
        "permissions": ":danger-full-access",
        "additionalContext": crate::turn_additional_context(state.vibe_mode(), state.agent_mode(), None),
        "input": state.turn_input(prompt.to_owned())
    });
    super::prepare_codex_turn_context(&mut params);
    super::apply_codex_tool_policy(&mut params);
    apply_codex_question_mode(&mut params).unwrap();
    server.request("turn/start", params).await.unwrap();
}

async fn await_question(
    server: &mut AppServer,
    state: &mut AppState,
    expect_plan_update: bool,
) -> Value {
    let mut plan_update_seen = false;
    loop {
        match event(server, state).await {
            ServerEvent::Request { id, method, params } => {
                assert_eq!(
                    method, "item/tool/requestUserInput",
                    "예상하지 않은 요청: {params}"
                );
                // Default mode labels the UI nonmodal, but the native tool
                // still waits for its RPC reply. Assert behavior below.
                assert!(
                    plan_update_seen || !expect_plan_update,
                    "선택 창 앞의 작업 목록 갱신이 실패함"
                );
                assert!(matches!(
                    state.begin_server_request(id.clone(), &method, &params),
                    Action::None
                ));
                assert!(state.awaiting_input());
                assert!(state.view().overlay.is_some());
                return id;
            }
            ServerEvent::Notification { method, params } => {
                if method == "turn/plan/updated" {
                    plan_update_seen = true;
                }
                assert_ne!(method, "turn/completed", "질문 없이 작업을 끝냄: {params}");
                if method == "item/started" {
                    assert!(
                        !matches!(
                            params.pointer("/item/type").and_then(Value::as_str),
                            Some("commandExecution" | "fileChange")
                        ),
                        "질문 전에 실행함: {params}"
                    );
                }
            }
            ServerEvent::Closed(message) | ServerEvent::ProtocolWarning(message) => {
                panic!("{message}")
            }
            _ => {}
        }
    }
}

async fn hold_question(server: &mut AppServer, state: &mut AppState, marker: &Path, seconds: u64) {
    let until = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < until {
        let remaining = until.saturating_duration_since(Instant::now());
        let Ok(event) = timeout(remaining, server.next_event()).await else {
            break;
        };
        match event.expect("답변 대기 중 연결 종료") {
            ServerEvent::Notification { method, params } => {
                if matches!(method.as_str(), "item/started" | "item/completed") {
                    assert!(
                        !matches!(
                            params.pointer("/item/type").and_then(Value::as_str),
                            Some("commandExecution" | "fileChange" | "agentMessage")
                        ),
                        "답변 전에 진행함: {params}"
                    );
                }
                assert!(
                    !matches!(
                        method.as_str(),
                        "turn/completed" | "item/agentMessage/delta" | "serverRequest/resolved"
                    ),
                    "답변 전에 진행함: {method} {params}"
                );
                state.handle_notification(&method, &params);
            }
            ServerEvent::Request { method, .. } => panic!("답변 대기 중 다른 요청: {method}"),
            ServerEvent::Closed(message) => panic!("{message}"),
            _ => {}
        }
    }
    assert!(!marker.exists(), "답변 전에 파일이 생성됨");
    state.render_tick();
    assert!(state.awaiting_input(), "질문이 저절로 닫힘");
}

async fn finish(server: &mut AppServer, state: &mut AppState, cancelled: bool) -> String {
    let mut response = String::new();
    loop {
        match event(server, state).await {
            ServerEvent::Notification { method, params } => {
                if method == "item/completed" && params.pointer("/item/type").and_then(Value::as_str) == Some("agentMessage") {
                    response.push_str(params.pointer("/item/text").and_then(Value::as_str).unwrap_or_default());
                }
                if cancelled {
                    assert_ne!(method, "item/agentMessage/delta", "취소 후 응답이 이어짐");
                    if method == "item/started" {
                        assert!(
                            !matches!(
                                params.pointer("/item/type").and_then(Value::as_str),
                                Some("commandExecution" | "fileChange")
                            ),
                            "취소 후 작업 실행: {params}"
                        );
                    }
                }
                if method == "turn/completed" {
                    assert_eq!(
                        params["turn"]["status"],
                        if cancelled {
                            "interrupted"
                        } else {
                            "completed"
                        },
                        "{params}"
                    );
                    return response;
                }
            }
            ServerEvent::Request { method, params, .. } => {
                panic!("예상하지 않은 후속 요청: {method} {params}")
            }
            ServerEvent::Closed(message) => panic!("{message}"),
            _ => {}
        }
    }
}

async fn exercise(model: &str) {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("devez-question-{model}-{stamp}"));
    std::fs::create_dir_all(&root).unwrap();
    let mut server = AppServer::spawn(Path::new("codex"), None).await.unwrap();
    let outcome = std::panic::AssertUnwindSafe(exercise_session(&mut server, model, &root, stamp))
        .catch_unwind()
        .await;
    server.shutdown().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    for name in ["choice.txt", "resumed.txt"] {
        std::fs::remove_file(root.join(name)).unwrap();
    }
    let _ = std::fs::remove_dir(&root);
}

async fn exercise_session(server: &mut AppServer, model: &str, root: &Path, stamp: u128) {
    server.initialize().await.unwrap();
    let response = server.request("thread/start", json!({
        "model": model, "ephemeral": true, "cwd": root,
        "approvalPolicy": "never", "permissions": ":danger-full-access",
        "developerInstructions": "질문 대기 검증용 세션입니다. 작업 목록·선택 질문과 요청한 파일만 현재 폴더에 쓰고 그 밖의 검색·하위 에이전트를 사용하지 마세요."
    })).await.unwrap();
    assert_eq!(response["model"], model, "다른 모델로 대체됨");
    let mut state = AppState::new(
        response["thread"]["id"].as_str().unwrap().into(),
        root.to_string_lossy().into(),
        "시험".into(),
        Vec::new(),
        model,
        Some("low"),
    );
    start(server, &mut state, model,
        "먼저 update_plan으로 '1. 선택 대기' 작업을 in_progress로 등록하세요. 이어서 request_user_input으로 첫째 또는 둘째를 고르는 질문 하나를 하세요. 답변 전에는 파일을 만들거나 다른 도구를 쓰지 마세요. 답변을 받으면 choice.txt에 사용자 답변을 그대로 적으세요. 직접 입력 답변도 그대로 사용하세요.").await;
    let id = await_question(server, &mut state, true).await;
    hold_question(server, &mut state, &root.join("choice.txt"), 90).await;
    // Enter the free-text row in the same state machine the real terminal uses.
    // The token is chosen only after the request, so it cannot be guessed first.
    state.handle_key(KeyEvent::from(KeyCode::Char('3')));
    let answer = format!("답변_{stamp}");
    for ch in answer.chars() {
        state.handle_key(KeyEvent::from(KeyCode::Char(ch)));
    }
    let Action::RpcResponse {
        id: answer_id,
        result,
    } = state.handle_key(KeyEvent::from(KeyCode::Enter))
    else {
        panic!("직접 입력이 답변으로 제출되지 않음")
    };
    assert_eq!(answer_id, id);
    server.respond(answer_id, result).unwrap();
    finish(server, &mut state, false).await;
    assert_eq!(
        std::fs::read_to_string(root.join("choice.txt"))
            .unwrap()
            .trim_start_matches('\u{feff}')
            .trim(),
        answer
    );

    start(server, &mut state, model,
        "request_user_input으로 첫째 또는 둘째를 고르는 질문 하나를 하세요. 답변을 받기 전에는 다른 작업을 하지 마세요. 답변을 받으면 cancelled.txt에 답변을 적으세요.").await;
    await_question(server, &mut state, false).await;
    hold_question(server, &mut state, &root.join("cancelled.txt"), 30).await;
    let cancel = if model == "gpt-6-astra" {
        state.handle_key(KeyEvent::new(
            KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        ))
    } else if model == "gpt-5.6-terra" {
        state.handle_key(KeyEvent::from(KeyCode::Char('4')))
    } else if model == "gpt-5.6-luna" {
        state.click_overlay_row(4)
    } else {
        state.handle_key(KeyEvent::from(KeyCode::Esc))
    };
    let Action::CancelUserInput {
        interrupt: true, ..
    } = cancel
    else {
        panic!("취소가 작업 중단으로 이어지지 않음")
    };
    server
        .request(
            "turn/interrupt",
            json!({"threadId": state.thread_id, "turnId": state.turn_id}),
        )
        .await
        .unwrap();
    // Just like the host: cancellation must never send an empty answer to Codex.
    finish(server, &mut state, true).await;
    assert!(!root.join("cancelled.txt").exists());

    start(server, &mut state, model,
        "이전 질문은 취소했습니다. 새 작업으로 resumed.txt에 resumed를 적으세요. cancelled.txt는 만들지 마세요. 추가 질문은 필요하지 않습니다.").await;
    finish(server, &mut state, false).await;
    assert_eq!(
        std::fs::read_to_string(root.join("resumed.txt"))
            .unwrap()
            .trim_start_matches('\u{feff}')
            .trim(),
        "resumed"
    );
    assert!(!root.join("cancelled.txt").exists());
    println!("검증 통과: {model} 무응답 대기·직접 입력·답변 후 실행·취소·새 작업 재개");
}

#[tokio::test]
#[ignore = "설치된 Codex와 실제 모델 사용 필요"]
async fn live_codex_question_astra() {
    exercise("gpt-6-astra").await;
}
#[tokio::test]
#[ignore = "설치된 Codex와 실제 모델 사용 필요"]
async fn live_codex_question_sol() {
    exercise("gpt-5.6-sol").await;
}
#[tokio::test]
#[ignore = "설치된 Codex와 실제 모델 사용 필요"]
async fn live_codex_question_terra() {
    exercise("gpt-5.6-terra").await;
}
#[tokio::test]
#[ignore = "설치된 Codex와 실제 모델 사용 필요"]
async fn live_codex_question_luna() {
    exercise("gpt-5.6-luna").await;
}

/// Enumerate the installed runtime's complete catalog, including pagination.
/// New 5.6/6 models or effort levels become test cases instead of silent gaps.
#[tokio::test]
#[ignore = "설치된 Codex와 모든 대상 모델·추론 수준 사용 필요"]
async fn live_codex_question_catalog_matrix() {
    let server = AppServer::spawn(Path::new("codex"), None).await.unwrap();
    server.initialize().await.unwrap();
    let mut cursor = Value::Null;
    let mut cases = std::collections::BTreeSet::new();
    let mut cursors = std::collections::HashSet::new();
    loop {
        let response = server
            .request(
                "model/list",
                json!({"limit": 100, "includeHidden": true, "cursor": cursor}),
            )
            .await
            .unwrap();
        for model in response["data"].as_array().unwrap() {
            let name = model["model"].as_str().unwrap();
            if !name.starts_with("gpt-5.6") && !name.starts_with("gpt-6") {
                continue;
            }
            let efforts = model["supportedReasoningEfforts"].as_array().unwrap();
            assert!(!efforts.is_empty(), "추론 수준이 없는 모델: {name}");
            for effort in efforts {
                cases.insert((
                    name.to_owned(),
                    effort["reasoningEffort"].as_str().unwrap().to_owned(),
                ));
            }
        }
        cursor = response["nextCursor"].clone();
        if cursor.is_null() {
            break;
        }
        assert!(
            cursors.insert(cursor.as_str().unwrap().to_owned()),
            "모델 목록의 순환 커서"
        );
    }
    server.shutdown().await;
    assert!(!cases.is_empty(), "검증 대상 모델이 없음");
    println!("검증 대상 조합: {}", cases.len());
    let results = futures_util::stream::iter(cases)
        .map(|(model, effort)| async move {
            let result = std::panic::AssertUnwindSafe(matrix_case(&model, &effort))
                .catch_unwind()
                .await;
            (model, effort, result)
        })
        .buffer_unordered(4)
        .collect::<Vec<_>>()
        .await;
    let failed = results
        .iter()
        .filter(|(_, _, result)| result.is_err())
        .map(|(model, effort, _)| format!("{model}/{effort}"))
        .collect::<Vec<_>>();
    assert!(failed.is_empty(), "실패한 모델·추론 수준: {failed:?}");
}

async fn matrix_case(model: &str, effort: &str) {
    let mut server = AppServer::spawn(Path::new("codex"), None).await.unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        server.initialize().await.unwrap();
        let root = std::env::temp_dir();
        let response = server.request("thread/start", json!({
            "model": model, "ephemeral": true, "cwd": root,
            "approvalPolicy": "never", "permissions": ":read-only",
            "developerInstructions": "질문 연결만 검증합니다. update_plan과 request_user_input만 사용하세요. 파일·셸·검색·하위 에이전트를 사용하지 마세요."
        })).await.unwrap();
        assert_eq!(response["model"], model, "다른 모델로 대체됨");
        let mut state = AppState::new(response["thread"]["id"].as_str().unwrap().into(), root.to_string_lossy().into(),
            "시험".into(), Vec::new(), model, Some(effort));
        start_with_effort(&server, &mut state, model, effort,
            "먼저 update_plan으로 '1. 선택 대기' 작업을 in_progress로 등록하세요. 다음으로 request_user_input을 호출해 '첫째'와 '둘째' 두 선택지를 주세요. 답변이 도착하기 전에는 후속 작업이나 최종 답변을 하지 마세요. 답변을 받으면 선택된 항목만 그대로 말하세요.").await;
        await_question(&mut server, &mut state, true).await;
        hold_question(&mut server, &mut state, &root.join(format!("devez-never-{model}-{effort}.txt")), 10).await;
        let Action::RpcResponse { id, result } = state.click_overlay_row(2)
            else { panic!("두 번째 선택지 클릭이 제출되지 않음") };
        let answer = result["answers"].as_object().unwrap().values().next().unwrap()["answers"][0].as_str().unwrap().to_owned();
        server.respond(id, result).unwrap();
        let mut text = String::new();
        loop {
            match event(&mut server, &mut state).await {
                ServerEvent::Notification { method, params } => {
                    if method == "item/completed" && params.pointer("/item/type").and_then(Value::as_str) == Some("agentMessage") {
                        text.push_str(params.pointer("/item/text").and_then(Value::as_str).unwrap_or_default());
                    }
                    if method == "turn/completed" {
                        assert_eq!(params["turn"]["status"], "completed", "{params}");
                        break;
                    }
                }
                ServerEvent::Request { method, .. } => panic!("선택 후 예상하지 않은 요청: {method}"),
                ServerEvent::Closed(message) => panic!("{message}"),
                _ => {}
            }
        }
        assert!(text.contains(&answer), "선택값이 최종 답변에 전달되지 않음: {text}");
        println!("검증 통과: {model}/{effort} 작업 목록·무응답 대기·마우스 선택·답변 전달");
    }).catch_unwind().await;
    server.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
#[ignore = "설치된 Codex와 실제 대화 재개 검증 필요"]
async fn live_codex_question_legacy_resume() {
    let model = "gpt-5.6-sol";
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("devez-resume-audit-{stamp}"));
    std::fs::create_dir_all(&root).unwrap();
    let mut old = AppServer::spawn(Path::new("codex"), None).await.unwrap();
    old.initialize().await.unwrap();
    let response = old.request("thread/start", json!({"model": model, "cwd": root,
        "approvalPolicy": "never", "permissions": ":read-only", "developerInstructions": crate::CODEX_QUESTION_INSTRUCTIONS})).await.unwrap();
    let id = response["thread"]["id"].as_str().unwrap().to_owned();
    let mut state = AppState::new(
        id.clone(),
        root.to_string_lossy().into(),
        "시험".into(),
        Vec::new(),
        model,
        Some("low"),
    );
    let old_result = std::panic::AssertUnwindSafe(async {
        old.request("turn/start", json!({"threadId": id, "model": model,
            "collaborationMode": {"mode": "plan", "settings": {"model": model, "reasoning_effort": "low", "developer_instructions": crate::CODEX_QUESTION_INSTRUCTIONS}},
            "input": [{"type": "text", "text": "질문 연결 시험입니다. request_user_input으로 첫째와 둘째 중 선택하게 하고 선택한 값만 답하세요. 다른 도구는 사용하지 마세요."}]})).await.unwrap();
        await_question(&mut old, &mut state, false).await;
        let Action::RpcResponse { id, result } = state.handle_key(KeyEvent::from(KeyCode::Char('2')))
            else { panic!("기존 대화 선택 실패") };
        old.respond(id, result).unwrap();
        finish(&mut old, &mut state, false).await;
    }).catch_unwind().await;
    old.shutdown().await;
    if let Err(panic) = old_result {
        std::panic::resume_unwind(panic);
    }

    let mut resumed = AppServer::spawn(Path::new("codex"), None).await.unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        resumed.initialize().await.unwrap();
        let response = resumed.request("thread/resume", json!({"threadId": id, "excludeTurns": true,
            "developerInstructions": crate::CODEX_QUESTION_INSTRUCTIONS})).await.unwrap();
        assert_eq!(response["model"], model);
        let mut state = AppState::new(id.clone(), root.to_string_lossy().into(), "시험".into(), Vec::new(), model, Some("low"));
        start(&resumed, &mut state, model,
            "먼저 update_plan으로 '1. 재개 확인'을 in_progress로 등록하세요. 다음으로 request_user_input으로 첫째와 둘째 중 선택하게 하고, 답변 후 선택한 값만 말하세요. 다른 도구는 사용하지 마세요.").await;
        await_question(&mut resumed, &mut state, true).await;
        hold_question(&mut resumed, &mut state, &root.join("never.txt"), 10).await;
        let Action::RpcResponse { id: request_id, result } = state.handle_key(KeyEvent::from(KeyCode::Char('2')))
            else { panic!("재개 후 선택 실패") };
        resumed.respond(request_id, result).unwrap();
        finish(&mut resumed, &mut state, false).await;
        resumed.request("thread/archive", json!({"threadId": id})).await.unwrap();
        println!("검증 통과: 이전 계획 모드 대화를 다시 열어 작업 목록·질문 대기·선택 응답 사용");
    }).catch_unwind().await;
    resumed.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    let _ = std::fs::remove_dir(&root);
}

#[tokio::test]
#[ignore = "설치된 Codex와 실제 연결 종료 검증 필요"]
async fn live_codex_question_disconnect() {
    let models = [
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
        "gpt-6-astra",
    ];
    futures_util::stream::iter(models).for_each_concurrent(4, |model| async move {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("devez-disconnect-{model}-{stamp}"));
        std::fs::create_dir_all(&root).unwrap();
        let mut server = AppServer::spawn(Path::new("codex"), None).await.unwrap();
        let result = std::panic::AssertUnwindSafe(async {
            server.initialize().await.unwrap();
            let response = server.request("thread/start", json!({"model": model, "cwd": root,
                "ephemeral": true, "approvalPolicy": "never", "permissions": ":danger-full-access",
                "developerInstructions": crate::CODEX_QUESTION_INSTRUCTIONS})).await.unwrap();
            assert_eq!(response["model"], model);
            let mut state = AppState::new(response["thread"]["id"].as_str().unwrap().into(), root.to_string_lossy().into(),
                "시험".into(), Vec::new(), model, Some("low"));
            start(&server, &mut state, model, "request_user_input으로 첫째와 둘째 중 하나를 고르게 하세요. 다른 도구를 쓰지 말고 답변을 기다리세요. 답변을 받은 경우에만 disconnected.txt에 답변을 쓰세요.").await;
            await_question(&mut server, &mut state, false).await;
            hold_question(&mut server, &mut state, &root.join("disconnected.txt"), 5).await;
            state.fallback_from_codex("시험용 연결 종료");
            assert!(!state.awaiting_input());
        }).catch_unwind().await;
        server.shutdown().await;
        if let Err(panic) = result { std::panic::resume_unwind(panic); }
        assert!(!root.join("disconnected.txt").exists(), "연결 종료가 답변으로 취급됨");
        let _ = std::fs::remove_dir(&root);
        println!("검증 통과: {model} 답변 대기 중 연결 종료 후 실행 없음");
    }).await;
}
