# Restoring the Sommelier vaults

Remediation plan for the withdrawal outage that began 2026-08-05.

**Status as of 2026-09-11: nothing has been deployed. All vaults remain frozen,
day 37.** Verified on chain today — ops Safe `codesize 0`, mainnet oracle
`codesize 0`, Registry `nextId` still 27, and `previewRedeem` still reverts
`Cellar__OracleFailure()` on Turbo stETH, Real Yield ETH and Turbo rsETH.

Everything below is built and reviewed. The blocker is a signature, not code.

---

## 1. What broke

Two independent outages, both caused by third parties decommissioning
infrastructure Sommelier depended on.

**Chainlink Automation v2.1 was decommissioned.** Last `UpkeepPerformed` was
block 25690383, 2026-08-05 18:01:23 UTC. Every `ERC4626SharePriceOracle` went
stale, `getLatest()` began reporting `isNotSafeToUse`, and
`CellarWithOracle._getTotalAssetsAndTotalSupply` reverts
`Cellar__OracleFailure()` — so every withdrawal fails. The dead registry is
`0x6593c7De001fC8542bB1703532EE1E5aA0D458fD` (`typeAndVersion` =
`"KeeperRegistry 2.1.0"`).

**Redstone price feeds were abandoned.** Separately, four vaults revert inside
`totalAssets()` itself because their Redstone adapters are dead or stale. No
oracle work fixes these — a replacement oracle's first `performUpkeep` would
revert too, because it reads `totalAssets()`.

Two vaults hold no oracle of their own and are frozen purely through Turbo
stETH: Real Yield ETH and Real Yield BTC both revert in `totalAssets()` at
`0x762e003b…9b8e::getLatest()`. **One oracle deployment unfreezes three
vaults.**

Not affected: Real Yield USD on mainnet has no share-price oracle and redeems
normally. ETH-BTC Trend and Fraximal likewise.

## 2. Money at stake

Valued 2026-08-25/27 (ETH $2,469.87, BTC $79,220.66). Re-derive before
quoting to depositors.

| Track | Vault | Chain | Value |
|---|---|---|---:|
| 1 | Real Yield ETH | mainnet | $180,307 |
| 1 | Turbo stETH | mainnet | $132,504 |
| 1 | Real Yield BTC | mainnet | $38,184 |
| 1 | Real Yield ETH | arbitrum | $133,020 |
| 1 | Real Yield ETH | optimism | $31,641 |
| 1 | Real Yield USD | arbitrum | $29,642 |
| 1 | Turbo ETHx | mainnet | $3,355 |
| 2 | Turbo rsETH | mainnet | $87,316 |
| 2 | Turbo ezETH | mainnet | $26,677 |
| 2 | Turbo eETH V2 | mainnet | $10,600 |
| 2 | Turbo swETH | mainnet | $5,397 |
| | **Total** | | **~$678,600** |

Dust, deliberately excluded: Turbo SOMM ($391 — SOMM at $0.00037263), Turbo GHO
($176), Morpho ETH Max ($103), Turbo divETH ($53). TurboStETH-Deposit ($3,981)
needs its own oracle and is not currently in scope.

## 3. What is already built

| Artifact | Where | State |
|---|---|---|
| `ERC4626SharePriceOracleKeeper` | [cellar-contracts#197](https://github.com/PeggyJV/cellar-contracts/pull/197) | open, 4 commits |
| `somm-oracle-keeper` service | [somm-oracle-keeper#1](https://github.com/PeggyJV/somm-oracle-keeper/pull/1) | open, 3 commits |

The contract differs from the audited upstream by **one keyword** — `virtual`
on `performUpkeep`, line 407 — plus a subclass whose body is byte-identical
apart from reading the share price from chain state instead of decoding it from
caller-supplied `performData`.

Reviewed three times independently: Codex on the Rust (one critical finding,
fixed in `e046187`), Codex on the Solidity (manipulation analysis, see §7), and
CodeRabbit on the contract (zero-keeper address, fixed in `4bf6244`).

Verification suites in `0fc271b`:

- **Derivation equivalence** — the subclass produces byte-identical storage to
  the audited base whenever `performData` matches chain state, across single
  steps, sequences, cold start and reverting paths. 4096 fuzz runs per property.
  If this holds, audit findings on the base transfer by construction.
- **TWAP window invariants** — a keeper spacing upkeeps within
  `[heartbeat, heartbeat + gracePeriod/(L-2)]` keeps `getLatest()` usable for
  any sequence in that band. 512 runs x 64 depth, 32,768 calls, no
  counterexample. Bound asserted tight in both directions.

## 4. Authority — who can do what

Three distinct authorities. They are not interchangeable, and confusing them is
the most likely way to build a batch that cannot execute.

| Authority | Address | Controls |
|---|---|---|
| Cork authority | `somm1lcsjy2d5s33h0sddd8lpuqvwyz5ruz7ju4aeqa` | `x/cork` and `x/axelarcork` — single signature |
| Registry / PriceRouter Safe | `0x7340D1FeCD4B64A4ac34f826B21c945d44d7407F` | mainnet Registry + PriceRouter, **4 of 7** |
| L2 Registry Safe | `0x85974Dc8978De3Ba84E8B1D0CC67b54F40028Eda` | Arbitrum + Optimism Registry, **4 of 7** |
| Gravity Bridge | `0x69592e6f9d21989a043646fE8225da2600e5A0f7` | owns every mainnet Cellar — reached only via cork |
| Axelar proxy | `0xEe75bA2C81C04DcA4b0ED6d1B7077c188FEde4d2` | owns L2 Cellars — reached via `x/axelarcork` |

**Sommelier v10.0.2 is what makes this tractable.** Before 2026-08-21 a cork
needed a >67% validator power tally. v10 removed that path entirely and replaced
it with a single governance-controlled `cork_authority`:

```go
// x/axelarcork/keeper/msg_server.go
if params.CorkAuthority == "" || signer.String() != params.CorkAuthority {
    return nil, errorsmod.Wrapf(sdkerrors.ErrUnauthorized, ...)
}
```

Re-verified 2026-09-11: chain on v10.0.2, `sommelier-3`, same cork authority,
`x/poa` safe mode inactive with 4 bonded authority validators.

Every vault in scope is already in the relevant cork allowlist. No governance
proposal is required.

## 5. Deterministic addresses

All CREATE2 via the canonical proxy `0x4e59b44847b379578588920cA78FbF26c0B4956C`
at salt 0. Fixed by initcode alone, so they hold regardless of when or by whom
deployment happens. `_startingAnswer` is pinned to each vault's last good
on-chain answer — a historical constant, not a live read — so the addresses do
not drift.

```
ops Safe (all three chains)   0xf267823cf091917b3072245566cd073833aff65b
  owners    [0x187559Cfd96d41B4E1343bDc1b36362E817b2F83,
             0x096CA3674329bB66dD7CC14D1511dfB7728b9193]
  threshold 1     singleton v1.3.0     saltNonce 0

mainnet  Turbo stETH          0x2e212d0315381da0db13e95d35d896ab697af776
arbitrum Real Yield USD       0x542967ea8378226868935ea38528d2534c5d08ef
arbitrum Real Yield ETH       0x1034ccff4713dbbb1d6444e109e49bd77195fd6c
optimism Real Yield ETH       0xe46b510c59e0551774cbb23363813eda2e80aa91
```

Constructor parameters, all four: `heartbeat 43200` (12h), `deviationTrigger 50`,
`gracePeriod 86400` (24h), `observationsToUse 3` (ring length 4), bounds
`7500 / 12500`, keeper = the ops Safe. L2s additionally set their sequencer
uptime feed (Arbitrum `0xFdB631F5EE196F0ed6FAa767959853A9F217697D`, Optimism
`0x371EAD81c9102C9BF4874A9075FFFf170F2Ee389`) with a 3600s grace.

That gives a **24h warm-up** before withdrawals reopen, and **12h of lateness
budget** per interval thereafter.

The ops Safe's first owner is a GCP KMS key: `share-price-oracle-keeper`
version 1 in `peggyjv-services`, `EC_SIGN_SECP256K1_SHA256`, HSM. Verified
2026-08-27 to derive to `0x187559Cf…2F83`, with
`oracle-keeper@peggyjv-services.iam.gserviceaccount.com` holding
`roles/cloudkms.signerVerifier`. **Not re-verified 2026-09-11** — the active
gcloud account lacks permission on that project. Re-run `somm-oracle-keeper
verify` before deploying; it checks the derivation and sends nothing.

## 6. Track 1 — the oracle replacement

Deploy and warm the oracle **before** the cork. The oracle reads
`target.totalAssets()` independently of whether the cellar points at it yet, so
warm-up runs in parallel with collecting signatures, and withdrawals reopen the
moment the cork lands rather than 24h later.

1. **Create the ops Safe** on all three chains. Must land at
   `0xf267823c…f65b`. If it does not, stop — every oracle address depends on it.
   *Only the key holder can do this. This is the blocker.*
2. **CREATE2 deploy the oracles.** Permissionless; anyone can send it. Verify
   each reports the expected `target()` and an `automationForwarder()` equal to
   the ops Safe.
3. **Start the keeper.** `performUpkeep(0x)` every 12 hours from the ops Safe.
   Goes safe after three upkeeps. Withdrawals stay frozen through warm-up —
   expected, not a failure.
4. **`register()` in the Registry.** 4-of-7. Mainnet `0xEED68C26…4476`; both L2s
   share `0xB7f57c1a…91BB`. **Read back the assigned id** — it is whatever
   `nextId` happens to be, not a constant. It was 27 on the fork and is still 27
   as of today, but do not hardcode it.
5. **Schedule and relay the cork.** Build `setSharePriceOracle(id, oracle)` with
   the id observed in step 4. Mainnet via `x/cork`, executing as the Gravity
   Bridge; L2s via `x/axelarcork` to the Axelar proxy on chain ids 42161 and 10,
   33.67 SOMM bridge fee each. Confirm `cellar.sharePriceOracle()` before moving
   on.

Dress-rehearsed end to end on forks. A real redemption of 5 Real Yield ETH
shares returned `5.854359884237941041` WETH — exactly `1.170871977` per share,
matching the oracle to the wei. Arbitrum and Optimism redeemed likewise.

## 7. Why reading the price on-chain is safe here

The subclass reads the share price inside `performUpkeep` rather than trusting a
caller-supplied value. An adversarial review produced a working proof of concept
showing that whatever `totalAssets()` returns at execution time is written into
the answer and the ring, and that the ±25% kill switch does not prevent it. That
is a genuine change of trust assumption, **not** a strict improvement — the base
is weaker on provenance and stronger on atomic-manipulation resistance, and
neither dominates.

It is not exploitable against these vaults, for two independent reasons.

**Every price is push-based.** Tracing `totalAssets()` on each cellar and reading
`PriceRouter.getAssetSettings` for every asset priced:

| Chain | Assets priced | Derivative |
|---|---|---|
| mainnet · Turbo stETH | WETH, stETH (dust) | 1 — Chainlink |
| arbitrum · RY USD | USDC, DAI, USD₮0, USDC.e | 1 — Chainlink |
| arbitrum · RY ETH | WETH, rETH | 1 — Chainlink |
| optimism · RY ETH | WETH, wstETH, rETH | 1 — Chainlink |

No Uniswap TWAP sources (derivative 2), no extensions (derivative 3), zero
Redstone calls in any trace.

**No balance is reversibly movable.** Turbo stETH has exactly three non-zero
contributions:

```
WETH  42.018861074946608064   plain ERC20 balance
WETH  11.629417134569303371   Lido withdrawal queue claim
stETH  0.000005009728865102   dust
```

Moving the ERC20 balance requires a donation, which cannot be taken back. The
11.63 WETH is a claim in Lido's stETH Withdrawal NFT (`0xE42C659D…94D9`,
confirmed by its canonical `STETH()`/`WSTETH()`), set by Lido's finalisation of
a request the cellar owns. The proof of concept reverses its manipulation by
burning tokens out of the vault's own balance — standing in for a flash-loan
round trip through a spot-priced position. No such position exists here.

The Cellar also consumes the oracle conservatively, taking
`min(latestAnswer, timeWeightedAverageAnswer)` for withdrawals, so a single-block
spike does not reach the value that gates redemptions.

> **This is a property of the current positions, not of the contract.** If a
> position priced from a spot or AMM source is ever added, the manipulation
> becomes live. Position additions are governance-gated behind the registry and
> a cork, and these vaults are winding down rather than being actively managed,
> so the exposure is bounded — but it is a standing constraint.

## 8. Track 2 — the Redstone vaults

Must complete **before** the oracle work for these four, not alongside it.

| Vault | Blocked by | Recovers | Fix |
|---|---|---:|---|
| Turbo rsETH | dead feed adapter | 35.678 WETH | Chainlink `rsETH / ETH Exchange Rate` `0x9d2F2f96…8549` |
| Turbo ezETH | stale since 2026-08-20 | 10.801 WETH | Chainlink `ezETH / ETH` `0x636A0002…641C` |
| Turbo eETH V2 | weETH, transitively | 4.351 WETH | Chainlink `weETH / ETH` `0x5c9C449B…f22` — eETH itself needs no change |
| Turbo swETH | stale since 2025-05-30 | 2.264 WETH | **new contract** — no Chainlink feed exists |

Three of four need zero new code: the feeds slot in as
`derivative = 1, inETH = true`, the pattern this router already uses for rETH and
cbETH. The largest resulting share-price move is swETH at +3.6%, far inside the
±25% band, so repricing trips no kill switch.

`PriceRouter.addAsset` reverts if the asset is already added, so the only path is
the two-step `startEditAsset` → wait → `completeEditAsset`. `EDIT_ASSET_DELAY` is
**604800 (7 days)** and `completeEditAsset` reverts unless the supplied
`_expectedAnswer` is within `EXPECTED_ANSWER_DEVIATION = 2e16` (2%) — so recompute
it at submission time.

> **Turbo rsETH depositors receive rsETH, not ETH.** Total on-chain rsETH pool
> inventory is about 12.46 against 32.457 to sell, and Curve has no pool at all,
> so the position cannot be traded out at any acceptable price. Kelp's own
> withdrawal queue does pay exact NAV, but `KelpDAOStakingAdaptor` implements
> only `_mintERC20` — the base `StakingAdaptor`'s `_requestBurn` and
> `_completeBurn` are hard reverts. **The Cellar can stake into rsETH and can
> never unstake out of it.** Depositors receive rsETH in kind and must redeem it
> themselves through Kelp. Recovering ETH instead needs a new unstaking adaptor
> plus a governance round — and the deployed Kelp ABI is
> `initiateWithdrawal(address,uint256,string)`, not the two-argument form this
> repo's interface assumes.

**Turbo rsETH also needs position surgery.** Only 0.660 of 35.678 WETH is
withdrawable (1.85%) because its rsETH positions carry `configurationData`
decoding to `false`, and there is no setter. A distrusted position id can never
be re-trusted — `trustPosition` reverts on `pData.adaptor != address(0)` — so the
position must be re-registered under a *different* ERC20 adaptor to obtain a
fresh position hash. Rehearsed: withdrawable goes 1.85% → 100% with `totalAssets`
unchanged.

> **Do the surgery while no oracle is live.** Between `forcePositionOut` and
> `addPosition` the vault reports `totalAssets` of 0.660 against 35.678 — a
> **−98.15%** excursion against a ±25% kill-switch band. A single upkeep landing
> in that window kills the oracle permanently. The old oracles are already dead
> and the new ones do not exist yet: that gap is the safe window, and using it
> removes any need for the calls to be atomic.

Sequence:

| Day | Authority | Action |
|---|---|---|
| 0 | Safe 4/7 | four `startEditAsset` — starts the 7-day clock. Deploy the swETH extension first. |
| 7 | Safe 4/7 | four `completeEditAsset` with freshly computed `_expectedAnswer`; then `trustPosition` for the new rsETH id and `distrustPosition` on the old. `trustPosition` requires `priceRouter.isSupported`, so it only works after repricing lands. |
| 7 | Cork | `addPositionToCatalogue`, `forcePositionOut`, `addPosition` — in the no-live-oracle window. |
| 7+ | Both | four more keeper oracles, register, cork, 24h warm-up. |

These four vaults' existing oracles point at forwarders on the same dead
registry, so they cannot be revived either. Seed `_startingAnswer` from the
measured share prices: rsETH 1.14529, ezETH 1.07390, eETH V2 1.07544,
swETH 1.12498.

The swETH extension is **new unaudited code** and should be made asset-bound
before use. The draft is generic — it takes an arbitrary rate-provider address
and selector and hardcodes a `1e18` divisor where the audited `weEthExtension`
derives it from `decimals()`. Mirroring that audited contract exactly removes
both problems and costs nothing, since exactly one asset needs it. swETH's own
rate source is healthy: `lastRepriceUNIX` was 5.5 hours old when checked on
2026-08-27.

## 9. Running the keeper

`performUpkeep` costs **753,548 gas** in steady state, of which 91% is
`Cellar.totalAssets()` pricing Turbo stETH's 18 positions through the
PriceRouter; the Safe adds ~31k and the oracle logic ~37k. There is no saving
available inside the keeper — cadence is the only lever, which is why the
heartbeat is 12h rather than the 1h originally chosen.

At 0.26 gwei: roughly **$29/month mainnet, $2.25 Arbitrum, $0.12 Optimism**.
Scales linearly with gas price.

The service refuses to start unless the KMS key derives to the expected address,
every RPC's chain id matches config, each Safe has threshold 1 with the signer as
an owner, and every oracle's `automationForwarder` is that Safe. All of those are
immutable post-deployment, so failing at boot beats failing one upkeep at a time.

`/healthz` returns 503 on staleness, a timed-out tick, a breach, an engaged kill
switch, or three consecutive failures. **Alert on upkeep reverts, not just
process uptime** — `performUpkeep` calls `totalAssets()`, so a stale third-party
feed reverts the upkeep while the keeper itself looks healthy.

**These vaults are winding down; the keeper is not permanent.** `/status` reports
`target_total_supply` and `target_total_assets` per oracle. Drop a vault from the
config once drained, and shut the service down when all are. Leaving it running
past that point burns gas for nobody.

## 10. Open items

- **The ops Safe does not exist.** Everything downstream is permissionless, a
  4-of-7 signature, or a single cork. This is the only true blocker.
- The swETH extension needs to be made asset-bound before use.
- `TurboStETH-Deposit` ($3,981) needs its own oracle; not currently in scope.
- Neither PR has CI.
- Turbo rsETH's in-kind outcome should be communicated to depositors before they
  attempt to withdraw.

### Not verified

- Whether a cork can carry multiple calls in one batch.
- The rsETH cellar's `allowedRebalanceDeviation` (internal, unexposed on that
  deployment).
- The cellars' adaptor and position catalogue contents — both mappings are
  internal and archive `eth_getLogs` was refused. Moot for the conclusion, since
  rsETH's market liquidity is absent either way.
- Whether Kelp would unlock the withdrawal queue nonce in practice.
- The KMS derivation, as of 2026-09-11 — see §5.
- The 24h warm-up end to end. The mechanism was verified at a 1h heartbeat; at
  12h a fork cannot reproduce it, because warping past the PriceRouter's own 24h
  WETH heartbeat makes `totalAssets()` revert with `PriceRouter__StalePrice`. The
  contract logic is parameter-independent and the arithmetic is unit-tested, but
  the 24h path is inferred rather than observed.
