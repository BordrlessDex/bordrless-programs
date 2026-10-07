//! Launches in tests (`docs/hooks-v2.md` §5).
//!
//! - Token rules and the §7.1 presets ([`rules`], [`presets`], rendered from the core's list).
//! - [`quote_launch_swap`]: a swap on a launch pool step by step, the pool hook's fees in the
//!   DEX's order (§3.1, §5.4) and Bordrless's share of them (the share model of launch pools),
//!   built from `bordrless_core` (the math the programs run). It is the reference the on-chain
//!   swaps are checked against and the source of the TypeScript vectors, so it mirrors
//!   `quoteLaunchSwap` in `packages/shared` field for field, failures included.
//!   [`max_buy_within`] and [`dev_buy_max`] mirror `maxBuyWithin` and `devBuyMaxLamports`.
//! - Launch-pool trades as a client builds them, filling a curve with wallets that each stay
//!   within max wallet, graduation, and [`LaunchMarket`], the [`Market`] the kit's money walk
//!   trades through on a real launch pool (the crossing buy graduates, as the site does it).

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_core::{
    fee_amount, launch_after_swap, launch_before_swap, max_wallet_cap, policy, protocol_share,
    sniper_lp_fee, CurveParams, LaunchFeeRates, Reserves,
};
use bordrless_launch::client as launch;
use bordrless_launch::state::{Launch, LaunchRules};
use bordrless_swap::events::{DeltaPaid, Swapped};
use bordrless_swap::state::Pool;
use bordrless_token::client as token;
use solana_keypair::Keypair;
use solana_signer::Signer;

use crate::env::Tx;
use crate::fixture::World;
use crate::kit::{KitToken, Market};

/// One SOL in lamports.
pub const SOL: u64 = 1_000_000_000;
/// The virtual quote of the policy's opening market cap (25 SOL).
pub const VQ: u64 = 28_125_000_000;
/// A day in seconds.
pub const DAY: u32 = 86_400;

/// Token rules.
#[allow(clippy::too_many_arguments)]
pub fn rules(
    holder_fee_buy_bps: u16,
    holder_fee_sell_bps: u16,
    burn_buy_bps: u16,
    burn_sell_bps: u16,
    max_wallet_bps: u16,
    creator_lock_secs: u32,
    early_window_secs: u32,
    early_lock_secs: u32,
) -> LaunchRules {
    LaunchRules {
        holder_fee_buy_bps,
        holder_fee_sell_bps,
        burn_buy_bps,
        burn_sell_bps,
        max_wallet_bps,
        creator_lock_secs,
        early_window_secs,
        early_lock_secs,
    }
}

/// The presets of the launch form (§7.1), from the core's list, and two rule sets the tests use
/// that are no preset (every rule at once; holder rewards with a cap).
pub mod presets {
    use super::{rules, DAY};
    use bordrless_core::policy::presets as core;
    use bordrless_launch::state::LaunchRules;

    /// The rules of a core preset.
    pub fn of(p: &core::Preset) -> LaunchRules {
        rules(
            p.holder_fee_buy_bps,
            p.holder_fee_sell_bps,
            p.burn_buy_bps,
            p.burn_sell_bps,
            p.max_wallet_bps,
            p.creator_lock_secs,
            p.early_window_secs,
            p.early_lock_secs,
        )
    }

    /// Plain: no rules (creator 1%).
    pub fn plain() -> LaunchRules {
        of(&core::PLAIN)
    }

    /// Diamond hands: early-buyer lock (first 5 min, until 24 h), max wallet 1%, creator wallet
    /// lock 90 days, holder rewards 1% both sides (creator 0.5%).
    pub fn diamond_hands() -> LaunchRules {
        of(&core::DIAMOND_HANDS)
    }

    /// Burn 0.5% both sides, holder rewards 0.5% (creator 0.5%).
    pub fn burn() -> LaunchRules {
        of(&core::BURN)
    }

    /// Holder rewards 2% on sells only, creator wallet lock 30 days (creator 0.5%).
    pub fn paid_to_hold() -> LaunchRules {
        of(&core::PAID_TO_HOLD)
    }

    /// A test rule set (no preset): holder rewards 1% both sides, max wallet 2%, creator wallet
    /// lock 30 days (creator 0.5%).
    pub fn rewards_and_cap() -> LaunchRules {
        rules(100, 100, 0, 0, 200, 30 * DAY, 0, 0)
    }

    /// Every rule: holder rewards 1% both sides, burn 0.5% both sides, max wallet 5%, creator
    /// wallet lock 30 days, early-buyer lock (first 60 s, until 1 h). With a 0.5% creator fee,
    /// 2% per side.
    pub fn every_rule() -> LaunchRules {
        rules(100, 100, 50, 50, 500, 30 * DAY, 60, 3_600)
    }
}

/// A swap on a launch pool, step by step (`quoteLaunchSwap`'s `LaunchSwap`). On a failure the
/// fields are what was computed up to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchSwap {
    /// Quote to the creator: from a buy's input, or a sell's output.
    pub creator_fee: u64,
    /// Quote to holders, likewise.
    pub holder_fee: u64,
    /// Tokens burned: from a buy's output, or a sell's input.
    pub burn: u64,
    /// The holder-fee threshold was met when the hook took this side's fees.
    pub holder_fee_on: bool,
    /// What reached the input vault.
    pub received: u64,
    /// The LP fee, in the input token.
    pub lp_fee: u64,
    /// Bordrless's share of the creator and holder fees (`LAUNCH_PROTOCOL_SHARE_BPS` of them,
    /// rounded up), always in the quote: on a buy from what reached the vault, before the curve;
    /// on a sell held back from the delivery.
    pub protocol_fee: u64,
    /// What went into the curve (negative when the fees take more than what arrived).
    pub net_in: i128,
    /// What the curve gave; `None` on a failure.
    pub amount_out: Option<u64>,
    /// What reaches the recipient; `None` on a failure.
    pub delivered: Option<u64>,
    /// `fees_exceed_input`, `no_output` or `insufficient_liquidity`; `None` when it lands.
    pub failure: Option<&'static str>,
}

impl LaunchSwap {
    /// What enters the input reserve: what reached the vault, less a buy's protocol fee.
    pub fn to_reserve_in(&self, buy: bool) -> u64 {
        if buy {
            self.received - self.protocol_fee
        } else {
            self.received
        }
    }
}

/// The constant-product output for `net_in` into the curve, rounded down, before the check
/// against the real reserve (`curveOutput`).
pub fn curve_output(r: &Reserves, buy: bool, net_in: u64) -> u128 {
    if net_in == 0 {
        return 0;
    }
    let (x, y) = if buy {
        (
            u128::from(r.quote_reserve) + u128::from(r.virtual_quote),
            u128::from(r.base_reserve) + u128::from(r.virtual_base),
        )
    } else {
        (
            u128::from(r.base_reserve) + u128::from(r.virtual_base),
            u128::from(r.quote_reserve) + u128::from(r.virtual_quote),
        )
    };
    u128::from(net_in) * y / (x + u128::from(net_in))
}

/// A swap of `amount_in` on a launch pool with `reserves` (§3.1, §5.4), under the share model: a
/// buy pays the creator and holder fees from its input, Bordrless's share of them and the LP fee
/// on what reached the vault, the curve, then the burn from the output; a sell pays the burn from
/// its input, the LP fee on what reached the vault, the curve, then the creator and holder fees
/// from the output and Bordrless's share of them held back from the delivery. The kit takes no
/// cut of its own, so the hooks' cuts are exactly those fees. `eligible` is the kit's count before
/// the trade; a sell's input leaves it before `after_swap` reads it (every seller is a holder).
#[allow(clippy::too_many_arguments)]
pub fn quote_launch_swap(
    r: &Reserves,
    buy: bool,
    amount_in: u64,
    lp_fee_bps: u16,
    protocol_share_bps: u16,
    rates: &LaunchFeeRates,
    eligible: u64,
    min_eligible: u64,
) -> LaunchSwap {
    let before = launch_before_swap(buy, amount_in, rates, eligible, min_eligible);
    let received = amount_in - before.creator_fee - before.holder_fee - before.burn;
    let eligible_at_fees = if buy {
        eligible
    } else {
        eligible.saturating_sub(amount_in)
    };
    let side_bps = if buy {
        rates.holder_fee_buy_bps
    } else {
        rates.holder_fee_sell_bps
    };
    let holder_fee_on = side_bps > 0 && eligible_at_fees >= min_eligible;
    let lp_fee = fee_amount(received, lp_fee_bps).expect("a fee below 100%");
    // A buy's cuts are the hook's quote fees; a sell's input side cuts nothing (the burn is not
    // a cut), so its share is taken from the output.
    let input_protocol_fee = if buy {
        protocol_share(before.creator_fee + before.holder_fee, protocol_share_bps)
            .expect("a share below 100%")
    } else {
        0
    };
    let net_in = i128::from(received) - i128::from(lp_fee) - i128::from(input_protocol_fee);
    let failed = |failure: &'static str, protocol_fee: u64| LaunchSwap {
        creator_fee: before.creator_fee,
        holder_fee: before.holder_fee,
        burn: before.burn,
        holder_fee_on,
        received,
        lp_fee,
        protocol_fee,
        net_in,
        amount_out: None,
        delivered: None,
        failure: Some(failure),
    };
    if received == 0 || net_in <= 0 {
        return failed("fees_exceed_input", input_protocol_fee);
    }
    let out = curve_output(r, buy, net_in as u64);
    if out == 0 {
        return failed("no_output", input_protocol_fee);
    }
    let real = if buy { r.base_reserve } else { r.quote_reserve };
    if out > u128::from(real) {
        return failed("insufficient_liquidity", input_protocol_fee);
    }
    let out = out as u64;
    if buy {
        let after = launch_after_swap(true, out, rates, eligible, min_eligible);
        return LaunchSwap {
            creator_fee: before.creator_fee,
            holder_fee: before.holder_fee,
            burn: after.burn,
            holder_fee_on,
            received,
            lp_fee,
            protocol_fee: input_protocol_fee,
            net_in,
            amount_out: Some(out),
            delivered: Some(out - after.burn),
            failure: None,
        };
    }
    let after = launch_after_swap(false, out, rates, eligible_at_fees, min_eligible);
    let protocol_fee = protocol_share(after.creator_fee + after.holder_fee, protocol_share_bps)
        .expect("a share below 100%");
    if after.creator_fee + after.holder_fee + protocol_fee >= out {
        // The fees were computed before the share took the rest: they are carried.
        return LaunchSwap {
            creator_fee: after.creator_fee,
            holder_fee: after.holder_fee,
            ..failed("no_output", protocol_fee)
        };
    }
    LaunchSwap {
        creator_fee: after.creator_fee,
        holder_fee: after.holder_fee,
        burn: before.burn,
        holder_fee_on,
        received,
        lp_fee,
        protocol_fee,
        net_in,
        amount_out: Some(out),
        delivered: Some(out - after.creator_fee - after.holder_fee - protocol_fee),
        failure: None,
    }
}

/// The largest buy the pool can fill whose delivery stays within `allowance`; 0 when no buy does
/// (`maxBuyWithin`: a doubling search, a bisection, then a look 64 units past the boundary, which
/// the rounding of the fees makes ragged by a few units).
#[allow(clippy::too_many_arguments)]
pub fn max_buy_within(
    r: &Reserves,
    lp_fee_bps: u16,
    protocol_share_bps: u16,
    rates: &LaunchFeeRates,
    eligible: u64,
    min_eligible: u64,
    allowance: u64,
) -> u64 {
    if allowance == 0 {
        return 0;
    }
    let quote = |a: u64| {
        quote_launch_swap(
            r,
            true,
            a,
            lp_fee_bps,
            protocol_share_bps,
            rates,
            eligible,
            min_eligible,
        )
    };
    let fits = |a: u64| {
        let q = quote(a);
        q.failure != Some("insufficient_liquidity") && q.delivered.is_none_or(|d| d <= allowance)
    };
    let (mut lo, mut hi) = (0u64, 1u64);
    while fits(hi) {
        lo = hi;
        if hi == u64::MAX {
            return u64::MAX;
        }
        hi = hi.saturating_mul(2);
    }
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let top = if lo < u64::MAX - 64 {
        lo + 64
    } else {
        u64::MAX
    };
    for a in lo + 1..=top {
        if fits(a) {
            lo = a;
        }
    }
    if quote(lo).delivered.is_none() {
        0
    } else {
        lo
    }
}

/// §8.2 `devBuyMaxLamports`: the largest first buy whose tokens after the buy burn stay within
/// max wallet at the opening reserves, with the fees of the creator's own first buy (the normal
/// LP fee, the creator fee and Bordrless's share of it; no holder fee, nobody being eligible).
/// `None` without max wallet.
pub fn dev_buy_max(
    curve: &CurveParams,
    supply: u64,
    rules: &LaunchRules,
    creator_fee_bps: u16,
    lp_fee_bps: u16,
    protocol_share_bps: u16,
) -> Option<u64> {
    if rules.max_wallet_bps == 0 {
        return None;
    }
    let opening = Reserves {
        base_reserve: curve.curve_tokens,
        quote_reserve: 0,
        virtual_base: curve.virtual_base,
        virtual_quote: curve.virtual_quote,
    };
    let rates = LaunchFeeRates {
        creator_fee_bps,
        holder_fee_buy_bps: 0,
        holder_fee_sell_bps: 0,
        burn_buy_bps: rules.burn_buy_bps,
        burn_sell_bps: rules.burn_sell_bps,
    };
    Some(max_buy_within(
        &opening,
        lp_fee_bps,
        protocol_share_bps,
        &rates,
        0,
        0,
        max_wallet_cap(supply, rules.max_wallet_bps),
    ))
}

/// The protocol lookup table of §6: fixed addresses no top-level instruction invokes (event and
/// hook authorities, config PDAs, the bridged-SOL mint and the SOL wrapper's accounts, the kit
/// program, the system and associated-token programs). A v0 message loads from it every one of
/// these it uses that does not sign and is not invoked at top level. The hook signers are the
/// token program's for the kit and the DEX's for the launch (one per hook program).
pub fn protocol_lookup_table(w: &World) -> Vec<Pubkey> {
    use bordrless_bridge::client as bridge;
    use bordrless_kit::client as kit;
    use bordrless_swap::client as swap;
    vec![
        token::event_authority(),
        token::hook_signer(&bordrless_kit::ID),
        swap::event_authority(),
        swap::hook_signer(&bordrless_launch::ID),
        swap::config_address(),
        bridge::event_authority(),
        bridge::config_address(),
        bridge::wrapper_address(&bordrless_bridge::constants::NATIVE_MINT),
        bridge::sol_vault_address(),
        launch::event_authority(),
        launch::hook_signer(),
        launch::config_address(),
        kit::event_authority(),
        kit::hook_authority(),
        bordrless_kit::ID,
        w.sol,
        crate::env::SYSTEM_PROGRAM_ID,
        crate::env::ASSOCIATED_TOKEN_ID,
    ]
}

/// The reserves of `pool`.
pub fn reserves_of(pool: &Pool) -> Reserves {
    Reserves {
        base_reserve: pool.base_reserve,
        quote_reserve: pool.quote_reserve,
        virtual_base: pool.virtual_base,
        virtual_quote: pool.virtual_quote,
    }
}

/// The `Swapped` event a launch-pool swap that lands emits, as `q` predicts it: a buy pays the
/// creator fee to the launch's quote holding and the holder fee to the holder vault from its
/// input and burns from its output; a sell burns from its input and pays both fees from its
/// output. The cuts the DEX measured are those fees (the kit takes none of its own).
pub fn expect_swapped(ev: &Swapped, q: &LaunchSwap, l: &Launch, buy: bool, lp_fee_bps: u16) {
    assert_eq!(q.failure, None, "the reference says the swap fails");
    let mut deltas = Vec::new();
    if q.creator_fee > 0 {
        deltas.push(DeltaPaid {
            holding: l.quote_holding,
            amount: q.creator_fee,
        });
    }
    if q.holder_fee > 0 {
        deltas.push(DeltaPaid {
            holding: l.holder_vault,
            amount: q.holder_fee,
        });
    }
    let fees = q.creator_fee + q.holder_fee;
    let (deltas_in, deltas_out, burn_in, burn_out, cuts_in, cuts_out) = if buy {
        (deltas, vec![], 0, q.burn, fees, 0)
    } else {
        (vec![], deltas, q.burn, 0, 0, fees)
    };
    assert_eq!(
        (
            ev.direction,
            ev.received_in,
            ev.lp_fee,
            ev.protocol_fee,
            ev.lp_fee_bps,
            ev.amount_out,
            ev.delivered_out
        ),
        (
            u8::from(buy),
            q.received,
            q.lp_fee,
            q.protocol_fee,
            lp_fee_bps,
            q.amount_out.unwrap(),
            q.delivered.unwrap()
        ),
        "the swap's amounts"
    );
    assert_eq!(
        (ev.deltas_in.clone(), ev.deltas_out.clone()),
        (deltas_in, deltas_out),
        "the hook's fees"
    );
    assert_eq!((ev.burn_in, ev.burn_out), (burn_in, burn_out), "the burns");
    assert_eq!((ev.cuts_in, ev.cuts_out), (cuts_in, cuts_out), "the cuts");
}

impl World {
    /// The pool of a launch, by its mint.
    pub fn launch_pool_key(&self, mint: &Pubkey) -> Pubkey {
        launch::pool_address(mint, &self.sol, policy::LP_FEE_BPS)
    }

    /// The kit's eligible supply and threshold for a launch (zeros without holder rewards).
    pub fn eligibility(&self, mint: &Pubkey) -> (u64, u64) {
        let l = self.launch(mint);
        if !l.rewards_on() {
            return (0, 0);
        }
        let c = self.env.kit_config(mint);
        (c.eligible, c.min_eligible)
    }

    /// The LP fee a swap of `trader` delivered to `recipient` pays now on the launch of `mint`:
    /// the normal fee for the creator's own first buy into its own wallet inside the window, the
    /// sniper schedule otherwise.
    pub fn launch_lp_fee(
        &self,
        mint: &Pubkey,
        trader: &Pubkey,
        recipient: &Pubkey,
        buy: bool,
    ) -> u16 {
        let l = self.launch(mint);
        let now = self.env.now;
        if buy
            && *trader == l.creator
            && *recipient == l.creator
            && !l.creator_bought
            && now < l.created_at + l.sniper_window_secs
        {
            return l.lp_fee_bps;
        }
        sniper_lp_fee(
            now,
            l.created_at,
            l.sniper_window_secs,
            l.sniper_start_bps,
            l.lp_fee_bps,
        )
    }

    /// The reference of a swap of `amount_in` by `trader` (delivered to itself) on the launch of
    /// `mint` now.
    pub fn launch_quote(
        &self,
        mint: &Pubkey,
        trader: &Pubkey,
        buy: bool,
        amount_in: u64,
    ) -> LaunchSwap {
        let l = self.launch(mint);
        let p = self.launch_pool(mint);
        let (eligible, min_eligible) = self.eligibility(mint);
        quote_launch_swap(
            &reserves_of(&p),
            buy,
            amount_in,
            self.launch_lp_fee(mint, trader, trader, buy),
            p.protocol_share_bps,
            &l.rules.fee_rates(l.creator_fee_bps),
            eligible,
            min_eligible,
        )
    }

    /// A swap on the launch of `mint` by `trader`, delivered to `recipient` (whose holding must
    /// exist).
    pub fn launch_swap_to_ix(
        &self,
        trader: &Pubkey,
        recipient: &Pubkey,
        mint: &Pubkey,
        direction: u8,
        amount_in: u64,
    ) -> Instruction {
        let slice = self.launch_base_slice(mint, trader, recipient, direction == 1);
        launch::swap_with_base_slice(
            &self.launch_keys(mint),
            *trader,
            *recipient,
            direction,
            amount_in,
            0,
            slice,
        )
    }

    /// The largest buy `buyer` can make now on the launch of `mint` without going over max
    /// wallet (or the pool's tokens), and what it would deliver.
    pub fn max_buy_for(&self, mint: &Pubkey, buyer: &Pubkey) -> u64 {
        let l = self.launch(mint);
        let p = self.launch_pool(mint);
        let (eligible, min_eligible) = self.eligibility(mint);
        let allowance = if l.modules & bordrless_kit::modules::MAX_WALLET != 0 {
            let c = self.env.kit_config(mint);
            assert_eq!(
                c.max_wallet_amount,
                max_wallet_cap(c.supply_at_init, l.rules.max_wallet_bps)
            );
            if c.graduated {
                u64::MAX
            } else {
                c.max_wallet_amount
                    .saturating_sub(self.env.holding(mint, buyer))
            }
        } else {
            u64::MAX
        };
        max_buy_within(
            &reserves_of(&p),
            self.launch_lp_fee(mint, buyer, buyer, true),
            p.protocol_share_bps,
            &l.rules.fee_rates(l.creator_fee_bps),
            eligible,
            min_eligible,
            allowance,
        )
    }

    /// Buys the curve of `mint` with fresh wallets, each within max wallet and holding bridged
    /// SOL for it, until the pool needs at most `leave` more quote to graduate. Answers the
    /// wallets.
    pub fn fill_curve(&mut self, mint: &Pubkey, leave: u64) -> Vec<Keypair> {
        let mut wallets = Vec::new();
        loop {
            let l = self.launch(mint);
            let p = self.launch_pool(mint);
            let missing = l.graduation_quote.saturating_sub(p.quote_reserve);
            if missing <= leave {
                return wallets;
            }
            assert!(wallets.len() < 200, "the curve does not fill");
            let wallet = self.env.funded(1_000 * SOL);
            let holder = wallet.pubkey();
            let mut amount = self.max_buy_for(mint, &holder);
            // Never more than what leaves `leave` to raise: the reserve gains what reached the
            // vault less the protocol fee.
            let target = missing - leave;
            if self
                .launch_quote(mint, &holder, true, amount)
                .to_reserve_in(true)
                > target
            {
                let (mut lo, mut hi) = (0u64, amount);
                while hi - lo > 1 {
                    let mid = lo + (hi - lo) / 2;
                    let q = self.launch_quote(mint, &holder, true, mid);
                    if q.failure.is_none() && q.to_reserve_in(true) <= target {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                amount = lo.max(1);
            }
            self.wrap_sol(&wallet, amount).ok();
            let ixs = [
                token::create_holding(holder, *mint, holder),
                self.launch_swap_ix(&holder, mint, 1, amount, 0),
            ];
            self.env.send_paid_by(&ixs, &wallet, &[]).ok();
            wallets.push(wallet);
        }
    }

    /// The buy by `buyer` (whose holdings exist, with bridged SOL) that raises the rest of the
    /// curve of `mint`, within max wallet: the smallest input after which the pool can graduate.
    pub fn crossing_buy_amount(&self, mint: &Pubkey, buyer: &Pubkey) -> u64 {
        let l = self.launch(mint);
        let p = self.launch_pool(mint);
        let missing = l.graduation_quote.saturating_sub(p.quote_reserve);
        let (mut lo, mut hi) = (0u64, 1u64);
        while self.launch_quote(mint, buyer, true, hi).to_reserve_in(true) < missing {
            lo = hi;
            hi *= 2;
        }
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if self
                .launch_quote(mint, buyer, true, mid)
                .to_reserve_in(true)
                < missing
            {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        hi
    }

    /// Fills the curve of `mint` and graduates it: a last buyer raises the rest and `graduate`
    /// follows in the same transaction. Answers the curve's buyers and the graduation's
    /// transaction.
    pub fn graduate_launch(&mut self, mint: &Pubkey) -> (Vec<Keypair>, Tx) {
        let mut wallets = self.fill_curve(mint, SOL / 10);
        let last = self.env.funded(1_000 * SOL);
        let amount = self.crossing_buy_amount(mint, &last.pubkey());
        self.wrap_sol(&last, amount).ok();
        let ixs = [
            token::create_holding(last.pubkey(), *mint, last.pubkey()),
            self.launch_swap_ix(&last.pubkey(), mint, 1, amount, 0),
            self.graduate_ix(&last.pubkey(), mint),
        ];
        let tx = self.env.send_paid_by(&ixs, &last, &[]);
        wallets.push(last);
        (wallets, tx)
    }
}

/// The money walk's market on a real launch pool: buys spend bridged SOL (`size` in lamports),
/// sells sell tokens, both through the launch pool's hook and the kit as a client builds them.
/// A trade the fees would eat, or that the pool cannot fill, is not made (the DEX's error codes
/// overlap the kit's, which the walk allows). The buy that raises the graduation threshold
/// carries `graduate`, as the site sends it.
#[derive(Default)]
pub struct LaunchMarket {
    /// The smallest buy, lamports.
    pub min_buy: u64,
    /// Buys that carried `graduate` and landed.
    pub graduations: usize,
    /// Swaps that landed, by side.
    pub buys: usize,
    /// See `buys`.
    pub sells: usize,
    /// Most compute of a buy that carried `graduate`.
    pub graduation_cu: u64,
}

impl Market for LaunchMarket {
    fn buy(&mut self, w: &mut World, k: &KitToken, buyer: &Keypair, size: u64) -> Option<Tx> {
        let who = buyer.pubkey();
        let have = w.env.holding(&w.sol, &who);
        let mut amount = size.max(self.min_buy).min(have);
        if amount < self.min_buy.max(1) {
            return None;
        }
        // Never more than the pool can fill.
        let mut q = w.launch_quote(&k.mint, &who, true, amount);
        while q.failure == Some("insufficient_liquidity") {
            amount /= 2;
            if amount < self.min_buy.max(1) {
                return None;
            }
            q = w.launch_quote(&k.mint, &who, true, amount);
        }
        if q.failure.is_some() {
            return None;
        }
        let l = w.launch(&k.mint);
        let p = w.launch_pool(&k.mint);
        let mut ixs = vec![w.launch_swap_ix(&who, &k.mint, 1, amount, 0)];
        let graduates = p.curve && p.quote_reserve + q.to_reserve_in(true) >= l.graduation_quote;
        if graduates {
            ixs.push(w.graduate_ix(&who, &k.mint));
        }
        let tx = w.env.send_paid_by(&ixs, buyer, &[]);
        if tx.result.is_ok() {
            self.buys += 1;
            if graduates {
                self.graduations += 1;
                self.graduation_cu = self.graduation_cu.max(tx.cu());
            }
        }
        Some(tx)
    }

    fn sell(&mut self, w: &mut World, k: &KitToken, seller: &Keypair, amount: u64) -> Option<Tx> {
        let q = w.launch_quote(&k.mint, &seller.pubkey(), false, amount);
        if q.failure.is_some() {
            return None;
        }
        let tx = w.sell(seller, &k.mint, amount);
        if tx.result.is_ok() {
            self.sells += 1;
        }
        Some(tx)
    }
}
