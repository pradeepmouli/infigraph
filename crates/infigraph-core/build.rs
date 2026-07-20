use std::path::Path;
use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let hash = match sha {
        Some(s) if dirty => format!("{s}-dirty"),
        Some(s) => s,
        None => "unknown".to_string(),
    };
    println!("cargo:rustc-env=INFIGRAPH_BUILD_HASH={hash}");

    // `.git/HEAD` alone only changes on a checkout of a different ref; an
    // ordinary commit on the current branch instead updates
    // `refs/heads/<branch>` (or `packed-refs`, once refs get packed), so all
    // three must be watched or build_hash() keeps reporting the previous
    // commit's SHA across normal dev-loop rebuilds. Resolve the git dir via
    // `git rev-parse --git-dir` rather than assuming a fixed relative path;
    // if git isn't available, skip rerun emission entirely -- this matches
    // the "unknown" degradation above.
    let git_dir = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    if let Some(git_dir) = git_dir {
        let git_dir = Path::new(&git_dir);
        let head_path = git_dir.join("HEAD");
        println!("cargo:rerun-if-changed={}", head_path.display());

        if let Ok(head_contents) = std::fs::read_to_string(&head_path) {
            if let Some(ref_path) = head_contents.trim().strip_prefix("ref: ") {
                println!(
                    "cargo:rerun-if-changed={}",
                    git_dir.join(ref_path).display()
                );
                println!(
                    "cargo:rerun-if-changed={}",
                    git_dir.join("packed-refs").display()
                );
            }
        }
    }
}
