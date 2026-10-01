//! R5.6 (#21): no request is ever left unanswered. The supervisor answers
//! a call the worker hung on once its deadline passes -- restarting the
//! worker and answering what queued behind it -- and answers a call in
//! flight when the worker crashes, naming the crash. Driven through a real
//! supervisor and worker, with a debug-build hook that makes one tool hang.

#![cfg(unix)]

use std::time::{Duration, Instant};

use serde_json::Value;

mod support;

use support::{registered_worker, start_supervisor, Server};

const STALLED: &str = "get_stats";

/// A supervisor whose worker hangs on every `get_stats` call.
fn start(extra_env: &[(&str, &str)]) -> Server {
    let mut env = vec![("INFIGRAPH_MCP_DEBUG_STALL_TOOL", STALLED)];
    env.extend_from_slice(extra_env);
    start_supervisor(&env)
}

fn error_message(reply: &Value) -> &str {
    reply["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected an error reply, got {reply}"))
}

#[test]
fn a_call_past_its_deadline_is_answered_and_the_worker_restarted() {
    let mut s = start(&[("INFIGRAPH_MCP_CALL_TIMEOUT_SECS", "2")]);
    let first = registered_worker(&s.instances, None);
    s.call(1, STALLED);
    s.list_tools(2); // queued behind the hung call

    let started = Instant::now();
    let late = s.reply(1, Duration::from_secs(30));
    assert!(
        error_message(&late).contains("no reply within 2s"),
        "{late}"
    );
    assert!(started.elapsed() < Duration::from_secs(20));
    let queued = s.reply(2, Duration::from_secs(5));
    assert!(
        error_message(&queued).contains("tools/list was queued behind a get_stats call"),
        "{queued}"
    );

    // The restarted worker serves the next request. Waiting for it to
    // register first: a debug-build worker can take longer than this test's
    // 2s deadline just to start.
    registered_worker(&s.instances, Some(first));
    s.list_tools(3);
    let served = s.reply(3, Duration::from_secs(60));
    assert!(served.get("result").is_some(), "{served}");
}

#[test]
fn a_call_in_flight_when_the_worker_crashes_is_answered_naming_the_crash() {
    let mut s = start(&[]);
    // A served call first: the worker is up and registered after it.
    s.call(1, "list_languages");
    assert!(s.reply(1, Duration::from_secs(60)).get("result").is_some());
    let worker = registered_worker(&s.instances, None);

    s.call(2, STALLED);
    std::thread::sleep(Duration::from_millis(500));
    // As crash_recovery_scope.rs explains, the first kill(2)'d SIGSEGV can
    // be swallowed by std's stack-overflow handler; repeat until it lands.
    for _ in 0..5 {
        // SAFETY: plain kill(2) on the worker this test's supervisor spawned.
        if unsafe { libc::kill(worker as libc::pid_t, libc::SIGSEGV) } != 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }

    let crashed = s.reply(2, Duration::from_secs(30));
    let msg = error_message(&crashed);
    assert!(
        msg.contains("crashed (SIGSEGV)") && msg.contains("get_stats"),
        "{crashed}"
    );

    s.call(3, "list_languages");
    assert!(s.reply(3, Duration::from_secs(60)).get("result").is_some());
}

/// R5.2 (#19): a worker over its hard ceiling restarts itself between
/// calls, and the supervisor starts a fresh worker rather than exiting.
/// A thread ceiling of 1 is breached by any process.
#[test]
fn a_worker_over_its_hard_ceiling_is_replaced_and_the_supervisor_stays_up() {
    let mut s = start(&[
        ("INFIGRAPH_WATCHDOG_THREADS_SOFT", "1"),
        ("INFIGRAPH_WATCHDOG_THREADS_HARD", "1"),
        ("INFIGRAPH_WATCHDOG_INTERVAL_SECS", "1"),
    ]);
    let first = registered_worker(&s.instances, None);
    let second = registered_worker(&s.instances, Some(first));
    assert_ne!(first, second);
    assert!(
        s.child.try_wait().unwrap().is_none(),
        "a watchdog restart is not a reason for the supervisor to exit"
    );
}
