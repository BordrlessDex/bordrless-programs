//! Audit, phase 2, game-manipulation lens, round 1: the last-buyer jackpot and the diamond-hands
//! streak (`bordrless_game::{jackpot, streak}`, the Studio starters, `instructions::kinds`).
//!
//! Tests named `poc_*` PASS while the issue exists: they assert the exploit's outcome (and say in
//! their doc what a fixed build should do instead). `control_*` tests check an attack that is
//! refused. `pure_*` tests drive the crate's rules directly (no SVM) with randomised operations
//! and check the invariants the docs promise. `demo_*` tests show an inherent design trade-off.
//!
//! The helpers (from `world` to `fund`) are copied from `companion_kinds.rs`.

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

// =============================================================================== audit helpers

/// A buy of `lamports` of the token paid by `payer` and delivered to `recipient`'s holding
/// (created first, by the payer): what any wallet can do for any address.
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

/// Settles every round that is over and settleable now (forfeits and payments), stopping at the
/// first refusal.
fn settle_all(c: &mut Coin) {
    for _ in 0..8 {
        let s = c.jackpot();
        let h = c.header();
        let g = c.game();
        let Some(r) = bordrless_game::settle_round(&h, &s, g.paid_buys, g.timer_secs, c.w.env.now)
        else {
            return;
        };
        if c.settle(&r.buyer).result.is_err() {
            return;
        }
    }
}

// =============================================================================== jackpot PoCs

/// FINDING 1. A round won by an owner that can't be credited SOL (here a deployed program's id:
/// on the curve, so `eligible`, and executable) can never be settled: `settle` finds the buyer
/// holds, pays it, and the runtime refuses the credit (`ExternalAccountLamportSpend`, as phase 1
/// measured for the lottery). Unlike the lottery (whose attempts expire), a jackpot round has no
/// way out: `settle_round` always answers the oldest open round, so every later round is stuck
/// behind it. Here the attacker buys the minimum for `half_life`'s id and lets the timer run out;
/// the launch then graduates (the curve's fill buys move the jammed round to `ended_*`, the
/// crossing buyer L takes the current round). L held through its timer and should be paid, but no
/// qualifying buy can ever come again, so neither round is ever settled: the jackpot is dead and its
/// pot can only be retired. A fixed build forfeits (or skips) a round whose winner can't be paid.
#[test]
fn poc_jackpot_unpayable_winner_jams_settle_for_ever() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let stale = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&stale).ok();

    let program = half_life::ID;
    assert!(
        program.is_on_curve(),
        "a keypair-made program id is eligible"
    );
    assert!(c.w.env.account(&program).unwrap().executable);
    let attacker = c.w.wallet_with_sol(SOL);
    buy_for(&mut c, &attacker, &program, SOL / 2).ok();
    let bought = c.balance(&program);
    assert!(bought >= studio_jackpot::MIN_TOKENS);
    let jammed = c.jackpot().buys;
    assert_eq!(c.header().last_buyer, program);
    assert_eq!(jackpot_mark(&c.slots(&program)), jammed);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    // FIXED (round 1): a buyer that can't be paid (an executable account) forfeits the round, so
    // nothing jams; the rounds after it settle as usual.
    let tx = c.settle(&program);
    tx.ok();
    let f: JackpotForfeited = tx.event();
    assert_eq!((f.round, f.buyer), (jammed, program));
    assert_eq!(c.game().paid_buys, jammed);
    let mint = c.mint;
    let (_, tx) = c.w.graduate_launch(&mint);
    tx.ok();
    let l = c.header().last_buyer;
    assert_ne!(l, program);
    c.claim_fees().ok();
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    // Older rounds the curve's fill made are settled first, then the crossing buyer L is paid.
    settle_all(&mut c);
    assert_eq!(c.game().paid_buys, c.jackpot().buys);
    assert!(c.game().prizes_paid > 0);
}

/// FINDING 1 (before graduation). The same jam delays every honest round: B holds through its
/// timer after the jammed round, and is paid only if somebody else buys after B's own timer ran
/// out (which moves B's round over the jammed one). With no such buy B is never paid.
#[test]
fn poc_jackpot_unpayable_winner_blocks_the_next_honest_round() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let stale = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&stale).ok();
    let program = half_life::ID;
    let attacker = c.w.wallet_with_sol(SOL);
    buy_for(&mut c, &attacker, &program, SOL / 2).ok();
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    // FIXED (round 1): the program's round is forfeited; B, buying after it, is paid once its own
    // timer runs out, with no later buy needed.
    let _: JackpotForfeited = c.settle(&program).event();
    let b = c.buyer(SOL);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let paid: JackpotPaid = c.settle(&b.pubkey()).event();
    assert_eq!(paid.winner, b.pubkey());
}

/// FINDING 2. The jackpot header remembers one ended round. While the oldest open round can't be
/// settled (here: its pot is below the minimum, `PotTooSmall`, which the docs say keeps the round
/// open "until the pot holds the minimum"), a second round that ends overwrites it, and the first
/// winner is dropped without a forfeit. An attacker C who sees B's round waiting on the pot buys
/// after B's timer, holds through its own timer, and buys once more: C's first round replaces B's,
/// and C is paid when the pot fills; B, who held all along, gets nothing.
#[test]
fn poc_jackpot_round_waiting_for_its_pot_is_overwritten() {
    let mut c = Coin::new(GameKind::Jackpot);
    let b = c.buyer(SOL);
    let nb = c.jackpot().buys;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    assert!(c.companion().pending_pot < MIN_POT);
    // FIXED (round 1): settle closes B's round at once (unfunded: the pot is below its minimum),
    // so no later round can take its place while it waits.
    let tx = c.settle(&b.pubkey());
    tx.ok();
    let u: JackpotUnfunded = tx.event();
    assert_eq!((u.round, u.winner), (nb, b.pubkey()));
    let att = c.buyer(SOL);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.buy(&att, SOL).ok();
    let j = c.jackpot();
    assert_eq!((j.ended_buyer, j.ended_buys), (att.pubkey(), nb + 1));
    assert_eq!(
        c.game().paid_buys,
        nb,
        "B's round was closed, not overtaken"
    );
}

// =============================================================================== jackpot controls

/// A delegate's transfer out of the last buyer's holding, or a burn from it, clears its mark: the
/// round is forfeited (the buyer did not hold what it bought).
#[test]
fn control_jackpot_delegate_send_and_burn_forfeit_the_round() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let stale = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&stale).ok();
    let mint = c.mint;
    let custom = c.w.custom_hook_accounts(&c.hook, &mint);
    // A delegate moves one base unit out of B's holding.
    let b = c.buyer(SOL);
    let d = c.w.wallet_with_sol(SOL);
    let ixs = [
        token::approve(
            b.pubkey(),
            token::holding_address(&mint, &b.pubkey()),
            d.pubkey(),
            1,
        ),
        token::create_holding(b.pubkey(), mint, d.pubkey()),
    ];
    c.w.env.send_paid_by(&ixs, &b, &[]).ok();
    assert_ne!(jackpot_mark(&c.slots(&b.pubkey())), 0);
    let ix = token::transfer_with(
        d.pubkey(),
        token::holding_address(&mint, &b.pubkey()),
        token::holding_address(&mint, &d.pubkey()),
        mint,
        Some(Hook::of(c.hook)),
        custom.extras.clone(),
        1,
    );
    c.w.env.send_paid_by(&[ix], &d, &[]).ok();
    assert_eq!(jackpot_mark(&c.slots(&b.pubkey())), 0);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let tx = c.settle(&b.pubkey());
    tx.ok();
    let _: JackpotForfeited = tx.event();
    // A burn of one base unit does the same.
    let e = c.buyer(SOL);
    let ix = token::burn_with(
        e.pubkey(),
        token::holding_address(&mint, &e.pubkey()),
        mint,
        Some(Hook::of(c.hook)),
        custom.extras.clone(),
        1,
    );
    c.w.env.send_paid_by(&[ix], &e, &[]).ok();
    assert_eq!(jackpot_mark(&c.slots(&e.pubkey())), 0);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let tx = c.settle(&e.pubkey());
    tx.ok();
    let _: JackpotForfeited = tx.event();
}

/// The last buyer of a round that has ended sells a little and buys again: its mark moves to the
/// new buy, so the ended round (bought before the sale) is forfeited, and only the new round can
/// pay it. Receiving tokens from others never clears or moves a mark.
#[test]
fn control_jackpot_sell_and_rebuy_never_wins_the_old_round() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let stale = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&stale).ok();
    let b = c.buyer(SOL);
    let nb = c.jackpot().buys;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let mint = c.mint;
    c.w.sell(&b, &mint, 1_000_000).ok();
    c.buy(&b, SOL).ok();
    let j = c.jackpot();
    assert_eq!(
        (j.ended_buyer, j.ended_buys, j.buys),
        (b.pubkey(), nb, nb + 1)
    );
    assert_eq!(jackpot_mark(&c.slots(&b.pubkey())), nb + 1);
    // A friend sends B tokens: the mark stays.
    let f = c.buyer(SOL);
    let amount = c.balance(&f.pubkey()) / 2;
    c.transfer(&f, &b.pubkey(), amount).ok();
    assert_eq!(jackpot_mark(&c.slots(&b.pubkey())), nb + 1);
    let tx = c.settle(&b.pubkey());
    tx.ok();
    let ev: JackpotForfeited = tx.event();
    assert_eq!(ev.round, nb);
    // f's buy came after B's second: f is the current round's buyer, B's second round is gone.
    assert_eq!(c.header().last_buyer, f.pubkey());
}

/// Buys for a wallet off the curve (a PDA) or to the companion's creator address restart nothing;
/// a self-transfer is refused by the token program (so it can't double-apply a hook's rules).
#[test]
fn control_jackpot_pda_recipient_and_self_transfer() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let j = c.jackpot();
    let h = c.header();
    let payer = c.w.wallet_with_sol(SOL);
    let pda = Pubkey::find_program_address(&[b"x"], &JACKPOT).0;
    buy_for(&mut c, &payer, &pda, SOL / 2).ok();
    assert!(c.balance(&pda) >= studio_jackpot::MIN_TOKENS);
    assert_eq!((c.jackpot(), c.header()), (j, h));
    let b = c.buyer(SOL);
    let held = c.balance(&b.pubkey());
    let mint = c.mint;
    let custom = c.w.custom_hook_accounts(&c.hook, &mint);
    let ix = token::transfer_with(
        b.pubkey(),
        token::holding_address(&mint, &b.pubkey()),
        token::holding_address(&mint, &b.pubkey()),
        mint,
        Some(Hook::of(c.hook)),
        custom.extras,
        held,
    );
    c.w.env.send_paid_by(&[ix], &b, &[]).expect_fail();
}

/// DEMO (design): a buy and a sell in one transaction is a qualifying buy (it restarts the timer
/// and takes the round) that its buyer forfeits at once: for the fees of a round trip of
/// `min_tokens`, anyone can keep a round from being won, and the round is forfeited (the pot
/// stays). This is the last-buyer game's nature, priced by `min_tokens` and the fees.
#[test]
fn demo_jackpot_buy_and_sell_in_one_tx_takes_the_round_and_forfeits_it() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let stale = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&stale).ok();
    let b = c.buyer(SOL);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER) - 5);
    let g = c.w.wallet_with_sol(SOL);
    let mint = c.mint;
    let ixs = [
        token::create_holding(g.pubkey(), mint, g.pubkey()),
        c.w.launch_swap_ix(&g.pubkey(), &mint, 1, SOL / 2, 0),
        c.w.launch_swap_ix(&g.pubkey(), &mint, 0, 1_000_000, 0),
    ];
    c.w.env.send_paid_by(&ixs, &g, &[]).ok();
    assert_eq!(c.header().last_buyer, g.pubkey());
    assert_eq!(jackpot_mark(&c.slots(&g.pubkey())), 0);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let tx = c.settle(&g.pubkey());
    tx.ok();
    let _: JackpotForfeited = tx.event();
    refused(&c.settle(&b.pubkey()), CompanionError::NotDue);
}

// =============================================================================== streak controls

/// A holder can't double-count by sending to itself (refused), and a delegate's send forfeits the
/// owner's weight like the owner's own send (the total loses it exactly).
#[test]
fn control_streak_self_transfer_refused_and_delegate_send_forfeits() {
    let (mut c, a, b, e) = streak_coin();
    let mint = c.mint;
    let custom = c.w.custom_hook_accounts(&c.hook, &mint);
    let held = c.balance(&a.pubkey());
    let ix = token::transfer_with(
        a.pubkey(),
        token::holding_address(&mint, &a.pubkey()),
        token::holding_address(&mint, &a.pubkey()),
        mint,
        Some(Hook::of(c.hook)),
        custom.extras.clone(),
        held,
    );
    c.w.env.send_paid_by(&[ix], &a, &[]).expect_fail();
    let wb = c.balance(&b.pubkey());
    let total = c.header().total;
    let d = c.w.wallet_with_sol(SOL);
    let ixs = [
        token::approve(
            a.pubkey(),
            token::holding_address(&mint, &a.pubkey()),
            d.pubkey(),
            1,
        ),
        token::create_holding(a.pubkey(), mint, d.pubkey()),
    ];
    c.w.env.send_paid_by(&ixs, &a, &[]).ok();
    let ix = token::transfer_with(
        d.pubkey(),
        token::holding_address(&mint, &a.pubkey()),
        token::holding_address(&mint, &d.pubkey()),
        mint,
        Some(Hook::of(c.hook)),
        custom.extras,
        1,
    );
    c.w.env.send_paid_by(&[ix], &d, &[]).ok();
    assert_eq!(c.header().total, total - held);
    assert_eq!(c.header().total, wb);
    assert!(c.slots(&a.pubkey()).range_in(e).is_none());
}

/// Claims of epoch e during e + 1: a holder who receives (or buys) in e + 1 keeps its share of e;
/// one who claims, then sells in the same epoch, keeps what it claimed but can't claim twice; a
/// receipt made for one epoch never blocks the next; nothing of e can be claimed in e + 2.
#[test]
fn control_streak_claims_across_epochs() {
    let (mut c, a, b, e) = streak_coin();
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    c.warp_into(e + 1, 30);
    c.close_epoch(e).ok();
    // A buys more in e + 1 (a receive): its share of e stays.
    c.buy(&a, SOL).ok();
    let tx = c.claim_share(e, &a.pubkey());
    tx.ok();
    // B claims, then sells: no second claim (receipt), and its e + 1 weight is gone.
    c.claim_share(e, &b.pubkey()).ok();
    let mint = c.mint;
    c.w.sell(&b, &mint, 1_000_000).ok();
    c.w.env.warp(1);
    c.claim_share(e, &b.pubkey()).expect_fail();
    // e + 1: A was written in e + 1 by its buy, registering what it held before the buy.
    let wa1 = c.slots(&a.pubkey()).range_in(e + 1).unwrap().weight;
    c.enter(&[&b.pubkey()]);
    assert!(c.slots(&b.pubkey()).range_in(e + 1).is_none());
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    c.warp_into(e + 2, 30);
    refused(
        &c.claim_share(e, &Keypair::new().pubkey()),
        CompanionError::DrawLate,
    );
    c.close_epoch(e + 1).ok();
    assert_eq!(c.game().total, wa1);
    let tx = c.claim_share(e + 1, &a.pubkey());
    tx.ok();
    let ev: ShareClaimed = tx.event();
    assert_eq!(ev.weight, wa1);
}

// =============================================================================== pure rules

/// A tiny deterministic generator (xorshift64*).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
}

struct PureHolder {
    owner: Pubkey,
    balance: u64,
    data: [u8; 64],
    last_send: i64,
    /// Lowest balance since the current epoch began (for the streak).
    min_bal: u64,
    sent_in: std::collections::BTreeSet<u32>,
    /// Lowest balance through each finished epoch, and whether it sent in it.
    epoch_min: std::collections::BTreeMap<u32, u64>,
}

fn holders(n: usize, _rng: &mut Rng) -> Vec<PureHolder> {
    (0..n)
        .map(|_| PureHolder {
            owner: Keypair::new().pubkey(),
            balance: 0,
            data: [0; 64],
            last_send: 0,
            min_bal: 0,
            sent_in: Default::default(),
            epoch_min: Default::default(),
        })
        .collect()
}

/// The streak's rules under random transfers, buys, sells, burns, `enter`s and clock moves, for
/// several `min_streak_secs` / `min_weight`: the header's total is always the exact sum of the
/// live weights of its epoch; no weight is above the balance held since the epoch began; a holder
/// that sent during an epoch has no weight in it; an ended epoch's claimable weights sum to at most
/// its final total, each at most the holding's lowest balance through it, and none belongs to a
/// holder that sent during the epoch or since (before claiming).
#[test]
fn pure_streak_rules_keep_totals_exact_and_weights_bounded() {
    use bordrless_game::{
        round_end, streak_on_enter, streak_on_receive, streak_on_send, streak_weight, StreakHeader,
    };
    const E: u32 = 3_600;
    let mint = Pubkey::new_unique();
    let mut checked_claims = 0u64;
    for (seed, min_streak, min_weight) in [
        (1u64, E, 1u64),
        (2, 0, 1),
        (3, E / 2, 1_000),
        (4, 2 * E, 1),
        (5, E, 500_000),
        (6, 1, 1),
    ] {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ seed);
        let mut now: i64 = 1_800_000_000;
        let mut header = GameHeader::new(mint, E, now);
        let streak = StreakHeader::new(min_streak, min_weight);
        let mut hs = holders(6, &mut rng);
        let mut epoch = round_of(now, E);
        // Final totals of ended epochs.
        let mut finals: std::collections::BTreeMap<u32, u64> = Default::default();
        for _step in 0..6_000 {
            // Clock.
            if rng.below(4) == 0 {
                now += rng.below(i64::from(E) as u64 / 2) as i64;
            }
            let e_now = round_of(now, E);
            if e_now != epoch {
                for h in hs.iter_mut() {
                    h.epoch_min.insert(epoch, h.min_bal);
                    // Epochs skipped entirely: held the balance throughout.
                    for k in epoch + 1..e_now {
                        h.epoch_min.insert(k, h.balance);
                    }
                    h.min_bal = h.balance;
                }
                // What the header will say of the ended epochs once it rolls.
                let mut probe = header;
                probe.roll(now);
                for k in epoch..e_now {
                    if let Some(t) = probe.total_of(k) {
                        finals.insert(k, t);
                    }
                }
                epoch = e_now;
            }
            let n = hs.len();
            let i = rng.below(n as u64) as usize;
            let mut j = rng.below(n as u64) as usize;
            if j == i {
                j = (j + 1) % n;
            }
            match rng.below(7) {
                // Transfer i -> j.
                0 | 1 => {
                    let amount = rng.below(hs[i].balance + 1);
                    if amount == 0 {
                        continue;
                    }
                    let left = hs[i].balance - amount;
                    let mut s = Slots::decode(&hs[i].data);
                    streak_on_send(&mut header, &mut s, left, now);
                    hs[i].data = s.encode();
                    hs[i].balance = left;
                    hs[i].last_send = now;
                    hs[i].sent_in.insert(e_now);
                    hs[i].min_bal = hs[i].min_bal.min(left);
                    let before = hs[j].balance;
                    let mut d = Slots::decode(&hs[j].data);
                    streak_on_receive(&mut header, &streak, &mut d, before, before + amount, now);
                    hs[j].data = d.encode();
                    hs[j].balance = before + amount;
                }
                // Buy from the pool (excluded source).
                2 => {
                    let amount = 1 + rng.below(800_000);
                    let before = hs[j].balance;
                    let mut d = Slots::decode(&hs[j].data);
                    streak_on_receive(&mut header, &streak, &mut d, before, before + amount, now);
                    hs[j].data = d.encode();
                    hs[j].balance = before + amount;
                }
                // Sell to the pool, or burn (both a send).
                3 => {
                    let amount = rng.below(hs[i].balance + 1);
                    if amount == 0 {
                        continue;
                    }
                    let left = hs[i].balance - amount;
                    let mut s = Slots::decode(&hs[i].data);
                    streak_on_send(&mut header, &mut s, left, now);
                    hs[i].data = s.encode();
                    hs[i].balance = left;
                    hs[i].last_send = now;
                    hs[i].sent_in.insert(e_now);
                    hs[i].min_bal = hs[i].min_bal.min(left);
                }
                // enter, by anyone.
                _ => {
                    let mut s = Slots::decode(&hs[i].data);
                    if streak_on_enter(&mut header, &streak, &mut s, hs[i].balance, now) {
                        hs[i].data = s.encode();
                    }
                }
            }
            // Invariants.
            let mut sum = 0u64;
            for h in &hs {
                let s = Slots::decode(&h.data);
                if s.current.is_live() && s.current.round == header.round {
                    sum += s.current.weight;
                }
                if let Some(r) = s.range_in(e_now) {
                    assert!(r.weight <= h.balance, "weight above balance");
                    assert!(
                        r.weight <= h.min_bal,
                        "weight above what was held all epoch"
                    );
                    assert!(!h.sent_in.contains(&e_now), "a sender kept its weight");
                    assert!(r.weight >= streak.floor());
                    assert!(
                        h.last_send <= round_end(e_now, E) - i64::from(min_streak),
                        "a holder that sent within min_streak of the epoch's end has weight"
                    );
                }
            }
            if header.round == e_now {
                assert_eq!(
                    header.total, sum,
                    "the total is the exact sum of live weights"
                );
            }
            // Claims of the epoch before, during this one.
            if e_now > 0 {
                let ended = e_now - 1;
                if let Some(&total) = finals.get(&ended) {
                    let mut claim_sum = 0u64;
                    for h in &hs {
                        let w = streak_weight(&h.data, ended, E, min_streak, min_weight, h.balance);
                        if w > 0 {
                            checked_claims += 1;
                            assert!(!h.sent_in.contains(&ended) && !h.sent_in.contains(&e_now));
                            assert!(w <= *h.epoch_min.get(&ended).unwrap_or(&0));
                            claim_sum += w;
                        }
                    }
                    assert!(claim_sum <= total, "claims above the epoch's total");
                }
            }
        }
    }
    assert!(
        checked_claims > 1_000,
        "the walk reached claims: {checked_claims}"
    );
}

/// The streak's boundary: with `min_streak_secs` = the epoch, a holding that sent at the last
/// second of e - 1 qualifies for e; one that sent (or first received) at e's first second does not.
#[test]
fn pure_streak_min_streak_boundary() {
    use bordrless_game::{round_start, streak_qualifies};
    const E: u32 = 3_600;
    let e = 500_000u32;
    let start = round_start(e, E);
    assert!(streak_qualifies(start - 1, e, E, E));
    assert!(
        streak_qualifies(start, e, E, E),
        "since = the epoch's start"
    );
    assert!(!streak_qualifies(start + 1, e, E, E));
    assert!(!streak_qualifies(0, e, E, E), "never received");
    // since = start qualifies, but no write at `start` can register weight in e: a send there
    // writes e with no weight, a first receive registers what was held before (nothing).
    let mint = Pubkey::new_unique();
    let mut header = GameHeader::new(mint, E, start - 10);
    let streak = bordrless_game::StreakHeader::new(E, 1);
    let mut s = Slots::default();
    bordrless_game::streak_on_receive(&mut header, &streak, &mut s, 0, 1_000, start);
    assert_eq!(s.since, start);
    assert!(s.range_in(e).is_none());
    assert!(!bordrless_game::streak_on_enter(
        &mut header,
        &streak,
        &mut s,
        1_000,
        start + 100
    ));
}

/// The jackpot's rules and the companion's settle logic under random buys (of random sizes, some
/// below the minimum), wallet transfers, sells, burns and settles at random times: a round is paid
/// at most once, rounds are settled in increasing order, and every payment goes to a round's last
/// qualifying buyer who sent nothing since that buy. Also counts the rounds that were won (timer
/// ran out with the buyer still holding) but never settled, because the header forgot them
/// (finding 2) when settles lag.
#[test]
fn pure_jackpot_settle_pays_only_holders_and_never_twice() {
    use bordrless_game::{
        jackpot_on_buy, jackpot_on_receive, jackpot_on_send, jackpot_winner_holds, qualifying_buy,
        settle_round, JackpotHeader, LaunchView,
    };
    const T: u32 = 600;
    const MIN: u64 = 1_000;
    let mint = Pubkey::new_unique();
    let pool = Pubkey::find_program_address(&[b"pool"], &JACKPOT).0;
    let launch = LaunchView {
        pool,
        on_curve: true,
    };
    let excluded = [pool];
    let mut total_lost = 0u64;
    for (seed, settle_odds) in [(11u64, 2u64), (12, 6), (13, 30)] {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03 ^ seed);
        let mut now: i64 = 1_800_000_000;
        let mut header = GameHeader::new(mint, 0, now);
        let mut jp = JackpotHeader::new(T, MIN);
        let mut hs = holders(5, &mut rng);
        let mut seq = 0u64;
        // n -> (buyer index, buy seq, at)
        let mut buys: std::collections::BTreeMap<u64, (usize, u64, i64)> = Default::default();
        let mut last_send_seq = vec![0u64; hs.len()];
        let mut paid = 0u64;
        let mut settled: std::collections::BTreeSet<u64> = Default::default();
        let mut won_holding: std::collections::BTreeSet<u64> = Default::default();
        for _ in 0..8_000 {
            seq += 1;
            if rng.below(3) == 0 {
                now += rng.below(u64::from(T) * 2) as i64;
            }
            let n = hs.len();
            let i = rng.below(n as u64) as usize;
            let mut j = rng.below(n as u64) as usize;
            if j == i {
                j = (j + 1) % n;
            }
            match rng.below(6) {
                0 | 1 => {
                    // A buy from the pool.
                    let amount = 1 + rng.below(3 * MIN);
                    let after = hs[j].balance + amount;
                    let mut d = Slots::decode(&hs[j].data);
                    jackpot_on_receive(&mut d, after, now);
                    let owner = hs[j].owner;
                    // Before it lands: the current round, if over, is won if its buyer holds.
                    if jp.buys > 0
                        && bordrless_game::timer_over(header.last_buy_at, T, now)
                        && qualifying_buy(Some(&launch), &pool, &owner, amount, MIN, &excluded)
                    {
                        let (b, s, _) = buys[&jp.buys];
                        if last_send_seq[b] < s {
                            won_holding.insert(jp.buys);
                        }
                    }
                    if qualifying_buy(Some(&launch), &pool, &owner, amount, MIN, &excluded) {
                        let k = jackpot_on_buy(&mut header, &mut jp, &mut d, &owner, amount, now)
                            .unwrap();
                        buys.insert(k, (j, seq, now));
                    }
                    hs[j].data = d.encode();
                    hs[j].balance = after;
                }
                2 => {
                    // Wallet to wallet.
                    let amount = rng.below(hs[i].balance + 1);
                    if amount == 0 {
                        continue;
                    }
                    let left = hs[i].balance - amount;
                    let mut s = Slots::decode(&hs[i].data);
                    jackpot_on_send(&mut s, left, now);
                    hs[i].data = s.encode();
                    hs[i].balance = left;
                    last_send_seq[i] = seq;
                    let after = hs[j].balance + amount;
                    let mut d = Slots::decode(&hs[j].data);
                    jackpot_on_receive(&mut d, after, now);
                    hs[j].data = d.encode();
                    hs[j].balance = after;
                }
                3 => {
                    // A sell or a burn.
                    let amount = rng.below(hs[i].balance / 8 + 1);
                    if amount == 0 {
                        continue;
                    }
                    let left = hs[i].balance - amount;
                    let mut s = Slots::decode(&hs[i].data);
                    jackpot_on_send(&mut s, left, now);
                    hs[i].data = s.encode();
                    hs[i].balance = left;
                    last_send_seq[i] = seq;
                }
                _ => {
                    if rng.below(settle_odds) != 0 {
                        continue;
                    }
                    // settle, as the companion runs it.
                    let Some(r) = settle_round(&header, &jp, paid, T, now) else {
                        continue;
                    };
                    assert!(r.number > paid, "rounds settle in order");
                    assert!(settled.insert(r.number), "a round settled twice");
                    let (b, s, at) = buys[&r.number];
                    assert_eq!((hs[b].owner, at), (r.buyer, r.at));
                    let wins =
                        r.amount >= MIN && jackpot_winner_holds(&hs[b].data, &r, hs[b].balance);
                    if wins {
                        assert!(last_send_seq[b] < s, "paid a buyer that sent since its buy");
                        assert!(bordrless_game::timer_over(r.at, T, now));
                    }
                    paid = r.number;
                }
            }
        }
        let lost = won_holding
            .iter()
            .filter(|n| !settled.contains(n) && **n <= paid)
            .count() as u64;
        println!(
            "jackpot walk (settle odds 1/{settle_odds}): {} rounds won, {} settled, {} won but skipped",
            won_holding.len(),
            settled.len(),
            lost
        );
        total_lost += lost;
    }
    assert!(
        total_lost > 0,
        "lagging settles drop won rounds (finding 2)"
    );
}
