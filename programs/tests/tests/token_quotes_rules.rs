//! Rules on coins quoted in tokens, phase 1 (`docs/token-quotes.md` §6.1): the burn on buys and
//! sells, max wallet, the creator wallet lock and the early-buyer lock, as on a SOL coin, with
//! `create_launch`'s `LaunchRules` and bounds. The burn is every leg's pool hook's; the other three
//! are the kit's, installed by `create_token_launch` through `init_legs`, which treats every leg's
//! pool as the launch's pool and lifts max wallet once the legs together have sold the supply's
//! `curve_bps` (counted by the pool hook on the `Legs`). Each rule alone and all of them, over one
//! leg and three, in SPL Token and Token-2022 quotes.
//!
//! The figures this suite measures (compute units, transaction sizes with and without the
//! protocol lookup table) are printed by `measures_rules_compute_and_sizes` (`--nocapture`).

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::{AnchorDeserialize, Discriminator, Space};
use bordrless_core::policy;
use bordrless_hook::HookAccountList;
use bordrless_kit::error::KitError;
use bordrless_kit::events::KitInstalled;
use bordrless_kit::mint_flags;
use bordrless_kit::state::{HolderData, KitConfig};
use bordrless_launch::client as launch;
use bordrless_launch::constants::TOKEN_LAUNCH_KIT_ACCOUNTS;
use bordrless_launch::error::LaunchError;
use bordrless_launch::events::{LegsMaxWalletLifted, TokenLaunchLocks, TokenLaunchRules};
use bordrless_launch::state::{Launch, LaunchRules, Legs};
use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::kit::refused_by_kit;
use bordrless_program_tests::launch::{
    max_buy_within, protocol_lookup_table, quote_launch_swap, reserves_of, LaunchSwap, DAY,
};
use bordrless_program_tests::quotes::{
    ext, pack_mint, xstock_extensions, Quote, QUOTE_FUNDS, TOKEN, TOKEN_2022,
};
use bordrless_swap::error::SwapError;
use bordrless_swap::events::Swapped;
use bordrless_swap::state::Pool;
use bordrless_token::client as token;
use bordrless_token::state::Mint;
use solana_keypair::Keypair;
use solana_signer::Signer;

const SUPPLY: u64 = policy::TOKEN_SUPPLY;
const FEE: u16 = 100;

fn lcode(e: LaunchError) -> u32 {
    u32::from(e)
}

/// A mint's extensions, given its key.
type Extensions = fn(Pubkey) -> Vec<ext::Ext>;

fn xstock(mint: Pubkey) -> Vec<ext::Ext> {
    xstock_extensions(mint, Pubkey::new_from_array([7; 32]))
}

fn none(_: Pubkey) -> Vec<ext::Ext> {
    vec![]
}

/// One quote of a setup: its program, decimals and extensions.
type Spec = (Pubkey, u8, Extensions);

/// The setups every rule runs over: one leg in an SPL Token mint, one in an xStocks-like Token-2022
/// mint, and three legs in both programs.
fn setups() -> Vec<(&'static str, Vec<Spec>)> {
    vec![
        ("1 leg, SPL Token", vec![(TOKEN, 6, none)]),
        ("1 leg, Token-2022", vec![(TOKEN_2022, 8, xstock)]),
        (
            "3 legs, SPL Token and Token-2022",
            vec![(TOKEN, 6, none), (TOKEN_2022, 8, xstock), (TOKEN, 9, none)],
        ),
    ]
}

/// The whole cap of a $5,000 coin in a token of `decimals` (a $1 token of 6, a $250 stock of 8,
/// a $200 token of 9).
fn cap_raw(decimals: u8) -> u64 {
    match decimals {
        6 => 5_000_000_000,
        8 => 2_000_000_000,
        _ => 25_000_000_000,
    }
}

/// `legShares`.
fn shares(n: usize) -> Vec<u64> {
    let even = SUPPLY / n as u64;
    (0..n)
        .map(|i| {
            if i == n - 1 {
                SUPPLY - even * (n as u64 - 1)
            } else {
                even
            }
        })
        .collect()
}

/// A coin's quotes: `(mint, program)` per leg.
type Quotes = Vec<(Pubkey, Pubkey)>;

/// Writes the quote mints of `specs`; answers them and the launch's quotes (with their virtual
/// quotes).
fn quote_mints(w: &mut World, specs: &[Spec]) -> (Quotes, Vec<Quote>) {
    let s = shares(specs.len());
    let issuer = w.env.deployer.pubkey();
    let mut quotes = Vec::new();
    let mut launch_quotes = Vec::new();
    for (i, (program, decimals, exts)) in specs.iter().enumerate() {
        let key = Pubkey::new_unique();
        w.env.put_mint(
            key,
            *program,
            pack_mint(
                Some(issuer),
                Some(issuer),
                *decimals,
                u64::MAX / 2,
                &exts(key),
            ),
        );
        let vq = (u128::from(cap_raw(*decimals)) * u128::from(s[i]) / u128::from(SUPPLY)) as u64;
        quotes.push((key, *program));
        launch_quotes.push((key, vq));
    }
    (quotes, launch_quotes)
}

/// A coin quoted in `specs` with `rules`, every leg open. Answers the coin, its creator (a wallet
/// with each quote) and its quotes.
fn launch_with(w: &mut World, specs: &[Spec], rules: LaunchRules) -> (Pubkey, Keypair, Quotes) {
    let (quotes, launch_quotes) = quote_mints(w, specs);
    let creator = trader(w, &quotes);
    let (mint, tx) = w.create_token_launch_with(&creator, FEE, &launch_quotes, rules);
    tx.ok();
    w.open_all_legs(&mint);
    (mint, creator, quotes)
}

/// A funded wallet holding plenty of every quote (its associated token accounts).
fn trader(w: &mut World, quotes: &Quotes) -> Keypair {
    let kp = w.env.funded(100_000_000_000);
    for (q, p) in quotes {
        w.env.token_account(&kp.pubkey(), q, p, QUOTE_FUNDS);
    }
    kp
}

fn leg_pool(w: &World, mint: &Pubkey, i: usize) -> Pool {
    w.env.read(&w.leg_keys(mint, i).pool())
}

fn supply(w: &World, mint: &Pubkey) -> u64 {
    w.env.read::<Mint>(mint).supply
}

/// A swap on leg `i` as `quote_launch_swap` predicts it under the pool's terms and the launch's
/// rules (no holder fee).
fn predict(w: &World, mint: &Pubkey, i: usize, buy: bool, amount: u64) -> LaunchSwap {
    let p = leg_pool(w, mint, i);
    let l = w.launch(mint);
    let q = quote_launch_swap(
        &reserves_of(&p),
        buy,
        amount,
        p.lp_fee_bps,
        p.protocol_share_bps,
        &l.rules.fee_rates(l.creator_fee_bps),
        0,
        0,
    );
    assert_eq!(q.failure, None, "the predicted swap lands");
    q
}

/// The largest buy on leg `i` that delivers at most `coins` (and at most nine tenths of what the leg
/// holds: a leg has no virtual base, so its last coins cost without bound).
fn buy_within(w: &World, mint: &Pubkey, i: usize, coins: u64) -> u64 {
    let p = leg_pool(w, mint, i);
    let l = w.launch(mint);
    let coins = coins.min(p.base_reserve / 10 * 9);
    max_buy_within(
        &reserves_of(&p),
        p.lp_fee_bps,
        p.protocol_share_bps,
        &l.rules.fee_rates(l.creator_fee_bps),
        0,
        0,
        coins,
    )
}

/// The legs' coins sold, from their pools: each leg's share less its pool's base reserve.
fn sold_by_pools(w: &World, mint: &Pubkey) -> i128 {
    let legs = w.legs(mint);
    legs.in_use()
        .iter()
        .enumerate()
        .map(|(i, leg)| i128::from(leg.share) - i128::from(leg_pool(w, mint, i).base_reserve))
        .sum()
}

/// What leg `i` can buy back: the coins it has sold (a leg has no virtual base, so its real quote
/// backs only those; a sell past them is `InsufficientLiquidity`).
fn sellable(w: &World, mint: &Pubkey, i: usize) -> u64 {
    w.legs(mint).legs[i].share - leg_pool(w, mint, i).base_reserve
}

/// Fresh wallets buy, each within the cap, round-robin over the legs, until the legs together are
/// within `cap / 2` of `lift_at` without reaching it (each buy leaves at least a quarter of a cap
/// to go). Checks on the way that nothing lifts and that the `Legs` counts what the pools sold.
/// Answers how many bought.
fn sell_toward_lift(w: &mut World, mint: &Pubkey, quotes: &Quotes, cap: u64) -> usize {
    let n = quotes.len();
    let lift_at = i128::from(w.legs(mint).lift_at());
    let mut k = 0;
    loop {
        let legs = w.legs(mint);
        assert!(!legs.lifted());
        assert_eq!(legs.total_sold(), sold_by_pools(w, mint));
        let left = lift_at - legs.total_sold();
        assert!(left > 0, "the threshold is not reached on the way");
        if left <= i128::from(cap / 2) {
            return k;
        }
        let coins = (left - i128::from(cap / 4)).min(i128::from(cap)) as u64;
        let b = trader(w, quotes);
        let i = k % n;
        w.leg_buy(&b, mint, i, buy_within(w, mint, i, coins)).ok();
        k += 1;
        assert!(k < 60, "the legs do not sell out");
    }
}

/// A holder's early-locked coins.
fn early_locked(w: &World, mint: &Pubkey, owner: &Pubkey) -> u64 {
    HolderData::read(&w.env.hook_data(mint, owner)).early_locked
}

fn b64(s: &str) -> Vec<u8> {
    let table = |c: u8| -> u32 {
        match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => 0,
        }
    };
    let bytes: Vec<u8> = s.bytes().filter(|c| *c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= table(*c) << (18 - 6 * i);
        }
        for i in 0..chunk.len() - 1 {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    out
}

/// `LegsMaxWalletLifted` events (logged by the pool hook with `emit!`).
fn lifts(tx: &Tx) -> Vec<LegsMaxWalletLifted> {
    tx.logs()
        .iter()
        .filter_map(|l| l.strip_prefix("Program data: "))
        .map(b64)
        .filter(|d| d.len() >= 8 && d[..8] == *LegsMaxWalletLifted::DISCRIMINATOR)
        .map(|d| LegsMaxWalletLifted::deserialize(&mut &d[8..]).expect("event decodes"))
        .collect()
}

/// Every rule but holder rewards: burn 0.5% both sides, max wallet 5%, creator wallet lock 30
/// days, early-buyer lock (first 60 s, until 1 h). With the 1% creator fee, 1.5% a side.
fn every_rule() -> LaunchRules {
    LaunchRules {
        holder_fee_buy_bps: 0,
        holder_fee_sell_bps: 0,
        burn_buy_bps: 50,
        burn_sell_bps: 50,
        max_wallet_bps: 500,
        creator_lock_secs: 30 * DAY,
        early_window_secs: 60,
        early_lock_secs: 3_600,
    }
}

/// What `create_token_launch` wrote for `rules`: the launch, the kit (when the rules install it),
/// the mint's hook, the `Legs`' max-wallet threshold, the events.
fn check_launch(w: &World, mint: &Pubkey, creator: &Pubkey, rules: LaunchRules) {
    let l: Launch = w.launch(mint);
    let legs = w.legs(mint);
    let m = rules.modules();
    assert!(l.is_token_quoted());
    assert_eq!((l.rules, l.modules), (rules, m));
    assert_eq!(l.holder_vault, Pubkey::default());
    assert_eq!(l.kit_config, launch::kit_config_address(mint));
    assert_eq!(
        l.creator_unlock_at,
        if rules.creator_lock_secs > 0 {
            l.created_at + i64::from(rules.creator_lock_secs)
        } else {
            0
        }
    );
    assert_eq!(
        (l.early_window_end, l.early_unlock_at),
        if rules.early_window_secs > 0 {
            (
                l.created_at + i64::from(rules.early_window_secs),
                l.created_at + i64::from(rules.early_lock_secs),
            )
        } else {
            (0, 0)
        }
    );
    assert_eq!(
        legs.lift_at(),
        if rules.max_wallet_bps > 0 {
            SUPPLY * policy::CURVE_BPS / 10_000
        } else {
            0
        }
    );
    let mt: Mint = w.env.read(mint);
    assert_eq!(
        (
            mt.hook_program,
            mt.hook_flags,
            mt.hook_authority,
            mt.mint_authority
        ),
        (
            (m != 0).then_some(bordrless_kit::ID),
            mint_flags(m),
            None,
            None
        )
    );
    if m == 0 {
        assert_eq!(l.kit_caller_bump, 0);
        assert!(w.env.account(&l.kit_config).is_none(), "no kit");
        return;
    }
    assert_eq!(l.kit_caller_bump, launch::kit_caller_address(mint).1);
    let c: KitConfig = w.env.read(&l.kit_config);
    assert!(c.is_token_quoted() && !c.is_native() && !c.rewards_on());
    assert_eq!(
        (c.mint, c.launch, c.pool, c.creator),
        (
            *mint,
            launch::launch_address(mint),
            legs.legs[0].pool,
            *creator
        )
    );
    assert_eq!(
        (c.reward_mint, c.reward_vault, c.modules),
        (Pubkey::default(), Pubkey::default(), m)
    );
    assert_eq!(
        (c.supply_at_init, c.min_eligible, c.max_wallet_bps),
        (SUPPLY, SUPPLY / 1_000, rules.max_wallet_bps)
    );
    assert_eq!(
        c.max_wallet_amount,
        SUPPLY * u64::from(rules.max_wallet_bps) / 10_000
    );
    assert_eq!(
        (c.creator_unlock_at, c.early_window_end, c.early_unlock_at),
        (l.creator_unlock_at, l.early_window_end, l.early_unlock_at)
    );
    let registry = w
        .env
        .account(&bordrless_kit::client::registry_address(mint))
        .unwrap();
    assert_eq!(
        HookAccountList::decode(&registry.data).unwrap(),
        bordrless_kit::instructions::legs_registry_list(l.kit_config)
    );
}

#[test]
fn create_token_launch_takes_the_rules_and_installs_the_kit_for_its_modules() {
    let rule_sets = [
        (
            "burn",
            LaunchRules {
                burn_buy_bps: 100,
                burn_sell_bps: 50,
                ..LaunchRules::NONE
            },
        ),
        (
            "max wallet",
            LaunchRules {
                max_wallet_bps: 500,
                ..LaunchRules::NONE
            },
        ),
        (
            "creator lock",
            LaunchRules {
                creator_lock_secs: 7 * DAY,
                ..LaunchRules::NONE
            },
        ),
        (
            "early-buyer lock",
            LaunchRules {
                early_window_secs: 60,
                early_lock_secs: 3_600,
                ..LaunchRules::NONE
            },
        ),
        ("every rule", every_rule()),
    ];
    for (name, specs) in setups() {
        for (rule, rules) in rule_sets {
            let mut w = World::new();
            let (quotes, launch_quotes) = quote_mints(&mut w, &specs);
            let creator = trader(&mut w, &quotes);
            let (mint, tx) = w.create_token_launch_with(&creator, FEE, &launch_quotes, rules);
            tx.ok();
            let ev: TokenLaunchRules = tx.event();
            assert_eq!(
                (ev.rules, ev.modules),
                (rules, rules.modules()),
                "{name}, {rule}"
            );
            assert_eq!(
                ev.kit_config,
                (rules.modules() != 0).then(|| launch::kit_config_address(&mint))
            );
            assert_eq!(ev.lift_at, w.legs(&mint).lift_at());
            let installed = tx.events::<KitInstalled>();
            assert_eq!(
                installed.len(),
                usize::from(rules.modules() != 0),
                "{name}, {rule}"
            );
            check_launch(&w, &mint, &creator.pubkey(), rules);
            // Every leg opens: its share moves into its pool through the kit (launch to pool:
            // excluded to excluded), whatever the cap.
            w.open_all_legs(&mint);
            let legs = w.legs(&mint);
            for (i, leg) in legs.in_use().iter().enumerate() {
                assert_eq!(w.env.holding(&mint, &leg.pool), leg.share);
                assert_eq!(leg_pool(&w, &mint, i).base_reserve, leg.share);
            }
            assert_eq!(w.env.holding(&mint, &launch::launch_address(&mint)), 0);
            if rules.modules() != 0 {
                assert_eq!(w.env.kit_config(&mint).eligible, 0, "pools are no holders");
            }
            assert_eq!((legs.total_sold(), legs.lifted()), (0, false));
        }
    }
}

#[test]
fn the_burn_takes_its_part_of_the_coin_on_every_leg() {
    let rules = LaunchRules {
        burn_buy_bps: 100,
        burn_sell_bps: 50,
        ..LaunchRules::NONE
    };
    for (name, specs) in setups() {
        let mut w = World::new();
        let (mint, _, quotes) = launch_with(&mut w, &specs, rules);
        // A burn alone runs in the pool hook: no kit, no token hook.
        assert_eq!(w.env.read::<Mint>(&mint).hook_program, None, "{name}");
        let b = trader(&mut w, &quotes);
        let mut burned = 0;
        for (i, _) in quotes.iter().enumerate() {
            let keys = w.leg_keys(&mint, i);
            let fees_before = w.env.token_amount(&keys.fee_account());
            let amount = leg_pool(&w, &mint, i).virtual_quote / 50;
            let q = predict(&w, &mint, i, true, amount);
            let (supply0, coins0) = (supply(&w, &mint), w.env.holding(&mint, &b.pubkey()));
            let ev: Swapped = w.leg_buy(&b, &mint, i, amount).event();
            assert!(q.burn > 0);
            assert_eq!(
                (ev.burn_in, ev.burn_out, ev.delivered_out),
                (0, q.burn, q.delivered.unwrap()),
                "{name}: a buy burns from its output, leg {i}"
            );
            assert_eq!(
                q.burn,
                q.amount_out.unwrap() / 100,
                "1% of the output, rounded down"
            );
            assert_eq!(supply(&w, &mint), supply0 - q.burn);
            assert_eq!(
                w.env.holding(&mint, &b.pubkey()),
                coins0 + q.delivered.unwrap()
            );
            assert_eq!(
                w.env.token_amount(&keys.fee_account()),
                fees_before + q.creator_fee
            );
            burned += q.burn;
            let half = q.delivered.unwrap() / 2;
            let s = predict(&w, &mint, i, false, half);
            let ev: Swapped = w.leg_sell(&b, &mint, i, half).event();
            assert!(s.burn > 0);
            assert_eq!(
                (ev.burn_in, ev.burn_out, ev.received_in, ev.delivered_out),
                (s.burn, 0, half - s.burn, s.delivered.unwrap()),
                "{name}: a sell burns from its input, leg {i}"
            );
            assert_eq!(s.burn, half / 200, "0.5% of the input, rounded down");
            burned += s.burn;
            assert_eq!(w.launch(&mint).burned_on_trades, burned);
            assert_eq!(supply(&w, &mint), SUPPLY - burned);
            w.env.assert_spl_invariant(&keys.pool());
        }
        // The burn needs the coin's mint writable: a client that passes it read-only is refused.
        let ix = launch::leg_swap(
            &w.leg_keys(&mint, 0),
            b.pubkey(),
            b.pubkey(),
            1,
            1_000_000,
            0,
        );
        w.env
            .send_paid_by(&[ix], &b, &[])
            .expect_code(u32::from(SwapError::MintNotWritable));
    }
}

#[test]
fn max_wallet_caps_a_wallet_across_legs_and_lifts_where_a_sol_coin_graduates() {
    let rules = LaunchRules {
        max_wallet_bps: 500,
        ..LaunchRules::NONE
    };
    for (name, specs) in setups() {
        let mut w = World::new();
        let (mint, _, quotes) = launch_with(&mut w, &specs, rules);
        let n = quotes.len();
        let cap = SUPPLY / 20;
        assert_eq!(w.env.kit_config(&mint).max_wallet_amount, cap);
        // Each leg's pool holds far more than the cap: pools are never capped.
        for leg in w.legs(&mint).in_use() {
            assert!(w.env.holding(&mint, &leg.pool) > cap);
        }
        // A wallet fills the cap across the legs, then no leg sells it one coin more.
        let a = trader(&mut w, &quotes);
        for i in 0..n {
            let left = cap - w.env.holding(&mint, &a.pubkey());
            let part = if i == n - 1 { left } else { cap / n as u64 };
            let amount = buy_within(&w, &mint, i, part);
            w.leg_buy(&a, &mint, i, amount).ok();
        }
        // Within what one raw unit of the quote buys (a few hundred thousand base units here).
        let held = w.env.holding(&mint, &a.pubkey());
        assert!(
            held <= cap && cap - held < cap / 1_000,
            "{name}: filled to the cap"
        );
        for i in 0..n {
            let tx = w.leg_buy(
                &a,
                &mint,
                i,
                buy_within(&w, &mint, i, cap - held + cap / 100),
            );
            assert!(
                refused_by_kit(&tx, KitError::MaxWalletExceeded),
                "{name}: leg {i}\n{}",
                tx.logs().join("\n")
            );
        }
        // A transfer that would put a wallet above the cap is refused, one that would not lands.
        let friend = trader(&mut w, &quotes);
        w.leg_buy(&friend, &mint, 0, buy_within(&w, &mint, 0, cap / 2))
            .ok();
        let f = w.env.holding(&mint, &friend.pubkey());
        let ix = w.kit_transfer_ix(mint, friend.pubkey(), &friend.pubkey(), &a.pubkey(), f);
        let tx = w.env.send_paid_by(&[ix], &friend, &[]);
        assert!(refused_by_kit(&tx, KitError::MaxWalletExceeded));
        let ix = w.kit_transfer_ix(mint, a.pubkey(), &a.pubkey(), &friend.pubkey(), held / 4);
        w.env.send_paid_by(&[ix], &a, &[]).ok();
        // Selling into any leg's pool is never capped (the pool already holds far more).
        for i in 0..n {
            let coins = w.env.holding(&mint, &a.pubkey()) / (n as u64 + 1);
            w.leg_sell(&a, &mint, i, coins).ok();
        }
        // The legs' pools and the launch are no holders: the eligible count is the wallets'.
        let wallets = [a.pubkey(), friend.pubkey()];
        let sum: u64 = wallets.iter().map(|o| w.env.holding(&mint, o)).sum();
        assert_eq!(w.env.kit_config(&mint).eligible, sum, "{name}");
        // The legs count what they sell; nothing lifts before they have sold `lift_at`.
        let lift_at = w.legs(&mint).lift_at();
        assert_eq!(lift_at, SUPPLY / 4 * 3);
        assert_eq!(w.legs(&mint).total_sold(), sold_by_pools(&w, &mint));
        let buyers = sell_toward_lift(&mut w, &mint, &quotes, cap);
        // Just below the threshold: a buy above the cap that would cross it is still capped.
        let i = buyers % n;
        let whale = trader(&mut w, &quotes);
        let big = buy_within(&w, &mint, i, 2 * cap);
        let tx = w.leg_buy(&whale, &mint, i, big);
        assert!(
            refused_by_kit(&tx, KitError::MaxWalletExceeded),
            "{name}: {big}\n{}",
            tx.logs().join("\n")
        );
        // A buy within the cap crosses it: counted, and capped like the buy that sells a SOL
        // coin's curve out (graduation follows it there).
        let crosser = trader(&mut w, &quotes);
        let tx = w.leg_buy(&crosser, &mint, i, buy_within(&w, &mint, i, cap));
        tx.ok();
        assert!(lifts(&tx).is_empty());
        let legs = w.legs(&mint);
        assert!(legs.total_sold() >= i128::from(lift_at) && !legs.lifted());
        assert!(!w.env.kit_config(&mint).graduated);
        // The next trade on any leg lifts it: the whale's buy above the cap lands (on the leg with
        // the most coins left).
        let j = (0..n)
            .max_by_key(|j| leg_pool(&w, &mint, *j).base_reserve)
            .unwrap();
        let big = buy_within(&w, &mint, j, 2 * cap);
        let tx = w.leg_buy(&whale, &mint, j, big);
        tx.ok();
        let lifted = lifts(&tx);
        assert_eq!(lifted.len(), 1, "{name}");
        assert_eq!(
            (
                lifted[0].launch,
                lifted[0].mint,
                lifted[0].pool,
                lifted[0].lift_at
            ),
            (
                launch::launch_address(&mint),
                mint,
                w.leg_keys(&mint, j).pool(),
                lift_at
            )
        );
        assert_eq!(i128::from(lifted[0].sold), legs.total_sold());
        assert!(w.legs(&mint).lifted());
        assert!(w.env.kit_config(&mint).graduated, "the kit remembers it");
        assert!(w.env.holding(&mint, &whale.pubkey()) > cap);
        // For good: transfers above the cap land too, and nothing is counted any more.
        let ix = w.kit_transfer_ix(
            mint,
            whale.pubkey(),
            &whale.pubkey(),
            &crosser.pubkey(),
            cap,
        );
        w.env.send_paid_by(&[ix], &whale, &[]).ok();
        let frozen = w.legs(&mint).total_sold();
        w.leg_sell(&whale, &mint, j, cap / 2).ok();
        assert_eq!(w.legs(&mint).total_sold(), frozen);
        assert!(lifts(&w.leg_buy(&whale, &mint, j, 1_000_000)).is_empty());
    }
}

#[test]
fn the_creator_wallet_lock_holds_the_creators_coins_on_every_leg() {
    let rules = LaunchRules {
        creator_lock_secs: 7 * DAY,
        ..LaunchRules::NONE
    };
    for (name, specs) in setups() {
        let mut w = World::new();
        let (mint, creator, quotes) = launch_with(&mut w, &specs, rules);
        let c = creator.pubkey();
        // The creator buys on every leg; it can't sell on any, nor send its coins anywhere.
        for i in 0..quotes.len() {
            let amount = leg_pool(&w, &mint, i).virtual_quote / 100;
            w.leg_buy(&creator, &mint, i, amount).ok();
        }
        let held = w.env.holding(&mint, &c);
        assert!(held > 0);
        for i in 0..quotes.len() {
            let tx = w.leg_sell(&creator, &mint, i, held / 10);
            assert!(
                refused_by_kit(&tx, KitError::CreatorLocked),
                "{name}: leg {i}"
            );
        }
        let other = trader(&mut w, &quotes);
        w.leg_buy(&other, &mint, 0, 1_000_000).ok();
        let ix = w.kit_transfer_ix(mint, c, &c, &other.pubkey(), 1);
        assert!(refused_by_kit(
            &w.env.send_paid_by(&[ix], &creator, &[]),
            KitError::CreatorLocked
        ));
        // Anyone else trades and sends freely; the creator still receives.
        let coins = w.env.holding(&mint, &other.pubkey());
        let ix = w.kit_transfer_ix(mint, other.pubkey(), &other.pubkey(), &c, coins / 2);
        w.env.send_paid_by(&[ix], &other, &[]).ok();
        w.leg_sell(&other, &mint, 0, coins / 4).ok();
        // At the unlock, the creator sells on every leg.
        let unlock = w.launch(&mint).creator_unlock_at;
        let now = w.env.now;
        w.env.warp(unlock - now);
        let held = w.env.holding(&mint, &c);
        for i in 0..quotes.len() {
            w.leg_sell(&creator, &mint, i, held / 10).ok();
        }
    }
}

#[test]
fn the_early_buyer_lock_counts_buys_out_of_every_leg() {
    let rules = LaunchRules {
        early_window_secs: 60,
        early_lock_secs: 3_600,
        ..LaunchRules::NONE
    };
    for (name, specs) in setups() {
        let mut w = World::new();
        let (mint, _, quotes) = launch_with(&mut w, &specs, rules);
        let n = quotes.len();
        let a = trader(&mut w, &quotes);
        let b = trader(&mut w, &quotes);
        w.env
            .send_paid_by(
                &[token::create_holding(b.pubkey(), mint, b.pubkey())],
                &b,
                &[],
            )
            .ok();
        // In the window, buys out of every leg are locked.
        let mut locked = 0;
        for i in 0..n {
            let before = w.env.holding(&mint, &a.pubkey());
            w.leg_buy(&a, &mint, i, leg_pool(&w, &mint, i).virtual_quote / 100)
                .ok();
            locked += w.env.holding(&mint, &a.pubkey()) - before;
            assert_eq!(
                early_locked(&w, &mint, &a.pubkey()),
                locked,
                "{name}: leg {i}"
            );
        }
        for i in 0..n {
            let tx = w.leg_sell(&a, &mint, i, 1);
            assert!(
                refused_by_kit(&tx, KitError::EarlyLocked),
                "{name}: leg {i}"
            );
        }
        let ix = w.kit_transfer_ix(mint, a.pubkey(), &a.pubkey(), &b.pubkey(), 1);
        assert!(refused_by_kit(
            &w.env.send_paid_by(&[ix], &a, &[]),
            KitError::EarlyLocked
        ));
        // After the window, buys out of any leg are free; the early coins stay locked.
        w.env.warp(61);
        let last = n - 1;
        let before = w.env.holding(&mint, &a.pubkey());
        w.leg_buy(
            &a,
            &mint,
            last,
            leg_pool(&w, &mint, last).virtual_quote / 100,
        )
        .ok();
        let free = w.env.holding(&mint, &a.pubkey()) - before;
        assert_eq!(early_locked(&w, &mint, &a.pubkey()), locked);
        w.leg_sell(&a, &mint, last, free).ok();
        let tx = w.leg_sell(&a, &mint, last, 1);
        assert!(refused_by_kit(&tx, KitError::EarlyLocked), "{name}");
        // Wallet to wallet in the window is no buy: nothing locked on the receiver.
        assert_eq!(early_locked(&w, &mint, &b.pubkey()), 0);
        // At the unlock everything is free, on every leg.
        w.env.warp(3_600 - 61);
        let held = w.env.holding(&mint, &a.pubkey());
        for i in 0..n {
            let coins = (held / (n as u64 + 1)).min(sellable(&w, &mint, i) / 2);
            w.leg_sell(&a, &mint, i, coins).ok();
        }
        assert_eq!(early_locked(&w, &mint, &a.pubkey()), 0);
    }
}

/// Review F1, the regression: the locks run from when trading opens (the last `open_leg`), not from
/// `create_token_launch`. `open_leg` is permissionless and separate, so a launcher could withhold
/// the last leg until a window counted from the launch had passed, then buy and sell at once while
/// the coin showed the lock. With its last leg opened 61 s after the launch (the finding's case: the
/// early window spent before anyone can trade) and 5,000 s after it (past even the unlock counted
/// from the launch): the first buys out of either leg are locked until an hour after trading opened,
/// the creator wallet lock ends a week after it, and the last `open_leg` says so
/// (`TokenLaunchLocks`).
#[test]
fn the_locks_run_from_when_trading_opens_not_from_the_launch() {
    let rules = LaunchRules {
        creator_lock_secs: 7 * DAY,
        early_window_secs: 60,
        early_lock_secs: 3_600,
        ..LaunchRules::NONE
    };
    let specs: Vec<Spec> = vec![(TOKEN, 6, none), (TOKEN_2022, 8, xstock)];
    for late in [61i64, 5_000] {
        let mut w = World::new();
        let (quotes, launch_quotes) = quote_mints(&mut w, &specs);
        let creator = trader(&mut w, &quotes);
        let (mint, tx) = w.create_token_launch_with(&creator, FEE, &launch_quotes, rules);
        tx.ok();
        let l = w.launch(&mint);
        let created = l.created_at;
        // Leg 0 opens at once (no event: trading is not open), leg 1 `late` seconds after.
        let tx = w.open_leg(&mint, 0);
        assert!(tx.events::<TokenLaunchLocks>().is_empty());
        w.env.warp(late);
        let tx = w.open_leg(&mint, 1);
        let opened = w.env.now;
        assert_eq!(opened - created, late);
        let ev: TokenLaunchLocks = tx.event();
        assert_eq!(
            (ev.launch, ev.mint, ev.trading_opened_at, ev.delay),
            (launch::launch_address(&mint), mint, opened, late)
        );
        assert_eq!(
            (
                ev.creator_unlock_at,
                ev.early_window_end,
                ev.early_unlock_at
            ),
            (opened + i64::from(7 * DAY), opened + 60, opened + 3_600),
            "the times in force"
        );
        // The accounts keep the times counted from the launch; the `Legs` gives the move.
        let c: KitConfig = w.env.kit_config(&mint);
        assert_eq!(
            (c.early_window_end, c.early_unlock_at),
            (created + 60, created + 3_600)
        );
        let legs = w.legs(&mint);
        assert_eq!(legs.trading_opened_at(), Some(opened));
        assert_eq!(legs.lock_delay(created, w.env.now), late);
        // The first buyer, in the first second of trading: locked, as on a SOL coin.
        let sniper = trader(&mut w, &quotes);
        let amount = leg_pool(&w, &mint, 0).virtual_quote / 50;
        w.leg_buy(&sniper, &mint, 0, amount).ok();
        let held = w.env.holding(&mint, &sniper.pubkey());
        assert_eq!(
            early_locked(&w, &mint, &sniper.pubkey()),
            held,
            "late {late}"
        );
        for i in 0..2 {
            let tx = w.leg_sell(&sniper, &mint, i, held / 4);
            assert!(
                refused_by_kit(&tx, KitError::EarlyLocked),
                "late {late}, leg {i}"
            );
        }
        // A buy out of the other leg inside the window is locked too.
        let amount = leg_pool(&w, &mint, 1).virtual_quote / 50;
        w.leg_buy(&sniper, &mint, 1, amount).ok();
        let held = w.env.holding(&mint, &sniper.pubkey());
        assert_eq!(early_locked(&w, &mint, &sniper.pubkey()), held);
        // The creator buys and stays locked.
        w.leg_buy(&creator, &mint, 0, 1_000_000).ok();
        assert!(refused_by_kit(
            &w.leg_sell(&creator, &mint, 0, 10),
            KitError::CreatorLocked
        ));
        // The window ends 60 s after trading opened: a later buy is free.
        w.env.warp(61);
        let b = trader(&mut w, &quotes);
        w.leg_buy(&b, &mint, 1, amount).ok();
        assert_eq!(early_locked(&w, &mint, &b.pubkey()), 0);
        let coins = w.env.holding(&mint, &b.pubkey());
        w.leg_sell(&b, &mint, 1, coins / 2).ok();
        // An hour after the launch is not an hour after trading opened.
        let now = w.env.now;
        if created + 3_600 > now {
            w.env.warp(created + 3_600 - now);
        }
        let tx = w.leg_sell(&sniper, &mint, 0, held / 10);
        assert!(refused_by_kit(&tx, KitError::EarlyLocked), "late {late}");
        let now = w.env.now;
        w.env.warp(opened + 3_600 - now);
        w.leg_sell(&sniper, &mint, 0, held / 10).ok();
        assert_eq!(early_locked(&w, &mint, &sniper.pubkey()), 0);
        // A week after the launch is not a week after trading opened.
        let now = w.env.now;
        w.env.warp(created + i64::from(7 * DAY) - now);
        assert!(refused_by_kit(
            &w.leg_sell(&creator, &mint, 0, 10),
            KitError::CreatorLocked
        ));
        let now = w.env.now;
        w.env.warp(opened + i64::from(7 * DAY) - now);
        let mine = w.env.holding(&mint, &creator.pubkey());
        w.leg_sell(&creator, &mint, 0, mine / 2).ok();
    }
    // A launch without either lock says nothing when its last leg opens.
    let mut w = World::new();
    let (quotes, launch_quotes) = quote_mints(&mut w, &specs);
    let creator = trader(&mut w, &quotes);
    let max_wallet = LaunchRules {
        max_wallet_bps: 500,
        burn_buy_bps: 50,
        ..LaunchRules::NONE
    };
    let (mint, tx) = w.create_token_launch_with(&creator, FEE, &launch_quotes, max_wallet);
    tx.ok();
    w.open_leg(&mint, 0).ok();
    w.env.warp(61);
    let tx = w.open_leg(&mint, 1);
    assert!(tx.events::<TokenLaunchLocks>().is_empty());
}

#[test]
fn every_rule_at_once_on_one_leg_and_three() {
    for (name, specs) in setups() {
        let mut w = World::new();
        let rules = every_rule();
        let (mint, creator, quotes) = launch_with(&mut w, &specs, rules);
        check_launch(&w, &mint, &creator.pubkey(), rules);
        let n = quotes.len();
        let cap = SUPPLY / 20;
        // Early buys on every leg: burned on the way out, locked, capped across legs.
        let a = trader(&mut w, &quotes);
        let mut burned = 0;
        for i in 0..n {
            let amount = buy_within(&w, &mint, i, cap / n as u64);
            let q = predict(&w, &mint, i, true, amount);
            let ev: Swapped = w.leg_buy(&a, &mint, i, amount).event();
            assert_eq!(ev.burn_out, q.burn, "{name}: leg {i}");
            burned += q.burn;
        }
        assert_eq!(
            early_locked(&w, &mint, &a.pubkey()),
            w.env.holding(&mint, &a.pubkey())
        );
        let tx = w.leg_buy(&a, &mint, 0, buy_within(&w, &mint, 0, cap));
        assert!(refused_by_kit(&tx, KitError::MaxWalletExceeded), "{name}");
        assert!(refused_by_kit(
            &w.leg_sell(&a, &mint, n - 1, 10),
            KitError::EarlyLocked
        ));
        // The creator buys and stays locked.
        let ev: Swapped = w.leg_buy(&creator, &mint, 0, 1_000_000).event();
        burned += ev.burn_out;
        assert!(refused_by_kit(
            &w.leg_sell(&creator, &mint, 0, 10),
            KitError::CreatorLocked
        ));
        // After the early unlock: sells burn from their input on every leg.
        w.env.warp(3_600);
        for i in 0..n {
            let coins = w.env.holding(&mint, &a.pubkey()) / 4;
            let s = predict(&w, &mint, i, false, coins);
            let ev: Swapped = w.leg_sell(&a, &mint, i, coins).event();
            assert_eq!(ev.burn_in, s.burn);
            burned += s.burn;
        }
        assert_eq!(w.launch(&mint).burned_on_trades, burned);
        assert_eq!(supply(&w, &mint), SUPPLY - burned);
        // The legs sell to the threshold; the next trade lifts max wallet; the creator is still
        // locked.
        let k = sell_toward_lift(&mut w, &mint, &quotes, cap);
        let b = trader(&mut w, &quotes);
        w.leg_buy(&b, &mint, k % n, buy_within(&w, &mint, k % n, cap))
            .ok();
        let legs = w.legs(&mint);
        assert!(legs.total_sold() >= i128::from(legs.lift_at()) && !legs.lifted());
        let j = (k + 1) % n;
        let tx = w.leg_buy(&b, &mint, j, buy_within(&w, &mint, j, cap));
        tx.ok();
        assert_eq!(lifts(&tx).len(), 1, "{name}");
        assert!(w.env.holding(&mint, &b.pubkey()) > cap);
        assert!(refused_by_kit(
            &w.leg_sell(&creator, &mint, 0, 10),
            KitError::CreatorLocked
        ));
        let unlock = w.launch(&mint).creator_unlock_at;
        let now = w.env.now;
        w.env.warp(unlock - now);
        let mine = w.env.holding(&mint, &creator.pubkey());
        w.leg_sell(&creator, &mint, 0, mine / 2).ok();
        for i in 0..n {
            w.env.assert_spl_invariant(&w.leg_keys(&mint, i).pool());
        }
    }
}

#[test]
fn holder_fees_launch_and_the_rules_bounds_are_a_sol_launchs() {
    let mut w = World::new();
    let (quotes, launch_quotes) = quote_mints(&mut w, &setups()[2].1);
    let creator = trader(&mut w, &quotes);
    let mut send = |rules: LaunchRules| {
        let (_, tx) = w.create_token_launch_with(&creator, FEE, &launch_quotes, rules);
        tx
    };
    let base = every_rule();
    for rules in [
        LaunchRules {
            holder_fee_buy_bps: 100,
            ..base
        },
        LaunchRules {
            holder_fee_sell_bps: 1,
            ..LaunchRules::NONE
        },
        LaunchRules {
            holder_fee_buy_bps: 50,
            holder_fee_sell_bps: 50,
            ..LaunchRules::NONE
        },
    ] {
        // Since §7.5 a holder fee launches (the kit's holder rewards in the legs' tokens: three
        // legs, rounds; `token_quotes_holder_rewards.rs`): 6082 is never raised.
        let tx = send(rules);
        tx.ok();
        let p = tx.event::<bordrless_launch::events::TokenLaunchPayouts>();
        assert_eq!(p.reward_kind, bordrless_kit::constants::REWARD_KIND_ROUNDS);
    }
    // The bounds `create_launch` holds rules to, checked first and in its order.
    let cases = [
        (
            LaunchRules {
                holder_fee_buy_bps: policy::MAX_HOLDER_FEE_BPS + 1,
                ..base
            },
            LaunchError::HolderFeeTooHigh,
        ),
        (
            LaunchRules {
                burn_sell_bps: policy::MAX_BURN_BPS + 1,
                ..base
            },
            LaunchError::BurnTooHigh,
        ),
        (
            LaunchRules {
                max_wallet_bps: policy::MIN_MAX_WALLET_BPS - 1,
                ..base
            },
            LaunchError::MaxWalletOutOfBounds,
        ),
        (
            LaunchRules {
                max_wallet_bps: policy::MAX_MAX_WALLET_BPS + 1,
                ..base
            },
            LaunchError::MaxWalletOutOfBounds,
        ),
        (
            LaunchRules {
                creator_lock_secs: policy::MAX_CREATOR_LOCK_SECS + 1,
                ..base
            },
            LaunchError::CreatorLockTooLong,
        ),
        (
            LaunchRules {
                early_window_secs: 0,
                ..base
            },
            LaunchError::InvalidEarlyLock,
        ),
        (
            LaunchRules {
                early_lock_secs: 60,
                ..base
            },
            LaunchError::InvalidEarlyLock,
        ),
        (
            LaunchRules {
                early_window_secs: policy::MAX_EARLY_WINDOW_SECS + 1,
                ..base
            },
            LaunchError::InvalidEarlyLock,
        ),
    ];
    for (rules, why) in cases {
        send(rules).expect_code(lcode(why));
    }
    // The creator fee and the burn together within `max_rules_fee_bps` (3%).
    let (_, tx) = w.create_token_launch_with(
        &creator,
        policy::MAX_CREATOR_FEE_BPS,
        &launch_quotes,
        LaunchRules {
            burn_buy_bps: 101,
            ..LaunchRules::NONE
        },
    );
    tx.expect_code(lcode(LaunchError::BurnTooHigh));
    let (_, tx) = w.create_token_launch_with(
        &creator,
        policy::MAX_CREATOR_FEE_BPS,
        &launch_quotes,
        LaunchRules {
            burn_buy_bps: 100,
            ..LaunchRules::NONE
        },
    );
    tx.ok();
    let mut args = bordrless_program_tests::fixture::policy_launch_config(
        w.admin().pubkey(),
        w.env.treasury.pubkey(),
        w.sol,
    );
    args.rule_bounds.max_rules_fee_bps = 250;
    let admin = w.admin();
    w.as_admin(launch::set_config(admin.pubkey(), args)).ok();
    let (_, tx) = w.create_token_launch_with(
        &creator,
        policy::MAX_CREATOR_FEE_BPS,
        &launch_quotes,
        LaunchRules {
            burn_buy_bps: 100,
            ..LaunchRules::NONE
        },
    );
    tx.expect_code(lcode(LaunchError::RulesFeeTooHigh));
}

#[test]
fn the_kits_accounts_are_checked_where_they_are_taken() {
    let mut w = World::new();
    let (quotes, launch_quotes) = quote_mints(&mut w, &setups()[2].1);
    let creator = trader(&mut w, &quotes);
    let rules = every_rule();
    // `create_token_launch`: the kit's six accounts after the quote mints, each at its address.
    let mint = Keypair::new();
    let full = w.token_launch_ix_with(
        &creator.pubkey(),
        &mint.pubkey(),
        FEE,
        &launch_quotes,
        rules,
    );
    let n = full.accounts.len();
    let mut ix = full.clone();
    ix.accounts.truncate(n - TOKEN_LAUNCH_KIT_ACCOUNTS);
    w.env
        .send_paid_by(&[ix], &creator, &[&mint])
        .expect_code(lcode(LaunchError::KitAccountsMissing));
    let other = Pubkey::new_unique();
    for at in [0usize, 1, 2, 4, 5] {
        let mut ix = full.clone();
        let k = n - TOKEN_LAUNCH_KIT_ACCOUNTS + at;
        ix.accounts[k].pubkey = if at == 2 {
            launch::kit_config_address(&other)
        } else {
            other
        };
        w.env
            .send_paid_by(&[ix], &creator, &[&mint])
            .expect_code(lcode(LaunchError::WrongKitAccount));
    }
    // Another mint's registry: the kit refuses it (its seeds).
    let mut ix = full.clone();
    ix.accounts[n - 3].pubkey = bordrless_kit::client::registry_address(&other);
    w.env.send_paid_by(&[ix], &creator, &[&mint]).expect_fail();
    // Rules without a kit module (a burn alone) take none: six more accounts are quote mints.
    let mut ix = w.token_launch_ix_with(
        &creator.pubkey(),
        &mint.pubkey(),
        FEE,
        &launch_quotes,
        LaunchRules {
            burn_buy_bps: 50,
            ..LaunchRules::NONE
        },
    );
    ix.accounts
        .extend(launch::token_launch_kit_accounts(&mint.pubkey()));
    w.env
        .send_paid_by(&[ix], &creator, &[&mint])
        .expect_code(lcode(LaunchError::InvalidLegCount));
    w.env.send_paid_by(&[full], &creator, &[&mint]).ok();
    let mint = mint.pubkey();
    // `open_leg`: the kit, its signer and the kit config, required with a kit.
    let payer = w.env.payer.pubkey();
    let ix = launch::open_leg(payer, w.env.treasury.pubkey(), &w.leg_keys(&mint, 0), 0);
    w.env
        .send(&[ix], &[])
        .expect_code(lcode(LaunchError::KitAccountsMissing));
    for at in [0usize, 1, 2] {
        let mut ix = w.open_leg_ix(&payer, &mint, 0);
        let k = ix.accounts.len() - 3 + at;
        ix.accounts[k].pubkey = launch::kit_config_address(&other);
        w.env
            .send(&[ix], &[])
            .expect_code(lcode(LaunchError::WrongKitAccount));
    }
    // A leg's deposit through the kit takes the `Legs`: the DEX's slice must carry it.
    w.open_all_legs(&mint);
    let b = trader(&mut w, &quotes);
    w.env
        .send_paid_by(
            &[token::create_holding(b.pubkey(), mint, b.pubkey())],
            &b,
            &[],
        )
        .ok();
    let keys = w.leg_keys(&mint, 1);
    let (base, quote) = (
        token::holding_address(&mint, &b.pubkey()),
        keys.quote_account(&b.pubkey()),
    );
    let buy = |slice: Vec<AccountMeta>| {
        launch::leg_swap_with_slice(&keys, b.pubkey(), base, quote, 1, 1_000_000, 0, slice, true)
    };
    // Without the `Legs` (the SOL kit's two extras alone).
    let tx = w.env.send_paid_by(
        &[buy(bordrless_kit::client::hook_slice(&mint, None))],
        &b,
        &[],
    );
    assert!(
        refused_by_kit(&tx, KitError::LegsMissing),
        "{}",
        tx.logs().join("\n")
    );
    // Another coin's `Legs`, or another account in its place.
    let (other_mint, _) = w.create_token_launch_with(&creator, FEE, &launch_quotes[..1], rules);
    for wrong in [
        launch::legs_address(&other_mint),
        launch::launch_address(&mint),
        launch::kit_config_address(&mint),
    ] {
        let mut slice = launch::leg_base_slice(&mint, rules.modules());
        slice[4].pubkey = wrong;
        let tx = w.env.send_paid_by(&[buy(slice)], &b, &[]);
        assert!(
            refused_by_kit(&tx, KitError::WrongLegs),
            "{}",
            tx.logs().join("\n")
        );
    }
    // As built: it lands.
    w.env
        .send_paid_by(
            &[buy(launch::leg_base_slice(&mint, rules.modules()))],
            &b,
            &[],
        )
        .ok();
}

#[test]
fn todays_call_without_rules_still_lands_and_trades_as_before() {
    for (_, specs) in setups() {
        let mut w = World::new();
        let (quotes, launch_quotes) = quote_mints(&mut w, &specs);
        let creator = trader(&mut w, &quotes);
        let mint = Keypair::new();
        let mut ix = w.token_launch_ix(&creator.pubkey(), &mint.pubkey(), FEE, &launch_quotes);
        // The data as clients built it before rules: no rules at the end.
        let n = ix.data.len();
        ix.data.truncate(n - LaunchRules::INIT_SPACE);
        let tx = w.env.send_paid_by(&[ix], &creator, &[&mint]);
        tx.ok();
        assert!(tx.events::<TokenLaunchRules>().is_empty());
        let mint = mint.pubkey();
        check_launch(&w, &mint, &creator.pubkey(), LaunchRules::NONE);
        // A part of the rules is refused.
        let other = Keypair::new();
        let mut ix = w.token_launch_ix(&creator.pubkey(), &other.pubkey(), FEE, &launch_quotes);
        let n = ix.data.len();
        ix.data.truncate(n - 1);
        w.env
            .send_paid_by(&[ix], &creator, &[&other])
            .expect_code(u32::from(
                anchor_lang::error::ErrorCode::InstructionDidNotDeserialize,
            ));
        // Its legs open and trade without a kit, the `Legs` counting nothing.
        w.open_all_legs(&mint);
        let b = trader(&mut w, &quotes);
        for i in 0..quotes.len() {
            let amount = leg_pool(&w, &mint, i).virtual_quote / 100;
            let q = predict(&w, &mint, i, true, amount);
            let ev: Swapped = w.leg_buy(&b, &mint, i, amount).event();
            assert_eq!((ev.burn_out, ev.delivered_out), (0, q.delivered.unwrap()));
            w.leg_sell(&b, &mint, i, q.delivered.unwrap() / 2).ok();
        }
        let legs = w.legs(&mint);
        assert_eq!(
            legs.reserved, [0; 64],
            "nothing counted on a launch without max wallet"
        );
        assert_eq!(w.launch(&mint).burned_on_trades, 0);
    }
}

#[test]
fn measures_rules_compute_and_sizes() {
    let mut w = World::new();
    let table_keys = protocol_lookup_table(&w);
    let table = w.env.put_lookup_table(Pubkey::new_unique(), &table_keys);
    let tables = [table];
    let specs6: Vec<Spec> = vec![
        (TOKEN, 6, none),
        (TOKEN, 9, none),
        (TOKEN, 6, none),
        (TOKEN_2022, 8, xstock),
        (TOKEN_2022, 6, none),
        (TOKEN_2022, 9, none),
    ];
    let (quotes, launch_quotes) = quote_mints(&mut w, &specs6);
    let creator = trader(&mut w, &quotes);
    let rules = every_rule();
    let budget = || [compute_unit_limit(400_000), compute_unit_price(10_000)];
    // `create_token_launch` with every rule, 1, 3 and 6 legs, and 6 with the longest metadata.
    let mut six = Pubkey::default();
    for (legs, long) in [(1usize, false), (3, false), (6, false), (6, true)] {
        let mint = Keypair::new();
        let mut args = World::token_launch_args_with("TQ", FEE, &launch_quotes[..legs], rules);
        if long {
            args.name = "N".repeat(32);
            args.symbol = "S".repeat(10);
            args.uri = "u".repeat(200);
        }
        let mints: Vec<Pubkey> = quotes[..legs].iter().map(|q| q.0).collect();
        let ix = launch::create_token_launch(
            creator.pubkey(),
            mint.pubkey(),
            w.env.treasury.pubkey(),
            args,
            &mints,
        );
        let accounts = ix.accounts.len();
        let mut ixs = budget().to_vec();
        ixs.push(ix);
        let without = w.env.v0_size(&ixs, &creator, &[&mint], &[]);
        let tx = w.env.send_v0(&ixs, &creator, &[&mint], &tables);
        tx.ok();
        println!(
            "create_token_launch, every rule, {legs} leg(s){}: {} CU, {accounts} accounts, {} B with the protocol table / {without} B without",
            if long { ", 32 B name, 10 B symbol, 200 B URI" } else { "" },
            tx.cu(),
            tx.size,
        );
        assert!(tx.size <= 1_232);
        if legs == 6 && !long {
            six = mint.pubkey();
        }
    }
    let mint = six;
    let payer = w.env.payer.insecure_clone();
    for (i, name) in [(0usize, "SPL Token"), (3, "Token-2022 xStocks-like")] {
        let ix = w.open_leg_ix(&payer.pubkey(), &mint, i);
        let accounts = ix.accounts.len();
        let mut ixs = budget().to_vec();
        ixs.push(ix);
        let without = w.env.v0_size(&ixs, &payer, &[], &[]);
        let tx = w.env.send_v0(&ixs, &payer, &[], &tables);
        tx.ok();
        println!(
            "open_leg with the kit ({name}): {} CU, {accounts} accounts, {} B with the protocol table / {without} B without",
            tx.cu(),
            tx.size
        );
        assert!(tx.size <= 1_232);
    }
    for i in [1usize, 2, 4] {
        w.open_leg(&mint, i).ok();
    }
    // The last leg: trading opens and, with the locks, `TokenLaunchLocks` is emitted.
    let ix = w.open_leg_ix(&payer.pubkey(), &mint, 5);
    let mut ixs = budget().to_vec();
    ixs.push(ix);
    let without = w.env.v0_size(&ixs, &payer, &[], &[]);
    let tx = w.env.send_v0(&ixs, &payer, &[], &tables);
    assert_eq!(tx.events::<TokenLaunchLocks>().len(), 1);
    println!(
        "open_leg with the kit, the last leg (Token-2022, with TokenLaunchLocks): {} CU, {} B with the protocol table / {without} B without",
        tx.cu(),
        tx.size
    );
    // Past the early window, so the measured trades are a plain buy and sell with the kit.
    w.env.warp(61);
    for (i, name) in [(0usize, "SPL Token"), (3, "Token-2022 xStocks-like")] {
        let b = trader(&mut w, &quotes);
        let who = b.pubkey();
        let amount = buy_within(&w, &mint, i, SUPPLY / 200);
        let swap = w.leg_swap_ix(&who, &who, &mint, i, 1, amount);
        let accounts = swap.accounts.len();
        let mut ixs = budget().to_vec();
        ixs.push(token::create_holding(who, mint, who));
        ixs.push(swap);
        let without = w.env.v0_size(&ixs, &b, &[], &[]);
        let tx = w.env.send_v0(&ixs, &b, &[], &tables);
        tx.ok();
        println!(
            "swap_spl buy with the kit's slice ({name}), with create_holding: {} CU, {accounts} accounts, {} B with the protocol table / {without} B without",
            tx.cu(),
            tx.size
        );
        assert!(tx.size <= 1_232);
        let amount = buy_within(&w, &mint, i, SUPPLY / 400);
        let mut ixs = budget().to_vec();
        ixs.push(w.leg_swap_ix(&who, &who, &mint, i, 1, amount));
        let without = w.env.v0_size(&ixs, &b, &[], &[]);
        let tx = w.env.send_v0(&ixs, &b, &[], &tables);
        tx.ok();
        println!(
            "swap_spl buy with the kit's slice ({name}): {} CU, {} B with the protocol table / {without} B without",
            tx.cu(),
            tx.size
        );
        let coins = w.env.holding(&mint, &who) / 2;
        let mut ixs = budget().to_vec();
        ixs.push(w.leg_swap_ix(&who, &who, &mint, i, 0, coins));
        let without = w.env.v0_size(&ixs, &b, &[], &[]);
        let tx = w.env.send_v0(&ixs, &b, &[], &tables);
        tx.ok();
        println!(
            "swap_spl sell with the kit's slice ({name}): {} CU, {} B with the protocol table / {without} B without",
            tx.cu(),
            tx.size
        );
        assert!(tx.size <= 1_232);
        w.env.assert_spl_invariant(&w.leg_keys(&mint, i).pool());
    }
    println!("Legs: {} bytes (unchanged)", Legs::LEN);
}
