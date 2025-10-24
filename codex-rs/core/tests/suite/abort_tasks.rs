use std::sync::Arc;
use std::time::Duration;

use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event_with_timeout;
use serde_json::json;

/// Integration test: spawn a long‑running shell tool via a mocked Responses SSE
/// function call, then interrupt the session and expect TurnAborted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_long_running_tool_emits_turn_aborted() {
    let command = vec![
        "bash".to_string(),
        "-lc".to_string(),
        "sleep 60".to_string(),
    ];

    let args = json!({
        "command": command,
        "timeout_ms": 60_000
    })
    .to_string();
    let body = sse(vec![
        ev_function_call("call_sleep", "shell", &args),
        ev_completed("done"),
    ]);

    let server = start_mock_server().await;
    mount_sse_once(&server, body).await;

    let codex = test_codex().build(&server).await.unwrap().codex;

    let wait_timeout = Duration::from_secs(5);

    // Kick off a turn that triggers the function call.
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "start sleep".into(),
            }],
        })
        .await
        .unwrap();

    // Wait until the exec begins to avoid a race, then interrupt.
    wait_for_event_with_timeout(
        &codex,
        |ev| matches!(ev, EventMsg::ExecCommandBegin(_)),
        wait_timeout,
    )
    .await;

    codex.submit(Op::Interrupt).await.unwrap();

    // Expect TurnAborted soon after.
    wait_for_event_with_timeout(
        &codex,
        |ev| matches!(ev, EventMsg::TurnAborted(_)),
        wait_timeout,
    )
    .await;
}

/// After an interrupt we expect the next request to the model to include both
/// the original tool call and an `"aborted"` `function_call_output`. This test
/// exercises the follow-up flow: it sends another user turn, inspects the mock
/// responses server, and ensures the model receives the synthesized abort.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_tool_records_history_entries() {
    let command = vec![
        "bash".to_string(),
        "-lc".to_string(),
        "sleep 60".to_string(),
    ];
    let call_id = "call-history";

    let args = json!({
        "command": command,
        "timeout_ms": 60_000
    })
    .to_string();
    let first_body = sse(vec![
        ev_response_created("resp-history"),
        ev_function_call(call_id, "shell", &args),
        ev_completed("resp-history"),
    ]);
    let follow_up_body = sse(vec![
        ev_response_created("resp-followup"),
        ev_completed("resp-followup"),
    ]);

    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(&server, vec![first_body, follow_up_body]).await;

    let fixture = test_codex().build(&server).await.unwrap();
    let codex = Arc::clone(&fixture.codex);

    let wait_timeout = Duration::from_millis(100);

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "start history recording".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event_with_timeout(
        &codex,
        |ev| matches!(ev, EventMsg::ExecCommandBegin(_)),
        wait_timeout,
    )
    .await;

    codex.submit(Op::Interrupt).await.unwrap();

    wait_for_event_with_timeout(
        &codex,
        |ev| matches!(ev, EventMsg::TurnAborted(_)),
        wait_timeout,
    )
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "follow up".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event_with_timeout(
        &codex,
        |ev| matches!(ev, EventMsg::TaskComplete(_)),
        wait_timeout,
    )
    .await;

    let requests = response_mock.requests();
    assert!(
        requests.len() == 2,
        "expected two calls to the responses API, got {}",
        requests.len()
    );

    assert!(
        response_mock.saw_function_call(call_id),
        "function call not recorded in responses payload"
    );
    assert_eq!(
        response_mock.function_call_output_text(call_id).as_deref(),
        Some("aborted"),
        "aborted function call output not recorded in responses payload"
    );
}
