//! Phase-2 integration audit (round 1): PoCs and measurements. Helpers copied from
//! `companion_kinds.rs` (a test file is its own crate). Run:
//! `CARGO_INCREMENTAL=0 cargo test -p bordrless-program-tests --test audit_p2_integration -- --nocapture`
//! With `SBF_OUT_DIR` pointing at a directory whose `bordrless_companion.so` is built with the
//! heap probe (`custom-heap`), `heap_peak` reports each step's heap high-water mark.

#![allow(clippy::too_many_lines)]

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{AccountDeserialize, AccountSerialize, InstructionData, ToAccountMetas};
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{CreateArgs, CreateGameArgs, GameKindArgs, HookStatusArgs};
use bordrless_companion::state::{Companion, DrawStatus, Game, GameKind, Split};
use bordrless_core::policy;
use bordrless_game::{round_of, round_start, GameHeader, Slots};
use bordrless_launch::client as launch;
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::Tx;
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
const HEAP: u64 = 32 * 1024;

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
        GameKind::Lottery => unreachable!(),
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
    w
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

struct Coin {
    w: World,
    mint: Pubkey,
    hook: Pubkey,
    cranker: Keypair,
    launch_heap: Option<u64>,
    launch_cu: u64,
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
        let tx = w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]);
        tx.ok();
        let (launch_heap, launch_cu) = (heap_peak(&tx), tx.cu());
        w.env.warp(31);
        let cranker = w.wallet_with_sol(5 * SOL);
        Self {
            w,
            mint,
            hook,
            cranker,
            launch_heap,
            launch_cu,
        }
    }

    fn game(&self) -> Game {
        self.w.env.read(&companion::game_address(&self.mint))
    }

    #[allow(dead_code)]
    fn companion(&self) -> Companion {
        self.w.env.read(&companion::companion_address(&self.mint))
    }

    fn header(&self) -> GameHeader {
        let data = self
            .w
            .env
            .account(&bordrless_game::state_address(&self.hook, &self.mint).0)
            .unwrap()
            .data;
        GameHeader::read(&data, &self.mint).unwrap()
    }

    fn slots(&self, owner: &Pubkey) -> Slots {
        Slots::decode(&self.w.env.hook_data(&self.mint, owner))
    }

    fn send(&mut self, ix: Instruction) -> Tx {
        let cranker = self.cranker.insecure_clone();
        self.w.env.send_paid_by(&[ix], &cranker, &[])
    }

    fn send_by(&mut self, who: &Keypair, ix: Instruction) -> Tx {
        self.w.env.send_paid_by(&[ix], who, &[])
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

    fn epoch(&self) -> u32 {
        round_of(self.w.env.now, EPOCH)
    }

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

fn report(name: &str, tx: &Tx) {
    let heap = heap_peak(tx);
    println!(
        "{name}: CU {} size {} height {} trace {} heap {:?}",
        tx.cu(),
        tx.size,
        tx.max_height(),
        tx.trace_len(),
        heap
    );
    assert!(
        tx.cu() < 200_000,
        "{name} fits the default 200k per instruction"
    );
    if let Some(h) = heap {
        assert!(h < HEAP / 2, "{name} heap {h} under half of 32 KiB");
    }
}

// ================================================================================================
// PoC (Low): under a block, `claim_share` succeeds for ANY epoch and ANY owner and makes the
// receipt `["claimed", game, epoch, owner]` before it checks the epoch. A block is lifted only by an
// audit (`set_hook_status`): the receipts made during the block for epochs to come then deny those
// owners their shares, and their rent comes back to whoever made them once those epochs end.
// ================================================================================================

/// Fails on purpose while the binary under test lets it happen (`target/deploy` built before the
/// fix in `process_claim_share`); passes once a block can no longer make a receipt for an epoch
/// whose claims are not open.
#[test]
fn poc_a_receipt_made_under_a_block_denies_a_future_share_after_the_audit() {
    let mut c = Coin::new(GameKind::Streak);
    let a = c.buyer(5 * SOL);
    let b = c.buyer(SOL);
    let e = c.epoch() + 1;
    // A pot, then the protocol blocks the hook (say, while it reviews it).
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    c.set_status(false, DEFAULT_POT_CAP, true).ok();
    // The first step after the block moves the pot (the keeper would send `retire`): a griefer
    // front-runs it with `claim_share` for a FUTURE epoch and A.
    let griefer = c.w.wallet_with_sol(SOL);
    let ix = companion::claim_share(griefer.pubkey(), c.mint, c.hook, e, a.pubkey());
    let tx = c.send_by(&griefer, ix);
    let receipt = companion::receipt_address(&c.mint, e, &a.pubkey());
    if tx.result.is_ok() {
        assert!(c.w.env.account(&receipt).is_some());
        // The protocol audits the hook: the block is lifted.
        c.set_status(true, 0, false).ok();
        // A and B hold through epoch e, are entered, the pot is funded again.
        c.warp_into(e, 60);
        c.enter(&[&a.pubkey(), &b.pubkey()]);
        assert!(c.slots(&a.pubkey()).range_in(e).is_some());
        c.volume(2, 20 * SOL);
        c.claim_fees().ok();
        c.warp_into(e + 1, 30);
        c.close_epoch(e).ok();
        let g = c.game();
        assert_eq!((g.status, g.round), (DrawStatus::Revealed, e));
        // A has a share of e, but its claim is refused: the receipt exists.
        let tx = c.claim_share(e, &a.pubkey());
        tx.expect_fail();
        assert!(tx.logs().iter().any(|l| l.contains("already in use")));
        c.claim_share(e, &b.pubkey()).ok();
        // Once e's claims end, the griefer takes the receipt's rent back.
        c.warp_into(e + 2, 0);
        let ix = companion::close_receipt(c.mint, e, a.pubkey(), griefer.pubkey());
        c.send(ix).ok();
        panic!("FINDING: a receipt made under a block for epoch {e} denied A its share after the audit");
    }
    assert!(
        c.w.env.account(&receipt).is_none(),
        "no receipt for an epoch not open"
    );
}

// ================================================================================================
// Measurements: CU, heap (with the probe build), size, height and trace of the new steps and of a
// Studio game's launch.
// ================================================================================================

#[test]
fn measure_the_new_steps() {
    // Jackpot: the launch, a settle that pays.
    let mut c = Coin::new(GameKind::Jackpot);
    println!(
        "jackpot launch: CU {} heap {:?}",
        c.launch_cu, c.launch_heap
    );
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    let b = c.buyer(SOL);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let ix = companion::settle(c.cranker.pubkey(), c.mint, c.hook, b.pubkey());
    let tx = c.send(ix);
    tx.ok();
    let _: JackpotPaid = tx.event();
    report("settle (paid)", &tx);

    // Streak: the launch, close_epoch, claim_share, close_receipt, enter.
    let mut c = Coin::new(GameKind::Streak);
    println!("streak launch: CU {} heap {:?}", c.launch_cu, c.launch_heap);
    let a = c.buyer(5 * SOL);
    let e = c.epoch() + 1;
    c.warp_into(e, 60);
    let tx = c.send(enter_ix(c.mint, a.pubkey()));
    tx.ok();
    println!(
        "enter: CU {} size {} height {} trace {}",
        tx.cu(),
        tx.size,
        tx.max_height(),
        tx.trace_len()
    );
    c.volume(2, 20 * SOL);
    let tx = c.claim_fees();
    tx.ok();
    report("claim_fees (streak game)", &tx);
    c.warp_into(e + 1, 30);
    let tx = c.close_epoch(e);
    tx.ok();
    report("close_epoch", &tx);
    let tx = c.claim_share(e, &a.pubkey());
    tx.ok();
    report("claim_share", &tx);
    c.warp_into(e + 2, 0);
    let ix = companion::close_receipt(c.mint, e, a.pubkey(), c.cranker.pubkey());
    let tx = c.send(ix);
    tx.ok();
    println!("close_receipt: CU {} size {}", tx.cu(), tx.size);
    // close_epoch of e + 1 releasing e (EpochEnded) with no weight: rolls over.
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    c.warp_into(e + 2, 60);
    let tx = c.close_epoch(e + 1);
    tx.ok();
    report("close_epoch (release + rollover)", &tx);
}

// ================================================================================================
// Live accounts: a phase-1 Companion and Game (the vectors' bytes, unchanged since mainnet) read
// under the phase-2 types as a lottery with nothing locked and zero kind fields, and write back
// byte for byte.
// ================================================================================================

fn vector_hex(json: &str, key: &str) -> Vec<u8> {
    let at = json.find(&format!("\"{key}\": \"")).expect(key) + key.len() + 5;
    let end = json[at..].find('"').unwrap() + at;
    let hex = &json[at..end];
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn phase1_accounts_read_the_same() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/vectors/companion-games.json");
    let json = std::fs::read_to_string(path).unwrap();
    // The phase-1 "accounts" section comes first in the file; "phase2" after it.
    let p1 = &json[json.find("\"accounts\": {").unwrap()..json.find("\"standard\": {").unwrap()];
    let comp = vector_hex(p1, "companion");
    let game = vector_hex(p1, "game");
    assert_eq!((comp.len(), game.len()), (300, 486));
    let c = Companion::try_deserialize(&mut &comp[..]).unwrap();
    assert_eq!(
        (c.game_kind, c.pot_locked, c.reserved),
        (GameKind::Lottery, 0, [0])
    );
    let mut back = Vec::new();
    c.try_serialize(&mut back).unwrap();
    assert_eq!(back, comp);
    let g = Game::try_deserialize(&mut &game[..]).unwrap();
    assert_eq!(g.kind, GameKind::Lottery);
    assert_eq!(
        (
            g.timer_secs,
            g.min_tokens,
            g.paid_buys,
            g.min_streak_secs,
            g.min_weight,
            g.epoch_paid
        ),
        (0, 0, 0, 0, 0, 0)
    );
    let mut back = Vec::new();
    g.try_serialize(&mut back).unwrap();
    assert_eq!(back, game);
    let _ = AccountMeta::new_readonly(Pubkey::default(), false);
    let _ = CompanionError::NoShare;
}

// ================================================================================================
// Sizes with the protocol's 22-address table: the launch (longest metadata), and the keeper's
// batches (8 Studio `enter`s a transaction, 8 `close_receipt`s), plus a claim with a compute
// price (what the keeper sends).
// ================================================================================================

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

#[test]
fn measure_sizes_with_the_table() {
    use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price};
    for kind in [GameKind::Jackpot, GameKind::Streak] {
        let mut w = world();
        let table = w.env.put_lookup_table(Pubkey::new_unique(), &table_22(&w));
        let launcher = w.wallet_with_sol(50 * SOL);
        let mint_kp = Keypair::new();
        let mint = mint_kp.pubkey();
        let tx = w.env.send_v0(
            &setup_ixs(&launcher.pubkey(), &mint, kind),
            &launcher,
            &[&mint_kp],
            std::slice::from_ref(&table),
        );
        tx.ok();
        println!("{kind:?} setup v0: {} B", tx.size);
        let hook = game_args(kind).0.hook;
        let config = hook_config(&mut w, &launcher, hook, kind.hook_flags());
        let ixs = [
            compute_unit_limit(1_400_000),
            compute_unit_price(20_000),
            launch_ix(&w, &launcher.pubkey(), &mint, &config),
        ];
        let tx = w
            .env
            .send_v0(&ixs, &launcher, &[&mint_kp], std::slice::from_ref(&table));
        tx.ok();
        println!(
            "{kind:?} launch v0: {} B, trace {}, height {}, CU {}",
            tx.size,
            tx.trace_len(),
            tx.max_height(),
            tx.cu()
        );
        assert!(tx.size <= 1_232 && tx.max_height() <= 5 && tx.trace_len() <= 64);
        let cranker = w.wallet_with_sol(SOL);
        let owners: Vec<Pubkey> = (0..8).map(|_| Pubkey::new_unique()).collect();
        let mut enters = vec![compute_unit_limit(300_000), compute_unit_price(20_000)];
        enters.extend(owners.iter().map(|o| enter_ix(mint, *o)));
        let size = w
            .env
            .v0_size(&enters, &cranker, &[], std::slice::from_ref(&table));
        println!("{kind:?} 8 Studio enters v0: {size} B");
        let mut closes = vec![compute_unit_limit(300_000), compute_unit_price(20_000)];
        closes.extend(
            owners
                .iter()
                .map(|o| companion::close_receipt(mint, 7, *o, cranker.pubkey())),
        );
        let size = w
            .env
            .v0_size(&closes, &cranker, &[], std::slice::from_ref(&table));
        println!("{kind:?} 8 close_receipts v0: {size} B");
        let claim = [
            compute_unit_limit(300_000),
            compute_unit_price(20_000),
            companion::claim_share(cranker.pubkey(), mint, hook, 7, owners[0]),
            companion::claim_share(cranker.pubkey(), mint, hook, 7, owners[1]),
        ];
        let size = w
            .env
            .v0_size(&claim, &cranker, &[], std::slice::from_ref(&table));
        println!("{kind:?} 2 claim_shares in one v0: {size} B");
    }
}

/// What a Studio game hook's launch weighs with more registry extras than the starters' one (the
/// state; the launch is in every launch anyway): `create_game` takes up to `MAX_GAME_HOOK_EXTRAS`
/// (3). Each extra here is a new address (a hook-own PDA such as a config or a stats account).
#[test]
fn measure_the_launch_with_more_hook_extras() {
    use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price};
    let mut w = world();
    let table = w.env.put_lookup_table(Pubkey::new_unique(), &table_22(&w));
    let launcher = w.wallet_with_sol(50 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    w.env
        .send_v0(
            &setup_ixs(&launcher.pubkey(), &mint, GameKind::Streak),
            &launcher,
            &[&mint_kp],
            std::slice::from_ref(&table),
        )
        .ok();
    let config = hook_config(&mut w, &launcher, STREAK, GameKind::Streak.hook_flags());
    let c = w.launch_config(&config);
    for added in 0..=2usize {
        let mut custom = w.custom_hook_accounts(&STREAK, &mint);
        for _ in 0..added {
            custom
                .extras
                .push(AccountMeta::new_readonly(Pubkey::new_unique(), false));
        }
        let mut args = World::launch_args("GAMEXXXXXX", c.creator_fee_bps, VQ, c.rules);
        args.name = "N".repeat(32);
        args.uri = format!("https://gateway.pinata.cloud/ipfs/{}", "b".repeat(94));
        let inner = launch::create_launch_with(
            companion::creator_address(&mint),
            mint,
            w.env.treasury.pubkey(),
            w.sol,
            policy::LP_FEE_BPS,
            args.clone(),
            Some(config),
            Some(&custom),
        );
        let ixs = [
            compute_unit_limit(1_400_000),
            compute_unit_price(20_000),
            companion::launch(launcher.pubkey(), mint, &inner, args),
        ];
        let size = w
            .env
            .v0_size(&ixs, &launcher, &[&mint_kp], std::slice::from_ref(&table));
        println!(
            "launch with {} registry extras besides the launch: {size} B (limit 1,232)",
            1 + added
        );
    }
}
