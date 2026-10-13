//! The partner fee (`docs/partner-fee.md`): a partner config (`create_partner_config`) names a
//! partner and its share of the creator fee (1 to 10,000 basis points of it), fixed for ever;
//! every launch made from it, whoever makes it, pays the partner that part of each claim, as SOL to
//! its holding. Every claim pays every party at once: the creator's, the author's, and anyone's
//! `distribute_fees`. Configs and launches made before keep their claims as they were.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::AccountMeta;
use bordrless_companion::client as companion;
use bordrless_companion::instructions::CreateArgs;
use bordrless_companion::state::Split;
use bordrless_core::policy;
use bordrless_launch::client as launch;
use bordrless_launch::error::LaunchError;
use bordrless_launch::events::{
    AuthorFeesPaid, ConfigListed, CreatorFeesClaimed, LaunchPartner, PartnerConfigured,
    PartnerFeesPaid,
};
use bordrless_launch::instructions::{author_part, fee_split, CreateConfigArgs};
use bordrless_launch::state::{LaunchRules, PartnerFee, QuoteCut};
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_token::client as token;
use solana_keypair::Keypair;
use solana_signer::Signer;

const NO_CUT: QuoteCut = QuoteCut {
    max_buy_bps: 0,
    max_sell_bps: 0,
};

fn args(label: &str) -> CreateConfigArgs {
    CreateConfigArgs {
        rules: LaunchRules::NONE,
        creator_fee_bps: 100,
        custom_hook: None,
        custom_hook_flags: 0,
        label: label.to_string(),
    }
}

fn code(e: LaunchError) -> u32 {
    u32::from(e)
}

/// A partner config by `author`, paying `partner` `share` (and the author `author_share`).
fn partner_config(
    w: &mut World,
    author: &Keypair,
    partner: Pubkey,
    share: u16,
    author_share: u16,
) -> (Pubkey, Tx) {
    let config = Keypair::new();
    let ix = launch::create_partner_config(
        author.pubkey(),
        config.pubkey(),
        args("Partner"),
        author_share,
        NO_CUT,
        partner,
        share,
    );
    let tx = w.env.send_paid_by(&[ix], author, &[&config]);
    (config.pubkey(), tx)
}

/// A partner wallet with a holding of bridged SOL (made by `payer`).
fn partner_wallet(w: &mut World, payer: &Keypair) -> Pubkey {
    let partner = Keypair::new().pubkey();
    w.env
        .send_paid_by(
            &[token::create_holding(payer.pubkey(), w.sol, partner)],
            payer,
            &[],
        )
        .ok();
    partner
}

/// Trades on `mint` until its creator fees have accrued; returns what the launch holds.
fn accrue(w: &mut World, mint: &Pubkey) -> u64 {
    w.env.warp(31);
    let trader = w.wallet_with_sol(10 * SOL);
    w.holdings(&trader, *mint, &[trader.pubkey()]);
    w.buy(&trader, mint, 2 * SOL).ok();
    let held = w.env.holding(mint, &trader.pubkey());
    w.sell(&trader, mint, held / 3).ok();
    let fees = w.env.holding(&w.sol, &launch::launch_address(mint));
    assert!(fees > 0);
    fees
}

fn sol_of(w: &World, owner: &Pubkey) -> u64 {
    w.env.holding(&w.sol, owner)
}

#[test]
fn the_split_adds_up_and_rounds_for_the_creator() {
    for amount in [
        0u64,
        1,
        2,
        3,
        7,
        9_999,
        10_000,
        10_001,
        123_456_789,
        u64::MAX,
    ] {
        for (a, p) in [
            (0, 0),
            (0, 1),
            (0, 10_000),
            (5_000, 5_000),
            (3_000, 2_000),
            (1, 9_999),
        ] {
            let (c, au, pa) = fee_split(amount, a, p).unwrap();
            assert_eq!(c as u128 + au as u128 + pa as u128, amount as u128);
            assert_eq!((au, pa), (author_part(amount, a), author_part(amount, p)));
        }
    }
    // All to the partner: nothing left for the creator.
    assert_eq!(fee_split(1_000, 0, 10_000).unwrap(), (0, 0, 1_000));
}

#[test]
fn a_partner_config_fixes_its_partner_and_every_refusal() {
    let mut w = World::new();
    let author = w.wallet_with_sol(5 * SOL);
    let partner = Keypair::new().pubkey();
    let (config, tx) = partner_config(&mut w, &author, partner, 2_500, 0);
    tx.ok();
    let ev: PartnerConfigured = tx.event();
    assert_eq!(
        (ev.config, ev.partner, ev.partner_share_bps),
        (config, partner, 2_500)
    );
    let lc = w.launch_config(&config);
    assert_eq!(
        (lc.partner_share_bps(), lc.author_share_bps, lc.creator),
        (2_500, 0, author.pubkey())
    );
    let (pf_key, bump) = PartnerFee::address(&config);
    assert_eq!(lc.partner_bump(), bump);
    let pf: PartnerFee = w.env.read(&pf_key);
    assert_eq!(
        (pf.config, pf.partner, pf.share_bps, pf.paid_total, pf.bump),
        (config, partner, 2_500, 0, bump)
    );
    // No author share: not listed.
    assert!(tx.events::<ConfigListed>().is_empty());

    // All of the creator fee may go to the partner.
    partner_config(&mut w, &author, partner, 10_000, 0).1.ok();
    // With an author's share too: listed, at most 10,000 in all.
    let (both, tx) = partner_config(&mut w, &author, partner, 5_000, 5_000);
    tx.ok();
    assert_eq!(tx.event::<ConfigListed>().author_share_bps, 5_000);
    assert_eq!(w.launch_config(&both).author_share_bps, 5_000);

    for (share, author_share, e) in [
        (0, 0, LaunchError::InvalidPartnerShare),
        (10_001, 0, LaunchError::InvalidPartnerShare),
        (u16::MAX, 0, LaunchError::InvalidPartnerShare),
        (5_001, 5_000, LaunchError::InvalidPartnerShare),
        (10_000, 1, LaunchError::InvalidPartnerShare),
        (1_000, 5_001, LaunchError::InvalidAuthorShare),
    ] {
        partner_config(&mut w, &author, partner, share, author_share)
            .1
            .expect_code(code(e));
    }
    partner_config(&mut w, &author, Pubkey::default(), 1_000, 0)
        .1
        .expect_code(code(LaunchError::InvalidPartner));

    // A SOL cut needs a custom hook, as `create_quote_cut_config` requires.
    let c = Keypair::new();
    let cut = QuoteCut {
        max_buy_bps: 100,
        max_sell_bps: 100,
    };
    let ix = launch::create_partner_config(
        author.pubkey(),
        c.pubkey(),
        args("Cut"),
        0,
        cut,
        partner,
        1_000,
    );
    w.env
        .send_paid_by(&[ix], &author, &[&c])
        .expect_code(code(LaunchError::InvalidQuoteCut));

    // Plain and listed configs carry no partner, as before.
    let (plain, tx) = w.create_config(&author, args("Plain"));
    tx.ok();
    assert_eq!(w.launch_config(&plain).partner_share_bps(), 0);
    let listed = Keypair::new();
    w.env
        .send_paid_by(
            &[launch::create_listed_config(
                author.pubkey(),
                listed.pubkey(),
                args("Listed"),
                3_000,
            )],
            &author,
            &[&listed],
        )
        .ok();
    assert_eq!(w.launch_config(&listed.pubkey()).partner_share_bps(), 0);
}

#[test]
fn every_launch_from_a_partner_config_pays_the_partner_on_every_claim() {
    let mut w = World::new();
    let author = w.wallet_with_sol(5 * SOL);
    let creator = w.wallet_with_sol(20 * SOL);
    let partner = partner_wallet(&mut w, &author);
    let (config, tx) = partner_config(&mut w, &author, partner, 2_500, 0);
    tx.ok();
    let (mint, tx) = w.create_launch_from_config(&creator, "PTR", VQ, &config);
    tx.ok();
    let l = w.launch(&mint);
    assert_eq!(
        (l.partner_share_bps(), l.author_share_bps, l.config),
        (2_500, 0, config)
    );
    let ev: LaunchPartner = tx.event();
    assert_eq!(
        (ev.mint, ev.config, ev.partner_share_bps),
        (mint, config, 2_500)
    );

    // The creator's claim needs the partner's accounts, exactly.
    let fees = accrue(&mut w, &mint);
    let bare = launch::claim_creator_fees(creator.pubkey(), mint, w.sol);
    w.env
        .send_paid_by(std::slice::from_ref(&bare), &creator, &[])
        .expect_code(code(LaunchError::PartnerAccountsMissing));
    let stranger = Keypair::new().pubkey();
    let wrong = launch::claim_creator_fees_with(
        creator.pubkey(),
        mint,
        w.sol,
        None,
        Some((config, stranger)),
    );
    w.env
        .send_paid_by(&[wrong], &creator, &[])
        .expect_code(code(LaunchError::WrongHolding));
    // The partner account read-only, or another config's: refused.
    let mut read_only = launch::claim_creator_fees_with(
        creator.pubkey(),
        mint,
        w.sol,
        None,
        Some((config, partner)),
    );
    let n = read_only.accounts.len();
    read_only.accounts[n - 2].is_writable = false;
    w.env
        .send_paid_by(&[read_only], &creator, &[])
        .expect_code(code(LaunchError::PartnerAccountsMissing));
    let (other, tx) = partner_config(&mut w, &author, partner, 2_500, 0);
    tx.ok();
    let mut other_ix = bare.clone();
    other_ix
        .accounts
        .extend(launch::partner_accounts(w.sol, other, partner));
    w.env
        .send_paid_by(&[other_ix], &creator, &[])
        .expect_code(code(LaunchError::PartnerAccountsMissing));

    let (p0, c0) = (sol_of(&w, &partner), sol_of(&w, &creator.pubkey()));
    let ix = launch::claim_creator_fees_with(
        creator.pubkey(),
        mint,
        w.sol,
        None,
        Some((config, partner)),
    );
    let tx = w.env.send_paid_by(&[ix], &creator, &[]);
    tx.ok();
    let to_partner = author_part(fees, 2_500);
    assert_eq!(sol_of(&w, &partner), p0 + to_partner);
    assert_eq!(sol_of(&w, &creator.pubkey()), c0 + fees - to_partner);
    assert_eq!(w.env.holding(&w.sol, &launch::launch_address(&mint)), 0);
    assert_eq!(tx.event::<CreatorFeesClaimed>().amount, fees - to_partner);
    let paid: PartnerFeesPaid = tx.event();
    assert_eq!(
        (
            paid.partner,
            paid.config,
            paid.mint,
            paid.amount,
            paid.paid_total
        ),
        (partner, config, mint, to_partner, to_partner)
    );
    assert!(tx.events::<AuthorFeesPaid>().is_empty());
    let pf: PartnerFee = w.env.read(&launch::partner_fee_address(&config));
    assert_eq!(pf.paid_total, to_partner);
    assert_eq!(w.launch(&mint).creator_fees_claimed, fees - to_partner);

    // Anyone distributes the next round: no signature but the fee payer's, same split.
    let more = accrue(&mut w, &mint);
    let cranker = w.wallet_with_sol(SOL);
    let (p1, c1) = (sol_of(&w, &partner), sol_of(&w, &creator.pubkey()));
    let ix = launch::distribute_fees(mint, w.sol, config, creator.pubkey(), partner, None);
    assert!(ix.accounts.iter().all(|a| !a.is_signer));
    let tx = w.env.send_paid_by(&[ix], &cranker, &[]);
    tx.ok();
    let second = author_part(more, 2_500);
    assert_eq!(sol_of(&w, &partner), p1 + second);
    assert_eq!(sol_of(&w, &creator.pubkey()), c1 + more - second);
    assert_eq!(
        tx.event::<PartnerFeesPaid>().paid_total,
        to_partner + second
    );

    // A second launch from the same config counts into the same total.
    let other_creator = w.wallet_with_sol(20 * SOL);
    let (mint2, tx) = w.create_launch_from_config(&other_creator, "PT2", VQ, &config);
    tx.ok();
    let fees2 = accrue(&mut w, &mint2);
    let ix = launch::distribute_fees(mint2, w.sol, config, other_creator.pubkey(), partner, None);
    let tx = w.env.send_paid_by(&[ix], &cranker, &[]);
    tx.ok();
    let third = author_part(fees2, 2_500);
    assert_eq!(
        tx.event::<PartnerFeesPaid>().paid_total,
        to_partner + second + third
    );

    // Nothing left: nothing distributed. Distribution pays only the fixed holdings.
    w.env.warp(1);
    let ix = launch::distribute_fees(mint, w.sol, config, creator.pubkey(), partner, None);
    w.env
        .send_paid_by(&[ix], &cranker, &[])
        .expect_code(code(LaunchError::NothingToClaim));
    accrue(&mut w, &mint);
    let thief = Keypair::new().pubkey();
    for ix in [
        launch::distribute_fees(mint, w.sol, config, thief, partner, None),
        launch::distribute_fees(mint, w.sol, config, creator.pubkey(), thief, None),
    ] {
        w.env
            .send_paid_by(&[ix], &cranker, &[])
            .expect_code(code(LaunchError::WrongHolding));
    }
}

#[test]
fn distribution_is_only_for_launches_that_pay_a_partner() {
    let mut w = World::new();
    let author = w.wallet_with_sol(5 * SOL);
    let creator = w.wallet_with_sol(20 * SOL);
    let partner = partner_wallet(&mut w, &author);
    let (plain, tx) = w.create_config(&author, args("Plain"));
    tx.ok();
    let (mint, tx) = w.create_launch_from_config(&creator, "PLN", VQ, &plain);
    tx.ok();
    assert_eq!(w.launch(&mint).partner_share_bps(), 0);
    let fees = accrue(&mut w, &mint);
    let cranker = w.wallet_with_sol(SOL);
    // Its config has no partner account at all: refused before anything is read further.
    let ix = launch::distribute_fees(mint, w.sol, plain, creator.pubkey(), partner, None);
    w.env.send_paid_by(&[ix], &cranker, &[]).expect_code(3012); // AccountNotInitialized
                                                                // Its creator claims as before, with no other account.
    let before = sol_of(&w, &creator.pubkey());
    w.env
        .send_paid_by(
            &[launch::claim_creator_fees(creator.pubkey(), mint, w.sol)],
            &creator,
            &[],
        )
        .ok();
    assert_eq!(sol_of(&w, &creator.pubkey()), before + fees);

    // A partner account of another config can't stand in for a launch's own.
    let (config, tx) = partner_config(&mut w, &author, partner, 2_500, 0);
    tx.ok();
    let (pmint, tx) = w.create_launch_from_config(&creator, "PTR", VQ, &config);
    tx.ok();
    let (other, tx) = partner_config(&mut w, &author, partner, 9_000, 0);
    tx.ok();
    accrue(&mut w, &pmint);
    let ix = launch::distribute_fees(pmint, w.sol, other, creator.pubkey(), partner, None);
    w.env.send_paid_by(&[ix], &cranker, &[]).expect_code(2006); // ConstraintSeeds
}

#[test]
fn all_to_the_partner_and_the_creator_is_paid_nothing() {
    let mut w = World::new();
    let author = w.wallet_with_sol(5 * SOL);
    let creator = w.wallet_with_sol(20 * SOL);
    let partner = partner_wallet(&mut w, &author);
    let (config, tx) = partner_config(&mut w, &author, partner, 10_000, 0);
    tx.ok();
    let (mint, tx) = w.create_launch_from_config(&creator, "ALL", VQ, &config);
    tx.ok();
    let fees = accrue(&mut w, &mint);
    let (p0, c0) = (sol_of(&w, &partner), sol_of(&w, &creator.pubkey()));
    let ix = launch::claim_creator_fees_with(
        creator.pubkey(),
        mint,
        w.sol,
        None,
        Some((config, partner)),
    );
    let tx = w.env.send_paid_by(&[ix], &creator, &[]);
    tx.ok();
    assert_eq!(sol_of(&w, &partner), p0 + fees);
    assert_eq!(sol_of(&w, &creator.pubkey()), c0);
    assert_eq!(tx.event::<CreatorFeesClaimed>().amount, 0);
    assert_eq!(tx.event::<PartnerFeesPaid>().amount, fees);
}

#[test]
fn a_partner_and_an_author_and_the_creator_share_each_claim() {
    let mut w = World::new();
    let author = w.wallet_with_sol(5 * SOL);
    let creator = w.wallet_with_sol(20 * SOL);
    let partner = partner_wallet(&mut w, &author);
    let (config, tx) = partner_config(&mut w, &author, partner, 2_000, 3_000);
    tx.ok();
    let (mint, tx) = w.create_launch_from_config(&creator, "TRI", VQ, &config);
    tx.ok();
    let l = w.launch(&mint);
    assert_eq!((l.author_share_bps, l.partner_share_bps()), (3_000, 2_000));

    // The creator's claim: the author's accounts, then the partner's; either missing is refused.
    let fees = accrue(&mut w, &mint);
    let only_author = launch::claim_creator_fees_with(
        creator.pubkey(),
        mint,
        w.sol,
        Some((config, author.pubkey())),
        None,
    );
    w.env
        .send_paid_by(&[only_author], &creator, &[])
        .expect_code(code(LaunchError::PartnerAccountsMissing));
    let only_partner = launch::claim_creator_fees_with(
        creator.pubkey(),
        mint,
        w.sol,
        None,
        Some((config, partner)),
    );
    w.env
        .send_paid_by(&[only_partner], &creator, &[])
        .expect_code(code(LaunchError::AuthorAccountsMissing));
    let before = (
        sol_of(&w, &creator.pubkey()),
        sol_of(&w, &author.pubkey()),
        sol_of(&w, &partner),
    );
    let ix = launch::claim_creator_fees_with(
        creator.pubkey(),
        mint,
        w.sol,
        Some((config, author.pubkey())),
        Some((config, partner)),
    );
    let tx = w.env.send_paid_by(&[ix], &creator, &[]);
    tx.ok();
    let (c, a, p) = fee_split(fees, 3_000, 2_000).unwrap();
    assert_eq!(
        (
            sol_of(&w, &creator.pubkey()),
            sol_of(&w, &author.pubkey()),
            sol_of(&w, &partner)
        ),
        (before.0 + c, before.1 + a, before.2 + p)
    );
    assert_eq!(
        (
            tx.event::<CreatorFeesClaimed>().amount,
            tx.event::<AuthorFeesPaid>().amount,
            tx.event::<PartnerFeesPaid>().amount
        ),
        (c, a, p)
    );

    // The author's claim needs the partner's accounts too.
    let more = accrue(&mut w, &mint);
    let ix = launch::claim_author_fees(author.pubkey(), mint, w.sol, config, creator.pubkey());
    w.env
        .send_paid_by(&[ix], &author, &[])
        .expect_code(code(LaunchError::PartnerAccountsMissing));
    let before = (
        sol_of(&w, &creator.pubkey()),
        sol_of(&w, &author.pubkey()),
        sol_of(&w, &partner),
    );
    let ix = launch::claim_author_fees_with_partner(
        author.pubkey(),
        mint,
        w.sol,
        config,
        creator.pubkey(),
        partner,
    );
    w.env.send_paid_by(&[ix], &author, &[]).ok();
    let (c, a, p) = fee_split(more, 3_000, 2_000).unwrap();
    assert_eq!(
        (
            sol_of(&w, &creator.pubkey()),
            sol_of(&w, &author.pubkey()),
            sol_of(&w, &partner)
        ),
        (before.0 + c, before.1 + a, before.2 + p)
    );

    // Distribution: the author's accounts are required, and it pays all three.
    let again = accrue(&mut w, &mint);
    let cranker = w.wallet_with_sol(SOL);
    let ix = launch::distribute_fees(mint, w.sol, config, creator.pubkey(), partner, None);
    w.env
        .send_paid_by(&[ix], &cranker, &[])
        .expect_code(code(LaunchError::AuthorAccountsMissing));
    let before = (
        sol_of(&w, &creator.pubkey()),
        sol_of(&w, &author.pubkey()),
        sol_of(&w, &partner),
    );
    let ix = launch::distribute_fees(
        mint,
        w.sol,
        config,
        creator.pubkey(),
        partner,
        Some(author.pubkey()),
    );
    w.env.send_paid_by(&[ix], &cranker, &[]).ok();
    let (c, a, p) = fee_split(again, 3_000, 2_000).unwrap();
    assert_eq!(
        (
            sol_of(&w, &creator.pubkey()),
            sol_of(&w, &author.pubkey()),
            sol_of(&w, &partner)
        ),
        (before.0 + c, before.1 + a, before.2 + p)
    );
    let l = w.launch(&mint);
    assert_eq!(
        l.author_fees_paid,
        tx.event::<AuthorFeesPaid>().paid_total + a + {
            let (_, a2, _) = fee_split(more, 3_000, 2_000).unwrap();
            a2
        }
    );
}

#[test]
fn the_author_launching_from_their_own_partner_config_still_pays_the_partner() {
    let mut w = World::new();
    let author = w.wallet_with_sol(30 * SOL);
    let partner = partner_wallet(&mut w, &author);
    let (config, tx) = partner_config(&mut w, &author, partner, 4_000, 3_000);
    tx.ok();
    let (mint, tx) = w.create_launch_from_config(&author, "OWN", VQ, &config);
    tx.ok();
    let l = w.launch(&mint);
    assert_eq!((l.author_share_bps, l.partner_share_bps()), (0, 4_000));
    let fees = accrue(&mut w, &mint);
    let (a0, p0) = (sol_of(&w, &author.pubkey()), sol_of(&w, &partner));
    let ix = launch::claim_creator_fees_with(
        author.pubkey(),
        mint,
        w.sol,
        None,
        Some((config, partner)),
    );
    w.env.send_paid_by(&[ix], &author, &[]).ok();
    let to_partner = author_part(fees, 4_000);
    assert_eq!(sol_of(&w, &partner), p0 + to_partner);
    assert_eq!(sol_of(&w, &author.pubkey()), a0 + fees - to_partner);
}

#[test]
fn the_partner_may_be_the_creator_and_a_missing_holding_only_waits() {
    let mut w = World::new();
    let author = w.wallet_with_sol(5 * SOL);
    let creator = w.wallet_with_sol(20 * SOL);
    // The creator names itself the partner: both parts reach its holding.
    let (config, tx) = partner_config(&mut w, &author, creator.pubkey(), 3_000, 0);
    tx.ok();
    let (mint, tx) = w.create_launch_from_config(&creator, "SLF", VQ, &config);
    tx.ok();
    let fees = accrue(&mut w, &mint);
    let c0 = sol_of(&w, &creator.pubkey());
    let ix = launch::claim_creator_fees_with(
        creator.pubkey(),
        mint,
        w.sol,
        None,
        Some((config, creator.pubkey())),
    );
    w.env.send_paid_by(&[ix], &creator, &[]).ok();
    assert_eq!(sol_of(&w, &creator.pubkey()), c0 + fees);

    // A partner with no holding yet: the claim fails and the fees wait; anyone makes the holding
    // (it is idempotent) and the claim goes through.
    let partner = Keypair::new().pubkey();
    let (config, tx) = partner_config(&mut w, &author, partner, 3_000, 0);
    tx.ok();
    let (mint, tx) = w.create_launch_from_config(&creator, "WAIT", VQ, &config);
    tx.ok();
    let fees = accrue(&mut w, &mint);
    let ix = launch::claim_creator_fees_with(
        creator.pubkey(),
        mint,
        w.sol,
        None,
        Some((config, partner)),
    );
    assert!(w
        .env
        .send_paid_by(std::slice::from_ref(&ix), &creator, &[])
        .result
        .is_err());
    assert_eq!(w.env.holding(&w.sol, &launch::launch_address(&mint)), fees);
    w.env.warp(1);
    let tx = w.env.send_paid_by(
        &[token::create_holding(creator.pubkey(), w.sol, partner), ix],
        &creator,
        &[],
    );
    tx.ok();
    assert_eq!(sol_of(&w, &partner), author_part(fees, 3_000));
}

#[test]
fn a_spoofed_partner_account_is_refused() {
    let mut w = World::new();
    let author = w.wallet_with_sol(5 * SOL);
    let creator = w.wallet_with_sol(20 * SOL);
    let partner = partner_wallet(&mut w, &author);
    let (config, tx) = partner_config(&mut w, &author, partner, 2_500, 0);
    tx.ok();
    let (mint, tx) = w.create_launch_from_config(&creator, "SPF", VQ, &config);
    tx.ok();
    accrue(&mut w, &mint);
    // A copy of the real partner account, naming the creator, at another address.
    let real = launch::partner_fee_address(&config);
    let mut account = w.env.account(&real).unwrap();
    let mut pf: PartnerFee = w.env.read(&real);
    pf.partner = creator.pubkey();
    let mut data = Vec::new();
    anchor_lang::AccountSerialize::try_serialize(&pf, &mut data).unwrap();
    account.data = data;
    let fake = Keypair::new().pubkey();
    w.env.svm.set_account(fake, account).unwrap();
    let mut ix = launch::claim_creator_fees(creator.pubkey(), mint, w.sol);
    ix.accounts.push(AccountMeta::new(fake, false));
    ix.accounts.push(AccountMeta::new(
        token::holding_address(&w.sol, &creator.pubkey()),
        false,
    ));
    w.env
        .send_paid_by(&[ix], &creator, &[])
        .expect_code(code(LaunchError::PartnerAccountsMissing));
}

const BUYBACK: Split = Split {
    buyback_bps: 10_000,
    holders_bps: 0,
    beneficiary_bps: 0,
};

/// A companion coin can't launch from a partner config: its creator fees are the companion's to
/// split (buybacks, pots, a script's author, payees), and a partner would take its part first
/// (`PartnerOnCompanion`). The same config launches a plain coin, and a companion coin launches from
/// a plain config, as before.
#[test]
fn a_companion_coin_cant_launch_from_a_partner_config() {
    let mut w = World::new();
    let author = w.wallet_with_sol(5 * SOL);
    let partner = partner_wallet(&mut w, &author);
    let (config, tx) = partner_config(&mut w, &author, partner, 2_500, 0);
    tx.ok();
    let launcher = w.wallet_with_sol(5 * SOL);
    let through = |w: &mut World, config: Pubkey| -> Tx {
        let mint = Keypair::new();
        let cargs = CreateArgs {
            split: BUYBACK,
            bounty_bps: 50,
            max_buyback: SOL,
            buyback_interval: 60,
            vest_secs: 0,
            fund: SOL / 2,
        };
        w.env
            .send_paid_by(
                &[companion::create(
                    launcher.pubkey(),
                    launcher.pubkey(),
                    mint.pubkey(),
                    cargs,
                )],
                &launcher,
                &[&mint],
            )
            .ok();
        let creator = companion::creator_address(&mint.pubkey());
        let a = World::launch_args("CMP", 100, VQ, LaunchRules::NONE);
        let inner = launch::create_launch_with(
            creator,
            mint.pubkey(),
            w.env.treasury.pubkey(),
            w.sol,
            policy::LP_FEE_BPS,
            a.clone(),
            Some(config),
            None,
        );
        w.env.send_paid_by(
            &[companion::launch(
                launcher.pubkey(),
                mint.pubkey(),
                &inner,
                a,
            )],
            &launcher,
            &[&mint],
        )
    };
    through(&mut w, config).expect_code(code(LaunchError::PartnerOnCompanion));
    let (plain, tx) = w.create_config(&author, args("Plain"));
    tx.ok();
    through(&mut w, plain).ok();
    // The partner config still launches a plain coin.
    let creator = w.wallet_with_sol(20 * SOL);
    w.create_launch_from_config(&creator, "PLN", VQ, &config)
        .1
        .ok();
}
