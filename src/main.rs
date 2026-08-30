mod cli;
mod dotenv;
mod error;
mod sources;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::Cli;
use crate::error::{Error, Result};
use crate::sources::{Source, SourceKind};

/// Nothing to report.
const CLEAN: u8 = 0;
/// envwire could not look, so nobody should read agreement into the silence.
const FAILED: u8 = 2;

fn main() -> ExitCode {
    match run(&Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("envwire: {err}");
            ExitCode::from(FAILED)
        }
    }
}

fn run(cli: &Cli) -> Result<u8> {
    let target = cli.target();
    if !target.is_dir() {
        return Err(Error::NotADirectory(target));
    }

    let found = sources::discover(&target);

    // check speaks only in findings, and no check is wired up yet.
    if cli.is_quiet() {
        return Ok(CLEAN);
    }

    if found.is_empty() {
        println!("{}: no env sources here.", target.display());
        return Ok(CLEAN);
    }

    // The heading carries the directory, so each line only needs the name under it.
    println!("{}", target.display());
    for source in &found {
        let name = source.path.strip_prefix(&target).unwrap_or(&source.path);
        println!(
            "  {:<8} {:<24} {}",
            source.kind.label(),
            name.display(),
            summarize(source)?
        );
    }
    println!("\nReading these is all envwire does so far. No checks run yet.");

    Ok(CLEAN)
}

/// What one source says, in the few words a listing has room for.
fn summarize(source: &Source) -> Result<String> {
    // Compose keeps its variables inside a service tree that envwire cannot read yet.
    if source.kind == SourceKind::Compose {
        return Ok("not read yet".to_string());
    }

    let doc = dotenv::read(&source.path)?;
    let mut summary = match doc.entries.len() {
        1 => "1 variable".to_string(),
        n => format!("{n} variables"),
    };
    if !doc.malformed.is_empty() {
        summary.push_str(&format!(", {} unreadable", doc.malformed.len()));
    }
    Ok(summary)
}
