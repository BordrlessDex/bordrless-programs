//! `create` and `launch`: a companion made for a mint that does not exist yet, then the launch
//! created through it, with the companion's creator address as the launch's creator.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::{system_program, InstructionData};
use bordrless_launch::instructions::CreateLaunchArgs;
use bordrless_launch::state::Launch;
use bordrless_swap::state::Pool;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::error::CompanionError;
use crate::events::{CompanionCreated, CompanionLaunched};
use crate::instructions::CreatorSeeds;
use crate::invoke::invoke_built;
use crate::state::*;

/// Arguments of `create`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct CreateArgs {
    pub split: Split,
    pub bounty_bps: u16,
    /// The most one buyback spends, lamports (ignored without a buyback share).
    pub max_buyback: u64,
    /// The least time between buybacks, seconds.
    pub buyback_interval: i64,
    /// The dev bag vests over this many seconds from the launch.
    pub vest_secs: i64,
    /// Lamports for the creator address: the launch's fee and rent (it pays them as the launch's
    /// creator) and its own rent-exempt minimum, which it keeps.
    pub fund: u64,
}

/// Accounts of `create`.
#[event_cpi]
#[derive(Accounts)]
pub struct Create<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: any address: who receives the beneficiary's part and the vested dev bag.
    pub beneficiary: UncheckedAccount<'info>,
    /// The launch's mint, a fresh keypair: it signs here, so only whoever holds it can make its
    /// companion (nobody can make one first and become its beneficiary), and the launch later.
    pub mint: Signer<'info>,
    #[account(init, payer = payer, space = Companion::LEN, seeds = [COMPANION_SEED, mint.key().as_ref()], bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address, `PDA(["creator", mint])`, system-owned and without data.
    #[account(mut, seeds = [CREATOR_SEED, mint.key().as_ref()], bump)]
    pub creator: UncheckedAccount<'info>,
    /// CHECK: bridged SOL (address-checked).
    #[account(address = BRIDGED_SOL_MINT)]
    pub bridged_sol_mint: UncheckedAccount<'info>,
    /// CHECK: the creator's holding of bridged SOL, created here (the token program checks it).
    #[account(mut)]
    pub creator_quote: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = TOKEN_ID)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority (the token program checks it).
    pub token_event_authority: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

pub fn process_create(ctx: Context<Create>, args: CreateArgs) -> Result<()> {
    require!(args.split.valid(), CompanionError::BadSplit);
    require!(
        args.bounty_bps <= MAX_BOUNTY_BPS,
        CompanionError::BountyTooHigh
    );
    if args.split.buyback_bps > 0 {
        require!(
            args.max_buyback >= MIN_MAX_BUYBACK
                && (MIN_BUYBACK_INTERVAL..=MAX_BUYBACK_INTERVAL).contains(&args.buyback_interval),
            CompanionError::BadBuybackLimits
        );
    }
    require!(
        (0..=MAX_VEST_SECS).contains(&args.vest_secs),
        CompanionError::VestTooLong
    );
    let rent_min = Rent::get()?.minimum_balance(0);
    let creator_after = ctx
        .accounts
        .creator
        .lamports()
        .checked_add(args.fund)
        .ok_or(CompanionError::MathOverflow)?;
    require!(creator_after >= rent_min, CompanionError::Underfunded);
    if args.fund > 0 {
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                system_program::Transfer {
                    from: ctx.accounts.payer.to_account_info(),
                    to: ctx.accounts.creator.to_account_info(),
                },
            ),
            args.fund,
        )?;
    }
    // The creator's bridged-SOL holding: creator fees are claimed into it.
    let creator = ctx.accounts.creator.key();
    let ix = token_client::create_holding(ctx.accounts.payer.key(), BRIDGED_SOL_MINT, creator);
    let available = ctx.accounts.to_account_infos();
    invoke_built(&ix, &available, &[])?;

    let mint = ctx.accounts.mint.key();
    let c = &mut ctx.accounts.companion;
    c.version = VERSION;
    c.bump = ctx.bumps.companion;
    c.creator_bump = ctx.bumps.creator;
    c.mint = mint;
    c.beneficiary = ctx.accounts.beneficiary.key();
    c.split = args.split;
    c.bounty_bps = args.bounty_bps;
    c.max_buyback = args.max_buyback;
    c.buyback_interval = args.buyback_interval;
    c.vest_secs = args.vest_secs;
    c.reserved = [0; 64];
    emit_cpi!(CompanionCreated {
        companion: c.key(),
        mint,
        creator,
        beneficiary: c.beneficiary,
        split: c.split,
        bounty_bps: c.bounty_bps,
        max_buyback: c.max_buyback,
        buyback_interval: c.buyback_interval,
        vest_secs: c.vest_secs,
    });
    Ok(())
}

/// Accounts of `launch`. The remaining accounts are exactly `create_launch`'s, in its order (the
/// launchpad's client builds them with the creator address as the creator); the launchpad checks
/// every one of them.
#[event_cpi]
#[derive(Accounts)]
pub struct LaunchIt<'info> {
    pub launcher: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked).
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    /// CHECK: the launchpad.
    #[account(address = LAUNCH_ID)]
    pub launch_program: UncheckedAccount<'info>,
}

/// `create_launch`'s first account is the creator, its fourth the mint, its fifth the launch.
const CREATOR_AT: usize = 0;
const MINT_AT: usize = 3;
const LAUNCH_AT: usize = 4;

pub fn process_launch<'info>(
    ctx: Context<'info, LaunchIt<'info>>,
    args: CreateLaunchArgs,
) -> Result<()> {
    let c = &ctx.accounts.companion;
    require!(!c.launched, CompanionError::AlreadyLaunched);
    // The companion's vesting replaces the kit's creator wallet lock, which would lock its creator
    // address (the dev bag) instead of a person; holders can only be paid with holder rewards on.
    require!(
        args.rules.creator_lock_secs == 0,
        CompanionError::CreatorLockUnsupported
    );
    if c.split.holders_bps > 0 {
        require!(args.rules.rewards_on(), CompanionError::HolderRewardsOff);
    }
    // With holder rewards the kit lets only wallets hold the token: the dev bag must have one to go to.
    if args.rules.rewards_on() {
        require!(
            c.beneficiary.is_on_curve(),
            CompanionError::BeneficiaryNotAWallet
        );
    }
    let remaining = ctx.remaining_accounts;
    require!(remaining.len() > LAUNCH_AT, CompanionError::MissingAccount);
    require_keys_eq!(
        *remaining[CREATOR_AT].key,
        ctx.accounts.creator.key(),
        CompanionError::WrongCreator
    );
    require_keys_eq!(*remaining[MINT_AT].key, c.mint, CompanionError::WrongMint);
    let creator = ctx.accounts.creator.key();
    let ix = Instruction {
        program_id: LAUNCH_ID,
        accounts: remaining
            .iter()
            .map(|a| AccountMeta {
                pubkey: *a.key,
                is_signer: a.is_signer || *a.key == creator,
                is_writable: a.is_writable,
            })
            .collect(),
        data: bordrless_launch::instruction::CreateLaunch { args }.data(),
    };
    let seeds = CreatorSeeds::new(c.mint, c.creator_bump);
    let mut available = remaining.to_vec();
    available.push(ctx.accounts.launch_program.to_account_info());
    available.push(ctx.accounts.creator.to_account_info());
    invoke_built(&ix, &available, &[&seeds.seeds()])?;

    // The launch as created: its creator this companion's, no custom hook (not supported yet), no
    // config author paid (their claims would pay the companion outside its steps).
    let launch = Account::<Launch>::try_from(&remaining[LAUNCH_AT])?;
    require_keys_eq!(launch.creator, creator, CompanionError::WrongCreator);
    require!(
        launch.custom_hook.is_none(),
        CompanionError::CustomHookUnsupported
    );
    require!(
        launch.author_share_bps == 0,
        CompanionError::AuthorShareUnsupported
    );
    // The buyback's reference price: the opening price, which nobody can have moved yet.
    let pool_info = remaining
        .iter()
        .find(|a| *a.key == launch.pool && *a.owner == SWAP_ID)
        .ok_or(CompanionError::MissingAccount)?;
    let pool = Pool::try_deserialize(&mut &pool_info.try_borrow_data()?[..])?;
    let reference = spot_price(
        pool.quote_reserve,
        pool.virtual_quote,
        pool.base_reserve,
        pool.virtual_base,
    )
    .ok_or(CompanionError::NoQuote)?;
    let now = Clock::get()?.unix_timestamp;
    let c = &mut ctx.accounts.companion;
    c.launched = true;
    c.launched_at = now;
    c.reference_price = reference;
    c.reference_at = now;
    emit_cpi!(CompanionLaunched {
        companion: c.key(),
        mint: c.mint,
        ts: now
    });
    Ok(())
}
