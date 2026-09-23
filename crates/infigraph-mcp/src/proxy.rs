//! R5.6 (#21): no black-holed requests.
//!
//! The supervisor sits between the MCP client and the worker, so it is the
//! one process that can still answer when the worker cannot. It remembers
//! every request it forwards until the worker replies. When the worker
//! crashes or exits, each request still in flight is answered with an error
//! naming what happened, instead of the client waiting forever with the
//! cause only in `mcp.log` (I-13). And every request has a deadline: a call
//! the worker never answers -- a hang, not a crash, as in I-20 -- is
//! answered at the deadline, and the worker is restarted, because it serves
//! one request at a time and every later call would queue behind the stuck
//! one.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use clap::Parser;
use serde_json::{json, Value};

const DEFAULT_TIMEOUT_SECS: u64 = 600;

infigraph_core::settings! {
    mcp_call {
        timeout_secs: u64 = DEFAULT_TIMEOUT_SECS,
    }
}

/// How long a request may go unanswered before the supervisor answers it
/// and restarts the worker. The default covers the longest legitimate call,
/// a full reindex routed through the daemon, which waits up to 600s itself.
/// Overridable via `INFIGRAPH_MCP_CALL_TIMEOUT_SECS` (seconds).
pub fn call_timeout() -> Duration {
    let cli = RawMcpCall::parse_from(std::iter::empty::<String>());
    Duration::from_secs(
        McpCall::resolve(cli, infigraph_core::settings_file::ConfigScope::User).timeout_secs,
    )
}

/// JSON-RPC's "server error" range; the call did not fail in the tool, the
/// server failed to run it.
const SERVER_ERROR: i64 = -32000;

/// A request forwarded to the worker and not yet answered.
#[derive(Debug, Clone)]
pub struct Call {
    pub id: Value,
    /// The tool for a `tools/call`, the method otherwise.
    pub what: String,
    pub since: Instant,
}

/// Every request forwarded to the worker that is still owed a reply.
#[derive(Debug, Default)]
pub struct Outstanding {
    calls: HashMap<String, Call>,
    /// Ids the supervisor answered itself. The worker's reply, if it ever
    /// comes, is dropped: the client already has its answer.
    answered: HashSet<String>,
}

/// Map key for a JSON-RPC id: ids are numbers or strings, and `1` and
/// `"1"` are different ids.
fn key(id: &Value) -> String {
    id.to_string()
}

impl Outstanding {
    /// Note a line the client sent. Only requests are owed a reply;
    /// notifications (no id) and unparseable lines are not tracked -- the
    /// worker answers the latter with a parse error itself.
    pub fn track(&mut self, line: &str, now: Instant) {
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let (Some(id), Some(method)) = (msg.get("id"), msg.get("method").and_then(Value::as_str))
        else {
            return;
        };
        if id.is_null() {
            return;
        }
        let what = match method {
            "tools/call" => msg
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or(method),
            other => other,
        };
        self.calls.insert(
            key(id),
            Call {
                id: id.clone(),
                what: what.to_string(),
                since: now,
            },
        );
    }

    /// Note a line the worker sent, and say whether to pass it to the
    /// client: everything is, except a reply to a request the supervisor
    /// already answered.
    pub fn settle(&mut self, line: &str) -> bool {
        let Some(id) = serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|msg| msg.get("id").cloned())
        else {
            return true;
        };
        let k = key(&id);
        if self.answered.remove(&k) {
            return false;
        }
        self.calls.remove(&k);
        true
    }

    /// The oldest request past `deadline`, if any.
    pub fn overdue(&self, now: Instant, deadline: Duration) -> Option<Call> {
        self.calls
            .values()
            .filter(|c| now.duration_since(c.since) >= deadline)
            .min_by_key(|c| c.since)
            .cloned()
    }

    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// Answer every outstanding request with an error, `why` naming the
    /// cause for each, and return the replies to send.
    pub fn fail_all(&mut self, why: impl Fn(&Call) -> String) -> Vec<Value> {
        let mut calls: Vec<Call> = self.calls.drain().map(|(_, c)| c).collect();
        calls.sort_by_key(|c| c.since);
        calls
            .into_iter()
            .map(|c| {
                self.answered.insert(key(&c.id));
                json!({
                    "jsonrpc": "2.0",
                    "id": c.id,
                    "error": { "code": SERVER_ERROR, "message": why(&c) },
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: Value, tool: &str) -> String {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
               "params": {"name": tool, "arguments": {}}})
        .to_string()
    }

    fn reply(id: Value) -> String {
        json!({"jsonrpc": "2.0", "id": id, "result": {}}).to_string()
    }

    #[test]
    fn requests_are_owed_replies_and_notifications_are_not() {
        let mut o = Outstanding::default();
        let now = Instant::now();
        o.track(&call(json!(1), "search"), now);
        o.track(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            now,
        );
        o.track("not json", now);
        assert!(!o.is_empty());

        assert!(o.settle(&reply(json!(1))));
        assert!(o.is_empty(), "a reply settles its request");
    }

    #[test]
    fn numeric_and_string_ids_are_distinct() {
        let mut o = Outstanding::default();
        let now = Instant::now();
        o.track(&call(json!(1), "search"), now);
        o.track(&call(json!("1"), "get_stats"), now);
        o.settle(&reply(json!("1")));
        let left = o.fail_all(|c| c.what.clone());
        assert_eq!(left.len(), 1);
        assert_eq!(left[0]["id"], json!(1));
    }

    #[test]
    fn a_request_past_its_deadline_is_overdue_oldest_first() {
        let mut o = Outstanding::default();
        let start = Instant::now();
        o.track(&call(json!(1), "search"), start);
        o.track(&call(json!(2), "get_stats"), start + Duration::from_secs(5));

        let deadline = Duration::from_secs(10);
        assert!(o
            .overdue(start + Duration::from_secs(9), deadline)
            .is_none());
        let late = o
            .overdue(start + Duration::from_secs(20), deadline)
            .unwrap();
        assert_eq!((late.id, late.what.as_str()), (json!(1), "search"));
    }

    #[test]
    fn failed_requests_get_errors_naming_the_cause_and_late_replies_are_dropped() {
        let mut o = Outstanding::default();
        let now = Instant::now();
        o.track(&call(json!(7), "search"), now);
        let replies = o.fail_all(|c| format!("worker crashed while serving {}", c.what));
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["id"], json!(7));
        assert_eq!(
            replies[0]["error"]["message"],
            "worker crashed while serving search"
        );
        assert!(o.is_empty());

        assert!(
            !o.settle(&reply(json!(7))),
            "the client already has its answer"
        );
        assert!(o.settle(&reply(json!(7))), "only one reply is dropped");
    }
}
