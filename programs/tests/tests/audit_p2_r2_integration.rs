//! Phase-2 integration audit, round 2: PoCs and measurements against the round-1 fixes (settle
//! never waits and needs the buyer's account; `retire_game` passes the hook state and waits while a
//! prize is due). Helpers copied from `audit_p2_integration.rs` / `audit_p2_game.rs` (a test file is
//! its own crate). Run:
//! `CARGO_INCREMENTAL=0 cargo test -p bordrless-program-tests --test audit_p2_r2_integration -- --nocapture --test-threads 1`

#![allow(clippy::too_many_lines)]

use std::str::FromStr;

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{InstructionData, ToAccountMetas};
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{CreateArgs, CreateGameArgs, GameKindArgs, HookStatusArgs};
use bordrless_companion::state::{Companion, Game, GameKind, Split};
use bordrless_core::policy;
use bordrless_game::{round_of, round_start, GameHeader, JackpotHeader};
use bordrless_launch::client as launch;
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::program_bytes;
use bordrless_token::client::{self as token};
use solana_keypair::Keypair;
use solana_signer::Signer;

const JACKPOT: Pubkey = studio_jackpot::ID;
const STREAK: Pubkey = studio_streak::ID;
const STUDIO_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[0];
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
        GameKind::Lottery | GameKind::Strategy => unreachable!(),
    }
}

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

fn launch_ix(w: &World, launcher: &Pubkey, mint: &Pubkey, config: &Pubkey) -> Instruction {
    let c = w.launch_config(config);
    let custom = c.custom_hook.map(|h| w.custom_hook_accounts(&h, mint));
    let mut args = World::launch_args("GAMEXXXXXX", c.creator_fee_bps, VQ, c.rules);
    args.name = "N".repeat(32);
    args.uri = format!("https://gateway.pinata.cloud/ipfs/{}", "b".repeat(94));
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
        w.env
            .send_paid_by(
                &setup_ixs(&launcher.pubkey(), &mint, kind),
                &launcher,
                &[&mint_kp],
            )
            .ok();
        let config = hook_config(&mut w, &launcher, hook, kind.hook_flags());
        let ixs = [launch_ix(&w, &launcher.pubkey(), &mint, &config)];
        w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]).ok();
        w.env.warp(31);
        let cranker = w.wallet_with_sol(5 * SOL);
        Self {
            w,
            mint,
            hook,
            cranker,
        }
    }

    fn game(&self) -> Game {
        self.w.env.read(&companion::game_address(&self.mint))
    }

    fn companion(&self) -> Companion {
        self.w.env.read(&companion::companion_address(&self.mint))
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

    fn send(&mut self, ix: Instruction) -> Tx {
        let cranker = self.cranker.insecure_clone();
        self.w.env.send_paid_by(&[ix], &cranker, &[])
    }

    fn buyer(&mut self, lamports: u64) -> Keypair {
        let t = self.w.wallet_with_sol(lamports + SOL);
        self.w.buy(&t, &self.mint, lamports).ok();
        t
    }

    fn volume(&mut self, wallets: usize, lamports: u64) {
        for _ in 0..wallets {
            let t = self.w.wallet_with_sol(lamports + SOL);
            self.w.buy(&t, &self.mint, lamports).ok();
            let held = self.w.env.holding(&self.mint, &t.pubkey());
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
        assert!(t >= self.w.env.now);
        self.w.env.warp(t - self.w.env.now);
    }

    fn settle(&mut self, buyer: &Pubkey) -> Tx {
        let ix = companion::settle(self.cranker.pubkey(), self.mint, self.hook, *buyer);
        self.send(ix)
    }

    fn retire_game(&mut self) -> Tx {
        let ix = companion::retire_game(self.cranker.pubkey(), self.mint, self.hook);
        self.send(ix)
    }

    /// Buys `lamports` of the launch, paid by `payer`, delivered to `recipient`'s holding (made
    /// first): the swap delivers to any holding of the mint.
    fn buy_for(&mut self, payer: &Keypair, recipient: &Pubkey, lamports: u64) -> Tx {
        let mint = self.mint;
        let keys = self.w.launch_keys(&mint);
        let slice = self
            .w
            .launch_base_slice(&mint, &payer.pubkey(), recipient, true);
        let ixs = [
            token::create_holding(payer.pubkey(), mint, *recipient),
            launch::swap_with_base_slice(&keys, payer.pubkey(), *recipient, 1, lamports, 0, slice),
        ];
        self.w.env.send_paid_by(&ixs, payer, &[])
    }
}

fn fund(c: &mut Coin) {
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    assert!(c.companion().pending_pot >= MIN_POT);
}

/// The runtime's reserved account keys (agave-reserved-account-keys 4.3.0, all active): a
/// transaction's message demotes any of them to read-only, whatever its account meta says.
const RESERVED: [&str; 30] = [
    "AddressLookupTab1e1111111111111111111111111",
    "BPFLoader1111111111111111111111111111111111",
    "BPFLoader2111111111111111111111111111111111",
    "BPFLoaderUpgradeab1e11111111111111111111111",
    "ComputeBudget111111111111111111111111111111",
    "Config1111111111111111111111111111111111111",
    "Ed25519SigVerify111111111111111111111111111",
    "Feature111111111111111111111111111111111111",
    "LoaderV411111111111111111111111111111111111",
    "KeccakSecp256k11111111111111111111111111111",
    "Secp256r1SigVerify1111111111111111111111111",
    "StakeConfig11111111111111111111111111111111",
    "Stake11111111111111111111111111111111111111",
    "11111111111111111111111111111111",
    "Vote111111111111111111111111111111111111111",
    "ZkE1Gama1Proof11111111111111111111111111111",
    "ZkTokenProof1111111111111111111111111111111",
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
];

/// The reserved keys a jackpot hook takes for a wallet (on the ed25519 curve) and that are not
/// executable here (so `settle`'s executable check does not forfeit them).
fn reserved_eligible_not_executable(c: &Coin) -> Vec<Pubkey> {
    let mut out = Vec::new();
    for s in RESERVED.iter().chain(std::iter::once(
        &"Sysvar1111111111111111111111111111111111111",
    )) {
        let key = Pubkey::from_str(s).unwrap();
        let acc = c.w.env.account(&key);
        let executable = acc.as_ref().is_some_and(|a| a.executable);
        println!(
            "{s}: on curve {}, exists {}, executable {executable}, lamports {}",
            key.is_on_curve(),
            acc.is_some(),
            acc.as_ref().map_or(0, |a| a.lamports)
        );
        if key.is_on_curve() && !executable {
            out.push(key);
        }
    }
    out
}

// ================================================================================================
// FINDING (medium): a jackpot round won by a reserved, non-executable key on the curve (a sysvar,
// say) can never be settled: `settle` finds the buyer holds and pays it, but the runtime demotes a
// reserved key to read-only in every transaction, so the credit fails. Round 1's fix forfeits only
// an executable buyer. Settle refuses every time; `retire` waits on `prize_due` (a round over, the
// pot above its minimum) for ever. Before graduation two later rounds free it; after graduation
// (or with no more buys) the pot is locked until the protocol blocks the hook.
// ================================================================================================

#[test]
fn poc_a_round_won_by_a_reserved_key_jams_settle_and_retire() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let stale = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&stale).ok();
    let candidates = reserved_eligible_not_executable(&c);
    println!("reserved, on the curve, not executable: {candidates:?}");
    assert!(!candidates.is_empty(), "no reserved key is on the curve");
    // The attacker buys the minimum (and more), delivered to a reserved key's holding.
    let attacker = c.w.wallet_with_sol(2 * SOL);
    // A sysvar that exists on mainnet (non-executable, owned by the sysvar program) first.
    let mut ordered: Vec<Pubkey> = candidates
        .iter()
        .filter(|k| k.to_string().starts_with("SysvarRent"))
        .copied()
        .collect();
    ordered.extend(candidates.iter().copied());
    let mut target = None;
    for key in &ordered {
        let tx = c.buy_for(&attacker, key, SOL / 2);
        if tx.result.is_ok() && c.header().last_buyer == *key {
            target = Some(*key);
            break;
        }
        println!("buy for {key} refused or not qualifying");
    }
    let target = target.expect("a reserved key took a round");
    println!("round {} won by reserved key {target}", c.jackpot().buys);
    let _ = c.claim_fees();
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let tx = c.settle(&target);
    println!(
        "settle for the reserved key: ok {} err {:?}",
        tx.result.is_ok(),
        tx.result.as_ref().err().map(|f| f.err.clone())
    );
    if tx.result.is_ok() {
        // The fixed behaviour: a reserved key can't be paid, so its round is forfeited (and the
        // rounds after it, and `retire`, go on).
        let f: JackpotForfeited = tx.event();
        assert_eq!(f.buyer, target);
        println!("fixed: the reserved key's round is forfeited");
        return;
    }
    for l in tx.logs().iter().rev().take(6) {
        println!("  {l}");
    }
    // Nothing else can move this game's pot: retire waits while the round is "due".
    let launched = c.companion().launched_at;
    let retirable = c.game().retirable_at(launched);
    c.warp_to(retirable + 1);
    let _ = c.claim_fees();
    assert!(c.companion().pending_pot >= MIN_POT);
    c.settle(&target).expect_fail();
    let tx = c.retire_game();
    tx.expect_code(code(CompanionError::DrawPending));
    // Graduation: the curve's fill buys move the jammed round to `ended_*`; it stays the oldest
    // open round for ever (no qualifying buy after graduation can overwrite it).
    let mint = c.mint;
    let (_, tx) = c.w.graduate_launch(&mint);
    if tx.result.is_ok() {
        let _ = c.claim_fees();
        c.warp_to(c.header().last_buy_at + i64::from(TIMER) + 1);
        let j = c.jackpot();
        let g = c.game();
        let r =
            bordrless_game::settle_round(&c.header(), &j, g.paid_buys, g.timer_secs, c.w.env.now)
                .unwrap();
        println!(
            "after graduation the oldest open round is {} by {}",
            r.number, r.buyer
        );
        if r.buyer == target {
            c.settle(&target).expect_fail();
            c.warp_to(c.w.env.now + 90 * 86_400);
            let _ = c.claim_fees();
            c.retire_game()
                .expect_code(code(CompanionError::DrawPending));
            println!("after graduation + 90 days: settle and retire still refused");
        }
    }
    // The only way out: the protocol blocks the hook (the pot goes to the buyback).
    c.set_status(false, DEFAULT_POT_CAP, true).ok();
    let _: PotToBuyback = c.retire_game().event();
    panic!("FINDING: a round won by reserved key {target} jams settle and retire (pot locked until a block)");
}

// ================================================================================================
// Measurements: settle (every branch) and retire_game, v0 with the 22-address table and a compute
// budget (what the keeper sends).
// ================================================================================================

fn v0(c: &mut Coin, table: &[Pubkey], f: impl Fn(&Coin) -> Instruction) -> Tx {
    let ix = f(c);
    let cranker = c.cranker.insecure_clone();
    let lookup = c.w.env.put_lookup_table(Pubkey::new_unique(), table);
    let ixs = [compute_unit_limit(300_000), compute_unit_price(20_000), ix];
    c.w.env
        .send_v0(&ixs, &cranker, &[], std::slice::from_ref(&lookup))
}

#[test]
fn measure_settle_and_retire_game() {
    // Jackpot: settle unfunded (no pot), paid, forfeited; retire_game waiting and retiring.
    let mut c = Coin::new(GameKind::Jackpot);
    let table = table_22(&c.w);
    let b = c.buyer(SOL);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    assert!(c.companion().pending_pot < MIN_POT);
    let stale = {
        let j = c.jackpot();
        let g = c.game();
        bordrless_game::settle_round(&c.header(), &j, g.paid_buys, g.timer_secs, c.w.env.now)
            .unwrap()
    };
    let tx = v0(&mut c, &table, |c| {
        companion::settle(c.cranker.pubkey(), c.mint, c.hook, stale.buyer)
    });
    tx.ok();
    let _: JackpotUnfunded = tx.event();
    println!(
        "settle (unfunded) v0: {} B, CU {}, height {}, trace {}",
        tx.size,
        tx.cu(),
        tx.max_height(),
        tx.trace_len()
    );
    let _ = b;
    fund(&mut c);
    let b = c.buyer(SOL);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    // Settle the older round first if the volume left one.
    loop {
        let j = c.jackpot();
        let g = c.game();
        let r =
            bordrless_game::settle_round(&c.header(), &j, g.paid_buys, g.timer_secs, c.w.env.now)
                .unwrap();
        if r.buyer == b.pubkey() {
            break;
        }
        c.settle(&r.buyer).ok();
    }
    let tx = v0(&mut c, &table, |c| {
        companion::settle(c.cranker.pubkey(), c.mint, c.hook, b.pubkey())
    });
    tx.ok();
    let _: JackpotPaid = tx.event();
    println!(
        "settle (paid) v0: {} B, CU {}, height {}, trace {}",
        tx.size,
        tx.cu(),
        tx.max_height(),
        tx.trace_len()
    );
    assert!(tx.size <= 1_232 && tx.cu() < 200_000);
    let s = c.buyer(SOL);
    let mint = c.mint;
    c.w.sell(&s, &mint, 1_000_000).ok();
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let tx = v0(&mut c, &table, |c| {
        companion::settle(c.cranker.pubkey(), c.mint, c.hook, s.pubkey())
    });
    tx.ok();
    let _: JackpotForfeited = tx.event();
    println!("settle (forfeited) v0: {} B, CU {}", tx.size, tx.cu());
    // retire_game: a round due (waits), then none (retires).
    let launched = c.companion().launched_at;
    let retirable = c.game().retirable_at(launched);
    c.warp_to(retirable - i64::from(TIMER) - 10);
    let b2 = c.buyer(SOL);
    fund(&mut c);
    c.warp_to(retirable + 1);
    let tx = v0(&mut c, &table, |c| {
        companion::retire_game(c.cranker.pubkey(), c.mint, c.hook)
    });
    tx.expect_code(code(CompanionError::DrawPending));
    println!(
        "retire_game (jackpot, waits) v0: {} B, CU {}",
        tx.size,
        tx.cu()
    );
    loop {
        let j = c.jackpot();
        let g = c.game();
        let Some(r) =
            bordrless_game::settle_round(&c.header(), &j, g.paid_buys, g.timer_secs, c.w.env.now)
        else {
            break;
        };
        c.settle(&r.buyer).ok();
    }
    let _ = b2;
    let tx = v0(&mut c, &table, |c| {
        companion::retire_game(c.cranker.pubkey(), c.mint, c.hook)
    });
    // A prize paid restarts the retire clock: retire is NotDue now, or retires when no prize.
    println!(
        "retire_game (jackpot, after settle) v0: {} B, CU {}, result {:?}",
        tx.size,
        tx.cu(),
        tx.result.as_ref().err().map(|f| f.err.clone())
    );
    assert!(tx.size <= 1_232);

    // Streak: retire_game with the state, when nothing is due.
    let mut c = Coin::new(GameKind::Streak);
    let table = table_22(&c.w);
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    let launched = c.companion().launched_at;
    let retirable = c.game().retirable_at(launched);
    c.warp_to(round_start(round_of(retirable, EPOCH) + 1, EPOCH) + 30);
    let tx = v0(&mut c, &table, |c| {
        companion::retire_game(c.cranker.pubkey(), c.mint, c.hook)
    });
    tx.ok();
    let _: PotRetired = tx.event();
    println!(
        "retire_game (streak, retires) v0: {} B, CU {}",
        tx.size,
        tx.cu()
    );
}

// ================================================================================================
// Keeper parity: a jackpot's pot below its minimum. The keeper lists [settle, retire] once
// retirable; settle closes the round unfunded, and retire (which never waits on a pot that can't
// pay) lands in the same pass. And a lottery-style `retire` without the hook state is enough then
// (prize_due stops at the pot), but not once the pot can pay.
// ================================================================================================

#[test]
fn keeper_parity_unfunded_settle_then_retire() {
    let mut c = Coin::new(GameKind::Jackpot);
    // A small pot: one trade's fee share, below the 0.1 SOL minimum.
    c.volume(1, SOL);
    c.claim_fees().ok();
    let pot = c.companion().pending_pot;
    println!("pot {pot} (min {MIN_POT})");
    assert!(pot > 0 && pot < MIN_POT);
    let launched = c.companion().launched_at;
    let retirable = c.game().retirable_at(launched);
    c.warp_to(retirable - i64::from(TIMER) - 10);
    let b = c.buyer(SOL / 10);
    c.warp_to(retirable + 1);
    // The keeper sends settle first: the oldest round over closes paying nothing.
    loop {
        let j = c.jackpot();
        let g = c.game();
        let Some(r) =
            bordrless_game::settle_round(&c.header(), &j, g.paid_buys, g.timer_secs, c.w.env.now)
        else {
            break;
        };
        let tx = c.settle(&r.buyer);
        tx.ok();
        if r.buyer == b.pubkey() {
            let _: JackpotUnfunded = tx.event();
        }
    }
    // Then retire (round 3: a jackpot's or a streak's retire always passes the hook's state and
    // the fee holdings, `retire_game`; a plain one is refused).
    let plain = companion::retire(c.cranker.pubkey(), c.mint, c.hook);
    c.send(plain).expect_fail();
    let tx = c.send(companion::retire_game(c.cranker.pubkey(), c.mint, c.hook));
    let _: PotRetired = tx.event();
    println!("retire_game with a pot below its minimum: retired");
}

/// The docs' size table as `companion_kinds` measures it (v0, the table, a compute price only).
#[test]
fn measure_sizes_as_the_docs_do() {
    let mut c = Coin::new(GameKind::Jackpot);
    let table = table_22(&c.w);
    let lookup = c.w.env.put_lookup_table(Pubkey::new_unique(), &table);
    let cranker = c.cranker.pubkey();
    let buyer = Keypair::new().pubkey();
    for (name, ix) in [
        ("settle", companion::settle(cranker, c.mint, c.hook, buyer)),
        (
            "retire_game",
            companion::retire_game(cranker, c.mint, c.hook),
        ),
        ("retire", companion::retire(cranker, c.mint, c.hook)),
    ] {
        let size = c.w.env.v0_size(
            &[compute_unit_price(20_000), ix],
            &c.cranker,
            &[],
            std::slice::from_ref(&lookup),
        );
        println!("{name} v0 (price only): {size} B");
    }
    let _ = enter_ix(c.mint, buyer);
}
