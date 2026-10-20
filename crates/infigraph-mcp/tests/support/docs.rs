//! A tiny Python project with a graph, a document index and (optionally) a git
//! history, for the tests of documents linked to code (`doc_links`,
//! `stale_docs`).

use std::path::Path;

use super::disable_background_watchers;

pub const APP: &str = "def compute_total(items):\n    total = 0\n    for item in items:\n        total += item\n    return total\n\n\nclass Cart:\n    def add(self, item):\n        self.items.append(item)\n";

pub fn write(root: &Path, file: &str, text: &str) {
    let path = root.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A project with `APP` and `extra` source files indexed, and `docs` (file,
/// text) written. Documents are not indexed: that is `index_documents`.
pub fn code_project(extra: &[(&str, &str)], docs: &[(&str, &str)]) -> tempfile::TempDir {
    disable_background_watchers();
    std::env::set_var("INFIGRAPH_BACKEND", "kuzu");
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/app.py", APP);
    for (file, text) in extra.iter().chain(docs) {
        write(dir.path(), file, text);
    }
    reindex_code(dir.path());
    dir
}

pub fn index_documents(dir: &Path) {
    infigraph_docs::ops::index_docs(dir, false).unwrap();
}

/// [`code_project`] with its documents indexed.
pub fn project(docs: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = code_project(&[], docs);
    index_documents(dir.path());
    dir
}

/// Index the code graph of `dir` (again, after its source changed). The handle
/// is dropped on return: the tools open the project themselves.
pub fn reindex_code(dir: &Path) {
    let mut ig =
        infigraph_core::CodeIndex::open(dir, infigraph_languages::bundled_registry().unwrap())
            .unwrap();
    ig.init().unwrap();
    ig.index().unwrap();
}

pub fn git(dir: &Path, args: &[&str], date: &str) {
    let out = std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Commits the sources and documents (not `.infigraph/`) as of `date`, with
/// `message` as the subject (the date when `None`).
pub fn commit_as(dir: &Path, date: &str, message: Option<&str>) {
    git(dir, &["add", "src", "docs"], date);
    git(dir, &["commit", "-q", "-m", message.unwrap_or(date)], date);
}

pub fn commit(dir: &Path, date: &str) {
    commit_as(dir, date, None);
}
