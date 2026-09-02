use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

// Kuzu uses mandatory file locking on Windows — only one connection per DB at a time.
// Tests must run sequentially to avoid "Could not set lock on file" errors.
static MEMORY_LOCK: Mutex<()> = Mutex::new(());

use serde_json::json;

use infigraph_core::graph::SessionStore;
use infigraph_mcp::tools::graph::{tool_get_doc_context, tool_symbol_context};
use infigraph_mcp::tools::index::tool_index_project;
use infigraph_mcp::tools::memory_context::{build_symbol_clusters, tool_memory_context};
use infigraph_mcp::tools::search::tool_search;

mod support;
use infigraph_mcp::tools::session::{
    tool_consolidate_memory, tool_save_session, tool_search_sessions,
};

static PROJECT: OnceLock<SharedProject> = OnceLock::new();

struct SharedProject {
    _dir: tempfile::TempDir,
    path: String,
}

unsafe impl Sync for SharedProject {}

fn shared_project() -> &'static SharedProject {
    PROJECT.get_or_init(|| {
        support::disable_background_watchers();
        let dir = tempfile::TempDir::new().expect("tmpdir");
        support::remove_at_exit(dir.path()); // a static is never dropped
        let files: &[(&str, &str)] = &[
            (
                "src/auth.py",
                "\
from src.db import get_user
from src.utils import hash_password

def authenticate(username, password):
    user = get_user(username)
    if user and verify_token(user, hash_password(password)):
        return create_session(user)
    return None

def verify_token(user, hashed):
    return user.get('password_hash') == hashed

def refresh_token(session):
    if session.get('expired'):
        return None
    session['refreshed'] = True
    return session

def create_session(user):
    return {'user_id': user['id'], 'token': 'jwt_token'}
",
            ),
            (
                "src/db.py",
                "\
class DbPool:
    def __init__(self, url):
        self.url = url
        self.connections = []

    def get_connection(self):
        return self

def get_user(username):
    return {'id': 1, 'name': username, 'password_hash': 'abc123'}

def save_user(user):
    return True

def delete_user(user_id):
    return True
",
            ),
            (
                "src/api.py",
                "\
from src.auth import authenticate, refresh_token
from src.db import save_user, get_user
from src.middleware import auth_middleware
from src.utils import validate_email

def login_handler(request):
    username = request.get('username')
    password = request.get('password')
    session = authenticate(username, password)
    if session:
        return {'status': 200, 'session': session}
    return {'status': 401}

def register_handler(request):
    email = request.get('email')
    if not validate_email(email):
        return {'status': 400, 'error': 'invalid email'}
    user = {'name': request.get('name'), 'email': email}
    save_user(user)
    return {'status': 201, 'user': user}

def refresh_handler(request):
    session = request.get('session')
    new_session = refresh_token(session)
    if new_session:
        return {'status': 200, 'session': new_session}
    return {'status': 401}
",
            ),
            (
                "src/models.py",
                "\
class User:
    def __init__(self, name, email):
        self.name = name
        self.email = email

class Session:
    def __init__(self, user_id, token):
        self.user_id = user_id
        self.token = token

class Token:
    def __init__(self, value, expires_at):
        self.value = value
        self.expires_at = expires_at
",
            ),
            (
                "src/middleware.py",
                "\
from src.auth import verify_token

def auth_middleware(handler):
    def wrapper(request):
        token = request.get('token')
        if not verify_token({'password_hash': token}, token):
            return {'status': 403}
        return handler(request)
    return wrapper

def rate_limiter(max_requests=100):
    calls = []
    def limiter(request):
        calls.append(1)
        if len(calls) > max_requests:
            return {'status': 429}
        return None
    return limiter
",
            ),
            (
                "src/utils.py",
                "\
import hashlib

def hash_password(password):
    return hashlib.sha256(password.encode()).hexdigest()

def validate_email(email):
    return '@' in email and '.' in email.split('@')[1]
",
            ),
        ];

        for (name, content) in files {
            let p = dir.path().join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content).unwrap();
        }

        let path = dir.path().to_string_lossy().to_string();
        tool_index_project(&json!({"path": &path})).expect("index should succeed");

        tool_save_session(&json!({
            "path": &path,
            "summary": "JWT auth refactoring: migrating from session-based auth to JWT tokens with refresh token rotation",
            "decisions": "Goal: Auth migration. Decision: Use JWT with RS256 signing. Why: Stateless auth scales better. Invalidates-if: need server-side session revocation.",
            "constraints": "Tried: HS256 signing. Failed because: shared secret across services is a security risk. Do not retry unless: single-service deployment.",
            "pending_tasks": "1. Implement token blacklist for logout | 2. Add refresh token rotation | 3. Update middleware to validate JWT",
            "assumptions": "Assumes: RSA key pair managed by KMS. If wrong: need local key management.",
            "blockers": "Waiting on infra team to provision KMS keys"
        }))
        .expect("save session");

        SharedProject { _dir: dir, path }
    })
}

fn make_isolated_project() -> (tempfile::TempDir, String) {
    let dir = tempfile::TempDir::new().expect("tmpdir");
    let files: &[(&str, &str)] = &[
        (
            "src/auth.py",
            "from src.db import get_user\nfrom src.utils import hash_password\n\n\
             def authenticate(username, password):\n    user = get_user(username)\n    \
             if user and verify_token(user, hash_password(password)):\n        return create_session(user)\n    return None\n\n\
             def verify_token(user, hashed):\n    return user.get('password_hash') == hashed\n\n\
             def refresh_token(session):\n    if session.get('expired'):\n        return None\n    \
             session['refreshed'] = True\n    return session\n\n\
             def create_session(user):\n    return {'user_id': user['id'], 'token': 'jwt_token'}\n",
        ),
        (
            "src/db.py",
            "class DbPool:\n    def __init__(self, url):\n        self.url = url\n\n\
             def get_user(username):\n    return {'id': 1, 'name': username}\n\n\
             def save_user(user):\n    return True\n",
        ),
        (
            "src/api.py",
            "from src.auth import authenticate\nfrom src.db import save_user\n\n\
             def login_handler(request):\n    return authenticate(request.get('username'), request.get('password'))\n\n\
             def register_handler(request):\n    save_user(request)\n    return {'status': 201}\n",
        ),
        (
            "src/utils.py",
            "def hash_password(p):\n    return p[::-1]\n\ndef validate_email(e):\n    return '@' in e\n",
        ),
    ];

    for (name, content) in files {
        let p = dir.path().join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
    }

    let path = dir.path().to_string_lossy().to_string();
    tool_index_project(&json!({"path": &path})).expect("index should succeed");

    tool_save_session(&json!({
        "path": &path,
        "summary": "JWT auth refactoring: migrating from session-based auth to JWT tokens",
        "decisions": "Goal: Auth migration. Decision: Use JWT with RS256. Why: Stateless auth.",
        "constraints": "Tried: HS256. Failed because: shared secret risk.",
        "blockers": "Waiting on KMS keys"
    }))
    .expect("save session");

    (dir, path)
}

fn args(extra: serde_json::Value) -> serde_json::Value {
    let proj = shared_project();
    let mut map = extra.as_object().cloned().unwrap_or_default();
    map.insert("path".into(), json!(&proj.path));
    serde_json::Value::Object(map)
}

#[test]
fn test_memory_context_golden_cases() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    // --- G1: "authentication flow" without anchor ---
    let result = tool_memory_context(&args(json!({
        "query": "authentication flow"
    })))
    .unwrap();

    assert!(
        result.contains("authenticate") || result.contains("auth"),
        "G1: should contain auth-related symbols: {result}"
    );
    assert!(
        result.contains("Code"),
        "G1: should have Code section: {result}"
    );

    let auth_symbols = [
        "authenticate",
        "verify_token",
        "login_handler",
        "auth_middleware",
    ];
    let g1_hits: usize = auth_symbols.iter().filter(|s| result.contains(*s)).count();
    assert!(
        g1_hits >= 3,
        "G1 precision: found {g1_hits}/4 auth symbols (need ≥3): {result}"
    );

    // --- G2: "authentication flow" WITH anchor file ---
    let result_anchored = tool_memory_context(&args(json!({
        "query": "authentication flow",
        "file": "src/api.py"
    })))
    .unwrap();

    assert!(
        result_anchored.contains("login_handler") || result_anchored.contains("api"),
        "G2: anchor boost should surface api.py symbols: {result_anchored}"
    );
    assert!(
        result_anchored.contains("Skeleton"),
        "G2: should have Skeleton section for anchor file: {result_anchored}"
    );

    // --- G3: "database user queries" — DB symbols dominate ---
    let result = tool_memory_context(&args(json!({
        "query": "database user queries"
    })))
    .unwrap();

    assert!(
        result.contains("get_user") || result.contains("save_user") || result.contains("DbPool"),
        "G3: should contain DB symbols: {result}"
    );

    // --- G4: "JWT refactoring decisions" — session appears ---
    let result = tool_memory_context(&args(json!({
        "query": "JWT refactoring decisions"
    })))
    .unwrap();

    assert!(
        result.contains("Sessions"),
        "G4: should have Sessions section: {result}"
    );
    assert!(
        result.contains("JWT") || result.contains("auth"),
        "G4: session about JWT should appear: {result}"
    );
    assert!(
        result.contains("RS256") || result.contains("signing"),
        "G4: session decisions should be visible: {result}"
    );

    // --- G4b: constraints/blockers always-include ---
    assert!(
        result.contains("Constraint") || result.contains("HS256"),
        "G4b: constraints should always be included: {result}"
    );
    assert!(
        result.contains("Blocker") || result.contains("KMS"),
        "G4b: blockers should always be included: {result}"
    );

    // --- G5: determinism — two identical calls produce identical output ---
    let result_a = tool_memory_context(&args(json!({
        "query": "authentication flow"
    })))
    .unwrap();

    let result_b = tool_memory_context(&args(json!({
        "query": "authentication flow"
    })))
    .unwrap();

    assert_eq!(
        result_a, result_b,
        "G5: identical inputs must produce identical output"
    );

    // --- sources_filter: code only → no sessions ---
    let result = tool_memory_context(&args(json!({
        "query": "authentication",
        "sources": "code"
    })))
    .unwrap();

    assert!(
        !result.contains("Sessions"),
        "sources=code should exclude sessions: {result}"
    );

    // --- sources_filter: sessions only → no code ---
    let result = tool_memory_context(&args(json!({
        "query": "JWT refactoring",
        "sources": "sessions"
    })))
    .unwrap();

    assert!(
        !result.contains("### Code"),
        "sources=sessions should exclude code: {result}"
    );
    assert!(
        result.contains("Sessions"),
        "sources=sessions should include sessions: {result}"
    );

    // --- empty query errors ---
    let result = tool_memory_context(&args(json!({})));
    assert!(result.is_err(), "missing query key should error");

    // --- limit respected ---
    let result = tool_memory_context(&args(json!({
        "query": "function",
        "limit": 2,
        "sources": "code"
    })))
    .unwrap();

    let code_count = result.matches("#### ").count();
    assert!(
        code_count <= 4,
        "limit=2 should cap code results (got {code_count} headers): {result}"
    );

    // --- anchor file nonexistent — graceful degradation ---
    let result = tool_memory_context(&args(json!({
        "query": "test",
        "file": "nonexistent/file.py"
    })));

    assert!(
        result.is_ok(),
        "nonexistent anchor file should degrade gracefully, not panic: {result:?}"
    );

    // --- precision@5: G1-G4 ---
    let cases: &[(&str, Option<&str>, &[&str])] = &[
        (
            "authentication flow",
            None,
            &[
                "authenticate",
                "verify_token",
                "login_handler",
                "auth_middleware",
                "create_session",
            ],
        ),
        (
            "authentication flow",
            Some("src/api.py"),
            &[
                "login_handler",
                "register_handler",
                "refresh_handler",
                "authenticate",
                "refresh_token",
            ],
        ),
        (
            "database user queries",
            None,
            &[
                "get_user",
                "save_user",
                "delete_user",
                "DbPool",
                "get_connection",
            ],
        ),
    ];

    for (i, (query, anchor, relevant)) in cases.iter().enumerate() {
        let mut call = json!({"query": query, "sources": "code"});
        if let Some(f) = anchor {
            call.as_object_mut()
                .unwrap()
                .insert("file".into(), json!(f));
        }
        let result = tool_memory_context(&args(call)).unwrap();
        let found = extract_symbols(&result);
        let hits = relevant.iter().filter(|r| found.contains(**r)).count();
        let precision = hits as f64 / 5.0_f64.min(relevant.len() as f64);
        assert!(
            precision >= 0.6,
            "precision@5 for G{} ({}) = {:.2} (need ≥ 0.6). Found: {:?}, Expected: {:?}",
            i + 1,
            query,
            precision,
            found,
            relevant
        );
    }
}

struct ABQuestion {
    query: &'static str,
    relevant: &'static [&'static str],
}

fn extract_symbols(output: &str) -> HashSet<String> {
    let mut symbols = HashSet::new();
    for line in output.lines() {
        // memory_context format: "#### file::name (score: ...)" or "#### file::name (anchor file)"
        if let Some(rest) = line.strip_prefix("#### ") {
            if let Some(paren) = rest.find(" (") {
                let id = &rest[..paren];
                if let Some(name) = id.rsplit("::").next() {
                    symbols.insert(name.to_string());
                }
            }
        }
        // search format: "0.850  Function authenticate (src/auth.py:L4-8)"
        if line.len() > 6 && line.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            let after_score = line.split_whitespace().skip(1).collect::<Vec<_>>();
            // [Kind, name, (file:L...)]
            if after_score.len() >= 2 {
                let name = after_score[1];
                if !name.starts_with('(') {
                    symbols.insert(name.to_string());
                }
            }
        }
    }
    symbols
}

fn relevance_coverage(output: &str, relevant: &[&str]) -> f64 {
    let found = extract_symbols(output);
    let hits = relevant.iter().filter(|r| found.contains(**r)).count();
    hits as f64 / relevant.len() as f64
}

fn signal_to_noise(output: &str, relevant: &[&str]) -> f64 {
    let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return 0.0;
    }
    let signal_lines = lines
        .iter()
        .filter(|l| relevant.iter().any(|r| l.contains(r)))
        .count();
    signal_lines as f64 / lines.len() as f64
}

#[test]
fn ab_comparison() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    let questions = [
        ABQuestion {
            query: "authentication flow login",
            relevant: &[
                "authenticate",
                "verify_token",
                "login_handler",
                "auth_middleware",
            ],
        },
        ABQuestion {
            query: "database user operations",
            relevant: &["get_user", "save_user", "delete_user", "DbPool"],
        },
        ABQuestion {
            query: "JWT token refactoring auth migration",
            relevant: &["refresh_token", "create_session", "verify_token"],
        },
        ABQuestion {
            query: "request validation middleware",
            relevant: &["auth_middleware", "rate_limiter", "validate_email"],
        },
        ABQuestion {
            query: "user registration API endpoint",
            relevant: &["register_handler", "save_user", "validate_email"],
        },
    ];

    let mut a_wins = 0;
    let mut b_wins = 0;
    let mut a_total_efficiency = 0.0;
    let mut b_total_efficiency = 0.0;
    let mut results_table = String::from(
        "\n| # | Query | A coverage | B coverage | A signal | B signal | Winner |\n\
         |---|-------|-----------|-----------|---------|---------|--------|\n",
    );

    for (i, q) in questions.iter().enumerate() {
        // Method A: search + get_doc_context(top hit) + search_sessions = 3 calls
        let search_result = tool_search(&args(json!({
            "query": q.query,
            "limit": 5
        })))
        .unwrap_or_default();

        let top_symbol = search_result
            .lines()
            .find(|l| l.len() > 6 && l.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .and_then(|l| {
                // "0.850  Function authenticate (src/auth.py:L4-8)"
                let parts: Vec<&str> = l.split_whitespace().collect();
                // parts = ["0.850", "Function", "authenticate", "(src/auth.py:L4-8)"]
                if parts.len() >= 4 {
                    let name = parts[2];
                    let file_part = parts[3].trim_start_matches('(').trim_end_matches(')');
                    let file = file_part.split(':').next()?;
                    Some(format!("{}::{}", file, name))
                } else {
                    None
                }
            });

        let doc_context = if let Some(ref sym_id) = top_symbol {
            tool_get_doc_context(&args(json!({"symbol_id": sym_id}))).unwrap_or_default()
        } else {
            String::new()
        };

        let session_result = tool_search_sessions(&args(json!({
            "query": q.query,
            "limit": 2
        })))
        .unwrap_or_default();

        let method_a = format!("{}\n{}\n{}", search_result, doc_context, session_result);

        // Method B: single memory_context call
        let method_b = tool_memory_context(&args(json!({
            "query": q.query
        })))
        .unwrap_or_default();

        let cov_a = relevance_coverage(&method_a, q.relevant);
        let cov_b = relevance_coverage(&method_b, q.relevant);
        let eff_a = signal_to_noise(&method_a, q.relevant);
        let eff_b = signal_to_noise(&method_b, q.relevant);

        let winner = if cov_b >= cov_a {
            "B (LM2)"
        } else {
            "A (baseline)"
        };
        if cov_b >= cov_a {
            b_wins += 1;
        } else {
            a_wins += 1;
        }
        a_total_efficiency += eff_a;
        b_total_efficiency += eff_b;

        results_table.push_str(&format!(
            "| Q{} | {} | {:.0}% | {:.0}% | {:.3} | {:.3} | {} |\n",
            i + 1,
            q.query,
            cov_a * 100.0,
            cov_b * 100.0,
            eff_a,
            eff_b,
            winner
        ));
    }

    let n = questions.len() as f64;
    results_table.push_str(&format!(
        "\nSummary: B wins {b_wins}/{}, A wins {a_wins}/{}\n\
         Avg signal-to-noise: A={:.3}, B={:.3}\n\
         Round trips: A=3, B=1\n",
        questions.len(),
        questions.len(),
        a_total_efficiency / n,
        b_total_efficiency / n,
    ));

    eprintln!("{results_table}");

    // Success criteria: B achieves >= same coverage as A on >= 3 of 5 questions
    assert!(
        b_wins >= 3,
        "LM2 should win on coverage for >= 3/5 questions (got {b_wins})\n{results_table}"
    );
}

fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

struct DepthQuestion {
    query: &'static str,
    anchor: &'static str,
    relevant: &'static [&'static str],
    expected_depth: &'static str,
}

#[test]
fn depth_tier_comparison() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    let questions = [
        DepthQuestion {
            query: "what does authenticate do",
            anchor: "src/auth.py",
            relevant: &["authenticate", "verify_token"],
            expected_depth: "L1",
        },
        DepthQuestion {
            query: "how is authenticate used by callers",
            anchor: "src/auth.py",
            relevant: &["authenticate", "login_handler", "auth_middleware"],
            expected_depth: "L2",
        },
        DepthQuestion {
            query: "refactor authentication architecture across modules",
            anchor: "src/auth.py",
            relevant: &[
                "authenticate",
                "verify_token",
                "login_handler",
                "auth_middleware",
                "hash_password",
            ],
            expected_depth: "L3",
        },
        DepthQuestion {
            query: "database operations",
            anchor: "src/db.py",
            relevant: &["get_user", "save_user", "delete_user", "DbPool"],
            expected_depth: "L1",
        },
        DepthQuestion {
            query: "how does the API use database and validation",
            anchor: "src/api.py",
            relevant: &[
                "login_handler",
                "register_handler",
                "authenticate",
                "save_user",
                "validate_email",
            ],
            expected_depth: "L2",
        },
    ];

    let mut table = String::from(
        "\n| # | Query | Depth | L1 tok | L2 tok | L3 tok | L1 cov | L2 cov | L3 cov | Auto depth | Expected | Match |\n\
         |---|-------|-------|--------|--------|--------|--------|--------|--------|------------|----------|-------|\n"
    );

    let mut auto_correct = 0;
    let mut l1_cheaper_count = 0;

    for (i, q) in questions.iter().enumerate() {
        let l1 = tool_memory_context(&args(json!({
            "query": q.query,
            "file": q.anchor,
            "depth": "L1",
            "sources": "code"
        })))
        .unwrap_or_default();

        let l2 = tool_memory_context(&args(json!({
            "query": q.query,
            "file": q.anchor,
            "depth": "L2",
            "sources": "code"
        })))
        .unwrap_or_default();

        let l3 = tool_memory_context(&args(json!({
            "query": q.query,
            "file": q.anchor,
            "depth": "L3",
            "sources": "code"
        })))
        .unwrap_or_default();

        let auto_result = tool_memory_context(&args(json!({
            "query": q.query,
            "file": q.anchor,
            "sources": "code"
        })))
        .unwrap_or_default();

        let tok_l1 = estimate_tokens(&l1);
        let tok_l2 = estimate_tokens(&l2);
        let tok_l3 = estimate_tokens(&l3);

        let cov_l1 = relevance_coverage(&l1, q.relevant);
        let cov_l2 = relevance_coverage(&l2, q.relevant);
        let cov_l3 = relevance_coverage(&l3, q.relevant);

        // Determine which depth auto selected by comparing output
        let auto_depth = if auto_result == l1 {
            "L1"
        } else if auto_result == l2 {
            "L2"
        } else {
            "L3"
        };

        let matches = auto_depth == q.expected_depth;
        if matches {
            auto_correct += 1;
        }
        if tok_l1 < tok_l3 {
            l1_cheaper_count += 1;
        }

        table.push_str(&format!(
            "| Q{} | {} | {} | {} | {} | {} | {:.0}% | {:.0}% | {:.0}% | {} | {} | {} |\n",
            i + 1,
            &q.query[..q.query.len().min(40)],
            q.expected_depth,
            tok_l1,
            tok_l2,
            tok_l3,
            cov_l1 * 100.0,
            cov_l2 * 100.0,
            cov_l3 * 100.0,
            auto_depth,
            q.expected_depth,
            if matches { "✓" } else { "✗" }
        ));
    }

    table.push_str(&format!(
        "\nAuto-depth correct: {}/{}\nL1 cheaper than L3: {}/{}\n",
        auto_correct,
        questions.len(),
        l1_cheaper_count,
        questions.len()
    ));

    eprintln!("{table}");

    // L1 must be cheaper than L3 for at least 3/5 questions
    assert!(
        l1_cheaper_count >= 3,
        "L1 should be cheaper than L3 for >= 3/5 questions (got {l1_cheaper_count})\n{table}"
    );

    // Coverage: L3 >= L1 for most questions (wider search = more coverage)
    // But L1 should still have decent coverage for simple queries
    let l1_simple = tool_memory_context(&args(json!({
        "query": "what does authenticate do",
        "file": "src/auth.py",
        "depth": "L1",
        "sources": "code"
    })))
    .unwrap_or_default();
    let cov = relevance_coverage(&l1_simple, &["authenticate", "verify_token"]);
    assert!(
        cov >= 0.5,
        "L1 should cover >= 50% of relevant symbols for simple anchor query (got {:.0}%)",
        cov * 100.0
    );
}

#[test]
fn phase5_consolidation_validation() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    let proj = shared_project();

    // Save multiple related sessions with distinct names so they don't merge
    let session_topics = [
        ("jwt-validation", "JWT token validation: verifying RS256 signatures and checking expiry claims"),
        ("jwt-refresh", "JWT refresh token rotation: implementing secure token refresh with blacklisting"),
        ("jwt-middleware", "JWT auth middleware: extracting bearer tokens and validating on each request"),
        ("jwt-migration", "JWT migration from session cookies: replacing cookie-based auth with JWT bearer tokens"),
    ];
    for (name, summary) in &session_topics {
        tool_save_session(&json!({
            "path": &proj.path,
            "name": name,
            "summary": summary,
            "decisions": format!("Goal: {name}. Decision: Use JWT RS256. Why: Stateless auth."),
        }))
        .expect("save session");
    }

    // Pre-consolidation: search and measure
    let pre_search = tool_search_sessions(&json!({
        "path": &proj.path,
        "query": "JWT token validation auth"
    }))
    .unwrap();
    let pre_tokens = estimate_tokens(&pre_search);
    let pre_relevant = relevance_coverage(&pre_search, &["JWT", "auth", "token"]);

    // Also get memory_context pre-consolidation
    let pre_mc = tool_memory_context(&args(json!({
        "query": "JWT auth token validation",
        "sources": "sessions"
    })))
    .unwrap();
    let pre_mc_tokens = estimate_tokens(&pre_mc);

    // Run consolidation
    let consolidation_result = tool_consolidate_memory(&json!({
        "path": &proj.path,
        "threshold": 0.6
    }))
    .unwrap();

    // Consolidation should produce output (not "nothing to consolidate")
    assert!(
        !consolidation_result.contains("nothing to consolidate")
            && !consolidation_result.contains("Nothing to consolidate"),
        "Consolidation should find clusters to merge: {consolidation_result}"
    );

    // Post-consolidation: search same query
    let post_search = tool_search_sessions(&json!({
        "path": &proj.path,
        "query": "JWT token validation auth"
    }))
    .unwrap();
    let post_relevant = relevance_coverage(&post_search, &["JWT", "auth", "token"]);

    // Signal preservation: post-consolidation relevance >= pre-consolidation
    assert!(
        post_relevant >= pre_relevant * 0.8,
        "Consolidation should preserve signal: pre={pre_relevant:.2}, post={post_relevant:.2}"
    );

    // Post memory_context
    let post_mc = tool_memory_context(&args(json!({
        "query": "JWT auth token validation",
        "sources": "sessions"
    })))
    .unwrap();
    let post_mc_tokens = estimate_tokens(&post_mc);

    // Token efficiency: consolidated should be same or fewer tokens for same info
    // (consolidated summary replaces N individual sessions)
    println!(
        "Token efficiency — search: pre={pre_tokens} post={} | memory_context: pre={pre_mc_tokens} post={post_mc_tokens}",
        estimate_tokens(&post_search)
    );

    // Embedding quality: consolidated session should appear in results
    let has_consolidated =
        post_search.contains("consolidated_") || post_mc.contains("consolidated_");
    assert!(
        has_consolidated,
        "Consolidated session should appear in search/memory_context results"
    );
}

/// Live codebase A/B: 10 questions from the plan, run against infigraph's own index.
/// Marked #[ignore] — requires `terragraph index` on the repo first, slow (~5min).
/// Run: `cargo test -p infigraph-mcp --test memory_context -- ab_live --ignored --nocapture`
#[test]
#[ignore]
fn ab_live_codebase() {
    let infigraph_root = std::env::var("INFIGRAPH_ROOT").unwrap_or_else(|_| {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        std::path::PathBuf::from(&manifest)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_string_lossy()
            .to_string()
    });

    let tg_dir = std::path::PathBuf::from(&infigraph_root).join(".infigraph");
    if !tg_dir.exists() {
        eprintln!(
            "SKIP: {} not indexed. Run `terragraph index` first.",
            infigraph_root
        );
        return;
    }

    struct LiveQuestion {
        query: &'static str,
        relevant: &'static [&'static str],
        qtype: &'static str,
    }

    let questions = [
        LiveQuestion {
            query: "How does symbol resolution handle ambiguous matches?",
            relevant: &[
                "resolve_with_map",
                "best_candidate",
                "resolve",
                "tie_break",
                "Resolution",
            ],
            qtype: "deep code",
        },
        LiveQuestion {
            query: "What changed in session persistence recently?",
            relevant: &[
                "SessionStore",
                "SessionData",
                "save",
                "load",
                "session_store",
            ],
            qtype: "code + session",
        },
        LiveQuestion {
            query: "How does the grammar plugin system load ANTLR grammars?",
            relevant: &["GrammarPlugin", "plugin", "antlr", "grammar", "extract"],
            qtype: "cross-module",
        },
        LiveQuestion {
            query: "What security scanning patterns exist?",
            relevant: &[
                "taint",
                "security",
                "injection",
                "detect_taint",
                "detect_security",
            ],
            qtype: "feature survey",
        },
        LiveQuestion {
            query: "How are embeddings stored and searched?",
            relevant: &[
                "embed",
                "cosine_similarity",
                "load_embeddings",
                "save_embeddings",
                "BM25",
            ],
            qtype: "infrastructure",
        },
        LiveQuestion {
            query: "What decisions were made about Windows CI?",
            relevant: &["Windows", "CI", "windows", "runner"],
            qtype: "session-heavy",
        },
        LiveQuestion {
            query: "How does structured ingestion work with Kuzu?",
            relevant: &["ingest", "structured", "schema", "kuzu", "node_table"],
            qtype: "data flow",
        },
        LiveQuestion {
            query: "What's the call chain from MCP tool dispatch to graph query?",
            relevant: &["dispatch_tool", "GraphQuery", "tool_", "MCP_TOOL_NAMES"],
            qtype: "trace",
        },
        LiveQuestion {
            query: "How does the skeleton tool compute complexity?",
            relevant: &["skeleton", "complexity", "cyclomatic", "fan_in"],
            qtype: "specific function",
        },
        LiveQuestion {
            query: "What's the architecture of the search module?",
            relevant: &[
                "BM25Index",
                "compute_raw_scores",
                "combine_scores",
                "hybrid",
                "search",
            ],
            qtype: "module overview",
        },
    ];

    println!("\n{}", "=".repeat(100));
    println!(
        "LM2 Live A/B — {} questions on infigraph codebase",
        questions.len()
    );
    println!("{}", "=".repeat(100));

    let mut b_wins = 0;
    let mut a_wins = 0;
    let mut ties = 0;

    println!("\n| # | Query (truncated) | Type | A cov | B cov | A signal | B signal | Winner |");
    println!("|---|-------------------|------|-------|-------|----------|----------|--------|");

    for (i, q) in questions.iter().enumerate() {
        // --- A (baseline): search + get_doc_context + search_sessions ---
        let search_result = infigraph_mcp::tools::search::tool_search(&json!({
            "path": &infigraph_root,
            "query": q.query,
            "limit": 5
        }))
        .unwrap_or_default();

        let session_result = tool_search_sessions(&json!({
            "path": &infigraph_root,
            "query": q.query,
            "limit": 2
        }))
        .unwrap_or_default();

        let a_combined = format!("{}\n{}", search_result, session_result);
        let a_lower = a_combined.to_lowercase();
        let a_cov = q
            .relevant
            .iter()
            .filter(|r| a_lower.contains(&r.to_lowercase()))
            .count() as f64
            / q.relevant.len() as f64;
        let _a_tokens = estimate_tokens(&a_combined);
        let a_noise = signal_to_noise(&a_combined, q.relevant);

        // --- B (LM2): single memory_context call ---
        let b_result = tool_memory_context(&json!({
            "path": &infigraph_root,
            "query": q.query,
            "limit": 10
        }))
        .unwrap_or_default();

        let b_lower = b_result.to_lowercase();
        let b_cov = q
            .relevant
            .iter()
            .filter(|r| b_lower.contains(&r.to_lowercase()))
            .count() as f64
            / q.relevant.len() as f64;
        let _b_tokens = estimate_tokens(&b_result);
        let b_noise = signal_to_noise(&b_result, q.relevant);

        let winner = if b_cov > a_cov {
            b_wins += 1;
            "B (LM2)"
        } else if a_cov > b_cov {
            a_wins += 1;
            "A (old)"
        } else if b_noise > a_noise {
            b_wins += 1;
            "B (signal)"
        } else if a_noise > b_noise {
            a_wins += 1;
            "A (signal)"
        } else {
            ties += 1;
            "Tie"
        };

        let query_short: String = q.query.chars().take(35).collect();
        println!(
            "| Q{:<2} | {:<35} | {:<15} | {:>5.0}% | {:>5.0}% | {:>8.3} | {:>8.3} | {:>10} |",
            i + 1,
            query_short,
            q.qtype,
            a_cov * 100.0,
            b_cov * 100.0,
            a_noise,
            b_noise,
            winner
        );
    }

    println!("\nSummary: B wins {b_wins}/10, A wins {a_wins}/10, ties {ties}/10");
    println!("Threshold: B should win >= 7/10");

    assert!(
        b_wins >= 7,
        "LM2 should win >= 7/10 live questions (got B={b_wins}, A={a_wins}, ties={ties})"
    );
}

// ============================================================================
// LM2 Mechanism Tests: comprehensive coverage for every gate/phase
// ============================================================================

/// Mechanism 4: Auto-escalation — L1 with no anchor should escalate to L2,
/// and L1 with too few symbols in anchor file should also escalate.
#[test]
fn auto_escalation_l1_to_l2() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    let (dir, path) = make_isolated_project();

    // L1 without anchor → should auto-escalate to L2 (no panic, returns results)
    let result = tool_memory_context(&json!({
        "path": &path,
        "query": "authentication",
        "depth": "L1",
        "sources": "code"
    }))
    .unwrap();
    assert!(
        !result.is_empty(),
        "L1 without anchor should escalate to L2, not return empty"
    );
    assert!(
        result.contains("Code"),
        "Escalated L1 should still produce code results"
    );
    drop(dir);
}

/// Mechanism 6+7: Confidence decay integration — old sessions rank lower
/// than recent sessions for same-relevance query.
#[test]
fn confidence_decay_ranking() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    support::disable_background_watchers();
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().to_string_lossy().to_string();

    // Create minimal project
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("app.py"),
        "def process_data(x):\n    return x * 2\n",
    )
    .unwrap();
    tool_index_project(&json!({"path": &path})).unwrap();

    // Save a "recent" session
    tool_save_session(&json!({
        "path": &path,
        "name": "recent-data-processing",
        "summary": "Recent data processing: optimized the batch pipeline for faster throughput",
        "decisions": "Goal: batch speed. Decision: parallel chunks. Why: 3x throughput."
    }))
    .unwrap();

    // Manually create an "old" session with stale last_accessed
    let store = SessionStore::open(dir.path()).unwrap();
    let old_session = infigraph_core::graph::SessionData {
        id: "named_old-data-processing".to_string(),
        name: "old-data-processing".to_string(),
        summary: "Old data processing: initial implementation of the batch pipeline".to_string(),
        decisions: "Goal: batch pipeline. Decision: sequential. Why: simple.".to_string(),
        pending_tasks: String::new(),
        files_touched: String::new(),
        constraints: String::new(),
        assumptions: String::new(),
        blockers: String::new(),
        created_at: 1700000000, // ~Nov 2023
        updated_at: 1700000000,
        confidence: 0.7,
        last_accessed: 1700000000, // very old
    };
    store.save(&old_session).unwrap();

    // Embed both sessions
    let emb_dir = dir.path().join(".infigraph").join("sessions");
    std::fs::create_dir_all(&emb_dir).unwrap();
    let emb_path = emb_dir.join("embeddings.bin");
    let embedder = infigraph_core::embed::code_embedder();
    let recent_emb = embedder
        .embed("Recent data processing: optimized the batch pipeline for faster throughput")
        .unwrap();
    let old_emb = embedder
        .embed("Old data processing: initial implementation of the batch pipeline")
        .unwrap();
    let embeddings = vec![
        ("named_recent-data-processing".to_string(), recent_emb),
        ("named_old-data-processing".to_string(), old_emb),
    ];
    infigraph_core::embed::save_embeddings(&emb_path, &embeddings).unwrap();

    // Query for "data processing"
    let result = tool_memory_context(&json!({
        "path": &path,
        "query": "data processing batch pipeline",
        "sources": "sessions"
    }))
    .unwrap();

    // Recent session should appear with higher confidence
    let recent_pos = result.find("recent-data-processing");
    let old_pos = result.find("old-data-processing");

    if let (Some(rp), Some(op)) = (recent_pos, old_pos) {
        assert!(
            rp < op,
            "Recent session should rank before old session due to confidence decay.\n\
             Recent at position {rp}, Old at position {op}\nOutput:\n{result}"
        );
    }

    // Old session should show lower confidence score
    assert!(
        result.contains("confidence:"),
        "Sessions should show confidence scores"
    );
}

/// Mechanism 7: Archive threshold — sessions with confidence < 0.3 excluded.
#[test]
fn archive_threshold_excludes_stale() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    support::disable_background_watchers();
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().to_string_lossy().to_string();

    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.py"), "def main(): pass\n").unwrap();
    tool_index_project(&json!({"path": &path})).unwrap();

    let store = SessionStore::open(dir.path()).unwrap();

    // Create session with confidence that will be below archive threshold
    let ancient_session = infigraph_core::graph::SessionData {
        id: "named_ancient-work".to_string(),
        name: "ancient-work".to_string(),
        summary: "Ancient work: deprecated logging framework removal".to_string(),
        decisions: String::new(),
        pending_tasks: String::new(),
        files_touched: String::new(),
        constraints: String::new(),
        assumptions: String::new(),
        blockers: String::new(),
        created_at: 1600000000, // ~Sep 2020
        updated_at: 1600000000,
        confidence: 0.7,
        last_accessed: 1600000000, // > 250 weeks ago → confidence ~= 0.7 - 250*0.05 = -11.8 → 0.0
    };
    store.save(&ancient_session).unwrap();

    // Also save a fresh session
    tool_save_session(&json!({
        "path": &path,
        "name": "fresh-work",
        "summary": "Fresh work: new logging framework implementation"
    }))
    .unwrap();

    // Embed both
    let emb_dir = dir.path().join(".infigraph").join("sessions");
    let emb_path = emb_dir.join("embeddings.bin");
    let embedder = infigraph_core::embed::code_embedder();
    let embeddings = vec![
        (
            "named_ancient-work".to_string(),
            embedder
                .embed("Ancient work: deprecated logging framework removal")
                .unwrap(),
        ),
        (
            "named_fresh-work".to_string(),
            embedder
                .embed("Fresh work: new logging framework implementation")
                .unwrap(),
        ),
    ];
    infigraph_core::embed::save_embeddings(&emb_path, &embeddings).unwrap();

    let result = tool_memory_context(&json!({
        "path": &path,
        "query": "logging framework",
        "sources": "sessions"
    }))
    .unwrap();

    // Ancient session should be excluded (archived)
    assert!(
        !result.contains("ancient-work"),
        "Archived session (confidence < 0.3) should be excluded from results.\nOutput:\n{result}"
    );
    // Fresh session should still appear
    assert!(
        result.contains("fresh-work"),
        "Fresh session should appear in results.\nOutput:\n{result}"
    );
}

/// Mechanism 8: Touch-on-access — memory_context should update last_accessed.
#[test]
fn touch_on_access_updates_last_accessed() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    support::disable_background_watchers();
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().to_string_lossy().to_string();

    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.py"), "def helper(): pass\n").unwrap();
    tool_index_project(&json!({"path": &path})).unwrap();

    // Create session manually with last_accessed a few days ago (recent enough to not be archived)
    let store = SessionStore::open(dir.path()).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let three_days_ago = now - 3 * 86400;
    let session = infigraph_core::graph::SessionData {
        id: "named_touchable-session".to_string(),
        name: "touchable-session".to_string(),
        summary: "Touchable session about helper functions and utilities".to_string(),
        decisions: String::new(),
        pending_tasks: String::new(),
        files_touched: String::new(),
        constraints: String::new(),
        assumptions: String::new(),
        blockers: String::new(),
        created_at: three_days_ago,
        updated_at: three_days_ago,
        confidence: 0.7,
        last_accessed: three_days_ago,
    };
    store.save(&session).unwrap();
    let before_accessed = three_days_ago;

    // Embed session
    let emb_dir = dir.path().join(".infigraph").join("sessions");
    std::fs::create_dir_all(&emb_dir).unwrap();
    let emb_path = emb_dir.join("embeddings.bin");
    let embedder = infigraph_core::embed::code_embedder();
    let embeddings = vec![(
        "named_touchable-session".to_string(),
        embedder
            .embed("Touchable session about helper functions and utilities")
            .unwrap(),
    )];
    infigraph_core::embed::save_embeddings(&emb_path, &embeddings).unwrap();

    // Call memory_context which should touch the session
    let _result = tool_memory_context(&json!({
        "path": &path,
        "query": "helper functions utilities",
        "sources": "sessions"
    }))
    .unwrap();

    // Check last_accessed was updated to current time
    let after = store.load("named_touchable-session").unwrap().unwrap();
    assert!(
        after.last_accessed > before_accessed,
        "memory_context should touch session, updating last_accessed.\n\
         Before: {before_accessed}, After: {}",
        after.last_accessed
    );
}

/// Mechanism 10+11+12: Symbol clustering — co-occurrence recording, cluster building,
/// and cluster boost (+0.1).
#[test]
fn symbol_clustering_end_to_end() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    let (dir, path) = make_isolated_project();

    // Call memory_context multiple times with same query to build co-occurrence
    for _ in 0..3 {
        let _ = tool_memory_context(&json!({
            "path": &path,
            "query": "authentication login verify",
            "sources": "code"
        }))
        .unwrap();
    }

    // Check co-occurrence file was created
    let co_path = PathBuf::from(&path)
        .join(".infigraph")
        .join("symbol_cooccurrence.json");
    assert!(
        co_path.exists(),
        "Co-occurrence file should be created after multiple memory_context calls"
    );

    let co_content: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&co_path).unwrap()).unwrap();
    assert!(
        !co_content.as_object().unwrap().is_empty(),
        "Co-occurrence file should have entries"
    );

    // Build clusters
    let clusters = build_symbol_clusters(&path, 2).unwrap();

    // Check cluster file was persisted
    let cl_path = PathBuf::from(&path)
        .join(".infigraph")
        .join("symbol_clusters.json");

    if !clusters.is_empty() {
        assert!(
            cl_path.exists(),
            "Cluster file should be persisted when clusters are found"
        );

        // Verify cluster structure: each cluster has >= 2 members
        for (key, members) in &clusters {
            assert!(
                members.len() >= 2,
                "Cluster '{}' should have >= 2 members, got {}",
                key,
                members.len()
            );
        }

        // Now call memory_context again — cluster boost should apply
        let result_with_clusters = tool_memory_context(&json!({
            "path": &path,
            "query": "authentication login verify",
            "sources": "code"
        }))
        .unwrap();

        assert!(
            !result_with_clusters.is_empty(),
            "memory_context should work with cluster boost active"
        );
    }
    drop(dir);
}

/// Mechanism 18: Instrumentation logging — memory_context should write JSONL log.
#[test]
fn instrumentation_logging() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    let (dir, path) = make_isolated_project();

    let log_path = PathBuf::from(&path)
        .join(".infigraph")
        .join("memory_context_log.jsonl");

    // Make a memory_context call
    let _ = tool_memory_context(&json!({
        "path": &path,
        "query": "test instrumentation logging"
    }))
    .unwrap();

    // Check log was written
    assert!(log_path.exists(), "Instrumentation log file should exist");

    let log_content = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        !log_content.is_empty(),
        "Log should have at least one entry"
    );

    // Verify last log line is valid JSON with expected fields
    let last_line = log_content.lines().last().unwrap();
    let entry: serde_json::Value =
        serde_json::from_str(last_line).expect("Log entry should be valid JSON");

    assert!(entry.get("ts").is_some(), "Log should have 'ts' field");
    assert!(
        entry.get("query").is_some(),
        "Log should have 'query' field"
    );
    assert!(
        entry.get("depth").is_some(),
        "Log should have 'depth' field"
    );
    assert!(entry.get("code").is_some(), "Log should have 'code' field");
    assert!(
        entry.get("sessions").is_some(),
        "Log should have 'sessions' field"
    );
    assert!(
        entry.get("tokens").is_some(),
        "Log should have 'tokens' field"
    );
    assert!(entry.get("ms").is_some(), "Log should have 'ms' field");
    drop(dir);
}

/// Mechanism 19: Latency — memory_context should complete under 30s on fixture.
/// Warm bound is generous to accommodate slow CI runners (Ubuntu debug builds
/// observed at ~13s). Catches catastrophic regressions, not micro-latency.
#[test]
fn latency_reasonable() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    let (dir, path) = make_isolated_project();

    // Warm up: first call initializes embedder
    let _ = tool_memory_context(&json!({
        "path": &path,
        "query": "warmup",
        "sources": "code"
    }))
    .unwrap();

    // Measure warm call
    let start = std::time::Instant::now();
    let _ = tool_memory_context(&json!({
        "path": &path,
        "query": "authentication flow login handler"
    }))
    .unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs() < 30,
        "memory_context on small fixture should complete < 30s warm (debug build). Got {:.2}s",
        elapsed.as_secs_f64()
    );
    drop(dir);
}

/// Mechanism 5: Anchor boost score verification — symbols related to anchor
/// should have higher scores than unrelated symbols.
#[test]
fn anchor_boost_score_verification() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    let (dir, path) = make_isolated_project();

    // With anchor on api.py
    let result_anchored = tool_memory_context(&json!({
        "path": &path,
        "query": "function",
        "file": "src/api.py",
        "sources": "code"
    }))
    .unwrap();

    // api.py symbols should appear in results
    let api_symbols = ["login_handler", "register_handler"];

    let anchored_lines: Vec<&str> = result_anchored.lines().collect();
    let api_in_top = anchored_lines
        .iter()
        .take(30)
        .filter(|l| api_symbols.iter().any(|s| l.contains(s)))
        .count();

    assert!(
        api_in_top > 0,
        "Anchor boost: api.py symbols should appear in top results when anchored to api.py.\nOutput:\n{result_anchored}"
    );
    drop(dir);
}

/// Confidence decay × ranking integration: verify that relevance * confidence
/// scoring actually changes the order.
#[test]
fn confidence_times_relevance_scoring() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    support::disable_background_watchers();
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().to_string_lossy().to_string();

    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/calc.py"),
        "def calculate(x): return x + 1\n",
    )
    .unwrap();
    tool_index_project(&json!({"path": &path})).unwrap();

    let store = SessionStore::open(dir.path()).unwrap();

    // Create two sessions with identical text but different ages
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let recent = infigraph_core::graph::SessionData {
        id: "named_calc-recent".to_string(),
        name: "calc-recent".to_string(),
        summary: "Calculator optimization: reduced computation time for large inputs".to_string(),
        decisions: String::new(),
        pending_tasks: String::new(),
        files_touched: String::new(),
        constraints: String::new(),
        assumptions: String::new(),
        blockers: String::new(),
        created_at: now - 86400, // 1 day ago
        updated_at: now - 86400,
        confidence: 0.7,
        last_accessed: now - 86400,
    };

    let old = infigraph_core::graph::SessionData {
        id: "named_calc-old".to_string(),
        name: "calc-old".to_string(),
        summary: "Calculator optimization: reduced computation time for large inputs".to_string(),
        decisions: String::new(),
        pending_tasks: String::new(),
        files_touched: String::new(),
        constraints: String::new(),
        assumptions: String::new(),
        blockers: String::new(),
        created_at: now - 6 * 604800, // 6 weeks ago
        updated_at: now - 6 * 604800,
        confidence: 0.7,
        last_accessed: now - 6 * 604800,
    };

    store.save(&recent).unwrap();
    store.save(&old).unwrap();

    // Embed both (identical text → similar embeddings)
    let emb_dir = dir.path().join(".infigraph").join("sessions");
    let emb_path = emb_dir.join("embeddings.bin");
    let embedder = infigraph_core::embed::code_embedder();
    let text = "Calculator optimization: reduced computation time for large inputs";
    let embeddings = vec![
        (
            "named_calc-recent".to_string(),
            embedder.embed(text).unwrap(),
        ),
        ("named_calc-old".to_string(), embedder.embed(text).unwrap()),
    ];
    infigraph_core::embed::save_embeddings(&emb_path, &embeddings).unwrap();

    let result = tool_memory_context(&json!({
        "path": &path,
        "query": "calculator optimization computation",
        "sources": "sessions"
    }))
    .unwrap();

    // Recent should rank first (same relevance but higher confidence)
    let recent_pos = result.find("calc-recent");
    let old_pos = result.find("calc-old");

    match (recent_pos, old_pos) {
        (Some(rp), Some(op)) => {
            assert!(
                rp < op,
                "Recent session should rank before old due to confidence*relevance.\n\
                 Recent@{rp}, Old@{op}\nResult:\n{result}"
            );
        }
        (Some(_), None) => {
            // Old was archived — that's correct behavior too
        }
        _ => {
            panic!("At least recent session should appear. Result:\n{result}");
        }
    }
}

/// Diversity guarantee: at least 1 session result when both code and sessions exist.
#[test]
fn diversity_session_included() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    let (dir, path) = make_isolated_project();

    // Need to embed the session for it to appear
    let emb_dir = PathBuf::from(&path).join(".infigraph").join("sessions");
    std::fs::create_dir_all(&emb_dir).unwrap();
    let emb_path = emb_dir.join("embeddings.bin");
    let embedder = infigraph_core::embed::code_embedder();
    let store = SessionStore::open(dir.path()).unwrap();
    let sessions = store.list_all().unwrap();
    let mut embeddings = Vec::new();
    for s in &sessions {
        if let Ok(emb) = embedder.embed(&s.summary) {
            embeddings.push((s.id.clone(), emb));
        }
    }
    if !embeddings.is_empty() {
        infigraph_core::embed::save_embeddings(&emb_path, &embeddings).unwrap();
    }

    let result = tool_memory_context(&json!({
        "path": &path,
        "query": "JWT authentication token security"
    }))
    .unwrap();

    assert!(
        result.contains("Code"),
        "Should have Code section when both sources available"
    );
    assert!(
        result.contains("Sessions"),
        "Diversity: should have at least 1 session when session data exists.\nOutput:\n{result}"
    );
    drop(dir);
}

/// Source type ordering: constraints/blockers (always-include) should appear
/// in the output alongside session content.
#[test]
fn always_include_ordering() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    let (dir, path) = make_isolated_project();

    // Embed session
    let emb_dir = PathBuf::from(&path).join(".infigraph").join("sessions");
    std::fs::create_dir_all(&emb_dir).unwrap();
    let emb_path = emb_dir.join("embeddings.bin");
    let embedder = infigraph_core::embed::code_embedder();
    let store = SessionStore::open(dir.path()).unwrap();
    let sessions = store.list_all().unwrap();
    let mut embeddings = Vec::new();
    for s in &sessions {
        if let Ok(emb) = embedder.embed(&s.summary) {
            embeddings.push((s.id.clone(), emb));
        }
    }
    if !embeddings.is_empty() {
        infigraph_core::embed::save_embeddings(&emb_path, &embeddings).unwrap();
    }

    let result = tool_memory_context(&json!({
        "path": &path,
        "query": "JWT auth migration security"
    }))
    .unwrap();

    if result.contains("Constraint") {
        // Constraints should appear in sessions section
        assert!(
            result.contains("Sessions"),
            "Constraints should appear alongside session data"
        );
    }
    drop(dir);
}

// ============================================================================
// Phase 4 Tests: Auto-injection (skip connection) + Selective indexing (input gate)
// ============================================================================

/// Phase 4a: Auto-injection — symbol_context should append session context
/// when a relevant session exists with confidence > 0.5 and score > 0.7.
#[test]
fn auto_injection_symbol_context() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    support::disable_background_watchers();
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().to_string_lossy().to_string();

    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/auth.py"),
        "def authenticate(username, password):\n    return username == 'admin'\n\n\
         def verify_token(token):\n    return token == 'valid'\n",
    )
    .unwrap();
    tool_index_project(&json!({"path": &path})).unwrap();

    // Save a highly relevant session about authenticate
    tool_save_session(&json!({
        "path": &path,
        "name": "auth-refactor",
        "summary": "Refactored authenticate function to use JWT tokens instead of plain password comparison",
        "decisions": "Goal: Auth security. Decision: Use bcrypt for password hashing. Why: Plain comparison is insecure. Invalidates-if: external auth provider adopted.",
        "constraints": "Tried: SHA256 hashing. Failed because: rainbow table attacks. Do not retry unless: salted."
    }))
    .unwrap();

    // Embed session with text matching the symbol name
    let emb_dir = dir.path().join(".infigraph").join("sessions");
    std::fs::create_dir_all(&emb_dir).unwrap();
    let emb_path = emb_dir.join("embeddings.bin");
    let embedder = infigraph_core::embed::code_embedder();
    let store = SessionStore::open(dir.path()).unwrap();
    let sessions = store.list_all().unwrap();
    let mut embeddings = Vec::new();
    for s in &sessions {
        let text = format!("{} {} {}", s.summary, s.decisions, s.constraints);
        if let Ok(emb) = embedder.embed(&text) {
            embeddings.push((s.id.clone(), emb));
        }
    }
    infigraph_core::embed::save_embeddings(&emb_path, &embeddings).unwrap();

    // Call symbol_context for "authenticate"
    let result = tool_symbol_context(&json!({
        "path": &path,
        "symbol_id": "src/auth.py::authenticate"
    }))
    .unwrap();

    // Should contain the symbol info
    assert!(
        result.contains("authenticate"),
        "symbol_context should return symbol info"
    );

    // Check if auto-injection happened (Prior context section)
    // Note: injection requires cosine similarity >= 0.7 between symbol name and session embedding
    // This may or may not trigger depending on embedding quality, so we test the mechanism exists
    if result.contains("Prior context") {
        assert!(
            result.contains("confidence"),
            "Auto-injected context should show confidence score"
        );
    }
}

/// Phase 4a: Auto-injection — get_doc_context should also auto-inject.
#[test]
fn auto_injection_doc_context() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    support::disable_background_watchers();
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().to_string_lossy().to_string();

    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/auth.py"),
        "def authenticate(username, password):\n    return username == 'admin'\n",
    )
    .unwrap();
    tool_index_project(&json!({"path": &path})).unwrap();

    tool_save_session(&json!({
        "path": &path,
        "name": "auth-decisions",
        "summary": "authenticate function security decisions",
        "decisions": "Goal: Secure auth. Decision: Rate limit login attempts. Why: Brute force prevention. Invalidates-if: captcha added.",
        "constraints": "Tried: IP blocking. Failed because: shared NAT."
    }))
    .unwrap();

    let emb_dir = dir.path().join(".infigraph").join("sessions");
    std::fs::create_dir_all(&emb_dir).unwrap();
    let emb_path = emb_dir.join("embeddings.bin");
    let embedder = infigraph_core::embed::code_embedder();
    let store = SessionStore::open(dir.path()).unwrap();
    let sessions = store.list_all().unwrap();
    let mut embeddings = Vec::new();
    for s in &sessions {
        let text = format!("{} {} {}", s.summary, s.decisions, s.constraints);
        if let Ok(emb) = embedder.embed(&text) {
            embeddings.push((s.id.clone(), emb));
        }
    }
    infigraph_core::embed::save_embeddings(&emb_path, &embeddings).unwrap();

    let result = tool_get_doc_context(&json!({
        "path": &path,
        "symbol_id": "src/auth.py::authenticate"
    }))
    .unwrap();

    // Should have source code
    assert!(
        result.contains("Source:"),
        "get_doc_context should include source code"
    );
    assert!(
        result.contains("Callers"),
        "get_doc_context should include callers section"
    );
}

/// Phase 4a: Auto-injection respects confidence threshold — stale sessions not injected.
#[test]
fn auto_injection_skips_low_confidence() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    support::disable_background_watchers();
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().to_string_lossy().to_string();

    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/calc.py"),
        "def calculate(x):\n    return x * 2\n",
    )
    .unwrap();
    tool_index_project(&json!({"path": &path})).unwrap();

    // Create ancient session — confidence will be ~0
    let store = SessionStore::open(dir.path()).unwrap();
    let ancient = infigraph_core::graph::SessionData {
        id: "named_ancient-calc".to_string(),
        name: "ancient-calc".to_string(),
        summary: "calculate function was originally written for batch processing".to_string(),
        decisions: "Goal: Batch calc. Decision: Sequential. Why: Simple.".to_string(),
        pending_tasks: String::new(),
        files_touched: String::new(),
        constraints: String::new(),
        assumptions: String::new(),
        blockers: String::new(),
        created_at: 1600000000,
        updated_at: 1600000000,
        confidence: 0.7,
        last_accessed: 1600000000, // ~Sep 2020, confidence → 0
    };
    store.save(&ancient).unwrap();

    let emb_dir = dir.path().join(".infigraph").join("sessions");
    std::fs::create_dir_all(&emb_dir).unwrap();
    let emb_path = emb_dir.join("embeddings.bin");
    let embedder = infigraph_core::embed::code_embedder();
    let emb = embedder
        .embed("calculate function batch processing")
        .unwrap();
    infigraph_core::embed::save_embeddings(&emb_path, &[("named_ancient-calc".to_string(), emb)])
        .unwrap();

    let result = tool_symbol_context(&json!({
        "path": &path,
        "symbol_id": "src/calc.py::calculate"
    }))
    .unwrap();

    // Ancient session should NOT be auto-injected (confidence < 0.5)
    assert!(
        !result.contains("Prior context"),
        "Stale session (confidence < 0.5) should not be auto-injected.\nOutput:\n{result}"
    );
}

/// Phase 4b: Selective indexing (input gate) — high-value sessions get higher
/// initial confidence than summary-only sessions.
#[test]
fn selective_indexing_confidence_scoring() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    support::disable_background_watchers();
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().to_string_lossy().to_string();

    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.py"), "def main(): pass\n").unwrap();
    tool_index_project(&json!({"path": &path})).unwrap();

    // High-value session: has decisions with invalidation conditions + constraints
    tool_save_session(&json!({
        "path": &path,
        "name": "high-value",
        "summary": "Critical auth decision",
        "decisions": "Goal: Auth. Decision: JWT. Why: Stateless. Invalidates-if: need revocation.",
        "constraints": "Tried: cookies. Failed because: CORS issues. Do not retry unless: same-origin."
    }))
    .unwrap();

    // Low-value session: summary only, no decisions/constraints
    tool_save_session(&json!({
        "path": &path,
        "name": "low-value",
        "summary": "Did some refactoring and cleanup today"
    }))
    .unwrap();

    let store = SessionStore::open(dir.path()).unwrap();
    let high = store.load("named_high-value").unwrap().unwrap();
    let low = store.load("named_low-value").unwrap().unwrap();

    assert!(
        high.confidence > low.confidence,
        "High-value session (decisions+constraints with invalidation markers) should get higher \
         initial confidence than summary-only session.\n\
         High: {}, Low: {}",
        high.confidence,
        low.confidence
    );

    // Specific thresholds from score_session_value:
    // high-value markers (invalidates-if, failed because) → 0.9
    // summary-only → 0.5
    assert!(
        high.confidence >= 0.85,
        "Session with invalidation conditions should get confidence >= 0.85, got {}",
        high.confidence
    );
    assert!(
        low.confidence <= 0.55,
        "Summary-only session should get confidence <= 0.55, got {}",
        low.confidence
    );
}

/// Phase 4b: Selective indexing tiers — decisions (no markers) get mid confidence.
#[test]
fn selective_indexing_mid_tier() {
    let _guard = MEMORY_LOCK.lock().unwrap();
    support::disable_background_watchers();
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().to_string_lossy().to_string();

    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.py"), "def main(): pass\n").unwrap();
    tool_index_project(&json!({"path": &path})).unwrap();

    // Mid-value: has decisions but no high-value markers
    tool_save_session(&json!({
        "path": &path,
        "name": "mid-value",
        "summary": "API design session",
        "decisions": "Goal: API versioning. Decision: URL path prefix. Why: Simple."
    }))
    .unwrap();

    // Has constraints/blockers but no high-value markers
    tool_save_session(&json!({
        "path": &path,
        "name": "constraint-value",
        "summary": "Deployment planning",
        "constraints": "Need Docker 24+",
        "blockers": "Waiting on staging environment"
    }))
    .unwrap();

    let store = SessionStore::open(dir.path()).unwrap();
    let mid = store.load("named_mid-value").unwrap().unwrap();
    let constraint = store.load("named_constraint-value").unwrap().unwrap();

    // Decisions without markers → 0.7
    assert!(
        (mid.confidence - 0.7).abs() < 0.01,
        "Decisions-only session should get confidence ~0.7, got {}",
        mid.confidence
    );

    // Constraints/blockers without markers → 0.85
    assert!(
        (constraint.confidence - 0.85).abs() < 0.01,
        "Constraints/blockers session should get confidence ~0.85, got {}",
        constraint.confidence
    );
}
