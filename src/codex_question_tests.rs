//! Explicit, opt-in checks against the installed Codex and the real model.
//! cargo test live_codex_question -- --ignored --nocapture
use super::apply_codex_question_mode;
use crate::{
    app_server::{AppServer, ServerEvent},
    state::{Action, AppState},
};
use crossterm::event::{KeyCode, KeyEvent};
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::{Instant, timeout};

async fn event(server: &mut AppServer, state: &mut AppState) -> ServerEvent {
    let event = timeout(Duration::from_secs(120), server.next_event())
        .await
        .expect("실제 모델 응답 시간 초과")
        .expect("연결 종료");
    if let ServerEvent::Notification { method, params } = &event {
        assert!(
            state.reject_unanswered_question(method, params).is_none(),
            "비동기 질문이 도착함"
        );
        state.handle_notification(method, params);
    }
    event
}

async fn start(server: &AppServer, state: &AppState, model: &str, prompt: &str) {
    let mut params = json!({
        "threadId": state.thread_id, "model": model, "effort": "low",
        "permissions": ":danger-full-access",
        "input": [{"type": "text", "text": prompt}]
    });
    apply_codex_question_mode(&mut params).unwrap();
    server.request("turn/start", params).await.unwrap();
}

async fn await_question(server: &mut AppServer, state: &mut AppState) -> Value {
    loop {
        match event(server, state).await {
            ServerEvent::Request { id, method, params } => {
                assert_eq!(
                    method, "item/tool/requestUserInput",
                    "예상하지 않은 요청: {params}"
                );
                assert_eq!(params["isBlocking"], true, "실행을 멈추지 않는 질문");
                assert!(matches!(
                    state.begin_server_request(id.clone(), &method, &params),
                    Action::None
                ));
                assert!(state.awaiting_input());
                assert!(state.view().overlay.is_some());
                return id;
            }
            ServerEvent::Notification { method, params } => {
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

async fn hold_question(server: &mut AppServer, state: &mut AppState, marker: &Path) {
    let until = Instant::now() + Duration::from_secs(30);
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

async fn finish(server: &mut AppServer, state: &mut AppState, cancelled: bool) {
    loop {
        match event(server, state).await {
            ServerEvent::Notification { method, params } => {
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
                    return;
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
        "developerInstructions": "질문 대기 검증용 세션입니다. 요청한 파일만 현재 폴더에 쓰고 그 밖의 도구·검색·하위 에이전트를 사용하지 마세요."
    })).await.unwrap();
    let mut state = AppState::new(
        response["thread"]["id"].as_str().unwrap().into(),
        root.to_string_lossy().into(),
        "시험".into(),
        Vec::new(),
        model,
        Some("low"),
    );
    start(server, &state, model,
        "request_user_input으로 첫째 또는 둘째를 고르는 질문 하나를 하세요. 답변 전에는 파일을 만들거나 다른 도구를 쓰지 마세요. 답변을 받으면 choice.txt에 사용자 답변을 그대로 적으세요. 직접 입력 답변도 그대로 사용하세요.").await;
    let id = await_question(server, &mut state).await;
    hold_question(server, &mut state, &root.join("choice.txt")).await;
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
            .trim(),
        answer
    );

    start(server, &state, model,
        "request_user_input으로 첫째 또는 둘째를 고르는 질문 하나를 하세요. 답변을 받기 전에는 다른 작업을 하지 마세요. 답변을 받으면 cancelled.txt에 답변을 적으세요.").await;
    await_question(server, &mut state).await;
    hold_question(server, &mut state, &root.join("cancelled.txt")).await;
    let Action::CancelUserInput {
        interrupt: true, ..
    } = state.handle_key(KeyEvent::from(KeyCode::Esc))
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

    start(server, &state, model,
        "이전 질문은 취소했습니다. 새 작업으로 resumed.txt에 resumed를 적으세요. cancelled.txt는 만들지 마세요. 추가 질문은 필요하지 않습니다.").await;
    finish(server, &mut state, false).await;
    assert_eq!(
        std::fs::read_to_string(root.join("resumed.txt"))
            .unwrap()
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
