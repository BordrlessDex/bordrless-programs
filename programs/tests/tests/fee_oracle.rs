//! An independent oracle for a launch pool's fees (`docs/hooks-v2.md` §3.1 and §5.4), written
//! from the specification with plain integer arithmetic and none of `bordrless_core`'s functions.
//! The pool hook runs `bordrless_core::launch_before_swap` and `launch_after_swap`, the DEX takes
//! Bordrless's share of their cuts; the LiteSVM suites check every `Swapped` against the
//! harness's `quote_launch_swap`, which calls the same core functions, and the TypeScript vectors
//! are rendered from them. So a rounding or guard error in the core would pass all of those. Here
//! the core functions, `quote_launch_swap` (the source of the vectors) and real swaps on chain are
//! each held against this oracle instead.

use anchor_lang::prelude::Pubkey;
use bordrless_core::{
    creator_and_holder_fees, launch_after_swap, launch_before_swap, trade_burn, LaunchFeeRates,
    Reserves,
};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::kit::Rng;
use bordrless_program_tests::launch::{quote_launch_swap, rules, SOL, VQ};
use bordrless_swap::events::Swapped;
use bordrless_token::client as token;
use solana_signer::Signer;

/// `ceil(amount * bps / 10_000)`, in u128 (§5.4: fees round up; §3.1: so does the share).
fn fee_up(amount: u64, bps: u16) -> u128 {
    (u128::from(amount) * u128::from(bps)).div_ceil(10_000)
}

/// `floor(amount * bps / 10_000)` (§5.4: burns round down).
fn burn_down(amount: u64, bps: u16) -> u128 {
    u128::from(amount) * u128::from(bps) / 10_000
}

/// §5.4: the creator fee, and the holder fee when holders are eligible, each rounded up; neither
/// unless together they are below the amount.
fn oracle_fees(amount: u64, creator_bps: u16, holder_bps: u16, holders: bool) -> (u64, u64) {
    let creator = fee_up(amount, creator_bps);
    let holder = if holder_bps > 0 && holders {
        fee_up(amount, holder_bps)
    } else {
        0
    };
    if creator + holder < u128::from(amount) {
        (creator as u64, holder as u64)
    } else {
        (0, 0)
    }
}

/// §5.4: a burn, rounded down, none unless below the amount.
fn oracle_burn(amount: u64, bps: u16) -> u64 {
    let burn = burn_down(amount, bps);
    if burn < u128::from(amount) {
        burn as u64
    } else {
        0
    }
}

/// One swap on a launch pool by the oracle.
#[derive(Debug, PartialEq, Eq)]
struct Expected {
    creator_fee: u64,
    holder_fee: u64,
    burn: u64,
    received: u64,
    lp_fee: u64,
    protocol_fee: u64,
    out_gross: Option<u64>,
    delivered: Option<u64>,
    failure: Option<&'static str>,
}

/// A swap of `amount_in` on a pool with `r` (§3.1, §5.4) under the share model. A buy: creator
/// and holder fees from the input; the rest reaches the vault; the LP fee on it and Bordrless's
/// share of the two fees (`share_bps` of them, rounded up) off it; the curve; the burn from the
/// output. A sell: the burn from the input; the rest reaches the vault; the LP fee on it; the
/// curve; creator and holder fees from the output, with the holders counted once the seller's
/// tokens are out; Bordrless's share of the two fees held back from the delivery.
#[allow(clippy::too_many_arguments)]
fn oracle_swap(
    r: &Reserves,
    buy: bool,
    amount_in: u64,
    lp_bps: u16,
    share_bps: u16,
    rates: &LaunchFeeRates,
    eligible: u64,
    min_eligible: u64,
) -> Expected {
    let (base, quote) = (
        u128::from(r.base_reserve) + u128::from(r.virtual_base),
        u128::from(r.quote_reserve) + u128::from(r.virtual_quote),
    );
    let mut e = Expected {
        creator_fee: 0,
        holder_fee: 0,
        burn: 0,
        received: 0,
        lp_fee: 0,
        protocol_fee: 0,
        out_gross: None,
        delivered: None,
        failure: None,
    };
    if buy {
        let (c, h) = oracle_fees(
            amount_in,
            rates.creator_fee_bps,
            rates.holder_fee_buy_bps,
            eligible >= min_eligible,
        );
        e.creator_fee = c;
        e.holder_fee = h;
        e.received = amount_in - c - h;
        e.lp_fee = fee_up(e.received, lp_bps) as u64;
        e.protocol_fee = fee_up(c + h, share_bps) as u64;
        let net = i128::from(e.received) - i128::from(e.lp_fee) - i128::from(e.protocol_fee);
        if e.received == 0 || net <= 0 {
            e.failure = Some("fees_exceed_input");
            return e;
        }
        let out = net as u128 * base / (quote + net as u128);
        if out == 0 {
            e.failure = Some("no_output");
            return e;
        }
        if out > u128::from(r.base_reserve) {
            e.failure = Some("insufficient_liquidity");
            return e;
        }
        let out = out as u64;
        e.burn = oracle_burn(out, rates.burn_buy_bps);
        e.out_gross = Some(out);
        e.delivered = Some(out - e.burn);
        return e;
    }
    e.burn = oracle_burn(amount_in, rates.burn_sell_bps);
    e.received = amount_in - e.burn;
    e.lp_fee = fee_up(e.received, lp_bps) as u64;
    let net = i128::from(e.received) - i128::from(e.lp_fee);
    if e.received == 0 || net <= 0 {
        e.failure = Some("fees_exceed_input");
        return e;
    }
    let out = net as u128 * quote / (base + net as u128);
    if out == 0 {
        e.failure = Some("no_output");
        return e;
    }
    if out > u128::from(r.quote_reserve) {
        e.failure = Some("insufficient_liquidity");
        return e;
    }
    let out = out as u64;
    let holders = eligible.saturating_sub(amount_in) >= min_eligible;
    let (c, h) = oracle_fees(
        out,
        rates.creator_fee_bps,
        rates.holder_fee_sell_bps,
        holders,
    );
    e.creator_fee = c;
    e.holder_fee = h;
    e.protocol_fee = fee_up(c + h, share_bps) as u64;
    if u128::from(c) + u128::from(h) + u128::from(e.protocol_fee) >= u128::from(out) {
        e.failure = Some("no_output");
        return e;
    }
    e.out_gross = Some(out);
    e.delivered = Some(out - c - h - e.protocol_fee);
    e
}

const AMOUNTS: [u64; 22] = [
    0,
    1,
    2,
    3,
    33,
    99,
    100,
    101,
    199,
    200,
    9_999,
    10_000,
    10_001,
    333_333,
    1_000_000,
    123_456_789,
    1_000_000_000,
    1_000_000_000_000,
    u64::MAX / 3,
    u64::MAX / 2,
    u64::MAX - 1,
    u64::MAX,
];
const BPS: [u16; 9] = [0, 1, 25, 50, 100, 200, 500, 9_999, 10_000];

#[test]
fn the_cores_hook_math_is_the_specs() {
    let mut rng = Rng::new(0x5EED_F00D);
    let mut amounts: Vec<u64> = AMOUNTS.to_vec();
    for _ in 0..200 {
        amounts.push(rng.next_u64() >> rng.below(64));
    }
    let mut checked = 0u64;
    for &amount in &amounts {
        for &creator in &BPS {
            for &holder in &BPS {
                for (eligible, min) in [(0u64, 1u64), (9, 10), (10, 10), (11, 10)] {
                    let expected = oracle_fees(amount, creator, holder, eligible >= min);
                    assert_eq!(
                        creator_and_holder_fees(amount, creator, holder, eligible, min),
                        expected,
                        "fees of {amount} at {creator}/{holder} bps, eligible {eligible}/{min}"
                    );
                    checked += 1;
                }
            }
            assert_eq!(trade_burn(amount, creator), oracle_burn(amount, creator));
            // Bordrless's share of a cut: rounded up, never above it (at most the whole cut).
            for &share in &BPS {
                if share <= 10_000 {
                    let s = bordrless_core::protocol_share(amount, share).unwrap();
                    assert_eq!(u128::from(s), fee_up(amount, share));
                    assert!(s <= amount);
                }
            }
        }
        // The two callbacks, each side.
        let rates = LaunchFeeRates {
            creator_fee_bps: 50,
            holder_fee_buy_bps: 100,
            holder_fee_sell_bps: 200,
            burn_buy_bps: 25,
            burn_sell_bps: 100,
        };
        for (eligible, min) in [(0u64, 1u64), (10, 10)] {
            let buy_in = launch_before_swap(true, amount, &rates, eligible, min);
            let (c, h) = oracle_fees(amount, 50, 100, eligible >= min);
            assert_eq!(
                (buy_in.creator_fee, buy_in.holder_fee, buy_in.burn),
                (c, h, 0)
            );
            let buy_out = launch_after_swap(true, amount, &rates, eligible, min);
            assert_eq!(
                (buy_out.creator_fee, buy_out.holder_fee, buy_out.burn),
                (0, 0, oracle_burn(amount, 25))
            );
            let sell_in = launch_before_swap(false, amount, &rates, eligible, min);
            assert_eq!(
                (sell_in.creator_fee, sell_in.holder_fee, sell_in.burn),
                (0, 0, oracle_burn(amount, 100))
            );
            let sell_out = launch_after_swap(false, amount, &rates, eligible, min);
            let (c, h) = oracle_fees(amount, 50, 200, eligible >= min);
            assert_eq!(
                (sell_out.creator_fee, sell_out.holder_fee, sell_out.burn),
                (c, h, 0)
            );
        }
    }
    // The guard's edge, by hand: 1% + 1% of 100 is 1 + 1 < 100; of 2 it rounds to 1 + 1, not
    // below 2, so neither is taken; of 3, 1 + 1 < 3.
    assert_eq!(creator_and_holder_fees(100, 100, 100, 1, 1), (1, 1));
    assert_eq!(creator_and_holder_fees(2, 100, 100, 1, 1), (0, 0));
    assert_eq!(creator_and_holder_fees(3, 100, 100, 1, 1), (1, 1));
    // A burn of the whole amount is no burn.
    assert_eq!(trade_burn(7, 10_000), 0);
    assert_eq!(trade_burn(10_000, 1), 1);
    // The share, by hand: a quarter of 10,000,000 (a 1% creator fee on 1 SOL) is 2,500,000, a
    // quarter of 1 lamport is 1, a quarter of nothing is nothing.
    assert_eq!(
        bordrless_core::protocol_share(10_000_000, 2_500),
        Some(2_500_000)
    );
    assert_eq!(bordrless_core::protocol_share(1, 2_500), Some(1));
    assert_eq!(bordrless_core::protocol_share(0, 2_500), Some(0));
    println!("{checked} fee cases agree with the oracle");
}

#[test]
fn the_reference_behind_the_typescript_vectors_is_the_specs() {
    // `quote_launch_swap` renders the `swaps` vectors the TypeScript mirror is pinned to.
    let mut rng = Rng::new(0xA11CE);
    let mut cases = 0;
    for _ in 0..20_000 {
        let r = Reserves {
            base_reserve: rng.range(0, 1 << 52),
            quote_reserve: rng.range(0, 1 << 48),
            virtual_base: rng.range(0, 1 << 52),
            virtual_quote: rng.range(1, 1 << 46),
        };
        let buy = rng.below(2) == 1;
        let amount_in = match rng.below(4) {
            0 => rng.range(0, 1_000),
            1 => rng.range(0, 1 << 30),
            2 => rng.range(0, 1 << 44),
            _ => rng.next_u64() >> 8,
        };
        let pick = |rng: &mut Rng| [0u16, 1, 25, 50, 100, 200, 300][rng.below(7) as usize];
        let rates = LaunchFeeRates {
            creator_fee_bps: pick(&mut rng),
            holder_fee_buy_bps: pick(&mut rng),
            holder_fee_sell_bps: pick(&mut rng),
            burn_buy_bps: pick(&mut rng),
            burn_sell_bps: pick(&mut rng),
        };
        let lp = [30u16, 100, 3_000, 8_000][rng.below(4) as usize];
        let share = [0u16, 2_500, 5_000, 10_000][rng.below(4) as usize];
        let min = rng.range(1, 1 << 40);
        let eligible = rng.range(0, 1 << 42);
        let q = quote_launch_swap(&r, buy, amount_in, lp, share, &rates, eligible, min);
        let e = oracle_swap(&r, buy, amount_in, lp, share, &rates, eligible, min);
        let got = Expected {
            creator_fee: q.creator_fee,
            holder_fee: q.holder_fee,
            burn: q.burn,
            received: q.received,
            lp_fee: q.lp_fee,
            protocol_fee: q.protocol_fee,
            out_gross: q.amount_out,
            delivered: q.delivered,
            failure: q.failure,
        };
        // On a failure the oracle stops where the swap does; the reference carries what it
        // computed up to there, which is the same.
        assert_eq!(
            got, e,
            "{r:?} buy {buy} in {amount_in} {rates:?} lp {lp} share {share}"
        );
        cases += 1;
    }
    println!("{cases} swaps agree with the oracle");
}

#[test]
fn launch_swaps_on_chain_pay_what_the_oracle_says() {
    let mut w = World::new();
    // Distinct rates on each side: holders 1% / 1.5%, burn 0.25% / 1%, creator 0.5% (3% on
    // sells, the policy's ceiling).
    let creator = w.wallet_with_sol(5 * SOL);
    let (mint, tx) = w.create_launch_with(
        &creator,
        "ORC",
        50,
        VQ,
        rules(100, 150, 25, 100, 0, 0, 0, 0),
    );
    tx.ok();
    w.env.warp(bordrless_core::policy::SNIPER_WINDOW_SECS);
    let l = w.launch(&mint);
    let rates = l.rules.fee_rates(l.creator_fee_bps);
    let traders: Vec<_> = (0..4).map(|_| w.wallet_with_sol(30 * SOL)).collect();
    for t in &traders {
        w.holdings(t, mint, &[t.pubkey()]);
    }
    let mut rng = Rng::new(77);
    let mut swaps = 0;
    for step in 0..40 {
        let t = &traders[rng.below(traders.len() as u64) as usize];
        let held = w.env.holding(&mint, &t.pubkey());
        let buy = held == 0 || rng.below(3) != 0;
        let amount = if buy {
            rng.range(1_000, 3 * SOL)
        } else {
            rng.range(1, held)
        };
        let pool = w.launch_pool(&mint);
        assert!(pool.shares_cuts());
        let r = Reserves {
            base_reserve: pool.base_reserve,
            quote_reserve: pool.quote_reserve,
            virtual_base: pool.virtual_base,
            virtual_quote: pool.virtual_quote,
        };
        let kit = w.env.kit_config(&mint);
        let e = oracle_swap(
            &r,
            buy,
            amount,
            pool.lp_fee_bps,
            pool.protocol_share_bps,
            &rates,
            kit.eligible,
            kit.min_eligible,
        );
        let ix = w.launch_swap_ix(&t.pubkey(), &mint, u8::from(buy), amount, 0);
        let tx = w.env.send_paid_by(&[ix], t, &[]);
        if e.failure.is_some() {
            tx.expect_fail();
            continue;
        }
        tx.ok();
        let s: Swapped = tx.event();
        let quote_holding = token::holding_address(&w.sol, &l.launch_key());
        let paid = |deltas: &[bordrless_swap::events::DeltaPaid], to: &Pubkey| {
            deltas
                .iter()
                .filter(|d| d.holding == *to)
                .map(|d| d.amount)
                .sum::<u64>()
        };
        let (fees_paid, burn) = if buy {
            (&s.deltas_in, s.burn_out)
        } else {
            (&s.deltas_out, s.burn_in)
        };
        assert_eq!(
            (
                paid(fees_paid, &quote_holding),
                paid(fees_paid, &l.holder_vault),
                burn,
                s.received_in,
                s.lp_fee,
                s.protocol_fee,
                Some(s.amount_out),
                Some(s.delivered_out),
            ),
            (
                e.creator_fee,
                e.holder_fee,
                e.burn,
                e.received,
                e.lp_fee,
                e.protocol_fee,
                e.out_gross,
                e.delivered,
            ),
            "step {step}: buy {buy} of {amount}"
        );
        // The cuts the DEX measured are the two fees, and the pool's books balance.
        let fees = e.creator_fee + e.holder_fee;
        assert_eq!(
            (s.cuts_in, s.cuts_out),
            if buy { (fees, 0) } else { (0, fees) }
        );
        let p = w.launch_pool(&mint);
        assert_eq!(
            w.env.holding(&w.sol, &l.pool),
            p.quote_reserve + p.protocol_fees_quote
        );
        assert_eq!(w.env.holding(&mint, &l.pool), p.base_reserve);
        swaps += 1;
    }
    assert!(swaps >= 30, "{swaps} swaps landed");
    println!("{swaps} swaps on chain agree with the oracle");
}

trait LaunchKey {
    fn launch_key(&self) -> Pubkey;
}

impl LaunchKey for bordrless_launch::state::Launch {
    fn launch_key(&self) -> Pubkey {
        bordrless_launch::client::launch_address(&self.mint)
    }
}
