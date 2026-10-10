//! Independent verification of the phase-2 fixes (log-p2-fix.md, finding 2): the jackpot's 8-round
//! ring under a full load. Helpers (from `world` to `buy_for`) copied verbatim from
//! `indep_p2_game.rs`.
//!
//! Run: `cargo test -p bordrless-program-tests --test verify_p2_ring -- --nocapture`

#![allow(dead_code, unused_imports, clippy::too_many_arguments)]

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
        GameKind::Lottery | GameKind::Strategy => unreachable!("phase 1's suites"),
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
    // Phase 3a: Studio's attestations, without which the companion takes no hook by its key.
    bordrless_program_tests::attest::attest_all(&mut w.env, &[JACKPOT, STREAK]);
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
        companion::create_game_v2_attested(*launcher, *mint, args, k, false),
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

/// The pot grows from volume, then a fee claim (copied from `companion_kinds.rs`).
fn fund(c: &mut Coin) {
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    assert!(c.companion().pending_pot >= MIN_POT);
}

/// A streak coin with two holders entered in the epoch after the launch's (copied).
fn streak_coin() -> (Coin, Keypair, Keypair, u32) {
    let mut c = Coin::new(GameKind::Streak);
    let a = c.buyer(5 * SOL);
    let b = c.buyer(SOL);
    let e = c.epoch() + 1;
    c.warp_into(e, 60);
    c.enter(&[&a.pubkey(), &b.pubkey()]);
    (c, a, b, e)
}

/// A buy of `lamports` paid by `payer`, delivered to `recipient`'s holding (copied).
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

/// FIXED (log-p2-fix.md, finding 2; was INFO, docs p2-b). Before the fix the hook remembered one
/// ended round: counted from when it could first be settled, a round was forgotten after ONE timer
/// (an interloper B buying the moment A's timer ran out took A's place once a buy came one timer
/// later). The jackpot header now remembers the last 8 ended rounds and `settle` pays the oldest
///
/// The round each settle closed (paid, forfeited or unfunded), and how.
fn settled_round(tx: &Tx) -> (u64, &'static str, Pubkey) {
    tx.ok();
    if let Some(e) = tx.events::<JackpotPaid>().first() {
        return (e.round, "paid", e.winner);
    }
    if let Some(e) = tx.events::<JackpotUnfunded>().first() {
        return (e.round, "unfunded", e.winner);
    }
    if let Some(e) = tx.events::<JackpotForfeited>().first() {
        return (e.round, "forfeited", e.buyer);
    }
    panic!("no settle event\n{}", tx.logs().join("\n"));
}

/// A full ring: 8 ended rounds kept, a 9th ending pushes the oldest out. Measures the CU of the
/// round-ending buy (the shift), of `retire_game` with a full ring (prize_due walks), and of each
/// settle; then settles every remembered round, oldest first, each once, in order, and checks no
/// round is ever paid twice (the current round after it is pushed into the ring included).
#[test]
fn verify_full_ring_order_once_and_cu() {
    let mut c = Coin::new(GameKind::Jackpot);
    // A big pot so that every round of the ring is funded.
    for _ in 0..4 {
        c.volume(3, 30 * SOL);
    }
    c.claim_fees().ok();
    let prev = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&prev).ok();
    let paid0 = c.game().paid_buys;

    // 10 rounds, each a single buy the moment the previous one ran out: rounds 1..=9 end (the
    // ring keeps the last 8 of them: 2..=9), round 10 is current.
    let mut rounds: Vec<(u64, Keypair)> = Vec::new();
    let mut buy_cu = Vec::new();
    for i in 0..10 {
        if i > 0 {
            c.warp_to(c.header().last_buy_at + i64::from(TIMER));
        }
        let x = c.w.wallet_with_sol(2 * SOL);
        let tx = c.buy(&x, SOL);
        tx.ok();
        buy_cu.push(tx.cu());
        rounds.push((c.jackpot().buys, x));
    }
    println!("round-ending buy CU (ring filling, then full): {buy_cu:?}");
    let j = c.jackpot();
    let ring: Vec<u64> = (0..8).map(|i| j.ended(i).unwrap().number).collect();
    println!("ring newest first: {ring:?}; current {}", j.buys);
    assert_eq!(ring[0], rounds[8].0);
    assert_eq!(ring[7], rounds[1].0, "the oldest kept is the second round");

    // retire with a full ring: waits (a round is due), CU measured.
    let tx = c.send(companion::retire_game(c.cranker.pubkey(), c.mint, c.hook));
    println!("retire_game with a full ring: CU {} (refused: {})", tx.cu(), tx.result.is_err());

    c.claim_fees().ok();
    // The first round was dropped (9 ended): the oldest remembered is round 2.
    let first = c.settle(&rounds[0].1.pubkey());
    refused(&first, CompanionError::WrongHolding);
    let mut seen = Vec::new();
    for (n, who) in rounds.iter().skip(1).take(8) {
        let tx = c.settle(&who.pubkey());
        let cu = tx.cu();
        let (round, how, winner) = settled_round(&tx);
        println!("settle round {round} ({how}): CU {cu}");
        assert_eq!(round, *n, "oldest first");
        assert_eq!(winner, who.pubkey());
        seen.push(round);
        // The same buyer again: never the same round twice.
        let again = c.settle(&who.pubkey());
        assert!(again.result.is_err(), "a round is settled once");
    }
    assert!(seen.windows(2).all(|w| w[0] < w[1]));
    assert!(c.game().paid_buys > paid0);
    // The current round is not over yet.
    let cur = rounds[9].1.pubkey();
    refused(&c.settle(&cur), CompanionError::NotDue);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let (round, _, _) = settled_round(&c.settle(&cur));
    assert_eq!(round, rounds[9].0);
    refused(&c.settle(&cur), CompanionError::NotDue);
    // A later buy pushes the settled current round into the ring: still never settled again.
    let y = c.w.wallet_with_sol(2 * SOL);
    c.buy(&y, SOL).ok();
    assert_eq!(c.jackpot().ended_buys, rounds[9].0);
    refused(&c.settle(&cur), CompanionError::NotDue);
    refused(&c.settle(&y.pubkey()), CompanionError::NotDue);
}

/// An attacker filling the ring as fast as it can (a minimal qualifying buy the second each round
/// runs out) cannot drop a round before 8 timers after it ended.
#[test]
fn verify_attacker_cannot_evict_before_eight_timers() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let prev = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&prev).ok();
    let a = c.buyer(SOL);
    let a_round = c.jackpot().buys;
    let a_end = c.header().last_buy_at + i64::from(TIMER);
    // Many buys at once within a round never push: only a buy after a whole timer does.
    let attacker = c.w.wallet_with_sol(50 * SOL);
    c.warp_to(a_end);
    let mut pushes = 0;
    while c.jackpot().earlier[6].number != a_round {
        for _ in 0..5 {
            c.buy(&attacker, SOL / 10).ok();
        }
        pushes += 1;
        c.warp_to(c.header().last_buy_at + i64::from(TIMER));
        assert!(pushes < 20);
    }
    // A is the oldest kept; the next push drops it, at the earliest now.
    let earliest_drop = c.w.env.now;
    println!(
        "A's round oldest in the ring; dropped at the earliest {} s after it ended ({} timers)",
        earliest_drop - a_end,
        (earliest_drop - a_end) / i64::from(TIMER)
    );
    assert!(earliest_drop - a_end >= 8 * i64::from(TIMER));
    c.claim_fees().ok();
    let paid: JackpotPaid = c.settle(&a.pubkey()).event();
    assert_eq!(paid.round, a_round);
}
