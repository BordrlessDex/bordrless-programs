//! `graduate`: the launch tells the kit its pool graduated, which lifts max wallet
//! (`docs/hooks-v2.md` §4.10).

use anchor_lang::prelude::*;

use crate::constants::*;
use crate::error::KitError;
use crate::events::KitGraduated;
use crate::state::KitConfig;

/// Accounts of `graduate`.
#[event_cpi]
#[derive(Accounts)]
pub struct Graduate<'info> {
    /// The launch program's kit-caller PDA for the config's mint, at the config's stored bump
    /// (checked in the handler).
    pub kit_caller: Signer<'info>,
    #[account(mut)]
    pub kit_config: Account<'info, KitConfig>,
}

/// `graduate`: once.
pub fn process_graduate(ctx: Context<Graduate>) -> Result<()> {
    let config = &mut ctx.accounts.kit_config;
    let expected = Pubkey::create_program_address(
        &[
            KIT_CALLER_SEED,
            config.mint.as_ref(),
            &[config.kit_caller_bump],
        ],
        &LAUNCH_ID,
    )
    .map_err(|_| KitError::NotKitCaller)?;
    require_keys_eq!(
        ctx.accounts.kit_caller.key(),
        expected,
        KitError::NotKitCaller
    );
    require!(!config.graduated, KitError::AlreadyGraduated);
    config.graduated = true;
    let mint = config.mint;
    emit_cpi!(KitGraduated {
        mint,
        ts: Clock::get()?.unix_timestamp,
    });
    Ok(())
}
