//! Companion v2 games: a lottery coin run entirely by its companion (`docs/companions.md`, the
//! monorepo's `docs/studio-companions.md` §1 and §4).
//!
//! The coin launches through the companion from a plain `LaunchConfig` naming `lottery_hook`, which
//! keeps every holder's tickets under the game ticket standard. Part of every fee claim goes into
//! the pot; once a round is over anyone draws: the companion asks ORAO VRF for randomness (the real
//! mainnet program, dumped into `fixtures/`, called through a CPI), the answer is written with
//! `set_account` as ORAO's last `fulfill_v2` leaves it (`orao::fulfil`), anyone reveals it, and the
//! holding whose range holds the drawn ticket is paid the prize as SOL. Every way a round can end
//! unpaid rolls the pot over, and the protocol can cap or block a hook that is not audited.
//!
//! The tests choose the randomness they fulfil with (`randomness_where`), so each one draws the
//! ticket its case needs, whatever the mint's address makes the seed. They also write the slot
//! hashes sysvar (`set_slot_hash`), whose first entry the keeper's draw names for its seed.
//!
//! A round's tickets are the tokens held since it began: holders buy, then the keeper enters them
//! in the next round (`Lotto::start_round`), whose draw is made in the round after.

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

#[test]
fn a_lottery_runs_end_to_end() {
    let mut l = Lotto::new();
    let mint = l.mint;
    let r0 = l.round();
    let g = l.game();
    assert_eq!(
        (g.kind, g.hook, g.status, g.next_round),
        (GameKind::Lottery, HOOK, DrawStatus::Idle, r0)
    );
    let c = l.companion();
    assert_eq!(
        (c.game_hook, c.pot_bps, c.round_secs, c.split),
        (HOOK, POT_BPS, ROUND, SPLIT)
    );

    // Two holders: A big, B small; their tokens count from the next round, where the keeper
    // enters them. The pool, the launch and the creator address never hold tickets.
    let a = l.buyer(5 * SOL);
    let b = l.buyer(SOL);
    assert!(l.slots(&a.pubkey()).range_in(r0).is_none());
    let r0 = l.start_round(&[&a, &b]);
    let (ra, rb) = (l.range(&a.pubkey(), r0), l.range(&b.pubkey(), r0));
    assert_eq!(ra.weight, l.balance(&a.pubkey()));
    assert_eq!(rb.start, ra.start + ra.weight);
    l.volume(2, 10 * SOL);
    // Trading volume adds no tickets: its buyers held nothing when the round began.
    assert_eq!(l.header().total, ra.weight + rb.weight);

    // The fee claim splits four ways: the bounty, 30% buyback, the pot the rest (its rounding).
    let accrued = l.w.env.holding(&l.w.sol, &launch::launch_address(&mint));
    let tx = l.claim_fees();
    tx.ok();
    let fees: FeesClaimed = tx.event();
    let pot: PotFunded = tx.event();
    assert_eq!(fees.claimed, accrued);
    assert_eq!(fees.bounty, accrued * 50 / 10_000);
    let rest = accrued - fees.bounty;
    assert_eq!(fees.to_buyback, rest * 3_000 / 10_000);
    assert_eq!((fees.to_holders, fees.to_beneficiary), (0, 0));
    assert_eq!(pot.to_pot, rest - fees.to_buyback);
    assert_eq!(pot.to_buyback, 0);
    let c = l.companion();
    assert_eq!(
        (c.pending_pot, c.pending_buyback),
        (pot.to_pot, fees.to_buyback)
    );
    assert!(c.pending_pot >= MIN_POT);

    // Not before the round is over.
    refused(&l.draw_round(r0), CompanionError::RoundNotOver);
    l.warp_into(r0 + 1, 5);
    let total = l.header().total_of(r0).unwrap();
    let oracle_payer = companion::oracle_payer_address(&mint);
    let (top_up, bounty) = request_cost(&l.w, 0);
    let treasury_before = l.w.env.lamports(&orao::TREASURY);
    let pot_before = l.companion().pending_pot;
    let tx = l.draw();
    tx.ok();
    println!(
        "draw: CU {} size {} height {} trace {}",
        tx.cu(),
        tx.size,
        tx.max_height(),
        tx.trace_len()
    );
    let committed: DrawCommitted = tx.event();
    let ev: DrawRequested = tx.event();
    let g = l.game();
    assert_eq!(
        (g.status, g.round, g.total, g.n, g.next_round),
        (DrawStatus::Requested, r0, total, 0, r0 + 1)
    );
    assert_eq!(
        (ev.round, ev.total, ev.n, ev.made, ev.top_up, ev.bounty, ev.fee),
        (r0, total, 0, true, top_up, bounty, orao::FEE)
    );
    // The seed is the parent slot's hash's: nobody could know it before the draw landed.
    let sysvar = l.w.env.account(&seeds::SLOT_HASHES).unwrap().data;
    let hash: [u8; 32] = sysvar[16..48].try_into().unwrap();
    assert_eq!(committed.slot, l.w.env.slot - 1);
    assert_eq!(
        g.seed,
        seeds::draw_seed(&mint, r0, 0, committed.slot, &hash)
    );
    assert_eq!((committed.seed, committed.request), (g.seed, g.request));
    assert_eq!(g.request, orao::request_address(&g.seed));
    // ORAO's request is ours: made by the oracle payer, which paid its fee and rent and kept its
    // rent-exempt minimum; the pot paid exactly that, and the bounty.
    assert_eq!(orao::pending_client(&l.w.env, &g.seed), Some(oracle_payer));
    assert_eq!(l.w.env.lamports(&oracle_payer), l.w.env.rent(0));
    assert_eq!(
        l.w.env.lamports(&orao::TREASURY),
        treasury_before + orao::FEE
    );
    let c = l.companion();
    assert_eq!(c.pending_pot, pot_before - top_up - bounty);
    assert_eq!(g.prize, c.pending_pot);
    // One draw at a time.
    refused(&l.draw_round(r0), CompanionError::DrawPending);
    // Nothing to reveal until ORAO answers.
    refused(&l.reveal(), CompanionError::OracleNotFulfilled);

    // ORAO answers: attempt 0 draws one of A's tickets. Its refund goes to the oracle payer.
    let r = randomness_where(total, |k, x| k != 0 || ra.contains(x));
    let refund = l.fulfil(&r);
    assert_eq!(l.w.env.lamports(&oracle_payer), l.w.env.rent(0) + refund);
    // `withdraw` sweeps the creator address's spare lamports (if any), never the oracle payer's.
    let _ = l.send(companion::withdraw(
        l.cranker.pubkey(),
        mint,
        l.launcher.pubkey(),
    ));
    assert_eq!(l.w.env.lamports(&oracle_payer), l.w.env.rent(0) + refund);
    let tx = l.reveal();
    tx.ok();
    let ev: DrawRevealed = tx.event();
    assert_eq!(ev.randomness, r);
    let g = l.game();
    assert_eq!((g.status, g.randomness), (DrawStatus::Revealed, r));
    let x = l.ticket(0);
    assert!(ra.contains(x));

    // Attempt 1 is not open; B does not hold the ticket; A does.
    refused(&l.claim(1, &a.pubkey()), CompanionError::AttemptClosed);
    refused(&l.claim(0, &b.pubkey()), CompanionError::NotTheWinner);
    let a_before = l.w.env.lamports(&a.pubkey());
    let cranker_before = l.w.env.lamports(&l.cranker.pubkey());
    let prize = g.prize;
    let tx = l.claim(0, &a.pubkey());
    tx.ok();
    println!(
        "claim_prize: CU {} size {} height {}",
        tx.cu(),
        tx.size,
        tx.max_height()
    );
    let paid: PrizePaid = tx.event();
    let claim_bounty = prize * u64::from(BOUNTY_BPS) / 10_000;
    assert_eq!(
        (
            paid.winner,
            paid.ticket,
            paid.attempt,
            paid.prize,
            paid.bounty
        ),
        (a.pubkey(), x, 0, prize - claim_bounty, claim_bounty)
    );
    assert_eq!(
        l.w.env.lamports(&a.pubkey()),
        a_before + prize - claim_bounty
    );
    assert!(l.w.env.lamports(&l.cranker.pubkey()) > cranker_before + claim_bounty - 10_000);
    let (c, g) = (l.companion(), l.game());
    assert_eq!(c.pending_pot, 0);
    assert_eq!(
        (g.status, g.prizes_paid, g.prizes_total, g.last_winner),
        (DrawStatus::Idle, 1, prize - claim_bounty, a.pubkey())
    );
    // Paid once: the round is done.
    refused(&l.claim(0, &a.pubkey()), CompanionError::NoDraw);
    refused(&l.draw_round(r0), CompanionError::RoundNotOver);
    // The creator's bridged SOL still covers everything set aside.
    let creator = companion::creator_address(&mint);
    assert!(l.w.env.holding(&l.w.sol, &creator) >= c.set_aside().unwrap());

    // The next round: the float the refund left pays part of the next request.
    l.enter(&[&a, &b]);
    l.fund_pot();
    l.warp_into(r0 + 2, 1);
    let payer_now = l.w.env.lamports(&oracle_payer);
    let (top_up, _) = request_cost(&l.w, payer_now);
    let tx = l.draw();
    tx.ok();
    let ev: DrawRequested = tx.event();
    assert_eq!((ev.round, ev.top_up), (r0 + 1, top_up));
    assert!(top_up < request_cost(&l.w, 0).0);
}

#[test]
fn a_winner_who_trades_in_the_next_round_still_claims() {
    let mut l = Lotto::new();
    let a = l.buyer(5 * SOL);
    let b = l.buyer(2 * SOL);
    let r0 = l.start_round(&[&a, &b]);
    l.fund_pot();
    let ra = l.range(&a.pubkey(), r0);
    l.warp_into(r0 + 1, 10);
    l.draw().ok();
    let total = l.game().total;
    // Attempt 0 draws one of A's first tickets.
    let r = randomness_where(total, |k, x| {
        k != 0 || (x >= ra.start && x < ra.start + ra.weight / 4)
    });
    l.fulfil(&r);
    l.reveal().ok();
    // Round r0 + 1: A buys more, then sells a little. Its round-r0 range is now the previous slot,
    // cut only to what A holds, which is more than it.
    let more = l.w.buy(&a, &l.mint.clone(), SOL);
    more.ok();
    let held = l.balance(&a.pubkey());
    l.w.sell(&a, &l.mint.clone(), held / 10).ok();
    // And anyone enters it, as often as they like: nothing changes in r0 + 1.
    l.enter(&[&a, &a]);
    let s = l.slots(&a.pubkey());
    assert_eq!(s.previous, ra, "the round-r0 range is kept whole");
    assert_eq!(s.current.round, r0 + 1);
    let tx = l.claim(0, &a.pubkey());
    tx.ok();
    assert_eq!(tx.event::<PrizePaid>().winner, a.pubkey());
}

/// Every way a round ends unpaid rolls the pot over: a round without tickets, two missed rounds,
/// tickets that died (a dead range), a winner who sold, an oracle that never answers.
#[test]
fn every_rollover() {
    // A round with no tickets: nothing moved in it.
    let mut l = Lotto::new();
    let r0 = l.round();
    l.fund_pot();
    l.warp_into(r0 + 2, 1);
    let pot = l.companion().pending_pot;
    let tx = l.draw_round(r0 + 1);
    tx.ok();
    let ev: RolledOver = tx.event();
    assert_eq!(
        (ev.round, ev.reason, ev.pending_pot),
        (r0 + 1, RolloverReason::NoTickets, pot)
    );
    assert!(tx.events::<DrawCommitted>().is_empty());
    let g = l.game();
    assert_eq!(
        (g.status, g.next_round, g.rollovers, g.draws),
        (DrawStatus::Idle, r0 + 2, 1, 0)
    );
    assert_eq!(l.companion().pending_pot, pot, "no oracle, no cost");
    // That round is done; the round before it was skipped (only the round just ended is drawn).
    refused(&l.draw_round(r0 + 1), CompanionError::RoundNotOver);
    refused(&l.draw_round(r0), CompanionError::RoundNotOver);

    // Two missed rounds: A's tickets of r0 are never drawn once two more rounds have ended.
    let mut l = Lotto::new();
    let a = l.buyer(5 * SOL);
    let b = l.buyer(SOL);
    let c2 = l.buyer(SOL);
    let r0 = l.start_round(&[&a]);
    l.fund_pot();
    l.warp_into(r0 + 1, 1);
    l.enter(&[&b]);
    l.warp_into(r0 + 2, 1);
    l.enter(&[&c2]);
    l.warp_into(r0 + 3, 1);
    refused(&l.draw_round(r0), CompanionError::RoundNotOver);
    refused(&l.draw_round(r0 + 1), CompanionError::RoundNotOver);
    let tx = l.draw_round(r0 + 2);
    tx.ok();
    let total = l.game().total;
    assert_eq!(total, l.balance(&c2.pubkey()));
    assert!(l.slots(&a.pubkey()).range_in(r0 + 2).is_none());
    assert!(l.slots(&b.pubkey()).range_in(r0 + 2).is_none());
    // The only live range of r0 + 2 is C's: it wins.
    l.fulfil(&randomness_where(total, |_, _| true));
    l.reveal().ok();
    l.claim(0, &c2.pubkey()).ok();

    // A dead range: every attempt draws a ticket whose holder sold it.
    let mut l = Lotto::new();
    let a = l.buyer(SOL);
    let d = l.buyer(5 * SOL);
    let r0 = l.start_round(&[&a, &d]);
    let rd = l.range(&d.pubkey(), r0);
    let dead = l.balance(&d.pubkey());
    l.w.sell(&d, &l.mint.clone(), dead).ok();
    assert!(
        l.slots(&d.pubkey()).range_in(r0).is_none(),
        "D's range died"
    );
    l.fund_pot();
    l.warp_into(r0 + 1, 1);
    l.draw().ok();
    let total = l.game().total;
    let pot_at_draw = l.companion().pending_pot;
    l.fulfil(&randomness_where(total, |_, x| rd.contains(x)));
    l.reveal().ok();
    for k in 0..ATTEMPTS {
        l.warp_to_attempt(k);
        refused(&l.claim(k, &a.pubkey()), CompanionError::NotTheWinner);
        refused(&l.claim(k, &d.pubkey()), CompanionError::NotTheWinner);
        if k + 1 < ATTEMPTS {
            refused(&l.expire(), CompanionError::NotDue);
        }
    }
    l.warp_to_attempt(ATTEMPTS);
    refused(
        &l.claim(ATTEMPTS - 1, &a.pubkey()),
        CompanionError::AttemptClosed,
    );
    let tx = l.expire();
    tx.ok();
    let ev: RolledOver = tx.event();
    assert_eq!(
        (ev.round, ev.reason, ev.pending_pot),
        (r0, RolloverReason::NoClaim, pot_at_draw)
    );
    assert_eq!(l.game().status, DrawStatus::Idle);
    assert_eq!(l.companion().pending_pot, pot_at_draw, "the pot rolls over");
    refused(&l.expire(), CompanionError::NoDraw);

    // A winner who sold: attempt 0 draws A's ticket, A sells before the claim, the other attempts
    // draw dead tickets.
    let mut l = Lotto::new();
    let a = l.buyer(3 * SOL);
    let d = l.buyer(5 * SOL);
    let r0 = l.start_round(&[&a, &d]);
    let ra = l.range(&a.pubkey(), r0);
    let rd = l.range(&d.pubkey(), r0);
    let all_d = l.balance(&d.pubkey());
    l.w.sell(&d, &l.mint.clone(), all_d).ok();
    l.fund_pot();
    l.warp_into(r0 + 1, 1);
    l.draw().ok();
    let total = l.game().total;
    l.fulfil(&randomness_where(total, |k, x| {
        if k == 0 {
            ra.contains(x)
        } else {
            rd.contains(x)
        }
    }));
    l.reveal().ok();
    let all_a = l.balance(&a.pubkey());
    l.w.sell(&a, &l.mint.clone(), all_a).ok();
    refused(&l.claim(0, &a.pubkey()), CompanionError::NotTheWinner);
    l.warp_to_attempt(ATTEMPTS);
    let tx = l.expire();
    tx.ok();
    assert_eq!(tx.event::<RolledOver>().reason, RolloverReason::NoClaim);

    // The oracle never answers: the round's one seed waits for it until the draw's claims end,
    // never replaced (an hour's silence included); then the round rolls over. An answer landing
    // after that is never read.
    let mut l = Lotto::new();
    let a = l.buyer(SOL);
    let r0 = l.start_round(&[&a]);
    l.fund_pot();
    l.warp_into(r0 + 1, 1);
    l.draw().ok();
    let first = l.game().seed;
    refused(&l.reveal(), CompanionError::OracleNotFulfilled);
    let end = l.game().claims_end();
    assert_eq!(end, i64::from(r0 + 2) * R);
    for wait in [600, 1_800, end - 1 - l.w.env.now - 2_400] {
        l.w.env.warp(wait);
        refused(&l.expire(), CompanionError::NotDue);
        let g = l.game();
        assert_eq!((g.status, g.n, g.seed), (DrawStatus::Requested, 0, first));
    }
    assert_eq!(l.w.env.now, end - 1);
    l.w.env.warp(1);
    let pot = l.companion().pending_pot;
    let tx = l.expire();
    tx.ok();
    let ev: RolledOver = tx.event();
    assert_eq!(
        (ev.round, ev.reason, ev.pending_pot),
        (r0, RolloverReason::OracleSilent, pot)
    );
    assert!(tx.events::<DrawCommitted>().is_empty());
    assert_eq!(l.game().status, DrawStatus::Idle);
    orao::fulfil(&mut l.w.env, &first, &[9u8; 64]);
    refused(&l.reveal(), CompanionError::NoDraw);
    // The next round draws afresh.
    l.w.buy(&a, &l.mint.clone(), SOL / 10).ok();
    let next = l.round() + 1;
    l.warp_into(next, 1);
    let tx = l.draw();
    tx.ok();
    assert_eq!(tx.event::<DrawRequested>().n, 0);

    // An answer that lands late, but before the claims end, still decides the draw: revealed then,
    // with the attempts that fit before the claims end.
    let mut l = Lotto::new();
    let a = l.buyer(SOL);
    let r0 = l.start_round(&[&a]);
    l.fund_pot();
    l.warp_into(r0 + 1, 1);
    l.draw().ok();
    l.warp_into(r0 + 1, R - i64::from(WINDOW) - 10);
    refused(&l.expire(), CompanionError::NotDue);
    l.fulfil(&[5u8; 64]);
    refused(&l.expire(), CompanionError::OracleFulfilled);
    l.reveal().ok();
    let tx = l.claim(0, &a.pubkey());
    tx.ok();
    assert_eq!(tx.event::<PrizePaid>().winner, a.pubkey());
    // Answered but never revealed before the claims end: the round rolls over (`Late`).
    let mut l = Lotto::new();
    let a = l.buyer(SOL);
    let r0 = l.start_round(&[&a]);
    l.fund_pot();
    l.warp_into(r0 + 1, 1);
    l.draw().ok();
    l.fulfil(&[5u8; 64]);
    l.warp_into(r0 + 2, 0);
    refused(&l.reveal(), CompanionError::DrawLate);
    let tx = l.expire();
    tx.ok();
    assert_eq!(tx.event::<RolledOver>().reason, RolloverReason::Late);

    // Too late for ORAO's answer, its reveal and a whole claim window before the claims end: a draw
    // after `Game::last_draw` rolls the round over (`Late`), no seed committed, nothing paid.
    let mut l = Lotto::new();
    let a = l.buyer(SOL);
    let r0 = l.start_round(&[&a]);
    l.fund_pot();
    let last = l.game().last_draw(r0);
    assert_eq!(
        last,
        i64::from(r0 + 2) * R - i64::from(WINDOW) - REVEAL_SECS
    );
    l.w.env.warp(last + 1 - l.w.env.now);
    let pot = l.companion().pending_pot;
    let tx = l.draw_round(r0);
    tx.ok();
    assert_eq!(tx.event::<RolledOver>().reason, RolloverReason::Late);
    assert!(tx.events::<DrawCommitted>().is_empty());
    let g = l.game();
    assert_eq!(
        (g.status, g.next_round, g.seed),
        (DrawStatus::Idle, r0 + 1, [0; 32])
    );
    assert_eq!(l.companion().pending_pot, pot);
    // At that very moment, the draw is made.
    let mut l = Lotto::new();
    let a = l.buyer(SOL);
    let r0 = l.start_round(&[&a]);
    l.fund_pot();
    let last = l.game().last_draw(r0);
    l.w.env.warp(last - l.w.env.now);
    l.draw_round(r0).ok();
    assert_eq!(l.game().status, DrawStatus::Requested);

    // ORAO's fee above the cap when the round ends: the pot can't pay for a request, so the draw
    // rolls the round over at once (`OracleUnpaid`) and commits no seed: none is ever public for
    // someone to pay for only once they have seen its answer. The pot stays.
    let mut l = Lotto::new();
    let a = l.buyer(SOL);
    let r0 = l.start_round(&[&a]);
    l.fund_pot();
    l.warp_into(r0 + 1, 1);
    orao::set_fee(&mut l.w.env, seeds::MAX_REQUEST_FEE + 1);
    let pot = l.companion().pending_pot;
    let tx = l.draw_round(r0);
    tx.ok();
    let ev: RolledOver = tx.event();
    assert_eq!(
        (ev.round, ev.reason, ev.pending_pot),
        (r0, RolloverReason::OracleUnpaid, pot)
    );
    assert!(tx.events::<DrawCommitted>().is_empty());
    let g = l.game();
    assert_eq!(
        (g.status, g.seed, g.next_round),
        (DrawStatus::Idle, [0; 32], r0 + 1)
    );
    refused(&l.draw_round(r0), CompanionError::RoundNotOver);
    assert_eq!(l.companion().pending_pot, pot);
    // Back within the cap, the next round's draw commits its seed and pays for its request at
    // once (there is no state between the two).
    orao::set_fee(&mut l.w.env, orao::FEE);
    let r1 = l.start_round(&[&a]);
    l.warp_into(r1 + 1, 1);
    let tx = l.draw_round(r1);
    tx.ok();
    assert!(tx.event::<DrawRequested>().made);
    assert_eq!(tx.event::<DrawCommitted>().seed, l.game().seed);
    assert_eq!(l.game().status, DrawStatus::Requested);

    // An answered request is revealed, never asked again (whoever dislikes the answer).
    let mut l = Lotto::new();
    let a = l.buyer(SOL);
    let r0 = l.start_round(&[&a]);
    l.fund_pot();
    l.warp_into(r0 + 1, 1);
    l.draw().ok();
    l.w.env.warp(1_200);
    l.fulfil(&[5u8; 64]);
    refused(&l.expire(), CompanionError::OracleFulfilled);
    l.reveal().ok();

    // ORAO answers in a form the companion can't read (a layout it does not know): never revealed,
    // and the round rolls over at once, with no new seed bought for it.
    let mut l = Lotto::new();
    let a = l.buyer(SOL);
    let r0 = l.start_round(&[&a]);
    l.fund_pot();
    l.warp_into(r0 + 1, 1);
    l.draw().ok();
    let g = l.game();
    l.fulfil(&[5u8; 64]);
    let mut acc = l.w.env.account(&g.request).unwrap();
    acc.data[8] = 2;
    l.w.env.put(g.request, acc);
    refused(&l.reveal(), CompanionError::OracleAccount);
    let pot = l.companion().pending_pot;
    let tx = l.expire();
    tx.ok();
    assert_eq!(
        tx.event::<RolledOver>().reason,
        RolloverReason::OracleUnreadable
    );
    assert!(tx.events::<DrawCommitted>().is_empty());
    assert_eq!(l.companion().pending_pot, pot);
    assert_eq!(l.game().status, DrawStatus::Idle);
}

/// Anyone can compute the seed a draw will be made from (the newest slot hashes are public) and ask
/// ORAO for it before the draw lands: with a v2 request (paying for it) or ORAO's legacy v1
/// `request`, which still works and lands at the same address. Such a request is made blind, so the
/// draw adopts it while it is pending and pays nothing; ORAO's answer is the seed's, whoever asked.
/// Nobody can block a draw, or make it take a new seed, by requesting its seed first. A request ORAO
/// has already answered is refused (`StaleSeed`): its answer was public before the draw.
#[test]
fn a_request_made_by_anyone_is_adopted() {
    let mut l = Lotto::new();
    let mint = l.mint;
    let a = l.buyer(5 * SOL);
    let r0 = l.start_round(&[&a]);
    l.fund_pot();
    l.warp_into(r0 + 1, 1);
    let at = set_slot_hash(&mut l.w.env);
    let seed = at.seed(&mint, r0);
    let other = l.w.wallet_with_sol(SOL);
    let terms = seeds::Terms {
        treasury: orao::TREASURY,
        fee: orao::FEE,
    };
    l.w.env
        .send_paid_by(
            &[seeds::request_ix(other.pubkey(), &terms, seed)],
            &other,
            &[],
        )
        .ok();
    assert_eq!(orao::pending_client(&l.w.env, &seed), Some(other.pubkey()));
    let pot = l.companion().pending_pot;
    let payer = companion::oracle_payer_address(&mint);
    let tx = l.draw_at(r0, at);
    tx.ok();
    assert_eq!(tx.event::<DrawCommitted>().seed, seed);
    let ev: DrawRequested = tx.event();
    assert_eq!((ev.made, ev.top_up, ev.bounty, ev.fee), (false, 0, 0, 0));
    assert_eq!(l.companion().pending_pot, pot, "nothing paid");
    assert_eq!(l.w.env.lamports(&payer), 0);
    let g = l.game();
    assert_eq!(
        (g.status, g.seed, g.prize),
        (DrawStatus::Requested, seed, pot)
    );
    refused(&l.draw_round(r0), CompanionError::DrawPending);
    let ra = l.range(&a.pubkey(), r0);
    let r = randomness_where(l.game().total, |_, x| ra.contains(x));
    let before = l.w.env.lamports(&other.pubkey());
    let refund = l.fulfil(&r);
    assert_eq!(l.w.env.lamports(&other.pubkey()), before + refund);
    l.reveal().ok();
    assert_eq!(l.game().randomness, r);
    l.claim(0, &a.pubkey()).ok();

    // The next round: the legacy v1 request, made before the draw lands.
    l.w.buy(&a, &mint, SOL / 2).ok();
    l.fund_pot();
    l.warp_into(r0 + 2, 1);
    let at = set_slot_hash(&mut l.w.env);
    let seed = at.seed(&mint, r0 + 1);
    l.w.env
        .send_paid_by(&[v1_request_ix(other.pubkey(), seed)], &other, &[])
        .ok();
    let tx = l.draw_at(r0 + 1, at);
    tx.ok();
    assert!(!tx.event::<DrawRequested>().made);
    let g = l.game();
    assert_eq!(g.seed, seed);
    refused(&l.reveal(), CompanionError::OracleNotFulfilled);
    let ra = l.range(&a.pubkey(), r0 + 1);
    let r = randomness_where(g.total, |_, x| ra.contains(x));
    fulfil_v1(&mut l.w.env, &g.seed, &r);
    l.reveal().ok();
    assert_eq!(l.game().randomness, r);
    let tx = l.claim(0, &a.pubkey());
    tx.ok();
    assert_eq!(tx.event::<PrizePaid>().round, r0 + 1);

    // A request ORAO has already answered (v2 or v1): whoever made it knew the answer before the
    // draw, so the draw refuses that seed. A newer slot's seed is drawn.
    l.w.buy(&a, &mint, SOL / 2).ok();
    l.fund_pot();
    l.warp_into(r0 + 3, 1);
    for v1 in [false, true] {
        let at = set_slot_hash(&mut l.w.env);
        let seed = at.seed(&mint, r0 + 2);
        if v1 {
            l.w.env
                .send_paid_by(&[v1_request_ix(other.pubkey(), seed)], &other, &[])
                .ok();
            fulfil_v1(&mut l.w.env, &seed, &[9u8; 64]);
        } else {
            l.w.env
                .send_paid_by(
                    &[seeds::request_ix(other.pubkey(), &terms, seed)],
                    &other,
                    &[],
                )
                .ok();
            orao::fulfil(&mut l.w.env, &seed, &[9u8; 64]);
        }
        refused(&l.draw_at(r0 + 2, at), CompanionError::StaleSeed);
        assert_eq!(l.game().status, DrawStatus::Idle);
        l.w.env.warp(1);
    }
    l.draw_round(r0 + 2).ok();
    assert_eq!(l.game().status, DrawStatus::Requested);
    assert!(orao::pending_client(&l.w.env, &l.game().seed).is_some());
}

/// A draw names the slot its seed is made from: any of the last 3 slots the sysvar holds (the keeper
/// reads the newest through RPC, and its draw may land a slot or two later), never an older one,
/// the current one, or one the sysvar does not hold (skipped).
#[test]
fn a_draw_names_one_of_the_last_three_slots() {
    let mut l = Lotto::new();
    let mint = l.mint;
    let a = l.buyer(SOL);
    let r0 = l.start_round(&[&a]);
    l.fund_pot();
    l.warp_into(r0 + 1, 1);
    let now = l.w.env.slot;
    // The sysvar as the runtime keeps it, the slot two before this one skipped.
    let entries = [now - 1, now - 3, now - 4, now - 5].map(|slot| SeedSlot {
        slot,
        hash: hash_of(slot),
    });
    set_slot_hashes(&mut l.w.env, &entries);
    for slot in [now - 4, now - 2, now, now + 1] {
        let at = SeedSlot {
            slot,
            hash: hash_of(slot),
        };
        refused(&l.draw_at(r0, at), CompanionError::StaleSeed);
    }
    let tx = l.draw_at(r0, entries[1]);
    tx.ok();
    let ev: DrawCommitted = tx.event();
    assert_eq!((ev.slot, ev.seed), (now - 3, entries[1].seed(&mint, r0)));
    assert_eq!(l.game().seed, ev.seed);
    assert!(tx.event::<DrawRequested>().made);
    assert!(orao::pending_client(&l.w.env, &ev.seed).is_some());
}

/// While the hook is not audited, a pot holds at most its cap (10 SOL without a status); the pot's
/// share above it goes to the buyback. The protocol can set the cap; an audit lifts it.
#[test]
fn the_pot_is_capped_while_the_hook_is_not_audited() {
    let mut l = Lotto::new();
    assert!(l
        .w
        .env
        .account(&companion::hook_status_address(&HOOK))
        .is_none());
    // About 30 SOL of creator fees: 70% of that is far above the default cap.
    l.volume(16, 50 * SOL);
    let tx = l.claim_fees();
    tx.ok();
    let fees: FeesClaimed = tx.event();
    let pot: PotFunded = tx.event();
    let share = fees.claimed - fees.bounty - fees.to_buyback;
    assert!(share > DEFAULT_POT_CAP, "{share}");
    assert_eq!(
        (pot.to_pot, pot.to_buyback, pot.pending_pot),
        (DEFAULT_POT_CAP, share - DEFAULT_POT_CAP, DEFAULT_POT_CAP)
    );
    let c = l.companion();
    assert_eq!(c.pending_pot, DEFAULT_POT_CAP);
    assert_eq!(c.pending_buyback, fees.to_buyback + share - DEFAULT_POT_CAP);
    // Full: the next share all goes to the buyback.
    l.volume(1, 10 * SOL);
    let tx = l.claim_fees();
    tx.ok();
    let pot: PotFunded = tx.event();
    assert_eq!(pot.to_pot, 0);
    assert!(pot.to_buyback > 0);

    // Only the protocol's upgrade authority sets a status.
    let stranger = l.w.wallet_with_sol(SOL);
    let ix = companion::set_hook_status(
        stranger.pubkey(),
        HOOK,
        HookStatusArgs {
            audited: true,
            pot_cap: 0,
            blocked: false,
        },
    );
    refused(
        &l.w.env.send_paid_by(&[ix], &stranger, &[]),
        CompanionError::NotProtocolAuthority,
    );

    // The protocol may lower the cap of a hook that is not audited (0.1 to 10 SOL), never lift it:
    // the pot above it goes to the buyback at the next step.
    for cap in [DEFAULT_POT_CAP + 1, u64::MAX, MIN_POT_CAP - 1, 0] {
        refused(
            &l.set_status(false, cap, false),
            CompanionError::BadHookStatus,
        );
    }
    l.set_status(false, 2 * SOL, false).ok();
    let before = l.companion();
    l.volume(1, SOL);
    let tx = l.claim_fees();
    tx.ok();
    let moved: PotToBuyback = tx.event();
    assert!(!moved.blocked);
    assert_eq!(moved.lamports, before.pending_pot - 2 * SOL);
    assert_eq!(l.companion().pending_pot, 2 * SOL);
    // A draw trims it too.
    l.set_status(false, SOL, false).ok();
    let t = l.buyer(SOL);
    let r = l.start_round(&[&t]);
    l.warp_into(r + 1, 1);
    let tx = l.draw();
    tx.ok();
    assert_eq!(tx.event::<PotToBuyback>().lamports, SOL);
    assert!(l.companion().pending_pot < SOL);
    // Back to the default cap (from a cap below it).
    l.set_status(false, DEFAULT_POT_CAP, false).ok();

    // Audited: no cap.
    let tx = l.set_status(true, 0, false);
    tx.ok();
    let ev: HookStatusSet = tx.event();
    assert!(ev.audited && !ev.blocked);
    let st: HookStatus = l.w.env.read(&companion::hook_status_address(&HOOK));
    assert_eq!(
        (st.hook, st.audited, st.blocked, st.updated_by),
        (HOOK, true, false, l.w.env.deployer.pubkey())
    );
    l.volume(16, 50 * SOL);
    let tx = l.claim_fees();
    tx.ok();
    let fees: FeesClaimed = tx.event();
    let pot: PotFunded = tx.event();
    assert_eq!(pot.to_buyback, 0);
    assert_eq!(pot.to_pot, fees.claimed - fees.bounty - fees.to_buyback);
    assert!(l.companion().pending_pot > DEFAULT_POT_CAP);
    // An audit is final: the hook can't be un-audited (so it can't be capped again, nor blocked).
    for (cap, blocked) in [
        (DEFAULT_POT_CAP, false),
        (DEFAULT_POT_CAP, true),
        (SOL, false),
    ] {
        refused(
            &l.set_status(false, cap, blocked),
            CompanionError::BadHookStatus,
        );
    }
    l.set_status(true, 0, false).ok();
}

/// A blocked hook's games pay nobody: the pot share and the pot go to the buyback, any draw ends.
/// Only a hook that is not audited can be blocked, and only an audit lifts a block.
#[test]
fn a_blocked_hook_pays_nobody() {
    let mut l = Lotto::new();
    let a = l.buyer(5 * SOL);
    let r0 = l.start_round(&[&a]);
    l.fund_pot();
    l.warp_into(r0 + 1, 1);
    l.draw().ok();
    let total = l.game().total;
    let ra = l.range(&a.pubkey(), r0);
    l.fulfil(&randomness_where(total, |_, x| ra.contains(x)));
    l.reveal().ok();

    // A hook is never marked audited and blocked at once. Not audited, it can be blocked.
    refused(&l.set_status(true, 0, true), CompanionError::BadHookStatus);
    let tx = l.set_status(false, DEFAULT_POT_CAP, true);
    tx.ok();
    assert!(tx.event::<HookStatusSet>().blocked);

    // A's winning claim pays nobody: the pot goes to the buyback and the draw ends.
    let c = l.companion();
    let a_before = l.w.env.lamports(&a.pubkey());
    let tx = l.claim(0, &a.pubkey());
    tx.ok();
    assert!(tx.events::<PrizePaid>().is_empty());
    let moved: PotToBuyback = tx.event();
    assert_eq!((moved.lamports, moved.blocked), (c.pending_pot, true));
    assert_eq!(tx.event::<RolledOver>().reason, RolloverReason::Blocked);
    assert_eq!(l.w.env.lamports(&a.pubkey()), a_before);
    let after = l.companion();
    assert_eq!(after.pending_pot, 0);
    assert_eq!(after.pending_buyback, c.pending_buyback + c.pending_pot);
    assert_eq!(l.game().status, DrawStatus::Idle);
    // Nothing left to do for a blocked game's steps.
    refused(&l.claim(0, &a.pubkey()), CompanionError::NothingToDo);
    refused(&l.draw(), CompanionError::NothingToDo);
    // The pot's share of every claim goes to the buyback.
    l.volume(1, 10 * SOL);
    let tx = l.claim_fees();
    tx.ok();
    let pot: PotFunded = tx.event();
    assert_eq!(pot.to_pot, 0);
    assert!(pot.to_buyback > 0);
    assert_eq!(l.companion().pending_pot, 0);
    // The buyback spends it (bought and burned).
    l.w.env.warp(61);
    for _ in 0..80 {
        let tx = l.send(companion::buyback_with(
            l.cranker.pubkey(),
            &l.keys(),
            false,
            Some(&l.custom.clone()),
        ));
        tx.ok();
        if !tx
            .events::<bordrless_companion::events::BoughtBack>()
            .is_empty()
        {
            break;
        }
        l.w.env.warp(61);
    }
    assert!(l.companion().burned_total > 0);

    // A block is lifted only by an audit.
    refused(
        &l.set_status(false, DEFAULT_POT_CAP, false),
        CompanionError::BadHookStatus,
    );
    l.set_status(true, 0, false).ok();
    l.volume(1, 10 * SOL);
    let tx = l.claim_fees();
    tx.ok();
    assert!(tx.event::<PotFunded>().to_pot > 0);
    // Audited, it can never be blocked again: not directly, and not by un-auditing it first, even
    // in one transaction.
    refused(
        &l.set_status(false, DEFAULT_POT_CAP, true),
        CompanionError::BadHookStatus,
    );
    let deployer = l.w.env.deployer.insecure_clone();
    let status = |audited, blocked| {
        companion::set_hook_status(
            deployer.pubkey(),
            HOOK,
            HookStatusArgs {
                audited,
                pot_cap: DEFAULT_POT_CAP,
                blocked,
            },
        )
    };
    let tx =
        l.w.env
            .send_paid_by(&[status(false, false), status(false, true)], &deployer, &[]);
    refused(&tx, CompanionError::BadHookStatus);
    let st: HookStatus = l.w.env.read(&companion::hook_status_address(&HOOK));
    assert!(st.audited && !st.blocked);
}

/// The site's launch of a lottery coin through its companion, as a v0 transaction with the
/// protocol's 22-address lookup table and the longest metadata the site sends, fits mainnet's
/// limits: at most 1,232 bytes, stack height 5, 64 trace entries.
#[test]
fn a_game_launch_fits_mainnet_limits() {
    let mut w = World::new();
    let mut addresses = protocol_lookup_table(&w);
    addresses.extend([
        companion::event_authority(),
        bordrless_launch::ID,
        bordrless_swap::ID,
        bordrless_token::ID,
    ]);
    assert_eq!(addresses.len(), 22);
    let table = w.env.put_lookup_table(Pubkey::new_unique(), &addresses);
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let setup = setup_ixs(&launcher.pubkey(), &mint, create_args(), game_args());
    let tx = w
        .env
        .send_v0(&setup, &launcher, &[&mint_kp], std::slice::from_ref(&table));
    tx.ok();
    println!("setup (create, prepare, create_game) v0: {} bytes", tx.size);
    assert!(tx.size <= 1_232, "setup {} bytes", tx.size);
    let config = hook_config(&mut w, &launcher, HOOK, lottery_hook::FLAGS);
    let c = w.launch_config(&config);
    let mut args = World::launch_args("TENCHARSXX", c.creator_fee_bps, VQ, c.rules);
    args.name = "N".repeat(32);
    args.uri = format!("https://gateway.pinata.cloud/ipfs/{}", "b".repeat(94));
    let custom = CustomHookAccounts {
        program: HOOK,
        extras: lottery::extras(&mint),
    };
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
    let tx = w
        .env
        .send_v0(&ixs, &launcher, &[&mint_kp], std::slice::from_ref(&table));
    tx.ok();
    println!(
        "game launch through the companion, v0 with the 22-address table: {} keys, {} bytes, {} trace, height {}, {} CU",
        tx.keys.len(),
        tx.size,
        tx.trace_len(),
        tx.max_height(),
        tx.cu()
    );
    assert!(tx.size <= 1_232, "{} bytes", tx.size);
    assert!(tx.max_height() <= 5, "height {}", tx.max_height());
    assert!(tx.trace_len() <= 64, "trace {}", tx.trace_len());
    let l = w.launch(&mint);
    assert_eq!(
        (l.creator, l.custom_hook, l.custom_hook_flags),
        (
            companion::creator_address(&mint),
            Some(HOOK),
            lottery_hook::FLAGS
        )
    );
    assert!(
        w.env
            .read::<Companion>(&companion::companion_address(&mint))
            .launched
    );

    // Every step of the game fits too, as the keeper sends it (with the same table).
    let keeper = w.wallet_with_sol(SOL);
    let keys = LaunchKeys::of(&w.launch(&mint));
    let at = SeedSlot {
        slot: 2,
        hash: [3; 32],
    };
    let seed = at.seed(&mint, 1);
    let budget = || [compute_unit_limit(400_000), compute_unit_price(20_000)];
    let steps: Vec<(&str, Vec<Instruction>)> = vec![
        (
            "draw (commit and request)",
            vec![companion::draw(
                keeper.pubkey(),
                mint,
                HOOK,
                1,
                at,
                orao::TREASURY,
            )],
        ),
        (
            "draw with the paid request",
            vec![companion::draw_after(
                keeper.pubkey(),
                mint,
                HOOK,
                1,
                at,
                orao::TREASURY,
                [4; 32],
            )],
        ),
        (
            "reveal",
            vec![companion::reveal(
                keeper.pubkey(),
                mint,
                HOOK,
                orao::request_address(&seed),
            )],
        ),
        (
            "claim_prize",
            vec![companion::claim_prize(
                keeper.pubkey(),
                mint,
                HOOK,
                0,
                Pubkey::new_unique(),
            )],
        ),
        (
            "expire",
            vec![companion::expire(
                keeper.pubkey(),
                mint,
                HOOK,
                orao::request_address(&seed),
            )],
        ),
        (
            "claim_fees",
            vec![companion::claim_fees_game(keeper.pubkey(), mint, HOOK)],
        ),
        (
            "buyback",
            vec![companion::buyback_with(
                keeper.pubkey(),
                &keys,
                false,
                Some(&custom),
            )],
        ),
        (
            "dev_buy",
            vec![companion::dev_buy_with(
                launcher.pubkey(),
                &keys,
                SOL,
                1,
                Some(&custom),
            )],
        ),
        (
            "release",
            vec![companion::release_with(
                keeper.pubkey(),
                &keys,
                false,
                launcher.pubkey(),
                Some(&custom),
            )],
        ),
    ];
    for (name, ixs) in steps {
        let mut all = budget().to_vec();
        all.extend(ixs);
        let payer = if name == "dev_buy" {
            &launcher
        } else {
            &keeper
        };
        let size = w
            .env
            .v0_size(&all, payer, &[], std::slice::from_ref(&table));
        println!("{name}: {size} v0 bytes with the 22-address table");
        assert!(size <= 1_232, "{name}: {size} bytes");
    }
}

/// Every step that moves the token passes the custom hook and its extras: the dev buy (a swap),
/// the buyback (a swap and a burn) and the release (a transfer). The creator address never holds
/// tickets; the beneficiary gets them when the bag reaches it.
#[test]
fn a_game_coin_moves_its_token_through_its_hook() {
    let mut l = Lotto::new();
    let mint = l.mint;
    let creator = companion::creator_address(&mint);
    let keys = l.keys();
    assert_eq!(keys.custom_hook, Some(HOOK));
    let launcher = l.launcher.insecure_clone();
    let custom = l.custom.clone();
    // The dev's buy, with the hook's slice on the swap.
    let tx = l.w.env.send_paid_by(
        &[companion::dev_buy_with(
            launcher.pubkey(),
            &keys,
            SOL,
            1,
            Some(&custom),
        )],
        &launcher,
        &[],
    );
    tx.ok();
    let bag = l.balance(&creator);
    assert!(bag > 0);
    assert_eq!(l.companion().dev_tokens, bag);
    assert_eq!(l.w.env.hook_data(&mint, &creator), [0u8; 64]);
    // Without the hook's registry, the program can't resolve the hook's accounts.
    let mut ix = companion::dev_buy_with(launcher.pubkey(), &keys, SOL / 10, 1, Some(&custom));
    let registry = bordrless_hook::hook_accounts_address(&HOOK, &mint).0;
    ix.accounts.retain(|m| m.pubkey != registry);
    refused(
        &l.w.env.send_paid_by(&[ix], &launcher, &[]),
        CompanionError::MissingAccount,
    );

    // The buyback: a swap and a burn, both through the hook.
    l.volume(2, 10 * SOL);
    l.claim_fees().ok();
    l.w.env.warp(61);
    let supply = l.w.env.read::<Mint>(&mint).supply;
    let mut burned = 0;
    for _ in 0..80 {
        let c = l.companion();
        let p = l.w.launch_pool(&mint);
        let spot = spot_price(
            p.quote_reserve,
            p.virtual_quote,
            p.base_reserve,
            p.virtual_base,
        )
        .unwrap();
        let tx = l.send(companion::buyback_with(
            l.cranker.pubkey(),
            &keys,
            false,
            Some(&custom),
        ));
        tx.ok();
        if let Some(ev) = tx
            .events::<bordrless_companion::events::BoughtBack>()
            .first()
        {
            assert!(spot <= c.reference_price * 10_300 / 10_000);
            burned = ev.burned;
            println!(
                "game buyback: CU {} size {} height {}",
                tx.cu(),
                tx.size,
                tx.max_height()
            );
            break;
        }
        l.w.env.warp(61);
    }
    assert!(burned > 0, "a buyback ran");
    assert_eq!(l.w.env.read::<Mint>(&mint).supply, supply - burned);
    assert_eq!(
        l.balance(&creator),
        bag,
        "the bag stays, the bought tokens burned"
    );
    assert_eq!(l.w.env.hook_data(&mint, &creator), [0u8; 64]);
    // The hook saw the burn: its header is in this round.
    assert_eq!(l.header().round, l.round());

    // The release: a transfer through the hook; the beneficiary's tokens are its tickets from the
    // next round.
    let tx = l.send(companion::release_with(
        l.cranker.pubkey(),
        &keys,
        false,
        launcher.pubkey(),
        Some(&custom),
    ));
    tx.ok();
    let got = l.balance(&launcher.pubkey());
    assert_eq!(got, bag);
    let s = l.slots(&launcher.pubkey());
    assert_eq!((s.current.round, s.current.weight), (l.round(), 0));
    let next = l.start_round(&[&launcher]);
    let s = l.slots(&launcher.pubkey());
    assert_eq!((s.current.round, s.current.weight), (next, bag));
    refused(
        &l.send(companion::release_with(
            l.cranker.pubkey(),
            &keys,
            false,
            launcher.pubkey(),
            Some(&custom),
        )),
        CompanionError::NothingToDo,
    );
}

#[test]
fn what_a_game_refuses() {
    let mut w = World::new();
    orao::load(&mut w.env);
    let launcher = w.wallet_with_sol(50 * SOL);

    // create_game's bounds, each refused before anything is made.
    type Case = (fn(&mut CreateGameArgs), CompanionError);
    let bad: [Case; 13] = [
        (|g| g.pot_bps = 6_999, CompanionError::BadSplit),
        (|g| g.pot_bps = 0, CompanionError::BadSplit),
        (
            |g| {
                g.split = Split {
                    buyback_bps: 2_000,
                    holders_bps: 1_000,
                    beneficiary_bps: 0,
                }
            },
            CompanionError::HolderRewardsOff,
        ),
        (|g| g.min_pot = MIN_MIN_POT - 1, CompanionError::BadGame),
        (|g| g.min_pot = MAX_MIN_POT + 1, CompanionError::BadGame),
        (|g| g.prize_bps = MIN_PRIZE_BPS - 1, CompanionError::BadGame),
        (|g| g.prize_bps = 10_001, CompanionError::BadGame),
        (
            |g| g.claim_window_secs = MIN_CLAIM_WINDOW - 1,
            CompanionError::BadGame,
        ),
        (|g| g.max_attempts = 0, CompanionError::BadGame),
        (
            |g| g.max_attempts = MAX_ATTEMPTS + 1,
            CompanionError::BadGame,
        ),
        // The attempts must end within half a round.
        (
            |g| g.claim_window_secs = WINDOW + 1,
            CompanionError::BadGame,
        ),
        (|g| g.hook = bordrless_token::ID, CompanionError::BadGame),
        // The header says an hour; the game another length.
        (|g| g.round_secs = 2 * ROUND, CompanionError::HookState),
    ];
    for (i, (f, e)) in bad.into_iter().enumerate() {
        let mint = Keypair::new();
        let mut g = game_args();
        f(&mut g);
        let ixs = [
            companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                mint.pubkey(),
                create_args(),
            ),
            lottery::prepare(launcher.pubkey(), mint.pubkey(), ROUND),
            companion::create_game(launcher.pubkey(), mint.pubkey(), g),
        ];
        let tx = w.env.send_paid_by(&ixs, &launcher, &[&mint]);
        assert_eq!(tx.custom(), code(e), "case {i}\n{}", tx.logs().join("\n"));
    }
    // The hook not prepared for the mint.
    let mint = Keypair::new();
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            mint.pubkey(),
            create_args(),
        ),
        companion::create_game(launcher.pubkey(), mint.pubkey(), game_args()),
    ];
    refused(
        &w.env.send_paid_by(&ixs, &launcher, &[&mint]),
        CompanionError::HookState,
    );
    // A companion without buyback limits: what a cap keeps out of the pot must have a buyback.
    let ixs = setup_ixs(
        &launcher.pubkey(),
        &mint.pubkey(),
        CreateArgs {
            split: Split {
                buyback_bps: 0,
                holders_bps: 0,
                beneficiary_bps: 10_000,
            },
            max_buyback: 0,
            ..create_args()
        },
        game_args(),
    );
    refused(
        &w.env.send_paid_by(&ixs, &launcher, &[&mint]),
        CompanionError::BadBuybackLimits,
    );
    // Only the mint's holder makes the game.
    let mint = Keypair::new();
    w.env
        .send_paid_by(
            &[
                companion::create(
                    launcher.pubkey(),
                    launcher.pubkey(),
                    mint.pubkey(),
                    create_args(),
                ),
                lottery::prepare(launcher.pubkey(), mint.pubkey(), ROUND),
            ],
            &launcher,
            &[&mint],
        )
        .ok();
    let mut ix = companion::create_game(launcher.pubkey(), mint.pubkey(), game_args());
    for m in ix.accounts.iter_mut().filter(|m| m.pubkey == mint.pubkey()) {
        m.is_signer = false;
    }
    w.env.send_paid_by(&[ix], &launcher, &[]).expect_fail();
    w.env
        .send_paid_by(
            &[companion::create_game(
                launcher.pubkey(),
                mint.pubkey(),
                game_args(),
            )],
            &launcher,
            &[&mint],
        )
        .ok();
    // Once only.
    w.env
        .send_paid_by(
            &[companion::create_game(
                launcher.pubkey(),
                mint.pubkey(),
                game_args(),
            )],
            &launcher,
            &[&mint],
        )
        .expect_fail();

    // The launch: a game companion must launch with its hook, with the lottery's callbacks.
    let plain = {
        let (config, tx) = w.create_config(
            &launcher,
            CreateConfigArgs {
                rules: LaunchRules::NONE,
                creator_fee_bps: CREATOR_FEE,
                custom_hook: None,
                custom_hook_flags: 0,
                label: "Plain".to_string(),
            },
        );
        tx.ok();
        config
    };
    let no_burns = hook_config(
        &mut w,
        &launcher,
        HOOK,
        bordrless_hook::token_flags::BEFORE_TRANSFER
            | bordrless_hook::token_flags::WRITES_HOOK_DATA,
    );
    // Exactly the lottery's flags: no more (deltas, after_burn), no fewer.
    let deltas = hook_config(
        &mut w,
        &launcher,
        HOOK,
        lottery_hook::FLAGS | bordrless_hook::token_flags::TRANSFER_RETURNS_DELTA,
    );
    let after_burn = hook_config(
        &mut w,
        &launcher,
        HOOK,
        lottery_hook::FLAGS | bordrless_hook::token_flags::AFTER_BURN,
    );
    let lottery_cfg = hook_config(&mut w, &launcher, HOOK, lottery_hook::FLAGS);
    for config in [plain, no_burns, deltas, after_burn] {
        let ix = launch_ix(&w, &launcher.pubkey(), &mint.pubkey(), &config);
        refused(
            &w.env.send_paid_by(&[ix], &launcher, &[&mint]),
            CompanionError::GameHookMismatch,
        );
    }
    // A companion without a game can't launch a custom hook.
    let other = Keypair::new();
    w.env
        .send_paid_by(
            &[
                companion::create(
                    launcher.pubkey(),
                    launcher.pubkey(),
                    other.pubkey(),
                    create_args(),
                ),
                lottery::prepare(launcher.pubkey(), other.pubkey(), ROUND),
            ],
            &launcher,
            &[&other],
        )
        .ok();
    let ix = launch_ix(&w, &launcher.pubkey(), &other.pubkey(), &lottery_cfg);
    refused(
        &w.env.send_paid_by(&[ix], &launcher, &[&other]),
        CompanionError::CustomHookUnsupported,
    );
    // The game companion launches from the lottery config.
    let ix = launch_ix(&w, &launcher.pubkey(), &mint.pubkey(), &lottery_cfg);
    w.env.send_paid_by(&[ix], &launcher, &[&mint]).ok();
    // A companion that launched without a game never gets one.
    let ix = launch_ix(&w, &launcher.pubkey(), &other.pubkey(), &plain);
    w.env.send_paid_by(&[ix], &launcher, &[&other]).ok();
    refused(
        &w.env.send_paid_by(
            &[companion::create_game(
                launcher.pubkey(),
                other.pubkey(),
                game_args(),
            )],
            &launcher,
            &[&other],
        ),
        CompanionError::AlreadyLaunched,
    );

    // The steps.
    let mut l = Lotto::new();
    let mint = l.mint;
    let launcher = l.launcher.insecure_clone();
    let (keys, custom) = (l.keys(), l.custom.clone());
    l.w.env
        .send_paid_by(
            &[companion::dev_buy_with(
                launcher.pubkey(),
                &keys,
                SOL / 10,
                1,
                Some(&custom),
            )],
            &launcher,
            &[],
        )
        .ok();
    let a = l.buyer(5 * SOL);
    let r0 = l.start_round(&[&a]);
    // A game's claim must show its hook's status (leaving it out never lifts a cap or a block).
    l.volume(1, 20 * SOL);
    let ix = companion::claim_fees(l.cranker.pubkey(), mint, None);
    refused(&l.send(ix), CompanionError::MissingAccount);
    // The pot below the minimum: no draw.
    l.warp_into(r0 + 1, 1);
    refused(&l.draw(), CompanionError::PotTooSmall);
    l.claim_fees().ok();
    // (ORAO's fee above the cap rolls the round over: `every_rollover`.)
    // Another treasury than ORAO's: the program asks for the one ORAO names.
    let round = l.round() - 1;
    let at = set_slot_hash(&mut l.w.env);
    let cranker = l.cranker.insecure_clone();
    let ix = companion::draw(
        cranker.pubkey(),
        mint,
        HOOK,
        round,
        at,
        Pubkey::new_unique(),
    );
    refused(&l.send(ix), CompanionError::MissingAccount);
    // A request built from another hash than the slot's: its address is not the seed's.
    let forged = SeedSlot {
        hash: [9; 32],
        ..at
    };
    refused(&l.draw_at(round, forged), CompanionError::MissingAccount);
    // A slot more than 3 slots old, or not done yet (the current one), or one the sysvar does not
    // hold (skipped): `StaleSeed`, whatever request is passed.
    for slot in [at.slot - 3, at.slot + 1, at.slot - 1] {
        let stale = SeedSlot { slot, ..at };
        refused(&l.draw_at(round, stale), CompanionError::StaleSeed);
    }
    assert_eq!(l.game().status, DrawStatus::Idle, "nothing committed");
    l.draw().ok();
    let g = l.game();
    // A request answered for another seed is never read as the draw's.
    let other = [3u8; 32];
    let griefer = l.w.wallet_with_sol(SOL);
    let terms = seeds::Terms {
        treasury: orao::TREASURY,
        fee: orao::FEE,
    };
    l.w.env
        .send_paid_by(
            &[seeds::request_ix(griefer.pubkey(), &terms, other)],
            &griefer,
            &[],
        )
        .ok();
    orao::fulfil(&mut l.w.env, &other, &[3u8; 64]);
    refused(&l.reveal(), CompanionError::OracleNotFulfilled);
    let ix = companion::reveal(
        l.cranker.pubkey(),
        mint,
        HOOK,
        orao::request_address(&other),
    );
    refused(&l.send(ix), CompanionError::MissingAccount);
    let total = g.total;
    let ra = l.range(&a.pubkey(), r0);
    l.fulfil(&randomness_where(total, |_, x| ra.contains(x)));
    l.reveal().ok();
    refused(&l.reveal(), CompanionError::NoDraw);
    // The creator address, the pool and the launch never win; nor a holding of another mint.
    let creator = companion::creator_address(&mint);
    refused(&l.claim(0, &creator), CompanionError::NotEligible);
    let pool = l.w.launch(&mint).pool;
    refused(&l.claim(0, &pool), CompanionError::NotEligible);
    let mut ix = companion::claim_prize(l.cranker.pubkey(), mint, HOOK, 0, a.pubkey());
    let theirs = token::holding_address(&mint, &a.pubkey());
    let sol_holding = token::holding_address(&l.w.sol, &a.pubkey());
    for m in ix.accounts.iter_mut().filter(|m| m.pubkey == theirs) {
        m.pubkey = sol_holding;
    }
    refused(&l.send(ix), CompanionError::WrongHolding);
    // The winner must be passed to be paid.
    let mut ix = companion::claim_prize(l.cranker.pubkey(), mint, HOOK, 0, a.pubkey());
    let winner_at = ix
        .accounts
        .iter()
        .rposition(|m| m.pubkey == a.pubkey())
        .unwrap();
    ix.accounts.remove(winner_at);
    refused(&l.send(ix), CompanionError::MissingAccount);
    l.claim(0, &a.pubkey()).ok();
}

/// A companion without a game is the companion it always was: the `_with` builders without a hook
/// build exactly the plain instructions, it never asks for a hook status, and its account reads
/// no game.
#[test]
fn kit_companions_are_unchanged() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint = Keypair::new();
    w.env
        .send_paid_by(
            &[companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                mint.pubkey(),
                CreateArgs {
                    split: Split {
                        buyback_bps: 5_000,
                        holders_bps: 5_000,
                        beneficiary_bps: 0,
                    },
                    ..create_args()
                },
            )],
            &launcher,
            &[&mint],
        )
        .ok();
    let rules = presets::burn();
    let a = World::launch_args("KIT", CREATOR_FEE, VQ, rules);
    let inner = launch::create_launch_with(
        companion::creator_address(&mint.pubkey()),
        mint.pubkey(),
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        a.clone(),
        None,
        None,
    );
    w.env
        .send_paid_by(
            &[companion::launch(
                launcher.pubkey(),
                mint.pubkey(),
                &inner,
                a,
            )],
            &launcher,
            &[&mint],
        )
        .ok();
    let mint = mint.pubkey();
    let c: Companion = w.env.read(&companion::companion_address(&mint));
    assert!(!c.is_game());
    assert_eq!(
        (
            c.pot_bps,
            c.pending_pot,
            c.round_secs,
            c.stranded_burned_at,
            (c.game_kind, c.pot_locked, c.reserved)
        ),
        (0, 0, 0, 0, (GameKind::Lottery, 0, [0; 1]))
    );
    assert!(w.env.account(&companion::game_address(&mint)).is_none());
    let keys = LaunchKeys::of(&w.launch(&mint));
    let x = Pubkey::new_unique();
    assert_eq!(
        companion::dev_buy(x, &keys, 7, 1),
        companion::dev_buy_with(x, &keys, 7, 1, None)
    );
    assert_eq!(
        companion::buyback(x, &keys, true),
        companion::buyback_with(x, &keys, true, None)
    );
    assert_eq!(
        companion::release(x, &keys, true, launcher.pubkey()),
        companion::release_with(x, &keys, true, launcher.pubkey(), None)
    );
    // Its claim splits three ways as ever (FeesClaimed alone), with no hook status passed.
    w.env.warp(31);
    for _ in 0..3 {
        let t = w.wallet_with_sol(3 * SOL);
        w.buy(&t, &mint, 2 * SOL).ok();
        let h = w.env.holding(&mint, &t.pubkey());
        w.sell(&t, &mint, h / 2).ok();
    }
    let cranker = w.wallet_with_sol(SOL);
    let tx = w.env.send_paid_by(
        &[companion::claim_fees(cranker.pubkey(), mint, None)],
        &cranker,
        &[],
    );
    tx.ok();
    let ev: FeesClaimed = tx.event();
    assert!(tx.events::<PotFunded>().is_empty());
    let rest = ev.claimed - ev.bounty;
    assert_eq!(ev.to_holders, rest * 5_000 / 10_000);
    assert_eq!(ev.to_buyback, rest - ev.to_holders);
    // No game: none of its steps run.
    let at = SeedSlot {
        slot: 1,
        hash: [1; 32],
    };
    let ix = companion::draw(cranker.pubkey(), mint, HOOK, 0, at, orao::TREASURY);
    w.env.send_paid_by(&[ix], &cranker, &[]).expect_fail();
}

/// Over many draws on a real round's ranges, each wallet's share of the drawn tickets follows its
/// share of the tickets (what the companion pays is `draw_index` over the hook's ranges).
#[test]
fn draws_follow_the_tickets() {
    let mut l = Lotto::new();
    let wallets: Vec<Keypair> = [SOL, 2 * SOL, 4 * SOL]
        .into_iter()
        .map(|sol| l.buyer(sol))
        .collect();
    let r0 = l.start_round(&wallets.iter().collect::<Vec<_>>());
    let total = l.header().total;
    let ranges: Vec<Range> = wallets.iter().map(|k| l.range(&k.pubkey(), r0)).collect();
    let n = 1_000u32;
    let mut hits = vec![0u32; wallets.len()];
    for i in 0..n {
        let mut seed = [1u8; 32];
        seed[..4].copy_from_slice(&i.to_le_bytes());
        let x = draw_index(&orao::randomness_for(&seed), 0, total).unwrap();
        if let Some(j) = ranges.iter().position(|r| r.contains(x)) {
            hits[j] += 1;
        }
    }
    for (j, r) in ranges.iter().enumerate() {
        let p = r.weight as f64 / total as f64;
        let expected = p * f64::from(n);
        let sigma = (f64::from(n) * p * (1.0 - p)).sqrt();
        println!(
            "wallet {j}: share {p:.3}, hits {} (expected {expected:.0})",
            hits[j]
        );
        assert!(
            (f64::from(hits[j]) - expected).abs() <= 4.0 * sigma + 1.0,
            "wallet {j}"
        );
    }
}

/// The client's account lists match what the program looks up.
#[test]
fn builders_name_the_programs_accounts() {
    let mint = Pubkey::new_unique();
    let keys = |ix: &Instruction| ix.accounts.iter().map(|m| m.pubkey).collect::<Vec<_>>();
    let named = [
        companion::companion_address(&mint),
        companion::creator_address(&mint),
        companion::game_address(&mint),
        companion::hook_status_address(&HOOK),
        companion::oracle_payer_address(&mint),
    ];
    let at = SeedSlot {
        slot: 77,
        hash: [1; 32],
    };
    let seed = seeds::draw_seed(&mint, 5, 0, 77, &[1; 32]);
    assert_eq!(at.seed(&mint, 5), seed);
    let draw = companion::draw(Pubkey::new_unique(), mint, HOOK, 5, at, orao::TREASURY);
    for k in named.iter().chain(&[
        lottery::state_address(&mint),
        seeds::SLOT_HASHES,
        orao::NETWORK_STATE,
        orao::TREASURY,
        orao::request_address(&seed),
        orao::ORAO_VRF_ID,
    ]) {
        assert!(keys(&draw).contains(k), "draw: {k}");
    }
    assert!(draw
        .accounts
        .iter()
        .any(|m| m.pubkey == orao::request_address(&seed) && m.is_writable));
    assert_eq!(
        draw.data,
        bordrless_companion::instruction::Draw { round: 5, slot: 77 }.data()
    );
    // With the breaker's paid request: one account more, read only.
    let after = companion::draw_after(
        Pubkey::new_unique(),
        mint,
        HOOK,
        5,
        at,
        orao::TREASURY,
        [8; 32],
    );
    assert_eq!(after.accounts.len(), draw.accounts.len() + 1);
    assert_eq!(
        after.accounts.last().map(|m| (m.pubkey, m.is_writable)),
        Some((orao::request_address(&[8; 32]), false))
    );
    // The seed changes with every part of it.
    assert_ne!(seed, seeds::draw_seed(&mint, 5, 0, 77, &[2; 32]));
    assert_ne!(seed, seeds::draw_seed(&mint, 5, 0, 78, &[1; 32]));
    assert_ne!(seed, seeds::draw_seed(&mint, 5, 1, 77, &[1; 32]));
    assert_ne!(seed, seeds::draw_seed(&mint, 6, 0, 77, &[1; 32]));
    // `expire` reads the draw's request; `retire` needs only the named accounts.
    let expire = companion::expire(
        Pubkey::new_unique(),
        mint,
        HOOK,
        orao::request_address(&seed),
    );
    for k in named.iter().chain(&[orao::request_address(&seed)]) {
        assert!(keys(&expire).contains(k), "expire: {k}");
    }
    let retire = companion::retire(Pubkey::new_unique(), mint, HOOK);
    for k in &named {
        assert!(keys(&retire).contains(k), "retire: {k}");
    }
    assert_eq!(
        retire.data,
        bordrless_companion::instruction::Retire {}.data()
    );
}
