//! The owner's answer to a needs-you card moves it and reaches the worker (LC-29).
//! Own process: AMUX_HOME and the owner name are process-global.
use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::{body::Body, http::{Request, StatusCode}};
use serde_json::{json, Value};
use tower::ServiceExt;

/// Both tests set the process-global AMUX_HOME; run them one at a time.
static HOME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A request from a local process on the server's own host (loopback peer), as a lane or script sends it.
async fn call(app: &axum::Router, method: &str, path: &str, worker: Option<&str>, body: Value) -> (StatusCode, Value) {
    let headers: Vec<(&str, &str)> = worker.map(|w| ("X-Amux-Worker", w)).into_iter().collect();
    send(app, method, path, &headers, true, body).await
}

/// Any request: extra headers, and whether the peer is loopback (the auth bypass) or remote.
async fn send(app: &axum::Router, method: &str, path: &str, headers: &[(&str, &str)], loopback: bool, body: Value) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path).header("Content-Type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut req = req.body(Body::from(body.to_string())).unwrap();
    let peer: std::net::SocketAddr = if loopback { ([127, 0, 0, 1], 40000).into() } else { ([192, 0, 2, 7], 40000).into() };
    req.extensions_mut().insert(axum::extract::ConnectInfo(peer));
    let r = app.clone().oneshot(req).await.unwrap();
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
    let _home = HOME_LOCK.lock().await;
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

/// sec-a: with an owner credential configured, an answer needs that credential or a verified
/// member session. A local caller that just omits its session header is refused, not recorded as
/// the owner (the AMUX-73 shape). Auth-disabled servers keep the old behaviour (test above).
#[tokio::test]
async fn an_answer_needs_the_owner_credential_or_a_member_session() {
    let _home = HOME_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join("sessions")).unwrap();
    std::fs::write(home.path().join("server.env"), "AMUX_OWNER_NAME=Test Owner\n").unwrap();
    let lane_env = home.path().join("sessions/lane-b.env");
    std::fs::write(&lane_env, "CC_DIR=/tmp\n").unwrap();
    std::env::set_var("AMUX_HOME", home.path());
    std::env::set_var("AMUX_BOARD_DELEGATION", "0");
    let store = std::sync::Arc::new(Store::open(&home.path().join("test.db")).unwrap());
    let app = router(AppState { store, started: std::time::Instant::now(), build_hash: "test".into(),
        auth_token: Some("owner-token-test".into()), reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)) });
    let (by_bearer, by_query, by_member) =
        (parked(&app, "lane-b", "Bearer").await, parked(&app, "lane-b", "Query").await, parked(&app, "lane-b", "Member").await);
    std::fs::remove_file(&lane_env).unwrap();
    let answer = |id: &str| format!("/api/board/{id}/answer");
    let card = |id: String| { let app = app.clone(); async move { call(&app, "GET", &format!("/api/board/{id}"), None, Value::Null).await.1 } };

    // Refused, and nothing written: loopback with no identity, a wrong bearer, a worker.
    let before = card(by_bearer.clone()).await;
    let (s, v) = call(&app, "POST", &answer(&by_bearer), None, json!({"verdict": "approved"})).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::FORBIDDEN, Some("answer_requires_owner_credential")), "{v}");
    let (s, v) = send(&app, "POST", &answer(&by_bearer), &[("Authorization", "Bearer wrong")], true, json!({"verdict": "approved"})).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::FORBIDDEN, Some("answer_requires_owner_credential")), "{v}");
    let (s, v) = send(&app, "POST", &answer(&by_bearer), &[("X-Amux-Session", "gen-codex"), ("Authorization", "Bearer owner-token-test")],
        true, json!({"verdict": "approved"})).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::FORBIDDEN, Some("answer_requires_owner")), "a worker stays refused: {v}");
    let after = card(by_bearer.clone()).await;
    assert_eq!((after["status"].as_str(), &after["log"]), (Some("needsyou"), &before["log"]), "refusals must not touch the card");
    assert!(!v.to_string().contains("auth_token") && !v.to_string().contains(".amux"), "the refusal must not say where the credential lives: {v}");

    // Accepted: the owner bearer (remote, as the dashboard sends it) and the `_token=` query form.
    let (s, v) = send(&app, "POST", &answer(&by_bearer), &[("Authorization", "Bearer owner-token-test")], false, json!({"verdict": "approved"})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(card(by_bearer).await["log"].as_str().unwrap().contains("`decision` Test Owner APPROVED"));
    let (s, v) = send(&app, "POST", &format!("{}?_token=owner-token-test", answer(&by_query)), &[], false, json!({"verdict": "rejected"})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(card(by_query).await["log"].as_str().unwrap().contains("`decision` Test Owner REJECTED"));

    // Accepted: a verified member (invite cookie), recorded as that member.
    let (s, v) = send(&app, "POST", "/api/org/invites", &[("Authorization", "Bearer owner-token-test")], false,
        json!({"email": "guest@example.com"})).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let token = v["token"].as_str().unwrap().to_string();
    let mut accept = Request::builder().method("POST").uri(format!("/invite/{token}"))
        .header("Content-Type", "application/x-www-form-urlencoded").body(Body::from("email=guest%40example.com&name=Guest")).unwrap();
    accept.extensions_mut().insert(axum::extract::ConnectInfo(std::net::SocketAddr::from(([192, 0, 2, 7], 40000))));
    let r = app.clone().oneshot(accept).await.unwrap();
    assert_eq!(r.status(), StatusCode::SEE_OTHER);
    let cookie = r.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();
    let (s, v) = send(&app, "POST", &answer(&by_member), &[("Cookie", &cookie)], false, json!({"verdict": "answered", "text": "go"})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(card(by_member).await["log"].as_str().unwrap().contains("`answered` member:guest@example.com: go"));
}
