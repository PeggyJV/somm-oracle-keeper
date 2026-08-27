//! Upkeep scheduling.
//!
//! The oracle's TWAP window is a band, not a floor. `_getTimeWeightedAverageAnswer`
//! rejects the answer when the span between the oldest and the most recently
//! completed observation falls outside
//!
//!     [ heartbeat * (L - 2) , heartbeat * (L - 2) + gracePeriod ]
//!
//! where `L` is `observationsLength`. That span is accumulated over `L - 2`
//! keeper intervals, so if every interval has the same spacing `S`:
//!
//!     heartbeat * (L-2)  <=  S * (L-2)  <=  heartbeat * (L-2) + gracePeriod
//!
//! which reduces to the per-interval bound this module enforces:
//!
//!     heartbeat  <=  S  <=  heartbeat + gracePeriod / (L - 2)
//!
//! Firing early is harmless — `performUpkeep` reverts with
//! `ERC4626SharePriceOracle__NoUpkeepConditionMet` and nothing changes. Firing
//! late is what refreezes the vault, so the whole budget here is lateness.

/// Per-interval lateness budget in seconds, derived from on-chain parameters.
///
/// With the deployed parameters (heartbeat 43200, grace 86400, length 4) this is
/// 43200s — the keeper may run up to 12 hours behind schedule on any given
/// interval before the oracle leaves its window and withdrawals refreeze.
pub fn lateness_budget(grace_period: u64, observations_length: u16) -> u64 {
    let l = observations_length as u64;
    // L must exceed 2 for the window to be defined at all; the contract's own
    // constructor enforces `observationsToUse + 1`, so L >= 2 always holds.
    // Guard anyway rather than divide by zero on a misconfigured oracle.
    if l <= 2 {
        return 0;
    }
    grace_period / (l - 2)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Not yet due. `due_in` is seconds until the next fire time.
    Wait { due_in: u64 },
    /// Due now. `lateness` is how far past the ideal fire time we already are,
    /// and `budget` is the point at which the oracle leaves its window.
    Fire { lateness: u64, budget: u64 },
}

impl Decision {
    /// True when we are late enough that an operator should be told, but the
    /// oracle is still inside its window. Set at 60% of the budget so there is
    /// room to react before withdrawals actually break.
    pub fn should_warn(&self) -> bool {
        match *self {
            Decision::Fire { lateness, budget } => budget > 0 && lateness * 5 >= budget * 3,
            Decision::Wait { .. } => false,
        }
    }

    /// True when the interval has already exceeded its budget. The oracle is
    /// either unsafe now or will be once this observation lands.
    pub fn is_breach(&self) -> bool {
        match *self {
            Decision::Fire { lateness, budget } => lateness > budget,
            Decision::Wait { .. } => false,
        }
    }
}

/// Decide whether to fire an upkeep.
///
/// `last_observation_ts` is the timestamp of `observations[currentIndex]`, which
/// is the most recent write. The next observation is due one `heartbeat` later.
/// `fire_buffer` pushes us just past that boundary so we never race the
/// contract's `timeDelta >= heartbeat` check and waste a reverting transaction.
pub fn decide(
    now: u64,
    last_observation_ts: u64,
    heartbeat: u64,
    grace_period: u64,
    observations_length: u16,
    fire_buffer: u64,
) -> Decision {
    let budget = lateness_budget(grace_period, observations_length);

    // A ring slot that has never been written carries the sentinel timestamp 1.
    // Warm-up: fire immediately and keep firing until the buffer is populated.
    if last_observation_ts <= 1 {
        return Decision::Fire {
            lateness: 0,
            budget,
        };
    }

    let due_at = last_observation_ts.saturating_add(heartbeat);
    let fire_at = due_at.saturating_add(fire_buffer);

    if now < fire_at {
        Decision::Wait {
            due_in: fire_at - now,
        }
    } else {
        Decision::Fire {
            lateness: now.saturating_sub(due_at),
            budget,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Production parameters for every oracle in this deployment.
    const HB: u64 = 3600;
    const GRACE: u64 = 3600;
    const LEN: u16 = 4;
    const BUF: u64 = 60;

    #[test]
    fn budget_matches_the_contract_window() {
        // heartbeat*(L-2) = 7200 (min), +grace = 10800 (max), over L-2 = 2
        // intervals => 1800s of slack per interval.
        assert_eq!(lateness_budget(GRACE, LEN), 1800);
    }

    // Deployed parameters: heartbeat 12h, grace 24h, length 4.
    const DEPLOYED_HB: u64 = 43_200;
    const DEPLOYED_GRACE: u64 = 86_400;

    #[test]
    fn deployed_parameters_give_twelve_hours_of_margin() {
        assert_eq!(lateness_budget(DEPLOYED_GRACE, LEN), 43_200);

        // Warm-up is (L-2) intervals, i.e. 24h before getLatest() is usable.
        let warm_up = DEPLOYED_HB * (LEN as u64 - 2);
        assert_eq!(warm_up, 86_400);

        // Every spacing from on-time to 12h late must keep the span inside the
        // contract's window.
        let min_duration = DEPLOYED_HB * (LEN as u64 - 2);
        let max_duration = min_duration + DEPLOYED_GRACE;
        for late in [0, 3_600, 21_600, 43_200] {
            let span = (DEPLOYED_HB + late) * (LEN as u64 - 2);
            assert!(span >= min_duration && span <= max_duration, "late by {late} escapes window");
        }
        // One second past the budget must fall out.
        let span = (DEPLOYED_HB + 43_201) * (LEN as u64 - 2);
        assert!(span > max_duration);
    }

    #[test]
    fn deployed_parameters_do_not_fire_early() {
        let last = 1_000_000;
        let d = decide(last + 43_000, last, DEPLOYED_HB, DEPLOYED_GRACE, LEN, BUF);
        assert!(matches!(d, Decision::Wait { .. }), "fired before the heartbeat elapsed");
        let d = decide(last + 43_260, last, DEPLOYED_HB, DEPLOYED_GRACE, LEN, BUF);
        assert!(matches!(d, Decision::Fire { .. }));
    }

    #[test]
    fn budget_is_zero_when_window_undefined() {
        assert_eq!(lateness_budget(GRACE, 2), 0);
        assert_eq!(lateness_budget(GRACE, 0), 0);
    }

    #[test]
    fn longer_ring_divides_the_same_grace_over_more_intervals() {
        // The production oracles this replaces used length 6, which leaves only
        // 28800/4 = 7200s per interval against a 86400s heartbeat.
        assert_eq!(lateness_budget(28800, 6), 7200);
    }

    #[test]
    fn waits_until_one_heartbeat_plus_buffer_has_passed() {
        let last = 1_000_000;
        assert_eq!(
            decide(last, last, HB, GRACE, LEN, BUF),
            Decision::Wait { due_in: 3660 }
        );
        assert_eq!(
            decide(last + 3599, last, HB, GRACE, LEN, BUF),
            Decision::Wait { due_in: 61 }
        );
        // One second before the buffer elapses we still hold.
        assert_eq!(
            decide(last + 3659, last, HB, GRACE, LEN, BUF),
            Decision::Wait { due_in: 1 }
        );
    }

    #[test]
    fn fires_once_past_the_buffer() {
        let last = 1_000_000;
        let d = decide(last + 3660, last, HB, GRACE, LEN, BUF);
        assert_eq!(
            d,
            Decision::Fire {
                lateness: 60,
                budget: 1800
            }
        );
        assert!(!d.should_warn());
        assert!(!d.is_breach());
    }

    #[test]
    fn unwritten_ring_slot_fires_immediately() {
        // Freshly deployed oracle: every slot holds the sentinel timestamp 1.
        let d = decide(1_000_000, 1, HB, GRACE, LEN, BUF);
        assert_eq!(
            d,
            Decision::Fire {
                lateness: 0,
                budget: 1800
            }
        );
    }

    #[test]
    fn warns_at_sixty_percent_of_budget() {
        let last = 1_000_000;
        // 1080s late = exactly 60% of 1800.
        assert!(decide(last + HB + 1080, last, HB, GRACE, LEN, BUF).should_warn());
        assert!(!decide(last + HB + 1079, last, HB, GRACE, LEN, BUF).should_warn());
    }

    #[test]
    fn breaches_only_past_the_budget() {
        let last = 1_000_000;
        // Exactly at budget the span equals maxDuration, which the contract
        // accepts (`timeDelta > maxDuration` is the rejection).
        assert!(!decide(last + HB + 1800, last, HB, GRACE, LEN, BUF).is_breach());
        assert!(decide(last + HB + 1801, last, HB, GRACE, LEN, BUF).is_breach());
    }

    #[test]
    fn spacing_at_the_budget_edge_keeps_the_span_inside_the_window() {
        // Reconstruct the contract's own arithmetic to prove the per-interval
        // bound really does keep timeDelta within [minDuration, maxDuration].
        let min_duration = HB * (LEN as u64 - 2);
        let max_duration = min_duration + GRACE;
        for spacing in [HB, HB + 900, HB + 1800] {
            let span = spacing * (LEN as u64 - 2);
            assert!(span >= min_duration, "spacing {spacing} underruns");
            assert!(span <= max_duration, "spacing {spacing} overruns");
        }
        // One second beyond the budget must fall outside.
        let span = (HB + 1801) * (LEN as u64 - 2);
        assert!(span > max_duration);
    }
}
