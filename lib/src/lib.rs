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
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync + 'static>>;

pub mod build;
pub mod client;

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
