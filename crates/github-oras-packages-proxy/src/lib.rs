//! Shared types for the GitHub ORAS packages proxy.
//!
//! The HTTP listener, routing, configuration, and OCI gateway are introduced
//! in later work items. Keeping this crate library-first lets those components
//! be unit-tested without starting a process.

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
