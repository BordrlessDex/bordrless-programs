//! Phase 2 audit, round 3 (hostile): the round-2 fixes (`payable`/`RESERVED_KEYS`,
//! `SETTLE_GRACE_SECS`, `FeesUnclaimed`, `prize_due` skipping stale rounds and failing open).
//! Helpers copied from `companion_kinds.rs`. `poc_*` assert the bad outcome they document;
//! `control_*` the safe one; `measure_*` print sizes and CU.
#![allow(dead_code, unused_imports, clippy::all)]

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{InstructionData, ToAccountMetas};
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{CreateArgs, CreateGameArgs, GameKindArgs, HookStatusArgs};
use bordrless_companion::state::{Companion, DrawStatus, Game, GameKind, ShareReceipt, Split};
use bordrless_core::policy;
use bordrless_game::{
    jackpot_mark, round_of, round_start, GameHeader, JackpotHeader, Slots, StreakHeader,
};
use bordrless_launch::client::{self as launch, CustomHookAccounts};
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::program_bytes;
use bordrless_swap::client as swap;
use bordrless_swap::instructions::{AddLiquidityArgs, RemoveLiquidityArgs};
use bordrless_token::client::{self as token, Hook};
use solana_keypair::Keypair;
use solana_signer::Signer;

const JACKPOT: Pubkey = studio_jackpot::ID;
const STREAK: Pubkey = studio_streak::ID;
/// Studio's upgrade key: what Studio deploys its hooks under.
const STUDIO_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[0];
const PACKET: usize = 1_232;
const CREATOR_FEE: u16 = 200;
const BOUNTY_BPS: u16 = 50;
const SPLIT: Split = Split {
    buyback_bps: 3_000,
    holders_bps: 0,
    beneficiary_bps: 0,
};
const POT_BPS: u16 = 7_000;
const MIN_POT: u64 = 100_000_000;
const TIMER: u32 = studio_jackpot::TIMER_SECS;
const EPOCH: u32 = studio_streak::EPOCH_SECS;
const CLAIM_WINDOW: u32 = 3_600;

fn code(e: CompanionError) -> u32 {
    u32::from(e)
}

#[track_caller]
fn refused(tx: &Tx, e: CompanionError) {
    tx.expect_code(code(e));
    let name = format!("{e:?}");
    assert!(
        tx.logs()
            .iter()
            .any(|l| l.contains(&format!("Error Code: {name}"))),
        "expected {name}\n{}",
        tx.logs().join("\n")
    );
}

/// The heap a transaction's companion instruction peaked at, when the companion was built with the
/// instrumented allocator (`custom-heap`, the audit's heap probe, which logs `0x4ea9` and the bytes
/// used at each new 256-byte high-water mark); `None` with the deployed build.
fn heap_peak(tx: &Tx) -> Option<u64> {
    tx.logs()
        .iter()
        .filter_map(|l| l.strip_prefix("Program log: 0x4ea9, 0x"))
        .filter_map(|rest| u64::from_str_radix(rest.split(',').next()?, 16).ok())
        .max()
}

fn create_args() -> CreateArgs {
    CreateArgs {
        split: Split {
            buyback_bps: 10_000,
            holders_bps: 0,
            beneficiary_bps: 0,
        },
        bounty_bps: BOUNTY_BPS,
        max_buyback: SOL,
        buyback_interval: 60,
        vest_secs: 0,
        fund: SOL / 2,
    }
}

fn game_args(kind: GameKind) -> (CreateGameArgs, GameKindArgs) {
    match kind {
        GameKind::Jackpot => (
            CreateGameArgs {
                kind,
                hook: JACKPOT,
                split: SPLIT,
                pot_bps: POT_BPS,
                round_secs: 0,
                min_pot: MIN_POT,
                prize_bps: 5_000,
                claim_window_secs: 0,
                max_attempts: 0,
            },
            GameKindArgs {
                timer_secs: TIMER,
                min_tokens: studio_jackpot::MIN_TOKENS,
                ..GameKindArgs::default()
            },
        ),
        GameKind::Streak => (
            CreateGameArgs {
                kind,
                hook: STREAK,
                split: SPLIT,
                pot_bps: POT_BPS,
                round_secs: EPOCH,
                min_pot: MIN_POT,
                prize_bps: 10_000,
                claim_window_secs: CLAIM_WINDOW,
                max_attempts: 0,
            },
            GameKindArgs {
                min_streak_secs: studio_streak::MIN_STREAK_SECS,
                min_weight: studio_streak::MIN_WEIGHT,
                ..GameKindArgs::default()
            },
        ),
        GameKind::Lottery => unreachable!("phase 1's suites"),
    }
}

/// The standard `prepare` of a Studio hook.
fn prepare_ix(hook: Pubkey, payer: Pubkey, mint: Pubkey) -> Instruction {
    let accounts = studio_jackpot::accounts::Prepare {
        payer,
        mint,
        state: bordrless_game::state_address(&hook, &mint).0,
        registry: bordrless_hook::hook_accounts_address(&hook, &mint).0,
        system_program: bordrless_program_tests::SYSTEM_PROGRAM_ID,
    }
    .to_account_metas(None);
    Instruction {
        program_id: hook,
        accounts,
        data: studio_jackpot::instruction::Prepare {}.data(),
    }
}

/// The streak starter's `enter` for `owner`'s holding.
fn enter_ix(mint: Pubkey, owner: Pubkey) -> Instruction {
    Instruction {
        program_id: STREAK,
        accounts: studio_streak::accounts::Enter {
            state: bordrless_game::state_address(&STREAK, &mint).0,
            mint,
            holding: token::holding_address(&mint, &owner),
            hook_authority: bordrless_game::cpi::hook_authority_address(&STREAK).0,
            token_program: bordrless_token::ID,
            token_event_authority: token::event_authority(),
        }
        .to_account_metas(None),
        data: studio_streak::instruction::Enter {}.data(),
    }
}

/// A world with both starters deployed as Studio deploys them: upgradeable by Studio's key.
fn world() -> World {
    let mut w = World::new();
    for (name, id) in [("studio_jackpot", JACKPOT), ("studio_streak", STREAK)] {
        w.env
            .svm
            .add_program(id, &program_bytes(name))
            .unwrap_or_else(|e| panic!("load {name}: {e:?}"));
        w.env.set_upgrade_authority(id, Some(STUDIO_KEY));
    }
    w
}

/// The protocol's 22-address lookup table, as the SDK lists it.
fn table_22(w: &World) -> Vec<Pubkey> {
    let mut addresses = protocol_lookup_table(w);
    addresses.extend([
        companion::event_authority(),
        bordrless_launch::ID,
        bordrless_swap::ID,
        bordrless_token::ID,
    ]);
    assert_eq!(addresses.len(), 22);
    addresses
}

fn setup_ixs(launcher: &Pubkey, mint: &Pubkey, kind: GameKind) -> Vec<Instruction> {
    let (args, k) = game_args(kind);
    vec![
        companion::create(*launcher, *launcher, *mint, create_args()),
        prepare_ix(args.hook, *launcher, *mint),
        companion::create_game_v2(*launcher, *mint, args, k),
    ]
}

fn hook_config(w: &mut World, creator: &Keypair, hook: Pubkey, flags: u16) -> Pubkey {
    let (config, tx) = w.create_config(
        creator,
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: CREATOR_FEE,
            custom_hook: Some(hook),
            custom_hook_flags: flags,
            label: "Game".to_string(),
        },
    );
    tx.ok();
    config
}

fn launch_ix(
    w: &World,
    launcher: &Pubkey,
    mint: &Pubkey,
    config: &Pubkey,
    uri: &str,
) -> Instruction {
    let c = w.launch_config(config);
    let custom = c.custom_hook.map(|h| w.custom_hook_accounts(&h, mint));
    let mut args = World::launch_args("GAMEXXXXXX", c.creator_fee_bps, VQ, c.rules);
    args.name = "N".repeat(32);
    args.uri = uri.to_string();
    let inner = launch::create_launch_with(
        companion::creator_address(mint),
        *mint,
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        args.clone(),
        Some(*config),
        custom.as_ref(),
    );
    companion::launch(*launcher, *mint, &inner, args)
}

/// A game coin launched through its companion, past the sniper window.
struct Coin {
    w: World,
    mint: Pubkey,
    hook: Pubkey,
    cranker: Keypair,
}

impl Coin {
    fn new(kind: GameKind) -> Self {
        let mut w = world();
        let launcher = w.wallet_with_sol(50 * SOL);
        let mint_kp = Keypair::new();
        let mint = mint_kp.pubkey();
        let hook = game_args(kind).0.hook;
        let tx = w.env.send_paid_by(
            &setup_ixs(&launcher.pubkey(), &mint, kind),
            &launcher,
            &[&mint_kp],
        );
        tx.ok();
        let set: GameKindSet = tx.event();
        assert_eq!(
            (set.kind, set.audited, set.pot_cap),
            (kind, false, DEFAULT_POT_CAP)
        );
        assert!(
            w.env
                .account(&companion::hook_status_address(&hook))
                .is_none(),
            "no status"
        );
        let config = hook_config(&mut w, &launcher, hook, kind.hook_flags());
        let ix = launch_ix(&w, &launcher.pubkey(), &mint, &config, "https://x.y/z");
        w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]).ok();
        assert_eq!(w.launch(&mint).custom_hook, Some(hook));
        w.env.warp(31);
        let cranker = w.wallet_with_sol(SOL);
        Self {
            w,
            mint,
            hook,
            cranker,
        }
    }

    fn state_data(&self) -> Vec<u8> {
        self.w
            .env
            .account(&bordrless_game::state_address(&self.hook, &self.mint).0)
            .unwrap()
            .data
    }

    fn header(&self) -> GameHeader {
        GameHeader::read(&self.state_data(), &self.mint).unwrap()
    }

    fn jackpot(&self) -> JackpotHeader {
        JackpotHeader::parse(&self.state_data()).unwrap()
    }

    fn slots(&self, owner: &Pubkey) -> Slots {
        Slots::decode(&self.w.env.hook_data(&self.mint, owner))
    }

    fn game(&self) -> Game {
        self.w.env.read(&companion::game_address(&self.mint))
    }

    fn companion(&self) -> Companion {
        self.w.env.read(&companion::companion_address(&self.mint))
    }

    fn balance(&self, owner: &Pubkey) -> u64 {
        self.w.env.holding(&self.mint, owner)
    }

    fn send(&mut self, ix: Instruction) -> Tx {
        let cranker = self.cranker.insecure_clone();
        self.w.env.send_paid_by(&[ix], &cranker, &[])
    }

    /// A wallet that buys `lamports` of the token.
    fn buyer(&mut self, lamports: u64) -> Keypair {
        let t = self.w.wallet_with_sol(lamports + SOL);
        self.w.buy(&t, &self.mint, lamports).ok();
        t
    }

    fn buy(&mut self, who: &Keypair, lamports: u64) -> Tx {
        self.w.buy(who, &self.mint, lamports)
    }

    /// `amount` tokens from `from` to `to`'s holding (created first).
    fn transfer(&mut self, from: &Keypair, to: &Pubkey, amount: u64) -> Tx {
        let custom = self.w.custom_hook_accounts(&self.hook, &self.mint);
        let ixs = [
            token::create_holding(from.pubkey(), self.mint, *to),
            token::transfer_with(
                from.pubkey(),
                token::holding_address(&self.mint, &from.pubkey()),
                token::holding_address(&self.mint, to),
                self.mint,
                Some(Hook::of(self.hook)),
                custom.extras,
                amount,
            ),
        ];
        self.w.env.send_paid_by(&ixs, from, &[])
    }

    /// Trading volume that leaves no holder behind: wallets that buy and sell everything.
    fn volume(&mut self, wallets: usize, lamports: u64) {
        for _ in 0..wallets {
            let t = self.w.wallet_with_sol(lamports + SOL);
            self.w.buy(&t, &self.mint, lamports).ok();
            let held = self.balance(&t.pubkey());
            self.w.sell(&t, &self.mint, held).ok();
        }
    }

    fn claim_fees(&mut self) -> Tx {
        let ix = companion::claim_fees_game(self.cranker.pubkey(), self.mint, self.hook);
        self.send(ix)
    }

    fn set_status(&mut self, audited: bool, pot_cap: u64, blocked: bool) -> Tx {
        let deployer = self.w.env.deployer.insecure_clone();
        let ix = companion::set_hook_status(
            deployer.pubkey(),
            self.hook,
            HookStatusArgs {
                audited,
                pot_cap,
                blocked,
            },
        );
        self.w.env.send_paid_by(&[ix], &deployer, &[])
    }

    fn warp_to(&mut self, t: i64) {
        assert!(t >= self.w.env.now, "the clock never goes back");
        self.w.env.warp(t - self.w.env.now);
    }

    // ---- jackpot ----

    fn settle(&mut self, buyer: &Pubkey) -> Tx {
        let ix = companion::settle(self.cranker.pubkey(), self.mint, self.hook, *buyer);
        self.send(ix)
    }

    // ---- streak ----

    fn epoch(&self) -> u32 {
        round_of(self.w.env.now, EPOCH)
    }

    /// To `secs` into epoch `epoch`.
    fn warp_into(&mut self, epoch: u32, secs: i64) {
        self.warp_to(round_start(epoch, EPOCH) + secs);
    }

    fn enter(&mut self, owners: &[&Pubkey]) {
        for o in owners {
            let ix = enter_ix(self.mint, **o);
            self.send(ix).ok();
        }
    }

    fn close_epoch(&mut self, epoch: u32) -> Tx {
        let ix = companion::close_epoch(self.cranker.pubkey(), self.mint, self.hook, epoch);
        self.send(ix)
    }

    fn claim_share(&mut self, epoch: u32, owner: &Pubkey) -> Tx {
        let ix = companion::claim_share(self.cranker.pubkey(), self.mint, self.hook, epoch, *owner);
        self.send(ix)
    }
}

// =============================================================================== jackpot

/// The pot grows from volume, then a fee claim. The volume's buyers sell everything again, so no
/// round they played can be won.
fn fund(c: &mut Coin) {
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    assert!(c.companion().pending_pot >= MIN_POT);
}
/// Buys `lamports` of the token from the pool for `recipient`'s holding (any address), `payer`
/// paying.
fn buy_for(c: &mut Coin, payer: &Keypair, recipient: &Pubkey, lamports: u64) -> Tx {
    let mint = c.mint;
    let keys = c.w.launch_keys(&mint);
    let slice =
        c.w.launch_base_slice(&mint, &payer.pubkey(), recipient, true);
    let ixs = [
        token::create_holding(payer.pubkey(), mint, *recipient),
        launch::swap_with_base_slice(&keys, payer.pubkey(), *recipient, 1, lamports, 0, slice),
    ];
    c.w.env.send_paid_by(&ixs, payer, &[])
}

/// Game F1: a round won by a program's address (on the curve, so a "wallet" to the hook, but it

// =============================================================================== round 3

/// Settles (forfeits) every round over before `b`'s; their buyers sold.
fn settle_until(c: &mut Coin, b: &Pubkey) {
    for _ in 0..6 {
        let (h, j, g) = (c.header(), c.jackpot(), c.game());
        let Some(r) = bordrless_game::settle_round(&h, &j, g.paid_buys, g.timer_secs, c.w.env.now)
        else {
            break;
        };
        if r.buyer == *b {
            break;
        }
        c.settle(&r.buyer).ok();
    }
}

/// `claim_fees` + `settle` in one v0 transaction with the 22-address table.
fn claim_and_settle(c: &mut Coin, buyer: &Pubkey, price: bool) -> Tx {
    let claim = companion::claim_fees_game(c.cranker.pubkey(), c.mint, c.hook);
    let settle = companion::settle(c.cranker.pubkey(), c.mint, c.hook, *buyer);
    let cranker = c.cranker.insecure_clone();
    let table =
        c.w.env
            .put_lookup_table(Pubkey::new_unique(), &table_22(&c.w));
    let mut ixs = vec![compute_unit_limit(400_000)];
    if price {
        ixs.push(compute_unit_price(1));
    }
    ixs.extend([claim, settle]);
    c.w.env
        .send_v0(&ixs, &cranker, &[], std::slice::from_ref(&table))
}

fn unclaimed(c: &Coin) -> u64 {
    let launch = launch::launch_address(&c.mint);
    c.w.env.holding(&c.w.sol, &launch)
}

/// A jackpot past `retirable_at` (no prize for 60 days, so dormant: threshold 0.1 SOL) whose pot is
/// below the threshold, whose last round (B's, B holding) is over, and whose launch holds unclaimed
/// fees that would bring the pot over it.
fn retirable_round_with_unclaimed_fees() -> (Coin, Keypair) {
    let mut c = Coin::new(GameKind::Jackpot);
    c.volume(1, 2 * SOL);
    c.claim_fees().ok();
    let p = c.companion().pending_pot;
    assert!(p > 0 && p < MIN_POT, "pot {p}");
    let launched = c.companion().launched_at;
    let retirable = c.game().retirable_at(launched);
    c.warp_to(retirable - i64::from(TIMER) - 10);
    c.volume(1, 2 * SOL);
    let b = c.buyer(SOL);
    c.warp_to(retirable + 1);
    settle_until(&mut c, &b.pubkey());
    let u = unclaimed(&c);
    let share = u * u64::from(POT_BPS) / 10_000;
    println!("pot {p}, unclaimed {u}, its pot share {share}, threshold {MIN_POT}");
    assert!(p + share >= MIN_POT && share < MIN_POT);
    (c, b)
}

/// R3-1: `retire` ignores the launch's unclaimed fees (`prize_due` compares only `pending_pot` with
/// the threshold), so a bare `retire_game` voids the very prize `FeesUnclaimed` refuses to void.
#[test]
fn poc_retire_voids_a_prize_that_fees_unclaimed_protects() {
    // Without the retire: the bare settle is refused, claim + settle pays B.
    let (mut c, b) = retirable_round_with_unclaimed_fees();
    refused(&c.settle(&b.pubkey()), CompanionError::FeesUnclaimed);
    let tx = claim_and_settle(&mut c, &b.pubkey(), true);
    tx.ok();
    println!(
        "claim_fees + settle (limit + price) v0: {} bytes, {} CU",
        tx.size,
        tx.cu()
    );
    assert!(tx.size <= PACKET);
    let paid: JackpotPaid = tx.event();
    assert_eq!(paid.winner, b.pubkey());

    // FIXED (round 3): a griefer's bare retire first is refused (`prize_due` counts what a fee claim
    // would bring), and claim + settle pays B.
    let (mut c, b) = retirable_round_with_unclaimed_fees();
    refused(&c.settle(&b.pubkey()), CompanionError::FeesUnclaimed);
    let ix = companion::retire_game(c.cranker.pubkey(), c.mint, c.hook);
    refused(&c.send(ix), CompanionError::DrawPending);
    let paid: JackpotPaid = claim_and_settle(&mut c, &b.pubkey(), true).event();
    assert_eq!(paid.winner, b.pubkey());
}

/// R3-2: a non-reserved, non-executable account that can't be credited: a legacy rent-paying
/// account (data, lamports below its rent-exempt minimum). Without SIMD-0392
/// (`relax_post_exec_min_balance_check`) the runtime refuses to credit it unless the credit makes
/// it exempt, so it jams `settle`, now only until `SETTLE_GRACE_SECS`; with SIMD-0392 (active on
/// mainnet per LiteSVM 0.17's feature list, and in this test world) it is grandfathered and paid.
fn rent_paying_winner(simd_0392: bool) {
    let mut c = Coin::new(GameKind::Jackpot);
    if !simd_0392 {
        let svm = std::mem::take(&mut c.w.env.svm);
        // The mainnet feature set (LiteSVM 0.17's list) less SIMD-0392.
        let mut features = litesvm::LiteSVM::mainnet_feature_set();
        features.deactivate(&Pubkey::from_str_const(
            "BY4JhHLahVzS9ynfDz4exzGPbVXhFmJvEyMWsXbDBqME", // relax_post_exec_min_balance_check
        ));
        c.w.env.svm = svm.with_feature_set(features);
    }
    fund(&mut c);
    let stale = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&stale).ok();
    let legacy = Keypair::new().pubkey();
    assert!(legacy.is_on_curve());
    let len = 1_000_000;
    c.w.env.put(
        legacy,
        solana_account::Account {
            lamports: 1_000_000,
            data: vec![0; len],
            owner: Pubkey::new_unique(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let attacker = c.w.wallet_with_sol(2 * SOL);
    buy_for(&mut c, &attacker, &legacy, SOL / 2).ok();
    assert_eq!(c.header().last_buyer, legacy);
    assert!(c.w.env.rent(len) > 1_000_000 + c.companion().pending_pot);
    let over = c.header().last_buy_at + i64::from(TIMER);
    c.warp_to(over);
    if simd_0392 {
        let paid: JackpotPaid = c.settle(&legacy).event();
        assert_eq!(paid.winner, legacy);
        return;
    }
    let tx = c.settle(&legacy);
    tx.expect_fail();
    println!(
        "settle of a rent-paying winner (no SIMD-0392): {:?}",
        tx.err()
    );
    // A later honest round can't be settled behind it (settle always answers the oldest).
    let h = c.buyer(SOL);
    c.claim_fees().ok();
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&h.pubkey()).expect_fail();
    c.warp_to(over + SETTLE_GRACE_SECS - 1);
    c.settle(&legacy).expect_fail();
    c.warp_to(over + SETTLE_GRACE_SECS);
    let f: JackpotForfeited = c.settle(&legacy).event();
    assert_eq!(f.buyer, legacy);
    // H's round, over about a timer after the jammed one, is paid if settled before it is stale.
    let paid: JackpotPaid = c.settle(&h.pubkey()).event();
    assert_eq!(paid.winner, h.pubkey());
}

#[test]
fn control_a_rent_paying_winner_is_paid_under_simd_0392() {
    rent_paying_winner(true);
}

#[test]
fn poc_a_rent_paying_winner_jams_settle_until_the_grace_without_simd_0392() {
    rent_paying_winner(false);
}

/// The stale forfeit is never early: one second before `SETTLE_GRACE_SECS` a holding winner is
/// paid.
#[test]
fn control_a_round_one_second_short_of_stale_is_paid() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let b = c.buyer(SOL);
    c.claim_fees().ok();
    settle_until(&mut c, &b.pubkey());
    c.warp_to(c.header().last_buy_at + i64::from(TIMER) + SETTLE_GRACE_SECS - 1);
    settle_until(&mut c, &b.pubkey());
    let paid: JackpotPaid = c.settle(&b.pubkey()).event();
    assert_eq!(paid.winner, b.pubkey());
}

/// `prize_due` skips a stale ended round but still waits for the current one behind it; settle
/// forfeits the stale one and pays the next.
#[test]
fn control_retire_waits_for_the_round_after_a_stale_one() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let launched = c.companion().launched_at;
    let retirable = c.game().retirable_at(launched);
    let j_at = retirable - SETTLE_GRACE_SECS - i64::from(TIMER) - 5;
    c.warp_to(j_at);
    let b1 = c.buyer(SOL);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER) + 1);
    let b2 = c.buyer(SOL);
    assert_eq!(c.jackpot().ended_buyer, b1.pubkey());
    c.claim_fees().ok();
    c.warp_to(retirable + 1);
    let ix = companion::retire_game(c.cranker.pubkey(), c.mint, c.hook);
    refused(&c.send(ix.clone()), CompanionError::DrawPending);
    // The fill's round (ended by B1's buy) is remembered too since the ring (log-p2-fix.md,
    // finding 2): it is the oldest, stale, and forfeited first.
    settle_until(&mut c, &b1.pubkey());
    let f: JackpotForfeited = c.settle(&b1.pubkey()).event();
    assert_eq!(f.buyer, b1.pubkey());
    let paid: JackpotPaid = c.settle(&b2.pubkey()).event();
    assert_eq!(paid.winner, b2.pubkey());
}

/// R3-3 (info): the `FeesUnclaimed` estimate counts only the launch's fee holding, not bridged SOL
/// already in the creator's holding above what is set aside, which `claim_fees` splits too
/// (`got = balance(creator) - set_aside`): with such a surplus a bare settle still voids a prize a
/// claim would fund.
#[test]
fn poc_a_creator_surplus_is_not_counted() {
    let run = |claim_first: bool| {
        let mut c = Coin::new(GameKind::Jackpot);
        c.volume(1, 2 * SOL);
        c.claim_fees().ok();
        let p = c.companion().pending_pot;
        assert!(p < MIN_POT);
        let donor = c.w.wallet_with_sol(SOL / 5);
        let sol = c.w.sol;
        let creator = companion::creator_address(&c.mint);
        let ix = token::transfer(
            donor.pubkey(),
            token::holding_address(&sol, &donor.pubkey()),
            token::holding_address(&sol, &creator),
            sol,
            None,
            vec![],
            SOL / 5,
        );
        c.w.env.send_paid_by(&[ix], &donor, &[]).ok();
        let b = c.buyer(SOL / 10);
        c.warp_to(c.header().last_buy_at + i64::from(TIMER));
        settle_until(&mut c, &b.pubkey());
        let share = unclaimed(&c) * u64::from(POT_BPS) / 10_000;
        assert!(p + share < MIN_POT, "the estimate alone says unfunded");
        if claim_first {
            let paid: JackpotPaid = claim_and_settle(&mut c, &b.pubkey(), false).event();
            assert_eq!(paid.winner, b.pubkey());
        } else {
            // FIXED (round 3): the creator's surplus is counted: a bare settle is refused.
            refused(&c.settle(&b.pubkey()), CompanionError::FeesUnclaimed);
        }
    };
    run(true);
    run(false);
}
