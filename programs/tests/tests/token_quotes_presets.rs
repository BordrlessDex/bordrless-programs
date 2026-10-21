//! The presets on coins quoted in tokens (`docs/token-quotes.md` §7.4, 21 Oct 2026, the owner: "DIY
//! configs should be available for all quotes: if the quote token is SPY, it should pay in SPY").
//! A preset companion (the launch form's Buyback, its launcher's part, a Split of the two, a
//! Snowball) launches a coin quoted in one token and in three (SPL Token and an xStocks-like
//! Token-2022 mint) through `launch_token`, which makes its pots on the legs (`CompanionLegs`):
//!
//! - **Claims** (`claim_fees_leg`): every leg's creator fee into the creator address's associated
//!   token account of the leg's token, split there by the companion's split into that leg's parts,
//!   the bounty in the leg's token to the sender's account of it (none passed, none paid).
//! - **The launcher's part** (`withdraw_leg`): paid in each leg's token to the launcher's
//!   associated token account of it (made when missing).
//! - **Buybacks** (`buyback_leg`): on the leg, from its part, within the pool share, with a minimum
//!   out of the leg's own curve; the coins bought burned. A price more than 3% above the leg's
//!   reference waits (the reference catching up), as on a SOL coin.
//! - **Snowball** (`snowball_step_leg`): every leg with its own pot and coins: a buy on a drop, a
//!   sell on a rise, the rebalancing trade of a flat market, a follow when the side has nothing.
//! - **With the coin's rules**: the kit's (burns, max wallet, the early-buyer window) and a
//!   Workshop hook's (`hook_tester`, its cut on every transfer), on the leg trades and the burns.
//! - **Refusals**: the SOL steps on such a coin (`NotOnTokenCoin`), a leg it does not have
//!   (`NoSuchLeg`), a step too early (`BuybackNotDue`, `SnowballNotDue`), nothing to do, another
//!   launcher, a leg step on a SOL coin (no pots on the legs).
//!
//! SOL coins' presets are unchanged: `token_quotes_sol_differential.rs` runs them against the
//! deployed programs. The figures (compute units, transaction sizes) are printed by
//! `measures_presets_compute_and_sizes` (`--nocapture`).

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_companion::client as companion;
use bordrless_companion::constants::{
    BUYBACK_AFTER_LAUNCH, BUYBACK_SLIPPAGE_BPS, LEG_CURRENCY_TOKEN, MAX_PREMIUM_BPS,
};
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::{
    CompanionLaunched, LegBeneficiaryPaid, LegBoughtBack, LegBuybackWaited, LegFeesClaimed,
    LegSnowballFollowed, LegSnowballTraded,
};
use bordrless_companion::instructions::{CreateArgs, CreateSnowballArgs};
use bordrless_companion::state::{spot_price, Companion, CompanionLegs, Split};
use bordrless_core::policy;
use bordrless_hook::{token_flags, Delta, HookReturn};
use bordrless_launch::client::{self as launch, CustomHookAccounts, LaunchKeys, LegKeys};
use bordrless_launch::instructions::{legs_registry_entry, CreateConfigArgs};
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::hooks::fixed;
use bordrless_program_tests::launch::protocol_lookup_table;
use bordrless_program_tests::quotes::{
    pack_mint, xstock_extensions, Quote, QUOTE_FUNDS, TOKEN, TOKEN_2022,
};
use bordrless_swap::state::Pool;
use bordrless_token::client as token;
use bordrless_token::state::Mint;
use hook_tester::callback;
use hook_tester::client as tester;
use solana_keypair::Keypair;
use solana_message::AddressLookupTableAccount;
use solana_signer::Signer;

const SOL: u64 = 1_000_000_000;
const SUPPLY: u64 = policy::TOKEN_SUPPLY;
const FEE: u16 = 200;
const BOUNTY_BPS: u16 = 50;
const HOOK: Pubkey = hook_tester::ID;
/// The Workshop hook's cut on every transfer of the coin (raw units), to its collector.
const CUT: u64 = 1_000;
const HOOK_FLAGS: u16 = token_flags::BEFORE_TRANSFER
    | token_flags::AFTER_TRANSFER
    | token_flags::BEFORE_MINT
    | token_flags::TRANSFER_RETURNS_DELTA;
/// The collector's holding among a token callback's accounts: the token program's five, the
/// hook's script (5), the coin's `Legs` (6), then it.
const CUT_INDEX: u8 = 7;
/// Anchor's `AccountNotInitialized`: a leg step of a companion with no pots on the legs.
const ACCOUNT_NOT_INITIALIZED: u32 = 3012;
/// Anchor's `ConstraintAddress`.
const CONSTRAINT_ADDRESS: u32 = 2012;

#[track_caller]
fn refused_c(tx: &Tx, e: CompanionError) {
    tx.expect_code(u32::from(e));
    let name = format!("Error Code: {e:?}");
    assert!(
        tx.logs().iter().any(|l| l.contains(&name)),
        "expected {e:?}\n{}",
        tx.logs().join("\n")
    );
}

// ---- Quotes -------------------------------------------------------------------------------------

/// One quote: its program, its decimals, whether it carries an xStock's extensions.
type Spec = (Pubkey, u8, bool);

/// A coin's quotes: `(mint, program)` per leg.
type Quotes = Vec<(Pubkey, Pubkey)>;

const ONE_SPL: [Spec; 1] = [(TOKEN, 6, false)];
const ONE_2022: [Spec; 1] = [(TOKEN_2022, 8, true)];
const THREE: [Spec; 3] = [(TOKEN, 6, false), (TOKEN_2022, 8, true), (TOKEN, 9, false)];
const SIX: [Spec; 6] = [
    (TOKEN, 6, false),
    (TOKEN_2022, 8, true),
    (TOKEN, 9, false),
    (TOKEN, 6, false),
    (TOKEN_2022, 6, false),
    (TOKEN_2022, 9, false),
];

/// The setups every preset runs over.
fn setups() -> [(&'static str, &'static [Spec]); 3] {
    [
        ("1 leg, SPL Token", &ONE_SPL),
        ("1 leg, Token-2022", &ONE_2022),
        ("3 legs, SPL Token and Token-2022", &THREE),
    ]
}

/// The whole cap of a $5,000 coin in a token of `decimals`.
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

// ---- Presets ------------------------------------------------------------------------------------

/// The launch form's presets a coin quoted in tokens takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Preset {
    /// The whole fee to buybacks.
    Buyback,
    /// The whole fee to the launcher.
    Launcher,
    /// 60% to buybacks, 40% to the launcher.
    Split,
    /// 70% the Snowball's pot, 30% the launcher's (flat trades on, or off).
    Snowball { flat: bool },
}

const PRESETS: [Preset; 4] = [
    Preset::Buyback,
    Preset::Launcher,
    Preset::Split,
    Preset::Snowball { flat: true },
];

fn split(buyback_bps: u16, beneficiary_bps: u16) -> Split {
    Split {
        buyback_bps,
        holders_bps: 0,
        beneficiary_bps,
    }
}

impl Preset {
    fn split(self) -> Split {
        match self {
            Preset::Buyback => split(10_000, 0),
            Preset::Launcher => split(0, 10_000),
            Preset::Split => split(6_000, 4_000),
            Preset::Snowball { .. } => split(0, 10_000),
        }
    }

    /// `(buyback, launcher, pot)` basis points of a claim after its bounty.
    fn parts(self) -> (u64, u64, u64) {
        match self {
            Preset::Buyback => (10_000, 0, 0),
            Preset::Launcher => (0, 10_000, 0),
            Preset::Split => (6_000, 4_000, 0),
            Preset::Snowball { .. } => (0, 3_000, 7_000),
        }
    }

    fn snowball(self) -> bool {
        matches!(self, Preset::Snowball { .. })
    }
}

fn snowball_args(flat: bool) -> CreateSnowballArgs {
    CreateSnowballArgs {
        split: split(0, 3_000),
        pot_bps: 7_000,
        buy_step_bps: 100,
        sell_step_bps: 100,
        buy_bps: 2_000,
        sell_bps: 2_000,
        interval: 60,
        max_trade: SOL,
        pool_bps: 500,
        flat_trades: flat,
    }
}

/// A preset companion for a fresh mint (`create`, and `create_snowball` for a Snowball).
fn preset_companion(w: &mut World, launcher: &Keypair, preset: Preset) -> Keypair {
    let mint = Keypair::new();
    let mut ixs = vec![companion::create(
        launcher.pubkey(),
        launcher.pubkey(),
        mint.pubkey(),
        CreateArgs {
            split: preset.split(),
            bounty_bps: BOUNTY_BPS,
            max_buyback: SOL,
            buyback_interval: 60,
            vest_secs: 0,
            fund: SOL,
        },
    )];
    if let Preset::Snowball { flat } = preset {
        ixs.push(companion::create_snowball(
            launcher.pubkey(),
            mint.pubkey(),
            snowball_args(flat),
        ));
    }
    w.env.send_paid_by(&ixs, launcher, &[&mint]).ok();
    mint
}

fn companion_of(w: &World, mint: &Pubkey) -> Companion {
    w.env.read(&companion::companion_address(mint))
}

fn pots(w: &World, mint: &Pubkey) -> CompanionLegs {
    w.env.read(&companion::companion_legs_address(mint))
}

// ---- The Workshop hook --------------------------------------------------------------------------

/// The Workshop hook prepared for `mint` (its registry: its script, the coin's `Legs`, the
/// collector's holding).
fn prepare_hook(w: &mut World, payer: &Keypair, mint: &Pubkey, collector: &Pubkey) {
    let extras = vec![
        legs_registry_entry(),
        fixed(token::holding_address(mint, collector), true),
    ];
    w.env
        .send_paid_by(
            &[tester::init_script(payer.pubkey(), *mint, extras)],
            payer,
            &[],
        )
        .ok();
}

/// A config naming the Workshop hook, made by `author`.
fn hook_config(w: &mut World, author: &Keypair) -> Pubkey {
    let (config, tx) = w.create_config(
        author,
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: FEE,
            custom_hook: Some(HOOK),
            custom_hook_flags: HOOK_FLAGS,
            label: "Workshop hook".to_string(),
        },
    );
    tx.ok();
    config
}

/// The hook's accounts for `mint` as a client resolves them from its registry.
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

/// Scripts the hook's cut on every transfer of `mint`: `CUT` to the collector's holding.
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

// ---- Coins --------------------------------------------------------------------------------------

/// A preset companion's coin quoted in tokens.
struct Coin {
    mint: Pubkey,
    launcher: Keypair,
    quotes: Quotes,
    rules: LaunchRules,
    /// The Workshop hook's accounts, when the coin has one.
    custom: Option<CustomHookAccounts>,
    /// Whom the hook's cut is paid (its holding listed in the hook's registry).
    collector: Pubkey,
    preset: Preset,
}

impl Coin {
    fn keys(&self, w: &World, i: usize) -> LegKeys {
        w.leg_keys(&self.mint, i)
    }

    fn creator(&self) -> Pubkey {
        companion::creator_address(&self.mint)
    }

    /// The creator address's associated token account of leg `i`'s token.
    fn creator_account(&self, w: &World, i: usize) -> Pubkey {
        self.keys(w, i).quote_account(&self.creator())
    }
}

/// `launch_token` of `mint` by `launcher` with `rules` (from `config` when given).
fn launch_ix(
    w: &World,
    launcher: &Pubkey,
    mint: &Pubkey,
    quotes: &[Quote],
    rules: LaunchRules,
    config: Option<&Pubkey>,
) -> Instruction {
    let creator = companion::creator_address(mint);
    let mints: Vec<Pubkey> = quotes.iter().map(|q| q.0).collect();
    let args = World::token_launch_args_with("PRST", FEE, quotes, rules);
    let custom = config.map(|_| hook_accounts(w, mint));
    let inner = launch::create_token_launch_with(
        creator,
        *mint,
        w.env.treasury.pubkey(),
        args.clone(),
        &mints,
        config.copied(),
        custom.as_ref(),
    );
    companion::launch_token(*launcher, *mint, &inner, args)
}

/// `open_leg(i)` of `mint`, with the hook's accounts when the coin has one.
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

/// A `preset` companion's coin quoted in `specs` with `rules` (and the Workshop hook's config,
/// `hooked`): launched through `launch_token`, no leg open yet.
fn launched_coin(
    w: &mut World,
    specs: &[Spec],
    preset: Preset,
    rules: LaunchRules,
    hooked: bool,
) -> Coin {
    let (quotes, launch_quotes) = quote_mints(w, specs);
    let launcher = w.env.funded(10 * SOL);
    let mint = preset_companion(w, &launcher, preset);
    let collector = Pubkey::new_unique();
    let config = if hooked {
        let author = w.env.funded(SOL);
        prepare_hook(w, &launcher, &mint.pubkey(), &collector);
        Some(hook_config(w, &author))
    } else {
        None
    };
    let ix = launch_ix(
        w,
        &launcher.pubkey(),
        &mint.pubkey(),
        &launch_quotes,
        rules,
        config.as_ref(),
    );
    let tx = w.env.send_paid_by(&[ix], &launcher, &[&mint]);
    tx.ok();
    tx.event::<CompanionLaunched>();
    let mint = mint.pubkey();
    let custom = hooked.then(|| hook_accounts(w, &mint));
    Coin {
        mint,
        launcher,
        quotes,
        rules,
        custom,
        collector,
        preset,
    }
}

/// Opens every leg of `coin` (trading opens with the last).
fn open_legs(w: &mut World, coin: &Coin) {
    let payer = w.env.payer.pubkey();
    for i in 0..coin.quotes.len() {
        let ix = open_leg_ix(w, &payer, &coin.mint, i);
        w.env.send(&[ix], &[]).ok();
    }
}

/// [`launched_coin`] with every leg open.
fn preset_coin(
    w: &mut World,
    specs: &[Spec],
    preset: Preset,
    rules: LaunchRules,
    hooked: bool,
) -> Coin {
    let coin = launched_coin(w, specs, preset, rules, hooked);
    open_legs(w, &coin);
    coin
}

/// The coin's token-hook slice on a leg trade.
fn base_slice(coin: &Coin) -> Vec<AccountMeta> {
    match &coin.custom {
        Some(h) => h.slice(),
        None => launch::leg_base_slice(&coin.mint, coin.rules.modules()),
    }
}

/// A trade on leg `i` by `t`, delivered to itself (a buy makes its holding first).
fn trade(w: &mut World, coin: &Coin, t: &Keypair, i: usize, direction: u8, amount: u64) -> Tx {
    let who = t.pubkey();
    let keys = coin.keys(w, i);
    let mut ixs = vec![];
    if direction == 1 {
        ixs.push(token::create_holding(who, coin.mint, who));
    }
    ixs.push(launch::leg_swap_with_slice(
        &keys,
        who,
        token::holding_address(&coin.mint, &who),
        keys.quote_account(&who),
        direction,
        amount,
        0,
        base_slice(coin),
        coin.rules.burns(),
    ));
    w.env.send_paid_by(&ixs, t, &[])
}

/// A buy of a fiftieth of every leg's virtual quote, then a sell of half of what it bought: every
/// leg's fee account holds creator fees and every price sits a little above its opening.
fn trade_every_leg(w: &mut World, coin: &Coin) -> Keypair {
    let t = trader(w, &coin.quotes);
    for i in 0..coin.quotes.len() {
        let vq = w.legs(&coin.mint).legs[i].virtual_quote;
        trade(w, coin, &t, i, 1, vq / 50).ok();
        let coins = w.env.holding(&coin.mint, &t.pubkey());
        trade(w, coin, &t, i, 0, coins / 2).ok();
    }
    t
}

/// A wallet with SOL and an empty associated token account of every leg's token (for bounties).
fn cranker(w: &mut World, coin: &Coin) -> Keypair {
    let kp = w.env.funded(10 * SOL);
    for (q, p) in &coin.quotes {
        w.env.token_account(&kp.pubkey(), q, p, 0);
    }
    kp
}

fn claim(w: &mut World, coin: &Coin, cranker: &Keypair, i: usize, bounty: bool) -> Tx {
    let ix = companion::claim_fees_leg(cranker.pubkey(), &coin.keys(w, i), i as u8, bounty);
    w.env.send_paid_by(&[ix], cranker, &[])
}

fn withdraw(w: &mut World, coin: &Coin, sender: &Keypair, i: usize) -> Tx {
    let ix = companion::withdraw_leg(
        sender.pubkey(),
        coin.launcher.pubkey(),
        &coin.keys(w, i),
        i as u8,
    );
    w.env.send_paid_by(&[ix], sender, &[])
}

fn buyback_ix(w: &World, coin: &Coin, cranker: &Pubkey, i: usize, bounty: bool) -> Instruction {
    companion::buyback_leg(
        *cranker,
        &coin.keys(w, i),
        i as u8,
        &coin.rules,
        coin.custom.as_ref(),
        bounty,
    )
}

fn buyback(w: &mut World, coin: &Coin, cranker: &Keypair, i: usize, bounty: bool) -> Tx {
    let ix = buyback_ix(w, coin, &cranker.pubkey(), i, bounty);
    w.env.send_paid_by(&[ix], cranker, &[])
}

fn snowball_ix(w: &World, coin: &Coin, cranker: &Pubkey, i: usize, bounty: bool) -> Instruction {
    companion::snowball_step_leg(
        *cranker,
        &coin.keys(w, i),
        i as u8,
        &coin.rules,
        coin.custom.as_ref(),
        bounty,
    )
}

fn snowball(w: &mut World, coin: &Coin, cranker: &Keypair, i: usize, bounty: bool) -> Tx {
    let ix = snowball_ix(w, coin, &cranker.pubkey(), i, bounty);
    w.env.send_paid_by(&[ix], cranker, &[])
}

fn leg_pool(w: &World, coin: &Coin, i: usize) -> Pool {
    w.env.read(&coin.keys(w, i).pool())
}

fn price(p: &Pool) -> u128 {
    spot_price(
        p.quote_reserve,
        p.virtual_quote,
        p.base_reserve,
        p.virtual_base,
    )
    .expect("a price")
}

fn supply(w: &World, mint: &Pubkey) -> u64 {
    w.env.read::<Mint>(mint).supply
}

/// Claims every leg (the bounty to `cranker`), checking the split; answers the claims.
fn claim_every_leg(w: &mut World, coin: &Coin, cranker: &Keypair, name: &str) -> Vec<u64> {
    let (buyback_bps, launcher_bps, pot_bps) = coin.preset.parts();
    let mut claimed = vec![];
    for i in 0..coin.quotes.len() {
        let bounty_to = coin.keys(w, i).quote_account(&cranker.pubkey());
        let before = w.env.token_amount(&bounty_to);
        let pot_before = pots(w, &coin.mint).legs[i];
        let tx = claim(w, coin, cranker, i, true);
        tx.ok();
        let e = tx.event::<LegFeesClaimed>();
        assert_eq!(e.index, i as u8, "{name}");
        assert_eq!(e.quote_mint, coin.quotes[i].0, "{name}");
        assert!(e.claimed > 0, "{name}");
        assert_eq!(
            e.bounty,
            e.claimed * u64::from(BOUNTY_BPS) / 10_000,
            "{name}"
        );
        assert_eq!(w.env.token_amount(&bounty_to) - before, e.bounty, "{name}");
        let rest = e.claimed - e.bounty;
        assert_eq!(e.to_holders, 0, "{name}");
        assert_eq!(e.to_beneficiary, rest * launcher_bps / 10_000, "{name}");
        if pot_bps > 0 {
            assert_eq!(e.to_buyback, 0, "{name}");
            assert_eq!(e.to_pot, rest - e.to_beneficiary, "{name}");
        } else {
            assert_eq!(e.to_pot, 0, "{name}");
            assert_eq!(e.to_buyback, rest - e.to_beneficiary, "{name}");
            if buyback_bps == 0 {
                assert_eq!(e.to_buyback, 0, "{name}");
            }
        }
        let pot = pots(w, &coin.mint).legs[i];
        assert_eq!(
            pot.pending_buyback - pot_before.pending_buyback,
            e.to_buyback,
            "{name}"
        );
        assert_eq!(
            pot.pending_beneficiary - pot_before.pending_beneficiary,
            e.to_beneficiary,
            "{name}"
        );
        assert_eq!(pot.pending_pot - pot_before.pending_pot, e.to_pot, "{name}");
        assert_eq!(
            pot.claimed_total - pot_before.claimed_total,
            e.claimed,
            "{name}"
        );
        // The creator address's account of the leg's token holds the leg's parts, nothing else.
        assert_eq!(
            w.env.token_amount(&coin.creator_account(w, i)),
            pot.set_aside().expect("set aside"),
            "{name}"
        );
        claimed.push(e.claimed);
    }
    claimed
}

// ---- Tests --------------------------------------------------------------------------------------

#[test]
fn every_preset_launches_a_coin_quoted_in_tokens_with_its_pots_in_the_leg_tokens() {
    for (name, specs) in setups() {
        for preset in PRESETS {
            let name = format!("{name}, {preset:?}");
            let mut w = World::new();
            let (quotes, launch_quotes) = quote_mints(&mut w, specs);
            let launcher = w.env.funded(10 * SOL);
            let mint = preset_companion(&mut w, &launcher, preset);
            let m = mint.pubkey();
            let creator = companion::creator_address(&m);
            let lamports = w.env.lamports(&creator);
            let ix = launch_ix(
                &w,
                &launcher.pubkey(),
                &m,
                &launch_quotes,
                LaunchRules::NONE,
                None,
            );
            w.env.send_paid_by(&[ix], &launcher, &[&mint]).ok();
            let c = companion_of(&w, &m);
            assert!(c.launched && c.is_native(), "{name}");
            let l = w.launch(&m);
            assert!(l.is_token_quoted(), "{name}");
            assert_eq!(l.creator, creator, "{name}");
            let p = pots(&w, &m);
            let key = companion::companion_legs_address(&m);
            assert_eq!(p.mint, m, "{name}");
            assert_eq!(usize::from(p.count), quotes.len(), "{name}");
            let rent = w.env.rent(CompanionLegs::LEN);
            assert_eq!(w.env.lamports(&key), rent, "{name}");
            assert_eq!(
                w.env.account(&key).expect("pots").owner,
                bordrless_companion::ID,
                "{name}"
            );
            // The creator address paid the pots' rent (and the launch's fee and rent).
            assert!(w.env.lamports(&creator) + rent <= lamports, "{name}");
            let legs = w.legs(&m);
            for (i, (q, program)) in quotes.iter().enumerate() {
                let pot = p.legs[i];
                assert_eq!(pot.quote_mint, *q, "{name}");
                assert_eq!(
                    bordrless_swap::spl::program_id(pot.quote_program),
                    *program,
                    "{name}"
                );
                assert_eq!(pot.decimals, specs[i].1, "{name}");
                assert_eq!(pot.currency, LEG_CURRENCY_TOKEN, "{name}");
                assert_eq!(pot.set_aside(), Some(0), "{name}");
                assert_eq!(
                    pot.reference_price,
                    spot_price(0, legs.legs[i].virtual_quote, legs.legs[i].share, 0).unwrap(),
                    "{name}"
                );
                // The Snowball starts from the same opening price, in its own field (review3 F1).
                assert_eq!(pot.snowball_reference, pot.reference_price, "{name}");
                assert!(p.pays_in_token(i), "{name}");
            }
            for pot in &p.legs[quotes.len()..] {
                assert_eq!(pot.quote_mint, Pubkey::default(), "{name}");
            }
            // The opening price is the leg pool's own once it opens.
            let payer = w.env.payer.pubkey();
            for i in 0..quotes.len() {
                let ix = open_leg_ix(&w, &payer, &m, i);
                w.env.send(&[ix], &[]).ok();
                let pool: Pool = w.env.read(&w.leg_keys(&m, i).pool());
                assert_eq!(price(&pool), p.legs[i].reference_price, "{name}");
            }
        }
    }
}

#[test]
fn claims_split_every_leg_in_its_own_token_and_the_launcher_is_paid_in_it() {
    for (name, specs) in setups() {
        for preset in PRESETS {
            let name = format!("{name}, {preset:?}");
            let mut w = World::new();
            let coin = preset_coin(&mut w, specs, preset, LaunchRules::NONE, false);
            let t = trade_every_leg(&mut w, &coin);
            let c = cranker(&mut w, &coin);
            claim_every_leg(&mut w, &coin, &c, &name);
            // Nothing new: nothing to claim.
            refused_c(
                &claim(&mut w, &coin, &c, 0, true),
                CompanionError::NothingToDo,
            );
            // No account of the sender's for the bounty: none paid, the parts keep it.
            trade(&mut w, &coin, &t, 0, 1, 1_000_000).ok();
            let other = w.env.funded(SOL);
            let tx = claim(&mut w, &coin, &other, 0, false);
            tx.ok();
            let e = tx.event::<LegFeesClaimed>();
            assert_eq!(e.bounty, 0, "{name}");
            assert_eq!(
                e.to_buyback + e.to_beneficiary + e.to_pot,
                e.claimed,
                "{name}"
            );
            // The launcher's part, in each leg's token, to the launcher's associated account
            // (made by the sender).
            let sender = w.env.funded(SOL);
            for i in 0..coin.quotes.len() {
                let owed = pots(&w, &coin.mint).legs[i].pending_beneficiary;
                let to = coin.keys(&w, i).quote_account(&coin.launcher.pubkey());
                let tx = withdraw(&mut w, &coin, &sender, i);
                if owed == 0 {
                    refused_c(&tx, CompanionError::NothingToDo);
                    continue;
                }
                tx.ok();
                let e = tx.event::<LegBeneficiaryPaid>();
                assert_eq!((e.amount, e.destination), (owed, to), "{name}");
                assert_eq!(w.env.token_amount(&to), owed, "{name}");
                let pot = pots(&w, &coin.mint).legs[i];
                assert_eq!(pot.pending_beneficiary, 0, "{name}");
                assert_eq!(pot.paid_beneficiary_total, owed, "{name}");
                assert_eq!(
                    w.env.token_amount(&coin.creator_account(&w, i)),
                    pot.set_aside().unwrap(),
                    "{name}"
                );
                w.env.warp(1);
                refused_c(
                    &withdraw(&mut w, &coin, &sender, i),
                    CompanionError::NothingToDo,
                );
            }
            // Only to the launcher.
            let mut ix =
                companion::withdraw_leg(sender.pubkey(), sender.pubkey(), &coin.keys(&w, 0), 0);
            ix.accounts[3].pubkey = sender.pubkey();
            w.env
                .send_paid_by(&[ix], &sender, &[])
                .expect_code(CONSTRAINT_ADDRESS);
        }
    }
}

#[test]
fn a_buyback_buys_on_the_leg_within_its_curve_and_burns_what_it_bought() {
    for (name, specs) in setups() {
        for preset in [Preset::Buyback, Preset::Split] {
            let name = format!("{name}, {preset:?}");
            let mut w = World::new();
            let coin = preset_coin(&mut w, specs, preset, LaunchRules::NONE, false);
            let t = trade_every_leg(&mut w, &coin);
            let c = cranker(&mut w, &coin);
            claim_every_leg(&mut w, &coin, &c, &name);
            // Not within a minute of the launch.
            refused_c(
                &buyback(&mut w, &coin, &c, 0, true),
                CompanionError::BuybackNotDue,
            );
            w.env.warp(BUYBACK_AFTER_LAUNCH);
            for i in 0..coin.quotes.len() {
                let pot = pots(&w, &coin.mint).legs[i];
                let pool = leg_pool(&w, &coin, i);
                // Within 3% of the opening price after the traders' round trip.
                assert!(
                    price(&pool)
                        <= pot.reference_price * u128::from(10_000 + MAX_PREMIUM_BPS) / 10_000,
                    "{name}"
                );
                let supply_before = supply(&w, &coin.mint);
                let bounty_to = coin.keys(&w, i).quote_account(&c.pubkey());
                let bounty_before = w.env.token_amount(&bounty_to);
                let tx = buyback(&mut w, &coin, &c, i, true);
                tx.ok();
                let e = tx.event::<LegBoughtBack>();
                let total = pot.pending_buyback;
                assert_eq!(e.bounty, total * u64::from(BOUNTY_BPS) / 10_000, "{name}");
                assert_eq!(e.spent, total - e.bounty, "{name}");
                assert_eq!(
                    w.env.token_amount(&bounty_to) - bounty_before,
                    e.bounty,
                    "{name}"
                );
                // At least the leg's own curve, less its fees and the slippage.
                let net = e.spent * (10_000 - 200 - 25) / 10_000;
                let quote = bordrless_core::swap_out(
                    net,
                    pool.quote_reserve,
                    pool.virtual_quote,
                    pool.base_reserve,
                    pool.virtual_base,
                )
                .unwrap();
                assert!(
                    e.burned >= quote * (10_000 - BUYBACK_SLIPPAGE_BPS) / 10_000,
                    "{name}"
                );
                assert_eq!(supply_before - supply(&w, &coin.mint), e.burned, "{name}");
                assert_eq!(w.env.holding(&coin.mint, &coin.creator()), 0, "{name}");
                let after = pots(&w, &coin.mint).legs[i];
                assert_eq!(after.pending_buyback, 0, "{name}");
                assert_eq!(after.burned_total, e.burned, "{name}");
                assert_eq!(after.spent_total, e.spent, "{name}");
                assert!(after.reference_price <= pot.reference_price, "{name}");
                assert_eq!(e.price_after, price(&leg_pool(&w, &coin, i)), "{name}");
                assert_eq!(
                    w.env.token_amount(&coin.creator_account(&w, i)),
                    after.set_aside().unwrap(),
                    "{name}"
                );
                // Once an interval.
                refused_c(
                    &buyback(&mut w, &coin, &c, i, true),
                    CompanionError::BuybackNotDue,
                );
            }
            // A pump of more than 3% over the reference: the buyback waits, the reference
            // catching up an interval at a time; nothing to buy with, nothing to do.
            w.env.warp(61);
            let vq = w.legs(&coin.mint).legs[0].virtual_quote;
            trade(&mut w, &coin, &t, 0, 1, vq / 10).ok();
            claim(&mut w, &coin, &c, 0, true).ok();
            let reference = pots(&w, &coin.mint).legs[0].reference_price;
            let tx = buyback(&mut w, &coin, &c, 0, true);
            tx.ok();
            let e = tx.event::<LegBuybackWaited>();
            assert_eq!(e.reference, reference, "{name}");
            assert!(e.price > reference * 103 / 100, "{name}");
            assert!(tx.events::<LegBoughtBack>().is_empty(), "{name}");
            let moved = pots(&w, &coin.mint).legs[0].reference_price;
            assert!(moved > reference && moved < e.price, "{name}");
            if coin.quotes.len() > 1 {
                // Another leg's part is its own: nothing in leg 1 to buy with.
                w.env.warp(61);
                refused_c(
                    &buyback(&mut w, &coin, &c, 1, true),
                    CompanionError::NothingToDo,
                );
            }
        }
    }
}

#[test]
fn a_snowball_trades_every_leg_with_its_own_pot() {
    for (name, specs) in setups() {
        let name = format!("{name}, Snowball");
        let mut w = World::new();
        let coin = preset_coin(
            &mut w,
            specs,
            Preset::Snowball { flat: true },
            LaunchRules::NONE,
            false,
        );
        let t = trade_every_leg(&mut w, &coin);
        let c = cranker(&mut w, &coin);
        claim_every_leg(&mut w, &coin, &c, &name);
        refused_c(
            &snowball(&mut w, &coin, &c, 0, true),
            CompanionError::SnowballNotDue,
        );
        w.env.warp(BUYBACK_AFTER_LAUNCH);
        // A pump on leg 0 (its pool's quote side deep enough for the dump below).
        let vq = w.legs(&coin.mint).legs[0].virtual_quote;
        trade(&mut w, &coin, &t, 0, 1, vq / 10).ok();
        for i in 0..coin.quotes.len() {
            // A price above the reference with nothing held, or a little above it: flat trades
            // rebalance with a buy.
            let pot = pots(&w, &coin.mint).legs[i];
            let tx = snowball(&mut w, &coin, &c, i, true);
            tx.ok();
            let e = tx.event::<LegSnowballTraded>();
            assert_eq!((e.index, e.side, e.on_step), (i as u8, 1, false), "{name}");
            let total = pot.pending_pot * 2_000 / 10_000;
            assert_eq!(e.quote + e.bounty, total, "{name}");
            let after = pots(&w, &coin.mint).legs[i];
            assert_eq!(after.pending_pot, pot.pending_pot - total, "{name}");
            assert_eq!(after.tokens_held, e.tokens, "{name}");
            // The Snowball's own reference (review3 F1); the buyback's is untouched.
            assert_eq!(after.snowball_reference, e.price_after, "{name}");
            assert_eq!(after.reference_price, pot.reference_price, "{name}");
            assert_eq!(
                w.env.token_amount(&coin.creator_account(&w, i)),
                after.set_aside().unwrap(),
                "{name}"
            );
        }
        // Every leg's coins sit in the creator address's one holding.
        let held: u64 = pots(&w, &coin.mint)
            .legs
            .iter()
            .map(|p| p.tokens_held)
            .sum();
        assert_eq!(w.env.holding(&coin.mint, &coin.creator()), held, "{name}");
        refused_c(
            &snowball(&mut w, &coin, &c, 0, true),
            CompanionError::SnowballNotDue,
        );
        let others = pots(&w, &coin.mint).legs[1..].to_vec();
        // A dump on leg 0 (2% of its share, more than the buy step): a buy on the step.
        w.env.warp(61);
        let share = w.legs(&coin.mint).legs[0].share;
        let before = pots(&w, &coin.mint).legs[0];
        trade(&mut w, &coin, &t, 0, 0, share / 50).ok();
        assert!(price(&leg_pool(&w, &coin, 0)) < before.snowball_reference * 99 / 100);
        let tx = snowball(&mut w, &coin, &c, 0, true);
        tx.ok();
        let e = tx.event::<LegSnowballTraded>();
        assert_eq!((e.side, e.on_step), (1, true), "{name}");
        // A pump on leg 0 (more than the sell step): a sell on the step, back into its pot.
        w.env.warp(61);
        trade(&mut w, &coin, &t, 0, 1, vq / 5).ok();
        let before = pots(&w, &coin.mint).legs[0];
        let tx = snowball(&mut w, &coin, &c, 0, true);
        tx.ok();
        let e = tx.event::<LegSnowballTraded>();
        assert_eq!((e.side, e.on_step), (0, true), "{name}");
        let after = pots(&w, &coin.mint).legs[0];
        assert_eq!(e.tokens, before.tokens_held * 2_000 / 10_000, "{name}");
        assert_eq!(after.tokens_held, before.tokens_held - e.tokens, "{name}");
        assert_eq!(after.pending_pot, before.pending_pot + e.quote, "{name}");
        assert_eq!(
            w.env.token_amount(&coin.creator_account(&w, 0)),
            after.set_aside().unwrap(),
            "{name}"
        );
        // The other legs' pots and coins never moved.
        assert_eq!(pots(&w, &coin.mint).legs[1..], others[..], "{name}");
    }
    // Without flat trades: a price called for the sell side with no coins to sell, the reference
    // follows the price.
    let mut w = World::new();
    let coin = preset_coin(
        &mut w,
        &ONE_SPL,
        Preset::Snowball { flat: false },
        LaunchRules::NONE,
        false,
    );
    let t = trader(&mut w, &coin.quotes);
    let vq = w.legs(&coin.mint).legs[0].virtual_quote;
    trade(&mut w, &coin, &t, 0, 1, vq / 20).ok();
    let c = cranker(&mut w, &coin);
    claim(&mut w, &coin, &c, 0, true).ok();
    w.env.warp(BUYBACK_AFTER_LAUNCH);
    let reference = pots(&w, &coin.mint).legs[0].snowball_reference;
    let buyback_reference = pots(&w, &coin.mint).legs[0].reference_price;
    let tx = snowball(&mut w, &coin, &c, 0, true);
    tx.ok();
    let e = tx.event::<LegSnowballFollowed>();
    assert_eq!(e.reference_before, reference);
    // The Snowball's reference follows; the buyback's stays (review3 F1).
    assert_eq!(pots(&w, &coin.mint).legs[0].snowball_reference, e.price);
    assert_eq!(
        pots(&w, &coin.mint).legs[0].reference_price,
        buyback_reference
    );
    // Within its steps and without flat trades: not due.
    w.env.warp(61);
    refused_c(
        &snowball(&mut w, &coin, &c, 0, true),
        CompanionError::SnowballNotDue,
    );
}

#[test]
fn presets_run_with_the_kits_rules_and_a_workshop_hook() {
    let kit = LaunchRules {
        burn_buy_bps: 50,
        burn_sell_bps: 50,
        max_wallet_bps: 500,
        early_window_secs: 60,
        early_lock_secs: 3_600,
        ..LaunchRules::NONE
    };
    for (name, specs) in setups() {
        // The kit's rules: burns on every trade, max wallet, the early-buyer window.
        for preset in [Preset::Buyback, Preset::Snowball { flat: true }] {
            let name = format!("{name}, {preset:?}, kit rules");
            let mut w = World::new();
            let coin = launched_coin(&mut w, specs, preset, kit, false);
            assert!(w.launch(&coin.mint).has_kit(), "{name}");
            // Trading opens half a minute after the launch; its early-buyer window a minute
            // later. Small buys only (coins bought in the window are locked), every leg: each
            // price within 3% of its opening.
            w.env.warp(30);
            open_legs(&mut w, &coin);
            let t = trader(&mut w, &coin.quotes);
            for i in 0..coin.quotes.len() {
                let vq = w.legs(&coin.mint).legs[i].virtual_quote;
                trade(&mut w, &coin, &t, i, 1, vq / 200).ok();
            }
            let c = cranker(&mut w, &coin);
            claim_every_leg(&mut w, &coin, &c, &name);
            // A minute after the launch, the window still running: a Snowball waits for its end
            // (as on a SOL coin); a buyback does not (as on a SOL coin), and burns what it bought.
            w.env.warp(BUYBACK_AFTER_LAUNCH - 30 + 1);
            let first = if preset.snowball() {
                refused_c(
                    &snowball(&mut w, &coin, &c, 0, true),
                    CompanionError::SnowballNotDue,
                );
                0
            } else {
                let tx = buyback(&mut w, &coin, &c, 0, true);
                tx.ok();
                assert!(tx.event::<LegBoughtBack>().burned > 0, "{name}");
                1
            };
            w.env.warp(30);
            for i in first..coin.quotes.len() {
                let supply_before = supply(&w, &coin.mint);
                if preset.snowball() {
                    let tx = snowball(&mut w, &coin, &c, i, true);
                    tx.ok();
                    let e = tx.event::<LegSnowballTraded>();
                    assert_eq!(e.side, 1, "{name}");
                    // The pool hook's burn on the buy.
                    assert!(supply_before > supply(&w, &coin.mint), "{name}");
                } else {
                    let tx = buyback(&mut w, &coin, &c, i, true);
                    tx.ok();
                    let e = tx.event::<LegBoughtBack>();
                    // The pool hook's burn on the buy, then the buyback's.
                    assert!(supply_before - supply(&w, &coin.mint) > e.burned, "{name}");
                }
            }
        }
        // A Workshop hook: its cut on every transfer, the creator address's buys and sells too.
        for preset in [Preset::Buyback, Preset::Snowball { flat: true }] {
            let name = format!("{name}, {preset:?}, Workshop hook");
            let mut w = World::new();
            let coin = preset_coin(&mut w, specs, preset, LaunchRules::NONE, true);
            let collector = coin.collector;
            let launcher = coin.launcher.insecure_clone();
            trade_every_leg(&mut w, &coin);
            let c = cranker(&mut w, &coin);
            claim_every_leg(&mut w, &coin, &c, &name);
            w.env.warp(BUYBACK_AFTER_LAUNCH);
            // The collector's holding, then the cut on.
            let payer = w.env.payer.insecure_clone();
            w.env
                .send(
                    &[token::create_holding(payer.pubkey(), coin.mint, collector)],
                    &[],
                )
                .ok();
            turn_cut_on(&mut w, &launcher, &coin.mint);
            for i in 0..coin.quotes.len() {
                let calls = hook_calls(&w, &coin.mint);
                let cut_before = w.env.holding(&coin.mint, &collector);
                let tx = if preset.snowball() {
                    snowball(&mut w, &coin, &c, i, true)
                } else {
                    buyback(&mut w, &coin, &c, i, true)
                };
                tx.ok();
                assert!(hook_calls(&w, &coin.mint) > calls, "{name}");
                assert_eq!(
                    w.env.holding(&coin.mint, &collector) - cut_before,
                    CUT,
                    "{name}"
                );
                if !preset.snowball() {
                    let e = tx.event::<LegBoughtBack>();
                    assert!(e.burned > 0, "{name}");
                    assert_eq!(w.env.holding(&coin.mint, &coin.creator()), 0, "{name}");
                }
            }
        }
    }
}

#[test]
fn the_sol_steps_and_wrong_legs_are_refused() {
    let mut w = World::new();
    let coin = preset_coin(&mut w, &THREE, Preset::Split, LaunchRules::NONE, false);
    trade_every_leg(&mut w, &coin);
    w.env.warp(BUYBACK_AFTER_LAUNCH);
    let c = cranker(&mut w, &coin);
    let keys = LaunchKeys::of(&w.launch(&coin.mint));
    // The SOL steps: no pool in SOL, no SOL fees.
    for (what, ix) in [
        (
            "claim_fees",
            companion::claim_fees(c.pubkey(), coin.mint, None),
        ),
        ("buyback", companion::buyback(c.pubkey(), &keys, false)),
        ("share", companion::share(c.pubkey(), coin.mint)),
        (
            "dev_buy",
            companion::dev_buy(coin.launcher.pubkey(), &keys, SOL / 10, 1),
        ),
    ] {
        let signer = if what == "dev_buy" {
            &coin.launcher
        } else {
            &c
        };
        let tx = w
            .env
            .send_paid_by(&[companion::native(ix, &keys)], signer, &[]);
        refused_c(&tx, CompanionError::NotOnTokenCoin);
    }
    // A leg the coin does not have.
    let ix = companion::claim_fees_leg(c.pubkey(), &coin.keys(&w, 0), 3, true);
    refused_c(
        &w.env.send_paid_by(&[ix], &c, &[]),
        CompanionError::NoSuchLeg,
    );
    // A leg's accounts missing (another leg's passed): its fee account is not found.
    let ix = companion::claim_fees_leg(c.pubkey(), &coin.keys(&w, 1), 0, true);
    refused_c(
        &w.env.send_paid_by(&[ix], &c, &[]),
        CompanionError::MissingAccount,
    );
    // Nothing claimed yet: nothing to buy with, nothing to pay.
    refused_c(
        &buyback(&mut w, &coin, &c, 0, true),
        CompanionError::NothingToDo,
    );
    refused_c(&withdraw(&mut w, &coin, &c, 0), CompanionError::NothingToDo);
    // A Snowball's step on a companion that is not one: it has no Snowball terms.
    let ix = snowball_ix(&w, &coin, &c.pubkey(), 0, true);
    w.env
        .send_paid_by(&[ix], &c, &[])
        .expect_code(ACCOUNT_NOT_INITIALIZED);
    // The SOL Snowball's step on a Snowball's coin quoted in tokens.
    let snow = preset_coin(
        &mut w,
        &ONE_SPL,
        Preset::Snowball { flat: true },
        LaunchRules::NONE,
        false,
    );
    let skeys = LaunchKeys::of(&w.launch(&snow.mint));
    let ix = companion::native(companion::snowball_step(c.pubkey(), &skeys), &skeys);
    refused_c(
        &w.env.send_paid_by(&[ix], &c, &[]),
        CompanionError::NotOnTokenCoin,
    );
    // A leg step on a SOL coin's companion: it has no pots on the legs.
    let launcher = w.env.funded(10 * SOL);
    let mint = preset_companion(&mut w, &launcher, Preset::Buyback);
    let m = mint.pubkey();
    let creator = companion::creator_address(&m);
    let args = World::launch_args("SOLC", FEE, 30 * SOL, LaunchRules::NONE);
    let inner = launch::create_launch_with(
        creator,
        m,
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        args.clone(),
        None,
        None,
    );
    w.env
        .send_paid_by(
            &[companion::launch(launcher.pubkey(), m, &inner, args)],
            &launcher,
            &[&mint],
        )
        .ok();
    let sol_keys = LegKeys {
        mint: m,
        quote_mint: coin.quotes[0].0,
        quote_program: TOKEN,
        lp_fee_bps: policy::LP_FEE_BPS,
    };
    let ix = companion::claim_fees_leg(c.pubkey(), &sol_keys, 0, false);
    w.env
        .send_paid_by(&[ix], &c, &[])
        .expect_code(ACCOUNT_NOT_INITIALIZED);
}

// ---- Measures -----------------------------------------------------------------------------------

/// The protocol lookup table of the token-quote suites, with the companion's event authority and
/// the programs and fixed accounts the steps name but never invoke at top level.
fn presets_table(w: &World) -> Vec<Pubkey> {
    let mut keys = protocol_lookup_table(w);
    keys.extend([
        companion::event_authority(),
        bordrless_launch::ID,
        bordrless_swap::ID,
        bordrless_token::ID,
        TOKEN,
        TOKEN_2022,
        bordrless_swap::spl::ATA_PROGRAM_ID,
        w.env.treasury.pubkey(),
    ]);
    keys
}

/// A coin's own table (the keeper's per-coin `stepTables`): every account of `ix` that neither
/// signs nor is invoked at top level.
fn coin_table(ix: &Instruction) -> Vec<Pubkey> {
    let mut keys: Vec<Pubkey> = Vec::new();
    for a in &ix.accounts {
        if !a.is_signer && a.pubkey != ix.program_id && !keys.contains(&a.pubkey) {
            keys.push(a.pubkey);
        }
    }
    keys
}

/// Sends `ix` with a compute budget, by `payer`, with the protocol table, or with the coin's own
/// table when past 1,232 bytes; prints its figures.
fn measure(
    w: &mut World,
    what: &str,
    ix: Instruction,
    payer: &Keypair,
    table: &AddressLookupTableAccount,
) -> Tx {
    let accounts = ix.accounts.len();
    let mut ixs = vec![compute_unit_limit(600_000), compute_unit_price(10_000)];
    let coin = coin_table(&ix);
    ixs.push(ix);
    let without = w.env.v0_size(&ixs, payer, &[], &[]);
    let protocol = w.env.v0_size(&ixs, payer, &[], std::slice::from_ref(table));
    let tx = if protocol > 1_232 {
        let t = w.env.put_lookup_table(Pubkey::new_unique(), &coin);
        w.env.send_v0(&ixs, payer, &[], &[t])
    } else {
        w.env.send_v0(&ixs, payer, &[], std::slice::from_ref(table))
    };
    tx.ok();
    println!(
        "{what}: {} CU, {accounts} accounts, {} B sent{} / {protocol} B with the protocol table / {without} B without",
        tx.cu(),
        tx.size,
        if protocol > 1_232 {
            " with the coin's table"
        } else {
            " with the protocol table"
        },
    );
    assert!(tx.size <= 1_232);
    tx
}

#[test]
fn measures_presets_compute_and_sizes() {
    let mut w = World::new();
    let table = w
        .env
        .put_lookup_table(Pubkey::new_unique(), &presets_table(&w));
    let tables = [table.clone()];
    // `launch_token` of a preset, 1, 3 and 6 legs.
    for legs in [1usize, 3, 6] {
        let (_q, launch_quotes) = quote_mints(&mut w, &SIX[..legs]);
        let launcher = w.env.funded(10 * SOL);
        let mint = preset_companion(&mut w, &launcher, Preset::Split);
        let ix = launch_ix(
            &w,
            &launcher.pubkey(),
            &mint.pubkey(),
            &launch_quotes,
            LaunchRules::NONE,
            None,
        );
        let accounts = ix.accounts.len();
        let ixs = vec![compute_unit_limit(600_000), compute_unit_price(10_000), ix];
        let without = w.env.v0_size(&ixs, &launcher, &[&mint], &[]);
        let tx = w.env.send_v0(&ixs, &launcher, &[&mint], &tables);
        tx.ok();
        println!(
            "launch_token (a preset, its pots made), {legs} leg(s): {} CU, {accounts} accounts, {} B with the table / {without} B without",
            tx.cu(),
            tx.size
        );
        assert!(tx.size <= 1_232);
    }
    // The steps: SPL Token and Token-2022 legs, no rules, the kit's, a Workshop hook.
    let kit = LaunchRules {
        burn_buy_bps: 50,
        burn_sell_bps: 50,
        max_wallet_bps: 500,
        ..LaunchRules::NONE
    };
    for (rules_name, rules, hooked) in [
        ("no rules", LaunchRules::NONE, false),
        ("the kit's burns and max wallet", kit, false),
        ("a Workshop hook", LaunchRules::NONE, true),
    ] {
        for preset in [Preset::Split, Preset::Snowball { flat: true }] {
            let coin = preset_coin(&mut w, &THREE[..2], preset, rules, hooked);
            let t = trade_every_leg(&mut w, &coin);
            let c = cranker(&mut w, &coin);
            for (i, leg) in [(0usize, "SPL Token"), (1, "Token-2022 xStocks-like")] {
                let what = |step: &str| format!("{step}, {preset:?}, {rules_name}, {leg} leg");
                // The first claim makes the creator address's token account.
                let ix = companion::claim_fees_leg(c.pubkey(), &coin.keys(&w, i), i as u8, true);
                measure(&mut w, &what("claim_fees_leg (first)"), ix, &c, &table);
                trade(&mut w, &coin, &t, i, 1, 1_000_000).ok();
                let ix = companion::claim_fees_leg(c.pubkey(), &coin.keys(&w, i), i as u8, true);
                measure(&mut w, &what("claim_fees_leg"), ix, &c, &table);
                let ix = companion::withdraw_leg(
                    c.pubkey(),
                    coin.launcher.pubkey(),
                    &coin.keys(&w, i),
                    i as u8,
                );
                measure(
                    &mut w,
                    &what("withdraw_leg (making the launcher's account)"),
                    ix,
                    &c,
                    &table,
                );
            }
            w.env.warp(BUYBACK_AFTER_LAUNCH);
            for (i, leg) in [(0usize, "SPL Token"), (1, "Token-2022 xStocks-like")] {
                let what = |step: &str| format!("{step}, {rules_name}, {leg} leg");
                if preset.snowball() {
                    let ix = snowball_ix(&w, &coin, &c.pubkey(), i, true);
                    measure(&mut w, &what("snowball_step_leg (a buy)"), ix, &c, &table);
                    w.env.warp(61);
                    // A fresh wallet's pump (within max wallet): the price past the sell step.
                    let pump = trader(&mut w, &coin.quotes);
                    let vq = w.legs(&coin.mint).legs[i].virtual_quote;
                    trade(&mut w, &coin, &pump, i, 1, vq / 10).ok();
                    let ix = snowball_ix(&w, &coin, &c.pubkey(), i, true);
                    let tx = measure(&mut w, &what("snowball_step_leg (a sell)"), ix, &c, &table);
                    assert_eq!(tx.event::<LegSnowballTraded>().side, 0);
                } else {
                    let ix = buyback_ix(&w, &coin, &c.pubkey(), i, true);
                    measure(
                        &mut w,
                        &what("buyback_leg (a buy and a burn)"),
                        ix,
                        &c,
                        &table,
                    );
                }
            }
        }
    }
    let size = std::fs::read(bordrless_program_tests::sbf_out_dir().join("bordrless_companion.so"))
        .map(|b| b.len())
        .unwrap_or(0);
    println!("bordrless_companion.so: {size} bytes");
}

// ---- Regressions of review3 F1 (21 Oct 2026): a Snowball with a buyback share kept apart --------
//
// The reviewer's two tests, as they reproduced F1 (Medium): `buyback_leg` and `snowball_step_leg`
// shared `LegPot.reference_price`, `reference_at` and `last_at`. They now pass with the Snowball's
// own `snowball_reference` and `snowball_last_at`; the one assertion that read the shared field
// after a follow now reads the Snowball's own.

/// A Snowball companion whose split also pays buybacks (the API takes one: `snowballProblem` only
/// asks the parts to sum to 10,000): 30% buybacks, 70% the market maker, nothing to the launcher.
fn snowball_buyback_companion(
    w: &mut World,
    launcher: &Keypair,
    flat: bool,
    snowball_interval: i64,
    buyback_interval: i64,
) -> Keypair {
    let mint = Keypair::new();
    let ixs = vec![
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            mint.pubkey(),
            CreateArgs {
                // `create` wants three parts that add up on their own (as the API does).
                split: split(0, 10_000),
                bounty_bps: BOUNTY_BPS,
                max_buyback: SOL,
                buyback_interval,
                vest_secs: 0,
                fund: SOL,
            },
        ),
        companion::create_snowball(
            launcher.pubkey(),
            mint.pubkey(),
            CreateSnowballArgs {
                split: Split {
                    buyback_bps: 3_000,
                    holders_bps: 0,
                    beneficiary_bps: 0,
                },
                pot_bps: 7_000,
                buy_step_bps: 100,
                sell_step_bps: 100,
                buy_bps: 2_000,
                sell_bps: 2_000,
                interval: snowball_interval,
                max_trade: SOL,
                pool_bps: 500,
                flat_trades: flat,
            },
        ),
    ];
    w.env.send_paid_by(&ixs, launcher, &[&mint]).ok();
    mint
}

/// [`snowball_buyback_companion`]'s coin quoted in `specs`, launched through `launch_token`, every
/// leg open.
fn snowball_buyback_coin(
    w: &mut World,
    specs: &[Spec],
    flat: bool,
    snowball_interval: i64,
    buyback_interval: i64,
) -> Coin {
    let (quotes, launch_quotes) = quote_mints(w, specs);
    let launcher = w.env.funded(10 * SOL);
    let mint = snowball_buyback_companion(w, &launcher, flat, snowball_interval, buyback_interval);
    let ix = launch_ix(
        w,
        &launcher.pubkey(),
        &mint.pubkey(),
        &launch_quotes,
        LaunchRules::NONE,
        None,
    );
    let tx = w.env.send_paid_by(&[ix], &launcher, &[&mint]);
    tx.ok();
    let c = companion_of(w, &mint.pubkey());
    assert!(c.is_snowball() && c.split.buyback_bps == 3_000 && c.pot_bps == 7_000);
    let coin = Coin {
        mint: mint.pubkey(),
        launcher,
        quotes,
        rules: LaunchRules::NONE,
        custom: None,
        collector: Pubkey::new_unique(),
        preset: Preset::Snowball { flat },
    };
    open_legs(w, &coin);
    coin
}

/// review3 F1 (starvation). On a SOL coin the Snowball's interval runs on `Snowball.last_at` and the
/// buyback's on `Companion.last_buyback_at`: one never delays the other. On a leg both read and
/// write `LegPot.last_at`, so a Snowball cranked every interval (the keeper does; anyone may, for
/// the bounty) keeps a buyback whose interval is longer from ever being due: the leg's buyback part
/// is never spent.
#[test]
fn review3_snowball_steps_never_let_the_legs_buyback_come_due() {
    let run = |crank_snowball: bool| -> (u32, Vec<u32>, u64, u64) {
        let mut w = World::new();
        // Snowball every 60 s (flat trades), buybacks every 120 s.
        let coin = snowball_buyback_coin(&mut w, &ONE_SPL, true, 60, 120);
        trade_every_leg(&mut w, &coin);
        let c = cranker(&mut w, &coin);
        claim(&mut w, &coin, &c, 0, true).ok();
        let start = pots(&w, &coin.mint).legs[0];
        assert!(start.pending_buyback > 0 && start.pending_pot > 0);
        w.env.warp(BUYBACK_AFTER_LAUNCH);
        let mut bought = 0u32;
        let mut refusals = vec![];
        let mut snowball_trades = 0u32;
        // Twenty minutes: ten buyback intervals.
        for _ in 0..20 {
            if crank_snowball {
                let tx = snowball(&mut w, &coin, &c, 0, true);
                if tx.result.is_err() {
                    refusals.push(tx.custom());
                } else {
                    snowball_trades += tx.events::<LegSnowballTraded>().len() as u32;
                }
            }
            w.env.warp(61);
            let tx = buyback(&mut w, &coin, &c, 0, true);
            if tx.result.is_err() {
                refusals.push(tx.custom());
            } else if !tx.events::<LegBoughtBack>().is_empty() {
                bought += 1;
            }
        }
        println!("crank_snowball {crank_snowball}: snowball trades {snowball_trades}");
        (
            bought,
            refusals,
            start.pending_buyback,
            pots(&w, &coin.mint).legs[0].pending_buyback,
        )
    };
    let (alone, _, _, left_alone) = run(false);
    let (with_snowball, refusals, start, stuck) = run(true);
    println!(
        "buyback part {start}; buybacks in 20 min without Snowball steps: {alone} (left {left_alone}); \
         with a Snowball step each interval: {with_snowball} (left {stuck}); refusals {refusals:?}"
    );
    assert!(alone > 0, "the buyback runs on its own");
    assert!(
        with_snowball > 0,
        "the leg buyback never came due while the Snowball was cranked each interval: \
         snowball_step_leg and buyback_leg share LegPot.last_at (refusals {refusals:?}, \
         {stuck} of the leg token stuck in pending_buyback)"
    );
}

/// review3 F1 (the 3% rule). On a SOL coin the buyback's reference moves up only by
/// `REFERENCE_STEP_BPS` an interval while it waits and down only after a buy; the Snowball's
/// reference is its own. On a leg a Snowball follow (anyone may send it once an interval) writes
/// the shared `LegPot.reference_price` to the spot price at once, so after a pump the buyback buys
/// far above 3% over the reference it had.
#[test]
fn review3_a_snowball_follow_lifts_the_legs_buyback_reference_to_a_pump() {
    let mut w = World::new();
    // No flat trades: a sell called with no coins held is a follow.
    let coin = snowball_buyback_coin(&mut w, &ONE_SPL, false, 60, 60);
    let t = trader(&mut w, &coin.quotes);
    let vq = w.legs(&coin.mint).legs[0].virtual_quote;
    trade(&mut w, &coin, &t, 0, 1, vq / 50).ok();
    let c = cranker(&mut w, &coin);
    claim(&mut w, &coin, &c, 0, true).ok();
    w.env.warp(BUYBACK_AFTER_LAUNCH);
    let before = pots(&w, &coin.mint).legs[0];
    assert!(before.pending_buyback > 0 && before.tokens_held == 0);
    let opening_ref = before.reference_price;
    // The attacker pumps the leg (half its virtual quote: the price about 2.2x).
    let attacker = trader(&mut w, &coin.quotes);
    let q_before = w
        .env
        .token_amount(&coin.keys(&w, 0).quote_account(&attacker.pubkey()));
    trade(&mut w, &coin, &attacker, 0, 1, vq / 2).ok();
    let pumped = price(&leg_pool(&w, &coin, 0));
    assert!(pumped > opening_ref * 2);
    // A Snowball follow: the Snowball's reference becomes the pump (the buyback's stays).
    let tx = snowball(&mut w, &coin, &attacker, 0, false);
    tx.ok();
    tx.event::<LegSnowballFollowed>();
    assert_eq!(pots(&w, &coin.mint).legs[0].snowball_reference, pumped);
    assert_eq!(pots(&w, &coin.mint).legs[0].reference_price, opening_ref);
    // A buyback interval later the buyback buys at the pump.
    w.env.warp(61);
    let tx = buyback(&mut w, &coin, &attacker, 0, false);
    tx.ok();
    let bought = tx.events::<LegBoughtBack>();
    // The attacker sells everything back.
    let coins = w.env.holding(&coin.mint, &attacker.pubkey());
    trade(&mut w, &coin, &attacker, 0, 0, coins).ok();
    let q_after = w
        .env
        .token_amount(&coin.keys(&w, 0).quote_account(&attacker.pubkey()));
    println!(
        "opening reference {opening_ref}, pumped price {pumped} ({:.2}x); buyback (spent, burned, price_after): {:?}; attacker PnL in the leg token: {}",
        pumped as f64 / opening_ref as f64,
        bought.first().map(|e| (e.spent, e.burned, e.price_after)),
        q_after as i128 - q_before as i128
    );
    assert!(
        bought.is_empty(),
        "the leg buyback bought at {:.2}x its own reference ({} > 3% over {}): the Snowball's \
         follow rewrote the shared LegPot.reference_price",
        pumped as f64 / opening_ref as f64,
        pumped,
        opening_ref
    );
}
