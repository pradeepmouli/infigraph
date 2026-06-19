use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use infigraph_core::embed;
use infigraph_core::graph::{SessionData, SessionStore};

pub fn open_session_store(args: &Value) -> Result<SessionStore> {
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path' argument")?;
    SessionStore::open(&PathBuf::from(path))
}

pub fn session_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn session_date_id() -> String {
    let secs = session_epoch();
    let days = secs / 86400;
    let mut y = 1970i64;
    let mut remaining = days;
    loop {
        let dy = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining < dy {
            break;
        }
        remaining -= dy;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let md = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut mo = 0usize;
    for (i, &d) in md.iter().enumerate() {
        if remaining < d {
            mo = i;
            break;
        }
        remaining -= d;
    }
    format!("session_{y:04}-{:02}-{:02}", mo + 1, remaining + 1)
}

pub fn tool_save_session(args: &Value) -> Result<String> {
    let store = open_session_store(args)?;
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path'")?;
    let summary = args
        .get("summary")
        .and_then(|s| s.as_str())
        .context("missing 'summary'")?;
    let pending_tasks = args
        .get("pending_tasks")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let decisions = args.get("decisions").and_then(|s| s.as_str()).unwrap_or("");
    let files_touched = args
        .get("files_touched")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let constraints = args
        .get("constraints")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let assumptions = args
        .get("assumptions")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let blockers = args.get("blockers").and_then(|s| s.as_str()).unwrap_or("");
    let narrative = args.get("narrative").and_then(|s| s.as_str()).unwrap_or("");
    let session_name = args.get("name").and_then(|s| s.as_str()).unwrap_or("");

    let now = session_epoch();
    let session_id = if session_name.is_empty() {
        session_date_id()
    } else {
        format!("named_{}", session_name.to_lowercase().replace(' ', "_"))
    };

    let new_files: Vec<&str> = files_touched
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    let session = if let Some(existing) = store.load(&session_id)? {
        let merged_decisions = if decisions.is_empty() {
            existing.decisions.clone()
        } else if existing.decisions.is_empty() {
            decisions.to_string()
        } else {
            format!("{} | {}", existing.decisions, decisions)
        };

        let mut all_files: Vec<String> = existing
            .files_touched
            .split(", ")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        for f in &new_files {
            if !all_files.iter().any(|x| x == f) {
                all_files.push(f.to_string());
            }
        }

        SessionData {
            id: session_id.clone(),
            name: session_name.to_string(),
            summary: summary.to_string(),
            pending_tasks: pending_tasks.to_string(),
            decisions: merged_decisions,
            files_touched: all_files.join(", "),
            constraints: constraints.to_string(),
            assumptions: assumptions.to_string(),
            blockers: blockers.to_string(),
            created_at: existing.created_at,
            updated_at: now,
        }
    } else {
        SessionData {
            id: session_id.clone(),
            name: session_name.to_string(),
            summary: summary.to_string(),
            pending_tasks: pending_tasks.to_string(),
            decisions: decisions.to_string(),
            files_touched: new_files.join(", "),
            constraints: constraints.to_string(),
            assumptions: assumptions.to_string(),
            blockers: blockers.to_string(),
            created_at: now,
            updated_at: now,
        }
    };

    store.save(&session)?;

    let root = PathBuf::from(path);
    let sessions_dir = root.join(".infigraph").join("sessions");

    if !narrative.is_empty() {
        let md_path = sessions_dir.join(format!("{session_id}.md"));
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&md_path)?;
        let ts_secs = now % 86400;
        let hh = ts_secs / 3600;
        let mm = (ts_secs % 3600) / 60;
        writeln!(f, "\n## Save @ {hh:02}:{mm:02} UTC\n")?;
        writeln!(f, "{narrative}")?;
    }

    let emb_path = sessions_dir.join("embeddings.bin");
    let embed_text =
        format!("{summary} {pending_tasks} {decisions} {constraints} {assumptions} {narrative}");
    let embedder = embed::code_embedder();
    let vec = embedder.embed(&embed_text)?;
    let mut emb_store = embed::load_embeddings(&emb_path).unwrap_or_default();
    emb_store.retain(|(id, _)| id != &session_id);
    emb_store.push((session_id.clone(), vec));
    embed::save_embeddings(&emb_path, &emb_store)?;

    if session_name.is_empty() {
        Ok(format!("Session saved: {session_id}"))
    } else {
        Ok(format!(
            "Session saved: {session_id} (name: {session_name})"
        ))
    }
}

pub const CLUSTER_GAP_SECS: i64 = 72 * 3600;

pub fn detect_session_cluster(store: &SessionStore) -> Result<Vec<SessionData>> {
    let sorted = store.list_by_updated()?;
    if sorted.len() <= 1 {
        return Ok(sorted);
    }

    let mut cluster = vec![sorted[0].clone()];
    for session in &sorted[1..] {
        let prev_updated = cluster.last().unwrap().updated_at;
        if prev_updated - session.updated_at <= CLUSTER_GAP_SECS {
            cluster.push(session.clone());
        } else {
            break;
        }
    }
    Ok(cluster)
}

pub fn date_from_session_id(id: &str) -> &str {
    id.strip_prefix("session_").unwrap_or(id)
}

pub fn format_session_output(
    session: &SessionData,
    idx: usize,
    total: usize,
    path: &str,
) -> String {
    let mut out = String::new();

    if total == 1 {
        out.push_str("## Last Session Context\n\n");
    } else {
        out.push_str(&format!("## Session {} of {}\n\n", idx + 1, total));
    }
    if !session.name.is_empty() {
        out.push_str(&format!(
            "**Session:** {} (name: **{}**)\n\n",
            session.id, session.name
        ));
    } else {
        out.push_str(&format!("**Session:** {}\n\n", session.id));
    }
    if !session.summary.is_empty() {
        out.push_str(&format!("**Summary:** {}\n\n", session.summary));
    }
    if !session.pending_tasks.is_empty() {
        out.push_str(&format!("**Pending Tasks:** {}\n\n", session.pending_tasks));
    }
    if !session.decisions.is_empty() {
        out.push_str(&format!("**Decisions:** {}\n\n", session.decisions));
    }
    if !session.files_touched.is_empty() {
        out.push_str(&format!("**Files Touched:** {}\n\n", session.files_touched));
    }
    if !session.constraints.is_empty() {
        out.push_str(&format!(
            "**Constraints (do not retry):** {}\n\n",
            session.constraints
        ));
    }
    if !session.assumptions.is_empty() {
        out.push_str(&format!(
            "**Assumptions (do not break):** {}\n\n",
            session.assumptions
        ));
    }
    if !session.blockers.is_empty() {
        out.push_str(&format!(
            "**Blockers (needs human):** {}\n\n",
            session.blockers
        ));
    }

    let narrative_path = PathBuf::from(path)
        .join(".infigraph")
        .join("sessions")
        .join(format!("{}.md", session.id));
    if narrative_path.exists() {
        out.push_str(&format!(
            "**Narrative log:** `{}` (read for full session context)\n\n",
            narrative_path.display()
        ));
    }
    out
}

pub fn append_activity_log(out: &mut String, path: &str) {
    let today_date = session_date_id().replace("session_", "");
    let activity_path = PathBuf::from(path)
        .join(".infigraph")
        .join("sessions")
        .join(format!("activity_{today_date}.jsonl"));
    if activity_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&activity_path) {
            let lines: Vec<&str> = content.lines().collect();
            let total = lines.len();
            let tail = if total > 20 {
                &lines[total - 20..]
            } else {
                &lines[..]
            };
            if !tail.is_empty() {
                out.push_str(&format!(
                    "## Activity Log (today, last {} of {} calls)\n\n",
                    tail.len(),
                    total
                ));
                for line in tail {
                    if let Ok(entry) = serde_json::from_str::<Value>(line) {
                        let tool = entry.get("tool").and_then(|t| t.as_str()).unwrap_or("?");
                        let status = entry.get("status").and_then(|s| s.as_str()).unwrap_or("ok");
                        let marker = if status == "ok" { "" } else { " FAILED" };
                        let args_obj = entry.get("args").cloned().unwrap_or(json!({}));
                        let args_str = serde_json::to_string(&args_obj).unwrap_or_default();
                        let preview = if args_str.len() > 80 {
                            &args_str[..80]
                        } else {
                            &args_str
                        };
                        out.push_str(&format!("- `{tool}`{marker} {preview}\n"));
                    }
                }
                out.push('\n');
            }
        }
    }
}

pub fn append_old_session_hint(sessions_dir: &std::path::Path, out: &mut String) {
    if let Ok(entries) = std::fs::read_dir(sessions_dir) {
        let session_files: Vec<_> = entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let s = name.to_string_lossy();
                s.starts_with("session_") && s.ends_with(".json")
            })
            .collect();
        if session_files.len() > 30 {
            out.push_str(&format!(
                "\n> {} session files found. Consider running `purge_sessions` to clean up old sessions.\n",
                session_files.len()
            ));
        }
    }
}

pub fn tool_get_latest_session(args: &Value) -> Result<String> {
    let path = args.get("path").and_then(|p| p.as_str()).unwrap_or(".");
    let explicit_limit = args.get("limit").and_then(|v| v.as_u64());
    let session_name = args.get("name").and_then(|s| s.as_str()).unwrap_or("");
    let store = open_session_store(args)?;

    if !session_name.is_empty() {
        return if let Some(session) = store.load_by_name(session_name)? {
            let mut out = format_session_output(&session, 0, 1, path);
            append_activity_log(&mut out, path);
            Ok(out)
        } else {
            Ok(format!("No session found with name '{session_name}'."))
        };
    }

    let sessions = if let Some(limit) = explicit_limit {
        store.list_recent(limit as usize)?
    } else {
        detect_session_cluster(&store)?
    };

    if sessions.is_empty() {
        return Ok("No previous sessions found. This is a fresh start.".to_string());
    }

    let mut out = String::new();
    let total = sessions.len();

    if total > 1 {
        let newest_date = date_from_session_id(&sessions[0].id);
        let oldest_date = date_from_session_id(&sessions[total - 1].id);
        out.push_str(&format!(
            "## {} parallel sessions detected ({} — {})\n\n\
             **Ask the user which session to resume before proceeding.**\n\n",
            total, oldest_date, newest_date
        ));
    }

    for (idx, session) in sessions.iter().enumerate() {
        out.push_str(&format_session_output(session, idx, total, path));
        if idx < total - 1 {
            out.push_str("\n---\n\n");
        }
    }

    append_activity_log(&mut out, path);
    append_old_session_hint(store.sessions_dir(), &mut out);

    Ok(out)
}

pub fn tool_purge_sessions(args: &Value) -> Result<String> {
    let store = open_session_store(args)?;
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path'")?;
    let older_than_days = args
        .get("older_than_days")
        .and_then(|v| v.as_u64())
        .unwrap_or(30);

    let now = session_epoch();
    let cutoff = now - (older_than_days as i64 * 86400);

    let all = store.list_all()?;
    let to_purge: Vec<&SessionData> = all.iter().filter(|s| s.created_at < cutoff).collect();

    if to_purge.is_empty() {
        return Ok(format!(
            "No sessions older than {older_than_days} days found."
        ));
    }

    let purged_ids: Vec<String> = to_purge.iter().map(|s| s.id.clone()).collect();

    for id in &purged_ids {
        store.delete(id)?;
    }

    let root = PathBuf::from(path);
    let emb_path = root
        .join(".infigraph")
        .join("sessions")
        .join("embeddings.bin");
    if emb_path.exists() {
        let mut emb_store = embed::load_embeddings(&emb_path).unwrap_or_default();
        let before = emb_store.len();
        emb_store.retain(|(id, _)| !purged_ids.contains(id));
        if emb_store.len() < before {
            embed::save_embeddings(&emb_path, &emb_store)?;
        }
    }

    let mut out = format!(
        "Purged {} session(s) older than {} days:\n",
        to_purge.len(),
        older_than_days
    );
    for s in &to_purge {
        let preview = if s.summary.len() > 60 {
            &s.summary[..60]
        } else {
            &s.summary
        };
        out.push_str(&format!("- {}: {preview}\n", s.id));
    }
    Ok(out)
}

pub fn tool_search_sessions(args: &Value) -> Result<String> {
    let store = open_session_store(args)?;
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path'")?;
    let query = args
        .get("query")
        .and_then(|s| s.as_str())
        .context("missing 'query'")?;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;

    let root = PathBuf::from(path);
    let emb_path = root
        .join(".infigraph")
        .join("sessions")
        .join("embeddings.bin");

    if !emb_path.exists() {
        return Ok(
            "No session embeddings found. Save at least one session with `save_session` first."
                .to_string(),
        );
    }

    let emb_store = embed::load_embeddings(&emb_path)?;
    if emb_store.is_empty() {
        return Ok("No session embeddings found.".to_string());
    }

    let embedder = embed::code_embedder();
    let query_vec = embedder.embed(query)?;
    if query_vec.is_empty() {
        return Ok("Failed to embed query.".to_string());
    }

    let mut scored: Vec<(f32, &str)> = emb_store
        .iter()
        .map(|(id, vec)| (embed::cosine_similarity(&query_vec, vec), id.as_str()))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);

    let mut out = format!("## Session Search: \"{query}\"\n\n");

    for (score, session_id) in &scored {
        if let Some(session) = store.load(session_id)? {
            out.push_str(&format!("### {} (relevance: {:.3})\n\n", session.id, score));
            if !session.summary.is_empty() {
                out.push_str(&format!("**Summary:** {}\n\n", session.summary));
            }
            if !session.pending_tasks.is_empty() {
                out.push_str(&format!("**Pending Tasks:** {}\n\n", session.pending_tasks));
            }
            if !session.decisions.is_empty() {
                out.push_str(&format!("**Decisions:** {}\n\n", session.decisions));
            }
            if !session.files_touched.is_empty() {
                out.push_str(&format!("**Files Touched:** {}\n\n", session.files_touched));
            }
            let narrative_path = root
                .join(".infigraph")
                .join("sessions")
                .join(format!("{session_id}.md"));
            if narrative_path.exists() {
                out.push_str(&format!(
                    "**Narrative log:** `{}` (read for full context)\n\n",
                    narrative_path.display()
                ));
            }
            out.push_str("---\n\n");
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use infigraph_core::graph::{SessionData, SessionStore};

    fn make_session(id: &str, created_at: i64, updated_at: i64) -> SessionData {
        SessionData {
            id: id.to_string(),
            name: String::new(),
            summary: format!("work on {id}"),
            pending_tasks: String::new(),
            decisions: String::new(),
            files_touched: String::new(),
            constraints: String::new(),
            assumptions: String::new(),
            blockers: String::new(),
            created_at,
            updated_at,
        }
    }

    fn store_with_sessions(sessions: &[SessionData]) -> (tempfile::TempDir, SessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_dir(dir.path()).unwrap();
        for s in sessions {
            store.save(s).unwrap();
        }
        (dir, store)
    }

    #[test]
    fn test_cluster_single_session() {
        let (_dir, store) = store_with_sessions(&[make_session("session_2026-06-08", 1000, 1000)]);
        let cluster = detect_session_cluster(&store).unwrap();
        assert_eq!(cluster.len(), 1);
        assert_eq!(cluster[0].id, "session_2026-06-08");
    }

    #[test]
    fn test_cluster_two_sessions_within_24h() {
        let now = 1_750_000_000i64;
        let (_dir, store) = store_with_sessions(&[
            make_session("session_2026-06-07", now - 86400, now - 3600),
            make_session("session_2026-06-08", now, now),
        ]);
        let cluster = detect_session_cluster(&store).unwrap();
        assert_eq!(cluster.len(), 2, "both sessions within 24h should cluster");
    }

    #[test]
    fn test_cluster_two_sessions_within_72h() {
        let now = 1_750_000_000i64;
        let (_dir, store) = store_with_sessions(&[
            make_session("session_2026-06-05", now - 200_000, now - 200_000),
            make_session("session_2026-06-08", now, now),
        ]);
        let cluster = detect_session_cluster(&store).unwrap();
        assert_eq!(
            cluster.len(),
            2,
            "sessions 55h apart should cluster (< 72h)"
        );
    }

    #[test]
    fn test_cluster_gap_exceeds_72h() {
        let now = 1_750_000_000i64;
        let old = now - (73 * 3600);
        let (_dir, store) = store_with_sessions(&[
            make_session("session_2026-06-01", old - 86400, old),
            make_session("session_2026-06-08", now, now),
        ]);
        let cluster = detect_session_cluster(&store).unwrap();
        assert_eq!(cluster.len(), 1, "73h gap should break cluster");
        assert_eq!(cluster[0].id, "session_2026-06-08");
    }

    #[test]
    fn test_cluster_chained_48h_gaps() {
        let now = 1_750_000_000i64;
        let h48 = 48 * 3600;
        let (_dir, store) = store_with_sessions(&[
            make_session("session_2026-06-04", now - 2 * h48, now - 2 * h48),
            make_session("session_2026-06-06", now - h48, now - h48),
            make_session("session_2026-06-08", now, now),
        ]);
        let cluster = detect_session_cluster(&store).unwrap();
        assert_eq!(
            cluster.len(),
            3,
            "chained 48h gaps should all cluster (each < 72h from neighbor)"
        );
    }

    #[test]
    fn test_cluster_chain_breaks_at_old_session() {
        let now = 1_750_000_000i64;
        let (_dir, store) = store_with_sessions(&[
            make_session("session_2026-05-01", now - 30 * 86400, now - 30 * 86400),
            make_session("session_2026-06-07", now - 86400, now - 86400),
            make_session("session_2026-06-08", now, now),
        ]);
        let cluster = detect_session_cluster(&store).unwrap();
        assert_eq!(
            cluster.len(),
            2,
            "old session 30d ago should not be in cluster"
        );
        assert_eq!(cluster[0].id, "session_2026-06-08");
        assert_eq!(cluster[1].id, "session_2026-06-07");
    }

    #[test]
    fn test_cluster_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_dir(dir.path()).unwrap();
        let cluster = detect_session_cluster(&store).unwrap();
        assert!(cluster.is_empty());
    }

    #[test]
    fn test_cluster_many_parallel_same_week() {
        let now = 1_750_000_000i64;
        let (_dir, store) = store_with_sessions(&[
            make_session("session_2026-06-04", now - 4 * 86400, now - 4 * 86400),
            make_session("session_2026-06-05", now - 3 * 86400, now - 3 * 86400),
            make_session("session_2026-06-06", now - 2 * 86400, now - 2 * 86400),
            make_session("session_2026-06-07", now - 86400, now - 86400),
            make_session("session_2026-06-08", now, now),
        ]);
        let cluster = detect_session_cluster(&store).unwrap();
        assert_eq!(cluster.len(), 5, "daily sessions should all cluster");
    }

    #[test]
    fn test_date_from_session_id() {
        assert_eq!(date_from_session_id("session_2026-06-08"), "2026-06-08");
        assert_eq!(date_from_session_id("weird_id"), "weird_id");
    }
}
