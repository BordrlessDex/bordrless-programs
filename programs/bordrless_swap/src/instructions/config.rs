//! The config: created once by the upgrade authority, changed by the admin.

use anchor_lang::prelude::*;

use crate::constants::*;
use crate::error::SwapError;
use crate::events::ConfigSet;
use crate::state::Config;

/// Arguments of `init_config` and `set_config`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ConfigArgs {
    /// The admin.
    pub admin: Pubkey,
    /// Protocol fee of ordinary pools created from now on.
    pub protocol_fee_bps: u16,
    /// Whose holdings receive collected protocol fees.
    pub fee_collector: Pubkey,
    /// Receives pool creation fees.
    pub treasury: Pubkey,
    /// Lamports paid to create a pool.
    pub pool_creation_fee_lamports: u64,
    /// Stops swaps and liquidity changes.
    pub paused: bool,
    /// Bordrless's share of what the hooks cut on launch pools (curves a hook program creates)
    /// created from now on, in basis points of the cuts.
    pub launch_protocol_share_bps: u16,
}

/// The upgrade authority of `program_id`, read from its ProgramData account (loader v3 layout:
/// a u32 tag of 3, the slot, then an optional authority).
pub fn upgrade_authority(
    program_data: &AccountInfo,
    program_id: &Pubkey,
) -> Result<Option<Pubkey>> {
    let (expected, _) =
        Pubkey::find_program_address(&[program_id.as_ref()], &BPF_LOADER_UPGRADEABLE_ID);
    require_keys_eq!(*program_data.key, expected, SwapError::NotUpgradeAuthority);
    require_keys_eq!(
        *program_data.owner,
        BPF_LOADER_UPGRADEABLE_ID,
        SwapError::NotUpgradeAuthority
    );
    let data = program_data.try_borrow_data()?;
    require!(
        data.len() >= 45 && data[..4] == [3, 0, 0, 0],
        SwapError::NotUpgradeAuthority
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

fn check_args(args: &ConfigArgs) -> Result<()> {
    require!(
        args.protocol_fee_bps <= MAX_PROTOCOL_FEE_BPS,
        SwapError::FeeTooHigh
    );
    require!(
        args.launch_protocol_share_bps <= MAX_PROTOCOL_SHARE_BPS,
        SwapError::FeeTooHigh
    );
    Ok(())
}

fn apply(config: &mut Config, args: &ConfigArgs) {
    config.admin = args.admin;
    config.protocol_fee_bps = args.protocol_fee_bps;
    config.launch_protocol_share_bps = args.launch_protocol_share_bps;
    config.fee_collector = args.fee_collector;
    config.treasury = args.treasury;
    config.pool_creation_fee_lamports = args.pool_creation_fee_lamports;
    config.paused = args.paused;
}

fn config_set(args: &ConfigArgs) -> Result<ConfigSet> {
    Ok(ConfigSet {
        admin: args.admin,
        protocol_fee_bps: args.protocol_fee_bps,
        launch_protocol_share_bps: args.launch_protocol_share_bps,
        fee_collector: args.fee_collector,
        treasury: args.treasury,
        pool_creation_fee_lamports: args.pool_creation_fee_lamports,
        paused: args.paused,
        ts: Clock::get()?.unix_timestamp,
    })
}

/// `init_config`.
pub fn process_init_config(ctx: Context<InitConfig>, args: ConfigArgs) -> Result<()> {
    let upgrade = upgrade_authority(&ctx.accounts.program_data, &crate::ID)?;
    require!(
        upgrade == Some(ctx.accounts.authority.key()),
        SwapError::NotUpgradeAuthority
    );
    check_args(&args)?;
    let config = &mut ctx.accounts.config;
    config.version = VERSION;
    config.bump = ctx.bumps.config;
    config.pools_created = 0;
    config.reserved = [0; 62];
    apply(config, &args);
    emit_cpi!(config_set(&args)?);
    Ok(())
}

/// Accounts of `set_config`.
#[event_cpi]
#[derive(Accounts)]
pub struct SetConfig<'info> {
    /// The admin.
    pub admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ SwapError::NotAdmin)]
    pub config: Account<'info, Config>,
}

/// `set_config`.
pub fn process_set_config(ctx: Context<SetConfig>, args: ConfigArgs) -> Result<()> {
    check_args(&args)?;
    let config = &mut ctx.accounts.config;
    apply(config, &args);
    emit_cpi!(config_set(&args)?);
    Ok(())
}
