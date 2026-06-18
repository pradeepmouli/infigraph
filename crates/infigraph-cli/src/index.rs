use std::path::Path;

use anyhow::{Context, Result};
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;

pub(crate) fn cmd_index(root: &Path, full: bool, no_embed: bool) -> Result<()> {
    if full {
        let tg_dir = root.join(".infigraph");
        if tg_dir.exists() {
            // Sessions are in a separate DB at .infigraph/sessions/db/ — preserve them
            let sessions_dir = tg_dir.join("sessions");
            let sessions_backup = root.join(".infigraph-sessions-backup");
            let had_sessions = sessions_dir.exists();
            if had_sessions {
                let _ = std::fs::rename(&sessions_dir, &sessions_backup);
            }
            std::fs::remove_dir_all(&tg_dir)?;
            if had_sessions {
                std::fs::create_dir_all(&tg_dir)?;
                let _ = std::fs::rename(&sessions_backup, &sessions_dir);
            }
            println!("Cleaned .infigraph/ for full reindex (sessions preserved)");
        }
    }

    let registry = crate::full_registry(Some(root))?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    println!("Indexing project...");
    let result = prism.index()?;
    println!(
        "Indexed {}/{} files",
        result.indexed_files, result.total_files
    );

    let mut by_lang: std::collections::HashMap<&str, (usize, usize)> =
        std::collections::HashMap::new();
    for ext in &result.extractions {
        let entry = by_lang.entry(&ext.language).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += ext.symbols.len();
    }
    for (lang, (files, symbols)) in &by_lang {
        println!("  {}: {} files, {} symbols", lang, files, symbols);
    }

    if result.resolve_stats.total_calls > 0 {
        println!("{}", result.resolve_stats);
    }

    // Detect cross-cutting concerns (authorization, caching, transactions, etc.)
    if let Some(store) = prism.store() {
        match infigraph_core::concerns::detect_cross_cutting(store) {
            Ok(matches) if !matches.is_empty() => {
                println!("Detected {} cross-cutting concerns", matches.len());
            }
            Ok(_) => {}
            Err(e) => eprintln!("warning: concern detection failed: {e}"),
        }
        match infigraph_core::config::detect_config_bindings(store) {
            Ok(bindings) if !bindings.is_empty() => {
                println!("Detected {} config bindings", bindings.len());
            }
            Ok(_) => {}
            Err(e) => eprintln!("warning: config binding detection failed: {e}"),
        }
        match infigraph_core::reflection::detect_reflection_sites(store, root) {
            Ok(sites) if !sites.is_empty() => {
                let resolved = sites.iter().filter(|s| s.resolved_to.is_some()).count();
                println!("Detected {} reflection sites ({} resolved)", sites.len(), resolved);
            }
            Ok(_) => {}
            Err(e) => eprintln!("warning: reflection detection failed: {e}"),
        }
        match infigraph_core::taint::detect_taint_flows(store, root) {
            Ok(flows) if !flows.is_empty() => {
                let active = flows.iter().filter(|f| !f.sanitized).count();
                println!("Detected {} taint flows ({} active, {} sanitized)", flows.len(), active, flows.len() - active);
            }
            Ok(_) => {}
            Err(e) => eprintln!("warning: taint analysis failed: {e}"),
        }
        match infigraph_core::taint::interprocedural::detect_interprocedural_taint(store, root, 5) {
            Ok(flows) if !flows.is_empty() => {
                println!("Detected {} inter-procedural taint flows", flows.len());
            }
            Ok(_) => {}
            Err(e) => eprintln!("warning: inter-procedural taint failed: {e}"),
        }
        match infigraph_core::taint::dynamic_urls::detect_dynamic_urls(store, root) {
            Ok(urls) if !urls.is_empty() => {
                let matched = urls.iter().filter(|u| u.matched_route.is_some()).count();
                println!("Detected {} dynamic URLs ({} matched to routes)", urls.len(), matched);
            }
            Ok(_) => {}
            Err(e) => eprintln!("warning: dynamic URL detection failed: {e}"),
        }
    }

    let stats = prism.stats()?;
    println!("\n{}", stats);

    // Hint: suggest .infigraphignore if none exists
    if !root.join(".infigraphignore").exists() {
        eprintln!("\nhint: Create .infigraphignore in the project root to exclude non-source directories.");
        eprintln!("      Common entries:");
        eprintln!("        target/        # Rust build output");
        eprintln!("        build/         # build output (Gradle, CMake, etc.)");
        eprintln!("        dist/          # distribution bundles");
        eprintln!("        out/           # compiler/IDE output");
        eprintln!("        vendor/        # vendored dependencies (Go, Ruby)");
        eprintln!("        bin/           # compiled binaries");
        eprintln!("        obj/           # intermediate build objects (.NET, C++)");
        eprintln!("        generated/     # auto-generated code");
        eprintln!("        third_party/   # third-party source copies");
        eprintln!("        CMakeFiles/    # CMake internal files");
        eprintln!("      One entry per line. Lines starting with # are comments.");
    }

    // Compute and save embeddings — only for new/changed symbols
    if no_embed {
        auto_scip(root, &result, prism.store())?;
        return Ok(());
    }
    {
        let store = prism.store().context("graph not initialized")?;
        let changed: Vec<&str> = result.extractions.iter().map(|e| e.file.as_str()).collect();
        let count = infigraph_core::embed::update_embeddings(store, root, &changed)?;
        println!("Saved {} embeddings to .infigraph/embeddings.bin", count);
    }

    // Auto-index documents (PDF, DOCX, XML, Markdown, etc.)
    match crate::commands::cmd_index_docs(root) {
        Ok(()) => {}
        Err(e) => eprintln!("warning: document indexing failed: {e}"),
    }

    // Drop prism to release the GraphStore handle before background SCIP
    let detected_languages: std::collections::HashSet<String> = result
        .extractions
        .iter()
        .map(|e| e.language.clone())
        .collect();
    drop(prism);

    // SCIP enrichment in a detached child process — parent returns immediately.
    spawn_scip_child_process(root, &detected_languages);

    Ok(())
}

fn spawn_scip_child_process(root: &Path, detected_languages: &std::collections::HashSet<String>) {
    use crate::scip_download;

    let indexers = scip_download::indexers_for_languages(detected_languages);
    if indexers.is_empty() {
        return;
    }

    let count = indexers.len();
    let indexer_names: Vec<&str> = indexers.iter().map(|i| i.binary_name).collect();
    println!(
        "SCIP enrichment starting in background ({count} indexer(s): {})...",
        indexer_names.join(", ")
    );

    let langs: String = detected_languages
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join(",");

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };

    let log_path = root.join(".infigraph").join("scip-enrich.log");
    let stderr_target = match std::fs::File::create(&log_path) {
        Ok(f) => std::process::Stdio::from(f),
        Err(_) => std::process::Stdio::null(),
    };

    let _ = std::process::Command::new(exe)
        .arg("scip-enrich")
        .arg("--languages")
        .arg(&langs)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr_target)
        .spawn();

    eprintln!("  Log: {}", log_path.display());
}

pub(crate) fn on_path(cmd: &str) -> bool {
    let lookup = if cfg!(windows) { "where" } else { "which" };
    std::process::Command::new(lookup)
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub(crate) fn import_scip_and_cleanup(
    root: &Path,
    scip_path: Option<&std::path::Path>,
    existing_store: Option<&infigraph_core::graph::GraphStore>,
) {
    let scip_out = scip_path
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| root.join("index.scip"));
    if !scip_out.exists() {
        return;
    }

    if let Some(store) = existing_store {
        match infigraph_core::scip::import_scip_index(&scip_out, store, Some(root)) {
            Ok(stats) => println!(
                "Auto-SCIP: enriched {} symbols, {} added, {} references, {} new symbols, {} corrections learned",
                stats.symbols_enriched, stats.relations_added, stats.references_added, stats.symbols_added, stats.corrections_learned
            ),
            Err(e) => eprintln!("Auto-SCIP: import failed: {e}"),
        }
        let _ = std::fs::remove_file(&scip_out);
        return;
    }

    let registry = match bundled_registry() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Auto-SCIP: import failed: {e}");
            return;
        }
    };
    let mut prism = match Infigraph::open(root, registry) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Auto-SCIP: import failed: {e}");
            return;
        }
    };
    if prism.init().is_err() {
        return;
    }
    let store = match prism.store() {
        Some(s) => s,
        None => return,
    };
    match infigraph_core::scip::import_scip_index(&scip_out, store, Some(root)) {
        Ok(stats) => println!(
            "Auto-SCIP: enriched {} symbols, {} added, {} references, {} new symbols, {} corrections learned",
            stats.symbols_enriched, stats.relations_added, stats.references_added, stats.symbols_added, stats.corrections_learned
        ),
        Err(e) => eprintln!("Auto-SCIP: import failed: {e}"),
    }
    let _ = std::fs::remove_file(&scip_out);
}

/// Foreground SCIP execution using scip_download catalog for all detected languages.
pub(crate) fn auto_scip(
    root: &Path,
    result: &infigraph_core::IndexResult,
    store: Option<&infigraph_core::graph::GraphStore>,
) -> Result<()> {
    use crate::scip_download;
    use std::collections::HashSet;

    let detected: HashSet<String> = result
        .extractions
        .iter()
        .map(|e| e.language.clone())
        .collect();
    if detected.is_empty() {
        return Ok(());
    }

    let indexers = scip_download::indexers_for_languages(&detected);
    if indexers.is_empty() {
        return Ok(());
    }

    println!(
        "Auto-SCIP: found {} applicable indexer(s) for detected languages",
        indexers.len()
    );

    // Parallel download: ensure all indexer binaries are available
    let binaries: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = indexers
            .iter()
            .map(|idx| s.spawn(move || (*idx, scip_download::ensure_indexer(idx))))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Sequential run: each indexer produces index.scip, import, cleanup
    for (indexer, bin_path) in &binaries {
        let Some(bin) = bin_path else { continue };
        if !should_run_indexer(root, indexer) {
            continue;
        }

        let cmd_str = bin.to_string_lossy();
        let extra = scip_download::extra_runtime_paths();
        let extra_path = if extra.is_empty() {
            None
        } else {
            Some(extra.as_str())
        };

        if indexer.binary_name == "scip-java" {
            let has_gradle = root.join("build.gradle").exists()
                || root.join("build.gradle.kts").exists()
                || root.join("settings.gradle").exists()
                || root.join("settings.gradle.kts").exists();
            let has_maven = root.join("pom.xml").exists();

            if has_gradle && has_maven {
                let primary = if root.join("settings.gradle").exists()
                    || root.join("settings.gradle.kts").exists()
                {
                    "gradle"
                } else {
                    "maven"
                };
                let fallback = if primary == "gradle" {
                    "maven"
                } else {
                    "gradle"
                };

                println!("Auto-SCIP: detected both Maven and Gradle, trying {primary}");
                let primary_args = ["index", "--build-tool", primary];
                if run_scip_indexer(
                    root,
                    &cmd_str,
                    &primary_args,
                    indexer.binary_name,
                    extra_path,
                ) {
                    import_scip_and_cleanup(root, None, store);
                } else {
                    println!("Auto-SCIP: {primary} failed, falling back to {fallback}");
                    let fallback_args = ["index", "--build-tool", fallback];
                    if run_scip_indexer(
                        root,
                        &cmd_str,
                        &fallback_args,
                        indexer.binary_name,
                        extra_path,
                    ) {
                        import_scip_and_cleanup(root, None, store);
                    }
                }
            } else if run_scip_indexer(
                root,
                &cmd_str,
                indexer.scip_args,
                indexer.binary_name,
                extra_path,
            ) {
                import_scip_and_cleanup(root, None, store);
            }
            continue;
        }

        if run_scip_indexer(
            root,
            &cmd_str,
            indexer.scip_args,
            indexer.binary_name,
            extra_path,
        ) {
            import_scip_and_cleanup(root, None, store);
        }
    }

    Ok(())
}

pub(crate) fn run_scip_indexer(
    root: &Path,
    cmd: &str,
    args: &[&str],
    label: &str,
    extra_path: Option<&str>,
) -> bool {
    println!("Auto-SCIP: running {label}...");
    let scip_out = root.join("index.scip");
    let mut command = std::process::Command::new(cmd);
    command.args(args).current_dir(root);
    if let Some(extra) = extra_path {
        let path = std::env::var("PATH").unwrap_or_default();
        let sep = if cfg!(windows) { ";" } else { ":" };
        command.env("PATH", format!("{extra}{sep}{path}"));
    }
    {
        let ig = crate::scip_download::infigraph_dir();
        let java_macos = ig.join("java").join("Contents").join("Home");
        if java_macos.exists() {
            command.env("JAVA_HOME", &java_macos);
        } else {
            let java_home = ig.join("java");
            if java_home.join("bin").exists() {
                command.env("JAVA_HOME", &java_home);
            }
        }
        let dotnet_root = ig.join("dotnet");
        if dotnet_root.exists() {
            command.env("DOTNET_ROOT", &dotnet_root);
        }
    }
    match command.status() {
        Ok(s) if s.success() && scip_out.exists() => true,
        Ok(s) => {
            eprintln!("Auto-SCIP: {label} exited with {s}");
            false
        }
        Err(e) => {
            eprintln!("Auto-SCIP: failed to run {label}: {e}");
            false
        }
    }
}

/// Entry point for the hidden `scip-enrich` subcommand (spawned by `index`).
pub(crate) fn cmd_scip_enrich(root: &Path, detected_languages: &std::collections::HashSet<String>) {
    auto_scip_background(root, detected_languages);
}

/// Background SCIP pipeline: download binaries, run indexers in parallel, import sequentially.
fn auto_scip_background(root: &Path, detected_languages: &std::collections::HashSet<String>) {
    use crate::scip_download;

    let indexers = scip_download::indexers_for_languages(detected_languages);
    if indexers.is_empty() {
        return;
    }

    // Parallel download: ensure all indexer binaries are available
    let binaries: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = indexers
            .iter()
            .map(|idx| s.spawn(move || (*idx, scip_download::ensure_indexer(idx))))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Filter to runnable indexers and build per-indexer tasks
    let scip_tmp = root.join(".infigraph").join("scip-tmp");
    let _ = std::fs::create_dir_all(&scip_tmp);

    let tasks: Vec<_> = binaries
        .into_iter()
        .filter_map(|(indexer, bin_path)| {
            let bin = bin_path?;
            if !should_run_indexer(root, indexer) {
                return None;
            }
            let output_path = scip_tmp.join(format!("{}.scip", indexer.binary_name));
            Some((indexer, bin, output_path))
        })
        .collect();

    if tasks.is_empty() {
        let _ = std::fs::remove_dir_all(&scip_tmp);
        return;
    }

    // Part A: Run indexers in parallel with per-indexer output paths
    let results: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = tasks
            .iter()
            .map(|(indexer, bin, output_path)| {
                s.spawn(move || {
                    let success = run_scip_indexer_to(root, bin, indexer, output_path);
                    (indexer.binary_name, output_path.clone(), success)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Part B: Import results sequentially (Kuzu graph is single-writer)
    let registry = match bundled_registry() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Auto-SCIP: import failed: {e}");
            return;
        }
    };
    let mut prism = match Infigraph::open(root, registry) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Auto-SCIP: import failed: {e}");
            return;
        }
    };
    if prism.init().is_err() {
        return;
    }
    let store = match prism.store() {
        Some(s) => s,
        None => return,
    };

    for (label, scip_path, success) in &results {
        if *success && scip_path.exists() {
            match infigraph_core::scip::import_scip_index(scip_path, store, Some(root)) {
                Ok(stats) => eprintln!(
                    "Auto-SCIP: {label} enriched {} symbols, {} added, {} references, {} new symbols, {} corrections learned",
                    stats.symbols_enriched, stats.relations_added, stats.references_added, stats.symbols_added, stats.corrections_learned
                ),
                Err(e) => eprintln!("Auto-SCIP: {label} import failed: {e}"),
            }
        }
        let _ = std::fs::remove_file(scip_path);
    }

    let _ = std::fs::remove_dir_all(&scip_tmp);

    // Embed any new symbols SCIP added (skips existing embeddings)
    let root_buf = root.to_path_buf();
    let pre_count = infigraph_core::embed::embedding_count(&root_buf);
    match infigraph_core::embed::update_embeddings(store, &root_buf, &[]) {
        Ok(n) => {
            let new = n.saturating_sub(pre_count);
            if new > 0 {
                eprintln!("Auto-SCIP: embedded {new} new symbols from SCIP enrichment");
            }
        }
        Err(e) => eprintln!("Auto-SCIP: embedding update failed: {e}"),
    }

    eprintln!("Auto-SCIP: background enrichment complete.");
}

fn should_run_indexer(root: &Path, indexer: &crate::scip_download::ScipIndexer) -> bool {
    if indexer.binary_name == "scip-clang" && !root.join("compile_commands.json").exists() {
        eprintln!("Auto-SCIP: skipping scip-clang — compile_commands.json not found");
        return false;
    }
    if indexer.binary_name == "scip-ruby" {
        let has_gemspec = std::fs::read_dir(root)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .any(|e| e.path().extension().is_some_and(|ext| ext == "gemspec"))
            })
            .unwrap_or(false);
        if !has_gemspec {
            eprintln!("Auto-SCIP: skipping scip-ruby — no .gemspec found");
            return false;
        }
    }
    true
}

fn run_scip_indexer_to(
    root: &Path,
    bin: &Path,
    indexer: &crate::scip_download::ScipIndexer,
    output_path: &Path,
) -> bool {
    let label = indexer.binary_name;
    eprintln!("Auto-SCIP: running {label}...");

    let cmd_str = bin.to_string_lossy();
    let extra = crate::scip_download::extra_runtime_paths();
    let extra_path = if extra.is_empty() {
        None
    } else {
        Some(extra.as_str())
    };

    if indexer.binary_name == "scip-java" {
        return run_scip_java(root, &cmd_str, output_path, extra_path);
    }

    run_scip_indexer_cmd(
        root,
        &cmd_str,
        indexer.scip_args,
        label,
        extra_path,
        indexer.output_flag,
        output_path,
    )
}

fn run_scip_java(root: &Path, cmd: &str, output_path: &Path, extra_path: Option<&str>) -> bool {
    let has_gradle = root.join("build.gradle").exists()
        || root.join("build.gradle.kts").exists()
        || root.join("settings.gradle").exists()
        || root.join("settings.gradle.kts").exists();
    let has_maven = root.join("pom.xml").exists();

    if has_gradle && has_maven {
        let primary =
            if root.join("settings.gradle").exists() || root.join("settings.gradle.kts").exists() {
                "gradle"
            } else {
                "maven"
            };
        let fallback = if primary == "gradle" {
            "maven"
        } else {
            "gradle"
        };

        eprintln!("Auto-SCIP: detected both Maven and Gradle, trying {primary}");
        let primary_args: Vec<&str> = vec!["index", "--build-tool", primary];
        if run_scip_indexer_cmd(
            root,
            cmd,
            &primary_args,
            "scip-java",
            extra_path,
            Some("--output"),
            output_path,
        ) {
            return true;
        }
        eprintln!("Auto-SCIP: {primary} failed, falling back to {fallback}");
        let fallback_args: Vec<&str> = vec!["index", "--build-tool", fallback];
        return run_scip_indexer_cmd(
            root,
            cmd,
            &fallback_args,
            "scip-java",
            extra_path,
            Some("--output"),
            output_path,
        );
    }

    run_scip_indexer_cmd(
        root,
        cmd,
        &["index"],
        "scip-java",
        extra_path,
        Some("--output"),
        output_path,
    )
}

fn run_scip_indexer_cmd(
    root: &Path,
    cmd: &str,
    args: &[&str],
    label: &str,
    extra_path: Option<&str>,
    output_flag: Option<&str>,
    output_path: &Path,
) -> bool {
    let mut command = std::process::Command::new(cmd);
    command.args(args).current_dir(root);

    if let Some(flag) = output_flag {
        command.arg(flag).arg(output_path);
    }

    if let Some(extra) = extra_path {
        let path = std::env::var("PATH").unwrap_or_default();
        let sep = if cfg!(windows) { ";" } else { ":" };
        command.env("PATH", format!("{extra}{sep}{path}"));
    }

    {
        let ig = crate::scip_download::infigraph_dir();
        let java_macos = ig.join("java").join("Contents").join("Home");
        if java_macos.exists() {
            command.env("JAVA_HOME", &java_macos);
        } else {
            let java_home = ig.join("java");
            if java_home.join("bin").exists() {
                command.env("JAVA_HOME", &java_home);
            }
        }
        let dotnet_root = ig.join("dotnet");
        if dotnet_root.exists() {
            command.env("DOTNET_ROOT", &dotnet_root);
        }
    }

    match command.status() {
        Ok(s) if s.success() => {
            if output_flag.is_none() {
                let default_out = root.join("index.scip");
                if default_out.exists() && default_out != output_path {
                    let _ = std::fs::rename(&default_out, output_path);
                }
            }
            output_path.exists()
        }
        Ok(s) => {
            eprintln!("Auto-SCIP: {label} exited with {s}");
            false
        }
        Err(e) => {
            eprintln!("Auto-SCIP: failed to run {label}: {e}");
            false
        }
    }
}
