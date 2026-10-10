//! Studio's pro-rata strategy starter (phase 3a, `docs/phase3a.md` §4.9): each period pays a share of
//! the pot to its holders by weight (tokens held through the period, the lottery hook's tickets).
//!
//! - `plan`: the budget is `budget_bps_of_max` of what the companion allows (`budget_max`: at most half
//!   the pot), fixed at `prepare`.
//! - `entitle`: `budget * weight / total`, never more than `max_amount` (the companion refuses an
//!   answer above it rather than clamping, so a strategy keeps within it itself). Summed over every
//!   holder it never exceeds the budget.
//! - `prepare` (once, signed by the mint, before the strategy game is made): the strategy's state for the
//!   mint and its registry naming it. No other instruction: a strategy keeps no admin switch.
//!
//! It makes no CPI but `prepare`'s account creation, moves no lamports and reads only the arguments
//! and its own state.

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use bordrless_hook::{AccountSource, ExtraAccount, HookAccountList, Seed};
use bordrless_strategy::{pro_rata, EntitleArgs, Entitlement, PlanArgs, PlanDecision, REGISTRY_SEED};

declare_id!("FhNdJpvN52Ecidb9dW3GkVQPTThVQGzE2m5FyCuxsj3s");

/// `PDA(["state", mint])`: the starter's settings for a mint.
pub const STATE_SEED: &[u8] = b"state";

#[program]
pub mod strategy_pro_rata {
    use super::*;

    /// The state for `mint` (its budget share) and the registry naming it.
    pub fn prepare(ctx: Context<Prepare>, budget_bps_of_max: u16) -> Result<()> {
        require!(
            (1..=10_000).contains(&budget_bps_of_max),
            StarterError::BadSettings
        );
        let s = &mut ctx.accounts.state;
        s.mint = ctx.accounts.mint.key();
        s.budget_bps_of_max = budget_bps_of_max;
        s.bump = ctx.bumps.state;
        let list = HookAccountList::new(vec![ExtraAccount {
            writable: false,
            source: AccountSource::Pda {
                program: crate::ID,
                seeds: vec![Seed::Literal(STATE_SEED.to_vec()), Seed::Account(0)],
            },
        }]);
        // The registry's bytes are the list (the magic, then Borsh), at its seeds, owned by this
        // program: created here at exactly that size. An address someone already sent lamports to
        // is topped up, allocated and assigned (as Anchor's `init` does), so nobody can block a
        // mint's `prepare` by funding its registry's address first.
        let bytes = list.encode();
        let mint = ctx.accounts.mint.key();
        let seeds: &[&[u8]] = &[REGISTRY_SEED, mint.as_ref(), &[ctx.bumps.registry]];
        let system = ctx.accounts.system_program.key();
        let payer = ctx.accounts.payer.to_account_info();
        let registry = ctx.accounts.registry.to_account_info();
        let rent = Rent::get()?.minimum_balance(bytes.len());
        use anchor_lang::system_program as sys;
        if registry.lamports() == 0 {
            sys::create_account(
                CpiContext::new_with_signer(
                    system,
                    sys::CreateAccount { from: payer, to: registry },
                    &[seeds],
                ),
                rent,
                bytes.len() as u64,
                &crate::ID,
            )?;
        } else {
            let top_up = rent.saturating_sub(registry.lamports());
            if top_up > 0 {
                sys::transfer(
                    CpiContext::new(system, sys::Transfer { from: payer, to: registry.clone() }),
                    top_up,
                )?;
            }
            sys::allocate(
                CpiContext::new_with_signer(
                    system,
                    sys::Allocate { account_to_allocate: registry.clone() },
                    &[seeds],
                ),
                bytes.len() as u64,
            )?;
            sys::assign(
                CpiContext::new_with_signer(
                    system,
                    sys::Assign { account_to_assign: registry },
                    &[seeds],
                ),
                &crate::ID,
            )?;
        }
        ctx.accounts.registry.try_borrow_mut_data()?[..bytes.len()].copy_from_slice(&bytes);
        Ok(())
    }

    /// The period's budget.
    pub fn plan(ctx: Context<Plan>, args: PlanArgs) -> Result<PlanDecision> {
        let s = &ctx.accounts.state;
        require_keys_eq!(s.mint, args.mint, StarterError::WrongMint);
        Ok(PlanDecision {
            budget: pro_rata(args.budget_max, u64::from(s.budget_bps_of_max), 10_000),
        })
    }

    /// One holding's amount: its share of the budget by weight, within `max_amount`.
    pub fn entitle(ctx: Context<Entitle>, args: EntitleArgs) -> Result<Entitlement> {
        require_keys_eq!(ctx.accounts.state.mint, args.mint, StarterError::WrongMint);
        Ok(Entitlement {
            amount: pro_rata(args.budget, args.weight, args.total).min(args.max_amount),
        })
    }
}

#[account]
#[derive(InitSpace)]
pub struct StarterState {
    pub mint: Pubkey,
    pub budget_bps_of_max: u16,
    /// The bump of the state's address, kept so `plan` and `entitle` check the address with one
    /// hash (`create_program_address`) instead of searching for the bump: their compute is then the
    /// same for every mint (a bump search costs about 1,500 units a try, and the number of tries
    /// depends on the mint).
    pub bump: u8,
}

#[derive(Accounts)]
pub struct Prepare<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// The coin's mint keypair, which signs (as `lottery_hook::prepare` asks): nobody else can fix
    /// a coin's terms before its creator does.
    pub mint: Signer<'info>,
    #[account(init, payer = payer, space = 8 + StarterState::INIT_SPACE, seeds = [STATE_SEED, mint.key().as_ref()], bump)]
    pub state: Account<'info, StarterState>,
    /// CHECK: the registry, created here (seeds-checked).
    #[account(mut, seeds = [REGISTRY_SEED, mint.key().as_ref()], bump)]
    pub registry: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// `plan`'s accounts: the companion's prefix (read, never trusted beyond the arguments), then the
/// starter's state.
#[derive(Accounts)]
pub struct Plan<'info> {
    /// CHECK: the game.
    pub game: UncheckedAccount<'info>,
    /// CHECK: the companion.
    pub companion: UncheckedAccount<'info>,
    /// CHECK: the ticket hook's state.
    pub hook_state: UncheckedAccount<'info>,
    /// CHECK: the launch.
    pub launch: UncheckedAccount<'info>,
    /// CHECK: the pool.
    pub pool: UncheckedAccount<'info>,
    #[account(seeds = [STATE_SEED, state.mint.as_ref()], bump = state.bump)]
    pub state: Account<'info, StarterState>,
}

/// `entitle`'s accounts: the companion's prefix, then the starter's state.
#[derive(Accounts)]
pub struct Entitle<'info> {
    /// CHECK: the game.
    pub game: UncheckedAccount<'info>,
    /// CHECK: the companion.
    pub companion: UncheckedAccount<'info>,
    /// CHECK: the ticket hook's state.
    pub hook_state: UncheckedAccount<'info>,
    /// CHECK: the launch.
    pub launch: UncheckedAccount<'info>,
    /// CHECK: the candidate's holding.
    pub holding: UncheckedAccount<'info>,
    #[account(seeds = [STATE_SEED, state.mint.as_ref()], bump = state.bump)]
    pub state: Account<'info, StarterState>,
}

#[error_code]
pub enum StarterError {
    #[msg("the budget share is 1 to 10,000 basis points of what the companion allows")]
    BadSettings,
    #[msg("the state is for another mint")]
    WrongMint,
}
