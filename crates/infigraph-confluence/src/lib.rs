// std's eprintln!/eprint! are shadowed: see infigraph_core::stderr_macros.
#[macro_use(eprintln, eprint)]
#[allow(unused_imports)]
extern crate infigraph_core;

pub mod client;
pub mod sync;
pub mod template;

pub use client::ConfluenceClient;
pub use sync::{ConfluenceSync, CrawlOptions, SyncResult};
pub use template::{fill_with_llm, parse_pipeline_template};
