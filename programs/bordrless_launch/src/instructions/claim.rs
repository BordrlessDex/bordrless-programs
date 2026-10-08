//! Creator fee claims. A launch made from a listed config by someone other than its author pays
//! the author `author_share_bps` of what each claim takes (rounded down; the creator gets the
//! rest), whoever claims: the creator with `claim_creator_fees`, the author with
//! `claim_author_fees`. Every other launch pays its creator alone, as before.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::error::LaunchError;
use crate::events::{AuthorFeesPaid, CreatorFeesClaimed};
use crate::instructions::launch::LaunchSeeds;
use crate::state::*;

/// Accounts of `claim_creator_fees`. A launch that pays its config's author also takes, as the
/// remaining accounts, the `LaunchConfig` (`launch.config`) and the author's holding of the quote
/// (writable).
#[event_cpi]
#[derive(Accounts)]
pub struct ClaimCreatorFees<'info> {
    /// The creator.
    pub creator: Signer<'info>,
    #[account(mut, seeds = [LAUNCH_SEED, launch.mint.as_ref()], bump = launch.bump, has_one = creator @ LaunchError::NotCreator)]
    pub launch: Account<'info, Launch>,
    /// CHECK: address-checked.
    #[account(address = launch.quote_mint @ LaunchError::WrongHolding)]
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(mut, address = launch.quote_holding @ LaunchError::WrongHolding)]
    pub launch_quote: UncheckedAccount<'info>,
    /// CHECK: the creator's holding of the quote (checked in the handler).
    #[account(mut)]
    pub creator_quote: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = bordrless_token::ID @ LaunchError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority.
    pub token_event_authority: UncheckedAccount<'info>,
}

/// Accounts of `claim_author_fees`.
#[event_cpi]
#[derive(Accounts)]
pub struct ClaimAuthorFees<'info> {
    /// The config's author.
    pub author: Signer<'info>,
    #[account(mut, seeds = [LAUNCH_SEED, launch.mint.as_ref()], bump = launch.bump)]
    pub launch: Account<'info, Launch>,
    /// The listed config the launch was made from.
    #[account(address = launch.config @ LaunchError::NoAuthorShare, constraint = launch_config.creator == author.key() @ LaunchError::NotAuthor)]
    pub launch_config: Account<'info, LaunchConfig>,
    /// CHECK: address-checked.
    #[account(address = launch.quote_mint @ LaunchError::WrongHolding)]
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(mut, address = launch.quote_holding @ LaunchError::WrongHolding)]
    pub launch_quote: UncheckedAccount<'info>,
    /// CHECK: the launch creator's holding of the quote (checked in the handler).
    #[account(mut)]
    pub creator_quote: UncheckedAccount<'info>,
    /// CHECK: the author's holding of the quote (checked in the handler).
    #[account(mut)]
    pub author_quote: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = bordrless_token::ID @ LaunchError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority.
    pub token_event_authority: UncheckedAccount<'info>,
}

/// The author's part of `amount` at `share_bps` (of 10,000), rounded down.
pub fn author_part(amount: u64, share_bps: u16) -> u64 {
    (u128::from(amount) * u128::from(share_bps) / 10_000) as u64
}

/// What a claim moves: everything in the launch's quote holding, to the creator and (with a
/// share) the author.
struct Payout<'a, 'info> {
    launch: &'a AccountInfo<'info>,
    launch_quote: &'a AccountInfo<'info>,
    quote_mint: &'a AccountInfo<'info>,
    token_program: &'a AccountInfo<'info>,
    token_event_authority: &'a AccountInfo<'info>,
    mint: Pubkey,
    bump: u8,
}

impl<'a, 'info> Payout<'a, 'info> {
    /// A transfer of `amount` of the quote from the launch's holding to `to` (the launch signs).
    fn send(&self, to: &AccountInfo<'info>, amount: u64) -> Result<()> {
        let launch_seeds = LaunchSeeds::new(self.mint, self.bump);
        let seeds = launch_seeds.seeds();
        let ix = token_client::transfer(
            *self.launch.key,
            *self.launch_quote.key,
            *to.key,
            *self.quote_mint.key,
            None,
            vec![],
            amount,
        );
        // Bridged SOL has no hook: the token program's id stands for its hook program and signer.
        invoke_signed(
            &ix,
            &[
                self.launch.clone(),
                self.launch_quote.clone(),
                to.clone(),
                self.quote_mint.clone(),
                self.token_program.clone(),
                self.token_program.clone(),
                self.token_event_authority.clone(),
                self.token_program.clone(),
            ],
            &[&seeds],
        )?;
        Ok(())
    }
}

/// The checks both claims share: the token program's event authority, the holdings, and
/// something to claim. Returns the amount.
fn claimable(launch_quote: &AccountInfo, token_event_authority: &AccountInfo) -> Result<u64> {
    require_keys_eq!(
        token_event_authority.key(),
        token_client::event_authority(),
        LaunchError::WrongProgram
    );
    let amount = token_client::read_holding(launch_quote)?.amount;
    require!(amount > 0, LaunchError::NothingToClaim);
    Ok(amount)
}

/// `claim_creator_fees`.
pub fn process_claim_creator_fees<'info>(
    ctx: Context<'info, ClaimCreatorFees<'info>>,
) -> Result<()> {
    let clock = Clock::get()?;
    let launch_key = ctx.accounts.launch.key();
    let quote_mint = ctx.accounts.launch.quote_mint;
    require_keys_eq!(
        ctx.accounts.creator_quote.key(),
        token_client::holding_address(&quote_mint, ctx.accounts.creator.key),
        LaunchError::WrongHolding
    );
    let amount = claimable(
        &ctx.accounts.launch_quote,
        &ctx.accounts.token_event_authority,
    )?;
    let share_bps = ctx.accounts.launch.author_share_bps;
    // With an author share: the listed config (to read its author) and the author's holding.
    let author = if share_bps > 0 {
        let [config_info, author_quote, ..] = ctx.remaining_accounts else {
            return err!(LaunchError::AuthorAccountsMissing);
        };
        require_keys_eq!(
            config_info.key(),
            ctx.accounts.launch.config,
            LaunchError::AuthorAccountsMissing
        );
        let config = Account::<LaunchConfig>::try_from(config_info)?;
        require_keys_eq!(
            author_quote.key(),
            token_client::holding_address(&quote_mint, &config.creator),
            LaunchError::WrongHolding
        );
        Some((config.creator, author_quote.clone()))
    } else {
        None
    };
    let launch_info = ctx.accounts.launch.to_account_info();
    let payout = Payout {
        launch: &launch_info,
        launch_quote: &ctx.accounts.launch_quote.to_account_info(),
        quote_mint: &ctx.accounts.quote_mint.to_account_info(),
        token_program: &ctx.accounts.token_program.to_account_info(),
        token_event_authority: &ctx.accounts.token_event_authority.to_account_info(),
        mint: ctx.accounts.launch.mint,
        bump: ctx.accounts.launch.bump,
    };
    let to_author = author
        .as_ref()
        .map_or(0, |_| author_part(amount, share_bps));
    let to_creator = amount - to_author;
    if let Some((_, author_quote)) = &author {
        if to_author > 0 {
            payout.send(author_quote, to_author)?;
        }
    }
    payout.send(&ctx.accounts.creator_quote.to_account_info(), to_creator)?;
    let config = ctx.accounts.launch.config;
    let launch = &mut ctx.accounts.launch;
    launch.creator_fees_claimed = launch
        .creator_fees_claimed
        .checked_add(to_creator)
        .ok_or(LaunchError::MathOverflow)?;
    launch.author_fees_paid = launch
        .author_fees_paid
        .checked_add(to_author)
        .ok_or(LaunchError::MathOverflow)?;
    let (mint, creator, claimed_total, paid_total) = (
        launch.mint,
        launch.creator,
        launch.creator_fees_claimed,
        launch.author_fees_paid,
    );
    emit_cpi!(CreatorFeesClaimed {
        launch: launch_key,
        mint,
        creator,
        amount: to_creator,
        claimed_total,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    if let Some((author, _)) = author {
        emit_cpi!(AuthorFeesPaid {
            launch: launch_key,
            mint,
            config,
            author,
            amount: to_author,
            paid_total,
            slot: clock.slot,
            ts: clock.unix_timestamp,
        });
    }
    Ok(())
}

/// `claim_author_fees`.
pub fn process_claim_author_fees(ctx: Context<ClaimAuthorFees>) -> Result<()> {
    let clock = Clock::get()?;
    let launch_key = ctx.accounts.launch.key();
    let quote_mint = ctx.accounts.launch.quote_mint;
    let share_bps = ctx.accounts.launch.author_share_bps;
    require!(share_bps > 0, LaunchError::NoAuthorShare);
    require_keys_eq!(
        ctx.accounts.creator_quote.key(),
        token_client::holding_address(&quote_mint, &ctx.accounts.launch.creator),
        LaunchError::WrongHolding
    );
    require_keys_eq!(
        ctx.accounts.author_quote.key(),
        token_client::holding_address(&quote_mint, ctx.accounts.author.key),
        LaunchError::WrongHolding
    );
    let amount = claimable(
        &ctx.accounts.launch_quote,
        &ctx.accounts.token_event_authority,
    )?;
    let launch_info = ctx.accounts.launch.to_account_info();
    let payout = Payout {
        launch: &launch_info,
        launch_quote: &ctx.accounts.launch_quote.to_account_info(),
        quote_mint: &ctx.accounts.quote_mint.to_account_info(),
        token_program: &ctx.accounts.token_program.to_account_info(),
        token_event_authority: &ctx.accounts.token_event_authority.to_account_info(),
        mint: ctx.accounts.launch.mint,
        bump: ctx.accounts.launch.bump,
    };
    let to_author = author_part(amount, share_bps);
    let to_creator = amount - to_author;
    if to_author > 0 {
        payout.send(&ctx.accounts.author_quote.to_account_info(), to_author)?;
    }
    payout.send(&ctx.accounts.creator_quote.to_account_info(), to_creator)?;
    let launch = &mut ctx.accounts.launch;
    launch.creator_fees_claimed = launch
        .creator_fees_claimed
        .checked_add(to_creator)
        .ok_or(LaunchError::MathOverflow)?;
    launch.author_fees_paid = launch
        .author_fees_paid
        .checked_add(to_author)
        .ok_or(LaunchError::MathOverflow)?;
    let (mint, creator, claimed_total, paid_total, config) = (
        launch.mint,
        launch.creator,
        launch.creator_fees_claimed,
        launch.author_fees_paid,
        launch.config,
    );
    emit_cpi!(CreatorFeesClaimed {
        launch: launch_key,
        mint,
        creator,
        amount: to_creator,
        claimed_total,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    emit_cpi!(AuthorFeesPaid {
        launch: launch_key,
        mint,
        config,
        author: ctx.accounts.author.key(),
        amount: to_author,
        paid_total,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    Ok(())
}
