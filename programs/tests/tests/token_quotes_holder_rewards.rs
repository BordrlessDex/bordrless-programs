//! Holder rewards on coins quoted in tokens (`docs/token-quotes.md` §7.5.1). The launchpad takes the
//! holder fee on the quote side of every leg trade into the kit config's associated token account
//! of the leg's token (callback index 9, derived by the hook: a trader's own account is refused).
//! A one-leg coin's kit pays it through its own accumulator, the vault being that token account
//! (`claim_token`, `claim_token_for`, `share_token` streamed over the hour). A coin of several legs
//! pays every leg's token in rounds (`start_round`, `pay_round`), each holding on the least it held
//! from the start of the round before (so a dust transfer never makes a holder miss a round, and a
//! round never pays more than it has). The SOL kind (the switch) runs with the venues in
//! `token_quotes_sol_switch.rs`.
//!
//! Every setup runs under SPL Token and an xStocks-like Token-2022 mint.

use anchor_lang::prelude::Pubkey;
use bordrless_core::policy;
use bordrless_hook::HookAccountList;
use bordrless_kit::client as kit;
use bordrless_kit::constants::{REWARD_KIND_ROUNDS, REWARD_KIND_TOKEN};
use bordrless_kit::error::KitError;
use bordrless_kit::events::{LegsRewardsInstalled, RewardsClaimed, RoundPaid, RoundStarted};
use bordrless_kit::state::{KitConfig, KitRounds, RoundData};
use bordrless_launch::client as launch;
use bordrless_launch::error::LaunchError;
use bordrless_launch::events::TokenLaunchPayouts;
use bordrless_launch::instructions::{CreateTokenLaunchArgs, TokenPayouts};
use bordrless_launch::state::{Launch, LaunchRules};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::quotes::{
    ext, pack_mint, xstock_extensions, Quote, TOKEN, TOKEN_2022,
};
use bordrless_swap::spl;
use bordrless_token::client as token;
use solana_keypair::Keypair;
use solana_signer::Signer;

const SUPPLY: u64 = policy::TOKEN_SUPPLY;
const FEE: u16 = 100;
const HOLDER_FEE: u16 = 200;
const HOUR: i64 = 3_600;

fn kcode(e: KitError) -> u32 {
    u32::from(e)
}

fn lcode(e: LaunchError) -> u32 {
    u32::from(e)
}

type Extensions = fn(Pubkey) -> Vec<ext::Ext>;

fn xstock(mint: Pubkey) -> Vec<ext::Ext> {
    xstock_extensions(mint, Pubkey::new_from_array([7; 32]))
}

fn none(_: Pubkey) -> Vec<ext::Ext> {
    vec![]
}

type Spec = (Pubkey, u8, Extensions);

fn cap_raw(decimals: u8) -> u64 {
    match decimals {
        6 => 5_000_000_000,
        8 => 2_000_000_000,
        _ => 25_000_000_000,
    }
}

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

/// A coin quoted in tokens with holder rewards: its mint, launcher and quotes `(mint, program)`.
struct Coin {
    mint: Pubkey,
    quotes: Vec<(Pubkey, Pubkey)>,
}

fn rules(holder_fee: u16) -> LaunchRules {
    LaunchRules {
        holder_fee_buy_bps: holder_fee,
        holder_fee_sell_bps: holder_fee,
        ..LaunchRules::NONE
    }
}

fn quote_mints(w: &mut World, specs: &[Spec]) -> (Vec<(Pubkey, Pubkey)>, Vec<Quote>) {
    let s = shares(specs.len());
    let issuer = w.env.deployer.pubkey();
    let mut quotes = vec![];
    let mut launch_quotes = vec![];
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

fn args(quotes: &[Quote], rules: LaunchRules, payouts: TokenPayouts) -> CreateTokenLaunchArgs {
    CreateTokenLaunchArgs {
        payouts,
        ..World::token_launch_args_with("HR", FEE, quotes, rules)
    }
}

/// A wallet funded with SOL and a large balance of each quote.
fn trader(w: &mut World, quotes: &[(Pubkey, Pubkey)]) -> Keypair {
    let kp = w.env.funded(100_000_000_000);
    for (q, p) in quotes {
        w.env.token_account(&kp.pubkey(), q, p, u64::MAX / 4);
    }
    kp
}

/// Launches a coin quoted in `specs` with `rules` and `payouts`, every leg open.
fn coin(w: &mut World, specs: &[Spec], rules: LaunchRules, payouts: TokenPayouts) -> Coin {
    let (quotes, launch_quotes) = quote_mints(w, specs);
    let creator = trader(w, &quotes);
    let mint = Keypair::new();
    let mints: Vec<Pubkey> = quotes.iter().map(|q| q.0).collect();
    let ix = launch::create_token_launch(
        creator.pubkey(),
        mint.pubkey(),
        w.env.treasury.pubkey(),
        args(&launch_quotes, rules, payouts),
        &mints,
    );
    w.env.send_paid_by(&[ix], &creator, &[&mint]).ok();
    w.open_all_legs(&mint.pubkey());
    Coin {
        mint: mint.pubkey(),
        quotes,
    }
}

fn config(w: &World, coin: &Coin) -> KitConfig {
    w.env.read(&kit::kit_config_address(&coin.mint))
}

fn vault(w: &World, coin: &Coin, leg: usize) -> Pubkey {
    launch::kit_vault_address(&w.leg_keys(&coin.mint, leg))
}

/// A buy of a twentieth of a leg's virtual quote (about 4.5% of the leg's coins).
fn chunk(w: &World, coin: &Coin, leg: usize) -> u64 {
    w.legs(&coin.mint).legs[leg].virtual_quote / 20
}

fn holding(w: &World, coin: &Coin, owner: &Pubkey) -> u64 {
    w.env.holding(&coin.mint, owner)
}

fn quote_balance(w: &World, coin: &Coin, leg: usize, owner: &Pubkey) -> u64 {
    let (q, p) = coin.quotes[leg];
    w.env
        .token_amount(&spl::associated_token_address(owner, &q, &p))
}

/// Sends a transfer of the coin between holders (the kit's slice resolved for the coin's kind).
fn transfer(
    w: &mut World,
    coin: &Coin,
    from: &Keypair,
    to: &Pubkey,
    amount: u64,
) -> bordrless_program_tests::env::Tx {
    let l: Launch = w.env.read(&launch::launch_address(&coin.mint));
    let count = w.legs(&coin.mint).count;
    let keys = w.leg_keys(&coin.mint, 0);
    let slice = launch::leg_base_slice_for(&l, count, &keys);
    let extras: Vec<_> = slice[2..].to_vec();
    let ixs = [
        token::create_holding(from.pubkey(), coin.mint, *to),
        token::transfer_with(
            from.pubkey(),
            token::holding_address(&coin.mint, &from.pubkey()),
            token::holding_address(&coin.mint, to),
            coin.mint,
            Some(token::Hook::of(bordrless_kit::KIT_ID)),
            extras,
            amount,
        ),
    ];
    w.env.send_paid_by(&ixs, from, &[])
}

// ---- Kind 1: one leg, the leg's token through the accumulator -------------------------------------

#[test]
fn one_leg_holder_rewards_accrue_and_pay_in_the_legs_token() {
    for (name, spec) in [
        ("SPL Token", (TOKEN, 6, none as Extensions)),
        ("Token-2022", (TOKEN_2022, 8, xstock as Extensions)),
    ] {
        let mut w = World::new();
        let (quotes, launch_quotes) = quote_mints(&mut w, &[spec]);
        let creator = trader(&mut w, &quotes);
        let mint = Keypair::new();
        let ix = launch::create_token_launch(
            creator.pubkey(),
            mint.pubkey(),
            w.env.treasury.pubkey(),
            args(&launch_quotes, rules(HOLDER_FEE), TokenPayouts::default()),
            &[quotes[0].0],
        );
        let tx = w.env.send_paid_by(&[ix], &creator, &[&mint]);
        tx.ok();
        let installed = tx.event::<LegsRewardsInstalled>();
        assert_eq!(installed.kind, REWARD_KIND_TOKEN, "{name}");
        let payouts = tx.event::<TokenLaunchPayouts>();
        assert_eq!(
            (payouts.flags, payouts.reward_kind),
            (0, REWARD_KIND_TOKEN),
            "{name}"
        );
        let coin = Coin {
            mint: mint.pubkey(),
            quotes: quotes.clone(),
        };
        let c = config(&w, &coin);
        assert_eq!(c.reward_kind(), REWARD_KIND_TOKEN);
        assert_eq!(c.reward_mint, quotes[0].0);
        assert_eq!(
            c.reward_vault,
            vault(&w, &coin, 0),
            "{name}: the vault is the config's ATA"
        );
        // The kit's registry lists the vault in place of the kit's id.
        let reg = w.env.account(&kit::registry_address(&coin.mint)).unwrap();
        let list = HookAccountList::decode(&reg.data).unwrap();
        assert_eq!(list.accounts.len(), 3);
        w.open_all_legs(&coin.mint);
        assert!(
            w.env.account(&vault(&w, &coin, 0)).is_some(),
            "{name}: open_leg made the vault"
        );
        // The leg's registry lists the vault as its fifth extra (callback index 9).
        let keys = w.leg_keys(&coin.mint, 0);
        let leg_reg = w
            .env
            .account(&launch::registry_address(&keys.pool()))
            .unwrap();
        assert_eq!(
            HookAccountList::decode(&leg_reg.data)
                .unwrap()
                .accounts
                .len(),
            5
        );

        // The first buyer: nobody eligible before the trade, so no holder fee.
        let a = trader(&mut w, &quotes);
        let b = trader(&mut w, &quotes);
        let amount = chunk(&w, &coin, 0);
        w.leg_buy(&a, &coin.mint, 0, amount).ok();
        assert_eq!(
            w.env.token_amount(&vault(&w, &coin, 0)),
            0,
            "{name}: nobody held yet"
        );
        // Then holders hold the threshold: every trade's holder fee goes to the vault, exactly.
        let before = w.env.token_amount(&vault(&w, &coin, 0));
        w.leg_buy(&b, &coin.mint, 0, amount).ok();
        let fee = (u128::from(amount) * u128::from(HOLDER_FEE)).div_ceil(10_000) as u64;
        assert_eq!(
            w.env.token_amount(&vault(&w, &coin, 0)) - before,
            fee,
            "{name}: the buy's holder fee"
        );
        let before = w.env.token_amount(&vault(&w, &coin, 0));
        let sold = holding(&w, &coin, &b.pubkey()) / 4;
        let q_before = quote_balance(&w, &coin, 0, &b.pubkey());
        w.leg_sell(&b, &coin.mint, 0, sold).ok();
        let got = quote_balance(&w, &coin, 0, &b.pubkey()) - q_before;
        let took = w.env.token_amount(&vault(&w, &coin, 0)) - before;
        assert!(
            took > 0 && took <= got / 40,
            "{name}: the sell's holder fee ({took} of {got})"
        );

        // A trader naming its own account at index 9 is refused.
        let l: Launch = w.env.read(&launch::launch_address(&coin.mint));
        let mut ix = launch::leg_swap_for(&l, 1, &keys, a.pubkey(), a.pubkey(), 1, amount, 0);
        let own = keys.quote_account(&a.pubkey());
        let last = ix.accounts.len() - 1;
        ix.accounts[last].pubkey = own;
        let tx = w.env.send_paid_by(&[ix], &a, &[]);
        tx.expect_code(lcode(LaunchError::WrongKitVault));

        // Claims: the holder's own (its account made), pushed to an account that exists, and
        // refused without one; the vault never pays more than it holds.
        w.env.warp(60);
        let vault_before = w.env.token_amount(&vault(&w, &coin, 0));
        let (q, p) = quotes[0];
        let tx = w
            .env
            .send_paid_by(&[kit::claim_token(a.pubkey(), coin.mint, q, p)], &a, &[]);
        tx.ok();
        let paid_a = tx.event::<RewardsClaimed>().amount;
        assert!(paid_a > 0, "{name}: a's rewards");
        let c = w.env.funded(1_000_000_000);
        // b has an account of the token (it traded): a push lands there.
        let tx = w.env.send_paid_by(
            &[kit::claim_token_for(
                c.pubkey(),
                b.pubkey(),
                coin.mint,
                q,
                p,
                None,
            )],
            &c,
            &[],
        );
        tx.ok();
        let paid_b = tx.event::<RewardsClaimed>().amount;
        assert!(paid_b > 0, "{name}: b's rewards, pushed");
        assert!(paid_a + paid_b <= vault_before, "{name}: within the vault");
        // A holder with no account of the token: the push is refused, its own claim makes one.
        let d = w.env.funded(1_000_000_000);
        let half = holding(&w, &coin, &a.pubkey()) / 2;
        transfer(&mut w, &coin, &a, &d.pubkey(), half).ok();
        w.env.warp(600);
        w.leg_buy(&b, &coin.mint, 0, amount).ok();
        w.env.warp(60);
        let tx = w.env.send_paid_by(
            &[kit::claim_token_for(
                c.pubkey(),
                d.pubkey(),
                coin.mint,
                q,
                p,
                None,
            )],
            &c,
            &[],
        );
        tx.expect_code(kcode(KitError::NoTokenAccount));
        let tx = w
            .env
            .send_paid_by(&[kit::claim_token(d.pubkey(), coin.mint, q, p)], &d, &[]);
        tx.ok();
        // A leg's pool is never a holder: its claim pushed is refused.
        let pool = keys.pool();
        let tx = w.env.send_paid_by(
            &[kit::claim_token_for(
                c.pubkey(),
                pool,
                coin.mint,
                q,
                p,
                None,
            )],
            &c,
            &[],
        );
        tx.expect_fail();
        // The bridged claim and share and the SOL claim refuse this kit.
        let tx = w
            .env
            .send_paid_by(&[kit::claim_sol(a.pubkey(), coin.mint)], &a, &[]);
        tx.expect_fail();

        // A share streams over the hour: little at once, the whole after it.
        let sharer = trader(&mut w, &quotes);
        let shared = 1_000_000;
        let claimed_before = config(&w, &coin).total_claimed;
        let tx = w.env.send_paid_by(
            &[kit::share_token(
                sharer.pubkey(),
                coin.mint,
                0,
                q,
                p,
                spl::associated_token_address(&sharer.pubkey(), &q, &p),
                shared,
            )],
            &sharer,
            &[],
        );
        tx.ok();
        let c1 = config(&w, &coin);
        assert_eq!(
            c1.stream_remaining + c1.stream_next,
            shared,
            "{name}: the share streams"
        );
        w.env.warp(2 * HOUR);
        // Every holder claims: the vault keeps at most rounding dust and what is not yet divided.
        for h in [&a, &b, &d] {
            w.env
                .send_paid_by(&[kit::claim_token(h.pubkey(), coin.mint, q, p)], h, &[])
                .ok();
        }
        let c2 = config(&w, &coin);
        assert!(c2.total_claimed > claimed_before, "{name}");
        let left = w.env.token_amount(&vault(&w, &coin, 0));
        // What stays is the settles' rounding down (each holder's share floored, in the vault's
        // favour): exactly what was divided and not claimed, a hundredth of a percent at most.
        assert_eq!(left, c2.total_distributed - c2.total_claimed, "{name}");
        assert!(left * 10_000 < c2.total_distributed, "{name}: {left} left");
        assert_eq!(
            (c2.held, c2.stream_remaining, c2.stream_next),
            (0, 0, 0),
            "{name}"
        );
    }
}

#[test]
fn an_issuer_draining_a_one_leg_vault_never_stops_the_coin() {
    // A Token-2022 quote with a permanent delegate (the xStocks' issuer): it can move the kit's
    // vault's tokens away; the kit takes the shortfall as its new base and the coin keeps trading.
    let mut w = World::new();
    let c = coin(
        &mut w,
        &[(TOKEN_2022, 8, xstock as Extensions)],
        rules(HOLDER_FEE),
        TokenPayouts::default(),
    );
    let a = trader(&mut w, &c.quotes);
    let b = trader(&mut w, &c.quotes);
    let amount = chunk(&w, &c, 0);
    w.leg_buy(&a, &c.mint, 0, amount).ok();
    w.leg_buy(&b, &c.mint, 0, amount).ok();
    let v = vault(&w, &c, 0);
    assert!(w.env.token_amount(&v) > 0);
    // The issuer takes every token (written directly, as its permanent delegate's transfer would).
    w.env.set_token_amount(&v, 0);
    w.env.warp(60);
    w.leg_buy(&a, &c.mint, 0, amount).ok();
    transfer(&mut w, &c, &a, &b.pubkey(), 1_000).ok();
    let (q, p) = c.quotes[0];
    // What arrives after it is paid as before.
    w.leg_buy(&b, &c.mint, 0, amount).ok();
    w.env.warp(60);
    w.env
        .send_paid_by(&[kit::claim_token(a.pubkey(), c.mint, q, p)], &a, &[])
        .ok();
}

// ---- Kind 2: every leg's token in rounds -----------------------------------------------------------

fn three() -> Vec<Spec> {
    vec![
        (TOKEN, 6, none as Extensions),
        (TOKEN_2022, 8, xstock as Extensions),
        (TOKEN, 9, none as Extensions),
    ]
}

fn legs_of(w: &World, coin: &Coin) -> Vec<(Pubkey, Pubkey)> {
    (0..coin.quotes.len())
        .map(|i| {
            let k = w.leg_keys(&coin.mint, i);
            (k.quote_mint, k.quote_program)
        })
        .collect()
}

fn start(w: &mut World, coin: &Coin, payer: &Keypair) -> bordrless_program_tests::env::Tx {
    let legs = legs_of(w, coin);
    w.env.send_paid_by(
        &[kit::start_round(payer.pubkey(), coin.mint, &legs)],
        payer,
        &[],
    )
}

fn pay(
    w: &mut World,
    coin: &Coin,
    payer: &Keypair,
    owner: &Pubkey,
    create: bool,
) -> bordrless_program_tests::env::Tx {
    let legs = legs_of(w, coin);
    w.env.send_paid_by(
        &[kit::pay_round(
            payer.pubkey(),
            *owner,
            coin.mint,
            &legs,
            create,
        )],
        payer,
        &[],
    )
}

/// Warps to the start of the next round.
fn next_round(w: &mut World, round_secs: i64) {
    let now = w.env.now;
    w.env.warp(round_secs - now.rem_euclid(round_secs) + 1);
}

fn rounds(w: &World, coin: &Coin) -> KitRounds {
    w.env.read(&kit::kit_rounds_address(&coin.mint))
}

#[test]
fn rounds_pay_every_legs_token_pro_rata_once_a_round() {
    let mut w = World::new();
    let c = coin(
        &mut w,
        &three(),
        rules(HOLDER_FEE),
        TokenPayouts {
            flags: 0,
            holder_round_secs: 600,
        },
    );
    let k = config(&w, &c);
    assert_eq!((k.reward_kind(), k.round_secs()), (REWARD_KIND_ROUNDS, 600));
    let (a, b) = (trader(&mut w, &c.quotes), trader(&mut w, &c.quotes));
    for leg in 0..3 {
        let amount = chunk(&w, &c, leg);
        w.leg_buy(&a, &c.mint, leg, amount).ok();
        w.leg_buy(&b, &c.mint, leg, amount / 2).ok();
        w.leg_buy(&a, &c.mint, leg, amount / 4).ok();
    }
    for leg in 0..3 {
        assert!(
            w.env.token_amount(&vault(&w, &c, leg)) > 0,
            "leg {leg}'s holder fees"
        );
    }
    let payer = w.env.funded(10_000_000_000);
    // Bought this round: not held through the round before.
    start(&mut w, &c, &payer).ok();
    start(&mut w, &c, &payer).expect_code(kcode(KitError::RoundAlreadyStarted));
    pay(&mut w, &c, &payer, &a.pubkey(), true).expect_code(kcode(KitError::NotHeldThroughRound));
    next_round(&mut w, 600);
    start(&mut w, &c, &payer).ok();
    pay(&mut w, &c, &payer, &a.pubkey(), true).expect_code(kcode(KitError::NotHeldThroughRound));
    // Two rounds on: held through the whole round before.
    next_round(&mut w, 600);
    let tx = start(&mut w, &c, &payer);
    tx.ok();
    let started = tx.event::<RoundStarted>();
    let r = rounds(&w, &c);
    assert_eq!(started.supply, config(&w, &c).eligible);
    for leg in 0..3 {
        assert_eq!(r.legs[leg].amount, w.env.token_amount(&vault(&w, &c, leg)));
    }
    let tx = pay(&mut w, &c, &payer, &a.pubkey(), true);
    tx.ok();
    let paid = tx.event::<RoundPaid>();
    let bal_a = holding(&w, &c, &a.pubkey());
    assert_eq!(paid.balance, bal_a);
    for leg in 0..3 {
        let expected =
            (u128::from(r.legs[leg].amount) * u128::from(bal_a) / u128::from(r.supply)) as u64;
        assert_eq!(paid.amounts[leg], expected, "leg {leg}: a's share");
    }
    pay(&mut w, &c, &payer, &a.pubkey(), true).expect_code(kcode(KitError::AlreadyPaidThisRound));
    pay(&mut w, &c, &payer, &b.pubkey(), true).ok();
    // A leg's pool is no holder.
    let pool = w.leg_keys(&c.mint, 1).pool();
    pay(&mut w, &c, &payer, &pool, true).expect_fail();
    // What the round did not pay rolls into the next one's amounts.
    let left: Vec<u64> = (0..3)
        .map(|i| w.env.token_amount(&vault(&w, &c, i)))
        .collect();
    next_round(&mut w, 600);
    start(&mut w, &c, &payer).ok();
    let r = rounds(&w, &c);
    for (leg, rest) in left.iter().enumerate() {
        assert_eq!(r.legs[leg].amount, *rest, "leg {leg}: the rest rolls");
        assert_eq!(r.legs[leg].paid, 0);
    }
}

#[test]
fn a_dust_transfer_never_makes_a_holder_miss_a_round() {
    let mut w = World::new();
    let c = coin(
        &mut w,
        &three(),
        rules(HOLDER_FEE),
        TokenPayouts {
            flags: 0,
            holder_round_secs: 600,
        },
    );
    let (a, b) = (trader(&mut w, &c.quotes), trader(&mut w, &c.quotes));
    for leg in 0..3 {
        let amount = chunk(&w, &c, leg);
        w.leg_buy(&a, &c.mint, leg, amount).ok();
        w.leg_buy(&b, &c.mint, leg, amount).ok();
    }
    next_round(&mut w, 600);
    next_round(&mut w, 600);
    // b sends a 1 every round to a: a receives, its least held is untouched, it is paid.
    transfer(&mut w, &c, &b, &a.pubkey(), 1).ok();
    let payer = w.env.funded(10_000_000_000);
    start(&mut w, &c, &payer).ok();
    transfer(&mut w, &c, &b, &a.pubkey(), 1).ok();
    let tx = pay(&mut w, &c, &payer, &a.pubkey(), true);
    tx.ok();
    // a's record: paid this round, and the least it held is what it held before the dust.
    let read = w
        .env
        .read::<bordrless_token::state::Holding>(&token::holding_address(&c.mint, &a.pubkey()));
    let rd = RoundData::read(&read.hook_data);
    assert_eq!(rd.paid, config(&w, &c).round_at(w.env.now));
    assert!(tx.event::<RoundPaid>().balance <= holding(&w, &c, &a.pubkey()) - 2);
}

#[test]
fn pay_round_makes_missing_accounts_only_when_asked() {
    let mut w = World::new();
    let c = coin(
        &mut w,
        &three(),
        rules(HOLDER_FEE),
        TokenPayouts {
            flags: 0,
            holder_round_secs: 600,
        },
    );
    let a = trader(&mut w, &c.quotes);
    for leg in 0..3 {
        let amount = chunk(&w, &c, leg);
        w.leg_buy(&a, &c.mint, leg, amount).ok();
        w.leg_buy(&a, &c.mint, leg, amount).ok();
    }
    // d holds the coin and no account of any leg's token.
    let d = w.env.funded(1_000_000_000);
    let third = holding(&w, &c, &a.pubkey()) / 3;
    transfer(&mut w, &c, &a, &d.pubkey(), third).ok();
    next_round(&mut w, 600);
    next_round(&mut w, 600);
    let payer = w.env.funded(10_000_000_000);
    start(&mut w, &c, &payer).ok();
    // Without `create_missing` every leg is skipped: nothing to pay, nothing written.
    pay(&mut w, &c, &payer, &d.pubkey(), false).expect_code(kcode(KitError::NothingToClaim));
    // With it, the payer makes them and every leg pays.
    let tx = pay(&mut w, &c, &payer, &d.pubkey(), true);
    tx.ok();
    let paid = tx.event::<RoundPaid>();
    for leg in 0..3 {
        assert!(paid.amounts[leg] > 0, "leg {leg}");
        assert_eq!(quote_balance(&w, &c, leg, &d.pubkey()), paid.amounts[leg]);
    }
}

#[test]
fn rounds_never_pay_more_than_their_amounts_whatever_the_transfers() {
    // A deterministic random walk: holders buy, sell, transfer and are paid across rounds; every
    // round's payments stay within its amounts on every leg, and nobody is paid twice in a round.
    let mut w = World::new();
    let c = coin(
        &mut w,
        &three(),
        rules(HOLDER_FEE),
        TokenPayouts {
            flags: 0,
            holder_round_secs: 600,
        },
    );
    let holders: Vec<Keypair> = (0..5).map(|_| trader(&mut w, &c.quotes)).collect();
    let payer = w.env.funded(100_000_000_000);
    let mut seed: u64 = 0x5eed_1234_abcd_0001;
    let mut rnd = |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    for h in &holders {
        let leg = rnd(3) as usize;
        let amount = chunk(&w, &c, leg);
        w.leg_buy(h, &c.mint, leg, amount).ok();
    }
    for _round in 0..6 {
        next_round(&mut w, 600);
        start(&mut w, &c, &payer).ok();
        let r0 = rounds(&w, &c);
        for _ in 0..12 {
            let who = rnd(holders.len() as u64) as usize;
            match rnd(5) {
                0 => {
                    let leg = rnd(3) as usize;
                    let amount = chunk(&w, &c, leg) / (1 + rnd(4));
                    w.leg_buy(&holders[who], &c.mint, leg, amount);
                }
                1 => {
                    let leg = rnd(3) as usize;
                    let bal = holding(&w, &c, &holders[who].pubkey());
                    if bal > 10 {
                        w.leg_sell(&holders[who], &c.mint, leg, bal / (2 + rnd(3)));
                    }
                }
                2 => {
                    let to = rnd(holders.len() as u64) as usize;
                    let bal = holding(&w, &c, &holders[who].pubkey());
                    if to != who && bal > 10 {
                        let to_key = holders[to].pubkey();
                        transfer(&mut w, &c, &holders[who], &to_key, bal / (1 + rnd(5)));
                    }
                }
                _ => {
                    let owner = holders[who].pubkey();
                    pay(&mut w, &c, &payer, &owner, true);
                }
            }
            w.env.warp(1 + rnd(60) as i64);
        }
        let r = rounds(&w, &c);
        assert_eq!(r.round, r0.round);
        for leg in 0..3 {
            assert!(
                r.legs[leg].paid <= r.legs[leg].amount,
                "leg {leg}: paid {} of {}",
                r.legs[leg].paid,
                r.legs[leg].amount
            );
        }
    }
}

// ---- Refusals and settings -------------------------------------------------------------------------

#[test]
fn payouts_and_rounds_are_held_to_their_bounds() {
    let mut w = World::new();
    let (quotes, launch_quotes) = quote_mints(&mut w, &three());
    let creator = trader(&mut w, &quotes);
    let mints: Vec<Pubkey> = quotes.iter().map(|q| q.0).collect();
    for (payouts, code) in [
        (
            TokenPayouts {
                flags: 2,
                holder_round_secs: 0,
            },
            LaunchError::PayoutsInvalid,
        ),
        (
            TokenPayouts {
                flags: 0,
                holder_round_secs: 599,
            },
            LaunchError::PayoutsInvalid,
        ),
        (
            TokenPayouts {
                flags: 0,
                holder_round_secs: 86_401,
            },
            LaunchError::PayoutsInvalid,
        ),
    ] {
        let mint = Keypair::new();
        let ix = launch::create_token_launch(
            creator.pubkey(),
            mint.pubkey(),
            w.env.treasury.pubkey(),
            args(&launch_quotes, rules(HOLDER_FEE), payouts),
            &mints,
        );
        w.env
            .send_paid_by(&[ix], &creator, &[&mint])
            .expect_code(lcode(code));
    }
    // A day and ten minutes are taken; no round given is an hour.
    for (secs, want) in [(600, 600), (86_400, 86_400), (0, 3_600)] {
        let c = coin(
            &mut w,
            &three(),
            rules(HOLDER_FEE),
            TokenPayouts {
                flags: 0,
                holder_round_secs: secs,
            },
        );
        assert_eq!(config(&w, &c).round_secs(), want);
    }
    // The holder fee within the config's bound, as on a SOL coin.
    let mint = Keypair::new();
    let ix = launch::create_token_launch(
        creator.pubkey(),
        mint.pubkey(),
        w.env.treasury.pubkey(),
        args(&launch_quotes, rules(501), TokenPayouts::default()),
        &mints,
    );
    w.env
        .send_paid_by(&[ix], &creator, &[&mint])
        .expect_code(lcode(LaunchError::HolderFeeTooHigh));
}

#[test]
fn a_launch_without_the_new_options_is_the_launch_it_was() {
    // No holder fee and no payouts: no `TokenLaunchPayouts`, the kit as phase 1 made it.
    let mut w = World::new();
    let c = coin(
        &mut w,
        &three(),
        LaunchRules {
            max_wallet_bps: 500,
            ..LaunchRules::NONE
        },
        TokenPayouts::default(),
    );
    let k = config(&w, &c);
    assert_eq!(k.reward_kind(), 0);
    assert!(!k.rewards_on());
    let l: Launch = w.env.read(&launch::launch_address(&c.mint));
    assert_eq!(l.payouts(), 0);
    // Its data: no payouts bytes (what the deployed launchpad and every client before sent).
    let plain = args(
        &[(Pubkey::new_unique(), 1_000)],
        LaunchRules::NONE,
        TokenPayouts::default(),
    );
    let with = args(
        &[(Pubkey::new_unique(), 1_000)],
        LaunchRules::NONE,
        TokenPayouts {
            flags: 1,
            holder_round_secs: 0,
        },
    );
    let a = borsh_vec(&plain);
    let b = borsh_vec(&with);
    assert_eq!(b.len(), a.len() + 5);
    assert_eq!(&b[..a.len()], &a[..]);
    let back: CreateTokenLaunchArgs = anchor_lang::AnchorDeserialize::try_from_slice(&a).unwrap();
    assert_eq!(back.payouts, TokenPayouts::default());
    let back: CreateTokenLaunchArgs = anchor_lang::AnchorDeserialize::try_from_slice(&b).unwrap();
    assert_eq!(back.payouts.flags, 1);
}

fn borsh_vec(args: &CreateTokenLaunchArgs) -> Vec<u8> {
    let mut v = Vec::new();
    anchor_lang::AnchorSerialize::serialize(args, &mut v).unwrap();
    v
}
