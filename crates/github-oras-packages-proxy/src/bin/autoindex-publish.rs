use std::{env, io::Write, path::PathBuf, process};

use github_oras_packages_proxy::publisher::{Publication, PublisherError};

const USAGE: &str = "Usage: autoindex-publish <source-directory> <new-oci-layout-directory> [reference]\n\nBuilds a standard OCI image layout from ordinary files. File-relative paths become OCI titles and every file layer is marked autoindex-visible. Empty directories are ignored; symlinks and unsafe/duplicate paths are rejected.\n\nPublish the result with:\n  oras cp --from-oci-layout --to-plain-http <layout>:<reference> <registry>/<repository>:<reference>\n";

fn main() {
    if run().is_err() {
        let _ = writeln!(
            std::io::stderr().lock(),
            "autoindex-publish: operation failed"
        );
        process::exit(2);
    }
}

fn run() -> Result<(), PublisherError> {
    let mut arguments = env::args_os().skip(1);
    let Some(source) = arguments.next() else {
        return Err(PublisherError::InvalidArguments);
    };
    if source == "--help" || source == "-h" {
        print_usage()?;
        return Ok(());
    }
    let layout = arguments.next().ok_or(PublisherError::InvalidArguments)?;
    let reference = match arguments.next() {
        Some(reference) => reference
            .into_string()
            .map_err(|_| PublisherError::InvalidReference)?,
        None => "autoindex.v1".to_owned(),
    };
    if arguments.next().is_some() {
        return Err(PublisherError::InvalidArguments);
    }

    let publication = Publication::from_directory(PathBuf::from(source))?;
    publication.write_oci_layout(PathBuf::from(layout), &reference)?;
    writeln!(
        std::io::stdout().lock(),
        "Built {} visible files as {} ({})",
        publication.paths().len(),
        reference,
        publication.manifest_digest()
    )
    .map_err(|_| PublisherError::Io)?;
    Ok(())
}

fn print_usage() -> Result<(), PublisherError> {
    std::io::stdout()
        .lock()
        .write_all(USAGE.as_bytes())
        .map_err(|_| PublisherError::Io)
}
