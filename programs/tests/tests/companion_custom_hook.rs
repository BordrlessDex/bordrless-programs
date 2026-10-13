//! A custom token hook beside a companion (2026-10-12): any companion launch (a game's must be its
//! game's) may take a custom hook. The site offers only hooks Bordrless approved (off chain: the
//! launchpad itself takes only a hook that Bordrless or nobody can upgrade). Every step that moves
//! the token carries the hook: the dev buy, the buyback and its burn, the release, and a Snowball's
//! buys and sells. The hook here is `tax_hook`: 1% of every transfer to a collector, taken from what
//! the destination receives, so every amount the companion moves is measured, never assumed.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::InstructionData;
use bordrless_companion::client as companion;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::{
    BoughtBack, BuybackWaited, PotFunded, SnowballFollowed, SnowballTraded,
};
use bordrless_companion::instructions::{CreateArgs, CreateSnowballArgs};
use bordrless_companion::state::{spot_price, Companion, Snowball, Split};
use bordrless_core::policy;
use bordrless_hook::hook_accounts_address;
use bordrless_launch::client::{self as launch, CustomHookAccounts, LaunchKeys};
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{Tx, SYSTEM_PROGRAM_ID};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_token::client as token;
use bordrless_token::state::Mint;
use solana_keypair::Keypair;
use solana_signer::Signer;

const CREATOR_FEE: u16 = 200;
/// tax_hook's fee: 1% of every transfer, to the collector.
const TAX_BPS: u16 = 100;

fn code(e: CompanionError) -> u32 {
    u32::from(e)
}

/// tax_hook prepared for `mint` (which does not exist yet): `TAX_BPS` of transfers to `collector`.
fn prepare_tax(w: &mut World, payer: &Keypair, mint: Pubkey, collector: Pubkey) {
    let tax = Pubkey::find_program_address(&[tax_hook::TAX_SEED, mint.as_ref()], &tax_hook::ID).0;
    let ix = Instruction {
        program_id: tax_hook::ID,
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(collector, false),
            AccountMeta::new(tax, false),
            AccountMeta::new(hook_accounts_address(&tax_hook::ID, &mint).0, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data: tax_hook::instruction::Prepare {
            fee_bps: TAX_BPS,
            max_wallet_bps: 0,
        }
        .data(),
    };
    w.env.send_paid_by(&[ix], payer, &[]).ok();
}

/// A launch config naming tax_hook (no kit rules: a custom hook excludes them).
fn tax_config(w: &mut World, creator: &Keypair) -> Pubkey {
    let (config, tx) = w.create_config(
        creator,
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: CREATOR_FEE,
            custom_hook: Some(tax_hook::ID),
            custom_hook_flags: tax_hook::FLAGS,
            label: "Taxed".to_string(),
        },
    );
    tx.ok();
    config
}

/// The companion's launch of `mint` from `config`, the hook's accounts resolved from its registry.
fn launch_ix(w: &World, launcher: &Pubkey, mint: &Pubkey, config: &Pubkey) -> Instruction {
    let c = w.launch_config(config);
    let custom = c.custom_hook.map(|h| w.custom_hook_accounts(&h, mint));
    let args = World::launch_args("TAXD", c.creator_fee_bps, VQ, c.rules);
    let inner = launch::create_launch_with(
        companion::creator_address(mint),
        *mint,
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        args.clone(),
        Some(*config),
        custom.as_ref(),
    );
    companion::launch(*launcher, *mint, &inner, args)
}

/// A companion made with `ixs_after` (beside `create`), tax_hook prepared, and the launch from a
/// tax config; then the collector's holding, so every transfer pays the tax. Answers the mint, the
/// collector and the launch's transaction.
fn launch_taxed(
    w: &mut World,
    launcher: &Keypair,
    args: CreateArgs,
    ixs_after: impl FnOnce(&Pubkey) -> Vec<Instruction>,
) -> (Pubkey, Pubkey, Tx) {
    let config = tax_config(w, launcher);
    let mint = Keypair::new();
    let mut ixs = vec![companion::create(
        launcher.pubkey(),
        launcher.pubkey(),
        mint.pubkey(),
        args,
    )];
    ixs.extend(ixs_after(&mint.pubkey()));
    w.env.send_paid_by(&ixs, launcher, &[&mint]).ok();
    let collector = Keypair::new().pubkey();
    prepare_tax(w, launcher, mint.pubkey(), collector);
    let ix = launch_ix(w, &launcher.pubkey(), &mint.pubkey(), &config);
    let size = packet_size(w, &ix, launcher, &[&mint]);
    assert!(size <= 1_232, "the launch: {size} bytes");
    let tx = w.env.send_paid_by(&[ix], launcher, &[&mint]);
    if tx.result.is_ok() {
        let payer = w.env.payer.pubkey();
        w.env
            .send(
                &[token::create_holding(payer, mint.pubkey(), collector)],
                &[],
            )
            .ok();
    }
    (mint.pubkey(), collector, tx)
}

fn create_args(split: Split) -> CreateArgs {
    CreateArgs {
        split,
        bounty_bps: 50,
        max_buyback: SOL,
        buyback_interval: 60,
        vest_secs: 0,
        fund: SOL / 2,
    }
}

fn keys(w: &World, mint: &Pubkey) -> LaunchKeys {
    LaunchKeys::of(&w.launch(mint))
}

/// The size of `ix` as the site and keepers send it: v0 with the protocol's 22-address lookup table
/// and a compute budget (limit and price).
fn packet_size(w: &mut World, ix: &Instruction, payer: &Keypair, signers: &[&Keypair]) -> usize {
    use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price};
    let mut table = protocol_lookup_table(w);
    table.extend([
        companion::event_authority(),
        bordrless_launch::ID,
        bordrless_swap::ID,
        bordrless_token::ID,
    ]);
    let lut = w.env.put_lookup_table(Pubkey::new_unique(), &table);
    let ixs = [
        compute_unit_limit(1_400_000),
        compute_unit_price(20_000),
        ix.clone(),
    ];
    w.env.v0_size(&ixs, payer, signers, &[lut])
}

fn custom(w: &World, mint: &Pubkey) -> CustomHookAccounts {
    w.custom_hook_accounts(&tax_hook::ID, mint)
}

fn companion_of(w: &World, mint: &Pubkey) -> Companion {
    w.env.read(&companion::companion_address(mint))
}

fn spot(w: &World, mint: &Pubkey) -> u128 {
    let p = w.launch_pool(mint);
    spot_price(
        p.quote_reserve,
        p.virtual_quote,
        p.base_reserve,
        p.virtual_base,
    )
    .unwrap()
}

/// Trades enough to accrue creator fees: buys and sells by a few wallets (through the hook).
fn trade(w: &mut World, mint: &Pubkey, wallets: usize, sol: u64) {
    for _ in 0..wallets {
        let t = w.wallet_with_sol(sol + SOL);
        w.buy(&t, mint, sol).ok();
        let held = w.env.holding(mint, &t.pubkey());
        w.sell(&t, mint, held / 4).ok();
    }
}

/// Buybacks that wait while the price is above the reference, until the next one would buy.
fn catch_up(w: &mut World, cranker: &Keypair, mint: &Pubkey) {
    for _ in 0..80 {
        let c = companion_of(w, mint);
        if spot(w, mint) <= c.reference_price * 10_300 / 10_000 {
            return;
        }
        w.env.warp(61);
        let ix = companion::buyback_with(
            cranker.pubkey(),
            &keys(w, mint),
            false,
            Some(&custom(w, mint)),
        );
        let tx = w.env.send_paid_by(&[ix], cranker, &[]);
        tx.ok();
        assert_eq!(tx.events::<BuybackWaited>().len(), 1);
    }
    panic!("the reference never caught up");
}

/// A classic companion (the whole split to the buyback) launches with a custom hook; its dev buy,
/// its buyback and burn and its release all carry the hook, each measuring what the hook's tax
/// left it.
#[test]
fn a_classic_companion_launches_with_a_custom_hook_and_buys_back_through_it() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(30 * SOL);
    let split = Split {
        buyback_bps: 10_000,
        holders_bps: 0,
        beneficiary_bps: 0,
    };
    let (mint, collector, tx) = launch_taxed(&mut w, &launcher, create_args(split), |_| vec![]);
    tx.ok();
    println!(
        "companion launch with a custom hook: CU {} size {} height {}",
        tx.cu(),
        tx.size,
        tx.max_height()
    );
    let creator = companion::creator_address(&mint);
    let l = w.launch(&mint);
    assert_eq!(
        (l.custom_hook, l.creator, l.modules),
        (Some(tax_hook::ID), creator, 0)
    );
    assert!(companion_of(&w, &mint).launched);
    let k = keys(&w, &mint);
    let hook = custom(&w, &mint);

    // The dev buy: through the hook, the bag what arrived after the tax.
    w.env
        .send_paid_by(
            &[companion::dev_buy_with(
                launcher.pubkey(),
                &k,
                SOL / 10,
                1,
                Some(&hook),
            )],
            &launcher,
            &[],
        )
        .ok();
    let bag = companion_of(&w, &mint).dev_tokens;
    assert!(bag > 0);
    assert_eq!(w.env.holding(&mint, &creator), bag);
    let taxed = w.env.holding(&mint, &collector);
    assert!(taxed > 0, "the dev buy paid the hook's tax");

    // Fees, then a claim: all of it to the buyback.
    w.env.warp(31);
    trade(&mut w, &mint, 4, 2 * SOL);
    let cranker = w.wallet_with_sol(SOL);
    w.env
        .send_paid_by(
            &[companion::claim_fees(cranker.pubkey(), mint, None)],
            &cranker,
            &[],
        )
        .ok();
    assert!(companion_of(&w, &mint).pending_buyback > 0);

    // The buyback: bought through the hook (the tax taken on the way), then burned.
    w.env.warp(60);
    catch_up(&mut w, &cranker, &mint);
    let supply = w.env.read::<Mint>(&mint).supply;
    let taxed = w.env.holding(&mint, &collector);
    let ix = companion::buyback_with(cranker.pubkey(), &k, false, Some(&hook));
    let size = packet_size(&mut w, &ix, &cranker, &[]);
    let tx = w.env.send_paid_by(&[ix], &cranker, &[]);
    tx.ok();
    println!(
        "buyback with a custom hook: CU {} size {} ({size} v0 with the protocol table) height {}",
        tx.cu(),
        tx.size,
        tx.max_height()
    );
    assert!(size <= 1_232, "a buyback: {size} bytes");
    assert!(tx.max_height() <= 5);
    let ev: BoughtBack = tx.event();
    assert!(ev.burned > 0);
    assert_eq!(w.env.read::<Mint>(&mint).supply, supply - ev.burned);
    assert!(
        w.env.holding(&mint, &collector) > taxed,
        "the buy paid the tax"
    );
    // Only the bag is left in the creator's holding: what it bought it burned, whatever arrived.
    assert_eq!(w.env.holding(&mint, &creator), bag);
    assert_eq!(companion_of(&w, &mint).burned_total, ev.burned);

    // The release: the bag to the launcher through the hook.
    let taxed = w.env.holding(&mint, &collector);
    w.env
        .send_paid_by(
            &[companion::release_with(
                cranker.pubkey(),
                &k,
                false,
                launcher.pubkey(),
                Some(&hook),
            )],
            &cranker,
            &[],
        )
        .ok();
    // The hook's tax (1%, rounded up) is taken from what the launcher receives.
    let tax = w.env.holding(&mint, &collector) - taxed;
    assert_eq!(tax, (bag * u64::from(TAX_BPS)).div_ceil(10_000));
    assert_eq!(w.env.holding(&mint, &launcher.pubkey()), bag - tax);
    assert_eq!(w.env.holding(&mint, &creator), 0);
    assert_eq!(companion_of(&w, &mint).dev_released, bag);
}

/// A custom hook stays impossible beside a holders' share: the hook excludes kit rules, and the
/// holders are paid through the kit.
#[test]
fn a_custom_hook_never_comes_with_a_holders_share() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(30 * SOL);
    let split = Split {
        buyback_bps: 5_000,
        holders_bps: 5_000,
        beneficiary_bps: 0,
    };
    let (_, _, tx) = launch_taxed(&mut w, &launcher, create_args(split), |_| vec![]);
    tx.expect_code(code(CompanionError::HolderRewardsOff));
}

fn snowball_terms() -> CreateSnowballArgs {
    CreateSnowballArgs {
        split: Split {
            buyback_bps: 0,
            holders_bps: 0,
            beneficiary_bps: 3_000,
        },
        pot_bps: 7_000,
        buy_step_bps: 200,
        sell_step_bps: 200,
        buy_bps: 2_500,
        sell_bps: 2_500,
        interval: 60,
        max_trade: 100 * SOL,
        pool_bps: 500,
        flat_trades: false,
    }
}

/// A Snowball with a custom hook follows, buys on a dip and sells on a rise, every trade through
/// the hook: its buys keep what arrived after the tax, its sells resolve the hook's accounts for
/// the creator's transfer to the pool.
#[test]
fn a_snowball_with_a_custom_hook_buys_and_sells() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(30 * SOL);
    let split = Split {
        buyback_bps: 0,
        holders_bps: 0,
        beneficiary_bps: 10_000,
    };
    let (mint, collector, tx) = launch_taxed(&mut w, &launcher, create_args(split), |mint| {
        vec![companion::create_snowball(
            launcher.pubkey(),
            *mint,
            snowball_terms(),
        )]
    });
    tx.ok();
    let creator = companion::creator_address(&mint);
    assert!(companion_of(&w, &mint).is_snowball());
    assert_eq!(w.launch(&mint).custom_hook, Some(tax_hook::ID));
    let k = keys(&w, &mint);
    let hook = custom(&w, &mint);
    let step = |w: &mut World, cranker: &Keypair| {
        let ix = companion::snowball_step_with(cranker.pubkey(), &k, Some(&hook), None);
        w.env.send_paid_by(&[ix], cranker, &[])
    };

    // Fees into the pot.
    w.env.warp(31);
    trade(&mut w, &mint, 4, 2 * SOL);
    let whale = w.wallet_with_sol(30 * SOL);
    w.buy(&whale, &mint, 5 * SOL).ok();
    let cranker = w.wallet_with_sol(SOL);
    let tx = w.env.send_paid_by(
        &[companion::claim_fees(cranker.pubkey(), mint, None)],
        &cranker,
        &[],
    );
    tx.ok();
    let funded: PotFunded = tx.event();
    assert!(funded.to_pot > 0);
    w.env.warp(60);

    // Above the opening price with nothing to sell: it follows.
    let tx = step(&mut w, &cranker);
    tx.ok();
    let _: SnowballFollowed = tx.event();

    // A dump: it buys, through the hook.
    let held = w.env.holding(&mint, &whale.pubkey());
    w.sell(&whale, &mint, held / 2).ok();
    w.env.warp(60);
    let taxed = w.env.holding(&mint, &collector);
    let tx = step(&mut w, &cranker);
    tx.ok();
    println!(
        "snowball buy with a custom hook: CU {} size {} height {}",
        tx.cu(),
        tx.size,
        tx.max_height()
    );
    let buy: SnowballTraded = tx.event();
    assert_eq!((buy.side, buy.on_step), (1, true));
    assert!(buy.tokens > 0);
    assert!(
        w.env.holding(&mint, &collector) > taxed,
        "the buy paid the tax"
    );
    let s: Snowball = w.env.read(&companion::snowball_address(&mint));
    assert_eq!(s.tokens_held, buy.tokens);
    assert_eq!(w.env.holding(&mint, &creator), buy.tokens);

    // A pump: it sells, through the hook (resolved for the creator's transfer to the pool).
    w.buy(&whale, &mint, 3 * SOL).ok();
    w.env.warp(60);
    let pot = companion_of(&w, &mint).pending_pot;
    let size = packet_size(
        &mut w,
        &companion::snowball_step_with(cranker.pubkey(), &k, Some(&hook), None),
        &cranker,
        &[],
    );
    let tx = step(&mut w, &cranker);
    tx.ok();
    println!(
        "snowball sell with a custom hook: CU {} size {} ({size} v0 with the protocol table) height {}",
        tx.cu(),
        tx.size,
        tx.max_height()
    );
    assert!(tx.max_height() <= 5);
    assert!(size <= 1_232, "a Snowball step: {size} bytes");
    let sell: SnowballTraded = tx.event();
    assert_eq!((sell.side, sell.on_step), (0, true));
    assert!(sell.sol > 0);
    assert_eq!(sell.tokens, buy.tokens * 2_500 / 10_000);
    let s: Snowball = w.env.read(&companion::snowball_address(&mint));
    assert_eq!(s.tokens_held, buy.tokens - sell.tokens);
    assert_eq!(w.env.holding(&mint, &creator), s.tokens_held);
    assert_eq!(companion_of(&w, &mint).pending_pot, pot + sell.sol);
    assert_eq!((s.buys, s.sells), (1, 1));

    // Without the hook's registry and extras a step can't resolve them: refused, nothing moved.
    w.buy(&whale, &mint, 3 * SOL).ok();
    w.env.warp(60);
    let mut bare = companion::snowball_step_with(cranker.pubkey(), &k, Some(&hook), None);
    let registry = hook_accounts_address(&tax_hook::ID, &mint).0;
    bare.accounts.retain(|m| m.pubkey != registry);
    w.env
        .send_paid_by(&[bare], &cranker, &[])
        .expect_code(code(CompanionError::MissingAccount));
}
