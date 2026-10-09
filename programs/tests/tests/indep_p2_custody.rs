//! Independent audit (phase 2, custody lens): PoCs and controls. Helpers copied from
//! audit_p2_custody.rs (itself from companion_kinds.rs).
//!
//! FIXED (log-p2-fix.md, finding 1): the companion reads a hook's kind header from the borrowed
//! state (`kinds::header_of` returns the account; `jackpot_of` parses the header's bytes in place),
//! never copying the state onto the heap. The PoCs below assert the safe outcome and pass; they
//! stay as regressions, at 40 KiB and at the largest account a hook can have (10 MiB).
#![allow(dead_code, unused_imports)]

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

/// A streak coin with two holders, A (big) and B (small), who bought in the launch's epoch and
/// are entered in the next one; the pot funded during it. Answers the coin, A, B and the epoch.
fn streak_coin() -> (Coin, Keypair, Keypair, u32) {
    let mut c = Coin::new(GameKind::Streak);
    let a = c.buyer(5 * SOL);
    let b = c.buyer(SOL);
    let e = c.epoch() + 1;
    c.warp_into(e, 60);
    c.enter(&[&a.pubkey(), &b.pubkey()]);
    (c, a, b, e)
}

fn retire_ix(c: &Coin) -> Instruction {
    companion::retire_game(c.cranker.pubkey(), c.mint, c.hook)
}

// =============================================================================== independent PoCs

/// The hook's state account grown to `len` bytes, its first bytes (the headers, the hook's own
/// fields) unchanged: what a Studio game hook that keeps more in its state (a history of buyers,
/// a leaderboard), growing it with `realloc`, has. The hook keeps working on it.
fn grow_state(c: &mut Coin, len: usize) {
    let key = bordrless_game::state_address(&c.hook, &c.mint).0;
    let mut acc = c.w.env.account(&key).unwrap();
    assert!(len > acc.data.len());
    acc.data.resize(len, 0);
    acc.lamports = c.w.env.rent(len);
    c.w.env.put(key, acc);
}

fn out_of_memory(tx: &Tx) -> bool {
    tx.logs().iter().any(|l| {
        l.to_lowercase().contains("out of memory") || l.contains("memory allocation failed")
    })
}

/// The step failed by aborting out of memory (the 32 KiB heap), and nothing moved.
#[track_caller]
fn assert_not_oom(tx: &Tx, step: &str) {
    assert!(
        !out_of_memory(tx),
        "{step} aborts out of memory: the companion copies the whole hook state onto the heap \
         (kinds.rs header_of: `info.try_borrow_data()?.to_vec()`)\n{}",
        tx.logs().join("\n")
    );
}

/// FINDING (low; the pot's liveness): `kinds::header_of` copies the hook's WHOLE state account
/// onto the 32 KiB heap (`info.try_borrow_data()?.to_vec()`) to parse an 80-byte kind header at
/// offset 120. A game hook whose state is bigger than the heap left makes every `settle` abort
/// (from about 21 KiB: settle already peaks at 11.6 KiB), and `close_epoch` and `retire_game`
/// (from about 28 KiB; `prize_due` -> `header_of`). An abort is not the `Err` that `prize_due`'s
/// "a state the hook broke owes nothing" catches, so the round's winner is never paid and the pot
/// is never retired: only a protocol block (which returns before `prize_due`) moves it.
/// `create_game_v2` and the launch only borrow the data, so such a hook is taken, and a Studio
/// hook that grows its state later (`realloc`, 10 KiB an instruction: a buyer history, a
/// leaderboard) bricks a running game. The hook keeps working for every trade.
///
/// Asserts the safe outcome: FAILS until `header_of` stops copying the account.
#[test]
fn indep_jackpot_large_state_bricks_settle() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let last = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&last).ok();
    // The hook's state grows to 40 KiB; the hook still runs every buy.
    grow_state(&mut c, 40 * 1024);
    let alice = c.buyer(SOL);
    assert_eq!(c.header().last_buyer, alice.pubkey(), "the hook works");
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let pot = c.companion().pending_pot;
    assert!(pot >= MIN_POT, "settle would pay Alice");
    let tx = c.settle(&alice.pubkey());
    assert_not_oom(&tx, "settle");
    let paid: JackpotPaid = tx.event();
    assert_eq!(paid.winner, alice.pubkey());
}

/// FINDING (same root): with the state at 40 KiB, nothing ever moves the pot again but a block:
/// settle aborts (even once the round is 30 days stale) and so does `retire_game` once due.
/// Asserts the safe outcome (a stale round forfeits, the pot retires): FAILS today.
#[test]
fn indep_jackpot_large_state_pot_never_retires() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let last = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&last).ok();
    grow_state(&mut c, 40 * 1024);
    let alice = c.buyer(SOL);
    let pot = c.companion().pending_pot;
    let retirable = c.game().retirable_at(c.companion().launched_at);
    c.warp_to(retirable.max(c.header().last_buy_at + i64::from(TIMER) + SETTLE_GRACE_SECS) + 1);
    // A stale round owes nothing: retire moves the pot, and a later settle forfeits the round.
    let tx = c.send(retire_ix(&c));
    assert_not_oom(&tx, "retire_game");
    tx.ok();
    let retired: PotRetired = tx.event();
    assert_eq!(retired.lamports, pot);
    let tx = c.settle(&alice.pubkey());
    assert_not_oom(&tx, "settle (stale round)");
    let forfeited: JackpotForfeited = tx.event();
    assert_eq!(forfeited.reason, ForfeitReason::Stale);
}

/// CONTROL: the same scene at a 4 KiB state pays Alice; at 40 KiB Bob is paid too (before the fix
/// settle aborted out of memory there); and under a block a 40 KiB state's pot reaches the buyback
/// (the block returns before the state is read).
#[test]
fn indep_control_small_state_settles_and_block_moves_a_large_states_pot() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let last = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&last).ok();
    grow_state(&mut c, 4 * 1024);
    let alice = c.buyer(SOL);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let tx = c.settle(&alice.pubkey());
    tx.ok();
    let paid: JackpotPaid = tx.event();
    assert_eq!(paid.winner, alice.pubkey());
    // At 40 KiB: settle pays Bob (it aborted out of memory before the fix).
    grow_state(&mut c, 40 * 1024);
    let bob = c.buyer(SOL);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let tx = c.settle(&bob.pubkey());
    assert_not_oom(&tx, "settle");
    let paid: JackpotPaid = tx.event();
    assert_eq!(paid.winner, bob.pubkey());
    // A block still moves what is left, without reading the state.
    let carol = c.buyer(SOL);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let _ = carol;
    let pot = c.companion().pending_pot;
    c.set_status(false, DEFAULT_POT_CAP, true).ok();
    let tx = c.send(retire_ix(&c));
    tx.ok();
    let moved: PotToBuyback = tx.event();
    assert_eq!(moved.lamports, pot);
}

/// FINDING (same root): a streak whose state outgrows the heap can never close an epoch, so no
/// holder is ever paid, and `retire_game` aborts while the pot can pay.
/// Asserts the safe outcome: FAILS today.
#[test]
fn indep_streak_large_state_bricks_close_epoch_and_retire() {
    let (mut c, a, _b, e) = streak_coin();
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    grow_state(&mut c, 40 * 1024);
    c.enter(&[&a.pubkey()]);
    c.warp_into(e + 1, 30);
    let tx = c.close_epoch(e);
    assert_not_oom(&tx, "close_epoch");
    tx.ok();
    c.claim_share(e, &a.pubkey()).ok();
}

/// REGRESSION (was MEASURE, ignored): the state size from which `settle` and `close_epoch` aborted
/// (before the fix: settle from 22 KiB, close_epoch from 28 KiB). Now neither aborts at any size a
/// hook's state can have, up to the runtime's 10 MiB account limit.
#[test]
fn indep_measure_state_size_thresholds() {
    for kb in [16usize, 22, 28, 64, 1024, 10 * 1024] {
        let mut c = Coin::new(GameKind::Jackpot);
        fund(&mut c);
        let last = c.header().last_buyer;
        c.warp_to(c.header().last_buy_at + i64::from(TIMER));
        c.settle(&last).ok();
        grow_state(&mut c, kb * 1024);
        let alice = c.buyer(SOL);
        c.warp_to(c.header().last_buy_at + i64::from(TIMER));
        let settle_oom = out_of_memory(&c.settle(&alice.pubkey()));
        let (mut s, a, _b, e) = streak_coin();
        s.volume(2, 20 * SOL);
        s.claim_fees().ok();
        grow_state(&mut s, kb * 1024);
        s.enter(&[&a.pubkey()]);
        s.warp_into(e + 1, 30);
        let close = s.close_epoch(e);
        let close_oom = out_of_memory(&close);
        println!("state {kb} KiB: settle oom {settle_oom}, close_epoch oom {close_oom}");
        assert!(!settle_oom && !close_oom, "state {kb} KiB");
        close.ok();
    }
}
