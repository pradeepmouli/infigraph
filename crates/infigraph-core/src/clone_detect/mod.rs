//! Near-duplicate function detection from function bodies.
//!
//! The legacy signal compared embeddings of a symbol's *signature text*
//! (`kind name: docstring`), so two functions with the same name and
//! docstring looked identical and a copy with a new name did not. This module
//! compares what the functions do: their bodies, normalized to tokens
//! ([`normalize`]), sketched for cheap comparison ([`sketch`]) and, where an
//! embedding exists, embedded.

pub mod bodies;
pub mod build;
pub mod normalize;
pub mod sketch;
