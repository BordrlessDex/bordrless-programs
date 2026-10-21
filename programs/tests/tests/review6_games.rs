//! Games on coins quoted in tokens (`docs/token-quotes.md` §7.6): a lottery, a jackpot and a streak
//! whose pots are kept per leg in the legs' tokens (`CompanionLegs`), their prizes paid in each leg's
//! token, their thresholds in basis points of the coin's opening value, an unaudited hook's cap and
//! a block applied per leg (a blocked pot bought back as the coin, never burned as tokens), launched
//! through the companion's `launch_token` from a config naming the game's hook, whose registry lists
//! the coin's `Legs` (`lottery_hook::prepare_legs`, the starters' `prepare`).
//!
//! Review 6's regressions (the second adversarial review of §7.6), as the reviewer wrote them:
//! they keep their suite's helpers whole, some unused here.

#![allow(dead_code, unused_imports, unused_variables)]

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

// ---- review6 ------------------------------------------------------------------------------------

/// The pot's size the companion measures once each leg is cut to its share of a cap of `cap_bps`
/// (`TokenPots::leg_cap`, then `pot_bps`), both rounded down per leg.
fn capped_pot_bps(c: &Coin, cap_bps: u64) -> u64 {
    let l = c.w.legs(&c.mint);
    let n = u128::from(l.count);
    (0..usize::from(l.count))
        .map(|i| {
            let vq = u128::from(l.legs[i].virtual_quote);
            let leg_cap = u128::from(cap_bps) * vq / 10_000;
            leg_cap * 10_000 / (vq * n)
        })
        .sum::<u128>() as u64
}

/// N1: a pot full at its cap must be drawn (the SOL rule: `draw_threshold` is the cap when the cap
/// is lower, "a pot full at its cap is drawn, never stuck below a minimum it can't reach"). In
/// tokens each leg is cut to `floor(cap_bps * vq_i / 10,000)` and measured as
/// `floor(pot_i * 10,000 / (vq_i * n))`: two roundings down, so a pot full at its cap can measure
/// below `cap_bps` and never reach a threshold equal to the cap.
#[test]
fn review6_a_lottery_pot_full_at_its_cap_is_drawn() {
    let mut c = Coin::new(GameKind::Lottery, &THREE);
    let mint = c.mint;
    let a = c.trader();
    let vq0 = c.w.legs(&mint).legs[0].virtual_quote;
    c.buy(&a, 0, vq0 / 50).ok();
    let r0 = c.round() + 1;
    c.warp_into(r0, 5);
    c.enter(&[&a.pubkey()]);
    c.fund();
    c.fund();
    // The protocol caps the hook at its least cap, 0.1 SOL: 20 basis points, below the game's 100.
    c.set_status(false, 100_000_000, false).ok();
    let cap_bps = lamports_as_bps(100_000_000);
    assert!(c.pot_bps() > cap_bps, "the pot is over the cap");
    let measured = capped_pot_bps(&c, cap_bps);
    println!("cap {cap_bps} bps; a pot full at its cap measures {measured} bps");
    c.warp_into(r0 + 1, 5);
    let tx = c.draw(r0);
    assert!(
        tx.result.is_ok(),
        "a pot full at its cap ({measured} bps against a threshold of {cap_bps}) must be drawn: {}",
        tx.logs()
            .iter()
            .filter(|l| l.contains("Error Code"))
            .cloned()
            .collect::<Vec<_>>()
            .join(" ")
    );
}

/// N1, the jackpot: a round whose pot is full at its cap must be paid, not closed unfunded.
#[test]
fn review6_a_jackpot_pot_full_at_its_cap_pays_its_round() {
    let mut c = Coin::new(GameKind::Jackpot, &THREE);
    let mint = c.mint;
    let buyer = c.trader();
    let vq = c.w.legs(&mint).legs[2].virtual_quote;
    c.buy(&buyer, 2, vq / 20).ok();
    c.w.env.warp(i64::from(studio_jackpot::TIMER_SECS) + 1);
    c.fund();
    c.fund();
    c.set_status(false, 100_000_000, false).ok();
    let mut extra = c.prize_accounts(&buyer.pubkey(), false);
    extra.extend(companion::leg_fee_accounts(&c.mint, &c.quotes));
    let ix = companion::with_accounts(
        companion::settle(c.cranker.pubkey(), mint, JACKPOT, buyer.pubkey()),
        extra,
    );
    let tx = c.send(ix);
    tx.ok();
    let paid: Vec<LegPrizePaid> = tx.events();
    let unfunded: Vec<JackpotUnfunded> = tx.events();
    println!("paid legs {}, unfunded {}", paid.len(), unfunded.len());
    assert!(
        !paid.is_empty(),
        "a round whose pot is full at its cap closed unfunded"
    );
}

/// F2: a refused read-only claim leaves no receipt; the honest claim pays every leg.
#[test]
fn review6_a_refused_read_only_share_claim_leaves_the_share_claimable() {
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
    let t = round_start(next + 1, studio_streak::EPOCH_SECS) + 5;
    c.w.env.warp(t - c.w.env.now);
    let ix = companion::close_epoch(c.cranker.pubkey(), mint, STREAK, next);
    c.send(companion::with_accounts(ix, c.pot_accounts())).ok();
    let key = c.keys(1).quote_account(&a.pubkey());
    let mut extra = c.prize_accounts(&a.pubkey(), false);
    for m in extra.iter_mut().filter(|m| m.pubkey == key) {
        m.is_writable = false;
    }
    let ix = companion::claim_share(c.cranker.pubkey(), mint, STREAK, next, a.pubkey());
    refused(
        &c.send(companion::with_accounts(ix, extra)),
        CompanionError::PayoutTarget,
    );
    let ix = companion::claim_share(c.cranker.pubkey(), mint, STREAK, next, a.pubkey());
    let tx = c.send(companion::with_accounts(
        ix,
        c.prize_accounts(&a.pubkey(), false),
    ));
    tx.ok();
    let paid: Vec<LegPrizePaid> = tx.events();
    assert_eq!(paid.len(), 3, "every leg paid");
}

/// N2 (low): `fundable_bps` counts every leg's unclaimed fees, claimable or not. When a quote
/// token's issuer freezes a leg's fee account (xStocks-like issuers hold freeze authority),
/// `claim_fees_leg` can never claim them, yet `settle` waits on them (`FeesUnclaimed`) for ever:
/// the round can't close, nor any after it, and `retire` waits on it too.
#[test]
fn review6_a_frozen_fee_account_jams_a_short_jackpot() {
    let mut c = Coin::new(GameKind::Jackpot, &THREE);
    let mint = c.mint;
    // Volume on leg 0 only: its fees alone would fund the round once claimed.
    let whale = c.trader();
    let vq0 = c.w.legs(&mint).legs[0].virtual_quote;
    c.buy(&whale, 0, 2 * vq0).ok();
    let coins = c.w.env.holding(&mint, &whale.pubkey());
    c.sell(&whale, 0, coins).ok();
    let buyer = c.trader();
    let vq2 = c.w.legs(&mint).legs[2].virtual_quote;
    c.buy(&buyer, 2, vq2 / 20).ok();
    c.w.env.warp(i64::from(studio_jackpot::TIMER_SECS) + 1);
    // The issuer freezes leg 0's fee account (`AccountState` 2 at byte 108).
    let fees0 = c.keys(0).fee_account();
    let mut a = c.w.env.account(&fees0).unwrap();
    a.data[108] = 2;
    c.w.env.put(fees0, a);
    let settle = |c: &Coin, who: &Pubkey| {
        let mut extra = c.prize_accounts(who, false);
        extra.extend(companion::leg_fee_accounts(&c.mint, &c.quotes));
        companion::with_accounts(
            companion::settle(c.cranker.pubkey(), c.mint, JACKPOT, *who),
            extra,
        )
    };
    // The whale's round, if still open, forfeits (it sold).
    let _ = c.send(settle(&c, &whale.pubkey()));
    // The claim can't run; the round waits on it.
    assert!(
        c.claim_leg(0).result.is_err(),
        "a frozen fee account can't be claimed"
    );
    let tx = c.send(settle(&c, &buyer.pubkey()));
    let codes: Vec<String> = tx
        .logs()
        .iter()
        .filter(|l| l.contains("Error Code"))
        .cloned()
        .collect();
    println!(
        "settle with leg 0's fees frozen: ok {} {:?}",
        tx.result.is_ok(),
        codes
    );
    // Expected (fixed): fees nobody can claim don't hold a round open; it closes (unfunded).
    assert!(
        tx.result.is_ok(),
        "the round can never be settled while leg 0's fee account is frozen"
    );
}

/// N2 bounded, as the review found it: past `SETTLE_GRACE_SECS` the round was stale and any settle
/// forfeited it before the funding check, so a frozen fee account delayed each round by the grace.
/// With N2 fixed (fees in a frozen account count for nothing) it doesn't delay it at all: the first
/// settle closes the round, unfunded. (Adapted from the review's test, which asserted the delay.)
#[test]
fn review6_a_frozen_fee_account_no_longer_delays_a_round() {
    let mut c = Coin::new(GameKind::Jackpot, &THREE);
    let mint = c.mint;
    let whale = c.trader();
    let vq0 = c.w.legs(&mint).legs[0].virtual_quote;
    c.buy(&whale, 0, 2 * vq0).ok();
    let coins = c.w.env.holding(&mint, &whale.pubkey());
    c.sell(&whale, 0, coins).ok();
    let buyer = c.trader();
    let vq2 = c.w.legs(&mint).legs[2].virtual_quote;
    c.buy(&buyer, 2, vq2 / 20).ok();
    c.w.env.warp(i64::from(studio_jackpot::TIMER_SECS) + 1);
    let fees0 = c.keys(0).fee_account();
    let mut a = c.w.env.account(&fees0).unwrap();
    a.data[108] = 2;
    c.w.env.put(fees0, a);
    let settle = |c: &Coin, who: &Pubkey| {
        let mut extra = c.prize_accounts(who, false);
        extra.extend(companion::leg_fee_accounts(&c.mint, &c.quotes));
        companion::with_accounts(
            companion::settle(c.cranker.pubkey(), c.mint, JACKPOT, *who),
            extra,
        )
    };
    let _ = c.send(settle(&c, &whale.pubkey()));
    let tx = c.send(settle(&c, &buyer.pubkey()));
    tx.ok();
    let ev: JackpotUnfunded = tx.event();
    assert_eq!(ev.winner, buyer.pubkey(), "closed at once, unfunded");
}
