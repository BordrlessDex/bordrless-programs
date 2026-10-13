//! `loss_cover_hook` on a real launch from a plain `LaunchConfig` (not through the companion: that is
//! `loss_cover.rs`). Prepared for the mint before it exists (the mint's keypair signs), launched,
//! traded: every rule of `docs/companions.md` "Loss cover" (the hook): a buy's basis at the swap's
//! curve price against the DEX's write order, the blend, the reference and its cap on a self-pump,
//! a plain transfer in at the spot, a send and a burn cutting the cover pro rata, an emptied holding
//! cleared, the exempt owners, graduation, and `record_payout` taking only the companion program's
//! payout authority.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::Discriminator;
use bordrless_hook::{
    hook_accounts_address, AccountSource, ExtraAccount, HookAccountList, Seed, TokenHookArgs,
};
use bordrless_launch::constants::STATUS_GRADUATED;
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::timelock::send_as;
use bordrless_strategy::PoolReserves;
use bordrless_swap::client as swap;
use bordrless_swap::state::Pool;
use bordrless_token::client as token;
use loss_cover_hook::{
    capped, client as hook, spot_of, step_toward, trade_price, LossCoverError, LossCoverState,
    Prepared, Record, FLAGS, FLAG_SOLD, HOOK_AUTHORITY, PAYOUT_AUTHORITY, REFERENCE_SECS,
};
use solana_keypair::Keypair;
use solana_signer::Signer;

const LP: u16 = bordrless_core::policy::LP_FEE_BPS;

#[track_caller]
fn refused(tx: &Tx, e: LossCoverError) {
    tx.expect_code(u32::from(e));
    let name = format!("{e:?}");
    assert!(
        tx.logs()
            .iter()
            .any(|l| l.contains(&format!("Error Code: {name}"))),
        "expected {name}\n{}",
        tx.logs().join("\n")
    );
}

fn state(w: &World, mint: &Pubkey) -> LossCoverState {
    w.env.read(&hook::state_address(mint))
}

fn record(w: &World, mint: &Pubkey, owner: &Pubkey) -> Record {
    Record::decode(&w.env.hook_data(mint, owner))
}

fn reserves(p: &Pool) -> PoolReserves {
    PoolReserves {
        base_mint: p.base_mint,
        quote_mint: p.quote_mint,
        lp_fee_bps: p.lp_fee_bps,
        base_reserve: p.base_reserve,
        quote_reserve: p.quote_reserve,
        virtual_base: p.virtual_base,
        virtual_quote: p.virtual_quote,
    }
}

fn spot(w: &World, mint: &Pubkey) -> u64 {
    spot_of(&reserves(&w.launch_pool(mint))).unwrap()
}

/// A config naming the hook (no kit rules, creator fee 1%).
fn cover_config(w: &mut World, creator: &Keypair) -> Pubkey {
    let (config, tx) = w.create_config(
        creator,
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: 100,
            custom_hook: Some(loss_cover_hook::ID),
            custom_hook_flags: FLAGS,
            label: "Cover".to_string(),
        },
    );
    tx.ok();
    config
}

/// Prepares the hook for a fresh mint (its keypair signing), makes a config naming it and launches
/// from it, then waits out the sniper window. Answers the creator and the mint.
fn launch_cover(w: &mut World) -> (Keypair, Pubkey) {
    let creator = w.wallet_with_sol(30 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    w.env
        .send_paid_by(
            &[hook::prepare(creator.pubkey(), mint)],
            &creator,
            &[&mint_kp],
        )
        .ok();
    let config = cover_config(w, &creator);
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint, "COVER", VQ, &config);
    let tx = w.env.send_paid_by(&[ix], &creator, &[&mint_kp]);
    tx.ok();
    assert_eq!(w.launch(&mint).custom_hook, Some(loss_cover_hook::ID));
    assert_eq!(state(w, &mint).pool, w.launch(&mint).pool);
    w.env.warp(31);
    (creator, mint)
}

fn trader(w: &mut World, mint: &Pubkey, sol: u64) -> Keypair {
    let t = w.wallet_with_sol(sol);
    w.holdings(&t, *mint, &[t.pubkey()]);
    t
}

/// A buy by `t`, answering what it got.
fn buy(w: &mut World, mint: &Pubkey, t: &Keypair, lamports: u64) -> u64 {
    let pre = w.env.holding(mint, &t.pubkey());
    w.buy(t, mint, lamports).ok();
    w.env.holding(mint, &t.pubkey()) - pre
}

/// `record_payout` sent as the companion's payout authority (a PDA the suite can't sign for:
/// sigverify off, its signature left empty).
fn record_payout(w: &mut World, mint: &Pubkey, owner: &Pubkey, lamports: u64, tag: u32) -> Tx {
    send_as(
        &mut w.env,
        &[hook::record_payout(*mint, *owner, lamports, tag)],
        &[],
        &[PAYOUT_AUTHORITY],
    )
}

#[test]
fn prepare_writes_the_state_and_the_registry() {
    let mut w = World::new();
    let payer = w.wallet_with_sol(5 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let tx = w
        .env
        .send_paid_by(&[hook::prepare(payer.pubkey(), mint)], &payer, &[&mint_kp]);
    tx.ok();
    let now = w.env.now;
    let launch = bordrless_launch::client::launch_address(&mint);
    let pool = w.launch_pool_key(&mint);
    let creator = bordrless_companion::client::creator_address(&mint);
    let s = state(&w, &mint);
    assert_eq!(
        (s.version, s.mint, s.launch, s.pool, s.creator),
        (loss_cover_hook::VERSION, mint, launch, pool, creator)
    );
    assert_eq!(
        (s.reference, s.reference_at, s.paid_total, s.records),
        (0, 0, 0, 0)
    );
    assert_eq!((s.prepared_by, s.prepared_at), (payer.pubkey(), now));
    let account = w.env.account(&hook::state_address(&mint)).unwrap();
    assert_eq!(
        (account.owner, account.data.len()),
        (loss_cover_hook::ID, 274)
    );
    let ev: Prepared = tx.event();
    assert_eq!(
        (
            ev.mint,
            ev.state,
            ev.launch,
            ev.pool,
            ev.creator,
            ev.lp_fee_bps,
            ev.prepared_by
        ),
        (
            mint,
            hook::state_address(&mint),
            launch,
            pool,
            creator,
            LP,
            payer.pubkey()
        )
    );
    assert_eq!(hook::pool_address(&mint, LP), pool);
    // The registry: the state (writable), the launch and the pool (read-only), the pool a PDA of
    // the DEX with its literal seeds; it resolves to the client's list.
    let registry = w
        .env
        .account(&hook_accounts_address(&loss_cover_hook::ID, &mint).0)
        .unwrap();
    assert_eq!(registry.owner, loss_cover_hook::ID);
    let list = HookAccountList::decode(&registry.data).unwrap();
    assert_eq!(list.accounts.len(), 3);
    assert_eq!(
        list.accounts[0],
        ExtraAccount {
            writable: true,
            source: AccountSource::Pda {
                program: loss_cover_hook::ID,
                seeds: vec![Seed::Literal(b"state".to_vec()), Seed::Account(1)],
            },
        }
    );
    assert_eq!(
        list.accounts[1],
        ExtraAccount {
            writable: false,
            source: AccountSource::Pda {
                program: bordrless_launch::ID,
                seeds: vec![Seed::Literal(b"launch".to_vec()), Seed::Account(1)],
            },
        }
    );
    assert_eq!(
        list.accounts[2],
        ExtraAccount {
            writable: false,
            source: AccountSource::Pda {
                program: bordrless_swap::ID,
                seeds: vec![
                    Seed::Literal(b"pool".to_vec()),
                    Seed::Account(1),
                    Seed::Literal(w.sol.to_bytes().to_vec()),
                    Seed::Literal(LP.to_le_bytes().to_vec()),
                    Seed::Literal(bordrless_launch::ID.to_bytes().to_vec()),
                ],
            },
        }
    );
    let resolved = w.env.token_hook_extras(
        &loss_cover_hook::ID,
        &mint,
        &Pubkey::new_unique(),
        &Pubkey::new_unique(),
        &Pubkey::new_unique(),
        &Pubkey::new_unique(),
        &Pubkey::new_unique(),
    );
    assert_eq!(resolved, hook::extras(&mint, LP));
    // The constants agree with the programs they name.
    assert_eq!(
        PAYOUT_AUTHORITY,
        bordrless_companion::client::payout_authority_address()
    );
    assert_eq!(
        loss_cover_hook::instruction::RecordPayout::DISCRIMINATOR,
        &bordrless_companion::constants::RECORD_PAYOUT_DISCRIMINATOR[..]
    );
    assert_eq!(
        loss_cover_hook::PRICE_SCALE,
        bordrless_companion::constants::PRICE_SCALE
    );
    assert_eq!(FLAGS, 145);
    // Once per mint; only the mint's keypair; only the launchpad's config.
    let tx = w
        .env
        .send_paid_by(&[hook::prepare(payer.pubkey(), mint)], &payer, &[&mint_kp]);
    tx.expect_fail();
    let other = Keypair::new();
    let mut ix = hook::prepare(payer.pubkey(), other.pubkey());
    ix.accounts[1].is_signer = false;
    w.env.send_paid_by(&[ix], &payer, &[]).expect_code(3010);
    // Another program's config in the launchpad's place: not the launchpad's account (Anchor's
    // owner check, before the address).
    let mut ix = hook::prepare(payer.pubkey(), other.pubkey());
    ix.accounts[4].pubkey = bordrless_swap::client::config_address();
    w.env
        .send_paid_by(&[ix], &payer, &[&other])
        .expect_code(3007);
    assert!(w
        .env
        .account(&hook::state_address(&other.pubkey()))
        .is_none());
}

/// A buy's basis is the swap's effective curve price, fees aside, read from the pool after the
/// DEX wrote its reserves and before it delivered: `Q' * 10^12 / (B' + a)`. Pinned against the
/// DEX's write order: the pre-trade reserves give another number. A second buy blends by weight.
#[test]
fn a_buy_sets_the_basis_at_the_swaps_curve_price() {
    let mut w = World::new();
    let (_creator, mint) = launch_cover(&mut w);
    let a = trader(&mut w, &mint, 20 * SOL);
    let before = w.launch_pool(&mint);
    let now = w.env.now;
    // The reference's quote of the buy, on the pool as it stands before it.
    let q = w.launch_quote(&mint, &a.pubkey(), true, SOL);
    let got = buy(&mut w, &mint, &a, SOL);
    assert_eq!(Some(got), q.delivered);
    let after = w.launch_pool(&mint);
    let expected = trade_price(&reserves(&after), got).unwrap();
    let pre_trade = trade_price(&reserves(&before), got).unwrap();
    assert_ne!(expected, pre_trade, "the order can be told");
    let r = record(&w, &mint, &a.pubkey());
    assert_eq!(
        r,
        Record {
            version: 1,
            flags: 0,
            entry: expected,
            amount: got,
            paid: 0,
            updated_slot: w.env.slot,
            paid_at: 0,
            basis_at: now,
            round: 0,
        }
    );
    // The effective price: what went into the curve over what came out, within rounding.
    let effective = u128::try_from(q.net_in).unwrap() * loss_cover_hook::PRICE_SCALE
        / u128::from(q.amount_out.unwrap());
    let diff = (u128::from(expected)).abs_diff(effective);
    assert!(
        diff * 1_000 <= effective,
        "entry {expected} vs the swap's effective price {effective}"
    );
    // A second buy of a different size: the average by weight, and `basis_at` moves.
    w.env.warp(5);
    let got2 = buy(&mut w, &mint, &a, 3 * SOL);
    let p2 = trade_price(&reserves(&w.launch_pool(&mint)), got2).unwrap();
    let blended = (u128::from(expected) * u128::from(got) + u128::from(p2) * u128::from(got2))
        / (u128::from(got) + u128::from(got2));
    let r2 = record(&w, &mint, &a.pubkey());
    assert_eq!(
        (r2.entry, r2.amount, r2.basis_at),
        (blended as u64, got + got2, w.env.now)
    );
    assert!(r2.entry > expected, "bought higher: the basis rose");
    // The reference: set at the first buy (the spot the callback read), not moved within 10
    // minutes, then stepped toward the spot by at most 5%.
    let s = state(&w, &mint);
    assert!(s.reference > 0);
    assert_eq!(s.reference_at, now);
    let reference = s.reference;
    w.env.warp(REFERENCE_SECS - 10);
    buy(&mut w, &mint, &a, SOL / 10);
    assert_eq!(state(&w, &mint).reference, reference);
    w.env.warp(10);
    let b = trader(&mut w, &mint, 60 * SOL);
    buy(&mut w, &mint, &b, 20 * SOL);
    let s = state(&w, &mint);
    assert_eq!(s.reference, step_toward(reference, spot(&w, &mint)));
    assert_eq!(s.reference, reference * 10_500 / 10_000, "a 5% step");
    assert_eq!(s.reference_at, w.env.now);
}

/// A buyer's own spike never sets a basis more than 20% above the reference: a whale's buy far
/// above it takes the capped entry, and so does what arrives by transfer at the spiked spot.
#[test]
fn the_reference_caps_a_self_pump() {
    let mut w = World::new();
    let (_creator, mint) = launch_cover(&mut w);
    let a = trader(&mut w, &mint, 5 * SOL);
    buy(&mut w, &mint, &a, SOL / 10);
    let reference = state(&w, &mint).reference;
    let whale = trader(&mut w, &mint, 100 * SOL);
    let got = buy(&mut w, &mint, &whale, 40 * SOL);
    let raw = trade_price(&reserves(&w.launch_pool(&mint)), got).unwrap();
    let cap = capped(raw, reference);
    assert!(raw > cap, "the pump's price {raw} is above the cap {cap}");
    assert_eq!(cap, reference * 12_000 / 10_000);
    assert_eq!(record(&w, &mint, &whale.pubkey()).entry, cap);
    // Tokens handed to a fresh wallet at the spiked spot: the same cap.
    let c = trader(&mut w, &mint, SOL);
    w.send_tokens(&whale, mint, &c.pubkey(), got / 2).ok();
    let now_spot = spot(&w, &mint);
    assert!(now_spot > cap);
    assert_eq!(record(&w, &mint, &c.pubkey()).entry, cap);
    // The reference itself only steps 5% a time: unmoved within its interval.
    assert_eq!(state(&w, &mint).reference, reference);
}

/// A plain transfer in blends at the spot: the receiver's basis is the price now, the sender's
/// record is cut to what it keeps with its cover pro rata, the entry unchanged and the sold flag
/// set; a holding emptied by a send, a sell or a burn is cleared and can be closed. Both sides of
/// one transfer are written from their own records.
#[test]
fn transfers_sells_and_burns_cut_the_cover_pro_rata() {
    let mut w = World::new();
    w.env.without_sigverify();
    let (_creator, mint) = launch_cover(&mut w);
    let a = trader(&mut w, &mint, 10 * SOL);
    let b = trader(&mut w, &mint, 10 * SOL);
    let c = trader(&mut w, &mint, 10 * SOL);
    let got_a = buy(&mut w, &mint, &a, SOL);
    let got_b = buy(&mut w, &mint, &b, SOL);
    // A was paid 1,000 lamports of cover.
    record_payout(&mut w, &mint, &a.pubkey(), 1_000, 7).ok();
    let ra = record(&w, &mint, &a.pubkey());
    assert_eq!((ra.paid, ra.round), (1_000, 7));
    // A sends a third to C (which held nothing): A keeps two thirds of its cover, C's basis is the
    // spot now, no cover, its own `basis_at`.
    w.env.warp(60);
    let third = got_a / 3;
    let spot_now = spot(&w, &mint);
    let tx = w.send_tokens(&a, mint, &c.pubkey(), third);
    tx.ok();
    println!("send through the hook: CU {}", tx.cu());
    let ra2 = record(&w, &mint, &a.pubkey());
    assert_eq!(
        (
            ra2.entry,
            ra2.amount,
            ra2.paid,
            ra2.flags,
            ra2.round,
            ra2.basis_at
        ),
        (
            ra.entry,
            got_a - third,
            1_000 * (got_a - third) / got_a,
            FLAG_SOLD,
            7,
            ra.basis_at
        )
    );
    assert_eq!(ra2.updated_slot, w.env.slot);
    let rc = record(&w, &mint, &c.pubkey());
    assert_eq!(
        (rc.entry, rc.amount, rc.paid, rc.flags, rc.basis_at),
        (spot_now, third, 0, 0, w.env.now)
    );
    // A sends more to B, which holds a record of its own: B's basis is blended with the spot, A's
    // is cut again; nothing duplicated, nothing wiped.
    let rb = record(&w, &mint, &b.pubkey());
    w.send_tokens(&a, mint, &b.pubkey(), third).ok();
    let rb2 = record(&w, &mint, &b.pubkey());
    let blended = (u128::from(rb.entry) * u128::from(got_b)
        + u128::from(spot_now) * u128::from(third))
        / u128::from(got_b + third);
    assert_eq!(
        (rb2.entry, rb2.amount, rb2.paid, rb2.flags),
        (blended as u64, got_b + third, 0, 0)
    );
    let ra3 = record(&w, &mint, &a.pubkey());
    assert_eq!(ra3.amount, got_a - 2 * third);
    assert_eq!(ra3.paid, ra2.paid * (got_a - 2 * third) / (got_a - third));
    // A sells the rest: cleared, and the holding closes.
    let left = w.env.holding(&mint, &a.pubkey());
    w.sell(&a, &mint, left).ok();
    assert_eq!(w.env.hook_data(&mint, &a.pubkey()), [0u8; 64]);
    let close = token::close_holding(
        a.pubkey(),
        mint,
        token::holding_address(&mint, &a.pubkey()),
        a.pubkey(),
    );
    w.env.send_paid_by(&[close], &a, &[]).ok();
    // A partial sell keeps its cover pro rata; a burn cuts like a send; burning the rest clears.
    record_payout(&mut w, &mint, &b.pubkey(), 2_000, 8).ok();
    let held = w.env.holding(&mint, &b.pubkey());
    w.sell(&b, &mint, held / 4).ok();
    let rb3 = record(&w, &mint, &b.pubkey());
    assert_eq!(
        (rb3.amount, rb3.paid, rb3.flags),
        (held - held / 4, 2_000 * (held - held / 4) / held, FLAG_SOLD)
    );
    assert_eq!(rb3.entry, rb2.entry);
    let held = rb3.amount;
    let burn = token::burn(
        b.pubkey(),
        token::holding_address(&mint, &b.pubkey()),
        mint,
        Some(loss_cover_hook::ID),
        hook::extras(&mint, LP),
        held / 2,
    );
    w.env.send_paid_by(&[burn], &b, &[]).ok();
    let rb4 = record(&w, &mint, &b.pubkey());
    assert_eq!(
        (rb4.amount, rb4.paid),
        (held - held / 2, rb3.paid * (held - held / 2) / held)
    );
    let burn = token::burn(
        b.pubkey(),
        token::holding_address(&mint, &b.pubkey()),
        mint,
        Some(loss_cover_hook::ID),
        hook::extras(&mint, LP),
        rb4.amount,
    );
    w.env.send_paid_by(&[burn], &b, &[]).ok();
    assert_eq!(w.env.hook_data(&mint, &b.pubkey()), [0u8; 64]);
    // A send to oneself is refused by the token program before any hook runs.
    let state_before = w.env.account(&hook::state_address(&mint)).unwrap().data;
    let rc_before = w.env.hook_data(&mint, &c.pubkey());
    let tx = w.send_tokens(&c, mint, &c.pubkey(), 1);
    tx.expect_fail();
    assert_eq!(
        w.env.account(&hook::state_address(&mint)).unwrap().data,
        state_before
    );
    assert_eq!(w.env.hook_data(&mint, &c.pubkey()), rc_before);
    // C buys after its transfer in: the basis blends the spot it arrived at with the buy's price,
    // and `basis_at` restarts.
    w.env.warp(60);
    let got_c = buy(&mut w, &mint, &c, SOL / 2);
    let p = trade_price(&reserves(&w.launch_pool(&mint)), got_c).unwrap();
    let rc2 = record(&w, &mint, &c.pubkey());
    let blended = (u128::from(rc.entry) * u128::from(third) + u128::from(p) * u128::from(got_c))
        / u128::from(third + got_c);
    assert_eq!(
        (rc2.entry, rc2.amount, rc2.basis_at, rc2.flags),
        (blended as u64, third + got_c, w.env.now, 0)
    );
}

/// The launch, its pool, the companion's creator address and any address off the curve never get
/// a record; graduation through the hook refuses nothing.
#[test]
fn exempt_owners_are_never_written() {
    let mut w = World::new();
    let (creator, mint) = launch_cover(&mut w);
    let a = trader(&mut w, &mint, 20 * SOL);
    buy(&mut w, &mint, &a, 2 * SOL);
    let l = w.launch(&mint);
    let launch_key = bordrless_launch::client::launch_address(&mint);
    assert_eq!(w.env.hook_data(&mint, &l.pool), [0u8; 64]);
    assert_eq!(w.env.hook_data(&mint, &launch_key), [0u8; 64]);
    let companion_creator = bordrless_companion::client::creator_address(&mint);
    let escrow = Pubkey::find_program_address(&[b"escrow"], &Pubkey::new_unique()).0;
    w.holdings(&a, mint, &[companion_creator, escrow]);
    w.send_tokens(&a, mint, &companion_creator, 1_000_000).ok();
    w.send_tokens(&a, mint, &escrow, 1_000_000).ok();
    assert_eq!(w.env.hook_data(&mint, &companion_creator), [0u8; 64]);
    assert_eq!(w.env.hook_data(&mint, &escrow), [0u8; 64]);
    // The sender's record was cut all the same.
    let ra = record(&w, &mint, &a.pubkey());
    assert_eq!(ra.amount, w.env.holding(&mint, &a.pubkey()));
    assert_eq!(ra.flags, FLAG_SOLD);
    // The launch's own creator here is a wallet: an ordinary holder.
    w.holdings(&creator, mint, &[creator.pubkey()]);
    buy(&mut w, &mint, &creator, SOL);
    assert!(record(&w, &mint, &creator.pubkey()).is_set());
    // Graduation: the reserve's top-up into the pool and its burn run through the hook.
    let (wallets, tx) = w.graduate_launch(&mint);
    tx.ok();
    assert_eq!(w.launch(&mint).status, STATUS_GRADUATED);
    assert_eq!(w.env.hook_data(&mint, &l.pool), [0u8; 64]);
    assert_eq!(w.env.hook_data(&mint, &launch_key), [0u8; 64]);
    for wallet in &wallets {
        let r = record(&w, &mint, &wallet.pubkey());
        assert_eq!(r.amount, w.env.holding(&mint, &wallet.pubkey()));
        assert!(r.entry > 0);
    }
    // After graduation the pool goes on: a buy and a sell still write.
    let b = trader(&mut w, &mint, 20 * SOL);
    let got = buy(&mut w, &mint, &b, SOL);
    assert_eq!(record(&w, &mint, &b.pubkey()).amount, got);
    w.sell(&b, &mint, got / 2).ok();
    assert_eq!(record(&w, &mint, &b.pubkey()).amount, got - got / 2);
}

/// `record_payout` takes only the companion program's payout authority, a holding of this mint
/// with a record whose owner may hold one, and the hook's own accounts; it adds the lamports,
/// stamps the round and, when something was paid, the time.
#[test]
fn record_payout_takes_only_the_companions_authority() {
    let mut w = World::new();
    w.env.without_sigverify();
    let (_creator, mint) = launch_cover(&mut w);
    let a = trader(&mut w, &mint, 10 * SOL);
    let got = buy(&mut w, &mint, &a, SOL);
    let ra = record(&w, &mint, &a.pubkey());
    // A stranger signing in the authority's place.
    let stranger = w.env.funded(SOL);
    let mut ix = hook::record_payout(mint, a.pubkey(), 1, 1);
    ix.accounts[0] = AccountMeta::new_readonly(stranger.pubkey(), true);
    refused(
        &w.env.send_paid_by(&[ix], &stranger, &[]),
        LossCoverError::NotTheCompanion,
    );
    // The authority unsigned.
    let mut ix = hook::record_payout(mint, a.pubkey(), 1, 1);
    ix.accounts[0].is_signer = false;
    w.env.send_paid_by(&[ix], &stranger, &[]).expect_fail();
    // A holding with no record.
    let empty = trader(&mut w, &mint, SOL);
    refused(
        &record_payout(&mut w, &mint, &empty.pubkey(), 0, 1),
        LossCoverError::NoRecord,
    );
    // A holding of another mint (A's bridged SOL).
    let mut ix = hook::record_payout(mint, a.pubkey(), 1, 1);
    ix.accounts[2].pubkey = token::holding_address(&w.sol, &a.pubkey());
    refused(
        &send_as(&mut w.env, &[ix], &[], &[PAYOUT_AUTHORITY]),
        LossCoverError::WrongMint,
    );
    // The pool's vault: its owner holds no record.
    let pool = w.launch(&mint).pool;
    refused(
        &record_payout(&mut w, &mint, &pool, 0, 1),
        LossCoverError::NotEligible,
    );
    // Another account in the hook authority's place.
    let mut ix = hook::record_payout(mint, a.pubkey(), 1, 1);
    ix.accounts[4].pubkey = Pubkey::new_unique();
    refused(
        &send_as(&mut w.env, &[ix], &[], &[PAYOUT_AUTHORITY]),
        LossCoverError::WrongAccount,
    );
    assert_eq!(record(&w, &mint, &a.pubkey()), ra, "nothing was written");
    // A payment: the lamports added, the round and the time stamped, the state counting.
    let now = w.env.now;
    let tx = record_payout(&mut w, &mint, &a.pubkey(), 5_000, 11);
    tx.ok();
    println!("record_payout: CU {} trace {}", tx.cu(), tx.trace_len());
    let r = record(&w, &mint, &a.pubkey());
    assert_eq!(
        (r.paid, r.round, r.paid_at, r.updated_slot),
        (5_000, 11, now, w.env.slot)
    );
    assert_eq!(
        (r.entry, r.amount, r.basis_at, r.flags),
        (ra.entry, got, ra.basis_at, 0)
    );
    let s = state(&w, &mint);
    assert_eq!((s.paid_total, s.records), (5_000, 1));
    // A touch: the round moves, the time does not; it counts as a record.
    w.env.warp(60);
    record_payout(&mut w, &mint, &a.pubkey(), 0, 12).ok();
    let r = record(&w, &mint, &a.pubkey());
    assert_eq!((r.paid, r.round, r.paid_at), (5_000, 12, now));
    let s = state(&w, &mint);
    assert_eq!((s.paid_total, s.records), (5_000, 2));
    assert_eq!(
        HOOK_AUTHORITY,
        bordrless_hook::hook_authority(&loss_cover_hook::ID).0
    );
}

#[test]
fn the_callbacks_only_take_the_token_programs_signer() {
    use anchor_lang::InstructionData;
    let mut w = World::new();
    let (creator, mint) = launch_cover(&mut w);
    let fake = Keypair::new();
    let t = w.env.funded(SOL);
    let args = TokenHookArgs {
        op: bordrless_hook::TokenOp::Transfer,
        phase: bordrless_hook::Phase::Before,
        mint,
        source: token::holding_address(&mint, &t.pubkey()),
        destination: token::holding_address(&mint, &creator.pubkey()),
        source_owner: t.pubkey(),
        destination_owner: creator.pubkey(),
        authority: t.pubkey(),
        authority_is_delegate: false,
        amount: 1,
        delta: 0,
        source_balance: 1,
        destination_balance: 0,
        decimals: 6,
        supply: 1,
        source_hook_data: [0; 64],
        destination_hook_data: [0; 64],
    };
    let metas = |signer: Pubkey, is_signer: bool| {
        let mut metas = vec![
            AccountMeta::new_readonly(signer, is_signer),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(args.source, false),
            AccountMeta::new_readonly(args.destination, false),
            AccountMeta::new_readonly(t.pubkey(), false),
        ];
        metas.extend(hook::extras(&mint, LP));
        metas
    };
    let before = w.env.account(&hook::state_address(&mint)).unwrap().data;
    for data in [
        loss_cover_hook::instruction::BeforeTransfer { args: args.clone() }.data(),
        loss_cover_hook::instruction::BeforeBurn { args: args.clone() }.data(),
    ] {
        let ix = anchor_lang::solana_program::instruction::Instruction {
            program_id: loss_cover_hook::ID,
            accounts: metas(fake.pubkey(), true),
            data: data.clone(),
        };
        refused(
            &w.env.send_paid_by(&[ix], &t, &[&fake]),
            LossCoverError::BadHookSigner,
        );
        let ix = anchor_lang::solana_program::instruction::Instruction {
            program_id: loss_cover_hook::ID,
            accounts: metas(loss_cover_hook::TOKEN_HOOK_SIGNER, false),
            data,
        };
        w.env.send_paid_by(&[ix], &t, &[]).expect_fail();
    }
    assert_eq!(
        w.env.account(&hook::state_address(&mint)).unwrap().data,
        before
    );
    // The pool's vault is a holding of the mint: the registry's pool is the DEX's.
    assert_eq!(
        swap::vault_address(&w.launch(&mint).pool, &mint),
        token::holding_address(&mint, &w.launch(&mint).pool)
    );
}
