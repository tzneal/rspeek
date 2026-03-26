use clap::Parser;
use rspeek::cli::Cli;

fn main() {
    let mut cli = Cli::parse();
    if cli.llm_help {
        print!("{}", rspeek::cli::LLM_HELP);
        return;
    }
    if cli.api {
        cli.signature = true;
        cli.impls = true;
    }
    match rspeek::run(&cli) {
        Ok(out) => {
            eprint!("{}", out.stderr);
            print!("{}", out.stdout);
        }
        Err(e) => {
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "error": e.message,
                        "suggestions": e.suggestions,
                    })
                );
            } else {
                eprint!("Error: {}", e.message);
                let hint = match e.suggestions.len() {
                    0 => String::new(),
                    1 => format!("\n\ndid you mean `{}`?", e.suggestions[0]),
                    _ => {
                        let list: Vec<String> =
                            e.suggestions.iter().map(|s| format!("`{s}`")).collect();
                        format!("\n\ndid you mean one of {}?", list.join(", "))
                    }
                };
                eprintln!("{hint}");
            }
            std::process::exit(1);
        }
    }
}
