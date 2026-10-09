//! Phase 2 audit, round 4 (hostile, final verification): the round-3 fixes (`fundable` counting the
//! launch's unclaimed fees and the creator holding's surplus in `settle`'s `FeesUnclaimed` and in
//! `prize_due`, `ForfeitReason`, `client::retire_game` passing both bridged-SOL holdings; the
//! keeper holding `retire` back after a missed settle). Helpers copied from `audit_p2_r3.rs`
//! (itself from `companion_kinds.rs`). `poc_*` assert the outcome they document; `control_*` the
//! safe one.
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

// =============================================================================== round 4

/// The keeper's fee-claim minimum (`CLAIM_MIN_LAMPORTS`, apps/server/src/keeper/companions.ts): it
/// claims only once the launch holds this much, and never looks at the creator holding's surplus.
const KEEPER_CLAIM_MIN: u64 = 20_000_000;

fn creator_surplus(c: &Coin) -> u64 {
    let creator = companion::creator_address(&c.mint);
    c.w.env
        .holding(&c.w.sol, &creator)
        .saturating_sub(c.companion().set_aside().unwrap())
}

/// The program's `fundable` as the test reads it.
fn fundable(c: &Coin) -> u64 {
    let co = c.companion();
    co.pending_pot
        + bordrless_companion::state::bps_of(
            unclaimed(c) + creator_surplus(c),
            u64::from(co.pot_bps),
        )
}

/// `lamports` of bridged SOL sent to `owner`'s holding by a fresh donor.
fn donate(c: &mut Coin, owner: &Pubkey, lamports: u64) {
    let donor = c.w.wallet_with_sol(lamports);
    let sol = c.w.sol;
    let ix = token::transfer(
        donor.pubkey(),
        token::holding_address(&sol, &donor.pubkey()),
        token::holding_address(&sol, owner),
        sol,
        None,
        vec![],
        lamports,
    );
    c.w.env.send_paid_by(&[ix], &donor, &[]).ok();
}

/// `ixs` in one v0 transaction with the 22-address table and a compute limit.
fn bundle(c: &mut Coin, ixs: Vec<Instruction>) -> Tx {
    let cranker = c.cranker.insecure_clone();
    let table =
        c.w.env
            .put_lookup_table(Pubkey::new_unique(), &table_22(&c.w));
    let mut all = vec![compute_unit_limit(600_000)];
    all.extend(ixs);
    c.w.env
        .send_v0(&all, &cranker, &[], std::slice::from_ref(&table))
}

fn retire_game(c: &mut Coin) -> Tx {
    let ix = companion::retire_game(c.cranker.pubkey(), c.mint, c.hook);
    c.send(ix)
}

/// A dormant streak (past `retirable_at`, threshold 0.1 SOL) with holder A entered in the epoch
/// that just ended (`last`), now 30 s into `last + 1`; its pot just under the threshold, and a fee
/// claim that would lift it over: the launch's leftover fees, below the keeper's claim minimum
/// (`surplus` false), or bridged SOL anyone sent to the creator's holding (`surplus` true; the
/// keeper never claims it).
fn stalled_streak(surplus: bool) -> (Coin, Keypair, u32) {
    let mut c = Coin::new(GameKind::Streak);
    let a = c.buyer(SOL);
    c.volume(1, 2 * SOL);
    c.claim_fees().ok();
    while c.companion().pending_pot < 90_000_000 {
        c.volume(1, SOL / 4);
        c.claim_fees().ok();
    }
    let p = c.companion().pending_pot;
    assert!(p < MIN_POT, "pot {p}");
    let margin = 1_000_000;
    if surplus {
        let need = (MIN_POT + margin - p) * 10_000 / u64::from(POT_BPS) + 1;
        let creator = companion::creator_address(&c.mint);
        donate(&mut c, &creator, need);
        assert_eq!(unclaimed(&c), 0);
    } else {
        while fundable(&c) < MIN_POT + margin {
            c.volume(1, SOL / 20);
        }
        assert!(
            unclaimed(&c) < KEEPER_CLAIM_MIN,
            "the keeper would claim {}",
            unclaimed(&c)
        );
    }
    let launched = c.companion().launched_at;
    let retirable = c.game().retirable_at(launched);
    let last = round_of(retirable, EPOCH);
    c.warp_into(last, 30);
    c.enter(&[&a.pubkey()]);
    c.warp_into(last + 1, 30);
    assert!(c.w.env.now >= retirable);
    println!(
        "pot {p}, launch unclaimed {}, creator surplus {}, fundable {}, threshold {MIN_POT}",
        unclaimed(&c),
        creator_surplus(&c),
        fundable(&c)
    );
    (c, a, last)
}

/// R4-1 (keeper, streak): `prize_due` counts what a fee claim would bring, so a streak epoch a claim
/// would fund holds `retire` back (`DrawPending`) as the round-3 fix meant. But `close_epoch`
/// itself needs the pot already funded (`PotTooSmall`), and the keeper neither claims (the launch
/// holds less than its 0.02 SOL minimum, or the fee is a creator-holding surplus it never reads) nor
/// bundles a claim with `close_epoch` (it sends `close_epoch` only while `pendingPot` reaches the
/// threshold). Both its steps fail every pass until the epoch's last claim window, when `prize_due`
/// lets go and its `retire` sweeps the pot that claim + close_epoch would have shared: the
/// keeper itself voids the epoch the fix protects. Anyone sending claim_fees + close_epoch in one
/// transaction closes it (control half).
#[test]
fn poc_the_keeper_lets_a_fee_fundable_streak_epoch_lapse_then_retires_it() {
    for surplus in [false, true] {
        // What the keeper does: retire (refused), no close_epoch (pendingPot < threshold), no claim.
        let (mut c, _a, last) = stalled_streak(surplus);
        assert!(
            c.companion().pending_pot < MIN_POT,
            "keeper: close_epoch not due"
        );
        refused(&retire_game(&mut c), CompanionError::DrawPending);
        refused(&c.close_epoch(last), CompanionError::PotTooSmall);
        // Every pass the same, until the last claim window of `last + 1`.
        c.warp_into(last + 2, -(i64::from(CLAIM_WINDOW)) - 60);
        refused(&retire_game(&mut c), CompanionError::DrawPending);
        c.warp_into(last + 2, -(i64::from(CLAIM_WINDOW)) + 1);
        let pot = c.companion().pending_pot;
        let ev: PotRetired = retire_game(&mut c).event();
        assert_eq!(ev.lamports, pot);
        println!("surplus {surplus}: the keeper's retire swept {pot} at the epoch's last window");

        // What it should do: claim_fees + close_epoch in one transaction; A is then paid.
        let (mut c, a, last) = stalled_streak(surplus);
        let claim = companion::claim_fees_game(c.cranker.pubkey(), c.mint, c.hook);
        let close = companion::close_epoch(c.cranker.pubkey(), c.mint, c.hook, last);
        let tx = bundle(&mut c, vec![claim, close]);
        tx.ok();
        let closed: EpochClosed = tx.event();
        assert_eq!(closed.epoch, last);
        let paid: ShareClaimed = c.claim_share(last, &a.pubkey()).event();
        assert_eq!(paid.owner, a.pubkey());
        assert!(paid.share > 0);
    }
}

/// A jackpot past `retirable_at`, its pot funded, B's round over and due (B holds); earlier rounds
/// settled.
fn due_jackpot() -> (Coin, Keypair) {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let launched = c.companion().launched_at;
    let retirable = c.game().retirable_at(launched);
    c.warp_to(retirable - i64::from(TIMER) - 10);
    let b = c.buyer(SOL);
    c.claim_fees().ok();
    c.warp_to(retirable + 1);
    settle_until(&mut c, &b.pubkey());
    assert!(c.companion().pending_pot >= MIN_POT);
    (c, b)
}

/// R4-2 (keeper, info): a hook state the companion can't read (here: its round length rewritten,
/// as a hook that broke its own state would) makes every `settle` fail `HookState`, while
/// `prize_due` fails open and `retire` goes on (the design: "a state the hook broke owes nothing").
/// The keeper's parser (`kindStateOf`) checks neither the round length nor the mint, so it keeps
/// finding the round due, its settle keeps failing with an error that is not `FeesUnclaimed`, and
/// the round-3 `settleMissed` rule holds its `retire` back every pass, for ever (until a block).
#[test]
fn poc_a_broken_state_fails_every_settle_while_retire_would_go_on() {
    let (mut c, b) = due_jackpot();
    refused(&retire_game(&mut c), CompanionError::DrawPending);
    let key = bordrless_game::state_address(&c.hook, &c.mint).0;
    let mut acct = c.w.env.account(&key).unwrap();
    let at = bordrless_game::header_offsets::ROUND_SECS;
    acct.data[at..at + 4].copy_from_slice(&3_600u32.to_le_bytes());
    c.w.env.put(key, acct);
    // The keeper's view still parses and finds B's round due.
    let h = GameHeader::parse(&c.state_data()).unwrap();
    let r = bordrless_game::settle_round(
        &h,
        &c.jackpot(),
        c.game().paid_buys,
        c.game().timer_secs,
        c.w.env.now,
    )
    .unwrap();
    assert_eq!(r.buyer, b.pubkey());
    refused(&c.settle(&b.pubkey()), CompanionError::HookState);
    c.warp_to(c.w.env.now + 3_600);
    refused(&c.settle(&b.pubkey()), CompanionError::HookState);
    let pot = c.companion().pending_pot;
    let ev: PotRetired = retire_game(&mut c).event();
    assert_eq!(ev.lamports, pot);
}

/// A jackpot past `retirable_at` with its pot below the threshold (0.0549 SOL), every round closed.
fn small_pot_jackpot() -> Coin {
    let mut c = Coin::new(GameKind::Jackpot);
    c.volume(1, 2 * SOL);
    c.claim_fees().ok();
    let p = c.companion().pending_pot;
    assert!(p > 0 && p < MIN_POT, "pot {p}");
    let launched = c.companion().launched_at;
    let retirable = c.game().retirable_at(launched);
    c.warp_to(retirable + 1);
    c
}

/// Donations to the launch's and the creator's holdings never hold `retire` back when no round is
/// due: every round closed.
#[test]
fn control_donations_hold_no_retire_without_a_round_due() {
    let mut c = small_pot_jackpot();
    let stale = c.header().last_buyer;
    let _: JackpotForfeited = c.settle(&stale).event();
    let (launch_key, creator) = (
        launch::launch_address(&c.mint),
        companion::creator_address(&c.mint),
    );
    donate(&mut c, &launch_key, SOL / 10);
    donate(&mut c, &creator, SOL / 10);
    assert!(fundable(&c) >= MIN_POT);
    let pot = c.companion().pending_pot;
    let tx = retire_game(&mut c);
    tx.ok();
    println!("retire_game: {} bytes, {} CU", tx.size, tx.cu());
    let ev: PotRetired = tx.event();
    assert_eq!(ev.lamports, pot);
}

/// A round 30 days stale holds no `retire` back whatever a claim would bring, and settle then
/// forfeits it `Stale` though its buyer still holds.
#[test]
fn control_a_stale_round_holds_no_retire_whatever_is_fundable() {
    let mut c = Coin::new(GameKind::Jackpot);
    c.volume(1, 2 * SOL);
    c.claim_fees().ok();
    let launched = c.companion().launched_at;
    let retirable = c.game().retirable_at(launched);
    c.warp_to(retirable - SETTLE_GRACE_SECS - i64::from(TIMER) - 5);
    let b = c.buyer(SOL);
    c.claim_fees().ok();
    assert!(c.companion().pending_pot < MIN_POT);
    let creator = companion::creator_address(&c.mint);
    donate(&mut c, &creator, SOL / 5);
    c.warp_to(retirable + 1);
    assert!(fundable(&c) >= MIN_POT);
    let _: PotRetired = retire_game(&mut c).event();
    settle_until(&mut c, &b.pubkey());
    let f: JackpotForfeited = c.settle(&b.pubkey()).event();
    assert_eq!((f.buyer, f.reason), (b.pubkey(), ForfeitReason::Stale));
    assert!(c.balance(&b.pubkey()) > 0);
}

/// A donation that makes a round due (fundable over the threshold, pot under it) only makes the
/// settle need the claim: claim + settle + retire in one transaction closes the round and retires.
#[test]
fn control_a_donated_due_round_is_closed_by_claim_and_settle() {
    let mut c = small_pot_jackpot();
    let stale = c.header().last_buyer;
    let _: JackpotForfeited = c.settle(&stale).event();
    let b = c.buyer(SOL / 10);
    c.claim_fees().ok();
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let p = c.companion().pending_pot;
    assert!(p < MIN_POT);
    // Just enough that the estimate passes and the real share (less the claim's bounty) does not.
    let need = (MIN_POT - p) * 10_000 / u64::from(POT_BPS) + 2;
    let creator = companion::creator_address(&c.mint);
    donate(&mut c, &creator, need);
    assert!(fundable(&c) >= MIN_POT);
    refused(&retire_game(&mut c), CompanionError::DrawPending);
    refused(&c.settle(&b.pubkey()), CompanionError::FeesUnclaimed);
    let claim = companion::claim_fees_game(c.cranker.pubkey(), c.mint, c.hook);
    let settle = companion::settle(c.cranker.pubkey(), c.mint, c.hook, b.pubkey());
    let retire = companion::retire_game(c.cranker.pubkey(), c.mint, c.hook);
    let tx = bundle(&mut c, vec![claim, settle, retire]);
    tx.ok();
    let unfunded: JackpotUnfunded = tx.event();
    assert_eq!(unfunded.winner, b.pubkey());
    let _: PotRetired = tx.event();
    assert_eq!(c.companion().pending_pot, 0);
}

/// `prize_due`'s accounts: leaving one out fails only its own sender (`MissingAccount`); under a
/// block `retire` returns before it, so the plain `retire` still moves the pot.
#[test]
fn control_missing_accounts_fail_only_their_sender() {
    let (mut c, _b) = due_jackpot();
    let plain = companion::retire(c.cranker.pubkey(), c.mint, c.hook);
    refused(&c.send(plain.clone()), CompanionError::MissingAccount);
    let mut ix = companion::retire_game(c.cranker.pubkey(), c.mint, c.hook);
    ix.accounts.pop();
    refused(&c.send(ix), CompanionError::MissingAccount);
    let mut ix = companion::retire_game(c.cranker.pubkey(), c.mint, c.hook);
    ix.accounts.remove(ix.accounts.len() - 2);
    refused(&c.send(ix), CompanionError::MissingAccount);
    c.set_status(false, DEFAULT_POT_CAP, true).ok();
    let pot = c.companion().pending_pot;
    let ev: PotToBuyback = c.send(plain).event();
    assert_eq!(ev.lamports, pot);
}
