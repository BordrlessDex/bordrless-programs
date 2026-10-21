//! Workshop items on coins quoted in tokens (`docs/token-quotes.md` §7, 21 Oct 2026, "builder
//! decides"): a coin quoted in tokens pays everything in its own tokens, and a Workshop builder's
//! script decides what to do with them. No converter.
//!
//! - **A script companion** launches a coin quoted in one token and in three (SPL Token and an
//!   xStocks-like Token-2022 mint) through its `launch_token`, which wraps the launchpad's
//!   `create_token_launch` with the companion's creator address as the creator; its whole creator
//!   fee is its pot. Anyone claims each leg's fee (`claim_creator_fees_token`) into the creator
//!   address's associated token account of the leg's token (a PDA's ATA, made by the claim). The
//!   script (`fixtures/scripts/example`'s leg modes) buys the coin on a leg with those tokens
//!   (`swap_spl`), sells it back and pays a holder in a leg's token, each step with a SOL pot of
//!   zero and no bounty. SOL that reaches the creator address is booked to the pot by `claim_fees`
//!   and spent within it, as on a SOL coin. The companion keeps the creator's token accounts its
//!   own (no close, hand-over, delegate or close authority) and the creator address's rent.
//! - **A Workshop hook** (`hook_tester`, its registry listing the coin's `Legs`) from a launch
//!   config, on a coin quoted in tokens launched by a wallet and by a companion (a pair): its rule
//!   (a cut on every transfer, to a collector) runs on wallet transfers and on every leg's buys and
//!   sells, the `Legs` reaching it on each; `open_leg` passes its slice on the deposit.
//! - **Every refusal** of the new paths: a config with a SOL cut, one that would pay its author or a
//!   partner, a hook whose registry lacks the `Legs`, the config's terms not repeated, its hook's or
//!   the kit's accounts missing or wrong, `open_leg` without the hook's accounts; on the companion,
//!   a launch by a game, by a companion with a holders' part or payees, a script with any part but
//!   the pot, the creator lock, the wrong creator or mint, a second launch, a dev buy; a script's
//!   misdeeds on the creator's token accounts and lamports. (The presets on such coins:
//!   `token_quotes_presets.rs`.)
//!
//! The figures (compute units, transaction sizes) are printed by
//! `measures_builders_compute_and_sizes` (`--nocapture`).

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_companion::client as companion;
use bordrless_companion::constants::{APPROVAL_KIND_SCRIPT, SCRIPT_AFTER_LAUNCH};
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::{
    CompanionLaunched, CompanionNativeSet, PotFunded, ScriptStepped,
};
use bordrless_companion::instructions::{CreateArgs, CreateScriptArgs};
use bordrless_companion::state::{Companion, Split};
use bordrless_core::policy;
use bordrless_hook::{token_flags, Delta, HookReturn};
use bordrless_launch::client::{self as launch, CustomHookAccounts, LaunchKeys};
use bordrless_launch::error::LaunchError;
use bordrless_launch::events::{
    LegCreatorFeesClaimed, TokenLaunchConfigured, TokenLaunchCreated, TokenLaunchRules,
};
use bordrless_launch::instructions::{
    legs_registry_entry, CreateConfigArgs, CreateTokenLaunchArgs,
};
use bordrless_launch::state::{LaunchRules, QuoteCut};
use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::hooks::fixed;
use bordrless_program_tests::launch::protocol_lookup_table;
use bordrless_program_tests::program_bytes;
use bordrless_program_tests::quotes::{
    pack_mint, xstock_extensions, Quote, QUOTE_FUNDS, TOKEN, TOKEN_2022,
};
use bordrless_program_tests::timelock::programdata_hash;
use bordrless_token::client::{self as token, Hook};
use bordrless_token::state::Mint;
use hook_tester::callback;
use hook_tester::client as tester;
use script_example as se;
use solana_keypair::Keypair;
use solana_signer::Signer;

const SOL: u64 = 1_000_000_000;
const SUPPLY: u64 = policy::TOKEN_SUPPLY;
const FEE: u16 = 200;
const SCRIPT: Pubkey = se::ID;
const HOOK: Pubkey = hook_tester::ID;
/// Bordrless's upgrade key: approved scripts are deployed under it, and `hook_tester` is
/// upgradeable by it (`Env::new`), one of the authorities a config's hook may have.
const BORDRLESS_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[1];
const STATE_LEN: u32 = 64;
/// The Workshop hook's cut on every transfer of the coin (raw units), to its collector.
const CUT: u64 = 1_000;
/// The hook's flags: before every transfer (answering the cut) and after it, before the mint.
const HOOK_FLAGS: u16 = token_flags::BEFORE_TRANSFER
    | token_flags::AFTER_TRANSFER
    | token_flags::BEFORE_MINT
    | token_flags::TRANSFER_RETURNS_DELTA;
/// The collector's holding among a token callback's accounts: the token program's five, the
/// hook's script (5), the coin's `Legs` (6), then it.
const CUT_INDEX: u8 = 7;

fn lcode(e: LaunchError) -> u32 {
    u32::from(e)
}

fn ccode(e: CompanionError) -> u32 {
    u32::from(e)
}

#[track_caller]
fn refused_by(tx: &Tx, code: u32, name: &str) {
    tx.expect_code(code);
    assert!(
        tx.logs()
            .iter()
            .any(|l| l.contains(&format!("Error Code: {name}"))),
        "expected {name}\n{}",
        tx.logs().join("\n")
    );
}

#[track_caller]
fn refused_l(tx: &Tx, e: LaunchError) {
    refused_by(tx, lcode(e), &format!("{e:?}"));
}

#[track_caller]
fn refused_c(tx: &Tx, e: CompanionError) {
    refused_by(tx, ccode(e), &format!("{e:?}"));
}

// ---- The world ----------------------------------------------------------------------------------

/// A world with the example script loaded (upgradeable by Bordrless's key, as approved scripts are)
/// and approved, naming no author.
fn world() -> World {
    let mut w = World::new();
    w.env
        .svm
        .add_program(SCRIPT, &program_bytes("script_example"))
        .expect("load the script");
    w.env.set_upgrade_authority(SCRIPT, Some(BORDRLESS_KEY));
    let deployer = w.env.deployer.insecure_clone();
    let hash = programdata_hash(&w.env, &SCRIPT);
    w.env
        .send(
            &[companion::approve(
                deployer.pubkey(),
                SCRIPT,
                APPROVAL_KIND_SCRIPT,
                hash,
                Pubkey::default(),
            )],
            &[&deployer],
        )
        .ok();
    w
}

/// One quote: its program, its decimals, whether it carries an xStock's extensions.
type Spec = (Pubkey, u8, bool);

/// A coin's quotes: `(mint, program)` per leg.
type Quotes = Vec<(Pubkey, Pubkey)>;

const ONE_SPL: [Spec; 1] = [(TOKEN, 6, false)];
const THREE: [Spec; 3] = [(TOKEN, 6, false), (TOKEN_2022, 8, true), (TOKEN, 9, false)];
const SIX: [Spec; 6] = [
    (TOKEN, 6, false),
    (TOKEN_2022, 8, true),
    (TOKEN, 9, false),
    (TOKEN, 6, false),
    (TOKEN_2022, 6, false),
    (TOKEN_2022, 9, false),
];

/// The whole cap of a $5,000 coin in a token of `decimals` (a $1 token of 6, a $250 stock of 8,
/// a $200 token of 9).
fn cap_raw(decimals: u8) -> u64 {
    match decimals {
        6 => 5_000_000_000,
        8 => 2_000_000_000,
        _ => 25_000_000_000,
    }
}

/// Writes the quote mints of `specs`; answers them and the launch's quotes (their virtual quotes).
fn quote_mints(w: &mut World, specs: &[Spec]) -> (Quotes, Vec<Quote>) {
    let n = specs.len() as u64;
    let issuer = w.env.deployer.pubkey();
    let mut quotes = Vec::new();
    let mut launch_quotes = Vec::new();
    for (i, (program, decimals, xstock)) in specs.iter().enumerate() {
        let key = Pubkey::new_unique();
        let exts = if *xstock {
            xstock_extensions(key, Pubkey::new_from_array([7; 32]))
        } else {
            vec![]
        };
        w.env.put_mint(
            key,
            *program,
            pack_mint(Some(issuer), Some(issuer), *decimals, u64::MAX / 2, &exts),
        );
        let share = if i as u64 == n - 1 {
            SUPPLY - SUPPLY / n * (n - 1)
        } else {
            SUPPLY / n
        };
        let vq = (u128::from(cap_raw(*decimals)) * u128::from(share) / u128::from(SUPPLY)) as u64;
        quotes.push((key, *program));
        launch_quotes.push((key, vq));
    }
    (quotes, launch_quotes)
}

/// A funded wallet holding plenty of every quote (its associated token accounts).
fn trader(w: &mut World, quotes: &Quotes) -> Keypair {
    let kp = w.env.funded(100 * SOL);
    for (q, p) in quotes {
        w.env.token_account(&kp.pubkey(), q, p, QUOTE_FUNDS);
    }
    kp
}

/// The owner field of a token account.
fn token_owner(w: &World, key: &Pubkey) -> Pubkey {
    let a = w.env.account(key).expect("a token account");
    Pubkey::new_from_array(a.data[32..64].try_into().expect("32 bytes"))
}

// ---- Companions ---------------------------------------------------------------------------------

fn create_args() -> CreateArgs {
    CreateArgs {
        split: Split {
            buyback_bps: 0,
            holders_bps: 0,
            beneficiary_bps: 10_000,
        },
        bounty_bps: 50,
        max_buyback: SOL,
        buyback_interval: 60,
        vest_secs: 0,
        fund: SOL,
    }
}

/// The script's terms: `pot_bps` of the creator fee to the pot, the rest to the launcher; a step a
/// minute at most.
fn script_args(pot_bps: u16) -> CreateScriptArgs {
    CreateScriptArgs {
        split: Split {
            buyback_bps: 0,
            holders_bps: 0,
            beneficiary_bps: 10_000 - pot_bps,
        },
        pot_bps,
        program: SCRIPT,
        interval: 60,
        state_len: STATE_LEN,
        init_data: vec![],
    }
}

/// A script companion for a fresh mint (`create`, `create_script`), its pot `pot_bps` of the fee.
fn script_companion(w: &mut World, launcher: &Keypair, pot_bps: u16) -> Keypair {
    let mint = Keypair::new();
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            mint.pubkey(),
            create_args(),
        ),
        companion::create_script(
            launcher.pubkey(),
            mint.pubkey(),
            script_args(pot_bps),
            vec![],
        ),
    ];
    w.env.send_paid_by(&ixs, launcher, &[&mint]).ok();
    mint
}

fn companion_of(w: &World, mint: &Pubkey) -> Companion {
    w.env.read(&companion::companion_address(mint))
}

// ---- The Workshop hook --------------------------------------------------------------------------

/// The Workshop hook (`hook_tester`) prepared for `mint` before its launch (as a Workshop hook's
/// `prepare`): its registry lists its script, the coin's `Legs` (when `legs`) and the cut's holding
/// (`collector`'s holding of the coin).
fn prepare_hook(w: &mut World, payer: &Keypair, mint: &Pubkey, collector: &Pubkey, legs: bool) {
    let mut extras = vec![];
    if legs {
        extras.push(legs_registry_entry());
    }
    extras.push(fixed(token::holding_address(mint, collector), true));
    w.env
        .send_paid_by(
            &[tester::init_script(payer.pubkey(), *mint, extras)],
            payer,
            &[],
        )
        .ok();
}

fn hook_config_args(rules: LaunchRules) -> CreateConfigArgs {
    CreateConfigArgs {
        rules,
        creator_fee_bps: FEE,
        custom_hook: Some(HOOK),
        custom_hook_flags: HOOK_FLAGS,
        label: "Workshop hook".to_string(),
    }
}

/// A config naming the Workshop hook (no kit rules beside it), made by `author`.
fn hook_config(w: &mut World, author: &Keypair) -> Pubkey {
    let (config, tx) = w.create_config(author, hook_config_args(LaunchRules::NONE));
    tx.ok();
    config
}

/// The hook's accounts for `mint` as a client resolves them from its registry (they do not depend
/// on the operation: its script and the `Legs` by the mint, the cut's holding fixed).
fn hook_accounts(w: &World, mint: &Pubkey) -> CustomHookAccounts {
    let launch_key = launch::launch_address(mint);
    let reserve = token::holding_address(mint, &launch_key);
    CustomHookAccounts {
        program: HOOK,
        extras: w.env.token_hook_extras(
            &HOOK,
            mint,
            &reserve,
            &reserve,
            &launch_key,
            &launch_key,
            &launch_key,
        ),
    }
}

/// Scripts the hook's cut on every transfer of `mint`: `CUT` to the collector's holding (which
/// must exist).
fn turn_cut_on(w: &mut World, authority: &Keypair, mint: &Pubkey) {
    let answer = HookReturn {
        deltas: vec![Delta {
            amount: CUT,
            account: CUT_INDEX,
        }],
        ..HookReturn::default()
    };
    w.env
        .send_paid_by(
            &[tester::answer(
                authority.pubkey(),
                *mint,
                callback::BEFORE_TRANSFER,
                &answer,
            )],
            authority,
            &[],
        )
        .ok();
}

fn hook_calls(w: &World, mint: &Pubkey) -> u64 {
    w.env
        .read::<hook_tester::Script>(&tester::script_address(mint))
        .calls
}

/// Whether a CPI into the hook in `tx` was handed `key`.
fn hook_saw(tx: &Tx, key: &Pubkey) -> bool {
    tx.ok().inner_instructions.iter().flatten().any(|i| {
        tx.keys[usize::from(i.instruction.program_id_index)] == HOOK
            && i.instruction
                .accounts
                .iter()
                .any(|a| tx.keys[usize::from(*a)] == *key)
    })
}

// ---- Launches, legs, trades and claims ----------------------------------------------------------

/// `create_token_launch` of `creator` for `mint`, quoted in `quotes`, from `config` (its custom
/// hook's accounts resolved from the hook's registry) when given.
fn token_launch_ix(
    w: &World,
    creator: &Pubkey,
    mint: &Pubkey,
    quotes: &[Quote],
    rules: LaunchRules,
    config: Option<&Pubkey>,
) -> (Instruction, CreateTokenLaunchArgs) {
    let mints: Vec<Pubkey> = quotes.iter().map(|q| q.0).collect();
    let args = World::token_launch_args_with("BLDR", FEE, quotes, rules);
    let custom = config
        .and_then(|c| w.launch_config(c).custom_hook)
        .map(|_| hook_accounts(w, mint));
    let ix = launch::create_token_launch_with(
        *creator,
        *mint,
        w.env.treasury.pubkey(),
        args.clone(),
        &mints,
        config.copied(),
        custom.as_ref(),
    );
    (ix, args)
}

/// The companion's `launch_token` of `mint`.
fn companion_launch_ix(
    w: &World,
    launcher: &Pubkey,
    mint: &Pubkey,
    quotes: &[Quote],
    rules: LaunchRules,
    config: Option<&Pubkey>,
) -> Instruction {
    let creator = companion::creator_address(mint);
    let (inner, args) = token_launch_ix(w, &creator, mint, quotes, rules, config);
    companion::launch_token(*launcher, *mint, &inner, args)
}

/// `open_leg(i)`: with the custom hook's accounts when the coin has one.
fn open_leg_ix(w: &World, payer: &Pubkey, mint: &Pubkey, i: usize) -> Instruction {
    match w.launch(mint).custom_hook {
        Some(_) => launch::open_leg_custom(
            *payer,
            w.env.treasury.pubkey(),
            &w.leg_keys(mint, i),
            i as u8,
            &hook_accounts(w, mint),
        ),
        None => w.open_leg_ix(payer, mint, i),
    }
}

fn open_legs(w: &mut World, mint: &Pubkey) {
    let payer = w.env.payer.pubkey();
    for i in 0..usize::from(w.legs(mint).count) {
        let ix = open_leg_ix(w, &payer, mint, i);
        w.env.send(&[ix], &[]).ok();
    }
}

/// The coin's token-hook slice on a leg trade: its custom hook's, the kit's, or none.
fn base_slice(w: &World, mint: &Pubkey) -> Vec<AccountMeta> {
    let l = w.launch(mint);
    match l.custom_hook {
        Some(_) => hook_accounts(w, mint).slice(),
        None => launch::leg_base_slice(mint, l.modules),
    }
}

/// A swap on leg `i` of `mint` by `trader`, delivered to itself, with the coin's hook slice.
fn leg_swap(
    w: &World,
    trader: &Pubkey,
    mint: &Pubkey,
    i: usize,
    direction: u8,
    amount: u64,
) -> Instruction {
    let keys = w.leg_keys(mint, i);
    launch::leg_swap_with_slice(
        &keys,
        *trader,
        token::holding_address(mint, trader),
        keys.quote_account(trader),
        direction,
        amount,
        0,
        base_slice(w, mint),
        w.launch(mint).rules.burns(),
    )
}

fn buy(w: &mut World, trader: &Keypair, mint: &Pubkey, i: usize, amount: u64) -> Tx {
    let who = trader.pubkey();
    let ixs = [
        token::create_holding(who, *mint, who),
        leg_swap(w, &who, mint, i, 1, amount),
    ];
    w.env.send_paid_by(&ixs, trader, &[])
}

fn sell(w: &mut World, trader: &Keypair, mint: &Pubkey, i: usize, coins: u64) -> Tx {
    let ix = leg_swap(w, &trader.pubkey(), mint, i, 0, coins);
    w.env.send_paid_by(&[ix], trader, &[])
}

/// Trades on every leg (a buy of a fiftieth of its virtual quote, a sell of half of it), so every
/// leg's fee account holds creator fees.
fn trade_every_leg(w: &mut World, mint: &Pubkey, quotes: &Quotes) {
    let t = trader(w, quotes);
    for i in 0..quotes.len() {
        let vq = w.legs(mint).legs[i].virtual_quote;
        buy(w, &t, mint, i, vq / 50).ok();
        let coins = w.env.holding(mint, &t.pubkey());
        sell(w, &t, mint, i, coins / 2).ok();
    }
}

/// `claim_creator_fees_token(i)` of `mint`, sent by `cranker`.
fn claim_ix(w: &World, cranker: &Pubkey, mint: &Pubkey, i: usize) -> Instruction {
    let creator = w.launch(mint).creator;
    launch::claim_creator_fees_token(*cranker, creator, &w.leg_keys(mint, i), i as u8)
}

fn claim(w: &mut World, cranker: &Keypair, mint: &Pubkey, i: usize) -> Tx {
    let ix = claim_ix(w, &cranker.pubkey(), mint, i);
    w.env.send_paid_by(&[ix], cranker, &[])
}

/// The creator address's associated token account of leg `i`'s token.
fn creator_account(w: &World, mint: &Pubkey, i: usize) -> Pubkey {
    w.leg_keys(mint, i)
        .quote_account(&companion::creator_address(mint))
}

// ---- Script steps -------------------------------------------------------------------------------

/// `ix`'s accounts and program as remaining accounts, nobody a signer.
fn metas(ix: &Instruction) -> Vec<AccountMeta> {
    let mut m: Vec<AccountMeta> = ix
        .accounts
        .iter()
        .map(|a| AccountMeta {
            pubkey: a.pubkey,
            is_signer: false,
            is_writable: a.is_writable,
        })
        .collect();
    m.push(AccountMeta::new_readonly(ix.program_id, false));
    m
}

/// What a leg step of the example script takes after its six (`[creator, mint, launch,
/// companion, pool, state]`): `aux`, the coin's hook slice (a trade), then the accounts of what the
/// step calls and the coin's `Legs` (where the script reads the leg's mint). Answers them and the
/// slice's length.
fn leg_step_accounts(
    w: &World,
    mint: &Pubkey,
    i: usize,
    mode: u8,
    aux: &Pubkey,
) -> (Vec<AccountMeta>, u8) {
    let creator = companion::creator_address(mint);
    let keys = w.leg_keys(mint, i);
    let account = keys.quote_account(&creator);
    let trade = matches!(mode, se::MODE_LEG_BUY | se::MODE_LEG_SELL);
    let slice = if trade { base_slice(w, mint) } else { vec![] };
    let n = slice.len() as u8;
    let mut extra = vec![AccountMeta::new(*aux, false)];
    extra.extend(slice.clone());
    if trade {
        extra.extend(metas(&launch::leg_swap_with_slice(
            &keys,
            creator,
            token::holding_address(mint, &creator),
            account,
            1,
            0,
            0,
            slice,
            w.launch(mint).rules.burns(),
        )));
    } else {
        extra.extend([
            AccountMeta::new(account, false),
            AccountMeta::new_readonly(keys.quote_mint, false),
            AccountMeta::new_readonly(keys.quote_program, false),
            AccountMeta::new_readonly(launch::legs_address(mint), false),
        ]);
    }
    (extra, n)
}

/// Rewrites the start of the script's state for `mint`.
fn set_state(w: &mut World, mint: &Pubkey, bytes: &[u8]) {
    let key = se::state_address(mint);
    let mut a = w.env.account(&key).expect("the script's state");
    a.data[..bytes.len()].copy_from_slice(bytes);
    w.env.put(key, a);
}

/// The `script_step` of a leg step: `mode` with `param` on leg `i`, `aux` its other account.
fn leg_step_ix(
    w: &mut World,
    cranker: &Pubkey,
    mint: &Pubkey,
    i: usize,
    mode: u8,
    param: u64,
    aux: &Pubkey,
) -> Instruction {
    let (extra, n) = leg_step_accounts(w, mint, i, mode, aux);
    set_state(w, mint, &se::leg_state_bytes(mode, param, i as u8, n));
    companion::script_step(*cranker, &LaunchKeys::of(&w.launch(mint)), SCRIPT, extra)
}

/// A leg step, sent and paid by `cranker`, an interval after the last.
fn leg_step(
    w: &mut World,
    cranker: &Keypair,
    mint: &Pubkey,
    i: usize,
    mode: u8,
    param: u64,
    aux: &Pubkey,
) -> Tx {
    w.env.warp(61);
    let ix = leg_step_ix(w, &cranker.pubkey(), mint, i, mode, param, aux);
    w.env.send_paid_by(&[ix], cranker, &[])
}

/// A step of one of the example script's SOL modes (`MODE_TAKE_LAMPORTS`, `MODE_NOTHING`), `aux`
/// its other account.
fn sol_step(
    w: &mut World,
    cranker: &Keypair,
    mint: &Pubkey,
    mode: u8,
    param: u64,
    aux: &Pubkey,
) -> Tx {
    w.env.warp(61);
    set_state(w, mint, &se::state_bytes(mode, param));
    let ix = companion::script_step(
        cranker.pubkey(),
        &LaunchKeys::of(&w.launch(mint)),
        SCRIPT,
        vec![AccountMeta::new(*aux, false)],
    );
    w.env.send_paid_by(&[ix], cranker, &[])
}

/// A script companion's coin quoted in `specs`, launched through `launch_token` (from `config`
/// when given, the hook prepared for it), every leg open, every leg traded, every leg's fee claimed
/// into the creator address's token accounts. Answers the coin, its launcher and its quotes.
fn companion_coin(
    w: &mut World,
    specs: &[Spec],
    config: Option<(&Pubkey, &Pubkey)>,
) -> (Pubkey, Keypair, Quotes) {
    let (quotes, launch_quotes) = quote_mints(w, specs);
    let launcher = w.env.funded(10 * SOL);
    let mint = script_companion(w, &launcher, 10_000);
    if let Some((_, collector)) = config {
        prepare_hook(w, &launcher, &mint.pubkey(), collector, true);
    }
    let ix = companion_launch_ix(
        w,
        &launcher.pubkey(),
        &mint.pubkey(),
        &launch_quotes,
        LaunchRules::NONE,
        config.map(|c| c.0),
    );
    w.env.send_paid_by(&[ix], &launcher, &[&mint]).ok();
    let mint = mint.pubkey();
    open_legs(w, &mint);
    trade_every_leg(w, &mint, &quotes);
    let cranker = w.env.funded(SOL);
    for i in 0..quotes.len() {
        claim(w, &cranker, &mint, i).ok();
    }
    (mint, launcher, quotes)
}

// ---- Tests --------------------------------------------------------------------------------------

#[test]
fn a_script_companion_launches_a_one_leg_coin_and_its_script_spends_the_fees_in_tokens() {
    let mut w = world();
    let (quotes, launch_quotes) = quote_mints(&mut w, &ONE_SPL);
    let launcher = w.env.funded(10 * SOL);
    let mint_kp = script_companion(&mut w, &launcher, 10_000);
    let mint = mint_kp.pubkey();
    let creator = companion::creator_address(&mint);
    let ix = companion_launch_ix(
        &w,
        &launcher.pubkey(),
        &mint,
        &launch_quotes,
        LaunchRules::NONE,
        None,
    );
    let tx = w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]);
    tx.ok();
    // The launchpad's launch, its creator the companion's creator address.
    let created: TokenLaunchCreated = tx.event();
    assert_eq!(created.creator, creator);
    assert_eq!(created.legs.len(), 1);
    let launched: CompanionLaunched = tx.event();
    assert_eq!(launched.mint, mint);
    let native: CompanionNativeSet = tx.event();
    assert!(native.native);
    let l = w.launch(&mint);
    assert!(l.is_token_quoted() && !l.is_native());
    assert_eq!(l.creator, creator);
    assert_eq!(l.custom_hook, None);
    assert_eq!(l.author_share_bps, 0);
    // The companion: launched, native from here, no SOL set aside but the funding's spare (the
    // launcher's), no buyback reference.
    let c = companion_of(&w, &mint);
    let rent0 = w.env.rent(0);
    assert!(c.launched && c.is_native());
    assert_eq!((c.pending_pot, c.reference_price), (0, 0));
    assert_eq!(c.pending_beneficiary, w.env.lamports(&creator) - rent0);
    assert_eq!(c.pending_beneficiary, native.spare);

    w.open_all_legs(&mint);
    let t = trader(&mut w, &quotes);
    buy(&mut w, &t, &mint, 0, 500_000_000).ok();
    let coins = w.env.holding(&mint, &t.pubkey());
    sell(&mut w, &t, &mint, 0, coins / 2).ok();
    let fees = w.env.token_amount(&w.leg_keys(&mint, 0).fee_account());
    assert!(fees > 0);

    // Anyone claims the leg's fee into the creator address's associated token account (made by
    // the claim, the sender paying): a PDA's ATA, owned by the creator address.
    let cranker = w.env.funded(SOL);
    let ata = creator_account(&w, &mint, 0);
    assert!(w.env.account(&ata).is_none());
    let tx = claim(&mut w, &cranker, &mint, 0);
    tx.ok();
    let ev: LegCreatorFeesClaimed = tx.event();
    assert_eq!(
        (ev.creator, ev.destination, ev.amount),
        (creator, ata, fees)
    );
    assert_eq!(w.env.token_amount(&ata), fees);
    assert_eq!(token_owner(&w, &ata), creator);
    assert_eq!(w.env.account(&ata).unwrap().owner, TOKEN);
    assert_eq!(w.env.token_amount(&w.leg_keys(&mint, 0).fee_account()), 0);
    refused_l(
        &claim(&mut w, &cranker, &mint, 0),
        LaunchError::NothingToClaim,
    );
    // More fees, claimed into the same account.
    buy(&mut w, &t, &mint, 0, 200_000_000).ok();
    let more = w.env.token_amount(&w.leg_keys(&mint, 0).fee_account());
    claim(&mut w, &cranker, &mint, 0).ok();
    let fees = fees + more;
    assert_eq!(w.env.token_amount(&ata), fees);

    // Not before a minute after the launch.
    assert!(w.env.now < companion_of(&w, &mint).launched_at + SCRIPT_AFTER_LAUNCH);
    let ix = leg_step_ix(
        &mut w,
        &cranker.pubkey(),
        &mint,
        0,
        se::MODE_NOTHING,
        0,
        &Pubkey::new_unique(),
    );
    refused_c(
        &w.env.send_paid_by(&[ix], &cranker, &[]),
        CompanionError::ScriptNotDue,
    );

    // The script buys the coin on the leg with half of those tokens: the coin to the creator's
    // holding, the tokens out of its account; the SOL pot stays empty, no bounty.
    let lamports = w.env.lamports(&creator);
    let half = fees / 2;
    let tx = leg_step(
        &mut w,
        &cranker,
        &mint,
        0,
        se::MODE_LEG_BUY,
        half,
        &Pubkey::new_unique(),
    );
    tx.ok();
    let stepped: ScriptStepped = tx.event();
    assert_eq!((stepped.bounty, stepped.pot_after), (0, 0));
    assert_eq!(stepped.tokens_before, 0);
    assert!(stepped.tokens_after > 0);
    assert_eq!(w.env.token_amount(&ata), fees - half);
    assert_eq!(w.env.holding(&mint, &creator), stepped.tokens_after);
    assert_eq!(w.env.lamports(&creator), lamports);
    w.env.assert_spl_invariant(&w.leg_keys(&mint, 0).pool());

    // It pays a holder in the leg's token.
    let holder = Pubkey::new_unique();
    let holder_account = w.env.token_account(&holder, &quotes[0].0, &quotes[0].1, 0);
    let pay = (fees - half) / 4;
    leg_step(
        &mut w,
        &cranker,
        &mint,
        0,
        se::MODE_LEG_PAY,
        pay,
        &holder_account,
    )
    .ok();
    assert_eq!(w.env.token_amount(&holder_account), pay);
    assert_eq!(w.env.token_amount(&ata), fees - half - pay);

    // It sells what it bought: the leg's token back into its account.
    let before = w.env.token_amount(&ata);
    leg_step(
        &mut w,
        &cranker,
        &mint,
        0,
        se::MODE_LEG_SELL,
        0,
        &Pubkey::new_unique(),
    )
    .ok();
    assert_eq!(w.env.holding(&mint, &creator), 0);
    assert!(w.env.token_amount(&ata) > before);
    assert_eq!(w.env.lamports(&creator), lamports);
    assert_eq!(companion_of(&w, &mint).pending_pot, 0);

    // `claim_fees` has no SOL to claim on such a coin: nothing to split.
    refused_c(
        &w.env.send_paid_by(
            &[companion::claim_fees_script(cranker.pubkey(), mint, None)],
            &cranker,
            &[],
        ),
        CompanionError::NothingToDo,
    );
    // The launcher's dev buy has no SOL pool to buy on.
    let keys = LaunchKeys::of(&w.launch(&mint));
    refused_c(
        &w.env.send_paid_by(
            &[companion::dev_buy(launcher.pubkey(), &keys, SOL / 10, 1)],
            &launcher,
            &[],
        ),
        CompanionError::NotOnTokenCoin,
    );
}

#[test]
fn sol_that_reaches_the_creator_address_is_the_pot_and_a_script_spends_only_that() {
    let mut w = world();
    let (mint, _launcher, _quotes) = companion_coin(&mut w, &ONE_SPL, None);
    let creator = companion::creator_address(&mint);
    let cranker = w.env.funded(10 * SOL);
    let aux = Pubkey::new_unique();
    w.env.fund(aux, w.env.rent(0));
    // With an empty pot the script moves no SOL: not a lamport above the spare (the launcher's)...
    refused_c(
        &sol_step(&mut w, &cranker, &mint, se::MODE_TAKE_LAMPORTS, 1, &aux),
        CompanionError::ScriptOverspent,
    );
    // ...nor the creator address's rent.
    let all = w.env.lamports(&creator);
    refused_c(
        &sol_step(&mut w, &cranker, &mint, se::MODE_TAKE_LAMPORTS, all, &aux),
        CompanionError::ScriptOverspent,
    );
    // SOL sent to the creator address (a script's own sale for SOL elsewhere, or anyone's) is
    // booked to the pot by `claim_fees` (its bounty out of it), and the script spends within it.
    let ix = anchor_lang::solana_program::system_instruction::transfer(
        &cranker.pubkey(),
        &creator,
        SOL / 2,
    );
    w.env.send_paid_by(&[ix], &cranker, &[]).ok();
    let tx = w.env.send_paid_by(
        &[companion::claim_fees_script(cranker.pubkey(), mint, None)],
        &cranker,
        &[],
    );
    tx.ok();
    let funded: PotFunded = tx.event();
    let bounty = SOL / 2 * 50 / 10_000;
    assert_eq!(funded.to_pot, SOL / 2 - bounty);
    assert_eq!(companion_of(&w, &mint).pending_pot, SOL / 2 - bounty);
    let pot = SOL / 2 - bounty;
    sol_step(
        &mut w,
        &cranker,
        &mint,
        se::MODE_TAKE_LAMPORTS,
        SOL / 10,
        &aux,
    )
    .ok();
    let after = companion_of(&w, &mint).pending_pot;
    assert_eq!(after, pot - SOL / 10 - se_bounty());
    refused_c(
        &sol_step(
            &mut w,
            &cranker,
            &mint,
            se::MODE_TAKE_LAMPORTS,
            after + 1,
            &aux,
        ),
        CompanionError::ScriptOverspent,
    );
}

/// The flat bounty of a step with no answer.
fn se_bounty() -> u64 {
    bordrless_companion::constants::SCRIPT_BOUNTY
}

#[test]
fn a_three_leg_coin_in_spl_token_and_token_2022_pays_its_script_in_every_leg_token() {
    let mut w = world();
    let (mint, _launcher, quotes) = companion_coin(&mut w, &THREE, None);
    let creator = companion::creator_address(&mint);
    let legs = w.legs(&mint);
    assert_eq!(legs.count, 3);
    // Every leg's fee in the creator address's account of that leg's token, the Token-2022 one
    // under Token-2022 with the mint's account extensions.
    for (i, (q, program)) in quotes.iter().enumerate() {
        let ata = creator_account(&w, &mint, i);
        let a = w.env.account(&ata).expect("claimed into");
        assert_eq!(a.owner, *program);
        assert_eq!(token_owner(&w, &ata), creator);
        assert_eq!(
            Pubkey::new_from_array(a.data[0..32].try_into().unwrap()),
            *q
        );
        assert!(w.env.token_amount(&ata) > 0);
        assert_eq!(legs.legs[i].creator_fees_claimed, w.env.token_amount(&ata));
        if *program == TOKEN_2022 {
            assert!(a.data.len() > 165 && a.data[165] == 2);
        }
    }
    let cranker = w.env.funded(SOL);
    // It buys on the Token-2022 leg with that leg's tokens.
    let t22 = creator_account(&w, &mint, 1);
    let have = w.env.token_amount(&t22);
    let tx = leg_step(
        &mut w,
        &cranker,
        &mint,
        1,
        se::MODE_LEG_BUY,
        have / 2,
        &Pubkey::new_unique(),
    );
    tx.ok();
    let bought = w.env.holding(&mint, &creator);
    assert!(bought > 0);
    assert_eq!(w.env.token_amount(&t22), have - have / 2);
    w.env.assert_spl_invariant(&w.leg_keys(&mint, 1).pool());
    // It pays a holder in the third leg's token.
    let holder = Pubkey::new_unique();
    let holder_account = w.env.token_account(&holder, &quotes[2].0, &quotes[2].1, 0);
    let third = creator_account(&w, &mint, 2);
    let pay = w.env.token_amount(&third) / 3;
    leg_step(
        &mut w,
        &cranker,
        &mint,
        2,
        se::MODE_LEG_PAY,
        pay,
        &holder_account,
    )
    .ok();
    assert_eq!(w.env.token_amount(&holder_account), pay);
    // It sells the coin on the first leg: the first leg's token back.
    let first = creator_account(&w, &mint, 0);
    let before = w.env.token_amount(&first);
    leg_step(
        &mut w,
        &cranker,
        &mint,
        0,
        se::MODE_LEG_SELL,
        bought,
        &Pubkey::new_unique(),
    )
    .ok();
    assert_eq!(w.env.holding(&mint, &creator), 0);
    assert!(w.env.token_amount(&first) > before);
    w.env.assert_spl_invariant(&w.leg_keys(&mint, 0).pool());
    assert_eq!(companion_of(&w, &mint).pending_pot, 0);
}

#[test]
fn a_script_keeps_the_creator_token_accounts_the_creators() {
    let mut w = world();
    let (mint, _launcher, quotes) = companion_coin(&mut w, &THREE, None);
    let cranker = w.env.funded(SOL);
    let thief = w.env.funded(SOL);
    // Handing an account over, a delegate, a close authority: refused on an SPL Token leg and on
    // the Token-2022 leg.
    for i in [0usize, 1] {
        let ata = creator_account(&w, &mint, i);
        let held = w.env.token_amount(&ata);
        for mode in [
            se::MODE_LEG_HAND_OVER,
            se::MODE_LEG_DELEGATE,
            se::MODE_LEG_CLOSE_AUTHORITY,
        ] {
            let tx = leg_step(&mut w, &cranker, &mint, i, mode, held, &thief.pubkey());
            if mode == se::MODE_LEG_HAND_OVER && quotes[i].1 == TOKEN_2022 {
                // A Token-2022 associated account is immutably owned: the token program refuses
                // the hand-over itself.
                tx.expect_fail();
                continue;
            }
            refused_c(&tx, CompanionError::ScriptBrokeCustody);
        }
        assert_eq!(w.env.token_amount(&ata), held);
        assert_eq!(token_owner(&w, &ata), companion::creator_address(&mint));
    }
    // Closing an emptied account (its rent): the script may spend every token, never the account.
    let ata = creator_account(&w, &mint, 0);
    let all = w.env.token_amount(&ata);
    let holder = w
        .env
        .token_account(&Pubkey::new_unique(), &quotes[0].0, &quotes[0].1, 0);
    leg_step(&mut w, &cranker, &mint, 0, se::MODE_LEG_PAY, all, &holder).ok();
    assert_eq!(w.env.token_amount(&ata), 0);
    let rent = w.env.lamports(&ata);
    refused_c(
        &leg_step(
            &mut w,
            &cranker,
            &mint,
            0,
            se::MODE_LEG_CLOSE,
            0,
            &thief.pubkey(),
        ),
        CompanionError::ScriptBrokeCustody,
    );
    assert_eq!(w.env.lamports(&ata), rent);
    // The emptied account takes the next claim.
    trade_every_leg(&mut w, &mint, &quotes);
    claim(&mut w, &cranker, &mint, 0).ok();
    assert!(w.env.token_amount(&ata) > 0);
}

#[test]
fn a_workshop_hook_runs_on_every_leg_and_learns_the_legs() {
    let mut w = world();
    let (quotes, launch_quotes) = quote_mints(&mut w, &THREE[..2]);
    let author = w.env.funded(SOL);
    let config = hook_config(&mut w, &author);
    let creator = trader(&mut w, &quotes);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let collector = Pubkey::new_unique();
    prepare_hook(&mut w, &creator, &mint, &collector, true);
    let (ix, _) = token_launch_ix(
        &w,
        &creator.pubkey(),
        &mint,
        &launch_quotes,
        LaunchRules::NONE,
        Some(&config),
    );
    let tx = w.env.send_paid_by(&[ix], &creator, &[&mint_kp]);
    tx.ok();
    let ev: TokenLaunchConfigured = tx.event();
    assert_eq!(
        (ev.config, ev.custom_hook, ev.custom_hook_flags),
        (config, Some(HOOK), HOOK_FLAGS)
    );
    assert!(tx.events::<TokenLaunchRules>().is_empty());
    let l = w.launch(&mint);
    assert_eq!(
        (l.config, l.custom_hook, l.custom_hook_flags),
        (config, Some(HOOK), HOOK_FLAGS)
    );
    assert_eq!((l.modules, l.author_share_bps), (0, 0));
    let m: Mint = w.env.read(&mint);
    assert_eq!(m.hook_program, Some(HOOK));
    assert_eq!(m.supply, SUPPLY);
    // The mint ran its hook (before the mint), the `Legs` handed to it.
    let script: hook_tester::Script = w.env.read(&tester::script_address(&mint));
    assert!(script.told_token(callback::BEFORE_MINT).is_some());
    assert!(hook_saw(&tx, &launch::legs_address(&mint)));
    // Each leg's deposit runs the hook (the launch's holding to the leg pool's).
    let payer = w.env.payer.pubkey();
    for i in 0..2 {
        let calls = hook_calls(&w, &mint);
        let tx = w.env.send(&[open_leg_ix(&w, &payer, &mint, i)], &[]);
        tx.ok();
        assert!(hook_saw(&tx, &launch::legs_address(&mint)));
        assert!(hook_calls(&w, &mint) > calls);
        let told = w
            .env
            .read::<hook_tester::Script>(&tester::script_address(&mint))
            .told_token(callback::BEFORE_TRANSFER)
            .expect("told");
        assert_eq!(told.destination_owner, w.leg_keys(&mint, i).pool());
        assert_eq!(told.amount, w.legs(&mint).legs[i].share);
    }
    // Its rule from here: a cut of every transfer to the collector.
    w.env
        .send(&[token::create_holding(payer, mint, collector)], &[])
        .ok();
    turn_cut_on(&mut w, &creator, &mint);
    let t = trader(&mut w, &quotes);
    for i in 0..2 {
        let pool = w.leg_keys(&mint, i).pool();
        // A buy: out of the leg pool's vault, the buyer gets the curve's output less the cut.
        let cut = w.env.holding(&mint, &collector);
        let had = w.env.holding(&mint, &t.pubkey());
        let vq = w.legs(&mint).legs[i].virtual_quote;
        let tx = buy(&mut w, &t, &mint, i, vq / 100);
        tx.ok();
        assert_eq!(w.env.holding(&mint, &collector), cut + CUT);
        let swapped: bordrless_swap::events::Swapped = tx.event();
        assert_eq!(swapped.cuts_out, CUT);
        assert_eq!(swapped.delivered_out, swapped.amount_out - CUT);
        assert_eq!(
            w.env.holding(&mint, &t.pubkey()),
            had + swapped.delivered_out
        );
        assert!(hook_saw(&tx, &launch::legs_address(&mint)));
        let told = w
            .env
            .read::<hook_tester::Script>(&tester::script_address(&mint))
            .told_token(callback::BEFORE_TRANSFER)
            .expect("told");
        assert_eq!(
            (told.source_owner, told.destination_owner),
            (pool, t.pubkey())
        );
        // A sell: into the vault, the cut out of what the seller sends.
        let coins = w.env.holding(&mint, &t.pubkey()) / 2;
        let tx = sell(&mut w, &t, &mint, i, coins);
        tx.ok();
        assert_eq!(w.env.holding(&mint, &collector), cut + 2 * CUT);
        assert!(hook_saw(&tx, &launch::legs_address(&mint)));
        let told = w
            .env
            .read::<hook_tester::Script>(&tester::script_address(&mint))
            .told_token(callback::BEFORE_TRANSFER)
            .expect("told");
        assert_eq!(
            (told.source_owner, told.destination_owner),
            (t.pubkey(), pool)
        );
        w.env.assert_spl_invariant(&pool);
    }
    // A wallet transfer: the same cut.
    let friend = Pubkey::new_unique();
    w.env
        .send(&[token::create_holding(payer, mint, friend)], &[])
        .ok();
    let cut = w.env.holding(&mint, &collector);
    let ix = token::transfer_with(
        t.pubkey(),
        token::holding_address(&mint, &t.pubkey()),
        token::holding_address(&mint, &friend),
        mint,
        Some(Hook::of(HOOK)),
        hook_accounts(&w, &mint).extras,
        50_000,
    );
    let tx = w.env.send_paid_by(&[ix], &t, &[]);
    tx.ok();
    assert!(hook_saw(&tx, &launch::legs_address(&mint)));
    assert_eq!(w.env.holding(&mint, &friend), 50_000 - CUT);
    assert_eq!(w.env.holding(&mint, &collector), cut + CUT);
    // The creator's fee on the quote side of every trade, claimed to the creator (a wallet).
    for i in 0..2 {
        let keys = w.leg_keys(&mint, i);
        let fees = w.env.token_amount(&keys.fee_account());
        assert!(fees > 0);
        claim(&mut w, &t, &mint, i).ok();
        assert_eq!(
            w.env.token_amount(&keys.quote_account(&creator.pubkey())),
            QUOTE_FUNDS + fees
        );
    }
}

#[test]
fn a_pair_a_script_companion_and_a_workshop_hook_on_a_coin_quoted_in_tokens() {
    let mut w = world();
    let author = w.env.funded(SOL);
    let config = hook_config(&mut w, &author);
    let collector = Pubkey::new_unique();
    let (mint, launcher, _quotes) =
        companion_coin(&mut w, &THREE[..2], Some((&config, &collector)));
    let creator = companion::creator_address(&mint);
    let l = w.launch(&mint);
    assert_eq!(
        (l.creator, l.custom_hook, l.config),
        (creator, Some(HOOK), config)
    );
    let payer = w.env.payer.pubkey();
    w.env
        .send(&[token::create_holding(payer, mint, collector)], &[])
        .ok();
    turn_cut_on(&mut w, &launcher, &mint);
    // The script buys on the Token-2022 leg with its tokens, the hook's slice on the coin's side:
    // the hook's rule runs on the companion's buy as on anyone's.
    let cranker = w.env.funded(SOL);
    let have = w.env.token_amount(&creator_account(&w, &mint, 1));
    let tx = leg_step(
        &mut w,
        &cranker,
        &mint,
        1,
        se::MODE_LEG_BUY,
        have / 2,
        &Pubkey::new_unique(),
    );
    tx.ok();
    assert!(hook_saw(&tx, &launch::legs_address(&mint)));
    assert_eq!(w.env.holding(&mint, &collector), CUT);
    let bought = w.env.holding(&mint, &creator);
    assert!(bought > 0);
    // And sells on the SPL Token leg.
    leg_step(
        &mut w,
        &cranker,
        &mint,
        0,
        se::MODE_LEG_SELL,
        0,
        &Pubkey::new_unique(),
    )
    .ok();
    assert_eq!(w.env.holding(&mint, &creator), 0);
    assert_eq!(w.env.holding(&mint, &collector), 2 * CUT);
}

#[test]
fn a_config_on_a_coin_quoted_in_tokens_is_held_to_the_rules() {
    let mut w = world();
    let (quotes, launch_quotes) = quote_mints(&mut w, &ONE_SPL);
    let author = w.env.funded(10 * SOL);
    let creator = trader(&mut w, &quotes);
    let collector = Pubkey::new_unique();
    // A launch from `config` by `who` for a fresh mint, the hook prepared (`legs` in its registry):
    // `edit` may change the instruction first.
    let try_launch = |w: &mut World,
                      who: &Keypair,
                      config: &Pubkey,
                      rules: LaunchRules,
                      legs: bool,
                      edit: &dyn Fn(&mut Instruction)|
     -> (Pubkey, Tx) {
        let mint = Keypair::new();
        if w.launch_config(config).custom_hook.is_some() {
            prepare_hook(w, who, &mint.pubkey(), &collector, legs);
        }
        let (mut ix, _) = token_launch_ix(
            w,
            &who.pubkey(),
            &mint.pubkey(),
            &launch_quotes,
            rules,
            Some(config),
        );
        edit(&mut ix);
        let tx = w.env.send_paid_by(&[ix], who, &[&mint]);
        (mint.pubkey(), tx)
    };
    let none = |_: &mut Instruction| {};

    // A config whose hook takes a SOL cut: nothing to pay it in.
    let cut_config = Keypair::new();
    w.env
        .send_paid_by(
            &[launch::create_quote_cut_config(
                author.pubkey(),
                cut_config.pubkey(),
                hook_config_args(LaunchRules::NONE),
                0,
                QuoteCut {
                    max_buy_bps: 100,
                    max_sell_bps: 100,
                },
            )],
            &author,
            &[&cut_config],
        )
        .ok();
    refused_l(
        &try_launch(
            &mut w,
            &creator,
            &cut_config.pubkey(),
            LaunchRules::NONE,
            true,
            &none,
        )
        .1,
        LaunchError::QuoteCutOnTokenLaunch,
    );
    // A listed config (its author's share of the creator fee): refused when someone else launches
    // from it (the share is paid by SOL claims); its author launches from it.
    let listed = Keypair::new();
    w.env
        .send_paid_by(
            &[launch::create_listed_config(
                author.pubkey(),
                listed.pubkey(),
                hook_config_args(LaunchRules::NONE),
                1_000,
            )],
            &author,
            &[&listed],
        )
        .ok();
    refused_l(
        &try_launch(
            &mut w,
            &creator,
            &listed.pubkey(),
            LaunchRules::NONE,
            true,
            &none,
        )
        .1,
        LaunchError::ConfigSharesOnTokenLaunch,
    );
    for (q, p) in &quotes {
        w.env.token_account(&author.pubkey(), q, p, QUOTE_FUNDS);
    }
    let (own, tx) = try_launch(
        &mut w,
        &author,
        &listed.pubkey(),
        LaunchRules::NONE,
        true,
        &none,
    );
    tx.ok();
    assert_eq!(w.launch(&own).author_share_bps, 0);
    // A partner config: refused.
    let partnered = Keypair::new();
    w.env
        .send_paid_by(
            &[launch::create_partner_config(
                author.pubkey(),
                partnered.pubkey(),
                hook_config_args(LaunchRules::NONE),
                0,
                QuoteCut::default(),
                Pubkey::new_unique(),
                1_000,
            )],
            &author,
            &[&partnered],
        )
        .ok();
    refused_l(
        &try_launch(
            &mut w,
            &creator,
            &partnered.pubkey(),
            LaunchRules::NONE,
            true,
            &none,
        )
        .1,
        LaunchError::ConfigSharesOnTokenLaunch,
    );
    // A hook whose registry does not list the coin's `Legs`.
    let config = hook_config(&mut w, &author);
    refused_l(
        &try_launch(&mut w, &creator, &config, LaunchRules::NONE, false, &none).1,
        LaunchError::HookMissingLegs,
    );
    // The config's terms not repeated: other rules (another creator fee below).
    let burn = LaunchRules {
        burn_buy_bps: 100,
        ..LaunchRules::NONE
    };
    refused_l(
        &try_launch(&mut w, &creator, &config, burn, true, &none).1,
        LaunchError::ConfigMismatch,
    );
    // The hook's accounts: none, one extra short, another hook.
    let config_at = |ix: &Instruction| {
        ix.accounts
            .iter()
            .position(|a| a.pubkey == config)
            .expect("the config")
    };
    refused_l(
        &try_launch(&mut w, &creator, &config, LaunchRules::NONE, true, &|ix| {
            let at = config_at(ix);
            ix.accounts.truncate(at + 1);
        })
        .1,
        LaunchError::CustomHookAccountsMissing,
    );
    refused_l(
        &try_launch(&mut w, &creator, &config, LaunchRules::NONE, true, &|ix| {
            ix.accounts.pop();
        })
        .1,
        LaunchError::HookExtrasMismatch,
    );
    refused_l(
        &try_launch(&mut w, &creator, &config, LaunchRules::NONE, true, &|ix| {
            let at = config_at(ix);
            ix.accounts[at + 1].pubkey = tax_hook::ID;
        })
        .1,
        LaunchError::InvalidCustomHook,
    );
    refused_l(
        &try_launch(&mut w, &creator, &config, LaunchRules::NONE, true, &|ix| {
            let at = config_at(ix);
            ix.accounts[at + 2].pubkey = token::hook_signer(&tax_hook::ID);
        })
        .1,
        LaunchError::WrongHookSigner,
    );
    // A hook not prepared for the mint (no registry).
    let mint = Keypair::new();
    let mints: Vec<Pubkey> = launch_quotes.iter().map(|q| q.0).collect();
    let ix = launch::create_token_launch_with(
        creator.pubkey(),
        mint.pubkey(),
        w.env.treasury.pubkey(),
        World::token_launch_args_with("BLDR", FEE, &launch_quotes, LaunchRules::NONE),
        &mints,
        Some(config),
        Some(&CustomHookAccounts {
            program: HOOK,
            extras: vec![],
        }),
    );
    refused_l(
        &w.env.send_paid_by(&[ix], &creator, &[&mint]),
        LaunchError::HookRegistryMissing,
    );
    // A config without a hook: its rules, with the kit's accounts after it when they install the
    // kit (and none after it otherwise).
    let (capped, tx) = w.create_config(
        &author,
        CreateConfigArgs {
            rules: LaunchRules {
                max_wallet_bps: 200,
                burn_sell_bps: 100,
                ..LaunchRules::NONE
            },
            creator_fee_bps: FEE,
            custom_hook: None,
            custom_hook_flags: 0,
            label: "Capped".to_string(),
        },
    );
    tx.ok();
    let rules = w.launch_config(&capped).rules;
    refused_l(
        &try_launch(&mut w, &creator, &capped, rules, true, &|ix| {
            ix.accounts.pop();
        })
        .1,
        LaunchError::KitAccountsMissing,
    );
    let (kit_coin, tx) = try_launch(&mut w, &creator, &capped, rules, true, &none);
    tx.ok();
    let l = w.launch(&kit_coin);
    assert_eq!((l.config, l.custom_hook, l.rules), (capped, None, rules));
    assert!(l.modules != 0);
    assert_eq!(tx.event::<TokenLaunchRules>().rules, rules);
    let (plain, tx) = w.create_config(
        &author,
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: FEE,
            custom_hook: None,
            custom_hook_flags: 0,
            label: "Plain".to_string(),
        },
    );
    tx.ok();
    refused_l(
        &try_launch(&mut w, &creator, &plain, LaunchRules::NONE, true, &|ix| {
            ix.accounts
                .push(AccountMeta::new_readonly(Pubkey::new_unique(), false));
        })
        .1,
        LaunchError::UnexpectedCustomHookAccounts,
    );
    let (plain_coin, tx) = try_launch(&mut w, &creator, &plain, LaunchRules::NONE, true, &none);
    tx.ok();
    assert_eq!(w.launch(&plain_coin).config, plain);
    // A config with a holder fee: none on a coin quoted in tokens.
    let (rewards, tx) = w.create_config(
        &author,
        CreateConfigArgs {
            rules: LaunchRules {
                holder_fee_buy_bps: 100,
                holder_fee_sell_bps: 100,
                ..LaunchRules::NONE
            },
            creator_fee_bps: FEE,
            custom_hook: None,
            custom_hook_flags: 0,
            label: "Rewards".to_string(),
        },
    );
    tx.ok();
    let rules = w.launch_config(&rewards).rules;
    // Since §7.5 it launches: the kit's holder rewards in the legs' tokens.
    try_launch(&mut w, &creator, &rewards, rules, true, &none)
        .1
        .ok();
    // The creator fee repeated wrongly.
    let mint = Keypair::new();
    prepare_hook(&mut w, &creator, &mint.pubkey(), &collector, true);
    let mints: Vec<Pubkey> = launch_quotes.iter().map(|q| q.0).collect();
    let args = World::token_launch_args_with("BLDR", FEE - 1, &launch_quotes, LaunchRules::NONE);
    let ix = launch::create_token_launch_with(
        creator.pubkey(),
        mint.pubkey(),
        w.env.treasury.pubkey(),
        args,
        &mints,
        Some(config),
        Some(&hook_accounts(&w, &mint.pubkey())),
    );
    refused_l(
        &w.env.send_paid_by(&[ix], &creator, &[&mint]),
        LaunchError::ConfigMismatch,
    );
}

#[test]
fn open_leg_on_a_custom_hook_coin_takes_the_hooks_accounts() {
    let mut w = world();
    let (quotes, launch_quotes) = quote_mints(&mut w, &ONE_SPL);
    let author = w.env.funded(SOL);
    let config = hook_config(&mut w, &author);
    let creator = trader(&mut w, &quotes);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    prepare_hook(&mut w, &creator, &mint, &Pubkey::new_unique(), true);
    let (ix, _) = token_launch_ix(
        &w,
        &creator.pubkey(),
        &mint,
        &launch_quotes,
        LaunchRules::NONE,
        Some(&config),
    );
    w.env.send_paid_by(&[ix], &creator, &[&mint_kp]).ok();
    let payer = w.env.payer.pubkey();
    let keys = w.leg_keys(&mint, 0);
    let treasury = w.env.treasury.pubkey();
    refused_l(
        &w.env
            .send(&[launch::open_leg(payer, treasury, &keys, 0)], &[]),
        LaunchError::CustomHookAccountsMissing,
    );
    let mut other = hook_accounts(&w, &mint);
    other.program = tax_hook::ID;
    refused_l(
        &w.env.send(
            &[launch::open_leg_custom(payer, treasury, &keys, 0, &other)],
            &[],
        ),
        LaunchError::InvalidCustomHook,
    );
    let mut ix = open_leg_ix(&w, &payer, &mint, 0);
    let at = ix.accounts.len() - hook_accounts(&w, &mint).extras.len() - 1;
    ix.accounts[at].pubkey = token::hook_signer(&tax_hook::ID);
    refused_l(&w.env.send(&[ix], &[]), LaunchError::WrongHookSigner);
    w.env.send(&[open_leg_ix(&w, &payer, &mint, 0)], &[]).ok();
    assert_eq!(w.legs(&mint).opened, 1);
}

/// A classic companion for a fresh mint, its split `split`.
fn classic_companion(w: &mut World, launcher: &Keypair, split: Split) -> Keypair {
    let mint = Keypair::new();
    let args = CreateArgs {
        split,
        ..create_args()
    };
    w.env
        .send_paid_by(
            &[companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                mint.pubkey(),
                args,
            )],
            launcher,
            &[&mint],
        )
        .ok();
    mint
}

#[test]
fn launch_token_is_held_to_its_rules() {
    let mut w = world();
    let (_quotes, launch_quotes) = quote_mints(&mut w, &ONE_SPL);
    let launcher = w.env.funded(10 * SOL);
    let launch_of = |w: &World, mint: &Pubkey, rules: LaunchRules| {
        companion_launch_ix(w, &launcher.pubkey(), mint, &launch_quotes, rules, None)
    };
    // A game companion whose launch names no game hook (its game hook written in, no game made):
    // refused as `launch` refuses it. Games on coins quoted in tokens are §7.6's
    // (`token_quotes_games.rs`).
    let game = classic_companion(&mut w, &launcher, create_args().split);
    let key = companion::companion_address(&game.pubkey());
    let mut c: Companion = w.env.read(&key);
    c.game_hook = Pubkey::new_unique();
    let mut a = w.env.account(&key).expect("the companion");
    let mut data: &mut [u8] = &mut a.data;
    anchor_lang::AccountSerialize::try_serialize(&c, &mut data).expect("serialize");
    w.env.put(key, a);
    let ix = launch_of(&w, &game.pubkey(), LaunchRules::NONE);
    refused_c(
        &w.env.send_paid_by(&[ix], &launcher, &[&game]),
        CompanionError::GameHookMismatch,
    );
    // A holders' part (§7.5): it pays through the kit's holder rewards, so it needs them
    // (`HolderRewardsOff` without a holder fee, as `launch`); with them it launches.
    let holders = classic_companion(
        &mut w,
        &launcher,
        Split {
            buyback_bps: 4_000,
            holders_bps: 3_000,
            beneficiary_bps: 3_000,
        },
    );
    let rewards = LaunchRules {
        holder_fee_buy_bps: 100,
        holder_fee_sell_bps: 100,
        ..LaunchRules::NONE
    };
    let ix = launch_of(&w, &holders.pubkey(), LaunchRules::NONE);
    refused_c(
        &w.env.send_paid_by(&[ix], &launcher, &[&holders]),
        CompanionError::HolderRewardsOff,
    );
    w.env.warp(1);
    let ix = launch_of(&w, &holders.pubkey(), rewards);
    w.env.send_paid_by(&[ix], &launcher, &[&holders]).ok();
    assert!(companion_of(&w, &holders.pubkey()).launched);
    // Payees: their parts are paid in SOL by `withdraw`.
    let payees = classic_companion(&mut w, &launcher, create_args().split);
    w.env
        .send_paid_by(
            &[companion::set_payees(
                launcher.pubkey(),
                payees.pubkey(),
                vec![bordrless_companion::instructions::PayeeArgs {
                    wallet: Pubkey::new_unique(),
                    bps: 1_000,
                }],
            )],
            &launcher,
            &[&payees],
        )
        .ok();
    // Payees (§7.5): paid in the legs' tokens by `withdraw_leg`, what they are owed kept in the
    // companion's `PayeesLegs`, which the launch makes (`PayeesLegsMissing` without it).
    let ix = launch_of(&w, &payees.pubkey(), LaunchRules::NONE);
    refused_c(
        &w.env.send_paid_by(&[ix], &launcher, &[&payees]),
        CompanionError::PayeesLegsMissing,
    );
    let mut ix = launch_of(&w, &payees.pubkey(), LaunchRules::NONE);
    ix.accounts.push(anchor_lang::prelude::AccountMeta::new(
        companion::payees_legs_address(&payees.pubkey()),
        false,
    ));
    w.env.send_paid_by(&[ix], &launcher, &[&payees]).ok();
    assert!(w
        .env
        .account(&companion::payees_legs_address(&payees.pubkey()))
        .is_some());
    // The launcher-only preset launches (its pots on the legs made).
    let classic = classic_companion(&mut w, &launcher, create_args().split);
    let ix = launch_of(&w, &classic.pubkey(), LaunchRules::NONE);
    w.env.send_paid_by(&[ix], &launcher, &[&classic]).ok();
    assert!(companion_of(&w, &classic.pubkey()).launched);
    assert!(w
        .env
        .account(&companion::companion_legs_address(&classic.pubkey()))
        .is_some());
    // A script whose pot is not the whole fee (§7.6): its pot and the launcher's part kept per leg.
    let split = script_companion(&mut w, &launcher, 7_000);
    let ix = companion_launch_ix(
        &w,
        &launcher.pubkey(),
        &split.pubkey(),
        &launch_quotes,
        LaunchRules::NONE,
        None,
    );
    w.env.send_paid_by(&[ix], &launcher, &[&split]).ok();
    assert!(w
        .env
        .account(&companion::companion_legs_address(&split.pubkey()))
        .is_some());
    let mint = script_companion(&mut w, &launcher, 10_000);
    // The creator wallet lock would lock the creator address.
    let locked = LaunchRules {
        creator_lock_secs: 86_400,
        ..LaunchRules::NONE
    };
    let ix = companion_launch_ix(
        &w,
        &launcher.pubkey(),
        &mint.pubkey(),
        &launch_quotes,
        locked,
        None,
    );
    refused_c(
        &w.env.send_paid_by(&[ix], &launcher, &[&mint]),
        CompanionError::CreatorLockUnsupported,
    );
    // Not `create_token_launch`'s accounts; another creator, another mint.
    let (mut inner, args) = token_launch_ix(
        &w,
        &launcher.pubkey(),
        &mint.pubkey(),
        &launch_quotes,
        LaunchRules::NONE,
        None,
    );
    let all = inner.accounts.clone();
    inner.accounts.truncate(4);
    let ix = companion::launch_token(launcher.pubkey(), mint.pubkey(), &inner, args.clone());
    refused_c(
        &w.env.send_paid_by(&[ix], &launcher, &[&mint]),
        CompanionError::MissingAccount,
    );
    inner.accounts = all;
    let ix = companion::launch_token(launcher.pubkey(), mint.pubkey(), &inner, args);
    refused_c(
        &w.env.send_paid_by(&[ix], &launcher, &[&mint]),
        CompanionError::WrongCreator,
    );
    let stranger = Keypair::new();
    let (inner, args) = token_launch_ix(
        &w,
        &companion::creator_address(&mint.pubkey()),
        &stranger.pubkey(),
        &launch_quotes,
        LaunchRules::NONE,
        None,
    );
    let ix = companion::launch_token(launcher.pubkey(), mint.pubkey(), &inner, args);
    refused_c(
        &w.env.send_paid_by(&[ix], &launcher, &[]),
        CompanionError::WrongMint,
    );
    // A listed config that would pay its author through a companion: the launchpad refuses it.
    let author = w.env.funded(SOL);
    let listed = Keypair::new();
    w.env
        .send_paid_by(
            &[launch::create_listed_config(
                author.pubkey(),
                listed.pubkey(),
                hook_config_args(LaunchRules::NONE),
                1_000,
            )],
            &author,
            &[&listed],
        )
        .ok();
    prepare_hook(
        &mut w,
        &launcher,
        &mint.pubkey(),
        &Pubkey::new_unique(),
        true,
    );
    let ix = companion_launch_ix(
        &w,
        &launcher.pubkey(),
        &mint.pubkey(),
        &launch_quotes,
        LaunchRules::NONE,
        Some(&listed.pubkey()),
    );
    refused_l(
        &w.env.send_paid_by(&[ix], &launcher, &[&mint]),
        LaunchError::ConfigSharesOnTokenLaunch,
    );
    // Launched once; never again, nor as a SOL coin.
    let ix = companion_launch_ix(
        &w,
        &launcher.pubkey(),
        &mint.pubkey(),
        &launch_quotes,
        LaunchRules::NONE,
        None,
    );
    w.env
        .send_paid_by(std::slice::from_ref(&ix), &launcher, &[&mint])
        .ok();
    w.env.warp(1);
    refused_c(
        &w.env.send_paid_by(&[ix], &launcher, &[&mint]),
        CompanionError::AlreadyLaunched,
    );
}

#[test]
fn claim_creator_fees_token_pays_only_the_creators_associated_account() {
    let mut w = world();
    let (mint, _launcher, quotes) = companion_coin(&mut w, &ONE_SPL, None);
    trade_every_leg(&mut w, &mint, &quotes);
    let cranker = w.env.funded(SOL);
    // Another creator (a wallet in the creator's place).
    let mut ix = claim_ix(&w, &cranker.pubkey(), &mint, 0);
    ix.accounts[5].pubkey = cranker.pubkey();
    ix.accounts[6].pubkey = w.leg_keys(&mint, 0).quote_account(&cranker.pubkey());
    refused_l(
        &w.env.send_paid_by(&[ix], &cranker, &[]),
        LaunchError::NotCreator,
    );
    // A token account of the creator address that is not its associated one: the ATA program
    // refuses the address.
    let mut ix = claim_ix(&w, &cranker.pubkey(), &mint, 0);
    ix.accounts[6].pubkey = Pubkey::new_unique();
    w.env.send_paid_by(&[ix], &cranker, &[]).expect_fail();
    claim(&mut w, &cranker, &mint, 0).ok();
}

// ---- Measures -----------------------------------------------------------------------------------

/// The protocol lookup table of the token-quote suites, with the companion's event authority and
/// the programs and fixed accounts a builder's transactions name but never invoke at top level.
fn builders_table(w: &World) -> Vec<Pubkey> {
    let mut keys = protocol_lookup_table(w);
    keys.extend([
        companion::event_authority(),
        bordrless_launch::ID,
        bordrless_swap::ID,
        bordrless_token::ID,
        TOKEN,
        TOKEN_2022,
        w.env.treasury.pubkey(),
    ]);
    keys
}

/// A coin's own table for its script steps (the keeper's `stepTables`): every account of `ix` that
/// neither signs nor is invoked at top level.
fn coin_table(ix: &Instruction) -> Vec<Pubkey> {
    let mut keys: Vec<Pubkey> = Vec::new();
    for a in &ix.accounts {
        if !a.is_signer && a.pubkey != ix.program_id && !keys.contains(&a.pubkey) {
            keys.push(a.pubkey);
        }
    }
    keys
}

#[test]
fn measures_builders_compute_and_sizes() {
    let mut w = world();
    let table = w
        .env
        .put_lookup_table(Pubkey::new_unique(), &builders_table(&w));
    let tables = [table];
    let budget = || [compute_unit_limit(400_000), compute_unit_price(10_000)];
    let author = w.env.funded(SOL);
    let config = hook_config(&mut w, &author);
    // The arguments of a launch quoted in `quotes`; with `long`, the longest metadata (a 32-byte
    // name, a 10-byte symbol, a 200-byte URI; the default URI is 66 bytes, an `ipfs://` CID's).
    let args_of = |quotes: &[Quote], long: bool| {
        let mut args = World::token_launch_args_with("BLDR", FEE, quotes, LaunchRules::NONE);
        if long {
            args.name = "N".repeat(32);
            args.symbol = "S".repeat(10);
            args.uri = "u".repeat(200);
        }
        args
    };
    let what = |legs: usize, hooked: bool, long: bool| {
        format!(
            "{legs} leg(s){}{}",
            if hooked {
                ", from a Workshop hook's config"
            } else {
                ""
            },
            if long {
                ", 32 B name, 10 B symbol, 200 B URI"
            } else {
                ""
            }
        )
    };
    let mut companions = Vec::new();
    for (legs, hooked, long) in [
        (1usize, false, false),
        (3, false, false),
        (6, false, false),
        (1, true, false),
        (3, true, false),
        (6, true, false),
        (6, true, true),
    ] {
        let (_quotes, launch_quotes) = quote_mints(&mut w, &SIX[..legs]);
        let launcher = w.env.funded(10 * SOL);
        let mint = script_companion(&mut w, &launcher, 10_000);
        let cfg = hooked.then_some(config);
        if hooked {
            prepare_hook(
                &mut w,
                &launcher,
                &mint.pubkey(),
                &Pubkey::new_unique(),
                true,
            );
        }
        let args = args_of(&launch_quotes, long);
        let mints: Vec<Pubkey> = launch_quotes.iter().map(|q| q.0).collect();
        let custom = hooked.then(|| hook_accounts(&w, &mint.pubkey()));
        let inner = launch::create_token_launch_with(
            companion::creator_address(&mint.pubkey()),
            mint.pubkey(),
            w.env.treasury.pubkey(),
            args.clone(),
            &mints,
            cfg,
            custom.as_ref(),
        );
        let ix = companion::launch_token(launcher.pubkey(), mint.pubkey(), &inner, args);
        let accounts = ix.accounts.len();
        let mut ixs = budget().to_vec();
        ixs.push(ix);
        let without = w.env.v0_size(&ixs, &launcher, &[&mint], &[]);
        let with_table = w.env.v0_size(&ixs, &launcher, &[&mint], &tables);
        // Past 1,232 bytes with the protocol table (six legs, a hook and the longest metadata), a
        // table that also holds the quote mints the launch form lists and the Workshop item's
        // accounts (its hook, the token program's signer for it, its config) takes it.
        let curated = if with_table > 1_232 {
            let mut keys = builders_table(&w);
            keys.extend(mints.iter().copied());
            keys.extend([HOOK, token::hook_signer(&HOOK), config]);
            Some(w.env.put_lookup_table(Pubkey::new_unique(), &keys))
        } else {
            None
        };
        let tx = match &curated {
            Some(t) => w
                .env
                .send_v0(&ixs, &launcher, &[&mint], std::slice::from_ref(t)),
            None => w.env.send_v0(&ixs, &launcher, &[&mint], &tables),
        };
        tx.ok();
        println!(
            "launch_token (companion), {}: {} CU, {accounts} accounts, {} B with the table{} / {without} B without",
            what(legs, hooked, long),
            tx.cu(),
            with_table,
            match &curated {
                Some(_) => format!(
                    " ({} B with one holding the quote mints and the Workshop item's accounts too)",
                    tx.size
                ),
                None => String::new(),
            },
        );
        assert!(tx.size <= 1_232);
        if !long {
            companions.push(mint.pubkey());
        }
    }
    // A wallet's launch from the hook's config, 1, 6 and 3 legs, then the 3 legs open with the hook.
    let mut hooked_coin = Pubkey::default();
    for (legs, long) in [(1usize, false), (6, false), (6, true), (3, false)] {
        let specs: &[Spec] = if legs == 3 { &THREE } else { &SIX[..legs] };
        let (quotes, launch_quotes) = quote_mints(&mut w, specs);
        let creator = trader(&mut w, &quotes);
        let mint = Keypair::new();
        prepare_hook(
            &mut w,
            &creator,
            &mint.pubkey(),
            &Pubkey::new_unique(),
            true,
        );
        let mints: Vec<Pubkey> = launch_quotes.iter().map(|q| q.0).collect();
        let ix = launch::create_token_launch_with(
            creator.pubkey(),
            mint.pubkey(),
            w.env.treasury.pubkey(),
            args_of(&launch_quotes, long),
            &mints,
            Some(config),
            Some(&hook_accounts(&w, &mint.pubkey())),
        );
        let accounts = ix.accounts.len();
        let mut ixs = budget().to_vec();
        ixs.push(ix);
        let without = w.env.v0_size(&ixs, &creator, &[&mint], &[]);
        let tx = w.env.send_v0(&ixs, &creator, &[&mint], &tables);
        tx.ok();
        println!(
            "create_token_launch (a wallet), {}: {} CU, {accounts} accounts, {} B with the table / {without} B without",
            what(legs, true, long),
            tx.cu(),
            tx.size,
        );
        assert!(tx.size <= 1_232);
        hooked_coin = mint.pubkey();
    }
    let payer = w.env.payer.insecure_clone();
    for (i, name) in [
        (0usize, "SPL Token"),
        (1, "Token-2022 xStocks-like"),
        (2, "SPL Token"),
    ] {
        let ix = open_leg_ix(&w, &payer.pubkey(), &hooked_coin, i);
        let accounts = ix.accounts.len();
        let mut ixs = budget().to_vec();
        ixs.push(ix);
        let without = w.env.v0_size(&ixs, &payer, &[], &[]);
        let tx = w.env.send_v0(&ixs, &payer, &[], &tables);
        tx.ok();
        println!(
            "open_leg with the Workshop hook ({name}): {} CU, {accounts} accounts, {} B with the table / {without} B without",
            tx.cu(),
            tx.size
        );
        assert!(tx.size <= 1_232);
    }
    // Leg trades with the hook's slice.
    let quotes: Quotes = (0..3)
        .map(|i| {
            let k = w.leg_keys(&hooked_coin, i);
            (k.quote_mint, k.quote_program)
        })
        .collect();
    let t = trader(&mut w, &quotes);
    for (i, name) in [(0usize, "SPL Token"), (1, "Token-2022 xStocks-like")] {
        let who = t.pubkey();
        let vq = w.legs(&hooked_coin).legs[i].virtual_quote;
        let mut ixs = budget().to_vec();
        ixs.push(token::create_holding(who, hooked_coin, who));
        ixs.push(leg_swap(&w, &who, &hooked_coin, i, 1, vq / 100));
        let accounts = ixs[3].accounts.len();
        let without = w.env.v0_size(&ixs, &t, &[], &[]);
        let tx = w.env.send_v0(&ixs, &t, &[], &tables);
        tx.ok();
        println!(
            "swap_spl buy with the Workshop hook's slice ({name}), with create_holding: {} CU, {accounts} accounts, {} B with the table / {without} B without",
            tx.cu(),
            tx.size
        );
        assert!(tx.size <= 1_232);
        let coins = w.env.holding(&hooked_coin, &who) / 2;
        let mut ixs = budget().to_vec();
        ixs.push(leg_swap(&w, &who, &hooked_coin, i, 0, coins));
        let without = w.env.v0_size(&ixs, &t, &[], &[]);
        let tx = w.env.send_v0(&ixs, &t, &[], &tables);
        tx.ok();
        println!(
            "swap_spl sell with the Workshop hook's slice ({name}): {} CU, {} B with the table / {without} B without",
            tx.cu(),
            tx.size
        );
        assert!(tx.size <= 1_232);
    }
    // The companion coins: legs open, traded, claimed into the creator address's accounts (made by
    // the claim), the script's steps.
    let mint = companions[1];
    open_legs(&mut w, &mint);
    let quotes: Quotes = (0..3)
        .map(|i| {
            let k = w.leg_keys(&mint, i);
            (k.quote_mint, k.quote_program)
        })
        .collect();
    trade_every_leg(&mut w, &mint, &quotes);
    let cranker = w.env.funded(10 * SOL);
    for (i, name) in [(0usize, "SPL Token"), (1, "Token-2022 xStocks-like")] {
        let ix = claim_ix(&w, &cranker.pubkey(), &mint, i);
        let accounts = ix.accounts.len();
        let mut ixs = budget().to_vec();
        ixs.push(ix);
        let without = w.env.v0_size(&ixs, &cranker, &[], &[]);
        let tx = w.env.send_v0(&ixs, &cranker, &[], &tables);
        tx.ok();
        println!(
            "claim_creator_fees_token into the companion's ATA, creating it ({name}): {} CU, {accounts} accounts, {} B with the table / {without} B without",
            tx.cu(),
            tx.size
        );
        assert!(tx.size <= 1_232);
    }
    claim(&mut w, &cranker, &mint, 2).ok();
    let holder_account = w
        .env
        .token_account(&Pubkey::new_unique(), &quotes[0].0, &quotes[0].1, 0);
    for (i, mode, aux, name) in [
        (
            0usize,
            se::MODE_LEG_BUY,
            Pubkey::new_unique(),
            "a buy on the SPL Token leg",
        ),
        (
            1,
            se::MODE_LEG_BUY,
            Pubkey::new_unique(),
            "a buy on the Token-2022 leg",
        ),
        (
            0,
            se::MODE_LEG_PAY,
            holder_account,
            "a payment in the SPL Token leg's token",
        ),
        (
            0,
            se::MODE_LEG_SELL,
            Pubkey::new_unique(),
            "a sell on the SPL Token leg",
        ),
    ] {
        w.env.warp(61);
        let param = match mode {
            se::MODE_LEG_SELL => 0,
            _ => w.env.token_amount(&creator_account(&w, &mint, i)) / 3,
        };
        let ix = leg_step_ix(&mut w, &cranker.pubkey(), &mint, i, mode, param, &aux);
        let accounts = ix.accounts.len();
        let coin = w
            .env
            .put_lookup_table(Pubkey::new_unique(), &coin_table(&ix));
        let mut ixs = budget().to_vec();
        ixs.push(ix);
        let without = w.env.v0_size(&ixs, &cranker, &[], &[]);
        let protocol = w.env.v0_size(&ixs, &cranker, &[], &tables);
        let tx = w.env.send_v0(&ixs, &cranker, &[], &[coin]);
        tx.ok();
        println!(
            "script_step, {name}: {} CU, {accounts} accounts, {} B with the coin's step table / {protocol} B with the protocol table / {without} B without",
            tx.cu(),
            tx.size
        );
        assert!(tx.size <= 1_232);
    }
    for (name, size) in [
        (
            "bordrless_launch",
            std::fs::read(bordrless_program_tests::sbf_out_dir().join("bordrless_launch.so"))
                .map(|b| b.len()),
        ),
        (
            "bordrless_companion",
            std::fs::read(bordrless_program_tests::sbf_out_dir().join("bordrless_companion.so"))
                .map(|b| b.len()),
        ),
    ] {
        println!("{name}.so: {} bytes", size.unwrap_or(0));
    }
}
