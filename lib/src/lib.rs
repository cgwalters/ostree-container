//! # APIs bridging OSTree and container images
//!
//! This crate contains APIs to bidirectionally map
//! between OSTree repositories and container images.

//#![deny(missing_docs)]
// Good defaults
#![deny(unused_results)]
#![deny(unsafe_code)]

/// Our generic catchall fatal error, expected to be converted
/// to a string to output to a terminal or logs.
type Result<T> = anyhow::Result<T>;

pub mod buildoci;
pub mod client;

pub mod oci;
mod ostree_ext;
mod import;