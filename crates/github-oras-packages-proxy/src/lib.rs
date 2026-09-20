//! Shared types for the GitHub ORAS packages proxy.
//!
//! The crate contains the validated HTTP boundary, fixed-origin OCI gateway,
//! and the initial fixture-backed PyPI frontend. Keeping those components
//! library-first lets them be tested without starting a process.

pub mod autoindex;
pub mod config;
pub mod errors;
pub mod inbound;
pub mod oci;
pub mod proxy;
pub mod routing;
pub mod server;

/// Identity of this binary, exposed only through explicitly designed safe
/// operational interfaces in later work items.
pub const SERVICE_NAME: &str = "github-oras-packages-proxy";

#[cfg(test)]
mod tests {
    use super::SERVICE_NAME;

    #[test]
    fn exposes_a_stable_service_name() {
        assert_eq!(SERVICE_NAME, "github-oras-packages-proxy");
    }
}
