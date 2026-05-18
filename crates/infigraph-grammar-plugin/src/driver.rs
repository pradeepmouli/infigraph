use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

pub struct GrammarDriver {
    inner: Mutex<DriverInner>,
}

struct DriverInner {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
    line_buf: String,
}

impl GrammarDriver {
    pub fn spawn(driver_jar: &str) -> Result<Self> {
        let mut child = Command::new("java")
            .args(["-jar", driver_jar])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("Failed to spawn JVM grammar driver. Is Java installed?")?;

        let stdout = child.stdout.take().context("No stdout from JVM process")?;
        let mut reader = BufReader::new(stdout);

        let mut line = String::new();
        reader.read_line(&mut line).context("No ready signal from JVM driver")?;
        let ready: serde_json::Value = serde_json::from_str(line.trim())
            .context("Invalid ready signal from JVM driver")?;
        if ready.get("ready") != Some(&serde_json::Value::Bool(true)) {
            bail!("JVM driver did not send ready signal: {}", line.trim());
        }

        Ok(Self {
            inner: Mutex::new(DriverInner {
                child,
                reader,
                line_buf: String::with_capacity(64 * 1024),
            }),
        })
    }

    pub fn load_grammar(
        &self,
        id: &str,
        lexer_path: &str,
        parser_path: &str,
        entry_rule: &str,
        preprocessor: Option<&str>,
        emit_referenced_form_imports: bool,
    ) -> Result<()> {
        let mut req = serde_json::json!({
            "cmd": "load",
            "id": id,
            "lexer": lexer_path,
            "parser": parser_path,
            "entry_rule": entry_rule,
        });
        if let Some(pp) = preprocessor {
            req.as_object_mut().unwrap().insert("preprocessor".into(), serde_json::json!(pp));
        }
        if emit_referenced_form_imports {
            req.as_object_mut().unwrap().insert("emit_referenced_form_imports".into(), serde_json::json!("true"));
        }
        let resp = self.send_request(&req)?;
        if resp.get("ok") != Some(&serde_json::Value::Bool(true)) {
            let err = resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error");
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
            let err = resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error");
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
            req.as_object_mut().unwrap().insert("defines".into(), serde_json::json!(d));
        }
        if let Some(ip) = include_paths {
            req.as_object_mut().unwrap().insert("include_paths".into(), serde_json::json!(ip));
        }
        let resp = self.send_request(&req)?;
        if resp.get("ok") != Some(&serde_json::Value::Bool(true)) {
            let err = resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error");
            bail!("Extract failed for '{}': {}", file_path, err);
        }
        Ok(resp)
    }

    pub fn parse(&self, grammar_id: &str, file_path: &str, source: &str) -> Result<serde_json::Value> {
        let req = serde_json::json!({
            "cmd": "parse",
            "id": grammar_id,
            "file": file_path,
            "source": source,
        });
        let resp = self.send_request(&req)?;
        if resp.get("ok") != Some(&serde_json::Value::Bool(true)) {
            let err = resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error");
            bail!("Parse failed for '{}': {}", file_path, err);
        }
        resp.get("tree").cloned().context("No tree in parse response")
    }

    fn send_request(&self, req: &serde_json::Value) -> Result<serde_json::Value> {
        let mut inner = self.inner.lock().map_err(|e| anyhow::anyhow!("Lock poisoned: {e}"))?;
        let DriverInner { ref mut child, ref mut reader, ref mut line_buf } = *inner;
        let stdin = child.stdin.as_mut().context("No stdin")?;

        let mut payload = serde_json::to_string(req)?;
        payload.push('\n');
        stdin.write_all(payload.as_bytes())?;
        stdin.flush()?;

        line_buf.clear();
        reader.read_line(line_buf)?;
        let resp: serde_json::Value = serde_json::from_str(line_buf.trim())
            .with_context(|| format!("Invalid JSON from driver: {}", line_buf.trim()))?;
        Ok(resp)
    }

    pub fn shutdown(&self) -> Result<()> {
        let req = serde_json::json!({"cmd": "shutdown"});
        let _ = self.send_request(&req);
        let mut inner = self.inner.lock().map_err(|e| anyhow::anyhow!("Lock poisoned: {e}"))?;
        let _ = inner.child.wait();
        Ok(())
    }
}

impl Drop for GrammarDriver {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            let _ = inner.child.kill();
            let _ = inner.child.wait();
        }
    }
}
