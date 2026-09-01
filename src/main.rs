mod cli;
mod compose;
mod dotenv;
mod error;
mod model;
mod sources;
mod template;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::Cli;
use crate::error::{Error, Result};
use crate::model::Project;
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

    let project = model::read(&found)?;

    // The heading carries the directory, so each line only needs the name under it.
    println!("{}", target.display());
    for source in &found {
        let name = source.path.strip_prefix(&target).unwrap_or(&source.path);
        println!(
            "  {:<8} {:<24} {}",
            source.kind.label(),
            name.display(),
            summarize(source, &project)?
        );
    }
    report_references(&project);
    println!("\nReading these is all envwire does so far. No checks run yet.");

    Ok(CLEAN)
}

/// What the Compose file asks the project `.env` for.
///
/// Only the root `.env` answers -- a service's `env_file:` never takes part in
/// interpolation -- so this is the whole of what Compose has to work with before a
/// container starts.
fn report_references(project: &Project) {
    let Some(compose) = &project.compose else {
        return;
    };

    // One line per variable, at the first place that names it.
    let mut first: Vec<&crate::model::Reference> = Vec::new();
    for reference in &project.references {
        if !first.iter().any(|seen| seen.name == reference.name) {
            first.push(reference);
        }
    }
    if first.is_empty() {
        return;
    }

    println!("\n{} asks for:", compose.display());
    for reference in first {
        // Never the value itself -- see `Value::disclosure`.
        let answer = match project.interpolation.get(&reference.name) {
            Some(bound) => format!("{}, at {}", bound.value.disclosure(), bound.origin),
            None => "not in .env".to_string(),
        };
        println!("  {:<30} {answer}", reference.name);
    }
}

/// What one source says, in the few words a listing has room for.
fn summarize(source: &Source, project: &Project) -> Result<String> {
    if source.kind == SourceKind::Compose {
        let compose = compose::read(&source.path)?;
        let variables: usize = compose.services.iter().map(|s| s.environment.len()).sum();
        return Ok(format!(
            "{}, {}",
            count(compose.services.len(), "service"),
            count(variables, "variable")
        ));
    }

    let Some(file) = project.files.iter().find(|f| f.path == source.path) else {
        return Ok(String::new());
    };
    let mut summary = count(file.settings.len(), "variable");
    if !file.malformed.is_empty() {
        summary.push_str(&format!(", {} unreadable", file.malformed.len()));
    }
    Ok(summary)
}

fn count(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        n => format!("{n} {noun}s"),
    }
}
