use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Find the environment variables your services disagree about.
#[derive(Debug, Parser)]
#[command(name = "envwire", version, about, long_about = None)]
pub struct Cli {
    /// Project to inspect (defaults to the working directory)
    #[arg(short, long, value_name = "DIR", global = true)]
    pub path: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Say only what is wrong, and exit non-zero when anything is
    Check,
}

impl Cli {
    /// The directory to inspect.
    pub fn target(&self) -> PathBuf {
        self.path.clone().unwrap_or_else(|| PathBuf::from("."))
    }

    /// Whether output should carry only findings, as CI wants it.
    pub fn is_quiet(&self) -> bool {
        self.command == Some(Command::Check)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_defaults_to_the_working_directory() {
        let cli = Cli::parse_from(["envwire"]);
        assert_eq!(cli.target(), PathBuf::from("."));
    }

    #[test]
    fn target_follows_the_path_flag() {
        let cli = Cli::parse_from(["envwire", "--path", "/srv/app"]);
        assert_eq!(cli.target(), PathBuf::from("/srv/app"));
    }

    #[test]
    fn the_path_flag_still_reaches_a_subcommand() {
        let cli = Cli::parse_from(["envwire", "check", "--path", "/srv/app"]);
        assert_eq!(cli.target(), PathBuf::from("/srv/app"));
        assert!(cli.is_quiet());
    }

    #[test]
    fn only_check_is_quiet() {
        assert!(!Cli::parse_from(["envwire"]).is_quiet());
        assert!(Cli::parse_from(["envwire", "check"]).is_quiet());
    }

    #[test]
    fn the_command_line_is_well_formed() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
