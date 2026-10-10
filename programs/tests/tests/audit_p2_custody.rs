//! Audit (phase 2, custody, round 1): PoCs and controls. Helpers copied from companion_kinds.rs.
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

// =============================================================================== audit PoCs

fn retire_ix(c: &Coin) -> Instruction {
    // FIXED (round 1): a jackpot's or a streak's retire passes the hook's state.
    companion::retire_game(c.cranker.pubkey(), c.mint, c.hook)
}

fn assert_lock_invariant(c: &Coin) {
    let co = c.companion();
    assert!(
        co.pot_locked <= co.pending_pot,
        "pot_locked {} > pending_pot {}",
        co.pot_locked,
        co.pending_pot
    );
}

/// FINDING (low): `close_epoch` applies the hook's status (`apply_status` -> `enforce_terms`)
/// BEFORE it releases the epoch whose claims have ended (`end_epoch_if_over`), so a cap lowered
/// while an epoch was locked is measured against that stale lock: the unclaimed remainder is never
/// trimmed to the cap, and the new epoch's pot is fixed from the untrimmed pot and locked again.
/// With the SDK's streak default (`prize_bps` 100%) this repeats every epoch, so the cap the
/// protocol lowered is not applied to that money at all (it is paid to holders instead of going
/// to the buyback).
#[test]
fn p2c_stale_epoch_lock_skips_a_lowered_cap() {
    let (mut c, a, b, e) = streak_coin();
    c.volume(3, 30 * SOL);
    c.claim_fees().ok();
    c.warp_into(e + 1, 30);
    c.close_epoch(e).ok();
    let locked_e = c.companion().pot_locked;
    assert!(locked_e > 10 * MIN_POT_CAP);
    // The protocol lowers the cap to its least while epoch e's claims are open. Fine so far:
    // e's shares are owed.
    c.set_status(false, MIN_POT_CAP, false).ok();
    // Nobody claims e. A and B hold through e + 1.
    c.enter(&[&a.pubkey(), &b.pubkey()]);
    assert_lock_invariant(&c);
    // e's claims end with e + 1. FIXED (round 1): the next close releases e's lock first, so the
    // cap trims what e left, and e + 1 locks at most the cap.
    c.warp_into(e + 2, 30);
    let tx = c.close_epoch(e + 1);
    tx.ok();
    let closed: EpochClosed = tx.event();
    let co = c.companion();
    assert!(co.pending_pot <= MIN_POT_CAP, "trimmed to the cap");
    assert!(closed.epoch_pot <= MIN_POT_CAP && co.pot_locked <= MIN_POT_CAP);
    let tx = c.claim_share(e + 1, &a.pubkey());
    tx.ok();
    let ev: ShareClaimed = tx.event();
    assert!(ev.share + ev.bounty <= MIN_POT_CAP);
    assert_lock_invariant(&c);
}

/// CONTROL: the same cap lowered while no epoch is locked trims the pot at the next close, and
/// the epoch locks at most the cap.
#[test]
fn p2c_control_cap_lowered_while_idle_is_applied() {
    let (mut c, _a, _b, e) = streak_coin();
    c.volume(3, 30 * SOL);
    c.claim_fees().ok();
    c.warp_into(e + 1, 30);
    c.set_status(false, MIN_POT_CAP, false).ok();
    c.close_epoch(e).ok();
    let co = c.companion();
    assert_eq!(co.pending_pot, MIN_POT_CAP);
    assert_eq!(co.pot_locked, MIN_POT_CAP);
}

/// FINDING (low): `claim_share` makes the receipt (an `init` account) before `apply_status`, and
/// under a block `apply_status` answers `None` and the instruction succeeds. So the one step that
/// moves a blocked game's pot can be a `claim_share` naming ANY epoch and ANY owner: it leaves a
/// receipt `PDA(["claimed", game, epoch, owner])` (amount 0). When an audit later lifts the block,
/// that owner can never claim that epoch (the receipt exists, `init` fails), and the receipt can't
/// be closed before that epoch's claims end.
#[test]
fn p2c_blocked_claim_share_plants_a_receipt_for_a_future_epoch() {
    let (mut c, a, b, e) = streak_coin();
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    c.set_status(false, DEFAULT_POT_CAP, true).ok();
    let target = e + 3;
    let attacker = c.w.wallet_with_sol(SOL);
    let ix = companion::claim_share(attacker.pubkey(), c.mint, c.hook, target, a.pubkey());
    let tx = c.w.env.send_paid_by(&[ix], &attacker, &[]);
    // FIXED (round 1): the epoch is checked before the block's early return: no receipt is made.
    refused(&tx, CompanionError::NoDraw);
    let planted = companion::receipt_address(&c.mint, target, &a.pubkey());
    assert!(c.w.env.account(&planted).is_none());
    // The protocol audits the hook: the block is lifted, the game goes on, and A claims.
    c.set_status(true, DEFAULT_POT_CAP, false).ok();
    c.warp_into(target, 60);
    c.enter(&[&a.pubkey(), &b.pubkey()]);
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    c.warp_into(target + 1, 30);
    c.close_epoch(target).ok();
    assert!(
        c.slots(&a.pubkey()).range_in(target).is_some(),
        "A qualifies"
    );
    c.claim_share(target, &a.pubkey()).ok();
    c.claim_share(target, &b.pubkey()).ok();
}

/// CONTROL: a block through `claim_fees` (which never touches the `Game`) leaves the epoch
/// `Revealed` with nothing locked; after an audit lifts the block and new fees fund the pot, no
/// share of that epoch is paid from them (the `pot_locked` bound holds).
#[test]
fn p2c_control_block_through_claim_fees_then_audit_pays_no_stale_share() {
    let (mut c, a, _b, e) = streak_coin();
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    c.warp_into(e + 1, 30);
    c.close_epoch(e).ok();
    c.set_status(false, DEFAULT_POT_CAP, true).ok();
    c.volume(1, 20 * SOL);
    c.claim_fees().ok();
    let co = c.companion();
    assert_eq!((co.pending_pot, co.pot_locked), (0, 0));
    assert_eq!(c.game().status, DrawStatus::Revealed);
    c.set_status(true, DEFAULT_POT_CAP, false).ok();
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    assert!(c.companion().pending_pot > 0);
    let before = c.w.env.lamports(&a.pubkey());
    refused(&c.claim_share(e, &a.pubkey()), CompanionError::NothingToDo);
    assert_eq!(c.w.env.lamports(&a.pubkey()), before);
    // A receipt closed after the claims ended never lets a share be claimed again.
    c.warp_into(e + 2, 0);
    refused(&c.claim_share(e, &a.pubkey()), CompanionError::DrawLate);
}

/// CONTROL: a claimed receipt, closed once its epoch's claims ended, does not re-open the claim;
/// a holding of another owner or a forged owner is refused; the lock never exceeds the pot.
#[test]
fn p2c_control_receipts_and_owners() {
    let (mut c, a, b, e) = streak_coin();
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    c.warp_into(e + 1, 30);
    c.close_epoch(e).ok();
    // A's holding with B as the owner (B paid with A's weight): refused.
    let creator = companion::creator_address(&c.mint);
    let mut ix = companion::claim_share(c.cranker.pubkey(), c.mint, c.hook, e, b.pubkey());
    ix.accounts[6].pubkey = bordrless_token::client::holding_address(&c.mint, &a.pubkey());
    refused(&c.send(ix), CompanionError::WrongHolding);
    // The creator address as the owner: not eligible (it holds nothing anyway).
    c.claim_share(e, &creator).expect_fail();
    c.claim_share(e, &a.pubkey()).ok();
    assert_lock_invariant(&c);
    c.w.env.warp(1);
    c.claim_share(e, &a.pubkey()).expect_fail();
    c.warp_into(e + 2, 0);
    let ix = companion::close_receipt(c.mint, e, a.pubkey(), c.cranker.pubkey());
    c.send(ix).ok();
    refused(&c.claim_share(e, &a.pubkey()), CompanionError::DrawLate);
    // B, who never claimed, can't once the claims ended.
    refused(&c.claim_share(e, &b.pubkey()), CompanionError::DrawLate);
    // A receipt closed to anyone but its payer: refused.
    let ix = companion::close_receipt(c.mint, e, b.pubkey(), b.pubkey());
    c.send(ix).expect_fail();
}

/// FINDING (low): `retire` gives a jackpot no "round in progress" guard (the lottery's is
/// `status == Idle`, always true for a jackpot). Once the game is retirable (60 days without a
/// paid prize, e.g. after forfeited rounds), a round whose timer has run out with a buyer who still
/// holds (a won round, `settle` would pay it) can be swept to the buyback by whoever sends
/// `retire` first; the winner then gets nothing (`PotTooSmall`).
#[test]
fn p2c_retire_sweeps_a_won_jackpot_round_before_settle() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    // The volume's round is forfeited (its buyer sold): the pot has paid no prize since launch.
    let last = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&last).ok();
    let retirable = c.game().retirable_at(c.companion().launched_at);
    c.warp_to(retirable - i64::from(TIMER) - 60);
    let alice = c.buyer(SOL);
    assert_eq!(c.header().last_buyer, alice.pubkey());
    c.warp_to(retirable.max(c.header().last_buy_at + i64::from(TIMER)));
    let pot = c.companion().pending_pot;
    assert!(pot >= MIN_MIN_POT, "settle would pay Alice now");
    // FIXED (round 1): retire waits while the round can be paid (and plain `retire`, without the
    // hook's state, is refused); settle pays Alice.
    let plain = companion::retire(c.cranker.pubkey(), c.mint, c.hook);
    refused(&c.send(plain), CompanionError::MissingAccount);
    refused(&c.send(retire_ix(&c)), CompanionError::DrawPending);
    let paid: JackpotPaid = c.settle(&alice.pubkey()).event();
    assert_eq!(paid.winner, alice.pubkey());
}

/// CONTROL: the same scene with `settle` first pays Alice, and `retire` is then not due.
#[test]
fn p2c_control_settle_first_pays_and_retire_waits() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let last = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&last).ok();
    let retirable = c.game().retirable_at(c.companion().launched_at);
    c.warp_to(retirable - i64::from(TIMER) - 60);
    let alice = c.buyer(SOL);
    c.warp_to(retirable.max(c.header().last_buy_at + i64::from(TIMER)));
    let tx = c.settle(&alice.pubkey());
    tx.ok();
    let paid: JackpotPaid = tx.event();
    assert_eq!(paid.winner, alice.pubkey());
    refused(&c.send(retire_ix(&c)), CompanionError::NotDue);
}

/// CONTROL: a forfeited round is never re-settled, a paid round is never paid twice, and settle
/// pays only the header's buyer (another buyer's holding is refused).
#[test]
fn p2c_control_jackpot_pays_once_to_the_buyer() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let last = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&last).ok();
    refused(&c.settle(&last), CompanionError::NotDue);
    let a = c.buyer(SOL);
    let b = c.buyer(SOL);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    refused(&c.settle(&a.pubkey()), CompanionError::WrongHolding);
    c.settle(&b.pubkey()).ok();
    refused(&c.settle(&b.pubkey()), CompanionError::NotDue);
    // A later buy moves B's paid round to `ended`: it is not paid again.
    c.w.env.warp(10);
    let d = c.buyer(SOL);
    refused(&c.settle(&b.pubkey()), CompanionError::NotDue);
    refused(&c.settle(&d.pubkey()), CompanionError::NotDue);
}

/// INFO: a game's kind is not tied to its hook's kind for a lottery: `create_game` (phase 1's,
/// lottery only) takes the streak starter (auto-vetted through Studio's key) since a lottery reads
/// no kind header. The streak's slots all start at ticket 0, so under the lottery's `wins` every
/// registered holding whose weight exceeds the drawn ticket wins: the first claimer takes the
/// prize (a whale nearly always). Creator-chosen; bounded by the 10 SOL cap while not audited, but
/// an audit (`HookStatus` is per hook, not per kind) would lift the cap for both uses.
#[test]
fn p2c_info_streak_hook_accepted_as_a_lottery_and_every_holder_wins_ticket_0() {
    let mut w = world();
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let args = CreateGameArgs {
        kind: GameKind::Lottery,
        hook: STREAK,
        split: SPLIT,
        pot_bps: POT_BPS,
        round_secs: EPOCH,
        min_pot: MIN_POT,
        prize_bps: 5_000,
        claim_window_secs: CLAIM_WINDOW,
        max_attempts: 4,
    };
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
        prepare_ix(STREAK, launcher.pubkey(), mint),
        companion::create_game_with_program_data(launcher.pubkey(), mint, args),
    ];
    // FIXED (round 1): a lottery refuses a jackpot's or a streak's state.
    refused(
        &w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]),
        CompanionError::HookState,
    );
    assert!(w.env.account(&companion::game_address(&mint)).is_none());
}

/// FINDING (low, same root as the jackpot one): a streak's `retire` only waits for an epoch that
/// is already closed (`Revealed`). Between an epoch's end and its `close_epoch` the game is
/// `Idle`, so once retirable a `retire` sent first sweeps the pot that epoch's holders held
/// through; `close_epoch` then fails (`PotTooSmall`) and they get nothing.
#[test]
fn p2c_retire_front_runs_close_epoch() {
    let (mut c, a, _b, _) = streak_coin();
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    let retirable = c.game().retirable_at(c.companion().launched_at);
    let last = round_of(retirable, EPOCH);
    c.enter(&[&a.pubkey()]);
    c.warp_into(last, 30);
    c.enter(&[&a.pubkey()]);
    assert!(
        c.slots(&a.pubkey()).range_in(last).is_some(),
        "A held through"
    );
    c.warp_into(last + 1, 30);
    assert!(c.w.env.now >= retirable);
    assert_eq!(c.game().status, DrawStatus::Idle);
    // FIXED (round 1): retire waits while the epoch with weight can be closed; it is closed and
    // A claims.
    refused(&c.send(retire_ix(&c)), CompanionError::DrawPending);
    c.close_epoch(last).ok();
    c.claim_share(last, &a.pubkey()).ok();
}
