use std::io::{self, BufRead, Write};

use anyhow::Result;
use serde_json::{json, Value};

use infigraph_mcp::web;

fn main() -> Result<()> {
    // A probe child must answer before anything else initialises: it exists
    // only to find out whether one graph file opens, out of process, so a
    // damaged image cannot take a real daemon down with it.
    if infigraph_core::probe::run_if_probe_child() {
        return Ok(());
    }

    let args: Vec<String> = std::env::args().collect();

    // Pure introspection MUST be answered before any supervisor/worker/
    // lock/registry side effects (#61 / I-21): `infigraph-mcp --version`
    // used to fall through into the normal startup path, whose mcp.lock
    // acquisition requested a handover from the live server -- a version
    // probe killed the in-use MCP server out from under its clients
    // (observed right after installing a newer build, which makes the
    // takeover eager). Checked even before the `--worker` branch so a
    // stray combination can never reach the lock path either.
    if args.iter().skip(1).any(|a| a == "--version" || a == "-V") {
        println!(
            "infigraph-mcp {} (build {})",
            env!("CARGO_PKG_VERSION"),
            infigraph_core::build_hash()
        );
        return Ok(());
    }
    if args.iter().skip(1).any(|a| a == "--help" || a == "-h") {
        println!(
            "infigraph-mcp {} — MCP server for infigraph code intelligence\n\
             \n\
             Usage: infigraph-mcp [OPTIONS]\n\
             \n\
             Options:\n\
             \x20 --mcp        Serve MCP over stdio (default when stdin is a pipe)\n\
             \x20 --serve      Serve MCP over HTTP\n\
             \x20 --ui         Serve the web UI\n\
             \x20 --version    Print version and exit\n\
             \x20 --help       Print this help and exit",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }

    if args.iter().any(|a| a == "--worker") {
        return run_worker();
    }

    // #74/#199: once per start, in the supervisor -- a worker failing this would
    // only be restarted into the same failure. Workers still validate in
    // every `Infigraph::init*`.
    let startup_root = infigraph_core::project::resolve_project_root(
        &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    );
    let checked = infigraph_mcp::lifecycle::startup_phase(
        "settings",
        infigraph_mcp::lifecycle::startup_phase_budget(),
        move || infigraph_core::check_settings_at_startup(&startup_root),
    )
    .and_then(|checked| checked);
    match checked {
        Ok(backend) => mcp_log("INFO", &format!("backend: {backend}")),
        Err(e) => {
            mcp_log("ERROR", &format!("{e:#}"));
            eprintln!("infigraph-mcp: {e:#}");
            std::process::exit(2);
        }
    }

    // Supervisor mode: spawn self as --worker, and restart it if it crashes.
    // The worker already logs a reason for every exit path it controls
    // (panic, signal, stdin EOF, idle grace, supervisor-gone); the
    // supervisor itself had neither a panic hook nor a signal handler --
    // its own death (a panic, or a signal delivered directly to it rather
    // than the process group) was exactly as invisible as what an
    // uncatchable SIGKILL leaves behind. The worker's own
    // `spawn_parent_monitor` already notices and self-exits (logging that)
    // once this process is gone, so this handler only needs to log its own
    // reason and exit -- not explicitly clean up the worker.
    install_panic_hook();
    {
        let pid = std::process::id();
        ctrlc::set_handler(move || {
            mcp_log(
                "INFO",
                &format!(
                    "supervisor (pid {pid}): termination signal received{} -- exiting",
                    infigraph_mcp::signal_sender::describe()
                ),
            );
            std::process::exit(0);
        })
        .ok();
        // #123: after ctrlc, so the sender-capturing handler chains to it.
        infigraph_mcp::signal_sender::install();
    }

    if serves_stdio(&args) {
        return supervise_stdio(&args);
    }

    // `--serve`/`--ui` only: the worker reads no stdin, so there is no
    // request to proxy (R5.7/#36 covers the HTTP transport).
    let mut crashes = infigraph_mcp::recovery::WorkerCrashes::default();
    loop {
        let status = worker_command(&args)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()?;
        match worker_crash(&status) {
            Some(how) => restart_after_crash(&mut crashes, &how),
            None if planned_restart(&status).is_some() => continue,
            None => std::process::exit(status.code().unwrap_or(1)),
        }
    }
}

/// The worker, as the supervisor spawns it.
fn worker_command(args: &[String]) -> std::process::Command {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from(&args[0]));
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--worker");
    for arg in args.iter().skip(1).filter(|a| *a != "--worker") {
        cmd.arg(arg);
    }
    // Let the worker detect supervisor death and exit instead of
    // lingering as an orphan holding the instance lock.
    cmd.env(
        infigraph_mcp::lifecycle::SUPERVISOR_PID_ENV,
        std::process::id().to_string(),
    );
    if YIELDED_LOCK.load(std::sync::atomic::Ordering::Relaxed) {
        cmd.env(infigraph_mcp::mcp_lock::YIELDED_ENV, "1");
    }
    cmd
}

/// #20: a crash restarts the worker and touches no project. It used to wipe
/// and reindex every registered project (I-14); see `WorkerCrashes` for why
/// that never repaired anything. Returns only if the worker should restart.
fn restart_after_crash(crashes: &mut infigraph_mcp::recovery::WorkerCrashes, how: &str) {
    if crashes.record(std::time::Instant::now()) {
        mcp_log(
            "CRASH",
            &format!("worker crashed ({how}) -- restarting it; no project graph is touched"),
        );
        eprintln!("infigraph-mcp: worker crashed ({how}), restarting it");
        return;
    }
    let why = format!(
        "worker crashed ({how}) {} times within {}s -- a crash loop; giving up",
        infigraph_mcp::recovery::WORKER_CRASH_LOOP_LIMIT,
        infigraph_mcp::recovery::WORKER_CRASH_LOOP_WINDOW.as_secs()
    );
    mcp_log("CRASH", &why);
    eprintln!("infigraph-mcp: {why}");
    std::process::exit(1);
}

/// Whether the worker serves MCP over stdio -- the same branches `run`
/// takes: `--mcp`, or neither `--ui` nor `--serve` to park it elsewhere.
fn serves_stdio(args: &[String]) -> bool {
    args.iter().any(|a| a == "--mcp")
        || (!ui_enabled_from(args) && !args.iter().any(|a| a == "--serve"))
}

enum Event {
    Client(String),
    ClientClosed,
    Worker(u64, String),
    WorkerClosed(u64),
}

/// How often the supervisor checks the worker and the deadline while idle.
const SUPERVISE_TICK: std::time::Duration = std::time::Duration::from_millis(200);

/// R5.6 (#21): stand between the client and the worker so that no request
/// is ever left unanswered -- see `infigraph_mcp::proxy`. Worker generations
/// number each spawn, so output from a worker already replaced is ignored.
fn supervise_stdio(args: &[String]) -> Result<()> {
    use infigraph_mcp::proxy::Outstanding;
    use std::sync::mpsc::RecvTimeoutError;

    let (tx, rx) = std::sync::mpsc::channel::<Event>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if tx.send(Event::Client(line)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Event::ClientClosed);
        });
    }

    let deadline = infigraph_mcp::proxy::call_timeout();
    let mut outstanding = Outstanding::default();
    let mut crashes = infigraph_mcp::recovery::WorkerCrashes::default();
    let mut client_open = true;
    let mut backlog: Vec<String> = Vec::new();
    let mut generation = 0u64;

    loop {
        generation += 1;
        let mut child = worker_command(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()?;
        let mut to_worker = Some(spawn_worker_writer(child.stdin.take().expect("piped")));
        spawn_worker_reader(child.stdout.take().expect("piped"), generation, tx.clone());
        for line in backlog.drain(..) {
            if let Some(w) = &to_worker {
                let _ = w.send(line);
            }
        }
        if !client_open {
            to_worker = None;
        }

        let mut worker_out_open = true;
        let ended = loop {
            match rx.recv_timeout(SUPERVISE_TICK) {
                Ok(Event::Client(line)) => {
                    outstanding.track(&line, std::time::Instant::now());
                    if let Some(w) = &to_worker {
                        let _ = w.send(line);
                    }
                }
                Ok(Event::ClientClosed) => {
                    client_open = false;
                    to_worker = None;
                }
                Ok(Event::Worker(g, line)) if g == generation => {
                    if outstanding.settle(&line) {
                        write_line(&line)?;
                    }
                }
                Ok(Event::WorkerClosed(g)) if g == generation => worker_out_open = false,
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    unreachable!("the supervisor holds a sender")
                }
            }
            if let Some(status) = child.try_wait()? {
                // Replies the worker wrote before exiting are still in its
                // pipe; deliver them before failing what is left.
                let drain_until = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while worker_out_open && std::time::Instant::now() < drain_until {
                    match rx.recv_timeout(SUPERVISE_TICK) {
                        Ok(Event::Worker(g, line)) if g == generation => {
                            if outstanding.settle(&line) {
                                write_line(&line)?;
                            }
                        }
                        Ok(Event::WorkerClosed(g)) if g == generation => worker_out_open = false,
                        Ok(Event::Client(line)) => {
                            outstanding.track(&line, std::time::Instant::now());
                            backlog.push(line);
                        }
                        Ok(Event::ClientClosed) => client_open = false,
                        _ => {}
                    }
                }
                break Ended::Exited(status);
            }
            if let Some(late) = outstanding.overdue(std::time::Instant::now(), deadline) {
                break Ended::Overdue(late);
            }
        };

        match ended {
            Ended::Exited(status) => {
                let crash = worker_crash(&status);
                let cause = match &crash {
                    Some(how) => format!("the worker crashed ({how})"),
                    None => match planned_restart(&status) {
                        Some(why) => why.to_string(),
                        None => format!("the worker exited ({status})"),
                    },
                };
                for reply in outstanding
                    .fail_all(|c| format!("{cause} while serving {}; it was not completed", c.what))
                {
                    write_line(&reply.to_string())?;
                }
                match crash {
                    Some(how) if client_open => restart_after_crash(&mut crashes, &how),
                    None if client_open && planned_restart(&status).is_some() => {}
                    _ => std::process::exit(status.code().unwrap_or(1)),
                }
            }
            Ended::Overdue(late) => {
                let why = format!(
                    "{} got no reply within {}s -- restarting the worker",
                    late.what,
                    deadline.as_secs()
                );
                mcp_log("TIMEOUT", &why);
                eprintln!("infigraph-mcp: {why}");
                let _ = child.kill();
                let _ = child.wait();
                for reply in outstanding.fail_all(|c| {
                    if c.id == late.id {
                        format!(
                            "{} got no reply within {}s (INFIGRAPH_MCP_CALL_TIMEOUT_SECS); the \
                             worker was restarted",
                            c.what,
                            deadline.as_secs()
                        )
                    } else {
                        format!(
                            "{} was queued behind a {} call that hung; the worker was \
                             restarted before it ran",
                            c.what, late.what
                        )
                    }
                }) {
                    write_line(&reply.to_string())?;
                }
                if !client_open {
                    std::process::exit(0);
                }
            }
        }
    }
}

enum Ended {
    Exited(std::process::ExitStatus),
    Overdue(infigraph_mcp::proxy::Call),
}

/// Lines for the worker go through a thread of their own: a worker stuck
/// on a call stops reading, and once its pipe fills a direct write would
/// block the supervisor too -- deadline and all.
fn spawn_worker_writer(mut stdin: std::process::ChildStdin) -> std::sync::mpsc::Sender<String> {
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in rx {
            if writeln!(stdin, "{line}")
                .and_then(|()| stdin.flush())
                .is_err()
            {
                return;
            }
        }
    });
    tx
}

fn spawn_worker_reader(
    stdout: std::process::ChildStdout,
    generation: u64,
    tx: std::sync::mpsc::Sender<Event>,
) {
    std::thread::spawn(move || {
        for line in io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(Event::Worker(generation, line)).is_err() {
                return;
            }
        }
        let _ = tx.send(Event::WorkerClosed(generation));
    });
}

/// One line to the client. A client that is gone ends the supervisor.
fn write_line(line: &str) -> Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

/// Set once a worker of this supervisor hands `mcp.lock` over; every
/// later worker is spawned with `mcp_lock::YIELDED_ENV` so it never asks
/// for the lock back.
static YIELDED_LOCK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Why the worker exited so the supervisor would start a fresh one, if it
/// did: its watchdog (#19), or a handover of `mcp.lock` to a newer build
/// (R2.3.2), which must not take this session's server down with it.
fn planned_restart(status: &std::process::ExitStatus) -> Option<&'static str> {
    match status.code() {
        Some(infigraph_mcp::lifecycle::WATCHDOG_RESTART_EXIT) => {
            Some("the worker restarted to recover resources (see mcp.log)")
        }
        Some(infigraph_mcp::lifecycle::HANDOVER_EXIT) => {
            YIELDED_LOCK.store(true, std::sync::atomic::Ordering::Relaxed);
            Some("the worker handed mcp.lock to a newer build and restarted")
        }
        _ => None,
    }
}

/// How the worker crashed, if its exit was a crash: SIGSEGV on Unix, an
/// unhandled exception (a negative exit code) on Windows.
fn worker_crash(status: &std::process::ExitStatus) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        (status.signal() == Some(libc::SIGSEGV)).then(|| "SIGSEGV".to_string())
    }
    #[cfg(windows)]
    {
        status
            .code()
            .filter(|code| *code < 0)
            .map(|code| format!("exit {code}"))
    }
}

/// Spawns a background thread that proactively starts watching the
/// directory this MCP server was launched in. Thin wrapper around the
/// library crate's `recovery::start_daemon_watcher_for_startup_dir`, which
/// does the actual gating (daemon mode + `[watch].auto_start_on_boot`) --
/// kept there rather than here so it's reachable from `infigraph-mcp`'s
/// integration tests, since this `main.rs` is a separate `[[bin]]` target.
/// Runs on its own thread so this doesn't delay this server's readiness to
/// serve the MCP client's `initialize` handshake.
fn start_daemon_watcher_for_startup_dir(startup_dir: Option<&std::path::Path>) {
    let startup_dir = startup_dir.map(|p| p.to_path_buf());
    infigraph_mcp::lifecycle::background_startup_phase("startup_true_up", move || {
        infigraph_mcp::recovery::start_daemon_watcher_for_startup_dir(startup_dir.as_deref());
    });
}

fn run_worker() -> Result<()> {
    install_panic_hook();

    // Exit if the supervisor dies, instead of surviving as an orphan
    // (PPID 1) that holds the instance lock forever. Stdin EOF alone is
    // not sufficient: --ui/--serve modes never read stdin.
    infigraph_mcp::lifecycle::spawn_parent_monitor();

    let _ = rayon::ThreadPoolBuilder::new()
        .stack_size(32 * 1024 * 1024)
        .build_global();

    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(run)
        .expect("failed to spawn MCP worker thread")
        .join()
        .expect("MCP worker thread panicked")
}

fn mcp_log(level: &str, msg: &str) {
    infigraph_mcp::mcp_log(level, msg);
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_string()
        };
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let bt = std::backtrace::Backtrace::force_capture();
        mcp_log("PANIC", &format!("{payload} at {location}\n{bt}"));
        eprintln!("PANIC: {payload} at {location}");
    }));
}

fn ui_enabled_from(args: &[String]) -> bool {
    args.iter().any(|a| a == "--ui" || a.starts_with("--ui="))
}

fn run() -> Result<()> {
    let mcp_lock_outcome = infigraph_mcp::lifecycle::required_startup_phase(
        "mcp_lock",
        infigraph_mcp::mcp_lock::acquire_with_takeover,
    );
    let (is_primary, mcp_lock) = match mcp_lock_outcome {
        infigraph_mcp::mcp_lock::AcquireOutcome::Primary(lock) => {
            mcp_log("INFO", "Acquired mcp.lock — running as primary");
            (true, Some(lock))
        }
        infigraph_mcp::mcp_lock::AcquireOutcome::Secondary => {
            mcp_log(
                "WARN",
                "Another MCP instance holds mcp.lock — will not start a new watcher",
            );
            infigraph_mcp::tools::watch::disable_watchers();
            (false, None)
        }
    };

    // Unconditional, independent of `is_primary`: a secondary must still be
    // able to prune a stale-build daemon for the project it was launched
    // in, and must still run its own true-up reindex -- neither is "start a
    // new watcher" (the thing `is_primary` actually needs to arbitrate).
    // `start_daemon_watcher_for_startup_dir`'s own `auto_start_watch`/
    // `auto_start_doc_watch` calls already no-op safely on a secondary via
    // `watchers_disabled()` (set above), so calling this unconditionally
    // can't produce a duplicate live watcher -- see that function's doc
    // comment for the daemon-mode + `auto_start_on_boot` gating it still
    // does internally.
    let project = infigraph_mcp::tools::helpers::startup_project();
    start_daemon_watcher_for_startup_dir(Some(&project));

    if let Some(mut lock) = mcp_lock {
        std::thread::spawn(move || loop {
            std::thread::sleep(infigraph_mcp::mcp_lock::heartbeat_interval());
            if infigraph_mcp::mcp_lock::heartbeat_and_check_handover(&mut lock) {
                infigraph_mcp::mcp_lock::release_to_successor(lock);
                infigraph_mcp::lifecycle::exit_between_calls(
                    infigraph_mcp::lifecycle::HANDOVER_EXIT,
                );
            }
        });
    }

    let args: Vec<String> = std::env::args().collect();
    let mcp_mode = args.iter().any(|a| a == "--mcp");
    let transport = if mcp_mode { "stdio" } else { "http" };
    let project_path = project.to_string_lossy().to_string();
    let instance_info = infigraph_core::instances::InstanceInfo::current(&project_path, transport);
    let _instance_guard =
        match infigraph_mcp::lifecycle::required_startup_phase("register_instance", move || {
            infigraph_core::instances::register_instance(&instance_info)
        }) {
            Ok(guard) => Some(guard),
            Err(e) => {
                mcp_log("WARN", &format!("Failed to register instance: {e:#}"));
                None
            }
        };

    // R5.4 (#79): SIGTERM/SIGINT must deregister this instance and exit
    // cleanly. Without a handler the signal kills the process mid-anything
    // with Drop handlers skipped, leaving a stale instance registration
    // (reaped only later) and a stale lock payload. The handler does the
    // one durable cleanup a signal context can do safely -- remove our own
    // registration file -- then exits; the flock releases with the
    // process, and the payload staleness is covered by holder_is_alive.
    {
        let pid = std::process::id();
        ctrlc::set_handler(move || {
            let _ = std::fs::remove_file(infigraph_core::instances::instance_path(pid));
            mcp_log(
                "INFO",
                &format!(
                    "termination signal received{} -- instance deregistered, exiting",
                    infigraph_mcp::signal_sender::describe()
                ),
            );
            std::process::exit(0);
        })
        .ok();
        // #123: after ctrlc, so the sender-capturing handler chains to it.
        infigraph_mcp::signal_sender::install();
    }

    let pid = std::process::id();
    let reaped = infigraph_mcp::lifecycle::required_startup_phase("reap_orphans", move || {
        infigraph_core::instances::reap_orphans_once(pid)
    });
    if reaped > 0 {
        mcp_log(
            "INFO",
            &format!("Reaped {reaped} orphaned instance(s) on startup"),
        );
    }

    infigraph_mcp::lifecycle::spawn_self_watch();

    std::thread::spawn(|| loop {
        std::thread::sleep(infigraph_core::instances::reap_scan_interval());
        let reaped = infigraph_core::instances::reap_orphans_once(std::process::id());
        if reaped > 0 {
            mcp_log(
                "INFO",
                &format!("Reaped {reaped} orphaned instance(s) (periodic scan)"),
            );
        }
    });

    let ui_enabled = ui_enabled_from(&args);
    let port: u16 = args
        .iter()
        .find(|a| a.starts_with("--port="))
        .and_then(|a| a.strip_prefix("--port="))
        .and_then(|p| p.parse().ok())
        .unwrap_or(9749);

    let serve_mode = args.iter().any(|a| a == "--serve");
    let not_ready = args.iter().any(|a| a == "--not-ready");
    if not_ready {
        web::set_ready(false);
    }
    let mcp_port: u16 = args
        .iter()
        .find(|a| a.starts_with("--mcp-port="))
        .and_then(|a| a.strip_prefix("--mcp-port="))
        .and_then(|p| p.parse().ok())
        .unwrap_or(8642);
    let health_path: String = args
        .iter()
        .find(|a| a.starts_with("--health-path="))
        .and_then(|a| a.strip_prefix("--health-path="))
        .unwrap_or("/health")
        .to_string();

    if ui_enabled {
        if web::start_ui_server(port) {
            eprintln!("Infigraph UI running at http://localhost:{}", port);
            eprintln!("Open: http://localhost:{}/?path=/your/project", port);
        } else {
            eprintln!(
                "Infigraph UI port {} already in use — skipping UI (MCP active)",
                port
            );
        }
        if !mcp_mode && !serve_mode {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
    }

    if serve_mode {
        if web::start_mcp_http_server(mcp_port, is_primary, &health_path) {
            eprintln!(
                "Infigraph MCP HTTP server at http://{}",
                web::bind_addr("INFIGRAPH_MCP_BIND", mcp_port)
            );
        } else {
            eprintln!("Infigraph MCP HTTP port {} already in use", mcp_port);
        }
        if !mcp_mode {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
    }

    mcp_log("INFO", "MCP server started");

    let stdin = io::stdin();
    let stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                mcp_log("INFO", &format!("stdin closed: {e}"));
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write_response(
                    &stdout,
                    json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": { "code": -32700, "message": format!("Parse error: {e}") }
                    }),
                )?;
                continue;
            }
        };

        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");

        mcp_log("DEBUG", &format!("method={method}"));

        let response = match method {
            "initialize" => handle_initialize(&id, is_primary),
            "tools/list" => handle_tools_list(&id),
            "tools/call" => {
                let tool = request
                    .pointer("/params/name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("?");
                mcp_log("DEBUG", &format!("tool_call={tool}"));
                handle_tools_call(&id, &request)
            }
            "notifications/initialized" | "notifications/cancelled" => continue,
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("Method not found: {method}") }
            }),
        };

        write_response(&stdout, response)?;
    }

    mcp_log("INFO", "stdin loop exited");

    // Reaching here means the MCP client's stdio connection closed — the
    // two OTHER "loop forever" branches earlier in this function (pure
    // --ui-only, pure --serve-only, neither with --mcp) return before ever
    // entering the stdin read loop, since they never had a stdio client to
    // begin with; they are legitimate standing daemons and are untouched
    // by this block. If --ui is also active, someone might still have the
    // local web UI open, so don't exit instantly — but don't loop forever
    // either (DESIGN-hardening.md I-5 / R2.2.3): self-terminate after an
    // idle grace period.
    if ui_enabled {
        let grace = infigraph_mcp::idle::idle_grace_period();
        let poll = infigraph_mcp::idle::idle_poll_interval();
        mcp_log(
            "INFO",
            &format!(
                "MCP client disconnected; UI still serving — exiting after {}s idle unless reconnected",
                grace.as_secs()
            ),
        );
        let stdin_closed_at = std::time::Instant::now();
        loop {
            std::thread::sleep(poll);
            if infigraph_mcp::idle::should_exit_idle(stdin_closed_at.elapsed(), grace) {
                mcp_log(
                    "INFO",
                    &format!(
                        "Idle grace period ({}s) elapsed since MCP client disconnected — exiting",
                        grace.as_secs()
                    ),
                );
                break;
            }
        }
    }

    Ok(())
}

fn write_response(stdout: &io::Stdout, response: Value) -> Result<()> {
    let msg = serde_json::to_string(&response)?;
    let mut out = stdout.lock();
    out.write_all(msg.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

fn handle_initialize(id: &Value, is_primary: bool) -> Value {
    infigraph_mcp::handle_initialize(id, is_primary)
}

fn handle_tools_list(id: &Value) -> Value {
    infigraph_mcp::handle_tools_list(id)
}

fn handle_tools_call(id: &Value, request: &Value) -> Value {
    infigraph_mcp::handle_tools_call(id, request)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bare_mcp_flag_does_not_enable_ui() {
        // Regression test: `--mcp` alone previously triggered the UI keep-alive
        // loop, leaking a worker process that never exits on stdin EOF.
        assert!(!ui_enabled_from(&args(&["--mcp"])));
    }

    #[test]
    fn no_flags_does_not_enable_ui() {
        assert!(!ui_enabled_from(&args(&[])));
    }

    #[test]
    fn ui_flag_enables_ui() {
        assert!(ui_enabled_from(&args(&["--ui"])));
    }

    #[test]
    fn ui_flag_with_value_enables_ui() {
        assert!(ui_enabled_from(&args(&["--ui=3000"])));
    }

    #[test]
    fn mcp_and_ui_together_enables_ui() {
        assert!(ui_enabled_from(&args(&["--mcp", "--ui"])));
    }
}
