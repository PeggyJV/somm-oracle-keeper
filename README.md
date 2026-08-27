# somm-oracle-keeper

Drives `performUpkeep` on Sommelier `ERC4626SharePriceOracleKeeper` contracts
across Ethereum, Arbitrum and Optimism, signing through a threshold-1 Gnosis
Safe with a GCP KMS key.

Replaces Chainlink Automation v2.1, which was decommissioned on 5 August 2026.

## Why this must not stop

The oracle's TWAP window is a band, not a floor. `getLatest()` reports
`notSafeToUse` unless the span between the oldest and most recently completed
observation sits inside

    [ heartbeat * (L-2) , heartbeat * (L-2) + gracePeriod ]

With the deployed parameters (heartbeat 43200, gracePeriod 86400, length 4) that
allows **12 hours of lateness per interval**. Exceed it and `previewRedeem`
reverts with `Cellar__OracleFailure()` — withdrawals freeze until the keeper
catches up. `schedule.rs` derives this budget from on-chain values rather than
hardcoding it, and the unit tests reconstruct the contract's own arithmetic.

Firing early is harmless: the contract reverts with
`ERC4626SharePriceOracle__NoUpkeepConditionMet` and nothing changes.

## Commands

    somm-oracle-keeper verify    # check KMS key, Safe threshold/ownership, forwarder wiring
    somm-oracle-keeper status    # read-only fleet state, no KMS needed
    somm-oracle-keeper run       # the loop, plus /healthz and /status on $PORT

`verify` is safe to run at any time and sends no transactions. Run it before
every deploy.

## Startup guards

The service refuses to start if any of these fail, rather than discovering them
one upkeep at a time:

- the KMS key does not derive to `kms.expected_address`
- an RPC reports a different chain id than the config claims
- the Safe's threshold is not 1
- the KMS signer is not an owner of the Safe
- any oracle's `automationForwarder` is not the configured Safe

The last three matter because `automationForwarder` and the Safe owner set are
both fixed at construction. A mismatch cannot be fixed by reconfiguring the
keeper — it needs a redeploy.

## Health

Each chain runs as its own task and publishes its own report as soon as it
finishes, so chains do not share a failure domain and a slow chain never hides
another's progress.

`/healthz` returns 503 when a chain has stopped reporting (`stale`), when a
chain's evaluation exceeded `tick_timeout_secs` (`stalled`), or when any oracle
is breached, has its kill switch engaged, or has failed three consecutive cycles
(`degraded`).

**Staleness is the load-bearing check.** Every other signal reads data a worker
published; if a worker hangs, only staleness catches it. All three timeouts
(`rpc_timeout_secs`, `tx_confirm_timeout_secs`, `tick_timeout_secs`) exist
because alloy defaults to no timeout on both HTTP requests and transaction
confirmation — an unresponsive RPC endpoint would otherwise block a worker
indefinitely while it reported its last good readings forever.

Alert on 503, and on the `upkeep running late` and `LATE PAST BUDGET` log lines.

Alert on upkeep *reverts*, not just process uptime. `performUpkeep` calls
`target.totalAssets()`, which prices positions through the PriceRouter, so a
stale Chainlink feed reverts the upkeep while the keeper itself looks healthy.

## Cost

Measured 753,548 gas per steady-state upkeep (768,759 cold). 91% of that is
`Cellar.totalAssets()` pricing Turbo stETH's 18 positions through the
PriceRouter; the Safe adds only ~31k and the oracle logic ~37k. There is no
meaningful saving available inside the keeper — cost is set by cadence alone.

At 0.26 gwei and a 12h heartbeat (60 upkeeps/month): roughly **$29/month on
mainnet**, $2.25 on Arbitrum, $0.12 on Optimism. It scales linearly with gas
price, so budget for spikes and monitor the Safe balances.

The heartbeat is a constructor argument, so changing it means a new oracle
address, a new `register()` and a new cork.

## Stopping

These vaults are being wound down; the keeper is not permanent infrastructure.
`/status` reports `target_total_supply` and `target_total_assets` per oracle.
When a vault's supply reaches dust, stop keeping it alive: remove it from the
config and redeploy. When every vault is drained, shut the service down. Leaving
it running past that point burns gas for nobody's benefit.

## Testing

    cargo test                        # scheduling invariants, no network
    cargo test -- --ignored           # end-to-end Safe signing against a fork

The ignored test needs `TEST_RPC`, `TEST_SAFE`, `TEST_ORACLE` pointing at a fork
with a threshold-1 Safe and an oracle forwarding to it. It exercises the exact
production signing path with a local key in place of KMS.

## IAM

The service account needs `cloudkms.cryptoKeyVersions.viewPublicKey` (startup)
and `cloudkms.cryptoKeyVersions.useToSign` (per upkeep). Both are included in
`roles/cloudkms.signerVerifier`, which is what is granted.

Verified 2026-08-27 against `peggyjv-services`: key
`somm-oracle-keeper/share-price-oracle-keeper` version 1, EC_SIGN_SECP256K1_SHA256,
HSM, ENABLED, derives to `0x187559Cfd96d41B4E1343bDc1b36362E817b2F83` — the
address baked into the ops Safe and every oracle's `automationForwarder`.
`oracle-keeper@peggyjv-services.iam.gserviceaccount.com` holds
`roles/cloudkms.signerVerifier` on it.
