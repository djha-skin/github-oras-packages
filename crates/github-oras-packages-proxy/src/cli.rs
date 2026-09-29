//! Minimal, dependency-free command-line surface for the service process.

use std::{fmt, path::PathBuf};

/// A parsed top-level command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Run the configured HTTP service.
    Serve,
    /// Print command help and exit successfully.
    Help,
    /// Create, replace, or remove a native autoindex file entry.
    Autoindex(AutoindexCommand),
}

/// A parsed native file operation against `autoindex.v1`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AutoindexCommand {
    /// Add a new file at a validated relative title.
    Create {
        source: PathBuf,
        path: String,
        dry_run: bool,
    },
    /// Replace the bytes at an existing relative title.
    Update {
        source: PathBuf,
        path: String,
        dry_run: bool,
    },
    /// Remove a file or every file below a directory prefix.
    Delete {
        path: String,
        confirm: bool,
        dry_run: bool,
    },
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

/// Parses the command surface without retaining malformed argument values.
///
/// An omitted command is equivalent to `serve` for compatibility with the
/// previous environment-driven binary. Destructive deletion is never
/// interactive: callers must provide `--yes`, or use `--dry-run`.
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
    match command.as_ref() {
        "serve" if arguments.next().is_none() => Ok(Command::Serve),
        "help" | "--help" | "-h" if arguments.next().is_none() => Ok(Command::Help),
        "autoindex" => parse_autoindex(arguments),
        _ => Err(CliError::InvalidArguments),
    }
}

fn parse_autoindex<I, S>(arguments: I) -> Result<Command, CliError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut arguments = arguments.into_iter();
    let operation = arguments.next().ok_or(CliError::InvalidArguments)?;
    let operation = operation.as_ref();
    if matches!(operation, "help" | "--help" | "-h") {
        return if arguments.next().is_none() {
            Ok(Command::Help)
        } else {
            Err(CliError::InvalidArguments)
        };
    }
    let first = arguments.next().ok_or(CliError::InvalidArguments)?;
    let first = first.as_ref();
    match operation {
        "create" | "update" => {
            let path = arguments.next().ok_or(CliError::InvalidArguments)?;
            let path = path.as_ref().to_owned();
            let mut dry_run = false;
            for argument in arguments {
                match argument.as_ref() {
                    "--dry-run" if !dry_run => dry_run = true,
                    _ => return Err(CliError::InvalidArguments),
                }
            }
            let command = if operation == "create" {
                AutoindexCommand::Create {
                    source: PathBuf::from(first),
                    path,
                    dry_run,
                }
            } else {
                AutoindexCommand::Update {
                    source: PathBuf::from(first),
                    path,
                    dry_run,
                }
            };
            Ok(Command::Autoindex(command))
        }
        "delete" => {
            let mut confirm = false;
            let mut dry_run = false;
            for argument in arguments {
                match argument.as_ref() {
                    "--yes" if !confirm => confirm = true,
                    "--dry-run" if !dry_run => dry_run = true,
                    _ => return Err(CliError::InvalidArguments),
                }
            }
            if confirm == dry_run {
                return Err(CliError::InvalidArguments);
            }
            Ok(Command::Autoindex(AutoindexCommand::Delete {
                path: first.to_owned(),
                confirm,
                dry_run,
            }))
        }
        _ => Err(CliError::InvalidArguments),
    }
}

/// Returns the stable human-readable command help.
pub const fn help() -> &'static str {
    "github-oras-packages\n\nUSAGE:\n    github-oras-packages serve\n    github-oras-packages autoindex create <source-file> <relative-path> [--dry-run]\n    github-oras-packages autoindex update <source-file> <relative-path> [--dry-run]\n    github-oras-packages autoindex delete <relative-path> [--yes | --dry-run]\n    github-oras-packages help\n\nAutoindex CRUD rebuilds the configured autoindex.v1 OCI artifact. Paths are\nordinary relative file titles. Delete requires --yes and is never interactive;\n--dry-run previews the operation without publishing.\n\nAll commands read validated ORAS_PROXY_* configuration. The serve command\nserves the configured repository at human-readable paths and exposes the\nstandard OCI /v2/ namespace. Private reads use ORAS_PROXY_TOKEN_USERNAME and\nORAS_PROXY_TOKEN_PASSWORD; ORAS writes use those credentials through stdin or\nthe existing ORAS credential store.\n"
}

#[cfg(test)]
mod tests {
    use super::{AutoindexCommand, CliError, Command, help, parse};

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
        assert!(help().contains("autoindex create"));
        assert!(help().contains("autoindex update"));
        assert!(help().contains("autoindex delete"));
        assert_eq!(
            parse(["proxy", "autoindex", "--help"]).unwrap(),
            Command::Help
        );
    }

    #[test]
    fn parses_scriptable_autoindex_operations_and_confirmation() {
        assert_eq!(
            parse([
                "proxy",
                "autoindex",
                "create",
                "wheel.whl",
                "packages/wheel.whl"
            ])
            .unwrap(),
            Command::Autoindex(AutoindexCommand::Create {
                source: "wheel.whl".into(),
                path: "packages/wheel.whl".into(),
                dry_run: false,
            })
        );
        assert_eq!(
            parse([
                "proxy",
                "autoindex",
                "update",
                "index.html",
                "simple/index.html",
                "--dry-run",
            ])
            .unwrap(),
            Command::Autoindex(AutoindexCommand::Update {
                source: "index.html".into(),
                path: "simple/index.html".into(),
                dry_run: true,
            })
        );
        assert_eq!(
            parse(["proxy", "autoindex", "delete", "simple/", "--yes"]).unwrap(),
            Command::Autoindex(AutoindexCommand::Delete {
                path: "simple/".into(),
                confirm: true,
                dry_run: false,
            })
        );
        assert_eq!(
            parse(["proxy", "autoindex", "delete", "simple/"]),
            Err(CliError::InvalidArguments)
        );
        assert_eq!(
            parse([
                "proxy",
                "autoindex",
                "delete",
                "simple/",
                "--yes",
                "--dry-run"
            ]),
            Err(CliError::InvalidArguments)
        );
    }
}
