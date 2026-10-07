//! The launchpad's token rules (`docs/hooks-v2.md` §5, the "Launch" items of §9): which token hook
//! a launch gets; the kit installed through `create_launch` for every module set; the kit
//! accounts `create_launch` takes; the pool hook's fee math on buys and sells against the
//! reference that also writes the TypeScript vectors; the holder-fee threshold; the creator's own
//! first buy; the holder vault and kit config the hook binds; the bounds and the hard ceilings;
//! max wallet at the first buy; graduation with every rule; pre-funded addresses; a burn alone;
//! second pools; protocol fees from a kit-token pool.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use bordrless_core::{fee_amount, max_wallet_cap, policy, spot_price_wad, CurveParams};
use bordrless_hook::HookAccountList;
use bordrless_kit::client as kit;
use bordrless_kit::error::KitError;
use bordrless_kit::events::{KitGraduated, KitInstalled};
use bordrless_kit::{mint_flags, modules, KitConfig};
use bordrless_launch::client as launch;
use bordrless_launch::constants::{ceilings, KIT_ID, STATUS_GRADUATED};
use bordrless_launch::error::LaunchError;
use bordrless_launch::events::{Graduated, LaunchCreated};
use bordrless_launch::state::{LaunchRules, RuleBounds};
use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price, Env, Tx};
use bordrless_program_tests::fixture::{policy_launch_config, policy_rule_bounds, World};
use bordrless_program_tests::hooks::{NewPool, SwapSpec};
use bordrless_program_tests::kit::{check_kit, KitToken};
use bordrless_program_tests::launch::*;
use bordrless_swap::client as swap;
use bordrless_swap::error::SwapError;
use bordrless_swap::events::{ProtocolFeesCollected, Swapped};
use bordrless_token::client as token;
use bordrless_token::state::{Holding, Mint};
use solana_keypair::Keypair;
use solana_signer::Signer;

/// A whole token (6 decimals).
const TOKEN: u64 = 1_000_000;
/// Index of the first of `create_launch`'s six kit accounts.
const CREATE_KIT_ACCOUNTS: usize = 23;

fn lcode(e: LaunchError) -> u32 {
    u32::from(e)
}

fn kcode(e: KitError) -> u32 {
    u32::from(e)
}

fn scode(e: SwapError) -> u32 {
    u32::from(e)
}

/// Asserts a failure with `code`, raised under `name` (codes of different programs overlap).
#[track_caller]
fn expect_err(tx: &Tx, code: u32, name: &str) {
    tx.expect_code(code);
    assert!(
        tx.logs()
            .iter()
            .any(|l| l.contains(&format!("Error Code: {name}"))),
        "expected {name}\n{}",
        tx.logs().join("\n")
    );
}

/// The rules of kit module set `m`: holder rewards 1% both sides, max wallet 5%, the creator
/// wallet locked 30 days, buys in the first minute locked for an hour.
fn rules_of(m: u8) -> LaunchRules {
    let on = |module: u8| m & module != 0;
    rules(
        if on(modules::HOLDER_REWARDS) { 100 } else { 0 },
        if on(modules::HOLDER_REWARDS) { 100 } else { 0 },
        0,
        0,
        if on(modules::MAX_WALLET) { 500 } else { 0 },
        if on(modules::CREATOR_WALLET_LOCK) {
            30 * DAY
        } else {
            0
        },
        if on(modules::EARLY_BUYER_LOCK) { 60 } else { 0 },
        if on(modules::EARLY_BUYER_LOCK) {
            3_600
        } else {
            0
        },
    )
}

/// A launch by a fresh creator holding 20 bridged SOL, opening at the policy's 25 SOL.
fn launch_with(
    w: &mut World,
    symbol: &str,
    creator_fee_bps: u16,
    rules: LaunchRules,
) -> (Keypair, Pubkey, Tx) {
    let creator = w.wallet_with_sol(20 * SOL);
    let (mint, tx) = w.create_launch_with(&creator, symbol, creator_fee_bps, VQ, rules);
    tx.ok();
    (creator, mint, tx)
}

/// A wallet holding `sol` lamports of bridged SOL and a holding of `mint`.
fn trader(w: &mut World, mint: &Pubkey, sol: u64) -> Keypair {
    let t = w.wallet_with_sol(sol);
    w.holdings(&t, *mint, &[t.pubkey()]);
    t
}

/// A swap of `amount` by `t` on the launch of `mint`, delivered to itself: it lands, and its
/// `Swapped` event is what the reference computed. Answers the reference.
fn checked_swap(
    w: &mut World,
    mint: &Pubkey,
    t: &Keypair,
    buy: bool,
    amount: u64,
) -> (LaunchSwap, Tx) {
    let q = w.launch_quote(mint, &t.pubkey(), buy, amount);
    let lp = w.launch_lp_fee(mint, &t.pubkey(), &t.pubkey(), buy);
    let l = w.launch(mint);
    let ix = w.launch_swap_ix(&t.pubkey(), mint, u8::from(buy), amount, 0);
    let tx = w.env.send_paid_by(&[ix], t, &[]);
    tx.ok();
    expect_swapped(&tx.event::<Swapped>(), &q, &l, buy, lp);
    (q, tx)
}

fn supply(w: &World, mint: &Pubkey) -> u64 {
    w.env.read::<Mint>(mint).supply
}

/// The balance of the holding at `address` (0 when it does not exist).
fn balance(w: &World, address: &Pubkey) -> u64 {
    w.env.try_read::<Holding>(address).map_or(0, |h| h.amount)
}

#[test]
fn the_rules_decide_the_token_hook() {
    let mut w = World::new();
    // No rules: no hook, no hook authority, no kit; the pool hook's registry still names the
    // holder vault and the kit config at their fixed addresses.
    let (_, plain, tx) = launch_with(&mut w, "PLN", 100, LaunchRules::NONE);
    let plain_cu = tx.cu();
    let ev: LaunchCreated = tx.event();
    assert_eq!(
        (ev.modules, ev.kit_config, ev.holder_vault, ev.rules),
        (0, None, None, LaunchRules::NONE)
    );
    assert_eq!(
        (
            ev.creator_unlock_at,
            ev.early_window_end,
            ev.early_unlock_at
        ),
        (0, 0, 0)
    );
    let m: Mint = w.env.read(&plain);
    assert_eq!(
        (
            m.hook_program,
            m.hook_authority,
            m.hook_flags,
            m.mint_authority
        ),
        (None, None, 0, None)
    );
    assert!(w.env.account(&kit::kit_config_address(&plain)).is_none());
    let l = w.launch(&plain);
    assert_eq!(
        (l.modules, l.kit_config, l.holder_vault, l.kit_caller_bump),
        (
            0,
            kit::kit_config_address(&plain),
            launch::holder_vault_address(&plain, &w.sol),
            0
        )
    );
    let pool = w.launch_pool_key(&plain);
    let registry = HookAccountList::decode(
        &w.env
            .account(&launch::registry_address(&pool))
            .unwrap()
            .data,
    )
    .unwrap();
    let resolved = registry
        .resolve(
            &[
                swap::hook_signer(&bordrless_launch::ID),
                pool,
                plain,
                w.sol,
                Pubkey::new_unique(),
            ],
            &Pubkey::default(),
            &Pubkey::default(),
        )
        .unwrap();
    assert_eq!(resolved, launch::hook_extras(&plain, &w.sol));
    let len = |key: &Pubkey| w.env.account(key).unwrap().data.len();
    let (config_len, launch_len) = (
        len(&launch::config_address()),
        len(&launch::launch_address(&plain)),
    );
    assert_eq!(
        (config_len, launch_len),
        (
            bordrless_launch::state::Config::LEN,
            bordrless_launch::state::Launch::LEN
        )
    );
    println!(
        "accounts: Config {config_len} bytes, Launch {launch_len} bytes, pool registry {} bytes",
        len(&launch::registry_address(&pool))
    );

    // A burn alone runs in the pool hook: no token hook either.
    let burn_only = rules(0, 0, 50, 50, 0, 0, 0, 0);
    let (_, burnt, tx) = launch_with(&mut w, "BRN", 50, burn_only);
    let ev: LaunchCreated = tx.event();
    assert_eq!((ev.modules, ev.kit_config, ev.rules), (0, None, burn_only));
    let m: Mint = w.env.read(&burnt);
    assert_eq!(
        (m.hook_program, m.hook_authority, m.hook_flags),
        (None, None, 0)
    );

    // Rules with a kit module: the kit is the hook from the mint's creation, with the flags of
    // its modules and no hook authority.
    let (_, kitted, tx) = launch_with(&mut w, "KIT", 50, presets::rewards_and_cap());
    let m: Mint = w.env.read(&kitted);
    assert_eq!(
        (
            m.hook_program,
            m.hook_authority,
            m.hook_flags,
            m.mint_authority
        ),
        (Some(KIT_ID), None, mint_flags(1 | 2 | 4), None)
    );
    let ev: LaunchCreated = tx.event();
    assert_eq!(
        (ev.modules, ev.kit_config, ev.holder_vault),
        (
            7,
            Some(kit::kit_config_address(&kitted)),
            Some(launch::holder_vault_address(&kitted, &w.sol))
        )
    );
    println!(
        "create_launch (legacy-size v0, no table): no rules CU {plain_cu}; rewards and cap CU {} \
         size {} trace {} height {}",
        tx.cu(),
        tx.size,
        tx.trace_len(),
        tx.max_height()
    );
}

#[test]
fn every_module_set_installs_the_kit_through_create_launch() {
    let mut w = World::new();
    let sol = w.sol;
    for m in 1..=modules::ALL {
        let r = rules_of(m);
        assert_eq!(r.modules(), m);
        let (creator, mint, tx) = launch_with(&mut w, &format!("M{m}"), 50, r);
        let now = w.env.now;
        let kit_config = kit::kit_config_address(&mint);
        let vault = launch::holder_vault_address(&mint, &sol);
        let rewards = m & modules::HOLDER_REWARDS != 0;
        // The mint: the kit as hook from its first instruction, its flags, nobody to change it,
        // the whole supply minted once.
        let mt: Mint = w.env.read(&mint);
        assert_eq!(
            (
                mt.hook_program,
                mt.hook_authority,
                mt.hook_flags,
                mt.mint_authority
            ),
            (Some(KIT_ID), None, mint_flags(m), None),
            "modules {m}"
        );
        assert_eq!(
            (mt.supply, mt.max_supply),
            (policy::TOKEN_SUPPLY, policy::TOKEN_SUPPLY)
        );
        // The kit's init, signed by the mint's kit-caller PDA.
        let installed: KitInstalled = tx.event();
        let c: KitConfig = w.env.kit_config(&mint);
        let l = w.launch(&mint);
        assert_eq!(
            (installed.mint, installed.kit_config, installed.modules),
            (mint, kit_config, m)
        );
        assert_eq!(
            (c.modules, c.launch, c.pool, c.creator, c.reward_mint),
            (
                m,
                launch::launch_address(&mint),
                l.pool,
                creator.pubkey(),
                sol
            )
        );
        assert_eq!(
            c.reward_vault,
            if rewards { vault } else { Pubkey::default() }
        );
        assert_eq!(
            (c.eligible, c.min_eligible, c.graduated),
            (0, policy::TOKEN_SUPPLY / 1_000, false)
        );
        assert_eq!(
            c.max_wallet_amount,
            if m & modules::MAX_WALLET != 0 {
                max_wallet_cap(policy::TOKEN_SUPPLY, 500)
            } else {
                0
            }
        );
        assert_eq!(c.kit_caller_bump, launch::kit_caller_address(&mint).1);
        let at = |on: bool, secs: i64| if on { now + secs } else { 0 };
        assert_eq!(
            (c.creator_unlock_at, c.early_window_end, c.early_unlock_at),
            (
                at(m & modules::CREATOR_WALLET_LOCK != 0, 30 * 86_400),
                at(m & modules::EARLY_BUYER_LOCK != 0, 60),
                at(m & modules::EARLY_BUYER_LOCK != 0, 3_600)
            )
        );
        // The launch keeps the same.
        assert_eq!(
            (
                l.modules,
                l.kit_config,
                l.holder_vault,
                l.kit_caller_bump,
                l.rules
            ),
            (m, kit_config, vault, c.kit_caller_bump, r)
        );
        assert_eq!(
            (l.creator_unlock_at, l.early_window_end, l.early_unlock_at),
            (c.creator_unlock_at, c.early_window_end, c.early_unlock_at)
        );
        // The vault exists exactly with holder rewards; the registry names it, or the kit.
        let v = w.env.try_read::<Holding>(&vault);
        assert_eq!(v.is_some(), rewards);
        if let Some(v) = v {
            assert_eq!((v.mint, v.owner, v.amount), (sol, kit_config, 0));
        }
        let list =
            HookAccountList::decode(&w.env.account(&kit::registry_address(&mint)).unwrap().data)
                .unwrap();
        let extras = list
            .resolve(
                &[Pubkey::default(); 5],
                &Pubkey::default(),
                &Pubkey::default(),
            )
            .unwrap();
        assert_eq!(extras, kit::hook_extras(&mint, rewards.then_some(vault)));
        // Launch to pool was excluded to excluded: nobody is eligible.
        assert_eq!(
            w.env.holding(&mint, &l.pool),
            l.curve_tokens,
            "the deposit reached the pool"
        );
        // A buy and a wallet-to-wallet transfer, after the windows.
        w.env.warp(61);
        let (a, b) = (trader(&mut w, &mint, SOL), trader(&mut w, &mint, SOL));
        checked_swap(&mut w, &mint, &a, true, SOL / 100);
        let got = w.env.holding(&mint, &a.pubkey());
        w.send_tokens(&a, mint, &b.pubkey(), got / 2).ok();
        assert_eq!(w.env.holding(&mint, &b.pubkey()), got / 2);
        let k = KitToken::of(&w.env, mint);
        check_kit(&w.env, &k, &[a.pubkey(), b.pubkey(), creator.pubkey()]);
        assert_eq!(w.env.kit_config(&mint).eligible, got);
    }
}

#[test]
fn create_launch_takes_the_kit_accounts_exactly_with_kit_rules() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(20 * SOL);
    let sol = w.sol;
    let other = Keypair::new().pubkey();
    // Each case: rules, then the account (of the six) to put in place, and the error.
    let try_launch = |w: &mut World, r: LaunchRules, i: usize, meta: AccountMeta| -> Tx {
        let mint = Keypair::new();
        let mut ix = w.create_launch_ix(&creator.pubkey(), &mint.pubkey(), "ACC", 50, VQ, r);
        ix.accounts[CREATE_KIT_ACCOUNTS + i] = meta;
        w.env.send_paid_by(&[ix], &creator, &[&mint])
    };
    let absent = AccountMeta::new_readonly(bordrless_launch::ID, false);
    // A launch without kit rules refuses any kit account.
    let kit_program = AccountMeta::new_readonly(KIT_ID, false);
    expect_err(
        &try_launch(&mut w, LaunchRules::NONE, 0, kit_program.clone()),
        lcode(LaunchError::UnexpectedKitAccounts),
        "UnexpectedKitAccounts",
    );
    // A launch with them refuses each one missing...
    let every = presets::every_rule();
    for i in 0..6 {
        expect_err(
            &try_launch(&mut w, every, i, absent.clone()),
            lcode(LaunchError::KitAccountsMissing),
            "KitAccountsMissing",
        );
    }
    // ... or at another address (the registry is the kit's own to check).
    let wrong = [
        AccountMeta::new_readonly(hook_tester::ID, false),
        AccountMeta::new(kit::kit_config_address(&other), false),
        AccountMeta::new(kit::registry_address(&other), false),
        AccountMeta::new(launch::holder_vault_address(&other, &sol), false),
        AccountMeta::new_readonly(launch::kit_caller_address(&other).0, false),
        AccountMeta::new_readonly(token::event_authority(), false),
    ];
    for (i, meta) in wrong.into_iter().enumerate() {
        let tx = try_launch(&mut w, every, i, meta);
        if i == 2 {
            // The kit's own seeds check.
            tx.expect_code(2006);
        } else {
            expect_err(&tx, lcode(LaunchError::WrongKitAccount), "WrongKitAccount");
        }
    }
    // Without holder rewards the vault is absent; passing one is refused.
    let no_rewards = rules(0, 0, 0, 0, 500, 0, 0, 0);
    let mint = Keypair::new();
    let ix = w.create_launch_ix(&creator.pubkey(), &mint.pubkey(), "ACC", 50, VQ, no_rewards);
    let vault = AccountMeta::new(launch::holder_vault_address(&mint.pubkey(), &sol), false);
    let mut ix2 = ix.clone();
    ix2.accounts[CREATE_KIT_ACCOUNTS + 3] = vault;
    expect_err(
        &w.env.send_paid_by(&[ix2], &creator, &[&mint]),
        lcode(LaunchError::UnexpectedKitAccounts),
        "UnexpectedKitAccounts",
    );
    // As built, it lands.
    w.env.send_paid_by(&[ix], &creator, &[&mint]).ok();
    assert_eq!(
        w.env.kit_config(&mint.pubkey()).modules,
        modules::MAX_WALLET
    );
}

#[test]
fn the_pool_hook_takes_each_side_at_its_own_rates() {
    let mut w = World::new();
    // Holder rewards 1% on buys and 2% on sells; burn 0.25% on buys and 1% on sells; creator
    // 0.5%. 1.75% on buys, 3.5% on sells under a raised bound.
    let mut config = policy_launch_config(w.env.deployer.pubkey(), w.env.treasury.pubkey(), w.sol);
    config.rule_bounds.max_rules_fee_bps = 400;
    let deployer = w.env.deployer.insecure_clone();
    w.env
        .send_paid_by(
            &[launch::set_config(deployer.pubkey(), config)],
            &deployer,
            &[],
        )
        .ok();
    let r = rules(100, 200, 25, 100, 0, 0, 0, 0);
    let (_, mint, _) = launch_with(&mut w, "FEE", 50, r);
    w.env.warp(policy::SNIPER_WINDOW_SECS);
    let l = w.launch(&mint);
    let supply0 = supply(&w, &mint);
    let (a, b) = (
        trader(&mut w, &mint, 20 * SOL),
        trader(&mut w, &mint, 20 * SOL),
    );
    // What each swap's hook took, as the reference computed it.
    let mut taken: Vec<LaunchSwap> = Vec::new();
    let mut tally = |q: &LaunchSwap| taken.push(q.clone());

    // Nobody holds yet: the first buy pays the creator fee and the burn, no holder fee.
    let (eligible, min) = w.eligibility(&mint);
    assert_eq!((eligible, min), (0, policy::TOKEN_SUPPLY / 1_000));
    let (q, _) = checked_swap(&mut w, &mint, &a, true, SOL);
    assert!(!q.holder_fee_on && q.holder_fee == 0);
    assert_eq!(q.creator_fee, fee_amount(SOL, 50).unwrap());
    assert_eq!(q.burn, q.amount_out.unwrap() * 25 / 10_000);
    tally(&q);
    // From the threshold up a buy pays the holder fee, at the buy rate, into the holder vault.
    assert!(w.eligibility(&mint).0 >= min);
    let (q, _) = checked_swap(&mut w, &mint, &b, true, SOL);
    assert!(q.holder_fee_on);
    assert_eq!(q.holder_fee, fee_amount(SOL, 100).unwrap());
    tally(&q);
    assert_eq!(balance(&w, &l.holder_vault), q.holder_fee);
    // A sell: the burn from the input at the sell rate; the creator and holder fees from the
    // curve's output (a kit token's input side cuts nothing, so the DEX takes nothing off it),
    // the holder fee at the sell rate; Bordrless's quarter of the two held back from the delivery.
    let tokens = w.env.holding(&mint, &a.pubkey()) / 2;
    let (q, _) = checked_swap(&mut w, &mint, &a, false, tokens);
    assert_eq!(q.burn, tokens / 100);
    let told = q.amount_out.unwrap();
    assert_eq!(
        (q.creator_fee, q.holder_fee),
        (
            fee_amount(told, 50).unwrap(),
            fee_amount(told, 200).unwrap()
        )
    );
    assert_eq!(
        q.protocol_fee,
        bordrless_core::protocol_share(
            q.creator_fee + q.holder_fee,
            policy::LAUNCH_PROTOCOL_SHARE_BPS
        )
        .unwrap()
    );
    tally(&q);

    // The sell-side guard: an output too small for both fees pays neither. The smallest sell
    // that delivers anything leaves one lamport after the protocol fee.
    let lands = |w: &World, d: u64| {
        w.launch_quote(&mint, &b.pubkey(), false, d)
            .failure
            .is_none()
    };
    let mut hi = 1u64;
    while !lands(&w, hi) {
        hi *= 2;
    }
    let mut lo = hi / 2;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if lands(&w, mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let dust = hi;
    let q = w.launch_quote(&mint, &b.pubkey(), false, dust);
    assert_eq!((q.creator_fee, q.holder_fee), (0, 0), "{q:?}");
    let (q, tx) = checked_swap(&mut w, &mint, &b, false, dust);
    assert!(tx.event::<Swapped>().deltas_out.is_empty());
    tally(&q);
    // The buy-side guard: 1 lamport would pay 1 + 1 in fees, so neither is taken, Bordrless's
    // share of nothing is nothing, and the DEX refuses what is left for the LP fee (without the
    // guard it would refuse the cut).
    let tx = w
        .env
        .send_paid_by(&[w.launch_swap_ix(&b.pubkey(), &mint, 1, 1, 0)], &b, &[]);
    expect_err(&tx, scode(SwapError::FeeExceedsInput), "FeeExceedsInput");

    // The launch counts what its hook took; the vaults and the supply agree.
    let creator_fees: u64 = taken.iter().map(|q| q.creator_fee).sum();
    let holder_fees: u64 = taken.iter().map(|q| q.holder_fee).sum();
    let burned: u64 = taken.iter().map(|q| q.burn).sum();
    let l = w.launch(&mint);
    assert_eq!(
        (
            l.creator_fees_accrued,
            l.holder_fees_accrued,
            l.burned_on_trades
        ),
        (creator_fees, holder_fees, burned)
    );
    assert_eq!(balance(&w, &l.quote_holding), creator_fees);
    assert_eq!(balance(&w, &l.holder_vault), holder_fees);
    assert_eq!(supply(&w, &mint), supply0 - burned);
    let p = w.launch_pool(&mint);
    assert_eq!(w.env.holding(&mint, &l.pool), p.base_reserve);
    assert_eq!(
        w.env.holding(&w.sol, &l.pool),
        p.quote_reserve + p.protocol_fees_quote
    );
}

#[test]
fn the_holder_fee_waits_for_the_threshold() {
    let mut w = World::new();
    let (_, mint, _) = launch_with(&mut w, "THR", 50, presets::burn());
    w.env.warp(policy::SNIPER_WINDOW_SECS);
    let (_, min) = w.eligibility(&mint);
    let holder = trader(&mut w, &mint, 5 * SOL);
    // A buy that leaves holders just below the threshold pays no holder fee, nor does the next
    // buy while they stay below it.
    let below = {
        let p = w.launch_pool(&mint);
        let l = w.launch(&mint);
        max_buy_within(
            &reserves_of(&p),
            policy::LP_FEE_BPS,
            policy::LAUNCH_PROTOCOL_SHARE_BPS,
            &l.rules.fee_rates(l.creator_fee_bps),
            0,
            min,
            min - 1,
        )
    };
    let (q, _) = checked_swap(&mut w, &mint, &holder, true, below);
    assert!(!q.holder_fee_on && q.holder_fee == 0);
    let eligible = w.eligibility(&mint).0;
    assert!(eligible < min && eligible > min / 2, "{eligible} {min}");
    let (q, _) = checked_swap(&mut w, &mint, &holder, true, SOL / 1_000);
    assert_eq!(q.holder_fee, 0);
    // Above it, buys pay.
    let (q, _) = checked_swap(&mut w, &mint, &holder, true, SOL / 10);
    assert!(q.holder_fee_on && q.holder_fee > 0);
    // A sell that takes holders back below it pays the creator fee but no holder fee: the kit
    // has counted the seller's tokens out by the time the hook reads it.
    let held = w.env.holding(&mint, &holder.pubkey());
    let sell = held - min / 2;
    assert!(w.eligibility(&mint).0 >= min);
    let (q, _) = checked_swap(&mut w, &mint, &holder, false, sell);
    assert!(!q.holder_fee_on && q.holder_fee == 0 && q.creator_fee > 0);
    assert!(w.eligibility(&mint).0 < min);
}

#[test]
fn the_creators_own_first_buy_pays_the_normal_lp_fee_once() {
    let mut w = World::new();
    let r = presets::rewards_and_cap();
    let (creator, mint, _) = launch_with(&mut w, "OWN", 50, r);
    w.holdings(&creator, mint, &[creator.pubkey()]);
    // A bot buys first, inside the window: the sniper fee.
    w.env.warp(1);
    let bot = trader(&mut w, &mint, 5 * SOL);
    let (_, tx) = checked_swap(&mut w, &mint, &bot, true, SOL / 100);
    assert!(tx.event::<Swapped>().lp_fee_bps > 5_000);
    assert!(!w.launch(&mint).creator_bought);
    // The creator's own first buy into its own wallet pays the normal fee all the same.
    w.env.warp(1);
    let (_, tx) = checked_swap(&mut w, &mint, &creator, true, SOL / 100);
    assert_eq!(tx.event::<Swapped>().lp_fee_bps, policy::LP_FEE_BPS);
    assert!(w.launch(&mint).creator_bought);
    // Only once.
    let (_, tx) = checked_swap(&mut w, &mint, &creator, true, SOL / 100);
    assert!(tx.event::<Swapped>().lp_fee_bps > 5_000);
    // The creator wallet lock holds what it bought: no sell into the launch pool until the
    // unlock (the kit refuses the transfer to the vault), then it sells like anyone.
    let held = w.env.holding(&mint, &creator.pubkey());
    let ix = w.launch_swap_ix(&creator.pubkey(), &mint, 0, held / 2, 0);
    expect_err(
        &w.env.send_paid_by(&[ix], &creator, &[]),
        kcode(KitError::CreatorLocked),
        "CreatorLocked",
    );
    let unlock = w.launch(&mint).creator_unlock_at;
    w.env.warp(unlock - w.env.now);
    checked_swap(&mut w, &mint, &creator, false, held / 2);

    // Bought for another wallet, the creator's buy pays the sniper fee and keeps the exemption.
    let (creator2, mint2, _) = launch_with(&mut w, "GFT", 50, r);
    let friend = w.env.funded(SOL);
    w.holdings(&creator2, mint2, &[creator2.pubkey(), friend.pubkey()]);
    w.env.warp(1);
    let lp = w.launch_lp_fee(&mint2, &creator2.pubkey(), &friend.pubkey(), true);
    assert!(lp > 5_000);
    let ix = w.launch_swap_to_ix(&creator2.pubkey(), &friend.pubkey(), &mint2, 1, SOL / 100);
    let tx = w.env.send_paid_by(&[ix], &creator2, &[]);
    tx.ok();
    let ev: Swapped = tx.event();
    assert_eq!((ev.lp_fee_bps, ev.recipient), (lp, friend.pubkey()));
    assert!(!w.launch(&mint2).creator_bought);
    let (_, tx) = checked_swap(&mut w, &mint2, &creator2, true, SOL / 100);
    assert_eq!(tx.event::<Swapped>().lp_fee_bps, policy::LP_FEE_BPS);
    // A bot buying into the creator's wallet is not the creator's buy.
    let (creator3, mint3, _) = launch_with(&mut w, "PSH", 50, r);
    w.holdings(&creator3, mint3, &[creator3.pubkey()]);
    let bot3 = trader(&mut w, &mint3, 5 * SOL);
    w.env.warp(1);
    let ix = w.launch_swap_to_ix(&bot3.pubkey(), &creator3.pubkey(), &mint3, 1, SOL / 100);
    let tx = w.env.send_paid_by(&[ix], &bot3, &[]);
    tx.ok();
    assert!(tx.event::<Swapped>().lp_fee_bps > 5_000);
    assert!(!w.launch(&mint3).creator_bought);
}

#[test]
fn the_hook_binds_its_holder_vault_and_kit_config() {
    let mut w = World::new();
    let (_, mint, _) = launch_with(&mut w, "BND", 50, presets::rewards_and_cap());
    let (_, other, _) = launch_with(&mut w, "OTH", 50, presets::rewards_and_cap());
    w.env.warp(policy::SNIPER_WINDOW_SECS);
    let t = trader(&mut w, &mint, 5 * SOL);
    let thief = w.wallet_with_sol(SOL);
    let thief_sol = token::holding_address(&w.sol, &thief.pubkey());
    // The pool hook's four extras are the swap's last accounts: indices 5 to 8.
    let swap_with = |w: &mut World, index: usize, meta: AccountMeta| -> Tx {
        let mut ix = w.launch_swap_ix(&t.pubkey(), &mint, 1, SOL / 10, 0);
        let n = ix.accounts.len();
        ix.accounts[n - 4 + (index - 5)] = meta;
        w.env.send_paid_by(&[ix], &t, &[])
    };
    let other_vault = launch::holder_vault_address(&other, &w.sol);
    for holding in [thief_sol, other_vault] {
        expect_err(
            &swap_with(&mut w, 7, AccountMeta::new(holding, false)),
            lcode(LaunchError::WrongHolderVault),
            "WrongHolderVault",
        );
    }
    expect_err(
        &swap_with(
            &mut w,
            8,
            AccountMeta::new_readonly(kit::kit_config_address(&other), false),
        ),
        lcode(LaunchError::WrongKitAccount),
        "WrongKitAccount",
    );
    expect_err(
        &swap_with(&mut w, 6, AccountMeta::new(thief_sol, false)),
        lcode(LaunchError::WrongHolding),
        "WrongHolding",
    );
    expect_err(
        &swap_with(
            &mut w,
            5,
            AccountMeta::new(launch::launch_address(&other), false),
        ),
        lcode(LaunchError::WrongPool),
        "WrongPool",
    );
    assert_eq!(w.env.holding(&mint, &t.pubkey()), 0);
    checked_swap(&mut w, &mint, &t, true, SOL / 10);
}

#[test]
fn a_first_buy_at_the_max_wallet_cap_lands_and_one_above_is_refused() {
    let mut w = World::new();
    // Max wallet 2% with a buy burn, which lets a larger input through.
    let r = rules(100, 100, 50, 50, 200, 0, 0, 0);
    let (creator, mint, _) = launch_with(&mut w, "CAP", 50, r);
    w.holdings(&creator, mint, &[creator.pubkey()]);
    let l = w.launch(&mint);
    let curve = CurveParams {
        curve_tokens: l.curve_tokens,
        reserve_tokens: l.reserve_tokens,
        virtual_quote: l.virtual_quote,
        virtual_base: l.virtual_base,
        graduation_quote: l.graduation_quote,
    };
    let max = dev_buy_max(
        &curve,
        policy::TOKEN_SUPPLY,
        &r,
        50,
        policy::LP_FEE_BPS,
        policy::LAUNCH_PROTOCOL_SHARE_BPS,
    )
    .unwrap();
    let cap = w.env.kit_config(&mint).max_wallet_amount;
    let at = w.launch_quote(&mint, &creator.pubkey(), true, max);
    let over = w.launch_quote(&mint, &creator.pubkey(), true, max + 1);
    assert!(at.delivered.unwrap() <= cap && over.delivered.unwrap() > cap);
    // One above the cap: the kit refuses, nothing moves, the exemption is kept.
    let ix = w.launch_swap_ix(&creator.pubkey(), &mint, 1, max + 1, 0);
    expect_err(
        &w.env.send_paid_by(&[ix], &creator, &[]),
        kcode(KitError::MaxWalletExceeded),
        "MaxWalletExceeded",
    );
    assert!(!w.launch(&mint).creator_bought);
    // At the cap: it lands, with the normal LP fee.
    let (q, tx) = checked_swap(&mut w, &mint, &creator, true, max);
    assert_eq!(tx.event::<Swapped>().lp_fee_bps, policy::LP_FEE_BPS);
    assert_eq!(
        w.env.holding(&mint, &creator.pubkey()),
        q.delivered.unwrap()
    );
    println!(
        "devBuyMaxLamports at 2% with a 0.5% buy burn: {max} lamports deliver {} of a {cap} cap",
        q.delivered.unwrap()
    );
}

#[test]
fn graduation_with_every_rule() {
    let mut w = World::new();
    let r = presets::every_rule();
    let (creator, mint, _) = launch_with(&mut w, "GRAD", 50, r);
    let (_, other, _) = launch_with(&mut w, "OTHR", 50, r);
    // Past the sniper window and the early-buyer unlock.
    w.env.warp(3_601);
    let buyers = w.fill_curve(&mint, SOL / 10);
    println!("curve filled by {} wallets within max wallet", buyers.len());
    let k = KitToken::of(&w.env, mint);
    let mut owners: Vec<Pubkey> = buyers.iter().map(|b| b.pubkey()).collect();
    owners.push(creator.pubkey());
    // The last buy raises the rest; the graduation is cranked after it, so its refusals can be
    // tried on a pool that is ready.
    let last = trader(&mut w, &mint, 10 * SOL);
    let amount = w.crossing_buy_amount(&mint, &last.pubkey());
    checked_swap(&mut w, &mint, &last, true, amount);
    owners.push(last.pubkey());
    let p = w.launch_pool(&mint);
    let l = w.launch(&mint);
    assert!(p.curve && p.quote_reserve >= l.graduation_quote);
    check_kit(&w.env, &k, &owners);
    let kit_before = w.env.kit_config(&mint);

    let cranker = w.env.funded(SOL);
    let grad = |w: &World| w.graduate_ix(&cranker.pubkey(), &mint);
    // The kit's five accounts sit before the event authority and the program.
    let with = |w: &World, i: usize, meta: AccountMeta| {
        let mut ix = grad(w);
        let n = ix.accounts.len();
        ix.accounts[n - 7 + i] = meta;
        ix
    };
    let absent = AccountMeta::new_readonly(bordrless_launch::ID, false);
    for i in 0..5 {
        let ix = with(&w, i, absent.clone());
        expect_err(
            &w.env.send_paid_by(&[ix], &cranker, &[]),
            lcode(LaunchError::KitAccountsMissing),
            "KitAccountsMissing",
        );
    }
    let wrong = [
        AccountMeta::new_readonly(hook_tester::ID, false),
        // Another token's config: what the kit's `graduate` would be bound to.
        AccountMeta::new(kit::kit_config_address(&other), false),
        // The kit's id stands for the vault only without holder rewards.
        AccountMeta::new_readonly(KIT_ID, false),
        AccountMeta::new_readonly(launch::kit_caller_address(&other).0, false),
        AccountMeta::new_readonly(token::event_authority(), false),
    ];
    for (i, meta) in wrong.into_iter().enumerate() {
        let ix = with(&w, i, meta);
        expect_err(
            &w.env.send_paid_by(&[ix], &cranker, &[]),
            lcode(LaunchError::WrongKitAccount),
            "WrongKitAccount",
        );
    }
    let ix = with(
        &w,
        2,
        AccountMeta::new_readonly(launch::holder_vault_address(&other, &w.sol), false),
    );
    expect_err(
        &w.env.send_paid_by(&[ix], &cranker, &[]),
        lcode(LaunchError::WrongKitAccount),
        "WrongKitAccount",
    );

    // The graduation.
    let supply_before = supply(&w, &mint);
    let reserve = w.env.holding(&mint, &launch::launch_address(&mint));
    let price_before = spot_price_wad(
        p.base_reserve,
        p.virtual_base,
        p.quote_reserve,
        p.virtual_quote,
    )
    .unwrap();
    let tx = w.env.send_paid_by(&[grad(&w)], &cranker, &[]);
    tx.ok();
    println!(
        "graduate, every rule: CU {} size {} trace {} height {}",
        tx.cu(),
        tx.size,
        tx.trace_len(),
        tx.max_height()
    );
    let ev: Graduated = tx.event();
    assert_eq!(ev.topup + ev.burned, reserve);
    assert_eq!(supply(&w, &mint), supply_before - ev.burned);
    assert_eq!(ev.supply, supply_before - ev.burned);
    let p = w.launch_pool(&mint);
    assert!(!p.curve);
    let price_after = spot_price_wad(p.base_reserve, 0, p.quote_reserve, 0).unwrap();
    assert!(
        price_before.abs_diff(price_after) * 1_000_000 <= price_before,
        "price {price_before} -> {price_after}"
    );
    assert_eq!(w.env.holding(&mint, &l.pool), p.base_reserve);
    assert_eq!(w.env.holding(&mint, &launch::launch_address(&mint)), 0);
    // The kit was told: max wallet lifts; the accounting does not change.
    let told: KitGraduated = tx.event();
    assert_eq!(told.mint, mint);
    let kit_after = w.env.kit_config(&mint);
    assert!(kit_after.graduated);
    assert_eq!(kit_after.eligible, kit_before.eligible);
    check_kit(&w.env, &k, &owners);
    assert_eq!(w.launch(&mint).status, STATUS_GRADUATED);
    // Max wallet lifted: one wallet now holds more than the cap.
    let whale = trader(&mut w, &mint, 100 * SOL);
    checked_swap(&mut w, &mint, &whale, true, 40 * SOL);
    assert!(w.env.holding(&mint, &whale.pubkey()) > kit_after.max_wallet_amount);
    owners.push(whale.pubkey());
    check_kit(&w.env, &k, &owners);
    // Once.
    expect_err(
        &w.env.send_paid_by(&[grad(&w)], &cranker, &[]),
        lcode(LaunchError::AlreadyGraduated),
        "AlreadyGraduated",
    );
}

#[test]
fn create_launch_fits_with_three_new_addresses_prefunded() {
    let mut w = World::new();
    let table = w
        .env
        .put_lookup_table(Pubkey::new_unique(), &protocol_lookup_table(&w));
    let creator = w.wallet_with_sol(10 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
    // Someone funds three of the addresses the launch creates as soon as it sees the mint: the
    // launch (Anchor's init), the kit config (the kit's init) and the reward vault (the token
    // program's holding).
    let funded = [
        launch::launch_address(&m),
        kit::kit_config_address(&m),
        launch::holder_vault_address(&m, &w.sol),
    ];
    for key in funded {
        w.env.fund(key, 1_000_000);
    }
    let ix = w.create_launch_ix(&creator.pubkey(), &m, "FUND", 50, VQ, presets::every_rule());
    let tx = w.env.send_v0(
        &[
            compute_unit_limit(1_400_000),
            compute_unit_price(20_000),
            ix,
        ],
        &creator,
        &[&mint],
        &[table],
    );
    tx.ok();
    println!(
        "create_launch, every module, 3 addresses pre-funded: CU {} v0 bytes {} trace {} height {}",
        tx.cu(),
        tx.size,
        tx.trace_len(),
        tx.max_height()
    );
    assert!(tx.trace_len() <= 64 && tx.size <= 1_232 && tx.max_height() <= 4);
    // Each was taken over as if it had been empty.
    let owners = [bordrless_launch::ID, bordrless_kit::ID, bordrless_token::ID];
    for (key, owner) in funded.iter().zip(owners) {
        assert_eq!(w.env.account(key).unwrap().owner, owner);
    }
    assert_eq!(w.launch(&m).mint, m);
    assert_eq!(w.env.kit_config(&m).modules, modules::ALL);
    let vault: Holding = w.env.read(&funded[2]);
    assert_eq!((vault.mint, vault.owner), (w.sol, funded[1]));
}

#[test]
fn the_bounds_and_the_hard_ceilings() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(50 * SOL);
    let try_rules = |w: &mut World, fee: u16, r: LaunchRules| -> Tx {
        let mint = Keypair::new();
        let ix = w.create_launch_ix(&creator.pubkey(), &mint.pubkey(), "BND", fee, VQ, r);
        w.env.send_paid_by(&[ix], &creator, &[&mint])
    };
    let refused = |w: &mut World, fee: u16, r: LaunchRules, e: LaunchError| {
        let name = format!("{e:?}");
        expect_err(&try_rules(w, fee, r), lcode(e), &name);
    };
    use LaunchError::*;
    // §5.2's policy, at and over each bound.
    refused(&mut w, 0, rules(201, 0, 0, 0, 0, 0, 0, 0), HolderFeeTooHigh);
    refused(&mut w, 0, rules(0, 201, 0, 0, 0, 0, 0, 0), HolderFeeTooHigh);
    refused(&mut w, 0, rules(0, 0, 101, 0, 0, 0, 0, 0), BurnTooHigh);
    refused(&mut w, 0, rules(0, 0, 0, 101, 0, 0, 0, 0), BurnTooHigh);
    refused(
        &mut w,
        100,
        rules(200, 0, 1, 0, 0, 0, 0, 0),
        RulesFeeTooHigh,
    );
    refused(
        &mut w,
        50,
        rules(0, 200, 0, 100, 0, 0, 0, 0),
        RulesFeeTooHigh,
    );
    refused(
        &mut w,
        0,
        rules(0, 0, 0, 0, 99, 0, 0, 0),
        MaxWalletOutOfBounds,
    );
    refused(
        &mut w,
        0,
        rules(0, 0, 0, 0, 501, 0, 0, 0),
        MaxWalletOutOfBounds,
    );
    refused(
        &mut w,
        0,
        rules(0, 0, 0, 0, 0, 90 * DAY + 1, 0, 0),
        CreatorLockTooLong,
    );
    refused(&mut w, 0, rules(0, 0, 0, 0, 0, 0, 0, 900), InvalidEarlyLock);
    refused(
        &mut w,
        0,
        rules(0, 0, 0, 0, 0, 0, 301, 900),
        InvalidEarlyLock,
    );
    refused(&mut w, 0, rules(0, 0, 0, 0, 0, 0, 60, 60), InvalidEarlyLock);
    refused(
        &mut w,
        0,
        rules(0, 0, 0, 0, 0, 0, 60, 7 * DAY + 1),
        InvalidEarlyLock,
    );
    refused(&mut w, 201, LaunchRules::NONE, CreatorFeeTooHigh);
    for (fee, r) in [
        (100, rules(200, 0, 0, 0, 0, 0, 0, 0)),
        (0, rules(200, 200, 100, 100, 0, 0, 0, 0)),
        (50, rules(0, 200, 0, 50, 0, 0, 0, 0)),
        (0, rules(0, 0, 0, 0, 100, 0, 0, 0)),
        (0, rules(0, 0, 0, 0, 500, 90 * DAY, 300, 7 * DAY)),
    ] {
        try_rules(&mut w, fee, r).ok();
    }

    // A config may raise the bounds to the ceilings and no further.
    let deployer = w.env.deployer.insecure_clone();
    let admin = deployer.pubkey();
    let base = policy_launch_config(admin, w.env.treasury.pubkey(), w.sol);
    type Change = fn(&mut RuleBounds, &mut u64);
    let set = |w: &mut World, change: Change| -> Tx {
        let mut args = base.clone();
        change(&mut args.rule_bounds, &mut args.supply);
        w.env
            .send_paid_by(&[launch::set_config(admin, args)], &deployer, &[])
    };
    let invalid = lcode(InvalidConfig);
    let over: [(&str, Change); 11] = [
        ("holder", |b, _| b.max_holder_fee_bps = 501),
        ("burn", |b, _| b.max_burn_bps = 501),
        ("rules", |b, _| b.max_rules_fee_bps = 1_001),
        ("min wallet 0", |b, _| b.min_max_wallet_bps = 0),
        ("max wallet", |b, _| b.max_max_wallet_bps = 10_000),
        ("min above max", |b, _| {
            b.min_max_wallet_bps = b.max_max_wallet_bps + 1
        }),
        ("creator lock", |b, _| {
            b.max_creator_lock_secs = 365 * DAY + 1
        }),
        ("early window", |b, _| b.max_early_window_secs = 3_601),
        ("early lock", |b, _| b.max_early_lock_secs = 30 * DAY + 1),
        ("supply 999", |_, s| *s = 999),
        ("supply above 1e16", |_, s| *s = ceilings::MAX_SUPPLY + 1),
    ];
    for (what, change) in over {
        let tx = set(&mut w, change);
        assert_eq!(tx.custom(), invalid, "{what}");
    }
    // At the ceilings it is taken, and a launch can then use them.
    set(&mut w, |b, _| {
        *b = RuleBounds {
            max_holder_fee_bps: ceilings::HOLDER_FEE_BPS,
            max_burn_bps: ceilings::BURN_BPS,
            max_rules_fee_bps: ceilings::RULES_FEE_BPS,
            min_max_wallet_bps: ceilings::MIN_MAX_WALLET_BPS,
            max_max_wallet_bps: ceilings::MAX_WALLET_BPS,
            max_creator_lock_secs: ceilings::CREATOR_LOCK_SECS,
            max_early_window_secs: ceilings::EARLY_WINDOW_SECS,
            max_early_lock_secs: ceilings::EARLY_LOCK_SECS,
        }
    })
    .ok();
    let (_, raised, _) = launch_with(
        &mut w,
        "RSD",
        0,
        rules(500, 500, 500, 500, 9_999, 365 * DAY, 3_600, 30 * DAY),
    );
    assert_eq!(w.env.kit_config(&raised).modules, modules::ALL);
    let tx = try_rules(&mut w, 1, rules(500, 0, 500, 0, 0, 0, 0, 0));
    expect_err(&tx, lcode(RulesFeeTooHigh), "RulesFeeTooHigh");
    // A max wallet that rounds to nothing at the supply is refused.
    set(&mut w, |b, s| {
        *s = 9_999;
        b.min_max_wallet_bps = 1;
    })
    .ok();
    let tx = try_rules(&mut w, 0, rules(0, 0, 0, 0, 1, 0, 0, 0));
    expect_err(&tx, lcode(MaxWalletOutOfBounds), "MaxWalletOutOfBounds");
    set(&mut w, |_, _| {}).ok();
    assert_eq!(
        w.env
            .read::<bordrless_launch::state::Config>(&launch::config_address())
            .rule_bounds,
        policy_rule_bounds()
    );

    // init_config holds the same ceilings: a fresh deployment.
    let mut env = Env::new();
    let deployer = env.deployer.insecure_clone();
    let init = |env: &mut Env, args: &bordrless_launch::instructions::ConfigArgs| -> Tx {
        env.send_paid_by(
            &[launch::init_config(deployer.pubkey(), args.clone())],
            &deployer,
            &[],
        )
    };
    let mut args = policy_launch_config(deployer.pubkey(), env.treasury.pubkey(), w.sol);
    args.supply = 999;
    init(&mut env, &args).expect_code(invalid);
    args.supply = 1_000;
    args.rule_bounds.max_burn_bps = 501;
    init(&mut env, &args).expect_code(invalid);
    args.rule_bounds.max_burn_bps = 100;
    init(&mut env, &args).ok();
}

#[test]
fn a_burn_alone_runs_in_the_pool_hook_and_not_in_a_second_pool() {
    let mut w = World::new();
    let r = rules(0, 0, 100, 100, 0, 0, 0, 0);
    let (_, mint, _) = launch_with(&mut w, "BURN", 100, r);
    w.env.warp(policy::SNIPER_WINDOW_SECS);
    let maker = trader(&mut w, &mint, 50 * SOL);
    let supply0 = supply(&w, &mint);
    let (q, tx) = checked_swap(&mut w, &mint, &maker, true, 5 * SOL);
    assert!(q.burn > 0);
    let ev: Swapped = tx.event();
    assert_eq!((ev.burn_in, ev.burn_out), (0, q.burn));
    assert_eq!(supply(&w, &mint), supply0 - q.burn);
    let held = w.env.holding(&mint, &maker.pubkey());
    let (q2, _) = checked_swap(&mut w, &mint, &maker, false, held / 10);
    assert_eq!(q2.burn, held / 10 / 100);
    assert_eq!(supply(&w, &mint), supply0 - q.burn - q2.burn);
    assert_eq!(w.launch(&mint).burned_on_trades, q.burn + q2.burn);
    // A second pool for the token: it has no hook, so nothing burns there.
    let held = w.env.holding(&mint, &maker.pubkey());
    let spec = NewPool {
        base: mint,
        quote: w.sol,
        lp_fee_bps: 25,
        tester_flags: None,
        extras: vec![],
        base_amount: held / 2,
        quote_amount: SOL,
    };
    let (pool2, tx) = w.new_pool(&maker, &spec);
    tx.ok();
    let other = trader(&mut w, &mint, 5 * SOL);
    let supply1 = supply(&w, &mint);
    for (direction, amount) in [(1u8, SOL / 2), (0u8, 1_000 * TOKEN)] {
        let ix = w
            .env
            .swap_ix(&SwapSpec::new(other.pubkey(), pool2, direction, amount));
        let tx = w.env.send_paid_by(&[ix], &other, &[]);
        tx.ok();
        let ev: Swapped = tx.event();
        assert_eq!((ev.burn_in, ev.burn_out), (0, 0));
        assert!(ev.deltas_in.is_empty() && ev.deltas_out.is_empty());
    }
    assert_eq!(supply(&w, &mint), supply1);
}

#[test]
fn a_max_wallet_launchs_second_pool_is_capped_until_graduation() {
    let mut w = World::new();
    let r = rules(0, 0, 0, 0, 500, 0, 0, 0);
    let (_, mint, _) = launch_with(&mut w, "MAXW", 100, r);
    w.env.warp(policy::SNIPER_WINDOW_SECS);
    let cap = w.env.kit_config(&mint).max_wallet_amount;
    let maker = trader(&mut w, &mint, 100 * SOL);
    let seller = trader(&mut w, &mint, 100 * SOL);
    for t in [&maker, &seller] {
        let amount = w.max_buy_for(&mint, &t.pubkey());
        checked_swap(&mut w, &mint, t, true, amount);
    }
    let held = w.env.holding(&mint, &maker.pubkey());
    let deposit = cap - 10 * TOKEN;
    assert!(held >= deposit, "{held} {deposit}");
    let spec = NewPool {
        base: mint,
        quote: w.sol,
        lp_fee_bps: 25,
        tester_flags: None,
        extras: vec![],
        base_amount: deposit,
        quote_amount: SOL,
    };
    let (pool2, tx) = w.new_pool(&maker, &spec);
    tx.ok();
    let sell2 = |w: &mut World, amount: u64| {
        let ix = w
            .env
            .swap_ix(&SwapSpec::new(seller.pubkey(), pool2, 0, amount));
        w.env.send_paid_by(&[ix], &seller, &[])
    };
    expect_err(
        &sell2(&mut w, 10 * TOKEN + 1),
        kcode(KitError::MaxWalletExceeded),
        "MaxWalletExceeded",
    );
    sell2(&mut w, 10 * TOKEN).ok();
    assert_eq!(w.env.holding(&mint, &pool2), cap);
    // The real graduation lifts the cap for that pool too.
    let (_, tx) = w.graduate_launch(&mint);
    tx.ok();
    assert!(w.env.kit_config(&mint).graduated);
    sell2(&mut w, 5 * TOKEN).ok();
    assert_eq!(w.env.holding(&mint, &pool2), cap + 5 * TOKEN);
}

#[test]
fn protocol_fees_from_a_kit_token_pool_go_to_the_collector_of_the_day() {
    let mut w = World::new();
    let (_, mint, _) = launch_with(&mut w, "PFEE", 50, presets::every_rule());
    w.env.warp(3_601);
    let sol = w.sol;
    let pool = w.launch_pool_key(&mint);
    let deployer = w.env.deployer.insecure_clone();
    let admin = deployer.pubkey();
    let traders: Vec<Keypair> = (0..3).map(|_| trader(&mut w, &mint, 20 * SOL)).collect();
    // Buys and sells: the protocol fee is in SOL on both sides, a share of the creator and holder
    // fees, from a buy's input and a sell's output.
    let trade = |w: &mut World, t: &Keypair, lamports: u64| -> u64 {
        let (q, _) = checked_swap(w, &mint, t, true, lamports);
        let held = w.env.holding(&mint, &t.pubkey());
        let (s, _) = checked_swap(w, &mint, t, false, held / 2);
        assert!(s.protocol_fee > 0);
        q.protocol_fee + s.protocol_fee
    };
    let mut fees = 0;
    for (i, t) in traders.iter().enumerate() {
        // Each buy stays within max wallet (5%, about 1.3 SOL at the opening price).
        fees += trade(&mut w, t, (i as u64 + 1) * SOL / 3);
        let p = w.launch_pool(&mint);
        assert_eq!(p.protocol_fees_quote, fees);
        assert_eq!(w.env.holding(&sol, &pool), p.quote_reserve + fees);
        assert_eq!(w.env.holding(&mint, &pool), p.base_reserve);
    }
    // The admin collects into the fee collector's SOL holding: no launched token moves, so the
    // collector never holds one.
    w.holdings(&deployer, sol, &[admin]);
    let before = w.env.holding(&sol, &admin);
    let tx = w
        .env
        .send_paid_by(&[w.env.collect_ix(&admin, &pool, &admin)], &deployer, &[]);
    tx.ok();
    let ev: ProtocolFeesCollected = tx.event();
    assert_eq!(
        (ev.pool, ev.quote_amount, ev.collector),
        (pool, fees, admin)
    );
    assert_eq!(w.env.holding(&sol, &admin), before + fees);
    assert_eq!(w.env.holding(&mint, &admin), 0);
    let p = w.launch_pool(&mint);
    assert_eq!(
        (p.protocol_fees_quote, w.env.holding(&sol, &pool)),
        (0, p.quote_reserve)
    );
    // Another fee collector: what accrues from now on goes to it; the old one is refused.
    let next = Pubkey::new_unique();
    w.holdings(&deployer, sol, &[next]);
    let config = bordrless_swap::instructions::ConfigArgs {
        admin,
        protocol_fee_bps: policy::PROTOCOL_FEE_BPS,
        launch_protocol_share_bps: policy::LAUNCH_PROTOCOL_SHARE_BPS,
        fee_collector: next,
        treasury: w.env.treasury.pubkey(),
        pool_creation_fee_lamports: 0,
        paused: false,
    };
    w.env
        .send_paid_by(&[swap::set_config(admin, config)], &deployer, &[])
        .ok();
    let more = trade(&mut w, &traders[0], SOL);
    expect_err(
        &w.env
            .send_paid_by(&[w.env.collect_ix(&admin, &pool, &admin)], &deployer, &[]),
        scode(SwapError::WrongHolding),
        "WrongHolding",
    );
    let tx = w
        .env
        .send_paid_by(&[w.env.collect_ix(&admin, &pool, &next)], &deployer, &[]);
    tx.ok();
    assert_eq!(tx.event::<ProtocolFeesCollected>().quote_amount, more);
    assert_eq!(w.env.holding(&sol, &next), more);
    assert_eq!(w.env.holding(&mint, &next), 0);
}
