//! Minimal, dependency-free command-line surface for the service process.

use std::fmt;

/// A parsed top-level command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// Run the configured HTTP service.
    Serve,
    /// Print command help and exit successfully.
    Help,
}

/// A command-line parse failure that does not retain user input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliError {
    /// An unknown command or option was supplied.
    InvalidArguments,
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid command-line arguments")
    }
}

impl std::error::Error for CliError {}

/// Parses the small top-level command surface.
///
/// An omitted command is equivalent to `serve` for compatibility with the
/// previous environment-driven binary. The explicit `serve` spelling is the
/// documented interface. Subcommand-specific options are intentionally left
/// to the configuration boundary until the CLI CRUD work lands.
pub fn parse<I, S>(arguments: I) -> Result<Command, CliError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut arguments = arguments.into_iter();
    let _program = arguments.next();
    let Some(command) = arguments.next() else {
        return Ok(Command::Serve);
    };
    let command = command.as_ref();
    if arguments.next().is_some() {
        return Err(CliError::InvalidArguments);
    }
    match command {
        "serve" => Ok(Command::Serve),
        "help" | "--help" | "-h" => Ok(Command::Help),
        _ => Err(CliError::InvalidArguments),
    }
}

/// Returns the stable human-readable command help.
pub const fn help() -> &'static str {
    "github-oras-packages\n\nUSAGE:\n    github-oras-packages serve\n    github-oras-packages help\n\nThe serve command reads validated ORAS_PROXY_* configuration, serves the\nconfigured OCI repository at human-readable autoindex paths, and exposes the\nstandard OCI /v2/ namespace.\n"
}

#[cfg(test)]
mod tests {
    use super::{CliError, Command, help, parse};

    #[test]
    fn omitted_command_and_serve_select_the_service() {
        assert_eq!(parse(["proxy"]).unwrap(), Command::Serve);
        assert_eq!(parse(["proxy", "serve"]).unwrap(), Command::Serve);
    }

    #[test]
    fn help_is_stable_and_unknown_commands_fail_without_input() {
        assert_eq!(parse(["proxy", "--help"]).unwrap(), Command::Help);
        assert_eq!(parse(["proxy", "unknown"]), Err(CliError::InvalidArguments));
        assert_eq!(
            parse(["proxy", "serve", "secret"]),
            Err(CliError::InvalidArguments)
        );
        assert!(help().contains("github-oras-packages serve"));
    }
}
