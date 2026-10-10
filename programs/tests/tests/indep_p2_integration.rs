//! Independent phase-2 audit (integration and limits lens). Helpers copied from `audit_p2_r4.rs`
//! (itself from `companion_kinds.rs`). `poc_*` assert the outcome they document (they pass while
//! the issue stands); `measure_*` print limits.
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

// =============================================================================== independent

/// `ixs` in one v0 transaction with the 22-address table, a compute limit and a compute price (as
/// the keeper's `compileTx` sends it on mainnet).
fn keeper_bundle(c: &mut Coin, ixs: Vec<Instruction>) -> Tx {
    let cranker = c.cranker.insecure_clone();
    let table =
        c.w.env
            .put_lookup_table(Pubkey::new_unique(), &table_22(&c.w));
    let mut all = vec![compute_unit_limit(600_000), compute_unit_price(1)];
    all.extend(ixs);
    c.w.env
        .send_v0(&all, &cranker, &[], std::slice::from_ref(&table))
}

/// A jackpot whose pot is funded and whose current round's last buyer is `a` (rounds before it
/// settled).
fn jackpot_with_buyer() -> (Coin, Keypair) {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let a = c.buyer(SOL);
    c.claim_fees().ok();
    settle_until(&mut c, &a.pubkey());
    assert_eq!(c.header().last_buyer, a.pubkey());
    (c, a)
}

/// FIXED (log-p2-fix.md, finding 2; was "Keeper latency (Medium)"): before the fix a jackpot round
/// was lost for good one timer after it could first be settled: an attacker who bought right after
/// A's timer ran out (round B) and again right after B's (round C) wiped A's round and was paid B's
/// prize. The hook now remembers the last 8 ended rounds and `settle` pays the oldest first: A's
/// round is still the oldest open one, A is paid, then the attacker's B.
#[test]
fn poc_a_round_is_forgotten_one_timer_after_it_is_over() {
    let (mut c, a) = jackpot_with_buyer();
    let t_a = c.header().last_buy_at;
    let a_round = c.jackpot().buys;
    let timer = i64::from(TIMER);
    // A's round is over at t_a + T: the attacker buys at once (round B).
    c.warp_to(t_a + timer);
    let m = c.buyer(SOL);
    assert_eq!(
        c.jackpot().ended_buyer,
        a.pubkey(),
        "A's round kept as the ended one"
    );
    // Nobody settles for one timer; the attacker buys again once B's timer has run out (round C).
    c.warp_to(t_a + 2 * timer);
    c.buy(&m, SOL).ok();
    let (h, j, g) = (c.header(), c.jackpot(), c.game());
    let r = bordrless_game::settle_round(&h, &j, g.paid_buys, g.timer_secs, c.w.env.now).unwrap();
    assert_eq!(
        (r.buyer, r.number),
        (a.pubkey(), a_round),
        "A's is still the oldest open round"
    );
    assert!(
        c.balance(&a.pubkey()) > 0 && jackpot_mark(&c.slots(&a.pubkey())) > 0,
        "A still holds everything it bought"
    );
    // A settle naming the attacker is refused; A is paid, then the attacker's round B.
    refused(&c.settle(&m.pubkey()), CompanionError::WrongHolding);
    let paid: JackpotPaid = c.settle(&a.pubkey()).event();
    assert_eq!((paid.winner, paid.round), (a.pubkey(), a_round));
    let paid: JackpotPaid = c.settle(&m.pubkey()).event();
    assert_eq!((paid.winner, paid.round), (m.pubkey(), a_round + 1));
}

/// Control: the same buys, with A's round settled within the timer, pay A first.
#[test]
fn control_a_round_settled_within_one_timer_is_paid() {
    let (mut c, a) = jackpot_with_buyer();
    let t_a = c.header().last_buy_at;
    let timer = i64::from(TIMER);
    c.warp_to(t_a + timer);
    let m = c.buyer(SOL);
    c.warp_to(t_a + 2 * timer - 1);
    let paid: JackpotPaid = c.settle(&a.pubkey()).event();
    assert_eq!(paid.winner, a.pubkey());
    c.warp_to(t_a + 2 * timer);
    c.buy(&m, SOL).ok();
    let paid: JackpotPaid = c.settle(&m.pubkey()).event();
    assert_eq!(paid.winner, m.pubkey());
}

/// The keeper's bundles as it sends them on mainnet (v0, the 22-address table, a compute limit and
/// a price): `claim_fees + settle` (FeesUnclaimed fallback) and `claim_fees + close_epoch`
/// (PotTooSmall fallback, new in round 4 and not in the docs' table), and `retire_game` (v0).
#[test]
fn measure_the_keepers_bundles() {
    // claim_fees + settle.
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    c.volume(2, 5 * SOL);
    let a = c.buyer(SOL);
    settle_until(&mut c, &a.pubkey());
    assert!(unclaimed(&c) > 0);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let claim = companion::claim_fees_game(c.cranker.pubkey(), c.mint, c.hook);
    let settle = companion::settle(c.cranker.pubkey(), c.mint, c.hook, a.pubkey());
    let ixs = vec![
        compute_unit_limit(600_000),
        compute_unit_price(1),
        claim.clone(),
        settle.clone(),
    ];
    let table =
        c.w.env
            .put_lookup_table(Pubkey::new_unique(), &table_22(&c.w));
    let cranker = c.cranker.insecure_clone();
    let size =
        c.w.env
            .v0_size(&ixs, &cranker, &[], std::slice::from_ref(&table));
    let tx = keeper_bundle(&mut c, vec![claim, settle]);
    tx.ok();
    println!("claim_fees + settle: {size} B, {} CU", tx.cu());
    assert!(size <= PACKET);

    // claim_fees + close_epoch (an entered holder, fees unclaimed).
    let mut c = Coin::new(GameKind::Streak);
    let h = c.buyer(SOL);
    let e = c.epoch() + 1;
    c.warp_into(e, 30);
    c.enter(&[&h.pubkey()]);
    c.volume(3, 10 * SOL);
    c.warp_into(e + 1, 30);
    let claim = companion::claim_fees_game(c.cranker.pubkey(), c.mint, c.hook);
    let close = companion::close_epoch(c.cranker.pubkey(), c.mint, c.hook, e);
    let ixs = vec![
        compute_unit_limit(600_000),
        compute_unit_price(1),
        claim.clone(),
        close.clone(),
    ];
    let table =
        c.w.env
            .put_lookup_table(Pubkey::new_unique(), &table_22(&c.w));
    let cranker = c.cranker.insecure_clone();
    let size =
        c.w.env
            .v0_size(&ixs, &cranker, &[], std::slice::from_ref(&table));
    let tx = keeper_bundle(&mut c, vec![claim, close]);
    tx.ok();
    let closed: EpochClosed = tx.event();
    assert_eq!(closed.epoch, e);
    println!("claim_fees + close_epoch: {size} B, {} CU", tx.cu());
    assert!(size <= PACKET);

    // retire_game, v0 with the table.
    let ix = companion::retire_game(c.cranker.pubkey(), c.mint, c.hook);
    let ixs = vec![compute_unit_limit(200_000), compute_unit_price(1), ix];
    let size =
        c.w.env
            .v0_size(&ixs, &cranker, &[], std::slice::from_ref(&table));
    println!("retire_game: {size} B (v0)");
}

// =============================================================================== finding 5

/// The site's longest metadata URI (an IPFS gateway URL), as `measure_the_launch_with_more_hook_extras`.
fn longest_uri() -> String {
    format!("https://gateway.pinata.cloud/ipfs/{}", "b".repeat(94))
}

/// The hook's registry for `mint` rewritten to the starter's (the state, the launch) plus `more`
/// fixed keys (a hook's own config or stats accounts), or to `list` as given.
fn put_registry(
    w: &mut World,
    hook: &Pubkey,
    mint: &Pubkey,
    list: &bordrless_hook::HookAccountList,
) {
    let key = bordrless_hook::hook_accounts_address(hook, mint).0;
    let data = list.encode();
    let lamports = w.env.rent(data.len());
    let mut acc = w.env.account(&key).expect("the registry prepare wrote");
    acc.data = data;
    acc.lamports = lamports;
    w.env.put(key, acc);
}

fn registry_with(more: usize) -> bordrless_hook::HookAccountList {
    let mut list = studio_streak::extra_accounts();
    for _ in 0..more {
        list.accounts.push(bordrless_hook::ExtraAccount {
            writable: false,
            source: bordrless_hook::AccountSource::Key(Pubkey::new_unique()),
        });
    }
    list
}

/// A streak coin set up (`create`, the starter's `prepare`) with its registry rewritten to `list`;
/// answers the world, the launcher, the mint's keypair and whether `create_game_v2` landed (its
/// transaction).
fn setup_with_registry(list: &bordrless_hook::HookAccountList) -> (World, Keypair, Keypair, Tx) {
    let mut w = world();
    let launcher = w.wallet_with_sol(50 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let ixs = setup_ixs(&launcher.pubkey(), &mint, GameKind::Streak);
    w.env.send_paid_by(&ixs[..2], &launcher, &[&mint_kp]).ok();
    put_registry(&mut w, &STREAK, &mint, list);
    let tx = w.env.send_paid_by(&ixs[2..], &launcher, &[&mint_kp]);
    (w, launcher, mint_kp, tx)
}

/// The companion's launch of `mint` at the site's longest name and URI, v0 with the protocol's
/// table, a compute limit and a price: its size, and the transaction.
fn long_launch(w: &mut World, launcher: &Keypair, mint_kp: &Keypair) -> (usize, Tx) {
    let mint = mint_kp.pubkey();
    let config = hook_config(w, launcher, STREAK, GameKind::Streak.hook_flags());
    let table = w.env.put_lookup_table(Pubkey::new_unique(), &table_22(w));
    let ixs = [
        compute_unit_limit(1_400_000),
        compute_unit_price(20_000),
        launch_ix(w, &launcher.pubkey(), &mint, &config, &longest_uri()),
    ];
    let size = w
        .env
        .v0_size(&ixs, launcher, &[mint_kp], std::slice::from_ref(&table));
    assert!(size <= PACKET, "the launch is {size} B, over a packet");
    let tx = w
        .env
        .send_v0(&ixs, launcher, &[mint_kp], std::slice::from_ref(&table));
    (size, tx)
}

/// FIXED (log-p2-fix.md, finding 5): `create_game_v2` takes a registry with at most 2 extras besides
/// the launch (`MAX_GAME_HOOK_EXTRAS_V2`), and such a coin's launch fits a packet at the site's
/// longest name and URI; 3 extras (which do not fit) are refused.
#[test]
fn a_kind_hook_registry_of_two_extras_fits_and_three_are_refused() {
    assert_eq!(MAX_GAME_HOOK_EXTRAS_V2, 2);
    // The starter's registry (the state) plus one more: 2 extras besides the launch.
    let (mut w, launcher, mint_kp, tx) = setup_with_registry(&registry_with(1));
    tx.ok();
    let (size, tx) = long_launch(&mut w, &launcher, &mint_kp);
    println!("launch with 2 extras besides the launch, longest name and URI: {size} B");
    assert!(size <= PACKET, "{size} B");
    tx.ok();
    assert_eq!(w.launch(&mint_kp.pubkey()).custom_hook, Some(STREAK));
    // Plus two more: 3 extras besides the launch, refused.
    let (_, _, _, tx) = setup_with_registry(&registry_with(2));
    refused(&tx, CompanionError::TooManyHookExtras);
}

/// FIXED (finding 5): a hook that rewrites its registry to 3 extras after `create_game_v2` is
/// refused at the launch (where the registry is checked again for a jackpot or a streak).
#[test]
fn a_registry_grown_after_create_game_v2_is_refused_at_the_launch() {
    let (mut w, launcher, mint_kp, tx) = setup_with_registry(&registry_with(1));
    tx.ok();
    let mint = mint_kp.pubkey();
    put_registry(&mut w, &STREAK, &mint, &registry_with(2));
    let config = hook_config(&mut w, &launcher, STREAK, GameKind::Streak.hook_flags());
    let table = w.env.put_lookup_table(Pubkey::new_unique(), &table_22(&w));
    let ixs = [
        compute_unit_limit(1_400_000),
        launch_ix(&w, &launcher.pubkey(), &mint, &config, "https://x.y/z"),
    ];
    let tx = w
        .env
        .send_v0(&ixs, &launcher, &[&mint_kp], std::slice::from_ref(&table));
    refused(&tx, CompanionError::TooManyHookExtras);
}

/// FIXED (log-p2-fix.md, finding 1, the same pattern): a registry is a hook-owned account the
/// companion decodes onto its heap; one out of the companion's bounds (here a PDA with 400 one-byte
/// seeds, which Borsh would grow into vectors of 28 KiB and more) is refused as `HookRegistry`
/// before it is decoded, never an abort out of memory.
#[test]
fn a_registry_out_of_bounds_is_refused_not_an_abort() {
    let mut list = studio_streak::extra_accounts();
    list.accounts.push(bordrless_hook::ExtraAccount {
        writable: false,
        source: bordrless_hook::AccountSource::Pda {
            program: STREAK,
            seeds: vec![bordrless_hook::Seed::SourceOwner; 400],
        },
    });
    let (_, _, _, tx) = setup_with_registry(&list);
    refused(&tx, CompanionError::HookRegistry);
    assert!(!tx
        .logs()
        .iter()
        .any(|l| l.to_lowercase().contains("out of memory")));
}
