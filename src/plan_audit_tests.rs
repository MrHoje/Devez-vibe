#[tokio::test]
#[ignore = "실제 Codex 로그인과 모델 호출 필요"]
async fn live_provider_plan_timing_codex() {
    exercise_live_plan_timing("gpt-6-luna").await;
}

#[tokio::test]
#[ignore = "실제 Claude 로그인과 모델 호출 필요"]
async fn live_provider_plan_timing_claude() {
    exercise_live_plan_timing("claude:sonnet").await;
}

async fn exercise_live_plan_timing(requested_model: &str) {
    use crate::{app_server::ServerEvent, backend::BackendServer};

    let root = std::env::temp_dir().join(format!(
        "dvz-plan-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let mut server = BackendServer::spawn(
        std::path::Path::new("codex"),
        std::path::Path::new("opencode"),
        std::path::Path::new("node"),
        std::path::Path::new("claude"),
        &root,
    )
    .await
    .unwrap();
    let mut codex = None;
    let outcome = tokio::time::timeout(Duration::from_secs(180), async {
        server.initialize().await?;
        let model = requested_model.to_owned();
        let prompt = if model.starts_with("claude:") {
            "TaskCreate로 '1. 확인', '2. 검증'을 만든 뒤, TaskUpdate로 첫 작업을 in_progress로 바꾸세요. TaskList를 한 번 호출한 다음 첫 작업의 subject를 '1. 확인한 설정'으로 바꾸면서 completed로, 둘째 작업을 in_progress로 바꾸고 마지막에 둘째도 completed로 바꾸세요."
        } else {
            let direct = crate::app_server::AppServer::spawn(std::path::Path::new("codex"), None).await?;
            direct.initialize().await?;
            codex = Some(direct);
            "update_plan으로 '1. 확인', '2. 검증' 두 작업을 만드세요. 첫 호출은 첫 작업 in_progress, 둘째 pending으로, 두 번째 호출은 첫 작업 제목을 '1. 확인한 설정'으로 바꾸면서 completed, 둘째 in_progress로, 세 번째 호출은 둘 다 completed로 보내세요."
        };
        let params = json!({
            "cwd": root, "model": model, "ephemeral": true,
            "approvalPolicy": "never", "permissions": ":read-only"
        });
        println!("실제 계획 검사 시작: {model}");
        let opened = if let Some(codex) = &codex { codex.request("thread/start", params).await? }
            else { server.request("thread/start", params).await? };
        let mut state = AppState::new(opened["thread"]["id"].as_str()
            .ok_or_else(|| anyhow::anyhow!("검증 세션 ID 없음"))?.into(),
            root.to_string_lossy().into(), "계획 검사".into(), Vec::new(), &model, None);
        let prompt = format!("계획 시간 표시 검사입니다. 파일·셸·검색·질문·하위 에이전트 도구는 사용하지 마세요. {prompt} 마지막 답변은 '확인' 한 단어로 쓰세요.");
        let params = json!({
            "threadId": state.thread_id, "model": model, "effort": "low",
            "permissions": ":read-only", "input": state.turn_input(prompt)
        });
        if let Some(codex) = &codex { codex.request("turn/start", params).await?; }
        else { server.request("turn/start", params).await?; }
        let mut first_done_seen = false;
        let mut finished = false;
        loop {
            let event = tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(16)) => {
                    state.drain_stream_text(Duration::from_millis(16));
                    state.tick();
                    if finished && !state.stream_events_pending() { break; }
                    continue;
                }
                event = async {
                    if let Some(codex) = &mut codex { codex.next_event().await }
                    else { server.next_event().await }
                } => event.ok_or_else(|| anyhow::anyhow!("제공자 연결 종료"))?
            };
            match event {
                ServerEvent::Notification { method, params } => {
                    if params.get("threadId").and_then(Value::as_str)
                        .is_some_and(|id| id != state.thread_id) { continue; }
                    state.handle_notification(&method, &params);
                    if method == "turn/plan/updated" {
                        if let Some(plan) = state.plan_summary.as_ref() {
                            if let Some(step) = plan.steps.first() {
                                if step.status == PlanStepStatus::Completed {
                                    anyhow::ensure!(step.elapsed.is_some(), "첫 완료 항목 시간 누락: {model}");
                                    if let Some(measured) = params.pointer("/plan/0/elapsedMs").and_then(Value::as_u64) {
                                        anyhow::ensure!(step.elapsed == Some(Duration::from_millis(measured)), "제공자 측정 시간과 표시 값 불일치: {model}");
                                    }
                                    first_done_seen = true;
                                }
                            }
                        }
                    }
                    if method == "turn/completed" {
                        anyhow::ensure!(params.pointer("/turn/error").is_none_or(Value::is_null), "제공자 응답 오류: {params}");
                        finished = true;
                    }
                }
                ServerEvent::Request { id, method, .. } => {
                    if let Some(codex) = &codex { codex.respond_error(id, -32601, "계획 검사에서 다른 요청은 허용하지 않습니다.")?; }
                    else { server.respond_error(id, -32601, "계획 검사에서 다른 요청은 허용하지 않습니다.")?; }
                    anyhow::bail!("예상하지 않은 제공자 요청: {method}");
                }
                ServerEvent::Closed(detail) => anyhow::bail!("제공자 연결 종료: {detail}"),
                _ => {}
            }
        }
        let plan = state.plan_summary.as_ref().ok_or_else(|| anyhow::anyhow!("계획 알림 없음: {model}"))?;
        anyhow::ensure!(!state.view().plan_active, "최종 계획의 진행 표시가 남음: {model}");
        anyhow::ensure!(first_done_seen && plan.steps.len() == 2 && plan.steps.iter()
            .all(|step| step.status == PlanStepStatus::Completed && step.elapsed.is_some()), "완료 항목의 시간 누락: {model}, 최초 완료 관측 {first_done_seen}, 최종 항목 {:?}", plan.steps);
        println!("실제 계획 검사 통과: {model}, 항목별 시간 {:?}", plan.steps.iter().map(|step| step.elapsed).collect::<Vec<_>>());
        Ok::<(), anyhow::Error>(())
    }).await;
    if let Some(codex) = codex {
        codex.shutdown().await;
    }
    server.shutdown().await;
    outcome.expect("실제 제공자 계획 검사 시간 초과").unwrap();
}
