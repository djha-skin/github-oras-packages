//! Process entry point for the GitHub ORAS packages proxy.

use std::sync::Arc;

use github_oras_packages_proxy::{
    autoindex_cli, cli, config::Config, oci::OciClient, proxy, server::Server,
};

#[tokio::main]
async fn main() {
    let command = match cli::parse(std::env::args()) {
        Ok(command) => command,
        Err(error) => {
            write_stderr(&error.to_string());
            std::process::exit(2);
        }
    };
    match command {
        cli::Command::Help => print_help(),
        cli::Command::Serve => serve().await,
        cli::Command::Autoindex(operation) => autoindex(operation).await,
    }
}

async fn serve() {
    let Ok(config) = Config::from_env() else {
        write_stderr("configuration is invalid");
        std::process::exit(2);
    };
    let Ok(client) = OciClient::new(&config) else {
        write_stderr("configuration is invalid");
        std::process::exit(2);
    };
    let client = Arc::new(client);
    let repository = config.repository().clone();
    let limits = config.inbound_limits();
    let handler = move |request| {
        let client = Arc::clone(&client);
        let repository = repository.clone();
        async move { proxy::handle_gateway(request, client, repository, limits).await }
    };
    let Ok(server) = Server::start(&config, handler).await else {
        write_stderr("service could not start");
        std::process::exit(1);
    };
    let _ = tokio::signal::ctrl_c().await;
    server.shutdown().await;
}

async fn autoindex(operation: cli::AutoindexCommand) {
    let Ok(config) = Config::from_env() else {
        write_stderr("configuration is invalid");
        std::process::exit(2);
    };
    let Ok(client) = OciClient::new(&config) else {
        write_stderr("configuration is invalid");
        std::process::exit(2);
    };
    match autoindex_cli::execute(operation, &config, &client).await {
        Ok(message) => write_stdout(&message),
        Err(error) => {
            write_stderr(&error.to_string());
            std::process::exit(1);
        }
    }
}

fn print_help() {
    use std::io::{self, Write};

    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(cli::help().as_bytes());
}

fn write_stdout(message: &str) {
    use std::io::{self, Write};

    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{message}");
}

fn write_stderr(message: &str) {
    use std::io::{self, Write};

    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "{message}");
}
