//! Phase 3a: strategy games (`docs/phase3a.md` §4). A builder's program decides a coin's payouts;
//! the companion keeps the pot, asks, bounds every answer and pays.
//!
//! - `create_strategy_game(args, s)`, before the launch and signed by the mint as `create_game` is:
//!   a game of kind `Strategy`, tickets from a lottery-format hook (Bordrless's `lottery_hook`, or
//!   one vetted as any game hook), its `StrategyTerms` (`PDA(["strategy", mint])`): the strategy
//!   program (immutable, timelocked, Bordrless-managed or given a status; never blocked), its
//!   budget, share, batch and compute bounds, and the extra accounts its registry names (at most
//!   2, each owned by the strategy, resolved once here).
//! - `plan_period(period)` (anyone, once period `period` is over): the period paid last releases
//!   what it did not pay; under the stricter of the two statuses the pot is trimmed (or, blocked,
//!   moved to the buyback and nothing else happens); then the strategy's `plan` is asked, every
//!   account read-only and no signer, its answer read from its own return data (8 bytes), its
//!   compute measured. A refused answer closes the period with nothing (`PeriodRejected`), 0 skips
//!   it (`PeriodSkipped`); otherwise the budget is locked for the period's holders until
//!   `claims_end(period)` (`PeriodPlanned`). A period never planned in time lapses (`Late`), the pot
//!   keeping its funds.
//! - `pay_strategy(period, n)` (anyone, while the period is open): `n` candidates `[holding, owner,
//!   receipt]`, each checked (the mint's holding at its owner's address, an eligible wallet, its
//!   weight for the period between `min_weight` and its balance, no receipt yet), asked `entitle`
//!   one at a time, its answer bounded (at most `max_share_bps` of the budget and what is left of it;
//!   never clamped), given a receipt (`ShareReceipt`, `PDA(["claimed", game, period, owner])`, the
//!   sender paying its rent) and paid as SOL less the sender's bounty, all in one unwrap. A
//!   candidate refused is skipped (`CandidateRejected`), the others go on.
//!
//! A strategy that fails (panics, errors, runs out of compute) makes the transaction fail: the
//! runtime can't catch a CPI's error. Nothing moves; the period lapses at its deadline if it never
//! succeeds, and after `RETIRE_DORMANT_PERIODS` dormant periods anyone retires the pot.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::{get_return_data, invoke, set_return_data};
use anchor_lang::system_program;
use bordrless_game::{eligible, read_state_at, round_end, round_of, round_start, state_address};
use bordrless_hook::authority::{
    classify, code_of, parse_programdata, programdata_address, timelock_address, AuthorityClass,
    BPF_LOADER_UPGRADEABLE_ID, LOADER_V4_ID,
};
use bordrless_hook::{AccountSource, Seed};
use bordrless_launch::client as launch_client;
use bordrless_launch::state::Launch;
use bordrless_strategy::{
    registry_address, weight_in, EntitleArgs, PlanArgs, ARGS_VERSION, ENTITLE, PLAN,
};
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::error::CompanionError;
use crate::events::*;
use crate::instructions::game::{
    check_hook_registry, check_kind_header, emit_event, rolled_over, vet_game_hook, CreateGameArgs,
    GameKindArgs,
};
use crate::instructions::kinds::end_epoch_if_over;
use crate::instructions::steps::{
    available, decode_registry, enforce_terms, find, game_hook_terms, read_hook_terms,
};
use crate::instructions::CreatorSeeds;
use crate::state::*;

/// The settings of a strategy game, beside `CreateGameArgs`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct StrategyArgs {
    /// The strategy program.
    pub strategy: Pubkey,
    /// The most of the unlocked pot a period may pay (1 to 5,000 basis points).
    pub budget_bps: u16,
    /// The most one holder gets of a period's budget (1 to 2,500 basis points).
    pub max_share_bps: u16,
    /// The most candidates one payment takes (1 to 4).
    pub max_per_tx: u8,
    /// The most compute `plan` (to 150,000) and `entitle` (to 60,000) may use.
    pub plan_cu_max: u32,
    pub entitle_cu_max: u32,
    /// The least weight that may be paid (at least 1).
    pub min_weight: u64,
}

// ---- create_strategy_game ----------------------------------------------------------------------

/// Accounts of `create_strategy_game`. Remaining, looked up by address: the ticket hook's vetting
/// accounts when it has no status (its ProgramData, its attestation, its `Timelock`), the
/// strategy's ProgramData and `Timelock` (when it has no status), and the accounts its registry
/// names.
#[event_cpi]
#[derive(Accounts)]
#[instruction(args: CreateGameArgs, s: StrategyArgs)]
pub struct CreateStrategyGame<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// The launch's mint (not created yet): it signs, as for `create`.
    pub mint: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, mint.key().as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    #[account(init, payer = payer, space = Game::LEN, seeds = [GAME_SEED, mint.key().as_ref()], bump)]
    pub game: Box<Account<'info, Game>>,
    #[account(
        init,
        payer = payer,
        space = StrategyTerms::LEN,
        seeds = [STRATEGY_SEED, mint.key().as_ref()],
        bump
    )]
    pub terms: Box<Account<'info, StrategyTerms>>,
    /// CHECK: the ticket hook's state for the mint (read as a lottery-format game header).
    pub hook_state: UncheckedAccount<'info>,
    /// CHECK: the ticket hook's registry for the mint.
    pub hook_registry: UncheckedAccount<'info>,
    /// CHECK: the ticket hook's status (seeds-checked; it need not exist).
    #[account(seeds = [HOOK_STATUS_SEED, args.hook.as_ref()], bump)]
    pub hook_status: UncheckedAccount<'info>,
    /// CHECK: the strategy program (address-checked; executable, class checked in the handler).
    #[account(address = s.strategy)]
    pub strategy: UncheckedAccount<'info>,
    /// CHECK: the strategy's status (seeds-checked; it need not exist).
    #[account(seeds = [HOOK_STATUS_SEED, s.strategy.as_ref()], bump)]
    pub strategy_status: UncheckedAccount<'info>,
    /// CHECK: the strategy's registry for the mint (address-checked; it need not exist).
    #[account(address = registry_address(&s.strategy, mint.key).0)]
    pub strategy_registry: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// A strategy game's settings within their bounds (`BadGame`, `BadStrategy`): a pot part and no
/// holders' part, periods of an hour to 30 days, a claim window of 5 minutes to half a period (the
/// least time a plan leaves for payments), no prize share or attempts (the strategy decides), and
/// the strategy's own bounds.
fn check_strategy_args(args: &CreateGameArgs, s: &StrategyArgs) -> Result<()> {
    require!(
        args.kind == GameKind::Strategy
            && (MIN_MIN_POT..=MAX_MIN_POT).contains(&args.min_pot)
            && args.prize_bps == 0
            && args.max_attempts == 0
            && bordrless_game::valid_round_secs(args.round_secs)
            && (MIN_CLAIM_WINDOW..=MAX_CLAIM_WINDOW).contains(&args.claim_window_secs)
            && u64::from(args.claim_window_secs) * u64::from(CLAIMS_PER_ROUND)
                <= u64::from(args.round_secs),
        CompanionError::BadGame
    );
    require!(
        (1..=MAX_STRATEGY_BUDGET_BPS).contains(&s.budget_bps)
            && (1..=MAX_STRATEGY_SHARE_BPS).contains(&s.max_share_bps)
            && (1..=MAX_STRATEGY_PER_TX).contains(&s.max_per_tx)
            && (1..=MAX_PLAN_CU).contains(&s.plan_cu_max)
            && (1..=MAX_ENTITLE_CU).contains(&s.entitle_cu_max)
            && s.min_weight >= 1,
        CompanionError::BadStrategy
    );
    Ok(())
}

/// The strategy's class and terms: an executable program that is none of the protocol's (nor the
/// companion, the oracle, the timelock, the system program or the game's own hook), immutable,
/// timelocked or Bordrless-managed (its ProgramData and `Timelock` among `available`), whatever its
/// status says; never blocked (`StrategyNotAccepted`). Its class is `STATUS` when the protocol
/// wrote it a status (whose terms then apply).
fn vet_strategy<'a, 'info>(
    available: &'a [AccountInfo<'info>],
    strategy: &'a AccountInfo<'info>,
    status: &AccountInfo<'info>,
    hook: &Pubkey,
) -> Result<(u8, HookTerms, Option<u64>)> {
    let key = *strategy.key;
    require!(
        strategy.executable
            && key != Pubkey::default()
            && key != crate::ID
            && key != crate::oracle::ORAO_VRF_ID
            && key != HOOK_TIMELOCK_ID
            && key != *hook
            && key != LOTTERY_HOOK_ID
            && !bordrless_launch::constants::PROTOCOL_PROGRAMS.contains(&key),
        CompanionError::StrategyNotAccepted
    );
    let class = strategy_class_now(available, strategy)?
        .ok_or_else(|| error!(CompanionError::StrategyNotAccepted))?;
    // A timelocked strategy is taken only while its `Timelock` holds no proposal (independent audit
    // X2): its "N days of notice" must be whole for the game's players, so nobody makes a game on
    // code whose replacement is already staged (or executable). The `Timelock` is among
    // `available` (its class could not be read otherwise).
    if class == strategy_class::TIMELOCKED {
        require!(
            !timelock_pending(available, &key)?,
            CompanionError::TimelockPending
        );
    }
    let audit = audit_holds(available, strategy, Some(class), status, None, true)?;
    let terms = read_strategy_terms(status, &key, audit.is_some())?;
    require!(!terms.blocked, CompanionError::StrategyNotAccepted);
    if *status.owner == crate::ID {
        return Ok((strategy_class::STATUS, terms, audit));
    }
    Ok((class, terms, audit))
}

/// Whether `program`'s `Timelock` among `available` (by address, owned by `hook_timelock`) holds a
/// proposal. One that is not passed, or not `hook_timelock`'s, holds none (the class check before
/// it found and validated it).
fn timelock_pending(available: &[AccountInfo], program: &Pubkey) -> Result<bool> {
    let lock = timelock_address(program).0;
    let Some(info) = available.iter().find(|a| *a.key == lock) else {
        return Ok(false);
    };
    if *info.owner != HOOK_TIMELOCK_ID {
        return Ok(false);
    }
    let data = info.try_borrow_data()?;
    Ok(bordrless_hook::authority::parse_timelock(&data).is_some_and(|v| v.pending.is_some()))
}

/// The strategy's class now, read from its ProgramData and `Timelock` among `all` (by address):
/// `Some` when it is immutable, timelocked (at least 3 days) or Bordrless-managed, `None` when
/// someone else can change it, and `StrategyNotAccepted` when its accounts are missing or forged
/// (so a sender can't make a strategy read as anything by leaving them out). Checked at creation
/// and again before every step: a strategy handed to an outside key since stops planning and
/// paying (its pot waits, then retires).
fn strategy_class_now<'a, 'info>(
    all: &'a [AccountInfo<'info>],
    strategy: &'a AccountInfo<'info>,
) -> Result<Option<u8>> {
    let key = *strategy.key;
    let pd_key = programdata_address(&key);
    let pd = all.iter().find(|a| *a.key == pd_key).unwrap_or(strategy);
    let lock = timelock_address(&key).0;
    let timelock = all.iter().find(|a| *a.key == lock);
    match classify(strategy, pd, timelock) {
        Ok(AuthorityClass::Immutable) => Ok(Some(strategy_class::IMMUTABLE)),
        Ok(AuthorityClass::Timelocked { .. }) => Ok(Some(strategy_class::TIMELOCKED)),
        Ok(AuthorityClass::Protocol(_)) => Ok(Some(strategy_class::MANAGED)),
        Ok(AuthorityClass::Author(_)) => Ok(None),
        Err(_) => err!(CompanionError::StrategyNotAccepted),
    }
}

/// Whether the strategy's audit holds for the code it runs now: the protocol recorded the audited
/// code's hash (`set_hook_status_v2`), the strategy is immutable or Bordrless-managed (`class`),
/// and its code is that hash. Answers the deploy slot it holds for (`None`: it doesn't). `memo` is
/// the slot it last held for (`StrategyTerms.audit_slot`): with `recompute`, the hash is
/// recomputed only when the code's deploy slot moved since (an upgrade, or anyone's
/// `ExtendProgram`); without, the memo is taken as it is (a payment never hashes code, so no
/// extension can change its terms).
fn audit_holds<'a, 'info>(
    all: &'a [AccountInfo<'info>],
    strategy: &'a AccountInfo<'info>,
    class: Option<u8>,
    status: &AccountInfo<'info>,
    memo: Option<u64>,
    recompute: bool,
) -> Result<Option<u64>> {
    let fixed = matches!(
        class,
        Some(strategy_class::IMMUTABLE) | Some(strategy_class::MANAGED)
    );
    if !fixed || *status.owner != crate::ID {
        return Ok(None);
    }
    let recorded = {
        let st = HookStatus::try_deserialize(&mut &status.try_borrow_data()?[..])?;
        if !st.audited || st.audited_hash() == [0; 32] {
            return Ok(None);
        }
        st.audited_hash()
    };
    if !recompute {
        return Ok(memo);
    }
    let v3 = *strategy.owner == BPF_LOADER_UPGRADEABLE_ID;
    let code_info = if v3 {
        let pd = programdata_address(strategy.key);
        all.iter()
            .find(|a| *a.key == pd)
            .ok_or(CompanionError::StrategyNotAccepted)?
    } else {
        strategy
    };
    let data = code_info.try_borrow_data()?;
    let slot = if v3 {
        parse_programdata(&data).map_or(0, |(slot, _)| slot)
    } else if *code_info.owner == LOADER_V4_ID {
        data.get(..8)
            .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap_or([0; 8])))
    } else {
        // Loader 2: the code never changes.
        0
    };
    if memo == Some(slot) {
        return Ok(Some(slot));
    }
    let code = code_of(code_info.owner, &data).ok_or(CompanionError::StrategyNotAccepted)?;
    Ok((crate::instructions::attest::code_hash(code) == recorded).then_some(slot))
}

/// A strategy's terms from its status (which need not exist): as a hook's, except that an audit
/// lifts its cap only while it holds for the code that runs (`audit_holds`): its hash recorded
/// (`set_hook_status_v2`; a v1 audit counts as none), the strategy immutable or Bordrless-managed,
/// and its code that hash.
fn read_strategy_terms(info: &AccountInfo, strategy: &Pubkey, audit_holds: bool) -> Result<HookTerms> {
    let terms = read_hook_terms(info, strategy)?;
    if terms.audited && !audit_holds {
        return Ok(HookTerms {
            audited: false,
            pot_cap: DEFAULT_POT_CAP,
            blocked: terms.blocked,
        });
    }
    Ok(terms)
}

/// The extra accounts a strategy's registry names (at most `MAX_STRATEGY_EXTRAS`), resolved against
/// `[mint, game]` (`bordrless_strategy::registry_prefix`), each passed among `available` and owned
/// by the strategy (`StrategyRegistry`). A registry that does not exist names none.
fn strategy_extras(
    available: &[AccountInfo],
    registry: &AccountInfo,
    strategy: &Pubkey,
    mint: &Pubkey,
    game: &Pubkey,
) -> Result<([Pubkey; 2], u8)> {
    let mut out = [Pubkey::default(); 2];
    if *registry.owner == system_program::ID && registry.data_is_empty() {
        return Ok((out, 0));
    }
    require_keys_eq!(*registry.owner, *strategy, CompanionError::StrategyRegistry);
    let list = decode_registry(registry).ok_or_else(|| error!(CompanionError::StrategyRegistry))?;
    require!(
        list.accounts.len() <= MAX_STRATEGY_EXTRAS,
        CompanionError::StrategyRegistry
    );
    for (i, extra) in list.accounts.iter().enumerate() {
        let key = match &extra.source {
            AccountSource::Key(k) => *k,
            AccountSource::Pda { program, seeds } => {
                let mut bytes: Vec<Vec<u8>> = Vec::with_capacity(seeds.len());
                for seed in seeds {
                    bytes.push(match seed {
                        Seed::Literal(b) => b.clone(),
                        Seed::Account(0) => mint.to_bytes().to_vec(),
                        Seed::Account(1) => game.to_bytes().to_vec(),
                        _ => return err!(CompanionError::StrategyRegistry),
                    });
                }
                let refs: Vec<&[u8]> = bytes.iter().map(Vec::as_slice).collect();
                Pubkey::find_program_address(&refs, program).0
            }
        };
        let info = available
            .iter()
            .find(|a| *a.key == key)
            .ok_or(CompanionError::StrategyRegistry)?;
        require_keys_eq!(*info.owner, *strategy, CompanionError::StrategyRegistry);
        out[i] = key;
    }
    Ok((out, list.accounts.len() as u8))
}

/// `create_strategy_game(args, s)`.
pub fn process_create_strategy_game<'info>(
    ctx: Context<'info, CreateStrategyGame<'info>>,
    args: CreateGameArgs,
    s: StrategyArgs,
) -> Result<()> {
    let c = &ctx.accounts.companion;
    require!(!c.launched, CompanionError::AlreadyLaunched);
    require!(!c.is_game(), CompanionError::BadGame);
    require!(args.kind == GameKind::Strategy, CompanionError::WrongGameKind);
    require!(
        args.pot_bps > 0 && args.split.valid_with_pot(args.pot_bps),
        CompanionError::BadSplit
    );
    require!(
        args.split.holders_bps == 0,
        CompanionError::HolderRewardsOff
    );
    require!(
        c.max_buyback >= MIN_MAX_BUYBACK
            && (MIN_BUYBACK_INTERVAL..=MAX_BUYBACK_INTERVAL).contains(&c.buyback_interval),
        CompanionError::BadBuybackLimits
    );
    check_strategy_args(&args, &s)?;
    require!(
        args.hook != Pubkey::default()
            && args.hook != crate::ID
            && args.hook != crate::oracle::ORAO_VRF_ID
            && !bordrless_launch::constants::PROTOCOL_PROGRAMS.contains(&args.hook),
        CompanionError::BadGame
    );
    let mint = ctx.accounts.mint.key();
    // The tickets: a lottery-format header with the game's period as its round length.
    let (state, state_bump) = state_address(&args.hook, &mint);
    require_keys_eq!(
        ctx.accounts.hook_state.key(),
        state,
        CompanionError::HookState
    );
    let header = read_state_at(&ctx.accounts.hook_state, &args.hook, &mint, state_bump)
        .map_err(|_| error!(CompanionError::HookState))?;
    require!(
        header.round_secs == args.round_secs,
        CompanionError::HookState
    );
    check_kind_header(
        &ctx.accounts.hook_state,
        GameKind::Lottery,
        &GameKindArgs::default(),
    )?;
    check_hook_registry(
        &ctx.accounts.hook_registry,
        &args.hook,
        &mint,
        MAX_GAME_HOOK_EXTRAS_V2,
    )?;
    let (hook_terms, hook_memo) = vet_game_hook(
        ctx.remaining_accounts,
        &args.hook,
        &ctx.accounts.hook_status,
    )?;
    let (class, strategy_terms, audit) = vet_strategy(
        ctx.remaining_accounts,
        &ctx.accounts.strategy,
        &ctx.accounts.strategy_status,
        &args.hook,
    )?;
    let game_key = ctx.accounts.game.key();
    let (extras, n_extras) = strategy_extras(
        ctx.remaining_accounts,
        &ctx.accounts.strategy_registry,
        &s.strategy,
        &mint,
        &game_key,
    )?;
    let now = Clock::get()?.unix_timestamp;
    let first_round = round_of(now, args.round_secs);

    let g = &mut ctx.accounts.game;
    g.version = GAME_VERSION;
    g.bump = ctx.bumps.game;
    g.kind = GameKind::Strategy;
    g.mint = mint;
    g.hook = args.hook;
    g.state_bump = state_bump;
    g.status_bump = ctx.bumps.hook_status;
    g.oracle_bump = Game::oracle_payer(&mint).1;
    g.round_secs = args.round_secs;
    g.min_pot = args.min_pot;
    g.prize_bps = 0;
    g.claim_window_secs = args.claim_window_secs;
    g.max_attempts = 0;
    g.created_at = now;
    g.status = DrawStatus::Idle;
    g.next_round = first_round;
    g.settled_at = 0;
    g.oracle_answered();
    g.timer_secs = 0;
    g.min_tokens = 0;
    g.paid_buys = 0;
    g.min_streak_secs = 0;
    g.min_weight = s.min_weight;
    g.epoch_paid = 0;
    g.set_hook_audit_memo(hook_memo);
    g.reserved = [0; 34];

    let t = &mut ctx.accounts.terms;
    t.version = STRATEGY_VERSION;
    t.bump = ctx.bumps.terms;
    t.game = game_key;
    t.mint = mint;
    t.strategy = s.strategy;
    t.status_bump = ctx.bumps.strategy_status;
    t.extras = extras;
    t.n_extras = n_extras;
    t.budget_bps = s.budget_bps;
    t.max_share_bps = s.max_share_bps;
    t.max_per_tx = s.max_per_tx;
    t.plan_cu_max = s.plan_cu_max;
    t.entitle_cu_max = s.entitle_cu_max;
    t.periods_planned = 0;
    t.paid_total = 0;
    t.last_plan_at = 0;
    t.paid_at_active = 0;
    t.audit_ok = audit.is_some();
    t.audit_slot = audit.unwrap_or(0);
    t.reserved = [0; 15];

    let c = &mut ctx.accounts.companion;
    c.split = args.split;
    c.pot_bps = args.pot_bps;
    c.game_hook = args.hook;
    c.round_secs = args.round_secs;
    c.game_kind = GameKind::Strategy;
    c.pot_locked = 0;
    let combined = hook_terms.stricter(&strategy_terms);
    emit_cpi!(GameCreated {
        companion: c.key(),
        game: game_key,
        mint,
        kind: GameKind::Strategy,
        hook: args.hook,
        split: args.split,
        pot_bps: args.pot_bps,
        round_secs: args.round_secs,
        min_pot: args.min_pot,
        prize_bps: 0,
        claim_window_secs: args.claim_window_secs,
        max_attempts: 0,
        first_round,
    });
    emit_cpi!(StrategySet {
        game: game_key,
        mint,
        strategy: s.strategy,
        class,
        budget_bps: s.budget_bps,
        max_share_bps: s.max_share_bps,
        max_per_tx: s.max_per_tx,
        plan_cu_max: s.plan_cu_max,
        entitle_cu_max: s.entitle_cu_max,
        min_weight: s.min_weight,
        extras: extras[..usize::from(n_extras)].to_vec(),
        audited: combined.audited,
        pot_cap: combined.cap().unwrap_or(0),
    });
    Ok(())
}

// ---- asking the strategy -------------------------------------------------------------------------

/// Calls `strategy` with `data`, every account of `accounts` read-only and none a signer, and reads
/// its answer: the return data the strategy itself set, exactly 8 bytes. The return data is cleared
/// first, so data left by an earlier call is never taken for an answer. Answers the value, or why
/// it was refused. An error of the strategy (or its running out of compute) is the transaction's.
///
/// The compute a call uses can't be measured on chain: the remaining-compute syscall is not active
/// on mainnet (feature `5TuppMutoyzhUSfuYdhgzD47F92GL1g89KpCZQKqedxP`, inactive on 2026-10-09). The
/// terms' `plan_cu_max` and `entitle_cu_max` are therefore the compute keepers budget for each call
/// (and Studio's simulator holds a strategy to 70% of them); a strategy that uses more than the
/// transaction's limit makes it fail, as any failure does.
fn ask<'info>(
    strategy: &AccountInfo<'info>,
    accounts: &[AccountInfo<'info>],
    data: Vec<u8>,
) -> Result<core::result::Result<u64, AnswerFault>> {
    let metas: Vec<AccountMeta> = accounts
        .iter()
        .map(|a| AccountMeta::new_readonly(*a.key, false))
        .collect();
    let ix = Instruction {
        program_id: *strategy.key,
        accounts: metas,
        data,
    };
    let mut infos = accounts.to_vec();
    infos.push(strategy.clone());
    set_return_data(&[]);
    invoke(&ix, &infos)?;
    let answer = match get_return_data() {
        None => Err(AnswerFault::NoAnswer),
        Some((program, _)) if program != *strategy.key => Err(AnswerFault::WrongProgram),
        Some((_, bytes)) if bytes.len() != bordrless_strategy::ANSWER_LEN => {
            Err(AnswerFault::BadLength)
        }
        Some((_, bytes)) => {
            let mut b = [0u8; 8];
            b.copy_from_slice(&bytes);
            Ok(u64::from_le_bytes(b))
        }
    };
    set_return_data(&[]);
    Ok(answer)
}

/// The strategy's extras among `all`, each still owned by the strategy (`None` otherwise).
fn extra_infos<'info>(
    all: &[AccountInfo<'info>],
    t: &StrategyTerms,
) -> Result<Option<Vec<AccountInfo<'info>>>> {
    let mut out = Vec::with_capacity(t.extras().len());
    for key in t.extras() {
        let info = find(all, key)?;
        if *info.owner != t.strategy {
            return Ok(None);
        }
        out.push(info.clone());
    }
    Ok(Some(out))
}

/// The stricter of the ticket hook's status and the strategy's (each from its account, which need not
/// exist).
fn strategy_terms(
    hook_terms: HookTerms,
    strategy_status: &AccountInfo,
    strategy: &Pubkey,
    audit_holds: bool,
) -> Result<HookTerms> {
    Ok(hook_terms.stricter(&read_strategy_terms(
        strategy_status,
        strategy,
        audit_holds,
    )?))
}

/// The terms a strategy step applies (`terms`: the stricter of the ticket hook's status and the
/// strategy's).
/// Under a block the pot (the budget locked included) goes to the buyback and any open period
/// ends: answers `None` (the step stops there and succeeds; `NothingToDo` when nothing moved and no
/// period was open). Otherwise a pot above a cap lowered since is trimmed to it (what a period
/// already locked stays).
fn apply_strategy_terms(
    event_authority: &AccountInfo,
    companion_key: Pubkey,
    game_key: Pubkey,
    c: &mut Companion,
    g: &mut Game,
    terms: HookTerms,
) -> Result<Option<HookTerms>> {
    require!(c.launched, CompanionError::NotLaunched);
    let moved = enforce_terms(c, &terms, Clock::get()?.unix_timestamp)?;
    if moved > 0 {
        emit_event(
            event_authority,
            &PotToBuyback {
                companion: companion_key,
                lamports: moved,
                blocked: terms.blocked,
                pending_pot: c.pending_pot,
            },
        )?;
    }
    if !terms.blocked {
        return Ok(Some(terms));
    }
    let ended = g.status != DrawStatus::Idle;
    require!(moved > 0 || ended, CompanionError::NothingToDo);
    if ended {
        rolled_over(event_authority, game_key, c, g, RolloverReason::Blocked)?;
    }
    Ok(None)
}

// ---- plan_period ---------------------------------------------------------------------------------

/// Accounts of `plan_period`. Remaining: the strategy's extras.
#[event_cpi]
#[derive(Accounts)]
pub struct PlanPeriod<'info> {
    /// Whoever sends it (paid nothing).
    pub cranker: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    #[account(mut, seeds = [GAME_SEED, companion.mint.as_ref()], bump = game.bump)]
    pub game: Box<Account<'info, Game>>,
    #[account(mut, seeds = [STRATEGY_SEED, companion.mint.as_ref()], bump = terms.bump)]
    pub terms: Box<Account<'info, StrategyTerms>>,
    /// CHECK: the ticket hook's status (seeds-checked; it need not exist).
    #[account(seeds = [HOOK_STATUS_SEED, game.hook.as_ref()], bump = game.status_bump)]
    pub hook_status: UncheckedAccount<'info>,
    /// CHECK: the strategy's status (seeds-checked; it need not exist).
    #[account(seeds = [HOOK_STATUS_SEED, terms.strategy.as_ref()], bump = terms.status_bump)]
    pub strategy_status: UncheckedAccount<'info>,
    #[account(address = launch_client::launch_address(&companion.mint))]
    pub launch: Box<Account<'info, Launch>>,
    /// CHECK: the launch's pool (address-checked), passed to the strategy.
    #[account(address = launch.pool)]
    pub pool: UncheckedAccount<'info>,
    /// CHECK: the ticket hook's state (owner, address and header checked in the handler).
    pub hook_state: UncheckedAccount<'info>,
    /// CHECK: the strategy program (address-checked).
    #[account(address = terms.strategy)]
    pub strategy: UncheckedAccount<'info>,
}

/// `plan_period(period)`.
pub fn process_plan_period<'info>(
    ctx: Context<'info, PlanPeriod<'info>>,
    period: u32,
) -> Result<()> {
    require!(
        ctx.accounts.game.kind == GameKind::Strategy,
        CompanionError::WrongGameKind
    );
    let now = Clock::get()?.unix_timestamp;
    let (companion_key, game_key) = (ctx.accounts.companion.key(), ctx.accounts.game.key());
    let event_authority = ctx.accounts.event_authority.to_account_info();
    // The period paid last: what it did not pay rolls over once its payments have ended.
    let (ended_period, unpaid) = {
        let g = &ctx.accounts.game;
        (g.round, g.prize.saturating_sub(g.epoch_paid))
    };
    let released = end_epoch_if_over(&mut ctx.accounts.companion, &mut ctx.accounts.game, now);
    // The strategy's class and its audit, before any terms are applied: accounts left out or
    // forged are refused here, so nobody trims a pot by misreporting them.
    // (One list of every account, made once: the heap is never freed.)
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let (class, audit) = {
        let class = strategy_class_now(&all, &ctx.accounts.strategy)?;
        let t = &ctx.accounts.terms;
        let memo = t.audit_ok.then_some(t.audit_slot);
        let audit = audit_holds(
            &all,
            &ctx.accounts.strategy,
            class,
            &ctx.accounts.strategy_status,
            memo,
            true,
        )?;
        (class, audit)
    };
    {
        let t = &mut ctx.accounts.terms;
        t.audit_ok = audit.is_some();
        t.audit_slot = audit.unwrap_or(0);
    }
    // The ticket hook's terms: a hashed audit holds only for its code (checked here, the game
    // keeping the slot; a payment takes this check as it is).
    let (hook_terms, hook_memo) = game_hook_terms(
        &all,
        &ctx.accounts.game.hook,
        &ctx.accounts.hook_status,
        ctx.accounts.game.hook_audit_memo(),
        true,
    )?;
    ctx.accounts.game.set_hook_audit_memo(hook_memo);
    let combined = strategy_terms(
        hook_terms,
        &ctx.accounts.strategy_status,
        &ctx.accounts.terms.strategy,
        audit.is_some(),
    )?;
    let Some(terms) = apply_strategy_terms(
        &event_authority,
        companion_key,
        game_key,
        &mut ctx.accounts.companion,
        &mut ctx.accounts.game,
        combined,
    )?
    else {
        return Ok(());
    };
    let (c, g) = (&ctx.accounts.companion, &ctx.accounts.game);
    require!(g.status == DrawStatus::Idle, CompanionError::DrawPending);
    let current = round_of(now, g.round_secs);
    require!(
        current > 0 && period == current - 1 && period >= g.next_round,
        CompanionError::RoundNotOver
    );
    require!(
        c.pending_pot >= terms.draw_threshold(g.min_pot_at(now, c.launched_at)),
        CompanionError::PotTooSmall
    );
    if released {
        emit_cpi!(EpochEnded {
            game: game_key,
            mint: g.mint,
            epoch: ended_period,
            unclaimed: unpaid,
            pending_pot: c.pending_pot,
        });
    }
    let next = period.checked_add(1).ok_or(CompanionError::MathOverflow)?;
    // Too late to leave its holders a whole window to be paid in: it lapses, the pot intact.
    let last_plan = claims_end(period, g.round_secs).saturating_sub(i64::from(g.claim_window_secs));
    if now > last_plan {
        let c = &ctx.accounts.companion;
        let g = &mut ctx.accounts.game;
        g.next_round = next;
        g.round = period;
        g.total = 0;
        return rolled_over(&event_authority, game_key, c, g, RolloverReason::Late);
    }
    let state = &ctx.accounts.hook_state;
    let header = read_state_at(state, &g.hook, &g.mint, g.state_bump)
        .map_err(|_| error!(CompanionError::HookState))?;
    require!(header.round_secs == g.round_secs, CompanionError::HookState);
    let total = match header.total_of(period) {
        Some(t) if t > 0 => t,
        other => {
            let c = &ctx.accounts.companion;
            let g = &mut ctx.accounts.game;
            g.next_round = next;
            g.round = period;
            g.total = 0;
            let reason = if other.is_none() {
                RolloverReason::RoundForgotten
            } else {
                RolloverReason::NoTickets
            };
            return rolled_over(&event_authority, game_key, c, g, reason);
        }
    };
    let t = &ctx.accounts.terms;
    // What may be planned: the strategy's share of the pot, which the terms have trimmed to its cap.
    let budget_max = bps_of(c.pending_pot, u64::from(t.budget_bps));
    require!(class.is_some(), CompanionError::StrategyNotAccepted);
    let extras = extra_infos(&all, t)?;
    let answer = match extras {
        None => Err(AnswerFault::Accounts),
        Some(extras) => {
            let mut accounts = vec![
                ctx.accounts.game.to_account_info(),
                ctx.accounts.companion.to_account_info(),
                state.to_account_info(),
                ctx.accounts.launch.to_account_info(),
                ctx.accounts.pool.to_account_info(),
            ];
            accounts.extend(extras);
            let args = PlanArgs {
                version: ARGS_VERSION,
                mint: g.mint,
                period,
                period_start: round_start(period, g.round_secs),
                period_end: round_end(period, g.round_secs),
                total,
                pot: c.pending_pot,
                budget_max,
                periods_planned: t.periods_planned,
                paid_total: t.paid_total,
                now,
            };
            let mut data = PLAN.to_vec();
            args.serialize(&mut data)?;
            ask(&ctx.accounts.strategy, &accounts, data)?
        }
    };
    let answer = answer.and_then(|budget| {
        if budget > budget_max {
            Err(AnswerFault::OverBound)
        } else {
            Ok(budget)
        }
    });
    let cranker = ctx.accounts.cranker.key();
    // A refused answer leaves the period unplanned: it may be asked again until its deadline (and
    // lapses then, the pot intact), so nobody can void a period by asking at a chosen moment.
    if let Err(reason) = answer {
        emit_cpi!(PeriodRejected {
            game: game_key,
            mint: ctx.accounts.game.mint,
            period,
            reason,
            cranker,
        });
        return Ok(());
    }
    let c = &mut ctx.accounts.companion;
    let g = &mut ctx.accounts.game;
    let t = &mut ctx.accounts.terms;
    g.next_round = next;
    g.round = period;
    g.total = total;
    match answer {
        Err(_) => {}
        Ok(0) => {
            emit_cpi!(PeriodSkipped {
                game: game_key,
                mint: g.mint,
                period,
                total,
                cranker,
            });
        }
        Ok(budget) => {
            g.prize = budget;
            g.epoch_paid = 0;
            g.status = DrawStatus::Revealed;
            g.revealed_at = now;
            g.draws = g.draws.saturating_add(1);
            c.pot_locked = budget;
            t.periods_planned = t.periods_planned.saturating_add(1);
            t.last_plan_at = now;
            emit_cpi!(PeriodPlanned {
                game: game_key,
                mint: g.mint,
                period,
                total,
                budget,
                budget_max,
                claims_end: g.claims_end(),
                pending_pot: c.pending_pot,
                cranker,
            });
        }
    }
    Ok(())
}

// ---- pay_strategy -------------------------------------------------------------------------------

/// Accounts of `pay_strategy`. Remaining: the strategy's extras, the bridge's `unwrap_sol` accounts,
/// then the `n` candidates' `[holding, owner (writable), receipt (writable)]`, last.
#[event_cpi]
#[derive(Accounts)]
pub struct PayStrategy<'info> {
    /// Whoever sends it: pays the receipts' rent (returned by `close_receipt`) and is paid the
    /// bounties.
    #[account(mut)]
    pub cranker: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked).
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    #[account(mut, seeds = [GAME_SEED, companion.mint.as_ref()], bump = game.bump)]
    pub game: Box<Account<'info, Game>>,
    #[account(mut, seeds = [STRATEGY_SEED, companion.mint.as_ref()], bump = terms.bump)]
    pub terms: Box<Account<'info, StrategyTerms>>,
    /// CHECK: the ticket hook's status (seeds-checked; it need not exist).
    #[account(seeds = [HOOK_STATUS_SEED, game.hook.as_ref()], bump = game.status_bump)]
    pub hook_status: UncheckedAccount<'info>,
    /// CHECK: the strategy's status (seeds-checked; it need not exist).
    #[account(seeds = [HOOK_STATUS_SEED, terms.strategy.as_ref()], bump = terms.status_bump)]
    pub strategy_status: UncheckedAccount<'info>,
    #[account(address = launch_client::launch_address(&companion.mint))]
    pub launch: Box<Account<'info, Launch>>,
    /// CHECK: the ticket hook's state (address checked in the handler), passed to the strategy.
    pub hook_state: UncheckedAccount<'info>,
    /// CHECK: the strategy program (address-checked).
    #[account(address = terms.strategy)]
    pub strategy: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Whether SOL can be paid to `owner`'s account (as `settle`'s `payable`): not executable, not a
/// sysvar, none of the runtime's reserved keys.
fn payable(account: &AccountInfo) -> bool {
    !account.executable
        && *account.owner != SYSVAR_PROGRAM_ID
        && !RESERVED_KEYS.contains(account.key)
}

/// Creates a strategy receipt at `receipt` (`PDA(["claimed", game, period, owner])`, `bump`), the
/// `payer` (a signer) paying its rent. An address someone has already funded is taken as Anchor's
/// `init` takes it (topped up, allocated, assigned), so nobody can block a holder's payment by
/// sending lamports to its receipt's address first.
fn create_receipt<'info>(
    payer: &AccountInfo<'info>,
    receipt: &AccountInfo<'info>,
    system: &AccountInfo<'info>,
    seeds: &[&[u8]],
) -> Result<()> {
    let space = ShareReceipt::LEN;
    let rent = Rent::get()?.minimum_balance(space);
    let current = receipt.lamports();
    if current == 0 {
        system_program::create_account(
            CpiContext::new_with_signer(
                system.key(),
                system_program::CreateAccount {
                    from: payer.clone(),
                    to: receipt.clone(),
                },
                &[seeds],
            ),
            rent,
            space as u64,
            &crate::ID,
        )?;
    } else {
        let top_up = rent.saturating_sub(current);
        if top_up > 0 {
            system_program::transfer(
                CpiContext::new(
                    system.key(),
                    system_program::Transfer {
                        from: payer.clone(),
                        to: receipt.clone(),
                    },
                ),
                top_up,
            )?;
        }
        system_program::allocate(
            CpiContext::new_with_signer(
                system.key(),
                system_program::Allocate {
                    account_to_allocate: receipt.clone(),
                },
                &[seeds],
            ),
            space as u64,
        )?;
        system_program::assign(
            CpiContext::new_with_signer(
                system.key(),
                system_program::Assign {
                    account_to_assign: receipt.clone(),
                },
                &[seeds],
            ),
            &crate::ID,
        )?;
    }
    Ok(())
}

/// `pay_strategy(period, n)`.
pub fn process_pay_strategy<'info>(
    ctx: Context<'info, PayStrategy<'info>>,
    period: u32,
    n: u8,
) -> Result<()> {
    require!(
        ctx.accounts.game.kind == GameKind::Strategy,
        CompanionError::WrongGameKind
    );
    let now = Clock::get()?.unix_timestamp;
    let (companion_key, game_key) = (ctx.accounts.companion.key(), ctx.accounts.game.key());
    // Only the period open for payments, and only before its payments end.
    {
        let g = &ctx.accounts.game;
        require!(
            g.status == DrawStatus::Revealed && g.round == period,
            CompanionError::PeriodNotOpen
        );
        require!(now < g.claims_end(), CompanionError::DrawLate);
        let t = &ctx.accounts.terms;
        require!(
            n >= 1 && n <= t.max_per_tx,
            CompanionError::TooManyCandidates
        );
        let state = state_address(&g.hook, &g.mint).0;
        require_keys_eq!(
            ctx.accounts.hook_state.key(),
            state,
            CompanionError::HookState
        );
    }
    let event_authority = ctx.accounts.event_authority.to_account_info();
    // As in `plan_period`: accounts left out or forged are refused before any terms apply. The
    // audit is the one the period's plan checked (a payment never hashes code).
    // (One list of every account, made once: the heap is never freed.)
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let (class, audit) = {
        let class = strategy_class_now(&all, &ctx.accounts.strategy)?;
        let t = &ctx.accounts.terms;
        let audit = audit_holds(
            &all,
            &ctx.accounts.strategy,
            class,
            &ctx.accounts.strategy_status,
            t.audit_ok.then_some(t.audit_slot),
            false,
        )?;
        (class, audit)
    };
    let (hook_terms, _) = game_hook_terms(
        &all,
        &ctx.accounts.game.hook,
        &ctx.accounts.hook_status,
        ctx.accounts.game.hook_audit_memo(),
        false,
    )?;
    let combined = strategy_terms(
        hook_terms,
        &ctx.accounts.strategy_status,
        &ctx.accounts.terms.strategy,
        audit.is_some(),
    )?;
    if apply_strategy_terms(
        &event_authority,
        companion_key,
        game_key,
        &mut ctx.accounts.companion,
        &mut ctx.accounts.game,
        combined,
    )?
    .is_none()
    {
        return Ok(());
    }
    let remaining = ctx.remaining_accounts;
    let n = usize::from(n);
    require!(
        remaining.len() >= 3 * n,
        CompanionError::MissingAccount
    );
    let candidates = &remaining[remaining.len() - 3 * n..];
    require!(class.is_some(), CompanionError::StrategyNotAccepted);
    let extras = extra_infos(&all, &ctx.accounts.terms)?
        .ok_or_else(|| error!(CompanionError::StrategyAccounts))?;
    let (c, g, t) = (
        &ctx.accounts.companion,
        &ctx.accounts.game,
        &ctx.accounts.terms,
    );
    let mint = c.mint;
    let creator = ctx.accounts.creator.key();
    let launch = &ctx.accounts.launch;
    let excluded = [
        launch.key(),
        launch.pool,
        creator,
        companion_key,
        game_key,
        t.key(),
        Game::oracle_payer(&mint).0,
        t.strategy,
        g.hook,
    ];
    let budget = g.prize;
    let share_cap = bps_of(budget, u64::from(t.max_share_bps));
    let rent_min = Rent::get()?.minimum_balance(0);
    let period_le = period.to_le_bytes();
    let mut paid = g.epoch_paid;
    // (candidate, amount, bounty, receipt bump)
    let mut payments: Vec<(usize, u64, u64, u8)> = Vec::with_capacity(n);
    let system = ctx.accounts.system_program.to_account_info();
    let payer = ctx.accounts.cranker.to_account_info();
    // First every question, then every receipt and payment: while the strategy answers, the
    // instructions this one has made (what a strategy could read with
    // `sol_get_processed_sibling_instruction`) are only the questions before (and rejections),
    // never a receipt naming the sender.
    for i in 0..n {
        let holding = &candidates[3 * i];
        let owner = &candidates[3 * i + 1];
        let receipt = &candidates[3 * i + 2];
        let reject = |reason: CandidateReason, answered: u64| {
            emit_event(
                &event_authority,
                &CandidateRejected {
                    game: game_key,
                    period,
                    owner: *owner.key,
                    reason,
                    answered,
                },
            )
        };
        // The token program's holding of the mint, at its owner's address.
        let Ok(h) = token_client::read_holding(holding) else {
            reject(CandidateReason::WrongHolding, 0)?;
            continue;
        };
        if h.mint != mint
            || h.owner != *owner.key
            || *holding.key != token_client::holding_address(&mint, owner.key)
        {
            reject(CandidateReason::WrongHolding, 0)?;
            continue;
        }
        if !eligible(owner.key, &excluded) || !payable(owner) {
            reject(CandidateReason::NotEligible, 0)?;
            continue;
        }
        let weight = weight_in(&h.hook_data, period);
        if weight == 0 || weight < g.min_weight || weight > h.amount {
            reject(CandidateReason::NoWeight, 0)?;
            continue;
        }
        let (receipt_key, receipt_bump) = Pubkey::find_program_address(
            &[CLAIMED_SEED, game_key.as_ref(), &period_le, owner.key.as_ref()],
            &crate::ID,
        );
        if *receipt.key != receipt_key {
            reject(CandidateReason::WrongHolding, 0)?;
            continue;
        }
        // Paid in an earlier transaction, or earlier in this one (receipts are made after every
        // question).
        if *receipt.owner == crate::ID
            || payments
                .iter()
                .any(|p| candidates[3 * p.0 + 1].key == owner.key)
        {
            reject(CandidateReason::AlreadyPaid, 0)?;
            continue;
        }
        let max_amount = share_cap.min(budget.saturating_sub(paid));
        let mut accounts = vec![
            ctx.accounts.game.to_account_info(),
            ctx.accounts.companion.to_account_info(),
            ctx.accounts.hook_state.to_account_info(),
            launch.to_account_info(),
            holding.clone(),
        ];
        accounts.extend(extras.iter().cloned());
        let args = EntitleArgs {
            version: ARGS_VERSION,
            mint,
            period,
            owner: *owner.key,
            balance: h.amount,
            weight,
            since: bordrless_strategy::slots_of(&h.hook_data).since,
            total: g.total,
            budget,
            paid,
            max_amount,
            now,
        };
        let mut data = ENTITLE.to_vec();
        args.serialize(&mut data)?;
        let answer = ask(&ctx.accounts.strategy, &accounts, data)?;
        let amount = match answer {
            Err(fault) => {
                reject(CandidateReason::Answer(fault), 0)?;
                continue;
            }
            Ok(a) if a > max_amount => {
                reject(CandidateReason::Answer(AnswerFault::OverBound), a)?;
                continue;
            }
            Ok(0) => {
                reject(CandidateReason::Zero, 0)?;
                continue;
            }
            Ok(a) => a,
        };
        let bounty = bps_of(amount, u64::from(c.bounty_bps));
        // An account below its rent-exempt minimum after the payment (an empty wallet paid less
        // than the minimum, or a legacy rent-paying one) can't be credited: the whole transaction
        // would fail. Skipped instead, so it never blocks the candidates beside it.
        if owner.lamports().saturating_add(amount - bounty) < rent_min {
            reject(CandidateReason::BelowRent, amount)?;
            continue;
        }
        paid = paid.checked_add(amount).ok_or(CompanionError::MathOverflow)?;
        payments.push((i, amount, bounty, receipt_bump));
    }
    for (i, amount, bounty, receipt_bump) in &payments {
        let owner = &candidates[3 * i + 1];
        let receipt = &candidates[3 * i + 2];
        let bump = [*receipt_bump];
        let seeds: [&[u8]; 5] = [
            CLAIMED_SEED,
            game_key.as_ref(),
            &period_le,
            owner.key.as_ref(),
            &bump,
        ];
        create_receipt(&payer, receipt, &system, &seeds)?;
        let r = ShareReceipt {
            version: RECEIPT_VERSION,
            bump: *receipt_bump,
            game: game_key,
            epoch: period,
            owner: *owner.key,
            payer: payer.key(),
            amount: amount - bounty,
            claimed_at: now,
        };
        let mut data = receipt.try_borrow_mut_data()?;
        let mut out: &mut [u8] = &mut data[..];
        r.try_serialize(&mut out)?;
    }
    let total: u64 = payments.iter().map(|p| p.1).sum();
    let bounty: u64 = payments.iter().map(|p| p.2).sum();
    let mut events = Vec::with_capacity(payments.len());
    if total > 0 {
        // Never more than the period's budget still locked, nor the pot.
        require!(
            total <= c.pot_locked && total <= c.pending_pot,
            CompanionError::MathOverflow
        );
        let seeds = CreatorSeeds::new(mint, c.creator_bump);
        crate::invoke::invoke_built(
            &bordrless_bridge::client::unwrap_sol(creator, total),
            &all,
            &[&seeds.seeds()],
        )?;
        let from = ctx.accounts.creator.to_account_info();
        let send = |to: &AccountInfo<'info>, lamports: u64| -> Result<()> {
            system_program::transfer(
                CpiContext::new_with_signer(
                    system.key(),
                    system_program::Transfer {
                        from: from.clone(),
                        to: to.clone(),
                    },
                    &[&seeds.seeds()],
                ),
                lamports,
            )
        };
        for (i, amount, b, _) in &payments {
            let owner = &candidates[3 * i + 1];
            send(owner, amount - b)?;
            events.push(StrategyPayment {
                owner: *owner.key,
                amount: amount - b,
                bounty: *b,
            });
        }
        if bounty > 0 {
            send(&payer, bounty)?;
        }
    }
    let c = &mut ctx.accounts.companion;
    c.pending_pot -= total;
    c.pot_locked -= total;
    c.bounties_total = c.bounties_total.saturating_add(bounty);
    let g = &mut ctx.accounts.game;
    g.epoch_paid = paid;
    g.prizes_paid = g.prizes_paid.saturating_add(payments.len() as u64);
    g.prizes_total = g.prizes_total.saturating_add(total - bounty);
    let t = &mut ctx.accounts.terms;
    t.paid_total = t.paid_total.saturating_add(total);
    if let Some((i, _, _, _)) = payments.last() {
        g.last_winner = *candidates[3 * i + 1].key;
        // A game is active (not retirable as dormant) only while it pays a real share of the pot:
        // at least `STRATEGY_ACTIVE_BPS` of it since it last counted as active, over however many
        // periods. Dust payments every few weeks keep nothing alive.
        let since = t.paid_total.saturating_sub(t.paid_at_active);
        if since >= bps_of(c.pending_pot.saturating_add(since), u64::from(STRATEGY_ACTIVE_BPS)) {
            g.settled_at = now;
            t.paid_at_active = t.paid_total;
        }
    }
    if !events.is_empty() {
        emit_cpi!(StrategyPaid {
            game: game_key,
            mint,
            period,
            payments: events,
            bounty,
            epoch_paid: paid,
            pending_pot: c.pending_pot,
            cranker: ctx.accounts.cranker.key(),
        });
    }
    Ok(())
}
