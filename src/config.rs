use alloy::primitives::Address;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub kms: Kms,
    #[serde(default)]
    pub cadence: Cadence,
    pub chains: Vec<Chain>,
}

#[derive(Debug, Deserialize)]
pub struct Kms {
    pub project_id: String,
    pub location: String,
    pub key_ring: String,
    pub key_name: String,
    #[serde(default = "default_key_version")]
    pub key_version: u64,
    /// The address this key must derive to.
    ///
    /// The ops Safe bakes its owner in at construction and the oracle bakes the
    /// Safe in as `automationForwarder`, neither with a setter. If the key does
    /// not derive to this address the keeper can never produce a valid
    /// signature, so startup aborts rather than discovering that per-upkeep.
    pub expected_address: Address,
}

fn default_key_version() -> u64 {
    1
}

#[derive(Debug, Deserialize)]
pub struct Cadence {
    /// How often to evaluate every oracle. Must be well below the lateness
    /// budget so a single skipped poll cannot exhaust it.
    #[serde(default = "default_poll")]
    pub poll_interval_secs: u64,
    /// Seconds past the heartbeat boundary before firing, so we never race the
    /// contract's `timeDelta >= heartbeat` check into a wasted revert.
    #[serde(default = "default_buffer")]
    pub fire_buffer_secs: u64,
    /// Per-HTTP-request timeout. Without one, alloy's reqwest transport waits
    /// forever on an unresponsive endpoint.
    #[serde(default = "default_rpc_timeout")]
    pub rpc_timeout_secs: u64,
    /// How long to wait for an `execTransaction` receipt before giving up. An
    /// underpriced or dropped transaction otherwise hangs the worker
    /// indefinitely — alloy's PendingTransactionBuilder defaults to no timeout.
    #[serde(default = "default_tx_timeout")]
    pub tx_confirm_timeout_secs: u64,
    /// Hard ceiling on one chain's whole evaluation pass.
    #[serde(default = "default_tick_timeout")]
    pub tick_timeout_secs: u64,
}

fn default_poll() -> u64 {
    300
}
fn default_buffer() -> u64 {
    60
}
fn default_rpc_timeout() -> u64 {
    30
}
fn default_tx_timeout() -> u64 {
    180
}
fn default_tick_timeout() -> u64 {
    600
}

impl Default for Cadence {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll(),
            fire_buffer_secs: default_buffer(),
            rpc_timeout_secs: default_rpc_timeout(),
            tx_confirm_timeout_secs: default_tx_timeout(),
            tick_timeout_secs: default_tick_timeout(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Chain {
    pub name: String,
    pub chain_id: u64,
    pub rpc_url: String,
    /// Threshold-1 ops Safe. Must equal every oracle's `automationForwarder`.
    pub safe: Address,
    pub oracles: Vec<Oracle>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Oracle {
    pub label: String,
    pub address: Address,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw).context("parsing config")?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.chains.is_empty() {
            bail!("config defines no chains");
        }
        for c in &self.chains {
            if c.oracles.is_empty() {
                bail!("chain {} defines no oracles", c.name);
            }
        }
        if self.cadence.poll_interval_secs == 0 {
            bail!("poll_interval_secs must be greater than zero");
        }
        for (name, v) in [
            ("rpc_timeout_secs", self.cadence.rpc_timeout_secs),
            ("tx_confirm_timeout_secs", self.cadence.tx_confirm_timeout_secs),
            ("tick_timeout_secs", self.cadence.tick_timeout_secs),
        ] {
            if v == 0 {
                bail!("{name} must be greater than zero; a zero timeout means wait forever");
            }
        }
        Ok(())
    }
}
