// Every plain `eprintln!`/`eprint!` in this crate (and its modules) goes through `infigraph_core::proclog`: std's
// own while the process is a test or a foreground command, a write that ignores failure once a long-running
// process has called `proclog::keep_running_on_stderr_failure()` (std's panics on a failed write, and a daemon
// or MCP server whose log or client pipe went away died of its own logging).
#[macro_use(eprintln, eprint)]
#[allow(unused_imports)]
extern crate infigraph_core;

pub mod client;
pub mod sync;
pub mod template;

pub use client::ConfluenceClient;
pub use sync::{ConfluenceSync, CrawlOptions, SyncResult};
pub use template::{fill_with_llm, parse_pipeline_template};
