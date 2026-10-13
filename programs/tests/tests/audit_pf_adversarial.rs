//! Independent adversarial audit of the partner fee (`docs/partner-fee.md`), 2026-10-13.
//!
//! Naming:
//! - `finding_*`: none remain. The two companion findings of the first audited tree were fixed
//!   during the audit (`PartnerOnCompanion`); their tests now assert the refusal (`sound_*`).
//!   (Original convention: asserts the SAFE behaviour and FAILS on the bug; the failure IS the
//!   proof of the finding.)
//! - `sound_*`: an attack that the current code refuses (or a property that holds). PASSES.
//! - `info_*`: documents a behaviour (passes); see the audit report for its weight.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::AccountMeta;
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::instructions::{CreateArgs, CreateScriptArgs};
use bordrless_companion::state::{Script, Split};
use bordrless_core::policy;
use bordrless_launch::client as launch;
use bordrless_launch::error::LaunchError;
use bordrless_launch::events::{CreatorFeesClaimed, PartnerFeesPaid};
use bordrless_launch::instructions::{author_part, fee_split, CreateConfigArgs};
use bordrless_launch::state::{LaunchRules, PartnerFee, QuoteCut};
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::program_bytes;
use bordrless_program_tests::timelock::programdata_hash;
use bordrless_token::client as token;
use script_example as se;
use solana_keypair::Keypair;
use solana_signer::Signer;

const NO_CUT: QuoteCut = QuoteCut {
    max_buy_bps: 0,
    max_sell_bps: 0,
};
const FEE: u16 = 200;

fn args(label: &str) -> CreateConfigArgs {
    CreateConfigArgs {
        rules: LaunchRules::NONE,
        creator_fee_bps: FEE,
        custom_hook: None,
        custom_hook_flags: 0,
        label: label.to_string(),
    }
}

fn code(e: LaunchError) -> u32 {
    u32::from(e)
}

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

fn listed_config(w: &mut World, author: &Keypair, share: u16) -> Pubkey {
    let c = Keypair::new();
    w.env
        .send_paid_by(
            &[launch::create_listed_config(
                author.pubkey(),
                c.pubkey(),
                args("Listed"),
                share,
            )],
            author,
            &[&c],
        )
        .ok();
    c.pubkey()
}

fn accrue(w: &mut World, mint: &Pubkey) -> u64 {
    w.env.warp(31);
    for _ in 0..2 {
        let trader = w.wallet_with_sol(4 * SOL);
        w.buy(&trader, mint, 2 * SOL).ok();
        let held = w.env.holding(mint, &trader.pubkey());
        w.sell(&trader, mint, held / 3).ok();
    }
    let fees = w.env.holding(&w.sol, &launch::launch_address(mint));
    assert!(fees > 0);
    fees
}

fn sol_of(w: &World, owner: &Pubkey) -> u64 {
    w.env.holding(&w.sol, owner)
}

const BUYBACK: Split = Split {
    buyback_bps: 10_000,
    holders_bps: 0,
    beneficiary_bps: 0,
};

/// Makes a plain companion for a fresh mint and launches it through the companion from `config`.
fn companion_from_config(w: &mut World, launcher: &Keypair, config: Pubkey) -> (Pubkey, Tx) {
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
            launcher,
            &[&mint],
        )
        .ok();
    let creator = companion::creator_address(&mint.pubkey());
    let a = World::launch_args("CMP", FEE, VQ, LaunchRules::NONE);
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
    let tx = w.env.send_paid_by(
        &[companion::launch(
            launcher.pubkey(),
            mint.pubkey(),
            &inner,
            a,
        )],
        launcher,
        &[&mint],
    );
    (mint.pubkey(), tx)
}

// ------------------------------------------------------------------------------------------------
// FINDINGS (fail on the current code)
// ------------------------------------------------------------------------------------------------

/// Was FINDING (high) in the first audited tree; FIXED by `PartnerOnCompanion`
/// (launch.rs, the guard in `plan`). The live companion refuses a launch that pays a config author
/// but cannot see a partner share: a companion's launcher (who holds the mint keypair) launched it
/// from their own partner config naming their own wallet at 10,000 bps, and every lamport of
/// creator fee bypassed the companion's split (buyback / holders / pot / bounty / script author).
/// Now refused.
#[test]
fn sound_companion_launch_from_a_partner_config_is_refused() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(10 * SOL);
    let sink = w.wallet_with_sol(SOL);
    let (config, tx) = partner_config(&mut w, &launcher, sink.pubkey(), 10_000, 0);
    tx.ok();
    let (mint, tx) = companion_from_config(&mut w, &launcher, config);
    tx.expect_code(code(LaunchError::PartnerOnCompanion));
    assert!(w
        .env
        .try_read::<bordrless_launch::state::Launch>(&launch::launch_address(&mint))
        .is_none());
    // The same partner config still serves an ordinary wallet's launch.
    let creator = w.wallet_with_sol(5 * SOL);
    w.create_launch_from_config(&creator, "OK", VQ, &config)
        .1
        .ok();
}

/// Was FINDING (medium) in the first audited tree; FIXED by the same guard.
/// The script companion's author earns `AUTHOR_SHARE_BPS` (50%) of "what each claim takes from the
/// launch", measured as the delta of the companion's holding across its own `claim_creator_fees`
/// CPI. A launch from a partner config can only be paid through `distribute_fees` (the CPI lacks
/// the partner's accounts), which lands the fees before the companion measures: the author's delta
/// is 0. A launcher uses a 1-bps partner config naming itself and never pays the script author.
#[test]
fn sound_script_author_share_bypass_via_partner_config_is_refused() {
    let mut w = World::new();
    w.env
        .svm
        .add_program(se::ID, &program_bytes("script_example"))
        .expect("load the script");
    let bordrless_key = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[1];
    w.env.set_upgrade_authority(se::ID, Some(bordrless_key));
    let author = w.env.funded(SOL);
    let deployer = w.env.deployer.insecure_clone();
    let hash = programdata_hash(&w.env, &se::ID);
    w.env
        .send(
            &[companion::approve(
                deployer.pubkey(),
                se::ID,
                APPROVAL_KIND_SCRIPT,
                hash,
                author.pubkey(),
            )],
            &[&deployer],
        )
        .ok();

    let launcher = w.wallet_with_sol(10 * SOL);
    let (config, tx) = partner_config(&mut w, &launcher, launcher.pubkey(), 1, 0);
    tx.ok();
    let mint = Keypair::new();
    let cargs = CreateArgs {
        split: Split {
            buyback_bps: 0,
            holders_bps: 0,
            beneficiary_bps: 10_000,
        },
        bounty_bps: 50,
        max_buyback: SOL,
        buyback_interval: 60,
        vest_secs: 0,
        fund: SOL / 2,
    };
    let sargs = CreateScriptArgs {
        split: Split {
            buyback_bps: 0,
            holders_bps: 0,
            beneficiary_bps: 3_000,
        },
        pot_bps: 7_000,
        program: se::ID,
        interval: 60,
        state_len: 64,
        init_data: se::state_bytes(se::MODE_NOTHING, 0),
    };
    w.env
        .send_paid_by(
            &[
                companion::create(launcher.pubkey(), launcher.pubkey(), mint.pubkey(), cargs),
                companion::create_script(launcher.pubkey(), mint.pubkey(), sargs, vec![]),
            ],
            &launcher,
            &[&mint],
        )
        .ok();
    let creator = companion::creator_address(&mint.pubkey());
    let a = World::launch_args("SCRP", FEE, VQ, LaunchRules::NONE);
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
    w.env
        .send_paid_by(
            &[companion::launch(
                launcher.pubkey(),
                mint.pubkey(),
                &inner,
                a,
            )],
            &launcher,
            &[&mint],
        )
        .expect_code(code(LaunchError::PartnerOnCompanion));
    // Before the fix the keeper's only flow, `distribute_fees` then the companion's
    // `claim_fees_script`, left the script author's delta (and share) at 0 on every claim.
    let _: Option<Script> = w.env.try_read(&companion::script_address(&mint.pubkey()));
}

// ------------------------------------------------------------------------------------------------
// SOUND: attacks refused
// ------------------------------------------------------------------------------------------------

/// A launch paying an author and a partner: remaining accounts in the wrong order, the partner's
/// holding of the wrong mint, and another config's partner account are all refused, on every path.
#[test]
fn sound_ordering_wrong_mint_and_foreign_partner_accounts_are_refused() {
    let mut w = World::new();
    let author = w.wallet_with_sol(5 * SOL);
    let creator = w.wallet_with_sol(20 * SOL);
    let partner = w.wallet_with_sol(SOL).pubkey();
    let (config, tx) = partner_config(&mut w, &author, partner, 2_000, 3_000);
    tx.ok();
    let (mint, tx) = w.create_launch_from_config(&creator, "TRI", VQ, &config);
    tx.ok();
    let fees = accrue(&mut w, &mint);

    // Partner accounts first, author accounts second.
    let mut swapped = launch::claim_creator_fees(creator.pubkey(), mint, w.sol);
    swapped
        .accounts
        .extend(launch::partner_accounts(w.sol, config, partner));
    swapped
        .accounts
        .push(AccountMeta::new_readonly(config, false));
    swapped.accounts.push(AccountMeta::new(
        token::holding_address(&w.sol, &author.pubkey()),
        false,
    ));
    w.env
        .send_paid_by(&[swapped], &creator, &[])
        .expect_code(code(LaunchError::AuthorAccountsMissing));

    // The partner's holding of the launched token instead of the quote.
    w.holdings(&creator, mint, &[partner]);
    let mut wrong_mint =
        launch::claim_creator_fees_shared(creator.pubkey(), mint, w.sol, config, author.pubkey());
    wrong_mint.accounts.push(AccountMeta::new(
        launch::partner_fee_address(&config),
        false,
    ));
    wrong_mint.accounts.push(AccountMeta::new(
        token::holding_address(&mint, &partner),
        false,
    ));
    w.env
        .send_paid_by(&[wrong_mint], &creator, &[])
        .expect_code(code(LaunchError::WrongHolding));
    let mut d = launch::distribute_fees(
        mint,
        w.sol,
        config,
        creator.pubkey(),
        partner,
        Some(author.pubkey()),
    );
    d.accounts[5].pubkey = token::holding_address(&mint, &partner);
    let cranker = w.wallet_with_sol(SOL);
    w.env
        .send_paid_by(&[d], &cranker, &[])
        .expect_code(code(LaunchError::WrongHolding));

    // The author's claim with another partner config's account (same partner, larger share).
    let (other, tx) = partner_config(&mut w, &author, partner, 5_000, 0);
    tx.ok();
    let mut foreign =
        launch::claim_author_fees(author.pubkey(), mint, w.sol, config, creator.pubkey());
    foreign
        .accounts
        .extend(launch::partner_accounts(w.sol, other, partner));
    w.env
        .send_paid_by(&[foreign], &author, &[])
        .expect_code(code(LaunchError::PartnerAccountsMissing));

    // distribute_fees with the author's accounts naming another config the author made (the
    // creator would pay a stranger's holding otherwise) or a stranger's holding.
    let listed = listed_config(&mut w, &author, 5_000);
    let mut d = launch::distribute_fees(
        mint,
        w.sol,
        config,
        creator.pubkey(),
        partner,
        Some(author.pubkey()),
    );
    let n = d.accounts.len();
    d.accounts[n - 2].pubkey = listed;
    w.env
        .send_paid_by(&[d], &cranker, &[])
        .expect_code(code(LaunchError::AuthorAccountsMissing));
    let mut d = launch::distribute_fees(
        mint,
        w.sol,
        config,
        creator.pubkey(),
        partner,
        Some(author.pubkey()),
    );
    d.accounts[n - 1].pubkey = token::holding_address(&w.sol, &cranker.pubkey());
    w.env
        .send_paid_by(&[d], &cranker, &[])
        .expect_code(code(LaunchError::WrongHolding));
    // distribute_fees with the partner account read-only: Anchor's `mut` refuses it.
    let mut d = launch::distribute_fees(
        mint,
        w.sol,
        config,
        creator.pubkey(),
        partner,
        Some(author.pubkey()),
    );
    d.accounts[1].is_writable = false;
    assert!(w.env.send_paid_by(&[d], &cranker, &[]).result.is_err());

    // Nothing moved; the honest distribution then pays exactly the split.
    assert_eq!(w.env.holding(&w.sol, &launch::launch_address(&mint)), fees);
    let before = (
        sol_of(&w, &creator.pubkey()),
        sol_of(&w, &author.pubkey()),
        sol_of(&w, &partner),
    );
    w.env.warp(1);
    w.env
        .send_paid_by(
            &[launch::distribute_fees(
                mint,
                w.sol,
                config,
                creator.pubkey(),
                partner,
                Some(author.pubkey()),
            )],
            &cranker,
            &[],
        )
        .ok();
    let (c, a, p) = fee_split(fees, 3_000, 2_000).unwrap();
    assert_eq!(
        (
            sol_of(&w, &creator.pubkey()),
            sol_of(&w, &author.pubkey()),
            sol_of(&w, &partner)
        ),
        (before.0 + c, before.1 + a, before.2 + p)
    );
    // The other config's partner account was not touched.
    let pf: PartnerFee = w.env.read(&launch::partner_fee_address(&other));
    assert_eq!(pf.paid_total, 0);
}

/// distribute_fees with the partner as the creator: the two writable holdings are the same account
/// (Anchor's duplicate check covers only serialized types), and the whole claim lands there.
#[test]
fn sound_distribute_when_partner_is_creator() {
    let mut w = World::new();
    let author = w.wallet_with_sol(5 * SOL);
    let creator = w.wallet_with_sol(20 * SOL);
    let (config, tx) = partner_config(&mut w, &author, creator.pubkey(), 3_000, 0);
    tx.ok();
    let (mint, tx) = w.create_launch_from_config(&creator, "SLF", VQ, &config);
    tx.ok();
    let fees = accrue(&mut w, &mint);
    let c0 = sol_of(&w, &creator.pubkey());
    let cranker = w.wallet_with_sol(SOL);
    w.env
        .send_paid_by(
            &[launch::distribute_fees(
                mint,
                w.sol,
                config,
                creator.pubkey(),
                creator.pubkey(),
                None,
            )],
            &cranker,
            &[],
        )
        .ok();
    assert_eq!(sol_of(&w, &creator.pubkey()), c0 + fees);
}

/// Launches without a partner (as every launch made before): extra partner accounts appended to
/// the creator's claim are ignored, the creator is paid everything as before, no partner event,
/// and no partner's total moves. A listed launch claims as before too.
#[test]
fn sound_launches_without_a_partner_claim_as_before() {
    let mut w = World::new();
    let author = w.wallet_with_sol(5 * SOL);
    let creator = w.wallet_with_sol(20 * SOL);
    let partner = w.wallet_with_sol(SOL).pubkey();
    let (pconfig, tx) = partner_config(&mut w, &author, partner, 9_000, 0);
    tx.ok();
    let (plain, tx) = w.create_config(&author, args("Plain"));
    tx.ok();
    let (mint, tx) = w.create_launch_from_config(&creator, "PLN", VQ, &plain);
    tx.ok();
    assert_eq!(w.launch(&mint).reserved, [0u8; 22]);
    assert_eq!(w.launch_config(&plain).reserved, [0u8; 30]);
    let fees = accrue(&mut w, &mint);
    let c0 = sol_of(&w, &creator.pubkey());
    let mut ix = launch::claim_creator_fees(creator.pubkey(), mint, w.sol);
    ix.accounts
        .extend(launch::partner_accounts(w.sol, pconfig, partner));
    let tx = w.env.send_paid_by(&[ix], &creator, &[]);
    tx.ok();
    assert_eq!(sol_of(&w, &creator.pubkey()), c0 + fees);
    assert!(tx.events::<PartnerFeesPaid>().is_empty());
    assert_eq!(tx.event::<CreatorFeesClaimed>().amount, fees);
    let pf: PartnerFee = w.env.read(&launch::partner_fee_address(&pconfig));
    assert_eq!(pf.paid_total, 0);
    // distribute_fees on it: refused.
    let cranker = w.wallet_with_sol(SOL);
    accrue(&mut w, &mint);
    let mut d = launch::distribute_fees(mint, w.sol, pconfig, creator.pubkey(), partner, None);
    assert!(w
        .env
        .send_paid_by(&[d.clone()], &cranker, &[])
        .result
        .is_err());
    d.accounts[1].pubkey = launch::partner_fee_address(&plain);
    assert!(w.env.send_paid_by(&[d], &cranker, &[]).result.is_err());

    // A listed launch: author 5,000 (the most), the creator still gets at least half.
    let listed = listed_config(&mut w, &author, 5_000);
    let (lmint, tx) = w.create_launch_from_config(&creator, "LST", VQ, &listed);
    tx.ok();
    let fees = accrue(&mut w, &lmint);
    let (c0, a0) = (sol_of(&w, &creator.pubkey()), sol_of(&w, &author.pubkey()));
    let tx = w.env.send_paid_by(
        &[launch::claim_creator_fees_shared(
            creator.pubkey(),
            lmint,
            w.sol,
            listed,
            author.pubkey(),
        )],
        &creator,
        &[],
    );
    tx.ok();
    let half = author_part(fees, 5_000);
    assert_eq!(sol_of(&w, &author.pubkey()), a0 + half);
    assert_eq!(sol_of(&w, &creator.pubkey()), c0 + fees - half);
    assert!(tx.events::<PartnerFeesPaid>().is_empty());
}

/// The split never pays more than the claim and the creator keeps the rounding, for every share
/// pair a config can hold (author 0..=5,000, partner 1..=10,000 - author), and the author share a
/// launch snapshots is the config's or 0, never more.
#[test]
fn sound_split_bounds_for_every_valid_share_pair() {
    for amount in [1u64, 2, 3, 9_999, 10_001, 1 << 40, u64::MAX] {
        for a in (0..=5_000u16).step_by(250) {
            for p in [1u16, 2, 5_000 - a.min(5_000), 10_000 - a] {
                if p == 0 {
                    continue;
                }
                let (c, au, pa) = fee_split(amount, a, p).unwrap();
                assert_eq!(c as u128 + au as u128 + pa as u128, amount as u128);
                assert!(au <= amount / 2 + 1 && pa <= amount);
            }
        }
    }
}

// ------------------------------------------------------------------------------------------------
// INFO
// ------------------------------------------------------------------------------------------------

/// A config naming as partner the launch PDA of a mint its author will launch: the partner's
/// holding IS the launch's quote holding, the token program refuses a transfer to itself
/// (`SameAccount`), so no claim of that launch ever succeeds (self-inflicted: only the author's own
/// launch, whose mint keypair it holds, can be targeted; other launches from the config pay their
/// partner part into that stuck holding).
#[test]
fn fixed_partner_equal_to_launch_pda_is_paid_nothing_and_claims_go_through() {
    // Was INFO: a partner naming the launch's own address bricked that launch's claims (the token
    // program refuses a transfer to itself). Now the partner's part is 0 there and the creator is paid.
    let mut w = World::new();
    let author = w.wallet_with_sol(20 * SOL);
    let mint = Keypair::new();
    let pda = launch::launch_address(&mint.pubkey());
    let (config, tx) = partner_config(&mut w, &author, pda, 1, 0);
    tx.ok();
    let ix = w.create_launch_from_config_ix(&author.pubkey(), &mint.pubkey(), "BRK", VQ, &config);
    w.env.send_paid_by(&[ix], &author, &[&mint]).ok();
    let mint = mint.pubkey();
    let fees = accrue(&mut w, &mint);
    let before = w.env.holding(&w.sol, &author.pubkey());
    let claim =
        launch::claim_creator_fees_with(author.pubkey(), mint, w.sol, None, Some((config, pda)));
    w.env.send_paid_by(&[claim], &author, &[]).ok();
    assert_eq!(w.env.holding(&w.sol, &author.pubkey()), before + fees);
    assert_eq!(w.env.holding(&w.sol, &pda), 0);
    accrue(&mut w, &mint);
    let cranker = w.wallet_with_sol(SOL);
    let d = launch::distribute_fees(mint, w.sol, config, author.pubkey(), pda, None);
    w.env.send_paid_by(&[d], &cranker, &[]).ok();
    assert_eq!(w.env.holding(&w.sol, &pda), 0);
}
