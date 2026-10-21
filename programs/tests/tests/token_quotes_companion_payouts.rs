//! A preset companion's paid parts on a coin quoted in tokens without the SOL switch
//! (`docs/token-quotes.md` §7.5): the holders' part shared with the kit's holder rewards in each
//! leg's token (`share_leg`: streamed over the hour on a one-leg coin, counted at the next round on
//! a coin of several legs), and payees paid their share of the launcher's part in each leg's token
//! (`withdraw_leg`, what one can't take owed in the companion's `PayeesLegs`). With the switch these
//! parts are sold to SOL (`token_quotes_sol_switch.rs`); here the SOL step refuses them. And the
//! figures of every holder-rewards instruction that runs without a venue.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use bordrless_companion::client as companion;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::{LegBeneficiaryPaid, LegPayeePaid, LegSharedWithHolders};
use bordrless_companion::instructions::{CreateArgs, PayeeArgs};
use bordrless_companion::state::{CompanionLegs, PayeesLegs, Split};
use bordrless_core::policy;
use bordrless_kit::client as kit;
use bordrless_kit::constants::{REWARD_KIND_ROUNDS, REWARD_KIND_TOKEN};
use bordrless_kit::state::{KitConfig, KitRounds};
use bordrless_launch::client as launch;
use bordrless_launch::instructions::{CreateTokenLaunchArgs, TokenPayouts};
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::protocol_lookup_table;
use bordrless_program_tests::quotes::{pack_mint, xstock_extensions, Quote, TOKEN, TOKEN_2022};
use bordrless_swap::spl;
use solana_keypair::Keypair;
use solana_signer::Signer;

const SOL: u64 = 1_000_000_000;
const SUPPLY: u64 = policy::TOKEN_SUPPLY;
const FEE: u16 = 200;

type Spec = (Pubkey, u8, bool);
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

fn ccode(e: CompanionError) -> u32 {
    u32::from(e)
}

fn cap_raw(decimals: u8) -> u64 {
    match decimals {
        6 => 5_000_000_000,
        8 => 2_000_000_000,
        _ => 25_000_000_000,
    }
}

fn quote_mints(w: &mut World, specs: &[Spec]) -> (Vec<(Pubkey, Pubkey)>, Vec<Quote>) {
    let n = specs.len() as u64;
    let issuer = w.env.deployer.pubkey();
    let mut quotes = vec![];
    let mut launch_quotes = vec![];
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

fn trader(w: &mut World, quotes: &[(Pubkey, Pubkey)]) -> Keypair {
    let kp = w.env.funded(100 * SOL);
    for (q, p) in quotes {
        w.env.token_account(&kp.pubkey(), q, p, u64::MAX / 8);
    }
    kp
}

struct Coin {
    mint: Pubkey,
    launcher: Keypair,
    quotes: Vec<(Pubkey, Pubkey)>,
}

fn holder_rules() -> LaunchRules {
    LaunchRules {
        holder_fee_buy_bps: 100,
        holder_fee_sell_bps: 100,
        ..LaunchRules::NONE
    }
}

/// A preset companion's coin quoted in `specs` (its `split`, `payees` when given), holder rewards
/// on, the switch off, every leg open.
fn coin(w: &mut World, specs: &[Spec], split: Split, payees: &[(Pubkey, u16)]) -> Coin {
    let (quotes, launch_quotes) = quote_mints(w, specs);
    let launcher = w.env.funded(100 * SOL);
    let mint = Keypair::new();
    let ix = companion::create(
        launcher.pubkey(),
        launcher.pubkey(),
        mint.pubkey(),
        CreateArgs {
            split,
            bounty_bps: 50,
            max_buyback: SOL,
            buyback_interval: 60,
            vest_secs: 0,
            fund: 2 * SOL,
        },
    );
    w.env.send_paid_by(&[ix], &launcher, &[&mint]).ok();
    if !payees.is_empty() {
        let ix = companion::set_payees(
            launcher.pubkey(),
            mint.pubkey(),
            payees
                .iter()
                .map(|(wallet, bps)| PayeeArgs {
                    wallet: *wallet,
                    bps: *bps,
                })
                .collect(),
        );
        w.env.send_paid_by(&[ix], &launcher, &[&mint]).ok();
    }
    let creator = companion::creator_address(&mint.pubkey());
    let mints: Vec<Pubkey> = quotes.iter().map(|q| q.0).collect();
    let args = CreateTokenLaunchArgs {
        payouts: TokenPayouts::default(),
        ..World::token_launch_args_with("CPAY", FEE, &launch_quotes, holder_rules())
    };
    let inner = launch::create_token_launch(
        creator,
        mint.pubkey(),
        w.env.treasury.pubkey(),
        args.clone(),
        &mints,
    );
    let ix = if payees.is_empty() {
        companion::launch_token(launcher.pubkey(), mint.pubkey(), &inner, args)
    } else {
        companion::launch_token_with_payees(launcher.pubkey(), mint.pubkey(), &inner, args)
    };
    let tx = w.env.send_paid_by(&[ix], &launcher, &[&mint]);
    tx.ok();
    w.open_all_legs(&mint.pubkey());
    Coin {
        mint: mint.pubkey(),
        launcher,
        quotes,
    }
}

fn pots(w: &World, c: &Coin) -> CompanionLegs {
    w.env.read(&companion::companion_legs_address(&c.mint))
}

/// Trades on every leg (two buyers, so holders hold the threshold), then claims every leg.
fn trade_and_claim(w: &mut World, c: &Coin) -> Keypair {
    let (a, b) = (trader(w, &c.quotes), trader(w, &c.quotes));
    for i in 0..c.quotes.len() {
        let vq = w.legs(&c.mint).legs[i].virtual_quote;
        w.leg_buy(&a, &c.mint, i, vq / 20).ok();
        w.leg_buy(&b, &c.mint, i, vq / 20).ok();
        w.leg_buy(&a, &c.mint, i, vq / 40).ok();
    }
    let crank = trader(w, &c.quotes);
    for i in 0..c.quotes.len() {
        let keys = w.leg_keys(&c.mint, i);
        w.env
            .send_paid_by(
                &[companion::claim_fees_leg(
                    crank.pubkey(),
                    &keys,
                    i as u8,
                    false,
                )],
                &crank,
                &[],
            )
            .ok();
    }
    crank
}

fn holders_split() -> Split {
    Split {
        buyback_bps: 2_000,
        holders_bps: 4_000,
        beneficiary_bps: 4_000,
    }
}

#[test]
fn the_holders_part_is_shared_with_the_kit_in_each_legs_token() {
    for (name, specs) in [
        ("1 leg, SPL Token", &ONE_SPL[..]),
        ("1 leg, Token-2022", &ONE_2022[..]),
        ("3 legs", &THREE[..]),
    ] {
        let mut w = World::new();
        let c = coin(&mut w, specs, holders_split(), &[]);
        let k: KitConfig = w.env.read(&kit::kit_config_address(&c.mint));
        let kind = if specs.len() == 1 {
            REWARD_KIND_TOKEN
        } else {
            REWARD_KIND_ROUNDS
        };
        assert_eq!(k.reward_kind(), kind, "{name}");
        assert!(pots(&w, &c).pays_in_token(0), "{name}: the switch is off");
        let crank = trade_and_claim(&mut w, &c);
        for i in 0..specs.len() {
            let keys = w.leg_keys(&c.mint, i);
            let held = pots(&w, &c).legs[i].pending_holders;
            assert!(held > 0, "{name}: leg {i}'s holders' part");
            let vault = launch::kit_vault_address(&keys);
            let before = w.env.token_amount(&vault);
            let tx = w.env.send_paid_by(
                &[companion::share_leg(crank.pubkey(), &keys, i as u8, true)],
                &crank,
                &[],
            );
            tx.ok();
            let e = tx.event::<LegSharedWithHolders>();
            assert_eq!(e.amount + e.bounty, held, "{name}");
            assert_eq!(
                w.env.token_amount(&vault) - before,
                e.amount,
                "{name}: into the kit's vault"
            );
            let pot = pots(&w, &c).legs[i];
            assert_eq!(
                (pot.pending_holders, pot.shared_total),
                (0, e.amount),
                "{name}"
            );
            // The creator address's account still holds exactly the leg's set-aside.
            assert_eq!(
                w.env
                    .token_amount(&keys.quote_account(&companion::creator_address(&c.mint))),
                pot.set_aside().unwrap(),
                "{name}"
            );
            // Nothing left: refused.
            let tx = w.env.send_paid_by(
                &[companion::share_leg(crank.pubkey(), &keys, i as u8, true)],
                &crank,
                &[],
            );
            tx.expect_code(ccode(CompanionError::NothingToDo));
        }
        if specs.len() == 1 {
            // A one-leg coin's kit streams the share over the hour.
            let k: KitConfig = w.env.read(&kit::kit_config_address(&c.mint));
            assert!(k.stream_remaining + k.stream_next > 0, "{name}: streaming");
        } else {
            // The next round counts it.
            let legs: Vec<(Pubkey, Pubkey)> = (0..3)
                .map(|i| {
                    let k = w.leg_keys(&c.mint, i);
                    (k.quote_mint, k.quote_program)
                })
                .collect();
            w.env
                .send_paid_by(
                    &[kit::start_round(crank.pubkey(), c.mint, &legs)],
                    &crank,
                    &[],
                )
                .ok();
            let r: KitRounds = w.env.read(&kit::kit_rounds_address(&c.mint));
            for i in 0..3 {
                let vault = launch::kit_vault_address(&w.leg_keys(&c.mint, i));
                assert_eq!(r.legs[i].amount, w.env.token_amount(&vault), "{name}");
            }
        }
        // Without the switch the SOL step refuses the buckets.
        let keys = w.leg_keys(&c.mint, 0);
        let tx = w.env.send_paid_by(
            &[companion::swap_bucket_to_sol(
                crank.pubkey(),
                &keys,
                0,
                0,
                vec![],
            )],
            &crank,
            &[],
        );
        tx.expect_code(ccode(CompanionError::SolSwitchOff));
        // The launcher's part, in the token, as before.
        let ix = companion::withdraw_leg(crank.pubkey(), c.launcher.pubkey(), &keys, 0);
        w.env.send_paid_by(&[ix], &crank, &[]).ok();
    }
}

#[test]
fn payees_are_paid_in_each_legs_token_and_owed_what_they_cannot_take() {
    for (name, specs) in [("1 leg, SPL Token", &ONE_SPL[..]), ("3 legs", &THREE[..])] {
        let mut w = World::new();
        let (p1, p2) = (Keypair::new(), Keypair::new());
        let split = Split {
            buyback_bps: 0,
            holders_bps: 0,
            beneficiary_bps: 10_000,
        };
        let c = coin(
            &mut w,
            specs,
            split,
            &[(p1.pubkey(), 3_000), (p2.pubkey(), 2_000)],
        );
        assert!(
            w.env
                .account(&companion::payees_legs_address(&c.mint))
                .is_some(),
            "{name}"
        );
        let crank = trade_and_claim(&mut w, &c);
        let wallets = [p1.pubkey(), p2.pubkey()];
        for i in 0..specs.len() {
            let keys = w.leg_keys(&c.mint, i);
            let part = pots(&w, &c).legs[i].pending_beneficiary;
            assert!(part > 0);
            // Without the payees' accounts: refused (nobody is paid around them).
            let ix = companion::withdraw_leg(crank.pubkey(), c.launcher.pubkey(), &keys, i as u8);
            w.env.send_paid_by(&[ix], &crank, &[]).expect_fail();
            let ix = companion::withdraw_leg_with_payees(
                crank.pubkey(),
                c.launcher.pubkey(),
                &keys,
                i as u8,
                &wallets,
            );
            let tx = w.env.send_paid_by(&[ix], &crank, &[]);
            tx.ok();
            let paid: Vec<LegPayeePaid> = tx.events();
            let p1_share = part * 3_000 / 10_000;
            let p2_share = part * 2_000 / 10_000;
            assert_eq!(
                (paid[0].amount, paid[1].amount),
                (p1_share, p2_share),
                "{name}: leg {i}"
            );
            assert_eq!(
                w.env.token_amount(&keys.quote_account(&p1.pubkey())),
                p1_share
            );
            assert_eq!(
                w.env.token_amount(&keys.quote_account(&p2.pubkey())),
                p2_share
            );
            let launcher = tx.event::<LegBeneficiaryPaid>();
            assert_eq!(
                launcher.amount,
                part - p1_share - p2_share,
                "{name}: the rest to the launcher"
            );
            assert_eq!(pots(&w, &c).legs[i].pending_beneficiary, 0);
        }
        // p1's account frozen by its token's issuer: its share waits, the others are paid.
        let keys = w.leg_keys(&c.mint, 0);
        let p1_ata = keys.quote_account(&p1.pubkey());
        let mut a = w.env.account(&p1_ata).unwrap();
        a.data[108] = 2;
        w.env.put(p1_ata, a.clone());
        let t = trader(&mut w, &c.quotes);
        let vq = w.legs(&c.mint).legs[0].virtual_quote;
        w.leg_buy(&t, &c.mint, 0, vq / 10).ok();
        w.env
            .send_paid_by(
                &[companion::claim_fees_leg(crank.pubkey(), &keys, 0, false)],
                &crank,
                &[],
            )
            .ok();
        let part = pots(&w, &c).legs[0].pending_beneficiary;
        let ix = companion::withdraw_leg_with_payees(
            crank.pubkey(),
            c.launcher.pubkey(),
            &keys,
            0,
            &wallets,
        );
        let tx = w.env.send_paid_by(&[ix], &crank, &[]);
        tx.ok();
        let paid: Vec<LegPayeePaid> = tx.events();
        let owed = part * 3_000 / 10_000;
        assert_eq!((paid[0].amount, paid[0].owed), (0, owed), "{name}: p1 owed");
        assert_eq!(paid[1].amount, part * 2_000 / 10_000);
        assert_eq!(
            pots(&w, &c).legs[0].pending_beneficiary,
            owed,
            "{name}: kept for p1"
        );
        let pl: PayeesLegs = w.env.read(&companion::payees_legs_address(&c.mint));
        assert_eq!(pl.owed[0][0], owed);
        // Thawed: the next withdrawal pays what it was owed, nobody else is paid twice.
        a.data[108] = 1;
        w.env.put(p1_ata, a);
        let before = w.env.token_amount(&p1_ata);
        let ix = companion::withdraw_leg_with_payees(
            crank.pubkey(),
            c.launcher.pubkey(),
            &keys,
            0,
            &wallets,
        );
        let tx = w.env.send_paid_by(&[ix], &crank, &[]);
        tx.ok();
        assert_eq!(
            w.env.token_amount(&p1_ata) - before,
            owed,
            "{name}: paid what it was owed"
        );
        assert_eq!(pots(&w, &c).legs[0].pending_beneficiary, 0);
        assert!(
            tx.events::<LegBeneficiaryPaid>().is_empty(),
            "{name}: the launcher had nothing new"
        );
        // Nothing to pay: refused.
        let ix = companion::withdraw_leg_with_payees(
            crank.pubkey(),
            c.launcher.pubkey(),
            &keys,
            0,
            &wallets,
        );
        w.env
            .send_paid_by(&[ix], &crank, &[])
            .expect_code(ccode(CompanionError::NothingToDo));
    }
}

// ---- Figures ------------------------------------------------------------------------------------------

fn measure(
    w: &mut World,
    what: &str,
    ixs: Vec<anchor_lang::solana_program::instruction::Instruction>,
    payer: &Keypair,
    table: &solana_message::AddressLookupTableAccount,
) -> Tx {
    let n = ixs.iter().map(|i| i.accounts.len()).max().unwrap_or(0);
    let mut all = vec![compute_unit_limit(600_000), compute_unit_price(10_000)];
    all.extend(ixs);
    let without = w.env.v0_size(&all, payer, &[], &[]);
    let tx = w.env.send_v0(&all, payer, &[], std::slice::from_ref(table));
    tx.ok();
    println!(
        "{what}: {} CU, {n} accounts, {} B with the table / {without} B without",
        tx.cu(),
        tx.size
    );
    assert!(tx.size <= 1_232, "{what}");
    tx
}

#[test]
fn measures_holder_rewards_compute_and_sizes() {
    let mut w = World::new();
    let mut t = protocol_lookup_table(&w);
    t.extend([
        spl::TOKEN_PROGRAM_ID,
        spl::TOKEN_2022_PROGRAM_ID,
        spl::ATA_PROGRAM_ID,
        kit::event_authority(),
        bordrless_kit::ID,
        launch::event_authority(),
        bordrless_launch::ID,
        companion::event_authority(),
        bordrless_companion::ID,
    ]);
    t.sort();
    t.dedup();
    let table = w.env.put_lookup_table(Pubkey::new_unique(), &t);
    for (name, specs) in [
        ("1 leg, SPL Token", &ONE_SPL[..]),
        ("1 leg, Token-2022", &ONE_2022[..]),
        ("3 legs", &THREE[..]),
        ("6 legs", &SIX[..]),
    ] {
        let c = coin(&mut w, specs, holders_split(), &[]);
        let a = trader(&mut w, &c.quotes);
        let b = trader(&mut w, &c.quotes);
        let vq = w.legs(&c.mint).legs[0].virtual_quote;
        w.leg_buy(&a, &c.mint, 0, vq / 20).ok();
        // A leg trade with the holder delta.
        {
            let (i, leg) = (0usize, "the first leg");
            let l = w.launch(&c.mint);
            let count = w.legs(&c.mint).count;
            let keys = w.leg_keys(&c.mint, i);
            let ix = launch::leg_swap_for(&l, count, &keys, b.pubkey(), b.pubkey(), 1, vq / 40, 0);
            measure(
                &mut w,
                &format!("swap_spl buy with the holder fee, {name}, {leg}"),
                vec![
                    bordrless_token::client::create_holding(b.pubkey(), c.mint, b.pubkey()),
                    ix,
                ],
                &b,
                &table,
            );
            let coins = w.env.holding(&c.mint, &b.pubkey()) / 3;
            let ix = launch::leg_swap_for(&l, count, &keys, b.pubkey(), b.pubkey(), 0, coins, 0);
            measure(
                &mut w,
                &format!("swap_spl sell with the holder fee, {name}, {leg}"),
                vec![ix],
                &b,
                &table,
            );
        }
        let crank = trader(&mut w, &c.quotes);
        let keys = w.leg_keys(&c.mint, 0);
        w.env
            .send_paid_by(
                &[companion::claim_fees_leg(crank.pubkey(), &keys, 0, false)],
                &crank,
                &[],
            )
            .ok();
        measure(
            &mut w,
            &format!("share_leg, {name}"),
            vec![companion::share_leg(crank.pubkey(), &keys, 0, true)],
            &crank,
            &table,
        );
        w.env.warp(600);
        let (q, p) = (keys.quote_mint, keys.quote_program);
        if specs.len() == 1 {
            measure(
                &mut w,
                &format!("claim_token (its account made), {name}"),
                vec![kit::claim_token(a.pubkey(), c.mint, q, p)],
                &a,
                &table,
            );
            w.env.warp(600);
            measure(
                &mut w,
                &format!("claim_token_for, {name}"),
                vec![kit::claim_token_for(
                    crank.pubkey(),
                    a.pubkey(),
                    c.mint,
                    q,
                    p,
                    None,
                )],
                &crank,
                &table,
            );
        } else {
            let legs: Vec<(Pubkey, Pubkey)> = (0..specs.len())
                .map(|i| {
                    let k = w.leg_keys(&c.mint, i);
                    (k.quote_mint, k.quote_program)
                })
                .collect();
            for i in 1..specs.len() {
                w.leg_buy(&a, &c.mint, i, w.legs(&c.mint).legs[i].virtual_quote / 40)
                    .ok();
                w.leg_buy(&b, &c.mint, i, w.legs(&c.mint).legs[i].virtual_quote / 40)
                    .ok();
            }
            let secs = i64::from(KitConfig::try_from_launch_round(&w, &c.mint));
            w.env.warp(2 * secs);
            measure(
                &mut w,
                &format!("start_round, {name}"),
                vec![kit::start_round(crank.pubkey(), c.mint, &legs)],
                &crank,
                &table,
            );
            let owner = a.pubkey();
            measure(
                &mut w,
                &format!("pay_round (its accounts made), {name}"),
                vec![kit::pay_round(crank.pubkey(), owner, c.mint, &legs, true)],
                &crank,
                &table,
            );
        }
    }
    let _: Option<AccountMeta> = None;
}

trait RoundSecs {
    fn try_from_launch_round(w: &World, mint: &Pubkey) -> u32;
}

impl RoundSecs for KitConfig {
    fn try_from_launch_round(w: &World, mint: &Pubkey) -> u32 {
        let k: KitConfig = w.env.read(&kit::kit_config_address(mint));
        k.round_secs()
    }
}
