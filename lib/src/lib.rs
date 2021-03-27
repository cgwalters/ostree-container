//! # APIs bridging OSTree and container images
//!
//! This crate contains APIs to bidirectionally map
//! between OSTree repositories and container images.

#![deny(unused_results)]
#![deny(missing_docs)]
// We're just a wrapper around openat, shouldn't have any unsafe here.
#![forbid(unsafe_code)]

/// Our generic catchall fatal error, expected to be converted
/// to a string to output to a terminal or logs.
type Result<T> = anyhow::Result<T>;

pub mod build;
pub mod client;
