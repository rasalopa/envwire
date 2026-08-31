mod cli;
mod compose;
mod dotenv;
mod error;
mod sources;
mod template;

use std::fs;
use std::process::ExitCode;

use clap::Parser;

use crate::cli::Cli;
use crate::error::{Error, Result};
use crate::sources::{Source, SourceKind};
use crate::template::Template;

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
    report_references(&found)?;
    println!("\nReading these is all envwire does so far. No checks run yet.");

    Ok(CLEAN)
}

/// What the Compose file asks the project `.env` for.
///
/// Only the root `.env` takes part in interpolation -- a service's `env_file:` never
/// does, not even when it names `.env` itself -- so this is the whole of what Compose
/// has to work with before a container starts.
fn report_references(found: &[Source]) -> Result<()> {
    let Some(compose) = found.iter().find(|s| s.kind == SourceKind::Compose) else {
        return Ok(());
    };
    let raw = fs::read_to_string(&compose.path).map_err(|source| Error::Read {
        path: compose.path.clone(),
        source,
    })?;

    // Every reference in the raw text, not just the ones under `environment:`:
    // `image: app:${TAG}` names a variable too, and the parsed model never sees it.
    let mut names = Vec::new();
    for line in raw.lines() {
        Template::parse(line).names(&mut names);
    }
    let mut distinct: Vec<String> = Vec::new();
    for name in names {
        if !distinct.contains(&name) {
            distinct.push(name);
        }
    }
    if distinct.is_empty() {
        return Ok(());
    }

    println!(
        "\n{} names {} variable{}:",
        compose.path.display(),
        distinct.len(),
        if distinct.len() == 1 { "" } else { "s" }
    );
    for name in &distinct {
        println!("  {name}");
    }
    Ok(())
}

/// What one source says, in the few words a listing has room for.
fn summarize(source: &Source) -> Result<String> {
    if source.kind == SourceKind::Compose {
        let compose = compose::read(&source.path)?;
        let variables: usize = compose.services.iter().map(|s| s.environment.len()).sum();
        return Ok(format!(
            "{}, {}",
            count(compose.services.len(), "service"),
            count(variables, "variable")
        ));
    }

    let doc = dotenv::read(&source.path)?;
    let mut summary = count(doc.entries.len(), "variable");
    if !doc.malformed.is_empty() {
        summary.push_str(&format!(", {} unreadable", doc.malformed.len()));
    }
    Ok(summary)
}

fn count(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        n => format!("{n} {noun}s"),
    }
}
