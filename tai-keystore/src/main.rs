use anyhow::{Context, bail};
use std::io::{self, BufRead, Write};
use tai_keystore::{Keystore, ServiceCredential, keystore_path};

fn prompt(prompt: &str) -> io::Result<String> {
    let mut stdout = io::stdout();
    write!(stdout, "{prompt} ")?;
    stdout.flush()?;
    let stdin = io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn prompt_passphrase(label: &str) -> io::Result<String> {
    prompt(&format!("{label} passphrase:"))
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: tai-keystore <init|add|remove|list> [args...]");
        bail!("usage: tai-keystore <init|add|remove|list> [args...]");
    }

    let path = keystore_path().context("failed to determine keystore path")?;

    match args[1].as_str() {
        "init" => {
            if path.exists() {
                eprintln!("keystore already exists at {}", path.display());
                bail!("keystore already exists at {}", path.display());
            }
            let passphrase = prompt_passphrase("new")?;
            let passphrase2 = prompt_passphrase("confirm")?;
            if passphrase != passphrase2 {
                bail!("passphrases do not match");
            }
            Keystore::init(&path, &passphrase).context("failed to initialize keystore")?;
            println!("created encrypted keystore at {}", path.display());
        }
        "add" => {
            if args.len() < 4 {
                eprintln!("usage: tai-keystore add <service> <type>");
                eprintln!("  types: api_key, x");
                bail!("missing arguments");
            }
            let service = args[2].clone();
            let cred_type = args[3].as_str();
            let passphrase = prompt_passphrase("keystore")?;

            let mut keystore =
                Keystore::load(&path, &passphrase).context("failed to load keystore")?;
            match cred_type {
                "api_key" => {
                    let key = prompt("API key:")?;
                    keystore.add(service.clone(), ServiceCredential::ApiKey { key });
                }
                "x" => {
                    let api_key = prompt("X API key (consumer key):")?;
                    let api_key_secret = prompt("X API key secret (consumer secret):")?;
                    let access_token = prompt("X access token:")?;
                    let access_token_secret = prompt("X access token secret:")?;
                    let bearer = prompt("X bearer token (optional, press enter to skip):")?;
                    let bearer_token = if bearer.is_empty() {
                        None
                    } else {
                        Some(bearer)
                    };
                    keystore.add(
                        service.clone(),
                        ServiceCredential::X {
                            api_key,
                            api_key_secret,
                            access_token,
                            access_token_secret,
                            bearer_token,
                        },
                    );
                }
                other => {
                    eprintln!("unknown credential type: {other}");
                    eprintln!("  supported: api_key, x");
                    bail!("unknown credential type: {other}");
                }
            }
            keystore
                .save(&path, &passphrase)
                .context("failed to save keystore")?;
            println!("added credential for service '{service}'");
        }
        "remove" => {
            if args.len() < 3 {
                eprintln!("usage: tai-keystore remove <service>");
                bail!("missing arguments");
            }
            let service = &args[2];
            let passphrase = prompt_passphrase("keystore")?;
            let mut keystore =
                Keystore::load(&path, &passphrase).context("failed to load keystore")?;
            if keystore.remove(service) {
                keystore
                    .save(&path, &passphrase)
                    .context("failed to save keystore")?;
                println!("removed credential for service '{service}'");
            } else {
                eprintln!("service '{service}' not found");
                bail!("service '{service}' not found");
            }
        }
        "list" => {
            if !path.exists() {
                eprintln!("keystore does not exist, run 'tai-keystore init' first");
                bail!("keystore does not exist");
            }
            let passphrase = prompt_passphrase("keystore")?;
            let keystore = Keystore::load(&path, &passphrase).context("failed to load keystore")?;
            let mut names: Vec<&String> = keystore.service_names().collect();
            names.sort();
            if names.is_empty() {
                println!("no credentials stored");
            } else {
                println!("stored credentials:");
                for name in names {
                    println!("  - {name}");
                }
            }
        }
        other => {
            eprintln!("unknown command: {other}");
            eprintln!("usage: tai-keystore <init|add|remove|list> [args...]");
            bail!("unknown command: {other}");
        }
    }

    Ok(())
}
