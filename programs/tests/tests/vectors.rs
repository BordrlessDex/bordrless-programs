//! Vectors for the TypeScript mirror in `packages/shared` (`docs/hooks-v2.md` §8.2, §9
//! "TypeScript"): `programs/tests/vectors/launch-fees.json` holds the launch form's presets, and
//! inputs and exact outputs of the launch pool hook's fee math in the DEX's order (buys and sells,
//! both rates, the creator + holder guard, the eligible threshold, Bordrless's share of the fees,
//! failures), the burns, max wallet (the cap, what a wallet can still receive, the largest first
//! buy within it), the early-buyer lock time and the rewards mirror of §4.13 over a range of kit
//! states.
//!
//! The file is rendered from the Rust reference: `bordrless_core` (what the programs run), the
//! tests' `quote_launch_swap` / `max_buy_within` / `dev_buy_max` (which the LiteSVM suites hold
//! the on-chain swaps to) and `bordrless_kit::mirror` (which the kit suites hold the claims to).
//! When the file differs from what the reference computes, this test rewrites it and fails, so a
//! stale file never passes.

use std::path::PathBuf;

use anchor_lang::prelude::Pubkey;
use bordrless_core::{
    curve_params, launch_after_swap, launch_before_swap, max_wallet_cap, policy, trade_burn,
    LaunchFeeRates, Reserves, BPS,
};
use bordrless_kit::constants::{MIN_SHARE_LAMPORTS, SCALE, SHARE_STREAM_SECS};
use bordrless_kit::math::settle;
use bordrless_kit::state::{HolderData, KitInitArgs};
use bordrless_kit::{mirror, KitConfig};
use bordrless_launch::constants::ceilings;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::fixture::policy_rule_bounds;
use bordrless_program_tests::launch::{
    dev_buy_max, max_buy_within, presets, quote_launch_swap, rules, LaunchSwap, DAY, SOL, VQ,
};

// ------------------------------------------------------------------------------------------ JSON

/// A JSON value, rendered deterministically. Integers that may exceed 2^53 are strings.
#[derive(Clone, Debug)]
enum J {
    Null,
    Bool(bool),
    Num(i128),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(&'static str, J)>),
}

fn big(n: impl Into<u128>) -> J {
    J::Str(n.into().to_string())
}

fn signed(n: i128) -> J {
    J::Str(n.to_string())
}

fn num(n: impl Into<i128>) -> J {
    J::Num(n.into())
}

fn s(text: &str) -> J {
    J::Str(text.to_string())
}

fn opt(v: Option<J>) -> J {
    v.unwrap_or(J::Null)
}

fn inline(j: &J, out: &mut String) {
    match j {
        J::Null => out.push_str("null"),
        J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        J::Num(n) => out.push_str(&n.to_string()),
        J::Str(text) => {
            out.push('"');
            for c in text.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    c => out.push(c),
                }
            }
            out.push('"');
        }
        J::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                inline(item, out);
            }
            out.push(']');
        }
        J::Obj(fields) => {
            out.push('{');
            for (i, (key, value)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                inline(&J::Str((*key).to_string()), out);
                out.push_str(": ");
                inline(value, out);
            }
            out.push('}');
        }
    }
}

/// The file: the top-level fields one per line, each section's cases one per line.
fn render(fields: &[(&'static str, J)]) -> String {
    let mut out = String::from("{\n");
    for (i, (key, value)) in fields.iter().enumerate() {
        out.push_str("  ");
        inline(&J::Str((*key).to_string()), &mut out);
        out.push_str(": ");
        match value {
            J::Arr(items) if !items.is_empty() => {
                out.push_str("[\n");
                for (k, item) in items.iter().enumerate() {
                    out.push_str("    ");
                    inline(item, &mut out);
                    if k + 1 < items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str("  ]");
            }
            other => inline(other, &mut out),
        }
        if i + 1 < fields.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("}\n");
    out
}

// ------------------------------------------------------------------------------------ the inputs

/// The opening reserves of a policy launch.
const OPEN: Reserves = Reserves {
    base_reserve: 750_000_000_000_000,
    quote_reserve: 0,
    virtual_base: 375_000_000_000_000,
    virtual_quote: 28_125_000_000,
};

const MIN_ELIGIBLE: u64 = policy::TOKEN_SUPPLY / 1_000;

fn rates(
    creator: u16,
    holder_buy: u16,
    holder_sell: u16,
    burn_buy: u16,
    burn_sell: u16,
) -> LaunchFeeRates {
    LaunchFeeRates {
        creator_fee_bps: creator,
        holder_fee_buy_bps: holder_buy,
        holder_fee_sell_bps: holder_sell,
        burn_buy_bps: burn_buy,
        burn_sell_bps: burn_sell,
    }
}

/// `LaunchFeeParams`: the rates and the kit's counts.
fn params(r: &LaunchFeeRates, eligible: u64, min_eligible: u64) -> J {
    J::Obj(vec![
        ("creatorFeeBps", num(r.creator_fee_bps)),
        ("holderFeeBuyBps", num(r.holder_fee_buy_bps)),
        ("holderFeeSellBps", num(r.holder_fee_sell_bps)),
        ("burnBuyBps", num(r.burn_buy_bps)),
        ("burnSellBps", num(r.burn_sell_bps)),
        ("eligible", big(eligible)),
        ("minEligible", big(min_eligible)),
    ])
}

fn reserves(r: &Reserves) -> J {
    J::Obj(vec![
        ("baseReserve", big(r.base_reserve)),
        ("quoteReserve", big(r.quote_reserve)),
        ("virtualBase", big(r.virtual_base)),
        ("virtualQuote", big(r.virtual_quote)),
    ])
}

fn side(buy: bool) -> J {
    s(if buy { "buy" } else { "sell" })
}

/// The named rate sets the cases run through.
fn rate_sets() -> Vec<(&'static str, LaunchFeeRates)> {
    vec![
        ("holders 1%", rates(50, 100, 100, 0, 0)),
        ("own rates per side", rates(50, 0, 200, 25, 100)),
        ("burn and holders", rates(50, 50, 50, 50, 50)),
        ("at the ceilings", rates(500, 500, 500, 500, 500)),
        ("creator only", rates(100, 0, 0, 0, 0)),
        ("nothing", rates(0, 0, 0, 0, 0)),
    ]
}

fn hook_fee_cases() -> Vec<J> {
    let amounts: [u64; 9] = [0, 1, 2, 3, 20, 199, 10_001, 123_456_789_012, u64::MAX];
    let mut cases = Vec::new();
    for (name, r) in rate_sets() {
        // Just below the threshold, and at it.
        let thresholds: &[(u64, u64)] = if r.holder_fee_buy_bps > 0 || r.holder_fee_sell_bps > 0 {
            &[
                (MIN_ELIGIBLE - 1, MIN_ELIGIBLE),
                (MIN_ELIGIBLE, MIN_ELIGIBLE),
            ]
        } else {
            &[(0, 0)]
        };
        for (callback, before) in [("beforeSwap", true), ("afterSwap", false)] {
            for buy in [true, false] {
                for &(eligible, min_eligible) in thresholds {
                    for amount in amounts {
                        let cut = if before {
                            launch_before_swap(buy, amount, &r, eligible, min_eligible)
                        } else {
                            launch_after_swap(buy, amount, &r, eligible, min_eligible)
                        };
                        // The guards: what is taken stays below the amount.
                        assert!(
                            cut.creator_fee + cut.holder_fee < amount
                                || cut.creator_fee + cut.holder_fee == 0
                        );
                        assert!(cut.burn < amount || cut.burn == 0);
                        cases.push(J::Obj(vec![
                            ("name", s(name)),
                            ("callback", s(callback)),
                            ("side", side(buy)),
                            ("amount", big(amount)),
                            ("params", params(&r, eligible, min_eligible)),
                            (
                                "expected",
                                J::Obj(vec![
                                    ("creatorFee", big(cut.creator_fee)),
                                    ("holderFee", big(cut.holder_fee)),
                                    ("burn", big(cut.burn)),
                                ]),
                            ),
                        ]));
                    }
                }
            }
        }
    }
    cases
}

fn burn_cases() -> Vec<J> {
    let mut cases = Vec::new();
    for amount in [0u64, 1, 5, 199, 200, 201, 10_001, 1_000_000_000, u64::MAX] {
        for bps in [0u16, 1, 25, 50, 100, 500, 9_999, 10_000, u16::MAX] {
            let burn = trade_burn(amount, bps);
            let floor = u128::from(amount) * u128::from(bps) / u128::from(BPS);
            assert_eq!(
                u128::from(burn),
                if floor < u128::from(amount) { floor } else { 0 }
            );
            cases.push(J::Obj(vec![
                ("amount", big(amount)),
                ("burnBps", num(bps)),
                ("expected", big(burn)),
            ]));
        }
    }
    cases
}

fn swap_json(q: &LaunchSwap) -> J {
    J::Obj(vec![
        ("creatorFee", big(q.creator_fee)),
        ("holderFee", big(q.holder_fee)),
        ("burn", big(q.burn)),
        ("holderFeeOn", J::Bool(q.holder_fee_on)),
        ("received", big(q.received)),
        ("lpFee", big(q.lp_fee)),
        ("protocolFee", big(q.protocol_fee)),
        ("netIn", signed(q.net_in)),
        ("amountOut", opt(q.amount_out.map(big))),
        ("delivered", opt(q.delivered.map(big))),
        ("failure", opt(q.failure.map(s))),
    ])
}

fn swap_cases() -> Vec<J> {
    // Pools: the opening of a launch, one a few SOL in, a graduated one (no virtual reserves), a
    // tiny one where rounding dominates.
    let traded = Reserves {
        base_reserve: OPEN.base_reserve - 100_000_000_000_000,
        quote_reserve: 4_200_000_000,
        ..OPEN
    };
    let graduated = Reserves {
        base_reserve: 120_000_000_000_000,
        quote_reserve: 56_250_000_000,
        virtual_base: 0,
        virtual_quote: 0,
    };
    let small = Reserves {
        base_reserve: 50_000,
        quote_reserve: 0,
        virtual_base: 50_000,
        virtual_quote: 10_000,
    };
    let thin = Reserves {
        base_reserve: OPEN.base_reserve - 10_000_000_000,
        quote_reserve: 1_000_000_000,
        ..OPEN
    };
    let pools = [
        ("open", OPEN),
        ("traded", traded),
        ("graduated", graduated),
        ("small", small),
        ("thin", thin),
    ];
    // Buys from dust (the guard, the fees taking everything) to more than the curve holds;
    // sells from dust (the curve giving nothing, the protocol fee taking it all) to more than the
    // quote reserve.
    let buys: [u64; 5] = [2, 3, 1_000_000_000, 30_000_000_000, 1_000_000_000_000];
    let sells: [u64; 5] = [
        1_000,
        40_121,
        1_000_000_000,
        200_000_000_000_000,
        1_000_000_000_000_000,
    ];
    let swap_rates = [
        ("holders 1%", rates(50, 100, 100, 0, 0)),
        ("own rates per side", rates(50, 0, 200, 25, 100)),
        ("at the ceilings", rates(500, 500, 500, 500, 500)),
        ("creator only", rates(100, 0, 0, 0, 0)),
        ("nothing", rates(0, 0, 0, 0, 0)),
    ];
    let mut cases = Vec::new();
    for (pool, r) in pools {
        // The policy's launch-pool fees everywhere (the LP fee and Bordrless's share of the
        // cuts); the sniper fee at the opening; a pool that shares nothing once graduated.
        let fee_sets: &[(u16, u16)] = match pool {
            "open" => &[
                (policy::LP_FEE_BPS, policy::LAUNCH_PROTOCOL_SHARE_BPS),
                (8_000, policy::LAUNCH_PROTOCOL_SHARE_BPS),
            ],
            "graduated" => &[
                (policy::LP_FEE_BPS, policy::LAUNCH_PROTOCOL_SHARE_BPS),
                (30, 0),
            ],
            _ => &[(policy::LP_FEE_BPS, policy::LAUNCH_PROTOCOL_SHARE_BPS)],
        };
        for (name, rates) in swap_rates {
            let rewards = rates.holder_fee_buy_bps > 0 || rates.holder_fee_sell_bps > 0;
            // Eligible: a crowd above the threshold; at the opening also nobody; on the traded
            // pool also holders just above it (a sell takes them below).
            let counts: &[(u64, u64)] = match (rewards, pool) {
                (false, _) => &[(0, 0)],
                (true, "open") => &[(50 * MIN_ELIGIBLE, MIN_ELIGIBLE), (0, MIN_ELIGIBLE)],
                (true, "traded") => &[
                    (50 * MIN_ELIGIBLE, MIN_ELIGIBLE),
                    (MIN_ELIGIBLE + 1_000_000, MIN_ELIGIBLE),
                ],
                (true, _) => &[(50 * MIN_ELIGIBLE, MIN_ELIGIBLE)],
            };
            for &(eligible, min_eligible) in counts {
                for &(lp, share) in fee_sets {
                    for (buy, amounts) in [(true, &buys), (false, &sells)] {
                        for &amount in amounts.iter() {
                            let q = quote_launch_swap(
                                &r,
                                buy,
                                amount,
                                lp,
                                share,
                                &rates,
                                eligible,
                                min_eligible,
                            );
                            if let (Some(out), Some(delivered)) = (q.amount_out, q.delivered) {
                                // What is delivered is what the curve gave less every cut.
                                let cuts = if buy {
                                    q.burn
                                } else {
                                    q.protocol_fee + q.creator_fee + q.holder_fee
                                };
                                assert_eq!(delivered + cuts, out);
                                let ins = if buy {
                                    q.creator_fee + q.holder_fee
                                } else {
                                    q.burn
                                };
                                assert_eq!(q.received + ins, amount);
                                // Bordrless's share: a quarter of the fees, rounded up, never
                                // more than them; nothing when the rules collect nothing.
                                let fees = q.creator_fee + q.holder_fee;
                                assert_eq!(
                                    q.protocol_fee,
                                    bordrless_core::protocol_share(fees, share).unwrap()
                                );
                                assert!(q.protocol_fee <= fees);
                                if share > 0 {
                                    assert_eq!(fees == 0, q.protocol_fee == 0);
                                }
                            }
                            cases.push(J::Obj(vec![
                                ("name", s(&format!("{pool}, {name}"))),
                                ("reserves", reserves(&r)),
                                ("side", side(buy)),
                                ("amountIn", big(amount)),
                                ("lpFeeBps", num(lp)),
                                ("protocolShareBps", num(share)),
                                ("params", params(&rates, eligible, min_eligible)),
                                ("expected", swap_json(&q)),
                            ]));
                        }
                    }
                }
            }
        }
    }
    cases
}

fn max_wallet_cases() -> Vec<J> {
    let mut cases = Vec::new();
    for supply in [
        policy::TOKEN_SUPPLY,
        999,
        1_000,
        9_999,
        ceilings::MAX_SUPPLY,
    ] {
        for bps in [0u16, 1, 100, 200, 500, 9_999] {
            let cap = max_wallet_cap(supply, bps);
            for balance in [0, cap.saturating_sub(1), cap, cap + 1, u64::from(u32::MAX)] {
                cases.push(J::Obj(vec![
                    ("supply", big(supply)),
                    ("maxWalletBps", num(bps)),
                    ("balance", big(balance)),
                    (
                        "expected",
                        J::Obj(vec![
                            ("cap", big(cap)),
                            ("left", big(cap.saturating_sub(balance))),
                        ]),
                    ),
                ]));
            }
        }
    }
    cases
}

/// `LaunchRulesInput` as the TypeScript side takes it (the creator lock in days).
fn rules_input(r: &LaunchRules) -> J {
    J::Obj(vec![
        ("holderFeeBuyBps", num(r.holder_fee_buy_bps)),
        ("holderFeeSellBps", num(r.holder_fee_sell_bps)),
        ("burnBuyBps", num(r.burn_buy_bps)),
        ("burnSellBps", num(r.burn_sell_bps)),
        ("maxWalletBps", num(r.max_wallet_bps)),
        ("creatorLockDays", num(r.creator_lock_secs / DAY)),
        ("earlyWindowSecs", num(r.early_window_secs)),
        ("earlyLockSecs", num(r.early_lock_secs)),
    ])
}

fn dev_buy_cases() -> Vec<J> {
    let mut cases = Vec::new();
    let launches = [
        (
            "rewards and cap",
            rules(100, 100, 0, 0, 200, 30 * DAY, 0, 0),
        ),
        ("max wallet 1%", rules(0, 0, 0, 0, 100, 0, 0, 0)),
        (
            "max wallet 5% with a buy burn",
            rules(100, 100, 50, 50, 500, 0, 0, 0),
        ),
        ("early lock", rules(50, 50, 0, 0, 200, 30 * DAY, 60, 3_600)),
        ("diamond hands", presets::diamond_hands()),
        ("no max wallet", rules(100, 100, 50, 50, 0, 0, 0, 0)),
    ];
    for vq in [
        VQ,
        policy::MIN_VIRTUAL_QUOTE,
        2 * SOL,
        policy::MAX_VIRTUAL_QUOTE,
    ] {
        let curve = curve_params(policy::TOKEN_SUPPLY, policy::CURVE_BPS, vq).unwrap();
        for (name, r) in &launches {
            for creator_fee in [0u16, 50, 200] {
                let max = dev_buy_max(
                    &curve,
                    policy::TOKEN_SUPPLY,
                    r,
                    creator_fee,
                    policy::LP_FEE_BPS,
                    policy::LAUNCH_PROTOCOL_SHARE_BPS,
                );
                if let Some(max) = max {
                    // At the boundary: the largest input within the cap, the next one above it.
                    let rates = rates(creator_fee, 0, 0, r.burn_buy_bps, r.burn_sell_bps);
                    let opening = Reserves {
                        base_reserve: curve.curve_tokens,
                        quote_reserve: 0,
                        virtual_base: curve.virtual_base,
                        virtual_quote: curve.virtual_quote,
                    };
                    let cap = max_wallet_cap(policy::TOKEN_SUPPLY, r.max_wallet_bps);
                    let at = |a| {
                        quote_launch_swap(
                            &opening,
                            true,
                            a,
                            policy::LP_FEE_BPS,
                            policy::LAUNCH_PROTOCOL_SHARE_BPS,
                            &rates,
                            0,
                            0,
                        )
                        .delivered
                    };
                    assert!(at(max).unwrap() <= cap && at(max + 1).unwrap() > cap);
                }
                cases.push(J::Obj(vec![
                    ("name", s(name)),
                    (
                        "curve",
                        J::Obj(vec![
                            ("curveTokens", big(curve.curve_tokens)),
                            ("reserveTokens", big(curve.reserve_tokens)),
                            ("virtualQuote", big(curve.virtual_quote)),
                            ("virtualBase", big(curve.virtual_base)),
                            ("graduationQuote", big(curve.graduation_quote)),
                        ]),
                    ),
                    ("supply", big(policy::TOKEN_SUPPLY)),
                    ("rules", rules_input(r)),
                    ("creatorFeeBps", num(creator_fee)),
                    ("lpFeeBps", num(policy::LP_FEE_BPS)),
                    ("protocolShareBps", num(policy::LAUNCH_PROTOCOL_SHARE_BPS)),
                    ("expected", opt(max.map(big))),
                ]));
            }
        }
    }
    cases
}

/// The launch form's presets (§7.1), from the core's list: what `RULE_PRESETS` is pinned to.
fn preset_cases() -> Vec<J> {
    bordrless_core::policy::presets::ALL
        .iter()
        .map(|p| {
            J::Obj(vec![
                ("name", s(p.name)),
                ("slug", s(p.slug)),
                ("creatorFeeBps", num(p.creator_fee_bps)),
                ("rules", rules_input(&presets::of(p))),
                (
                    "default",
                    J::Bool(*p == bordrless_core::policy::presets::DEFAULT),
                ),
            ])
        })
        .collect()
}

fn early_lock_cases() -> Vec<J> {
    let launched_at = 1_800_000_000i64;
    let mut cases = Vec::new();
    for (window, lock) in [(0u32, 0u32), (30, 900), (60, 3_600), (300, 86_400)] {
        let ends = (window > 0).then(|| launched_at + i64::from(window));
        let unlock = (window > 0).then(|| launched_at + i64::from(lock));
        for offset in [
            0i64,
            1,
            i64::from(window) - 1,
            i64::from(window),
            i64::from(window) + 1,
            i64::from(lock),
        ] {
            let now = launched_at + offset.max(0);
            // The kit locks what the pool sends before the window ends, until the unlock.
            let expected = match (ends, unlock) {
                (Some(end), Some(at)) if now < end && now < at => Some(at),
                _ => None,
            };
            cases.push(J::Obj(vec![
                ("launchedAt", num(launched_at)),
                ("earlyWindowSecs", num(window)),
                ("earlyLockSecs", num(lock)),
                (
                    "rules",
                    J::Obj(vec![
                        ("earlyWindowEndsAt", opt(ends.map(num))),
                        ("earlyUnlockAt", opt(unlock.map(num))),
                    ]),
                ),
                ("now", num(now)),
                ("expected", opt(expected.map(num))),
            ]));
        }
    }
    cases
}

// ------------------------------------------------------------------------------- rewards mirror

const T0: i64 = 1_800_000_000;

/// A kit with every module installed at `T0` on the policy supply.
fn kit_installed() -> KitConfig {
    let args = KitInitArgs {
        launch: Pubkey::new_from_array([1; 32]),
        pool: Pubkey::new_from_array([2; 32]),
        creator: Pubkey::new_from_array([3; 32]),
        reward_mint: Pubkey::new_from_array([4; 32]),
        modules: 15,
        max_wallet_bps: 200,
        creator_unlock_at: T0 + 30 * 86_400,
        early_window_end: T0 + 60,
        early_unlock_at: T0 + 3_600,
        kit_caller_bump: 255,
    };
    KitConfig::install(
        &args,
        Pubkey::new_from_array([5; 32]),
        policy::TOKEN_SUPPLY,
        254,
        Some(Pubkey::new_from_array([6; 32])),
        T0,
    )
    .unwrap()
}

/// One state for the mirror: the config, the vault, the time, whose holding and what it holds.
struct RewardState {
    name: &'static str,
    config: KitConfig,
    vault: u64,
    now: i64,
    owner: Pubkey,
    balance: u64,
    data: HolderData,
}

/// A small ledger run with the kit's own sync and settle, to reach states the program reaches.
struct Ledger {
    config: KitConfig,
    vault: u64,
    holders: Vec<(u64, HolderData)>,
}

impl Ledger {
    fn new() -> Self {
        Self {
            config: kit_installed(),
            vault: 0,
            holders: vec![(0, HolderData::default()); 3],
        }
    }

    /// The pool sends `amount` to holder `i` (a buy): sync, settle, count it in.
    fn buy(&mut self, i: usize, amount: u64, now: i64) {
        self.config.sync(self.vault, now).unwrap();
        let (balance, data) = &mut self.holders[i];
        settle(data, *balance, *balance + amount, self.config.acc_per_share).unwrap();
        *balance += amount;
        self.config.eligible += amount;
    }

    /// Holder `i` sends `amount` to holder `j`.
    fn transfer(&mut self, i: usize, j: usize, amount: u64, now: i64) {
        self.config.sync(self.vault, now).unwrap();
        let acc = self.config.acc_per_share;
        {
            let (balance, data) = &mut self.holders[i];
            settle(data, *balance, *balance - amount, acc).unwrap();
            *balance -= amount;
        }
        let (balance, data) = &mut self.holders[j];
        settle(data, *balance, *balance + amount, acc).unwrap();
        *balance += amount;
    }

    /// A share at `now`: synced, into the vault, then streamed at its own rate (the kit's own
    /// `add_share`).
    fn share(&mut self, amount: u64, now: i64) {
        self.config.sync(self.vault, now).unwrap();
        self.vault += amount;
        self.config.add_share(amount, now).unwrap();
    }

    /// Holder `i` sells `amount` into the pool: sync, settle, count it out.
    fn sell(&mut self, i: usize, amount: u64, now: i64) {
        self.config.sync(self.vault, now).unwrap();
        let acc = self.config.acc_per_share;
        let (balance, data) = &mut self.holders[i];
        settle(data, *balance, *balance - amount, acc).unwrap();
        *balance -= amount;
        self.config.eligible -= amount;
    }

    /// Holder `i` claims what it is owed (as much as the vault holds).
    fn claim(&mut self, i: usize, now: i64) -> u64 {
        self.config.sync(self.vault, now).unwrap();
        let acc = self.config.acc_per_share;
        let (balance, data) = &mut self.holders[i];
        settle(data, *balance, *balance, acc).unwrap();
        let pay = data.owed.min(self.vault);
        data.owed -= pay;
        self.vault -= pay;
        self.config.total_claimed += pay;
        pay
    }

    fn state(&self, name: &'static str, i: usize, now: i64) -> RewardState {
        RewardState {
            name,
            config: clone_config(&self.config),
            vault: self.vault,
            now,
            owner: Pubkey::new_from_array([100 + i as u8; 32]),
            balance: self.holders[i].0,
            data: self.holders[i].1,
        }
    }
}

fn reward_states() -> Vec<RewardState> {
    let mut states = Vec::new();
    let mut l = Ledger::new();
    let pool = l.config.pool;
    states.push(l.state("nothing yet", 0, T0));
    // A donation while nobody holds: held at the next sync.
    l.vault += 5_000_000;
    states.push(l.state("a donation while nobody is eligible", 0, T0 + 10));
    // A holder just below the threshold: still held.
    l.buy(0, MIN_ELIGIBLE - 1, T0 + 20);
    l.vault += 3_000_000;
    states.push(l.state("holders below the threshold", 0, T0 + 30));
    // Over the threshold: what was held and what arrives is divided at the next sync.
    l.buy(1, 2 * MIN_ELIGIBLE + 7, T0 + 40);
    states.push(l.state("held lamports divide at the next sync", 0, T0 + 50));
    states.push(l.state("held lamports divide at the next sync", 1, T0 + 50));
    l.vault += 1_000_001;
    l.transfer(1, 2, MIN_ELIGIBLE / 3, T0 + 60);
    states.push(l.state("after a transfer settled both sides", 1, T0 + 70));
    states.push(l.state("after a transfer settled both sides", 2, T0 + 70));
    // A share streams over the hour.
    l.share(3 * MIN_SHARE_LAMPORTS + 1, T0 + 100);
    states.push(l.state("a share, just made", 0, T0 + 100));
    states.push(l.state("a share, a tenth of the way", 0, T0 + 460));
    states.push(l.state("a share, half way, fees on top", 2, T0 + 1_900));
    l.vault += 123_457;
    l.claim(2, T0 + 2_000);
    states.push(l.state("after a partial stream and a claim", 2, T0 + 2_400));
    // A second share while the first streams waits for it (`streamNext`), then streams over the
    // hour after the first ends (the first ends at T0 + 3,700).
    l.share(MIN_SHARE_LAMPORTS, T0 + 2_500);
    states.push(l.state("a second share waits for the first to end", 1, T0 + 2_600));
    states.push(l.state(
        "the waiting share streams the hour after the first",
        1,
        T0 + 3_700 + 600,
    ));
    states.push(l.state(
        "every stream released",
        1,
        T0 + 3_700 + SHARE_STREAM_SECS + 1,
    ));
    l.buy(0, 5 * MIN_ELIGIBLE, T0 + 9_000);
    l.vault += 999_999_999;
    states.push(l.state("a large holder and a large fee", 0, T0 + 9_100));
    states.push(l.state("a large holder and a large fee", 2, T0 + 9_100));
    // The pool and the launch never earn.
    let mut excluded = l.state("the pool earns nothing", 0, T0 + 9_100);
    excluded.owner = pool;
    excluded.balance = 600_000_000_000_000;
    states.push(excluded);
    // A vault that holds less than is owed pays what it holds.
    let mut short = l.state("a vault short of what is owed", 0, T0 + 9_100);
    short.data.owed = short.vault + 10_000_000;
    states.push(short);
    // Hook data with an early-buyer lock: it does not change what is claimable.
    let mut locked = l.state("an early buyer with rewards", 1, T0 + 9_100);
    locked.data.early_locked = 12_345_678;
    states.push(locked);
    // A share and one waiting for it, then the large holders sell below the threshold: the stream
    // pauses (it releases nothing while nobody is eligible, its end and the waiting share's hour
    // moving on with the clock) and resumes at its rate once holders are eligible again; the
    // running share then ends at T0 + 23,000 and the waiting one streams the hour after.
    l.share(7 * MIN_SHARE_LAMPORTS, T0 + 10_000);
    l.share(5 * MIN_SHARE_LAMPORTS, T0 + 10_300);
    states.push(l.state(
        "a share and one waiting for it, before the holders leave",
        0,
        T0 + 10_600,
    ));
    let (all, most) = (l.holders[0].0, l.holders[1].0 - 1);
    l.sell(0, all, T0 + 10_600);
    l.sell(1, most, T0 + 10_600);
    assert!(l.config.eligible < l.config.min_eligible);
    states.push(l.state(
        "a share and one waiting for it, paused while nobody is eligible",
        2,
        T0 + 20_000,
    ));
    l.buy(1, 2 * MIN_ELIGIBLE, T0 + 20_000);
    states.push(l.state("the paused share resumes at its rate", 1, T0 + 20_600));
    states.push(l.state("the paused share resumes at its rate", 2, T0 + 20_600));
    assert_eq!(l.config.stream_end, T0 + 23_000);
    states.push(l.state(
        "the waiting share streams the hour after the resumed one",
        1,
        T0 + 24_200,
    ));
    // Two shares in one second stream over the same hour, each at its own rate.
    l.share(2 * MIN_SHARE_LAMPORTS, T0 + 30_000);
    l.share(MIN_SHARE_LAMPORTS, T0 + 30_000);
    assert_eq!(
        (l.config.stream_remaining, l.config.stream_next),
        (3 * MIN_SHARE_LAMPORTS, 0)
    );
    states.push(l.state(
        "two shares in one second stream over one hour",
        2,
        T0 + 30_900,
    ));
    states
}

fn reward_cases() -> Vec<J> {
    let mut cases = Vec::new();
    for st in reward_states() {
        let c = &st.config;
        let bytes = st.data.to_bytes();
        let excluded = c.is_excluded(&st.owner);
        let claimable =
            mirror::claimable(c, st.vault, &st.owner, st.balance, &bytes, st.now).unwrap();
        let synced = mirror::synced(c, st.vault, st.now).unwrap();
        // The mirror against the kit's own sync and settle.
        if !excluded {
            let mut after = clone_config(c);
            after.sync(st.vault, st.now).unwrap();
            let mut data = st.data;
            settle(&mut data, st.balance, st.balance, after.acc_per_share).unwrap();
            assert_eq!(data.owed, claimable, "{}", st.name);
            assert_eq!(after.total_distributed, synced.distributed, "{}", st.name);
            assert_eq!(
                (after.stream_remaining, after.stream_next),
                (synced.stream_remaining, synced.stream_next),
                "{}",
                st.name
            );
        }
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        cases.push(J::Obj(vec![
            ("name", s(st.name)),
            (
                "kit",
                J::Obj(vec![
                    ("eligible", big(c.eligible)),
                    ("minEligible", big(c.min_eligible)),
                    ("accPerShare", big(c.acc_per_share)),
                    ("rem", big(c.rem)),
                    ("held", big(c.held)),
                    ("seen", big(c.seen)),
                    ("streamRemaining", big(c.stream_remaining)),
                    ("streamLast", num(c.stream_last)),
                    ("streamEnd", num(c.stream_end)),
                    ("streamNext", big(c.stream_next)),
                    ("totalDistributed", big(c.total_distributed)),
                    ("totalClaimed", big(c.total_claimed)),
                    ("totalShared", big(c.total_shared)),
                ]),
            ),
            ("vaultAmount", big(st.vault)),
            ("now", num(st.now)),
            ("balance", big(st.balance)),
            (
                "hookData",
                J::Obj(vec![
                    ("hex", J::Str(hex)),
                    ("snapshot", big(st.data.snapshot)),
                    ("owed", big(st.data.owed)),
                    ("earlyLocked", big(st.data.early_locked)),
                ]),
            ),
            ("excluded", J::Bool(excluded)),
            (
                "expected",
                J::Obj(vec![
                    ("claimable", big(claimable)),
                    ("payable", big(claimable.min(st.vault))),
                    ("distributed", big(synced.distributed)),
                    ("claimed", big(c.total_claimed)),
                    ("shared", big(c.total_shared)),
                    ("streaming", big(synced.streaming())),
                ]),
            ),
        ]));
    }
    cases
}

fn clone_config(c: &KitConfig) -> KitConfig {
    let mut data = Vec::new();
    anchor_lang::AccountSerialize::try_serialize(c, &mut data).unwrap();
    anchor_lang::AccountDeserialize::try_deserialize(&mut &data[..]).unwrap()
}

// ----------------------------------------------------------------------------------- the file

fn bounds_json(b: &bordrless_launch::state::RuleBounds) -> J {
    J::Obj(vec![
        ("maxHolderFeeBps", num(b.max_holder_fee_bps)),
        ("maxBurnBps", num(b.max_burn_bps)),
        ("maxRulesFeeBps", num(b.max_rules_fee_bps)),
        ("minMaxWalletBps", num(b.min_max_wallet_bps)),
        ("maxMaxWalletBps", num(b.max_max_wallet_bps)),
        ("maxCreatorLockSecs", num(b.max_creator_lock_secs)),
        ("maxEarlyWindowSecs", num(b.max_early_window_secs)),
        ("maxEarlyLockSecs", num(b.max_early_lock_secs)),
    ])
}

fn vectors() -> Vec<(&'static str, J)> {
    let ceiling = bordrless_launch::state::RuleBounds {
        max_holder_fee_bps: ceilings::HOLDER_FEE_BPS,
        max_burn_bps: ceilings::BURN_BPS,
        max_rules_fee_bps: ceilings::RULES_FEE_BPS,
        min_max_wallet_bps: ceilings::MIN_MAX_WALLET_BPS,
        max_max_wallet_bps: ceilings::MAX_WALLET_BPS,
        max_creator_lock_secs: ceilings::CREATOR_LOCK_SECS,
        max_early_window_secs: ceilings::EARLY_WINDOW_SECS,
        max_early_lock_secs: ceilings::EARLY_LOCK_SECS,
    };
    vec![
        (
            "description",
            s("Inputs and exact outputs of the launch pool hook's fee math in the v2 DEX's order \
               (docs/hooks-v2.md §3.1, §5.4) under the share model (Bordrless takes \
               launchProtocolShareBps of what the rules collect, rounded up, in SOL: from a buy's \
               input before the curve, held back from a sell's delivery), the burns, max wallet, \
               the early-buyer lock and the rewards mirror (§4.13), rendered from the Rust \
               reference that the LiteSVM suites hold the programs to. Integers that can exceed \
               2^53 are decimal strings. Sections: presets (RULE_PRESETS), hookFees \
               (launchBeforeSwap / launchAfterSwap), burns (tradeBurn), swaps (quoteLaunchSwap; \
               on a failure the fields are those computed up to it), maxWallet (maxWalletCap / \
               maxWalletLeft), devBuyMax (devBuyMaxLamports), earlyLock (buyLockedUntil), rewards \
               (rewardsClaimable / rewardTotals)."),
        ),
        (
            "generatedBy",
            s("cargo test -p bordrless-program-tests --test vectors (rewrites this file and fails when it is stale)"),
        ),
        (
            "constants",
            J::Obj(vec![
                ("bps", big(BPS)),
                ("scale", big(SCALE)),
                ("tokenSupply", big(policy::TOKEN_SUPPLY)),
                ("minEligible", big(MIN_ELIGIBLE)),
                ("lpFeeBps", num(policy::LP_FEE_BPS)),
                ("protocolFeeBps", num(policy::PROTOCOL_FEE_BPS)),
                ("launchProtocolShareBps", num(policy::LAUNCH_PROTOCOL_SHARE_BPS)),
                ("minLaunchSupply", big(ceilings::MIN_SUPPLY)),
                ("maxLaunchSupply", big(ceilings::MAX_SUPPLY)),
                ("shareStreamSecs", num(SHARE_STREAM_SECS)),
                ("minShareLamports", big(MIN_SHARE_LAMPORTS)),
                ("ruleBounds", bounds_json(&policy_rule_bounds())),
                ("ruleCeilings", bounds_json(&ceiling)),
            ]),
        ),
        ("presets", J::Arr(preset_cases())),
        ("hookFees", J::Arr(hook_fee_cases())),
        ("burns", J::Arr(burn_cases())),
        ("swaps", J::Arr(swap_cases())),
        ("maxWallet", J::Arr(max_wallet_cases())),
        ("devBuyMax", J::Arr(dev_buy_cases())),
        ("earlyLock", J::Arr(early_lock_cases())),
        ("rewards", J::Arr(reward_cases())),
    ]
}

fn vectors_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("vectors")
        .join("launch-fees.json")
}

#[test]
fn the_max_buy_search_matches_a_brute_force() {
    // The same small pool as the TypeScript test: every input from 0 to 20,000.
    let small = Reserves {
        base_reserve: 50_000,
        quote_reserve: 0,
        virtual_base: 50_000,
        virtual_quote: 10_000,
    };
    let r = rates(200, 200, 0, 100, 0);
    let quotes: Vec<LaunchSwap> = (0..=20_000u64)
        .map(|a| quote_launch_swap(&small, true, a, 30, 2_500, &r, 1, 1))
        .collect();
    assert_eq!(quotes[20_000].failure, Some("insufficient_liquidity"));
    for allowance in [0u64, 1, 7, 9, 100, 2_500, 30_000, 49_999, 1_000_000_000] {
        let mut brute = 0u64;
        for (a, q) in quotes.iter().enumerate() {
            if q.failure.is_none() && q.delivered.unwrap() <= allowance {
                brute = a as u64;
            }
        }
        assert_eq!(
            max_buy_within(&small, 30, 2_500, &r, 1, 1, allowance),
            brute,
            "allowance {allowance}"
        );
    }
}

#[test]
fn the_launch_fee_vectors_are_current() {
    let json = render(&vectors());
    let path = vectors_path();
    let on_disk = std::fs::read_to_string(&path).ok();
    let counts: Vec<String> = vectors()
        .iter()
        .filter_map(|(k, v)| match v {
            J::Arr(items) => Some(format!("{k} {}", items.len())),
            _ => None,
        })
        .collect();
    println!(
        "{}: {} bytes; cases: {}",
        path.display(),
        json.len(),
        counts.join(", ")
    );
    if on_disk.as_deref() != Some(json.as_str()) {
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("vectors directory");
        std::fs::write(&path, &json).expect("write the vectors");
        panic!(
            "{} was stale (or missing) and has been rewritten from the Rust reference: review the \
             change, then run the tests again",
            path.display()
        );
    }
}
