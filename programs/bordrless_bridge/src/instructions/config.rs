//! The config: created once by the upgrade authority, changed by the admin.

use anchor_lang::prelude::*;

use crate::constants::*;
use crate::error::BridgeError;
use crate::events::ConfigSet;
use crate::state::Config;

/// Arguments of `init_config` and `set_config`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ConfigArgs {
    /// The admin.
    pub admin: Pubkey,
    /// Stops wrapping.
    pub paused: bool,
}

/// The upgrade authority of `program_id`, read from its ProgramData account.
pub fn upgrade_authority(
    program_data: &AccountInfo,
    program_id: &Pubkey,
) -> Result<Option<Pubkey>> {
    let (expected, _) =
        Pubkey::find_program_address(&[program_id.as_ref()], &BPF_LOADER_UPGRADEABLE_ID);
    require_keys_eq!(
        *program_data.key,
        expected,
        BridgeError::NotUpgradeAuthority
    );
    require_keys_eq!(
        *program_data.owner,
        BPF_LOADER_UPGRADEABLE_ID,
        BridgeError::NotUpgradeAuthority
    );
    let data = program_data.try_borrow_data()?;
    require!(
        data.len() >= 45 && data[..4] == [3, 0, 0, 0],
        BridgeError::NotUpgradeAuthority
    );
    if data[12] == 0 {
        return Ok(None);
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&data[13..45]);
    Ok(Some(Pubkey::new_from_array(key)))
}

/// Accounts of `init_config`.
#[event_cpi]
#[derive(Accounts)]
pub struct InitConfig<'info> {
    /// The upgrade authority, paying the rent.
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(init, payer = authority, space = Config::LEN, seeds = [CONFIG_SEED], bump)]
    pub config: Account<'info, Config>,
    /// CHECK: this program's ProgramData, parsed in the handler.
    pub program_data: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// `init_config`.
pub fn process_init_config(ctx: Context<InitConfig>, args: ConfigArgs) -> Result<()> {
    let upgrade = upgrade_authority(&ctx.accounts.program_data, &crate::ID)?;
    require!(
        upgrade == Some(ctx.accounts.authority.key()),
        BridgeError::NotUpgradeAuthority
    );
    let config = &mut ctx.accounts.config;
    config.version = VERSION;
    config.bump = ctx.bumps.config;
    config.admin = args.admin;
    config.paused = args.paused;
    config.wrappers = 0;
    config.reserved = [0; 32];
    emit_cpi!(ConfigSet {
        admin: args.admin,
        paused: args.paused,
        ts: Clock::get()?.unix_timestamp
    });
    Ok(())
}

/// Accounts of `set_config`.
#[event_cpi]
#[derive(Accounts)]
pub struct SetConfig<'info> {
    /// The admin.
    pub admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ BridgeError::NotAdmin)]
    pub config: Account<'info, Config>,
}

/// `set_config`.
pub fn process_set_config(ctx: Context<SetConfig>, args: ConfigArgs) -> Result<()> {
    let config = &mut ctx.accounts.config;
    config.admin = args.admin;
    config.paused = args.paused;
    emit_cpi!(ConfigSet {
        admin: args.admin,
        paused: args.paused,
        ts: Clock::get()?.unix_timestamp
    });
    Ok(())
}
