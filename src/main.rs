use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use ridgewarden_library::app::App;
use ridgewarden_library::config::{Config, EXAMPLE};
use ridgewarden_library::{spawn_loops, web};

#[derive(Parser)]
#[command(
    version,
    about = "Offline library for AREDN meshes: Wikipedia, maps and NWS alerts at every site (a ridgewarden app)"
)]
struct Cli {
    #[arg(
        long,
        short,
        global = true,
        default_value = "/etc/ridgewarden/library.toml"
    )]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write a starter config
    Init,
    /// Follow the core, fetch and mirror content, serve the library pages
    Run,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ridgewarden_library=info,ridgewarden_client=info".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init => {
            if cli.config.exists() {
                bail!(
                    "{} already exists; not overwriting it",
                    cli.config.display()
                );
            }
            if let Some(d) = cli.config.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&cli.config, EXAMPLE)?;
            println!("Wrote {}", cli.config.display());
            Ok(())
        }
        Cmd::Run => {
            let cfg = Config::load(&cli.config)?;
            tokio::runtime::Runtime::new()?.block_on(run(cfg))
        }
    }
}

async fn run(cfg: Config) -> Result<()> {
    let listen = cfg.listen;
    let app = Arc::new(App::new(cfg)?);
    tracing::info!(
        "ridgewarden-library {} following {}, pages on http://{listen}",
        env!("CARGO_PKG_VERSION"),
        app.core.base()
    );
    spawn_loops(app.clone());
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let served = axum::serve(listener, web::router(app.clone()))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    app.kiwix.stop();
    Ok(served?)
}
