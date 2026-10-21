//! Holder rewards on coins quoted in tokens (`docs/token-quotes.md` §7.5.1): the reward kind and
//! the round in `KitConfig.reserved`, and a holding's `RoundData` (bytes 32..56), which pays each
//! holding on the least it held from the start of the round before.

use super::*;
use crate::state::RoundData;
use bordrless_hook::HOOK_DATA_LEN;

#[test]
fn the_reward_kind_and_the_round_live_in_reserved_bytes_every_older_kit_has_zero() {
    let mut kit = setup(modules::HOLDER_REWARDS);
    // A SOL kit, and a coin quoted in tokens before §7.5: kind 0, the accumulator as it was.
    assert_eq!(kit.config.reward_kind(), 0);
    assert!(kit.config.accumulates() && !kit.config.pays_rounds() && !kit.config.pays_token());
    assert_eq!(kit.config.reserved[REWARD_KIND_OFFSET..], [0u8; 45][..]);
    kit.config.set_token_quoted();
    assert!(kit.config.accumulates());
    // Kind 1: one leg's token through the accumulator.
    kit.config.set_reward_kind(REWARD_KIND_TOKEN);
    kit.config.set_reward_token(1, 8);
    assert!(kit.config.accumulates() && kit.config.pays_token() && !kit.config.pays_rounds());
    assert_eq!(kit.config.reward_token(), (1, 8));
    assert_eq!(
        (kit.config.reserved[15], kit.config.reserved[16]),
        (1, 8),
        "bytes 391 and 392 of the config"
    );
    // Kind 2: rounds, never the accumulator.
    kit.config.set_reward_kind(REWARD_KIND_ROUNDS);
    kit.config.set_round_secs(DEFAULT_ROUND_SECS);
    assert!(!kit.config.accumulates() && kit.config.pays_rounds() && !kit.config.pays_token());
    assert_eq!(kit.config.round_secs(), 3_600);
    assert_eq!(kit.config.reserved[11..15], 3_600u32.to_le_bytes());
    assert_eq!(kit.config.round_at(NOW), (NOW / 3_600) as u32);
    assert_eq!(
        kit.config.round_at(NOW + 3_599 - NOW % 3_600),
        (NOW / 3_600) as u32
    );
    assert_eq!(
        kit.config.round_at(NOW + 3_600 - NOW % 3_600),
        (NOW / 3_600) as u32 + 1
    );
    // Without holder rewards no kind pays anything.
    let mut off = setup(modules::MAX_WALLET);
    off.config.set_reward_kind(REWARD_KIND_ROUNDS);
    assert!(!off.config.accumulates() && !off.config.pays_rounds() && !off.config.pays_token());
}

#[test]
fn a_holding_is_paid_on_the_least_it_held_since_the_start_of_the_round_before() {
    // Bought 100 in round 10: nothing for rounds 10 and 11 (it held nothing at their start).
    let mut r = RoundData::default();
    r.on_transfer(10, 0, 100);
    assert_eq!(r.eligible_balance(10, 100), 0);
    assert_eq!(r.eligible_balance(11, 100), 0);
    assert_eq!(
        r.eligible_balance(12, 100),
        100,
        "held through rounds 11 and 12"
    );
    // In round 12 it sells to 40, then dust lifts it to 41: rounds 12 and 13 pay on 40.
    r.on_transfer(12, 100, 40);
    r.on_transfer(12, 40, 41);
    assert_eq!(r.eligible_balance(12, 41), 40);
    assert_eq!(r.eligible_balance(13, 41), 40);
    assert_eq!(r.eligible_balance(14, 41), 41);
    // A buy to 500 in round 14: round 14 and 15 still pay on 41, round 16 on 500.
    r.on_transfer(14, 41, 500);
    assert_eq!(r.eligible_balance(14, 500), 41);
    assert_eq!(r.eligible_balance(15, 500), 41);
    assert_eq!(r.eligible_balance(16, 500), 500);
    // Paid in round 16, then emptied and filled again within it: the record cleared, the round
    // pays the refill nothing (no second payment in a round), the next nothing, the one after it.
    r.paid = 16;
    r.on_transfer(16, 500, 0);
    assert_eq!(r, RoundData::default(), "an emptied holding keeps nothing");
    r.on_transfer(16, 0, 700);
    assert_eq!(r.eligible_balance(16, 700), 0);
    assert_eq!(r.eligible_balance(17, 700), 0);
    assert_eq!(r.eligible_balance(18, 700), 700);
}

#[test]
fn a_rolled_record_counts_only_the_rounds_it_saw() {
    // Last updated in round 5 (least 30, the round before 20), holding 30 since.
    let mut r = RoundData {
        mark: 5,
        paid: 4,
        min_cur: 30,
        min_prev: 20,
    };
    assert_eq!(r.eligible_balance(5, 30), 20);
    assert_eq!(r.eligible_balance(6, 30), 30);
    // Rolled one round on (a transfer in round 6): round 5's least becomes the round before's.
    let mut next = r;
    next.roll(6, 30);
    assert_eq!((next.mark, next.min_prev, next.min_cur), (6, 30, 30));
    // Rolled several rounds on: it held `before` the whole time.
    r.roll(9, 30);
    assert_eq!((r.mark, r.min_prev, r.min_cur, r.paid), (9, 30, 30, 4));
    // A roll backwards (an older round) changes nothing.
    let same = r;
    r.roll(3, 1);
    assert_eq!(r, same);
}

#[test]
fn round_data_is_bytes_32_to_56_and_leaves_the_rest() {
    let mut data = [0xabu8; HOOK_DATA_LEN];
    let r = RoundData {
        mark: 0x0102_0304,
        paid: 7,
        min_cur: 9,
        min_prev: u64::MAX,
    };
    r.write(&mut data);
    assert_eq!(data[..32], [0xab; 32]);
    assert_eq!(data[56..], [0xab; 8]);
    assert_eq!(data[32..36], 0x0102_0304u32.to_le_bytes());
    assert_eq!(RoundData::read(&data), r);
    // The reward fields' writer (`claim`) keeps them.
    let h = HolderData {
        snapshot: 1,
        owed: 2,
        early_locked: 3,
    };
    assert_eq!(RoundData::read(&h.with_rewards_of(&data)), r);
    // Every other kit's holding: bytes 32..64 zero, read as no record.
    assert_eq!(RoundData::read(&h.to_bytes()), RoundData::default());
}
