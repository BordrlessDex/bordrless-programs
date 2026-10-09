//! Phase 2: the last-buyer jackpot and the diamond-hands streak, run by the companion on Studio's
//! starter hooks (`fixtures/starters`, built on `bordrless-game` only), and the auto-vetting of
//! Studio hooks (`create_game` takes a hook only the protocol's keys can upgrade without a status).
//!
//! Each coin launches through its companion from a plain `LaunchConfig` naming its starter hook,
//! deployed here as Studio deploys it: upgradeable by Studio's key, with no `HookStatus` (so not
//! audited: its pot is capped at 10 SOL).

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

#[test]
fn a_jackpot_runs_end_to_end() {
    let mut c = Coin::new(GameKind::Jackpot);
    let g = c.game();
    assert_eq!(
        (
            g.kind,
            g.timer_secs,
            g.min_tokens,
            g.paid_buys,
            g.round_secs
        ),
        (GameKind::Jackpot, TIMER, studio_jackpot::MIN_TOKENS, 0, 0)
    );
    let co = c.companion();
    assert_eq!(
        (co.game_kind, co.game_hook, co.round_secs),
        (GameKind::Jackpot, JACKPOT, 0)
    );
    assert_eq!(c.jackpot().buys, 0, "no buy counted during the launch");
    fund(&mut c);
    let forfeited = c.jackpot().buys;
    assert!(forfeited > 0, "the volume's buys counted");
    // The volume's last buyer sold everything: once the timer runs out, settle forfeits its round.
    let last = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let tx = c.settle(&last);
    tx.ok();
    let ev: JackpotForfeited = tx.event();
    assert_eq!((ev.round, ev.buyer), (forfeited, last));
    assert_eq!(c.game().paid_buys, forfeited);
    // Nothing more to settle.
    refused(&c.settle(&last), CompanionError::NotDue);

    // A buys, then B: B is the last buyer; A's buy only restarted the timer.
    let a = c.buyer(SOL);
    c.w.env.warp(60);
    let b = c.buyer(SOL / 2);
    let h = c.header();
    assert_eq!(
        (h.last_buyer, h.last_amount),
        (b.pubkey(), c.balance(&b.pubkey()))
    );
    assert_eq!(c.jackpot().buys, forfeited + 2);
    assert_eq!(jackpot_mark(&c.slots(&b.pubkey())), forfeited + 2);
    assert_eq!(jackpot_mark(&c.slots(&a.pubkey())), forfeited + 1);
    // Not before the timer runs out.
    c.warp_to(h.last_buy_at + i64::from(TIMER) - 1);
    refused(&c.settle(&b.pubkey()), CompanionError::NotDue);
    c.w.env.warp(1);
    // The holding passed must be the buyer's.
    refused(&c.settle(&a.pubkey()), CompanionError::WrongHolding);
    let pot = c.companion().pending_pot;
    let before = c.w.env.lamports(&b.pubkey());
    let cranker_before = c.w.env.lamports(&c.cranker.pubkey());
    let tx = c.settle(&b.pubkey());
    tx.ok();
    println!(
        "settle: CU {} size {} height {} trace {} heap {:?}",
        tx.cu(),
        tx.size,
        tx.max_height(),
        tx.trace_len(),
        heap_peak(&tx)
    );
    let paid: JackpotPaid = tx.event();
    let prize = pot * 5_000 / 10_000;
    let bounty = prize * u64::from(BOUNTY_BPS) / 10_000;
    assert_eq!(
        (paid.round, paid.winner, paid.prize, paid.bounty),
        (forfeited + 2, b.pubkey(), prize - bounty, bounty)
    );
    assert_eq!(c.w.env.lamports(&b.pubkey()), before + prize - bounty);
    // The cranker paid the network fee of its own transaction (5,000 lamports) and got the bounty.
    assert_eq!(
        c.w.env.lamports(&c.cranker.pubkey()),
        cranker_before + bounty - 5_000
    );
    let co = c.companion();
    assert_eq!(co.pending_pot, pot - prize);
    let g = c.game();
    assert_eq!(
        (g.paid_buys, g.prizes_paid, g.last_winner, g.settled_at),
        (forfeited + 2, 1, b.pubkey(), c.w.env.now)
    );
    // Settled once: nothing more until a new buy's timer runs out.
    refused(&c.settle(&b.pubkey()), CompanionError::NotDue);

    // A new round: A buys again (A held since its first buy, so its mark stays), then sells one
    // token: forfeited.
    c.buy(&a, SOL).ok();
    assert_eq!(c.header().last_buyer, a.pubkey());
    assert_eq!(jackpot_mark(&c.slots(&a.pubkey())), forfeited + 1);
    let held = c.balance(&a.pubkey());
    c.w.sell(&a, &c.mint.clone(), 1_000_000).ok();
    assert_eq!(jackpot_mark(&c.slots(&a.pubkey())), 0);
    assert_eq!(c.balance(&a.pubkey()), held - 1_000_000);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let pot = c.companion().pending_pot;
    let tx = c.settle(&a.pubkey());
    tx.ok();
    let _: JackpotForfeited = tx.event();
    assert_eq!(c.companion().pending_pot, pot, "the pot stays");
}

#[test]
fn a_buy_after_the_timer_ran_out_never_takes_the_winners_place() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let stale = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&stale).ok();
    // B buys; the timer runs out; before anyone settles, C buys (a bot racing the settle).
    let b = c.buyer(SOL);
    let nb = c.jackpot().buys;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER) + 30);
    let cc = c.buyer(SOL);
    let j = c.jackpot();
    assert_eq!(
        (j.ended_buys, j.ended_buyer, j.buys),
        (nb, b.pubkey(), nb + 1)
    );
    assert_eq!(c.header().last_buyer, cc.pubkey());
    // The ended round is settled first: B is paid, whoever sends it. A settle naming C now is
    // refused (it is not the oldest open round).
    refused(&c.settle(&cc.pubkey()), CompanionError::WrongHolding);
    let tx = c.settle(&b.pubkey());
    tx.ok();
    let paid: JackpotPaid = tx.event();
    assert_eq!((paid.round, paid.winner), (nb, b.pubkey()));
    // C's round is next, once its own timer runs out.
    refused(&c.settle(&cc.pubkey()), CompanionError::NotDue);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let tx = c.settle(&cc.pubkey());
    tx.ok();
    let paid: JackpotPaid = tx.event();
    assert_eq!((paid.round, paid.winner), (nb + 1, cc.pubkey()));
}

#[test]
fn dust_other_owners_and_the_companions_own_buys_never_restart_the_timer() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let a = c.buyer(SOL);
    let j = c.jackpot();
    let h = c.header();
    // A buy below the minimum (dust): no round.
    let dust = c.w.wallet_with_sol(SOL);
    c.w.env.warp(100);
    c.buy(&dust, 1_000_000).ok();
    assert!(c.balance(&dust.pubkey()) > 0);
    assert!(c.balance(&dust.pubkey()) < studio_jackpot::MIN_TOKENS);
    assert_eq!((c.jackpot(), c.header().last_buy_at), (j, h.last_buy_at));
    // A wallet-to-wallet transfer of a lot: no round (not from the pool).
    let whale = c.buyer(5 * SOL);
    let j = c.jackpot();
    let h = c.header();
    let to = Keypair::new().pubkey();
    let amount = c.balance(&whale.pubkey());
    c.transfer(&whale, &to, amount).ok();
    assert_eq!((c.jackpot(), c.header()), (j, h));
    // The companion's own buyback (to its creator address): no round.
    let keys = launch::LaunchKeys::of(&c.w.launch(&c.mint));
    let custom = c.w.custom_hook_accounts(&c.hook, &c.mint);
    let ix = companion::buyback_with(c.cranker.pubkey(), &keys, false, Some(&custom));
    let tx = c.send(ix);
    tx.ok();
    assert_eq!(
        (c.jackpot(), c.header()),
        (j, h),
        "the buyback is no qualifying buy"
    );
    let _ = a;
}

#[test]
fn a_graduated_jackpot_counts_no_buy_and_no_liquidity_removal() {
    let mut c = Coin::new(GameKind::Jackpot);
    let mint = c.mint;
    let (_, tx) = c.w.graduate_launch(&mint);
    tx.ok();
    assert!(!c.w.launch_pool(&mint).curve, "graduated");
    let j = c.jackpot();
    let h = c.header();
    let last = h.last_buyer;
    // The crossing buy (the round under way when it graduated) is still the last one, and can be
    // settled once its timer runs out.
    assert!(j.buys > 0);
    // A buy on the graduated pool: no round.
    let b = c.buyer(5 * SOL);
    assert!(c.balance(&b.pubkey()) > studio_jackpot::MIN_TOKENS);
    assert_eq!((c.jackpot(), c.header()), (j, h));
    // B adds liquidity and takes it out again: the removal is a transfer out of the pool, which
    // no hook can tell from a buy; it counts for nothing.
    let pool = launch::pool_address(&mint, &c.w.sol, policy::LP_FEE_BPS);
    let lp_mint = swap::lp_mint_address(&pool);
    let lkeys = swap::LiquidityKeys {
        provider: b.pubkey(),
        pool,
        base_mint: mint,
        quote_mint: c.w.sol,
        hook_program: Some(bordrless_launch::ID),
    };
    let held = c.balance(&b.pubkey());
    c.w.wrap_sol(&b, 3 * SOL).ok();
    let base_in = c.w.env.token_hook_slice(
        &mint,
        &token::holding_address(&mint, &b.pubkey()),
        &swap::vault_address(&pool, &mint),
        &b.pubkey(),
        &b.pubkey(),
        &pool,
    );
    let mut extras = base_in.clone();
    extras.extend(launch::hook_extras(&mint, &c.w.sol));
    let ixs = [
        token::create_holding(b.pubkey(), lp_mint, b.pubkey()),
        swap::add_liquidity(
            &lkeys,
            AddLiquidityArgs {
                base_desired: held,
                quote_desired: 3 * SOL,
                min_lp: 1,
                base_hook_accounts: base_in.len() as u8,
                quote_hook_accounts: 0,
                hook_data: vec![],
            },
            extras,
        ),
    ];
    c.w.env.send_paid_by(&ixs, &b, &[]).ok();
    let shares = c.w.env.holding(&lp_mint, &b.pubkey());
    assert!(shares > 0);
    let base_out = c.w.env.token_hook_slice(
        &mint,
        &swap::vault_address(&pool, &mint),
        &token::holding_address(&mint, &b.pubkey()),
        &pool,
        &pool,
        &b.pubkey(),
    );
    let mut extras = base_out.clone();
    extras.extend(launch::hook_extras(&mint, &c.w.sol));
    let tx = c.w.env.send_paid_by(
        &[swap::remove_liquidity(
            &lkeys,
            RemoveLiquidityArgs {
                lp_amount: shares,
                min_base: 0,
                min_quote: 0,
                base_hook_accounts: base_out.len() as u8,
                quote_hook_accounts: 0,
                hook_data: vec![],
            },
            extras,
        )],
        &b,
        &[],
    );
    tx.ok();
    assert!(
        c.balance(&b.pubkey()) >= studio_jackpot::MIN_TOKENS,
        "tokens out of the pool"
    );
    assert_eq!(
        (c.jackpot(), c.header()),
        (j, h),
        "the removal is no qualifying buy"
    );
    // The round under way at graduation is settled as any other.
    c.claim_fees().ok();
    c.warp_to(h.last_buy_at + i64::from(TIMER));
    let tx = c.settle(&last);
    tx.ok();
    let paid: JackpotPaid = tx.event();
    assert_eq!(paid.winner, last, "the crossing buyer still holds its buy");
    refused(&c.settle(&last), CompanionError::NotDue);
}

#[test]
fn a_jackpot_waits_for_its_pot_and_never_pays_under_a_block() {
    let mut c = Coin::new(GameKind::Jackpot);
    // B buys before any fee was claimed: the pot is empty when the timer runs out. Settling closes
    // the round at once, paying nothing (a settle never waits, so no later round can take B's place
    // while it waits); the pot stays.
    let b = c.buyer(SOL);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    assert!(c.companion().pending_pot < MIN_POT);
    let tx = c.settle(&b.pubkey());
    tx.ok();
    let unfunded: JackpotUnfunded = tx.event();
    assert_eq!(
        (unfunded.round, unfunded.winner),
        (c.jackpot().buys, b.pubkey())
    );
    assert_eq!(c.game().paid_buys, c.jackpot().buys);
    refused(&c.settle(&b.pubkey()), CompanionError::NotDue);
    // With the pot funded, the next round is paid.
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    let e = c.buyer(SOL);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let older = c.jackpot();
    if older.ended_buys > c.game().paid_buys {
        c.settle(&older.ended_buyer).ok();
    }
    let tx = c.settle(&e.pubkey());
    tx.ok();
    let paid: JackpotPaid = tx.event();
    assert_eq!(paid.winner, e.pubkey());

    // A block: the pot goes to the buyback, nobody is paid, and settle does nothing more.
    let d = c.buyer(SOL);
    c.claim_fees().ok();
    c.set_status(false, DEFAULT_POT_CAP, true).ok();
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let pot = c.companion().pending_pot;
    assert!(pot > 0);
    let before = c.w.env.lamports(&d.pubkey());
    let tx = c.settle(&d.pubkey());
    tx.ok();
    let moved: PotToBuyback = tx.event();
    assert_eq!((moved.lamports, moved.blocked), (pot, true));
    assert_eq!(c.companion().pending_pot, 0);
    assert_eq!(c.w.env.lamports(&d.pubkey()), before);
    refused(&c.settle(&d.pubkey()), CompanionError::NothingToDo);
}

#[test]
fn each_kinds_steps_refuse_the_other_kinds() {
    let mut c = Coin::new(GameKind::Jackpot);
    let ix = companion::close_epoch(c.cranker.pubkey(), c.mint, c.hook, 0);
    refused(&c.send(ix), CompanionError::WrongGameKind);
    let ix = companion::claim_share(c.cranker.pubkey(), c.mint, c.hook, 0, c.cranker.pubkey());
    let tx = c.send(ix);
    tx.expect_fail();
    let ix = companion::reveal(c.cranker.pubkey(), c.mint, c.hook, Pubkey::new_unique());
    refused(&c.send(ix), CompanionError::NotAGame);
    let ix = companion::expire(c.cranker.pubkey(), c.mint, c.hook, Pubkey::new_unique());
    refused(&c.send(ix), CompanionError::NotAGame);
    let mut s = Coin::new(GameKind::Streak);
    let me = s.cranker.pubkey();
    refused(&s.settle(&me), CompanionError::WrongGameKind);
}

// =============================================================================== streak

/// A streak coin with two holders, A (big) and B (small), who bought in the launch's epoch and
/// are entered in the next one; the pot funded during it. Answers the coin, A, B and the epoch.
fn streak_coin() -> (Coin, Keypair, Keypair, u32) {
    let mut c = Coin::new(GameKind::Streak);
    let a = c.buyer(5 * SOL);
    let b = c.buyer(SOL);
    let e = c.epoch() + 1;
    c.warp_into(e, 60);
    c.enter(&[&a.pubkey(), &b.pubkey()]);
    (c, a, b, e)
}

#[test]
fn a_streak_runs_end_to_end() {
    let (mut c, a, b, e) = streak_coin();
    let (wa, wb) = (c.balance(&a.pubkey()), c.balance(&b.pubkey()));
    assert_eq!(c.slots(&a.pubkey()).range_in(e).unwrap().weight, wa);
    assert_eq!(c.header().total, wa + wb);
    // Volume during the epoch: its buyers hold nothing since it began, and sell out.
    c.volume(2, 20 * SOL);
    assert_eq!(c.header().total, wa + wb, "volume adds no weight");
    c.claim_fees().ok();
    let pot = c.companion().pending_pot;
    assert!(pot >= MIN_POT);
    // Not before the epoch is over.
    refused(&c.close_epoch(e), CompanionError::RoundNotOver);
    c.warp_into(e + 1, 30);
    refused(&c.close_epoch(e - 1), CompanionError::RoundNotOver);
    let tx = c.close_epoch(e);
    tx.ok();
    println!(
        "close_epoch: CU {} size {} heap {:?}",
        tx.cu(),
        tx.size,
        heap_peak(&tx)
    );
    let closed: EpochClosed = tx.event();
    assert_eq!(
        (closed.epoch, closed.total, closed.epoch_pot),
        (e, wa + wb, pot)
    );
    let g = c.game();
    assert_eq!(
        (g.status, g.round, g.total, g.prize, g.next_round),
        (DrawStatus::Revealed, e, wa + wb, pot, e + 1)
    );
    assert_eq!(c.companion().pot_locked, pot);
    // Closed once.
    refused(&c.close_epoch(e), CompanionError::DrawPending);

    // Anyone claims for A: A is paid its share, the cranker the bounty and the receipt's rent.
    let a_before = c.w.env.lamports(&a.pubkey());
    let tx = c.claim_share(e, &a.pubkey());
    tx.ok();
    println!(
        "claim_share: CU {} size {} height {} heap {:?}",
        tx.cu(),
        tx.size,
        tx.max_height(),
        heap_peak(&tx)
    );
    let ev: ShareClaimed = tx.event();
    let share_a = (u128::from(pot) * u128::from(wa) / u128::from(wa + wb)) as u64;
    let bounty_a = share_a * u64::from(BOUNTY_BPS) / 10_000;
    assert_eq!(
        (ev.owner, ev.weight, ev.share, ev.bounty),
        (a.pubkey(), wa, share_a - bounty_a, bounty_a)
    );
    assert_eq!(c.w.env.lamports(&a.pubkey()), a_before + share_a - bounty_a);
    let receipt_key = companion::receipt_address(&c.mint, e, &a.pubkey());
    let r: ShareReceipt = c.w.env.read(&receipt_key);
    assert_eq!(
        (r.epoch, r.owner, r.payer, r.amount),
        (e, a.pubkey(), c.cranker.pubkey(), share_a - bounty_a)
    );
    // A second claim for A is refused (the receipt exists).
    c.w.env.warp(1);
    c.claim_share(e, &a.pubkey()).expect_fail();
    // B claims its own.
    let tx = c.claim_share(e, &b.pubkey());
    tx.ok();
    let ev: ShareClaimed = tx.event();
    let share_b = (u128::from(pot) * u128::from(wb) / u128::from(wa + wb)) as u64;
    assert_eq!(ev.share + ev.bounty, share_b);
    let co = c.companion();
    assert_eq!(co.pending_pot, pot - share_a - share_b);
    assert_eq!(
        co.pot_locked,
        pot - share_a - share_b,
        "the dust stays locked"
    );
    assert!(pot - share_a - share_b <= 1, "rounding dust only");

    // The receipt's rent comes back to whoever paid it, once the epoch's claims have ended.
    let ix = companion::close_receipt(c.mint, e, a.pubkey(), c.cranker.pubkey());
    refused(&c.send(ix.clone()), CompanionError::NotDue);
    c.warp_into(e + 2, 0);
    // No claim of epoch e after its claims ended.
    let nobody = Keypair::new().pubkey();
    refused(&c.claim_share(e, &nobody), CompanionError::DrawLate);
    let rent = c.w.env.lamports(&receipt_key);
    let payer_before = c.w.env.lamports(&c.cranker.pubkey());
    c.send(ix).ok();
    assert!(c.w.env.account(&receipt_key).is_none());
    assert_eq!(
        c.w.env.lamports(&c.cranker.pubkey()),
        payer_before + rent - 5_000
    );
    println!("receipt rent: {rent} lamports");
}

#[test]
fn a_send_forfeits_the_share_and_what_is_not_claimed_rolls_over() {
    let (mut c, a, b, e) = streak_coin();
    let wb = c.balance(&b.pubkey());
    // A sends one token to a friend during the epoch: A's weight leaves the total at once.
    let friend = Keypair::new().pubkey();
    c.transfer(&a, &friend, 1).ok();
    assert_eq!(c.header().total, wb);
    assert!(c.slots(&a.pubkey()).range_in(e).is_none());
    // The friend received during the epoch: nothing for it either, even entered.
    c.enter(&[&friend]);
    assert!(c.slots(&friend).range_in(e).is_none());
    // A late buyer (bought during the epoch) gets nothing for it.
    let late = c.buyer(5 * SOL);
    c.enter(&[&late.pubkey()]);
    assert!(c.slots(&late.pubkey()).range_in(e).is_none());
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    c.warp_into(e + 1, 30);
    c.close_epoch(e).ok();
    let g = c.game();
    assert_eq!(g.total, wb);
    refused(&c.claim_share(e, &a.pubkey()), CompanionError::NoShare);
    refused(&c.claim_share(e, &late.pubkey()), CompanionError::NoShare);
    // B sells one token before claiming: its share of e is forfeited too.
    let mint = c.mint;
    c.w.sell(&b, &mint, 1_000_000).ok();
    refused(&c.claim_share(e, &b.pubkey()), CompanionError::NoShare);
    // Nobody is paid; at the next close the whole epoch pot rolls over.
    let locked = c.companion().pot_locked;
    assert_eq!(locked, g.prize);
    // A, B and the late buyer are entered in e + 1. A sent during e, before e + 1 began, and has
    // held since: it qualifies for e + 1 (the streak asked is the epoch's length). B sold during
    // e + 1: nothing for it. The late buyer has held since before e + 1 began.
    c.enter(&[&a.pubkey(), &b.pubkey(), &late.pubkey()]);
    assert!(c.slots(&late.pubkey()).range_in(e + 1).is_some());
    assert!(c.slots(&a.pubkey()).range_in(e + 1).is_some());
    assert!(c.slots(&b.pubkey()).range_in(e + 1).is_none());
    c.warp_into(e + 2, 30);
    let pot = c.companion().pending_pot;
    let tx = c.close_epoch(e + 1);
    tx.ok();
    let ended: EpochEnded = tx.event();
    assert_eq!((ended.epoch, ended.unclaimed), (e, g.prize));
    let closed: EpochClosed = tx.event();
    assert_eq!(
        closed.epoch_pot, pot,
        "everything unclaimed rolled into the next epoch"
    );
    c.claim_share(e + 1, &late.pubkey()).ok();
}

#[test]
fn small_holders_and_the_excluded_get_no_share() {
    let mut c = Coin::new(GameKind::Streak);
    let big = c.buyer(5 * SOL);
    // A holder below the minimum weight.
    let small = c.w.wallet_with_sol(SOL);
    c.buy(&small, 1_000_000).ok();
    assert!(c.balance(&small.pubkey()) < studio_streak::MIN_WEIGHT);
    let e = c.epoch() + 1;
    c.warp_into(e, 60);
    c.enter(&[&big.pubkey(), &small.pubkey()]);
    assert!(c.slots(&small.pubkey()).range_in(e).is_none());
    // The creator address can't be entered (and holds nothing anyway).
    let creator = companion::creator_address(&c.mint);
    let tx = c.send(enter_ix(c.mint, creator));
    tx.expect_fail();
    c.volume(1, 20 * SOL);
    c.claim_fees().ok();
    c.warp_into(e + 1, 30);
    c.close_epoch(e).ok();
    refused(&c.claim_share(e, &small.pubkey()), CompanionError::NoShare);
    c.claim_share(e, &big.pubkey()).ok();
}

#[test]
fn a_lowered_cap_never_trims_an_epochs_shares_and_a_block_ends_them() {
    let (mut c, a, b, e) = streak_coin();
    c.volume(3, 30 * SOL);
    c.claim_fees().ok();
    c.warp_into(e + 1, 30);
    c.close_epoch(e).ok();
    let g = c.game();
    let pot = g.prize;
    assert!(pot > 2 * MIN_POT_CAP);
    // The protocol lowers the cap to its least: the locked epoch pot stays whole.
    c.set_status(false, MIN_POT_CAP, false).ok();
    c.claim_fees().expect_fail();
    let tx = c.claim_share(e, &a.pubkey());
    tx.ok();
    let ev: ShareClaimed = tx.event();
    let wa = c.slots(&a.pubkey()).range_in(e).unwrap().weight;
    assert_eq!(
        ev.share + ev.bounty,
        (u128::from(pot) * u128::from(wa) / u128::from(g.total)) as u64
    );
    // A block: what is left goes to the buyback; B is paid nothing.
    c.set_status(false, MIN_POT_CAP, true).ok();
    let before = c.w.env.lamports(&b.pubkey());
    c.claim_share(e, &b.pubkey()).ok();
    assert_eq!(c.w.env.lamports(&b.pubkey()), before);
    let co = c.companion();
    assert_eq!((co.pending_pot, co.pot_locked), (0, 0));
    assert_eq!(c.game().status, DrawStatus::Idle);
}

#[test]
fn a_late_close_rolls_over_and_retire_waits_for_open_claims() {
    let (mut c, _a, _b, e) = streak_coin();
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    // Too late in e + 1 to leave a whole claim window: it rolls over.
    c.warp_into(e + 2, -(i64::from(CLAIM_WINDOW)) + 1);
    let tx = c.close_epoch(e);
    tx.ok();
    let ev: RolledOver = tx.event();
    assert_eq!((ev.round, ev.reason), (e, RolloverReason::Late));
    assert_eq!(c.game().next_round, e + 1);
    // An epoch with no weight rolls over too.
    c.warp_into(e + 2, 30);
    let tx = c.close_epoch(e + 1);
    tx.ok();
    let ev: RolledOver = tx.event();
    assert_eq!(ev.reason, RolloverReason::NoTickets);
    // A dormant pot: retire waits while an epoch's claims are open, and takes the pot after.
    let (mut c, a, _b, _) = streak_coin();
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    let launched = c.companion().launched_at;
    let retirable = c.game().retirable_at(launched);
    let last = round_of(retirable, EPOCH);
    c.enter(&[&a.pubkey()]);
    c.warp_into(last, 30);
    c.enter(&[&a.pubkey()]);
    c.warp_into(last + 1, 30);
    c.close_epoch(last).ok();
    assert!(c.w.env.now >= retirable);
    let ix = companion::retire_game(c.cranker.pubkey(), c.mint, c.hook);
    refused(&c.send(ix.clone()), CompanionError::DrawPending);
    c.warp_into(last + 2, 0);
    let pot = c.companion().pending_pot;
    let tx = c.send(ix);
    tx.ok();
    let ev: PotRetired = tx.event();
    assert_eq!(ev.lamports, pot);
    let co = c.companion();
    assert_eq!((co.pending_pot, co.pot_locked), (0, 0));
}

// =============================================================================== auto-vetting

/// The setup of a jackpot coin with `hook` and `extra` instructions before `create_game_v2`,
/// whose ProgramData `program_data` names (or none).
fn setup_with(w: &mut World, hook: Pubkey, program_data: bool) -> Tx {
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let (mut args, k) = game_args(GameKind::Jackpot);
    args.hook = hook;
    let mut create = companion::create_game_v2(launcher.pubkey(), mint, args, k);
    if !program_data {
        create.accounts.pop();
    }
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
        prepare_ix(hook, launcher.pubkey(), mint),
        create,
    ];
    w.env.send_paid_by(&ixs, &launcher, &[&mint_kp])
}

#[test]
fn studio_hooks_are_taken_only_when_bordrless_can_upgrade_them() {
    let mut w = world();
    // Studio's key: taken without a status (not audited, capped).
    setup_with(&mut w, JACKPOT, true).ok();
    // The protocol's own key: taken too.
    w.env.set_upgrade_authority(
        JACKPOT,
        Some(bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[1]),
    );
    setup_with(&mut w, JACKPOT, true).ok();
    // Without its ProgramData passed: refused.
    refused(
        &setup_with(&mut w, JACKPOT, false),
        CompanionError::GameHookNotAccepted,
    );
    // Upgradeable by an outsider: refused.
    w.env
        .set_upgrade_authority(JACKPOT, Some(Pubkey::new_unique()));
    refused(
        &setup_with(&mut w, JACKPOT, true),
        CompanionError::GameHookNotAccepted,
    );
    // Immutable and nobody vetted it: refused; with a status from the protocol: taken.
    w.env.set_upgrade_authority(JACKPOT, None);
    refused(
        &setup_with(&mut w, JACKPOT, true),
        CompanionError::GameHookNotAccepted,
    );
    let deployer = w.env.deployer.insecure_clone();
    let status = |blocked| {
        companion::set_hook_status(
            deployer.pubkey(),
            JACKPOT,
            HookStatusArgs {
                audited: false,
                pot_cap: DEFAULT_POT_CAP,
                blocked,
            },
        )
    };
    w.env.send_paid_by(&[status(false)], &deployer, &[]).ok();
    setup_with(&mut w, JACKPOT, true).ok();
    // Blocked: refused, even upgradeable by Studio's key.
    w.env.send_paid_by(&[status(true)], &deployer, &[]).ok();
    w.env.set_upgrade_authority(JACKPOT, Some(STUDIO_KEY));
    refused(
        &setup_with(&mut w, JACKPOT, true),
        CompanionError::GameHookNotAccepted,
    );
}

#[test]
fn another_programs_programdata_or_a_forged_one_vets_nothing() {
    let mut w = world();
    w.env
        .set_upgrade_authority(STREAK, Some(Pubkey::new_unique()));
    // STREAK's settings at JACKPOT's place are refused for the header first; use the streak
    // starter with the jackpot starter's ProgramData (Studio's key) swapped in.
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let (args, k) = game_args(GameKind::Streak);
    let mut create = companion::create_game_v2(launcher.pubkey(), mint, args, k);
    let last = create.accounts.len() - 1;
    create.accounts[last] =
        AccountMeta::new_readonly(companion::hook_program_data_address(&JACKPOT), false);
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
        prepare_ix(STREAK, launcher.pubkey(), mint),
        create,
    ];
    refused(
        &w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]),
        CompanionError::GameHookNotAccepted,
    );
    // A forged ProgramData at the right address but not the loader's: nobody but the loader can
    // own an account there; written here directly, it is refused for its owner.
    let pd = companion::hook_program_data_address(&STREAK);
    let mut account = w.env.account(&pd).unwrap();
    bordrless_program_tests::env::write_programdata_header(&mut account.data, 1, Some(STUDIO_KEY));
    account.owner = Pubkey::new_unique();
    w.env.put(pd, account);
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let (args, k) = game_args(GameKind::Streak);
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
        prepare_ix(STREAK, launcher.pubkey(), mint),
        companion::create_game_v2(launcher.pubkey(), mint, args, k),
    ];
    refused(
        &w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]),
        CompanionError::GameHookNotAccepted,
    );
}

#[test]
fn create_game_v2_checks_each_kinds_settings_against_its_hook() {
    let mut w = world();
    type Tweak = fn(&mut CreateGameArgs, &mut GameKindArgs);
    let try_create = |w: &mut World, kind: GameKind, f: Tweak| {
        let launcher = w.wallet_with_sol(5 * SOL);
        let mint_kp = Keypair::new();
        let mint = mint_kp.pubkey();
        let (mut args, mut k) = game_args(kind);
        f(&mut args, &mut k);
        let ixs = [
            companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
            prepare_ix(args.hook, launcher.pubkey(), mint),
            companion::create_game_v2(launcher.pubkey(), mint, args, k),
        ];
        w.env.send_paid_by(&ixs, &launcher, &[&mint_kp])
    };
    // Out of bounds: BadGame.
    let jackpot_bad: [Tweak; 6] = [
        |_, k| k.timer_secs = 299,
        |_, k| k.min_tokens = 0,
        |a, _| a.round_secs = 3_600,
        |a, _| a.claim_window_secs = 300,
        |a, _| a.max_attempts = 1,
        |_, k| k.min_weight = 1,
    ];
    for f in jackpot_bad {
        refused(
            &try_create(&mut w, GameKind::Jackpot, f),
            CompanionError::BadGame,
        );
    }
    let streak_bad: [Tweak; 5] = [
        |_, k| k.min_weight = 0,
        |_, k| k.timer_secs = 600,
        |a, _| a.max_attempts = 1,
        |a, _| a.claim_window_secs = EPOCH,
        |_, k| k.min_streak_secs = 366 * 86_400,
    ];
    for f in streak_bad {
        refused(
            &try_create(&mut w, GameKind::Streak, f),
            CompanionError::BadGame,
        );
    }
    // Within bounds but not what the hook says: HookState.
    refused(
        &try_create(&mut w, GameKind::Jackpot, |_, k| k.timer_secs = 601),
        CompanionError::HookState,
    );
    refused(
        &try_create(&mut w, GameKind::Jackpot, |_, k| k.min_tokens += 1),
        CompanionError::HookState,
    );
    refused(
        &try_create(&mut w, GameKind::Streak, |_, k| k.min_weight += 1),
        CompanionError::HookState,
    );
    refused(
        &try_create(&mut w, GameKind::Streak, |a, _| a.round_secs = EPOCH / 7),
        CompanionError::HookState,
    );
    // The other kind's hook: its header is not this kind's.
    refused(
        &try_create(&mut w, GameKind::Streak, |a, _| a.hook = JACKPOT),
        CompanionError::HookState,
    );
    // The phase-1 `create_game` still takes only a lottery.
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let (args, _) = game_args(GameKind::Jackpot);
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
        prepare_ix(JACKPOT, launcher.pubkey(), mint),
        companion::create_game_with_program_data(launcher.pubkey(), mint, args),
    ];
    refused(
        &w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]),
        CompanionError::BadGame,
    );
}

#[test]
fn a_game_launch_needs_its_kinds_flags() {
    let mut w = world();
    let launcher = w.wallet_with_sol(50 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    w.env
        .send_paid_by(
            &setup_ixs(&launcher.pubkey(), &mint, GameKind::Streak),
            &launcher,
            &[&mint_kp],
        )
        .ok();
    // Taking deltas (which could skim the companion's buybacks), or missing the burns' callback:
    // refused at the launch.
    use bordrless_hook::token_flags as f;
    for flags in [
        GameKind::Streak.hook_flags() | f::TRANSFER_RETURNS_DELTA,
        GameKind::Streak.hook_flags() & !f::BEFORE_BURN,
    ] {
        let wrong = hook_config(&mut w, &launcher, STREAK, flags);
        let ix = launch_ix(&w, &launcher.pubkey(), &mint, &wrong, "https://x.y/z");
        refused(
            &w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]),
            CompanionError::GameHookMismatch,
        );
    }
    // With the kind's flags, it launches.
    let right = hook_config(&mut w, &launcher, STREAK, GameKind::Streak.hook_flags());
    let ix = launch_ix(&w, &launcher.pubkey(), &mint, &right, "https://x.y/z");
    w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]).ok();
}

// =============================================================================== limits

/// The setup, the launch and every new step fit mainnet's limits as v0 transactions with the
/// protocol's 22-address lookup table, at the site's longest metadata.
#[test]
fn the_new_games_fit_mainnet_limits() {
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
        println!(
            "{kind:?} setup (create, prepare, create_game_v2) v0: {} bytes, {} CU",
            tx.size,
            tx.cu()
        );
        assert!(tx.size <= PACKET);
        let hook = game_args(kind).0.hook;
        let config = hook_config(&mut w, &launcher, hook, kind.hook_flags());
        let uri = format!("https://gateway.pinata.cloud/ipfs/{}", "b".repeat(94));
        let ixs = [
            compute_unit_limit(1_400_000),
            compute_unit_price(20_000),
            launch_ix(&w, &launcher.pubkey(), &mint, &config, &uri),
        ];
        let tx = w
            .env
            .send_v0(&ixs, &launcher, &[&mint_kp], std::slice::from_ref(&table));
        tx.ok();
        println!(
            "{kind:?} launch v0: {} bytes, {} trace, height {}, {} CU",
            tx.size,
            tx.trace_len(),
            tx.max_height(),
            tx.cu()
        );
        assert!(tx.size <= PACKET && tx.max_height() <= 5 && tx.trace_len() <= 64);
        let custom = CustomHookAccounts {
            program: hook,
            extras: w.custom_hook_accounts(&hook, &mint).extras,
        };
        assert_eq!(custom.extras.len(), 2, "state and launch");
        // The steps.
        let cranker = w.wallet_with_sol(SOL);
        let buyer = Keypair::new().pubkey();
        let step_ixs: Vec<(&str, Instruction)> = match kind {
            GameKind::Jackpot => vec![(
                "settle",
                companion::settle(cranker.pubkey(), mint, hook, buyer),
            )],
            _ => vec![
                (
                    "close_epoch",
                    companion::close_epoch(cranker.pubkey(), mint, hook, 1),
                ),
                (
                    "claim_share",
                    companion::claim_share(cranker.pubkey(), mint, hook, 1, buyer),
                ),
                (
                    "close_receipt",
                    companion::close_receipt(mint, 1, buyer, cranker.pubkey()),
                ),
                ("enter", enter_ix(mint, buyer)),
            ],
        };
        for (name, ix) in step_ixs {
            let size = w.env.v0_size(
                &[compute_unit_price(20_000), ix],
                &cranker,
                &[],
                std::slice::from_ref(&table),
            );
            println!("{kind:?} {name} v0: {size} bytes");
            assert!(size <= PACKET);
        }
        // The dev buy through the hook.
        let _ = &launcher;
    }
}

/// The jackpot and streak hooks' callbacks, measured on a buy and a send: compute well within
/// what a launch and a trade leave them.
#[test]
fn the_starters_callbacks_are_cheap() {
    for kind in [GameKind::Jackpot, GameKind::Streak] {
        let mut c = Coin::new(kind);
        let a = c.w.wallet_with_sol(2 * SOL);
        let tx = c.buy(&a, SOL);
        tx.ok();
        println!(
            "{kind:?} buy through the hook: {} CU, height {}",
            tx.cu(),
            tx.max_height()
        );
        let to = Keypair::new().pubkey();
        let tx = c.transfer(&a, &to, 1_000);
        tx.ok();
        println!("{kind:?} transfer through the hook: {} CU", tx.cu());
        assert!(tx.cu() < 100_000);
        if kind == GameKind::Streak {
            let e = c.epoch() + 1;
            c.warp_into(e, 5);
            let tx = c.send(enter_ix(c.mint, to));
            tx.ok();
            println!(
                "streak enter: {} CU, {} bytes, height {}",
                tx.cu(),
                tx.size,
                tx.max_height()
            );
            let s = StreakHeader::parse(&c.state_data()).unwrap();
            assert_eq!(s.min_weight, studio_streak::MIN_WEIGHT);
        }
    }
}

// =============================================================================== audit fixes (custody r1)

/// F1: a cap the protocol lowered while an epoch's pot was locked applies to the whole pot at the
/// next close, before the next epoch's pot is fixed: no epoch is ever locked above the cap.
#[test]
fn a_cap_lowered_under_a_lock_applies_at_the_next_close() {
    let (mut c, a, b, e) = streak_coin();
    c.volume(3, 30 * SOL);
    c.claim_fees().ok();
    c.warp_into(e + 1, 30);
    c.close_epoch(e).ok();
    assert!(c.game().prize > 2 * MIN_POT_CAP);
    c.set_status(false, MIN_POT_CAP, false).ok();
    // Nobody claims; the holders are entered for e + 1, which closes after e's claims end.
    c.enter(&[&a.pubkey(), &b.pubkey()]);
    c.warp_into(e + 2, 30);
    let tx = c.close_epoch(e + 1);
    tx.ok();
    let closed: EpochClosed = tx.event();
    assert!(closed.epoch_pot <= MIN_POT_CAP, "{}", closed.epoch_pot);
    let co = c.companion();
    assert!(co.pending_pot <= MIN_POT_CAP && co.pot_locked <= MIN_POT_CAP);
}

/// F2: `claim_share` refuses any epoch but the one open for claims before anything else, so a
/// block's early return never leaves a receipt for another epoch behind.
#[test]
fn no_receipt_for_another_epoch_even_under_a_block() {
    let (mut c, a, _b, e) = streak_coin();
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    c.set_status(false, DEFAULT_POT_CAP, true).ok();
    refused(&c.claim_share(e + 5, &a.pubkey()), CompanionError::NoDraw);
    assert!(c
        .w
        .env
        .account(&companion::receipt_address(&c.mint, e + 5, &a.pubkey()))
        .is_none());
}

/// F3 (jackpot): `retire` waits while a round is over and the pot can pay it; it needs the hook's
/// state (without it: refused), and once the round is settled (here forfeited) it goes on.
#[test]
fn retire_waits_for_a_jackpot_round_the_pot_can_pay() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let launched = c.companion().launched_at;
    let retirable = c.game().retirable_at(launched);
    c.warp_to(retirable - i64::from(TIMER) - 10);
    let b = c.buyer(SOL);
    c.claim_fees().ok();
    c.warp_to(retirable + 1);
    let plain = companion::retire(c.cranker.pubkey(), c.mint, c.hook);
    refused(&c.send(plain), CompanionError::MissingAccount);
    let ix = companion::retire_game(c.cranker.pubkey(), c.mint, c.hook);
    refused(&c.send(ix.clone()), CompanionError::DrawPending);
    // B sells one token: its round is forfeited; nothing is due, and the pot is retired.
    let mint = c.mint;
    c.w.sell(&b, &mint, 1_000_000).ok();
    let older = c.jackpot().ended_buyer;
    if older != Pubkey::default() && c.game().paid_buys < c.jackpot().ended_buys {
        c.settle(&older).ok();
    }
    let _: JackpotForfeited = c.settle(&b.pubkey()).event();
    let pot = c.companion().pending_pot;
    let ev: PotRetired = c.send(ix).event();
    assert_eq!(ev.lamports, pot);
}

/// F3 (streak): `retire` waits while the epoch that just ended has weight and can be closed.
#[test]
fn retire_waits_for_a_streak_epoch_that_can_be_closed() {
    let (mut c, a, _b, _) = streak_coin();
    c.volume(2, 20 * SOL);
    c.claim_fees().ok();
    let launched = c.companion().launched_at;
    let retirable = c.game().retirable_at(launched);
    let last = round_of(retirable, EPOCH);
    c.warp_into(last, 30);
    c.enter(&[&a.pubkey()]);
    c.warp_into(last + 1, 30);
    assert!(c.w.env.now >= retirable);
    let ix = companion::retire_game(c.cranker.pubkey(), c.mint, c.hook);
    refused(&c.send(ix.clone()), CompanionError::DrawPending);
    // Too late to close it (the last claim window): nothing is due, retire goes on.
    c.warp_into(last + 2, -(i64::from(CLAIM_WINDOW)) + 1);
    let _: PotRetired = c.send(ix).event();
}

/// F4: a lottery never runs on a jackpot's or a streak's hook (a streak's weights all start at
/// ticket 0).
#[test]
fn a_lottery_refuses_a_kind_hook() {
    let mut w = world();
    for hook in [STREAK, JACKPOT] {
        let launcher = w.wallet_with_sol(5 * SOL);
        let mint_kp = Keypair::new();
        let mint = mint_kp.pubkey();
        let round_secs = if hook == STREAK { EPOCH } else { 0 };
        let args = CreateGameArgs {
            kind: GameKind::Lottery,
            hook,
            split: SPLIT,
            pot_bps: POT_BPS,
            round_secs: round_secs.max(3_600),
            min_pot: MIN_POT,
            prize_bps: 10_000,
            claim_window_secs: 300,
            max_attempts: 6,
        };
        let ixs = [
            companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
            prepare_ix(hook, launcher.pubkey(), mint),
            companion::create_game_with_program_data(launcher.pubkey(), mint, args),
        ];
        refused(
            &w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]),
            CompanionError::HookState,
        );
    }
}

// =============================================================================== audit fixes (game r1)

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
/// can't be paid) is forfeited, so it never jams the rounds after it.
#[test]
fn a_round_won_by_a_program_address_is_forfeited_not_jammed() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let stale = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&stale).ok();
    let attacker = c.w.wallet_with_sol(2 * SOL);
    let program = half_life::ID;
    assert!(program.is_on_curve());
    buy_for(&mut c, &attacker, &program, SOL / 2).ok();
    assert_eq!(c.header().last_buyer, program);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let tx = c.settle(&program);
    tx.ok();
    let forfeited: JackpotForfeited = tx.event();
    assert_eq!(forfeited.buyer, program);
    // The next round is paid as usual.
    let b = c.buyer(SOL);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    let paid: JackpotPaid = c.settle(&b.pubkey()).event();
    assert_eq!(paid.winner, b.pubkey());
}

/// Game F2: a round waiting for its pot can't be overtaken: it is closed as soon as it is settled
/// (paid nothing below the minimum), so the round after it is a round of its own.
#[test]
fn a_waiting_round_is_never_overtaken() {
    let mut c = Coin::new(GameKind::Jackpot);
    let b = c.buyer(SOL);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    // C buys after B's timer, holds through its own, and buys again: B's round was settled first
    // (unfunded), so C's first round is the ended one, not B's in disguise.
    c.settle(&b.pubkey()).ok();
    let cc = c.buyer(SOL);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.buy(&cc, SOL / 10).ok();
    let j = c.jackpot();
    assert_eq!(j.ended_buyer, cc.pubkey());
    assert!(c.game().paid_buys < j.ended_buys);
}

// =============================================================================== audit fixes (round 2)

/// R2-1: the companion's list of the runtime's reserved keys is agave's, exactly.
#[test]
fn the_reserved_keys_are_the_runtimes() {
    let mut ours: Vec<[u8; 32]> = RESERVED_KEYS.iter().map(|k| k.to_bytes()).collect();
    let mut theirs: Vec<[u8; 32]> =
        agave_reserved_account_keys::ReservedAccountKeys::all_keys_iter()
            .map(|k| k.to_bytes())
            .collect();
    ours.sort();
    theirs.sort();
    assert_eq!(ours, theirs);
}

/// R2-1: a round won by a reserved key (a sysvar on the curve, read-only in every transaction) is
/// forfeited, so it never jams `settle` nor holds `retire`.
#[test]
fn a_round_won_by_a_sysvar_is_forfeited() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let stale = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&stale).ok();
    let attacker = c.w.wallet_with_sol(2 * SOL);
    let sysvars = RESERVED_KEYS
        .iter()
        .filter(|k| k.is_on_curve())
        .copied()
        .collect::<Vec<_>>();
    assert!(
        sysvars.len() >= 5,
        "several reserved keys look like wallets"
    );
    for key in sysvars.iter().take(3) {
        buy_for(&mut c, &attacker, key, SOL / 4).ok();
        assert_eq!(c.header().last_buyer, *key);
        c.warp_to(c.header().last_buy_at + i64::from(TIMER));
        let f: JackpotForfeited = c.settle(key).event();
        assert_eq!(f.buyer, *key);
    }
}

/// R2-1 (liveness): a round left unsettled `SETTLE_GRACE_SECS` after its timer is forfeited by the
/// next settle, whoever its buyer.
#[test]
fn a_stale_round_is_forfeited() {
    let mut c = Coin::new(GameKind::Jackpot);
    fund(&mut c);
    let stale = c.header().last_buyer;
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    c.settle(&stale).ok();
    let b = c.buyer(SOL);
    c.claim_fees().ok();
    c.warp_to(c.header().last_buy_at + i64::from(TIMER) + SETTLE_GRACE_SECS);
    let f: JackpotForfeited = c.settle(&b.pubkey()).event();
    assert_eq!(f.buyer, b.pubkey());
}

/// R2-2: a prize the launch's unclaimed fees could fund is not voided by a bare settle: it is
/// refused until the fees are claimed, and claiming them in the same transaction pays it.
#[test]
fn unclaimed_fees_that_could_fund_a_prize_are_claimed_first() {
    let mut c = Coin::new(GameKind::Jackpot);
    // Volume, not claimed: the launch holds the fees, the pot is empty.
    c.volume(2, 20 * SOL);
    let b = c.buyer(SOL);
    c.warp_to(c.header().last_buy_at + i64::from(TIMER));
    assert!(c.companion().pending_pot < MIN_POT);
    // The volume's rounds come first: they forfeit (their buyers sold), never needing the pot.
    for _ in 0..4 {
        let j = c.jackpot();
        let h = c.header();
        let g = c.game();
        let Some(r) = bordrless_game::settle_round(&h, &j, g.paid_buys, g.timer_secs, c.w.env.now)
        else {
            break;
        };
        if r.buyer == b.pubkey() {
            break;
        }
        c.settle(&r.buyer).ok();
    }
    refused(&c.settle(&b.pubkey()), CompanionError::FeesUnclaimed);
    let claim = companion::claim_fees_game(c.cranker.pubkey(), c.mint, c.hook);
    let settle = companion::settle(c.cranker.pubkey(), c.mint, c.hook, b.pubkey());
    let cranker = c.cranker.insecure_clone();
    let table =
        c.w.env
            .put_lookup_table(Pubkey::new_unique(), &table_22(&c.w));
    let tx = c.w.env.send_v0(
        &[compute_unit_limit(400_000), claim, settle],
        &cranker,
        &[],
        std::slice::from_ref(&table),
    );
    tx.ok();
    println!("claim_fees + settle v0: {} bytes, {} CU", tx.size, tx.cu());
    assert!(tx.size <= PACKET);
    let paid: JackpotPaid = tx.event();
    assert_eq!(paid.winner, b.pubkey());
}
