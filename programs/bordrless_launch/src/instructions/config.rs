//! The config: created once by the upgrade authority, changed by the admin.

use anchor_lang::prelude::*;
use bordrless_core::{curve_params, BPS};

use crate::constants::*;
use crate::error::LaunchError;
use crate::events::ConfigSet;
use crate::state::{Config, RuleBounds};

/// Arguments of `init_config` and `set_config`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ConfigArgs {
    /// The admin.
    pub admin: Pubkey,
    /// Receives launch fees; must also be the DEX config's treasury.
    pub treasury: Pubkey,
    /// The quote every launch is paired with.
    pub quote_mint: Pubkey,
    /// Lamports paid to launch.
    pub launch_fee_lamports: u64,
    /// LP fee of launch pools.
    pub lp_fee_bps: u16,
    /// Largest creator fee.
    pub max_creator_fee_bps: u16,
    /// Seconds of elevated LP fee after creation.
    pub sniper_window_secs: i64,
    /// LP fee at the start of the window.
    pub sniper_start_bps: u16,
    /// Share of the supply sold on the curve.
    pub curve_bps: u16,
    /// Supply of a launched token, base units.
    pub supply: u64,
    /// Decimals of a launched token.
    pub decimals: u8,
    /// Smallest virtual quote reserve.
    pub min_virtual_quote: u64,
    /// Largest virtual quote reserve.
    pub max_virtual_quote: u64,
    /// Stops new launches.
    pub paused: bool,
    /// The bounds of token rules, each within the hard ceilings.
    pub rule_bounds: RuleBounds,
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
        LaunchError::NotUpgradeAuthority
    );
    require_keys_eq!(
        *program_data.owner,
        BPF_LOADER_UPGRADEABLE_ID,
        LaunchError::NotUpgradeAuthority
    );
    let data = program_data.try_borrow_data()?;
    require!(
        data.len() >= 45 && data[..4] == [3, 0, 0, 0],
        LaunchError::NotUpgradeAuthority
    );
    if data[12] == 0 {
        return Ok(None);
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&data[13..45]);
    Ok(Some(Pubkey::new_from_array(key)))
}

fn check_args(args: &ConfigArgs) -> Result<()> {
    require!(
        args.lp_fee_bps <= bordrless_swap::constants::MAX_LP_FEE_BPS,
        LaunchError::InvalidConfig
    );
    require!(
        args.sniper_start_bps <= bordrless_swap::constants::MAX_LP_FEE_BPS
            && args.sniper_start_bps >= args.lp_fee_bps,
        LaunchError::InvalidConfig
    );
    require!(
        args.sniper_window_secs >= 0 && args.sniper_window_secs <= 3_600,
        LaunchError::InvalidConfig
    );
    require!(
        args.max_creator_fee_bps <= 1_000,
        LaunchError::InvalidConfig
    );
    require!(
        u64::from(args.curve_bps) > BPS / 2 && u64::from(args.curve_bps) < BPS,
        LaunchError::InvalidConfig
    );
    // A supply the kit can install on: at least 1,000 base units (so its threshold is at least
    // one unit) and at most what bounds its reward math.
    require!(
        (ceilings::MIN_SUPPLY..=ceilings::MAX_SUPPLY).contains(&args.supply) && args.decimals <= 12,
        LaunchError::InvalidConfig
    );
    check_rule_bounds(&args.rule_bounds)?;
    require!(
        args.min_virtual_quote > 0 && args.min_virtual_quote <= args.max_virtual_quote,
        LaunchError::InvalidConfig
    );
    // The curve must be derivable at both bounds.
    require!(
        curve_params(
            args.supply,
            u64::from(args.curve_bps),
            args.min_virtual_quote
        )
        .is_some(),
        LaunchError::InvalidConfig
    );
    require!(
        curve_params(
            args.supply,
            u64::from(args.curve_bps),
            args.max_virtual_quote
        )
        .is_some(),
        LaunchError::InvalidConfig
    );
    Ok(())
}

/// The token-rule bounds within the hard ceilings no config can raise (§5.2).
pub fn check_rule_bounds(b: &RuleBounds) -> Result<()> {
    require!(
        b.max_holder_fee_bps <= ceilings::HOLDER_FEE_BPS
            && b.max_burn_bps <= ceilings::BURN_BPS
            && b.max_rules_fee_bps <= ceilings::RULES_FEE_BPS,
        LaunchError::InvalidConfig
    );
    require!(
        ceilings::MIN_MAX_WALLET_BPS <= b.min_max_wallet_bps
            && b.min_max_wallet_bps <= b.max_max_wallet_bps
            && b.max_max_wallet_bps <= ceilings::MAX_WALLET_BPS,
        LaunchError::InvalidConfig
    );
    require!(
        b.max_creator_lock_secs <= ceilings::CREATOR_LOCK_SECS
            && b.max_early_window_secs <= ceilings::EARLY_WINDOW_SECS
            && b.max_early_lock_secs <= ceilings::EARLY_LOCK_SECS,
        LaunchError::InvalidConfig
    );
    Ok(())
}

fn apply(config: &mut Config, args: &ConfigArgs) {
    config.admin = args.admin;
    config.treasury = args.treasury;
    config.quote_mint = args.quote_mint;
    config.launch_fee_lamports = args.launch_fee_lamports;
    config.lp_fee_bps = args.lp_fee_bps;
    config.max_creator_fee_bps = args.max_creator_fee_bps;
    config.sniper_window_secs = args.sniper_window_secs;
    config.sniper_start_bps = args.sniper_start_bps;
    config.curve_bps = args.curve_bps;
    config.supply = args.supply;
    config.decimals = args.decimals;
    config.min_virtual_quote = args.min_virtual_quote;
    config.max_virtual_quote = args.max_virtual_quote;
    config.paused = args.paused;
    config.rule_bounds = args.rule_bounds;
}

fn event(args: &ConfigArgs) -> Result<ConfigSet> {
    Ok(ConfigSet {
        admin: args.admin,
        treasury: args.treasury,
        quote_mint: args.quote_mint,
        launch_fee_lamports: args.launch_fee_lamports,
        lp_fee_bps: args.lp_fee_bps,
        max_creator_fee_bps: args.max_creator_fee_bps,
        sniper_window_secs: args.sniper_window_secs,
        sniper_start_bps: args.sniper_start_bps,
        curve_bps: args.curve_bps,
        supply: args.supply,
        decimals: args.decimals,
        min_virtual_quote: args.min_virtual_quote,
        max_virtual_quote: args.max_virtual_quote,
        paused: args.paused,
        rule_bounds: args.rule_bounds,
        ts: Clock::get()?.unix_timestamp,
    })
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
        LaunchError::NotUpgradeAuthority
    );
    check_args(&args)?;
    let config = &mut ctx.accounts.config;
    config.version = VERSION;
    config.bump = ctx.bumps.config;
    config.launches = 0;
    config.reserved = [0; 64];
    apply(config, &args);
    let ev = event(&args)?;
    emit_cpi!(ev);
    Ok(())
}

/// Accounts of `set_config`.
#[event_cpi]
#[derive(Accounts)]
pub struct SetConfig<'info> {
    /// The admin.
    pub admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ LaunchError::NotAdmin)]
    pub config: Account<'info, Config>,
}

/// `set_config`.
pub fn process_set_config(ctx: Context<SetConfig>, args: ConfigArgs) -> Result<()> {
    check_args(&args)?;
    apply(&mut ctx.accounts.config, &args);
    let ev = event(&args)?;
    emit_cpi!(ev);
    Ok(())
}
