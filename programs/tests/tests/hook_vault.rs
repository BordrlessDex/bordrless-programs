//! `hook_vault` (`docs/phase3a.md` §5): a launchpad coin's token hook sends its cuts to slots whose
//! policies were fixed before the launch, and anyone runs them under the companion's buyback guards,
//! mirrored for sells.
//!
//! The coin's hook is `tax_hook`, its collector slot 0's owner: every transfer pays 1% into slot 0's
//! holding once `open_vault` has created it (tax_hook takes nothing while its collector's holding is
//! missing, and exempts the collector on both sides, so slot 0's own sells pay no tax). Coins are
//! launched from a `LaunchConfig` naming tax_hook.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::InstructionData;
use bordrless_core::policy as core_policy;
use bordrless_launch::client::{self as launch, LaunchKeys};
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{compute_unit_limit, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::hooks::NewPool;
use bordrless_program_tests::launch::{protocol_lookup_table, SOL, VQ};
use bordrless_program_tests::vault::*;
use bordrless_swap::state::Pool;
use bordrless_token::client as token;
use bordrless_token::state::Mint;
use hook_vault::client as vault_client;
use hook_vault::constants::*;
use hook_vault::error::VaultError;
use hook_vault::events::*;
use hook_vault::instructions::{CreateVaultArgs, SlotArgs};
use solana_keypair::Keypair;
use solana_signer::Signer;

/// A transaction's most bytes.
const PACKET: usize = 1_232;
/// The runtime's fee for one signature.
const FEE: u64 = 5_000;

fn code(e: VaultError) -> u32 {
    u32::from(e)
}

fn wealth(w: &World, who: &Pubkey) -> i128 {
    w.env.holding(&w.sol, who) as i128 + w.env.lamports(who) as i128
}

fn supply(w: &World, mint: &Pubkey) -> u64 {
    w.env.read::<Mint>(mint).supply
}

/// A plain launch of another token to buy and burn, with kit rules (max wallet 2%, so its hook is
/// the kit). Answers its mint and pool.
fn other_token(w: &mut World, rules: LaunchRules) -> (Pubkey, Pubkey) {
    let creator = w.wallet_with_sol(5 * SOL);
    let (x, tx) = w.create_launch_with(&creator, "XX", 100, VQ, rules);
    tx.ok();
    (x, w.launch_pool_key(&x))
}

fn max_wallet(bps: u16) -> LaunchRules {
    LaunchRules {
        max_wallet_bps: bps,
        ..LaunchRules::NONE
    }
}

/// `create_vault` for a fresh mint, paid by `payer`.
fn try_create(w: &mut World, payer: &Keypair, args: CreateVaultArgs, buy_mints: &[Pubkey]) -> Tx {
    let mint = Keypair::new();
    let ix = vault_client::create_vault(payer.pubkey(), mint.pubkey(), args, buy_mints);
    w.env.send_paid_by(&[ix], payer, &[&mint])
}

/// Coin bought by a fresh wallet, half of it sent to slot `i`'s holding (tax_hook takes its fee on
/// the way unless the slot is its collector). Answers what was sent.
fn fund_slot(w: &mut World, mint: &Pubkey, i: u8, lamports: u64) -> u64 {
    let h = w.wallet_with_sol(lamports + SOL);
    w.buy(&h, mint, lamports).ok();
    let got = w.env.holding(mint, &h.pubkey()) / 2;
    w.send_tokens(&h, *mint, &slot_owner(mint, i), got).ok();
    got
}

// ---- create_vault ----------------------------------------------------------------------------------

#[test]
fn only_the_mints_keypair_makes_its_vault_once() {
    let mut w = World::new();
    let payer = w.wallet_with_sol(5 * SOL);
    let mint = Keypair::new();
    let mut ix =
        vault_client::create_vault(payer.pubkey(), mint.pubkey(), vault_args(vec![burn()]), &[]);
    assert_eq!(ix.accounts[1].pubkey, mint.pubkey());
    ix.accounts[1].is_signer = false;
    // Anchor's AccountNotSigner.
    w.env.send_paid_by(&[ix], &payer, &[]).expect_code(3010);
    let ix =
        vault_client::create_vault(payer.pubkey(), mint.pubkey(), vault_args(vec![burn()]), &[]);
    let tx = w
        .env
        .send_paid_by(std::slice::from_ref(&ix), &payer, &[&mint]);
    let ev = tx.event::<VaultCreated>();
    assert_eq!(ev.slot_owners, vec![slot_owner(&mint.pubkey(), 0)]);
    let v = w.vault(&mint.pubkey());
    assert_eq!(
        (v.mint, v.hook, v.creator, v.n_slots),
        (mint.pubkey(), tax_hook::ID, payer.pubkey(), 1)
    );
    assert!(!v.opened);
    // There is one vault a mint, made once: its policies can't be replaced by making it again.
    w.env.warp(1);
    w.env.send_paid_by(&[ix], &payer, &[&mint]).expect_fail();
}

/// A bound's case: its name, how it changes the arguments, and the refusal it expects (none: it
/// lands).
type Case<'a> = (
    &'static str,
    Box<dyn Fn(&mut CreateVaultArgs) + 'a>,
    Option<VaultError>,
);

#[test]
fn create_vault_bounds_each_off_by_one() {
    let mut w = World::new();
    let payer = w.wallet_with_sol(20 * SOL);
    let to = Keypair::new().pubkey();
    let ok = |w: &mut World, f: &dyn Fn(&mut CreateVaultArgs)| {
        let mut a = vault_args(vec![sell_for_sol(to)]);
        f(&mut a);
        try_create(w, &payer, a, &[])
    };
    let cases: Vec<Case<'_>> = vec![
        ("bounty 100", Box::new(|a| a.bounty_bps = 100), None),
        (
            "bounty 101",
            Box::new(|a| a.bounty_bps = 101),
            Some(VaultError::BountyTooHigh),
        ),
        ("max sell 1", Box::new(|a| a.max_sell_bps = 1), None),
        (
            "max sell 0",
            Box::new(|a| a.max_sell_bps = 0),
            Some(VaultError::BadMaxSell),
        ),
        ("max sell 100", Box::new(|a| a.max_sell_bps = 100), None),
        (
            "max sell 101",
            Box::new(|a| a.max_sell_bps = 101),
            Some(VaultError::BadMaxSell),
        ),
        ("interval 60", Box::new(|a| a.interval = 60), None),
        (
            "interval 59",
            Box::new(|a| a.interval = 59),
            Some(VaultError::BadInterval),
        ),
        (
            "interval 30 d",
            Box::new(|a| a.interval = 30 * 86_400),
            None,
        ),
        (
            "interval 30 d + 1",
            Box::new(|a| a.interval = 30 * 86_400 + 1),
            Some(VaultError::BadInterval),
        ),
        (
            "hook cut 5000",
            Box::new(|a| a.max_hook_cut_bps = 5_000),
            None,
        ),
        (
            "hook cut 5001",
            Box::new(|a| a.max_hook_cut_bps = 5_001),
            Some(VaultError::BadHookCut),
        ),
        (
            "hook: the default key",
            Box::new(|a| a.hook = Pubkey::default()),
            Some(VaultError::BadHook),
        ),
        (
            "hook: the token program",
            Box::new(|a| a.hook = bordrless_token::ID),
            Some(VaultError::BadHook),
        ),
        (
            "hook: the DEX",
            Box::new(|a| a.hook = bordrless_swap::ID),
            Some(VaultError::BadHook),
        ),
        (
            "hook: the launchpad",
            Box::new(|a| a.hook = bordrless_launch::ID),
            Some(VaultError::BadHook),
        ),
        (
            "hook: the kit",
            Box::new(|a| a.hook = bordrless_kit::ID),
            Some(VaultError::BadHook),
        ),
        (
            "hook: the vault",
            Box::new(|a| a.hook = hook_vault::ID),
            Some(VaultError::BadHook),
        ),
        (
            "no slot",
            Box::new(|a| a.slots.clear()),
            Some(VaultError::BadSlots),
        ),
        (
            "three slots",
            Box::new(|a| a.slots = vec![burn(), burn(), sell_for_sol(to)]),
            None,
        ),
        (
            "four slots",
            Box::new(|a| a.slots = vec![burn(); 4]),
            Some(VaultError::BadSlots),
        ),
        (
            "policy 0",
            Box::new(|a| {
                a.slots = vec![SlotArgs {
                    policy: 0,
                    target: Pubkey::default(),
                    max_cut_bps: 0,
                }]
            }),
            Some(VaultError::BadSlots),
        ),
        (
            "policy 4",
            Box::new(|a| {
                a.slots = vec![SlotArgs {
                    policy: 4,
                    target: to,
                    max_cut_bps: 0,
                }]
            }),
            Some(VaultError::BadSlots),
        ),
        (
            "a burn with a target",
            Box::new(|a| {
                a.slots = vec![SlotArgs {
                    policy: policy::BURN,
                    target: to,
                    max_cut_bps: 0,
                }]
            }),
            Some(VaultError::BadTarget),
        ),
        (
            "a sale to nobody",
            Box::new(|a| a.slots = vec![sell_for_sol(Pubkey::default())]),
            Some(VaultError::BadTarget),
        ),
        (
            "a declared buy cut on a sale slot",
            Box::new(|a| a.slots[0].max_cut_bps = 1),
            Some(VaultError::BadHookCut),
        ),
        (
            "a declared buy cut on a burn slot",
            Box::new(|a| {
                a.slots = vec![SlotArgs {
                    max_cut_bps: 1,
                    ..burn()
                }]
            }),
            Some(VaultError::BadHookCut),
        ),
    ];
    for (name, f, want) in cases {
        let tx = ok(&mut w, &*f);
        match want {
            None => {
                tx.ok();
            }
            Some(e) => {
                println!("{name}: refused");
                tx.expect_code(code(e));
            }
        }
    }
    // A sale to one of the vault's own slot owners (refused since round 1 as a wallet that can't
    // take SOL for good: `BadWallet`, before `BadTarget`).
    let mint = Keypair::new();
    let a = vault_args(vec![burn(), sell_for_sol(slot_owner(&mint.pubkey(), 0))]);
    let ix = vault_client::create_vault(payer.pubkey(), mint.pubkey(), a, &[]);
    w.env
        .send_paid_by(&[ix], &payer, &[&mint])
        .expect_code(code(VaultError::BadWallet));
}

/// A wallet chosen for a mint.
type WalletOf = Box<dyn Fn(&Pubkey) -> Pubkey>;

/// Audit 3a vault r1, F8: a `SellForSol` wallet must be able to take SOL for good. Every address of
/// the vault's own (each slot index's owner, used or not, the vault, the mint, the event
/// authority, the program), every reserved key, any executable and any account of a loader or the
/// sysvar program is refused; the wallet must be passed. An empty address, or a wallet that
/// exists, is taken.
#[test]
fn a_sale_wallet_must_be_able_to_take_sol() {
    let mut w = World::new();
    let payer = w.wallet_with_sol(20 * SOL);
    let try_to = |w: &mut World, to: &dyn Fn(&Pubkey) -> Pubkey| {
        let mint = Keypair::new();
        let a = vault_args(vec![burn(), sell_for_sol(to(&mint.pubkey()))]);
        let ix = vault_client::create_vault(payer.pubkey(), mint.pubkey(), a, &[]);
        w.env.send_paid_by(&[ix], &payer, &[&mint])
    };
    let programdata = Pubkey::find_program_address(
        &[tax_hook::ID.as_ref()],
        &bordrless_launch::constants::BPF_LOADER_UPGRADEABLE_ID,
    )
    .0;
    let refused: Vec<(&str, WalletOf)> = vec![
        ("slot 0's owner", Box::new(|m| slot_owner(m, 0))),
        (
            "an unused slot index's owner",
            Box::new(|m| slot_owner(m, 2)),
        ),
        ("the vault", Box::new(vault_client::vault_address)),
        ("the mint", Box::new(|m| *m)),
        (
            "the event authority",
            Box::new(|_| vault_client::event_authority()),
        ),
        ("the vault program", Box::new(|_| hook_vault::ID)),
        ("a program", Box::new(|_| tax_hook::ID)),
        (
            "the clock sysvar",
            Box::new(|_| Pubkey::from_str_const("SysvarC1ock11111111111111111111111111111111")),
        ),
        (
            "the vote program",
            Box::new(|_| Pubkey::from_str_const("Vote111111111111111111111111111111111111111")),
        ),
        (
            "the stake config",
            Box::new(|_| Pubkey::from_str_const("StakeConfig11111111111111111111111111111111")),
        ),
        ("a program's data", Box::new(move |_| programdata)),
    ];
    for (name, to) in &refused {
        println!("a sale to {name}: refused");
        try_to(&mut w, &**to).expect_code(code(VaultError::BadWallet));
    }
    // Taken: a wallet with nothing on it yet, and one that exists.
    try_to(&mut w, &|_| Keypair::new().pubkey()).ok();
    let funded = w.env.funded(SOL).pubkey();
    try_to(&mut w, &|_| funded).ok();
    // The wallet must be passed.
    let mint = Keypair::new();
    let mut ix = vault_client::create_vault(
        payer.pubkey(),
        mint.pubkey(),
        vault_args(vec![sell_for_sol(funded)]),
        &[],
    );
    ix.accounts.retain(|a| a.pubkey != funded);
    w.env
        .send_paid_by(&[ix], &payer, &[&mint])
        .expect_code(code(VaultError::MissingAccount));
}

/// The list of reserved keys the vault refuses is the runtime's (`agave-reserved-account-keys`).
#[test]
fn the_reserved_keys_are_the_runtimes() {
    let mut ours: Vec<Pubkey> = RESERVED_KEYS.to_vec();
    let mut runtime: Vec<Pubkey> =
        agave_reserved_account_keys::ReservedAccountKeys::all_keys_iter()
            .copied()
            .collect();
    ours.sort();
    runtime.sort();
    assert_eq!(ours, runtime);
}

/// Audit 3a vault r1, F7: the vault is made before its coin. Once the mint exists, its keypair can no
/// longer make one.
#[test]
fn a_vault_is_made_only_before_its_mint_exists() {
    let mut w = World::new();
    let c = w.vault_coin(vault_args(vec![burn()]), &[], true);
    // A launched coin without a vault, its keypair at hand.
    let creator = w.wallet_with_sol(20 * SOL);
    let mint = Keypair::new();
    let ix = w.create_launch_ix(
        &creator.pubkey(),
        &mint.pubkey(),
        "NOV",
        100,
        VQ,
        LaunchRules::NONE,
    );
    w.env.send_paid_by(&[ix], &creator, &[&mint]).ok();
    let ix = vault_client::create_vault(
        creator.pubkey(),
        mint.pubkey(),
        vault_args(vec![burn()]),
        &[],
    );
    w.env
        .send_paid_by(&[ix], &creator, &[&mint])
        .expect_code(code(VaultError::MintExists));
    // A funded but empty mint address is still "not created": lamports alone prove nothing.
    let fresh = Keypair::new();
    w.env.fund(fresh.pubkey(), SOL);
    let ix = vault_client::create_vault(
        creator.pubkey(),
        fresh.pubkey(),
        vault_args(vec![burn()]),
        &[],
    );
    w.env.send_paid_by(&[ix], &creator, &[&fresh]).ok();
    assert!(w.vault(&c.mint).opened);
}

#[test]
fn a_buy_slot_takes_only_another_launchpad_tokens_sol_pool() {
    let mut w = World::new();
    let payer = w.wallet_with_sol(20 * SOL);
    let (x, x_pool) = other_token(&mut w, max_wallet(200));
    try_create(
        &mut w,
        &payer,
        vault_args(vec![sell_buy_burn(x_pool)]),
        &[x],
    )
    .ok();
    // Audit 3a vault r1, F1: one buy slot a pool (two would only stack their slices).
    try_create(
        &mut w,
        &payer,
        vault_args(vec![sell_buy_burn(x_pool), burn(), sell_buy_burn(x_pool)]),
        &[x],
    )
    .expect_code(code(VaultError::DuplicatePool));
    // F6: a declared cut only for a token with its own hook (x is a kit token).
    try_create(
        &mut w,
        &payer,
        vault_args(vec![sell_buy_burn_cut(x_pool, 1)]),
        &[x],
    )
    .expect_code(code(VaultError::BadHookCut));
    // A custom-hook token takes one, up to 50%.
    let y = w.vault_coin(vault_args(vec![burn()]), &[], true).mint;
    let y_pool = w.launch_pool_key(&y);
    try_create(
        &mut w,
        &payer,
        vault_args(vec![sell_buy_burn_cut(y_pool, MAX_HOOK_CUT_BPS)]),
        &[y],
    )
    .ok();
    try_create(
        &mut w,
        &payer,
        vault_args(vec![sell_buy_burn_cut(y_pool, MAX_HOOK_CUT_BPS + 1)]),
        &[y],
    )
    .expect_code(code(VaultError::BadHookCut));
    // The pool's launch must be passed.
    try_create(&mut w, &payer, vault_args(vec![sell_buy_burn(x_pool)]), &[])
        .expect_code(code(VaultError::MissingAccount));
    // Not a pool: the launch account itself.
    try_create(
        &mut w,
        &payer,
        vault_args(vec![sell_buy_burn(launch::launch_address(&x))]),
        &[x],
    )
    .expect_code(code(VaultError::BadPool));
    // A token with holder rewards: the kit lets only wallets hold it, and the slot's owner is a
    // program address.
    let rewards = LaunchRules {
        holder_fee_buy_bps: 100,
        holder_fee_sell_bps: 100,
        ..LaunchRules::NONE
    };
    let (y, y_pool) = other_token(&mut w, rewards);
    try_create(
        &mut w,
        &payer,
        vault_args(vec![sell_buy_burn(y_pool)]),
        &[y],
    )
    .expect_code(code(VaultError::BadPool));
    // An ordinary DEX pool (no launchpad hook), quoted in bridged SOL.
    let owner = w.wallet_with_sol(5 * SOL);
    let z = w.mint_to_owner(&owner, 6, 1_000_000_000, "ZZ");
    let (z_pool, tx) = w.new_pool(
        &owner,
        &NewPool {
            base: z,
            quote: w.sol,
            lp_fee_bps: 30,
            tester_flags: None,
            extras: vec![],
            base_amount: 500_000_000,
            quote_amount: SOL,
        },
    );
    tx.ok();
    try_create(
        &mut w,
        &payer,
        vault_args(vec![sell_buy_burn(z_pool)]),
        &[z],
    )
    .expect_code(code(VaultError::BadPool));
}

// ---- open_vault ------------------------------------------------------------------------------------

#[test]
fn open_vault_waits_for_a_launch_with_the_vaults_hook() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(50 * SOL);
    let config = w.tax_hook_config(&creator, 100, LaunchRules::NONE);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let setup = w.vault_setup_ixs(
        &creator.pubkey(),
        &m,
        vault_args(vec![burn()]),
        &[],
        &CoinSpec::default(),
    );
    w.env.send_paid_by(&setup, &creator, &[&mint]).ok();
    // Before the launch.
    let keys = LaunchKeys::new(m, w.sol, core_policy::LP_FEE_BPS, &LaunchRules::NONE);
    let ix = vault_client::open_vault(creator.pubkey(), &w.vault(&m), &keys);
    w.env
        .send_paid_by(&[ix], &creator, &[])
        .expect_code(code(VaultError::LaunchMissing));
    // Nothing to execute before it opens: before the launch the step has no launch to read
    // (Anchor's AccountNotInitialized), after it the vault is not open.
    let cranker = w.wallet_with_sol(SOL);
    let execute = |w: &World| {
        vault_client::execute(
            cranker.pubkey(),
            &w.vault(&m),
            0,
            &keys,
            &w.slot_burn_hook(&m, 0),
        )
    };
    w.env
        .send_paid_by(&[execute(&w)], &cranker, &[])
        .expect_code(3012);
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &m, "VLT", VQ, &config);
    w.env.send_paid_by(&[ix], &creator, &[&mint]).ok();
    w.env
        .send_paid_by(&[execute(&w)], &cranker, &[])
        .expect_code(code(VaultError::NotOpen));
    // Trades before the opening pay the hook nothing: slot 0's holding does not exist.
    w.churn(&m, 1, SOL);
    assert!(w.env.account(&vault_client::slot_holding(&m, 0)).is_none());
    let price = w.coin_price(&m);
    // Nobody but the creator (or the mint's keypair) opens it (X3).
    let stranger = w.wallet_with_sol(SOL);
    w.open_vault(&stranger, &m)
        .expect_code(code(VaultError::NotOpener));
    // A sender with the mint's keypair signing too may (a flow that holds the mint).
    let mut ix = w.open_vault_ix(&stranger.pubkey(), &m);
    ix.accounts[3].is_signer = true;
    let tx = w.env.send_paid_by(&[ix], &stranger, &[&mint]);
    let ev = tx.event::<VaultOpened>();
    // The sells' reference: the price, at most the opening price (the churn raised the price).
    let opening = w.opening_price(&m);
    assert!(price > opening);
    assert_eq!(ev.price, price.min(opening));
    let v = w.vault(&m);
    assert!(v.opened);
    assert_eq!(v.opened_at, w.env.now);
    let holding = w.env.account(&vault_client::slot_holding(&m, 0)).unwrap();
    assert_eq!(holding.owner, bordrless_token::ID);
    // A burn slot holds no SOL: no bridged-SOL holding, no funding.
    assert_eq!(w.env.lamports(&slot_owner(&m, 0)), 0);
    // Once.
    w.env.warp(1);
    w.open_vault(&creator, &m)
        .expect_code(code(VaultError::AlreadyOpen));
    // From now on every trade pays slot 0.
    w.churn(&m, 1, SOL);
    assert!(w.slot_coin(&m, 0) > 0);

    // A vault whose hook is another program than the launch's.
    let mint = Keypair::new();
    let m = mint.pubkey();
    let mut a = vault_args(vec![burn()]);
    a.hook = half_life::ID;
    let setup = w.vault_setup_ixs(&creator.pubkey(), &m, a, &[], &CoinSpec::default());
    w.env.send_paid_by(&setup, &creator, &[&mint]).ok();
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &m, "VLT", VQ, &config);
    w.env.send_paid_by(&[ix], &creator, &[&mint]).ok();
    w.open_vault(&creator, &m)
        .expect_code(code(VaultError::WrongHook));

    // A coin launched with inline rules: no custom hook at all.
    let mint = Keypair::new();
    let m = mint.pubkey();
    let setup = w.vault_setup_ixs(
        &creator.pubkey(),
        &m,
        vault_args(vec![burn()]),
        &[],
        &CoinSpec::default(),
    );
    w.env.send_paid_by(&setup, &creator, &[&mint]).ok();
    let ix = w.create_launch_ix(&creator.pubkey(), &m, "VLT", 100, VQ, LaunchRules::NONE);
    w.env.send_paid_by(&[ix], &creator, &[&mint]).ok();
    w.open_vault(&creator, &m)
        .expect_code(code(VaultError::WrongHook));
}

#[test]
fn the_launch_and_the_opening_fit_one_transaction() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(50 * SOL);
    let config = w.tax_hook_config(&creator, 100, LaunchRules::NONE);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let setup = w.vault_setup_ixs(
        &creator.pubkey(),
        &m,
        vault_args(vec![burn()]),
        &[],
        &CoinSpec::default(),
    );
    w.env.send_paid_by(&setup, &creator, &[&mint]).ok();
    let launch_ix = w.create_launch_from_config_ix(&creator.pubkey(), &m, "VLT", VQ, &config);
    let keys = LaunchKeys {
        custom_hook: Some(tax_hook::ID),
        ..LaunchKeys::new(m, w.sol, core_policy::LP_FEE_BPS, &LaunchRules::NONE)
    };
    let open = vault_client::open_vault(creator.pubkey(), &w.vault(&m), &keys);
    let tx = w.env.send_paid_by(&[launch_ix, open], &creator, &[&mint]);
    tx.ok();
    assert_eq!(
        w.vault(&m).slots[0].reference_price,
        0,
        "a burn slot keeps no reference"
    );
    assert!(w.vault(&m).opened);
}

// ---- execute: burn ---------------------------------------------------------------------------------

#[test]
fn deltas_land_in_slot_zero_and_a_burn_slot_burns_them() {
    let mut w = World::new();
    let c = w.vault_coin(vault_args(vec![burn()]), &[], true);
    let m = c.mint;
    let t = w.wallet_with_sol(3 * SOL);
    w.buy(&t, &m, SOL).ok();
    let bought = w.env.holding(&m, &t.pubkey());
    let cut = w.slot_coin(&m, 0);
    // 1% of what left the pool's vault (rounded up), the trader got the rest.
    assert_eq!(cut, (bought + cut).div_ceil(100));
    w.sell(&t, &m, bought).ok();
    let held = w.slot_coin(&m, 0);
    assert_eq!(held - cut, bought.div_ceil(100));
    let cranker = w.wallet_with_sol(SOL);
    let before = supply(&w, &m);
    let lamports = w.env.lamports(&cranker.pubkey());
    let tx = w.execute(&cranker, &m, 0);
    let ev = tx.event::<SlotBurned>();
    assert_eq!(ev.amount, held);
    assert_eq!(supply(&w, &m), before - held);
    assert_eq!(w.slot_coin(&m, 0), 0);
    // No bounty: the cranker only paid the fee.
    assert_eq!(w.env.lamports(&cranker.pubkey()), lamports - FEE);
    let v = w.vault(&m);
    assert_eq!((v.slots[0].burned, v.slots[0].last_at), (held, w.env.now));
    w.env.warp(1);
    w.execute(&cranker, &m, 0)
        .expect_code(code(VaultError::NothingToDo));
    // No interval for a burn: the next cut burns at once.
    w.churn(&m, 1, SOL);
    w.execute(&cranker, &m, 0).ok();
    assert_eq!(w.slot_coin(&m, 0), 0);
}

// ---- execute: sell for SOL -------------------------------------------------------------------------

/// A coin whose slot 0 sells for SOL to a funded wallet, its slot full of a few round trips' cuts.
fn sale(open_trades: usize) -> (World, VaultCoin, Keypair) {
    let mut w = World::new();
    let to = w.env.funded(SOL);
    let c = w.vault_coin(vault_args(vec![sell_for_sol(to.pubkey())]), &[], true);
    w.churn(&c.mint, open_trades, 2 * SOL);
    (w, c, to)
}

#[test]
fn a_sale_pays_its_wallet_in_sol_and_the_crank_its_bounty() {
    let (mut w, c, to) = sale(5);
    let m = c.mint;
    w.env.warp(61);
    let held = w.slot_coin(&m, 0);

    let cranker = w.wallet_with_sol(SOL);
    let (to_before, crank_before) = (
        w.env.lamports(&to.pubkey()),
        w.env.lamports(&cranker.pubkey()),
    );
    let owner_lamports = w.env.lamports(&slot_owner(&m, 0));
    assert_eq!(
        owner_lamports,
        w.env.rent(0),
        "open_vault funded the selling slot's owner"
    );
    let supply_before = supply(&w, &m);
    let tx = w.execute(&cranker, &m, 0);
    let ev = tx.event::<SlotSold>();
    println!(
        "sale: sold {} of {held}, got {}, bounty {}, paid {}",
        ev.sold, ev.got, ev.bounty, ev.paid
    );
    assert_eq!(ev.sold, held, "a few round trips' cut is below the slice");
    assert_eq!(ev.bounty, ev.got * 50 / 10_000);
    assert_eq!(ev.paid, ev.got - ev.bounty);
    assert_eq!(ev.pending_sol, 0);
    assert_eq!(w.env.lamports(&to.pubkey()), to_before + ev.paid);
    assert_eq!(
        w.env.lamports(&cranker.pubkey()),
        crank_before + ev.bounty - FEE
    );
    // The slot ends with nothing: no coin, no bridged SOL, its owner's lamports as they were.
    assert_eq!((w.slot_coin(&m, 0), w.slot_sol(&m, 0)), (0, 0));
    assert_eq!(w.env.lamports(&slot_owner(&m, 0)), owner_lamports);
    // A sale burns nothing: the coin went to the pool.
    assert_eq!(supply(&w, &m), supply_before);
    let v = w.vault(&m);
    assert_eq!(
        (v.slots[0].sold, v.slots[0].sol_out, v.slots[0].bounties),
        (held, ev.got, ev.bounty)
    );
    // Slot 0 is tax_hook's collector: its sale paid no tax (the DEX measured no cut on the way in).
    let swapped = tx.event::<bordrless_swap::events::Swapped>();
    assert_eq!(swapped.cuts_in, 0);
    assert_eq!(swapped.amount_in, held);
}

#[test]
fn sales_are_spaced_and_never_in_the_launchs_first_minute() {
    let (mut w, c, _) = sale(3);
    let m = c.mint;
    let cranker = w.wallet_with_sol(SOL);
    let created = w.launch(&m).created_at;
    w.env.warp(created + 59 - w.env.now);
    w.execute(&cranker, &m, 0)
        .expect_code(code(VaultError::NotDue));
    w.env.warp(1);
    w.execute(&cranker, &m, 0).ok();
    w.churn(&m, 1, SOL);
    w.env.warp(59);
    w.execute(&cranker, &m, 0)
        .expect_code(code(VaultError::NotDue));
    w.env.warp(1);
    w.execute(&cranker, &m, 0).ok();
}

#[test]
fn a_sale_is_one_slice_capped_by_the_pools_fees() {
    // (creator fee, max_sell_bps, the slice): (LP 30 + Bordrless's quarter of the creator fee) both
    // ways, a quarter of it, at most max_sell_bps. Since round 1 (F2) the creator fee itself is not
    // counted (it returns to the creator): 1% creator fee gives (2 * (30 + 25)) / 4 = 27, not 65.
    for (fee, max_sell, slice) in [(100u16, 100u16, 27u64), (100, 10, 10), (0, 100, 15)] {
        let mut w = World::new();
        let to = w.env.funded(SOL);
        let mut a = vault_args(vec![sell_for_sol(to.pubkey())]);
        a.max_sell_bps = max_sell;
        let c = w.vault_coin_spec(
            a,
            &[],
            &CoinSpec {
                creator_fee_bps: fee,
                ..CoinSpec::default()
            },
        );
        let m = c.mint;
        fund_slot(&mut w, &m, 0, 5 * SOL);
        w.env.warp(61);
        let p: Pool = w.launch_pool(&m);
        let q = u128::from(p.quote_reserve + p.virtual_quote);
        let b = u128::from(p.base_reserve + p.virtual_base);
        let cap = (q * u128::from(slice) / 10_000) as u64;
        let worth = (u128::from(cap) * b / q) as u64;
        let held = w.slot_coin(&m, 0);
        assert!(held > worth);
        let cranker = w.wallet_with_sol(SOL);
        let ev = w.execute(&cranker, &m, 0).event::<SlotSold>();
        println!(
            "fee {fee}, max_sell {max_sell}: sold {} of {held} for {} lamports (cap {cap})",
            ev.sold, ev.got
        );
        assert_eq!(ev.sold, worth);
        assert!(ev.got < cap, "the slice's SOL is below the cap's");
        assert_eq!(w.slot_coin(&m, 0), held - worth);
    }
}

/// Audit 3a vault r1, F1 and r2, V1: each of a vault's `n` selling slots sells at most `1/n` of the
/// slice a sale, once an interval. Two slots cranked in one block sell no more than one slice
/// together, and neither can take the other's part, in either order.
#[test]
fn selling_slots_split_one_slice_an_interval() {
    let mut w = World::new();
    let (a, b) = (w.env.funded(SOL), w.env.funded(SOL));
    let c = w.vault_coin_spec(
        vault_args(vec![sell_for_sol(a.pubkey()), sell_for_sol(b.pubkey())]),
        &[],
        &CoinSpec {
            tax_bps: 0,
            ..CoinSpec::default()
        },
    );
    let m = c.mint;
    fund_slot(&mut w, &m, 0, 4 * SOL);
    fund_slot(&mut w, &m, 1, 4 * SOL);
    w.env.warp(61);
    let cranker = w.wallet_with_sol(SOL);
    for order in [[0u8, 1], [1, 0]] {
        let p: Pool = w.launch_pool(&m);
        let q = u128::from(p.quote_reserve + p.virtual_quote);
        let b = u128::from(p.base_reserve + p.virtual_base);
        let slice = (q * 27 / 10_000) as u64;
        let part = (u128::from(slice / 2) * b / q) as u64;
        let first = w.execute(&cranker, &m, order[0]).event::<SlotSold>();
        let second = w.execute(&cranker, &m, order[1]).event::<SlotSold>();
        println!(
            "order {order:?}: sold {} and {} (each part {part}, slice {slice} lamports); got {} + {}",
            first.sold, second.sold, first.got, second.got
        );
        assert_eq!(first.sold, part, "the first slot sells its half, no more");
        assert!(second.sold > 0);
        assert!(first.got <= slice / 2 && second.got <= slice / 2);
        assert!(first.got + second.got < slice);
        // Each slot sells once an interval.
        w.execute(&cranker, &m, order[0])
            .expect_code(code(VaultError::NotDue));
        w.env.warp(60);
    }
}

/// Audit 3a vault r1, F3 and r2, V2: after a wait the slot's next attempt is a minute later, so no
/// sell follows a wait in the same transaction or block, yet a one-block dip costs it a minute, not
/// an interval; retries within the interval don't step the reference again. A sale restarts the
/// reference's clock.
#[test]
fn a_wait_holds_the_slot_back_a_minute() {
    let mut w = World::new();
    let to = w.env.funded(SOL);
    let mut args = vault_args(vec![sell_for_sol(to.pubkey())]);
    args.interval = 3_600;
    let c = w.vault_coin_spec(
        args,
        &[],
        &CoinSpec {
            tax_bps: 0,
            open: false,
            ..CoinSpec::default()
        },
    );
    let m = c.mint;
    // After a whale's buy, the reference is the price then (as sales at that price leave it).
    let whale = w.wallet_with_sol(25 * SOL);
    w.buy(&whale, &m, 20 * SOL).ok();
    w.open_vault(&c.creator, &m).ok();
    w.set_sell_reference_to_price(&m, 0);
    fund_slot(&mut w, &m, 0, 4 * SOL);
    w.env.warp(61);
    let cranker = w.wallet_with_sol(SOL);
    w.execute(&cranker, &m, 0).event::<SlotSold>();
    let sold_at = w.env.now;
    let s = w.vault(&m).slots[0];
    assert_eq!((s.last_at, s.reference_at), (sold_at, sold_at));
    // The whale dumps: far below the reference. An interval later, a wait.
    let held = w.env.holding(&m, &whale.pubkey());
    w.sell(&whale, &m, held).ok();
    w.env.warp(3_600);
    let ev = w.execute(&cranker, &m, 0).event::<SellWaited>();
    let s = w.vault(&m).slots[0];
    assert_eq!((s.waited_at, s.last_at), (w.env.now, sold_at));
    assert_eq!(s.reference_price, ev.new_reference);
    assert!(ev.new_reference < ev.reference);
    // Same block, and up to a minute later: not due.
    w.execute(&cranker, &m, 0)
        .expect_code(code(VaultError::NotDue));
    w.env.warp(MIN_INTERVAL - 1);
    w.execute(&cranker, &m, 0)
        .expect_code(code(VaultError::NotDue));
    // A minute later it tries again: still below, a wait, but the reference doesn't step again
    // within the interval.
    w.env.warp(1);
    let again = w.execute(&cranker, &m, 0).event::<SellWaited>();
    assert_eq!(again.new_reference, again.reference);
    assert_eq!(w.vault(&m).slots[0].waited_at, w.env.now);
}

#[test]
fn a_sale_waits_while_the_price_is_3_percent_below_its_reference() {
    let mut w = World::new();
    let to = w.env.funded(SOL);
    let c = w.vault_coin(vault_args(vec![sell_for_sol(to.pubkey())]), &[], true);
    let m = c.mint;
    let opening = w.vault(&m).slots[0].reference_price;
    assert_eq!(opening, w.coin_price(&m));
    // A whale's buy lifts the price; the slot fills; four sales, a minute apart, each raise the
    // reference toward the price they leave, by at most 5%.
    let whale = w.wallet_with_sol(25 * SOL);
    w.buy(&whale, &m, 20 * SOL).ok();
    fund_slot(&mut w, &m, 0, 10 * SOL);
    let cranker = w.wallet_with_sol(SOL);
    let mut reference = opening;
    for _ in 0..4 {
        w.env.warp(61);
        let ev = w.execute(&cranker, &m, 0).event::<SlotSold>();
        // Up only, by a step toward the price the sale left.
        let after = w.coin_price(&m);
        assert_eq!(
            ev.reference,
            reference.max(hook_vault::state::step_toward(reference, after))
        );
        assert!(ev.reference <= reference * 105 / 100 && ev.reference >= reference);
        reference = ev.reference;
    }
    println!(
        "reference after four sales: {:.4} of the opening price",
        reference as f64 / opening as f64
    );
    assert!(reference > opening * 12 / 10);
    // The whale dumps: the price falls well below the reference. Each crank now waits and lowers
    // the reference a step for each interval since it last moved, never below the price.
    let held = w.env.holding(&m, &whale.pubkey());
    w.sell(&whale, &m, held).ok();
    let price = w.coin_price(&m);
    assert!(price < reference * 97 / 100);
    let mut waits = 0;
    loop {
        w.env.warp(60);
        let v = w.vault(&m);
        let steps = ((w.env.now - v.slots[0].reference_at) / 60).clamp(1, 64) as u32;
        let floor = v.slots[0].reference_price * 97 / 100;
        let tx = w.execute(&cranker, &m, 0);
        if let Some(ev) = tx.events::<SellWaited>().first() {
            let mut want = ev.reference;
            for _ in 0..steps {
                want = hook_vault::state::step_toward(want, price);
            }
            println!(
                "wait {waits}: price/reference {:.4}, reference {:.4} -> {:.4} of the opening",
                ev.price as f64 / ev.reference as f64,
                ev.reference as f64 / opening as f64,
                ev.new_reference as f64 / opening as f64
            );
            assert_eq!(ev.new_reference, want);
            assert!(tx.events::<SlotSold>().is_empty());
            waits += 1;
            continue;
        }
        let ev = tx.event::<SlotSold>();
        assert!(ev.price >= floor);
        break;
    }
    println!("{waits} waits before the next sale");
    assert!(waits >= 2, "the reference came down step by step");
}

#[test]
fn an_empty_wallet_is_paid_once_a_payment_can_open_it() {
    let mut w = World::new();
    let to = Keypair::new().pubkey();
    let c = w.vault_coin(vault_args(vec![sell_for_sol(to)]), &[], true);
    let m = c.mint;
    let rent = w.env.rent(0);
    // One small round trip: a cut worth far less than a wallet's rent-exempt minimum.
    w.churn(&m, 1, SOL / 50);
    w.env.warp(61);
    let cranker = w.wallet_with_sol(SOL);
    let ev = w.execute(&cranker, &m, 0).event::<SlotSold>();
    assert!(ev.got - ev.bounty < rent);
    assert_eq!((ev.paid, ev.pending_sol), (0, ev.got - ev.bounty));
    assert_eq!(w.env.lamports(&to), 0);
    assert_eq!(w.slot_sol(&m, 0), ev.pending_sol);
    let pending = ev.pending_sol;
    // Nothing left to sell, and the wallet still can't take it: nothing to do.
    w.env.warp(60);
    w.execute(&cranker, &m, 0)
        .expect_code(code(VaultError::NothingToDo));
    // More trades: the next sale and what waited are paid together, enough to open the wallet.
    w.churn(&m, 2, SOL / 5);
    let ev = w.execute(&cranker, &m, 0).event::<SlotSold>();
    assert_eq!(ev.paid, pending + ev.got - ev.bounty);
    assert!(ev.paid >= rent);
    assert_eq!(ev.pending_sol, 0);
    assert_eq!(w.env.lamports(&to), ev.paid);

    // A wallet that exists takes a payment held back, with nothing to sell.
    let mut w = World::new();
    let to = Keypair::new().pubkey();
    let c = w.vault_coin(vault_args(vec![sell_for_sol(to)]), &[], true);
    let m = c.mint;
    w.churn(&m, 1, SOL / 50);
    w.env.warp(61);
    let cranker = w.wallet_with_sol(SOL);
    let pending = w.execute(&cranker, &m, 0).event::<SlotSold>().pending_sol;
    assert!(pending > 0);
    w.env.fund(to, 1);
    w.env.warp(60);
    let tx = w.execute(&cranker, &m, 0);
    assert_eq!(tx.event::<PendingPaid>().paid, pending);
    assert_eq!(w.env.lamports(&to), 1 + pending);
    assert_eq!(
        (w.vault(&m).slots[0].pending_sol, w.slot_sol(&m, 0)),
        (0, 0)
    );
}

#[test]
fn a_declared_hook_cut_loosens_min_out_by_that_cut_and_no_more() {
    // tax_hook takes 5% of slot 1's sale (slot 1 is not its collector), and the DEX a quarter of that
    // cut's value in SOL: a vault that declared no cut can't sell (its min_out holds), one that
    // declared 5% can.
    for (declared, lands) in [(0u16, false), (200, false), (500, true)] {
        let mut w = World::new();
        let (a, b) = (w.env.funded(SOL), w.env.funded(SOL));
        let mut args = vault_args(vec![sell_for_sol(a.pubkey()), sell_for_sol(b.pubkey())]);
        args.max_hook_cut_bps = declared;
        let c = w.vault_coin_spec(
            args,
            &[],
            &CoinSpec {
                tax_bps: 500,
                ..CoinSpec::default()
            },
        );
        let m = c.mint;
        fund_slot(&mut w, &m, 1, SOL / 2);
        w.env.warp(61);
        let cranker = w.wallet_with_sol(SOL);
        let tx = w.execute(&cranker, &m, 1);
        println!("declared cut {declared}: lands {}", tx.result.is_ok());
        if lands {
            let ev = tx.event::<SlotSold>();
            let swapped = tx.event::<bordrless_swap::events::Swapped>();
            assert_eq!(
                swapped.cuts_in,
                ev.sold.div_ceil(20),
                "tax_hook's 5% on the way in"
            );
        } else {
            // The DEX's own min_out check (`SlippageExceeded`).
            tx.expect_fail();
            assert!(
                tx.logs().iter().any(|l| l.contains("Slippage")),
                "{}",
                tx.logs().join("\n")
            );
        }
        // Slot 0, the collector, sells untaxed either way (its own half of the slice).
        w.env.warp(60);
        w.execute(&cranker, &m, 0).ok();
    }
}

// ---- SellBuyBurn ---------------------------------------------------------------------------------

/// A coin whose slot 0 sells and buys and burns `x`, a launch with `rules` (max wallet 2%: a kit
/// token).
fn buy_burn(rules: LaunchRules) -> (World, VaultCoin, Pubkey, Pubkey) {
    let mut w = World::new();
    let (x, x_pool) = other_token(&mut w, rules);
    let c = w.vault_coin(vault_args(vec![sell_buy_burn(x_pool)]), &[x], true);
    (w, c, x, x_pool)
}

#[test]
fn a_buy_burn_slot_sells_then_buys_and_burns_the_other_token() {
    let (mut w, c, x, x_pool) = buy_burn(max_wallet(200));
    let m = c.mint;
    let v = w.vault(&m);
    let x_price = {
        let p: Pool = w.env.read(&x_pool);
        hook_vault::state::spot_price(
            p.quote_reserve,
            p.virtual_quote,
            p.base_reserve,
            p.virtual_base,
        )
        .unwrap()
    };
    assert_eq!(v.slots[0].buy_reference, x_price, "set at the opening");
    assert!(w.env.lamports(&slot_owner(&m, 0)) >= w.env.rent(0));
    w.churn(&m, 5, 2 * SOL);
    w.env.warp(61);
    let cranker = w.wallet_with_sol(SOL);
    // Nothing to buy with yet.
    w.execute_buy(&cranker, &m, 0)
        .expect_code(code(VaultError::NothingToDo));
    let sold = w.execute(&cranker, &m, 0).event::<SlotSold>();
    assert_eq!(sold.paid, 0);
    assert_eq!(sold.pending_sol, sold.got - sold.bounty);
    assert_eq!(w.slot_sol(&m, 0), sold.pending_sol);
    let x_supply = supply(&w, &x);
    let lamports = w.env.lamports(&cranker.pubkey());
    let tx = w.execute_buy(&cranker, &m, 0);
    let ev = tx.event::<SlotBought>();
    println!(
        "buy and burn: spent {} of {}, burned {} of the other token, bounty {}",
        ev.spent, sold.pending_sol, ev.burned, ev.bounty
    );
    assert_eq!(
        ev.spent + ev.bounty,
        sold.pending_sol,
        "below the slice: all of it"
    );
    assert_eq!(ev.bounty, sold.pending_sol * 50 / 10_000);
    assert!(ev.burned > 0);
    assert_eq!(supply(&w, &x), x_supply - ev.burned);
    assert_eq!(w.env.holding(&x, &slot_owner(&m, 0)), 0);
    assert_eq!((ev.pending_sol, w.slot_sol(&m, 0)), (0, 0));
    // The cranker paid the slot's holding of the token (its rent) and the fee, and got the bounty.
    let rent = w.env.rent(bordrless_token::state::Holding::LEN);
    assert_eq!(
        w.env.lamports(&cranker.pubkey()),
        lamports + ev.bounty - FEE - rent
    );
    let v = w.vault(&m);
    assert_eq!(
        (v.slots[0].x_burned, v.slots[0].buy_last_at),
        (ev.burned, w.env.now)
    );
    assert!(v.slots[0].buy_reference <= x_price * 105 / 100);
    // Spaced as the sales are.
    w.env.warp(30);
    w.execute(&cranker, &m, 0).expect_fail();
    w.execute_buy(&cranker, &m, 0)
        .expect_code(code(VaultError::NotDue));
    // Only a buy-burn slot buys.
    let to = w.env.funded(SOL);
    let c2 = w.vault_coin(vault_args(vec![sell_for_sol(to.pubkey())]), &[], true);
    let ix = vault_client::execute_buy(
        cranker.pubkey(),
        &w.vault(&c2.mint),
        0,
        &w.launch_keys(&x),
        vault_client::BuyHook::Kit { rewards: false },
    );
    w.env
        .send_paid_by(&[ix], &cranker, &[])
        .expect_code(code(VaultError::NotSellBuyBurn));
}

#[test]
fn a_buy_waits_while_the_other_token_is_pumped() {
    let (mut w, c, x, x_pool) = buy_burn(LaunchRules::NONE);
    let m = c.mint;
    fund_slot(&mut w, &m, 0, 3 * SOL);
    w.env.warp(61);
    let cranker = w.wallet_with_sol(SOL);
    w.execute(&cranker, &m, 0).ok();
    // Someone pumps the other token 10% or more.
    let pumper = w.wallet_with_sol(5 * SOL);
    w.buy(&pumper, &x, 2 * SOL).ok();
    let reference = w.vault(&m).slots[0].buy_reference;
    let price = {
        let p: Pool = w.env.read(&x_pool);
        hook_vault::state::spot_price(
            p.quote_reserve,
            p.virtual_quote,
            p.base_reserve,
            p.virtual_base,
        )
        .unwrap()
    };
    println!(
        "the other token at {:.4} of the buy's reference",
        price as f64 / reference as f64
    );
    assert!(price > reference * 103 / 100);
    let mut waits = 0;
    loop {
        w.env.warp(60);
        let tx = w.execute_buy(&cranker, &m, 0);
        if let Some(ev) = tx.events::<BuyWaited>().first() {
            assert!(ev.new_reference > ev.reference && ev.new_reference <= ev.price);
            assert!(ev.new_reference <= ev.reference * 105 / 100 * 105 / 100 * 105 / 100);
            waits += 1;
            continue;
        }
        let ev = tx.event::<SlotBought>();
        assert!(ev.burned > 0);
        break;
    }
    println!("{waits} waits before the buy");
    assert!(waits >= 1);
}

// ---- Sandwiches (mirrored from the companion's buyback tests and launch_money.rs) ------------------

/// A sale sandwiched: the attacker dumps `front` worth of the coin just before a sale it cranks
/// itself, and buys back with all it got right after. No tax (tax_hook at 0%), so the attacker pays
/// only the launch's fees. Answers the slice sold and the attacker's profit in lamports: SOL and
/// bridged SOL, plus the coin it ended with less what it started with, valued at the final price.
fn sale_sandwich(creator_fee: u16, front: u64) -> (u64, u64, i128) {
    let mut w = World::new();
    let to = w.env.funded(SOL);
    let c = w.vault_coin_spec(
        vault_args(vec![sell_for_sol(to.pubkey())]),
        &[],
        &CoinSpec {
            creator_fee_bps: creator_fee,
            tax_bps: 0,
            ..CoinSpec::default()
        },
    );
    let m = c.mint;
    let attacker = w.wallet_with_sol(40 * SOL);
    w.buy(&attacker, &m, 30 * SOL).ok();
    fund_slot(&mut w, &m, 0, 5 * SOL);
    w.env.warp(61);
    let a = attacker.pubkey();
    let (coin0, wealth0) = (w.env.holding(&m, &a), wealth(&w, &a));
    // The front-run: sell `front` lamports' worth.
    let p = w.launch_pool(&m);
    let dump = (u128::from(front) * u128::from(p.base_reserve + p.virtual_base)
        / u128::from(p.quote_reserve + p.virtual_quote)) as u64;
    let sol0 = w.env.holding(&w.sol, &a);
    w.sell(&attacker, &m, dump.min(coin0)).ok();
    let got = w.env.holding(&w.sol, &a) - sol0;
    let tx = w.execute(&attacker, &m, 0);
    tx.ok();
    let sold = tx.events::<SlotSold>().first().map_or(0, |e| e.sold);
    let bounty = tx.events::<SlotSold>().first().map_or(0, |e| e.bounty);
    // The back-run: buy back with everything the dump brought.
    w.env
        .send_paid_by(&[w.launch_swap_ix(&a, &m, 1, got, 0)], &attacker, &[])
        .ok();
    let price = w.coin_price(&m);
    let coins = w.env.holding(&m, &a) as i128 - coin0 as i128;
    let value = coins * price as i128 / hook_vault::constants::PRICE_SCALE as i128;
    (sold, bounty, wealth(&w, &a) - wealth0 + value)
}

#[test]
fn a_sale_sandwich_does_not_pay() {
    for fee in [0u16, 100] {
        for front in [SOL / 10, SOL / 2, 2 * SOL, 8 * SOL] {
            let (sold, bounty, profit) = sale_sandwich(fee, front);
            println!(
                "sale sandwich, creator fee {fee}, front-run {front}: slice {sold}, bounty {bounty}, \
                 attacker profit {profit} lamports (fees on the round trip included)"
            );
            assert!(sold > 0);
            // Never more than an honest crank's bounty, and a loss for any real front-run.
            assert!(profit < bounty as i128, "fee {fee} front {front}: {profit}");
            if front >= SOL / 2 {
                assert!(
                    profit < 0,
                    "fee {fee} front {front}: the sandwich paid {profit}"
                );
            }
        }
    }
}

/// A buy sandwiched (the companion's `sandwich`, on the vault's buy leg): the attacker pumps the
/// other token by `front` lamports before a buy it cranks itself and sells after.
fn buy_sandwich(front: u64) -> (u64, bool, i128) {
    let (mut w, c, x, _) = buy_burn(LaunchRules::NONE);
    let m = c.mint;
    fund_slot(&mut w, &m, 0, 8 * SOL);
    w.env.warp(61);
    let attacker = w.wallet_with_sol(30 * SOL);
    w.execute(&attacker, &m, 0).ok();
    w.env.warp(61);
    let a = attacker.pubkey();
    let start = wealth(&w, &a);
    let ixs = [
        token::create_holding(a, x, a),
        w.launch_swap_ix(&a, &x, 1, front, 0),
    ];
    w.env.send_paid_by(&ixs, &attacker, &[]).ok();
    let got = w.env.holding(&x, &a);
    let tx = w.execute_buy(&attacker, &m, 0);
    tx.ok();
    let waited = !tx.events::<BuyWaited>().is_empty();
    let spent = tx.events::<SlotBought>().first().map_or(0, |e| e.spent);
    w.sell(&attacker, &x, got).ok();
    (spent, waited, wealth(&w, &a) - start)
}

#[test]
fn a_buy_sandwich_does_not_pay() {
    for front in [SOL / 5, SOL, 5 * SOL] {
        let (spent, waited, profit) = buy_sandwich(front);
        println!("buy sandwich, front-run {front}: spent {spent}, waited {waited}, attacker profit {profit} lamports");
        assert!(waited || spent > 0);
        assert!(profit < 0, "front-run {front}: the sandwich paid {profit}");
    }
}

// ---- A hook that refuses the vault; retire ---------------------------------------------------------

#[test]
fn a_hook_refusing_the_vault_leaves_the_slot_to_retire() {
    let mut w = World::new();
    let (x, x_pool) = other_token(&mut w, max_wallet(200));
    // Two buy slots buy two tokens (one slot a pool since round 1, F1).
    let (z, z_pool) = other_token(&mut w, max_wallet(200));
    let c = w.vault_coin(
        vault_args(vec![sell_buy_burn(x_pool), sell_buy_burn(z_pool)]),
        &[x, z],
        true,
    );
    let m = c.mint;
    fund_slot(&mut w, &m, 1, SOL);
    w.churn(&m, 3, SOL);
    w.env.warp(61);
    let cranker = w.wallet_with_sol(SOL);
    w.execute(&cranker, &m, 0).ok();
    w.execute(&cranker, &m, 1).ok();
    w.churn(&m, 2, SOL);
    fund_slot(&mut w, &m, 1, SOL / 2);
    let sold_at = w.env.now;
    // The hook turns on a wallet cap of 0.01%: no transfer into the pool's vault passes any more,
    // the vault's sales among them (a honeypot).
    w.set_tax(&m, 100, 1);
    w.env.warp(61);
    let tx = w.execute(&cranker, &m, 0);
    tx.expect_code(6003); // tax_hook's WalletTooLarge
    let pending = [
        w.vault(&m).slots[0].pending_sol,
        w.vault(&m).slots[1].pending_sol,
    ];
    let coin = [w.slot_coin(&m, 0), w.slot_coin(&m, 1)];
    assert!(pending.iter().all(|p| *p > 0) && coin.iter().all(|c| *c > 0));
    // Not before 60 days without a run.
    w.retire(&cranker, &m, 0, true)
        .expect_code(code(VaultError::NotRetirable));
    w.env.warp(sold_at + RETIRE_SECS - 1 - w.env.now);
    w.retire(&cranker, &m, 0, true)
        .expect_code(code(VaultError::NotRetirable));
    w.env.warp(1);
    let incinerator = w.env.lamports(&INCINERATOR);
    let (supply0, lamports) = (supply(&w, &m), w.env.lamports(&cranker.pubkey()));
    let tx = w.retire(&cranker, &m, 0, true);
    let ev = tx.event::<SlotRetired>();
    assert_eq!((ev.sol_burned, ev.coin_burned), (pending[0], coin[0]));
    assert_eq!(w.env.lamports(&INCINERATOR), incinerator + pending[0]);
    assert_eq!(supply(&w, &m), supply0 - coin[0]);
    assert_eq!((w.slot_coin(&m, 0), w.slot_sol(&m, 0)), (0, 0));
    // Nobody received anything: the cranker paid the fee.
    assert_eq!(w.env.lamports(&cranker.pubkey()), lamports - FEE);
    // Slot 1 keeps its coin (a hook that refused burns too would fail a burn); its SOL is burned.
    let tx = w.retire(&cranker, &m, 1, false);
    let ev = tx.event::<SlotRetired>();
    assert_eq!((ev.sol_burned, ev.coin_burned), (pending[1], 0));
    assert_eq!(w.slot_coin(&m, 1), coin[1]);
    assert_eq!(
        w.env.lamports(&INCINERATOR),
        incinerator + pending[0] + pending[1]
    );
    // A retirement is a run: the next one waits another 60 days, and then needs something to burn.
    w.env.warp(1);
    w.retire(&cranker, &m, 1, true)
        .expect_code(code(VaultError::NotRetirable));
    w.env.warp(RETIRE_SECS);
    w.retire(&cranker, &m, 0, true)
        .expect_code(code(VaultError::NothingToDo));
    w.retire(&cranker, &m, 1, true).event::<SlotRetired>();
    assert_eq!(w.slot_coin(&m, 1), 0);
}

// ---- PDA signer scope ----------------------------------------------------------------------------

#[test]
fn a_slot_owner_signs_only_for_its_own_slot() {
    let mut w = World::new();
    let c = w.vault_coin(vault_args(vec![burn(), burn()]), &[], true);
    let m = c.mint;
    w.churn(&m, 2, SOL);
    fund_slot(&mut w, &m, 1, SOL);
    let (zero, one) = (w.slot_coin(&m, 0), w.slot_coin(&m, 1));
    assert!(zero > 0 && one > 0);
    let cranker = w.wallet_with_sol(SOL);
    // execute(0) with slot 1's owner in its place.
    let mut ix = w.execute_ix(&cranker.pubkey(), &m, 0);
    assert_eq!(ix.accounts[2].pubkey, slot_owner(&m, 0));
    ix.accounts[2].pubkey = slot_owner(&m, 1);
    w.env
        .send_paid_by(&[ix], &cranker, &[])
        .expect_code(code(VaultError::WrongSlotOwner));
    // execute(1) handed slot 0's holding and burn: the program builds slot 1's own burn, and finds
    // its holding missing.
    let one_ix = w.execute_ix(&cranker.pubkey(), &m, 1);
    let zero_ix = w.execute_ix(&cranker.pubkey(), &m, 0);
    let mut mixed = Instruction {
        program_id: hook_vault::ID,
        accounts: one_ix.accounts[..7].to_vec(),
        data: one_ix.data.clone(),
    };
    mixed.accounts.extend(
        zero_ix.accounts[7..]
            .iter()
            .filter(|a| a.pubkey != vault_client::slot_holding(&m, 1))
            .cloned(),
    );
    w.env
        .send_paid_by(&[mixed], &cranker, &[])
        .expect_code(code(VaultError::MissingAccount));
    // No such slot.
    let mut ix = w.execute_ix(&cranker.pubkey(), &m, 1);
    ix.data = hook_vault::instruction::Execute { i: 2 }.data();
    w.env
        .send_paid_by(&[ix], &cranker, &[])
        .expect_code(code(VaultError::WrongSlot));
    // execute(1) burns slot 1's coin, and nothing of slot 0's.
    w.execute(&cranker, &m, 1).ok();
    assert_eq!((w.slot_coin(&m, 0), w.slot_coin(&m, 1)), (zero, 0));
}

// ---- Policies are fixed --------------------------------------------------------------------------

#[test]
fn policies_never_change() {
    let (mut w, _, x, x_pool) = buy_burn(max_wallet(200));
    let to = w.env.funded(SOL);
    let mut a = vault_args(vec![
        burn(),
        sell_for_sol(to.pubkey()),
        sell_buy_burn(x_pool),
    ]);
    a.bounty_bps = 77;
    a.max_hook_cut_bps = 123;
    let c = w.vault_coin(a, &[x], true);
    let m = c.mint;
    let fixed = |v: &hook_vault::state::Vault| {
        (
            v.mint,
            v.hook,
            v.creator,
            v.n_slots,
            v.slots
                .map(|s| (s.policy, s.target, s.owner_bump, s.max_cut_bps)),
            (
                v.bounty_bps,
                v.max_sell_bps,
                v.interval,
                v.max_hook_cut_bps,
                v.created_at,
            ),
        )
    };
    let before = fixed(&w.vault(&m));
    fund_slot(&mut w, &m, 1, SOL);
    fund_slot(&mut w, &m, 2, SOL);
    let cranker = w.wallet_with_sol(SOL);
    // (An interval apart: not needed since round 2, kept so the numbers stay comparable.)
    for i in 0..3 {
        w.env.warp(61);
        w.execute(&cranker, &m, i).ok();
    }
    w.execute_buy(&cranker, &m, 2).ok();
    w.env.warp(RETIRE_SECS);
    w.churn(&m, 1, SOL);
    w.retire(&cranker, &m, 0, true).ok();
    assert_eq!(fixed(&w.vault(&m)), before);
    // The vault's whole interface: nothing else writes a vault.
    let names: Vec<&str> = vec![
        "create_vault",
        "open_vault",
        "execute",
        "execute_buy",
        "retire",
    ];
    let disc = |n: &str| {
        let h = solana_sha256_hasher::hash(format!("global:{n}").as_bytes());
        h.to_bytes()[..8].to_vec()
    };
    use anchor_lang::Discriminator;
    assert_eq!(
        vec![
            hook_vault::instruction::CreateVault::DISCRIMINATOR.to_vec(),
            hook_vault::instruction::OpenVault::DISCRIMINATOR.to_vec(),
            hook_vault::instruction::Execute::DISCRIMINATOR.to_vec(),
            hook_vault::instruction::ExecuteBuy::DISCRIMINATOR.to_vec(),
            hook_vault::instruction::Retire::DISCRIMINATOR.to_vec(),
        ],
        names.iter().map(|n| disc(n)).collect::<Vec<_>>()
    );
    // Any other 8 bytes are no instruction.
    let mut ix = w.execute_ix(&cranker.pubkey(), &m, 0);
    ix.data = disc("set_policy");
    ix.data.push(0);
    w.env.send_paid_by(&[ix], &cranker, &[]).expect_code(101); // InstructionFallbackNotFound
}

// ---- Limits --------------------------------------------------------------------------------------

/// The protocol's lookup table (§6) with the vault's event authority and the programs a vault step
/// reaches but never invokes at top level: 22 addresses.
fn table_22(w: &World) -> Vec<Pubkey> {
    let mut addresses = protocol_lookup_table(w);
    addresses.extend([
        vault_client::event_authority(),
        bordrless_launch::ID,
        bordrless_swap::ID,
        bordrless_token::ID,
    ]);
    assert_eq!(addresses.len(), 22);
    addresses
}

/// Sends `ix` through the table with a 1.4M compute limit, from `cranker`, and checks the limits.
fn measured(w: &mut World, name: &str, cranker: &Keypair, ix: Instruction) -> Tx {
    let table = table_22(w);
    let lut = w.env.put_lookup_table(Pubkey::new_unique(), &table);
    let tx = w.env.send_v0(
        &[compute_unit_limit(1_400_000), ix],
        cranker,
        &[],
        std::slice::from_ref(&lut),
    );
    tx.ok();
    // A `heap-probe` build of the vault logs the heap each step used.
    let heap: Vec<&str> = tx
        .logs()
        .iter()
        .filter_map(|l| l.split("heap used: ").nth(1))
        .collect();
    println!(
        "{name}: {} bytes, {} CU, height {}, trace {}{}",
        tx.size,
        tx.cu(),
        tx.max_height(),
        tx.trace_len(),
        if heap.is_empty() {
            String::new()
        } else {
            format!(", heap {}", heap.join("/"))
        }
    );
    assert!(tx.size <= PACKET, "{name} is {} bytes", tx.size);
    assert!(tx.cu() < 1_400_000);
    assert!(tx.max_height() <= 5);
    assert!(tx.trace_len() <= 64);
    tx
}

#[test]
fn every_step_fits_a_packet_and_the_runtime_limits() {
    let mut w = World::new();
    let (x, x_pool) = other_token(&mut w, max_wallet(200));
    let to = w.env.funded(SOL);
    let c = w.vault_coin(
        vault_args(vec![
            burn(),
            sell_for_sol(to.pubkey()),
            sell_buy_burn(x_pool),
        ]),
        &[x],
        false,
    );
    let m = c.mint;
    let cranker = w.wallet_with_sol(5 * SOL);
    // The vault's creator opens it (X3: nobody else picks the moment).
    let creator = c.creator.insecure_clone();
    let ix = w.open_vault_ix(&creator.pubkey(), &m);
    let tx = measured(&mut w, "open_vault, 3 slots", &creator, ix);
    assert_eq!(tx.max_height(), 3);
    w.churn(&m, 3, SOL);
    fund_slot(&mut w, &m, 1, SOL);
    fund_slot(&mut w, &m, 2, 2 * SOL);
    w.env.warp(61);
    let ix = w.execute_ix(&cranker.pubkey(), &m, 0);
    let tx = measured(&mut w, "execute: burn", &cranker, ix);
    assert_eq!(tx.max_height(), 3, "vault, token, its event");
    let ix = w.execute_ix(&cranker.pubkey(), &m, 1);
    let tx = measured(&mut w, "execute: sell for SOL", &cranker, ix);
    // vault (1) → DEX (2) → token (3) → the coin's hook (4).
    assert_eq!(tx.max_height(), 4);
    // (An interval later: not needed since round 2, kept so the numbers stay comparable.)
    w.env.warp(61);
    let ix = w.execute_ix(&cranker.pubkey(), &m, 2);
    let tx = measured(&mut w, "execute: sell to buy", &cranker, ix);
    assert_eq!(tx.max_height(), 4);
    let ix = w.execute_buy_ix(&cranker.pubkey(), &m, 2);
    let tx = measured(&mut w, "execute_buy: a kit token", &cranker, ix);
    // vault (1) → DEX (2) → token (3) → the kit (4).
    assert_eq!(tx.max_height(), 4);
    // A slot with both SOL and coin to retire.
    fund_slot(&mut w, &m, 2, SOL);
    w.env.warp(61);
    w.execute(&cranker, &m, 2).ok();
    fund_slot(&mut w, &m, 2, SOL);
    w.env.warp(RETIRE_SECS);
    let ix = w.retire_ix(&cranker.pubkey(), &m, 2, true);
    measured(&mut w, "retire: SOL and coin", &cranker, ix);

    // The buy leg into a custom-hook token (another vault coin as the token bought).
    let mut w = World::new();
    let y = w.vault_coin(vault_args(vec![burn()]), &[], true).mint;
    let y_pool = w.launch_pool_key(&y);
    let c = w.vault_coin(vault_args(vec![sell_buy_burn(y_pool)]), &[y], true);
    let m = c.mint;
    let cranker = w.wallet_with_sol(5 * SOL);
    fund_slot(&mut w, &m, 0, 2 * SOL);
    w.env.warp(61);
    w.execute(&cranker, &m, 0).ok();
    let ix = w.execute_buy_ix(&cranker.pubkey(), &m, 0);
    let tx = measured(&mut w, "execute_buy: a custom-hook token", &cranker, ix);
    // The token bought pays its own hook's 1% on the way to the slot (to its own vault's slot 0).
    assert!(tx.event::<SlotBought>().burned > 0);
    assert!(w.slot_coin(&y, 0) > 0);
}

// ---- The port --------------------------------------------------------------------------------------

#[test]
fn the_guards_are_the_companions() {
    use bordrless_companion::constants as cc;
    assert_eq!(MAX_PREMIUM_BPS, cc::MAX_PREMIUM_BPS);
    assert_eq!(MAX_DISCOUNT_BPS, cc::MAX_PREMIUM_BPS);
    assert_eq!(REFERENCE_STEP_BPS, cc::REFERENCE_STEP_BPS);
    assert_eq!(MAX_REFERENCE_STEPS, cc::MAX_REFERENCE_STEPS);
    assert_eq!(PRICE_SCALE, cc::PRICE_SCALE);
    assert_eq!(SLIPPAGE_BPS, cc::BUYBACK_SLIPPAGE_BPS);
    assert_eq!(AFTER_LAUNCH, cc::BUYBACK_AFTER_LAUNCH);
    assert_eq!(POOL_SHARE_BPS, cc::BUYBACK_POOL_SHARE_BPS);
    assert_eq!(MAX_BOUNTY_BPS, cc::MAX_BOUNTY_BPS);
    assert_eq!(
        (MIN_INTERVAL, MAX_INTERVAL),
        (cc::MIN_BUYBACK_INTERVAL, cc::MAX_BUYBACK_INTERVAL)
    );
    assert_eq!(
        (
            MAX_REGISTRY_ACCOUNTS,
            MAX_REGISTRY_SEEDS,
            MAX_REGISTRY_SEED_LEN,
            MAX_REGISTRY_LEN
        ),
        (
            cc::MAX_REGISTRY_ACCOUNTS,
            cc::MAX_REGISTRY_SEEDS,
            cc::MAX_REGISTRY_SEED_LEN,
            cc::MAX_REGISTRY_LEN
        )
    );
    assert_eq!(INCINERATOR, cc::INCINERATOR);
    for (r, s) in [
        (1_000_000u128, 2_000_000u128),
        (1_000_000, 10),
        (0, 5),
        (7, 7),
    ] {
        assert_eq!(
            hook_vault::state::step_toward(r, s),
            bordrless_companion::state::step_toward(r, s)
        );
    }
    // The slice, for launches of every fee shape.
    let mut w = World::new();
    let creator = w.wallet_with_sol(20 * SOL);
    for (fee, rules) in [
        (0u16, LaunchRules::NONE),
        (100, LaunchRules::NONE),
        (
            200,
            LaunchRules {
                burn_buy_bps: 50,
                burn_sell_bps: 100,
                ..LaunchRules::NONE
            },
        ),
        (
            50,
            LaunchRules {
                holder_fee_buy_bps: 100,
                holder_fee_sell_bps: 50,
                ..LaunchRules::NONE
            },
        ),
    ] {
        let (mint, tx) = w.create_launch_with(&creator, "FEE", fee, VQ, rules);
        tx.ok();
        let l = w.launch(&mint);
        assert_eq!(
            hook_vault::instructions::common::pool_share_bps(&l),
            bordrless_companion::instructions::pool_share_bps(&l)
        );
    }
}
