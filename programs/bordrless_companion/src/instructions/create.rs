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
    // No game until `create_game` (the v2 fields, zero as the reserved bytes they replace were).
    c.game_hook = Pubkey::default();
    c.pot_bps = 0;
    c.pending_pot = 0;
    c.round_secs = 0;
    c.stranded_burned_at = 0;
    c.game_kind = GameKind::Lottery;
    c.pot_locked = 0;
    c.reserved = [0; 1];
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

    // The launch as created: its creator this companion's, no custom hook but its game's (whose
    // state must be the game's), no config author paid (their claims would pay the companion
    // outside its steps).
    let launch = Account::<Launch>::try_from(&remaining[LAUNCH_AT])?;
    require_keys_eq!(launch.creator, creator, CompanionError::WrongCreator);
    if c.is_game() {
        check_game_launch(c, &launch, remaining)?;
    } else {
        require!(
            launch.custom_hook.is_none(),
            CompanionError::CustomHookUnsupported
        );
    }
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

/// A game companion's launch: the token's hook is the game's, with exactly the callbacks its kind
/// needs (`GameKind::hook_flags`: transfers and burns, writing hook data; no deltas, which could
/// skim the companion's buybacks, and no callback the hook may lack, which would fail every burn),
/// and the hook's state for the mint is a game ticket header for this mint with the game's round
/// length (and, for a jackpot or a streak, its kind's header after it). The state is the hook's own
/// extra account in `create_launch`'s, so the check needs no account of its own (nor the `Game`:
/// the companion keeps the hook, its kind and the round length).
fn check_game_launch(c: &Companion, launch: &Launch, remaining: &[AccountInfo]) -> Result<()> {
    require!(
        launch.custom_hook == Some(c.game_hook)
            && launch.custom_hook_flags == c.game_kind.hook_flags(),
        CompanionError::GameHookMismatch
    );
    let state = bordrless_game::state_address(&c.game_hook, &c.mint).0;
    let info = remaining
        .iter()
        .find(|a| *a.key == state)
        .ok_or(CompanionError::MissingAccount)?;
    let header = bordrless_game::read_state(info, &c.game_hook, &c.mint)
        .map_err(|_| error!(CompanionError::HookState))?;
    require!(header.round_secs == c.round_secs, CompanionError::HookState);
    let kind_header = match c.game_kind {
        GameKind::Lottery => true,
        GameKind::Jackpot => {
            bordrless_game::JackpotHeader::parse(&info.try_borrow_data()?).is_some()
        }
        GameKind::Streak => bordrless_game::StreakHeader::parse(&info.try_borrow_data()?).is_some(),
    };
    require!(kind_header, CompanionError::HookState);
    // A jackpot's or a streak's registry, as the launch carries it, still lists at most
    // `MAX_GAME_HOOK_EXTRAS_V2` extras (the hook may have rewritten it since `create_game_v2`).
    // A lottery's launch is phase 1's, unchanged.
    if c.game_kind != GameKind::Lottery {
        let registry = bordrless_hook::hook_accounts_address(&c.game_hook, &c.mint).0;
        let info = remaining
            .iter()
            .find(|a| *a.key == registry)
            .ok_or(CompanionError::MissingAccount)?;
        crate::instructions::game::check_hook_registry(
            info,
            &c.game_hook,
            &c.mint,
            MAX_GAME_HOOK_EXTRAS_V2,
        )?;
    }
    Ok(())
}
