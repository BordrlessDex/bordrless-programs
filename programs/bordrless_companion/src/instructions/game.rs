//! Games (v2, `docs/companions.md`): a lottery run entirely by the companion, its tickets kept by
//! the coin's token hook under the game ticket standard (`bordrless-game`).
//!
//! - `create_game`, before the launch and signed by the mint as `create` is: the `Game` at
//!   `PDA(["game", mint])`, the pot's part of the split, and the hook, whose state for the mint must
//!   already be a game header with the game's round length (and whose registry a launch can
//!   carry). The hook is Bordrless's lottery hook, or one the protocol has vetted (written a
//!   status for), never a blocked one. The companion keeps the hook, the pot share and the round
//!   length itself, so `launch` checks the hook without the `Game`.
//! - The steps, each permissionless: `draw` (once a round is over and the pot holds at least
//!   `min_pot`, or its cap when lower, or `MIN_MIN_POT` once the game is dormant: the round's
//!   ticket total read from the hook's header, the draw's seed committed from the hash of one of
//!   the last `oracle::SEED_SLOTS` slots, and the oracle asked for it, the pot paying, or a pending
//!   request ORAO already holds for it adopted, all in one instruction; or, when the pot can't pay
//!   for the oracle's request, the round rolled over with no seed), `reveal` (the oracle's answer
//!   stored), `claim_prize` (attempt `k`, in its window: the holding whose range for the round
//!   holds ticket `draw_index(R, k, total)`, still holding it, is paid the prize as SOL), `expire`
//!   (the attempts all passed, or the draw's claims ended: the round rolls over), `retire` (a pot
//!   that has paid no prize for two dormant periods goes to the buyback). `draw` and `claim_prize`
//!   pay their sender `bounty_bps` of what they move (the oracle's top-up, the prize); `reveal`,
//!   `expire`, `retire` and a rollover pay nothing.
//! - **A round has one seed, and it is final.** ORAO's answer to it decides the draw however late
//!   it lands before the draw's claims end; it is never replaced by a new seed. Anyone can learn
//!   ORAO's answer to a public seed early (ORAO's devnet signs with mainnet's keys) and keep
//!   ORAO's fulfilment out of the blocks for a while (it pays no priority fee); with a new seed
//!   after a timeout, that would buy a re-draw. Now it can at most delay the reveal, or, kept up
//!   until the claims end, roll the round over (`OracleSilent`).
//! - **No pot is locked for ever.** A game whose pot has paid no prize for `Game::dormant_secs`
//!   (30 days, or 4 rounds) is dormant: it is drawn from `MIN_MIN_POT`. After two dormant periods
//!   anyone may `retire` the pot to the buyback, which pays nobody (as a block does): a pot below
//!   the floor, an oracle that can't be paid or stopped answering, or nobody claiming. Under a
//!   blocked hook, a buyback that has neither bought nor waited on its reference price for 30 days
//!   (its hook refusing the companion's own token moves) is burned as SOL by anyone
//!   (`burn_stranded`), paying nobody; each burn restarts the 30 days, as does a fee claim that
//!   credits the buyback at least what it held, and a pot it finds unmoved is moved into the
//!   buyback for a whole wait instead of burned.
//! - **A dead oracle costs a request now and then, not one a round.** While ORAO has not answered
//!   the last request the pot paid for (`Game.paid_seed`), the pot pays for a new one only for a
//!   draw 1, 2, 4… rounds after it (at most 30 days apart; `Game::oracle_backoff_rounds`). A
//!   reveal, or that request found answered, resets it.
//! - **A seed is never on chain without its request.** Anyone can preview ORAO's answer to a public
//!   seed. A seed committed before its request would leave whoever sends (or doesn't send) the
//!   request the choice of drawing it once they know who wins, and every other round would roll
//!   over to their benefit. So `draw` commits the seed and requests it in one instruction, and when
//!   the pot can't pay for the request (the breaker holding, ORAO's fee above the cap or its network
//!   state unreadable, the pot short) it rolls the round over instead
//!   (`RolloverReason::OracleUnpaid`). Its seed comes from a slot so recent that nobody could have
//!   previewed it, and a seed ORAO already answered is refused (`StaleSeed`). A pending request
//!   someone made for the seed is adopted: it was made blind, and ORAO answers it the same way.
//! - **A draw leaves time to be claimed.** It is made at least `REVEAL_SECS` and a whole claim
//!   window before its claims end (`Game::last_draw`), or the round rolls over (`Late`).
//! - **A draw of round `r` ends with round `r + 1`** (`Game::claims_end`): a holding keeps its
//!   ranges of two rounds, so in `r + 2` anyone could make a winner's range of `r` forgotten (one
//!   write to its holding in `r + 1` and one in `r + 2`) and hand its attempt to a later one. No
//!   draw is requested in its last claim window, no answer revealed and no attempt claimed after
//!   `r + 1`, and a draw that is not done by then rolls over (`RolloverReason::Late`).
//! - `set_hook_status`, the protocol's only say over a game: a hook audited (its pots uncapped; an
//!   audit is final), its cap while not (at most 10 SOL), or blocked (only a hook not audited;
//!   lifted only by an audit). Under a blocked hook every step sends the pot to the buyback and
//!   ends any draw: nobody is paid a prize.
//!
//! Nothing here calls the hook: the companion only reads its state and the holdings' hook data.

use anchor_lang::prelude::*;
use bordrless_game::{
    draw_index, eligible, read_state_at, round_of, state_address, state_address_at,
    valid_round_secs, wins,
};
use bordrless_hook::{hook_accounts_address, AccountSource, HookAccountList, Seed};
use bordrless_launch::client as launch_client;
use bordrless_launch::state::Launch;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::error::CompanionError;
use crate::events::*;
use crate::instructions::steps::{
    available, decode_registry, enforce_terms, find, pay_sol, read_hook_terms,
};
use crate::instructions::CreatorSeeds;
use crate::invoke::invoke_built;
use crate::oracle;
use crate::state::*;

/// What `emit_cpi!` does (the event as a self-CPI signed by this program's event authority), for
/// the helpers the steps share.
pub(crate) fn emit_event<E: anchor_lang::Event>(
    event_authority: &AccountInfo,
    event: &E,
) -> Result<()> {
    let mut data = anchor_lang::event::EVENT_IX_TAG_LE.to_vec();
    data.extend(event.data());
    let ix = anchor_lang::solana_program::instruction::Instruction::new_with_bytes(
        crate::ID,
        &data,
        vec![
            anchor_lang::solana_program::instruction::AccountMeta::new_readonly(
                *event_authority.key,
                true,
            ),
        ],
    );
    anchor_lang::solana_program::program::invoke_signed(
        &ix,
        std::slice::from_ref(event_authority),
        &[&[b"__event_authority", &[crate::EVENT_AUTHORITY_AND_BUMP.1]]],
    )?;
    Ok(())
}

// ---- create_game -------------------------------------------------------------------------------

/// Arguments of `create_game`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct CreateGameArgs {
    pub kind: GameKind,
    /// The coin's token hook (game ticket standard v1), already prepared for the mint with
    /// `round_secs`; the launch's config must name it.
    pub hook: Pubkey,
    /// Every fee claim's split, with the pot's part: the four sum to 10,000. No holders' part: a
    /// game coin has its own hook, so no kit.
    pub split: Split,
    pub pot_bps: u16,
    /// An hour to 30 days, as the hook's header says.
    pub round_secs: u32,
    /// No draw while the pot holds less (0.1 to 1,000 SOL).
    pub min_pot: u64,
    /// The part of the pot one draw pays (10% to 100%).
    pub prize_bps: u16,
    /// How long each claim attempt is open (5 minutes to a day).
    pub claim_window_secs: u32,
    /// Attempts per draw (1 to 16); all of them within half a round (`max_attempts *
    /// claim_window_secs <= round_secs / 2`), so a draw made early in the round after its own
    /// keeps every attempt before its claims end.
    pub max_attempts: u8,
}

/// Accounts of `create_game`.
#[event_cpi]
#[derive(Accounts)]
#[instruction(args: CreateGameArgs)]
pub struct CreateGame<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// The launch's mint (not created yet): it signs, as for `create`, so only whoever holds it
    /// makes its game.
    pub mint: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, mint.key().as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    #[account(init, payer = payer, space = Game::LEN, seeds = [GAME_SEED, mint.key().as_ref()], bump)]
    pub game: Box<Account<'info, Game>>,
    /// CHECK: the hook's state for the mint, read as a game header (owner the hook, address
    /// `PDA(["state", mint], hook)`, magic, mint).
    pub hook_state: UncheckedAccount<'info>,
    /// CHECK: the hook's registry for the mint (owner the hook, address
    /// `PDA(["bordrless-hook-accounts", mint], hook)`): the extra accounts a launch carries.
    pub hook_registry: UncheckedAccount<'info>,
    /// CHECK: what the protocol says of the hook (seeds-checked; owner checked in the handler). It
    /// must exist, and not block the hook, for any hook but Bordrless's lottery hook.
    #[account(seeds = [HOOK_STATUS_SEED, args.hook.as_ref()], bump)]
    pub hook_status: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Whether an extra account of a hook's registry is the launch of the mint (`["launch", mint]`
/// under the launchpad), which every launch transaction carries anyway.
fn is_the_launch(source: &AccountSource, mint: &Pubkey) -> bool {
    match source {
        AccountSource::Key(key) => *key == bordrless_launch::client::launch_address(mint),
        AccountSource::Pda { program, seeds } => {
            *program == LAUNCH_ID
                && seeds.len() == 2
                && seeds[0] == Seed::Literal(bordrless_launch::constants::LAUNCH_SEED.to_vec())
                && seeds[1] == Seed::Account(1)
        }
    }
}

/// The hook's registry for the mint lists at most `max` accounts besides the launch
/// (`MAX_GAME_HOOK_EXTRAS` for `create_game`, `MAX_GAME_HOOK_EXTRAS_V2` for `create_game_v2` and a
/// jackpot's or a streak's launch), so the companion's launch of a game coin fits a transaction
/// (owner, address, bounds and layout checked).
pub(crate) fn check_hook_registry(
    info: &AccountInfo,
    hook: &Pubkey,
    mint: &Pubkey,
    max: usize,
) -> Result<()> {
    require_keys_eq!(
        *info.key,
        hook_accounts_address(hook, mint).0,
        CompanionError::HookRegistry
    );
    require_keys_eq!(*info.owner, *hook, CompanionError::HookRegistry);
    let list: HookAccountList =
        decode_registry(info).ok_or_else(|| error!(CompanionError::HookRegistry))?;
    let extras = list
        .accounts
        .iter()
        .filter(|a| !is_the_launch(&a.source, mint))
        .count();
    require!(extras <= max, CompanionError::TooManyHookExtras);
    Ok(())
}

/// Phase 2's settings of a game, beside `CreateGameArgs` (`create_game_v2`): a jackpot's timer and
/// minimum buy, a streak's minimum streak and weight. All zero for a lottery. Each must equal what
/// the hook's kind header says (the hook applies them; the companion checks them again).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GameKindArgs {
    /// Jackpot: a round ends this long after its last qualifying buy (5 minutes to 30 days).
    pub timer_secs: u32,
    /// Jackpot: the least a qualifying buy delivers, in the token's base units (at least 1).
    pub min_tokens: u64,
    /// Streak: a holding shares in an epoch only if, by its end, it has sent nothing for this long
    /// (at most a year).
    pub min_streak_secs: u32,
    /// Streak: the least weight that shares (at least 1).
    pub min_weight: u64,
}

/// Whether `hook` is upgradeable by one of the protocol's keys (`HOOK_UPGRADE_AUTHORITIES`: Studio's
/// and the protocol's), from its ProgramData among `available`: at `PDA([hook], loader)`, owned by
/// the upgradeable loader (which alone writes accounts it owns at that address: they exist only
/// for a program deployed there), a ProgramData header naming one of the keys. The way a Studio
/// game hook, deployed under Studio's key, is taken without a status: Bordrless can upgrade it (to
/// a hook that issues no tickets) as well as block it. Not audited: its pots are capped.
fn upgradeable_by_the_protocol(available: &[AccountInfo], hook: &Pubkey) -> bool {
    let expected = Pubkey::find_program_address(&[hook.as_ref()], &BPF_LOADER_UPGRADEABLE_ID).0;
    let Some(info) = available.iter().find(|a| *a.key == expected) else {
        return false;
    };
    if *info.owner != BPF_LOADER_UPGRADEABLE_ID {
        return false;
    }
    let Ok(data) = info.try_borrow_data() else {
        return false;
    };
    data.len() >= 45
        && data[..4] == 3u32.to_le_bytes()
        && data[12] == 1
        && HOOK_UPGRADE_AUTHORITIES
            .iter()
            .any(|key| key.as_ref() == &data[13..45])
}

/// The settings of a game of `args.kind`, within its bounds (`BadGame` otherwise). A lottery's are
/// phase 1's, and takes no kind settings. A jackpot has no rounds (`round_secs` 0), no claim
/// windows or attempts; a streak's epochs are its rounds (an hour to 30 days), and its claim window
/// is the least time a closed epoch leaves for claims (5 minutes to half an epoch), with no
/// attempts.
fn check_game_args(args: &CreateGameArgs, k: &GameKindArgs) -> Result<()> {
    let pot_and_prize = (MIN_MIN_POT..=MAX_MIN_POT).contains(&args.min_pot)
        && (u64::from(MIN_PRIZE_BPS)..=BPS).contains(&u64::from(args.prize_bps));
    let ok = pot_and_prize
        && match args.kind {
            GameKind::Lottery => {
                *k == GameKindArgs::default()
                    && valid_round_secs(args.round_secs)
                    && (MIN_CLAIM_WINDOW..=MAX_CLAIM_WINDOW).contains(&args.claim_window_secs)
                    && (1..=MAX_ATTEMPTS).contains(&args.max_attempts)
                    // A draw's attempts take at most half a round: one made early in the round
                    // after its own keeps them all before its claims end with that round.
                    && u64::from(args.claim_window_secs)
                        * u64::from(args.max_attempts)
                        * u64::from(CLAIMS_PER_ROUND)
                        <= u64::from(args.round_secs)
            }
            GameKind::Jackpot => {
                args.round_secs == 0
                    && args.claim_window_secs == 0
                    && args.max_attempts == 0
                    && bordrless_game::valid_timer_secs(k.timer_secs)
                    && k.min_tokens >= 1
                    && k.min_streak_secs == 0
                    && k.min_weight == 0
            }
            GameKind::Streak => {
                valid_round_secs(args.round_secs)
                    && (MIN_CLAIM_WINDOW..=MAX_CLAIM_WINDOW).contains(&args.claim_window_secs)
                    && u64::from(args.claim_window_secs) * u64::from(CLAIMS_PER_ROUND)
                        <= u64::from(args.round_secs)
                    && args.max_attempts == 0
                    && k.timer_secs == 0
                    && k.min_tokens == 0
                    && k.min_streak_secs <= bordrless_game::streak::MAX_MIN_STREAK_SECS
                    && k.min_weight >= 1
            }
        };
    require!(ok, CompanionError::BadGame);
    Ok(())
}

/// The hook's kind header for a game of `kind` says what `k` says: a jackpot's timer and minimum
/// buy, with no buy yet; a streak's minimum streak and weight. Nothing for a lottery.
fn check_kind_header(state: &AccountInfo, kind: GameKind, k: &GameKindArgs) -> Result<()> {
    let ok = match kind {
        // A lottery's hook keeps ranges: never a jackpot's or a streak's state (a streak's
        // weights all start at ticket 0, so every holder would hold the drawn ticket).
        GameKind::Lottery => {
            let data = state.try_borrow_data()?;
            bordrless_game::JackpotHeader::parse(&data).is_none()
                && bordrless_game::StreakHeader::parse(&data).is_none()
        }
        GameKind::Jackpot => bordrless_game::JackpotHeader::parse(&state.try_borrow_data()?)
            // A fresh jackpot: the settings `k` says, no buy yet and no ended round remembered.
            .is_some_and(|j| j == bordrless_game::JackpotHeader::new(k.timer_secs, k.min_tokens)),
        GameKind::Streak => bordrless_game::StreakHeader::parse(&state.try_borrow_data()?)
            .is_some_and(|s| {
                s.min_streak_secs == k.min_streak_secs && s.min_weight == k.min_weight
            }),
    };
    require!(ok, CompanionError::HookState);
    Ok(())
}

/// `create_game(args)`: a lottery (phase 1's instruction, unchanged for a lottery; it also takes
/// a Studio hook upgradeable by the protocol's keys, its ProgramData passed as a remaining
/// account).
pub fn process_create_game(ctx: Context<CreateGame>, args: CreateGameArgs) -> Result<()> {
    require!(args.kind == GameKind::Lottery, CompanionError::BadGame);
    create_any_game(ctx, args, GameKindArgs::default(), false)
}

/// `create_game_v2(args, kind)`: a game of any kind, with its kind's settings.
pub fn process_create_game_v2(
    ctx: Context<CreateGame>,
    args: CreateGameArgs,
    kind: GameKindArgs,
) -> Result<()> {
    create_any_game(ctx, args, kind, true)
}

fn create_any_game(
    ctx: Context<CreateGame>,
    args: CreateGameArgs,
    k: GameKindArgs,
    v2: bool,
) -> Result<()> {
    let c = &ctx.accounts.companion;
    require!(!c.launched, CompanionError::AlreadyLaunched);
    require!(!c.is_game(), CompanionError::BadGame);
    require!(
        args.pot_bps > 0 && args.split.valid_with_pot(args.pot_bps),
        CompanionError::BadSplit
    );
    // A game coin's hook is its own, so it has no kit and holders can't be paid through one.
    require!(
        args.split.holders_bps == 0,
        CompanionError::HolderRewardsOff
    );
    // What a hook's status keeps out of the pot goes to the buyback: every game needs its limits.
    require!(
        c.max_buyback >= MIN_MAX_BUYBACK
            && (MIN_BUYBACK_INTERVAL..=MAX_BUYBACK_INTERVAL).contains(&c.buyback_interval),
        CompanionError::BadBuybackLimits
    );
    check_game_args(&args, &k)?;
    require!(
        args.hook != Pubkey::default()
            && args.hook != crate::ID
            && args.hook != oracle::ORAO_VRF_ID
            && !bordrless_launch::constants::PROTOCOL_PROGRAMS.contains(&args.hook),
        CompanionError::BadGame
    );
    let mint = ctx.accounts.mint.key();
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
    check_kind_header(&ctx.accounts.hook_state, args.kind, &k)?;
    let max_extras = if v2 {
        MAX_GAME_HOOK_EXTRAS_V2
    } else {
        MAX_GAME_HOOK_EXTRAS
    };
    check_hook_registry(&ctx.accounts.hook_registry, &args.hook, &mint, max_extras)?;
    // The hook: Bordrless's lottery hook, a hook the protocol has vetted (written a status for),
    // or one only the protocol's keys can upgrade (a Studio hook: its ProgramData among the
    // remaining accounts); never a blocked one. A hook nobody vetted could refuse only the
    // companion's own token moves (its buyback's transfer or burn) while holders trade as usual,
    // and strand the buyback with every pot a block or `retire` sends there; an immutable hook
    // needs a status. A hook without a status is not audited: its pots are capped at 10 SOL.
    let status = &ctx.accounts.hook_status;
    let terms = read_hook_terms(status, &args.hook)?;
    let vetted = args.hook == LOTTERY_HOOK_ID
        || *status.owner == crate::ID
        || upgradeable_by_the_protocol(ctx.remaining_accounts, &args.hook);
    require!(
        vetted && !terms.blocked,
        CompanionError::GameHookNotAccepted
    );
    let now = Clock::get()?.unix_timestamp;
    let first_round = round_of(now, args.round_secs);

    let g = &mut ctx.accounts.game;
    g.version = GAME_VERSION;
    g.bump = ctx.bumps.game;
    g.kind = args.kind;
    g.mint = mint;
    g.hook = args.hook;
    g.state_bump = state_bump;
    g.status_bump = ctx.bumps.hook_status;
    g.oracle_bump = Game::oracle_payer(&mint).1;
    g.round_secs = args.round_secs;
    g.min_pot = args.min_pot;
    g.prize_bps = args.prize_bps;
    g.claim_window_secs = args.claim_window_secs;
    g.max_attempts = args.max_attempts;
    g.created_at = now;
    g.status = DrawStatus::Idle;
    g.next_round = first_round;
    g.settled_at = 0;
    g.oracle_answered();
    g.timer_secs = k.timer_secs;
    g.min_tokens = k.min_tokens;
    g.paid_buys = 0;
    g.min_streak_secs = k.min_streak_secs;
    g.min_weight = k.min_weight;
    g.epoch_paid = 0;
    g.reserved = [0; 43];
    let game = g.key();

    let c = &mut ctx.accounts.companion;
    c.split = args.split;
    c.pot_bps = args.pot_bps;
    c.game_hook = args.hook;
    c.round_secs = args.round_secs;
    c.game_kind = args.kind;
    c.pot_locked = 0;
    emit_cpi!(GameCreated {
        companion: c.key(),
        game,
        mint,
        kind: args.kind,
        hook: args.hook,
        split: args.split,
        pot_bps: args.pot_bps,
        round_secs: args.round_secs,
        min_pot: args.min_pot,
        prize_bps: args.prize_bps,
        claim_window_secs: args.claim_window_secs,
        max_attempts: args.max_attempts,
        first_round,
    });
    if v2 {
        emit_cpi!(GameKindSet {
            game,
            mint,
            kind: args.kind,
            timer_secs: k.timer_secs,
            min_tokens: k.min_tokens,
            min_streak_secs: k.min_streak_secs,
            min_weight: k.min_weight,
            audited: terms.audited,
            pot_cap: terms.cap().unwrap_or(0),
        });
    }
    Ok(())
}

// ---- the steps ------------------------------------------------------------------------------------

/// Accounts of `draw`, `reveal`, `expire` and `retire`. Remaining, looked up by key: the hook's
/// state, the slot hashes sysvar, the oracle's request accounts and the bridge's `unwrap_sol`
/// accounts (`draw`), the request (`reveal`, `expire`); none for `retire`.
#[event_cpi]
#[derive(Accounts)]
pub struct GameStep<'info> {
    /// Whoever sends it; paid the bounty.
    #[account(mut)]
    pub cranker: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked).
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    #[account(mut, seeds = [GAME_SEED, companion.mint.as_ref()], bump = game.bump)]
    pub game: Box<Account<'info, Game>>,
    /// CHECK: the game hook's status (seeds-checked); it need not exist.
    #[account(seeds = [HOOK_STATUS_SEED, game.hook.as_ref()], bump = game.status_bump)]
    pub hook_status: UncheckedAccount<'info>,
    /// CHECK: the oracle payer (seeds-checked): system-owned, no data.
    #[account(mut, seeds = [ORACLE_SEED, companion.mint.as_ref()], bump = game.oracle_bump)]
    pub oracle_payer: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Accounts of `claim_prize`. Remaining: the winner (the holding's owner, writable; paid the prize)
/// and the bridge's `unwrap_sol` accounts.
#[event_cpi]
#[derive(Accounts)]
pub struct ClaimPrize<'info> {
    /// Whoever sends it; paid the bounty.
    #[account(mut)]
    pub cranker: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked).
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    #[account(mut, seeds = [GAME_SEED, companion.mint.as_ref()], bump = game.bump)]
    pub game: Box<Account<'info, Game>>,
    /// CHECK: the game hook's status (seeds-checked); it need not exist.
    #[account(seeds = [HOOK_STATUS_SEED, game.hook.as_ref()], bump = game.status_bump)]
    pub hook_status: UncheckedAccount<'info>,
    #[account(address = launch_client::launch_address(&companion.mint))]
    pub launch: Box<Account<'info, Launch>>,
    /// CHECK: the holding said to win: read as the token program's (owner and discriminator),
    /// of this mint, at its owner's address.
    pub holding: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// The status a step applies, after the launch. Under a blocked hook the step only sends the pot
/// to the buyback and ends any draw (answers `None`: the step stops there and succeeds); otherwise
/// a pot above a cap lowered since is trimmed to it, and the step goes on under the terms it
/// answers.
pub(crate) fn apply_status(
    event_authority: &AccountInfo,
    companion_key: Pubkey,
    game_key: Pubkey,
    c: &mut Companion,
    g: &mut Game,
    hook_status: &AccountInfo,
) -> Result<Option<HookTerms>> {
    require!(c.launched, CompanionError::NotLaunched);
    let terms = read_hook_terms(hook_status, &g.hook)?;
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

/// The lottery's steps (`draw`, `reveal`, `claim_prize`, `expire`) run only on
/// a lottery: a kind added later (appended to `GameKind`) brings steps of its own, and none of
/// these may read its state as a lottery's.
fn lottery(g: &Game) -> Result<()> {
    require!(g.kind == GameKind::Lottery, CompanionError::NotAGame);
    Ok(())
}

/// The draw's round ends unpaid: the pot stays (or went to the buyback, blocked).
pub(crate) fn rolled_over(
    event_authority: &AccountInfo,
    game_key: Pubkey,
    c: &Companion,
    g: &mut Game,
    reason: RolloverReason,
) -> Result<()> {
    g.status = DrawStatus::Idle;
    g.rollovers = g.rollovers.saturating_add(1);
    emit_event(
        event_authority,
        &RolledOver {
            game: game_key,
            mint: g.mint,
            round: g.round,
            reason,
            pending_pot: c.pending_pot,
        },
    )
}

/// What a request cost, and whether the companion made it (then `paid_streak` is the game's
/// count of paid requests since ORAO last answered one, this one included).
struct Made {
    made: bool,
    fee: u64,
    top_up: u64,
    bounty: u64,
    paid_streak: u8,
}

/// The oracle's circuit breaker, before the pot pays for a request for the draw of `round`:
/// answers how many requests the pot has paid for since ORAO last answered one of this game's,
/// before this one. When the pot's last paid request (`Game.paid_seed`, its account among `all`,
/// which must be passed) is answered, that is 0. While it is not (still pending, in a form this
/// program can't read, or gone), the pot pays for no new request (`None`) until the draw is
/// `Game::oracle_backoff_rounds` rounds after that one's: an oracle that takes requests but never
/// answers (one of ORAO's three signers offline is enough), or answers unreadably, costs the pot a
/// request now and then, not one every round.
fn unanswered_streak(all: &[AccountInfo], g: &Game, round: u32) -> Result<Option<u8>> {
    if !g.has_paid_request() {
        return Ok(Some(0));
    }
    let last = find(all, &oracle::request_address(&g.paid_seed))?;
    if let Ok(Some(_)) = oracle::randomness(last, &g.paid_seed) {
        return Ok(Some(0));
    }
    Ok(g.oracle_backoff_over_for(round).then_some(g.paid_streak))
}

/// What the pot's own request for the draw of `round` takes, when the pot can pay for it now.
struct Payment {
    /// The breaker's count before this request ([`unanswered_streak`]).
    streak: u8,
    terms: oracle::Terms,
    /// What the pot sends the oracle payer, and the sender's bounty on it.
    top_up: u64,
    bounty: u64,
}

/// Whether the pot can pay for the oracle's request for the draw of `round` now, and what it
/// takes: the breaker allows it ([`unanswered_streak`]), ORAO's terms are readable and its fee is
/// within `MAX_REQUEST_FEE` (`oracle::terms`), and the pot holds the top-up and the bounty. `None`
/// when it can't, for any of these reasons; an error only for an account missing from `all` (the
/// network state, or the pot's last paid request while `Game.paid_seed` is set), so leaving one
/// out never rolls a round over. `draw` asks before committing the round's seed: a seed is only
/// ever committed with its request.
fn pot_payment(
    all: &[AccountInfo],
    c: &Companion,
    g: &Game,
    round: u32,
    oracle_payer: &AccountInfo,
) -> Result<Option<Payment>> {
    let Some(streak) = unanswered_streak(all, g, round)? else {
        return Ok(None);
    };
    let Ok(terms) = oracle::terms(find(all, &oracle::NETWORK_STATE)?) else {
        return Ok(None);
    };
    let top_up = oracle::top_up(oracle_payer.lamports(), terms.cost()?)?;
    let bounty = bps_of(top_up, u64::from(c.bounty_bps));
    let spend = top_up
        .checked_add(bounty)
        .ok_or(CompanionError::MathOverflow)?;
    Ok((spend <= c.pending_pot).then_some(Payment {
        streak,
        terms,
        top_up,
        bounty,
    }))
}

/// Commits the seed of the draw of `round` (ticket total `total`), the round's only one: made from
/// slot `slot`, one of the last `oracle::SEED_SLOTS` before `current` (its hash read from the slot
/// hashes sysvar among `all`), so nobody could have learnt its answer before this instruction.
/// [`request_seed`] asks the oracle for it in the same instruction.
fn commit(
    all: &[AccountInfo],
    g: &mut Game,
    round: u32,
    total: u64,
    slot: u64,
    current: u64,
    now: i64,
) -> Result<()> {
    let hash = oracle::recent_slot_hash(find(all, &oracle::SLOT_HASHES)?, slot, current)?;
    let seed = oracle::draw_seed(&g.mint, round, 0, slot, &hash);
    g.round = round;
    g.total = total;
    g.n = 0;
    g.seed = seed;
    g.request = oracle::request_address(&seed);
    g.committed_at = now;
    g.randomness = [0; 64];
    g.revealed_at = 0;
    Ok(())
}

/// Asks the oracle for the seed just committed, the pot paying what [`pot_payment`] found it can
/// (`pay`): the oracle payer topped up to the request's cost (it keeps its rent-exempt minimum), the
/// request made in its name, the sender paid `bounty_bps` of the top-up. A pending request ORAO
/// already holds for the seed (anyone's, v2 or v1) is adopted instead, and nothing is paid: it was
/// made before anyone could know its answer, its randomness is the same, and nobody can block a
/// draw by requesting its seed first. One ORAO has already answered is refused (`StaleSeed`): its
/// answer was public before the draw. The pot is not debited here.
fn request_seed<'info>(
    all: &[AccountInfo<'info>],
    c: &Companion,
    g: &Game,
    pay: Payment,
    creator: Pubkey,
    oracle_payer: &AccountInfo<'info>,
    cranker: &AccountInfo<'info>,
) -> Result<Made> {
    let request = find(all, &g.request)?;
    if oracle::is_requested(request) {
        // Checked to be ORAO's request for exactly this seed, and still pending.
        require!(
            oracle::randomness(request, &g.seed)?.is_none(),
            CompanionError::StaleSeed
        );
        return Ok(Made {
            made: false,
            fee: 0,
            top_up: 0,
            bounty: 0,
            paid_streak: g.paid_streak,
        });
    }
    let Payment {
        streak,
        terms,
        top_up,
        bounty,
    } = pay;
    let seeds = CreatorSeeds::new(c.mint, c.creator_bump);
    pay_sol(all, &seeds, creator, oracle_payer, top_up)?;
    let ix = oracle::request_ix(*oracle_payer.key, &terms, g.seed);
    let bump = [g.oracle_bump];
    invoke_built(&ix, all, &[&[ORACLE_SEED, c.mint.as_ref(), &bump]])?;
    // What the oracle made is a pending request for the seed (the info sees the callee's writes).
    require!(
        oracle::randomness(request, &g.seed)?.is_none(),
        CompanionError::OracleAccount
    );
    pay_sol(all, &seeds, creator, cranker, bounty)?;
    Ok(Made {
        made: true,
        fee: terms.fee,
        top_up,
        bounty,
        paid_streak: streak.saturating_add(1),
    })
}

/// Books a request: the pot pays for it (and the breaker records it, when the companion made it),
/// the draw waits for the oracle's answer, and the prize is fixed (`prize_bps` of the pot now).
fn book_request(c: &mut Companion, g: &mut Game, r: &Made, now: i64) -> Result<()> {
    let spend = r
        .top_up
        .checked_add(r.bounty)
        .ok_or(CompanionError::MathOverflow)?;
    c.pending_pot = c
        .pending_pot
        .checked_sub(spend)
        .ok_or(CompanionError::MathOverflow)?;
    c.bounties_total = c.bounties_total.saturating_add(r.bounty);
    if r.made {
        g.paid_seed = g.seed;
        g.paid_round = g.round;
        g.paid_streak = r.paid_streak;
    }
    g.status = DrawStatus::Requested;
    g.requested_at = now;
    g.draws = g.draws.saturating_add(1);
    g.oracle_total = g.oracle_total.saturating_add(r.top_up);
    g.prize = bps_of(c.pending_pot, u64::from(g.prize_bps));
    Ok(())
}

/// `draw(round, slot)`: once `round` is over (the clock is in the next round: the draw is always for
/// the round that just ended, and each round is drawn at most once), with no draw in progress and
/// at least `min_pot` in the pot (or the hook's cap, when lower; or `MIN_MIN_POT` once the game is
/// dormant, its pot having paid no prize for `Game::dormant_secs`).
///
/// - After `Game::last_draw` (too late to leave the reveal its margin and a whole claim window
///   before the claims end), the round rolls over (`Late`).
/// - Its ticket total is read from the hook's header: with no tickets (or a total the header has
///   forgotten) the round rolls over.
/// - When the pot can't pay for the oracle's request now ([`pot_payment`]: the breaker, ORAO's
///   terms, the pot), the round rolls over too (`OracleUnpaid`), no seed committed.
/// - Else, in this one instruction: the seed `sha256("bordrless-draw", mint, round, 0, slot, slot's
///   hash)` is committed, `slot` being one of the last `oracle::SEED_SLOTS` slots
///   ([`commit`]); the oracle is asked for it, the pot paying and the sender paid `bounty_bps` of
///   the top-up, or ORAO's pending request for it adopted ([`request_seed`]); and the prize is
///   fixed. A seed is never on chain without its request, so nobody can preview its answer and
///   then choose whether it is drawn.
///
/// Remaining: the hook's state, the slot hashes sysvar, the oracle's request accounts for the seed
/// (its network state, its treasury, the request, its program), the bridge's `unwrap_sol` accounts
/// and, while `Game.paid_seed` is set, its request (`oracle::request_address(paid_seed)`).
pub fn process_draw<'info>(
    ctx: Context<'info, GameStep<'info>>,
    round: u32,
    slot: u64,
) -> Result<()> {
    lottery(&ctx.accounts.game)?;
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let (companion_key, game_key) = (ctx.accounts.companion.key(), ctx.accounts.game.key());
    let event_authority = ctx.accounts.event_authority.to_account_info();
    let Some(terms) = apply_status(
        &event_authority,
        companion_key,
        game_key,
        &mut ctx.accounts.companion,
        &mut ctx.accounts.game,
        &ctx.accounts.hook_status,
    )?
    else {
        return Ok(());
    };
    let (c, g) = (&ctx.accounts.companion, &ctx.accounts.game);
    require!(g.status == DrawStatus::Idle, CompanionError::DrawPending);
    let current = round_of(now, g.round_secs);
    require!(
        current > 0 && round == current - 1 && round >= g.next_round,
        CompanionError::RoundNotOver
    );
    // At least the minimum, or the cap when the cap is lower: a pot full at its cap is drawn. A
    // dormant game's minimum is the floor: a pot its fees no longer grow is still paid out.
    require!(
        c.pending_pot >= terms.draw_threshold(g.min_pot_at(now, c.launched_at)),
        CompanionError::PotTooSmall
    );
    // Too late in the round after it for the reveal and a whole claim window: the round rolls
    // over undrawn.
    if now > g.last_draw(round) {
        let c = &ctx.accounts.companion;
        let g = &mut ctx.accounts.game;
        g.next_round = round.checked_add(1).ok_or(CompanionError::MathOverflow)?;
        g.round = round;
        g.total = 0;
        return rolled_over(&event_authority, game_key, c, g, RolloverReason::Late);
    }
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    // The round's tickets, from the hook's header: final, since every write after the round
    // goes to a later one.
    let state = state_address_at(&g.hook, &g.mint, g.state_bump)
        .ok_or_else(|| error!(CompanionError::HookState))?;
    let header = read_state_at(find(&all, &state)?, &g.hook, &g.mint, g.state_bump)
        .map_err(|_| error!(CompanionError::HookState))?;
    require!(header.round_secs == g.round_secs, CompanionError::HookState);
    let total = header.total_of(round);
    // A seed is committed only with its request, so only when the pot can pay for one now.
    let pay = match total {
        Some(t) if t > 0 => pot_payment(&all, c, g, round, &ctx.accounts.oracle_payer)?,
        _ => None,
    };
    let c = &ctx.accounts.companion;
    let g = &mut ctx.accounts.game;
    g.next_round = round.checked_add(1).ok_or(CompanionError::MathOverflow)?;
    let (total, pay) = match (total, pay) {
        (Some(total), Some(pay)) if total > 0 => (total, pay),
        (Some(total), _) if total > 0 => {
            g.round = round;
            g.total = total;
            return rolled_over(
                &event_authority,
                game_key,
                c,
                g,
                RolloverReason::OracleUnpaid,
            );
        }
        _ => {
            g.round = round;
            g.total = 0;
            let reason = if total.is_none() {
                RolloverReason::RoundForgotten
            } else {
                RolloverReason::NoTickets
            };
            return rolled_over(&event_authority, game_key, c, g, reason);
        }
    };
    commit(&all, g, round, total, slot, clock.slot, now)?;
    emit_cpi!(DrawCommitted {
        game: game_key,
        mint: g.mint,
        round,
        total,
        n: 0,
        seed: g.seed,
        request: g.request,
        slot,
        cranker: ctx.accounts.cranker.key(),
    });
    let (c, g) = (&ctx.accounts.companion, &ctx.accounts.game);
    let r = request_seed(
        &all,
        c,
        g,
        pay,
        ctx.accounts.creator.key(),
        &ctx.accounts.oracle_payer,
        &ctx.accounts.cranker,
    )?;
    let c = &mut ctx.accounts.companion;
    let g = &mut ctx.accounts.game;
    book_request(c, g, &r, now)?;
    emit_cpi!(DrawRequested {
        game: game_key,
        mint: g.mint,
        round: g.round,
        total: g.total,
        n: g.n,
        seed: g.seed,
        request: g.request,
        made: r.made,
        fee: r.fee,
        top_up: r.top_up,
        bounty: r.bounty,
        paid_streak: g.paid_streak,
        prize: g.prize,
        pending_pot: c.pending_pot,
        cranker: ctx.accounts.cranker.key(),
    });
    Ok(())
}

/// `reveal`: the oracle's answer for the draw's seed (ORAO's request account at the seed's
/// address) stored as the draw's randomness, before the draw's claims end. The claim windows start
/// now (those that open before the claims end; the last one open is cut there). Remaining: the
/// request.
pub fn process_reveal<'info>(ctx: Context<'info, GameStep<'info>>) -> Result<()> {
    lottery(&ctx.accounts.game)?;
    let now = Clock::get()?.unix_timestamp;
    let (companion_key, game_key) = (ctx.accounts.companion.key(), ctx.accounts.game.key());
    let event_authority = ctx.accounts.event_authority.to_account_info();
    if apply_status(
        &event_authority,
        companion_key,
        game_key,
        &mut ctx.accounts.companion,
        &mut ctx.accounts.game,
        &ctx.accounts.hook_status,
    )?
    .is_none()
    {
        return Ok(());
    }
    let g = &ctx.accounts.game;
    require!(g.status == DrawStatus::Requested, CompanionError::NoDraw);
    require!(now < g.claims_end(), CompanionError::DrawLate);
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let randomness = oracle::randomness(find(&all, &g.request)?, &g.seed)?
        .ok_or_else(|| error!(CompanionError::OracleNotFulfilled))?;
    let g = &mut ctx.accounts.game;
    g.randomness = randomness;
    g.revealed_at = now;
    g.status = DrawStatus::Revealed;
    // ORAO answers this game's requests: the pot may pay for the next one at once.
    g.oracle_answered();
    emit_cpi!(DrawRevealed {
        game: game_key,
        mint: g.mint,
        round: g.round,
        seed: g.seed,
        randomness,
        cranker: ctx.accounts.cranker.key(),
    });
    Ok(())
}

/// `claim_prize(attempt)`: during attempt `attempt`'s window (`[revealed_at + attempt * window,
/// revealed_at + (attempt + 1) * window)`, `attempt < max_attempts`, and before the draw's claims
/// end with the round after the drawn one; nobody chooses which attempt is open), the holding
/// whose range for the draw's round holds ticket
/// `draw_index(randomness, attempt, total)` wins, when it is of this mint, its owner may hold
/// tickets (a wallet: not the launch, its pool or the creator address) and its range is no larger
/// than its balance. The prize (`prize_bps` of the pot at the request, at most the pot now) is
/// paid to its owner as SOL, less the sender's bounty.
pub fn process_claim_prize<'info>(
    ctx: Context<'info, ClaimPrize<'info>>,
    attempt: u8,
) -> Result<()> {
    lottery(&ctx.accounts.game)?;
    let now = Clock::get()?.unix_timestamp;
    let (companion_key, game_key) = (ctx.accounts.companion.key(), ctx.accounts.game.key());
    let event_authority = ctx.accounts.event_authority.to_account_info();
    if apply_status(
        &event_authority,
        companion_key,
        game_key,
        &mut ctx.accounts.companion,
        &mut ctx.accounts.game,
        &ctx.accounts.hook_status,
    )?
    .is_none()
    {
        return Ok(());
    }
    let (c, g) = (&ctx.accounts.companion, &ctx.accounts.game);
    require!(g.status == DrawStatus::Revealed, CompanionError::NoDraw);
    // Never after the draw's claims end (the round after the drawn one): from then on a holding
    // written in both rounds since may have forgotten its range of the drawn round.
    require!(now < g.claims_end(), CompanionError::DrawLate);
    let opens = g
        .attempt_opens(attempt)
        .ok_or(CompanionError::MathOverflow)?;
    let closes = g
        .attempt_closes(attempt)
        .ok_or(CompanionError::MathOverflow)?;
    require!(
        attempt < g.max_attempts && now >= opens && now < closes,
        CompanionError::AttemptClosed
    );
    let ticket = draw_index(&g.randomness, u32::from(attempt), g.total)
        .ok_or_else(|| error!(CompanionError::NoDraw))?;
    let mint = c.mint;
    let creator = ctx.accounts.creator.key();
    let holding_key = ctx.accounts.holding.key();
    let holding = token_client::read_holding(&ctx.accounts.holding)?;
    require_keys_eq!(holding.mint, mint, CompanionError::WrongHolding);
    require_keys_eq!(
        holding_key,
        token_client::holding_address(&mint, &holding.owner),
        CompanionError::WrongHolding
    );
    let launch = &ctx.accounts.launch;
    require!(
        eligible(&holding.owner, &[launch.key(), launch.pool, creator]),
        CompanionError::NotEligible
    );
    require!(
        wins(&holding.hook_data, g.round, ticket, holding.amount),
        CompanionError::NotTheWinner
    );
    let prize = g.prize.min(c.pending_pot);
    require!(prize > 0, CompanionError::NothingToDo);
    let bounty = bps_of(prize, u64::from(c.bounty_bps));
    let paid = prize - bounty;
    let seeds = CreatorSeeds::new(mint, c.creator_bump);
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let winner = find(&all, &holding.owner)?.clone();
    pay_sol(&all, &seeds, creator, &winner, paid)?;
    pay_sol(
        &all,
        &seeds,
        creator,
        &ctx.accounts.cranker.to_account_info(),
        bounty,
    )?;
    let c = &mut ctx.accounts.companion;
    c.pending_pot -= prize;
    c.bounties_total = c.bounties_total.saturating_add(bounty);
    let g = &mut ctx.accounts.game;
    g.status = DrawStatus::Idle;
    g.prizes_paid = g.prizes_paid.saturating_add(1);
    g.prizes_total = g.prizes_total.saturating_add(paid);
    g.last_winner = holding.owner;
    g.settled_at = now;
    emit_cpi!(PrizePaid {
        game: game_key,
        mint,
        round: g.round,
        attempt,
        ticket,
        holding: holding_key,
        winner: holding.owner,
        prize: paid,
        bounty,
        pending_pot: c.pending_pot,
        cranker: ctx.accounts.cranker.key(),
    });
    Ok(())
}

/// `expire`: a draw that can't go on.
///
/// - Revealed, and every attempt's window has passed unclaimed: the round rolls over (`NoClaim`);
///   or the draw's claims ended first (`Late`).
/// - Requested: the round's seed is final, so ORAO's answer is waited for until the draw's claims
///   end, however late (`reveal` takes it until then), and never replaced by a new seed. Once the
///   claims end the round rolls over: `OracleSilent` while the request is still pending, `Late`
///   when it was answered but nobody revealed it in time. An answer in a form the companion can't
///   read rolls the round over at once (`OracleUnreadable`): waiting would not mend it.
///
/// (A draw is never left committed without its request: `draw` makes both at once.) Remaining: the
/// draw's request (`Game.request`).
pub fn process_expire<'info>(ctx: Context<'info, GameStep<'info>>) -> Result<()> {
    lottery(&ctx.accounts.game)?;
    let now = Clock::get()?.unix_timestamp;
    let (companion_key, game_key) = (ctx.accounts.companion.key(), ctx.accounts.game.key());
    let event_authority = ctx.accounts.event_authority.to_account_info();
    if apply_status(
        &event_authority,
        companion_key,
        game_key,
        &mut ctx.accounts.companion,
        &mut ctx.accounts.game,
        &ctx.accounts.hook_status,
    )?
    .is_none()
    {
        return Ok(());
    }
    let g = &ctx.accounts.game;
    let late = now >= g.claims_end();
    let reason = match g.status {
        // `Committed` is never set: `draw` requests its seed in the same instruction.
        DrawStatus::Idle | DrawStatus::Committed => return err!(CompanionError::NoDraw),
        DrawStatus::Revealed => {
            let all_passed = g
                .attempt_opens(g.max_attempts)
                .ok_or(CompanionError::MathOverflow)?;
            if now >= all_passed {
                RolloverReason::NoClaim
            } else if late {
                RolloverReason::Late
            } else {
                return err!(CompanionError::NotDue);
            }
        }
        DrawStatus::Requested => {
            let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
            // The request was booked only once read as ORAO's pending request for the seed.
            match oracle::randomness(find(&all, &g.request)?, &g.seed) {
                // Answered: reveal it; nobody did before the claims ended.
                Ok(Some(_)) if !late => return err!(CompanionError::OracleFulfilled),
                Ok(Some(_)) => RolloverReason::Late,
                // Still pending: ORAO's answer counts whenever it lands before the claims end.
                Ok(None) if !late => return err!(CompanionError::NotDue),
                Ok(None) => RolloverReason::OracleSilent,
                // ORAO answered in a form this program does not know (a changed layout), which
                // no wait would mend.
                Err(_) => RolloverReason::OracleUnreadable,
            }
        }
    };
    let c = &ctx.accounts.companion;
    let g = &mut ctx.accounts.game;
    rolled_over(&event_authority, game_key, c, g, reason)
}

/// `retire`: the pot of a game that has paid no prize for `RETIRE_DORMANT_PERIODS` dormant periods
/// (`Game::retirable_at`: 60 days, or 8 rounds when longer, since the launch or the pot's last
/// prize or retirement), with no draw in progress, sent to the buyback. It pays nobody, its sender
/// included (as a block does, owner decision c), and needs no status: a pot nobody can win (below
/// the floor a dormant draw takes, under an oracle that can't be paid or has stopped answering,
/// with nobody claiming) is never locked for ever, audited hook or not. The game goes on: later
/// fees fill the pot again, and its clock restarts now.
pub fn process_retire<'info>(ctx: Context<'info, GameStep<'info>>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let (companion_key, game_key) = (ctx.accounts.companion.key(), ctx.accounts.game.key());
    let event_authority = ctx.accounts.event_authority.to_account_info();
    if apply_status(
        &event_authority,
        companion_key,
        game_key,
        &mut ctx.accounts.companion,
        &mut ctx.accounts.game,
        &ctx.accounts.hook_status,
    )?
    .is_none()
    {
        return Ok(());
    }
    if ctx.accounts.game.kind == GameKind::Streak {
        // A streak epoch whose claims have ended releases what it still held: it rolls over.
        crate::instructions::kinds::end_epoch_if_over(
            &mut ctx.accounts.companion,
            &mut ctx.accounts.game,
            now,
        );
    }
    let (c, g) = (&ctx.accounts.companion, &ctx.accounts.game);
    // Never mid-draw: a draw's prize is the pot's, until it is paid or rolls over (and a streak
    // epoch's pot is its holders', until its claims end).
    require!(g.status == DrawStatus::Idle, CompanionError::DrawPending);
    require!(now >= g.retirable_at(c.launched_at), CompanionError::NotDue);
    // Nor before a jackpot round or a streak epoch the pot can pay now is settled or closed
    // (the hook's state is passed for those kinds).
    if g.kind != GameKind::Lottery {
        let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
        let terms = read_hook_terms(&ctx.accounts.hook_status, &g.hook)?;
        require!(
            !crate::instructions::kinds::prize_due(&all, c, g, &terms, now)?,
            CompanionError::DrawPending
        );
    }
    let idle_since = g.idle_since(c.launched_at);
    let lamports = c.pending_pot;
    require!(lamports > 0, CompanionError::NothingToDo);
    let c = &mut ctx.accounts.companion;
    c.pending_buyback = c
        .pending_buyback
        .checked_add(lamports)
        .ok_or(CompanionError::MathOverflow)?;
    c.pending_pot = 0;
    let g = &mut ctx.accounts.game;
    g.settled_at = now;
    emit_cpi!(PotRetired {
        game: game_key,
        mint: g.mint,
        lamports,
        idle_since,
        pending_buyback: c.pending_buyback,
        cranker: ctx.accounts.cranker.key(),
    });
    Ok(())
}

// ---- burn_stranded ---------------------------------------------------------------------------------

/// Accounts of `burn_stranded`. Remaining: the bridge's `unwrap_sol` accounts.
#[event_cpi]
#[derive(Accounts)]
pub struct BurnStranded<'info> {
    /// Whoever sends it (paid nothing).
    pub cranker: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked).
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    /// CHECK: the game hook's status (seeds-checked; owner and contents checked in the handler).
    #[account(seeds = [HOOK_STATUS_SEED, companion.game_hook.as_ref()], bump)]
    pub hook_status: UncheckedAccount<'info>,
    /// CHECK: the incinerator (address-checked): what it receives is burned.
    #[account(mut, address = INCINERATOR)]
    pub incinerator: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// `burn_stranded`: a blocked game hook's buyback that no buyback has spent, or waited on, for
/// `STRANDED_SECS` (or `STRANDED_INTERVALS` buyback intervals, when longer), burned as SOL: all of
/// it unwrapped and sent to the incinerator. The wait runs from the latest of the launch, the last
/// buyback, the reference price's last move, the status's last write, the last burn or move of the
/// pot into the buyback, and the last fee claim that credited the buyback at least what it held.
/// It pays nobody, its sender included (owner decision c: a blocked game's pot pays no person).
///
/// A pot no step had moved yet (every game step and fee claim moves it under a block) is moved to
/// the buyback instead, and nothing is burned. Whichever call moves a blocked game's pot restarts
/// the wait (`Companion.stranded_burned_at`, set by `enforce_terms`), so that pot gets the whole
/// wait for a buyback to spend it, even when a step that moves it and a burn share a transaction.
///
/// The buyback is a blocked pot's only other exit, and it moves the game's token: a hook that
/// refuses the companion's own moves (the buyback's transfer to the creator address, or its burn),
/// or that grew past what a transaction can carry, would strand the pot and the buyback share for
/// ever. The wait leaves keepers every chance to land the buybacks a working hook allows:
///
/// - each landed buyback restarts it;
/// - so does each wait that moves the reference price (a buyback finding the price more than
///   `MAX_PREMIUM_BPS` above the reference, which it raises once an interval). A working hook's
///   buyback that only waits for the reference to catch up with a risen price is never burned;
///   the burn comes only once buybacks are attempted at the reference and still land nothing;
/// - so does each burn, so a buyback share credited since (by a fee claim in the very same
///   transaction, say) gets the whole wait before it can be burned;
/// - so does a fee claim that credits the buyback at least what it held
///   (`Companion::restart_stranded_wait`): once a working hook's buybacks have spent everything,
///   nothing buys for a while and the wait runs on, so the next claim's share would otherwise be
///   burned in its own transaction, or the next, before any buyback was tried. A refusing hook's
///   buyback never empties, so only a credit at least as large as all it holds (fees paid to the
///   game, every lamport of them burned with the rest) can put its burn off, and the next such
///   credit must be twice that: credits smaller than what it holds never do;
/// - and so does the pot's move into the buyback, by whichever call.
pub fn process_burn_stranded<'info>(ctx: Context<'info, BurnStranded<'info>>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let c = &ctx.accounts.companion;
    require!(c.launched, CompanionError::NotLaunched);
    require!(c.is_game(), CompanionError::NotAGame);
    let info = &ctx.accounts.hook_status;
    require_keys_eq!(*info.owner, crate::ID, CompanionError::HookNotBlocked);
    let status = HookStatus::try_deserialize(&mut &info.try_borrow_data()?[..])?;
    require_keys_eq!(status.hook, c.game_hook, CompanionError::HookStatusAccount);
    require!(status.blocked, CompanionError::HookNotBlocked);
    let since = c.stranded_since(status.updated_at);
    require!(
        now >= c.stranded_at(status.updated_at),
        CompanionError::NotDue
    );
    let (mint, creator, companion_key) = (c.mint, ctx.accounts.creator.key(), c.key());
    let seeds = CreatorSeeds::new(mint, c.creator_bump);
    let c = &mut ctx.accounts.companion;
    // A pot no step had moved: moved now, which restarts the wait (`enforce_terms`), and never
    // burned in the call that moved it.
    let from_pot = enforce_terms(c, &status.terms(), now)?;
    if from_pot > 0 {
        emit_cpi!(PotToBuyback {
            companion: companion_key,
            lamports: from_pot,
            blocked: true,
            pending_pot: c.pending_pot,
        });
        return Ok(());
    }
    let lamports = c.pending_buyback;
    require!(lamports > 0, CompanionError::NothingToDo);
    c.pending_buyback = 0;
    // The next burn waits its whole period from now.
    c.stranded_burned_at = now;
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let incinerator = ctx.accounts.incinerator.to_account_info();
    pay_sol(&all, &seeds, creator, &incinerator, lamports)?;
    emit_cpi!(StrandedBurned {
        companion: companion_key,
        lamports,
        since,
        cranker: ctx.accounts.cranker.key(),
    });
    Ok(())
}

// ---- set_hook_status -------------------------------------------------------------------------------

/// Arguments of `set_hook_status`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct HookStatusArgs {
    /// Audited with the companion: its games' pots are not capped. Clears a block. Final: an
    /// audited hook stays audited (so it can never be blocked).
    pub audited: bool,
    /// The most each of its games' pots holds while it is not audited: `MIN_POT_CAP` (0.1 SOL)
    /// to `DEFAULT_POT_CAP` (10 SOL). Not read once audited.
    pub pot_cap: u64,
    /// Its games' pot share and pots go to the buyback, and no prize is paid. Only for a hook
    /// that is not audited.
    pub blocked: bool,
}

/// Accounts of `set_hook_status`.
#[event_cpi]
#[derive(Accounts)]
#[instruction(hook: Pubkey)]
pub struct SetHookStatus<'info> {
    /// The companion program's upgrade authority (the protocol's), paying the rent the first time.
    #[account(mut)]
    pub authority: Signer<'info>,
    /// CHECK: this program's ProgramData (address and owner checked in the handler).
    pub program_data: UncheckedAccount<'info>,
    /// What the protocol says of `hook`, made the first time.
    #[account(init_if_needed, payer = authority, space = HookStatus::LEN, seeds = [HOOK_STATUS_SEED, hook.as_ref()], bump)]
    pub hook_status: Box<Account<'info, HookStatus>>,
    pub system_program: Program<'info, System>,
}

/// The upgrade authority of this program, read from its ProgramData account (address, owner and
/// layout checked).
fn upgrade_authority(program_data: &AccountInfo) -> Result<Option<Pubkey>> {
    let expected =
        Pubkey::find_program_address(&[crate::ID.as_ref()], &BPF_LOADER_UPGRADEABLE_ID).0;
    require_keys_eq!(
        *program_data.key,
        expected,
        CompanionError::NotProtocolAuthority
    );
    require_keys_eq!(
        *program_data.owner,
        BPF_LOADER_UPGRADEABLE_ID,
        CompanionError::NotProtocolAuthority
    );
    let data = program_data.try_borrow_data()?;
    require!(
        data.len() >= 45 && data[..4] == [3, 0, 0, 0],
        CompanionError::NotProtocolAuthority
    );
    if data[12] == 0 {
        return Ok(None);
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&data[13..45]);
    Ok(Some(Pubkey::new_from_array(key)))
}

/// `set_hook_status(hook, args)`: what the protocol says of `hook`, written by this program's
/// upgrade authority only (owner decision c):
///
/// - an audit is final: a status once audited stays audited, so an audited hook can never be
///   blocked, not even through an un-audit first (in the same transaction or any other);
/// - an audit clears a block; a block needs a hook that is not audited and stays until an audit
///   lifts it;
/// - a hook not audited keeps a pot cap of 0.1 to 10 SOL: the protocol may lower the cap, never
///   lift it without an audit.
pub fn process_set_hook_status(
    ctx: Context<SetHookStatus>,
    hook: Pubkey,
    args: HookStatusArgs,
) -> Result<()> {
    let authority = ctx.accounts.authority.key();
    require!(
        upgrade_authority(&ctx.accounts.program_data)? == Some(authority),
        CompanionError::NotProtocolAuthority
    );
    let s = &mut ctx.accounts.hook_status;
    // A status made just now reads all zeros: not audited, not blocked.
    let made_before = s.version != 0;
    if made_before {
        require_keys_eq!(s.hook, hook, CompanionError::HookStatusAccount);
    }
    let (was_audited, was_blocked) = (s.audited, s.blocked);
    // An audit is final.
    if was_audited {
        require!(args.audited, CompanionError::BadHookStatus);
    }
    if args.audited {
        require!(!args.blocked, CompanionError::BadHookStatus);
    } else {
        require!(
            (MIN_POT_CAP..=DEFAULT_POT_CAP).contains(&args.pot_cap),
            CompanionError::BadHookStatus
        );
    }
    if args.blocked {
        require!(!was_audited, CompanionError::BadHookStatus);
    }
    if was_blocked && !args.blocked {
        require!(args.audited, CompanionError::BadHookStatus);
    }
    s.version = HOOK_STATUS_VERSION;
    s.bump = ctx.bumps.hook_status;
    s.hook = hook;
    s.audited = args.audited;
    s.pot_cap = args.pot_cap;
    s.blocked = args.blocked;
    s.updated_at = Clock::get()?.unix_timestamp;
    s.updated_by = authority;
    s.reserved = [0; 32];
    emit_cpi!(HookStatusSet {
        hook,
        audited: args.audited,
        pot_cap: args.pot_cap,
        blocked: args.blocked,
        authority,
    });
    Ok(())
}
