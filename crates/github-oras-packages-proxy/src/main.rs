//! Process entry point for the GitHub ORAS packages proxy.

use std::sync::Arc;

use github_oras_packages_proxy::{cli, config::Config, oci::OciClient, proxy, server::Server};

#[tokio::main]
async fn main() {
    let command = match cli::parse(std::env::args()) {
        Ok(command) => command,
        Err(_) => std::process::exit(2),
    };
    if command == cli::Command::Help {
        print_help();
        return;
    }

    let Ok(config) = Config::from_env() else {
        std::process::exit(2);
    };
    let Ok(client) = OciClient::new(&config) else {
        std::process::exit(2);
    };
    let client = Arc::new(client);
    let repository = config.repository().clone();
    let limits = config.inbound_limits();
    let handler = move |request| {
        let client = Arc::clone(&client);
        let repository = repository.clone();
        async move { proxy::handle_autoindex(request, client, repository, limits).await }
    };
    let Ok(server) = Server::start(&config, handler).await else {
        std::process::exit(1);
    };
    let _ = tokio::signal::ctrl_c().await;
    server.shutdown().await;
}

fn print_help() {
    use std::io::{self, Write};

    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(cli::help().as_bytes());
}
