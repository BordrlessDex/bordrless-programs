//! A Bordrless game hook, made with Studio: the last-buyer jackpot.
//!
//! Every buy on the coin's launch pool of at least `MIN_TOKENS` restarts a countdown of
//! `TIMER_SECS`. When it runs out, the Bordrless companion (which holds the pot) pays the last
//! qualifying buyer a share of the pot in SOL, if they still hold everything they bought. This hook
//! only keeps the score, under the game ticket standard (`bordrless-game`):
//!
//! - the standard's header in its state (`GameHeader`, with no rounds) and the jackpot header after
//!   it (`JackpotHeader`): the last qualifying buy, the count of them, and the last 8 rounds that
//!   ended (so a round nobody settled yet stays payable for 8 timers after it ended);
//! - each holding's mark in its hook data: its first qualifying buy since it last sent anything.
//!
//! It never refuses a transfer, takes no cut, never sees SOL and calls nothing: its callbacks answer
//! hook data only. The rules are the crate's (`qualifying_buy`, `jackpot_on_buy`,
//! `jackpot_on_send`, `jackpot_on_receive`); a variant changes `TIMER_SECS` and `MIN_TOKENS`, or
//! who may win (fewer owners, never more: never the launch, its pool or the companion).
//!
//! A buy counts only while the launch is on its bonding curve: once it graduates, anyone can add
//! liquidity to the pool and take it out again, and that is a transfer out of the pool no hook can
//! tell from a buy. The jackpot ends at graduation (the round under way is still paid).
//!
//! The standard `prepare` (every Studio hook has it, unchanged in its accounts): before a launch,
//! anyone sends it once for the new mint; it creates the state at `["state", mint]` and writes the
//! registry (the state, writable, then the launch).

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use bordrless_game::{
    eligible, jackpot_on_buy, jackpot_on_receive, jackpot_on_send, qualifying_buy, read_launch,
    GameHeader, JackpotHeader, Slots,
};
use bordrless_hook::{
    hook_accounts_address, hook_signer, write_registry, AccountSource, ExtraAccount,
    HookAccountList, HookReturn, Seed, TokenHookArgs, TokenOp,
};

declare_id!("Ay75xkGDvTRjFD97Qwt1qf6QxT9BFV2LB5qoJNUrYMbR");

/// A round ends this long after its last qualifying buy: 10 minutes (5 minutes to 30 days).
pub const TIMER_SECS: u32 = 600;
/// The least a qualifying buy delivers: 1,000,000 tokens (6 decimals), 0.1% of the supply.
pub const MIN_TOKENS: u64 = 1_000_000_000_000;
/// The Bordrless token program: the only caller of the callbacks.
pub const TOKEN_PROGRAM: Pubkey =
    Pubkey::from_str_const("2XoEWp8cF3kRXg74eVwPAyTFhVCAztn3V88komxAvr22");
/// The Bordrless launchpad.
pub const LAUNCH_PROGRAM: Pubkey =
    Pubkey::from_str_const("1jcBymHxBjniZDhNPy51Vgm5Nz7pLUdxa9UBHc4TavC");
/// `["state", mint]`: this hook's state for a mint (the standard `prepare` creates it; the game
/// ticket standard reads its header there).
pub const STATE_SEED: &[u8] = b"state";
/// The launchpad's `["launch", mint]` seed.
pub const LAUNCH_SEED: &[u8] = b"launch";
/// The companion's `["creator", mint]`: a companion launch's creator, which never wins.
pub const COMPANION_PROGRAM: Pubkey =
    Pubkey::from_str_const("6ZUM1gWBH9hBBNoJoaVAGwSftyZ6CUda6vUZTW9MsJuo");

/// The callbacks' extra accounts, after the token program's five: the state (written on every
/// transfer), the launch (read for its pool and its curve).
pub fn extra_accounts() -> HookAccountList {
    HookAccountList::new(vec![
        ExtraAccount {
            writable: true,
            source: AccountSource::Pda {
                program: crate::ID,
                seeds: vec![Seed::Literal(STATE_SEED.to_vec()), Seed::Account(1)],
            },
        },
        ExtraAccount {
            writable: false,
            source: AccountSource::Pda {
                program: LAUNCH_PROGRAM,
                seeds: vec![Seed::Literal(LAUNCH_SEED.to_vec()), Seed::Account(1)],
            },
        },
    ])
}

#[program]
pub mod studio_hook {
    use super::*;

    /// The standard `prepare`: the state for `mint` (the jackpot's settings are this program's
    /// constants) and its registry. Anyone may send it, once per mint.
    pub fn prepare(ctx: Context<Prepare>) -> Result<()> {
        let mint = ctx.accounts.mint.key();
        let now = Clock::get()?.unix_timestamp;
        let state = &mut ctx.accounts.state;
        state.header = GameHeader::new(mint, 0, now);
        state.jackpot = JackpotHeader::new(TIMER_SECS, MIN_TOKENS);
        state.version = 1;
        state.bump = ctx.bumps.state;
        state.launch =
            Pubkey::find_program_address(&[LAUNCH_SEED, mint.as_ref()], &LAUNCH_PROGRAM).0;
        state.creator =
            Pubkey::find_program_address(&[b"creator", mint.as_ref()], &COMPANION_PROGRAM).0;
        state.prepared_by = ctx.accounts.payer.key();
        state.reserved = [0; 32];
        let (registry, bump) = hook_accounts_address(&crate::ID, &mint);
        require_keys_eq!(
            ctx.accounts.registry.key(),
            registry,
            HookError::WrongAccount
        );
        write_registry(
            &ctx.accounts.payer.to_account_info(),
            &ctx.accounts.registry.to_account_info(),
            &ctx.accounts.system_program.to_account_info(),
            &crate::ID,
            &mint,
            bump,
            &extra_accounts(),
        )
    }

    /// Before every transfer: a sender's mark is cleared (it no longer holds all it bought); a
    /// qualifying buy names its buyer the last one (restarting the timer) and marks its holding.
    pub fn before_transfer(ctx: Context<Callback>, args: TokenHookArgs) -> Result<HookReturn> {
        check_call(&ctx, &args)?;
        if args.op != TokenOp::Transfer {
            return Ok(HookReturn::default());
        }
        let now = Clock::get()?.unix_timestamp;
        let launch = read_launch(&ctx.accounts.launch, &args.mint);
        let state = &mut ctx.accounts.state;
        let pool = launch.map(|l| l.pool).unwrap_or_default();
        let excluded = [state.launch, state.creator, pool];
        let mut answer = HookReturn::default();
        if eligible(&args.source_owner, &excluded) {
            let mut slots = Slots::decode(&args.source_hook_data);
            jackpot_on_send(
                &mut slots,
                args.source_balance.saturating_sub(args.amount),
                now,
            );
            answer.source_hook_data = Some(slots.encode());
        }
        if eligible(&args.destination_owner, &excluded) {
            let mut slots = Slots::decode(&args.destination_hook_data);
            let after = args.destination_balance.saturating_add(args.amount);
            jackpot_on_receive(&mut slots, after, now);
            if qualifying_buy(
                launch.as_ref(),
                &args.source_owner,
                &args.destination_owner,
                args.amount,
                state.jackpot.min_tokens,
                &excluded,
            ) {
                let JackpotState {
                    header, jackpot, ..
                } = &mut ***state;
                jackpot_on_buy(
                    header,
                    jackpot,
                    &mut slots,
                    &args.destination_owner,
                    args.amount,
                    now,
                );
            }
            answer.destination_hook_data = Some(slots.encode());
        }
        Ok(answer)
    }

    /// Before a burn: like a send.
    pub fn before_burn(ctx: Context<Callback>, args: TokenHookArgs) -> Result<HookReturn> {
        check_call(&ctx, &args)?;
        if args.op != TokenOp::Burn {
            return Ok(HookReturn::default());
        }
        let now = Clock::get()?.unix_timestamp;
        let state = &ctx.accounts.state;
        let mut answer = HookReturn::default();
        if eligible(&args.source_owner, &[state.launch, state.creator]) {
            let mut slots = Slots::decode(&args.source_hook_data);
            jackpot_on_send(
                &mut slots,
                args.source_balance.saturating_sub(args.amount),
                now,
            );
            answer.source_hook_data = Some(slots.encode());
        }
        Ok(answer)
    }
}

/// Every callback: signed by the token program's signer for this hook, about this state's mint.
fn check_call(ctx: &Context<Callback>, args: &TokenHookArgs) -> Result<()> {
    let signer = &ctx.accounts.hook_signer;
    require!(signer.is_signer, HookError::BadHookSigner);
    require_keys_eq!(
        signer.key(),
        hook_signer(&TOKEN_PROGRAM, &crate::ID).0,
        HookError::BadHookSigner
    );
    require_keys_eq!(
        args.mint,
        ctx.accounts.state.header.mint,
        HookError::WrongAccount
    );
    Ok(())
}

/// The state for one mint, at `["state", mint]`: the standard's header first, the jackpot's after
/// it (the companion reads both at the standard's offsets), then this hook's own fields.
#[account]
#[derive(InitSpace)]
pub struct JackpotState {
    /// The standard's header (no rounds: `round_secs` 0); its `last_*` fields are the current
    /// round's last qualifying buy.
    pub header: GameHeader,
    /// The jackpot header: the settings, the count of qualifying buys, the last 8 rounds that ended.
    pub jackpot: JackpotHeader,
    /// Layout version.
    pub version: u8,
    /// Bump.
    pub bump: u8,
    /// The launchpad's `["launch", mint]`: never wins.
    pub launch: Pubkey,
    /// The companion's `["creator", mint]`: never wins.
    pub creator: Pubkey,
    /// Who sent `prepare`.
    pub prepared_by: Pubkey,
    /// Reserved.
    pub reserved: [u8; 32],
}

/// Accounts of the standard `prepare` (the same in every Studio hook).
#[derive(Accounts)]
pub struct Prepare<'info> {
    /// Pays the rent.
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: the mint the hook is prepared for; need not exist yet.
    pub mint: UncheckedAccount<'info>,
    #[account(init, payer = payer, space = 8 + JackpotState::INIT_SPACE, seeds = [STATE_SEED, mint.key().as_ref()], bump)]
    pub state: Box<Account<'info, JackpotState>>,
    /// CHECK: the registry PDA, written here (address-checked in the handler).
    #[account(mut)]
    pub registry: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Accounts of every callback: the token program's five, then the registry's extras.
#[derive(Accounts)]
pub struct Callback<'info> {
    /// CHECK: the token program's signer for this hook (checked in `check_call`).
    pub hook_signer: UncheckedAccount<'info>,
    /// CHECK: the mint.
    pub mint: UncheckedAccount<'info>,
    /// CHECK: the source holding.
    pub source: UncheckedAccount<'info>,
    /// CHECK: the destination holding (the mint for a burn).
    pub destination: UncheckedAccount<'info>,
    /// CHECK: who signed the operation.
    pub authority: UncheckedAccount<'info>,
    #[account(mut, seeds = [STATE_SEED, mint.key().as_ref()], bump = state.bump)]
    pub state: Box<Account<'info, JackpotState>>,
    /// CHECK: the mint's launch account (address-checked; not readable during the launch).
    #[account(address = state.launch @ HookError::WrongAccount)]
    pub launch: UncheckedAccount<'info>,
}

/// Errors.
#[error_code]
pub enum HookError {
    #[msg("an account is not the one this mint's hook expects")]
    WrongAccount,
    #[msg("the hook signer is not the token program's signer for this hook")]
    BadHookSigner,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_constants_are_the_standards() {
        assert_eq!(TOKEN_PROGRAM, bordrless_game::TOKEN_PROGRAM_ID);
        assert_eq!(LAUNCH_PROGRAM, bordrless_game::LAUNCH_PROGRAM_ID);
        assert_eq!(STATE_SEED, bordrless_game::STATE_SEED);
        assert_eq!(LAUNCH_SEED, bordrless_game::LAUNCH_SEED);
        assert_eq!(COMPANION_PROGRAM, bordrless_game::COMPANION_PROGRAM_ID);
    }
}
