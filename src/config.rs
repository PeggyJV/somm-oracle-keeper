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
}

fn default_poll() -> u64 {
    300
}
fn default_buffer() -> u64 {
    60
}

impl Default for Cadence {
    fn default() -> Self {
        Self { poll_interval_secs: default_poll(), fire_buffer_secs: default_buffer() }
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
        Ok(())
    }
}
