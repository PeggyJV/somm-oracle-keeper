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
use keeper::{now_unix, ChainReport, ChainWorker, SharedStatus};
use std::path::PathBuf;
use std::collections::BTreeMap;
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

async fn build_workers(cfg: &Config, signer: &GcpSigner) -> Result<Vec<Arc<ChainWorker>>> {
    let mut workers = Vec::new();
    for chain in &cfg.chains {
        workers.push(ChainWorker::new(chain, signer, &cfg.cadence).await?);
    }
    Ok(workers.into_iter().map(Arc::new).collect())
}

async fn run(cfg: Config, signer: GcpSigner, port: u16) -> Result<()> {
    let workers = build_workers(&cfg, &signer).await?;
    for w in &workers {
        w.preflight(signer.address()).await?;
    }

    let status: SharedStatus = Arc::new(tokio::sync::RwLock::new(BTreeMap::new()));

    // A chain is considered to have stopped reporting after this long. It must
    // exceed one poll plus a full tick budget, or a slow-but-working chain would
    // flap the health check.
    let stale_after =
        cfg.cadence.poll_interval_secs + cfg.cadence.tick_timeout_secs * 2;
    let tick_budget = Duration::from_secs(cfg.cadence.tick_timeout_secs);

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/status", get(status_handler))
        .with_state(HealthState { status: status.clone(), stale_after });
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
                // One task per chain. Chains must not share a failure domain:
                // a single unresponsive RPC endpoint previously blocked every
                // other chain's upkeep and left /healthz reporting ok forever.
                let mut handles = Vec::with_capacity(workers.len());
                for w in &workers {
                    let w = w.clone();
                    let status = status.clone();
                    handles.push(tokio::spawn(async move {
                        let prev = status
                            .read()
                            .await
                            .get(w.name())
                            .map(|r| r.oracles.clone())
                            .unwrap_or_default();

                        let (oracles, stalled) =
                            match tokio::time::timeout(tick_budget, w.tick(&prev)).await {
                                Ok(v) => (v, false),
                                Err(_) => {
                                    error!(
                                        chain = w.name(),
                                        "chain tick exceeded its timeout budget; \
                                         publishing a stalled report"
                                    );
                                    (w.stalled_statuses(&prev), true)
                                }
                            };

                        for s in oracles.iter().filter(|s| s.unhealthy()) {
                            warn!(chain = %s.chain, oracle = %s.label, "oracle unhealthy");
                        }

                        // Publish per chain as it finishes, so one slow chain
                        // never hides another's progress.
                        status.write().await.insert(
                            w.name().to_string(),
                            ChainReport {
                                chain: w.name().to_string(),
                                updated_at: now_unix(),
                                stalled,
                                oracles,
                            },
                        );
                    }));
                }
                for h in handles {
                    // A panicked worker must not take the loop down with it.
                    if let Err(e) = h.await {
                        error!("chain worker task failed: {e}");
                    }
                }
            }
            _ = &mut shutdown => {
                info!("shutdown signal received");
                return Ok(());
            }
        }
    }
}

#[derive(Clone)]
struct HealthState {
    status: SharedStatus,
    stale_after: u64,
}

/// Pure health decision, split out so it can be tested without a server.
///
/// Order matters: staleness is checked first because it is the only signal that
/// survives a worker hanging. Every other check reads data a worker published,
/// so a worker that has stopped publishing would otherwise look permanently
/// healthy on its last good readings.
fn health_verdict(
    reports: &BTreeMap<String, ChainReport>,
    now: u64,
    stale_after: u64,
) -> (StatusCode, &'static str) {
    // Before the first tick completes there is nothing to judge; report healthy
    // so Cloud Run does not kill the container during startup.
    if reports.is_empty() {
        return (StatusCode::OK, "starting");
    }
    for r in reports.values() {
        if now.saturating_sub(r.updated_at) > stale_after {
            return (StatusCode::SERVICE_UNAVAILABLE, "stale");
        }
        if r.stalled {
            return (StatusCode::SERVICE_UNAVAILABLE, "stalled");
        }
        if r.oracles.iter().any(|o| o.unhealthy()) {
            return (StatusCode::SERVICE_UNAVAILABLE, "degraded");
        }
    }
    (StatusCode::OK, "ok")
}

async fn healthz(State(st): State<HealthState>) -> impl IntoResponse {
    let s = st.status.read().await;
    health_verdict(&s, now_unix(), st.stale_after)
}

async fn status_handler(State(st): State<HealthState>) -> Json<Vec<ChainReport>> {
    Json(st.status.read().await.values().cloned().collect())
}


#[cfg(test)]
mod health_tests {
    use super::*;
    use alloy::primitives::Address;
    use keeper::OracleStatus;

    fn oracle(failures: u32, breached: bool) -> OracleStatus {
        OracleStatus {
            chain: "ethereum".into(),
            label: "turbo-steth".into(),
            address: Address::ZERO,
            safe_to_use: true,
            kill_switch: false,
            last_observation_ts: 1_000_000,
            seconds_until_due: Some(43_200),
            lateness_secs: 0,
            lateness_budget_secs: 43_200,
            breached,
            target_total_supply: None,
            target_total_assets: None,
            consecutive_failures: failures,
            last_error: None,
            last_upkeep_tx: None,
        }
    }

    fn reports(updated_at: u64, stalled: bool, o: OracleStatus) -> BTreeMap<String, ChainReport> {
        let mut m = BTreeMap::new();
        m.insert(
            "ethereum".to_string(),
            ChainReport { chain: "ethereum".into(), updated_at, stalled, oracles: vec![o] },
        );
        m
    }

    const STALE_AFTER: u64 = 1500;
    const NOW: u64 = 2_000_000;

    #[test]
    fn empty_is_healthy_during_startup() {
        let (code, body) = health_verdict(&BTreeMap::new(), NOW, STALE_AFTER);
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body, "starting");
    }

    #[test]
    fn fresh_and_working_is_ok() {
        let r = reports(NOW - 60, false, oracle(0, false));
        assert_eq!(health_verdict(&r, NOW, STALE_AFTER).0, StatusCode::OK);
    }

    /// The regression this whole change exists for: a worker that stops
    /// publishing must fail the health check even though its last readings were
    /// perfectly healthy. Before per-chain reports and staleness detection, a
    /// hung RPC left the service reporting ok indefinitely while firing nothing.
    #[test]
    fn a_worker_that_stopped_reporting_is_unhealthy_despite_good_readings() {
        let healthy_reading = oracle(0, false);
        let r = reports(NOW - STALE_AFTER - 1, false, healthy_reading);
        let (code, body) = health_verdict(&r, NOW, STALE_AFTER);
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, "stale");
    }

    #[test]
    fn staleness_boundary_is_inclusive() {
        let r = reports(NOW - STALE_AFTER, false, oracle(0, false));
        assert_eq!(health_verdict(&r, NOW, STALE_AFTER).0, StatusCode::OK);
    }

    #[test]
    fn a_timed_out_tick_is_unhealthy() {
        let r = reports(NOW - 10, true, oracle(0, false));
        assert_eq!(health_verdict(&r, NOW, STALE_AFTER).1, "stalled");
    }

    #[test]
    fn breach_and_repeated_failure_are_unhealthy() {
        let r = reports(NOW - 10, false, oracle(0, true));
        assert_eq!(health_verdict(&r, NOW, STALE_AFTER).1, "degraded");
        let r = reports(NOW - 10, false, oracle(3, false));
        assert_eq!(health_verdict(&r, NOW, STALE_AFTER).1, "degraded");
    }

    /// One healthy chain must not mask another chain having stopped.
    #[test]
    fn one_stale_chain_fails_the_whole_check() {
        let mut m = reports(NOW - 10, false, oracle(0, false));
        m.insert(
            "arbitrum".to_string(),
            ChainReport {
                chain: "arbitrum".into(),
                updated_at: NOW - STALE_AFTER - 1,
                stalled: false,
                oracles: vec![oracle(0, false)],
            },
        );
        assert_eq!(health_verdict(&m, NOW, STALE_AFTER).0, StatusCode::SERVICE_UNAVAILABLE);
    }
}
