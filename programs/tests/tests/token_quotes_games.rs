//! Games on coins quoted in tokens (`docs/token-quotes.md` §7.6): a lottery, a jackpot and a streak
//! whose pots are kept per leg in the legs' tokens (`CompanionLegs`), their prizes paid in each leg's
//! token, their thresholds in basis points of the coin's opening value, an unaudited hook's cap and
//! a block applied per leg (a blocked pot bought back as the coin, never burned as tokens), launched
//! through the companion's `launch_token` from a config naming the game's hook, whose registry lists
//! the coin's `Legs` (`lottery_hook::prepare_legs`, the starters' `prepare`).

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{InstructionData, ToAccountMetas};
use bordrless_companion::client::{self as companion, SeedSlot};
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{CreateArgs, CreateGameArgs, GameKindArgs, HookStatusArgs};
use bordrless_companion::oracle as seeds;
use bordrless_companion::state::{
    bps_of, lamports_as_bps, CompanionLegs, DrawStatus, Game, GameKind, Split,
};
use bordrless_core::policy;
use bordrless_game::{draw_index, round_of, round_start, GameHeader, JackpotHeader};
use bordrless_launch::client::{self as launch, CustomHookAccounts, LegKeys};
use bordrless_launch::instructions::{CreateConfigArgs, TokenPayouts};
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{Env, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::orao;
use bordrless_program_tests::program_bytes;
use bordrless_program_tests::quotes::{pack_mint, Quote, QUOTE_FUNDS, TOKEN, TOKEN_2022};
use bordrless_token::client as token;
use lottery_hook::client as lottery;
use solana_keypair::Keypair;
use solana_signer::Signer;

const SOL: u64 = 1_000_000_000;
const SUPPLY: u64 = policy::TOKEN_SUPPLY;
const FEE: u16 = 200;
const BOUNTY_BPS: u16 = 50;
/// 70% of every claim to the pot, 30% to the buyback.
const SPLIT: Split = Split {
    buyback_bps: 3_000,
    holders_bps: 0,
    beneficiary_bps: 0,
};
const POT_BPS: u16 = 7_000;
/// 1% of the opening value: the site's default.
const MIN_POT_BPS: u16 = 100;
const ROUND: u32 = 3_600;
const WINDOW: u32 = 300;
const ATTEMPTS: u8 = 6;
const LOTTERY: Pubkey = lottery_hook::ID;
const JACKPOT: Pubkey = studio_jackpot::ID;
const STREAK: Pubkey = studio_streak::ID;
const STUDIO_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[0];

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

/// One leg's quote: its program and its decimals.
type Spec = (Pubkey, u8);
const ONE: [Spec; 1] = [(TOKEN, 6)];
const THREE: [Spec; 3] = [(TOKEN, 6), (TOKEN_2022, 6), (TOKEN, 9)];
const SIX: [Spec; 6] = [
    (TOKEN, 6),
    (TOKEN_2022, 6),
    (TOKEN, 9),
    (TOKEN, 6),
    (TOKEN_2022, 9),
    (TOKEN, 6),
];

/// A $5,000 opening in a token of `decimals` (a $1 token of 6, a $200 one of 9).
fn cap_raw(decimals: u8) -> u64 {
    if decimals == 9 {
        25_000_000_000
    } else {
        5_000_000_000
    }
}

fn world() -> World {
    let mut w = World::new();
    orao::load(&mut w.env);
    for (name, id) in [("studio_jackpot", JACKPOT), ("studio_streak", STREAK)] {
        w.env
            .svm
            .add_program(id, &program_bytes(name))
            .unwrap_or_else(|e| panic!("load {name}: {e:?}"));
        w.env.set_upgrade_authority(id, Some(STUDIO_KEY));
    }
    bordrless_program_tests::attest::attest_all(&mut w.env, &[JACKPOT, STREAK]);
    w
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
    let base = CreateGameArgs {
        kind,
        hook: LOTTERY,
        split: SPLIT,
        pot_bps: POT_BPS,
        round_secs: ROUND,
        min_pot: 100_000_000,
        prize_bps: 10_000,
        claim_window_secs: WINDOW,
        max_attempts: ATTEMPTS,
    };
    match kind {
        GameKind::Jackpot => (
            CreateGameArgs {
                hook: JACKPOT,
                round_secs: 0,
                prize_bps: 5_000,
                claim_window_secs: 0,
                max_attempts: 0,
                ..base
            },
            GameKindArgs {
                timer_secs: studio_jackpot::TIMER_SECS,
                min_tokens: studio_jackpot::MIN_TOKENS,
                ..GameKindArgs::default()
            },
        ),
        GameKind::Streak => (
            CreateGameArgs {
                hook: STREAK,
                round_secs: studio_streak::EPOCH_SECS,
                claim_window_secs: 3_600,
                max_attempts: 0,
                ..base
            },
            GameKindArgs {
                min_streak_secs: studio_streak::MIN_STREAK_SECS,
                min_weight: studio_streak::MIN_WEIGHT,
                ..GameKindArgs::default()
            },
        ),
        _ => (base, GameKindArgs::default()),
    }
}

/// A Studio starter's `prepare_legs` (its registry: the state, the launch and the coin's `Legs`).
fn prepare_starter(hook: Pubkey, payer: Pubkey, mint: Pubkey) -> Instruction {
    Instruction {
        program_id: hook,
        accounts: studio_jackpot::accounts::Prepare {
            payer,
            mint,
            state: bordrless_game::state_address(&hook, &mint).0,
            registry: bordrless_hook::hook_accounts_address(&hook, &mint).0,
            system_program: bordrless_program_tests::SYSTEM_PROGRAM_ID,
        }
        .to_account_metas(None),
        data: studio_jackpot::instruction::PrepareLegs {}.data(),
    }
}

/// The streak starter's `enter` for `owner`'s holding.
fn streak_enter(mint: Pubkey, owner: Pubkey) -> Instruction {
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

fn set_slot_hash(env: &mut Env) -> SeedSlot {
    let slot = env.slot - 1;
    let mut from = [0u8; 32];
    from[..8].copy_from_slice(&slot.to_le_bytes());
    let hash: [u8; 32] = orao::randomness_for(&from)[..32].try_into().unwrap();
    let mut sysvar = env.account(&seeds::SLOT_HASHES).expect("slot hashes");
    sysvar.data[..8].copy_from_slice(&1u64.to_le_bytes());
    sysvar.data[8..16].copy_from_slice(&slot.to_le_bytes());
    sysvar.data[16..48].copy_from_slice(&hash);
    env.put(seeds::SLOT_HASHES, sysvar);
    SeedSlot { slot, hash }
}

/// A randomness whose first attempt draws a ticket `want` takes, for `total` tickets.
fn randomness_where(total: u64, want: impl Fn(u64) -> bool) -> [u8; 64] {
    for i in 0u64..2_000_000 {
        let mut seed = [7u8; 32];
        seed[..8].copy_from_slice(&i.to_le_bytes());
        let r = orao::randomness_for(&seed);
        if want(draw_index(&r, 0, total).unwrap()) {
            return r;
        }
    }
    panic!("no randomness found");
}

/// A game coin quoted in tokens, launched through its companion from a config naming its hook,
/// every leg open, past the sniper window.
struct Coin {
    w: World,
    kind: GameKind,
    mint: Pubkey,
    hook: Pubkey,
    /// Each leg's quote mint and token program.
    quotes: Vec<(Pubkey, Pubkey)>,
    custom: CustomHookAccounts,
    cranker: Keypair,
    launcher: Keypair,
}

impl Coin {
    fn new(kind: GameKind, specs: &[Spec]) -> Self {
        Self::try_new(kind, specs, Some(MIN_POT_BPS), false)
            .unwrap_or_else(|tx| panic!("launch: {}", tx.logs().join("\n")))
    }

    /// The setup and the launch; `terms` the game's token terms (none: not set), `switch` the coin's
    /// SOL switch. The launch's transaction when it fails.
    #[allow(clippy::result_large_err)]
    fn try_new(
        kind: GameKind,
        specs: &[Spec],
        terms: Option<u16>,
        switch: bool,
    ) -> Result<Self, Tx> {
        let mut w = world();
        // The legs' tokens, each at an even share of a $5,000 opening.
        let issuer = w.env.deployer.pubkey();
        let n = specs.len() as u64;
        let mut quotes = vec![];
        let mut launch_quotes: Vec<Quote> = vec![];
        for (i, (program, decimals)) in specs.iter().enumerate() {
            let key = Pubkey::new_unique();
            w.env.put_mint(
                key,
                *program,
                pack_mint(Some(issuer), None, *decimals, u64::MAX / 2, &[]),
            );
            let share = if i as u64 == n - 1 {
                SUPPLY - SUPPLY / n * (n - 1)
            } else {
                SUPPLY / n
            };
            let vq =
                (u128::from(cap_raw(*decimals)) * u128::from(share) / u128::from(SUPPLY)) as u64;
            quotes.push((key, *program));
            launch_quotes.push((key, vq));
        }
        Self::try_launch(w, kind, quotes, launch_quotes, terms, switch)
    }

    /// The launch in `w` on `quotes` (each leg's token and program) opening at `launch_quotes`.
    #[allow(clippy::result_large_err)]
    fn try_launch(
        mut w: World,
        kind: GameKind,
        quotes: Vec<(Pubkey, Pubkey)>,
        launch_quotes: Vec<Quote>,
        terms: Option<u16>,
        switch: bool,
    ) -> Result<Self, Tx> {
        let launcher = w.env.funded(100 * SOL);
        let mint_kp = Keypair::new();
        let mint = mint_kp.pubkey();
        let (args, k) = game_args(kind);
        let hook = args.hook;
        let mut ixs = vec![companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            mint,
            create_args(),
        )];
        match kind {
            GameKind::Lottery => {
                ixs.push(lottery::prepare_legs(launcher.pubkey(), mint, ROUND));
                ixs.push(companion::create_game(launcher.pubkey(), mint, args));
            }
            _ => {
                ixs.push(prepare_starter(hook, launcher.pubkey(), mint));
                ixs.push(companion::create_game_v2_attested(
                    launcher.pubkey(),
                    mint,
                    args,
                    k,
                    false,
                ));
            }
        }
        if let Some(bps) = terms {
            ixs.push(companion::set_game_token_terms(mint, bps));
        }
        w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]).ok();
        let (config, tx) = w.create_config(
            &launcher,
            CreateConfigArgs {
                rules: LaunchRules::NONE,
                creator_fee_bps: FEE,
                custom_hook: Some(hook),
                custom_hook_flags: kind.hook_flags(),
                label: "Game".to_string(),
            },
        );
        tx.ok();
        let custom = CustomHookAccounts {
            program: hook,
            extras: vec![
                AccountMeta::new(bordrless_game::state_address(&hook, &mint).0, false),
                AccountMeta::new_readonly(launch::launch_address(&mint), false),
                AccountMeta::new_readonly(launch::legs_address(&mint), false),
            ],
        };
        let mints: Vec<Pubkey> = quotes.iter().map(|q| q.0).collect();
        let mut largs =
            World::token_launch_args_with("GAME", FEE, &launch_quotes, LaunchRules::NONE);
        if switch {
            largs.payouts = TokenPayouts {
                flags: 1,
                holder_round_secs: 0,
            };
        }
        let inner = launch::create_token_launch_with(
            companion::creator_address(&mint),
            mint,
            w.env.treasury.pubkey(),
            largs.clone(),
            &mints,
            Some(config),
            Some(&custom),
        );
        let ix = companion::launch_token_game(launcher.pubkey(), mint, &inner, largs);
        let tx = w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]);
        if tx.result.is_err() {
            return Err(tx);
        }
        let payer = w.env.payer.insecure_clone();
        for i in 0..quotes.len() {
            let ix = launch::open_leg_custom(
                payer.pubkey(),
                w.env.treasury.pubkey(),
                &w.leg_keys(&mint, i),
                i as u8,
                &custom,
            );
            w.env.send(&[ix], &[]).ok();
        }
        w.env.warp(31);
        let cranker = w.env.funded(10 * SOL);
        Ok(Self {
            w,
            kind,
            mint,
            hook,
            quotes,
            custom,
            cranker,
            launcher,
        })
    }

    fn keys(&self, i: usize) -> LegKeys {
        self.w.leg_keys(&self.mint, i)
    }

    fn game(&self) -> Game {
        self.w.env.read(&companion::game_address(&self.mint))
    }

    fn legs(&self) -> CompanionLegs {
        self.w
            .env
            .read(&companion::companion_legs_address(&self.mint))
    }

    /// A wallet holding plenty of every leg's token.
    fn trader(&mut self) -> Keypair {
        let kp = self.w.env.funded(10 * SOL);
        for (q, p) in &self.quotes {
            self.w.env.token_account(&kp.pubkey(), q, p, QUOTE_FUNDS);
        }
        kp
    }

    fn leg_swap(&self, who: &Pubkey, i: usize, direction: u8, amount: u64) -> Instruction {
        let keys = self.keys(i);
        launch::leg_swap_with_slice(
            &keys,
            *who,
            token::holding_address(&self.mint, who),
            keys.quote_account(who),
            direction,
            amount,
            0,
            self.custom.slice(),
            false,
        )
    }

    fn buy(&mut self, t: &Keypair, i: usize, amount: u64) -> Tx {
        let who = t.pubkey();
        let ixs = [
            token::create_holding(who, self.mint, who),
            self.leg_swap(&who, i, 1, amount),
        ];
        self.w.env.send_paid_by(&ixs, t, &[])
    }

    fn sell(&mut self, t: &Keypair, i: usize, coins: u64) -> Tx {
        let ix = self.leg_swap(&t.pubkey(), i, 0, coins);
        self.w.env.send_paid_by(&[ix], t, &[])
    }

    /// Volume on every leg (four buys of an eighth of the leg's opening value, each one's coins
    /// sold back: about the opening value traded on each leg, 2% of it in fees, 70% of that to the
    /// pot, about 1.4% of the opening value), then each leg's fee claimed and split: the pot's
    /// part to each leg's pot.
    fn fund(&mut self) {
        self.volume();
        self.claim_waiting();
    }

    /// [`Self::fund`]'s volume, its fees left in the legs' fee accounts.
    fn volume(&mut self) {
        let t = self.trader();
        for i in 0..self.quotes.len() {
            for _ in 0..4 {
                let vq = self.w.legs(&self.mint).legs[i].virtual_quote;
                self.buy(&t, i, vq / 8).ok();
                let coins = self.w.env.holding(&self.mint, &t.pubkey());
                self.sell(&t, i, coins).ok();
            }
        }
    }

    /// Each leg's fee claimed where the leg holds any.
    fn claim_waiting(&mut self) {
        for i in 0..self.quotes.len() {
            let keys = self.keys(i);
            let waiting = self
                .w
                .env
                .account(&keys.fee_account())
                .map(|a| u64::from_le_bytes(a.data[64..72].try_into().unwrap()))
                .unwrap_or(0);
            if waiting > 0 {
                self.claim_leg(i).ok();
            }
        }
    }

    fn claim_leg(&mut self, i: usize) -> Tx {
        let ix = companion::claim_fees_leg(self.cranker.pubkey(), &self.keys(i), i as u8, false);
        self.send(ix)
    }

    fn send(&mut self, ix: Instruction) -> Tx {
        let cranker = self.cranker.insecure_clone();
        self.w.env.send_paid_by(&[ix], &cranker, &[])
    }

    /// The pot as a share of the opening value, as the companion measures it.
    fn pot_bps(&self) -> u64 {
        let legs = self.legs();
        let l = self.w.legs(&self.mint);
        let n = u128::from(legs.count);
        (0..usize::from(legs.count))
            .map(|i| {
                u128::from(legs.legs[i].pending_pot) * 10_000
                    / (u128::from(l.legs[i].virtual_quote) * n)
            })
            .sum::<u128>() as u64
    }

    /// The accounts a pot in the legs' tokens adds to a step.
    fn pot_accounts(&self) -> Vec<AccountMeta> {
        companion::token_pot_accounts(&self.mint)
    }

    /// The accounts paying a prize to `winner` adds.
    fn prize_accounts(&self, winner: &Pubkey, bounty: bool) -> Vec<AccountMeta> {
        companion::leg_prize_accounts(
            &self.mint,
            &self.quotes,
            winner,
            &self.cranker.pubkey(),
            bounty,
        )
    }

    /// `owner`'s balance of leg `i`'s token (0 without an account).
    fn quote_balance(&self, owner: &Pubkey, i: usize) -> u64 {
        let (q, p) = self.quotes[i];
        let key = bordrless_swap::spl::associated_token_address(owner, &q, &p);
        if self.w.env.account(&key).is_some() {
            self.w.env.token_amount(&key)
        } else {
            0
        }
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

    // ---- The lottery ------------------------------------------------------------------------------

    fn round(&self) -> u32 {
        round_of(self.w.env.now, ROUND)
    }

    fn warp_into(&mut self, round: u32, secs: i64) {
        let t = i64::from(round) * i64::from(ROUND) + secs;
        assert!(t >= self.w.env.now);
        self.w.env.warp(t - self.w.env.now);
    }

    fn enter(&mut self, holders: &[&Pubkey]) {
        for h in holders {
            let ix = match self.kind {
                GameKind::Streak => streak_enter(self.mint, **h),
                _ => lottery::enter(self.mint, **h),
            };
            self.send(ix).ok();
        }
    }

    fn draw(&mut self, round: u32) -> Tx {
        let at = set_slot_hash(&mut self.w.env);
        let paid = self.game().paid_seed;
        let ix = companion::draw_after(
            self.cranker.pubkey(),
            self.mint,
            self.hook,
            round,
            at,
            orao::TREASURY,
            paid,
        );
        let ix = companion::with_accounts(ix, self.pot_accounts());
        self.send(ix)
    }

    fn reveal(&mut self) -> Tx {
        let request = self.game().request;
        let ix = companion::reveal(self.cranker.pubkey(), self.mint, self.hook, request);
        let ix = companion::with_accounts(ix, self.pot_accounts());
        self.send(ix)
    }

    fn claim_prize(&mut self, winner: &Pubkey, bounty: bool) -> Tx {
        let ix = companion::claim_prize(self.cranker.pubkey(), self.mint, self.hook, 0, *winner);
        let ix = companion::with_accounts(ix, self.prize_accounts(winner, bounty));
        self.send(ix)
    }
}

/// A three-leg lottery (SPL Token and Token-2022): tickets for holders of the coin bought on any
/// leg; the pot each leg's part of its fees; the draw measured on the pot's share of the opening
/// value, the oracle paid from the SOL float; each leg's prize fixed at the request and paid to the
/// winner's account of each leg's token, less the sender's bounty in each.
#[test]
fn a_lottery_in_the_legs_tokens_draws_and_pays_every_leg() {
    for specs in [&ONE[..], &THREE[..]] {
        let mut c = Coin::new(GameKind::Lottery, specs);
        let mint = c.mint;
        let g = c.game();
        assert!(g.in_tokens() && g.min_pot_bps() == MIN_POT_BPS);
        // The float: the launcher's funded spare, booked to the SOL pot, not to the launcher.
        let comp: bordrless_companion::state::Companion =
            c.w.env.read(&companion::companion_address(&mint));
        assert!(comp.pending_pot > 0 && comp.pending_beneficiary == 0);
        let legs = c.legs();
        assert_eq!(usize::from(legs.count), specs.len());
        // Holders on different legs.
        let a = c.trader();
        let b = c.trader();
        let vq0 = c.w.legs(&mint).legs[0].virtual_quote;
        c.buy(&a, 0, vq0 / 50).ok();
        let last = specs.len() - 1;
        let vq = c.w.legs(&mint).legs[last].virtual_quote;
        c.buy(&b, last, vq / 100).ok();
        let r0 = c.round() + 1;
        c.warp_into(r0, 5);
        c.enter(&[&a.pubkey(), &b.pubkey()]);
        // Below the least pot: no draw.
        c.warp_into(r0 + 1, 5);
        refused(&c.draw(r0), CompanionError::PotTooSmall);
        // Volume on every leg, each leg's fee claimed: the pot's part on each leg.
        c.fund();
        assert!(c.pot_bps() >= u64::from(MIN_POT_BPS), "the pot reaches 1%");
        let pots_before: Vec<u64> = c.legs().legs.iter().map(|p| p.pending_pot).collect();
        let tx = c.draw(r0);
        tx.ok();
        println!(
            "token draw ({} legs): CU {} size {}",
            specs.len(),
            tx.cu(),
            tx.size
        );
        let fixed: LegPrizesFixed = tx.event();
        assert_eq!(fixed.prizes.len(), specs.len());
        for (i, p) in fixed.prizes.iter().enumerate() {
            assert_eq!(*p, pots_before[i], "prize_bps 100%: the whole leg pot");
        }
        assert_eq!(c.game().prize, 0, "no SOL prize");
        // ORAO answers; the first attempt draws a's ticket.
        let total = c.game().total;
        let ra = c.w.env.hook_data(&mint, &a.pubkey());
        let range = bordrless_game::Slots::decode(&ra).range_in(r0).unwrap();
        let randomness = randomness_where(total, |t| range.contains(t));
        let seed = c.game().seed;
        orao::fulfil(&mut c.w.env, &seed, &randomness);
        c.reveal().ok();
        // b holds no winning ticket.
        refused(
            &c.claim_prize(&b.pubkey(), true),
            CompanionError::NotTheWinner,
        );
        // A sender can't leave a leg out: every leg's account of the winner must be passed.
        let left_out = c.keys(specs.len() - 1).quote_account(&a.pubkey());
        let mut extra = c.prize_accounts(&a.pubkey(), false);
        extra.retain(|m| m.pubkey != left_out);
        let ix = companion::with_accounts(
            companion::claim_prize(c.cranker.pubkey(), mint, c.hook, 0, a.pubkey()),
            extra,
        );
        refused(&c.send(ix), CompanionError::MissingAccount);
        // Nor pass one read-only (it could take nothing, and the leg would be skipped).
        let mut extra = c.prize_accounts(&a.pubkey(), false);
        for m in extra.iter_mut().filter(|m| m.pubkey == left_out) {
            m.is_writable = false;
        }
        let ix = companion::with_accounts(
            companion::claim_prize(c.cranker.pubkey(), mint, c.hook, 0, a.pubkey()),
            extra,
        );
        refused(&c.send(ix), CompanionError::PayoutTarget);
        // The sender's accounts of each leg's token, for the bounty.
        let cranker = c.cranker.pubkey();
        for (q, p) in c.quotes.clone() {
            c.w.env.token_account(&cranker, &q, &p, 0);
        }
        let before: Vec<u64> = (0..specs.len())
            .map(|i| c.quote_balance(&a.pubkey(), i))
            .collect();
        let tx = c.claim_prize(&a.pubkey(), true);
        tx.ok();
        println!(
            "token claim_prize ({} legs): CU {} size {}",
            specs.len(),
            tx.cu(),
            tx.size
        );
        let paid: Vec<LegPrizePaid> = tx.events();
        assert_eq!(paid.len(), specs.len());
        for (i, e) in paid.iter().enumerate() {
            let prize = fixed.prizes[i];
            let bounty = bps_of(prize, u64::from(BOUNTY_BPS));
            assert_eq!(
                (e.index as usize, e.amount, e.bounty),
                (i, prize - bounty, bounty)
            );
            assert_eq!(c.quote_balance(&a.pubkey(), i) - before[i], prize - bounty);
            assert_eq!(c.quote_balance(&cranker, i), bounty);
            let leg = c.legs().legs[i];
            assert_eq!(leg.pending_pot, pots_before[i] - prize);
            assert_eq!(leg.prize_paid, prize);
        }
        assert_eq!(c.game().status, DrawStatus::Idle);
        // The creator address's accounts hold exactly the legs' parts.
        for i in 0..specs.len() {
            let leg = c.legs().legs[i];
            let held = c.quote_balance(&companion::creator_address(&mint), i);
            assert_eq!(held, leg.set_aside().unwrap());
        }
    }
}

/// A game on a coin quoted in tokens takes its terms before the launch: none, it can't launch
/// without the switch; with the switch, it plays as a SOL coin's and takes none. The terms are 0
/// to 10,000 basis points, set by the mint before the launch only.
#[test]
fn a_game_needs_its_token_terms_and_none_with_the_switch() {
    let tx = Coin::try_new(GameKind::Lottery, &ONE, None, false)
        .err()
        .unwrap();
    refused(&tx, CompanionError::TokenGameTerms);
    let tx = Coin::try_new(GameKind::Lottery, &ONE, Some(MIN_POT_BPS), true)
        .err()
        .unwrap();
    refused(&tx, CompanionError::TokenGameTerms);
    let c = Coin::try_new(GameKind::Lottery, &ONE, None, true)
        .unwrap_or_else(|tx| panic!("{}", tx.logs().join("\n")));
    assert!(!c.game().in_tokens(), "a SOL coin's game");
    // Bounds, and nothing after the launch.
    let mut c = Coin::new(GameKind::Lottery, &ONE);
    let mut ix = companion::set_game_token_terms(c.mint, 50);
    ix.accounts[0].is_signer = false;
    let launcher = c.launcher.insecure_clone();
    let tx = c.w.env.send_paid_by(&[ix], &launcher, &[]);
    assert!(tx.result.is_err(), "the mint must sign");
    let mut w = world();
    let launcher = w.env.funded(10 * SOL);
    let mint = Keypair::new();
    let (args, _) = game_args(GameKind::Lottery);
    w.env
        .send_paid_by(
            &[
                companion::create(
                    launcher.pubkey(),
                    launcher.pubkey(),
                    mint.pubkey(),
                    create_args(),
                ),
                lottery::prepare_legs(launcher.pubkey(), mint.pubkey(), ROUND),
                companion::create_game(launcher.pubkey(), mint.pubkey(), args),
            ],
            &launcher,
            &[&mint],
        )
        .ok();
    let tx = w.env.send_paid_by(
        &[companion::set_game_token_terms(mint.pubkey(), 10_001)],
        &launcher,
        &[&mint],
    );
    refused(&tx, CompanionError::BadTokenTerms);
    let tx = w.env.send_paid_by(
        &[companion::set_game_token_terms(mint.pubkey(), 10_000)],
        &launcher,
        &[&mint],
    );
    let ev: GameTokenTermsSet = tx.event();
    assert_eq!(ev.min_pot_bps, 10_000);
}

/// An unaudited hook's cap on a pot in tokens: each leg's pot above its share of the cap (the
/// lamport cap read as basis points of the opening value) moves to the leg's buyback; a block moves
/// all of it; the buyback buys the coin back on the leg and burns it (never the leg's token). A
/// dormant game's pot is retired the same way.
#[test]
fn a_cap_and_a_block_move_each_legs_pot_to_its_buyback_which_buys_the_coin_back() {
    let mut c = Coin::new(GameKind::Lottery, &THREE);
    c.fund();
    c.fund();
    let pots: Vec<u64> = c.legs().legs.iter().map(|p| p.pending_pot).collect();
    // A cap of a SOL: 200 basis points of the opening value, under the pot and over its least.
    assert!(c.pot_bps() > 200);
    c.set_status(false, SOL, false).ok();
    let cap_bps = lamports_as_bps(SOL);
    let r = c.round();
    c.warp_into(r + 1, 5);
    let tx = c.draw(r);
    let moved: Vec<LegPotToBuyback> = tx.events();
    assert!(!moved.is_empty());
    let l = c.w.legs(&c.mint);
    for e in &moved {
        let i = usize::from(e.index);
        let leg_cap = (u128::from(cap_bps) * u128::from(l.legs[i].virtual_quote) / 10_000) as u64;
        assert_eq!((e.reason, e.pending_pot), (0, leg_cap));
        assert_eq!(e.amount, pots[i] - leg_cap);
    }
    // A block: every leg's pot to its buyback.
    c.set_status(false, SOL, true).ok();
    let r = c.round();
    c.warp_into(r + 1, 5);
    let tx = c.draw(r);
    tx.ok();
    let moved: Vec<LegPotToBuyback> = tx.events();
    assert!(moved.iter().all(|e| e.reason == 1 && e.pending_pot == 0));
    assert!(c.legs().legs[..3].iter().all(|p| p.pending_pot == 0));
    // The buyback buys the coin back on each leg and burns it; the leg's token is spent, not burned.
    let supply_before = c.w.env.read::<bordrless_token::state::Mint>(&c.mint).supply;
    for i in 0..3 {
        let quote_supply_before = c.w.env.account(&c.quotes[i].0).unwrap().data[36..44].to_vec();
        let ix = companion::buyback_leg(
            c.cranker.pubkey(),
            &c.keys(i),
            i as u8,
            &LaunchRules::NONE,
            Some(&c.custom),
            false,
        );
        let tx = c.send(ix);
        tx.ok();
        let ev: LegBoughtBack = tx.event();
        assert!(ev.burned > 0);
        assert_eq!(
            c.w.env.account(&c.quotes[i].0).unwrap().data[36..44].to_vec(),
            quote_supply_before,
            "the leg's token is never burned"
        );
    }
    assert!(c.w.env.read::<bordrless_token::state::Mint>(&c.mint).supply < supply_before);
}

/// A dormant lottery's pots in tokens are retired to each leg's buyback.
#[test]
fn retire_moves_each_legs_pot_to_its_buyback() {
    let mut c = Coin::new(GameKind::Lottery, &THREE);
    c.fund();
    let pots: Vec<u64> = c.legs().legs.iter().map(|p| p.pending_pot).collect();
    let g = c.game();
    let comp: bordrless_companion::state::Companion =
        c.w.env.read(&companion::companion_address(&c.mint));
    let at = g.retirable_at(comp.launched_at);
    c.w.env.warp(at - c.w.env.now + 1);
    let ix = companion::retire(c.cranker.pubkey(), c.mint, c.hook);
    let tx = c.send(companion::with_accounts(ix, c.pot_accounts()));
    tx.ok();
    let moved: Vec<LegPotToBuyback> = tx.events();
    assert_eq!(moved.len(), 3);
    for e in &moved {
        assert_eq!((e.reason, e.amount), (2, pots[usize::from(e.index)]));
    }
    assert!(c.legs().legs[..3].iter().all(|p| p.pending_pot == 0));
}

/// A jackpot on a coin quoted in tokens: a buy out of any leg is a qualifying buy; `settle` pays
/// `prize_bps` of each leg's pot to the round's buyer, in each leg's token; a pot short of its
/// least while a leg's fee account holds fees waits for the claim (`FeesUnclaimed`).
#[test]
fn a_jackpot_counts_buys_out_of_any_leg_and_pays_each_leg() {
    let mut c = Coin::new(GameKind::Jackpot, &THREE);
    let mint = c.mint;
    let buyer = c.trader();
    // Enough coins for a qualifying buy (1,000,000 tokens) on the third leg.
    let vq = c.w.legs(&mint).legs[2].virtual_quote;
    c.buy(&buyer, 2, vq / 20).ok();
    let state =
        c.w.env
            .account(&bordrless_game::state_address(&JACKPOT, &mint).0)
            .unwrap();
    let header = GameHeader::read(&state.data, &mint).unwrap();
    let jackpot = JackpotHeader::parse(&state.data).unwrap();
    assert_eq!((header.last_buyer, jackpot.buys), (buyer.pubkey(), 1));
    // Fees waiting on the legs that would fund the round while the pot is short: settle waits for
    // their claim (`FeesUnclaimed`), as `fundable` makes it wait on a SOL coin.
    c.w.env.warp(i64::from(studio_jackpot::TIMER_SECS) + 1);
    c.volume();
    assert_eq!(c.pot_bps(), 0, "nothing claimed yet");
    // Leg 0's fees claimed raw (the launchpad's own claim, which anyone may send): unsplit in the
    // companion address's account, they count as a `claim_fees_leg` would split them.
    let ix = launch::claim_creator_fees_token(
        c.cranker.pubkey(),
        companion::creator_address(&mint),
        &c.keys(0),
        0,
    );
    c.send(ix).ok();
    let settle = |c: &Coin, winner: &Pubkey| {
        let ix = companion::settle(c.cranker.pubkey(), c.mint, JACKPOT, *winner);
        let mut extra = c.prize_accounts(winner, false);
        extra.extend(companion::leg_fee_accounts(&c.mint, &c.quotes));
        companion::with_accounts(ix, extra)
    };
    let ix = settle(&c, &buyer.pubkey());
    refused(&c.send(ix), CompanionError::FeesUnclaimed);
    // Its fees claimed: the pot at its least, half of each leg's pot won. (The trader's buys
    // started new rounds; the buyer's round had ended.)
    c.claim_waiting();
    c.claim_leg(0).ok();
    let pots: Vec<u64> = c.legs().legs.iter().map(|p| p.pending_pot).collect();
    assert!(c.pot_bps() >= u64::from(MIN_POT_BPS));
    let before: Vec<u64> = (0..3)
        .map(|i| c.quote_balance(&buyer.pubkey(), i))
        .collect();
    let ix = settle(&c, &buyer.pubkey());
    let tx = c.send(ix);
    tx.ok();
    println!("token settle (3 legs): CU {} size {}", tx.cu(), tx.size);
    let paid: Vec<LegPrizePaid> = tx.events();
    assert_eq!(paid.len(), 3);
    for e in &paid {
        let i = usize::from(e.index);
        assert_eq!(e.kind, 1);
        assert_eq!(e.amount, pots[i] / 2);
        assert_eq!(c.quote_balance(&buyer.pubkey(), i) - before[i], pots[i] / 2);
    }
}

/// A streak on a coin quoted in tokens: `close_epoch` fixes each leg's epoch pot; `claim_share`
/// pays the holding its share of each, in each leg's token, once.
#[test]
fn a_streak_locks_and_shares_each_leg() {
    let mut c = Coin::new(GameKind::Streak, &THREE);
    let mint = c.mint;
    let a = c.trader();
    let vq = c.w.legs(&mint).legs[1].virtual_quote;
    c.buy(&a, 1, vq / 20).ok();
    let epoch = round_of(c.w.env.now, studio_streak::EPOCH_SECS);
    // Held through the next epoch, entered at its start.
    let next = epoch + 1;
    let t = round_start(next, studio_streak::EPOCH_SECS) + 5;
    c.w.env.warp(t - c.w.env.now);
    c.enter(&[&a.pubkey()]);
    c.fund();
    let t = round_start(next + 1, studio_streak::EPOCH_SECS) + 5;
    c.w.env.warp(t - c.w.env.now);
    let pots: Vec<u64> = c.legs().legs.iter().map(|p| p.pending_pot).collect();
    let ix = companion::close_epoch(c.cranker.pubkey(), mint, STREAK, next);
    let tx = c.send(companion::with_accounts(ix, c.pot_accounts()));
    tx.ok();
    let fixed: LegPrizesFixed = tx.event();
    assert_eq!(fixed.kind, 2);
    for (i, p) in fixed.prizes.iter().enumerate() {
        assert_eq!(*p, pots[i]);
    }
    let ix = companion::claim_share(c.cranker.pubkey(), mint, STREAK, next, a.pubkey());
    let ix = companion::with_accounts(ix, c.prize_accounts(&a.pubkey(), false));
    let tx = c.send(ix);
    tx.ok();
    println!(
        "token claim_share (3 legs): CU {} size {}",
        tx.cu(),
        tx.size
    );
    let paid: Vec<LegPrizePaid> = tx.events();
    assert_eq!(
        paid.len(),
        3,
        "the only holder takes each leg's whole epoch pot"
    );
    for e in &paid {
        assert_eq!((e.kind, e.amount), (2, fixed.prizes[usize::from(e.index)]));
    }
    // Once.
    let ix = companion::claim_share(c.cranker.pubkey(), mint, STREAK, next, a.pubkey());
    let ix = companion::with_accounts(ix, c.prize_accounts(&a.pubkey(), false));
    assert!(c.send(ix).result.is_err());
}

/// The measured steps on six legs.
#[test]
fn measures_games_in_tokens() {
    let mut c = Coin::new(GameKind::Lottery, &SIX);
    let mint = c.mint;
    let a = c.trader();
    let vq = c.w.legs(&mint).legs[0].virtual_quote;
    c.buy(&a, 0, vq / 50).ok();
    let r0 = c.round() + 1;
    c.warp_into(r0, 5);
    c.enter(&[&a.pubkey()]);
    c.fund();
    c.warp_into(r0 + 1, 5);
    let tx = c.draw(r0);
    tx.ok();
    println!(
        "token draw (6 legs): CU {} size {} accounts {}",
        tx.cu(),
        tx.size,
        tx.keys.len()
    );
    let total = c.game().total;
    let randomness = randomness_where(total, |_| true);
    let seed = c.game().seed;
    orao::fulfil(&mut c.w.env, &seed, &randomness);
    c.reveal().ok();
    // Through the keeper's per-coin lookup table (every account but the cranker, the winner and
    // the winner's accounts of the legs' tokens).
    let winner = a.pubkey();
    let ix = companion::with_accounts(
        companion::claim_prize(c.cranker.pubkey(), c.mint, c.hook, 0, winner),
        c.prize_accounts(&winner, false),
    );
    let theirs: Vec<Pubkey> = c
        .quotes
        .iter()
        .map(|(q, p)| bordrless_swap::spl::associated_token_address(&winner, q, p))
        .chain([winner, c.cranker.pubkey()])
        .collect();
    let mut keys: Vec<Pubkey> = ix
        .accounts
        .iter()
        .map(|m| m.pubkey)
        .filter(|k| !theirs.contains(k))
        .collect();
    keys.sort();
    keys.dedup();
    let table = c.w.env.put_lookup_table(Pubkey::new_unique(), &keys);
    let cranker = c.cranker.insecure_clone();
    let through_table = c.w.env.v0_size(&[ix], &cranker, &[], &[table]);
    let tx = c.claim_prize(&winner, false);
    tx.ok();
    println!(
        "token claim_prize (6 legs): CU {} size {} accounts {} through a table {}",
        tx.cu(),
        tx.size,
        tx.keys.len(),
        through_table
    );
}

/// With the SOL switch a game plays as a SOL coin's: no token terms; its pot's part of each claim
/// waits on the leg in the leg's token until `swap_bucket_to_sol(index, 2)` sells it into the SOL
/// pot on the token's reference pool (TSLAx on Raydium CLMM); then the draw, the oracle and the
/// prize are SOL, as on a SOL coin, with nothing per leg. Ignored unless the venue programs' dumps
/// are in `fixtures/venues` (`scripts/solana/venue-fixtures.sh`).
#[test]
#[ignore = "needs the venue programs: scripts/solana/venue-fixtures.sh"]
fn a_switched_lottery_sells_its_pot_to_sol_and_plays_as_a_sol_coins() {
    use bordrless_companion::events::LegSwappedToSol;
    use bordrless_program_tests::venues::{load_programs, Fixture, Venue, VENUE_NOW};
    let tslax = Pubkey::from_str_const("XsDoVfqeBukxuZHWhdvWHBhgEHjGNst4MLodqsJHzoB");
    let mut w = world();
    load_programs(&mut w.env);
    let fixture = Fixture::load("clmm_tslax_wsol");
    fixture.put(&mut w.env);
    w.env.now = VENUE_NOW;
    w.env.slot += 1_000;
    w.env.sync_clock();
    let v = Venue::clmm(&fixture, tslax);
    let payer = w.env.funded(10 * SOL);
    let ix = launch::register_quote_pool(payer.pubkey(), v.token, v.accounts.clone(), None, None);
    w.env.send_paid_by(&[ix], &payer, &[]).ok();
    let program = w.env.account(&tslax).expect("TSLAx").owner;
    let mut c = Coin::try_launch(
        w,
        GameKind::Lottery,
        vec![(tslax, program)],
        vec![(tslax, 1_300_000_000)],
        None,
        true,
    )
    .unwrap_or_else(|tx| panic!("launch: {}", tx.logs().join("\n")));
    let mint = c.mint;
    assert!(!c.game().in_tokens(), "no token terms with the switch");
    let a = c.trader();
    let vq = c.w.legs(&mint).legs[0].virtual_quote;
    c.buy(&a, 0, vq / 50).ok();
    let r0 = c.round() + 1;
    c.warp_into(r0, 5);
    c.enter(&[&a.pubkey()]);
    c.fund();
    let leg = c.legs().legs[0];
    assert!(leg.pending_pot > 0 && !c.legs().pays_in_token(0));
    // The pot's part sold to SOL: the leg's bucket into the companion's SOL pot.
    let comp = |c: &Coin| -> bordrless_companion::state::Companion {
        c.w.env.read(&companion::companion_address(&mint))
    };
    let before = comp(&c).pending_pot;
    let ix =
        companion::swap_bucket_to_sol(c.cranker.pubkey(), &c.keys(0), 0, 2, v.accounts.clone());
    let tx = c.send(ix);
    tx.ok();
    let e: LegSwappedToSol = tx.event();
    println!(
        "swap_bucket_to_sol of a game's pot: {} TSLAx raw for {} lamports, CU {}",
        e.amount_in,
        e.lamports,
        tx.cu()
    );
    assert!(e.amount_in > 0 && e.amount_in <= leg.pending_pot);
    assert_eq!(c.legs().legs[0].pending_pot, leg.pending_pot - e.amount_in);
    let pot = comp(&c).pending_pot;
    assert_eq!(pot, before + e.lamports);
    assert!(
        pot >= c.game().min_pot,
        "the pot reaches the game's least in SOL"
    );
    // The draw, the oracle and the prize in SOL, the SOL steps' own accounts only.
    c.warp_into(r0 + 1, 5);
    let at = set_slot_hash(&mut c.w.env);
    let paid = c.game().paid_seed;
    let ix = companion::draw_after(
        c.cranker.pubkey(),
        mint,
        c.hook,
        r0,
        at,
        orao::TREASURY,
        paid,
    );
    let tx = c.send(ix);
    tx.ok();
    assert!(tx.events::<LegPrizesFixed>().is_empty());
    let prize = c.game().prize;
    assert!(prize > 0, "a SOL prize");
    let total = c.game().total;
    let randomness = randomness_where(total, |_| true);
    let seed = c.game().seed;
    orao::fulfil(&mut c.w.env, &seed, &randomness);
    let request = c.game().request;
    let ix = companion::reveal(c.cranker.pubkey(), mint, c.hook, request);
    c.send(ix).ok();
    let before = c.w.env.lamports(&a.pubkey());
    let ix = companion::claim_prize(c.cranker.pubkey(), mint, c.hook, 0, a.pubkey());
    let tx = c.send(ix);
    tx.ok();
    assert!(tx.events::<LegPrizePaid>().is_empty());
    assert!(c.w.env.lamports(&a.pubkey()) > before, "paid in SOL");
    assert_eq!(c.game().status, DrawStatus::Idle);
    // The creator address's account of TSLAx holds exactly the leg's parts.
    let leg = c.legs().legs[0];
    let held = c.quote_balance(&companion::creator_address(&mint), 0);
    assert_eq!(held, leg.set_aside().unwrap());
}

/// A lottery in the legs' tokens pays its oracle from its SOL float: a float that can't pay rolls
/// the round over (`OracleUnpaid`), the pots in the tokens untouched; SOL anyone sends the creator
/// address is booked into the float at the next draw, which then commits and fixes the prizes.
#[test]
fn a_lottery_whose_float_runs_out_rolls_over_until_someone_tops_it_up() {
    use anchor_lang::AccountSerialize;
    let mut c = Coin::new(GameKind::Lottery, &ONE);
    let mint = c.mint;
    let a = c.trader();
    let vq = c.w.legs(&mint).legs[0].virtual_quote;
    c.buy(&a, 0, vq / 50).ok();
    let r0 = c.round() + 1;
    c.warp_into(r0, 5);
    c.enter(&[&a.pubkey()]);
    c.fund();
    // The float spent: the companion's SOL pot, and the creator address's lamports less it.
    let key = companion::companion_address(&mint);
    let mut comp: bordrless_companion::state::Companion = c.w.env.read(&key);
    let float = comp.pending_pot;
    assert!(float > 0, "the launcher's funded spare");
    comp.pending_pot = 0;
    let mut acc = c.w.env.account(&key).unwrap();
    let mut data: &mut [u8] = &mut acc.data;
    comp.try_serialize(&mut data).unwrap();
    c.w.env.put(key, acc);
    let creator = companion::creator_address(&mint);
    let mut held = c.w.env.account(&creator).unwrap();
    held.lamports -= float;
    c.w.env.put(creator, held);
    let pots: Vec<u64> = c.legs().legs.iter().map(|p| p.pending_pot).collect();
    c.warp_into(r0 + 1, 5);
    let tx = c.draw(r0);
    tx.ok();
    let e: RolledOver = tx.event();
    assert!(matches!(e.reason, RolloverReason::OracleUnpaid));
    let after: Vec<u64> = c.legs().legs.iter().map(|p| p.pending_pot).collect();
    assert_eq!(after, pots, "the pots in the tokens untouched");
    // Anyone tops it up; the next round's draw books it and commits.
    c.enter(&[&a.pubkey()]);
    let donor = c.w.env.funded(SOL);
    let ix = anchor_lang::solana_program::system_instruction::transfer(
        &donor.pubkey(),
        &creator,
        SOL / 20,
    );
    c.w.env.send_paid_by(&[ix], &donor, &[]).ok();
    c.warp_into(r0 + 2, 5);
    let tx = c.draw(r0 + 1);
    tx.ok();
    let fixed: LegPrizesFixed = tx.event();
    assert_eq!(fixed.prizes[..], pots[..1], "prize_bps 100%: the leg's pot");
    let comp: bordrless_companion::state::Companion = c.w.env.read(&key);
    assert!(
        comp.pending_pot < SOL / 20,
        "the oracle paid from the topped-up float"
    );
    assert_eq!(c.game().status, DrawStatus::Requested);
}

/// Token terms set before a SOL coin's `launch` never count: its game is no game in tokens
/// (`launch_token` alone marks one) and every step runs the SOL path (here a draw short of its
/// least pot: `PotTooSmall`, not the token path's `LegAccountsMissing`).
#[test]
fn token_terms_before_a_sol_launch_leave_a_sol_game() {
    use bordrless_program_tests::launch::VQ;
    let mut w = world();
    let launcher = w.env.funded(100 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let (args, _) = game_args(GameKind::Lottery);
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
        lottery::prepare(launcher.pubkey(), mint, ROUND),
        companion::create_game(launcher.pubkey(), mint, args),
        companion::set_game_token_terms(mint, MIN_POT_BPS),
    ];
    w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]).ok();
    let (config, tx) = w.create_config(
        &launcher,
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: FEE,
            custom_hook: Some(LOTTERY),
            custom_hook_flags: GameKind::Lottery.hook_flags(),
            label: "Game".to_string(),
        },
    );
    tx.ok();
    let custom = w.custom_hook_accounts(&LOTTERY, &mint);
    let largs = World::launch_args("SOLG", FEE, VQ, LaunchRules::NONE);
    let inner = launch::create_launch_with(
        companion::creator_address(&mint),
        mint,
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        largs.clone(),
        Some(config),
        Some(&custom),
    );
    let ix = companion::launch(launcher.pubkey(), mint, &inner, largs);
    w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]).ok();
    let g: Game = w.env.read(&companion::game_address(&mint));
    assert!(g.min_pot_bps() == MIN_POT_BPS && !g.in_tokens());
    let round = round_of(w.env.now, ROUND);
    let t = i64::from(round + 1) * i64::from(ROUND) + 5;
    w.env.warp(t - w.env.now);
    let at = set_slot_hash(&mut w.env);
    let cranker = w.env.funded(SOL);
    let ix = companion::draw_after(
        cranker.pubkey(),
        mint,
        LOTTERY,
        round,
        at,
        orao::TREASURY,
        g.paid_seed,
    );
    refused(
        &w.env.send_paid_by(&[ix], &cranker, &[]),
        CompanionError::PotTooSmall,
    );
}

/// A pot full at its cap holds the epoch when the cap is the threshold (an unaudited hook capped at
/// 0.1 SOL, 20 basis points, below the game's 100): each leg cut to its share of the cap measures a
/// little under 20 basis points (two roundings down), and the epoch closes all the same, as a SOL
/// coin's pot at its cap is drawn (review 6, N1).
#[test]
fn a_streak_pot_full_at_its_cap_closes_its_epoch() {
    let mut c = Coin::new(GameKind::Streak, &THREE);
    let mint = c.mint;
    let a = c.trader();
    let vq = c.w.legs(&mint).legs[1].virtual_quote;
    c.buy(&a, 1, vq / 20).ok();
    let epoch = round_of(c.w.env.now, studio_streak::EPOCH_SECS);
    let next = epoch + 1;
    let t = round_start(next, studio_streak::EPOCH_SECS) + 5;
    c.w.env.warp(t - c.w.env.now);
    c.enter(&[&a.pubkey()]);
    c.fund();
    let cap = bordrless_companion::constants::MIN_POT_CAP;
    c.set_status(false, cap, false).ok();
    let t = round_start(next + 1, studio_streak::EPOCH_SECS) + 5;
    c.w.env.warp(t - c.w.env.now);
    let ix = companion::close_epoch(c.cranker.pubkey(), mint, STREAK, next);
    let tx = c.send(companion::with_accounts(ix, c.pot_accounts()));
    tx.ok();
    let moved: Vec<LegPotToBuyback> = tx.events();
    assert!(!moved.is_empty(), "each leg cut to its share of the cap");
    assert!(
        c.pot_bps() < lamports_as_bps(cap),
        "measured just under the cap"
    );
    let fixed: LegPrizesFixed = tx.event();
    assert!(fixed.prizes.iter().all(|p| *p > 0));
}
