use codex_core::features::Feature;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_sandbox;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use wiremock::matchers::body_string_contains;
use wiremock::matchers::header;

fn parse_tool_output_json(mock: &ResponseMock, call_id: &str) -> serde_json::Value {
    let text = mock
        .function_call_output_text(call_id)
        .unwrap_or_else(|| panic!("missing tool output for {call_id}"));
    serde_json::from_str(&text).unwrap_or_else(|_| panic!("invalid JSON tool output for {call_id}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_spawn_then_poll_waits_until_complete() {
    skip_if_no_network!();
    skip_if_sandbox!();

    let server = start_mock_server().await;

    // Main request 1: model asks to spawn a background subagent.
    let spawn_call_id = "call-spawn-1";
    let poll_call_id = "call-poll-1";
    let agent_id = "agent-1";
    let label = "subagent-test";

    let spawn_args = serde_json::json!({
        "agent_id": agent_id,
        "label": label,
        "mode": "explore",
        "prompt": "Summarize what you find in this repo, in 3 bullets.",
    })
    .to_string();
    let sse_main_1 = sse(vec![
        ev_response_created("resp-main-1"),
        ev_function_call(spawn_call_id, "subagent_spawn", &spawn_args),
        ev_completed("resp-main-1"),
    ]);
    let _main_1 = mount_sse_once_match(
        &server,
        body_string_contains("trigger-subagent-test"),
        sse_main_1,
    )
    .await;

    // Subagent request: return a short assistant message.
    let sse_subagent = sse(vec![
        ev_response_created("resp-sub-1"),
        ev_assistant_message("msg-sub-1", "Subagent output"),
        ev_completed("resp-sub-1"),
    ]);
    let subagent_mock =
        mount_sse_once_match(&server, header("x-openai-subagent", label), sse_subagent).await;

    // Main request 2: after spawn tool output, model polls and waits for completion.
    let poll_args = serde_json::json!({
        "agent_id": agent_id,
        "await_ms": 5000,
    })
    .to_string();
    let sse_main_2 = sse(vec![
        ev_response_created("resp-main-2"),
        ev_function_call(poll_call_id, "subagent_poll", &poll_args),
        ev_completed("resp-main-2"),
    ]);
    let main_2 =
        mount_sse_once_match(&server, body_string_contains(spawn_call_id), sse_main_2).await;

    // Main request 3: finish the turn.
    let sse_main_3 = sse(vec![
        ev_response_created("resp-main-3"),
        ev_assistant_message("msg-main-3", "done"),
        ev_completed("resp-main-3"),
    ]);
    let main_3 =
        mount_sse_once_match(&server, body_string_contains(poll_call_id), sse_main_3).await;

    let mut builder = test_codex()
        .with_model("gpt-5.1-codex")
        .with_config(|config| {
            config.features.enable(Feature::Subagents);
        });
    let test = builder.build(&server).await.expect("build test codex");

    test.codex
        .submit(Op::UserInput {
            items: vec![codex_protocol::user_input::UserInput::Text {
                text: "trigger-subagent-test".to_string(),
            }],
        })
        .await
        .expect("submit");

    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    // Assert subagent request is tagged and received the delegated prompt.
    let prompt = "Summarize what you find in this repo, in 3 bullets.";
    let requests = subagent_mock.requests();
    let request = requests
        .iter()
        .find(|req| {
            req.message_input_texts("user")
                .contains(&prompt.to_string())
        })
        .expect("subagent request with delegated prompt");
    assert_eq!(request.header("x-openai-subagent"), Some(label.to_string()),);

    // Spawn output is produced in the subsequent main request as function_call_output.
    let spawn_output = parse_tool_output_json(&main_2, spawn_call_id);
    assert_eq!(spawn_output["agent_id"], agent_id);
    assert_eq!(spawn_output["status"], "queued");

    // Poll output is produced in the final main request as function_call_output.
    let poll_output = parse_tool_output_json(&main_3, poll_call_id);
    assert_eq!(poll_output["agent_id"], agent_id);
    assert_eq!(poll_output["status"], "complete");
    assert_eq!(poll_output["final_output"], "Subagent output");
}
