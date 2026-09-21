use crate::config::{Chain, Oracle};
use crate::contracts::{ISafe, ISharePriceOracle, IVault};
use crate::schedule::{decide, Decision};
use alloy::network::{Ethereum, EthereumWallet};
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::signers::gcp::GcpSigner;
use alloy::signers::Signer;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

/// Live view of one oracle, published on `/status`.
#[derive(Debug, Clone, Serialize)]
pub struct OracleStatus {
    pub chain: String,
    pub label: String,
    pub address: Address,
    pub safe_to_use: bool,
    pub kill_switch: bool,
    pub last_observation_ts: u64,
    pub seconds_until_due: Option<u64>,
    pub lateness_secs: u64,
    pub lateness_budget_secs: u64,
    /// Set when the interval overran its budget. The oracle is out of its TWAP
    /// window and withdrawals are frozen until the keeper catches up.
    pub breached: bool,
    /// Underlying vault size, so an operator can tell when the wind-down is done
    /// and the keeper can be switched off.
    pub target_total_supply: Option<String>,
    pub target_total_assets: Option<String>,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
    pub last_upkeep_tx: Option<String>,
}

impl OracleStatus {
    /// Whether this oracle should fail the health check. A single transient RPC
    /// error is not worth restarting the service over; a breach or a run of
    /// failures is.
    pub fn unhealthy(&self) -> bool {
        self.breached || self.kill_switch || self.consecutive_failures >= 3
    }
}

pub struct ChainWorker {
    name: String,
    provider: DynProvider<Ethereum>,
    safe: Address,
    oracles: Vec<Oracle>,
    fire_buffer: u64,
    tx_confirm_timeout: Duration,
    /// `None` in read-only mode: `status` is useful to an operator who has RPC
    /// access but no KMS permission. Held as a trait object so the Safe signing
    /// path can be exercised in tests with a local key rather than KMS.
    signer: Option<Arc<dyn Signer + Send + Sync>>,
}

impl ChainWorker {
    /// Read-only worker: no signer, no wallet on the provider. `fire_upkeep`
    /// refuses to run.
    pub async fn new_readonly(chain: &Chain, fire_buffer: u64) -> Result<Self> {
        let url = chain
            .rpc_url
            .parse()
            .with_context(|| format!("chain {}: rpc_url is not a valid URL", chain.name))?;
        let provider = ProviderBuilder::new()
            .with_reqwest(url, |b| {
                b.timeout(Duration::from_secs(30)).build().expect("reqwest client")
            })
            .erased();
        let actual = provider.get_chain_id().await.with_context(|| {
            format!("chain {}: could not read chain id from {}", chain.name, chain.rpc_url)
        })?;
        if actual != chain.chain_id {
            bail!(
                "chain {}: rpc reports chain id {} but config says {}",
                chain.name,
                actual,
                chain.chain_id
            );
        }
        Ok(Self {
            name: chain.name.clone(),
            provider,
            safe: chain.safe,
            oracles: chain.oracles.clone(),
            fire_buffer,
            tx_confirm_timeout: Duration::from_secs(180),
            signer: None,
        })
    }

    pub async fn new(
        chain: &Chain,
        signer: &GcpSigner,
        cadence: &crate::config::Cadence,
    ) -> Result<Self> {
        let mut chain_signer = signer.clone();
        chain_signer.set_chain_id(Some(chain.chain_id));
        let wallet = EthereumWallet::from(chain_signer.clone());
        Self::new_with(chain, wallet, Arc::new(chain_signer), cadence).await
    }

    /// Shared constructor. `wallet` pays for and signs the outer transaction;
    /// `signer` produces the inner Safe owner signature. In production both are
    /// the same KMS key.
    pub async fn new_with(
        chain: &Chain,
        wallet: EthereumWallet,
        signer: Arc<dyn Signer + Send + Sync>,
        cadence: &crate::config::Cadence,
    ) -> Result<Self> {
        let url = chain.rpc_url.parse().with_context(|| {
            format!("chain {}: rpc_url is not a valid URL", chain.name)
        })?;
        // A request timeout is mandatory: alloy's reqwest transport sets none,
        // so an unresponsive endpoint would block this worker forever.
        let rpc_timeout = Duration::from_secs(cadence.rpc_timeout_secs);
        let provider = ProviderBuilder::new()
            .wallet(wallet)
            .with_reqwest(url, move |b| {
                b.timeout(rpc_timeout).build().expect("reqwest client")
            })
            .erased();

        // Fail fast on a misrouted RPC: signing for the wrong chain would
        // produce transactions that are valid somewhere we did not intend.
        let actual = provider.get_chain_id().await.with_context(|| {
            format!("chain {}: could not read chain id from {}", chain.name, chain.rpc_url)
        })?;
        if actual != chain.chain_id {
            bail!(
                "chain {}: rpc reports chain id {} but config says {}",
                chain.name,
                actual,
                chain.chain_id
            );
        }

        Ok(Self {
            name: chain.name.clone(),
            provider,
            safe: chain.safe,
            oracles: chain.oracles.clone(),
            fire_buffer: cadence.fire_buffer_secs,
            tx_confirm_timeout: Duration::from_secs(cadence.tx_confirm_timeout_secs),
            signer: Some(signer),
        })
    }

    /// Confirm the Safe is usable before any upkeep is attempted: threshold 1,
    /// signer is an owner, and every oracle points its forwarder at this Safe.
    pub async fn preflight(&self, signer_address: Address) -> Result<()> {
        let safe = ISafe::new(self.safe, &self.provider);

        let threshold = safe.getThreshold().call().await.with_context(|| {
            format!("chain {}: no Safe at {} (getThreshold reverted)", self.name, self.safe)
        })?;
        if threshold != U256::from(1) {
            bail!(
                "chain {}: Safe {} has threshold {}, keeper requires 1",
                self.name,
                self.safe,
                threshold
            );
        }

        if !safe.isOwner(signer_address).call().await? {
            bail!(
                "chain {}: KMS signer {} is not an owner of Safe {}",
                self.name,
                signer_address,
                self.safe
            );
        }

        for o in &self.oracles {
            let oracle = ISharePriceOracle::new(o.address, &self.provider);
            let fwd = oracle.automationForwarder().call().await.with_context(|| {
                format!("chain {}: no oracle at {} ({})", self.name, o.address, o.label)
            })?;
            if fwd != self.safe {
                bail!(
                    "chain {}: oracle {} ({}) has automationForwarder {} but Safe is {} \
                     — this keeper can never drive it",
                    self.name,
                    o.address,
                    o.label,
                    fwd,
                    self.safe
                );
            }
        }

        info!(
            chain = %self.name,
            safe = %self.safe,
            oracles = self.oracles.len(),
            "preflight passed"
        );
        Ok(())
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Statuses to publish when this chain's tick exceeded its budget. Carries
    /// the previous readings forward but marks the failure, so a hang surfaces
    /// as unhealthy rather than as stale-but-fine data.
    pub fn stalled_statuses(&self, prev: &[OracleStatus]) -> Vec<OracleStatus> {
        self.oracles
            .iter()
            .map(|o| {
                let p = prev.iter().find(|p| p.address == o.address && p.chain == self.name);
                let mut st = p.cloned().unwrap_or_else(|| OracleStatus {
                    chain: self.name.clone(),
                    label: o.label.clone(),
                    address: o.address,
                    safe_to_use: false,
                    kill_switch: false,
                    last_observation_ts: 0,
                    seconds_until_due: None,
                    lateness_secs: 0,
                    lateness_budget_secs: 0,
                    breached: false,
                    target_total_supply: None,
                    target_total_assets: None,
                    consecutive_failures: 0,
                    last_error: None,
                    last_upkeep_tx: None,
                });
                st.consecutive_failures = st.consecutive_failures.saturating_add(1);
                st.last_error = Some("chain tick exceeded its timeout budget".into());
                st
            })
            .collect()
    }

    /// Evaluate every oracle on this chain once, firing upkeeps that are due.
    pub async fn tick(&self, prev: &[OracleStatus]) -> Vec<OracleStatus> {
        self.tick_inner(prev, true).await
    }

    /// Evaluate without sending anything. Used by `verify` and `status`.
    pub async fn tick_readonly(&self, prev: &[OracleStatus]) -> Vec<OracleStatus> {
        self.tick_inner(prev, false).await
    }

    async fn tick_inner(&self, prev: &[OracleStatus], fire: bool) -> Vec<OracleStatus> {
        let mut out = Vec::with_capacity(self.oracles.len());
        for o in &self.oracles {
            let previous = prev.iter().find(|p| p.address == o.address && p.chain == self.name);
            out.push(self.tick_one(o, previous, fire).await);
        }
        out
    }

    async fn tick_one(&self, o: &Oracle, prev: Option<&OracleStatus>, fire: bool) -> OracleStatus {
        let failures = prev.map(|p| p.consecutive_failures).unwrap_or(0);
        let last_tx = prev.and_then(|p| p.last_upkeep_tx.clone());

        let mut status = OracleStatus {
            chain: self.name.clone(),
            label: o.label.clone(),
            address: o.address,
            safe_to_use: false,
            kill_switch: false,
            last_observation_ts: 0,
            seconds_until_due: None,
            lateness_secs: 0,
            lateness_budget_secs: 0,
            breached: false,
            target_total_supply: prev.and_then(|p| p.target_total_supply.clone()),
            target_total_assets: prev.and_then(|p| p.target_total_assets.clone()),
            consecutive_failures: failures,
            last_error: prev.and_then(|p| p.last_error.clone()),
            last_upkeep_tx: last_tx,
        };

        match self.evaluate_and_fire(o, &mut status, fire).await {
            Ok(()) => {
                status.consecutive_failures = 0;
                status.last_error = None;
            }
            Err(e) => {
                status.consecutive_failures = failures.saturating_add(1);
                let msg = format!("{e:#}");
                error!(
                    chain = %self.name,
                    oracle = %o.label,
                    failures = status.consecutive_failures,
                    "upkeep cycle failed: {msg}"
                );
                status.last_error = Some(msg);
            }
        }
        status
    }

    async fn evaluate_and_fire(
        &self,
        o: &Oracle,
        status: &mut OracleStatus,
        fire: bool,
    ) -> Result<()> {
        let oracle = ISharePriceOracle::new(o.address, &self.provider);

        let heartbeat = oracle.heartbeat().call().await?;
        let grace = oracle.gracePeriod().call().await?;
        let obs_len = oracle.observationsLength().call().await?;
        let idx = oracle.currentIndex().call().await?;
        let obs = oracle.observations(U256::from(idx)).call().await?;
        let latest = oracle.getLatest().call().await;
        let kill = oracle.killSwitch().call().await.unwrap_or(false);

        status.kill_switch = kill;
        status.last_observation_ts = obs.timestamp;
        status.safe_to_use = matches!(&latest, Ok(l) if !l.notSafeToUse);

        // The kill switch is one-way and disables performUpkeep entirely. Do not
        // burn gas discovering that every poll.
        if kill {
            bail!("oracle {} kill switch is engaged; manual intervention required", o.label);
        }

        // Drain progress. Best-effort: totalAssets can revert for reasons that
        // have nothing to do with this oracle (a stale third-party feed), and
        // that must not stop the upkeep.
        let target = oracle.target().call().await.ok();
        if let Some(t) = target {
            let vault = IVault::new(t, &self.provider);
            status.target_total_supply =
                vault.totalSupply().call().await.ok().map(|v| v.to_string());
            status.target_total_assets =
                vault.totalAssets().call().await.ok().map(|v| v.to_string());
        }

        let now = self.block_timestamp().await?;
        let decision =
            decide(now, obs.timestamp, heartbeat, grace, obs_len, self.fire_buffer);

        match decision {
            Decision::Wait { due_in } => {
                status.seconds_until_due = Some(due_in);
                status.lateness_budget_secs =
                    crate::schedule::lateness_budget(grace, obs_len);
                Ok(())
            }
            Decision::Fire { lateness, budget } => {
                status.lateness_secs = lateness;
                status.lateness_budget_secs = budget;
                status.breached = decision.is_breach();

                if decision.is_breach() {
                    error!(
                        chain = %self.name, oracle = %o.label, lateness, budget,
                        "LATE PAST BUDGET — oracle is outside its TWAP window, withdrawals frozen"
                    );
                } else if decision.should_warn() {
                    warn!(
                        chain = %self.name, oracle = %o.label, lateness, budget,
                        "upkeep running late"
                    );
                }

                if !fire {
                    status.seconds_until_due = Some(0);
                    return Ok(());
                }

                let tx = self.fire_upkeep(o).await?;
                info!(chain = %self.name, oracle = %o.label, tx = %tx, "upkeep confirmed");
                status.last_upkeep_tx = Some(tx);
                status.seconds_until_due = Some(heartbeat);
                Ok(())
            }
        }
    }

    /// Chain time, not wall-clock time. The contract compares against
    /// `block.timestamp`, and on an L2 that can lag the keeper's own clock.
    async fn block_timestamp(&self) -> Result<u64> {
        let block = self
            .provider
            .get_block(alloy::eips::BlockId::latest())
            .await?
            .context("latest block missing")?;
        Ok(block.header.timestamp)
    }

    /// Build, sign and submit `performUpkeep(0x)` through the Safe.
    async fn fire_upkeep(&self, o: &Oracle) -> Result<String> {
        let oracle = ISharePriceOracle::new(o.address, &self.provider);
        // The keeper subclass ignores performData entirely — it reads the share
        // price from chain state — so an empty payload is correct here.
        let inner = oracle.performUpkeep(Bytes::new()).calldata().clone();

        let safe = ISafe::new(self.safe, &self.provider);
        let nonce = safe.nonce().call().await.context("reading Safe nonce")?;

        // Ask the Safe for its own EIP-712 digest rather than reconstructing the
        // domain separator here; the Safe is the authority on what it will accept.
        let safe_tx_hash = safe
            .getTransactionHash(
                o.address,
                U256::ZERO,
                inner.clone(),
                0, // CALL
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
                Address::ZERO,
                Address::ZERO,
                nonce,
            )
            .call()
            .await
            .context("computing Safe transaction hash")?;

        let signer = self
            .signer
            .as_ref()
            .context("keeper is in read-only mode; no signer configured")?;
        let sig = signer
            .sign_hash(&safe_tx_hash)
            .await
            .context("KMS refused to sign the Safe transaction hash")?;
        // Safe's checkSignatures treats v of 27/28 as a direct ecrecover over the
        // hash, which is what `as_bytes` produces.
        let signatures = Bytes::from(sig.as_bytes().to_vec());

        let pending = safe
            .execTransaction(
                o.address,
                U256::ZERO,
                inner,
                0,
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
                Address::ZERO,
                Address::ZERO,
                signatures,
            )
            .send()
            .await
            .context("submitting execTransaction")?;

        // Without an explicit timeout alloy never reaps the pending transaction,
        // so a dropped or underpriced tx would hang this worker indefinitely.
        let receipt = pending
            .with_timeout(Some(self.tx_confirm_timeout))
            .get_receipt()
            .await
            .context("awaiting execTransaction receipt")?;
        if !receipt.status() {
            bail!("execTransaction reverted in tx {:#x}", receipt.transaction_hash);
        }
        Ok(format!("{:#x}", receipt.transaction_hash))
    }
}

/// One chain's most recent evaluation, with the wall-clock time it completed.
///
/// `updated_at` exists purely so `/healthz` can detect that a worker has stopped
/// reporting. Without it, a hung worker leaves the last good statuses in place
/// and the service reports healthy forever while firing nothing.
#[derive(Debug, Clone, Serialize)]
pub struct ChainReport {
    pub chain: String,
    pub updated_at: u64,
    pub stalled: bool,
    pub oracles: Vec<OracleStatus>,
}

pub type SharedStatus = Arc<tokio::sync::RwLock<std::collections::BTreeMap<String, ChainReport>>>;

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::config::{Chain, Oracle};
    use alloy::signers::local::PrivateKeySigner;

    /// End-to-end exercise of the Safe signing path against a forked chain.
    ///
    /// This is the one part of the keeper that unit tests cannot reach: building
    /// the Safe transaction hash, producing an owner signature the Safe's
    /// `checkSignatures` accepts, and landing `execTransaction`. It runs with a
    /// local key instead of KMS — identical code path, different `Signer`.
    ///
    /// Requires a fork with a threshold-1 Safe and an oracle whose
    /// `automationForwarder` is that Safe. Ignored by default; run with
    /// `cargo test -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "requires a local anvil fork with fixtures deployed"]
    async fn fires_upkeep_through_the_safe() {
        let rpc = std::env::var("TEST_RPC").unwrap_or_else(|_| "http://127.0.0.1:8546".into());
        let safe: Address = std::env::var("TEST_SAFE").expect("TEST_SAFE").parse().unwrap();
        let oracle_addr: Address =
            std::env::var("TEST_ORACLE").expect("TEST_ORACLE").parse().unwrap();
        // anvil account 0
        let pk = std::env::var("TEST_PK").unwrap_or_else(|_| {
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".into()
        });

        let signer: PrivateKeySigner = pk.parse().unwrap();
        let wallet = EthereumWallet::from(signer.clone());

        let chain = Chain {
            name: "fork".into(),
            chain_id: 1,
            rpc_url: rpc.clone(),
            safe,
            oracles: vec![Oracle { label: "turbo-steth".into(), address: oracle_addr }],
        };

        let cadence = crate::config::Cadence::default();
        let worker = ChainWorker::new_with(&chain, wallet, Arc::new(signer.clone()), &cadence)
            .await
            .expect("worker");

        worker.preflight(signer.address()).await.expect("preflight should pass");

        // A freshly deployed oracle holds the sentinel timestamp, so the first
        // tick must decide to fire.
        let first = worker.tick(&[]).await;
        let s = &first[0];
        assert!(s.last_error.is_none(), "unexpected error: {:?}", s.last_error);
        assert!(s.last_upkeep_tx.is_some(), "no upkeep transaction was sent");
        assert_eq!(s.consecutive_failures, 0);
        assert_eq!(s.lateness_budget_secs, 1800, "budget must come from on-chain grace/length");

        // The observation must actually have advanced past the sentinel.
        let provider =
            ProviderBuilder::new().connect_http(rpc.parse().unwrap()).erased();
        let oracle = ISharePriceOracle::new(oracle_addr, &provider);
        let idx = oracle.currentIndex().call().await.unwrap();
        let obs = oracle.observations(U256::from(idx)).call().await.unwrap();
        assert!(obs.timestamp > 1, "ring slot still holds the sentinel timestamp");

        println!(
            "upkeep landed: tx={} currentIndex={} observation_ts={}",
            s.last_upkeep_tx.as_deref().unwrap(),
            idx,
            obs.timestamp
        );
    }
}
