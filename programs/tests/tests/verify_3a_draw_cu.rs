#![allow(dead_code, unused_imports)]
//! Verifier PoC (read-only verification of the 3a fixes): what a lottery draw costs once its hook carries a hashed audit and the code must be rehashed (after a v2 audit, an upgrade or anyone's ExtendProgram), against the keeper's fixed DRAW_UNITS (250k, sent without a dry run).

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::InstructionData;
use bordrless_companion::client::{self as companion, SeedSlot};
use bordrless_companion::constants::*;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{CreateArgs, CreateGameArgs, HookStatusArgs};
use bordrless_companion::oracle as seeds;
use bordrless_companion::state::{
    spot_price, Companion, DrawStatus, Game, GameKind, HookStatus, Split,
};
use bordrless_core::policy;
use bordrless_game::{draw_index, round_of, GameHeader, Range, Slots};
use bordrless_launch::client::{self as launch, CustomHookAccounts, LaunchKeys};
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price, Env, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::orao;
use bordrless_token::client as token;
use bordrless_token::state::Mint;
use lottery_hook::client as lottery;
use solana_keypair::Keypair;
use solana_signer::Signer;

/// An hour a round, the shortest the standard allows. `T0` starts a round.
const ROUND: u32 = 3_600;
const R: i64 = ROUND as i64;
const CREATOR_FEE: u16 = 200;
const BOUNTY_BPS: u16 = 50;
/// 70% of every claim to the pot, 30% to the buyback.
const SPLIT: Split = Split {
    buyback_bps: 3_000,
    holders_bps: 0,
    beneficiary_bps: 0,
};
const POT_BPS: u16 = 7_000;
const MIN_POT: u64 = 100_000_000;
/// Six attempts of 5 minutes: half a round, the most `create_game` allows, so a draw made early in
/// the round after its own keeps every attempt before its claims end with that round.
const WINDOW: u32 = 300;
const ATTEMPTS: u8 = 6;
const HOOK: Pubkey = lottery_hook::ID;

fn code(e: CompanionError) -> u32 {
    u32::from(e)
}

/// Refused with the companion's own error, not one of ORAO's that shares its number (ORAO's codes
/// are 6000 to 6009 too).
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

fn game_args() -> CreateGameArgs {
    CreateGameArgs {
        kind: GameKind::Lottery,
        hook: HOOK,
        split: SPLIT,
        pot_bps: POT_BPS,
        round_secs: ROUND,
        min_pot: MIN_POT,
        prize_bps: 10_000,
        claim_window_secs: WINDOW,
        max_attempts: ATTEMPTS,
    }
}

/// A plain `LaunchConfig` naming the lottery hook with `flags` (no kit rules, creator fee 2%).
fn hook_config(w: &mut World, creator: &Keypair, hook: Pubkey, flags: u16) -> Pubkey {
    let (config, tx) = w.create_config(
        creator,
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: CREATOR_FEE,
            custom_hook: Some(hook),
            custom_hook_flags: flags,
            label: "Lottery".to_string(),
        },
    );
    tx.ok();
    config
}

/// The companion's `launch` of `mint` from `config`, its custom hook's accounts as the client
/// resolves them (none for a config without one).
fn launch_ix(w: &World, launcher: &Pubkey, mint: &Pubkey, config: &Pubkey) -> Instruction {
    let c = w.launch_config(config);
    let custom = c.custom_hook.map(|h| w.custom_hook_accounts(&h, mint));
    let mut args = World::launch_args("LOTTO", c.creator_fee_bps, VQ, c.rules);
    args.name = "Lottery".to_string();
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

/// The companion, the hook prepared for the mint and the game, in one transaction as the backend
/// sends it (the mint signs all three).
fn setup_ixs(
    launcher: &Pubkey,
    mint: &Pubkey,
    c: CreateArgs,
    g: CreateGameArgs,
) -> [Instruction; 3] {
    [
        companion::create(*launcher, *launcher, *mint, c),
        lottery::prepare(*launcher, *mint, g.round_secs),
        companion::create_game(*launcher, *mint, g),
    ]
}

/// Writes the slot hashes sysvar's entries, the newest first, as the runtime keeps them.
fn set_slot_hashes(env: &mut Env, entries: &[SeedSlot]) {
    let mut sysvar = env
        .account(&seeds::SLOT_HASHES)
        .expect("the slot hashes sysvar");
    sysvar.data[..8].copy_from_slice(&(entries.len() as u64).to_le_bytes());
    for (i, e) in entries.iter().enumerate() {
        let at = 8 + i * 40;
        sysvar.data[at..at + 8].copy_from_slice(&e.slot.to_le_bytes());
        sysvar.data[at + 8..at + 40].copy_from_slice(&e.hash);
    }
    env.put(seeds::SLOT_HASHES, sysvar);
}

/// A slot's hash in these tests: it changes with every slot, and nobody chooses it.
fn hash_of(slot: u64) -> [u8; 32] {
    let mut from = [0u8; 32];
    from[..8].copy_from_slice(&slot.to_le_bytes());
    orao::randomness_for(&from)[..32].try_into().unwrap()
}

/// Writes the slot hashes sysvar with its first entry the current slot's parent. Answers that
/// entry, as the keeper's draw names it.
fn set_slot_hash(env: &mut Env) -> SeedSlot {
    let slot = env.slot - 1;
    let at = SeedSlot {
        slot,
        hash: hash_of(slot),
    };
    set_slot_hashes(env, &[at]);
    at
}

/// ORAO's legacy v1 `request` of `seed`, `payer` paying: anyone can still send it, and it makes its
/// account at the very address a v2 request for the seed would use.
fn v1_request_ix(payer: Pubkey, seed: [u8; 32]) -> Instruction {
    let mut data = vec![46, 101, 67, 11, 76, 137, 12, 173]; // sha256("global:request")[..8]
    data.extend_from_slice(&seed);
    Instruction {
        program_id: orao::ORAO_VRF_ID,
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(orao::NETWORK_STATE, false),
            AccountMeta::new(orao::TREASURY, false),
            AccountMeta::new(orao::request_address(&seed), false),
            AccountMeta::new_readonly(bordrless_program_tests::SYSTEM_PROGRAM_ID, false),
        ],
        data,
    }
}

/// ORAO's answer to a v1 request: the 64 random bytes after the seed (as all 29,672 v1 requests on
/// mainnet hold theirs).
fn fulfil_v1(env: &mut Env, seed: &[u8; 32], randomness: &[u8; 64]) {
    let key = orao::request_address(seed);
    let mut account = env.account(&key).expect("a v1 request");
    assert_eq!(account.data[..8], [188, 96, 216, 248, 93, 94, 49, 112]);
    assert_eq!(&account.data[8..40], &seed[..]);
    account.data[40..104].copy_from_slice(randomness);
    env.put(key, account);
}

/// A randomness whose attempts all land where `want(attempt, ticket)` says, for a round of
/// `total` tickets: searched among `orao::randomness_for` outputs, so it is repeatable.
fn randomness_where(total: u64, want: impl Fn(u8, u64) -> bool) -> [u8; 64] {
    for i in 0u64..2_000_000 {
        let mut seed = [7u8; 32];
        seed[..8].copy_from_slice(&i.to_le_bytes());
        let r = orao::randomness_for(&seed);
        if (0..ATTEMPTS).all(|k| want(k, draw_index(&r, u32::from(k), total).unwrap())) {
            return r;
        }
    }
    panic!("no randomness found");
}

/// A lottery coin launched through its companion, past the sniper window.
struct Lotto {
    w: World,
    launcher: Keypair,
    mint: Pubkey,
    custom: CustomHookAccounts,
    cranker: Keypair,
    round_secs: u32,
}

impl Lotto {
    fn new() -> Self {
        Self::with(|_| {}, |_| {})
    }

    fn with(c: impl FnOnce(&mut CreateArgs), g: impl FnOnce(&mut CreateGameArgs)) -> Self {
        let mut w = World::new();
        orao::load(&mut w.env);
        let launcher = w.wallet_with_sol(50 * SOL);
        let mint_kp = Keypair::new();
        let mint = mint_kp.pubkey();
        let (mut ca, mut ga) = (create_args(), game_args());
        c(&mut ca);
        g(&mut ga);
        let round_secs = ga.round_secs;
        let tx = w.env.send_paid_by(
            &setup_ixs(&launcher.pubkey(), &mint, ca, ga),
            &launcher,
            &[&mint_kp],
        );
        tx.ok();
        let ev: GameCreated = tx.event();
        assert_eq!(ev.first_round, round_of(w.env.now, round_secs));
        let config = hook_config(&mut w, &launcher, HOOK, lottery_hook::FLAGS);
        let ix = launch_ix(&w, &launcher.pubkey(), &mint, &config);
        w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]).ok();
        assert_eq!(w.launch(&mint).custom_hook, Some(HOOK));
        let custom = CustomHookAccounts {
            program: HOOK,
            extras: lottery::extras(&mint),
        };
        assert_eq!(custom, w.custom_hook_accounts(&HOOK, &mint));
        w.env.warp(31);
        let cranker = w.wallet_with_sol(SOL);
        Self {
            w,
            launcher,
            mint,
            custom,
            cranker,
            round_secs,
        }
    }

    fn round(&self) -> u32 {
        round_of(self.w.env.now, self.round_secs)
    }

    /// To `secs` into round `round`.
    fn warp_into(&mut self, round: u32, secs: i64) {
        let t = i64::from(round) * i64::from(self.round_secs) + secs;
        assert!(t >= self.w.env.now, "the clock never goes back");
        self.w.env.warp(t - self.w.env.now);
    }

    /// The keeper's `enter` for each of `holders` (the cranker pays).
    fn enter(&mut self, holders: &[&Keypair]) {
        for h in holders {
            let ix = lottery::enter(self.mint, h.pubkey());
            self.send(ix).ok();
        }
    }

    /// The next round, 5 seconds in, with `holders` entered by the keeper: their tokens, held
    /// since it began, are their tickets in it. Answers the round.
    fn start_round(&mut self, holders: &[&Keypair]) -> u32 {
        let next = self.round() + 1;
        self.warp_into(next, 5);
        self.enter(holders);
        next
    }

    fn game(&self) -> Game {
        self.w.env.read(&companion::game_address(&self.mint))
    }

    fn companion(&self) -> Companion {
        self.w.env.read(&companion::companion_address(&self.mint))
    }

    fn header(&self) -> GameHeader {
        let data = self
            .w
            .env
            .account(&lottery::state_address(&self.mint))
            .unwrap()
            .data;
        GameHeader::read(&data, &self.mint).unwrap()
    }

    fn slots(&self, owner: &Pubkey) -> Slots {
        Slots::decode(&self.w.env.hook_data(&self.mint, owner))
    }

    fn range(&self, owner: &Pubkey, round: u32) -> Range {
        self.slots(owner)
            .range_in(round)
            .expect("a range that round")
    }

    fn keys(&self) -> LaunchKeys {
        LaunchKeys::of(&self.w.launch(&self.mint))
    }

    fn balance(&self, owner: &Pubkey) -> u64 {
        self.w.env.holding(&self.mint, owner)
    }

    fn send(&mut self, ix: Instruction) -> Tx {
        let cranker = self.cranker.insecure_clone();
        self.w.env.send_paid_by(&[ix], &cranker, &[])
    }

    /// A wallet that buys `sol` of the token.
    fn buyer(&mut self, sol: u64) -> Keypair {
        let t = self.w.wallet_with_sol(sol + SOL);
        self.w.buy(&t, &self.mint, sol).ok();
        t
    }

    /// Trading volume: wallets that buy `sol` each and sell it all (their tickets die).
    fn volume(&mut self, wallets: usize, sol: u64) {
        for _ in 0..wallets {
            let t = self.w.wallet_with_sol(sol + SOL);
            self.w.buy(&t, &self.mint, sol).ok();
            let held = self.balance(&t.pubkey());
            self.w.sell(&t, &self.mint, held).ok();
        }
    }

    fn claim_fees(&mut self) -> Tx {
        let ix = companion::claim_fees_game(self.cranker.pubkey(), self.mint, HOOK);
        self.send(ix)
    }

    /// Volume, then a fee claim: the pot holds at least the minimum.
    fn fund_pot(&mut self) {
        self.volume(1, 20 * SOL);
        self.claim_fees().ok();
        assert!(self.companion().pending_pot >= MIN_POT);
    }

    /// The keeper's draw of `round`, its seed made from the slot `at` names: committed and
    /// requested in one instruction (with the pot's last paid request, as the keeper sends it).
    fn draw_at(&mut self, round: u32, at: SeedSlot) -> Tx {
        let paid = self.game().paid_seed;
        let ix = companion::draw_after(
            self.cranker.pubkey(),
            self.mint,
            HOOK,
            round,
            at,
            orao::TREASURY,
            paid,
        );
        self.send(ix)
    }

    /// The keeper's draw of `round`, from the newest slot hash (the parent slot's).
    fn draw_round(&mut self, round: u32) -> Tx {
        let at = set_slot_hash(&mut self.w.env);
        self.draw_at(round, at)
    }

    /// The draw of the round that just ended.
    fn draw(&mut self) -> Tx {
        let round = self.round() - 1;
        self.draw_round(round)
    }

    fn reveal(&mut self) -> Tx {
        let request = self.game().request;
        let ix = companion::reveal(self.cranker.pubkey(), self.mint, HOOK, request);
        self.send(ix)
    }

    /// ORAO answers the pending request with `randomness`; answers its refund.
    fn fulfil(&mut self, randomness: &[u8; 64]) -> u64 {
        let seed = self.game().seed;
        orao::fulfil(&mut self.w.env, &seed, randomness)
    }

    fn claim(&mut self, attempt: u8, winner: &Pubkey) -> Tx {
        let ix = companion::claim_prize(self.cranker.pubkey(), self.mint, HOOK, attempt, *winner);
        self.send(ix)
    }

    /// `expire`.
    fn expire(&mut self) -> Tx {
        set_slot_hash(&mut self.w.env);
        let request = self.game().request;
        let ix = companion::expire(self.cranker.pubkey(), self.mint, HOOK, request);
        self.send(ix)
    }

    /// To the opening of claim attempt `attempt`.
    fn warp_to_attempt(&mut self, attempt: u8) {
        let g = self.game();
        let t = g.revealed_at + i64::from(attempt) * i64::from(WINDOW);
        if t > self.w.env.now {
            self.w.env.warp(t - self.w.env.now);
        }
    }

    /// The ticket attempt `attempt` draws.
    fn ticket(&self, attempt: u8) -> u64 {
        let g = self.game();
        draw_index(&g.randomness, u32::from(attempt), g.total).unwrap()
    }

    fn set_status(&mut self, audited: bool, pot_cap: u64, blocked: bool) -> Tx {
        let deployer = self.w.env.deployer.insecure_clone();
        let ix = companion::set_hook_status(
            deployer.pubkey(),
            HOOK,
            HookStatusArgs {
                audited,
                pot_cap,
                blocked,
            },
        );
        self.w.env.send_paid_by(&[ix], &deployer, &[])
    }
}

/// What a draw's request costs the pot: the oracle payer topped up to the fee, the pending
/// account's rent and its own rent-exempt minimum, and the sender's bounty on that.
fn request_cost(w: &World, payer_lamports: u64) -> (u64, u64) {
    let need = orao::FEE + w.env.rent(orao::PENDING_LEN) + w.env.rent(0);
    let top_up = need.saturating_sub(payer_lamports);
    (top_up, top_up * u64::from(BOUNTY_BPS) / 10_000)
}


use bordrless_hook::authority::programdata_address;
use bordrless_program_tests::timelock::{programdata_hash, programdata_slot};
use hook_timelock::loader;

/// Keeper's `DRAW_UNITS` (apps/server/src/keeper/games.ts).
const KEEPER_DRAW_UNITS: u64 = 250_000;

fn ready_to_draw(l: &mut Lotto, hashed: bool) {
    if hashed {
        let deployer = l.w.env.deployer.insecure_clone();
        let hash = programdata_hash(&l.w.env, &HOOK);
        let ix = companion::set_hook_status_v2(
            deployer.pubkey(),
            HOOK,
            HookStatusArgs { audited: true, pot_cap: 0, blocked: false },
            hash,
        );
        l.w.env.send_paid_by(&[ix], &deployer, &[]).ok();
    }
    let a = l.buyer(5 * SOL);
    let b = l.buyer(SOL);
    let r0 = l.start_round(&[&a, &b]);
    l.volume(2, 10 * SOL);
    let mut ix = companion::claim_fees_game(l.cranker.pubkey(), l.mint, HOOK);
    if hashed {
        ix = companion::with_hook_code(ix, &HOOK);
    }
    l.send(ix).ok();
    l.warp_into(r0 + 1, 5);
}

fn draw_ix(l: &mut Lotto, hashed: bool) -> Instruction {
    let round = l.round() - 1;
    let at = set_slot_hash(&mut l.w.env);
    let paid = l.game().paid_seed;
    let ix = companion::draw_after(l.cranker.pubkey(), l.mint, HOOK, round, at, orao::TREASURY, paid);
    if hashed { companion::with_hook_code(ix, &HOOK) } else { ix }
}

#[test]
fn verify_3a_draw_units_with_a_hashed_audit() {
    // Control: no status (as on mainnet today).
    let mut l = Lotto::new();
    ready_to_draw(&mut l, false);
    let ix = draw_ix(&mut l, false);
    let tx = l.send(ix);
    tx.ok();
    let base = tx.cu();

    // The same draw once lottery_hook carries a v2 (hashed) audit: the game's memo is empty, so
    // the draw rehashes the hook's code.
    let mut h = Lotto::new();
    ready_to_draw(&mut h, true);
    // The keeper's draw (250k, at once): refused out of units, every try, the memo never written.
    for _ in 0..3 {
        let ix = draw_ix(&mut h, true);
        let cranker = h.cranker.insecure_clone();
        let tx = h.w.env.send_v0(&[compute_unit_limit(KEEPER_DRAW_UNITS as u32), ix], &cranker, &[], &[]);
        assert!(tx.result.is_err(), "the keeper's draw fails");
        assert!(!h.game().hook_audit_ok);
        h.w.env.warp(1);
    }
    let ix = draw_ix(&mut h, true);
    let tx = h.send(ix);
    tx.ok();
    let rehash = tx.cu();
    assert!(h.game().hook_audit_ok);

    // Anyone extends the hook's ProgramData by one byte: the slot moves, the next step rehashes.
    let mut x = Lotto::new();
    ready_to_draw(&mut x, true);
    // a memo first (a step that lands at the keeper's units: claim_fees does not write it).
    let slot_before = programdata_slot(&x.w.env, &HOOK);
    let payer = x.w.env.payer.pubkey();
    x.w.env.send(&[loader::extend_program(programdata_address(&HOOK), HOOK, payer, 10_240)], &[]).ok();
    x.w.env.warp(1);
    assert_ne!(programdata_slot(&x.w.env, &HOOK), slot_before, "extend moved the slot");
    let ix = draw_ix(&mut x, true);
    let cranker = x.cranker.insecure_clone();
    let tx = x.w.env.send_v0(&[compute_unit_limit(KEEPER_DRAW_UNITS as u32), ix], &cranker, &[], &[]);
    let code_len = x.w.env.account(&programdata_address(&HOOK)).unwrap().data.len() - 45;
    println!(
        "draw CU: no status {base}; hashed audit, memo empty {rehash} (+{}); hook code {code_len} B; at the keeper's {KEEPER_DRAW_UNITS} after an extend: {:?}",
        rehash - base,
        tx.result.as_ref().map(|m| m.compute_units_consumed).map_err(|f| (f.err.clone(), f.meta.compute_units_consumed))
    );
}
