pub mod cache;
pub mod cfg;
pub mod cli;
pub mod index;
pub mod output;
pub mod query;
pub mod reexport;
pub mod resolve;
pub mod suggest;

use std::fmt::Write;

/// Collected output from an rspeek invocation.
#[derive(Default)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn println(&mut self, s: &str) {
        writeln!(self.stdout, "{s}").unwrap();
    }
}

/// Run rspeek with a parsed CLI.
pub fn run(cli: &cli::Cli) -> Result<Output, cli::NotFound> {
    let mut out = Output::default();
    cli::run(cli, &mut out)?;
    Ok(out)
}
