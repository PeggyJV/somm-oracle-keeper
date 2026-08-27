// The Safe ABI's execTransaction/getTransactionHash take 10-11 parameters;
// that arity is fixed by the contract and flows into the generated bindings.
#![allow(clippy::too_many_arguments)]

mod config;
mod contracts;
mod keeper;
mod schedule;

use alloy::signers::gcp::{
    gcloud_sdk::{
        google::cloud::kms::v1::key_management_service_client::KeyManagementServiceClient,
        GoogleApi,
    },
    GcpKeyRingRef, GcpSigner, KeySpecifier,
};
use alloy::signers::Signer;
use anyhow::{bail, Context, Result};
use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Json, Router};
use clap::{Parser, Subcommand};
use config::Config;
use keeper::{ChainWorker, OracleStatus, SharedStatus};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(
    name = "somm-oracle-keeper",
    about = "Drives performUpkeep on Sommelier share-price oracles through a threshold-1 Safe"
)]
struct Cli {
    #[arg(long, env = "KEEPER_CONFIG", default_value = "keeper.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check the KMS key, Safe and oracle wiring without sending a transaction.
    Verify,
    /// Print current oracle state without sending a transaction.
    Status,
    /// Run the keeper loop.
    Run {
        #[arg(long, env = "PORT", default_value_t = 8080)]
        port: u16,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "somm_oracle_keeper=info,warn".into()),
        )
        .json()
        .init();

    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)?;

    // `status` deliberately does not touch KMS, so an operator with RPC access
    // but no signing permission can still inspect the fleet.
    if let Command::Status = cli.command {
        let mut all = Vec::new();
        for chain in &cfg.chains {
            let w = ChainWorker::new_readonly(chain, cfg.cadence.fire_buffer_secs).await?;
            all.extend(w.tick_readonly(&[]).await);
        }
        println!("{}", serde_json::to_string_pretty(&all)?);
        return Ok(());
    }

    let signer = build_signer(&cfg).await?;

    match cli.command {
        Command::Verify => {
            let workers = build_workers(&cfg, &signer).await?;
            for w in &workers {
                w.preflight(signer.address()).await?;
            }
            println!("OK — signer {} verified against {} chain(s)", signer.address(), workers.len());
            Ok(())
        }
        Command::Status => unreachable!("handled above without a signer"),
        Command::Run { port } => run(cfg, signer, port).await,
    }
}

/// Build the KMS signer and refuse to continue unless it derives to the address
/// the ops Safe was constructed with.
async fn build_signer(cfg: &Config) -> Result<GcpSigner> {
    let keyring =
        GcpKeyRingRef::new(&cfg.kms.project_id, &cfg.kms.location, &cfg.kms.key_ring);
    let client =
        GoogleApi::from_function(KeyManagementServiceClient::new, "https://cloudkms.googleapis.com", None)
            .await
            .context("creating GCP KMS client (is the service account configured?)")?;
    let specifier = KeySpecifier::new(keyring, &cfg.kms.key_name, cfg.kms.key_version);
    let signer = GcpSigner::new(client, specifier, None)
        .await
        .context("loading KMS key (needs cloudkms.cryptoKeyVersions.viewPublicKey)")?;

    if signer.address() != cfg.kms.expected_address {
        bail!(
            "KMS key derives to {} but config expects {}. The ops Safe owner and the oracle's \
             automationForwarder are both immutable, so a mismatch cannot be fixed by \
             reconfiguring the keeper — confirm the key version.",
            signer.address(),
            cfg.kms.expected_address
        );
    }
    info!(signer = %signer.address(), "KMS key verified against expected address");
    Ok(signer)
}

async fn build_workers(cfg: &Config, signer: &GcpSigner) -> Result<Vec<ChainWorker>> {
    let mut workers = Vec::new();
    for chain in &cfg.chains {
        workers.push(ChainWorker::new(chain, signer, cfg.cadence.fire_buffer_secs).await?);
    }
    Ok(workers)
}

async fn run(cfg: Config, signer: GcpSigner, port: u16) -> Result<()> {
    let workers = build_workers(&cfg, &signer).await?;
    for w in &workers {
        w.preflight(signer.address()).await?;
    }

    let status: SharedStatus = Arc::new(tokio::sync::RwLock::new(Vec::new()));

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/status", get(status_handler))
        .with_state(status.clone());
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    info!(port, "health server listening");
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            error!("health server exited: {e}");
        }
    });

    let mut ticker = interval(Duration::from_secs(cfg.cadence.poll_interval_secs));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut shutdown = std::pin::pin!(tokio::signal::ctrl_c());
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let prev = status.read().await.clone();
                let mut next = Vec::new();
                for w in &workers {
                    next.extend(w.tick(&prev).await);
                }
                for s in next.iter().filter(|s| s.unhealthy()) {
                    warn!(chain = %s.chain, oracle = %s.label, "oracle unhealthy");
                }
                *status.write().await = next;
            }
            _ = &mut shutdown => {
                info!("shutdown signal received");
                return Ok(());
            }
        }
    }
}

async fn healthz(State(status): State<SharedStatus>) -> impl IntoResponse {
    let s = status.read().await;
    // Before the first tick completes there is nothing to judge; report healthy
    // so Cloud Run does not kill the container during startup.
    if s.is_empty() {
        return (StatusCode::OK, "starting");
    }
    if s.iter().any(|o| o.unhealthy()) {
        (StatusCode::SERVICE_UNAVAILABLE, "degraded")
    } else {
        (StatusCode::OK, "ok")
    }
}

async fn status_handler(State(status): State<SharedStatus>) -> Json<Vec<OracleStatus>> {
    Json(status.read().await.clone())
}
