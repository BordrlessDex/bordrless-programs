//! Launch configs (`docs/hooks-v2.md` §5.7, §5.8): a `LaunchConfig` anyone makes with the SDK,
//! holding the rules, the creator fee and, for a build-your-own token, the creator's own token
//! hook. It is a keypair account (its key is what a creator pastes into the launch form), fixed
//! once created, checked against the launch config's bounds here and again at every launch that
//! uses it. It can never touch the protocol's share.

use anchor_lang::prelude::*;
use bordrless_hook::token_flags;

use crate::constants::*;
use crate::error::LaunchError;
use crate::events::{ConfigListed, LaunchConfigCreated};
use crate::instructions::launch::check_rules;
use crate::state::*;

/// Arguments of `create_config`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct CreateConfigArgs {
    /// The token rules, within the launch config's bounds.
    pub rules: LaunchRules,
    /// Creator fee in basis points of the quote, at most the config's maximum.
    pub creator_fee_bps: u16,
    /// The creator's own token hook program, for a build-your-own token; then no kit rule may be
    /// on (holder rewards, max wallet, the creator wallet lock, the early-buyer lock), and the
    /// program must be executable and none of the protocol's. Burn and the creator fee stay pool
    /// hook rules and may be combined with it.
    pub custom_hook: Option<Pubkey>,
    /// The hook's flags (`bordrless_hook::token_flags`): at least one callback, no unknown bit;
    /// 0 without a custom hook.
    pub custom_hook_flags: u16,
    /// A short name (at most 32 bytes).
    pub label: String,
}

/// Accounts of `create_config`.
#[event_cpi]
#[derive(Accounts)]
pub struct CreateConfig<'info> {
    /// Whoever makes the config: pays the rent.
    #[account(mut)]
    pub creator: Signer<'info>,
    /// The launch config, for the bounds.
    #[account(address = CONFIG_ADDRESS)]
    pub config: Box<Account<'info, Config>>,
    /// The new config: a keypair that signs.
    #[account(init, payer = creator, space = LaunchConfig::LEN)]
    pub launch_config: Box<Account<'info, LaunchConfig>>,
    /// CHECK: the custom hook program, present exactly when `args.custom_hook` names one (this
    /// program's id for none); checked executable in the handler.
    pub hook_program: Option<UncheckedAccount<'info>>,
    pub system_program: Program<'info, System>,
}

/// The rules of a custom hook (§5.8): none without one (flags 0, no program account); with one,
/// no kit module, a program that is none of the protocol's, flags that name at least one callback
/// and no unknown bit, and the program account present and executable.
pub fn check_custom_hook(
    hook: Option<Pubkey>,
    flags: u16,
    rules: &LaunchRules,
    program: Option<&AccountInfo>,
) -> Result<()> {
    match hook {
        None => {
            require!(flags == 0, LaunchError::InvalidCustomHookFlags);
            require!(program.is_none(), LaunchError::UnexpectedCustomHookAccounts);
        }
        Some(h) => {
            require!(rules.modules() == 0, LaunchError::CustomHookWithKitRules);
            require!(
                !PROTOCOL_PROGRAMS.contains(&h),
                LaunchError::InvalidCustomHook
            );
            require!(
                flags != 0 && flags & !token_flags::ALL == 0,
                LaunchError::InvalidCustomHookFlags
            );
            let info = program.ok_or(LaunchError::CustomHookAccountsMissing)?;
            require_keys_eq!(*info.key, h, LaunchError::InvalidCustomHook);
            require!(info.executable, LaunchError::InvalidCustomHook);
        }
    }
    Ok(())
}

/// `create_config`.
pub fn process_create_config(ctx: Context<CreateConfig>, args: CreateConfigArgs) -> Result<()> {
    make_config(ctx, args, 0)
}

/// `create_listed_config`: `create_config`, plus the author's share of the creator fee on every
/// launch made from it by someone else (1 to `MAX_AUTHOR_SHARE_BPS` of it), fixed for ever.
pub fn process_create_listed_config(
    ctx: Context<CreateConfig>,
    args: CreateConfigArgs,
    author_share_bps: u16,
) -> Result<()> {
    require!(
        author_share_bps > 0 && author_share_bps <= MAX_AUTHOR_SHARE_BPS,
        LaunchError::InvalidAuthorShare
    );
    make_config(ctx, args, author_share_bps)
}

fn make_config(
    ctx: Context<CreateConfig>,
    args: CreateConfigArgs,
    author_share_bps: u16,
) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let config = &ctx.accounts.config;
    require!(args.label.len() <= LABEL_MAX, LaunchError::InvalidLabel);
    require!(
        args.creator_fee_bps <= config.max_creator_fee_bps,
        LaunchError::CreatorFeeTooHigh
    );
    check_rules(&args.rules, args.creator_fee_bps, config)?;
    let program = ctx
        .accounts
        .hook_program
        .as_ref()
        .map(|a| a.to_account_info());
    check_custom_hook(
        args.custom_hook,
        args.custom_hook_flags,
        &args.rules,
        program.as_ref(),
    )?;
    let creator = ctx.accounts.creator.key();
    let key = ctx.accounts.launch_config.key();
    let lc = &mut ctx.accounts.launch_config;
    lc.version = VERSION;
    lc.creator = creator;
    lc.rules = args.rules;
    lc.creator_fee_bps = args.creator_fee_bps;
    lc.custom_hook = args.custom_hook;
    lc.custom_hook_flags = args.custom_hook_flags;
    lc.label = args.label.clone();
    lc.created_at = now;
    lc.author_share_bps = author_share_bps;
    lc.reserved = [0; 30];
    emit_cpi!(LaunchConfigCreated {
        config: key,
        creator,
        rules: args.rules,
        creator_fee_bps: args.creator_fee_bps,
        custom_hook: args.custom_hook,
        custom_hook_flags: args.custom_hook_flags,
        label: args.label,
        ts: now,
    });
    if author_share_bps > 0 {
        emit_cpi!(ConfigListed {
            config: key,
            author: creator,
            author_share_bps,
            ts: now,
        });
    }
    Ok(())
}
