//! Independent audit, phase 2, game fairness and manipulation lens (jackpot and streak).
//!
//! No real (exploitable) issue was found on chain; this file holds `demo_*` tests that pin down
//! the behaviour behind the report's informational notes, and `control_*` tests for attacks that
//! are refused. All pass against the current build. Helpers (from `world` to `streak_coin`) are
//! copied from `audit_p2_game.rs` (themselves from `companion_kinds.rs`).
//!
//! Run: `CARGO_INCREMENTAL=0 cargo test -p bordrless-program-tests --test indep_p2_game`

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
/// first: the same buys leave A's round first in line, A is paid, then B.
#[test]
fn demo_an_ended_round_is_settleable_for_one_timer_after_it_ends() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    // Settle whatever the fill left, so A's round is the next one.
    let prev = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&prev).ok();

    let a = c.buyer(SOL);
    let a_round = c.jackpot().buys;
    let a_end = c.header().last_buy_at + i64::from(TIMER);
    // B buys the second A's round can be settled; nobody settles A.
    c.warp_to(a_end);
    let b = c.w.wallet_with_sol(2 * SOL);
    c.buy(&b, SOL).ok();
    assert_eq!(
        c.jackpot().ended_buys,
        a_round,
        "A's round is the ended one"
    );
    let b_round = c.jackpot().buys;
    // One timer after A's round ended, a qualifying buy ends B's round: A's is still remembered.
    c.warp_to(a_end + i64::from(TIMER));
    let d = c.w.wallet_with_sol(2 * SOL);
    c.buy(&d, SOL).ok();
    let j = c.jackpot();
    assert_eq!(j.ended_buys, b_round, "B's round is the newest ended one");
    assert_eq!(j.earlier[0].number, a_round, "A's round is remembered");
    c.claim_fees().ok();
    // A, who held all along, is paid first.
    let paid: JackpotPaid = c.settle(&a.pubkey()).event();
    assert_eq!(paid.winner, a.pubkey(), "the oldest round is paid first");
    assert_eq!(paid.round, a_round);
    // Then B's round.
    let paid: JackpotPaid = c.settle(&b.pubkey()).event();
    assert_eq!(paid.winner, b.pubkey());
    assert_eq!(paid.round, b_round);
}

/// The ring's bound, end to end: A's round, never settled, is still paid after 7 later rounds have
/// ended (the 8 remembered; the 8th later round's buy came 8 timers after A's ended); the 8th
/// later round ending drops it, and settle goes on with the oldest round still remembered.
#[test]
fn an_unsettled_round_is_payable_for_eight_timers_then_dropped() {
    for extra in [7u64, 8] {
        let mut c = Coin::new(GameKind::Jackpot);
        fund(&mut c);
        let prev = c.header().last_buyer;
        c.warp_to(c.header().last_buy_at + i64::from(TIMER));
        c.settle(&prev).ok();
        let a = c.buyer(SOL);
        let a_round = c.jackpot().buys;
        // `extra` more rounds end after A's, each bought the moment the last one ran out, and a
        // last buy ends the last of them (`extra` ended rounds after A's, the last buy at
        // A's end + `extra` timers).
        let mut buyers = Vec::new();
        for _ in 0..=extra {
            c.warp_to(c.header().last_buy_at + i64::from(TIMER));
            let x = c.w.wallet_with_sol(2 * SOL);
            c.buy(&x, SOL).ok();
            buyers.push(x);
        }
        c.claim_fees().ok();
        let first = c.settle(&a.pubkey());
        if extra == 7 {
            let paid: JackpotPaid = first.event();
            assert_eq!((paid.winner, paid.round), (a.pubkey(), a_round));
        } else {
            // Dropped: the oldest remembered round is the first buyer's after A.
            refused(&first, CompanionError::WrongHolding);
            let paid: JackpotPaid = c.settle(&buyers[0].pubkey()).event();
            assert_eq!(paid.winner, buyers[0].pubkey());
            assert_eq!(paid.round, a_round + 1);
        }
    }
}

/// CONTROL. A dust transfer into a holder before it is entered, and an early `enter`, never lock
/// it out of the epoch: the dust's receive registers what it held since the epoch began, and the
/// later `enter` writes nothing. Its share is the same as an untouched holder's of equal weight.
#[test]
fn control_dust_and_early_enter_never_lock_a_holder_out() {
    let mut c = Coin::new(GameKind::Streak);
    let a = c.buyer(2 * SOL);
    let b = c.w.wallet_with_sol(SOL);
    let held = c.balance(&a.pubkey());
    // B holds exactly what A holds.
    c.transfer(&a, &b.pubkey(), held / 2).ok();
    let e = c.epoch() + 1;
    c.warp_into(e, 60);
    // An attacker gives A one base unit (A's first write of e), then enters both.
    let x = c.buyer(SOL);
    c.transfer(&x, &a.pubkey(), 1).ok();
    c.enter(&[&a.pubkey(), &b.pubkey()]);
    let wa = c.slots(&a.pubkey()).current.weight;
    let wb = c.slots(&b.pubkey()).current.weight;
    assert_eq!(wa, held - held / 2);
    assert_eq!(wb, held / 2);
    c.volume(2, 20 * SOL);
    c.warp_into(e + 1, 30);
    c.claim_fees().ok();
    let _: EpochClosed = c.close_epoch(e).event();
    let sa: ShareClaimed = c.claim_share(e, &a.pubkey()).event();
    let sb: ShareClaimed = c.claim_share(e, &b.pubkey()).event();
    assert!(sa.share + sa.bounty >= sb.share + sb.bounty);
    assert!(sa.share + sa.bounty - (sb.share + sb.bounty) <= 1);
}
