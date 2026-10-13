//! The WDIG backend: one binary, `wdig`, with the service as its default command and the
//! operator's jobs as subcommands, so the same image serves and runs one-off work.

mod accounts;
mod app;
mod auth;
mod config;
mod db;
mod errors;
mod health;
mod ingest;
mod logging;
mod partitions;
mod ratelimit;
mod seed;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "wdig", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the API and the web (the default)
    Serve,
    /// Create the monthly partitions of `points` for the current month and the next ones
    Partitions {
        /// How many months ahead of the current one to create
        #[arg(long, default_value_t = 3)]
        ahead: u32,
        /// The date to count from, YYYY-MM-DD; today when left out (for tests)
        #[arg(long)]
        today: Option<String>,
    },
    /// Write a month of made-up points and segments for an account, to look at before any phone has recorded
    Seed(seed::SeedArgs),
    /// Make invite codes for registration and print them, one per line
    Invite {
        #[arg(long, default_value_t = 1)]
        count: u32,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = config::Config::from_env()?;
    logging::init(&config);
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(config).await,
        Command::Partitions { ahead, today } => {
            let pool = db::connect(&config).await?;
            let today = partitions::parse_day(today.as_deref())?;
            partitions::ensure(&pool, today, ahead, true).await?;
            Ok(())
        }
        Command::Seed(args) => {
            let pool = db::connect(&config).await?;
            seed::run(&pool, args).await
        }
        Command::Invite { count } => {
            let pool = db::connect(&config).await?;
            for code in accounts::make_invites(&pool, count).await? {
                println!("{code}");
            }
            Ok(())
        }
    }
}

async fn serve(config: config::Config) -> Result<()> {
    config.check_for_serving()?;
    let pool = db::connect(&config).await?;
    partitions::spawn_keeper(pool.clone());
    let app = app::router(app::AppState::new(pool, &config));
    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    tracing::info!(addr = %config.bind_addr, version = %config.version, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("a SIGTERM handler")
            .recv()
            .await;
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("draining");
}
