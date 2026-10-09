//! Audit, phase 2, round 2 (core): the jackpot's and the streak's companion steps after the
//! round-1 fixes (settle never waits and forfeits an executable buyer, close_epoch releases first,
//! claim_share checks the epoch first, retire waits while `kinds::prize_due`, a lottery refuses a
//! kind header).
//!
//! Tests named `poc_*` PASS while the issue exists: they assert the bad outcome (their doc says
//! what a fixed build should do). `control_*` tests show the safe behaviour. `probe_*` tests
//! measure a fact the findings rely on.
//!
//! The helpers (from `world` to `streak_coin`) are copied from `companion_kinds.rs` (through
//! `audit_p2_game.rs`, which copied them first).

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
/// (created first, by the payer): what any wallet can do for any address (copied from
/// `audit_p2_game.rs`).
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

/// The oldest round `settle` would close now, by the crate's rule.
fn due_round(c: &Coin) -> Option<bordrless_game::JackpotRound> {
    let g = c.game();
    bordrless_game::settle_round(
        &c.header(),
        &c.jackpot(),
        g.paid_buys,
        g.timer_secs,
        c.w.env.now,
    )
}

/// Settles every round that is over and settleable now, stopping at the first refusal.
fn settle_all(c: &mut Coin) {
    for _ in 0..16 {
        let Some(r) = due_round(c) else {
            return;
        };
        if c.settle(&r.buyer).result.is_err() {
            return;
        }
    }
}

fn retire(c: &mut Coin) -> Tx {
    let ix = companion::retire_game(c.cranker.pubkey(), c.mint, c.hook);
    c.send(ix)
}

/// The keys the runtime reserves (agave-reserved-account-keys, active set): a transaction can
/// never write-lock them, whatever its message says (they are demoted to read-only).
const RESERVED: [&str; 30] = [
    "SysvarC1ock11111111111111111111111111111111",
    "SysvarEpochRewards1111111111111111111111111",
    "SysvarEpochSchedu1e111111111111111111111111",
    "SysvarFees111111111111111111111111111111111",
    "Sysvar1nstructions1111111111111111111111111",
    "SysvarLastRestartS1ot1111111111111111111111",
    "SysvarRecentB1ockHashes11111111111111111111",
    "SysvarRent111111111111111111111111111111111",
    "SysvarRewards111111111111111111111111111111",
    "SysvarS1otHashes111111111111111111111111111",
    "SysvarS1otHistory11111111111111111111111111",
    "SysvarStakeHistory1111111111111111111111111",
    "NativeLoader1111111111111111111111111111111",
    "Sysvar1111111111111111111111111111111111111",
    "Feature111111111111111111111111111111111111",
    "StakeConfig11111111111111111111111111111111",
    "ZkTokenProof1111111111111111111111111111111",
    "ZkE1Gama1Proof11111111111111111111111111111",
    "Config1111111111111111111111111111111111111",
    "Stake11111111111111111111111111111111111111",
    "Vote111111111111111111111111111111111111111",
    "AddressLookupTab1e1111111111111111111111111",
    "BPFLoader1111111111111111111111111111111111",
    "BPFLoader2111111111111111111111111111111111",
    "BPFLoaderUpgradeab1e11111111111111111111111",
    "LoaderV411111111111111111111111111111111111",
    "ComputeBudget111111111111111111111111111111",
    "Ed25519SigVerify111111111111111111111111111",
    "KeccakSecp256k11111111111111111111111111111",
    "Secp256r1SigVerify1111111111111111111111111",
];

/// The reserved keys a jackpot treats as a wallet (on the curve) and `settle` does not forfeit
/// (not executable in this SVM; with no account at all, `executable` reads false too).
fn reserved_wallet_like(w: &World) -> Vec<(Pubkey, bool)> {
    RESERVED
        .iter()
        .map(|s| s.parse::<Pubkey>().unwrap())
        .filter(|k| k.is_on_curve())
        .filter_map(|k| {
            let acc = w.env.account(&k);
            let executable = acc.as_ref().is_some_and(|a| a.executable);
            (!executable).then_some((k, acc.is_some()))
        })
        .collect()
}

// =============================================================================== probes

/// PROBE. Which reserved keys are on the ed25519 curve (so `eligible`), and not executable (so
/// `settle`'s round-1 check does not forfeit them).
#[test]
fn probe_reserved_keys_that_look_like_wallets() {
    let w = world();
    for s in RESERVED {
        let k: Pubkey = s.parse().unwrap();
        let acc = w.env.account(&k);
        println!(
            "{s}: on_curve={} exists={} executable={} owner={:?}",
            k.is_on_curve(),
            acc.is_some(),
            acc.as_ref().is_some_and(|a| a.executable),
            acc.as_ref().map(|a| a.owner)
        );
    }
    let found = reserved_wallet_like(&w);
    println!("wallet-like reserved keys: {found:?}");
    assert!(!found.is_empty());
}

// =============================================================================== jackpot

fn clock_sysvar() -> Pubkey {
    "SysvarC1ock11111111111111111111111111111111"
        .parse()
        .unwrap()
}

/// FINDING R2-1. Round 1's fix forfeits an executable buyer only. A reserved key that is on the
/// curve and not executable (the Clock sysvar: on mainnet too it is a non-executable account owned
/// by `Sysvar1111…`; also RecentBlockhashes, Rent, SlotHashes, SlotHistory, and the account-less
/// Rewards, NativeLoader, Sysvar, Feature and Config keys) is `eligible`, so a buy for it is a
/// qualifying buy and the round's "winner" holds. The runtime demotes reserved keys to read-only
/// in every transaction, so `settle`'s payment to it always fails: the round jams, as round 1's
/// F1 did. Worse than in round 1: `retire` now waits while `prize_due` (a round over and
/// unsettled while the pot can pay), which a jammed round is for ever, so the pot (and the pot
/// share of every later fee claim) can never leave either. After graduation nothing can lift the
/// jam (no later buy can overwrite the `ended_*` slot). Only a block moves the pot, and an audited
/// hook can't be blocked.
#[test]
fn poc_reserved_sysvar_buyer_jams_settle_and_locks_the_pot() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    settle_all(&mut c);
    assert!(due_round(&c).is_none());

    let sysvar = clock_sysvar();
    assert!(sysvar.is_on_curve(), "eligible: a 'wallet' to the hook");
    assert!(!c.w.env.account(&sysvar).unwrap().executable);
    let attacker = c.w.wallet_with_sol(SOL);
    buy_for(&mut c, &attacker, &sysvar, SOL / 10).ok();
    assert!(c.balance(&sysvar) >= studio_jackpot::MIN_TOKENS);
    let jammed = c.jackpot().buys;
    assert_eq!(c.header().last_buyer, sysvar);
    assert_eq!(jackpot_mark(&c.slots(&sysvar)), jammed);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    assert!(c.companion().pending_pot >= MIN_POT, "funded");

    // FIXED (round 2): a reserved key can't be paid, so its round is forfeited; the rounds after
    // it settle, and `retire` is never held.
    let f: JackpotForfeited = c.settle(&sysvar).event();
    assert_eq!((f.round, f.buyer), (jammed, sysvar));
    let mint = c.mint;
    let (_, tx) = c.w.graduate_launch(&mint);
    tx.ok();
    c.claim_fees().ok();
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    settle_all(&mut c);
    assert!(due_round(&c).is_none(), "every round closed");
    c.warp_to(c.w.env.now + 61 * 86_400);
    c.volume(1, 5 * SOL);
    c.claim_fees().ok();
    let _: PotRetired = retire(&mut c).event();
}

/// The dormant floor (0.1 SOL): what the test's pot must hold for `retire` to wait.
const MIN_POT_FLOOR_FOR_TEST: u64 = 100_000_000;

/// FINDING R2-1 (breadth). Every reserved key that is on the curve and not executable jams a
/// round the same way, with or without an account behind it (an account-less one reads 0
/// lamports, and the prize is above the rent-exempt minimum, so `settle` tries to pay it).
#[test]
fn poc_every_wallet_like_reserved_key_jams_settle() {
    let keys = reserved_wallet_like(&world());
    for (key, exists) in keys {
        let mut c = Coin::new(GameKind::Jackpot);
        fund(&mut c);
        c.warp_to(c.header().last_buy_at + i64::from(TIMER));
        settle_all(&mut c);
        let attacker = c.w.wallet_with_sol(SOL);
        buy_for(&mut c, &attacker, &key, SOL / 10).ok();
        assert_eq!(c.header().last_buyer, key);
        c.warp_to(c.header().last_buy_at + i64::from(TIMER));
        let tx = c.settle(&key);
        println!(
            "{key} (account: {exists}): settle {}",
            if tx.result.is_err() { "FAILS" } else { "ok" }
        );
        // FIXED (round 2): forfeited, every one of them.
        let f: JackpotForfeited = tx.event();
        assert_eq!(f.buyer, key);
        assert!(due_round(&c).is_none());
    }
}

/// FINDING R2-2. `settle` never waits now, and judges "funded" on `pending_pot` alone, which
/// moves only when someone sends `claim_fees`. The creator fees the launch holds unclaimed are
/// not counted. So whoever settles first at the timer's end decides the round: settled before
/// the fee claim it is closed unfunded (the winner gets nothing, the round is gone); after it, it
/// pays. A griefer (or the next buyer, who wants the pot to grow for their own round) wins that
/// race by sending a bare `settle` the moment the timer runs out (the keeper claims fees before
/// its game steps in a pass, but only above `CLAIM_MIN_LAMPORTS`, and in a transaction of its own,
/// so it can be front-run). Under round 1's rule the round waited for the fee claim.
#[test]
fn poc_bare_settle_before_claim_fees_voids_a_prize_the_fees_would_fund() {
    fn coin_with_unclaimed_fees() -> (Coin, Keypair) {
        let mut c = Coin::new(GameKind::Jackpot);
        c.volume(2, 20 * SOL); // fees enough for the minimum, not claimed yet
        let winner = c.buyer(SOL);
        c.warp_to(c.header().last_buy_at + i64::from(TIMER));
        assert_eq!(due_round(&c).unwrap().buyer, winner.pubkey());
        assert!(
            c.companion().pending_pot < MIN_POT,
            "below the minimum until claimed"
        );
        (c, winner)
    }

    // FIXED (round 2): a bare settle is refused while the launch's unclaimed fees could fund the
    // prize (`FeesUnclaimed`): the fees are claimed first, and the winner is paid.
    let (mut c, winner) = coin_with_unclaimed_fees();
    refused(&c.settle(&winner.pubkey()), CompanionError::FeesUnclaimed);
    assert_eq!(
        due_round(&c).unwrap().buyer,
        winner.pubkey(),
        "the round stays"
    );

    // The same round, the fee claim first: the winner is paid.
    let (mut c, winner) = coin_with_unclaimed_fees();
    c.claim_fees().ok();
    let tx = c.settle(&winner.pubkey());
    println!("settle (paid) CU: {}", tx.cu());
    let paid: JackpotPaid = tx.event();
    assert_eq!(paid.winner, winner.pubkey());
    assert!(paid.prize > 0);
}

/// INFO R2-3. `retire`'s new wait covers only a round already over. Once the pot has paid no
/// prize for two dormant periods (rounds forfeited or closed unfunded don't count), a `retire`
/// sent while a buyer's timer runs (here 1 second before it ends) moves the pot to the buyback,
/// and the round then closes unfunded. The lottery behaves alike (its round in progress can be
/// retired), so this is a property of `retire`, noted because the docs say retire "waits" for a
/// jackpot round.
#[test]
fn poc_retire_during_a_running_timer_voids_the_round() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c); // the volume wallets sold: their rounds are forfeited, no prize is paid
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    settle_all(&mut c);
    assert_eq!(c.game().prizes_paid, 0);
    c.warp_to(c.w.env.now + 61 * 86_400);
    let winner = c.buyer(SOL);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER) - 1);
    let tx = retire(&mut c);
    tx.ok();
    let _: PotRetired = tx.event();
    c.warp_to(c.w.env.now + 1);
    let u: JackpotUnfunded = c.settle(&winner.pubkey()).event();
    assert_eq!(u.winner, winner.pubkey());
}

// =============================================================================== streak

/// DOC R2-4. p2-m says "a streak nobody claims sends its pot to the buyback". With `prize_due`
/// it never does while holders keep registering weight (the keeper enters them each epoch) and
/// epochs are closed: before the close `retire` waits for it (`prize_due`), after it the epoch
/// is open for claims (`DrawPending`), until the next epoch is closable again. Here 11 weekly
/// epochs, past `retirable_at` (two dormant periods of 30 days), and `retire` is refused at
/// every point tried. Not a loss (holders can claim at any time), only the docs' promise.
#[test]
fn poc_streak_nobody_claims_never_retires() {
    let (mut c, a, b, e0) = streak_coin();
    fund(&mut c);
    let retirable = c.game().retirable_at(c.companion().launched_at);
    let mut e = e0 + 1;
    loop {
        c.warp_into(e, 60);
        c.enter(&[&a.pubkey(), &b.pubkey()]);
        if c.w.env.now >= retirable {
            refused(&retire(&mut c), CompanionError::DrawPending); // before the close
        }
        c.close_epoch(e - 1).ok();
        if c.w.env.now >= retirable {
            refused(&retire(&mut c), CompanionError::DrawPending); // after it
            c.warp_into(e, i64::from(EPOCH) - 1);
            refused(&retire(&mut c), CompanionError::DrawPending); // the epoch's last second
        }
        if c.w.env.now > retirable + 2 * i64::from(EPOCH) {
            break;
        }
        e += 1;
    }
    assert!(e - e0 >= 9, "{} epochs", e - e0);
    assert!(c.companion().pending_pot >= MIN_POT);
    assert_eq!(c.game().prizes_paid, 0);
}

// =============================================================================== controls

/// CONTROL. `retire`'s `prize_due` reads the hook's state only at its derived address, owned by
/// the hook: without it a jackpot's retire is refused (`MissingAccount`) once the pot can pay,
/// and an account that merely looks like a state, at another address, is not read. With the real
/// state it waits while a round is due, and goes through once the round is settled.
#[test]
fn control_retire_reads_only_the_real_state() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    settle_all(&mut c);
    c.warp_to(c.w.env.now + 61 * 86_400);
    let winner = c.buyer(SOL);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    assert!(due_round(&c).is_some());

    // No state: refused.
    let bare = companion::retire(c.cranker.pubkey(), c.mint, c.hook);
    refused(&c.send(bare), CompanionError::MissingAccount);
    // A copy of the state with the round already "settled" (buys rewound), at another address
    // owned by the hook: not the state, so still refused.
    let real = bordrless_game::state_address(&c.hook, &c.mint).0;
    let mut fake = c.w.env.account(&real).unwrap();
    let at = bordrless_game::jackpot::jackpot_offsets::BUYS;
    fake.data[at..at + 8].copy_from_slice(&0u64.to_le_bytes());
    let fake_key = Pubkey::new_unique();
    c.w.env.svm.set_account(fake_key, fake).unwrap();
    let mut ix = companion::retire(c.cranker.pubkey(), c.mint, c.hook);
    ix.accounts.push(AccountMeta::new_readonly(fake_key, false));
    refused(&c.send(ix), CompanionError::MissingAccount);
    // The real state: it waits for the round.
    refused(&retire(&mut c), CompanionError::DrawPending);
    let paid: JackpotPaid = c.settle(&winner.pubkey()).event();
    assert_eq!(paid.winner, winner.pubkey());
    // Paid: the clock restarted, so retire is not due any more.
    refused(&retire(&mut c), CompanionError::NotDue);
}
