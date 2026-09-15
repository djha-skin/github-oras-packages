//! Process entry point for the GitHub ORAS packages proxy.
//!
//! Network behavior is deliberately added in the runtime-configuration and
//! transport work items. This entry point establishes the supported Tokio
//! runtime without making accidental network or logging policy decisions.

#[tokio::main]
async fn main() {
    let _service_name = github_oras_packages_proxy::SERVICE_NAME;
}
