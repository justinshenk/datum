//! `datum-mcp`: serve datum to an MCP client over stdio.

use std::io::{stdin, stdout};

use datum_mcp::{parse_args, ProcessRunner, Server};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = match parse_args(&args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(if args.iter().any(|a| a == "-h" || a == "--help") {
                0
            } else {
                2
            });
        }
    };
    eprintln!(
        "datum-mcp: serving {} as actor {}",
        config.db.as_deref().unwrap_or("datum's default database"),
        config.actor
    );
    if let Err(e) = Server::new(config, ProcessRunner).serve(stdin().lock(), stdout().lock()) {
        eprintln!("datum-mcp: {e}");
        std::process::exit(1);
    }
}
