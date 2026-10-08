//! The config marketplace: a listed config (`create_listed_config`) carries its author's share of
//! the creator fee, fixed for ever; every launch someone else makes from it pays the author that
//! part of each claim, whoever claims (the creator, or the author with `claim_author_fees`). A
//! config's author launching from it, or a plain config, pays its creator alone.

use anchor_lang::prelude::Pubkey;
use bordrless_launch::client as launch;
use bordrless_launch::error::LaunchError;
use bordrless_launch::events::{
    AuthorFeesPaid, ConfigListed, CreatorFeesClaimed, LaunchConfigCreated,
};
use bordrless_launch::instructions::{author_part, CreateConfigArgs};
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use solana_keypair::Keypair;
use solana_signer::Signer;

const SHARE: u16 = 3_000;

fn args(label: &str) -> CreateConfigArgs {
    CreateConfigArgs {
        rules: LaunchRules::NONE,
        creator_fee_bps: 100,
        custom_hook: None,
        custom_hook_flags: 0,
        label: label.to_string(),
    }
}

fn list(w: &mut World, author: &Keypair, share: u16) -> (Pubkey, Tx) {
    let config = Keypair::new();
    let ix = launch::create_listed_config(author.pubkey(), config.pubkey(), args("Listed"), share);
    let tx = w.env.send_paid_by(&[ix], author, &[&config]);
    (config.pubkey(), tx)
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

#[test]
fn a_listed_config_fixes_its_authors_share() {
    let mut w = World::new();
    let author = w.wallet_with_sol(5 * SOL);
    let (config, tx) = list(&mut w, &author, SHARE);
    tx.ok();
    assert_eq!(tx.event::<LaunchConfigCreated>().config, config);
    let ev: ConfigListed = tx.event();
    assert_eq!(
        (ev.config, ev.author, ev.author_share_bps),
        (config, author.pubkey(), SHARE)
    );
    assert_eq!(w.launch_config(&config).author_share_bps, SHARE);

    // 1 to 5,000 basis points of the creator fee: none, or more than half, is refused.
    for bad in [0, 5_001, u16::MAX] {
        list(&mut w, &author, bad)
            .1
            .expect_code(u32::from(LaunchError::InvalidAuthorShare));
    }
    list(&mut w, &author, 5_000).1.ok();

    // A plain config has none.
    let (plain, tx) = w.create_config(&author, args("Plain"));
    tx.ok();
    assert_eq!(w.launch_config(&plain).author_share_bps, 0);
}

#[test]
fn a_launch_from_someone_elses_listed_config_pays_its_author_on_every_claim() {
    let mut w = World::new();
    let author = w.wallet_with_sol(5 * SOL);
    let creator = w.wallet_with_sol(20 * SOL);
    let (config, tx) = list(&mut w, &author, SHARE);
    tx.ok();
    let (mint, tx) = w.create_launch_from_config(&creator, "MKT", VQ, &config);
    tx.ok();
    assert_eq!(
        (w.launch(&mint).author_share_bps, w.launch(&mint).config),
        (SHARE, config)
    );

    // The creator's claim needs the config and the author's holding.
    let fees = accrue(&mut w, &mint);
    let bare = w.env.send_paid_by(
        &[launch::claim_creator_fees(creator.pubkey(), mint, w.sol)],
        &creator,
        &[],
    );
    bare.expect_code(u32::from(LaunchError::AuthorAccountsMissing));
    let stranger = Keypair::new().pubkey();
    let wrong = launch::claim_creator_fees_shared(creator.pubkey(), mint, w.sol, config, stranger);
    w.env
        .send_paid_by(&[wrong], &creator, &[])
        .expect_code(u32::from(LaunchError::WrongHolding));

    let (author_before, creator_before) = (
        w.env.holding(&w.sol, &author.pubkey()),
        w.env.holding(&w.sol, &creator.pubkey()),
    );
    let ix =
        launch::claim_creator_fees_shared(creator.pubkey(), mint, w.sol, config, author.pubkey());
    let tx = w.env.send_paid_by(&[ix], &creator, &[]);
    tx.ok();
    let to_author = author_part(fees, SHARE);
    assert_eq!(to_author, (fees as u128 * 3_000 / 10_000) as u64);
    assert_eq!(
        w.env.holding(&w.sol, &author.pubkey()),
        author_before + to_author
    );
    assert_eq!(
        w.env.holding(&w.sol, &creator.pubkey()),
        creator_before + fees - to_author
    );
    assert_eq!(w.env.holding(&w.sol, &launch::launch_address(&mint)), 0);
    assert_eq!(tx.event::<CreatorFeesClaimed>().amount, fees - to_author);
    let paid: AuthorFeesPaid = tx.event();
    assert_eq!(
        (paid.author, paid.config, paid.amount, paid.paid_total),
        (author.pubkey(), config, to_author, to_author)
    );
    let l = w.launch(&mint);
    assert_eq!(
        (l.creator_fees_claimed, l.author_fees_paid),
        (fees - to_author, to_author)
    );

    // The author claims the next round; the creator's part goes to the creator at the same time.
    w.env.warp(1);
    let more = accrue(&mut w, &mint);
    let (author_before, creator_before) = (
        w.env.holding(&w.sol, &author.pubkey()),
        w.env.holding(&w.sol, &creator.pubkey()),
    );
    let ix = launch::claim_author_fees(author.pubkey(), mint, w.sol, config, creator.pubkey());
    let tx = w.env.send_paid_by(&[ix], &author, &[]);
    tx.ok();
    let second = author_part(more, SHARE);
    assert_eq!(
        w.env.holding(&w.sol, &author.pubkey()),
        author_before + second
    );
    assert_eq!(
        w.env.holding(&w.sol, &creator.pubkey()),
        creator_before + more - second
    );
    assert_eq!(tx.event::<AuthorFeesPaid>().paid_total, to_author + second);
    assert_eq!(w.launch(&mint).author_fees_paid, to_author + second);

    // Nobody but the author can claim as the author; nothing left, nothing claimed.
    w.env.warp(1);
    accrue(&mut w, &mint);
    let ix = launch::claim_author_fees(creator.pubkey(), mint, w.sol, config, creator.pubkey());
    w.env
        .send_paid_by(&[ix], &creator, &[])
        .expect_code(u32::from(LaunchError::NotAuthor));
    let ix = launch::claim_author_fees(author.pubkey(), mint, w.sol, config, creator.pubkey());
    w.env.send_paid_by(&[ix], &author, &[]).ok();
    w.env.warp(1);
    let ix = launch::claim_author_fees(author.pubkey(), mint, w.sol, config, creator.pubkey());
    w.env
        .send_paid_by(&[ix], &author, &[])
        .expect_code(u32::from(LaunchError::NothingToClaim));
}

#[test]
fn the_author_launching_from_their_own_config_and_plain_configs_pay_the_creator_alone() {
    let mut w = World::new();
    let author = w.wallet_with_sol(30 * SOL);
    let (config, tx) = list(&mut w, &author, SHARE);
    tx.ok();
    let (own, tx) = w.create_launch_from_config(&author, "OWN", VQ, &config);
    tx.ok();
    assert_eq!(w.launch(&own).author_share_bps, 0);
    let fees = accrue(&mut w, &own);
    let before = w.env.holding(&w.sol, &author.pubkey());
    w.env
        .send_paid_by(
            &[launch::claim_creator_fees(author.pubkey(), own, w.sol)],
            &author,
            &[],
        )
        .ok();
    assert_eq!(w.env.holding(&w.sol, &author.pubkey()), before + fees);
    // There is no author share to claim on it.
    let ix = launch::claim_author_fees(author.pubkey(), own, w.sol, config, author.pubkey());
    w.env
        .send_paid_by(&[ix], &author, &[])
        .expect_code(u32::from(LaunchError::NoAuthorShare));

    // A launch from a plain config, or with inline rules, keeps its claim as it was.
    let creator = w.wallet_with_sol(20 * SOL);
    let (plain, tx) = w.create_config(&author, args("Plain"));
    tx.ok();
    let (mint, tx) = w.create_launch_from_config(&creator, "PLN", VQ, &plain);
    tx.ok();
    assert_eq!(w.launch(&mint).author_share_bps, 0);
    let fees = accrue(&mut w, &mint);
    let before = w.env.holding(&w.sol, &creator.pubkey());
    let tx = w.env.send_paid_by(
        &[launch::claim_creator_fees(creator.pubkey(), mint, w.sol)],
        &creator,
        &[],
    );
    tx.ok();
    assert_eq!(w.env.holding(&w.sol, &creator.pubkey()), before + fees);
    assert_eq!(w.launch(&mint).author_fees_paid, 0);
}
