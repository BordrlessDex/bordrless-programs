//! Audit round 1 of companion v2 games (`bordrless_companion`, `bordrless-game`, `lottery_hook`):
//! every finding's proof of concept, turned into a regression test that passes on the fixed code.
//! This file replaces the auditors' scratch files (`audit_games_custody_r1.rs`,
//! `audit_games_game_r1.rs`, `audit_games_integration_r1.rs`) and keeps their passing checks.
//!
//! 1. (high) A holding remembers two rounds of tickets, but a draw's claims could run into round
//!    `r + 2`, where two writes to the winner's holding (anyone's `enter`, or 1 token) erased its
//!    range and a later attempt (the attacker's) was paid. Fixed: a draw of round `r` is requested,
//!    revealed and claimed in round `r + 1` or rolls over (`DrawLate`, `RolloverReason::Late`);
//!    attempts take at most half a round. (Round 2 then made a round's one seed final.)
//! 2. (medium) A pot capped below its `min_pot` could never be drawn. Fixed: a draw needs the
//!    minimum or the cap, whichever is lower; caps are 0.1 to 10 SOL.
//! 3. (medium) An audited hook could be blocked in one transaction (un-audit, then block), and the
//!    10 SOL unaudited cap could be lifted without an audit. Fixed: an audit is final; an
//!    unaudited cap is at most 10 SOL.
//! 4. (medium) Self-transfers flooded a round with dead tickets. Fixed in the standard: a round's
//!    tickets are the tokens held since it began.
//! 5. (low) A game hook taking transfer deltas could skim the companion's buybacks; (low) extra
//!    flags (`AFTER_BURN`) stranded every buyback. Fixed: a game launch's flags are exactly the
//!    lottery's.
//! 6. (low) Splitting buys across wallets earned more tickets than consolidating. Fixed by 4.
//! 7. (low) A game hook with four registry extras besides the launch made a launch the site can't
//!    send. Fixed: `create_game` refuses such a hook.
//! 8. (low) An answer ORAO gave in a form the companion can't read bought two more requests each
//!    round. Fixed: the round rolls over (`RolloverReason::OracleUnreadable`).
//!
//! Audit round 2 (the scratch files `audit_games_custody_r2.rs`, `audit_games_game_r2.rs` and
//! `audit_games_integration_r2.rs`, replaced by the section at the end of this file):
//!
//! 9. (medium) A pot below its draw threshold (a coin nobody trades, the remainder of a prize below
//!    100%) was locked for ever under an audited hook, which can't be capped or blocked. Fixed: a
//!    game whose pot has paid no prize for `Game::dormant_secs` (30 days, or 4 rounds) is drawn
//!    from `MIN_MIN_POT`; after two such periods anyone may `retire` the pot to the buyback, which
//!    pays nobody.
//! 10. (medium) Keeping ORAO's answer out of the blocks until the oracle timeout bought a new
//!     seed: a re-draw (the answer previewed on devnet), or a rollover after three. Fixed: a round
//!     has one seed, and it is final; ORAO is waited for until the draw's claims end.
//! 11. (low) An audited pot was locked for ever while the oracle could not be paid. Fixed by 9:
//!     retired after two dormant periods.
//! 12. (low) A claim window can be stuffed by the holder of a later attempt. Not changed on
//!     chain: every claim pays its sender a bounty, so keepers and bots can outbid a stuffer;
//!     documented.
//! 13. (low) A fulfilled answer in another layout under the same discriminator was read. Fixed:
//!     each ORAO layout is bound by its exact length.
//! 14. (low) Adopted v1 squats ORAO might stop answering cost three seeds and the round. With 10,
//!     a squat costs at most its round and buys no new seed; with 9, it can't lock the pot.
//!     Adopting v1 is kept: refusing it would make every squat a sure rollover today.
//!
//! Audit round 3 (the scratch files `audit_games_custody_r3.rs`, `audit_games_game_r3.rs` and
//! `audit_games_integration_r3.rs`, replaced by the section at the end of this file; the custody
//! lens's file is kept whole as the module `custody_r3`):
//!
//! 15. (medium) An oracle that takes requests but never answers (one of ORAO's three signers
//!     offline), or answers in a form the companion can't read, made the pot pay for a request
//!     every round, with a bounty that paid bots to keep asking: about 7.2 SOL per game over the
//!     60 days before `retire`. Fixed: a circuit breaker. While ORAO has not answered the pot's
//!     last paid request (`Game.paid_seed`), the pot pays for a new one only for a draw 1, 2, 4…
//!     rounds after it (at most 30 days apart): 11 requests in those 60 days. A reveal, or that
//!     request found answered, resets it. (Since the final audit, a round the breaker holds rolls
//!     over at its draw, with no seed committed: finding 19.)
//! 16. (low) A game hook that refuses the companion's own token moves (the buyback's transfer to
//!     the creator address) stranded the buyback for ever, with every pot a block or `retire`
//!     sent there. Fixed two ways: `create_game` takes only Bordrless's lottery hook or a hook the
//!     protocol has vetted (written a status for), never a blocked one; and under a blocked hook,
//!     a buyback that has neither bought nor waited for 30 days is burned as SOL by anyone
//!     (`burn_stranded`), paying nobody (the waits since the final audit: finding 18).
//! 17. (low) The same with a hook that refuses the buyback's burn. Fixed by 16.
//!
//! (Info, same round: the lottery's steps now check `Game.kind`, so a kind added later can't be
//! run through them.)
//!
//! The final audit (the scratch files `audit_final_custody.rs`, `audit_final_game.rs` and
//! `audit_final_integration.rs`, replaced by the module `final_audit` at the end of this file):
//!
//! 18. (low) `burn_stranded` burned a working blocked hook's buyback as SOL when it was only
//!     waiting for its reference price to catch up with a risen price (a wait never moved
//!     `last_buyback_at`), and, restarting no clock, let every later buyback share be claimed and
//!     burned in one transaction. Fixed: a wait that moves the reference restarts the 30 days, and
//!     so does each burn (`Companion.stranded_burned_at`).
//! 19. (medium) While the pot could not pay for ORAO's request (its fee above the cap, its network
//!     state unreadable, the breaker holding), a committed draw stayed open and anyone's request
//!     for its public seed was adopted: a holder who previewed the answer paid only for seeds it
//!     won, and took every prize. Fixed: `draw` commits a seed only when the pot can pay for its
//!     request, else rolls the round over (`RolloverReason::OracleUnpaid`). The round-3 test that
//!     had a held round's seed adopted now shows no seed committed. (Since 21, `draw` makes the
//!     request itself.)
//! 20. (info) A game coin's `dev_buy` and `buyback` need more than the default 200k CU; the docs
//!     said otherwise. Fixed in the docs.
//!
//! (The final audit's integration finding, the monorepo keeper aborting its whole pass on the
//! first game companion and cranking no game, is fixed in the monorepo.)
//!
//! The final audit's residuals (the verifier's notes; regressions in `final_audit`):
//!
//! 21. (low, game) The committer chose how long a committed seed stayed open: `draw` could commit
//!     moments before the draw's last request time, and its sender, previewing ORAO's answer on
//!     devnet, paid for the request only if it won; otherwise the round rolled over (`Late`): a
//!     free re-roll each time the pot crossed its minimum. Fixed at the root: `draw` commits the
//!     seed and makes the pot's request (or adopts a pending one) in one instruction, so a seed is
//!     never on chain without its request, and `request_randomness` is gone. The seed is made from
//!     a slot the draw names, one of the last 3 (`oracle::SEED_SLOTS`, below ORAO's latency); a
//!     seed ORAO already answered is refused (`StaleSeed`). And a draw leaves `REVEAL_SECS` and a
//!     whole claim window before its claims end (`Game::last_draw`), or the round rolls over.
//! 22. (low, custody) `burn_stranded` moved a blocked game's pot that no step had moved yet into
//!     the buyback and burned it in the same instruction, with no buyback window. Fixed: whichever
//!     call moves a blocked game's pot (a game step, a fee claim, the burn itself) restarts the wait
//!     (`Companion.stranded_burned_at`), and the burn's own call then burns nothing; so a step and
//!     a burn bundled in one transaction can't burn it either. The monorepo keeper moves such a pot
//!     itself, with a game step, as soon as it sees the block.
//!
//! The residuals' verification (the verifier's `verify_residuals_custody.rs`, replaced by the tests
//! of 23 in `final_audit`):
//!
//! 23. (low, custody) A fee share credited to a blocked game's buyback after the wait had run out
//!     was burned at once, by the protocol's own keeper too: once a working hook's buybacks had
//!     spent everything, nothing bought for a month, and the next claim's share was burned in its
//!     own transaction or the next, before any buyback was tried. Fixed: a fee claim that credits
//!     a blocked game's buyback at least what it held (an emptied one: any credit) restarts the
//!     wait (`Companion::restart_stranded_wait`), so the share gets the whole wait. It can't put a
//!     refusing hook's burn off for ever: that buyback never empties, so only a credit at least as
//!     large as all it holds (fees paid to the game, burned with the rest) restarts the wait, and
//!     each such credit at least doubles what the next must be.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_companion::client::{self as companion, SeedSlot};
use bordrless_companion::constants::*;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{CreateArgs, CreateGameArgs, HookStatusArgs};
use bordrless_companion::oracle as seeds;
use bordrless_companion::state::{Companion, DrawStatus, Game, GameKind, HookStatus, Split};
use bordrless_core::policy;
use bordrless_game::{draw_index, round_of, GameHeader, Range, Slots};
use bordrless_hook::{token_flags, AccountSource, ExtraAccount, HookAccountList, Seed};
use bordrless_launch::client::{self as launch, CustomHookAccounts, LaunchKeys};
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price, Env, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::orao;
use bordrless_token::client as token;
use lottery_hook::client as lottery;
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;

const ROUND: u32 = 3_600;
const R: i64 = ROUND as i64;
const CREATOR_FEE: u16 = 200;
const BOUNTY_BPS: u16 = 50;
const SPLIT: Split = Split {
    buyback_bps: 3_000,
    holders_bps: 0,
    beneficiary_bps: 0,
};
const POT_BPS: u16 = 7_000;
const MIN_POT: u64 = 100_000_000;
/// Six attempts of 5 minutes: half of a 1-hour round, the most `create_game` allows now.
const WINDOW: u32 = 300;
const ATTEMPTS: u8 = 6;
const HOOK: Pubkey = lottery_hook::ID;
const PACKET: usize = 1_232;

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

fn code(e: CompanionError) -> u32 {
    u32::from(e)
}

/// Refused with the companion's own error (ORAO's codes overlap its numbers).
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

/// The slot hashes sysvar as the runtime keeps it: its first entry the current slot's parent.
/// Answers that entry, as a keeper's draw names it.
fn set_slot_hash(env: &mut Env) -> SeedSlot {
    let slot = env.slot - 1;
    let mut from = [0u8; 32];
    from[..8].copy_from_slice(&slot.to_le_bytes());
    let hash: [u8; 32] = orao::randomness_for(&from)[..32].try_into().unwrap();
    let mut sysvar = env
        .account(&seeds::SLOT_HASHES)
        .expect("the slot hashes sysvar");
    sysvar.data[..8].copy_from_slice(&1u64.to_le_bytes());
    sysvar.data[8..16].copy_from_slice(&slot.to_le_bytes());
    sysvar.data[16..48].copy_from_slice(&hash);
    env.put(seeds::SLOT_HASHES, sysvar);
    SeedSlot { slot, hash }
}

/// A randomness (repeatable) whose attempts all land where `want(attempt, ticket)` says.
fn randomness_where(total: u64, want: impl Fn(u8, u64) -> bool) -> [u8; 64] {
    for i in 0u64..4_000_000 {
        let mut seed = [9u8; 32];
        seed[..8].copy_from_slice(&i.to_le_bytes());
        let r = orao::randomness_for(&seed);
        if (0..ATTEMPTS).all(|k| want(k, draw_index(&r, u32::from(k), total).unwrap())) {
            return r;
        }
    }
    panic!("no randomness found");
}

/// The companion's `launch` of `mint` from `config` (whose custom hook is `hook`).
fn companion_launch_ix(
    w: &World,
    launcher: &Pubkey,
    mint: &Pubkey,
    config: &Pubkey,
    hook: Pubkey,
) -> Instruction {
    let c = w.launch_config(config);
    let custom = w.custom_hook_accounts(&hook, mint);
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
        Some(&custom),
    );
    companion::launch(*launcher, *mint, &inner, args)
}

/// The protocol vets `hook` for games (round 3: `create_game` takes any hook but Bordrless's
/// lottery hook only once the protocol has written its status): not audited, the 10 SOL cap, not
/// blocked, the same terms a hook without a status has.
fn vet(w: &mut World, hook: Pubkey) {
    let deployer = w.env.deployer.insecure_clone();
    let ix = companion::set_hook_status(
        deployer.pubkey(),
        hook,
        HookStatusArgs {
            audited: false,
            pot_cap: DEFAULT_POT_CAP,
            blocked: false,
        },
    );
    w.env.send_paid_by(&[ix], &deployer, &[]).ok();
}

fn lottery_config(w: &mut World, launcher: &Keypair, rules: LaunchRules, flags: u16) -> Pubkey {
    let (config, tx) = w.create_config(
        launcher,
        CreateConfigArgs {
            rules,
            creator_fee_bps: CREATOR_FEE,
            custom_hook: Some(HOOK),
            custom_hook_flags: flags,
            label: "Lottery".to_string(),
        },
    );
    tx.ok();
    config
}

/// A lottery coin launched through its companion (`lottery_hook`, ORAO), past the sniper window.
struct Lotto {
    w: World,
    launcher: Keypair,
    mint: Pubkey,
    cranker: Keypair,
    custom: CustomHookAccounts,
    launch_tx: Tx,
}

impl Lotto {
    fn new() -> Self {
        Self::with(|_| {}, LaunchRules::NONE, lottery_hook::FLAGS)
    }

    fn with(g: impl FnOnce(&mut CreateGameArgs), rules: LaunchRules, flags: u16) -> Self {
        let mut w = World::new();
        orao::load(&mut w.env);
        let launcher = w.wallet_with_sol(50 * SOL);
        let mint_kp = Keypair::new();
        let mint = mint_kp.pubkey();
        let mut ga = game_args();
        g(&mut ga);
        let setup = [
            companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
            lottery::prepare(launcher.pubkey(), mint, ga.round_secs),
            companion::create_game(launcher.pubkey(), mint, ga),
        ];
        w.env.send_paid_by(&setup, &launcher, &[&mint_kp]).ok();
        let config = lottery_config(&mut w, &launcher, rules, flags);
        let c = w.launch_config(&config);
        let custom = w.custom_hook_accounts(&HOOK, &mint);
        let mut args = World::launch_args("LOTTO", c.creator_fee_bps, VQ, c.rules);
        args.name = "Lottery".to_string();
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
        let ix = companion::launch(launcher.pubkey(), mint, &inner, args);
        let launch_tx = w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]);
        w.env.warp(31);
        let cranker = w.wallet_with_sol(SOL);
        Self {
            w,
            launcher,
            mint,
            cranker,
            custom,
            launch_tx,
        }
    }

    fn round(&self) -> u32 {
        round_of(self.w.env.now, ROUND)
    }

    fn warp_into(&mut self, round: u32, secs: i64) {
        let t = i64::from(round) * R + secs;
        assert!(t >= self.w.env.now, "the clock never goes back");
        self.w.env.warp(t - self.w.env.now);
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

    fn balance(&self, owner: &Pubkey) -> u64 {
        self.w.env.holding(&self.mint, owner)
    }

    fn keys(&self) -> LaunchKeys {
        LaunchKeys::of(&self.w.launch(&self.mint))
    }

    fn send(&mut self, ixs: &[Instruction]) -> Tx {
        let cranker = self.cranker.insecure_clone();
        self.w.env.send_paid_by(ixs, &cranker, &[])
    }

    fn buyer(&mut self, sol: u64) -> Keypair {
        let t = self.w.wallet_with_sol(sol + SOL);
        self.w.buy(&t, &self.mint, sol).ok();
        t
    }

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
        self.send(&[ix])
    }

    fn fund_pot(&mut self) {
        self.volume(1, 20 * SOL);
        self.claim_fees().ok();
        assert!(self.companion().pending_pot >= MIN_POT);
    }

    /// The permissionless `lottery_hook::enter` for `owner`, sent (and paid) by `by`.
    fn enter_for(&mut self, by: &Keypair, owner: &Pubkey) -> Tx {
        self.w
            .env
            .send_paid_by(&[lottery::enter(self.mint, *owner)], by, &[])
    }

    /// The keeper's `enter` of `owner`.
    fn enter(&mut self, owner: &Pubkey) -> Tx {
        let cranker = self.cranker.insecure_clone();
        self.enter_for(&cranker, owner)
    }

    /// The next round, 5 seconds in, with `holders` entered by the keeper. Answers the round.
    fn start_round(&mut self, holders: &[&Keypair]) -> u32 {
        let next = self.round() + 1;
        self.warp_into(next, 5);
        let cranker = self.cranker.insecure_clone();
        for h in holders {
            self.enter_for(&cranker, &h.pubkey()).ok();
        }
        next
    }

    /// The keeper's `draw(round)`: its seed made from the newest slot hash, committed and
    /// requested in one instruction, with the pot's last paid request (as the keeper sends it).
    fn draw_round(&mut self, round: u32) -> Tx {
        let at = set_slot_hash(&mut self.w.env);
        self.draw_at(round, at)
    }

    /// `draw(round)` naming `at` for its seed.
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
        self.send(&[ix])
    }

    /// The keeper's draw of the round that just ended.
    fn draw(&mut self) -> Tx {
        let round = self.round() - 1;
        self.draw_round(round)
    }

    fn reveal(&mut self) -> Tx {
        let request = self.game().request;
        let ix = companion::reveal(self.cranker.pubkey(), self.mint, HOOK, request);
        self.send(&[ix])
    }

    fn fulfil(&mut self, randomness: &[u8; 64]) -> u64 {
        let seed = self.game().seed;
        orao::fulfil(&mut self.w.env, &seed, randomness)
    }

    fn claim(&mut self, attempt: u8, winner: &Pubkey) -> Tx {
        let ix = companion::claim_prize(self.cranker.pubkey(), self.mint, HOOK, attempt, *winner);
        self.send(&[ix])
    }

    fn expire(&mut self) -> Tx {
        set_slot_hash(&mut self.w.env);
        let request = self.game().request;
        let ix = companion::expire(self.cranker.pubkey(), self.mint, HOOK, request);
        self.send(&[ix])
    }

    fn warp_to_attempt(&mut self, attempt: u8) {
        let t = self.game().attempt_opens(attempt).unwrap();
        if t > self.w.env.now {
            self.w.env.warp(t - self.w.env.now);
        }
    }

    /// To `t` (unix seconds), never back.
    fn warp_to(&mut self, t: i64) {
        assert!(t >= self.w.env.now, "the clock never goes back");
        self.w.env.warp(t - self.w.env.now);
    }

    fn retire_ix(&self) -> Instruction {
        companion::retire(self.cranker.pubkey(), self.mint, HOOK)
    }

    fn retire(&mut self) -> Tx {
        let ix = self.retire_ix();
        self.send(&[ix])
    }

    fn launched_at(&self) -> i64 {
        self.companion().launched_at
    }

    fn status_ix(&self, audited: bool, pot_cap: u64, blocked: bool) -> Instruction {
        companion::set_hook_status(
            self.w.env.deployer.pubkey(),
            HOOK,
            HookStatusArgs {
                audited,
                pot_cap,
                blocked,
            },
        )
    }

    fn set_status(&mut self, audited: bool, pot_cap: u64, blocked: bool) -> Tx {
        let deployer = self.w.env.deployer.insecure_clone();
        let ix = self.status_ix(audited, pot_cap, blocked);
        self.w.env.send_paid_by(&[ix], &deployer, &[])
    }

    fn buyback(&mut self) -> Tx {
        self.w.env.warp(61);
        for _ in 0..80 {
            let keys = self.keys();
            let custom = self.custom.clone();
            let ix = companion::buyback_with(self.cranker.pubkey(), &keys, false, Some(&custom));
            let tx = self.send(&[ix]);
            if tx.result.is_err() || !tx.events::<BoughtBack>().is_empty() {
                return tx;
            }
            self.w.env.warp(61);
        }
        panic!("no buyback ran");
    }
}

// ---- Finding 1 (high): a draw's claims end with the round after the drawn one -----------------

/// The PoCs used eight attempts of 7.5 minutes in 1-hour rounds (the attempts filling a whole
/// round, so a draw made after the round began spilled into the round after). `create_game` now
/// keeps a draw's attempts within half a round.
#[test]
fn create_game_keeps_a_draws_attempts_within_half_a_round() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(5 * SOL);
    for (window, attempts, ok) in [
        (450, 8, false),
        (300, 7, false),
        (301, 6, false),
        (300, 6, true),
        (900, 2, true),
    ] {
        let mint = Keypair::new();
        let ixs = [
            companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                mint.pubkey(),
                create_args(),
            ),
            lottery::prepare(launcher.pubkey(), mint.pubkey(), ROUND),
            companion::create_game(
                launcher.pubkey(),
                mint.pubkey(),
                CreateGameArgs {
                    claim_window_secs: window,
                    max_attempts: attempts,
                    ..game_args()
                },
            ),
        ];
        let tx = w.env.send_paid_by(&ixs, &launcher, &[&mint]);
        if ok {
            tx.ok();
        } else {
            refused(&tx, CompanionError::BadGame);
        }
    }
}

/// The custody PoC's scene: A and B hold tickets of r0, a seller's dead ones too. The draw of r0
/// is made late in r0 + 1, 45 minutes in, the last moment a draw may be (the pot reached `min_pot`
/// late, or the keeper was slow), and revealed 5 minutes later (ORAO slow). Its attempts open every
/// 5 minutes from the reveal: 0 and 1 in r0 + 1, the rest at or after the draw's claims end
/// (r0 + 2), where they no longer exist.
fn late_draw_scene(
    want: impl Fn(u8, u64, &Range, &Range, &Range) -> bool,
) -> (Lotto, Keypair, Keypair, u32) {
    let mut l = Lotto::new();
    let a = l.buyer(3 * SOL);
    let b = l.buyer(3 * SOL);
    let d = l.buyer(3 * SOL);
    let r0 = l.start_round(&[&a, &b, &d]);
    let (ra, rb, rd) = (
        l.range(&a.pubkey(), r0),
        l.range(&b.pubkey(), r0),
        l.range(&d.pubkey(), r0),
    );
    let all_d = l.balance(&d.pubkey());
    l.w.sell(&d, &l.mint.clone(), all_d).ok();
    l.fund_pot();
    l.warp_into(r0 + 1, 2_700);
    assert_eq!(l.w.env.now, l.game().last_draw(r0));
    l.draw().ok();
    assert_eq!(l.game().status, DrawStatus::Requested);
    let total = l.game().total;
    let r = randomness_where(total, |k, x| want(k, x, &ra, &rb, &rd));
    l.w.env.warp(300);
    l.fulfil(&r);
    l.reveal().ok();
    let g = l.game();
    assert_eq!(g.status, DrawStatus::Revealed);
    assert_eq!(g.claims_end(), i64::from(r0 + 2) * R);
    assert_eq!(
        g.attempt_opens(2),
        Some(g.claims_end()),
        "attempt 2 opens in r0 + 2"
    );
    (l, a, b, r0)
}

/// Control: B wins attempt 1, the last one inside r0 + 1. Whatever anyone writes to B's holding in
/// r0 + 1 (the attacker's `enter`s, dust), B is paid.
#[test]
fn a_winner_inside_the_round_after_is_paid_whatever_is_written_to_it() {
    let (mut l, a, b, _r0) = late_draw_scene(|k, x, ra, rb, rd| match k {
        0 => rd.contains(x),
        1 => rb.contains(x),
        2 => ra.contains(x),
        _ => true,
    });
    let attacker = a.insecure_clone();
    l.enter_for(&attacker, &b.pubkey()).ok();
    l.w.send_tokens(&attacker, l.mint, &b.pubkey(), 1).ok();
    l.warp_to_attempt(1);
    l.enter_for(&attacker, &b.pubkey()).ok();
    let tx = l.claim(1, &b.pubkey());
    tx.ok();
    assert_eq!(tx.event::<PrizePaid>().winner, b.pubkey());
}

/// FINDING 1 (custody PoC): B won attempt 2 and A attempt 3, both opening in r0 + 2. A stranger
/// (the attacker A) enters B in r0 + 1 and again in r0 + 2, which erases B's range of r0. Neither
/// attempt exists any more: B's claim and A's are refused (`DrawLate`), the round rolls over
/// (`Late`) and nobody is paid; the pot stays.
#[test]
fn a_stranger_cannot_take_a_late_attempt_from_its_winner() {
    let (mut l, a, b, r0) = late_draw_scene(|k, x, ra, rb, rd| match k {
        0 | 1 => rd.contains(x),
        2 => rb.contains(x),
        3 => ra.contains(x),
        _ => true,
    });
    let attacker = a.insecure_clone();
    l.enter_for(&attacker, &b.pubkey()).ok();
    l.warp_into(r0 + 2, 5);
    l.enter_for(&attacker, &b.pubkey()).ok();
    assert!(
        l.slots(&b.pubkey()).range_in(r0).is_none(),
        "B's range of r0 is forgotten in r0 + 2"
    );
    let pot = l.companion().pending_pot;
    let a_before = l.w.env.lamports(&a.pubkey());
    l.warp_to_attempt(2);
    refused(&l.claim(2, &b.pubkey()), CompanionError::DrawLate);
    l.warp_to_attempt(3);
    refused(&l.claim(3, &a.pubkey()), CompanionError::DrawLate);
    let tx = l.expire();
    tx.ok();
    let ev: RolledOver = tx.event();
    assert_eq!((ev.round, ev.reason), (r0, RolloverReason::Late));
    assert_eq!(l.companion().pending_pot, pot, "the pot rolls over");
    assert!(
        l.w.env.lamports(&a.pubkey()) <= a_before,
        "the attacker is paid nothing"
    );
}

/// FINDING 1, honest variant: a winner who buys in r0 + 1 and enters (or is entered) any number of
/// times keeps its attempt of r0, claimed in r0 + 1.
#[test]
fn a_winner_who_trades_and_enters_the_next_round_keeps_its_attempt() {
    let (mut l, _a, b, r0) = late_draw_scene(|k, x, _ra, rb, rd| match k {
        0 => rd.contains(x),
        1 => rb.contains(x),
        _ => true,
    });
    l.w.buy(&b, &l.mint.clone(), SOL / 10).ok();
    l.enter_for(&b.insecure_clone(), &b.pubkey()).ok();
    assert_eq!(l.slots(&b.pubkey()).current.round, r0 + 1);
    l.warp_to_attempt(1);
    l.enter_for(&b.insecure_clone(), &b.pubkey()).ok();
    let tx = l.claim(1, &b.pubkey());
    tx.ok();
    assert_eq!(tx.event::<PrizePaid>().winner, b.pubkey());
}

/// FINDING 1 (game PoC 1): A holds 5 SOL of tokens, the attacker E 0.5 SOL. ORAO is slow (its
/// answer lands ten minutes after the request, still in r0 + 1); E enters A before and after. A,
/// who holds attempt 0's ticket, is paid; E's attempt never opens.
/// And when ORAO answers only in r0 + 2, the draw is never revealed: it rolls over, nobody paid.
#[test]
fn a_third_party_cannot_erase_a_winners_tickets() {
    for late in [false, true] {
        let mut l = Lotto::new();
        let a = l.buyer(5 * SOL);
        let e = l.buyer(SOL / 2);
        let r0 = l.start_round(&[&a, &e]);
        l.fund_pot();
        let (ra, re) = (l.range(&a.pubkey(), r0), l.range(&e.pubkey(), r0));
        l.warp_into(r0 + 1, 5);
        l.draw().ok();
        l.enter_for(&e, &a.pubkey()).ok();
        assert_eq!(l.slots(&a.pubkey()).previous, ra);
        if late {
            l.warp_into(r0 + 2, 5);
            l.enter_for(&e, &a.pubkey()).ok();
            let total = l.game().total;
            l.fulfil(&randomness_where(total, |k, x| match k {
                0 => ra.contains(x),
                1 => re.contains(x),
                _ => true,
            }));
            refused(&l.reveal(), CompanionError::DrawLate);
            let pot = l.companion().pending_pot;
            let tx = l.expire();
            tx.ok();
            assert_eq!(tx.event::<RolledOver>().reason, RolloverReason::Late);
            assert_eq!(l.companion().pending_pot, pot);
            refused(&l.claim(1, &e.pubkey()), CompanionError::NoDraw);
            continue;
        }
        l.w.env.warp(R / 6);
        assert_eq!(l.round(), r0 + 1);
        refused(&l.expire(), CompanionError::NotDue);
        let total = l.game().total;
        l.fulfil(&randomness_where(total, |k, x| match k {
            0 => ra.contains(x),
            1 => re.contains(x),
            _ => true,
        }));
        l.reveal().ok();
        l.enter_for(&e, &a.pubkey()).ok();
        l.w.send_tokens(&e, l.mint, &a.pubkey(), 1).ok();
        let tx = l.claim(0, &a.pubkey());
        tx.ok();
        assert_eq!(tx.event::<PrizePaid>().winner, a.pubkey());
        l.warp_to_attempt(1);
        refused(&l.claim(1, &e.pubkey()), CompanionError::NoDraw);
    }
}

/// FINDING 1 (game PoC 2, no oracle failure): the pot reaches `min_pot` only half-way through
/// r0 + 1, so the draw is made then. All its attempts still end by r0 + 2 (half a round): A, who
/// bought more in r0 + 1 and is entered by E right before its attempt, is paid. A draw that could
/// not give its first attempt a whole window before r0 + 2 rolls the round over instead.
#[test]
fn a_late_draw_keeps_every_attempt_within_the_round_after() {
    let mut l = Lotto::new();
    let a = l.buyer(5 * SOL);
    let e = l.buyer(SOL / 2);
    let d = l.buyer(3 * SOL);
    let r0 = l.start_round(&[&a, &e, &d]);
    let (ra, re, rd) = (
        l.range(&a.pubkey(), r0),
        l.range(&e.pubkey(), r0),
        l.range(&d.pubkey(), r0),
    );
    let all_d = l.balance(&d.pubkey());
    l.w.sell(&d, &l.mint.clone(), all_d).ok();
    l.warp_into(r0 + 1, 1_800);
    l.fund_pot();
    l.draw().ok();
    let total = l.game().total;
    l.fulfil(&randomness_where(total, |k, x| match k {
        0..=3 => rd.contains(x),
        4 => ra.contains(x),
        5 => re.contains(x),
        _ => true,
    }));
    l.reveal().ok();
    let g = l.game();
    assert_eq!(
        g.attempt_closes(ATTEMPTS - 1),
        Some(g.claims_end()),
        "the last attempt closes as the claims end"
    );
    l.w.buy(&a, &l.mint.clone(), SOL / 10).ok();
    assert_eq!(l.slots(&a.pubkey()).previous, ra);
    l.warp_to_attempt(4);
    assert_eq!(l.round(), r0 + 1);
    l.enter_for(&e, &a.pubkey()).ok();
    let tx = l.claim(4, &a.pubkey());
    tx.ok();
    assert_eq!(tx.event::<PrizePaid>().winner, a.pubkey());
    l.warp_to_attempt(5);
    refused(&l.claim(5, &e.pubkey()), CompanionError::NoDraw);

    // Too late for a whole window: the draw rolls the round over, and the round is done.
    let mut l = Lotto::new();
    let a = l.buyer(SOL);
    let r0 = l.start_round(&[&a]);
    l.fund_pot();
    l.warp_into(r0 + 1, R - i64::from(WINDOW) + 1);
    let pot = l.companion().pending_pot;
    let tx = l.draw_round(r0);
    tx.ok();
    assert!(tx.events::<DrawCommitted>().is_empty());
    let ev: RolledOver = tx.event();
    assert_eq!(
        (ev.round, ev.reason, ev.pending_pot),
        (r0, RolloverReason::Late, pot)
    );
    assert_eq!(l.game().next_round, r0 + 1);
    refused(&l.draw_round(r0), CompanionError::RoundNotOver);
}

// ---- Finding 2 (medium): a pot capped below its minimum ------------------------------------------

/// FINDING 2: `min_pot` 11 SOL on a hook capped at 10 SOL (not audited). The pot fills to the cap,
/// and is drawn there.
#[test]
fn a_pot_full_at_its_cap_can_be_drawn() {
    let mut l = Lotto::with(
        |g| g.min_pot = 11 * SOL,
        LaunchRules::NONE,
        lottery_hook::FLAGS,
    );
    l.launch_tx.ok();
    let a = l.buyer(3 * SOL);
    let r0 = l.start_round(&[&a]);
    l.volume(16, 50 * SOL);
    l.claim_fees().ok();
    assert_eq!(l.companion().pending_pot, DEFAULT_POT_CAP);
    l.volume(1, 10 * SOL);
    let tx = l.claim_fees();
    tx.ok();
    assert_eq!(tx.event::<PotFunded>().to_pot, 0, "the pot is full");
    l.warp_into(r0 + 1, 10);
    let tx = l.draw();
    tx.ok();
    let ev: DrawRequested = tx.event();
    assert!(ev.made && ev.prize > 9 * SOL);
}

/// FINDING 2, the authority's variant: a cap below the lowest minimum pot is refused; one below the
/// game's own minimum makes the pot drawable at the cap.
#[test]
fn a_cap_lowered_below_min_pot_does_not_brick_a_live_game() {
    let mut l = Lotto::with(
        |g| g.min_pot = SOL / 2,
        LaunchRules::NONE,
        lottery_hook::FLAGS,
    );
    let a = l.buyer(3 * SOL);
    let r0 = l.start_round(&[&a]);
    l.volume(1, 40 * SOL);
    l.claim_fees().ok();
    assert!(l.companion().pending_pot >= SOL / 2);
    refused(
        &l.set_status(false, MIN_POT_CAP / 2, false),
        CompanionError::BadHookStatus,
    );
    refused(
        &l.set_status(false, 0, false),
        CompanionError::BadHookStatus,
    );
    l.set_status(false, MIN_POT_CAP, false).ok();
    l.volume(1, SOL);
    l.claim_fees().ok();
    assert_eq!(l.companion().pending_pot, MIN_POT_CAP);
    l.warp_into(r0 + 1, 10);
    let tx = l.draw();
    tx.ok();
    assert!(tx.event::<DrawRequested>().made);
}

// ---- Finding 3 (medium): decision (c) holds ---------------------------------------------------------

/// FINDING 3: an audit is final. The authority can't un-audit a hook and block it, in one
/// transaction or two; the audited pot stays whole.
#[test]
fn an_audited_hook_cannot_be_blocked_even_in_one_transaction() {
    let mut l = Lotto::new();
    l.set_status(true, 0, false).ok();
    l.volume(16, 50 * SOL);
    l.claim_fees().ok();
    let pot = l.companion().pending_pot;
    assert!(
        pot > DEFAULT_POT_CAP,
        "an audited game's pot is uncapped: {pot}"
    );
    let deployer = l.w.env.deployer.insecure_clone();
    let ixs = [
        l.status_ix(false, DEFAULT_POT_CAP, false),
        l.status_ix(false, DEFAULT_POT_CAP, true),
    ];
    refused(
        &l.w.env.send_paid_by(&ixs, &deployer, &[]),
        CompanionError::BadHookStatus,
    );
    refused(
        &l.set_status(false, DEFAULT_POT_CAP, false),
        CompanionError::BadHookStatus,
    );
    refused(
        &l.set_status(false, DEFAULT_POT_CAP, true),
        CompanionError::BadHookStatus,
    );
    refused(&l.set_status(true, 0, true), CompanionError::BadHookStatus);
    let st: HookStatus = l.w.env.read(&companion::hook_status_address(&HOOK));
    assert!(st.audited && !st.blocked);
    // The next step leaves the pot alone.
    l.volume(1, SOL);
    let tx = l.claim_fees();
    tx.ok();
    assert!(tx.events::<PotToBuyback>().is_empty());
    assert!(l.companion().pending_pot >= pot);
}

/// FINDING 3, the cap: a hook not audited is capped at 10 SOL at most; a higher cap is refused.
#[test]
fn an_unaudited_pot_never_holds_more_than_10_sol() {
    let mut l = Lotto::new();
    for cap in [DEFAULT_POT_CAP + 1, u64::MAX] {
        refused(
            &l.set_status(false, cap, false),
            CompanionError::BadHookStatus,
        );
        refused(
            &l.set_status(false, cap, true),
            CompanionError::BadHookStatus,
        );
    }
    l.set_status(false, DEFAULT_POT_CAP, false).ok();
    l.volume(16, 50 * SOL);
    l.claim_fees().ok();
    assert_eq!(l.companion().pending_pot, DEFAULT_POT_CAP);
}

// ---- Finding 4 (medium): dead-ticket flooding ---------------------------------------------------------

/// FINDING 4 (game PoC 2): the attacker's 0.5 SOL stash bounced 1,500 times between two of its own
/// wallets (75 transactions of 20 hops). Not one ticket is added: the round's total is what its
/// holders held when it began, and the honest holders' odds stay whole.
#[test]
fn self_transfers_add_no_dead_tickets() {
    let mut l = Lotto::new();
    let honest: Vec<Keypair> = (0..4).map(|_| l.buyer(5 * SOL / 2)).collect();
    let e1 = l.buyer(SOL / 2);
    let e2 = l.w.wallet_with_sol(SOL);
    l.w.holdings(&e2, l.mint, &[e2.pubkey()]);
    let mut entered: Vec<&Keypair> = honest.iter().collect();
    entered.push(&e1);
    let r0 = l.start_round(&entered);
    let live_honest: u64 = honest.iter().map(|h| l.range(&h.pubkey(), r0).weight).sum();
    let stash = l.balance(&e1.pubkey());
    let total_before = l.header().total;
    assert_eq!(total_before, live_honest + stash);
    let mint = l.mint;
    let extras = l.w.env.token_hook_extras(
        &HOOK,
        &mint,
        &token::holding_address(&mint, &e1.pubkey()),
        &token::holding_address(&mint, &e2.pubkey()),
        &e1.pubkey(),
        &e1.pubkey(),
        &e2.pubkey(),
    );
    for _ in 0..75 {
        let mut ixs = Vec::new();
        for h in 0..20 {
            let (from, to) = if h % 2 == 0 { (&e1, &e2) } else { (&e2, &e1) };
            ixs.push(token::transfer(
                from.pubkey(),
                token::holding_address(&mint, &from.pubkey()),
                token::holding_address(&mint, &to.pubkey()),
                mint,
                Some(HOOK),
                extras.clone(),
                stash,
            ));
        }
        l.w.env.send_paid_by(&ixs, &e1, &[&e2]).ok();
    }
    let cranker = l.cranker.insecure_clone();
    l.enter_for(&cranker, &e1.pubkey()).ok();
    l.enter_for(&cranker, &e2.pubkey()).ok();
    let total = l.header().total;
    assert_eq!(total, total_before, "1,500 hops added no ticket");
    let live_share = live_honest as f64 / total as f64;
    let p_no_winner = (1.0 - live_share).powi(i32::from(ATTEMPTS));
    println!("live share {live_share:.4}; P(all {ATTEMPTS} attempts dead) = {p_no_winner:.2e}");
    assert!(p_no_winner < 1e-6);
    // The stash counts again from the next round, once.
    let r1 = l.start_round(&[&e1, &e2]);
    let stash_tickets: u64 = [&e1, &e2]
        .iter()
        .filter_map(|k| l.slots(&k.pubkey()).range_in(r1))
        .map(|r| r.weight)
        .sum();
    assert_eq!(stash_tickets, stash);
}

/// FINDING 6 (low, game PoC 3): splitting buys across wallets earns no more tickets than
/// consolidating them: what arrives during a round counts from the next, for everyone.
#[test]
fn splitting_buys_across_wallets_earns_no_more_than_consolidating() {
    let mut l = Lotto::new();
    let h = l.buyer(2 * SOL);
    let s1 = l.buyer(2 * SOL);
    let x = l.buyer(SOL / 100);
    let r0 = l.start_round(&[&h, &s1, &x]);
    let (h0, s0) = (
        l.range(&h.pubkey(), r0).weight,
        l.range(&s1.pubkey(), r0).weight,
    );
    // Both top up by 1 SOL: H into its own wallet, S into a fresh one. Neither top-up counts in r0.
    let mint = l.mint;
    l.w.buy(&h, &mint, SOL).ok();
    let s2 = l.buyer(SOL);
    let cranker = l.cranker.insecure_clone();
    l.enter_for(&cranker, &h.pubkey()).ok();
    l.enter_for(&cranker, &s2.pubkey()).ok();
    assert_eq!(l.range(&h.pubkey(), r0).weight, h0);
    assert!(l.slots(&s2.pubkey()).range_in(r0).is_none());
    assert_eq!(l.range(&s1.pubkey(), r0).weight, s0);
    // From r0 + 1, a ticket for every token, both ways.
    let r1 = l.start_round(&[&h, &s1, &s2]);
    assert_eq!(l.range(&h.pubkey(), r1).weight, l.balance(&h.pubkey()));
    assert_eq!(
        l.range(&s1.pubkey(), r1).weight + l.range(&s2.pubkey(), r1).weight,
        l.balance(&s1.pubkey()) + l.balance(&s2.pubkey())
    );
}

// ---- Finding 5 (low): a game launch's flags are exactly the lottery's ---------------------------------

/// FINDING 5 (custody PoC 4): a game hook that takes transfer deltas could skim every companion
/// buyback (and a blocked pot) into a holding of its choosing. Here `hook_tester` plays the game
/// hook (a game header for the mint, a registry naming the attacker's holding): `create_game`
/// takes it, but a launch from a config whose flags add `TRANSFER_RETURNS_DELTA` is refused.
#[test]
fn a_game_hook_cannot_take_deltas_from_the_companions_buyback() {
    let mut w = World::new();
    let hook = hook_tester::ID;
    vet(&mut w, hook);
    let launcher = w.wallet_with_sol(50 * SOL);
    let attacker = w.wallet_with_sol(SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let (state, _) = bordrless_game::state_address(&hook, &mint);
    let mut data = vec![0u8; 8];
    data.extend_from_slice(&GameHeader::new(mint, ROUND, w.env.now).encode());
    let lamports = w.env.rent(data.len());
    w.env.put(
        state,
        Account {
            lamports,
            data,
            owner: hook,
            executable: false,
            rent_epoch: 0,
        },
    );
    let extras = vec![
        ExtraAccount {
            writable: true,
            source: AccountSource::Pda {
                program: hook,
                seeds: vec![Seed::Literal(b"state".to_vec()), Seed::Account(1)],
            },
        },
        ExtraAccount {
            writable: true,
            source: AccountSource::Key(token::holding_address(&mint, &attacker.pubkey())),
        },
    ];
    let setup = [
        companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
        hook_tester::client::init_script(launcher.pubkey(), mint, extras),
        companion::create_game(
            launcher.pubkey(),
            mint,
            CreateGameArgs {
                hook,
                ..game_args()
            },
        ),
    ];
    w.env.send_paid_by(&setup, &launcher, &[&mint_kp]).ok();
    for flags in [
        lottery_hook::FLAGS | token_flags::TRANSFER_RETURNS_DELTA,
        lottery_hook::FLAGS | token_flags::AFTER_BURN,
    ] {
        let (config, tx) = w.create_config(
            &launcher,
            CreateConfigArgs {
                rules: LaunchRules::NONE,
                creator_fee_bps: CREATOR_FEE,
                custom_hook: Some(hook),
                custom_hook_flags: flags,
                label: "Lottery".to_string(),
            },
        );
        tx.ok();
        let ix = companion_launch_ix(&w, &launcher.pubkey(), &mint, &config, hook);
        refused(
            &w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]),
            CompanionError::GameHookMismatch,
        );
    }
    assert!(w.env.account(&launch::launch_address(&mint)).is_none());
}

/// FINDING 5 (integration PoC 2): `lottery_hook` named with `AFTER_BURN` too (a callback it lacks)
/// would launch, then fail every burn: the buyback stranded, graduation impossible. Such a launch
/// is refused; with exactly the lottery's flags, buybacks burn and the coin graduates.
#[test]
fn a_lottery_launch_with_extra_hook_flags_is_refused() {
    for extra in [token_flags::AFTER_BURN, token_flags::TRANSFER_RETURNS_DELTA] {
        let l = Lotto::with(|_| {}, LaunchRules::NONE, lottery_hook::FLAGS | extra);
        refused(&l.launch_tx, CompanionError::GameHookMismatch);
    }
    let mut l = Lotto::new();
    l.launch_tx.ok();
    l.volume(3, 10 * SOL);
    l.claim_fees().ok();
    assert!(l.companion().pending_buyback > 0);
    let tx = l.buyback();
    tx.ok();
    assert!(l.companion().burned_total > 0);
    let mint = l.mint;
    let (_, tx) = l.w.graduate_launch(&mint);
    tx.ok();
}

// ---- Finding 7 (low): a game hook's registry a launch can carry -------------------------------------

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

/// FINDING 7 (integration PoC 1): at the site's real metadata URI (Pinata's gateway and a CIDv1,
/// 93 bytes), a game hook launch fits with the lottery hook (state + launch) and with 3 extras
/// besides the launch, but not with 4 besides it (1,240 bytes). `create_game` refuses a hook whose
/// registry lists 4 extras besides the launch, before anything is spent on its launch.
#[test]
fn a_game_hook_launch_fits_at_the_sites_uri() {
    let mut w = World::new();
    let table = w.env.put_lookup_table(Pubkey::new_unique(), &table_22(&w));
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let size = |w: &World, extras: Vec<AccountMeta>| {
        let mut args = World::launch_args(&"S".repeat(10), CREATOR_FEE, VQ, LaunchRules::NONE);
        args.name = "N".repeat(32);
        args.uri = "https://gateway.pinata.cloud/ipfs/bafkreigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi"
            .to_string();
        assert_eq!(args.uri.len(), 93);
        let custom = CustomHookAccounts {
            program: HOOK,
            extras,
        };
        let inner = launch::create_launch_with(
            companion::creator_address(&mint),
            mint,
            w.env.treasury.pubkey(),
            w.sol,
            policy::LP_FEE_BPS,
            args.clone(),
            Some(Pubkey::new_unique()),
            Some(&custom),
        );
        let ixs = [
            compute_unit_limit(1_400_000),
            compute_unit_price(20_000),
            companion::launch(launcher.pubkey(), mint, &inner, args),
        ];
        w.env
            .v0_size(&ixs, &launcher, &[&mint_kp], std::slice::from_ref(&table))
    };
    let state = AccountMeta::new(lottery::state_address(&mint), false);
    let launch_pda = AccountMeta::new_readonly(launch::launch_address(&mint), false);
    let new = || AccountMeta::new(Pubkey::new_unique(), false);
    let lottery_size = size(&w, lottery::extras(&mint));
    let three_and_launch = size(&w, vec![state.clone(), launch_pda, new(), new()]);
    let four = size(&w, vec![state, new(), new(), new()]);
    println!(
        "at the site's URI: lottery hook {lottery_size} bytes; state, launch and 2 more \
         {three_and_launch}; state and 3 more {four}"
    );
    assert!(lottery_size <= PACKET && three_and_launch <= PACKET);
    assert!(four > PACKET, "the limit `create_game` enforces");

    // `create_game` reads the hook's registry: 4 extras besides the launch are refused, 3 and the
    // launch are fine. `hook_tester` plays the hook (its registry starts with its own script),
    // vetted by the protocol.
    let hook = hook_tester::ID;
    vet(&mut w, hook);
    let pda = |program: Pubkey, tag: &[u8]| ExtraAccount {
        writable: false,
        source: AccountSource::Pda {
            program,
            seeds: vec![Seed::Literal(tag.to_vec()), Seed::Account(1)],
        },
    };
    for (extras, ok) in [
        (
            vec![pda(hook, b"state"), pda(hook, b"x1"), pda(hook, b"x2")],
            false,
        ),
        (
            vec![
                pda(hook, b"state"),
                pda(bordrless_launch::ID, b"launch"),
                pda(hook, b"x1"),
            ],
            true,
        ),
        (
            vec![
                pda(hook, b"state"),
                ExtraAccount {
                    writable: false,
                    source: AccountSource::Key(Pubkey::new_unique()),
                },
                ExtraAccount {
                    writable: false,
                    source: AccountSource::Key(Pubkey::new_unique()),
                },
            ],
            false,
        ),
    ] {
        let mint = Keypair::new();
        let (state, _) = bordrless_game::state_address(&hook, &mint.pubkey());
        let mut data = vec![0u8; 8];
        data.extend_from_slice(&GameHeader::new(mint.pubkey(), ROUND, w.env.now).encode());
        let lamports = w.env.rent(data.len());
        w.env.put(
            state,
            Account {
                lamports,
                data,
                owner: hook,
                executable: false,
                rent_epoch: 0,
            },
        );
        let ixs = [
            companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                mint.pubkey(),
                create_args(),
            ),
            hook_tester::client::init_script(launcher.pubkey(), mint.pubkey(), extras),
            companion::create_game(
                launcher.pubkey(),
                mint.pubkey(),
                CreateGameArgs {
                    hook,
                    ..game_args()
                },
            ),
        ];
        let tx = w.env.send_paid_by(&ixs, &launcher, &[&mint]);
        if ok {
            tx.ok();
        } else {
            refused(&tx, CompanionError::TooManyHookExtras);
        }
    }
    // A registry not the hook's: refused.
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
    let registry = lottery::registry_address(&mint.pubkey());
    let mut acc = w.env.account(&registry).unwrap();
    acc.owner = bordrless_swap::ID;
    w.env.put(registry, acc);
    refused(
        &w.env.send_paid_by(
            &[companion::create_game(
                launcher.pubkey(),
                mint.pubkey(),
                game_args(),
            )],
            &launcher,
            &[&mint],
        ),
        CompanionError::HookRegistry,
    );
}

// ---- Finding 8 (low): an answer the companion can't read ---------------------------------------------

/// FINDING 8 (integration PoC 3): ORAO answers every request of the game in a layout the companion
/// does not read. The pot pays for one request; the round then rolls over (`OracleUnreadable`)
/// with no new seed bought.
#[test]
fn an_unreadable_answer_buys_no_more_requests() {
    let mut l = Lotto::new();
    let a = l.buyer(5 * SOL);
    let r0 = l.start_round(&[&a]);
    l.volume(1, 20 * SOL);
    l.claim_fees().ok();
    l.warp_into(r0 + 1, 1);
    let pot_before = l.companion().pending_pot;
    let tx = l.draw_round(r0);
    tx.ok();
    let paid = u32::from(tx.event::<DrawRequested>().made);
    let g = l.game();
    assert_eq!(g.status, DrawStatus::Requested);
    orao::fulfil(&mut l.w.env, &g.seed, &[7u8; 64]);
    let key = orao::request_address(&g.seed);
    let mut acc = l.w.env.account(&key).unwrap();
    acc.data[8] = 2;
    l.w.env.put(key, acc);
    refused(&l.reveal(), CompanionError::OracleAccount);
    l.w.env.warp(R / 6);
    let tx = l.expire();
    tx.ok();
    assert_eq!(
        tx.event::<RolledOver>().reason,
        RolloverReason::OracleUnreadable
    );
    assert_eq!(l.game().status, DrawStatus::Idle);
    assert!(tx.events::<DrawRequested>().is_empty(), "no new request");
    assert_eq!(
        paid, 1,
        "one request paid, none after the unreadable answer"
    );
    let spent = pot_before - l.companion().pending_pot;
    println!("round {r0}: {paid} request paid ({spent} lamports), then rolled over");
}

// ---- What the integration audit checked and found sound (kept) --------------------------------------

/// Asserts a landed transaction fits mainnet's limits, and prints its measures.
#[track_caller]
fn within_limits(name: &str, tx: &Tx) {
    tx.ok();
    println!(
        "{name}: CU {}, height {}, trace {}",
        tx.cu(),
        tx.max_height(),
        tx.trace_len()
    );
    assert!(tx.max_height() <= 5, "{name}: height {}", tx.max_height());
    assert!(tx.trace_len() <= 64, "{name}: trace {}", tx.trace_len());
    assert!(tx.cu() <= 1_400_000, "{name}: {} CU", tx.cu());
}

/// Every game path at its heaviest (burn rules on both sides; the hook's registry rewritten after
/// the launch to 4 extras: its 2 and 2 it ignores) stays at stack height <= 5, a trace <= 64 and
/// far below 1.4M CU; the 32 KiB heap holds.
#[test]
fn every_game_path_fits_with_burns_and_four_extras() {
    let rules = LaunchRules {
        burn_buy_bps: 50,
        burn_sell_bps: 50,
        ..LaunchRules::NONE
    };
    let mut l = Lotto::with(|_| {}, rules, lottery_hook::FLAGS);
    within_limits("launch", &l.launch_tx);
    let mint = l.mint;
    let registry = lottery::registry_address(&mint);
    let pda = |program: Pubkey, tag: &[u8], writable: bool| ExtraAccount {
        writable,
        source: AccountSource::Pda {
            program,
            seeds: vec![Seed::Literal(tag.to_vec()), Seed::Account(1)],
        },
    };
    let list = HookAccountList::new(vec![
        pda(HOOK, b"state", true),
        pda(bordrless_launch::ID, b"launch", false),
        pda(HOOK, b"x1", true),
        pda(HOOK, b"x2", true),
    ]);
    let mut acc = l.w.env.account(&registry).unwrap();
    acc.data = list.encode();
    acc.lamports = l.w.env.rent(acc.data.len());
    l.w.env.put(registry, acc);
    l.custom = l.w.custom_hook_accounts(&HOOK, &mint);
    assert_eq!(l.custom.extras.len(), 4);
    let custom = l.custom.clone();
    let keys = l.keys();
    let launcher = l.launcher.insecure_clone();
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
    within_limits("dev_buy", &tx);
    let a = l.w.wallet_with_sol(6 * SOL);
    l.w.buy(&a, &mint, 5 * SOL).ok();
    let r0 = l.start_round(&[&a]);
    l.volume(3, 10 * SOL);
    let tx = l.claim_fees();
    within_limits("claim_fees", &tx);
    let tx = l.buyback();
    within_limits("buyback", &tx);
    let tx = l.send(&[companion::release_with(
        l.cranker.pubkey(),
        &keys,
        false,
        launcher.pubkey(),
        Some(&custom),
    )]);
    within_limits("release", &tx);
    assert!(l.companion().pending_pot >= MIN_POT);
    l.warp_into(r0 + 1, 1);
    let tx = l.draw_round(r0);
    within_limits("draw", &tx);
    assert!(tx.event::<DrawRequested>().made);
    let g = l.game();
    let range = l.range(&a.pubkey(), r0);
    let r = randomness_where(g.total, |k, x| k != 0 || range.contains(x));
    l.fulfil(&r);
    let tx = l.reveal();
    within_limits("reveal", &tx);
    let tx = l.claim(0, &a.pubkey());
    within_limits("claim_prize", &tx);
    assert_eq!(tx.event::<PrizePaid>().winner, a.pubkey());
    assert_eq!(l.game().status, DrawStatus::Idle);
}

/// The seeds of the newest slots are public, so anyone can fund the request address of the seed a
/// draw is about to name (a lamport, a rent-exempt minimum, 10 SOL). ORAO's init tops it up, never
/// refuses it: nobody blocks a draw's request that way, so it is no cheap way to make the pot unable
/// to pay (the final audit's control for finding 19).
#[test]
fn a_funded_request_address_does_not_block_the_request() {
    for prefund in [1, 0, 10 * SOL] {
        let mut l = Lotto::new();
        let a = l.buyer(5 * SOL);
        let r0 = l.start_round(&[&a]);
        l.volume(1, 20 * SOL);
        l.claim_fees().ok();
        l.warp_into(r0 + 1, 1);
        let at = set_slot_hash(&mut l.w.env);
        let seed = at.seed(&l.mint, r0);
        let lamports = if prefund == 0 {
            l.w.env.rent(0)
        } else {
            prefund
        };
        l.w.env.fund(orao::request_address(&seed), lamports);
        let tx = l.draw_at(r0, at);
        tx.ok();
        assert!(tx.event::<DrawRequested>().made);
        let g = l.game();
        assert_eq!(g.seed, seed);
        assert_eq!(
            orao::pending_client(&l.w.env, &g.seed),
            Some(companion::oracle_payer_address(&l.mint))
        );
        l.fulfil(&orao::randomness_for(&g.seed));
        l.reveal().ok();
        assert_eq!(l.game().status, DrawStatus::Revealed);
    }
}

/// ORAO's legacy v1 `request(seed)`, `payer` paying: still in the deployed binary, and its account
/// lands at the very address a v2 request for the seed would use.
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

/// ORAO's answer to a v1 request: the 64 random bytes after the seed (the account keeps its
/// length, as all of mainnet's v1 requests do).
fn fulfil_v1(env: &mut Env, seed: &[u8; 32], randomness: &[u8; 64]) {
    let key = orao::request_address(seed);
    let mut account = env.account(&key).expect("a v1 request");
    assert_eq!(account.data[..8], [188, 96, 216, 248, 93, 94, 49, 112]);
    assert_eq!(&account.data[8..40], &seed[..]);
    account.data[40..104].copy_from_slice(randomness);
    env.put(key, account);
}

// ---- Finding 9 (medium): a pot below its draw threshold was locked for ever ----------------------

/// FINDING 9 (custody PoC 1a): a lottery coin under the audited lottery hook, `min_pot` 2 SOL. Its
/// pot reaches about 1.1 SOL, then trading stops. Until the game is dormant the pot is below its
/// minimum, and the audited hook can't be capped or blocked (decision c holds). Once the pot has
/// paid no prize for 30 days, the keeper's next draw takes it from 0.1 SOL: the last holder is
/// paid the whole pot. The prize restarts the clock: the next pot waits for the minimum again.
#[test]
fn a_dead_coins_pot_is_drawn_once_its_game_is_dormant() {
    let mut l = Lotto::with(
        |g| g.min_pot = 2 * SOL,
        LaunchRules::NONE,
        lottery_hook::FLAGS,
    );
    l.set_status(true, 0, false).ok();
    let holder = l.buyer(2 * SOL);
    l.volume(1, 40 * SOL);
    l.claim_fees().ok();
    let pot = l.companion().pending_pot;
    assert!(pot > SOL && pot < 2 * SOL, "below its minimum: {pot}");
    let dormant = l.launched_at() + DORMANT_SECS;
    assert_eq!(l.game().dormant_secs(), DORMANT_SECS);

    // The last round drawn before the game is dormant: refused, and nothing else moves the pot.
    let r = round_of(dormant, ROUND) - 2;
    l.warp_into(r, 5);
    l.enter(&holder.pubkey()).ok();
    l.warp_into(r + 1, 5);
    assert!(l.w.env.now < dormant);
    refused(&l.draw_round(r), CompanionError::PotTooSmall);
    refused(
        &l.set_status(false, DEFAULT_POT_CAP, true),
        CompanionError::BadHookStatus,
    );
    refused(&l.retire(), CompanionError::NotDue);
    assert_eq!(l.companion().pending_pot, pot);

    // Dormant: the keeper enters the holder and draws that round, from the floor.
    let d = round_of(dormant, ROUND) + 1;
    l.warp_into(d, 5);
    l.enter(&holder.pubkey()).ok();
    l.warp_into(d + 1, 5);
    let tx = l.draw_round(d);
    tx.ok();
    assert_eq!(tx.event::<DrawCommitted>().round, d);
    let total = l.game().total;
    assert_eq!(total, l.balance(&holder.pubkey()));
    l.fulfil(&randomness_where(total, |_, _| true));
    l.reveal().ok();
    let before = l.w.env.lamports(&holder.pubkey());
    let tx = l.claim(0, &holder.pubkey());
    tx.ok();
    let paid: PrizePaid = tx.event();
    assert_eq!(paid.winner, holder.pubkey());
    assert_eq!(l.w.env.lamports(&holder.pubkey()) - before, paid.prize);
    assert!(paid.prize > pot / 100 * 98, "the whole pot, less costs");
    assert_eq!(l.companion().pending_pot, 0);
    assert_eq!(l.game().settled_at, l.w.env.now);

    // The prize restarted the clock: a new pot below the minimum waits for it again.
    l.volume(1, 10 * SOL);
    l.claim_fees().ok();
    let pot = l.companion().pending_pot;
    assert!((MIN_MIN_POT..2 * SOL).contains(&pot), "{pot}");
    let next = l.round() + 1;
    l.warp_into(next, 5);
    l.enter(&holder.pubkey()).ok();
    l.warp_into(next + 1, 5);
    refused(&l.draw_round(next), CompanionError::PotTooSmall);
}

/// FINDING 9 (custody PoC 1a, the certain case): a prize below 100% leaves part of the pot, and once
/// trading stops nothing tops it up. Here the pot is below even the dormant floor (0.1 SOL), under
/// a hook with no status at all: no draw ever takes it. After two dormant periods with no prize,
/// anyone may retire it to the buyback, which burns it: nobody is paid it, the sender included. No
/// cap or block of the hook (which would bind every game of the hook) is needed.
#[test]
fn a_pot_below_the_floor_is_retired_after_two_dormant_periods() {
    let mut l = Lotto::new();
    let holder = l.buyer(SOL);
    l.volume(1, 2 * SOL);
    l.claim_fees().ok();
    let pot = l.companion().pending_pot;
    assert!(pot > 0 && pot < MIN_MIN_POT, "below the floor: {pot}");
    assert!(l
        .w
        .env
        .account(&companion::hook_status_address(&HOOK))
        .is_none());
    let launched = l.launched_at();
    let retirable = l.game().retirable_at(launched);
    assert_eq!(retirable, launched + 2 * DORMANT_SECS);

    // Dormant, and still below the floor.
    let d = round_of(launched + DORMANT_SECS, ROUND) + 1;
    l.warp_into(d, 5);
    l.enter(&holder.pubkey()).ok();
    l.warp_into(d + 1, 5);
    refused(&l.draw_round(d), CompanionError::PotTooSmall);
    refused(&l.retire(), CompanionError::NotDue);
    l.warp_to(retirable - 1);
    refused(&l.retire(), CompanionError::NotDue);

    l.w.env.warp(1);
    let c0 = l.companion();
    let cranker = l.cranker.pubkey();
    let cranker_sol = l.w.env.lamports(&cranker);
    let tx = l.retire();
    tx.ok();
    let ev: PotRetired = tx.event();
    assert_eq!(
        (ev.lamports, ev.idle_since, ev.mint, ev.cranker),
        (pot, launched, l.mint, cranker)
    );
    let c = l.companion();
    assert_eq!(c.pending_pot, 0);
    assert_eq!(c.pending_buyback, c0.pending_buyback + pot);
    assert_eq!(ev.pending_buyback, c.pending_buyback);
    assert_eq!(c.bounties_total, c0.bounties_total, "no bounty");
    assert!(l.w.env.lamports(&cranker) < cranker_sol, "nobody is paid");
    assert_eq!(l.game().settled_at, l.w.env.now);
    // The clock restarted: nothing more to retire for two dormant periods.
    refused(&l.retire(), CompanionError::NotDue);
    // The buyback spends it and burns what it buys.
    let tx = l.buyback();
    tx.ok();
    assert!(tx.event::<BoughtBack>().burned > 0);
}

// ---- Finding 10 (medium): a round's seed is final ------------------------------------------------

/// The game PoCs' scene: A holds about ten times E's tickets of r0. The draw of r0 is committed and
/// requested 5 seconds into r0 + 1. ORAO's answer to its seed pays A at attempt 0 (and E at
/// attempt 1, were A's claim to fail). Answers the lottery, A, E and that answer.
fn censor_scene() -> (Lotto, Keypair, Keypair, [u8; 64]) {
    let mut l = Lotto::new();
    let a = l.buyer(5 * SOL);
    let e = l.buyer(SOL / 2);
    let r0 = l.start_round(&[&a, &e]);
    l.fund_pot();
    let (ra, re) = (l.range(&a.pubkey(), r0), l.range(&e.pubkey(), r0));
    l.warp_into(r0 + 1, 5);
    l.draw_round(r0).ok();
    assert_eq!(l.game().status, DrawStatus::Requested);
    let total = l.game().total;
    let answer = randomness_where(total, |k, x| match k {
        0 => ra.contains(x),
        1 => re.contains(x),
        _ => true,
    });
    (l, a, e, answer)
}

/// Control: nobody keeps ORAO out. Its answer lands, and A is paid.
#[test]
fn the_first_answer_decides_the_draw() {
    let (mut l, a, _e, answer) = censor_scene();
    l.fulfil(&answer);
    l.reveal().ok();
    let tx = l.claim(0, &a.pubkey());
    tx.ok();
    assert_eq!(tx.event::<PrizePaid>().winner, a.pubkey());
}

/// FINDING 10 (game PoC 1, a re-draw): E previews ORAO's answer on devnet (A wins) and keeps it out
/// of the blocks for the old timeout (ten minutes), then twice more. `expire` never gives the draw
/// a new seed: when ORAO's answer lands it decides the draw, and A is paid; E's attempt never opens.
#[test]
fn keeping_orao_out_until_a_timeout_buys_no_redraw() {
    let (mut l, a, e, answer) = censor_scene();
    let seed = l.game().seed;
    for _ in 0..3 {
        l.w.env.warp(600);
        refused(&l.expire(), CompanionError::NotDue);
        let g = l.game();
        assert_eq!((g.status, g.n, g.seed), (DrawStatus::Requested, 0, seed));
    }
    l.fulfil(&answer);
    l.reveal().ok();
    refused(&l.claim(0, &e.pubkey()), CompanionError::NotTheWinner);
    let tx = l.claim(0, &a.pubkey());
    tx.ok();
    assert_eq!(tx.event::<PrizePaid>().winner, a.pubkey());
    l.warp_to_attempt(1);
    refused(&l.claim(1, &e.pubkey()), CompanionError::NoDraw);
}

/// FINDING 10 (game PoC 2, a forced rollover): ORAO kept out three timeouts long, then answering,
/// pays A. All a censor can still do is keep ORAO out for the rest of the round after the drawn
/// one (about 55 minutes of 1-hour rounds, paying for every block): the round then rolls over
/// (`OracleSilent`) with no new seed, ORAO's late answer is never read, nobody is paid and the pot
/// waits. It can never choose between answers.
#[test]
fn keeping_orao_out_only_delays_a_draw_or_rolls_it_over() {
    let (mut l, a, _e, answer) = censor_scene();
    l.w.env.warp(3 * 600);
    refused(&l.expire(), CompanionError::NotDue);
    l.fulfil(&answer);
    l.reveal().ok();
    let tx = l.claim(0, &a.pubkey());
    tx.ok();
    assert_eq!(tx.event::<PrizePaid>().winner, a.pubkey());

    // Kept out until the claims end.
    let (mut l, a, e, answer) = censor_scene();
    let seed = l.game().seed;
    let end = l.game().claims_end();
    l.warp_to(end - 1);
    refused(&l.expire(), CompanionError::NotDue);
    l.w.env.warp(1);
    let pot = l.companion().pending_pot;
    let (a_sol, e_sol) = (l.w.env.lamports(&a.pubkey()), l.w.env.lamports(&e.pubkey()));
    let tx = l.expire();
    tx.ok();
    assert!(tx.events::<DrawCommitted>().is_empty(), "no new seed");
    let ev: RolledOver = tx.event();
    assert_eq!(
        (ev.reason, ev.pending_pot),
        (RolloverReason::OracleSilent, pot)
    );
    orao::fulfil(&mut l.w.env, &seed, &answer);
    refused(&l.reveal(), CompanionError::NoDraw);
    refused(&l.claim(0, &a.pubkey()), CompanionError::NoDraw);
    assert_eq!(
        (l.w.env.lamports(&a.pubkey()), l.w.env.lamports(&e.pubkey())),
        (a_sol, e_sol)
    );
    assert_eq!(l.companion().pending_pot, pot);
}

// ---- Finding 11 (low): an audited pot the oracle can't draw --------------------------------------

/// FINDING 11 (custody PoC 1b): ORAO's fee goes above the cap (or ORAO stops) under an audited hook
/// whose pot holds about 11 SOL, uncapped. Every round's draw rolls over (since the final audit at
/// once, `OracleUnpaid`, committing no seed), and the hook can't be blocked. After two dormant
/// periods with no prize, anyone retires the pot to the buyback; nobody is paid it. Fees fill the
/// pot again, and the clock restarts.
#[test]
fn an_audited_pot_the_oracle_cant_draw_is_retired_after_two_dormant_periods() {
    let mut l = Lotto::new();
    l.set_status(true, 0, false).ok();
    let holder = l.buyer(2 * SOL);
    l.volume(10, 40 * SOL);
    l.claim_fees().ok();
    let pot = l.companion().pending_pot;
    assert!(pot > DEFAULT_POT_CAP, "uncapped: {pot}");
    orao::set_fee(&mut l.w.env, seeds::MAX_REQUEST_FEE + 1);
    let r0 = l.start_round(&[&holder]);
    l.warp_into(r0 + 1, 5);
    let tx = l.draw_round(r0);
    tx.ok();
    assert_eq!(
        tx.event::<RolledOver>().reason,
        RolloverReason::OracleUnpaid
    );
    assert_eq!(l.game().status, DrawStatus::Idle);
    refused(&l.draw_round(r0), CompanionError::RoundNotOver);
    refused(&l.retire(), CompanionError::NotDue);
    refused(
        &l.set_status(false, DEFAULT_POT_CAP, true),
        CompanionError::BadHookStatus,
    );

    // Two months on, the keeper still draws every round, and every draw rolls over at once.
    let launched = l.launched_at();
    let retirable = l.game().retirable_at(launched);
    let x = round_of(retirable, ROUND) - 2;
    l.warp_into(x, 5);
    refused(&l.retire(), CompanionError::NotDue);
    l.enter(&holder.pubkey()).ok();
    l.warp_into(x + 1, 5);
    let tx = l.draw_round(x);
    assert_eq!(
        tx.event::<RolledOver>().reason,
        RolloverReason::OracleUnpaid
    );
    l.warp_to(retirable - 1);
    refused(&l.retire(), CompanionError::NotDue);
    l.w.env.warp(1);
    // No draw is ever in progress: the pot goes to the buyback.
    let c0 = l.companion();
    let tx = l.retire();
    tx.ok();
    let ev: PotRetired = tx.event();
    assert_eq!((ev.lamports, ev.idle_since), (pot, launched));
    let c = l.companion();
    assert_eq!(
        (c.pending_pot, c.pending_buyback),
        (0, c0.pending_buyback + pot)
    );
    // Decision (c) holds: the hook is still audited, and still can't be blocked.
    let status: HookStatus = l.w.env.read(&companion::hook_status_address(&HOOK));
    assert!(status.audited && !status.blocked);
    refused(
        &l.set_status(false, DEFAULT_POT_CAP, true),
        CompanionError::BadHookStatus,
    );
    // The game goes on: fees fill the pot again, and the clock restarted.
    l.volume(1, 20 * SOL);
    l.claim_fees().ok();
    assert!(l.companion().pending_pot > 0);
    refused(&l.retire(), CompanionError::NotDue);
}

// ---- Finding 13 (low): ORAO's layouts bound by their length ----------------------------------------

/// FINDING 13 (integration PoC 1): ORAO upgraded so a fulfilled answer carries 64 more bytes before
/// the randomness, under the same discriminator and variant. The companion read the public bytes
/// at the old offsets as R. Now a fulfilled v2 answer must be exactly 137 bytes: `reveal` refuses
/// it (`OracleAccount`) and `expire` rolls the round over at once (`OracleUnreadable`), the pot
/// intact. A pending request of another length than 749 bytes is refused too, and so is a v1
/// request of another length than ORAO's 780, while ORAO's own v1 request (made by its deployed
/// binary) is adopted and, answered, read.
#[test]
fn an_answer_in_another_layout_fails_closed() {
    let mut l = Lotto::new();
    let a = l.buyer(5 * SOL);
    let b = l.buyer(5 * SOL);
    let r0 = l.start_round(&[&a, &b]);
    l.fund_pot();
    l.warp_into(r0 + 1, 1);
    l.draw_round(r0).ok();
    let g = l.game();
    let pending = l.w.env.account(&g.request).unwrap();
    assert_eq!(pending.data.len(), seeds::PENDING_LEN);
    let mut public = [0u8; 64];
    public[..32].copy_from_slice(orao::NETWORK_STATE.as_ref());
    public[32..].copy_from_slice(orao::TREASURY.as_ref());
    let mut data = Vec::with_capacity(seeds::FULFILLED_LEN + 64);
    data.extend_from_slice(&orao::RANDOMNESS_V2);
    data.push(1);
    data.extend_from_slice(&pending.data[9..73]); // client, seed
    data.extend_from_slice(&public);
    data.extend_from_slice(&orao::randomness_for(&g.seed));
    let lamports = l.w.env.rent(data.len());
    l.w.env.put(
        g.request,
        Account {
            lamports,
            data,
            owner: orao::ORAO_VRF_ID,
            executable: false,
            rent_epoch: 0,
        },
    );
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
    assert_eq!(l.game().randomness, [0u8; 64], "nothing was read");

    // A pending request of another length at the address of the seed a draw is about to name:
    // refused, the pot pays nothing and nothing is committed. The next slot's seed is drawn.
    let r1 = l.start_round(&[&a, &b]);
    l.warp_into(r1 + 1, 1);
    let at = set_slot_hash(&mut l.w.env);
    let seed = at.seed(&l.mint, r1);
    let mut data = vec![0u8; seeds::PENDING_LEN + 96];
    data[..8].copy_from_slice(&orao::RANDOMNESS_V2);
    data[9..41].copy_from_slice(Pubkey::new_unique().as_ref());
    data[41..73].copy_from_slice(&seed);
    let lamports = l.w.env.rent(data.len());
    l.w.env.put(
        orao::request_address(&seed),
        Account {
            lamports,
            data,
            owner: orao::ORAO_VRF_ID,
            executable: false,
            rent_epoch: 0,
        },
    );
    let pot = l.companion().pending_pot;
    refused(&l.draw_at(r1, at), CompanionError::OracleAccount);
    assert_eq!(l.companion().pending_pot, pot);
    assert_eq!(l.game().status, DrawStatus::Idle);
    l.w.env.warp(1);
    let tx = l.draw_round(r1);
    tx.ok();
    assert!(tx.event::<DrawRequested>().made);
    assert_ne!(l.game().seed, seed);

    // ORAO's own v1 request is 780 bytes: adopted, and read once answered; cut short, refused.
    let r2 = l.start_round(&[&a, &b]);
    l.warp_into(r2 + 1, 1);
    l.expire().ok();
    let at = set_slot_hash(&mut l.w.env);
    let seed = at.seed(&l.mint, r2);
    let key = orao::request_address(&seed);
    let squatter = l.w.wallet_with_sol(SOL);
    l.w.env
        .send_paid_by(&[v1_request_ix(squatter.pubkey(), seed)], &squatter, &[])
        .ok();
    assert_eq!(l.w.env.account(&key).unwrap().data.len(), seeds::V1_LEN);
    let tx = l.draw_at(r2, at);
    tx.ok();
    assert!(!tx.event::<DrawRequested>().made);
    assert_eq!(l.game().seed, seed);
    let ra = l.range(&a.pubkey(), r2);
    let answer = randomness_where(l.game().total, |k, x| k != 0 || ra.contains(x));
    fulfil_v1(&mut l.w.env, &seed, &answer);
    let whole = l.w.env.account(&key).unwrap();
    let mut short = whole.clone();
    short.data.truncate(104);
    l.w.env.put(key, short);
    refused(&l.reveal(), CompanionError::OracleAccount);
    l.w.env.put(key, whole);
    l.reveal().ok();
    assert_eq!(l.game().randomness, answer);
    let tx = l.claim(0, &a.pubkey());
    tx.ok();
    assert_eq!(tx.event::<PrizePaid>().winner, a.pubkey());
}

// ---- Finding 14 (low): v1 squats ORAO might stop answering -----------------------------------------

/// FINDING 14 (integration PoC 2): the seed a draw is about to name can be computed from the newest
/// slot hashes, and anyone may request it first with ORAO's deprecated v1 `request` (blind: before
/// anyone knows its answer), which the draw adopts (its answer equals the v2 one, and ORAO answers
/// v1 today). Were ORAO to stop answering v1, each squat
/// would leave its round unanswered. Now a squat costs at most that round: no new seed is ever made
/// for it (one would let the squatter, who can preview answers on devnet, pick among draws), and
/// the round rolls over only when its claims end: one squat a round (three before). Kept up for two
/// dormant periods, it can't lock the pot: the pot is retired to the buyback.
#[test]
fn an_unanswered_v1_squat_costs_one_round_and_never_locks_the_pot() {
    let mut l = Lotto::new();
    let a = l.buyer(5 * SOL);
    l.fund_pot();
    let attacker = l.w.wallet_with_sol(SOL);
    let before = l.w.env.lamports(&attacker.pubkey());
    let pot = l.companion().pending_pot;
    for _ in 0..2 {
        let r = l.start_round(&[&a]);
        l.warp_into(r + 1, 1);
        let at = set_slot_hash(&mut l.w.env);
        let seed = at.seed(&l.mint, r);
        l.w.env
            .send_paid_by(&[v1_request_ix(attacker.pubkey(), seed)], &attacker, &[])
            .ok();
        let tx = l.draw_at(r, at);
        tx.ok();
        assert_eq!(l.game().seed, seed);
        assert!(
            !tx.event::<DrawRequested>().made,
            "adopted: the pot pays nothing"
        );
        refused(&l.reveal(), CompanionError::OracleNotFulfilled);
        // ORAO never answers v1 here: no new seed, however long it waits.
        l.w.env.warp(R / 2);
        refused(&l.expire(), CompanionError::NotDue);
        assert_eq!((l.game().seed, l.game().n), (seed, 0));
        l.warp_into(r + 2, 0);
        let tx = l.expire();
        tx.ok();
        assert!(tx.events::<DrawCommitted>().is_empty());
        assert_eq!(
            tx.event::<RolledOver>().reason,
            RolloverReason::OracleSilent
        );
    }
    let cost = before - l.w.env.lamports(&attacker.pubkey());
    println!("two rounds squatted with one v1 request each: {cost} lamports");
    assert_eq!(l.companion().pending_pot, pot, "the pot paid ORAO nothing");
    // However long it goes on, the pot is retired after two dormant periods with no prize.
    let retirable = l.game().retirable_at(l.launched_at());
    l.warp_to(retirable);
    let tx = l.retire();
    tx.ok();
    assert_eq!(tx.event::<PotRetired>().lamports, pot);
    assert_eq!(l.companion().pending_pot, 0);
}

// ---- Checked and found sound by the integration audit (round 2), kept -------------------------------

/// After its curve graduates (`launch.pool` stays the same pool, its curve off), a game coin's
/// companion still claims and splits its fees, and its buyback still swaps and burns through the
/// custom hook.
#[test]
fn a_graduated_game_coin_still_buys_back_through_its_hook() {
    let mut l = Lotto::new();
    let mint = l.mint;
    let keys = l.keys();
    let custom = l.w.custom_hook_accounts(&HOOK, &mint);
    let (_, tx) = l.w.graduate_launch(&mint);
    tx.ok();
    assert!(!l.w.launch_pool(&mint).curve, "graduated");
    // Volume on the graduated pool, then the fees.
    l.volume(3, 10 * SOL);
    l.claim_fees().ok();
    assert!(l.companion().pending_buyback > 0);
    let supply_before = l.w.env.read::<bordrless_token::state::Mint>(&mint).supply;
    let mut bought = false;
    for _ in 0..80 {
        l.w.env.warp(61);
        let ix = companion::buyback_with(l.cranker.pubkey(), &keys, false, Some(&custom));
        let tx = l.send(&[ix]);
        tx.ok();
        if !tx.events::<BoughtBack>().is_empty() {
            bought = true;
            break;
        }
    }
    assert!(bought, "a buyback ran on the graduated pool");
    let supply_after = l.w.env.read::<bordrless_token::state::Mint>(&mint).supply;
    assert!(supply_after < supply_before, "and burned through the hook");
    assert_eq!(
        l.w.env.hook_data(&mint, &companion::creator_address(&mint)),
        [0u8; 64],
        "the creator address never holds tickets"
    );
}

// ==== Audit round 3 ==================================================================================

// ---- Finding 15 (medium): a silent oracle drained the pot, one request every round ----------------

/// The rounds (after `first`) whose draw the pot pays a request for while ORAO never answers: the
/// first, then 1, 2, 4, 8… rounds after each (`Game::oracle_backoff_rounds`).
fn backoff_schedule(first: u32, rounds: u32) -> Vec<u32> {
    let mut paid = vec![first];
    let mut gap = 1;
    while paid.last().unwrap() + gap < first + rounds {
        paid.push(paid.last().unwrap() + gap);
        gap *= 2;
    }
    paid
}

/// FINDING 15 (integration r3 PoC 1). ORAO's program keeps taking requests but never answers (its
/// quorum is all three of its signers: one offline is enough). A bot cranks every hourly round:
/// `enter`, `draw` (which requests), and at the claims' end `expire`. Before the fix the pot
/// paid ORAO's fee and a 749-byte account's rent every round (48 requests in 48 rounds, about 7.2
/// SOL over the 60 days before `retire` on mainnet), and the bounty paid the bot to go on.
///
/// Now the pot pays for a new request only while ORAO answered its last one, or 1, 2, 4, 8…
/// rounds after it: 6 requests in 48 rounds. In between, the draw rolls its round over at once
/// (`OracleUnpaid`, final audit), committing no seed: it costs nothing and pays no bounty. Once ORAO
/// answers its last paid request, the next draw is paid at once and the game goes on.
#[test]
fn a_silent_oracle_costs_the_pot_a_request_now_and_then_not_every_round() {
    const ROUNDS: u32 = 48;
    let mut l = Lotto::new();
    l.launch_tx.ok();
    let holder = l.buyer(5 * SOL);
    l.volume(4, 40 * SOL);
    l.claim_fees().ok();
    let pot0 = l.companion().pending_pot;
    assert!(pot0 > SOL, "a pot to drain: {pot0}");
    let payer = companion::oracle_payer_address(&l.mint);
    let first = l.round() + 1;
    l.warp_into(first, 5);
    l.enter(&holder.pubkey()).ok();
    let mut paid = Vec::new();
    let mut paid_by_pot = 0u64;
    let mut seeds_paid = Vec::new();
    for y in first + 1..=first + ROUNDS {
        l.warp_into(y, 5);
        // The previous round's draw: ORAO never answered it (`OracleSilent`). A draw the breaker
        // held was never committed.
        match l.game().status {
            DrawStatus::Requested => {
                let tx = l.expire();
                assert_eq!(
                    tx.event::<RolledOver>().reason,
                    RolloverReason::OracleSilent
                );
            }
            status => assert_eq!(status, DrawStatus::Idle),
        }
        l.enter(&holder.pubkey()).ok();
        let (pot, cranker) = (
            l.companion().pending_pot,
            l.w.env.lamports(&l.cranker.pubkey()),
        );
        let commit = l.draw_round(y - 1);
        commit.ok();
        if l.game().status == DrawStatus::Requested {
            let ev: DrawRequested = commit.event();
            assert!(ev.made);
            paid.push(y - 1);
            assert_eq!(usize::from(ev.paid_streak), paid.len());
            paid_by_pot += ev.top_up + ev.bounty;
            seeds_paid.push(l.game().seed);
        } else {
            assert_eq!(
                commit.event::<RolledOver>().reason,
                RolloverReason::OracleUnpaid
            );
            assert!(
                commit.events::<DrawCommitted>().is_empty(),
                "no seed public"
            );
            assert_eq!(
                l.companion().pending_pot,
                pot,
                "a round the breaker holds costs nothing"
            );
            assert!(
                l.w.env.lamports(&l.cranker.pubkey()) < cranker,
                "and pays no bounty"
            );
            refused(&l.draw_round(y - 1), CompanionError::RoundNotOver);
        }
    }
    assert_eq!(paid, backoff_schedule(first, ROUNDS));
    assert_eq!(paid.len(), 6, "{paid:?}");
    let pot = l.companion().pending_pot;
    assert_eq!(pot0 - pot, paid_by_pot, "the pot paid only those");
    assert_eq!(l.game().prizes_paid, 0);
    for s in &seeds_paid {
        assert_eq!(orao::pending_client(&l.w.env, s), Some(payer));
    }
    // Mainnet's figures for hourly rounds over the 60 days before `retire`: at most 11 requests
    // (then one every 30 days), where every round paid one before.
    let mut g = l.game();
    g.paid_seed = [1; 32];
    g.paid_round = 0;
    g.paid_streak = 1;
    let mut requests = 1;
    for round in 1..60 * 24 {
        g.round = round;
        if g.oracle_backoff_over() {
            requests += 1;
            g.paid_round = round;
            g.paid_streak += 1;
        }
    }
    let per_request = orao::FEE + (128 + seeds::PENDING_LEN as u64) * 5_080;
    println!(
        "silent ORAO, 48 hourly rounds: {} requests paid ({paid_by_pot} lamports, {pot0} -> \
         {pot}); over 60 days of hourly rounds: {requests} requests, about {} SOL on mainnet \
         (it was 1,440 requests, about {} SOL)",
        paid.len(),
        (per_request * requests) as f64 / 1e9,
        (per_request * 1_440) as f64 / 1e9,
    );
    assert_eq!(requests, 11);

    // ORAO answers its last paid request (late, as its service does): the next round's draw is
    // paid at once, and the holder is paid.
    let last = *seeds_paid.last().unwrap();
    assert_eq!(l.game().paid_seed, last);
    orao::fulfil(&mut l.w.env, &last, &[5; 64]);
    let next = l.round() + 1;
    l.warp_into(next, 5);
    if l.game().status == DrawStatus::Requested {
        l.expire().ok();
    }
    l.enter(&holder.pubkey()).ok();
    let tx = l.draw_round(next - 1);
    tx.ok();
    let ev: DrawRequested = tx.event();
    assert!(ev.made);
    assert_eq!(ev.paid_streak, 1, "the breaker reset");
    l.fulfil(&[6; 64]);
    l.reveal().ok();
    assert!(!l.game().has_paid_request());
    let tx = l.claim(0, &holder.pubkey());
    tx.ok();
    assert_eq!(tx.event::<PrizePaid>().winner, holder.pubkey());
}

/// While the breaker holds, `draw` rolls the round over at once (`OracleUnpaid`, final audit):
/// no seed is committed, so nobody can preview its answer and pay for it only if it wins (the
/// round-3 version of this test had anyone's request for the held round adopted). Leaving the
/// unanswered request out of `draw` is refused, so it can't get around the breaker. Once ORAO
/// answers the pot's last paid request, the next draw is paid at once with the breaker reset, and
/// its reveal clears it. A draw (which requests) with the unanswered request passed still fits a
/// packet.
#[test]
fn while_the_breaker_holds_no_seed_is_committed_and_an_answer_resets_it() {
    let mut l = Lotto::new();
    let a = l.buyer(5 * SOL);
    let r0 = l.start_round(&[&a]);
    l.volume(2, 20 * SOL);
    l.claim_fees().ok();
    // r0's draw: paid; ORAO never answers it.
    let r1 = l.start_round(&[&a]);
    let tx = l.draw();
    tx.ok();
    assert_eq!(tx.event::<DrawRequested>().paid_streak, 1);
    let g = l.game();
    assert_eq!((g.paid_seed, g.paid_round), (g.seed, r0));
    // r1's draw: the last paid request is unanswered, but one round after it: paid.
    let r2 = l.start_round(&[&a]);
    assert_eq!(
        l.expire().event::<RolledOver>().reason,
        RolloverReason::OracleSilent
    );
    let tx = l.draw();
    within_limits("draw after an unanswered request", &tx);
    println!("  {} bytes", tx.size);
    assert!(tx.size <= PACKET, "{} bytes", tx.size);
    let ev: DrawRequested = tx.event();
    assert!(ev.made);
    assert_eq!(ev.paid_streak, 2);
    let r1_seed = l.game().seed;
    assert_eq!((l.game().paid_round, l.game().paid_seed), (r1, r1_seed));
    // r2's draw: only one round after r1's, the breaker holds. Without the unanswered request,
    // refused; with it, the round rolls over and no seed is committed. The pot pays nothing.
    let r3 = l.start_round(&[&a]);
    l.expire().ok();
    let pot = l.companion().pending_pot;
    let at = set_slot_hash(&mut l.w.env);
    let plain = companion::draw(l.cranker.pubkey(), l.mint, HOOK, r2, at, orao::TREASURY);
    refused(&l.send(&[plain]), CompanionError::MissingAccount);
    let tx = l.draw_round(r2);
    tx.ok();
    let ev: RolledOver = tx.event();
    assert_eq!((ev.round, ev.reason), (r2, RolloverReason::OracleUnpaid));
    assert!(tx.events::<DrawCommitted>().is_empty());
    let g = l.game();
    assert_eq!(
        (g.status, g.seed, g.next_round),
        (DrawStatus::Idle, r1_seed, r3)
    );
    assert_eq!(l.companion().pending_pot, pot, "the pot paid nothing");
    refused(&l.draw_round(r2), CompanionError::RoundNotOver);
    // ORAO answers r1's request after all: r3's draw is paid at once, the breaker reset.
    orao::fulfil(&mut l.w.env, &r1_seed, &[5; 64]);
    l.start_round(&[&a]);
    let tx = l.draw();
    tx.ok();
    let ev: DrawRequested = tx.event();
    assert!(ev.made);
    assert_eq!(
        ev.paid_streak, 1,
        "an answered paid request resets the breaker"
    );
    // Revealed: the breaker clears, and the holder is paid.
    let range = l.range(&a.pubkey(), r3);
    let total = l.game().total;
    l.fulfil(&randomness_where(total, |k, x| k != 0 || range.contains(x)));
    l.reveal().ok();
    let g = l.game();
    assert!(!g.has_paid_request());
    assert_eq!((g.paid_round, g.paid_streak), (0, 0));
    let tx = l.claim(0, &a.pubkey());
    tx.ok();
    assert_eq!(tx.event::<PrizePaid>().winner, a.pubkey());
    // The next draw is paid at once, the first request still unanswered.
    l.fund_pot();
    l.start_round(&[&a]);
    let tx = l.draw();
    tx.ok();
    let ev: DrawRequested = tx.event();
    assert!(ev.made);
    assert_eq!(ev.paid_streak, 1);
}

/// FINDING 15, second form (integration r3 PoC 2): ORAO answers in a layout the companion fails
/// closed on (64 more bytes). Each round paid for a new request, which `expire` rolled over at once
/// (`OracleUnreadable`): 24 requests in 24 rounds. Now an unreadable answer holds the breaker
/// like a silent one: 5 requests in 24 rounds, then one every 30 days at most, until this program
/// learns the new layout.
#[test]
fn an_answer_the_companion_cant_read_costs_a_request_now_and_then() {
    const ROUNDS: u32 = 24;
    let mut l = Lotto::new();
    let holder = l.buyer(5 * SOL);
    l.volume(2, 40 * SOL);
    l.claim_fees().ok();
    let pot0 = l.companion().pending_pot;
    let payer = companion::oracle_payer_address(&l.mint);
    let first = l.round() + 1;
    l.warp_into(first, 5);
    l.enter(&holder.pubkey()).ok();
    let mut paid = Vec::new();
    for y in first + 1..=first + ROUNDS {
        l.warp_into(y, 5);
        assert_eq!(l.game().status, DrawStatus::Idle);
        l.enter(&holder.pubkey()).ok();
        let commit = l.draw_round(y - 1);
        commit.ok();
        if l.game().status != DrawStatus::Requested {
            // The breaker holds: rolled over at once, no seed committed (final audit).
            assert_eq!(
                commit.event::<RolledOver>().reason,
                RolloverReason::OracleUnpaid
            );
            continue;
        }
        assert!(commit.event::<DrawRequested>().made);
        paid.push(y - 1);
        // ORAO's answer, as its last `fulfill_v2` would leave it in a 201-byte layout.
        let g = l.game();
        let mut acc = l.w.env.account(&g.request).unwrap();
        let mut data = acc.data[..73].to_vec();
        data[8] = 1;
        data.extend_from_slice(&[7u8; 64]);
        data.extend_from_slice(&orao::randomness_for(&g.seed));
        let keep = l.w.env.rent(data.len());
        let refund = acc.lamports - keep;
        acc.data = data;
        acc.lamports = keep;
        l.w.env.put(g.request, acc);
        let to = l.w.env.lamports(&payer) + refund;
        l.w.env.fund(payer, to);
        let tx = l.expire();
        assert_eq!(
            tx.event::<RolledOver>().reason,
            RolloverReason::OracleUnreadable
        );
    }
    assert_eq!(paid, backoff_schedule(first, ROUNDS));
    assert_eq!(paid.len(), 5, "{paid:?}");
    assert_eq!(l.game().prizes_paid, 0);
    println!(
        "unreadable answers, {ROUNDS} rounds: {} requests paid, pot {pot0} -> {}",
        paid.len(),
        l.companion().pending_pot
    );
}

// ---- Findings 16 and 17 (low): a game hook could strand the buyback, and a blocked pot with it ---

/// The setup of a game coin on `hook_tester` (a game header for the mint in its state, its
/// registry naming its script and that state), playing a hook its launcher wrote and deployed
/// immutable; `answer(mint)` scripts it before `create_game`. Answers the setup transaction and
/// the mint.
fn tester_setup(
    w: &mut World,
    launcher: &Keypair,
    answer: impl FnOnce(Pubkey) -> Vec<Instruction>,
) -> (Tx, Keypair) {
    let hook = hook_tester::ID;
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let (state, _) = bordrless_game::state_address(&hook, &mint);
    let mut data = vec![0u8; 8];
    data.extend_from_slice(&GameHeader::new(mint, ROUND, w.env.now).encode());
    let lamports = w.env.rent(data.len());
    w.env.put(
        state,
        Account {
            lamports,
            data,
            owner: hook,
            executable: false,
            rent_epoch: 0,
        },
    );
    let state_extra = ExtraAccount {
        writable: false,
        source: AccountSource::Pda {
            program: hook,
            seeds: vec![Seed::Literal(b"state".to_vec()), Seed::Account(1)],
        },
    };
    let mut setup = vec![
        companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
        hook_tester::client::init_script(launcher.pubkey(), mint, vec![state_extra]),
    ];
    setup.extend(answer(mint));
    setup.push(companion::create_game(
        launcher.pubkey(),
        mint,
        CreateGameArgs {
            hook,
            ..game_args()
        },
    ));
    let tx = w.env.send_paid_by(&setup, launcher, &[&mint_kp]);
    (tx, mint_kp)
}

/// FINDINGS 16 and 17, first fix: in phase 1 `create_game` takes Bordrless's lottery hook, or a
/// hook the protocol has vetted (written a status for), never a blocked one. An unvetted hook
/// (here `hook_tester`, which could refuse only the companion's own moves) is refused before
/// anything is spent on its launch.
#[test]
fn create_game_takes_the_lottery_hook_or_a_hook_the_protocol_vetted() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(50 * SOL);
    let hook = hook_tester::ID;
    let (tx, _) = tester_setup(&mut w, &launcher, |_| vec![]);
    refused(&tx, CompanionError::GameHookNotAccepted);
    vet(&mut w, hook);
    let (tx, mint) = tester_setup(&mut w, &launcher, |_| vec![]);
    tx.ok();
    let g: Game = w.env.read(&companion::game_address(&mint.pubkey()));
    assert_eq!(
        (g.hook, g.status_bump),
        (hook, HookStatus::address(&hook).1)
    );
    // Blocked: no new game takes it.
    let deployer = w.env.deployer.insecure_clone();
    let block = |hook| {
        companion::set_hook_status(
            deployer.pubkey(),
            hook,
            HookStatusArgs {
                audited: false,
                pot_cap: DEFAULT_POT_CAP,
                blocked: true,
            },
        )
    };
    w.env.send_paid_by(&[block(hook)], &deployer, &[]).ok();
    let (tx, _) = tester_setup(&mut w, &launcher, |_| vec![]);
    refused(&tx, CompanionError::GameHookNotAccepted);
    // The lottery hook needs no status (every other test makes its game so)...
    let lottery_game = |w: &mut World| {
        let mint = Keypair::new();
        let m = mint.pubkey();
        let ixs = [
            companion::create(launcher.pubkey(), launcher.pubkey(), m, create_args()),
            lottery::prepare(launcher.pubkey(), m, ROUND),
            companion::create_game(launcher.pubkey(), m, game_args()),
        ];
        w.env.send_paid_by(&ixs, &launcher, &[&mint])
    };
    lottery_game(&mut w).ok();
    assert_eq!(LOTTERY_HOOK_ID, lottery_hook::ID);
    // ...but a blocked one is refused too.
    w.env.send_paid_by(&[block(HOOK)], &deployer, &[]).ok();
    refused(&lottery_game(&mut w), CompanionError::GameHookNotAccepted);
}

/// FINDING 17 (integration r3 PoC). `hook_tester`, vetted by the protocol, then refusing every
/// burn (a real hook would refuse only the companion's creator address's, so graduation still
/// burns). People buy and sell as usual, and every buyback fails at its burn. The protocol blocks
/// the hook: the pot moves to the buyback, and still no buyback can run. Before the fix nothing
/// else could spend `pending_buyback`: 6.47 SOL were locked for ever. Second fix: once no buyback
/// has bought or waited (moved its reference price) for 30 days since the block, anyone may burn
/// it as SOL (`burn_stranded`), paying nobody. Each burn restarts the 30 days (final audit), so a
/// later fee claim's share is burned only 30 days after the last burn.
#[test]
fn a_blocked_hook_that_refuses_burns_has_its_buyback_burned_as_sol() {
    let hook = hook_tester::ID;
    let mut w = World::new();
    vet(&mut w, hook);
    let launcher = w.wallet_with_sol(50 * SOL);
    let l = launcher.pubkey();
    let (tx, mint_kp) = tester_setup(&mut w, &launcher, |mint| {
        vec![hook_tester::client::set_answer(
            l,
            mint,
            hook_tester::callback::BEFORE_BURN,
            hook_tester::mode::FAIL,
            vec![],
        )]
    });
    tx.ok();
    let mint = mint_kp.pubkey();
    let (config, tx) = w.create_config(
        &launcher,
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: CREATOR_FEE,
            custom_hook: Some(hook),
            custom_hook_flags: lottery_hook::FLAGS,
            label: "Game".to_string(),
        },
    );
    tx.ok();
    let custom = w.custom_hook_accounts(&hook, &mint);
    let ix = companion_launch_ix(&w, &launcher.pubkey(), &mint, &config, hook);
    w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]).ok();
    w.env.warp(31);

    // People trade as on any coin.
    for _ in 0..4 {
        let t = w.wallet_with_sol(41 * SOL);
        w.buy(&t, &mint, 40 * SOL).ok();
        let held = w.env.holding(&mint, &t.pubkey());
        w.sell(&t, &mint, held).ok();
    }
    let cranker = w.wallet_with_sol(SOL);
    let claim = |w: &mut World| {
        w.env.send_paid_by(
            &[companion::claim_fees_game(cranker.pubkey(), mint, hook)],
            &cranker,
            &[],
        )
    };
    claim(&mut w).ok();
    let buyback = |w: &mut World| {
        w.env.warp(61);
        let keys = w.launch_keys(&mint);
        let ix = companion::buyback_with(cranker.pubkey(), &keys, false, Some(&custom));
        w.env.send_paid_by(&[ix], &cranker, &[])
    };
    let tx = buyback(&mut w);
    assert!(
        tx.result.is_err()
            && tx
                .logs()
                .iter()
                .any(|l| l.contains(&format!("Program {hook} failed"))),
        "{}",
        tx.logs().join("\n")
    );

    // The protocol blocks the hook: the pot goes to the buyback, which still can't run.
    let deployer = w.env.deployer.insecure_clone();
    w.env
        .send_paid_by(
            &[companion::set_hook_status(
                deployer.pubkey(),
                hook,
                HookStatusArgs {
                    audited: false,
                    pot_cap: DEFAULT_POT_CAP,
                    blocked: true,
                },
            )],
            &deployer,
            &[],
        )
        .ok();
    let blocked_at = w.env.now;
    let t = w.wallet_with_sol(11 * SOL);
    w.buy(&t, &mint, 10 * SOL).ok();
    claim(&mut w).ok();
    let c: Companion = w.env.read(&companion::companion_address(&mint));
    assert_eq!(c.pending_pot, 0, "blocked: the pot went to the buyback");
    // Buybacks fail at the burn (or wait, the price above the reference, moving it): none buys.
    let mut waited = 0;
    for _ in 0..5 {
        let tx = buyback(&mut w);
        assert!(tx.result.is_err() || tx.events::<BoughtBack>().is_empty());
        waited += usize::from(tx.result.is_ok());
    }
    let c: Companion = w.env.read(&companion::companion_address(&mint));
    let since = blocked_at.max(c.reference_at);
    assert!(
        waited > 0 && since > blocked_at,
        "a wait moved the reference"
    );
    let burn = |w: &mut World| {
        let ix = companion::burn_stranded(cranker.pubkey(), mint, hook);
        w.env.send_paid_by(&[ix], &cranker, &[])
    };
    // Not before 30 days with no buy, and no wait, since the block.
    refused(&burn(&mut w), CompanionError::NotDue);
    w.env.warp(since + STRANDED_SECS - 1 - w.env.now);
    refused(&burn(&mut w), CompanionError::NotDue);
    w.env.warp(1);
    let c: Companion = w.env.read(&companion::companion_address(&mint));
    let stranded = c.pending_buyback;
    let creator = companion::creator_address(&mint);
    let (held, burned0, cranker0, beneficiary0) = (
        w.env.holding(&w.sol, &creator),
        w.env.lamports(&INCINERATOR),
        w.env.lamports(&cranker.pubkey()),
        w.env.lamports(&launcher.pubkey()),
    );
    let tx = burn(&mut w);
    tx.ok();
    let ev: StrandedBurned = tx.event();
    assert_eq!(
        (ev.lamports, ev.since, ev.cranker),
        (stranded, since, cranker.pubkey())
    );
    let burned_at = w.env.now;
    assert_eq!(w.env.lamports(&INCINERATOR) - burned0, stranded, "burned");
    assert_eq!(w.env.holding(&w.sol, &creator), held - stranded);
    assert!(
        w.env.lamports(&cranker.pubkey()) < cranker0,
        "the sender is paid nothing"
    );
    assert_eq!(w.env.lamports(&launcher.pubkey()), beneficiary0);
    let c: Companion = w.env.read(&companion::companion_address(&mint));
    assert_eq!((c.pending_buyback, c.pending_pot), (0, 0));
    assert!(w.env.holding(&w.sol, &creator) >= c.set_aside().unwrap());
    // The burn restarted the wait.
    refused(&burn(&mut w), CompanionError::NotDue);
    // Holders still trade; a later fee claim's share (all of it, blocked) is burned the same way,
    // once 30 days have passed since the last burn: not in the claim's own transaction.
    let held = w.env.holding(&mint, &t.pubkey());
    w.sell(&t, &mint, held).ok();
    claim(&mut w).ok();
    let c: Companion = w.env.read(&companion::companion_address(&mint));
    assert_eq!(c.stranded_burned_at, burned_at);
    assert!(c.pending_buyback > 0);
    refused(&burn(&mut w), CompanionError::NotDue);
    w.env.warp(burned_at + STRANDED_SECS - 1 - w.env.now);
    refused(&burn(&mut w), CompanionError::NotDue);
    w.env.warp(1);
    let tx = burn(&mut w);
    tx.ok();
    let ev: StrandedBurned = tx.event();
    assert_eq!((ev.lamports, ev.since), (c.pending_buyback, burned_at));
}

/// `burn_stranded` takes only a blocked game hook's buyback that has neither bought nor waited for
/// 30 days: not a companion without a game, not a hook with no status or one not blocked, and not
/// while keepers land the buybacks a working hook allows (each restarts the wait, as does each
/// wait that moves the reference: `final_audit`). A pot no step had moved yet is moved to the
/// buyback by the burn's call and kept there a whole wait, burned only by a later call.
#[test]
fn burn_stranded_takes_only_a_blocked_hooks_stranded_buyback() {
    // A companion without a game.
    let mut w = World::new();
    let launcher = w.wallet_with_sol(50 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
    w.env
        .send_paid_by(
            &[companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                m,
                create_args(),
            )],
            &launcher,
            &[&mint],
        )
        .ok();
    let args = World::launch_args("PLAIN", CREATOR_FEE, VQ, LaunchRules::NONE);
    let inner = launch::create_launch_with(
        companion::creator_address(&m),
        m,
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        args.clone(),
        None,
        None,
    );
    let ix = companion::launch(launcher.pubkey(), m, &inner, args);
    w.env.send_paid_by(&[ix], &launcher, &[&mint]).ok();
    let ix = companion::burn_stranded(launcher.pubkey(), m, Pubkey::default());
    refused(
        &w.env.send_paid_by(&[ix], &launcher, &[]),
        CompanionError::NotAGame,
    );

    // A game whose hook has no status, then one not blocked.
    let mut l = Lotto::new();
    l.volume(4, 40 * SOL);
    l.claim_fees().ok();
    let burn = |l: &mut Lotto| {
        let ix = companion::burn_stranded(l.cranker.pubkey(), l.mint, HOOK);
        l.send(&[ix])
    };
    refused(&burn(&mut l), CompanionError::HookNotBlocked);
    l.set_status(false, DEFAULT_POT_CAP, false).ok();
    refused(&burn(&mut l), CompanionError::HookNotBlocked);
    // Blocked: the hook works, so keepers land buybacks, each restarting the wait.
    l.set_status(false, DEFAULT_POT_CAP, true).ok();
    let blocked_at = l.w.env.now;
    refused(&burn(&mut l), CompanionError::NotDue);
    l.warp_to(blocked_at + STRANDED_SECS - 86_400);
    l.buyback().ok();
    let last = l.companion().last_buyback_at;
    assert!(last > blocked_at);
    l.warp_to(blocked_at + STRANDED_SECS + 60);
    refused(&burn(&mut l), CompanionError::NotDue);
    // Then nobody buys back for 30 days. The pot no step had moved is moved to the buyback by the
    // burn's call, and nothing is burned: the pot gets a whole wait of its own, for a buyback to
    // spend it (the final audit's residual), as a share credited to a buyback that had spent all
    // it was given does (residual 23).
    l.warp_to(last + STRANDED_SECS);
    let c = l.companion();
    let (pot, buyback) = (c.pending_pot, c.pending_buyback);
    assert!(pot > 0 && buyback > 0);
    let burned0 = l.w.env.lamports(&INCINERATOR);
    let tx = burn(&mut l);
    tx.ok();
    let moved: PotToBuyback = tx.event();
    assert!(moved.blocked);
    assert_eq!((moved.lamports, moved.pending_pot), (pot, 0));
    assert!(tx.events::<StrandedBurned>().is_empty(), "nothing burned");
    assert_eq!(l.w.env.lamports(&INCINERATOR), burned0);
    let moved_at = l.w.env.now;
    let c = l.companion();
    assert_eq!(
        (c.pending_pot, c.pending_buyback, c.stranded_burned_at),
        (0, pot + buyback, moved_at)
    );
    assert_eq!(c.stranded_at(blocked_at), moved_at + STRANDED_SECS);
    refused(&burn(&mut l), CompanionError::NotDue);
    l.warp_to(moved_at + STRANDED_SECS - 1);
    refused(&burn(&mut l), CompanionError::NotDue);
    // Still nobody buys back: now the whole buyback is burned, the pot with it.
    l.w.env.warp(1);
    let tx = burn(&mut l);
    tx.ok();
    assert!(tx.events::<PotToBuyback>().is_empty());
    let ev: StrandedBurned = tx.event();
    assert_eq!((ev.lamports, ev.since), (pot + buyback, moved_at));
    assert_eq!(l.w.env.lamports(&INCINERATOR) - burned0, pot + buyback);
    let c = l.companion();
    assert_eq!((c.pending_pot, c.pending_buyback), (0, 0));
}

// ---- Checked and found sound in round 3, kept ---------------------------------------------------

/// (Game lens, round 3.) G sends its whole bag to the holding of a deployed program's id (here
/// `half_life`'s, an executable account on the ed25519 curve), A keeps its own. The keeper enters
/// both; attempt 0 lands in the program id's range and attempt 1 in A's. The claim of attempt 0
/// is refused by the runtime (`ExternalAccountLamportSpend`: a CPI never writes an executable
/// account back to its caller), the pot keeps the prize, and A is paid attempt 1.
#[test]
fn a_ticket_held_at_a_program_id_passes_to_the_next_attempt() {
    let program = half_life::ID;
    let account = |l: &Lotto| l.w.env.account(&program).expect("the program is deployed");
    let mut l = Lotto::new();
    assert!(
        program.is_on_curve(),
        "a keypair-made program id is on the curve"
    );
    assert!(account(&l).executable, "and it is an executable account");
    let a = l.buyer(3 * SOL);
    let g = l.buyer(3 * SOL);
    l.w.env
        .send_paid_by(
            &[token::create_holding(g.pubkey(), l.mint, program)],
            &g,
            &[],
        )
        .ok();
    let bag = l.balance(&g.pubkey());
    let mint = l.mint;
    l.w.send_tokens(&g, mint, &program, bag).ok();
    assert_eq!(l.balance(&program), bag);
    let r0 = l.round() + 1;
    l.warp_into(r0, 5);
    l.enter(&a.pubkey()).ok();
    l.enter(&program).ok();
    let (ra, rp) = (l.range(&a.pubkey(), r0), l.range(&program, r0));
    assert_eq!(
        rp.weight, bag,
        "the program id holds tickets like any holder"
    );
    l.fund_pot();
    l.warp_into(r0 + 1, 5);
    l.draw().ok();
    let total = l.game().total;
    l.fulfil(&randomness_where(total, |k, x| match k {
        0 => rp.contains(x),
        1 => ra.contains(x),
        _ => true,
    }));
    l.reveal().ok();
    let prize = l.game().prize;
    let pot = l.companion().pending_pot;
    let program_before = account(&l).lamports;
    let tx = l.claim(0, &program);
    tx.expect_fail();
    assert!(
        format!("{:?}", tx.err()).contains("ExternalAccountLamportSpend"),
        "{:?}",
        tx.err()
    );
    assert_eq!(
        account(&l).lamports,
        program_before,
        "nothing reaches the program"
    );
    assert_eq!(l.companion().pending_pot, pot, "the pot keeps the prize");
    assert_eq!(l.game().status, DrawStatus::Revealed, "the draw goes on");
    l.warp_to_attempt(1);
    let tx = l.claim(1, &a.pubkey());
    tx.ok();
    let ev: PrizePaid = tx.event();
    assert_eq!((ev.winner, ev.attempt), (a.pubkey(), 1));
    assert_eq!(ev.prize + ev.bounty, prize);
}

/// (Integration lens, round 3.) The companion's game launch with the lottery hook: at most 64
/// accounts locked, height 5, and what runs at height 5 (the system program, the token program's
/// event CPIs, the lottery hook's own `before_transfer`): a game hook's callbacks can make no CPI.
#[test]
fn the_game_launch_locks_few_accounts_and_its_hook_runs_at_height_5() {
    let l = Lotto::new();
    let tx = &l.launch_tx;
    let meta = tx.ok();
    let keys = &tx.keys;
    let mut unique = keys.clone();
    unique.sort();
    unique.dedup();
    let mut at5 = Vec::new();
    for inner in meta.inner_instructions.iter().flatten() {
        if inner.stack_height == 5 {
            at5.push(keys[usize::from(inner.instruction.program_id_index)]);
        }
    }
    at5.dedup();
    println!(
        "game launch: {} accounts, {} bytes, height {}, trace {}, CU {}; at height 5: {:?}",
        unique.len(),
        tx.size,
        tx.max_height(),
        tx.trace_len(),
        tx.cu(),
        at5
    );
    assert!(unique.len() <= 64);
    assert!(tx.max_height() <= 5);
    assert!(at5.contains(&HOOK));
}

// ---- The custody lens's round-3 checks, kept --------------------------------------------------------

/// The custody lens's round-3 file (`audit_games_custody_r3.rs`), with its own coin and ledger: its
/// controls, which pin on the built programs what its line-by-line review relied on, and its
/// finding (16), now a regression test.
///
/// 1. The pot's ledger balances through every path that moves pot money: fee claims with the pot
///    share above an unaudited cap sent to the buyback, the oracle request (top-up and bounty), a
///    claimed prize (prize and bounty), a cap lowered mid-draw (the prize cut to what the pot
///    holds), a dormant draw, a retirement, a block. Every lamport the pot received is accounted
///    for by an event, and after every transaction the creator's bridged SOL covers every bucket.
/// 2. A winner whose wallet does not exist yet (tokens received, never any SOL) is paid even the
///    smallest prize the bounds allow, so no prize is lost to the rent-exempt minimum.
/// 3. The finding's coin, its hook answering nothing, buys back and burns.
mod custody_r3 {
    use anchor_lang::prelude::Pubkey;
    use anchor_lang::solana_program::instruction::Instruction;
    use bordrless_companion::client::{self as companion, SeedSlot};
    use bordrless_companion::constants::*;
    use bordrless_companion::error::CompanionError;
    use bordrless_companion::events::*;
    use bordrless_companion::instructions::{CreateArgs, CreateGameArgs, HookStatusArgs};
    use bordrless_companion::oracle as seeds;
    use bordrless_companion::state::{Companion, DrawStatus, Game, GameKind, Split};
    use bordrless_core::policy;
    use bordrless_game::{draw_index, round_of, GameHeader, Range, Slots};
    use bordrless_hook::{AccountSource, ExtraAccount, Seed};
    use bordrless_launch::client::{self as launch, CustomHookAccounts, LaunchKeys};
    use bordrless_launch::instructions::CreateConfigArgs;
    use bordrless_launch::state::LaunchRules;
    use bordrless_program_tests::env::{Env, Tx};
    use bordrless_program_tests::fixture::World;
    use bordrless_program_tests::launch::*;
    use bordrless_program_tests::orao;
    use bordrless_token::client::{self as token, Hook};
    use lottery_hook::client as lottery;
    use solana_account::Account;
    use solana_keypair::Keypair;
    use solana_signer::Signer;

    const ROUND: u32 = 3_600;
    const CREATOR_FEE: u16 = 200;
    const BOUNTY_BPS: u16 = 100;
    const SPLIT: Split = Split {
        buyback_bps: 2_000,
        holders_bps: 0,
        beneficiary_bps: 1_000,
    };
    const POT_BPS: u16 = 7_000;
    const WINDOW: u32 = 300;
    const ATTEMPTS: u8 = 6;
    const HOOK: Pubkey = lottery_hook::ID;

    #[track_caller]
    fn refused(tx: &Tx, e: CompanionError) {
        tx.expect_code(u32::from(e));
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
            min_pot: MIN_MIN_POT,
            prize_bps: 5_000,
            claim_window_secs: WINDOW,
            max_attempts: ATTEMPTS,
        }
    }

    /// The slot hashes sysvar's first entry, changed every slot.
    fn set_slot_hash(env: &mut Env) -> (u64, [u8; 32]) {
        let slot = env.slot - 1;
        let mut from = [0u8; 32];
        from[..8].copy_from_slice(&slot.to_le_bytes());
        let hash: [u8; 32] = orao::randomness_for(&from)[..32].try_into().unwrap();
        let mut sysvar = env.account(&seeds::SLOT_HASHES).expect("slot hashes");
        sysvar.data[..8].copy_from_slice(&1u64.to_le_bytes());
        sysvar.data[8..16].copy_from_slice(&slot.to_le_bytes());
        sysvar.data[16..48].copy_from_slice(&hash);
        env.put(seeds::SLOT_HASHES, sysvar);
        (slot, hash)
    }

    /// A repeatable randomness whose attempt 0 lands where `want` says.
    fn randomness_where(total: u64, want: impl Fn(u64) -> bool) -> [u8; 64] {
        for i in 0u64..2_000_000 {
            let mut seed = [3u8; 32];
            seed[..8].copy_from_slice(&i.to_le_bytes());
            let r = orao::randomness_for(&seed);
            if want(draw_index(&r, 0, total).unwrap()) {
                return r;
            }
        }
        panic!("no randomness found");
    }

    /// What moved the pot, summed from the events of every transaction sent.
    #[derive(Default, Debug)]
    struct Ledger {
        funded: u64,
        over_cap: u64,
        oracle: u64,
        request_bounties: u64,
        prizes: u64,
        prize_bounties: u64,
        to_buyback: u64,
        retired: u64,
    }

    /// A lottery coin launched through its companion, past the sniper window; every transaction it
    /// sends is recorded in the ledger and checked for solvency.
    struct Lotto {
        w: World,
        launcher: Keypair,
        mint: Pubkey,
        hook: Pubkey,
        custom: CustomHookAccounts,
        cranker: Keypair,
        ledger: Ledger,
    }

    impl Lotto {
        /// A lottery coin on `lottery_hook`.
        fn with(g: impl FnOnce(&mut CreateGameArgs)) -> Self {
            let mut ga = game_args();
            g(&mut ga);
            let round_secs = ga.round_secs;
            Self::launch(HOOK, ga, move |_, launcher, mint| {
                vec![lottery::prepare(launcher, mint, round_secs)]
            })
        }

        /// A game coin on `hook`, which `prepare` readies for the mint (its instructions run in the
        /// setup transaction, between the companion's `create` and `create_game`, the mint signing).
        fn launch(
            hook: Pubkey,
            ga: CreateGameArgs,
            prepare: impl FnOnce(&mut World, Pubkey, Pubkey) -> Vec<Instruction>,
        ) -> Self {
            let mut w = World::new();
            orao::load(&mut w.env);
            let launcher = w.wallet_with_sol(50 * SOL);
            let mint_kp = Keypair::new();
            let mint = mint_kp.pubkey();
            let mut setup = vec![companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                mint,
                create_args(),
            )];
            setup.extend(prepare(&mut w, launcher.pubkey(), mint));
            setup.push(companion::create_game(
                launcher.pubkey(),
                mint,
                CreateGameArgs { hook, ..ga },
            ));
            w.env.send_paid_by(&setup, &launcher, &[&mint_kp]).ok();
            let (config, tx) = w.create_config(
                &launcher,
                CreateConfigArgs {
                    rules: LaunchRules::NONE,
                    creator_fee_bps: CREATOR_FEE,
                    custom_hook: Some(hook),
                    custom_hook_flags: lottery_hook::FLAGS,
                    label: "Lottery".to_string(),
                },
            );
            tx.ok();
            let c = w.launch_config(&config);
            let custom = w.custom_hook_accounts(&hook, &mint);
            let args = World::launch_args("LOTTO", c.creator_fee_bps, VQ, c.rules);
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
            let ix = companion::launch(launcher.pubkey(), mint, &inner, args);
            w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]).ok();
            w.env.warp(31);
            let cranker = w.wallet_with_sol(10 * SOL);
            Self {
                w,
                launcher,
                mint,
                hook,
                custom,
                cranker,
                ledger: Ledger::default(),
            }
        }

        fn round(&self) -> u32 {
            round_of(self.w.env.now, ROUND)
        }

        fn warp_into(&mut self, round: u32, secs: i64) {
            let t = i64::from(round) * i64::from(ROUND) + secs;
            assert!(t >= self.w.env.now);
            self.w.env.warp(t - self.w.env.now);
        }

        fn game(&self) -> Game {
            self.w.env.read(&companion::game_address(&self.mint))
        }

        fn companion(&self) -> Companion {
            self.w.env.read(&companion::companion_address(&self.mint))
        }

        fn keys(&self) -> LaunchKeys {
            LaunchKeys::of(&self.w.launch(&self.mint))
        }

        fn balance(&self, owner: &Pubkey) -> u64 {
            self.w.env.holding(&self.mint, owner)
        }

        fn range(&self, owner: &Pubkey, round: u32) -> Range {
            Slots::decode(&self.w.env.hook_data(&self.mint, owner))
                .range_in(round)
                .expect("a range that round")
        }

        /// The creator's bridged SOL covers every bucket.
        #[track_caller]
        fn solvent(&self) {
            let c = self.companion();
            let held = self
                .w
                .env
                .holding(&self.w.sol, &companion::creator_address(&self.mint));
            assert!(
                held >= c.set_aside().unwrap(),
                "insolvent: holds {held}, set aside {:?}",
                c.set_aside()
            );
        }

        fn record(&mut self, tx: &Tx) {
            if tx.result.is_err() {
                return;
            }
            let l = &mut self.ledger;
            for e in tx.events::<PotFunded>() {
                l.funded += e.to_pot;
                l.over_cap += e.to_buyback;
            }
            for e in tx.events::<DrawRequested>() {
                l.oracle += e.top_up;
                l.request_bounties += e.bounty;
            }
            for e in tx.events::<PrizePaid>() {
                l.prizes += e.prize;
                l.prize_bounties += e.bounty;
            }
            for e in tx.events::<PotToBuyback>() {
                l.to_buyback += e.lamports;
            }
            for e in tx.events::<PotRetired>() {
                l.retired += e.lamports;
            }
            self.solvent();
        }

        fn send_all(&mut self, ixs: &[Instruction]) -> Tx {
            let cranker = self.cranker.insecure_clone();
            let tx = self.w.env.send_paid_by(ixs, &cranker, &[]);
            self.record(&tx);
            tx
        }

        fn send(&mut self, ix: Instruction) -> Tx {
            self.send_all(&[ix])
        }

        fn buyer(&mut self, sol: u64) -> Keypair {
            let t = self.w.wallet_with_sol(sol + SOL);
            self.w.buy(&t, &self.mint, sol).ok();
            t
        }

        fn volume(&mut self, wallets: usize, sol: u64) {
            for _ in 0..wallets {
                let t = self.w.wallet_with_sol(sol + SOL);
                self.w.buy(&t, &self.mint, sol).ok();
                let held = self.balance(&t.pubkey());
                self.w.sell(&t, &self.mint, held).ok();
            }
        }

        fn enter(&mut self, owners: &[Pubkey]) {
            for o in owners {
                let ix = lottery::enter(self.mint, *o);
                self.send(ix).ok();
            }
        }

        fn claim_fees(&mut self) -> Tx {
            let ix = companion::claim_fees_game(self.cranker.pubkey(), self.mint, self.hook);
            self.send(ix)
        }

        /// `draw` of `round`, which commits its seed and requests it.
        fn draw(&mut self, round: u32) -> Tx {
            let (slot, hash) = set_slot_hash(&mut self.w.env);
            let at = SeedSlot { slot, hash };
            let paid = self.game().paid_seed;
            let c = self.cranker.pubkey();
            self.send_all(&[companion::draw_after(
                c,
                self.mint,
                self.hook,
                round,
                at,
                orao::TREASURY,
                paid,
            )])
        }

        fn fulfil(&mut self, randomness: &[u8; 64]) {
            let seed = self.game().seed;
            orao::fulfil(&mut self.w.env, &seed, randomness);
        }

        fn reveal(&mut self) -> Tx {
            let request = self.game().request;
            let ix = companion::reveal(self.cranker.pubkey(), self.mint, self.hook, request);
            self.send(ix)
        }

        fn claim(&mut self, winner: &Pubkey) -> Tx {
            let ix =
                companion::claim_prize(self.cranker.pubkey(), self.mint, self.hook, 0, *winner);
            self.send(ix)
        }

        fn retire(&mut self) -> Tx {
            let ix = companion::retire(self.cranker.pubkey(), self.mint, self.hook);
            self.send(ix)
        }

        fn burn_stranded(&mut self) -> Tx {
            let ix = companion::burn_stranded(self.cranker.pubkey(), self.mint, self.hook);
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

        /// A full draw of the round that just ended, attempt 0 landing in `winner`'s range, claimed.
        fn draw_and_pay(&mut self, winner: &Pubkey) -> PrizePaid {
            let round = self.round() - 1;
            self.draw(round).ok();
            let range = self.range(winner, round);
            let total = self.game().total;
            self.fulfil(&randomness_where(total, |x| range.contains(x)));
            self.reveal().ok();
            let tx = self.claim(winner);
            tx.ok();
            tx.event()
        }

        /// `times` buybacks, a minute apart; answers how many bought (and burned).
        fn buybacks(&mut self, times: usize) -> usize {
            let mut landed = 0;
            for _ in 0..times {
                self.w.env.warp(61);
                let ix = companion::buyback_with(
                    self.cranker.pubkey(),
                    &self.keys(),
                    false,
                    Some(&self.custom.clone()),
                );
                let tx = self.send(ix);
                if tx.result.is_ok() && !tx.events::<BoughtBack>().is_empty() {
                    landed += 1;
                }
            }
            landed
        }
    }

    /// CONTROL 1: the pot's ledger balances through every path that moves pot money, and the creator's
    /// bridged SOL covers every bucket after every transaction (`Lotto::record`).
    ///
    /// Paths: the pot share above the unaudited cap (10 SOL) to the buyback; requests (top-up and
    /// bounty); prizes (prize and bounty); a cap lowered between a request and its claim (the prize is
    /// cut to the trimmed pot, the rest to the buyback); a dormant draw from the 0.1 SOL floor; a
    /// retirement after two dormant periods; a block. Inflows (`PotFunded.to_pot`) equal outflows plus
    /// what the pot still holds, to the lamport.
    #[test]
    fn control_the_pot_ledger_balances_through_every_custody_path() {
        let mut l = Lotto::with(|g| g.min_pot = 2 * SOL);
        let a = l.buyer(5 * SOL);
        let b = l.buyer(3 * SOL);
        let (pa, pb) = (a.pubkey(), b.pubkey());

        // Round 1: A and B entered; enough volume to fill the pot past its 10 SOL cap.
        let r1 = l.round() + 1;
        l.warp_into(r1, 5);
        l.enter(&[pa, pb]);
        for _ in 0..24 {
            l.volume(1, 20 * SOL);
            l.claim_fees().ok();
        }
        let c = l.companion();
        assert_eq!(c.pending_pot, DEFAULT_POT_CAP, "held at the unaudited cap");
        assert!(
            l.ledger.over_cap > 0,
            "the share above it went to the buyback"
        );

        // Its draw pays A half the pot (prize_bps 50%).
        l.warp_into(r1 + 1, 5);
        l.enter(&[pa, pb]);
        let paid = l.draw_and_pay(&pa);
        assert_eq!(paid.winner, pa);

        // Round r1 + 1: the draw is requested (prize fixed at half the pot), then the protocol lowers
        // the cap to 0.1 SOL before anyone claims. The claim trims the pot first, and pays what is left.
        l.volume(2, 20 * SOL);
        l.claim_fees().ok();
        l.warp_into(r1 + 2, 5);
        l.enter(&[pa, pb]);
        l.draw(r1 + 1).ok();
        let prize = l.game().prize;
        assert!(prize > MIN_POT_CAP);
        l.set_status(false, MIN_POT_CAP, false).ok();
        let range = l.range(&pb, r1 + 1);
        let total = l.game().total;
        l.fulfil(&randomness_where(total, |x| range.contains(x)));
        // The next game step (here the reveal) trims the pot to the new cap first.
        let tx = l.reveal();
        tx.ok();
        let trimmed: PotToBuyback = tx.event();
        assert!(!trimmed.blocked);
        assert_eq!(trimmed.pending_pot, MIN_POT_CAP);
        let tx = l.claim(&pb);
        tx.ok();
        let paid: PrizePaid = tx.event();
        assert_eq!(
            paid.prize + paid.bounty,
            MIN_POT_CAP,
            "cut to the trimmed pot"
        );
        assert_eq!(l.companion().pending_pot, 0);

        // Back to the full cap; the game then earns nothing for 30 days: dormant, it is drawn from the
        // 0.1 SOL floor (its minimum is 2 SOL).
        l.set_status(false, DEFAULT_POT_CAP, false).ok();
        l.volume(1, 5 * SOL);
        l.claim_fees().ok();
        let pot = l.companion().pending_pot;
        assert!(
            (MIN_MIN_POT..2 * SOL).contains(&pot),
            "below the minimum: {pot}"
        );
        let settled = l.game().settled_at;
        let dormant = l.game().dormant_secs();
        let round = round_of(settled + dormant, ROUND) + 1;
        l.warp_into(round, 5);
        l.enter(&[pa, pb]);
        l.warp_into(round + 1, 5);
        let paid = l.draw_and_pay(&pa);
        assert_eq!(paid.winner, pa);

        // Then two dormant periods with no prize: anyone retires the rest to the buyback.
        l.volume(1, 5 * SOL);
        l.claim_fees().ok();
        refused(&l.retire(), CompanionError::NotDue);
        let at = l.game().settled_at + 2 * l.game().dormant_secs();
        l.w.env.warp(at - l.w.env.now);
        let pot = l.companion().pending_pot;
        let tx = l.retire();
        tx.ok();
        assert_eq!(tx.event::<PotRetired>().lamports, pot);

        // A block: the next fee claim's pot share goes to the buyback with what the pot held.
        l.volume(1, 5 * SOL);
        l.claim_fees().ok();
        l.set_status(false, DEFAULT_POT_CAP, true).ok();
        l.volume(1, 5 * SOL);
        let tx = l.claim_fees();
        tx.ok();
        assert!(tx.event::<PotToBuyback>().blocked);
        assert_eq!(l.companion().pending_pot, 0);

        // The buyback spends what reached it (and stays solvent doing so).
        l.buybacks(5);

        // Every lamport the pot received is accounted for.
        let led = &l.ledger;
        let c = l.companion();
        println!("{led:?} pending_pot {}", c.pending_pot);
        assert_eq!(
            led.funded,
            led.oracle
                + led.request_bounties
                + led.prizes
                + led.prize_bounties
                + led.to_buyback
                + led.retired
                + c.pending_pot,
            "the pot's ledger balances"
        );
        let g = l.game();
        assert_eq!(g.prizes_paid, 3);
        assert_eq!(g.prizes_total, led.prizes);
        assert_eq!(g.oracle_total, led.oracle);
        assert_eq!(g.status, DrawStatus::Idle);
    }

    /// CONTROL 2: a winner whose wallet holds no SOL (its tokens were sent to it; its system account
    /// does not exist) is paid even the smallest prize the bounds allow: a 0.1 SOL pot (the lowest
    /// cap and minimum), 10% of it after the request. Its wallet is created by the prize.
    #[test]
    fn control_a_winner_without_a_wallet_account_is_paid_the_smallest_prize() {
        let mut l = Lotto::with(|g| {
            g.min_pot = MIN_MIN_POT;
            g.prize_bps = MIN_PRIZE_BPS;
        });
        l.set_status(false, MIN_POT_CAP, false).ok();
        let a = l.buyer(5 * SOL);
        // A sends every token to Y, a fresh key nobody ever sent SOL.
        let y = Keypair::new().pubkey();
        assert!(l.w.env.account(&y).is_none());
        let held = l.balance(&a.pubkey());
        let mint = l.mint;
        let ixs = [
            token::create_holding(a.pubkey(), mint, y),
            token::transfer_with(
                a.pubkey(),
                token::holding_address(&mint, &a.pubkey()),
                token::holding_address(&mint, &y),
                mint,
                Some(Hook::of(HOOK)),
                lottery::extras(&mint),
                held,
            ),
        ];
        l.w.env.send_paid_by(&ixs, &a, &[]).ok();
        assert_eq!(l.balance(&y), held);
        assert!(l.w.env.account(&y).is_none(), "Y has no wallet account");

        let r = l.round() + 1;
        l.warp_into(r, 5);
        l.enter(&[y]);
        l.volume(1, 10 * SOL);
        l.claim_fees().ok();
        assert_eq!(l.companion().pending_pot, MIN_POT_CAP);
        l.warp_into(r + 1, 5);
        let paid = l.draw_and_pay(&y);
        assert_eq!(paid.winner, y);
        let rent = l.w.env.rent(0);
        assert!(paid.prize > rent, "{} vs rent {rent}", paid.prize);
        assert_eq!(l.w.env.lamports(&y), paid.prize);
    }

    // ---- Finding 1 (low): a game hook can strand the buyback, and a blocked pot with it ---------------

    /// A game coin whose hook is `hook_tester`, playing a game hook its launcher wrote and deployed
    /// immutable (the launchpad accepts an immutable custom hook), vetted by the protocol: a game
    /// header for the mint in its state, its registry listing its script and that state, the
    /// lottery's exact flags.
    fn tester_game() -> Lotto {
        let hook = hook_tester::ID;
        Lotto::launch(hook, game_args(), |w, launcher, mint| {
            // Vetted by the protocol: `create_game` takes no other hook but the lottery's.
            super::vet(w, hook);
            let (state, _) = bordrless_game::state_address(&hook, &mint);
            let mut data = vec![0u8; 8];
            data.extend_from_slice(&GameHeader::new(mint, ROUND, w.env.now).encode());
            let lamports = w.env.rent(data.len());
            w.env.put(
                state,
                Account {
                    lamports,
                    data,
                    owner: hook,
                    executable: false,
                    rent_epoch: 0,
                },
            );
            let extras = vec![ExtraAccount {
                writable: true,
                source: AccountSource::Pda {
                    program: hook,
                    seeds: vec![Seed::Literal(b"state".to_vec()), Seed::Account(1)],
                },
            }];
            vec![hook_tester::client::init_script(launcher, mint, extras)]
        })
    }

    /// Control: on that coin, with its hook answering nothing, the companion's buyback lands (bought
    /// and burned): the hook's refusal is the only cause of the finding below.
    #[test]
    fn control_a_test_hook_game_buys_back() {
        let mut l = tester_game();
        l.volume(3, 10 * SOL);
        l.claim_fees().ok();
        assert!(l.companion().pending_buyback > 0);
        assert!(l.buybacks(3) > 0);
        assert!(l.companion().burned_total > 0);
    }

    /// FINDING 16 (custody r3 finding 1). A vetted game hook that later refuses every transfer into
    /// the companion's creator address, and nothing else (`hook_tester`'s honeypot mode; a real hook
    /// needs one comparison in `before_transfer`). Holders trade as usual, but every buyback fails
    /// inside its swap. Before the fix, the buyback share of every fee claim, and the pot a block
    /// (decision c) or `retire` sent there, were stranded for ever (1.40 SOL here): only a buyback
    /// ever spent `pending_buyback`. Now, once nothing has bought (or waited) for 30 days since the
    /// block, anyone burns it as SOL (`burn_stranded`): the exit moves no game token, so a hook
    /// that refuses the swap (not only the burn) can't hold it either, and it pays nobody.
    #[test]
    fn a_blocked_hook_that_refuses_the_companions_buy_has_its_buyback_burned_as_sol() {
        let mut l = tester_game();
        let (mint, creator) = (l.mint, companion::creator_address(&l.mint));
        // The hook refuses any transfer whose destination owner is the creator address.
        let launcher = l.launcher.insecure_clone();
        let ix =
            hook_tester::client::honeypot(launcher.pubkey(), mint, creator, Pubkey::new_unique());
        l.w.env.send_paid_by(&[ix], &launcher, &[]).ok();
        // Holders trade as usual (buys and sells), and fees accrue: the pot and the buyback.
        l.volume(3, 10 * SOL);
        l.claim_fees().ok();
        let c = l.companion();
        assert!(c.pending_pot > 0 && c.pending_buyback > 0);
        // Every buyback fails inside its swap: the hook refused the transfer to the creator address.
        l.w.env.warp(61);
        let ix = companion::buyback_with(
            l.cranker.pubkey(),
            &l.keys(),
            false,
            Some(&l.custom.clone()),
        );
        let tx = l.send(ix);
        tx.expect_fail();
        assert!(
            tx.logs()
                .iter()
                .any(|m| m.contains("the scripted callback refused")),
            "{}",
            tx.logs().join("\n")
        );
        // The protocol blocks the hook, as decision (c) prescribes: the pot goes to the buyback.
        l.set_status(false, DEFAULT_POT_CAP, true).ok();
        let blocked_at = l.w.env.now;
        l.volume(1, 10 * SOL);
        let tx = l.claim_fees();
        tx.ok();
        assert!(tx.event::<PotToBuyback>().blocked);
        let c = l.companion();
        assert_eq!(c.pending_pot, 0);
        let stranded = c.pending_buyback;
        // Keepers crank buybacks: none lands.
        assert_eq!(l.buybacks(20), 0);
        // 30 days after the block, with no buy landed since, anyone burns it as SOL.
        refused(&l.burn_stranded(), CompanionError::NotDue);
        let due = blocked_at + STRANDED_SECS;
        l.w.env.warp(due - 1 - l.w.env.now);
        refused(&l.burn_stranded(), CompanionError::NotDue);
        l.w.env.warp(1);
        let (burned0, cranker0) = (
            l.w.env.lamports(&INCINERATOR),
            l.w.env.lamports(&l.cranker.pubkey()),
        );
        let tx = l.burn_stranded();
        tx.ok();
        let ev: StrandedBurned = tx.event();
        assert_eq!((ev.lamports, ev.since), (stranded, blocked_at));
        assert_eq!(l.w.env.lamports(&INCINERATOR) - burned0, stranded);
        assert!(
            l.w.env.lamports(&l.cranker.pubkey()) < cranker0,
            "paid nothing"
        );
        let c = l.companion();
        assert_eq!((c.pending_buyback, c.pending_pot), (0, 0));
    }
}

// ==== The final audit ================================================================================

/// The final audit's findings (the scratch files `audit_final_custody.rs`, `audit_final_game.rs`
/// and `audit_final_integration.rs`, replaced by this module), each proof of concept turned into a
/// regression test that passes on the fixed code, with the controls it relied on.
///
/// - 18 (low, custody): `burn_stranded` read only `last_buyback_at`, which a buyback that waits (the
///   price more than 3% above its reference, which catches up 5% an interval) never moves. A working
///   hook's buyback, cranked daily while the reference caught up with a risen price, was burned as
///   SOL at 30 days (6.39 SOL in the PoC); and since a burn restarted no clock, every later buyback
///   share could be claimed and burned in one transaction. Fixed: a wait that moves the reference
///   restarts the wait as a landed buyback does, and so does each burn
///   (`Companion.stranded_burned_at`).
/// - 19 (medium, game): with the pot unable to pay for ORAO's request (its fee above the cap, its
///   network state unreadable, or the breaker holding), a committed draw stayed open, and any
///   request for its public seed was adopted for free. A holder who previewed ORAO's answer on
///   devnet paid for the seed only when it won: every prize, whatever its tickets. Fixed: `draw`
///   commits a seed only when the pot can pay for its request, else rolls the round over
///   (`OracleUnpaid`); since 21 it makes the request in the same instruction.
/// - 21 and 22, the residuals the verifier found (see the file's header), at the end; and 23,
///   found verifying them.
/// - 20 (info, docs): a game coin's `dev_buy` and `buyback` take more than the default 200k CU;
///   the docs said every step but the launch fits it. Fixed in the docs; pinned here.
///
/// (The round's integration finding, the monorepo keeper aborting its pass on the first game
/// companion, is fixed and tested in the monorepo: `apps/server/src/keeper/companions.ts`.)
mod final_audit {
    use super::*;
    use bordrless_companion::state::spot_price;

    const DAY: i64 = 86_400;

    // ---- Finding 18: a working blocked hook's buyback is never burned while it waits ----------------

    /// Whether a `buyback` landed (bought and burned) rather than failed, was not due, or waited.
    fn bought(tx: &Tx) -> bool {
        tx.result.is_ok() && !tx.events::<BoughtBack>().is_empty()
    }

    /// A lottery coin (Bordrless's own `lottery_hook`, which never refuses a transfer or a burn),
    /// its buybacks at least `buyback_interval` apart, launched through its companion, past the
    /// sniper window.
    struct Coin {
        w: World,
        mint: Pubkey,
        cranker: Keypair,
        custom: CustomHookAccounts,
    }

    impl Coin {
        fn new(buyback_interval: i64) -> Self {
            let mut w = World::new();
            orao::load(&mut w.env);
            let launcher = w.wallet_with_sol(50 * SOL);
            let l = launcher.pubkey();
            let mint_kp = Keypair::new();
            let mint = mint_kp.pubkey();
            let args = CreateArgs {
                buyback_interval,
                ..create_args()
            };
            let setup = [
                companion::create(l, l, mint, args),
                lottery::prepare(l, mint, ROUND),
                companion::create_game(l, mint, game_args()),
            ];
            w.env.send_paid_by(&setup, &launcher, &[&mint_kp]).ok();
            let config = lottery_config(&mut w, &launcher, LaunchRules::NONE, lottery_hook::FLAGS);
            let ix = companion_launch_ix(&w, &l, &mint, &config, HOOK);
            w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]).ok();
            let custom = w.custom_hook_accounts(&HOOK, &mint);
            w.env.warp(31);
            let cranker = w.wallet_with_sol(SOL);
            Self {
                w,
                mint,
                cranker,
                custom,
            }
        }

        fn companion(&self) -> Companion {
            self.w.env.read(&companion::companion_address(&self.mint))
        }

        fn send(&mut self, ixs: &[Instruction]) -> Tx {
            let cranker = self.cranker.insecure_clone();
            self.w.env.send_paid_by(ixs, &cranker, &[])
        }

        fn volume(&mut self, wallets: usize, sol: u64) {
            for _ in 0..wallets {
                let t = self.w.wallet_with_sol(sol + SOL);
                self.w.buy(&t, &self.mint, sol).ok();
                let held = self.w.env.holding(&self.mint, &t.pubkey());
                self.w.sell(&t, &self.mint, held).ok();
            }
        }

        fn claim_fees_ix(&self) -> Instruction {
            companion::claim_fees_game(self.cranker.pubkey(), self.mint, HOOK)
        }

        fn claim_fees(&mut self) -> Tx {
            let ix = self.claim_fees_ix();
            self.send(&[ix])
        }

        fn buyback(&mut self) -> Tx {
            let keys = LaunchKeys::of(&self.w.launch(&self.mint));
            let ix =
                companion::buyback_with(self.cranker.pubkey(), &keys, false, Some(&self.custom));
            self.send(&[ix])
        }

        fn burn_ix(&self) -> Instruction {
            companion::burn_stranded(self.cranker.pubkey(), self.mint, HOOK)
        }

        fn burn(&mut self) -> Tx {
            let ix = self.burn_ix();
            self.send(&[ix])
        }

        /// The pool's price and the buyback's ceiling (the reference plus `MAX_PREMIUM_BPS`): a
        /// buyback buys only while the price is at most the ceiling.
        fn price_and_ceiling(&self) -> (u128, u128) {
            let pool = self.w.launch_pool(&self.mint);
            let spot = spot_price(
                pool.quote_reserve,
                pool.virtual_quote,
                pool.base_reserve,
                pool.virtual_base,
            )
            .unwrap();
            let reference = self.companion().reference_price;
            (
                spot,
                reference * u128::from(BPS + MAX_PREMIUM_BPS) / u128::from(BPS),
            )
        }

        fn block(&mut self) {
            let deployer = self.w.env.deployer.insecure_clone();
            let ix = companion::set_hook_status(
                deployer.pubkey(),
                HOOK,
                HookStatusArgs {
                    audited: false,
                    pot_cap: DEFAULT_POT_CAP,
                    blocked: true,
                },
            );
            self.w.env.send_paid_by(&[ix], &deployer, &[]).ok();
        }

        /// To `t` (unix seconds), never back.
        fn warp_to(&mut self, t: i64) {
            assert!(t >= self.w.env.now);
            self.w.env.warp(t - self.w.env.now);
        }

        /// Fees, a buyback that lands (the hook works), then the protocol blocks the hook and the
        /// pot moves to the buyback at the next fee claim. Answers when the hook was blocked.
        fn landed_then_blocked(&mut self) -> i64 {
            self.volume(4, 40 * SOL);
            self.claim_fees().ok();
            self.w.env.warp(61);
            assert!(bought(&self.buyback()), "a buyback lands");
            self.w.env.warp(60);
            self.block();
            let blocked_at = self.w.env.now;
            self.volume(1, 10 * SOL);
            self.claim_fees().ok();
            let c = self.companion();
            assert_eq!(c.pending_pot, 0, "blocked: the pot went to the buyback");
            assert!(c.pending_buyback > SOL, "{}", c.pending_buyback);
            blocked_at
        }

        /// Fees, then the protocol blocks the hook with a pot; the keeper moves the pot at once
        /// (`retire`), and the working hook's buybacks (one an interval) spend everything in the
        /// buyback. Answers when the hook was blocked.
        fn blocked_and_spent(&mut self) -> i64 {
            self.volume(4, 40 * SOL);
            self.claim_fees().ok();
            let pot = self.companion().pending_pot;
            assert!(pot > SOL, "{pot}");
            self.block();
            let blocked_at = self.w.env.now;
            let retire = companion::retire(self.cranker.pubkey(), self.mint, HOOK);
            self.send(&[retire]).ok();
            assert_eq!(self.companion().pending_pot, 0, "the keeper moved the pot");
            let interval = self.companion().buyback_interval;
            let mut tries = 0;
            while self.companion().pending_buyback > 0 {
                tries += 1;
                assert!(tries <= 90, "the working hook's buybacks spend it");
                self.w.env.warp(interval);
                self.buyback().ok();
            }
            blocked_at
        }
    }

    /// The buyback a fee claim credited: its buyback share, its pot share (all of it under a
    /// block) and any pot it moved.
    fn credited(tx: &Tx) -> u64 {
        tx.event::<FeesClaimed>().to_buyback
            + tx.event::<PotFunded>().to_buyback
            + tx.events::<PotToBuyback>()
                .iter()
                .map(|e| e.lamports)
                .sum::<u64>()
    }

    /// FINDING 18, first form (custody PoC A). A lottery coin with weekly buybacks; its hook works
    /// and a buyback lands. The protocol blocks the hook, a buyer lifts the price about 30%, and a
    /// keeper cranks `buyback` every day: each one is not due, or waits while the reference climbs
    /// 5% a week. Before the fix, `burn_stranded` burned the pot and the buyback (6.39 SOL) at 30
    /// days. Now each wait that moves the reference restarts the 30 days, so it is refused every
    /// day, until the buyback lands (13 days later) and restarts them again.
    #[test]
    fn a_working_blocked_hooks_buyback_waiting_on_a_risen_price_is_never_burned() {
        let mut c = Coin::new(7 * DAY);
        let blocked_at = c.landed_then_blocked();
        let whale = c.w.wallet_with_sol(5 * SOL);
        c.w.buy(&whale, &c.mint, 4 * SOL).ok();
        let (spot, ceiling) = c.price_and_ceiling();
        assert!(spot > ceiling, "the price is above the buyback's ceiling");
        let stranded = c.companion().pending_buyback;
        let mut landed = None;
        let mut moves = 0;
        for day in 1..=60 {
            c.w.env.warp(DAY);
            let reference_at = c.companion().reference_at;
            let tx = c.buyback();
            if bought(&tx) {
                landed = Some(day);
                break;
            }
            moves += usize::from(c.companion().reference_at != reference_at);
            // Every day, the 30 days included: the buyback only waited, so it is not stranded.
            refused(&c.burn(), CompanionError::NotDue);
        }
        let day = landed.expect("the working hook's buyback lands");
        println!(
            "the buyback waited {moves} times on the reference, then landed {day} days after the \
             block; burn_stranded was refused every day ({stranded} lamports kept)"
        );
        assert!(i64::from(day) * DAY > STRANDED_SECS && moves >= 4);
        assert!(c.w.env.now - blocked_at <= STRANDED_SECS + 14 * DAY);
        refused(&c.burn(), CompanionError::NotDue);
        assert!(c.companion().spent_total > 0);
        assert_eq!(c.w.env.lamports(&INCINERATOR), 0, "nothing burned");
    }

    /// FINDING 18, second form (custody PoC A'). A burn restarted no clock: once one had run, every
    /// later buyback share could be claimed and burned in one transaction, while a buyback would
    /// buy. Here the keeper is away for 30 days after the block, so the first burn is due (a hook
    /// nobody cranks can't be told from one that refuses). Then the price is under the ceiling and
    /// the buyback is due: `claim_fees` with `burn_stranded` in one transaction is refused, and the
    /// keeper's buyback spends the share.
    #[test]
    fn a_burn_restarts_the_wait_so_a_share_claimed_with_it_is_never_burned() {
        let mut c = Coin::new(7 * DAY);
        let blocked_at = c.landed_then_blocked();
        c.warp_to(blocked_at + STRANDED_SECS - 1);
        refused(&c.burn(), CompanionError::NotDue);
        c.w.env.warp(1);
        let tx = c.burn();
        within_limits("burn_stranded", &tx);
        let ev: StrandedBurned = tx.event();
        assert_eq!(ev.since, blocked_at);
        let burned_at = c.w.env.now;
        assert_eq!(c.companion().stranded_burned_at, burned_at);
        // A day on: fees, the price under the ceiling, a buyback due.
        c.w.env.warp(DAY);
        c.volume(2, 10 * SOL);
        let (spot, ceiling) = c.price_and_ceiling();
        let comp = c.companion();
        assert!(spot <= ceiling, "a buyback would buy now");
        assert!(
            c.w.env.now >= comp.last_buyback_at + comp.buyback_interval,
            "and is due"
        );
        assert_eq!(comp.pending_buyback, 0);
        let ixs = [c.claim_fees_ix(), c.burn_ix()];
        refused(&c.send(&ixs), CompanionError::NotDue);
        c.claim_fees().ok();
        let share = c.companion().pending_buyback;
        assert!(share > 0);
        let tx = c.buyback();
        assert!(bought(&tx), "the keeper's buyback spends it");
        assert!(c.companion().pending_buyback < share);
        // The next burn waits 30 days from the landed buyback.
        c.warp_to(burned_at + STRANDED_SECS);
        refused(&c.burn(), CompanionError::NotDue);
    }

    /// Control (custody): without the rally the keeper's buybacks land after the block, and
    /// `burn_stranded` is not due 30 days after it.
    #[test]
    fn control_without_a_rally_landed_buybacks_keep_burn_stranded_away() {
        let mut c = Coin::new(7 * DAY);
        let blocked_at = c.landed_then_blocked();
        let mut landed = 0;
        for _ in 0..30 {
            c.w.env.warp(DAY);
            landed += usize::from(bought(&c.buyback()));
        }
        assert!(landed > 0);
        c.warp_to(blocked_at + STRANDED_SECS);
        refused(&c.burn(), CompanionError::NotDue);
    }

    // ---- Finding 19: no seed is left public while the pot can't pay for its request ---------------

    /// `draw` sent by anyone (it is permissionless), from the newest slot hash, with the pot's
    /// last paid request.
    fn draw_by(l: &mut Lotto, by: &Keypair, round: u32) -> Tx {
        let at = set_slot_hash(&mut l.w.env);
        draw_by_at(l, by, round, at)
    }

    /// `draw` sent by anyone, naming `at` for its seed.
    fn draw_by_at(l: &mut Lotto, by: &Keypair, round: u32, at: SeedSlot) -> Tx {
        let ix = draw_ix(l, by, round, at);
        l.w.env.send_paid_by(&[ix], by, &[])
    }

    fn draw_ix(l: &Lotto, by: &Keypair, round: u32, at: SeedSlot) -> Instruction {
        let paid = l.game().paid_seed;
        companion::draw_after(by.pubkey(), l.mint, HOOK, round, at, orao::TREASURY, paid)
    }

    /// What anyone learns from ORAO's devnet once the seed is known (the suite's stand-in for
    /// ORAO's deterministic answer): the first attempt whose ticket a live range holds, and whose.
    fn preview(l: &Lotto, owners: &[Pubkey]) -> Option<(u8, Pubkey)> {
        let g = l.game();
        let r = orao::randomness_for(&g.seed);
        (0..g.max_attempts).find_map(|k| {
            let x = draw_index(&r, u32::from(k), g.total)?;
            owners
                .iter()
                .find(|o| {
                    bordrless_game::wins(&l.w.env.hook_data(&l.mint, o), g.round, x, l.balance(o))
                })
                .map(|o| (k, *o))
        })
    }

    /// The attacker's own `request_v2` for `seed`, paying ORAO's `fee` (anyone may request any
    /// seed).
    fn request_seed(l: &mut Lotto, by: &Keypair, seed: [u8; 32], fee: u64) -> Tx {
        let terms = seeds::Terms {
            treasury: orao::TREASURY,
            fee,
        };
        l.w.env
            .send_paid_by(&[seeds::request_ix(by.pubkey(), &terms, seed)], by, &[])
    }

    /// A round the pot can't pay for: rolled over at once (`OracleUnpaid`), with no seed committed
    /// and nothing to request.
    #[track_caller]
    fn rolled_over_unpaid(l: &mut Lotto, tx: &Tx, round: u32) {
        tx.ok();
        let ev: RolledOver = tx.event();
        assert_eq!((ev.round, ev.reason), (round, RolloverReason::OracleUnpaid));
        assert!(tx.events::<DrawCommitted>().is_empty(), "no seed published");
        assert_eq!(l.game().status, DrawStatus::Idle);
        refused(&l.draw_round(round), CompanionError::RoundNotOver);
    }

    /// FINDING 19, first form (game PoC F1a). ORAO's fee goes above the companion's cap. Before the
    /// fix, anyone's `draw` still committed each round's seed and left it open to adoption: a
    /// holder with 19% of the tickets previewed each answer and paid ORAO only for the seeds it
    /// won, and was paid every prize while every round the other holder won rolled over. Now every
    /// draw, the attacker's own included, rolls its round over at once and publishes no seed; the
    /// pot only carries. (Since the final audit's residuals there is no time between a draw and its
    /// request for the fee to change in: `draw` makes both.) Once the fee is back, draws pay by the
    /// tickets.
    #[test]
    fn with_orao_s_fee_above_the_cap_no_draw_is_left_to_whoever_previews_and_pays() {
        let mut l = Lotto::new();
        let attacker = l.buyer(SOL);
        let victim = l.buyer(5 * SOL);
        let holders = [attacker.pubkey(), victim.pubkey()];
        l.volume(1, 20 * SOL);
        l.claim_fees().ok();
        let fee = seeds::MAX_REQUEST_FEE + 1_000_000;
        orao::set_fee(&mut l.w.env, fee);
        let mut ended = l.start_round(&[&attacker, &victim]);
        for _ in 0..15 {
            let now = l.start_round(&[&attacker, &victim]);
            let pot = l.companion().pending_pot;
            let tx = draw_by(&mut l, &attacker, ended);
            rolled_over_unpaid(&mut l, &tx, ended);
            assert_eq!(l.game().seed, [0; 32], "no seed was ever committed");
            assert_eq!(l.companion().pending_pot, pot, "the pot carries");
            l.volume(1, 20 * SOL);
            l.claim_fees().ok();
            ended = now;
        }
        assert_eq!(l.game().prizes_paid, 0);
        assert_eq!(l.game().rollovers, 15);

        // The fee back under the cap: the keeper's draw of the round that just ended is paid, and
        // its ticket's holder is paid.
        orao::set_fee(&mut l.w.env, orao::FEE);
        l.start_round(&[&attacker, &victim]);
        let tx = l.draw_round(ended);
        assert!(tx.event::<DrawRequested>().made);
        let seed = l.game().seed;
        orao::fulfil(&mut l.w.env, &seed, &orao::randomness_for(&seed));
        l.reveal().ok();
        if let Some((k, w)) = preview(&l, &holders) {
            l.warp_to_attempt(k);
            assert_eq!(l.claim(k, &w).event::<PrizePaid>().winner, w);
        }
    }

    /// FINDING 19, second form (game PoC F1b): the breaker. ORAO stops answering for 16 hourly
    /// rounds (the pot pays for requests 1, 2, 4, 8 rounds apart, 5 in all), then answers new
    /// requests again but never the pot's last one. Before the fix, each held round was committed
    /// and the keeper sent `draw` alone so a request could be adopted; the attacker paid only for
    /// the seed it had previewed winning, after the other holder's winning rounds had rolled over
    /// (9 of them, 9.95 SOL carried to it). Now every held round rolls over at its draw with no
    /// seed committed, so there is nothing to preview or adopt. When the backoff ends, the pot
    /// pays, ORAO answers, and the draw goes to whoever holds the ticket.
    #[test]
    fn while_the_breaker_holds_no_round_is_left_open_to_a_previewed_request() {
        let mut l = Lotto::new();
        let attacker = l.buyer(SOL);
        let victim = l.buyer(5 * SOL);
        let holders = [attacker.pubkey(), victim.pubkey()];
        l.volume(1, 20 * SOL);
        l.claim_fees().ok();
        let mut ended = l.start_round(&[&attacker, &victim]);
        // ORAO silent: every paid request stays pending for ever.
        let mut paid = 0;
        for _ in 0..16 {
            let now = l.start_round(&[&attacker, &victim]);
            if l.game().status == DrawStatus::Requested {
                l.expire().ok();
            }
            let tx = l.draw_round(ended);
            if l.game().status == DrawStatus::Requested {
                assert!(tx.event::<DrawRequested>().made);
                paid += 1;
            } else {
                rolled_over_unpaid(&mut l, &tx, ended);
            }
            l.volume(1, 20 * SOL);
            l.claim_fees().ok();
            ended = now;
        }
        assert_eq!(paid, 5, "requests 1, 2, 4, 8 rounds apart");
        // ORAO answers new requests again; the pot's last paid request stays unanswered. Every
        // round the breaker holds rolls over at its draw, whoever sends it.
        let mut held = 0;
        loop {
            let now = l.start_round(&[&attacker, &victim]);
            if l.game().status == DrawStatus::Requested {
                l.expire().ok();
            }
            let tx = draw_by(&mut l, &attacker, ended);
            if l.game().status == DrawStatus::Idle {
                rolled_over_unpaid(&mut l, &tx, ended);
                held += 1;
            } else {
                // The backoff is over: the pot pays, ORAO answers, the ticket's holder is paid.
                let ev: DrawRequested = tx.event();
                assert!(ev.made);
                assert_eq!(ev.paid_streak, 6);
                let seed = l.game().seed;
                orao::fulfil(&mut l.w.env, &seed, &orao::randomness_for(&seed));
                l.reveal().ok();
                assert!(!l.game().has_paid_request(), "the breaker reset");
                if let Some((k, w)) = preview(&l, &holders) {
                    l.warp_to_attempt(k);
                    assert_eq!(l.claim(k, &w).event::<PrizePaid>().winner, w);
                }
                break;
            }
            l.volume(1, 20 * SOL);
            l.claim_fees().ok();
            ended = now;
        }
        println!("breaker: {held} rounds held, none with a seed anyone could preview");
        assert_eq!(
            held, 15,
            "16 rounds after the last paid request, less the one it was for"
        );
    }

    // ---- The final audit's residuals (the verifier's notes) ------------------------------------------

    /// RESIDUAL 21 (game, low; the verifier's PoC): the committer chose how long a committed seed
    /// stayed open. With the pot just under its minimum (nothing for the keeper to draw), an
    /// attacker bundled a donation of bridged SOL to the creator address, `claim_fees` and `draw`
    /// 10 seconds before the draw's last request time, previewed ORAO's answer on devnet and paid
    /// for the request only if it won; otherwise `expire` rolled the round over (`Late`): a free
    /// re-roll each time the pot crossed its minimum. Now:
    ///
    /// - that late, the bundle's draw rolls the round over (`Late`) before any seed is committed:
    ///   a draw leaves `REVEAL_SECS` and a whole claim window before its claims end;
    /// - at the last moment a draw may be made, it commits the seed and makes the pot's request in
    ///   one instruction: when it lands ORAO holds the request, and whatever the attacker learns of
    ///   the answer, the round is drawn by it (its first attempt keeping a whole window);
    /// - the seeds of the newest slots are public before any draw, but a draw names one of the last
    ///   3 slots, and a devnet preview of ORAO's answer comes 7 slots or more after its request (8
    ///   devnet requests measured on 2026-10-09): a seed whose answer is already on
    ///   chain is refused (`StaleSeed`), and so is a slot more than 3 slots old, as a preview from
    ///   ORAO's devnet would need.
    #[test]
    fn a_draw_never_leaves_a_seed_to_preview_before_its_request() {
        let mut l = Lotto::new();
        let attacker = l.buyer(SOL);
        let victim = l.buyer(5 * SOL);
        let holders = [attacker.pubkey(), victim.pubkey()];
        let mint = l.mint;
        let creator = companion::creator_address(&mint);
        // A pot just under its minimum (the holders' buys' fees).
        l.claim_fees().ok();
        let pot = l.companion().pending_pot;
        assert!(pot > 0 && pot < MIN_POT, "{pot}");
        let r = l.start_round(&[&attacker, &victim]);
        // The verifier's bundle, 10 seconds before the old last request time.
        let claims_end = i64::from(r + 2) * R;
        l.warp_to(claims_end - i64::from(WINDOW) - 10);
        assert!(l.w.env.now > l.game().last_draw(r));
        let at = set_slot_hash(&mut l.w.env);
        let sol = l.w.sol;
        let ixs = [
            token::transfer(
                attacker.pubkey(),
                token::holding_address(&sol, &attacker.pubkey()),
                token::holding_address(&sol, &creator),
                sol,
                None,
                vec![],
                2 * MIN_POT,
            ),
            companion::claim_fees_game(attacker.pubkey(), mint, HOOK),
            draw_ix(&l, &attacker, r, at),
        ];
        let tx = l.w.env.send_paid_by(&ixs, &attacker, &[]);
        tx.ok();
        assert!(
            l.companion().pending_pot >= MIN_POT,
            "the bundle filled the pot"
        );
        let ev: RolledOver = tx.event();
        assert_eq!((ev.round, ev.reason), (r, RolloverReason::Late));
        assert!(
            tx.events::<DrawCommitted>().is_empty(),
            "no seed to preview"
        );
        let g = l.game();
        assert_eq!(
            (g.status, g.seed, g.next_round),
            (DrawStatus::Idle, [0; 32], r + 1)
        );

        // The next round, drawn by the attacker at the last moment a draw may be made: the seed is
        // committed with the pot's request, and the answer decides the round.
        let r1 = l.start_round(&[&attacker, &victim]);
        let last = l.game().last_draw(r1);
        assert_eq!(
            last,
            i64::from(r1 + 2) * R - i64::from(WINDOW) - REVEAL_SECS
        );
        l.warp_to(last);
        let at = set_slot_hash(&mut l.w.env);
        let tx = draw_by_at(&mut l, &attacker, r1, at);
        tx.ok();
        let dc: DrawCommitted = tx.event();
        let dr: DrawRequested = tx.event();
        assert!(dr.made);
        assert_eq!((dr.seed, dc.slot), (dc.seed, at.slot));
        assert_eq!(
            orao::pending_client(&l.w.env, &dc.seed),
            Some(companion::oracle_payer_address(&mint))
        );
        assert_eq!(l.game().status, DrawStatus::Requested);
        orao::fulfil(&mut l.w.env, &dc.seed, &orao::randomness_for(&dc.seed));
        l.reveal().ok();
        let g = l.game();
        assert_eq!(
            g.attempt_closes(0),
            Some(g.revealed_at + i64::from(WINDOW)),
            "the first attempt keeps a whole window"
        );
        if let Some((k, w)) = preview(&l, &holders) {
            if g.attempt_opens(k).unwrap() < g.claims_end() {
                l.warp_to_attempt(k);
                assert_eq!(l.claim(k, &w).event::<PrizePaid>().winner, w);
            }
        }

        // The seed of the newest slot, requested by the attacker and answered on chain before its
        // draw: refused. One whose answer the attacker waited for on ORAO's devnet: too old.
        let r2 = l.start_round(&[&attacker, &victim]);
        if l.game().status != DrawStatus::Idle {
            l.expire().ok();
        }
        l.volume(1, 20 * SOL);
        l.claim_fees().ok();
        l.warp_into(r2 + 1, 1);
        let at = set_slot_hash(&mut l.w.env);
        let seed = at.seed(&mint, r2);
        request_seed(&mut l, &attacker, seed, orao::FEE).ok();
        orao::fulfil(&mut l.w.env, &seed, &orao::randomness_for(&seed));
        refused(
            &draw_by_at(&mut l, &attacker, r2, at),
            CompanionError::StaleSeed,
        );
        l.w.env.warp(1);
        let at = set_slot_hash(&mut l.w.env);
        for _ in 0..seeds::SEED_SLOTS {
            l.w.env.warp(0);
        }
        refused(
            &draw_by_at(&mut l, &attacker, r2, at),
            CompanionError::StaleSeed,
        );
        assert_eq!(l.game().status, DrawStatus::Idle);
        // The keeper's draw from the newest slot is made, with the pot's own request.
        let tx = l.draw_round(r2);
        tx.ok();
        assert!(tx.event::<DrawRequested>().made);
        assert_ne!(l.game().seed, seed);
    }

    /// RESIDUAL 22 (custody, low; the verifier's note). A quiet lottery coin whose hook works is
    /// blocked while its game is idle: no step moves its pot (no fee claim, no game step), and
    /// landed buybacks spend its buyback share. Before the fix, `burn_stranded` 30 days after the
    /// last of them moved the pot to the buyback and burned it in the same instruction, though no
    /// buyback had ever been able to spend it. Now whichever call moves a blocked game's pot (a game
    /// step, a fee claim, or the burn itself) restarts the wait, so a step and a burn bundled in one
    /// transaction can't do it either, and the burn's own call only moves the pot; the working
    /// hook's buybacks spend it, and no burn is ever due. (The monorepo keeper now also moves such a
    /// pot itself, with a game step, as soon as it sees the block.)
    #[test]
    fn a_blocked_idle_pot_is_never_burned_in_the_call_that_moves_it() {
        let mut c = Coin::new(DAY);
        c.volume(4, 40 * SOL);
        c.claim_fees().ok();
        let pot = c.companion().pending_pot;
        assert!(pot > SOL, "{pot}");
        c.block();
        let blocked_at = c.w.env.now;
        // Landed buybacks spend the buyback share; nothing moves the pot.
        for _ in 0..60 {
            if c.companion().pending_buyback == 0 {
                break;
            }
            c.w.env.warp(DAY);
            c.buyback();
        }
        let k = c.companion();
        assert_eq!((k.pending_buyback, k.pending_pot), (0, pot));
        // 30 days after the buyback last bought or waited. A step that moves the pot (a game step,
        // a fee claim) sent with a burn: the move restarts the wait, so the burn, and the whole
        // transaction with it, is refused.
        c.warp_to(k.stranded_at(blocked_at));
        c.volume(1, SOL);
        for first in [
            companion::retire(c.cranker.pubkey(), c.mint, HOOK),
            c.claim_fees_ix(),
        ] {
            let burn = c.burn_ix();
            refused(&c.send(&[first, burn]), CompanionError::NotDue);
            assert_eq!(c.companion().pending_pot, pot, "nothing moved");
        }
        // The burn alone moves the pot and burns nothing.
        let burned0 = c.w.env.lamports(&INCINERATOR);
        let tx = c.burn();
        tx.ok();
        let moved: PotToBuyback = tx.event();
        assert_eq!((moved.lamports, moved.blocked), (pot, true));
        assert!(tx.events::<StrandedBurned>().is_empty());
        assert_eq!(c.w.env.lamports(&INCINERATOR), burned0, "nothing burned");
        let moved_at = c.w.env.now;
        let k = c.companion();
        assert_eq!(
            (k.pending_pot, k.pending_buyback, k.stranded_burned_at),
            (0, pot, moved_at)
        );
        refused(&c.burn(), CompanionError::NotDue);
        // The working hook's buybacks spend it, each restarting the wait: no burn is ever due.
        let mut days = 0;
        while c.companion().pending_buyback > 0 {
            days += 1;
            assert!(days <= 90, "the buyback spends the pot");
            c.w.env.warp(DAY);
            c.buyback();
            refused(&c.burn(), CompanionError::NotDue);
        }
        println!("the moved pot ({pot} lamports) was bought back over {days} days, none burned");
        assert_eq!(c.w.env.lamports(&INCINERATOR), burned0);
    }

    /// RESIDUAL 23 (custody, low; the verifier's `verify_residuals_custody.rs`, replaced by this
    /// test). A lottery coin with daily buybacks is blocked; the keeper moves its pot (`retire`)
    /// and the working hook's buybacks spend everything, the pot included. Then 31 quiet days:
    /// nothing to buy, so no buyback lands and the wait runs out. One trade's fees reach the
    /// keeper's claim minimum, and its pass sends `claim_fees`, then (reading the companion again)
    /// `burn_stranded`. Before the fix, the share the claim had just credited (516,003,893
    /// lamports) was burned, bundled with the claim or in the next transaction, before any
    /// buyback was tried. Now a claim that credits the buyback at least what it held (here, an
    /// empty one) restarts the wait: the bundle and the burn are refused, and the keeper's
    /// buyback spends the share; nothing is ever burned.
    #[test]
    fn a_share_credited_after_a_quiet_month_gets_its_whole_wait() {
        for bundled in [true, false] {
            let mut c = Coin::new(DAY);
            let blocked_at = c.blocked_and_spent();
            let k = c.companion();
            assert_eq!((k.pending_buyback, k.pending_pot), (0, 0));
            let stranded_at = k.stranded_at(blocked_at);
            // A quiet month: nothing to buy, so no buyback lands and the wait runs out.
            c.w.env.warp(31 * DAY);
            assert!(c.w.env.now >= stranded_at);
            // One trade's fees, then the keeper's pass.
            c.volume(1, 10 * SOL);
            let burned0 = c.w.env.lamports(&INCINERATOR);
            if bundled {
                let ixs = [c.claim_fees_ix(), c.burn_ix()];
                refused(&c.send(&ixs), CompanionError::NotDue);
                assert_eq!(c.companion().pending_buyback, 0, "nothing credited");
            }
            let tx = c.claim_fees();
            tx.ok();
            let share = credited(&tx);
            let claimed_at = c.w.env.now;
            let k = c.companion();
            assert_eq!(k.pending_buyback, share);
            assert!(share > 0);
            // The claim restarted the wait: the share has its whole 30 days.
            assert_eq!(k.stranded_burned_at, claimed_at);
            assert_eq!(k.stranded_at(blocked_at), claimed_at + STRANDED_SECS);
            refused(&c.burn(), CompanionError::NotDue);
            // The keeper's next transaction, and every day after, until a buyback has spent it.
            c.w.env.warp(1);
            refused(&c.burn(), CompanionError::NotDue);
            let mut days = 0;
            while c.companion().pending_buyback > 0 {
                days += 1;
                assert!(days <= 30, "the keeper's buybacks spend it");
                assert!(bought(&c.buyback()), "the working hook's buyback buys");
                refused(&c.burn(), CompanionError::NotDue);
                c.w.env.warp(DAY);
            }
            println!(
                "bundled={bundled}: a claim after a quiet month credited {share} lamports to the \
                 buyback; burn_stranded was refused and {days} buyback(s) spent it"
            );
            assert_eq!(c.w.env.lamports(&INCINERATOR), burned0, "nothing burned");
        }
    }

    /// A game coin whose hook (`hook_tester`, vetted by the protocol) refuses every burn, so no
    /// buyback of it ever lands: blocked once fees have filled its pot, the pot moved to the
    /// buyback by the next fee claim.
    struct Honeypot {
        w: World,
        mint: Pubkey,
        cranker: Keypair,
        blocked_at: i64,
    }

    impl Honeypot {
        const HOOK: Pubkey = hook_tester::ID;

        fn new() -> Self {
            let hook = Self::HOOK;
            let mut w = World::new();
            vet(&mut w, hook);
            let launcher = w.wallet_with_sol(50 * SOL);
            let l = launcher.pubkey();
            let (tx, mint_kp) = tester_setup(&mut w, &launcher, |mint| {
                vec![hook_tester::client::set_answer(
                    l,
                    mint,
                    hook_tester::callback::BEFORE_BURN,
                    hook_tester::mode::FAIL,
                    vec![],
                )]
            });
            tx.ok();
            let mint = mint_kp.pubkey();
            let (config, tx) = w.create_config(
                &launcher,
                CreateConfigArgs {
                    rules: LaunchRules::NONE,
                    creator_fee_bps: CREATOR_FEE,
                    custom_hook: Some(hook),
                    custom_hook_flags: lottery_hook::FLAGS,
                    label: "Game".to_string(),
                },
            );
            tx.ok();
            let ix = companion_launch_ix(&w, &l, &mint, &config, hook);
            w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]).ok();
            w.env.warp(31);
            let cranker = w.wallet_with_sol(SOL);
            let mut h = Self {
                w,
                mint,
                cranker,
                blocked_at: 0,
            };
            h.volume(4, 40 * SOL);
            h.claim().ok();
            assert!(h.companion().pending_pot > SOL);
            let deployer = h.w.env.deployer.insecure_clone();
            let ix = companion::set_hook_status(
                deployer.pubkey(),
                hook,
                HookStatusArgs {
                    audited: false,
                    pot_cap: DEFAULT_POT_CAP,
                    blocked: true,
                },
            );
            h.w.env.send_paid_by(&[ix], &deployer, &[]).ok();
            h.blocked_at = h.w.env.now;
            h.volume(1, 10 * SOL);
            let tx = h.claim();
            tx.ok();
            assert!(tx.event::<PotToBuyback>().blocked);
            assert_eq!(h.companion().pending_pot, 0);
            h
        }

        fn companion(&self) -> Companion {
            self.w.env.read(&companion::companion_address(&self.mint))
        }

        fn send(&mut self, ixs: &[Instruction]) -> Tx {
            let cranker = self.cranker.insecure_clone();
            self.w.env.send_paid_by(ixs, &cranker, &[])
        }

        /// `wallets` buys of `sol` each, every one sold back.
        fn volume(&mut self, wallets: usize, sol: u64) {
            for _ in 0..wallets {
                let t = self.w.wallet_with_sol(sol + SOL);
                self.w.buy(&t, &self.mint, sol).ok();
                let held = self.w.env.holding(&self.mint, &t.pubkey());
                self.w.sell(&t, &self.mint, held).ok();
            }
        }

        fn claim(&mut self) -> Tx {
            let ix = companion::claim_fees_game(self.cranker.pubkey(), self.mint, Self::HOOK);
            self.send(&[ix])
        }

        /// A trade of `sol` and a fee claim: answers what the claim credited to the buyback, what
        /// the buyback held before it, and whether the claim restarted the wait.
        fn grief(&mut self, sol: u64) -> (u64, u64, bool) {
            let before = self.companion();
            self.volume(1, sol);
            let tx = self.claim();
            tx.ok();
            let after = self.companion();
            (
                credited(&tx),
                before.pending_buyback,
                after.stranded_burned_at != before.stranded_burned_at,
            )
        }

        fn burn(&mut self) -> Tx {
            let ix = companion::burn_stranded(self.cranker.pubkey(), self.mint, Self::HOOK);
            self.send(&[ix])
        }

        fn warp_to(&mut self, t: i64) {
            assert!(t >= self.w.env.now);
            self.w.env.warp(t - self.w.env.now);
        }
    }

    /// RESIDUAL 23's bound: restarting the wait on a credit can't put a refusing hook's burn off
    /// for ever. Its buyback never empties, so a claim restarts the wait only when it credits at
    /// least what the buyback holds: small credits restart nothing, and each wait bought costs at
    /// least everything the buyback holds, every lamport of it burned with the rest, so the next
    /// costs twice as much.
    #[test]
    fn credits_smaller_than_the_buyback_never_put_off_a_refusing_hooks_burn() {
        let mut h = Honeypot::new();
        let blocked_at = h.blocked_at;
        let k = h.companion();
        let held = k.pending_buyback;
        assert!(held > SOL, "{held}");
        assert_eq!(
            k.stranded_burned_at, blocked_at,
            "the pot's move restarted the wait"
        );
        let due = k.stranded_at(blocked_at);
        assert_eq!(due, blocked_at + STRANDED_SECS);
        // A griefer trades a little and claims every three days of the month: each claim credits
        // less than the buyback holds and restarts nothing.
        let (mut griefed, mut claims) = (0, 0);
        for day in (3..30).step_by(3) {
            h.warp_to(blocked_at + day * DAY);
            let (credit, before, restarted) = h.grief(SOL);
            assert!(credit > 0 && credit < before && !restarted);
            griefed += credit;
            claims += 1;
        }
        assert_eq!(h.companion().stranded_at(blocked_at), due);
        // The burn comes when it was due, and burns the griefer's credits with the rest.
        h.warp_to(due - 1);
        refused(&h.burn(), CompanionError::NotDue);
        h.w.env.warp(1);
        let tx = h.burn();
        tx.ok();
        let ev: StrandedBurned = tx.event();
        assert_eq!((ev.lamports, ev.since), (held + griefed, blocked_at));
        // The burn emptied the buyback: the next claim restarts the wait, as a working hook's
        // would (it can't be told apart).
        h.w.env.warp(DAY);
        let (first, before, restarted) = h.grief(2 * SOL);
        assert!(before == 0 && first > 0 && restarted);
        let restarted_at = h.w.env.now;
        // A smaller credit restarts nothing: the burn stays due 30 days after the first.
        h.w.env.warp(DAY);
        let (second, before, restarted) = h.grief(SOL);
        assert!(second < before && !restarted);
        assert_eq!(h.companion().stranded_burned_at, restarted_at);
        // To put it off once more, a credit must be at least all the buyback holds: the griefer
        // pays at least that in fees, which joins the buyback, so it at least doubles.
        h.warp_to(restarted_at + STRANDED_SECS - DAY);
        let (third, before, restarted) = h.grief(8 * SOL);
        assert!(third >= before && before == first + second && restarted);
        let bought_at = h.w.env.now;
        let holds = h.companion().pending_buyback;
        assert!(holds >= 2 * before);
        // The next wait costs twice as much: the same trade no longer buys one.
        h.w.env.warp(DAY);
        let (fourth, before, restarted) = h.grief(8 * SOL);
        assert!(fourth < before && !restarted);
        // Then the burn takes everything, the griefer's fees with it.
        h.warp_to(bought_at + STRANDED_SECS - 1);
        refused(&h.burn(), CompanionError::NotDue);
        h.w.env.warp(1);
        let tx = h.burn();
        tx.ok();
        let ev: StrandedBurned = tx.event();
        assert_eq!((ev.lamports, ev.since), (holds + fourth, bought_at));
        println!(
            "a griefer's {claims} small credits ({griefed} lamports) put nothing off; after the \
             burn, one more wait took a credit of {third} lamports against {} held, and the next \
             would take {holds}",
            first + second
        );
    }

    // ---- Finding 20 (info): the compute a game coin's token moves take -----------------------------

    /// A game coin's `dev_buy` and `buyback` (swap and burn through the custom hook) take more than
    /// the 200k CU a transaction gets by default, so a crank must set a compute-unit limit (the
    /// docs now say so; the keeper simulates). With the heaviest a game hook may be (burn rules on
    /// both sides, 4 registry extras), 450k is enough.
    #[test]
    fn a_game_coins_dev_buy_and_buyback_need_a_compute_unit_limit() {
        let rules = LaunchRules {
            burn_buy_bps: 50,
            burn_sell_bps: 50,
            ..LaunchRules::NONE
        };
        for heavy in [false, true] {
            let mut l = Lotto::with(
                |_| {},
                if heavy { rules } else { LaunchRules::NONE },
                lottery_hook::FLAGS,
            );
            if heavy {
                let registry = lottery::registry_address(&l.mint);
                let pda = |program: Pubkey, tag: &[u8], writable: bool| ExtraAccount {
                    writable,
                    source: AccountSource::Pda {
                        program,
                        seeds: vec![Seed::Literal(tag.to_vec()), Seed::Account(1)],
                    },
                };
                let list = HookAccountList::new(vec![
                    pda(HOOK, b"state", true),
                    pda(bordrless_launch::ID, b"launch", false),
                    pda(HOOK, b"x1", true),
                    pda(HOOK, b"x2", true),
                ]);
                let mut acc = l.w.env.account(&registry).unwrap();
                acc.data = list.encode();
                acc.lamports = l.w.env.rent(acc.data.len());
                l.w.env.put(registry, acc);
                l.custom = l.w.custom_hook_accounts(&HOOK, &l.mint);
            }
            let custom = l.custom.clone();
            let keys = l.keys();
            let launcher = l.launcher.insecure_clone();
            let dev_buy = companion::dev_buy_with(launcher.pubkey(), &keys, SOL, 1, Some(&custom));
            let bare =
                l.w.env
                    .send_bare(std::slice::from_ref(&dev_buy), &[&launcher]);
            assert!(bare.result.is_err(), "dev_buy at the default 200k CU");
            let tx =
                l.w.env
                    .send_bare(&[compute_unit_limit(450_000), dev_buy], &[&launcher]);
            within_limits("dev_buy", &tx);
            let dev_buy_cu = tx.cu();
            l.volume(4, 40 * SOL);
            l.claim_fees().ok();
            let cranker = l.cranker.insecure_clone();
            let mut buyback = None;
            for _ in 0..80 {
                l.w.env.warp(61);
                let keys = l.keys();
                let ix = companion::buyback_with(cranker.pubkey(), &keys, false, Some(&custom));
                let tx =
                    l.w.env
                        .send_bare(&[compute_unit_limit(450_000), ix], &[&cranker]);
                tx.ok();
                if !tx.events::<BoughtBack>().is_empty() {
                    buyback = Some(tx);
                    break;
                }
            }
            let tx = buyback.expect("a buyback that buys");
            within_limits("buyback", &tx);
            println!(
                "{}: dev_buy {dev_buy_cu} CU, buyback {} CU",
                if heavy {
                    "burns, 4 extras"
                } else {
                    "lottery hook"
                },
                tx.cu()
            );
            assert!(
                dev_buy_cu > 200_000 && tx.cu() > 200_000,
                "over the default"
            );
        }
    }
}
