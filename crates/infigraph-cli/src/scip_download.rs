use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::index::on_path;

pub(crate) struct ScipIndexer {
    pub binary_name: &'static str,
    pub scip_args: &'static [&'static str],
    pub output_flag: Option<&'static str>,
    pub download: DownloadStrategy,
}

impl ScipIndexer {
    /// The languages this indexer serves (`infigraph_core::scip::INDEXER_LANGUAGES`).
    pub fn lang_tags(&self) -> &'static [&'static str] {
        infigraph_core::scip::indexer_languages(self.binary_name)
    }
}

pub(crate) enum DownloadStrategy {
    GithubRelease {
        owner: &'static str,
        repo: &'static str,
        asset_pattern: fn(&str, &str) -> Option<String>,
    },
    NpmInstall {
        package: &'static str,
    },
    DotnetTool {
        package: &'static str,
    },
    ComposerInstall {
        package: &'static str,
    },
    DartPubInstall {
        package: &'static str,
    },
}

fn scip_go_asset(os: &str, arch: &str) -> Option<String> {
    let os_tag = match os {
        "macos" => "darwin",
        "linux" => "linux",
        _ => return None,
    };
    let arch_tag = match arch {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        _ => return None,
    };
    Some(format!("scip-go-{os_tag}-{arch_tag}.tar.gz"))
}

fn rust_analyzer_asset(os: &str, arch: &str) -> Option<String> {
    let os_tag = match os {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        "windows" => "pc-windows-msvc",
        _ => return None,
    };
    let arch_tag = match arch {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        _ => return None,
    };
    if os == "windows" {
        Some(format!("rust-analyzer-{arch_tag}-{os_tag}.zip"))
    } else {
        Some(format!("rust-analyzer-{arch_tag}-{os_tag}.gz"))
    }
}

fn scip_ruby_asset(os: &str, arch: &str) -> Option<String> {
    let os_tag = match os {
        "macos" => "darwin",
        "linux" => "linux",
        _ => return None,
    };
    let arch_tag = match arch {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        _ => return None,
    };
    Some(format!("scip-ruby-{arch_tag}-{os_tag}"))
}

fn scip_clang_asset(os: &str, arch: &str) -> Option<String> {
    match (os, arch) {
        ("macos", "aarch64") => Some("scip-clang-arm64-darwin".to_string()),
        ("linux", "x86_64") => Some("scip-clang-x86_64-linux".to_string()),
        _ => None,
    }
}

fn scip_java_asset(_os: &str, _arch: &str) -> Option<String> {
    Some("scip-java".to_string())
}

pub(crate) static CATALOG: &[ScipIndexer] = &[
    ScipIndexer {
        binary_name: "scip-typescript",
        scip_args: &["index", "--infer-tsconfig"],
        output_flag: Some("--output"),
        download: DownloadStrategy::NpmInstall {
            package: "@sourcegraph/scip-typescript",
        },
    },
    ScipIndexer {
        binary_name: "scip-python",
        scip_args: &["index", "--cwd", "."],
        output_flag: Some("--output"),
        download: DownloadStrategy::NpmInstall {
            package: "@sourcegraph/scip-python",
        },
    },
    ScipIndexer {
        binary_name: "rust-analyzer",
        scip_args: &["scip", "."],
        output_flag: Some("--output"),
        download: DownloadStrategy::GithubRelease {
            owner: "rust-lang",
            repo: "rust-analyzer",
            asset_pattern: rust_analyzer_asset,
        },
    },
    ScipIndexer {
        binary_name: "scip-java",
        scip_args: &["index"],
        output_flag: Some("--output"),
        download: DownloadStrategy::GithubRelease {
            owner: "sourcegraph",
            repo: "scip-java",
            asset_pattern: scip_java_asset,
        },
    },
    ScipIndexer {
        binary_name: "scip-go",
        scip_args: &["index"],
        output_flag: Some("-o"),
        download: DownloadStrategy::GithubRelease {
            owner: "sourcegraph",
            repo: "scip-go",
            asset_pattern: scip_go_asset,
        },
    },
    ScipIndexer {
        binary_name: "scip-ruby",
        scip_args: &["."],
        output_flag: None,
        download: DownloadStrategy::GithubRelease {
            owner: "sourcegraph",
            repo: "scip-ruby",
            asset_pattern: scip_ruby_asset,
        },
    },
    ScipIndexer {
        binary_name: "scip-dotnet",
        scip_args: &["index"],
        output_flag: Some("--output"),
        download: DownloadStrategy::DotnetTool {
            package: "scip-dotnet",
        },
    },
    ScipIndexer {
        binary_name: "scip-clang",
        scip_args: &[],
        output_flag: None,
        download: DownloadStrategy::GithubRelease {
            owner: "sourcegraph",
            repo: "scip-clang",
            asset_pattern: scip_clang_asset,
        },
    },
    ScipIndexer {
        binary_name: "scip-dart",
        scip_args: &["index"],
        output_flag: Some("--output"),
        download: DownloadStrategy::DartPubInstall {
            package: "scip_dart",
        },
    },
    ScipIndexer {
        binary_name: "scip-php",
        scip_args: &["index"],
        output_flag: Some("--output"),
        download: DownloadStrategy::ComposerInstall {
            package: "davidrjenni/scip-php",
        },
    },
];

pub(crate) fn cache_dir() -> PathBuf {
    if cfg!(windows) {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("infigraph")
            .join("bin")
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".infigraph")
            .join("bin")
    }
}

pub(crate) fn extra_runtime_paths() -> String {
    let mut dirs: Vec<PathBuf> = Vec::new();

    let node_dir = infigraph_dir().join("node");
    let node_bin = if cfg!(windows) {
        node_dir.clone()
    } else {
        node_dir.join("bin")
    };
    if node_bin.exists() {
        dirs.push(node_bin);
    }

    let java_home_bin = infigraph_dir()
        .join("java")
        .join("Contents")
        .join("Home")
        .join("bin");
    if java_home_bin.exists() {
        dirs.push(java_home_bin);
    } else {
        let java_bin_dir = infigraph_dir().join("java").join("bin");
        if java_bin_dir.exists() {
            dirs.push(java_bin_dir);
        }
    }

    let dotnet_dir = infigraph_dir().join("dotnet");
    if dotnet_dir.exists() {
        dirs.push(dotnet_dir);
    }

    let dotnet_tools = infigraph_dir().join("dotnet-tools");
    if dotnet_tools.exists() {
        dirs.push(dotnet_tools);
    }

    let dart_bin_dir = infigraph_dir().join("dart").join("bin");
    if dart_bin_dir.exists() {
        dirs.push(dart_bin_dir);
    }

    if let Some(home) = dirs::home_dir() {
        let pub_cache = home.join(".pub-cache").join("bin");
        if pub_cache.exists() {
            dirs.push(pub_cache);
        }
    }

    let php_dir = infigraph_dir().join("php");
    if php_dir.exists() {
        dirs.push(php_dir);
    }

    let composer_bin = infigraph_dir().join("composer").join("vendor").join("bin");
    if composer_bin.exists() {
        dirs.push(composer_bin);
    }

    let bin_dir = cache_dir();
    if bin_dir.exists() {
        dirs.push(bin_dir);
    }

    let sep = if cfg!(windows) { ";" } else { ":" };
    dirs.iter()
        .map(|d| d.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(sep)
}

pub(crate) fn indexers_for_languages(detected: &HashSet<String>) -> Vec<&'static ScipIndexer> {
    let mut result: Vec<&'static ScipIndexer> = Vec::new();
    for indexer in CATALOG {
        if indexer.lang_tags().iter().any(|t| detected.contains(*t)) {
            result.push(indexer);
        }
    }
    result
}

pub(crate) fn ensure_indexer(indexer: &ScipIndexer) -> Option<PathBuf> {
    if !ensure_runtime_for(indexer) {
        return None;
    }

    if on_path(indexer.binary_name) {
        return which_path(indexer.binary_name);
    }

    let bin_dir = cache_dir();
    let bin_name = binary_file_name(indexer.binary_name);
    let cached = bin_dir.join(&bin_name);
    if cached.exists() {
        return Some(cached);
    }

    let _ = std::fs::create_dir_all(&bin_dir);
    match &indexer.download {
        DownloadStrategy::GithubRelease {
            owner,
            repo,
            asset_pattern,
        } => {
            download_github_release(owner, repo, asset_pattern, indexer.binary_name, &bin_dir).ok()
        }
        DownloadStrategy::NpmInstall { package } => install_via_npm(package, indexer.binary_name),
        DownloadStrategy::DotnetTool { package } => {
            install_via_dotnet(package, indexer.binary_name)
        }
        DownloadStrategy::ComposerInstall { package } => {
            install_via_composer(package, indexer.binary_name)
        }
        DownloadStrategy::DartPubInstall { package } => {
            install_via_dart_pub(package, indexer.binary_name)
        }
    }
}

fn ensure_runtime_for(indexer: &ScipIndexer) -> bool {
    if indexer.binary_name == "scip-java" && ensure_java().is_none() {
        eprintln!("Auto-SCIP: skipping scip-java — no JVM available");
        return false;
    }
    true
}

fn binary_file_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn which_path(cmd: &str) -> Option<PathBuf> {
    let lookup = if cfg!(windows) { "where" } else { "which" };
    std::process::Command::new(lookup)
        .arg(cmd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            let line = s.lines().next()?;
            Some(PathBuf::from(line.trim()))
        })
}

fn download_github_release(
    owner: &str,
    repo: &str,
    asset_pattern: &dyn Fn(&str, &str) -> Option<String>,
    binary_name: &str,
    dest_dir: &Path,
) -> Result<PathBuf, String> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    let expected_asset = asset_pattern(os, arch)
        .ok_or_else(|| format!("{binary_name}: no asset for {os}/{arch}"))?;

    println!("Auto-SCIP: downloading {binary_name} from github.com/{owner}/{repo}...");

    let api_url = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");

    let download_url = match fetch_release_asset_url(&api_url, &expected_asset) {
        Ok(url) => url,
        Err(e) => {
            eprintln!("Auto-SCIP: API fetch failed ({e}), trying curl fallback...");
            fetch_release_asset_url_curl(&api_url, &expected_asset)?
        }
    };

    let tmp_dir = std::env::temp_dir();
    let tmp_file = tmp_dir.join(&expected_asset);

    if let Err(e) = download_file(&download_url, &tmp_file) {
        eprintln!("Auto-SCIP: download failed ({e}), trying curl fallback...");
        download_file_curl(&download_url, &tmp_file)?;
    }

    let bin_name = binary_file_name(binary_name);
    let dest = dest_dir.join(&bin_name);

    if expected_asset.ends_with(".tar.gz") {
        let status = std::process::Command::new("tar")
            .args([
                "-xzf",
                &tmp_file.to_string_lossy(),
                "-C",
                &dest_dir.to_string_lossy(),
            ])
            .status()
            .map_err(|e| format!("tar failed: {e}"))?;
        if !status.success() {
            return Err("tar extraction failed".to_string());
        }
        let extracted = dest_dir.join(binary_name);
        if extracted.exists() && extracted != dest {
            std::fs::rename(&extracted, &dest).map_err(|e| format!("rename failed: {e}"))?;
        }
    } else if expected_asset.ends_with(".gz") && !expected_asset.ends_with(".tar.gz") {
        let status = std::process::Command::new("gunzip")
            .args(["-f", &tmp_file.to_string_lossy()])
            .status()
            .map_err(|e| format!("gunzip failed: {e}"))?;
        if !status.success() {
            return Err("gunzip failed".to_string());
        }
        let decompressed = tmp_dir.join(expected_asset.trim_end_matches(".gz"));
        std::fs::rename(&decompressed, &dest).map_err(|e| format!("rename failed: {e}"))?;
    } else if expected_asset.ends_with(".zip") {
        let status = if cfg!(windows) {
            std::process::Command::new("tar")
                .args([
                    "-xf",
                    &tmp_file.to_string_lossy(),
                    "-C",
                    &dest_dir.to_string_lossy(),
                ])
                .status()
        } else {
            std::process::Command::new("unzip")
                .args([
                    "-o",
                    &tmp_file.to_string_lossy(),
                    "-d",
                    &dest_dir.to_string_lossy(),
                ])
                .status()
        }
        .map_err(|e| format!("unzip failed: {e}"))?;
        if !status.success() {
            return Err("zip extraction failed".to_string());
        }
    } else {
        std::fs::rename(&tmp_file, &dest).map_err(|e| format!("rename failed: {e}"))?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&dest) {
            let mut perms = meta.permissions();
            perms.set_mode(perms.mode() | 0o755);
            let _ = std::fs::set_permissions(&dest, perms);
        }
    }

    let _ = std::fs::remove_file(&tmp_file);
    println!("Auto-SCIP: {binary_name} installed to {}", dest.display());
    Ok(dest)
}

fn fetch_release_asset_url(api_url: &str, asset_name: &str) -> Result<String, String> {
    let resp = ureq::get(api_url)
        .set("Accept", "application/vnd.github.v3+json")
        .set("User-Agent", "infigraph")
        .call()
        .map_err(|e| format!("HTTP request failed: {e}"))?;

    let body: serde_json::Value = resp
        .into_json()
        .map_err(|e| format!("JSON parse failed: {e}"))?;

    let assets = body["assets"]
        .as_array()
        .ok_or("no assets array in release")?;

    for asset in assets {
        let name = asset["name"].as_str().unwrap_or("");
        if name == asset_name
            || (asset_name == "scip-java"
                && name.starts_with("scip-java")
                && !name.ends_with(".sha256")
                && !name.ends_with(".bat"))
        {
            if let Some(url) = asset["browser_download_url"].as_str() {
                return Ok(url.to_string());
            }
        }
    }

    Err(format!("asset '{asset_name}' not found in release"))
}

fn fetch_release_asset_url_curl(api_url: &str, asset_name: &str) -> Result<String, String> {
    let output = std::process::Command::new("curl")
        .args([
            "-sSfL",
            "-H",
            "Accept: application/vnd.github.v3+json",
            api_url,
        ])
        .output()
        .map_err(|e| format!("curl failed: {e}"))?;

    if !output.status.success() {
        return Err("curl request failed".to_string());
    }

    let body: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("JSON parse failed: {e}"))?;

    let assets = body["assets"]
        .as_array()
        .ok_or("no assets array in release")?;

    for asset in assets {
        let name = asset["name"].as_str().unwrap_or("");
        if name == asset_name
            || (asset_name == "scip-java"
                && name.starts_with("scip-java")
                && !name.ends_with(".sha256")
                && !name.ends_with(".bat"))
        {
            if let Some(url) = asset["browser_download_url"].as_str() {
                return Ok(url.to_string());
            }
        }
    }

    Err(format!("asset '{asset_name}' not found in release (curl)"))
}

fn download_file(url: &str, dest: &Path) -> Result<(), String> {
    let resp = ureq::get(url)
        .set("User-Agent", "infigraph")
        .call()
        .map_err(|e| format!("download failed: {e}"))?;

    let mut file = std::fs::File::create(dest).map_err(|e| format!("create file failed: {e}"))?;

    std::io::copy(&mut resp.into_reader(), &mut file).map_err(|e| format!("write failed: {e}"))?;

    Ok(())
}

fn download_file_curl(url: &str, dest: &Path) -> Result<(), String> {
    let status = std::process::Command::new("curl")
        .args(["-sSfL", "-o", &dest.to_string_lossy(), url])
        .status()
        .map_err(|e| format!("curl failed: {e}"))?;

    if !status.success() {
        return Err("curl download failed".to_string());
    }
    Ok(())
}

const NODE_VERSION: &str = "22.16.0";

fn node_download_url() -> Option<String> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let os_tag = match os {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "win",
        _ => return None,
    };
    let arch_tag = match arch {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        _ => return None,
    };
    if os == "windows" {
        Some(format!(
            "https://nodejs.org/dist/v{NODE_VERSION}/node-v{NODE_VERSION}-{os_tag}-{arch_tag}.zip"
        ))
    } else {
        Some(format!("https://nodejs.org/dist/v{NODE_VERSION}/node-v{NODE_VERSION}-{os_tag}-{arch_tag}.tar.gz"))
    }
}

fn ensure_node() -> Option<PathBuf> {
    if on_path("node") {
        return which_path("node");
    }

    let node_dir = cache_dir()
        .parent()
        .map(|p| p.join("node"))
        .unwrap_or_else(|| cache_dir().join("node"));

    let node_bin = if cfg!(windows) {
        node_dir.join("node.exe")
    } else {
        node_dir.join("bin").join("node")
    };

    if node_bin.exists() && runtime_version_matches("node", NODE_VERSION) {
        return Some(node_bin);
    }
    if node_dir.exists() && !runtime_version_matches("node", NODE_VERSION) {
        let _ = std::fs::remove_dir_all(&node_dir);
    }

    let url = node_download_url()?;
    println!("Auto-SCIP: downloading portable Node.js v{NODE_VERSION}...");

    let tmp = std::env::temp_dir();
    let archive_name = url.rsplit('/').next().unwrap_or("node.tar.gz");
    let tmp_file = tmp.join(archive_name);

    if let Err(e) = download_file(&url, &tmp_file) {
        eprintln!("Auto-SCIP: Node.js download failed ({e}), trying curl...");
        if let Err(e) = download_file_curl(&url, &tmp_file) {
            eprintln!("Auto-SCIP: Node.js download failed via curl too: {e}");
            return None;
        }
    }

    let _ = std::fs::create_dir_all(&node_dir);

    if archive_name.ends_with(".tar.gz") {
        let ok = std::process::Command::new("tar")
            .args([
                "--strip-components=1",
                "-xzf",
                &tmp_file.to_string_lossy(),
                "-C",
                &node_dir.to_string_lossy(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("Auto-SCIP: failed to extract Node.js");
            let _ = std::fs::remove_file(&tmp_file);
            return None;
        }
    } else if archive_name.ends_with(".zip") {
        let extract_dir = tmp.join("node_extract");
        let _ = std::fs::create_dir_all(&extract_dir);
        let ok = std::process::Command::new("tar")
            .args([
                "-xf",
                &tmp_file.to_string_lossy(),
                "-C",
                &extract_dir.to_string_lossy(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("Auto-SCIP: failed to extract Node.js");
            let _ = std::fs::remove_file(&tmp_file);
            return None;
        }
        let inner = extract_dir.join(format!("node-v{NODE_VERSION}-win-x64"));
        if inner.exists() {
            for entry in std::fs::read_dir(&inner).ok()?.flatten() {
                let dest = node_dir.join(entry.file_name());
                let _ = std::fs::rename(entry.path(), dest);
            }
        }
        let _ = std::fs::remove_dir_all(&extract_dir);
    }

    let _ = std::fs::remove_file(&tmp_file);

    if node_bin.exists() {
        write_runtime_version("node", NODE_VERSION);
        println!(
            "Auto-SCIP: portable Node.js v{NODE_VERSION} installed to {}",
            node_dir.display()
        );
        Some(node_bin)
    } else {
        eprintln!("Auto-SCIP: Node.js binary not found after extraction");
        None
    }
}

fn npm_path_for_node(node_bin: &Path) -> PathBuf {
    if cfg!(windows) {
        node_bin.parent().unwrap_or(node_bin).join("npm.cmd")
    } else {
        node_bin.parent().unwrap_or(node_bin).join("npm")
    }
}

fn install_via_npm(package: &str, binary_name: &str) -> Option<PathBuf> {
    let (npm_cmd, node_dir_prefix) = if on_path("npm") {
        ("npm".to_string(), None)
    } else {
        let node_bin = ensure_node()?;
        let npm = npm_path_for_node(&node_bin);
        if !npm.exists() {
            eprintln!("Auto-SCIP: skipping {binary_name} — npm not found");
            return None;
        }
        let prefix = node_bin
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf());
        (npm.to_string_lossy().to_string(), prefix)
    };

    println!("Auto-SCIP: installing {binary_name} via npm...");

    let mut cmd = std::process::Command::new(&npm_cmd);
    cmd.args(["install", "-g", package]);
    if let Some(ref prefix) = node_dir_prefix {
        cmd.arg("--prefix").arg(prefix);
        let path = std::env::var("PATH").unwrap_or_default();
        let bin_path = prefix.join("bin");
        cmd.env("PATH", format!("{}:{path}", bin_path.display()));
    }

    let ok = cmd.status().map(|s| s.success()).unwrap_or(false);

    if ok {
        let installed_bin = if let Some(ref prefix) = node_dir_prefix {
            let p = if cfg!(windows) {
                prefix.join(binary_file_name(binary_name))
            } else {
                prefix.join("bin").join(binary_name)
            };
            if p.exists() {
                Some(p)
            } else {
                which_path(binary_name)
            }
        } else {
            which_path(binary_name)
        };
        if installed_bin.is_some() {
            println!("Auto-SCIP: {binary_name} installed via npm");
            return installed_bin;
        }
    }

    eprintln!("Auto-SCIP: npm install of {binary_name} failed");
    None
}

const JRE_VERSION: &str = "21";

fn runtime_version_matches(name: &str, expected: &str) -> bool {
    let marker = infigraph_dir().join(name).join(".version");
    match std::fs::read_to_string(&marker) {
        Ok(v) => v.trim() == expected,
        Err(_) => false,
    }
}

fn write_runtime_version(name: &str, version: &str) {
    let marker = infigraph_dir().join(name).join(".version");
    let _ = std::fs::write(&marker, version);
}

pub(crate) fn clean_runtimes() {
    let ig = infigraph_dir();
    for name in &[
        "node",
        "java",
        "dotnet",
        "dart",
        "php",
        "composer",
        "dotnet-tools",
    ] {
        let dir = ig.join(name);
        if dir.exists() {
            let _ = std::fs::remove_dir_all(&dir);
            println!("Cleaned {}", dir.display());
        }
    }
    let bin = cache_dir();
    if bin.exists() {
        let _ = std::fs::remove_dir_all(&bin);
        println!("Cleaned {}", bin.display());
    }
}

fn java_works() -> bool {
    std::process::Command::new("java")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn ensure_java() -> Option<PathBuf> {
    if java_works() {
        return which_path("java");
    }

    let java_dir = infigraph_dir().join("java");
    let java_bin = if cfg!(target_os = "macos") {
        java_dir
            .join("Contents")
            .join("Home")
            .join("bin")
            .join("java")
    } else if cfg!(windows) {
        java_dir.join("bin").join("java.exe")
    } else {
        java_dir.join("bin").join("java")
    };
    if java_bin.exists() && runtime_version_matches("java", JRE_VERSION) {
        return Some(java_bin);
    }
    if java_dir.exists() && !runtime_version_matches("java", JRE_VERSION) {
        let _ = std::fs::remove_dir_all(&java_dir);
    }

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let os_tag = match os {
        "macos" => "mac",
        "linux" => "linux",
        "windows" => "windows",
        _ => return None,
    };
    let arch_tag = match arch {
        "aarch64" => "aarch64",
        "x86_64" => "x64",
        _ => return None,
    };

    let url = format!(
        "https://api.adoptium.net/v3/binary/latest/{JRE_VERSION}/ga/{os_tag}/{arch_tag}/jre/hotspot/normal/eclipse"
    );

    println!("Auto-SCIP: downloading portable JRE {JRE_VERSION} (Adoptium Temurin)...");
    let tmp = std::env::temp_dir();
    let ext = if os == "windows" { "zip" } else { "tar.gz" };
    let tmp_file = tmp.join(format!("temurin-jre.{ext}"));

    if let Err(e) = download_file_curl(&url, &tmp_file) {
        eprintln!("Auto-SCIP: JRE curl failed ({e}), trying ureq...");
        if let Err(e) = download_file(&url, &tmp_file) {
            eprintln!("Auto-SCIP: JRE download failed: {e}");
            return None;
        }
    }

    let _ = std::fs::create_dir_all(&java_dir);
    let ok = if ext == "zip" {
        std::process::Command::new("tar")
            .args([
                "-xf",
                &tmp_file.to_string_lossy(),
                "-C",
                &java_dir.to_string_lossy(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        std::process::Command::new("tar")
            .args([
                "--strip-components=1",
                "-xzf",
                &tmp_file.to_string_lossy(),
                "-C",
                &java_dir.to_string_lossy(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    let _ = std::fs::remove_file(&tmp_file);

    if ok && java_bin.exists() {
        write_runtime_version("java", JRE_VERSION);
        println!(
            "Auto-SCIP: portable JRE {JRE_VERSION} installed to {}",
            java_dir.display()
        );
        Some(java_bin)
    } else {
        eprintln!("Auto-SCIP: JRE extraction failed");
        None
    }
}

const DOTNET_VERSION: &str = "8.0.412";

fn ensure_dotnet() -> Option<PathBuf> {
    if on_path("dotnet") {
        return which_path("dotnet");
    }

    let dotnet_dir = infigraph_dir().join("dotnet");
    let dotnet_bin = if cfg!(windows) {
        dotnet_dir.join("dotnet.exe")
    } else {
        dotnet_dir.join("dotnet")
    };
    if dotnet_bin.exists() && runtime_version_matches("dotnet", DOTNET_VERSION) {
        return Some(dotnet_bin);
    }
    if dotnet_dir.exists() && !runtime_version_matches("dotnet", DOTNET_VERSION) {
        let _ = std::fs::remove_dir_all(&dotnet_dir);
    }

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let os_tag = match os {
        "macos" => "osx",
        "linux" => "linux",
        "windows" => "win",
        _ => return None,
    };
    let arch_tag = match arch {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        _ => return None,
    };
    let ext = if os == "windows" { "zip" } else { "tar.gz" };

    let url = format!(
        "https://builds.dotnet.microsoft.com/dotnet/Sdk/{DOTNET_VERSION}/dotnet-sdk-{DOTNET_VERSION}-{os_tag}-{arch_tag}.{ext}"
    );

    println!("Auto-SCIP: downloading portable .NET SDK {DOTNET_VERSION}...");
    let tmp_file = std::env::temp_dir().join(format!("dotnet-sdk.{ext}"));

    if let Err(e) = download_file_curl(&url, &tmp_file) {
        eprintln!("Auto-SCIP: .NET SDK curl failed ({e}), trying ureq...");
        if let Err(e) = download_file(&url, &tmp_file) {
            eprintln!("Auto-SCIP: .NET SDK download failed: {e}");
            return None;
        }
    }

    let _ = std::fs::create_dir_all(&dotnet_dir);
    let ok = std::process::Command::new("tar")
        .args([
            "-xzf",
            &tmp_file.to_string_lossy(),
            "-C",
            &dotnet_dir.to_string_lossy(),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = std::fs::remove_file(&tmp_file);

    if ok && dotnet_bin.exists() {
        write_runtime_version("dotnet", DOTNET_VERSION);
        println!(
            "Auto-SCIP: portable .NET SDK {DOTNET_VERSION} installed to {}",
            dotnet_dir.display()
        );
        Some(dotnet_bin)
    } else {
        eprintln!("Auto-SCIP: .NET SDK extraction failed");
        None
    }
}

const DART_VERSION: &str = "3.12.1";

fn ensure_dart() -> Option<PathBuf> {
    if on_path("dart") {
        return which_path("dart");
    }

    let dart_dir = infigraph_dir().join("dart");
    let dart_bin = if cfg!(windows) {
        dart_dir.join("bin").join("dart.exe")
    } else {
        dart_dir.join("bin").join("dart")
    };
    if dart_bin.exists() && runtime_version_matches("dart", DART_VERSION) {
        return Some(dart_bin);
    }
    if dart_dir.exists() && !runtime_version_matches("dart", DART_VERSION) {
        let _ = std::fs::remove_dir_all(&dart_dir);
    }

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let os_tag = match os {
        "macos" => "macos",
        "linux" => "linux",
        "windows" => "windows",
        _ => return None,
    };
    let arch_tag = match arch {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        _ => return None,
    };

    let url = format!(
        "https://storage.googleapis.com/dart-archive/channels/stable/release/{DART_VERSION}/sdk/dartsdk-{os_tag}-{arch_tag}-release.zip"
    );

    println!("Auto-SCIP: downloading portable Dart SDK {DART_VERSION}...");
    let tmp_file = std::env::temp_dir().join("dart-sdk.zip");

    if let Err(e) = download_file_curl(&url, &tmp_file) {
        eprintln!("Auto-SCIP: Dart SDK curl failed ({e}), trying ureq...");
        if let Err(e) = download_file(&url, &tmp_file) {
            eprintln!("Auto-SCIP: Dart SDK download failed: {e}");
            return None;
        }
    }

    let extract_dir = std::env::temp_dir().join("dart_extract");
    let _ = std::fs::create_dir_all(&extract_dir);
    let ok = std::process::Command::new("unzip")
        .args([
            "-qo",
            &tmp_file.to_string_lossy(),
            "-d",
            &extract_dir.to_string_lossy(),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = std::fs::remove_file(&tmp_file);

    if ok {
        let inner = extract_dir.join("dart-sdk");
        if inner.exists() {
            let _ = std::fs::rename(&inner, &dart_dir);
        }
    }
    let _ = std::fs::remove_dir_all(&extract_dir);

    if dart_bin.exists() {
        write_runtime_version("dart", DART_VERSION);
        println!(
            "Auto-SCIP: portable Dart SDK {DART_VERSION} installed to {}",
            dart_dir.display()
        );
        Some(dart_bin)
    } else {
        eprintln!("Auto-SCIP: Dart SDK extraction failed");
        None
    }
}

const PHP_VERSION: &str = "8.4.21";

fn ensure_php() -> Option<PathBuf> {
    if on_path("php") {
        return which_path("php");
    }

    let php_dir = infigraph_dir().join("php");
    let php_bin = if cfg!(windows) {
        php_dir.join("php.exe")
    } else {
        php_dir.join("php")
    };
    if php_bin.exists() && runtime_version_matches("php", PHP_VERSION) {
        return Some(php_bin);
    }
    if php_dir.exists() && !runtime_version_matches("php", PHP_VERSION) {
        let _ = std::fs::remove_dir_all(&php_dir);
    }

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let os_tag = match os {
        "macos" => "macos",
        "linux" => "linux",
        _ => return None,
    };
    let arch_tag = match arch {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        _ => return None,
    };

    let url = format!(
        "https://dl.static-php.dev/static-php-cli/common/php-{PHP_VERSION}-cli-{os_tag}-{arch_tag}.tar.gz"
    );

    println!("Auto-SCIP: downloading portable PHP {PHP_VERSION}...");
    let tmp_file = std::env::temp_dir().join("php-static.tar.gz");

    if let Err(e) = download_file_curl(&url, &tmp_file) {
        eprintln!("Auto-SCIP: PHP curl failed ({e}), trying ureq...");
        if let Err(e) = download_file(&url, &tmp_file) {
            eprintln!("Auto-SCIP: PHP download failed: {e}");
            return None;
        }
    }

    let _ = std::fs::create_dir_all(&php_dir);
    let ok = std::process::Command::new("tar")
        .args([
            "-xzf",
            &tmp_file.to_string_lossy(),
            "-C",
            &php_dir.to_string_lossy(),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = std::fs::remove_file(&tmp_file);

    if !ok || !php_bin.exists() {
        eprintln!("Auto-SCIP: PHP extraction failed");
        return None;
    }

    let composer_phar = php_dir.join("composer.phar");
    if !composer_phar.exists() {
        println!("Auto-SCIP: downloading Composer...");
        let composer_url = "https://getcomposer.org/download/latest-stable/composer.phar";
        if download_file(composer_url, &composer_phar).is_err() {
            let _ = download_file_curl(composer_url, &composer_phar);
        }
    }

    write_runtime_version("php", PHP_VERSION);
    println!(
        "Auto-SCIP: portable PHP {PHP_VERSION} installed to {}",
        php_dir.display()
    );
    Some(php_bin)
}

pub(crate) fn infigraph_dir() -> PathBuf {
    if cfg!(windows) {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("infigraph")
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".infigraph")
    }
}

fn install_via_dotnet(package: &str, binary_name: &str) -> Option<PathBuf> {
    let dotnet_cmd = if on_path("dotnet") {
        "dotnet".to_string()
    } else {
        let dotnet_bin = ensure_dotnet()?;
        dotnet_bin.to_string_lossy().to_string()
    };

    let dotnet_dir = infigraph_dir().join("dotnet");
    let tool_dir = infigraph_dir().join("dotnet-tools");
    let _ = std::fs::create_dir_all(&tool_dir);

    println!("Auto-SCIP: installing {binary_name} via dotnet tool...");
    let mut cmd = std::process::Command::new(&dotnet_cmd);
    cmd.args([
        "tool",
        "install",
        "--tool-path",
        &tool_dir.to_string_lossy(),
        package,
    ]);
    if dotnet_dir.exists() {
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{path}", dotnet_dir.display()));
        cmd.env("DOTNET_ROOT", &dotnet_dir);
    }

    let ok = cmd.status().map(|s| s.success()).unwrap_or(false);
    let tool_bin = tool_dir.join(binary_file_name(binary_name));

    if ok && tool_bin.exists() {
        println!("Auto-SCIP: {binary_name} installed via dotnet");
        Some(tool_bin)
    } else if ok && on_path(binary_name) {
        which_path(binary_name)
    } else {
        eprintln!("Auto-SCIP: dotnet tool install of {binary_name} failed");
        None
    }
}

fn find_ca_bundle() -> Result<String, ()> {
    if let Ok(v) = std::env::var("SSL_CERT_FILE") {
        if std::path::Path::new(&v).exists() {
            return Ok(v);
        }
    }
    for path in &[
        "/etc/ssl/cert.pem",
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/ca-bundle.pem",
    ] {
        if std::path::Path::new(path).exists() {
            return Ok(path.to_string());
        }
    }
    Err(())
}

fn install_via_composer(package: &str, binary_name: &str) -> Option<PathBuf> {
    let php_dir = infigraph_dir().join("php");
    let php_bin = if on_path("php") {
        "php".to_string()
    } else {
        let p = ensure_php()?;
        p.to_string_lossy().to_string()
    };

    let composer_cmd = if on_path("composer") {
        "composer".to_string()
    } else {
        let phar = php_dir.join("composer.phar");
        if !phar.exists() {
            eprintln!("Auto-SCIP: skipping {binary_name} — composer not available");
            return None;
        }
        phar.to_string_lossy().to_string()
    };

    let vendor_dir = infigraph_dir().join("composer");
    let _ = std::fs::create_dir_all(&vendor_dir);

    println!("Auto-SCIP: installing {binary_name} via composer...");

    let vdir = vendor_dir.to_string_lossy().to_string();
    let composer_json = vendor_dir.join("composer.json");
    let _ = std::fs::write(
        &composer_json,
        r#"{"config":{"policy":{"advisories":{"block":false}}}}"#,
    );

    let ok = if composer_cmd.ends_with(".phar") {
        let ca = find_ca_bundle().unwrap_or_default();
        let mut cmd = std::process::Command::new(&php_bin);
        cmd.args([
            "-d",
            &format!("openssl.cafile={ca}"),
            &composer_cmd,
            "require",
            "--no-audit",
            "--working-dir",
            &vdir,
            package,
        ]);
        cmd.env("SSL_CERT_FILE", &ca);
        cmd.status().map(|s| s.success()).unwrap_or(false)
    } else {
        std::process::Command::new(&composer_cmd)
            .args(["require", "--no-audit", "--working-dir", &vdir, package])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };

    let tool_bin = vendor_dir.join("vendor").join("bin").join(binary_name);
    if ok && tool_bin.exists() {
        println!("Auto-SCIP: {binary_name} installed via composer");
        Some(tool_bin)
    } else {
        eprintln!("Auto-SCIP: composer install of {binary_name} failed");
        None
    }
}

fn install_via_dart_pub(package: &str, binary_name: &str) -> Option<PathBuf> {
    let dart_cmd = if on_path("dart") {
        "dart".to_string()
    } else {
        let dart_bin = ensure_dart()?;
        dart_bin.to_string_lossy().to_string()
    };

    println!("Auto-SCIP: installing {binary_name} via dart pub...");

    let dart_dir = infigraph_dir().join("dart");
    let mut cmd = std::process::Command::new(&dart_cmd);
    cmd.args(["pub", "global", "activate", package]);
    if dart_dir.join("bin").exists() {
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{path}", dart_dir.join("bin").display()));
    }

    let ok = cmd.status().map(|s| s.success()).unwrap_or(false);

    if ok {
        let pub_cache_bin =
            dirs::home_dir().map(|h| h.join(".pub-cache").join("bin").join(binary_name));
        if let Some(ref p) = pub_cache_bin {
            if p.exists() {
                println!("Auto-SCIP: {binary_name} installed via dart pub");
                return Some(p.clone());
            }
        }
        if on_path(binary_name) {
            println!("Auto-SCIP: {binary_name} installed via dart pub");
            return which_path(binary_name);
        }
    }

    eprintln!("Auto-SCIP: dart pub install of {binary_name} failed");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_catalog_coverage() {
        for indexer in CATALOG {
            assert!(
                !indexer.lang_tags().is_empty(),
                "{} has no lang_tags",
                indexer.binary_name
            );
            assert!(!indexer.binary_name.is_empty());
            if indexer.binary_name != "scip-clang" {
                assert!(
                    !indexer.scip_args.is_empty(),
                    "{} has no scip_args",
                    indexer.binary_name
                );
            }
        }
    }

    #[test]
    fn test_indexers_for_languages() {
        let detected: HashSet<String> = ["typescript", "python", "rust"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let result = indexers_for_languages(&detected);
        assert_eq!(result.len(), 3);
        let names: Vec<&str> = result.iter().map(|i| i.binary_name).collect();
        assert!(names.contains(&"scip-typescript"));
        assert!(names.contains(&"scip-python"));
        assert!(names.contains(&"rust-analyzer"));
    }

    #[test]
    fn test_indexers_dedup() {
        let detected: HashSet<String> = ["java", "kotlin"].iter().map(|s| s.to_string()).collect();
        let result = indexers_for_languages(&detected);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].binary_name, "scip-java");
    }

    #[test]
    fn test_asset_pattern_platforms() {
        assert_eq!(
            scip_go_asset("macos", "aarch64"),
            Some("scip-go-darwin-arm64.tar.gz".to_string())
        );
        assert_eq!(
            scip_go_asset("linux", "x86_64"),
            Some("scip-go-linux-amd64.tar.gz".to_string())
        );
        assert_eq!(scip_go_asset("windows", "x86_64"), None);

        assert_eq!(
            rust_analyzer_asset("macos", "aarch64"),
            Some("rust-analyzer-aarch64-apple-darwin.gz".to_string())
        );
        assert_eq!(
            rust_analyzer_asset("linux", "x86_64"),
            Some("rust-analyzer-x86_64-unknown-linux-gnu.gz".to_string())
        );
        assert_eq!(
            rust_analyzer_asset("windows", "x86_64"),
            Some("rust-analyzer-x86_64-pc-windows-msvc.zip".to_string())
        );

        assert_eq!(
            scip_ruby_asset("macos", "aarch64"),
            Some("scip-ruby-arm64-darwin".to_string())
        );
        assert_eq!(
            scip_ruby_asset("linux", "x86_64"),
            Some("scip-ruby-x86_64-linux".to_string())
        );
        assert_eq!(scip_ruby_asset("windows", "x86_64"), None);

        assert_eq!(
            scip_clang_asset("macos", "aarch64"),
            Some("scip-clang-arm64-darwin".to_string())
        );
        assert_eq!(
            scip_clang_asset("linux", "x86_64"),
            Some("scip-clang-x86_64-linux".to_string())
        );
        assert_eq!(scip_clang_asset("windows", "x86_64"), None);
    }

    #[test]
    fn test_cache_dir() {
        let dir = cache_dir();
        let path_str = dir.to_string_lossy();
        assert!(
            path_str.contains("infigraph"),
            "cache dir should contain 'infigraph': {path_str}"
        );
        assert!(
            path_str.ends_with("bin"),
            "cache dir should end with 'bin': {path_str}"
        );
    }

    #[test]
    fn test_no_duplicate_lang_tags() {
        let mut all_tags: Vec<&str> = Vec::new();
        for indexer in CATALOG {
            for tag in indexer.lang_tags() {
                assert!(
                    !all_tags.contains(tag),
                    "duplicate lang_tag '{}' in {} (already in another indexer)",
                    tag,
                    indexer.binary_name
                );
                all_tags.push(tag);
            }
        }
    }

    /// The language table lives in core (`[scip] disable` is checked there);
    /// the catalog looks its tags up by binary name, so a row on one side
    /// only would give an indexer no languages, or a name nothing runs.
    #[test]
    fn the_catalog_and_the_core_language_table_name_the_same_indexers() {
        let mut catalog: Vec<&str> = CATALOG.iter().map(|i| i.binary_name).collect();
        let mut table: Vec<&str> = infigraph_core::scip::INDEXER_LANGUAGES
            .iter()
            .map(|(name, _)| *name)
            .collect();
        catalog.sort_unstable();
        table.sort_unstable();
        assert_eq!(catalog, table);
    }

    #[test]
    fn test_binary_file_name() {
        let name = binary_file_name("scip-go");
        if cfg!(windows) {
            assert_eq!(name, "scip-go.exe");
        } else {
            assert_eq!(name, "scip-go");
        }
    }
}
