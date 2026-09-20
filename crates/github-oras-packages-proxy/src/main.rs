//! Process entry point for the GitHub ORAS packages proxy.
//!
//! The binary loads validated configuration, starts the loopback HTTP
//! lifecycle, and dispatches the enabled package routes through the fixed
//! origin OCI gateway.

use std::sync::Arc;

use github_oras_packages_proxy::{config::Config, oci::OciClient, proxy, server::Server};

#[tokio::main]
async fn main() {
    let Ok(config) = Config::from_env() else {
        return;
    };
    let Ok(client) = OciClient::new(&config) else {
        return;
    };
    let client = Arc::new(client);
    let enabled_protocols = config.enabled_protocols();
    let limits = config.inbound_limits();
    let handler = move |request| {
        let client = Arc::clone(&client);
        async move { proxy::handle(request, client, enabled_protocols, limits).await }
    };
    let Ok(server) = Server::start(&config, handler).await else {
        return;
    };
    let _ = tokio::signal::ctrl_c().await;
    server.shutdown().await;
}
