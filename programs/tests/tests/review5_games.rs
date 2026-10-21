//! Games on coins quoted in tokens (`docs/token-quotes.md` §7.6): a lottery, a jackpot and a streak
//! whose pots are kept per leg in the legs' tokens (`CompanionLegs`), their prizes paid in each leg's
//! token, their thresholds in basis points of the coin's opening value, an unaudited hook's cap and
//! a block applied per leg (a blocked pot bought back as the coin, never burned as tokens), launched
//! through the companion's `launch_token` from a config naming the game's hook, whose registry lists
//! the coin's `Legs` (`lottery_hook::prepare_legs`, the starters' `prepare`).
//!
//! Review 5's regressions (the adversarial review of §7.6), as the reviewer wrote them: they keep
//! their suite's helpers whole, some unused here.

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
        let t = self.trader();
        for i in 0..self.quotes.len() {
            for _ in 0..4 {
                let vq = self.w.legs(&self.mint).legs[i].virtual_quote;
                self.buy(&t, i, vq / 8).ok();
                let coins = self.w.env.holding(&self.mint, &t.pubkey());
                self.sell(&t, i, coins).ok();
            }
        }
        self.claim_waiting();
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

// ---- review5 (adversarial review of §7.6) -------------------------------------------------------

use bordrless_program_tests::launch::VQ;

/// `extra` with `owner`'s account of leg `i`'s token made read-only (it stays in the list, so the
/// claim's "every leg's account must be passed" holds).
fn read_only(
    c: &Coin,
    mut extra: Vec<AccountMeta>,
    owner: &Pubkey,
    legs: &[usize],
) -> Vec<AccountMeta> {
    for i in legs {
        let key = c.keys(*i).quote_account(owner);
        for m in extra.iter_mut().filter(|m| m.pubkey == key) {
            m.is_writable = false;
        }
    }
    extra
}

/// F2 (lottery): a sender passes the winner's account of one leg's token read-only. `receivable`
/// reads that as "can't take its token now" and the leg is skipped; the draw is still closed
/// (`Idle`), so the winner can never claim that leg. The leg's prize stays in the pot for later
/// draws (which the sender may win).
#[test]
fn review5_a_read_only_account_forfeits_a_leg_of_the_lottery_prize() {
    let mut c = Coin::new(GameKind::Lottery, &THREE);
    let mint = c.mint;
    let a = c.trader();
    let vq0 = c.w.legs(&mint).legs[0].virtual_quote;
    c.buy(&a, 0, vq0 / 50).ok();
    let r0 = c.round() + 1;
    c.warp_into(r0, 5);
    c.enter(&[&a.pubkey()]);
    c.fund();
    c.warp_into(r0 + 1, 5);
    let tx = c.draw(r0);
    tx.ok();
    let fixed: LegPrizesFixed = tx.event();
    let total = c.game().total;
    let ra = c.w.env.hook_data(&mint, &a.pubkey());
    let range = bordrless_game::Slots::decode(&ra).range_in(r0).unwrap();
    let randomness = randomness_where(total, |t| range.contains(t));
    let seed = c.game().seed;
    orao::fulfil(&mut c.w.env, &seed, &randomness);
    c.reveal().ok();
    let before: Vec<u64> = (0..3).map(|i| c.quote_balance(&a.pubkey(), i)).collect();
    // Anyone sends the claim, a's account of leg 2's token read-only.
    let extra = read_only(&c, c.prize_accounts(&a.pubkey(), false), &a.pubkey(), &[2]);
    let ix = companion::with_accounts(
        companion::claim_prize(c.cranker.pubkey(), mint, c.hook, 0, a.pubkey()),
        extra,
    );
    let tx = c.send(ix);
    // EXPECTED (fixed): the winner's account of each paying leg must be present AND writable; a
    // sender passing it read-only is malformed and must be refused, never let a leg be silently
    // skipped while the draw closes. FAILS on the build under review (the claim succeeds, leg 2 is
    // skipped and the winner can never claim it). `receivable` must treat a sender-chosen
    // read-only account as an error, distinct from the owner's own frozen/memo account.
    assert!(
        tx.result.is_err(),
        "a prize claim with the winner's leg-2 account read-only must be refused; it succeeded, \
         paid {} of 3 legs, winner balance of leg 2 unchanged ({} vs {}), draw now {:?}",
        tx.events::<LegPrizePaid>().len(),
        c.quote_balance(&a.pubkey(), 2),
        before[2],
        c.game().status
    );
}

/// F2 (jackpot): every one of the winner's accounts passed read-only makes a funded round pay
/// nothing; `settle` closes it as unfunded (`JackpotUnfunded`) and the next round takes the pot.
#[test]
fn review5_read_only_accounts_forfeit_a_funded_jackpot_round() {
    let mut c = Coin::new(GameKind::Jackpot, &THREE);
    let mint = c.mint;
    let buyer = c.trader();
    let vq = c.w.legs(&mint).legs[2].virtual_quote;
    c.buy(&buyer, 2, vq / 20).ok();
    c.w.env.warp(i64::from(studio_jackpot::TIMER_SECS) + 1);
    c.claim_waiting();
    c.fund();
    assert!(c.pot_bps() >= u64::from(MIN_POT_BPS), "funded");
    let pots: Vec<u64> = c.legs().legs.iter().map(|p| p.pending_pot).collect();
    let before: Vec<u64> = (0..3)
        .map(|i| c.quote_balance(&buyer.pubkey(), i))
        .collect();
    let mut extra = read_only(
        &c,
        c.prize_accounts(&buyer.pubkey(), false),
        &buyer.pubkey(),
        &[0, 1, 2],
    );
    extra.extend(companion::leg_fee_accounts(&c.mint, &c.quotes));
    let ix = companion::with_accounts(
        companion::settle(c.cranker.pubkey(), mint, JACKPOT, buyer.pubkey()),
        extra,
    );
    let _ = &pots;
    let tx = c.send(ix);
    // EXPECTED (fixed): a settle whose winner ATAs are all passed read-only is malformed and must
    // be refused, never close a funded round as unfunded and forfeit a legitimate winner. FAILS on
    // the build under review (settle succeeds, emits JackpotUnfunded, the round is closed and the
    // buyer is paid nothing of a funded pot).
    assert!(
        tx.result.is_err(),
        "a funded settle with read-only winner accounts must be refused; it succeeded, winner {} \
         balances {:?} unchanged from {:?}",
        tx.events::<JackpotUnfunded>().first().map(|e| e.winner) == Some(buyer.pubkey()),
        (0..3)
            .map(|i| c.quote_balance(&buyer.pubkey(), i))
            .collect::<Vec<_>>(),
        before
    );
}

/// F2 (streak): a claim with one leg's account read-only pays the others and makes the receipt;
/// the owner's share of that leg is gone.
#[test]
fn review5_a_read_only_account_forfeits_a_leg_of_a_streak_share() {
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
    let before = c.quote_balance(&a.pubkey(), 1);
    let extra = read_only(&c, c.prize_accounts(&a.pubkey(), false), &a.pubkey(), &[1]);
    let ix = companion::claim_share(c.cranker.pubkey(), mint, STREAK, next, a.pubkey());
    let tx = c.send(companion::with_accounts(ix, extra));
    // EXPECTED (fixed): a claim_share whose owner leg account is passed read-only must be refused,
    // never skip that leg while making the one-shot receipt (which bars a correct re-claim). FAILS
    // on the build under review (claim succeeds, leg 1 skipped, receipt made, share lost).
    assert!(
        tx.result.is_err(),
        "a claim_share with a read-only leg account must be refused; it succeeded, paid {} legs, \
         leg-1 balance {} unchanged from {}",
        tx.events::<LegPrizePaid>().len(),
        c.quote_balance(&a.pubkey(), 1),
        before
    );
}

/// F3: fees claimed into the creator address's accounts by the launchpad's own (permissionless)
/// `claim_creator_fees_token` sit there unsplit; `settle` counts neither them nor anything but the
/// leg-fee accounts, so a round a `claim_fees_leg` would fund is closed unfunded. (On a SOL coin
/// `fundable` counts the creator's surplus above what is set aside.)
#[test]
fn review5_fees_claimed_raw_let_a_fundable_jackpot_round_close_unfunded() {
    for raw in [true, false] {
        let mut c = Coin::new(GameKind::Jackpot, &THREE);
        let mint = c.mint;
        let whale = c.trader();
        let buyer = c.trader();
        let vq = c.w.legs(&mint).legs[2].virtual_quote;
        // The whale's round, then the buyer's; the whale sells everything (fees, no new round).
        c.buy(&whale, 2, 2 * vq).ok();
        c.buy(&buyer, 2, vq / 20).ok();
        let coins = c.w.env.holding(&mint, &whale.pubkey());
        c.sell(&whale, 2, coins).ok();
        c.w.env.warp(i64::from(studio_jackpot::TIMER_SECS) + 1);
        let settle = |c: &Coin, who: &Pubkey| {
            let mut extra = c.prize_accounts(who, false);
            extra.extend(companion::leg_fee_accounts(&c.mint, &c.quotes));
            companion::with_accounts(
                companion::settle(c.cranker.pubkey(), c.mint, JACKPOT, *who),
                extra,
            )
        };
        // The whale's round, when still open: forfeited (it sold).
        let state =
            c.w.env
                .account(&bordrless_game::state_address(&JACKPOT, &mint).0)
                .unwrap();
        let j = JackpotHeader::parse(&state.data).unwrap();
        let h = GameHeader::read(&state.data, &mint).unwrap();
        println!(
            "buys {} last_buyer is buyer {} ended_buyer is whale {} ended_buys {}",
            j.buys,
            h.last_buyer == buyer.pubkey(),
            j.ended_buyer == whale.pubkey(),
            j.ended_buys
        );
        let tx = c.send(settle(&c, &whale.pubkey()));
        println!("whale settle ok: {}", tx.result.is_ok());
        if raw {
            // Anyone claims every leg's fee with the launchpad's own claim.
            let payer = c.cranker.pubkey();
            let creator = companion::creator_address(&mint);
            for i in 0..3 {
                let keys = c.keys(i);
                let ix = launch::claim_creator_fees_token(payer, creator, &keys, i as u8);
                // Legs with no fees answer NothingToClaim.
                let _ = c.send(ix);
            }
        } else {
            c.claim_waiting();
        }
        let before = c.quote_balance(&buyer.pubkey(), 2);
        println!("raw {raw}: pot_bps {}", c.pot_bps());
        let ix = settle(&c, &buyer.pubkey());
        let tx = c.send(ix);
        if raw {
            // EXPECTED (fixed): fees already claimed into the creator address's token accounts
            // (unsplit) still fund the pot (a `claim_fees_leg` would book them, as the SOL path's
            // `fundable` counts the creator's surplus). A round that such a claim would fund must
            // not close unfunded: settle must refuse (FeesUnclaimed) or pay. FAILS on the build
            // under review (settle emits JackpotUnfunded and forfeits the winner, though the same
            // fees sitting unsplit would fund the pot via claim_fees_leg).
            c.claim_leg(2).ok();
            let fundable = c.pot_bps() >= u64::from(MIN_POT_BPS);
            assert!(
                !(tx.result.is_ok()
                    && tx.events::<JackpotUnfunded>().first().map(|e| e.winner)
                        == Some(buyer.pubkey())
                    && fundable),
                "a round a claim_fees_leg would fund closed unfunded (pot now {} bps)",
                c.pot_bps()
            );
        } else {
            tx.ok();
            let paid: Vec<LegPrizePaid> = tx.events();
            assert!(
                !paid.is_empty(),
                "control: claimed through the companion, the round pays"
            );
            assert!(c.quote_balance(&buyer.pubkey(), 2) > before);
        }
    }
}

/// F4: `set_game_token_terms` is accepted for any game before its launch, and the SOL `launch`
/// never looks at it. A SOL coin's game with token terms runs every step down the token branch,
/// which needs a `CompanionLegs` no SOL coin has: `draw` and `retire` fail for ever while
/// `claim_fees` keeps filling the SOL pot.
#[test]
fn review5_token_terms_on_a_sol_coins_game_lock_its_pot() {
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
    let tx = w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]);
    // EXPECTED (fixed): the SOL (non-token-quoted) `launch` must reject a game carrying token terms
    // (`set_game_token_terms`); a game in token mode belongs to `launch_token`. FAILS on the build
    // under review: the SOL launch succeeds with the game stuck in token mode, so every game step
    // (draw, retire, …) takes the token branch and fails `LegAccountsMissing` for ever — there is
    // no `CompanionLegs` on a SOL coin — while `claim_fees` keeps filling the SOL pot, locking it.
    if tx.result.is_ok() {
        let g: Game = w.env.read(&companion::game_address(&mint));
        assert!(
            !g.in_tokens(),
            "a SOL launch accepted a game in token mode: its SOL pot can never be drawn, retired \
             or burned (every step hits LegAccountsMissing)"
        );
    }
}
