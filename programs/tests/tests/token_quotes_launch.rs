//! Launches quoted in tokens (`docs/token-quotes.md` §2.2): a coin launched against 1 to 6 real SPL
//! Token or Token-2022 mints, one DEX curve pool (a leg) per quote, the supply split evenly, each
//! leg opened by `open_leg`, the launchpad as every leg's hook taking the creator fee on the quote
//! side into the leg's fee account, claimed per leg. After every instruction that moves a leg's
//! quote the pool invariant is asserted exactly (vault == quote_reserve + protocol_fees_quote).
//!
//! The figures this suite measures (compute units, account counts, transaction sizes) are printed
//! by `measures_compute_accounts_and_sizes` (run with `--nocapture`).

use anchor_lang::prelude::{AccountMeta, Pubkey};
use bordrless_core::policy;
use bordrless_hook::HookAccountList;
use bordrless_launch::client as launch;
use bordrless_launch::constants::{LEG_HOOK_FLAGS, MAX_LEGS, MIN_LEG_VIRTUAL_QUOTE};
use bordrless_launch::error::LaunchError;
use bordrless_launch::events::{LegCreatorFeesClaimed, LegOpened, TokenLaunchCreated};
use bordrless_launch::instructions::leg_registry_list;
use bordrless_launch::state::{Launch, LaunchRules, Legs};
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::{quote_launch_swap, reserves_of, VQ};
use bordrless_program_tests::quotes::{
    ext, pack_mint, xstock_extensions, Quote, QUOTE_FUNDS, TOKEN, TOKEN_2022,
};
use bordrless_program_tests::timelock::send_as;
use bordrless_swap::client as swap;
use bordrless_swap::error::SwapError;
use bordrless_swap::events::{PoolCreated, ProtocolFeesCollected, SplPoolCreated, Swapped};
use bordrless_swap::instructions::CreatePoolArgs;
use bordrless_swap::spl;
use bordrless_swap::state::Pool;
use bordrless_token::client as token;
use bordrless_token::state::Mint;
use hook_tester::client as tester;
use solana_keypair::Keypair;
use solana_signer::Signer;

const SUPPLY: u64 = policy::TOKEN_SUPPLY;
const FEE: u16 = 100;
/// $5,000 fully diluted in a $1 token of 6 decimals: the whole cap, raw.
const CAP_USDC: u64 = 5_000_000_000;

fn lcode(e: LaunchError) -> u32 {
    u32::from(e)
}

fn scode(e: SwapError) -> u32 {
    u32::from(e)
}

/// `legVirtualQuote` (`packages/shared/src/quotes.ts`) for a whole cap of `cap_raw`.
fn leg_vq(cap_raw: u64, share: u64) -> u64 {
    (u128::from(cap_raw) * u128::from(share) / u128::from(SUPPLY)) as u64
}

/// `legShares`.
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

/// A quote mint written at a fresh key: its extensions get the key (an xStocks-like mint's
/// metadata points at itself).
fn mint_at(
    w: &mut World,
    program: Pubkey,
    decimals: u8,
    exts: impl Fn(Pubkey) -> Vec<ext::Ext>,
) -> Pubkey {
    let key = Pubkey::new_unique();
    let issuer = w.env.deployer.pubkey();
    w.env.put_mint(
        key,
        program,
        pack_mint(
            Some(issuer),
            Some(issuer),
            decimals,
            u64::MAX / 2,
            &exts(key),
        ),
    );
    key
}

/// A mint's extensions, given its key.
type Extensions = fn(Pubkey) -> Vec<ext::Ext>;

fn xstock(mint: Pubkey) -> Vec<ext::Ext> {
    xstock_extensions(mint, Pubkey::new_from_array([7; 32]))
}

fn none(_: Pubkey) -> Vec<ext::Ext> {
    vec![]
}

fn leg_pool(w: &World, mint: &Pubkey, i: usize) -> Pool {
    w.env.read(&w.leg_keys(mint, i).pool())
}

/// A swap of the coin on leg `i`, as `quote_launch_swap` predicts it under the pool's terms.
fn predict(
    w: &World,
    mint: &Pubkey,
    i: usize,
    buy: bool,
    amount: u64,
) -> bordrless_program_tests::launch::LaunchSwap {
    let p = leg_pool(w, mint, i);
    let l = w.launch(mint);
    let q = quote_launch_swap(
        &reserves_of(&p),
        buy,
        amount,
        p.lp_fee_bps,
        p.protocol_share_bps,
        &LaunchRules::NONE.fee_rates(l.creator_fee_bps),
        0,
        0,
    );
    assert_eq!(q.failure, None, "the predicted swap lands");
    q
}

/// A buy and then a sell of half of it on leg `i`, each checked against the prediction: the
/// creator fee lands in the leg's fee account and is counted on the leg, Bordrless's LP fee and
/// share stay in the pool, the invariant holds. Answers the buy's and the sell's transactions.
fn trade_leg(w: &mut World, mint: &Pubkey, i: usize, buyer: &Keypair, amount: u64) -> (Tx, Tx) {
    let keys = w.leg_keys(mint, i);
    let fees = keys.fee_account();
    let fees_before = w.env.token_amount(&fees);
    let accrued_before = w.legs(mint).legs[i].creator_fees_accrued;
    let p0 = leg_pool(w, mint, i);
    let coins_before = w.env.holding(mint, &buyer.pubkey());
    let quote_acct = keys.quote_account(&buyer.pubkey());
    let quote_before = w.env.token_amount(&quote_acct);
    let q = predict(w, mint, i, true, amount);
    let buy = w.leg_buy(buyer, mint, i, amount);
    let ev: Swapped = buy.event();
    assert_eq!(ev.pool, keys.pool());
    assert_eq!(
        (ev.direction, ev.amount_in, ev.received_in),
        (1, amount, q.received)
    );
    assert_eq!(ev.deltas_in.len(), 1);
    assert_eq!(
        (ev.deltas_in[0].holding, ev.deltas_in[0].amount),
        (fees, q.creator_fee)
    );
    assert_eq!(ev.delivered_out, q.delivered.unwrap());
    assert_eq!(
        ev.lp_fee_bps, p0.lp_fee_bps,
        "a leg keeps its pool's LP fee"
    );
    assert_eq!(
        w.env.holding(mint, &buyer.pubkey()),
        coins_before + q.delivered.unwrap()
    );
    assert_eq!(w.env.token_amount(&quote_acct), quote_before - amount);
    assert_eq!(w.env.token_amount(&fees), fees_before + q.creator_fee);
    let p1 = leg_pool(w, mint, i);
    assert_eq!(
        p1.protocol_fees_quote,
        p0.protocol_fees_quote + q.lp_fee + q.protocol_fee
    );
    assert_eq!(p1.quote_reserve, p0.quote_reserve + q.to_reserve_in(true));
    w.env.assert_spl_invariant(&keys.pool());
    // The sell.
    let half = q.delivered.unwrap() / 2;
    let s = predict(w, mint, i, false, half);
    let quote_mid = w.env.token_amount(&quote_acct);
    let sell = w.leg_sell(buyer, mint, i, half);
    let ev: Swapped = sell.event();
    assert_eq!((ev.direction, ev.received_in), (0, half));
    assert_eq!(
        (ev.deltas_out[0].holding, ev.deltas_out[0].amount),
        (fees, s.creator_fee)
    );
    assert_eq!(ev.delivered_out, s.delivered.unwrap());
    assert_eq!(
        w.env.token_amount(&quote_acct),
        quote_mid + s.delivered.unwrap()
    );
    assert_eq!(
        w.env.token_amount(&fees),
        fees_before + q.creator_fee + s.creator_fee
    );
    assert_eq!(
        w.legs(mint).legs[i].creator_fees_accrued,
        accrued_before + q.creator_fee + s.creator_fee
    );
    let p2 = leg_pool(w, mint, i);
    assert_eq!(
        p2.protocol_fees_quote,
        p1.protocol_fees_quote + s.lp_fee + s.protocol_fee
    );
    w.env.assert_spl_invariant(&keys.pool());
    (buy, sell)
}

/// Pays leg `i`'s creator fees (sent by a stranger), checking the event and the counters.
fn claim(w: &mut World, mint: &Pubkey, i: usize) -> Tx {
    let keys = w.leg_keys(mint, i);
    let creator = w.launch(mint).creator;
    let owed = w.env.token_amount(&keys.fee_account());
    let to = keys.quote_account(&creator);
    let had = w.env.token_amount(&to);
    let claimed_before = w.legs(mint).legs[i].creator_fees_claimed;
    let stranger = w.env.funded(1_000_000_000);
    let ix = launch::claim_creator_fees_token(stranger.pubkey(), creator, &keys, i as u8);
    let tx = w.env.send_paid_by(&[ix], &stranger, &[]);
    let ev: LegCreatorFeesClaimed = tx.event();
    assert_eq!(
        (
            ev.index,
            ev.quote_mint,
            ev.creator,
            ev.destination,
            ev.amount
        ),
        (i as u8, keys.quote_mint, creator, to, owed)
    );
    assert_eq!(ev.claimed_total, claimed_before + owed);
    assert_eq!(w.env.token_amount(&to), had + owed);
    assert_eq!(w.env.token_amount(&keys.fee_account()), 0);
    assert_eq!(
        w.legs(mint).legs[i].creator_fees_claimed,
        claimed_before + owed
    );
    tx
}

#[test]
fn a_one_leg_launch_trades_pays_and_never_graduates() {
    for (program, decimals, exts) in [(TOKEN, 6u8, none as Extensions), (TOKEN_2022, 8, xstock)] {
        let mut w = World::new();
        let creator = w.env.funded(10_000_000_000);
        let quote = mint_at(&mut w, program, decimals, exts);
        let treasury = w.env.treasury.pubkey();
        let treasury_before = w.env.lamports(&treasury);
        let (mint, tx) = w.create_token_launch(&creator, FEE, &[(quote, CAP_USDC)]);
        let ev: TokenLaunchCreated = tx.event();
        assert_eq!(
            w.env.lamports(&treasury),
            treasury_before + policy::LAUNCH_FEE_LAMPORTS
        );
        let lk = launch::launch_address(&mint);
        let keys = w.leg_keys(&mint, 0);
        assert_eq!(
            (ev.launch, ev.mint, ev.creator, ev.legs_account),
            (lk, mint, creator.pubkey(), launch::legs_address(&mint))
        );
        assert_eq!(
            (ev.supply, ev.decimals, ev.creator_fee_bps, ev.lp_fee_bps),
            (SUPPLY, policy::TOKEN_DECIMALS, FEE, policy::LP_FEE_BPS)
        );
        assert_eq!(ev.launch_fee_lamports, policy::LAUNCH_FEE_LAMPORTS);
        assert_eq!(ev.legs.len(), 1);
        let t = &ev.legs[0];
        assert_eq!(
            (
                t.quote_mint,
                t.quote_program,
                t.decimals,
                t.virtual_quote,
                t.share
            ),
            (quote, program, decimals, CAP_USDC, SUPPLY)
        );
        assert_eq!((t.pool, t.fee_account), (keys.pool(), keys.fee_account()));
        assert!(tx
            .events::<bordrless_launch::events::LaunchCreated>()
            .is_empty());
        // The launch.
        let l: Launch = w.launch(&mint);
        assert!(l.is_token_quoted() && !l.is_native());
        assert_eq!((l.reserved[17], l.reserved[18]), (0, 2));
        assert_eq!(
            (l.pool, l.quote_mint, l.quote_holding),
            (keys.pool(), quote, keys.fee_account())
        );
        assert_eq!(
            (l.creator, l.creator_fee_bps, l.lp_fee_bps),
            (creator.pubkey(), FEE, policy::LP_FEE_BPS)
        );
        assert_eq!(
            (l.status, l.graduation_quote, l.curve_tokens),
            (0, u64::MAX, SUPPLY)
        );
        assert_eq!(
            (l.virtual_quote, l.virtual_base, l.reserve_tokens),
            (0, 0, 0)
        );
        assert_eq!((l.sniper_window_secs, l.sniper_start_bps), (0, 0));
        assert_eq!((l.modules, l.rules), (0, LaunchRules::NONE));
        assert_eq!(
            (l.holder_vault, l.lp_holding),
            (Pubkey::default(), Pubkey::default())
        );
        assert_eq!(l.kit_config, launch::kit_config_address(&mint));
        assert_eq!((l.custom_hook, l.config), (None, Pubkey::default()));
        // The legs.
        let legs: Legs = w.legs(&mint);
        assert_eq!(
            (legs.count, legs.opened, legs.mint, legs.launch),
            (1, 0, mint, lk)
        );
        let leg = legs.legs[0];
        assert_eq!(
            (
                leg.quote_mint,
                leg.quote_program,
                leg.decimals,
                leg.virtual_quote,
                leg.share
            ),
            (
                quote,
                u8::from(program == TOKEN_2022),
                decimals,
                CAP_USDC,
                SUPPLY
            )
        );
        assert_eq!(
            (leg.pool, leg.fee_account, leg.opened_at),
            (keys.pool(), keys.fee_account(), 0)
        );
        assert!(legs.legs[1..].iter().all(|l| *l == Default::default()));
        // The coin: no hook, no more minting, the whole supply in the launch's holding.
        let m: Mint = w.env.read(&mint);
        assert_eq!(
            (m.hook_program, m.mint_authority, m.supply),
            (None, None, SUPPLY)
        );
        assert_eq!(w.env.holding(&mint, &lk), SUPPLY);
        // No pool before the leg opens.
        assert!(w.env.account(&keys.pool()).is_none());
        // The leg opens: its fee account, its pool (the share model, the leg's own LP fee), its
        // registry.
        let open = w.open_leg(&mint, 0);
        let ev: LegOpened = open.event();
        assert_eq!(
            (
                ev.launch,
                ev.mint,
                ev.index,
                ev.quote_mint,
                ev.pool,
                ev.fee_account
            ),
            (lk, mint, 0, quote, keys.pool(), keys.fee_account())
        );
        assert_eq!(
            (ev.virtual_quote, ev.share, ev.opened, ev.count),
            (CAP_USDC, SUPPLY, 1, 1)
        );
        let created: PoolCreated = open.event();
        let spl_created: SplPoolCreated = open.event();
        assert_eq!(
            (created.creator, created.hook_program),
            (lk, Some(bordrless_launch::ID))
        );
        assert_eq!(spl_created.quote_program, program);
        let p = leg_pool(&w, &mint, 0);
        assert!(p.quote_spl() && p.curve);
        assert_eq!(
            (p.fee_model, p.protocol_fee_bps, p.protocol_share_bps),
            (1, 0, policy::LAUNCH_PROTOCOL_SHARE_BPS)
        );
        assert_eq!(
            (p.hook_program, p.hook_flags),
            (Some(bordrless_launch::ID), LEG_HOOK_FLAGS)
        );
        assert_eq!(
            (p.lp_fee_bps, p.virtual_base, p.virtual_quote),
            (policy::LP_FEE_BPS, 0, CAP_USDC)
        );
        assert_eq!(
            (p.base_reserve, p.quote_reserve, p.creator),
            (SUPPLY, 0, lk)
        );
        assert_eq!(p.quote_vault, keys.quote_vault());
        assert_eq!(w.env.holding(&mint, &lk), 0);
        assert_eq!(w.env.holding(&mint, &keys.pool()), SUPPLY);
        let fee_acct = w.env.account(&keys.fee_account()).unwrap();
        assert_eq!(fee_acct.owner, program);
        assert_eq!(
            Pubkey::new_from_array(fee_acct.data[32..64].try_into().unwrap()),
            lk,
            "the leg's fee account is the launch's"
        );
        let registry = w
            .env
            .account(&launch::registry_address(&keys.pool()))
            .unwrap();
        assert_eq!(
            HookAccountList::decode(&registry.data).unwrap(),
            leg_registry_list(launch::kit_config_address(&mint))
        );
        let legs = w.legs(&mint);
        assert_eq!((legs.opened, legs.legs[0].opened_at), (1, w.env.now));
        w.env.assert_spl_invariant(&keys.pool());
        // The raw offsets the SDK reads (§2.4): the launch's quote kind at byte 529 (its
        // `custom_hook` is always none), the pool's quote kind, program and decimals at 351, 368
        // and 369 (its hook is always some).
        let raw_launch = w.env.account(&lk).unwrap().data;
        assert_eq!(
            (raw_launch.len(), raw_launch[529], raw_launch[528]),
            (Launch::LEN, 2, 0)
        );
        assert_eq!(raw_launch[498], 0, "custom_hook: none");
        let raw_pool = w.env.account(&keys.pool()).unwrap().data;
        assert_eq!(raw_pool.len(), Pool::LEN);
        assert_eq!(
            (raw_pool[351], raw_pool[368], raw_pool[369]),
            (2, u8::from(program == TOKEN_2022), decimals)
        );
        assert_eq!(raw_pool[171], 1, "hook_program: some");
        // Trades.
        let buyer = w.quote_wallet(&quote, &program, QUOTE_FUNDS);
        trade_leg(&mut w, &mint, 0, &buyer, 1_000_000_000);
        trade_leg(&mut w, &mint, 0, &buyer, 37_123_457);
        // The opening price: a leg holds no quote at first, its price is virtual_quote / share,
        // and it rises as coins are bought.
        let p = leg_pool(&w, &mint, 0);
        assert!(
            u128::from(p.quote_reserve + p.virtual_quote) * u128::from(SUPPLY)
                > u128::from(CAP_USDC) * u128::from(p.base_reserve)
        );
        // Claim, by anyone, to the creator's token account (made by the sender).
        let creator_ata = keys.quote_account(&creator.pubkey());
        assert!(w.env.account(&creator_ata).is_none());
        claim(&mut w, &mint, 0);
        let stranger = w.env.funded(1_000_000_000);
        w.env.warp(1);
        let ix = launch::claim_creator_fees_token(stranger.pubkey(), creator.pubkey(), &keys, 0);
        w.env
            .send_paid_by(&[ix], &stranger, &[])
            .expect_code(lcode(LaunchError::NothingToClaim));
        // Bordrless's take to the fee collector's token account.
        let fees = leg_pool(&w, &mint, 0).protocol_fees_quote;
        let collector = w.env.deployer.pubkey();
        let ix = swap::collect_protocol_fees_spl(
            stranger.pubkey(),
            keys.pool(),
            quote,
            program,
            collector,
        );
        let tx = w.env.send_paid_by(&[ix], &stranger, &[]);
        assert_eq!(tx.event::<ProtocolFeesCollected>().quote_amount, fees);
        assert_eq!(
            w.env
                .token_amount(&spl::associated_token_address(&collector, &quote, &program)),
            fees
        );
        w.env.assert_spl_invariant(&keys.pool());
        // It never graduates, and the SOL launches' claims refuse it.
        let ix = launch::graduate(stranger.pubkey(), mint, quote, policy::LP_FEE_BPS, 0);
        w.env
            .send_paid_by(&[ix], &stranger, &[])
            .expect_code(lcode(LaunchError::TokenQuotedLaunch));
        let ix = launch::claim_creator_fees(creator.pubkey(), mint, quote);
        w.env
            .send_paid_by(&[ix], &creator, &[])
            .expect_code(lcode(LaunchError::TokenQuotedLaunch));
        let ix = launch::claim_creator_fees_sol(creator.pubkey(), mint, None, None);
        w.env
            .send_paid_by(&[ix], &creator, &[])
            .expect_code(lcode(LaunchError::NotNativeLaunch));
        // The admin can't turn it native (its quote holding is leg 0's fee account).
        let a = w.admin().pubkey();
        w.pause_dex(true);
        let mut ix = launch::migrate_launch_to_native(a, mint, quote, 0);
        ix.accounts[5].pubkey = keys.fee_account();
        w.as_admin(ix)
            .expect_code(lcode(LaunchError::TokenQuotedLaunch));
        w.pause_dex(false);
    }
}

/// Six quotes: three SPL Token (6, 9 and 2 decimals), three Token-2022 (xStocks-like at 8
/// decimals, plain, and one with metadata, a close authority, interest and new accounts
/// initialized).
fn six_quotes(w: &mut World) -> Vec<(Pubkey, Pubkey, u64)> {
    fn rich(mint: Pubkey) -> Vec<ext::Ext> {
        let issuer = Pubkey::new_from_array([9; 32]);
        vec![
            ext::default_state(1),
            ext::mint_close(issuer),
            ext::interest_bearing(issuer),
            ext::metadata_pointer(issuer, mint),
            ext::token_metadata(issuer, mint, "Rich", "RICH"),
        ]
    }
    let s = shares(6);
    let specs: [(Pubkey, u8, Extensions, u64); 6] = [
        (TOKEN, 6, none, CAP_USDC),
        // A $200 token of 9 decimals: 25 whole of them.
        (TOKEN, 9, none, 25_000_000_000),
        // A $1 token of 2 decimals.
        (TOKEN, 2, none, 500_000),
        // A $250 stock of 8 decimals: 20 whole of them.
        (TOKEN_2022, 8, xstock, 2_000_000_000),
        (TOKEN_2022, 6, none, CAP_USDC),
        (TOKEN_2022, 9, rich, 25_000_000_000),
    ];
    specs
        .iter()
        .enumerate()
        .map(|(i, (program, decimals, exts, cap))| {
            let mint = mint_at(w, *program, *decimals, exts);
            (mint, *program, leg_vq(*cap, s[i]))
        })
        .collect()
}

#[test]
fn a_six_leg_launch_opens_leg_by_leg_and_trades_once_all_are_open() {
    let mut w = World::new();
    let creator = w.env.funded(10_000_000_000);
    let quotes = six_quotes(&mut w);
    let q: Vec<Quote> = quotes.iter().map(|(m, _, vq)| (*m, *vq)).collect();
    let (mint, tx) = w.create_token_launch(&creator, 200, &q);
    tx.ok();
    let legs = w.legs(&mint);
    assert_eq!(legs.count, 6);
    let s = shares(6);
    assert_eq!(s.iter().sum::<u64>(), SUPPLY);
    assert_eq!(s[5], SUPPLY - 5 * (SUPPLY / 6));
    for (i, leg) in legs.legs.iter().enumerate() {
        assert_eq!(leg.share, s[i]);
        assert_eq!(leg.quote_mint, quotes[i].0);
        assert_eq!(leg.virtual_quote, quotes[i].2);
        assert!(leg.virtual_quote >= MIN_LEG_VIRTUAL_QUOTE);
    }
    // Five of six open: trading is refused on every open leg.
    let mut buyers = Vec::new();
    for (quote, program, _) in &quotes {
        let b = w.quote_wallet(quote, program, QUOTE_FUNDS);
        buyers.push(b);
    }
    for i in 0..5 {
        w.open_leg(&mint, i).ok();
        assert_eq!(w.legs(&mint).opened, i as u8 + 1);
    }
    for (i, b) in buyers.iter().enumerate().take(5) {
        let b = b.insecure_clone();
        w.leg_buy(&b, &mint, i, 1_000_000)
            .expect_code(lcode(LaunchError::LegsNotOpen));
    }
    // A leg opens once.
    w.env.warp(1);
    w.open_leg(&mint, 2)
        .expect_code(lcode(LaunchError::LegAlreadyOpen));
    w.open_leg(&mint, 5).ok();
    assert_eq!(w.legs(&mint).opened, 6);
    assert_eq!(w.env.holding(&mint, &launch::launch_address(&mint)), 0);
    let mut held = 0;
    for i in 0..6 {
        held += w.env.holding(&mint, &w.leg_keys(&mint, i).pool());
    }
    assert_eq!(held, SUPPLY);
    // Every leg trades; every leg's creator fee is its own token, claimed per leg.
    for (i, b) in buyers.iter().enumerate() {
        let b = b.insecure_clone();
        let p = leg_pool(&w, &mint, i);
        // A buy of about 1% of the leg's opening cap.
        let amount = (p.virtual_quote / 100).max(10_000);
        trade_leg(&mut w, &mint, i, &b, amount);
    }
    for i in 0..6 {
        claim(&mut w, &mint, i);
        w.env.assert_spl_invariant(&w.leg_keys(&mint, i).pool());
    }
    // The opening prices in USD are the same on every leg: virtual_quote / share in each token,
    // at that token's price, is the cap / supply (within a raw unit of each token's rounding).
    let legs = w.legs(&mint);
    for (i, cap) in [
        CAP_USDC,
        25_000_000_000,
        500_000,
        2_000_000_000,
        CAP_USDC,
        25_000_000_000,
    ]
    .iter()
    .enumerate()
    {
        assert_eq!(legs.legs[i].virtual_quote, leg_vq(*cap, legs.legs[i].share));
    }
}

#[test]
fn create_token_launch_refuses_what_section_2_2_refuses() {
    let mut w = World::new();
    let creator = w.env.funded(10_000_000_000);
    let issuer = w.env.deployer.pubkey();
    let usdc = mint_at(&mut w, TOKEN, 6, none);
    let send = |w: &mut World, quotes: &[Quote], fee: u16| {
        let (_, tx) = w.create_token_launch(&creator.insecure_clone(), fee, quotes);
        tx
    };
    // 0 or 7 quotes; a virtual quote missing.
    send(&mut w, &[], FEE).expect_code(lcode(LaunchError::InvalidLegCount));
    let seven: Vec<Quote> = (0..MAX_LEGS + 1)
        .map(|_| (mint_at(&mut w, TOKEN, 6, none), CAP_USDC))
        .collect();
    send(&mut w, &seven, FEE).expect_code(lcode(LaunchError::InvalidLegCount));
    let mint = Keypair::new();
    let mut args = World::token_launch_args("TQ", FEE, &[(usdc, CAP_USDC)]);
    args.virtual_quotes.push(CAP_USDC);
    let ix = launch::create_token_launch(
        creator.pubkey(),
        mint.pubkey(),
        w.env.treasury.pubkey(),
        args,
        &[usdc],
    );
    w.env
        .send_paid_by(&[ix], &creator, &[&mint])
        .expect_code(lcode(LaunchError::InvalidLegCount));
    // SOL in either form; a Bordrless mint (bridged SOL); the same token twice.
    w.env
        .put_mint(spl::NATIVE_MINT, TOKEN, pack_mint(None, None, 9, 0, &[]));
    send(&mut w, &[(spl::NATIVE_MINT, CAP_USDC)], FEE)
        .expect_code(lcode(LaunchError::SolQuoteToken));
    w.env.put_mint(
        spl::NATIVE_MINT_2022,
        TOKEN_2022,
        pack_mint(None, None, 9, 0, &[]),
    );
    send(
        &mut w,
        &[(usdc, CAP_USDC), (spl::NATIVE_MINT_2022, CAP_USDC)],
        FEE,
    )
    .expect_code(lcode(LaunchError::SolQuoteToken));
    let bridged = w.sol;
    send(&mut w, &[(bridged, CAP_USDC)], FEE).expect_code(lcode(LaunchError::QuoteMintInvalid));
    send(&mut w, &[(usdc, CAP_USDC), (usdc, CAP_USDC)], FEE)
        .expect_code(lcode(LaunchError::DuplicateQuoteToken));
    // A virtual quote below the minimum (and at it, which lands).
    send(&mut w, &[(usdc, MIN_LEG_VIRTUAL_QUOTE - 1)], FEE)
        .expect_code(lcode(LaunchError::LegVirtualQuoteTooSmall));
    send(&mut w, &[(usdc, MIN_LEG_VIRTUAL_QUOTE)], FEE).ok();
    // Every refused extension, by the launchpad's own codes (the DEX repeats them at open_leg).
    let refused: Vec<(Vec<ext::Ext>, LaunchError)> = vec![
        (
            vec![ext::transfer_fee(50)],
            LaunchError::QuoteMintTransferFee,
        ),
        (
            vec![ext::confidential_fee()],
            LaunchError::QuoteMintTransferFee,
        ),
        (
            vec![ext::non_transferable()],
            LaunchError::QuoteMintNonTransferable,
        ),
        (
            vec![ext::default_state(2)],
            LaunchError::QuoteMintFrozenByDefault,
        ),
        (
            vec![ext::transfer_hook(Some(issuer), Some(Pubkey::new_unique()))],
            LaunchError::QuoteMintTransferHook,
        ),
        (
            vec![ext::pausable(issuer, true)],
            LaunchError::QuoteMintPaused,
        ),
        (
            vec![ext::confidential_mint_burn()],
            LaunchError::QuoteMintConfidential,
        ),
        (
            vec![ext::raw(28, 32)],
            LaunchError::QuoteMintUnknownExtension,
        ),
    ];
    for (exts, why) in refused {
        let quote = w.env.new_quote_mint(TOKEN_2022, 6, &exts);
        send(&mut w, &[(usdc, CAP_USDC), (quote, CAP_USDC)], FEE).expect_code(lcode(why));
    }
    // The creator fee within the config's bounds.
    send(&mut w, &[(usdc, CAP_USDC)], policy::MAX_CREATOR_FEE_BPS + 1)
        .expect_code(lcode(LaunchError::CreatorFeeTooHigh));
    send(&mut w, &[(usdc, CAP_USDC)], 0).ok();
    // Metadata as for any launch.
    let mint = Keypair::new();
    let mut args = World::token_launch_args("TQ", FEE, &[(usdc, CAP_USDC)]);
    args.symbol = "WAY-TOO-LONG-SYMBOL".into();
    let ix = launch::create_token_launch(
        creator.pubkey(),
        mint.pubkey(),
        w.env.treasury.pubkey(),
        args,
        &[usdc],
    );
    w.env
        .send_paid_by(&[ix], &creator, &[&mint])
        .expect_code(lcode(LaunchError::InvalidMetadata));
    // A companion coin (its creator the companion's PDA for the mint, signing as a companion
    // would): refused.
    let mint = Keypair::new();
    let companion = bordrless_companion::client::creator_address(&mint.pubkey());
    w.env.fund(companion, 10_000_000_000);
    let ix = w.token_launch_ix(&companion, &mint.pubkey(), FEE, &[(usdc, CAP_USDC)]);
    w.env.without_sigverify();
    send_as(&mut w.env, &[ix], &[&mint], &[companion])
        .expect_code(lcode(LaunchError::CompanionTokenLaunch));
}

#[test]
fn launches_are_paused_with_the_config() {
    let mut w = World::new();
    let creator = w.env.funded(10_000_000_000);
    let usdc = mint_at(&mut w, TOKEN, 6, none);
    let admin = w.admin();
    let mut args = bordrless_program_tests::fixture::policy_launch_config(
        admin.pubkey(),
        w.env.treasury.pubkey(),
        w.sol,
    );
    args.paused = true;
    w.as_admin(launch::set_config(admin.pubkey(), args)).ok();
    let (_, tx) = w.create_token_launch(&creator, FEE, &[(usdc, CAP_USDC)]);
    tx.expect_code(lcode(LaunchError::Paused));
}

#[test]
fn open_leg_checks_its_accounts_and_the_dex_checks_the_mint_again() {
    let mut w = World::new();
    let creator = w.env.funded(10_000_000_000);
    let usdc = mint_at(&mut w, TOKEN, 6, none);
    let issuer = w.env.deployer.pubkey();
    let stock = Pubkey::new_unique();
    let stock_exts = xstock_extensions(stock, issuer);
    w.env.put_mint(
        stock,
        TOKEN_2022,
        pack_mint(Some(issuer), Some(issuer), 8, 1, &stock_exts),
    );
    let (mint, tx) =
        w.create_token_launch(&creator, FEE, &[(usdc, CAP_USDC), (stock, 1_000_000_000)]);
    tx.ok();
    let payer = w.env.payer.pubkey();
    // No such leg.
    let mut ix = w.open_leg_ix(&payer, &mint, 0);
    ix.data = bordrless_launch::instruction::OpenLeg { index: 2 }.data_vec();
    w.env
        .send(&[ix], &[])
        .expect_code(lcode(LaunchError::NoSuchLeg));
    // Another leg's quote, pool, fee account or program.
    let k1 = w.leg_keys(&mint, 1);
    for (at, key) in [
        (4, k1.quote_mint),
        (10, k1.pool()),
        (5, k1.fee_account()),
        (18, TOKEN_2022),
    ] {
        let mut ix = w.open_leg_ix(&payer, &mint, 0);
        ix.accounts[at].pubkey = key;
        w.env
            .send(&[ix], &[])
            .expect_code(lcode(LaunchError::WrongLegAccount));
    }
    // The stock's issuer pauses it after the launch was made: the DEX refuses the leg's pool until
    // it is unpaused.
    let paused: Vec<ext::Ext> = stock_exts
        .iter()
        .map(|e| {
            if e.0 == spl::ext::PAUSABLE {
                ext::pausable(issuer, true)
            } else {
                e.clone()
            }
        })
        .collect();
    w.env.put_mint(
        stock,
        TOKEN_2022,
        pack_mint(Some(issuer), Some(issuer), 8, 1, &paused),
    );
    w.open_leg(&mint, 1)
        .expect_code(scode(SwapError::QuoteMintPaused));
    // ... or sets a transfer hook program.
    let hooked: Vec<ext::Ext> = stock_exts
        .iter()
        .map(|e| {
            if e.0 == spl::ext::TRANSFER_HOOK {
                ext::transfer_hook(Some(issuer), Some(Pubkey::new_unique()))
            } else {
                e.clone()
            }
        })
        .collect();
    w.env.put_mint(
        stock,
        TOKEN_2022,
        pack_mint(Some(issuer), Some(issuer), 8, 1, &hooked),
    );
    w.env.warp(1);
    w.open_leg(&mint, 1)
        .expect_code(scode(SwapError::QuoteMintTransferHook));
    // Back to as it was: the leg opens.
    w.env.put_mint(
        stock,
        TOKEN_2022,
        pack_mint(Some(issuer), Some(issuer), 8, 1, &stock_exts),
    );
    w.env.warp(1);
    w.open_leg(&mint, 1).ok();
    // A SOL launch has no `Legs` (Anchor refuses the missing account before the launch's
    // `NotTokenLaunch` constraint is read).
    let (sol_mint, tx) = w.create_launch(&creator, "SOL", FEE, VQ);
    tx.ok();
    let mut ix = w.open_leg_ix(&payer, &mint, 0);
    ix.accounts[1].pubkey = launch::launch_address(&sol_mint);
    ix.accounts[2].pubkey = launch::legs_address(&sol_mint);
    w.env.send(&[ix], &[]).expect_code(u32::from(
        anchor_lang::error::ErrorCode::AccountNotInitialized,
    ));
    let ix = launch::claim_creator_fees_token(
        payer,
        creator.pubkey(),
        &launch::LegKeys {
            mint: sol_mint,
            quote_mint: usdc,
            quote_program: TOKEN,
            lp_fee_bps: policy::LP_FEE_BPS,
        },
        0,
    );
    w.env.send(&[ix], &[]).expect_code(u32::from(
        anchor_lang::error::ErrorCode::AccountNotInitialized,
    ));
}

#[test]
fn nobody_can_take_a_legs_addresses_before_it_opens() {
    let mut w = World::new();
    let creator = w.env.funded(10_000_000_000);
    let usdc = mint_at(&mut w, TOKEN, 6, none);
    let stock = mint_at(&mut w, TOKEN_2022, 8, xstock);
    let (mint, tx) =
        w.create_token_launch(&creator, FEE, &[(usdc, CAP_USDC), (stock, 1_000_000_000)]);
    tx.ok();
    let k = w.leg_keys(&mint, 1);
    let griefer = w.env.funded(10_000_000_000);
    // The leg's pool through the DEX: only the launchpad's hook authority creates a pool in its
    // name (a keypair can't sign for it; another program's authority isn't it).
    let args = CreatePoolArgs {
        lp_fee_bps: policy::LP_FEE_BPS,
        hook_program: bordrless_launch::ID,
        hook_flags: LEG_HOOK_FLAGS,
        virtual_base: 0,
        virtual_quote: 1,
        base_amount: 1,
        quote_amount: 0,
        base_hook_accounts: 0,
        quote_hook_accounts: 0,
        hook_data: vec![],
    };
    let keys = swap::CreateSplPoolKeys {
        payer: griefer.pubkey(),
        authority: griefer.pubkey(),
        treasury: w.env.treasury.pubkey(),
        base_mint: mint,
        quote_mint: stock,
        quote_program: TOKEN_2022,
        hook_caller: tester::hook_authority().0,
    };
    w.env
        .send_paid_by(
            &[tester::as_hook(swap::create_spl_pool(&keys, args, vec![]))],
            &griefer,
            &[],
        )
        .expect_code(scode(SwapError::BadHookCaller));
    // Every account the leg will create, made or funded first: its fee account (lamports), its
    // pool (lamports), the pool's vaults (made: anyone may) and its registry (lamports).
    for key in [
        k.fee_account(),
        k.pool(),
        launch::registry_address(&k.pool()),
    ] {
        w.env.fund(key, 1_000_000);
    }
    w.env
        .send_paid_by(
            &[
                token::create_holding(griefer.pubkey(), mint, k.pool()),
                bordrless_program_tests::quotes::create_ata_ix(
                    griefer.pubkey(),
                    k.pool(),
                    stock,
                    TOKEN_2022,
                ),
            ],
            &griefer,
            &[],
        )
        .ok();
    w.open_all_legs(&mint);
    assert_eq!(w.legs(&mint).opened, 2);
    w.env.assert_spl_invariant(&k.pool());
    let b = w.quote_wallet(&stock, &TOKEN_2022, QUOTE_FUNDS);
    trade_leg(&mut w, &mint, 1, &b, 1_000_000);
}

#[test]
fn a_legs_callbacks_take_only_its_own_accounts() {
    let mut w = World::new();
    let creator = w.env.funded(10_000_000_000);
    let usdc = mint_at(&mut w, TOKEN, 6, none);
    let other = mint_at(&mut w, TOKEN, 6, none);
    let (mint, _) = w.create_token_launch(&creator, FEE, &[(usdc, CAP_USDC), (other, CAP_USDC)]);
    w.open_all_legs(&mint);
    let (mint2, _) = w.create_token_launch(&creator, FEE, &[(usdc, CAP_USDC)]);
    w.open_all_legs(&mint2);
    let b = w.quote_wallet(&usdc, &TOKEN, QUOTE_FUNDS);
    w.env
        .send_paid_by(
            &[token::create_holding(b.pubkey(), mint, b.pubkey())],
            &b,
            &[],
        )
        .ok();
    let k = w.leg_keys(&mint, 0);
    let buy = || launch::leg_swap(&k, b.pubkey(), b.pubkey(), 1, 1_000_000, 0);
    // The extras: 16 the launch, 17 the leg's fee account, 18 the Legs, 19 the kit config.
    let cases: Vec<(usize, Pubkey, bool, u32)> = vec![
        // Another leg's fee account.
        (
            17,
            w.leg_keys(&mint, 1).fee_account(),
            true,
            lcode(LaunchError::WrongLegAccount),
        ),
        // Another launch's fee account for the same quote.
        (
            17,
            w.leg_keys(&mint2, 0).fee_account(),
            true,
            lcode(LaunchError::WrongLegAccount),
        ),
        // Not the Legs: the kit config, another launch's Legs.
        (
            18,
            launch::kit_config_address(&mint),
            true,
            lcode(LaunchError::LegsAccountInvalid),
        ),
        (
            18,
            launch::legs_address(&mint2),
            true,
            lcode(LaunchError::LegsAccountInvalid),
        ),
        // The Legs read-only.
        (
            18,
            launch::legs_address(&mint),
            false,
            lcode(LaunchError::LegsAccountInvalid),
        ),
        // Another launch.
        (
            16,
            launch::launch_address(&mint2),
            true,
            lcode(LaunchError::WrongPool),
        ),
    ];
    for (at, key, writable, code) in cases {
        let mut ix = buy();
        ix.accounts[at] = if writable {
            AccountMeta::new(key, false)
        } else {
            AccountMeta::new_readonly(key, false)
        };
        w.env.send_paid_by(&[ix], &b, &[]).expect_code(code);
    }
    // The leg of one launch can't be traded with the other's accounts: its pool is not among
    // the other's legs.
    let mut ix = buy();
    let k2 = w.leg_keys(&mint2, 0);
    ix.accounts[16] = AccountMeta::new(launch::launch_address(&mint2), false);
    ix.accounts[17] = AccountMeta::new(k2.fee_account(), false);
    ix.accounts[18] = AccountMeta::new(launch::legs_address(&mint2), false);
    w.env
        .send_paid_by(&[ix], &b, &[])
        .expect_code(lcode(LaunchError::WrongPool));
    // As built: it lands.
    w.env.send_paid_by(&[buy()], &b, &[]).ok();
    w.env.assert_spl_invariant(&k.pool());
}

#[test]
fn claims_check_the_leg_and_the_creator() {
    let mut w = World::new();
    let creator = w.env.funded(10_000_000_000);
    let usdc = mint_at(&mut w, TOKEN, 6, none);
    let other = mint_at(&mut w, TOKEN_2022, 6, none);
    let (mint, _) = w.create_token_launch(&creator, FEE, &[(usdc, CAP_USDC), (other, CAP_USDC)]);
    let payer = w.env.payer.pubkey();
    let k0 = w.leg_keys(&mint, 0);
    let k1 = w.leg_keys(&mint, 1);
    // Before the leg opens: nothing to claim.
    let ix = launch::claim_creator_fees_token(payer, creator.pubkey(), &k0, 0);
    w.env
        .send(&[ix], &[])
        .expect_code(lcode(LaunchError::NothingToClaim));
    w.open_all_legs(&mint);
    let b = w.quote_wallet(&usdc, &TOKEN, QUOTE_FUNDS);
    w.leg_buy(&b, &mint, 0, 1_000_000_000).ok();
    // No such leg; another leg's accounts; someone else as the creator.
    let ix = launch::claim_creator_fees_token(payer, creator.pubkey(), &k0, 2);
    w.env
        .send(&[ix], &[])
        .expect_code(lcode(LaunchError::NoSuchLeg));
    let ix = launch::claim_creator_fees_token(payer, creator.pubkey(), &k1, 0);
    w.env
        .send(&[ix], &[])
        .expect_code(lcode(LaunchError::WrongLegAccount));
    let thief = Pubkey::new_unique();
    let ix = launch::claim_creator_fees_token(payer, thief, &k0, 0);
    w.env
        .send(&[ix], &[])
        .expect_code(lcode(LaunchError::NotCreator));
    // The creator's token account must be its own.
    let mut ix = launch::claim_creator_fees_token(payer, creator.pubkey(), &k0, 0);
    ix.accounts[6].pubkey = k0.quote_account(&thief);
    w.env.send(&[ix], &[]).expect_fail();
    // A leg with nothing yet.
    w.env.warp(1);
    let ix = launch::claim_creator_fees_token(payer, creator.pubkey(), &k1, 1);
    w.env
        .send(&[ix], &[])
        .expect_code(lcode(LaunchError::NothingToClaim));
    claim(&mut w, &mint, 0);
}

#[test]
fn sol_launches_trade_beside_token_launches() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(100_000_000_000);
    let usdc = mint_at(&mut w, TOKEN, 6, none);
    let (tq, _) = w.create_token_launch(&creator, FEE, &[(usdc, CAP_USDC)]);
    w.open_all_legs(&tq);
    let (sol_mint, tx) = w.create_launch(&creator, "SOL", FEE, VQ);
    tx.ok();
    let trader = w.wallet_with_sol(10_000_000_000);
    w.buy(&trader, &sol_mint, 1_000_000_000).ok();
    let coins = w.env.holding(&sol_mint, &trader.pubkey());
    assert!(coins > 0);
    w.sell(&trader, &sol_mint, coins / 2).ok();
    let l = w.launch(&sol_mint);
    assert!(!l.is_token_quoted() && l.creator_fees_accrued > 0);
    w.assert_pool_invariant(&w.launch_pool_key(&sol_mint));
    // The token launch's leg trades too.
    let b = w.quote_wallet(&usdc, &TOKEN, QUOTE_FUNDS);
    trade_leg(&mut w, &tq, 0, &b, 5_000_000);
}

#[test]
fn measures_compute_accounts_and_sizes() {
    let mut w = World::new();
    let creator = w.env.funded(10_000_000_000);
    let quotes = six_quotes(&mut w);
    let q: Vec<Quote> = quotes.iter().map(|(m, _, vq)| (*m, *vq)).collect();
    let mint = Keypair::new();
    let ix = w.token_launch_ix(&creator.pubkey(), &mint.pubkey(), FEE, &q);
    let accounts6 = ix.accounts.len();
    let tx = w.env.send_paid_by(&[ix], &creator, &[&mint]);
    println!(
        "create_token_launch (6 legs): {} CU, {} accounts, {} bytes",
        tx.cu(),
        accounts6,
        tx.size
    );
    let mint1 = Keypair::new();
    let ix = w.token_launch_ix(&creator.pubkey(), &mint1.pubkey(), FEE, &q[..1]);
    let accounts1 = ix.accounts.len();
    let tx1 = w.env.send_paid_by(&[ix], &creator, &[&mint1]);
    println!(
        "create_token_launch (1 leg): {} CU, {} accounts, {} bytes",
        tx1.cu(),
        accounts1,
        tx1.size
    );
    let mint = mint.pubkey();
    let payer = w.env.payer.pubkey();
    for i in [0usize, 3] {
        let ix = w.open_leg_ix(&payer, &mint, i);
        let n = ix.accounts.len();
        let tx = w.env.send(&[ix], &[]);
        let program = if quotes[i].1 == TOKEN {
            "SPL Token"
        } else {
            "Token-2022 xStocks-like"
        };
        println!(
            "open_leg ({program}): {} CU, {} accounts, {} bytes",
            tx.cu(),
            n,
            tx.size
        );
    }
    for i in [1usize, 2, 4, 5] {
        w.open_leg(&mint, i).ok();
    }
    for i in [0usize, 3] {
        let (quote, program, _) = quotes[i];
        let b = w.quote_wallet(&quote, &program, QUOTE_FUNDS);
        let (buy, sell) = trade_leg(&mut w, &mint, i, &b, (quotes[i].2 / 100).max(10_000));
        let name = if program == TOKEN {
            "SPL Token"
        } else {
            "Token-2022 xStocks-like"
        };
        let n = launch::leg_swap(&w.leg_keys(&mint, i), b.pubkey(), b.pubkey(), 1, 1, 0)
            .accounts
            .len();
        println!(
            "swap_spl buy ({name}): {} CU (with the trader's create_holding), {} accounts, {} bytes",
            buy.cu(),
            n,
            buy.size
        );
        println!(
            "swap_spl sell ({name}): {} CU, {} accounts, {} bytes",
            sell.cu(),
            n,
            sell.size
        );
        let tx = claim(&mut w, &mint, i);
        println!(
            "claim_creator_fees_token ({name}, creating the ATA): {} CU, {} bytes",
            tx.cu(),
            tx.size
        );
        let keys = w.leg_keys(&mint, i);
        let collector = w.env.deployer.pubkey();
        let ix = swap::collect_protocol_fees_spl(payer, keys.pool(), quote, program, collector);
        let n = ix.accounts.len();
        let tx = w.env.send(&[ix], &[]);
        println!(
            "collect_protocol_fees_spl ({name}, creating the ATA): {} CU, {} accounts, {} bytes",
            tx.cu(),
            n,
            tx.size
        );
        w.env.assert_spl_invariant(&keys.pool());
    }
    // A buy alone (the trader's holding already made).
    for i in [0usize, 3] {
        let (quote, program, vq) = quotes[i];
        let b = w.quote_wallet(&quote, &program, QUOTE_FUNDS);
        w.env
            .send_paid_by(
                &[token::create_holding(b.pubkey(), mint, b.pubkey())],
                &b,
                &[],
            )
            .ok();
        let ix = launch::leg_swap(
            &w.leg_keys(&mint, i),
            b.pubkey(),
            b.pubkey(),
            1,
            vq / 100,
            0,
        );
        let tx = w.env.send_paid_by(&[ix], &b, &[]);
        let name = if program == TOKEN {
            "SPL Token"
        } else {
            "Token-2022 xStocks-like"
        };
        println!(
            "swap_spl buy alone ({name}): {} CU, {} bytes",
            tx.cu(),
            tx.size
        );
        w.env.assert_spl_invariant(&w.leg_keys(&mint, i).pool());
    }
    println!(
        "Legs account: {} bytes; Pool: {} bytes",
        Legs::LEN,
        Pool::LEN
    );
}

trait DataVec {
    fn data_vec(self) -> Vec<u8>;
}

impl<T: anchor_lang::InstructionData> DataVec for T {
    fn data_vec(self) -> Vec<u8> {
        self.data()
    }
}
