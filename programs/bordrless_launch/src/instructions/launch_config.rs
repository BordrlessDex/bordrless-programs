//! Launch configs (`docs/hooks-v2.md` §5.7, §5.8): a `LaunchConfig` anyone makes with the SDK,
//! holding the rules, the creator fee and, for a build-your-own token, the creator's own token
//! hook. It is a keypair account (its key is what a creator pastes into the launch form), fixed
//! once created, checked against the launch config's bounds here and again at every launch that
//! uses it. It can never touch the protocol's share.

use anchor_lang::prelude::*;
use bordrless_hook::token_flags;

use crate::constants::*;
use crate::error::LaunchError;
use crate::events::{ConfigListed, LaunchConfigCreated, PartnerConfigured, QuoteCutConfigured};
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

/// Accounts of `create_partner_config`: those of `create_config`, and the partner's account.
#[event_cpi]
#[derive(Accounts)]
pub struct CreatePartnerConfig<'info> {
    /// Whoever makes the config (its author): pays the rent of both accounts.
    #[account(mut)]
    pub creator: Signer<'info>,
    /// The launch config, for the bounds.
    #[account(address = CONFIG_ADDRESS)]
    pub config: Box<Account<'info, Config>>,
    /// The new config: a keypair that signs.
    #[account(init, payer = creator, space = LaunchConfig::LEN)]
    pub launch_config: Box<Account<'info, LaunchConfig>>,
    /// The config's partner, made with it.
    #[account(init, payer = creator, space = PartnerFee::LEN, seeds = [PARTNER_SEED, launch_config.key().as_ref()], bump)]
    pub partner_fee: Box<Account<'info, PartnerFee>>,
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

/// The program data address of an upgradeable program: `PDA([program], upgradeable loader)`.
pub fn programdata_address(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[program.as_ref()], &BPF_LOADER_UPGRADEABLE_ID).0
}

/// Who may upgrade the custom hook (§5.8; `docs/phase3a.md` §2): no one, Studio's upgrade key or
/// the protocol's (see `HOOK_UPGRADE_AUTHORITIES`), or the hook's own `hook_timelock` (a delay of at
/// least 3 days, `timelock` its `Timelock` account), else `HookUpgradeable`. `programdata` is the
/// account at [`programdata_address`], which every client passes with a custom hook (for a program
/// that is not upgradeable it is an unused address, read for nothing).
///
/// The classification is `bordrless_hook::authority::classify`, which reads exactly the bytes this
/// check always read: the upgradeable loader's program (`[2u32, programdata]`) and ProgramData
/// (`[3u32, slot u64, Option<Pubkey>]`), the BPF loader 2 (immutable) and loader v4 (`[slot u64,
/// authority, status u64]`, status 2 being finalized). Only a hook whose authority is its timelock
/// address reads `timelock` (and fails `HookTimelockInvalid` without a valid one); every other hook
/// is checked exactly as before, whatever else is passed.
///
/// A timelocked hook is taken only while its `Timelock` holds no proposal (`HookTimelockPending`):
/// its "N days of public notice" must be whole for the coin's first buyers, so a creator can't
/// stage other code, wait out the delay, then make the config and launch with the switch
/// executable. Answers whether the hook is timelocked (the config records it: every launch from it
/// checks the timelock again, [`check_timelock_idle`]).
pub fn check_hook_authority(
    program: &AccountInfo,
    programdata: &AccountInfo,
    timelock: Option<&AccountInfo>,
) -> Result<bool> {
    use bordrless_hook::authority::{classify, AuthorityClass, ClassError};
    require_keys_eq!(
        *programdata.key,
        programdata_address(program.key),
        LaunchError::HookProgramDataMissing
    );
    match classify(program, programdata, timelock) {
        Ok(AuthorityClass::Immutable) | Ok(AuthorityClass::Protocol(_)) => Ok(false),
        Ok(AuthorityClass::Timelocked { .. }) => {
            check_timelock_idle(program.key, timelock)?;
            Ok(true)
        }
        Ok(AuthorityClass::Author(_)) => err!(LaunchError::HookUpgradeable),
        Err(ClassError::WrongProgramData) => err!(LaunchError::HookProgramDataMissing),
        Err(ClassError::TimelockMissing) | Err(ClassError::TimelockInvalid) => {
            err!(LaunchError::HookTimelockInvalid)
        }
    }
}

/// The custom hook `hook`'s `Timelock` (`timelock`: owned by `hook_timelock`, at the address its
/// bump gives for `hook`, a `Timelock` of `hook`; else `HookTimelockInvalid`) holds no proposal
/// (else `HookTimelockPending`). A finalized timelock (its program made immutable) holds none.
pub fn check_timelock_idle(hook: &Pubkey, timelock: Option<&AccountInfo>) -> Result<()> {
    use bordrless_hook::authority::{parse_timelock, HOOK_TIMELOCK_ID, TIMELOCK_SEED};
    let info = timelock.ok_or(LaunchError::HookTimelockInvalid)?;
    require_keys_eq!(
        *info.owner,
        HOOK_TIMELOCK_ID,
        LaunchError::HookTimelockInvalid
    );
    let view = parse_timelock(&info.try_borrow_data()?).ok_or(LaunchError::HookTimelockInvalid)?;
    let at = Pubkey::create_program_address(
        &[TIMELOCK_SEED, hook.as_ref(), &[view.bump]],
        &HOOK_TIMELOCK_ID,
    )
    .map_err(|_| error!(LaunchError::HookTimelockInvalid))?;
    require!(
        at == *info.key && view.program == *hook,
        LaunchError::HookTimelockInvalid
    );
    require!(view.pending.is_none(), LaunchError::HookTimelockPending);
    Ok(())
}

/// `create_config`.
pub fn process_create_config(ctx: Context<CreateConfig>, args: CreateConfigArgs) -> Result<()> {
    make_config(ctx, args, 0, None)
}

/// `create_quote_cut_config`: a config whose custom hook also takes a SOL cut on every trade
/// (`docs/quote-cut.md`): the hook's `quote_cut` answers a rate, the pool hook takes at most `cut`'s
/// cap of the trade's SOL with the creator fee. `author_share_bps` 0 makes it as `create_config`
/// does, else as `create_listed_config` (the author's share covers the SOL cut too: it is part of
/// the creator fee).
pub fn process_create_quote_cut_config(
    ctx: Context<CreateConfig>,
    args: CreateConfigArgs,
    author_share_bps: u16,
    cut: QuoteCut,
) -> Result<()> {
    require!(
        args.custom_hook.is_some() && cut.valid(),
        LaunchError::InvalidQuoteCut
    );
    require!(
        author_share_bps <= MAX_AUTHOR_SHARE_BPS,
        LaunchError::InvalidAuthorShare
    );
    make_config(ctx, args, author_share_bps, Some(cut))
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
    make_config(ctx, args, author_share_bps, None)
}

/// What every config instruction checks and writes: the label, the creator fee within the
/// launch config's maximum, the rules, the custom hook and who may upgrade it (its program data,
/// and a timelocked hook's `Timelock`, as the first remaining accounts), then the config itself.
/// Returns the time it was made.
#[allow(clippy::too_many_arguments)]
fn fill_config(
    config: &Config,
    lc: &mut LaunchConfig,
    creator: Pubkey,
    program: Option<&AccountInfo>,
    remaining: &[AccountInfo],
    args: &CreateConfigArgs,
    author_share_bps: u16,
    quote_cut: Option<QuoteCut>,
) -> Result<i64> {
    let now = Clock::get()?.unix_timestamp;
    require!(args.label.len() <= LABEL_MAX, LaunchError::InvalidLabel);
    require!(
        args.creator_fee_bps <= config.max_creator_fee_bps,
        LaunchError::CreatorFeeTooHigh
    );
    check_rules(&args.rules, args.creator_fee_bps, config)?;
    check_custom_hook(
        args.custom_hook,
        args.custom_hook_flags,
        &args.rules,
        program,
    )?;
    // A custom hook's program data follows as the first remaining account: who may upgrade it.
    // A timelocked hook's `Timelock` follows it (`docs/phase3a.md` §3.6).
    let mut timelocked = false;
    if let Some(program) = program {
        let programdata = remaining
            .first()
            .ok_or(LaunchError::HookProgramDataMissing)?;
        timelocked = check_hook_authority(program, programdata, remaining.get(1))?;
    }
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
    if timelocked {
        lc.reserved[0] = LaunchConfig::TIMELOCKED_HOOK;
    }
    if let Some(cut) = &quote_cut {
        lc.set_quote_cut(cut);
    }
    Ok(now)
}

fn make_config(
    ctx: Context<CreateConfig>,
    args: CreateConfigArgs,
    author_share_bps: u16,
    quote_cut: Option<QuoteCut>,
) -> Result<()> {
    let program = ctx
        .accounts
        .hook_program
        .as_ref()
        .map(|a| a.to_account_info());
    let creator = ctx.accounts.creator.key();
    let key = ctx.accounts.launch_config.key();
    let now = fill_config(
        &ctx.accounts.config,
        &mut ctx.accounts.launch_config,
        creator,
        program.as_ref(),
        ctx.remaining_accounts,
        &args,
        author_share_bps,
        quote_cut,
    )?;
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
    if let (Some(cut), Some(hook)) = (quote_cut, args.custom_hook) {
        emit_cpi!(QuoteCutConfigured {
            config: key,
            hook,
            max_buy_bps: cut.max_buy_bps,
            max_sell_bps: cut.max_sell_bps,
            ts: now,
        });
    }
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

/// `create_partner_config` (`docs/partner-fee.md`): a config as `create_config` makes it, whose
/// every launch pays `partner` `partner_share_bps` of its creator fee (1 to 10,000 basis points of
/// it, whoever launches), fixed for ever. It may also carry an author's share (0 to 5,000, listed
/// when above 0, paid as a listed config's is: only on launches its author does not make) and a SOL
/// cut (`cut`, zeros for none: as `create_quote_cut_config`). The author's and the partner's
/// shares are at most 10,000 in all; the launch's creator gets the rest.
pub fn process_create_partner_config(
    ctx: Context<CreatePartnerConfig>,
    args: CreateConfigArgs,
    author_share_bps: u16,
    cut: QuoteCut,
    partner: Pubkey,
    partner_share_bps: u16,
) -> Result<()> {
    require!(partner != Pubkey::default(), LaunchError::InvalidPartner);
    require!(
        author_share_bps <= MAX_AUTHOR_SHARE_BPS,
        LaunchError::InvalidAuthorShare
    );
    require!(
        partner_share_bps > 0
            && partner_share_bps <= MAX_PARTNER_SHARE_BPS
            && u32::from(author_share_bps) + u32::from(partner_share_bps)
                <= u32::from(MAX_PARTNER_SHARE_BPS),
        LaunchError::InvalidPartnerShare
    );
    let quote_cut = if cut.on() {
        require!(
            args.custom_hook.is_some() && cut.valid(),
            LaunchError::InvalidQuoteCut
        );
        Some(cut)
    } else {
        None
    };
    let program = ctx
        .accounts
        .hook_program
        .as_ref()
        .map(|a| a.to_account_info());
    let creator = ctx.accounts.creator.key();
    let key = ctx.accounts.launch_config.key();
    let bump = ctx.bumps.partner_fee;
    let now = fill_config(
        &ctx.accounts.config,
        &mut ctx.accounts.launch_config,
        creator,
        program.as_ref(),
        ctx.remaining_accounts,
        &args,
        author_share_bps,
        quote_cut,
    )?;
    ctx.accounts
        .launch_config
        .set_partner(partner_share_bps, bump);
    let pf = &mut ctx.accounts.partner_fee;
    pf.version = VERSION;
    pf.bump = bump;
    pf.config = key;
    pf.partner = partner;
    pf.share_bps = partner_share_bps;
    pf.paid_total = 0;
    pf.reserved = [0; 32];
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
    if let (Some(cut), Some(hook)) = (quote_cut, args.custom_hook) {
        emit_cpi!(QuoteCutConfigured {
            config: key,
            hook,
            max_buy_bps: cut.max_buy_bps,
            max_sell_bps: cut.max_sell_bps,
            ts: now,
        });
    }
    if author_share_bps > 0 {
        emit_cpi!(ConfigListed {
            config: key,
            author: creator,
            author_share_bps,
            ts: now,
        });
    }
    emit_cpi!(PartnerConfigured {
        config: key,
        partner,
        partner_share_bps,
        ts: now,
    });
    Ok(())
}
