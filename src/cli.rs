//! CLI dispatcher.
//!
//! Three subcommands:
//!
//!   `init`   — Generate a fresh wallet, encrypt with the unlock
//!              passphrase, save to `wallet.json`, and print the
//!              mnemonic *once* for the operator to back up. Refuses to
//!              overwrite an existing wallet file unless `--force` is
//!              passed.
//!
//!   `import` — Read a BIP39 mnemonic from stdin's *first line*, encrypt
//!              with the unlock passphrase, save to `wallet.json`.
//!              Useful for restoring a wallet on a new host.
//!
//!   `run`    — Start the daemon. Decrypts the wallet file with the
//!              unlock passphrase, opens the SQLite DB, and (in later
//!              stages) starts the sync loop, API server, and webhook
//!              workers.
//!
//! Config path is read from `--config <path>` or the
//! `MERCHANT_KIT_CONFIG` env var, defaulting to `./config.toml`.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::storage::Db;
use crate::sync::{self, SyncState};
use crate::wallet::{unlock, Wallet};
use std::io::{BufRead, Write};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Init { force: bool },
    Import,
    Run,
    Help,
    Version,
}

/// Parse argv into a Command + config path. Hand-rolled rather than
/// pulling in clap — three subcommands and one option (`--force`) doesn't
/// justify the extra dep.
pub fn parse(args: &[String]) -> (Command, PathBuf) {
    let mut cmd: Option<Command> = None;
    let mut force = false;
    let mut config_path: Option<PathBuf> = None;
    let mut iter = args.iter().skip(1); // skip argv[0]
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "init" => cmd = Some(Command::Init { force: false }),
            "import" => cmd = Some(Command::Import),
            "run" => cmd = Some(Command::Run),
            "--help" | "-h" | "help" => cmd = Some(Command::Help),
            "--version" | "-V" => cmd = Some(Command::Version),
            "--force" | "-f" => force = true,
            "--config" | "-c" => {
                if let Some(val) = iter.next() {
                    config_path = Some(PathBuf::from(val));
                }
            }
            _ => {}
        }
    }
    // Apply --force to init.
    if let Some(Command::Init { .. }) = cmd {
        cmd = Some(Command::Init { force });
    }
    let cmd = cmd.unwrap_or(Command::Help);
    let path = config_path
        .or_else(|| std::env::var("MERCHANT_KIT_CONFIG").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("./config.toml"));
    (cmd, path)
}

pub async fn dispatch(cmd: Command, config_path: PathBuf) -> Result<()> {
    match cmd {
        Command::Help => {
            print_help();
            Ok(())
        }
        Command::Version => {
            println!("pivx-merchant-kit {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Init { force } => run_init(&config_path, force).await,
        Command::Import => run_import(&config_path).await,
        Command::Run => run_daemon(&config_path).await,
    }
}

fn print_help() {
    println!(
        "pivx-merchant-kit {} — self-hosted PIVX payment processor

Usage:
  pivx-merchant-kit init   [--force] [--config PATH]
  pivx-merchant-kit import [--config PATH]    < mnemonic-on-stdin-line-1
  pivx-merchant-kit run    [--config PATH]
  pivx-merchant-kit --help
  pivx-merchant-kit --version

Commands:
  init    Generate a fresh wallet (24-word BIP39 mnemonic), encrypt with
          the unlock passphrase, save to wallet.json. Refuses to overwrite
          an existing wallet unless --force is passed.

  import  Read a BIP39 mnemonic from stdin (first line), encrypt with the
          unlock passphrase, save to wallet.json. Use this to restore a
          wallet on a new host.

  run     Start the daemon. Decrypts the wallet, opens the database, and
          (in later versions) runs the sync loop, API server, and webhook
          workers.

Options:
  --config PATH   Config file location (default: ./config.toml). Can also be
                  set via MERCHANT_KIT_CONFIG=path env var.
  --force         When passed to `init`, overwrite an existing wallet.

Unlock passphrase:
  Provided via stdin (preferred — supports `cat secret | ...` or interactive
  prompt) or the MERCHANT_KIT_UNLOCK_PASSPHRASE env var. Stdin wins when both
  are present.",
        env!("CARGO_PKG_VERSION")
    );
}

async fn run_init(config_path: &std::path::Path, force: bool) -> Result<()> {
    let cfg = Config::from_file(config_path)?;
    let wallet_path = Wallet::file_in(&cfg.wallet.data_dir);
    if wallet_path.exists() && !force {
        return Err(Error::Config(format!(
            "wallet already exists at {} — refuse to overwrite. Re-run with --force \
             ONLY if you intend to wipe the existing wallet (this is unrecoverable).",
            wallet_path.display()
        )));
    }
    std::fs::create_dir_all(&cfg.wallet.data_dir)?;

    eprintln!("Generating fresh 24-word wallet…");
    let (wallet, mnemonic) = Wallet::create_new(0)?;

    let unlock_key = unlock::load_unlock_key()?;
    wallet.save_encrypted(&wallet_path, &unlock_key)?;

    println!(
        "\n\
         ╔══════════════════════════════════════════════════════════════════╗\n\
         ║  WRITE THIS DOWN. STORE IT OFFLINE. NEVER SHARE IT.              ║\n\
         ║                                                                  ║\n\
         ║  Whoever holds this mnemonic controls the wallet.                ║\n\
         ║  This is the ONLY time the daemon will show it.                  ║\n\
         ╚══════════════════════════════════════════════════════════════════╝\n"
    );
    println!("{}\n", mnemonic);
    eprintln!("Wallet saved encrypted at {}", wallet_path.display());
    Ok(())
}

async fn run_import(config_path: &std::path::Path) -> Result<()> {
    let cfg = Config::from_file(config_path)?;
    let wallet_path = Wallet::file_in(&cfg.wallet.data_dir);
    if wallet_path.exists() {
        return Err(Error::Config(format!(
            "wallet already exists at {} — `import` doesn't overwrite. Run \
             `init --force` first if you really want to replace it.",
            wallet_path.display()
        )));
    }
    std::fs::create_dir_all(&cfg.wallet.data_dir)?;

    // Importing reads the mnemonic from stdin *first*, then the unlock
    // passphrase from the env var. We can't read both from stdin without
    // ambiguity, and the env var is the natural way to feed the
    // passphrase to a one-shot operation.
    eprint!("Paste your 12/24-word mnemonic and press Enter: ");
    let _ = std::io::stderr().flush();
    let mut mnemonic = String::new();
    std::io::stdin().lock().read_line(&mut mnemonic)?;
    let mnemonic = mnemonic.trim();
    if mnemonic.is_empty() {
        return Err(Error::Config("empty mnemonic on stdin".into()));
    }

    let wallet = Wallet::import(mnemonic, 0)?;

    // For import, fall back to env-only unlock since stdin was consumed
    // by the mnemonic. Operators can re-run `init --force` to use the
    // stdin unlock path interactively if needed.
    let unlock_key = match std::env::var("MERCHANT_KIT_UNLOCK_PASSPHRASE") {
        Ok(s) if !s.trim().is_empty() => {
            use sha2::{Digest, Sha256};
            let mut out = [0u8; 32];
            out.copy_from_slice(&Sha256::digest(s.trim().as_bytes()));
            out
        }
        _ => {
            return Err(Error::Config(
                "import requires MERCHANT_KIT_UNLOCK_PASSPHRASE since stdin is used \
                 for the mnemonic"
                    .into(),
            ))
        }
    };
    wallet.save_encrypted(&wallet_path, &unlock_key)?;
    eprintln!("Wallet imported and saved encrypted at {}", wallet_path.display());
    Ok(())
}

async fn run_daemon(config_path: &std::path::Path) -> Result<()> {
    let cfg = Config::from_file(config_path)?;
    let wallet_path = Wallet::file_in(&cfg.wallet.data_dir);
    if !wallet_path.exists() {
        return Err(Error::Config(format!(
            "no wallet at {} — run `pivx-merchant-kit init` or `import` first",
            wallet_path.display()
        )));
    }

    let unlock_key = unlock::load_unlock_key()?;
    let wallet = Wallet::from_encrypted_file(&wallet_path, &unlock_key)?;
    tracing::info!("wallet decrypted");

    let db_path = std::path::Path::new(&cfg.wallet.data_dir).join("merchant.db");
    let db = Db::open(&db_path).await?;
    tracing::info!(path = %db_path.display(), "database ready");

    // Loud warning if zero-conf is configured.
    if cfg.payments.confirmations == 0 {
        tracing::warn!(
            "zero-conf is enabled — invoices will fire webhooks as soon as the tx \
             appears in the mempool. ONLY safe for microtransactions where a \
             rollback wouldn't hurt."
        );
    }
    if cfg.refunds.enabled {
        tracing::info!("automatic refunds enabled — invoices must carry refund_address");
    }

    tracing::info!(
        address = %wallet.inner.get_transparent_address().unwrap_or_else(|_| "<unavailable>".into()),
        last_block = wallet.inner.last_block,
        "wallet ready"
    );

    let state = SyncState::new(db, wallet, wallet_path, unlock_key, cfg)?;
    let sync_task = tokio::spawn(sync::run(state));

    tracing::info!("daemon running — API server and matcher land in Stages 5/4");

    // For Stage 3b, the daemon stays up while the sync loop runs. SIGINT
    // handling lands in Stage 8; for now Ctrl-C terminates the process,
    // sync loop never returns voluntarily (it's a loop {}).
    tokio::signal::ctrl_c().await.ok();
    tracing::info!("shutdown signal received, exiting");
    sync_task.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        let mut v = vec!["pivx-merchant-kit".into()];
        v.extend(items.iter().map(|s| s.to_string()));
        v
    }

    #[test]
    fn parses_init() {
        let (cmd, _) = parse(&args(&["init"]));
        assert_eq!(cmd, Command::Init { force: false });
    }

    #[test]
    fn parses_init_force() {
        let (cmd, _) = parse(&args(&["init", "--force"]));
        assert_eq!(cmd, Command::Init { force: true });
    }

    #[test]
    fn parses_run() {
        let (cmd, _) = parse(&args(&["run"]));
        assert_eq!(cmd, Command::Run);
    }

    #[test]
    fn parses_import() {
        let (cmd, _) = parse(&args(&["import"]));
        assert_eq!(cmd, Command::Import);
    }

    #[test]
    fn no_args_yields_help() {
        let (cmd, _) = parse(&args(&[]));
        assert_eq!(cmd, Command::Help);
    }

    #[test]
    fn unknown_arg_falls_through_to_help() {
        let (cmd, _) = parse(&args(&["nonsense"]));
        assert_eq!(cmd, Command::Help);
    }

    #[test]
    fn config_flag_overrides_default_path() {
        let (_, path) = parse(&args(&["run", "--config", "/etc/merchant.toml"]));
        assert_eq!(path, PathBuf::from("/etc/merchant.toml"));
    }

    #[test]
    fn config_short_flag_works() {
        let (_, path) = parse(&args(&["run", "-c", "alt.toml"]));
        assert_eq!(path, PathBuf::from("alt.toml"));
    }

    #[test]
    fn defaults_to_config_toml_in_cwd() {
        let (_, path) = parse(&args(&["run"]));
        assert_eq!(path, PathBuf::from("./config.toml"));
    }

    #[test]
    fn version_and_help_flags_parse() {
        assert_eq!(parse(&args(&["--version"])).0, Command::Version);
        assert_eq!(parse(&args(&["-V"])).0, Command::Version);
        assert_eq!(parse(&args(&["--help"])).0, Command::Help);
        assert_eq!(parse(&args(&["-h"])).0, Command::Help);
        assert_eq!(parse(&args(&["help"])).0, Command::Help);
    }
}
