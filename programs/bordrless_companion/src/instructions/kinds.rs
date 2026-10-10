//! Phase 2's games (`docs/games.md`): the last-buyer jackpot and the diamond-hands streak, run by the
//! companion on what the coin's hook keeps under the game ticket standard (`bordrless_game::jackpot`
//! and `bordrless_game::streak`). No randomness. As for the lottery, the companion holds the pot and
//! makes every payout, only reads the hook's state and the holdings' hook data, and never calls the
//! hook; the protocol can cap or block a hook that is not audited, never take a pot.
//!
//! - **Jackpot**: `settle` (anyone) closes the oldest round that is over and not settled yet (the
//!   round a later buy ended, else the current one once its timer has run out): it pays
//!   `prize_bps` of the pot as SOL to the round's last qualifying buyer if their holding still holds
//!   what they bought, with nothing sent since; else it forfeits the round and the pot stays. Pays
//!   its sender `bounty_bps` of the prize.
//! - **Streak**: `close_epoch(e)` (anyone, during epoch `e + 1`) fixes epoch `e`'s pot
//!   (`prize_bps` of the pot then) and total (from the hook's header), and locks that pot for its
//!   holders (`Companion.pot_locked`). `claim_share(e)` (anyone, for any holding, during `e + 1`)
//!   pays the holding's owner `pot * weight / total` as SOL, less the sender's bounty, once: the
//!   receipt `PDA(["claimed", game, e, owner])` it makes (the sender paying its rent) refuses a
//!   second claim. What is not claimed by the end of `e + 1` rolls over, the dust of the rounding
//!   too. `close_receipt` returns a receipt's rent to whoever paid it once its epoch's claims have
//!   ended.

use anchor_lang::prelude::*;
use bordrless_game::{
    eligible, jackpot_winner_holds, read_state_at, round_of, settle_round, share_of,
    state_address_at, streak_weight, GameHeader, JackpotHeader, JackpotRound, JACKPOT_ENDED_ROUNDS,
};
use bordrless_launch::client as launch_client;
use bordrless_launch::state::Launch;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::error::CompanionError;
use crate::events::*;
use crate::instructions::game::{apply_status, rolled_over, ClaimPrize, GameStep};
use crate::instructions::steps::{available, balance, find, pay_sol};
use crate::instructions::CreatorSeeds;
use crate::state::*;

fn of_kind(g: &Game, kind: GameKind) -> Result<()> {
    require!(g.kind == kind, CompanionError::WrongGameKind);
    Ok(())
}

/// The hook's header for the game's mint, from its state among `all` (owner, address, magic, mint;
/// the round length the game's), and the state's account, whose data the kind's header is read
/// from where it lies (borrowed, never copied: a hook's state can be any size, and the companion's
/// heap is 32 KiB).
fn header_of<'a, 'info>(
    all: &'a [AccountInfo<'info>],
    g: &Game,
) -> Result<(GameHeader, &'a AccountInfo<'info>)> {
    let state = state_address_at(&g.hook, &g.mint, g.state_bump)
        .ok_or_else(|| error!(CompanionError::HookState))?;
    let info = find(all, &state)?;
    let header = read_state_at(info, &g.hook, &g.mint, g.state_bump)
        .map_err(|_| error!(CompanionError::HookState))?;
    require!(header.round_secs == g.round_secs, CompanionError::HookState);
    Ok((header, info))
}

/// The jackpot header in a hook's state, read from the borrowed data (its bytes at the
/// standard's offsets only; the rest of the state is never touched).
fn jackpot_of(state: &AccountInfo) -> Option<JackpotHeader> {
    JackpotHeader::parse(&state.try_borrow_data().ok()?)
}

/// The oldest jackpot round over and not settled (`settle_round`), from the hook's state (its
/// jackpot header read in its own stack frame); `HookState` when the state has none.
#[inline(never)]
fn open_round(
    state: &AccountInfo,
    header: &GameHeader,
    g: &Game,
    now: i64,
) -> Result<Option<JackpotRound>> {
    let jackpot = jackpot_of(state).ok_or_else(|| error!(CompanionError::HookState))?;
    Ok(settle_round(
        header,
        &jackpot,
        g.paid_buys,
        g.timer_secs,
        now,
    ))
}

/// Whether a jackpot round the pot could pay is open: one over, not settled and not stale (a
/// stale round is forfeited by any settle and owes nothing; a round after it may). At most the
/// remembered ended rounds and the current one are open. False for a state with no jackpot header.
#[inline(never)]
fn jackpot_prize_due(state: &AccountInfo, header: &GameHeader, g: &Game, now: i64) -> bool {
    let Some(jackpot) = jackpot_of(state) else {
        return false;
    };
    let mut paid = g.paid_buys;
    for _ in 0..=JACKPOT_ENDED_ROUNDS {
        let Some(r) = settle_round(header, &jackpot, paid, g.timer_secs, now) else {
            return false;
        };
        let stale_at =
            r.at.saturating_add(i64::from(g.timer_secs))
                .saturating_add(SETTLE_GRACE_SECS);
        if now < stale_at {
            return true;
        }
        paid = r.number;
    }
    false
}

// ---- jackpot ----------------------------------------------------------------------------------

/// The most the pot could hold after a fee claim now: what it holds, plus the pot's part of the
/// creator fees the launch holds unclaimed and of any bridged SOL the creator's holding holds above
/// what is set aside (both of which `claim_fees` splits), counted before any bounty or cap (an
/// upper bound: it errs on a winner's side). Both holdings must be passed.
fn fundable(all: &[AccountInfo], c: &Companion, launch: &Pubkey) -> Result<u64> {
    let unclaimed = balance(all, &BRIDGED_SOL_MINT, launch)?;
    let creator = Companion::creator(&c.mint).0;
    let surplus = balance(all, &BRIDGED_SOL_MINT, &creator)?
        .saturating_sub(c.set_aside().unwrap_or(u64::MAX));
    Ok(c.pending_pot.saturating_add(bps_of(
        unclaimed.saturating_add(surplus),
        u64::from(c.pot_bps),
    )))
}

/// Whether SOL can be paid to `account`: not executable (a program's), not a sysvar (owned by the
/// sysvar program), and none of the runtime's reserved keys (`RESERVED_KEYS`: read-only in every
/// transaction, whether or not their account exists).
fn payable(account: &AccountInfo) -> bool {
    !account.executable
        && *account.owner != SYSVAR_PROGRAM_ID
        && !RESERVED_KEYS.contains(account.key)
}

/// Whether the round's buyer still holds what it bought: `holding` is the token program's holding
/// of `mint` at the buyer's address, its owner eligible, its mark the round's buy or earlier (no
/// send since) and its balance at least the round's amount, which is at least the game's minimum
/// buy. A holding that is gone (closed: it sent everything) does not.
fn jackpot_winner(
    holding: &AccountInfo,
    mint: &Pubkey,
    round: &JackpotRound,
    excluded: &[Pubkey],
    min_tokens: u64,
) -> bool {
    if *holding.owner != TOKEN_ID {
        return false;
    }
    let Ok(h) = token_client::read_holding(holding) else {
        return false;
    };
    h.mint == *mint
        && h.owner == round.buyer
        && eligible(&round.buyer, excluded)
        && round.amount >= min_tokens
        && jackpot_winner_holds(&h.hook_data, round, h.amount)
}

/// `settle`: the oldest jackpot round that is over and not settled
/// (`bordrless_game::settle_round`: the round a later qualifying buy ended, else the current round
/// once its timer has run out at `now`), closed.
///
/// - Its buyer still holds what they bought, with nothing sent since (`jackpot_winner_holds`), and
///   may hold tickets (a wallet, not the launch, its pool or the creator address; a program's
///   address can't be paid): they are paid `prize_bps` of the pot as SOL, less the sender's
///   `bounty_bps` of it, when the pot holds at least `min_pot` (or the cap when lower, or the floor
///   once dormant), else nothing (`JackpotUnfunded`; nothing too when an empty wallet couldn't take
///   the prize). The round is closed either way: a settle never waits, so a round can't be
///   overtaken while it waits.
/// - Otherwise the round is forfeited: nobody is paid, the pot stays, and the next round can be
///   settled.
///
/// `holding` must be the buyer's holding of the mint (`WrongHolding` otherwise), whether or not it
/// still exists. Remaining: the hook's state, the buyer (writable; paid the prize) and the bridge's
/// `unwrap_sol` accounts.
pub fn process_settle<'info>(ctx: Context<'info, ClaimPrize<'info>>) -> Result<()> {
    of_kind(&ctx.accounts.game, GameKind::Jackpot)?;
    let now = Clock::get()?.unix_timestamp;
    let (companion_key, game_key) = (ctx.accounts.companion.key(), ctx.accounts.game.key());
    let event_authority = ctx.accounts.event_authority.to_account_info();
    let Some(terms) = apply_status(
        &event_authority,
        companion_key,
        game_key,
        &mut ctx.accounts.companion,
        &mut ctx.accounts.game,
        &ctx.accounts.hook_status,
        ctx.remaining_accounts,
    )?
    else {
        return Ok(());
    };
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let (c, g) = (&ctx.accounts.companion, &ctx.accounts.game);
    let (header, state) = header_of(&all, g)?;
    let round =
        open_round(state, &header, g, now)?.ok_or_else(|| error!(CompanionError::NotDue))?;
    let mint = c.mint;
    let creator = ctx.accounts.creator.key();
    let holding = &ctx.accounts.holding;
    require_keys_eq!(
        holding.key(),
        token_client::holding_address(&mint, &round.buyer),
        CompanionError::WrongHolding
    );
    let launch = &ctx.accounts.launch;
    // The buyer, as passed (it must be: it is paid). An address that can't be paid (a program's,
    // a sysvar's or another of the runtime's reserved keys, read-only in every transaction) forfeits
    // its round, never left open to jam the rounds after it; so does a round left unsettled for
    // `SETTLE_GRACE_SECS` after its timer ran out, whatever its buyer.
    let winner = find(&all, &round.buyer)?.clone();
    let over_at = round.at.saturating_add(i64::from(g.timer_secs));
    let stale = now >= over_at.saturating_add(SETTLE_GRACE_SECS);
    let reason = if stale {
        ForfeitReason::Stale
    } else if !payable(&winner) {
        ForfeitReason::Unpayable
    } else {
        ForfeitReason::NotHeld
    };
    let wins = reason == ForfeitReason::NotHeld
        && jackpot_winner(
            holding,
            &mint,
            &round,
            &[launch.key(), launch.pool, creator],
            g.min_tokens,
        );
    if !wins {
        let pending_pot = c.pending_pot;
        let g = &mut ctx.accounts.game;
        g.paid_buys = round.number;
        g.rollovers = g.rollovers.saturating_add(1);
        emit_cpi!(JackpotForfeited {
            game: game_key,
            mint,
            round: round.number,
            reason,
            buyer: round.buyer,
            amount: round.amount,
            bought_at: round.at,
            pending_pot,
            cranker: ctx.accounts.cranker.key(),
        });
        return Ok(());
    }
    // A round over is always closed: a settle never waits, so no later round can take its place
    // while it waits. It pays `prize_bps` of the pot when the pot holds its minimum (or the cap
    // when lower, or the floor once dormant), else nothing; and nothing when an empty wallet could
    // not take the prize (below its rent-exempt minimum).
    let threshold = terms.draw_threshold(g.min_pot_at(now, c.launched_at));
    let funded = c.pending_pot >= threshold;
    // Fees the launch holds and nobody has claimed yet could fund the prize: then the round is not
    // closed unfunded (that would void a prize any `claim_fees` would pay), and the sender claims
    // first (`claim_fees` and `settle` in one transaction). The pot's part of them is counted
    // before any bounty or cap, so this errs on the winner's side.
    if !funded {
        require!(
            fundable(&all, c, &launch.key())? < threshold,
            CompanionError::FeesUnclaimed
        );
    }
    let mut prize = if funded {
        bps_of(c.pending_pot, u64::from(g.prize_bps))
    } else {
        0
    };
    let rent_min = Rent::get()?.minimum_balance(0);
    if winner.lamports() == 0
        && prize.saturating_sub(bps_of(prize, u64::from(c.bounty_bps))) < rent_min
    {
        prize = 0;
    }
    if prize == 0 {
        let pending_pot = c.pending_pot;
        let g = &mut ctx.accounts.game;
        g.paid_buys = round.number;
        g.rollovers = g.rollovers.saturating_add(1);
        emit_cpi!(JackpotUnfunded {
            game: game_key,
            mint,
            round: round.number,
            winner: round.buyer,
            amount: round.amount,
            pending_pot,
            cranker: ctx.accounts.cranker.key(),
        });
        return Ok(());
    }
    let bounty = bps_of(prize, u64::from(c.bounty_bps));
    let paid = prize - bounty;
    let seeds = CreatorSeeds::new(mint, c.creator_bump);
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
    g.paid_buys = round.number;
    g.prizes_paid = g.prizes_paid.saturating_add(1);
    g.prizes_total = g.prizes_total.saturating_add(paid);
    g.last_winner = round.buyer;
    g.settled_at = now;
    emit_cpi!(JackpotPaid {
        game: game_key,
        mint,
        round: round.number,
        holding: holding.key(),
        winner: round.buyer,
        amount: round.amount,
        bought_at: round.at,
        prize: paid,
        bounty,
        pending_pot: c.pending_pot,
        cranker: ctx.accounts.cranker.key(),
    });
    Ok(())
}

/// Whether `retire` must wait for a jackpot's or a streak's prize that can be paid now: a jackpot
/// round over and not settled while the pot can pay it (`settle` would pay or forfeit it), or a
/// streak epoch that just ended, with weight, still closable while the pot can pay it
/// (`close_epoch` would share it). The hook's state is read from `all`; a lottery never waits here
/// (its draw is `Game.status`). A pot below what a prize needs never waits: it can't be paid.
pub(crate) fn prize_due(
    all: &[AccountInfo],
    c: &Companion,
    g: &Game,
    terms: &HookTerms,
    now: i64,
) -> Result<bool> {
    if g.kind == GameKind::Lottery {
        return Ok(false);
    }
    // What a fee claim would bring counts too: a `retire` never voids a prize a claim would fund.
    let launch = launch_client::launch_address(&g.mint);
    if fundable(all, c, &launch)? < terms.draw_threshold(g.min_pot_at(now, c.launched_at)) {
        return Ok(false);
    }
    // The state must be passed; one that can't be read (a hook that broke its own state) owes no
    // prize, so it never holds the pot for ever.
    let state = state_address_at(&g.hook, &g.mint, g.state_bump)
        .ok_or_else(|| error!(CompanionError::HookState))?;
    find(all, &state)?;
    let Ok((header, state)) = header_of(all, g) else {
        return Ok(false);
    };
    Ok(match g.kind {
        GameKind::Jackpot => jackpot_prize_due(state, &header, g, now),
        _ => {
            let current = round_of(now, g.round_secs);
            current > 0
                && current > g.next_round
                && now
                    <= claims_end(current - 1, g.round_secs)
                        .saturating_sub(i64::from(g.claim_window_secs))
                && header.total_of(current - 1).is_some_and(|t| t > 0)
        }
    })
}

// ---- streak -----------------------------------------------------------------------------------

/// A streak epoch open for claims whose claims have ended (with the epoch after it) is over: what
/// its pot did not pay rolls over (the lock is released). Answers whether it ended one.
pub(crate) fn end_epoch_if_over(c: &mut Companion, g: &mut Game, now: i64) -> bool {
    // A strategy's period ends as a streak's epoch does (phase 3a).
    let kind = g.kind == GameKind::Streak || g.kind == GameKind::Strategy;
    if !kind || g.status != DrawStatus::Revealed || now < g.claims_end() {
        return false;
    }
    g.status = DrawStatus::Idle;
    c.pot_locked = 0;
    true
}

/// `close_epoch(epoch)`: once epoch `epoch` is over (the clock is in the next epoch: an epoch is
/// closed only during the one after it, and at most once), the epoch whose claims ended with it
/// released (what it did not pay rolls over), and `epoch` closed:
///
/// - after `claims_end(epoch)` less a whole claim window, too late to leave its holders time to
///   claim: it rolls over (`Late`);
/// - its total, read from the hook's header: none (`NoTickets`), or forgotten (`RoundForgotten`),
///   it rolls over;
/// - else its pot is fixed, `prize_bps` of the pot (which must hold at least `min_pot`, or the cap
///   when lower, or the floor once dormant), and locked for its holders (`Companion.pot_locked`),
///   and its claims open until `claims_end(epoch)`.
///
/// Pays nobody. Remaining: the hook's state.
pub fn process_close_epoch<'info>(ctx: Context<'info, GameStep<'info>>, epoch: u32) -> Result<()> {
    of_kind(&ctx.accounts.game, GameKind::Streak)?;
    let now = Clock::get()?.unix_timestamp;
    let (companion_key, game_key) = (ctx.accounts.companion.key(), ctx.accounts.game.key());
    let event_authority = ctx.accounts.event_authority.to_account_info();
    // The epoch open for claims before this one: its claims ended with `epoch`. Released first,
    // so the hook's terms (a cap lowered while its pot was locked) apply to the whole pot before
    // this epoch's pot is fixed.
    let (ended_epoch, unclaimed) = {
        let g = &ctx.accounts.game;
        (g.round, g.prize.saturating_sub(g.epoch_paid))
    };
    let released = end_epoch_if_over(&mut ctx.accounts.companion, &mut ctx.accounts.game, now);
    let Some(terms) = apply_status(
        &event_authority,
        companion_key,
        game_key,
        &mut ctx.accounts.companion,
        &mut ctx.accounts.game,
        &ctx.accounts.hook_status,
        ctx.remaining_accounts,
    )?
    else {
        return Ok(());
    };
    let (c, g) = (&ctx.accounts.companion, &ctx.accounts.game);
    require!(g.status == DrawStatus::Idle, CompanionError::DrawPending);
    let current = round_of(now, g.round_secs);
    require!(
        current > 0 && epoch == current - 1 && epoch >= g.next_round,
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
            epoch: ended_epoch,
            unclaimed,
            pending_pot: c.pending_pot,
        });
    }
    let next = epoch.checked_add(1).ok_or(CompanionError::MathOverflow)?;
    // Too late to leave its holders a whole claim window: it rolls over.
    let last_close = claims_end(epoch, g.round_secs).saturating_sub(i64::from(g.claim_window_secs));
    if now > last_close {
        let c = &ctx.accounts.companion;
        let g = &mut ctx.accounts.game;
        g.next_round = next;
        g.round = epoch;
        g.total = 0;
        return rolled_over(&event_authority, game_key, c, g, RolloverReason::Late);
    }
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let (header, _) = header_of(&all, &ctx.accounts.game)?;
    let total = header.total_of(epoch);
    let c = &ctx.accounts.companion;
    let g = &mut ctx.accounts.game;
    g.next_round = next;
    g.round = epoch;
    let total = match total {
        Some(t) if t > 0 => t,
        other => {
            g.total = 0;
            let reason = if other.is_none() {
                RolloverReason::RoundForgotten
            } else {
                RolloverReason::NoTickets
            };
            return rolled_over(&event_authority, game_key, c, g, reason);
        }
    };
    let pot = bps_of(c.pending_pot, u64::from(g.prize_bps));
    g.total = total;
    g.prize = pot;
    g.epoch_paid = 0;
    g.status = DrawStatus::Revealed;
    g.revealed_at = now;
    g.draws = g.draws.saturating_add(1);
    let c = &mut ctx.accounts.companion;
    c.pot_locked = pot;
    emit_cpi!(EpochClosed {
        game: game_key,
        mint: g.mint,
        epoch,
        total,
        epoch_pot: pot,
        claims_end: g.claims_end(),
        pending_pot: c.pending_pot,
        cranker: ctx.accounts.cranker.key(),
    });
    Ok(())
}

/// Accounts of `claim_share`. Remaining: the bridge's `unwrap_sol` accounts.
#[event_cpi]
#[derive(Accounts)]
#[instruction(epoch: u32)]
pub struct ClaimShare<'info> {
    /// Whoever sends it: pays the receipt's rent (returned by `close_receipt`) and is paid the
    /// bounty.
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
    /// CHECK: the owner's holding of the mint: read as the token program's (owner and
    /// discriminator), of this mint, at the owner's address.
    pub holding: UncheckedAccount<'info>,
    /// CHECK: the holding's owner (checked in the handler), paid the share.
    #[account(mut)]
    pub owner: UncheckedAccount<'info>,
    /// The epoch's receipt for the owner: made here, so a second claim fails.
    #[account(
        init,
        payer = cranker,
        space = ShareReceipt::LEN,
        seeds = [CLAIMED_SEED, game.key().as_ref(), &epoch.to_le_bytes(), owner.key().as_ref()],
        bump
    )]
    pub receipt: Box<Account<'info, ShareReceipt>>,
    pub system_program: Program<'info, System>,
}

/// `claim_share(epoch)`: during the claims of epoch `epoch` (the epoch closed last, until the end
/// of the epoch after it), the owner's holding's share of its pot, `pot * weight / total` rounded
/// down (`bordrless_game::share_of`), paid to the owner as SOL less the sender's `bounty_bps` of it,
/// once (the receipt). The weight is the holding's for the epoch (`bordrless_game::streak_weight`):
/// at least `min_weight`, at most its balance, its owner still qualifying (no send since: a send
/// forfeits the share) and eligible (a wallet, not the launch, its pool or the creator address).
/// No share is ever more than what the epoch's pot still holds (`Companion.pot_locked`).
pub fn process_claim_share<'info>(
    ctx: Context<'info, ClaimShare<'info>>,
    epoch: u32,
) -> Result<()> {
    of_kind(&ctx.accounts.game, GameKind::Streak)?;
    let now = Clock::get()?.unix_timestamp;
    let (companion_key, game_key) = (ctx.accounts.companion.key(), ctx.accounts.game.key());
    let owner = ctx.accounts.owner.key();
    {
        let r = &mut ctx.accounts.receipt;
        r.version = RECEIPT_VERSION;
        r.bump = ctx.bumps.receipt;
        r.game = game_key;
        r.epoch = epoch;
        r.owner = owner;
        r.payer = ctx.accounts.cranker.key();
        r.amount = 0;
        r.claimed_at = now;
    }
    // Only the epoch open for claims, before anything else (a block's early return included):
    // no receipt is ever made for another epoch.
    {
        let g = &ctx.accounts.game;
        require!(
            g.status == DrawStatus::Revealed && g.round == epoch,
            CompanionError::NoDraw
        );
        require!(now < g.claims_end(), CompanionError::DrawLate);
    }
    let event_authority = ctx.accounts.event_authority.to_account_info();
    if apply_status(
        &event_authority,
        companion_key,
        game_key,
        &mut ctx.accounts.companion,
        &mut ctx.accounts.game,
        &ctx.accounts.hook_status,
        ctx.remaining_accounts,
    )?
    .is_none()
    {
        return Ok(());
    }
    let (c, g) = (&ctx.accounts.companion, &ctx.accounts.game);
    let mint = c.mint;
    let creator = ctx.accounts.creator.key();
    let holding_key = ctx.accounts.holding.key();
    let holding = token_client::read_holding(&ctx.accounts.holding)?;
    require_keys_eq!(holding.mint, mint, CompanionError::WrongHolding);
    require_keys_eq!(holding.owner, owner, CompanionError::WrongHolding);
    require_keys_eq!(
        holding_key,
        token_client::holding_address(&mint, &owner),
        CompanionError::WrongHolding
    );
    let launch = &ctx.accounts.launch;
    require!(
        eligible(&owner, &[launch.key(), launch.pool, creator]),
        CompanionError::NotEligible
    );
    let weight = streak_weight(
        &holding.hook_data,
        epoch,
        g.round_secs,
        g.min_streak_secs,
        g.min_weight,
        holding.amount,
    );
    require!(weight > 0, CompanionError::NoShare);
    let share = share_of(g.prize, weight, g.total)
        .min(c.pot_locked)
        .min(c.pending_pot);
    require!(share > 0, CompanionError::NothingToDo);
    let bounty = bps_of(share, u64::from(c.bounty_bps));
    let paid = share - bounty;
    let seeds = CreatorSeeds::new(mint, c.creator_bump);
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let to = ctx.accounts.owner.to_account_info();
    pay_sol(&all, &seeds, creator, &to, paid)?;
    pay_sol(
        &all,
        &seeds,
        creator,
        &ctx.accounts.cranker.to_account_info(),
        bounty,
    )?;
    let c = &mut ctx.accounts.companion;
    c.pending_pot -= share;
    c.pot_locked -= share;
    c.bounties_total = c.bounties_total.saturating_add(bounty);
    let g = &mut ctx.accounts.game;
    g.epoch_paid = g.epoch_paid.saturating_add(share);
    g.prizes_paid = g.prizes_paid.saturating_add(1);
    g.prizes_total = g.prizes_total.saturating_add(paid);
    g.last_winner = owner;
    g.settled_at = now;
    ctx.accounts.receipt.amount = paid;
    emit_cpi!(ShareClaimed {
        game: game_key,
        mint,
        epoch,
        holding: holding_key,
        owner,
        weight,
        total: g.total,
        share: paid,
        bounty,
        epoch_paid: g.epoch_paid,
        pending_pot: c.pending_pot,
        cranker: ctx.accounts.cranker.key(),
    });
    Ok(())
}

/// Accounts of `close_receipt`.
#[derive(Accounts)]
pub struct CloseReceipt<'info> {
    /// The receipt, closed: its rent goes back to whoever paid it.
    #[account(
        mut,
        close = payer,
        has_one = payer,
        has_one = game,
        seeds = [CLAIMED_SEED, game.key().as_ref(), &receipt.epoch.to_le_bytes(), receipt.owner.as_ref()],
        bump = receipt.bump
    )]
    pub receipt: Box<Account<'info, ShareReceipt>>,
    /// CHECK: who paid the receipt's rent (`receipt.payer`).
    #[account(mut)]
    pub payer: UncheckedAccount<'info>,
    pub game: Box<Account<'info, Game>>,
}

/// `close_receipt`: anyone, once the receipt's epoch's claims have ended (`claims_end(epoch)`, when
/// no claim of that epoch can be made again), closes it, its rent to whoever paid it.
pub fn process_close_receipt(ctx: Context<CloseReceipt>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let r = &ctx.accounts.receipt;
    require!(
        now >= claims_end(r.epoch, ctx.accounts.game.round_secs),
        CompanionError::NotDue
    );
    Ok(())
}
