use std::time::Instant;

use codex_core::features::Feature;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_sandbox;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use wiremock::Mock;
use wiremock::matchers::body_string_contains;
use wiremock::matchers::header_exists;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

#[cfg(unix)]
fn rusage_self() -> libc::rusage {
    // Safety: libc::rusage is a plain C struct; zero initialization is valid.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // Safety: libc call that writes to the provided pointer.
    let _ = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    usage
}

#[cfg(unix)]
fn timeval_to_duration(tv: libc::timeval) -> std::time::Duration {
    let secs: u64 = tv.tv_sec.try_into().unwrap_or(0);
    let micros: u64 = tv.tv_usec.try_into().unwrap_or(0);
    std::time::Duration::from_secs(secs) + std::time::Duration::from_micros(micros)
}

#[cfg(unix)]
fn maxrss_bytes(usage: &libc::rusage) -> u64 {
    #[cfg(target_os = "linux")]
    {
        (usage.ru_maxrss as u64).saturating_mul(1024)
    }

    #[cfg(not(target_os = "linux"))]
    {
        usage.ru_maxrss as u64
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn bench_subagents_spawn_poll_16() {
    skip_if_no_network!();
    skip_if_sandbox!();

    const N: usize = 16;
    let server = start_mock_server().await;

    // Respond to any subagent call (all subagent requests include x-openai-subagent).
    let subagent_body = sse(vec![
        ev_response_created("resp-sub-bench"),
        ev_assistant_message("msg-sub-bench", "ok"),
        ev_completed("resp-sub-bench"),
    ]);
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .and(header_exists("x-openai-subagent"))
        .respond_with(sse_response(subagent_body))
        .up_to_n_times((N * 4) as u64)
        .mount(&server)
        .await;

    // Main request 1: spawn N subagents in parallel.
    let mut spawn_events = vec![ev_response_created("resp-main-bench-1")];
    for i in 0..N {
        let args = serde_json::json!({
            "agent_id": format!("bench-agent-{i}"),
            "label": "bench",
            "mode": "explore",
            "prompt": "return ok",
        })
        .to_string();
        spawn_events.push(ev_function_call(
            &format!("call-spawn-{i}"),
            "subagent_spawn",
            &args,
        ));
    }
    spawn_events.push(ev_completed("resp-main-bench-1"));
    let sse_main_1 = sse(spawn_events);
    let _main_1 = mount_sse_once_match(
        &server,
        body_string_contains("trigger-subagent-bench"),
        sse_main_1,
    )
    .await;

    // Main request 2: poll all subagents and wait for completion.
    let mut poll_events = vec![ev_response_created("resp-main-bench-2")];
    for i in 0..N {
        let args = serde_json::json!({
            "agent_id": format!("bench-agent-{i}"),
            "await_ms": 10_000,
        })
        .to_string();
        poll_events.push(ev_function_call(
            &format!("call-poll-{i}"),
            "subagent_poll",
            &args,
        ));
    }
    poll_events.push(ev_completed("resp-main-bench-2"));
    let sse_main_2 = sse(poll_events);
    let _main_2 =
        mount_sse_once_match(&server, body_string_contains("call-spawn-0"), sse_main_2).await;

    // Main request 3: finish.
    let sse_main_3 = sse(vec![
        ev_response_created("resp-main-bench-3"),
        ev_assistant_message("msg-main-bench-3", "done"),
        ev_completed("resp-main-bench-3"),
    ]);
    let _main_3 =
        mount_sse_once_match(&server, body_string_contains("call-poll-0"), sse_main_3).await;

    let mut builder = test_codex()
        .with_model("gpt-5.1-codex")
        .with_config(|config| {
            config.features.enable(Feature::Subagents);
        });
    let test = builder.build(&server).await.expect("build test codex");

    #[cfg(unix)]
    let usage_before = rusage_self();
    let start = Instant::now();
    test.codex
        .submit(Op::UserInput {
            items: vec![codex_protocol::user_input::UserInput::Text {
                text: "trigger-subagent-bench".to_string(),
            }],
        })
        .await
        .expect("submit");
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;
    let wall = start.elapsed();
    #[cfg(unix)]
    let usage_after = rusage_self();

    #[cfg(unix)]
    {
        let user = timeval_to_duration(usage_after.ru_utime)
            .saturating_sub(timeval_to_duration(usage_before.ru_utime));
        let sys = timeval_to_duration(usage_after.ru_stime)
            .saturating_sub(timeval_to_duration(usage_before.ru_stime));
        let maxrss_mb = maxrss_bytes(&usage_after) / (1024 * 1024);
        eprintln!(
            "bench_subagents_spawn_poll_16: wall={}ms user={}ms sys={}ms maxrss={}MB",
            wall.as_millis(),
            user.as_millis(),
            sys.as_millis(),
            maxrss_mb
        );
    }

    #[cfg(not(unix))]
    eprintln!("bench_subagents_spawn_poll_16: {}ms", wall.as_millis());
}
