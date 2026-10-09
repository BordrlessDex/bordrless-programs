//! A Bordrless game hook, made with Studio: the diamond-hands streak.
//!
//! Each epoch (`EPOCH_SECS`), the Bordrless companion (which holds the pot) shares part of the pot
//! among the holders who held through the whole epoch without sending a single token, in
//! proportion to what they held. Selling, or sending to anyone (yourself included), forfeits your
//! share; what you forfeit stays in the pot for the next epoch. This hook only keeps the weights,
//! under the game ticket standard (`bordrless-game`):
//!
//! - the standard's header in its state (`GameHeader`: the epochs are its rounds, and its totals
//!   are exact weight totals) and the streak header after it (`StreakHeader`: `MIN_STREAK_SECS`,
//!   `MIN_WEIGHT`);
//! - each holding's slots in its hook data: its weight this epoch and last, and `since`.
//!
//! It never refuses a transfer, takes no cut and never sees SOL. Its callbacks call nothing and
//! answer hook data only; `enter` makes its one call, `bordrless_game::write_own_hook_data`. The
//! rules are the crate's (`streak_on_send`, `streak_on_receive`, `streak_on_enter`); a variant
//! changes the constants, or who may share (fewer owners, never more: never the launch, its pool
//! or the companion).
//!
//! The standard `prepare` (every Studio hook has it, unchanged in its accounts): before a launch,
//! anyone sends it once for the new mint; it creates the state at `["state", mint]` and writes the
//! registry (the state, writable, then the launch).

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use bordrless_game::{
    eligible, read_holding, read_launch, streak_on_enter, streak_on_receive, streak_on_send,
    write_own_hook_data, GameHeader, Slots, StreakHeader, TOKEN_EVENT_AUTHORITY,
};
use bordrless_hook::{
    hook_accounts_address, hook_signer, write_registry, AccountSource, ExtraAccount,
    HookAccountList, HookReturn, Seed, TokenHookArgs, TokenOp,
};

declare_id!("9BmAknPDZeW5EjXKMRJyAZvvguF7S8cdGecZRXLjgfvT");

/// An epoch: a week (an hour to 30 days).
pub const EPOCH_SECS: u32 = 7 * 86_400;
/// A holding shares in an epoch only if, by its end, it has sent nothing for this long: a week, so
/// whoever held through the whole epoch qualifies (at most a year).
pub const MIN_STREAK_SECS: u32 = 7 * 86_400;
/// The least weight that shares: 100,000 tokens (6 decimals), 0.01% of the supply.
pub const MIN_WEIGHT: u64 = 100_000_000_000;
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
/// The companion's `["creator", mint]`: a companion launch's creator, which never shares.
pub const COMPANION_PROGRAM: Pubkey =
    Pubkey::from_str_const("6ZUM1gWBH9hBBNoJoaVAGwSftyZ6CUda6vUZTW9MsJuo");

/// The callbacks' extra accounts, after the token program's five: the state (written on every
/// transfer), the launch (read for its pool until the state remembers it).
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

    /// The standard `prepare`: the state for `mint` (the streak's settings are this program's
    /// constants) and its registry. Anyone may send it, once per mint.
    pub fn prepare(ctx: Context<Prepare>) -> Result<()> {
        let mint = ctx.accounts.mint.key();
        let now = Clock::get()?.unix_timestamp;
        let state = &mut ctx.accounts.state;
        state.header = GameHeader::new(mint, EPOCH_SECS, now);
        state.streak = StreakHeader::new(MIN_STREAK_SECS, MIN_WEIGHT);
        state.version = 1;
        state.bump = ctx.bumps.state;
        state.launch =
            Pubkey::find_program_address(&[LAUNCH_SEED, mint.as_ref()], &LAUNCH_PROGRAM).0;
        state.pool = Pubkey::default();
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

    /// Anyone, for any holding of the mint: registers its whole balance for the current epoch, when
    /// it has not been written this epoch and qualifies. Writes nothing otherwise (so nobody can
    /// lock a holder out by entering it).
    pub fn enter(ctx: Context<Enter>) -> Result<()> {
        let mint = ctx.accounts.mint.key();
        let holding = read_holding(&ctx.accounts.holding).ok_or(HookError::WrongAccount)?;
        require_keys_eq!(holding.mint, mint, HookError::WrongAccount);
        let now = Clock::get()?.unix_timestamp;
        let state = &mut ctx.accounts.state;
        require!(
            eligible(&holding.owner, &state.excluded()),
            HookError::NotEligible
        );
        let mut slots = Slots::decode(&holding.hook_data);
        let StreakState { header, streak, .. } = &mut ***state;
        if !streak_on_enter(header, streak, &mut slots, holding.amount, now) {
            return Ok(());
        }
        write_own_hook_data(
            &crate::ID,
            &ctx.accounts.hook_authority,
            &ctx.accounts.mint,
            &ctx.accounts.holding,
            &ctx.accounts.token_event_authority,
            &ctx.accounts.token_program,
            slots.encode(),
        )
    }

    /// Before every transfer: the sender's weights forfeited (this epoch's leaves the total), the
    /// receiver's epoch opened at its first write (for what it held before).
    pub fn before_transfer(ctx: Context<Callback>, args: TokenHookArgs) -> Result<HookReturn> {
        check_call(&ctx, &args)?;
        let now = Clock::get()?.unix_timestamp;
        let state = &mut ctx.accounts.state;
        state.header.roll(now);
        remember_pool(state, &ctx.accounts.launch);
        if args.op != TokenOp::Transfer {
            return Ok(HookReturn::default());
        }
        let excluded = state.excluded();
        let StreakState { header, streak, .. } = &mut ***state;
        let mut answer = HookReturn::default();
        if eligible(&args.source_owner, &excluded) {
            let mut slots = Slots::decode(&args.source_hook_data);
            streak_on_send(
                header,
                &mut slots,
                args.source_balance.saturating_sub(args.amount),
                now,
            );
            answer.source_hook_data = Some(slots.encode());
        }
        if eligible(&args.destination_owner, &excluded) {
            let mut slots = Slots::decode(&args.destination_hook_data);
            let after = args.destination_balance.saturating_add(args.amount);
            streak_on_receive(
                header,
                streak,
                &mut slots,
                args.destination_balance,
                after,
                now,
            );
            answer.destination_hook_data = Some(slots.encode());
        }
        Ok(answer)
    }

    /// Before a burn: like a send.
    pub fn before_burn(ctx: Context<Callback>, args: TokenHookArgs) -> Result<HookReturn> {
        check_call(&ctx, &args)?;
        let now = Clock::get()?.unix_timestamp;
        let state = &mut ctx.accounts.state;
        state.header.roll(now);
        remember_pool(state, &ctx.accounts.launch);
        if args.op != TokenOp::Burn {
            return Ok(HookReturn::default());
        }
        let mut answer = HookReturn::default();
        if eligible(&args.source_owner, &state.excluded()) {
            let mut slots = Slots::decode(&args.source_hook_data);
            streak_on_send(
                &mut state.header,
                &mut slots,
                args.source_balance.saturating_sub(args.amount),
                now,
            );
            answer.source_hook_data = Some(slots.encode());
        }
        Ok(answer)
    }
}

/// Remembers the launch's pool once the launch can be read (not during its own transaction).
fn remember_pool(state: &mut StreakState, launch: &AccountInfo) {
    if state.pool != Pubkey::default() {
        return;
    }
    if let Some(view) = read_launch(launch, &state.header.mint) {
        state.pool = view.pool;
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

/// The state for one mint, at `["state", mint]`: the standard's header first, the streak's after
/// it (the companion reads both at the standard's offsets), then this hook's own fields.
#[account]
#[derive(InitSpace)]
pub struct StreakState {
    /// The standard's header: the epochs (rounds of `EPOCH_SECS`) and their exact weight totals.
    pub header: GameHeader,
    /// The streak header: the minimum streak and weight.
    pub streak: StreakHeader,
    /// Layout version.
    pub version: u8,
    /// Bump.
    pub bump: u8,
    /// The launchpad's `["launch", mint]`: never shares.
    pub launch: Pubkey,
    /// The launch's pool, once read (default until then): never shares.
    pub pool: Pubkey,
    /// The companion's `["creator", mint]`: never shares.
    pub creator: Pubkey,
    /// Who sent `prepare`.
    pub prepared_by: Pubkey,
    /// Reserved.
    pub reserved: [u8; 32],
}

impl StreakState {
    /// The owners that never share, besides the default key and every address off the curve.
    pub fn excluded(&self) -> [Pubkey; 3] {
        [self.launch, self.pool, self.creator]
    }
}

/// Accounts of the standard `prepare` (the same in every Studio hook).
#[derive(Accounts)]
pub struct Prepare<'info> {
    /// Pays the rent.
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: the mint the hook is prepared for; need not exist yet.
    pub mint: UncheckedAccount<'info>,
    #[account(init, payer = payer, space = 8 + StreakState::INIT_SPACE, seeds = [STATE_SEED, mint.key().as_ref()], bump)]
    pub state: Box<Account<'info, StreakState>>,
    /// CHECK: the registry PDA, written here (address-checked in the handler).
    #[account(mut)]
    pub registry: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Accounts of `enter`.
#[derive(Accounts)]
pub struct Enter<'info> {
    #[account(mut, seeds = [STATE_SEED, mint.key().as_ref()], bump = state.bump)]
    pub state: Box<Account<'info, StreakState>>,
    /// CHECK: the mint (the state's; the token program checks the holding is of it and that its
    /// hook is this program).
    pub mint: UncheckedAccount<'info>,
    /// CHECK: the holding, read as the token program's; the token program writes its hook data.
    #[account(mut)]
    pub holding: UncheckedAccount<'info>,
    /// CHECK: this program's `["hook-authority"]` (checked by `write_own_hook_data`).
    pub hook_authority: UncheckedAccount<'info>,
    /// CHECK: the token program (address-checked).
    #[account(address = TOKEN_PROGRAM @ HookError::WrongAccount)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: its event authority (address-checked).
    #[account(address = TOKEN_EVENT_AUTHORITY @ HookError::WrongAccount)]
    pub token_event_authority: UncheckedAccount<'info>,
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
    pub state: Box<Account<'info, StreakState>>,
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
    #[msg("this owner can't share: the launch, its pool, the companion's creator address, or an address off the ed25519 curve")]
    NotEligible,
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
