//! The stdin loop: read a line, dispatch, reply, flush. Nothing else —
//! rootle owns this process's lifecycle and may respawn it at any
//! time (initialize runs once per generation, cheaply).

use rootle_bitbucket::{Handler, api, respond};

fn main() {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("rootle-bitbucket {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let mut instance = api::DEFAULT_INSTANCE.to_string();
    let mut token_env = api::DEFAULT_TOKEN_ENV.to_string();
    let mut username_env = api::DEFAULT_USERNAME_ENV.to_string();
    let mut cache_base: Option<std::path::PathBuf> = None;
    let mut workspaces: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--instance" => instance = args.next().unwrap_or_default(),
            "--token-env" => token_env = args.next().unwrap_or_default(),
            "--username-env" => username_env = args.next().unwrap_or_default(),
            "--cache" => cache_base = args.next().map(std::path::PathBuf::from),
            // Repeatable. Serves tokens scoped to repositories only
            // (no account read) — CHANGE-2770 killed discovery for
            // them. BITBUCKET_WORKSPACES (comma list) works too.
            "--workspace" => workspaces.push(args.next().unwrap_or_default()),
            other => {
                eprintln!("rootle-bitbucket: unknown flag {other:?}");
                std::process::exit(2);
            }
        }
    }
    if workspaces.is_empty()
        && let Ok(list) = std::env::var("BITBUCKET_WORKSPACES")
    {
        workspaces = list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
    }

    let handler = Handler::new(&instance, &token_env, &username_env, cache_base, workspaces);
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    use std::io::BufRead;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = respond(&handler, &line) {
            println!("{reply}");
            use std::io::Write;
            let _ = out.flush();
        }
    }
}
