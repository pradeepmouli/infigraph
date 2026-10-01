use std::process::Command;
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use infigraph_core::child::{ChildTimeouts, LineChild};

/// One JVM serving every grammar plugin. A request that times out poisons it
/// (the JVM is killed with its group, and every later call fails naming the
/// first failure): see [`LineChild`].
pub struct GrammarDriver {
    child: Mutex<LineChild>,
    timeouts: ChildTimeouts,
}

pub struct LoadGrammarOptions<'a> {
    pub entry_rule: &'a str,
    pub preprocessor: Option<&'a str>,
    pub emit_referenced_form_imports: bool,
    pub pipe_strings: bool,
}

impl GrammarDriver {
    pub fn spawn(driver_jar: &str) -> Result<Self> {
        let mut command = Command::new("java");
        command.args(["-jar", driver_jar]);
        Self::spawn_command(command, infigraph_core::child::ChildTimeouts::DEFAULT)
    }

    /// Starts the driver from `command`. `spawn` is this with the JVM; the
    /// seam lets tests stand a script in for it and shorten the deadlines.
    pub fn spawn_command(command: Command, timeouts: ChildTimeouts) -> Result<Self> {
        let mut child = LineChild::spawn(command, "JVM grammar driver", false)
            .context("Failed to spawn JVM grammar driver. Is Java installed?")?;

        let line = child
            .read_line(timeouts.ready)
            .context("No ready signal from JVM driver")?;
        let ready: serde_json::Value =
            serde_json::from_str(line.trim()).context("Invalid ready signal from JVM driver")?;
        if ready.get("ready") != Some(&serde_json::Value::Bool(true)) {
            bail!("JVM driver did not send ready signal: {}", line.trim());
        }

        Ok(Self {
            child: Mutex::new(child),
            timeouts,
        })
    }

    pub fn load_grammar(
        &self,
        id: &str,
        lexer_path: &str,
        parser_path: &str,
        opts: &LoadGrammarOptions,
    ) -> Result<()> {
        let mut req = serde_json::json!({
            "cmd": "load",
            "id": id,
            "lexer": lexer_path,
            "parser": parser_path,
            "entry_rule": opts.entry_rule,
        });
        if let Some(pp) = opts.preprocessor {
            req.as_object_mut()
                .unwrap()
                .insert("preprocessor".into(), serde_json::json!(pp));
        }
        if opts.emit_referenced_form_imports {
            req.as_object_mut().unwrap().insert(
                "emit_referenced_form_imports".into(),
                serde_json::json!("true"),
            );
        }
        if opts.pipe_strings {
            req.as_object_mut().unwrap().insert(
                "preprocessor_pipe_strings".into(),
                serde_json::json!("true"),
            );
        }
        let resp = self.send_request(&req)?;
        if resp.get("ok") != Some(&serde_json::Value::Bool(true)) {
            let err = resp
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            bail!("Failed to load grammar '{}': {}", id, err);
        }
        Ok(())
    }

    pub fn set_extractor(&self, grammar_id: &str, class_name: &str) -> Result<()> {
        let req = serde_json::json!({
            "cmd": "set_extractor",
            "id": grammar_id,
            "class": class_name,
        });
        let resp = self.send_request(&req)?;
        if resp.get("ok") != Some(&serde_json::Value::Bool(true)) {
            let err = resp
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            bail!("Failed to set extractor '{}': {}", class_name, err);
        }
        Ok(())
    }

    pub fn extract(
        &self,
        grammar_id: &str,
        file_path: &str,
        source: &str,
        defines: Option<&str>,
        include_paths: Option<&str>,
    ) -> Result<serde_json::Value> {
        let mut req = serde_json::json!({
            "cmd": "extract",
            "id": grammar_id,
            "file": file_path,
            "source": source,
        });
        if let Some(d) = defines {
            req.as_object_mut()
                .unwrap()
                .insert("defines".into(), serde_json::json!(d));
        }
        if let Some(ip) = include_paths {
            req.as_object_mut()
                .unwrap()
                .insert("include_paths".into(), serde_json::json!(ip));
        }
        let resp = self.send_request(&req)?;
        if resp.get("ok") != Some(&serde_json::Value::Bool(true)) {
            let err = resp
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            bail!("Extract failed for '{}': {}", file_path, err);
        }
        Ok(resp)
    }

    pub fn parse(
        &self,
        grammar_id: &str,
        file_path: &str,
        source: &str,
    ) -> Result<serde_json::Value> {
        let req = serde_json::json!({
            "cmd": "parse",
            "id": grammar_id,
            "file": file_path,
            "source": source,
        });
        let resp = self.send_request(&req)?;
        if resp.get("ok") != Some(&serde_json::Value::Bool(true)) {
            let err = resp
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            bail!("Parse failed for '{}': {}", file_path, err);
        }
        resp.get("tree")
            .cloned()
            .context("No tree in parse response")
    }

    fn send_request(&self, req: &serde_json::Value) -> Result<serde_json::Value> {
        let mut child = self
            .child
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {e}"))?;
        let line = child.request(&serde_json::to_string(req)?, self.timeouts.request)?;
        serde_json::from_str(line.trim())
            .with_context(|| format!("Invalid JSON from driver: {}", line.trim()))
    }

    pub fn shutdown(&self) -> Result<()> {
        let req = serde_json::json!({"cmd": "shutdown"});
        let _ = self.send_request(&req);
        let mut child = self
            .child
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {e}"))?;
        child.finish(std::time::Duration::from_secs(5));
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use infigraph_core::child::ChildTimeouts;
    use std::time::Duration;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    const QUICK: ChildTimeouts = ChildTimeouts {
        ready: Duration::from_millis(600),
        request: Duration::from_millis(600),
    };

    fn alive(pid: u32) -> bool {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// A JVM that starts but never says it is ready (a wedged class load) no
    /// longer hangs `spawn` forever, and takes its stderr with it into the
    /// error.
    #[test]
    fn a_driver_that_never_sends_ready_fails_spawn_within_the_deadline() {
        let started = std::time::Instant::now();
        let err = GrammarDriver::spawn_command(
            sh("echo 'Error: could not open jar' >&2; sleep 600"),
            QUICK,
        )
        .err()
        .expect("spawn must fail");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        let msg = format!("{err:#}");
        assert!(
            msg.contains("could not open jar"),
            "stderr tail missing: {msg}"
        );
    }

    /// A request the driver never answers fails within the request deadline,
    /// poisons the driver (killing its process group), and every later call
    /// names the first failure instead of reading a stale reply.
    #[test]
    fn a_request_that_hangs_poisons_the_driver_and_kills_its_group() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = tmp.path().join("grandchild.pid");
        let driver = GrammarDriver::spawn_command(
            sh(&format!(
                "echo '{{\"ready\":true}}'; sleep 600 & echo $! > '{}'; read l; wait",
                pid_file.display()
            )),
            QUICK,
        )
        .expect("ready handshake");
        let started = std::time::Instant::now();
        let first = driver.parse("g", "a.x", "src").unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert!(format!("{first:#}").contains("sent nothing"), "{first:#}");
        let later = driver.parse("g", "b.x", "src").unwrap_err();
        let msg = format!("{later:#}");
        assert!(msg.contains("first failure"), "{msg}");
        let grandchild: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while alive(grandchild) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !alive(grandchild),
            "the driver's grandchild outlived the poison"
        );
    }

    /// A healthy conversation still works through the new plumbing.
    #[test]
    fn a_healthy_driver_answers_requests() {
        let driver = GrammarDriver::spawn_command(
            sh("echo '{\"ready\":true}'; while read l; do echo '{\"ok\":true,\"tree\":{\"n\":1}}'; done"),
            ChildTimeouts::DEFAULT,
        )
        .expect("ready handshake");
        let tree = driver.parse("g", "a.x", "src").unwrap();
        assert_eq!(tree["n"], 1);
        let tree = driver.parse("g", "b.x", "src").unwrap();
        assert_eq!(tree["n"], 1);
    }
}
