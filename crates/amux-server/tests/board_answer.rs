//! The owner's answer to a needs-you card moves it and reaches the worker (LC-29).
//! Own process: AMUX_HOME and the owner name are process-global.
use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::{body::Body, http::{Request, StatusCode}};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn call(app: &axum::Router, method: &str, path: &str, worker: Option<&str>, body: Value) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path).header("Content-Type", "application/json");
    if let Some(w) = worker {
        req = req.header("X-Amux-Worker", w);
    }
    let r = app.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = r.status();
    let bytes = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// A card the lane parked on the owner, tagged `needs:you` in the same move.
async fn parked(app: &axum::Router, lane: &str, title: &str) -> String {
    let (s, v) = call(app, "POST", "/api/board", Some(lane), json!({
        "title": title, "status": "backlog", "type": "chore",
        "next_action": "Wait for the owner", "acceptance_criteria": ["Owner decided"],
    })).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let id = v["id"].as_str().unwrap().to_string();
    let (s, v) = call(app, "PATCH", &format!("/api/board/{id}"), Some(lane), json!({
        "status": "needsyou", "tags": ["needs:you"], "ask_type": "decision", "ask_actor": "Test Owner",
        "ask_question": "Ship the release today?", "ask_unblocks": "the owner says yes or no",
    })).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "needsyou", "{v}");
    assert!(v["tags"].as_array().unwrap().iter().any(|t| t == "needs:you"), "the parked card carries its tag: {v}");
    id
}

#[tokio::test]
async fn an_answer_moves_the_card_records_the_owner_and_reports_delivery() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join("sessions")).unwrap();
    std::fs::write(home.path().join("server.env"), "AMUX_OWNER_NAME=Test Owner\n").unwrap();
    let lane_env = home.path().join("sessions/lane-a.env");
    std::fs::write(&lane_env, "CC_DIR=/tmp\n").unwrap();
    std::env::set_var("AMUX_HOME", home.path());
    std::env::set_var("AMUX_BOARD_DELEGATION", "0");
    let store = std::sync::Arc::new(Store::open(&home.path().join("test.db")).unwrap());
    let app = router(AppState { store, started: std::time::Instant::now(), build_hash: "test".into(), auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)) });

    let approve = parked(&app, "lane-a", "Release decision").await;
    let reject = parked(&app, "lane-a", "Risky migration").await;
    let answer = parked(&app, "lane-a", "Which port?").await;
    // Unregister the lane so delivery is refused instead of waking a real worker.
    std::fs::remove_file(&lane_env).unwrap();

    // A worker cannot answer its own ask.
    let (s, v) = call(&app, "POST", &format!("/api/board/{approve}/answer"), Some("lane-a"), json!({"verdict": "approved"})).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["code"], "answer_requires_owner");
    // Bad input writes nothing.
    let (s, _) = call(&app, "POST", &format!("/api/board/{answer}/answer"), None, json!({"verdict": "answered", "text": "  "})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(&app, "POST", &format!("/api/board/{answer}/answer"), None, json!({"verdict": "maybe"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (_, card) = call(&app, "GET", &format!("/api/board/{approve}"), None, Value::Null).await;
    assert_eq!(card["status"], "needsyou", "a refused answer must not move the card");

    // Approve with a note.
    let (s, v) = call(&app, "POST", &format!("/api/board/{approve}/answer"), None, json!({"verdict": "approved", "text": "ship it"})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["owner"], "Test Owner");
    assert_eq!(v["delivery"]["attempted"], true);
    assert_eq!(v["delivery"]["refused"], true, "an unregistered lane must be reported as not delivered: {v}");
    let (_, card) = call(&app, "GET", &format!("/api/board/{approve}"), None, Value::Null).await;
    assert_eq!(card["status"], "todo", "answered card returns to the worker's queue: {card}");
    assert_eq!(card["session"], "lane-a");
    let log = card["log"].as_str().unwrap();
    assert!(log.contains("`decision` Test Owner APPROVED: ship it"), "{log}");
    assert!(log.contains("answer NOT delivered to lane-a"), "delivery outcome must be on the card: {log}");
    assert!(card["ask_question"].is_null() && card["ask_type"].is_null(), "the ask is resolved: {card}");
    assert!(!card["tags"].as_array().unwrap().iter().any(|t| t == "needs:you"), "{card}");
    assert!(card["next_action"].as_str().unwrap().contains("APPROVED"), "{card}");

    // Answering twice is refused: the card no longer waits on the owner.
    let (s, v) = call(&app, "POST", &format!("/api/board/{approve}/answer"), None, json!({"verdict": "rejected"})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "not_needsyou");

    // Reject, and a typed answer.
    let (s, v) = call(&app, "POST", &format!("/api/board/{reject}/answer"), None, json!({"verdict": "rejected"})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, card) = call(&app, "GET", &format!("/api/board/{reject}"), None, Value::Null).await;
    assert_eq!(card["status"], "todo");
    assert!(card["log"].as_str().unwrap().contains("`decision` Test Owner REJECTED"), "{card}");
    let (s, v) = call(&app, "POST", &format!("/api/board/{answer}/answer"), None, json!({"verdict": "answered", "text": "use 8824"})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, card) = call(&app, "GET", &format!("/api/board/{answer}"), None, Value::Null).await;
    assert_eq!(card["status"], "todo");
    assert!(card["log"].as_str().unwrap().contains("`answered` Test Owner: use 8824"), "{card}");
    assert!(card["next_action"].as_str().unwrap().contains("use 8824"), "{card}");

    let (s, _) = call(&app, "POST", "/api/board/NOPE-1/answer", None, json!({"verdict": "approved"})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
