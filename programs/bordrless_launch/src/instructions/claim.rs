//! Creator fee claims. A launch made from a listed config by someone other than its author pays
//! the author `author_share_bps` of what each claim takes (rounded down; the creator gets the
//! rest), whoever claims: the creator with `claim_creator_fees`, the author with
//! `claim_author_fees`. A launch made from a partner config (`docs/partner-fee.md`) also pays the
//! config's partner its share of each claim, whoever claims, and anyone may claim it with
//! `distribute_fees`: every claim pays every party at once, each to its own fixed holding. Every
//! other launch pays its creator alone, as before.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::error::LaunchError;
use crate::events::{AuthorFeesPaid, CreatorFeesClaimed, PartnerFeesPaid};
use crate::instructions::launch::LaunchSeeds;
use crate::state::*;

/// Accounts of `claim_creator_fees`. A launch that pays its config's author also takes, as the
/// remaining accounts, the `LaunchConfig` (`launch.config`) and the author's holding of the quote
/// (writable); a launch that pays a partner then takes its `PartnerFee` (writable) and the
/// partner's holding of the quote (writable).
#[event_cpi]
#[derive(Accounts)]
pub struct ClaimCreatorFees<'info> {
    /// The creator.
    pub creator: Signer<'info>,
    #[account(mut, seeds = [LAUNCH_SEED, launch.mint.as_ref()], bump = launch.bump, has_one = creator @ LaunchError::NotCreator, constraint = !launch.is_token_quoted() @ LaunchError::TokenQuotedLaunch)]
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

/// Accounts of `claim_author_fees`. A launch that pays a partner also takes, as the remaining
/// accounts, its `PartnerFee` (writable) and the partner's holding of the quote (writable).
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

/// Accounts of `distribute_fees`: anyone. A launch that also pays its config's author takes, as the
/// remaining accounts, the `LaunchConfig` (`launch.config`) and the author's holding of the quote
/// (writable).
#[event_cpi]
#[derive(Accounts)]
pub struct DistributeFees<'info> {
    #[account(mut, seeds = [LAUNCH_SEED, launch.mint.as_ref()], bump = launch.bump, constraint = launch.partner_share_bps() > 0 @ LaunchError::NoPartnerShare)]
    pub launch: Account<'info, Launch>,
    /// The partner of the config the launch was made from.
    #[account(mut, seeds = [PARTNER_SEED, launch.config.as_ref()], bump = partner_fee.bump)]
    pub partner_fee: Account<'info, PartnerFee>,
    /// CHECK: address-checked.
    #[account(address = launch.quote_mint @ LaunchError::WrongHolding)]
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(mut, address = launch.quote_holding @ LaunchError::WrongHolding)]
    pub launch_quote: UncheckedAccount<'info>,
    /// CHECK: the launch creator's holding of the quote (checked in the handler).
    #[account(mut)]
    pub creator_quote: UncheckedAccount<'info>,
    /// CHECK: the partner's holding of the quote (checked in the handler).
    #[account(mut)]
    pub partner_quote: UncheckedAccount<'info>,
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

/// The checks every claim shares: the token program's event authority, the holdings, and
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

/// How a claim of `amount` splits: `(creator, author, partner)`. The author and the partner each
/// get their share of it, rounded down; the creator gets the rest. The shares are at most 10,000 in
/// all (`create_partner_config`), so the parts add up to `amount`.
pub fn fee_split(amount: u64, author_bps: u16, partner_bps: u16) -> Result<(u64, u64, u64)> {
    let to_author = author_part(amount, author_bps);
    let to_partner = author_part(amount, partner_bps);
    let to_creator = amount
        .checked_sub(to_author)
        .and_then(|rest| rest.checked_sub(to_partner))
        .ok_or(LaunchError::MathOverflow)?;
    Ok((to_creator, to_author, to_partner))
}

/// The author of a launch that pays one, from the remaining accounts `[config, author_quote]`:
/// the author's key and holding of the quote.
fn author_of<'info>(
    launch: &Launch,
    config_info: Option<&'info AccountInfo<'info>>,
    author_quote: Option<&'info AccountInfo<'info>>,
) -> Result<(Pubkey, AccountInfo<'info>)> {
    let (Some(config_info), Some(author_quote)) = (config_info, author_quote) else {
        return err!(LaunchError::AuthorAccountsMissing);
    };
    require_keys_eq!(
        config_info.key(),
        launch.config,
        LaunchError::AuthorAccountsMissing
    );
    let config = Account::<LaunchConfig>::try_from(config_info)?;
    require_keys_eq!(
        author_quote.key(),
        token_client::holding_address(&launch.quote_mint, &config.creator),
        LaunchError::WrongHolding
    );
    Ok((config.creator, author_quote.clone()))
}

/// The partner of a launch that pays one, from the remaining accounts `[partner_fee,
/// partner_quote]`: its `PartnerFee` (writable, at `["partner", launch.config]` with the bump the
/// launch recorded) and the partner's holding of the quote (writable).
fn partner_of<'info>(
    launch: &Launch,
    partner_fee: Option<&'info AccountInfo<'info>>,
    partner_quote: Option<&'info AccountInfo<'info>>,
) -> Result<(Account<'info, PartnerFee>, AccountInfo<'info>)> {
    let (Some(partner_fee), Some(partner_quote)) = (partner_fee, partner_quote) else {
        return err!(LaunchError::PartnerAccountsMissing);
    };
    let expected = Pubkey::create_program_address(
        &[
            PARTNER_SEED,
            launch.config.as_ref(),
            &[launch.partner_bump()],
        ],
        &crate::ID,
    )
    .map_err(|_| error!(LaunchError::PartnerAccountsMissing))?;
    require_keys_eq!(
        partner_fee.key(),
        expected,
        LaunchError::PartnerAccountsMissing
    );
    require!(
        partner_fee.is_writable && partner_quote.is_writable,
        LaunchError::PartnerAccountsMissing
    );
    let pf = Account::<PartnerFee>::try_from(partner_fee)?;
    require_keys_eq!(
        pf.config,
        launch.config,
        LaunchError::PartnerAccountsMissing
    );
    check_partner_quote(launch, &pf, partner_quote)?;
    Ok((pf, partner_quote.clone()))
}

/// The partner's holding of the quote.
fn check_partner_quote(
    launch: &Launch,
    pf: &PartnerFee,
    partner_quote: &AccountInfo,
) -> Result<()> {
    require_keys_eq!(
        partner_quote.key(),
        token_client::holding_address(&launch.quote_mint, &pf.partner),
        LaunchError::WrongHolding
    );
    Ok(())
}

/// What one claim paid.
struct Paid {
    creator: u64,
    author: u64,
    partner: u64,
}

/// Pays a claim of `amount`: the partner's part, the author's, then the creator's, each only when
/// above zero, and counts them (the partner's in its `PartnerFee`, every launch of its config).
fn settle<'info>(
    payout: &Payout<'_, 'info>,
    launch: &mut Launch,
    amount: u64,
    creator_quote: &AccountInfo<'info>,
    author_quote: Option<&AccountInfo<'info>>,
    partner: Option<(&mut PartnerFee, &AccountInfo<'info>)>,
) -> Result<Paid> {
    let author_bps = if author_quote.is_some() {
        launch.author_share_bps
    } else {
        0
    };
    // A partner whose holding is the launch's own fee holding (a config naming the launch of a mint
    // its author made) is paid nothing: the token program refuses a transfer to itself, which would
    // stop every claim of the launch for good.
    let partner_bps = match &partner {
        Some((_, partner_quote)) if partner_quote.key != payout.launch_quote.key => {
            launch.partner_share_bps()
        }
        _ => 0,
    };
    let (to_creator, to_author, to_partner) = fee_split(amount, author_bps, partner_bps)?;
    if let Some((pf, partner_quote)) = partner {
        if to_partner > 0 {
            payout.send(partner_quote, to_partner)?;
        }
        pf.paid_total = pf
            .paid_total
            .checked_add(to_partner)
            .ok_or(LaunchError::MathOverflow)?;
    }
    if let Some(author_quote) = author_quote {
        if to_author > 0 {
            payout.send(author_quote, to_author)?;
        }
    }
    if to_creator > 0 {
        payout.send(creator_quote, to_creator)?;
    }
    launch.creator_fees_claimed = launch
        .creator_fees_claimed
        .checked_add(to_creator)
        .ok_or(LaunchError::MathOverflow)?;
    launch.author_fees_paid = launch
        .author_fees_paid
        .checked_add(to_author)
        .ok_or(LaunchError::MathOverflow)?;
    Ok(Paid {
        creator: to_creator,
        author: to_author,
        partner: to_partner,
    })
}

/// The creator's holding of the quote.
fn check_creator_quote(launch: &Launch, creator_quote: &AccountInfo) -> Result<()> {
    require_keys_eq!(
        creator_quote.key(),
        token_client::holding_address(&launch.quote_mint, &launch.creator),
        LaunchError::WrongHolding
    );
    Ok(())
}

/// The events of a claim: the creator's part always, the author's and the partner's when the
/// launch pays them.
macro_rules! emit_claim {
    ($ctx:ident, $launch_key:expr, $paid:expr, $author:expr, $partner:expr, $clock:expr) => {{
        // `emit_cpi!` reads `ctx` by name: bound here, so the name it writes resolves.
        let ctx = &$ctx;
        let launch = &ctx.accounts.launch;
        let (mint, creator, config) = (launch.mint, launch.creator, launch.config);
        let (claimed_total, author_total) = (launch.creator_fees_claimed, launch.author_fees_paid);
        emit_cpi!(CreatorFeesClaimed {
            launch: $launch_key,
            mint,
            creator,
            amount: $paid.creator,
            claimed_total,
            slot: $clock.slot,
            ts: $clock.unix_timestamp,
        });
        if let Some(author) = $author {
            emit_cpi!(AuthorFeesPaid {
                launch: $launch_key,
                mint,
                config,
                author,
                amount: $paid.author,
                paid_total: author_total,
                slot: $clock.slot,
                ts: $clock.unix_timestamp,
            });
        }
        if let Some((partner, paid_total)) = $partner {
            emit_cpi!(PartnerFeesPaid {
                launch: $launch_key,
                mint,
                config,
                partner,
                amount: $paid.partner,
                paid_total,
                slot: $clock.slot,
                ts: $clock.unix_timestamp,
            });
        }
    }};
}

/// `claim_creator_fees`.
pub fn process_claim_creator_fees<'info>(
    ctx: Context<'info, ClaimCreatorFees<'info>>,
) -> Result<()> {
    let clock = Clock::get()?;
    let launch_key = ctx.accounts.launch.key();
    require_keys_eq!(
        ctx.accounts.creator_quote.key(),
        token_client::holding_address(&ctx.accounts.launch.quote_mint, ctx.accounts.creator.key),
        LaunchError::WrongHolding
    );
    let amount = claimable(
        &ctx.accounts.launch_quote,
        &ctx.accounts.token_event_authority,
    )?;
    // With an author share: the listed config (to read its author) and the author's holding; with
    // a partner: its account and its holding, after them.
    let mut rest = ctx.remaining_accounts.iter();
    let author = if ctx.accounts.launch.author_share_bps > 0 {
        Some(author_of(&ctx.accounts.launch, rest.next(), rest.next())?)
    } else {
        None
    };
    let mut partner = if ctx.accounts.launch.partner_share_bps() > 0 {
        Some(partner_of(&ctx.accounts.launch, rest.next(), rest.next())?)
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
    let paid = settle(
        &payout,
        &mut ctx.accounts.launch,
        amount,
        &ctx.accounts.creator_quote.to_account_info(),
        author.as_ref().map(|(_, q)| q),
        partner.as_mut().map(|(pf, q)| (&mut **pf, &*q)),
    )?;
    let partner_ev = match partner {
        Some((pf, _)) => {
            pf.exit(&crate::ID)?;
            Some((pf.partner, pf.paid_total))
        }
        None => None,
    };
    emit_claim!(
        ctx,
        launch_key,
        paid,
        author.map(|(a, _)| a),
        partner_ev,
        clock
    );
    Ok(())
}

/// `claim_author_fees`.
pub fn process_claim_author_fees<'info>(ctx: Context<'info, ClaimAuthorFees<'info>>) -> Result<()> {
    let clock = Clock::get()?;
    let launch_key = ctx.accounts.launch.key();
    require!(
        ctx.accounts.launch.author_share_bps > 0,
        LaunchError::NoAuthorShare
    );
    check_creator_quote(&ctx.accounts.launch, &ctx.accounts.creator_quote)?;
    require_keys_eq!(
        ctx.accounts.author_quote.key(),
        token_client::holding_address(&ctx.accounts.launch.quote_mint, ctx.accounts.author.key),
        LaunchError::WrongHolding
    );
    let amount = claimable(
        &ctx.accounts.launch_quote,
        &ctx.accounts.token_event_authority,
    )?;
    // With a partner: its account and its holding.
    let mut rest = ctx.remaining_accounts.iter();
    let mut partner = if ctx.accounts.launch.partner_share_bps() > 0 {
        Some(partner_of(&ctx.accounts.launch, rest.next(), rest.next())?)
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
    let author_quote = ctx.accounts.author_quote.to_account_info();
    let paid = settle(
        &payout,
        &mut ctx.accounts.launch,
        amount,
        &ctx.accounts.creator_quote.to_account_info(),
        Some(&author_quote),
        partner.as_mut().map(|(pf, q)| (&mut **pf, &*q)),
    )?;
    let partner_ev = match partner {
        Some((pf, _)) => {
            pf.exit(&crate::ID)?;
            Some((pf.partner, pf.paid_total))
        }
        None => None,
    };
    let author = ctx.accounts.author.key();
    emit_claim!(ctx, launch_key, paid, Some(author), partner_ev, clock);
    Ok(())
}

/// `distribute_fees`: anyone pays out a partner launch's creator fees, every party at once, each to
/// its own fixed holding, as the creator's claim would.
pub fn process_distribute_fees<'info>(ctx: Context<'info, DistributeFees<'info>>) -> Result<()> {
    let clock = Clock::get()?;
    let launch_key = ctx.accounts.launch.key();
    check_creator_quote(&ctx.accounts.launch, &ctx.accounts.creator_quote)?;
    check_partner_quote(
        &ctx.accounts.launch,
        &ctx.accounts.partner_fee,
        &ctx.accounts.partner_quote,
    )?;
    let amount = claimable(
        &ctx.accounts.launch_quote,
        &ctx.accounts.token_event_authority,
    )?;
    let mut rest = ctx.remaining_accounts.iter();
    let author = if ctx.accounts.launch.author_share_bps > 0 {
        Some(author_of(&ctx.accounts.launch, rest.next(), rest.next())?)
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
    let partner_quote = ctx.accounts.partner_quote.to_account_info();
    let paid = settle(
        &payout,
        &mut ctx.accounts.launch,
        amount,
        &ctx.accounts.creator_quote.to_account_info(),
        author.as_ref().map(|(_, q)| q),
        Some((&mut *ctx.accounts.partner_fee, &partner_quote)),
    )?;
    let partner_ev = Some((
        ctx.accounts.partner_fee.partner,
        ctx.accounts.partner_fee.paid_total,
    ));
    emit_claim!(
        ctx,
        launch_key,
        paid,
        author.map(|(a, _)| a),
        partner_ev,
        clock
    );
    Ok(())
}

// ---- Native launches: the same claims, paid in SOL from the launch account. ----

/// What a native launch holds for its claims: its lamports above the rent-exempt minimum (its
/// creator fees and any SOL cut, paid there by the pool hook's deltas).
fn claimable_sol(launch: &AccountInfo) -> Result<u64> {
    let rent = Rent::get()?.minimum_balance(launch.data_len());
    let amount = launch.lamports().saturating_sub(rent);
    require!(amount > 0, LaunchError::NothingToClaim);
    Ok(amount)
}

/// Pays `amount` lamports from the launch (this program owns it) to `to`, which must end at or
/// above the rent-exempt minimum of its size (`RecipientBelowRent`).
fn pay_sol(launch: &AccountInfo, to: &AccountInfo, amount: u64) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    let after = to
        .lamports()
        .checked_add(amount)
        .ok_or(LaunchError::MathOverflow)?;
    require!(
        after >= Rent::get()?.minimum_balance(to.data_len()),
        LaunchError::RecipientBelowRent
    );
    require!(to.is_writable, LaunchError::WrongHolding);
    let from = launch.lamports();
    **launch.try_borrow_mut_lamports()? =
        from.checked_sub(amount).ok_or(LaunchError::MathOverflow)?;
    **to.try_borrow_mut_lamports()? = after;
    Ok(())
}

/// The author of a native launch that pays one, from the remaining accounts `[config,
/// author_wallet]`: the author's key and wallet.
fn author_wallet_of<'info>(
    launch: &Launch,
    config_info: Option<&'info AccountInfo<'info>>,
    author_wallet: Option<&'info AccountInfo<'info>>,
) -> Result<(Pubkey, AccountInfo<'info>)> {
    let (Some(config_info), Some(author_wallet)) = (config_info, author_wallet) else {
        return err!(LaunchError::AuthorAccountsMissing);
    };
    require_keys_eq!(
        config_info.key(),
        launch.config,
        LaunchError::AuthorAccountsMissing
    );
    let config = Account::<LaunchConfig>::try_from(config_info)?;
    require_keys_eq!(
        author_wallet.key(),
        config.creator,
        LaunchError::WrongHolding
    );
    Ok((config.creator, author_wallet.clone()))
}

/// The partner of a native launch that pays one, from the remaining accounts `[partner_fee,
/// partner_wallet]`.
fn partner_wallet_of<'info>(
    launch: &Launch,
    partner_fee: Option<&'info AccountInfo<'info>>,
    partner_wallet: Option<&'info AccountInfo<'info>>,
) -> Result<(Account<'info, PartnerFee>, AccountInfo<'info>)> {
    let (Some(partner_fee), Some(partner_wallet)) = (partner_fee, partner_wallet) else {
        return err!(LaunchError::PartnerAccountsMissing);
    };
    let expected = Pubkey::create_program_address(
        &[
            PARTNER_SEED,
            launch.config.as_ref(),
            &[launch.partner_bump()],
        ],
        &crate::ID,
    )
    .map_err(|_| error!(LaunchError::PartnerAccountsMissing))?;
    require_keys_eq!(
        partner_fee.key(),
        expected,
        LaunchError::PartnerAccountsMissing
    );
    require!(
        partner_fee.is_writable && partner_wallet.is_writable,
        LaunchError::PartnerAccountsMissing
    );
    let pf = Account::<PartnerFee>::try_from(partner_fee)?;
    require_keys_eq!(
        pf.config,
        launch.config,
        LaunchError::PartnerAccountsMissing
    );
    require_keys_eq!(partner_wallet.key(), pf.partner, LaunchError::WrongHolding);
    Ok((pf, partner_wallet.clone()))
}

/// [`settle`] in SOL: the partner's part, the author's, then the creator's, each from the launch's
/// lamports to the party's wallet. A partner that is the launch itself is paid nothing (its part
/// would only stay where it is).
fn settle_sol<'info>(
    launch_info: &AccountInfo<'info>,
    launch: &mut Launch,
    amount: u64,
    creator_wallet: &AccountInfo<'info>,
    author_wallet: Option<&AccountInfo<'info>>,
    partner: Option<(&mut PartnerFee, &AccountInfo<'info>)>,
) -> Result<Paid> {
    let author_bps = if author_wallet.is_some() {
        launch.author_share_bps
    } else {
        0
    };
    let partner_bps = match &partner {
        Some((_, wallet)) if wallet.key != launch_info.key => launch.partner_share_bps(),
        _ => 0,
    };
    let (to_creator, to_author, to_partner) = fee_split(amount, author_bps, partner_bps)?;
    if let Some((pf, wallet)) = partner {
        pay_sol(launch_info, wallet, to_partner)?;
        pf.paid_total = pf
            .paid_total
            .checked_add(to_partner)
            .ok_or(LaunchError::MathOverflow)?;
    }
    if let Some(wallet) = author_wallet {
        pay_sol(launch_info, wallet, to_author)?;
    }
    pay_sol(launch_info, creator_wallet, to_creator)?;
    launch.creator_fees_claimed = launch
        .creator_fees_claimed
        .checked_add(to_creator)
        .ok_or(LaunchError::MathOverflow)?;
    launch.author_fees_paid = launch
        .author_fees_paid
        .checked_add(to_author)
        .ok_or(LaunchError::MathOverflow)?;
    Ok(Paid {
        creator: to_creator,
        author: to_author,
        partner: to_partner,
    })
}

/// Accounts of `claim_creator_fees_sol`: the creator (its wallet receives the SOL) and the native
/// launch. A launch that pays its config's author also takes, as the remaining accounts, the
/// `LaunchConfig` and the author's wallet (writable); a launch that pays a partner then takes its
/// `PartnerFee` (writable) and the partner's wallet (writable).
#[event_cpi]
#[derive(Accounts)]
pub struct ClaimCreatorFeesSol<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(
        mut,
        seeds = [LAUNCH_SEED, launch.mint.as_ref()],
        bump = launch.bump,
        has_one = creator @ LaunchError::NotCreator,
        constraint = launch.is_native() @ LaunchError::NotNativeLaunch
    )]
    pub launch: Account<'info, Launch>,
}

/// `claim_creator_fees_sol`.
pub fn process_claim_creator_fees_sol<'info>(
    ctx: Context<'info, ClaimCreatorFeesSol<'info>>,
) -> Result<()> {
    let clock = Clock::get()?;
    let launch_key = ctx.accounts.launch.key();
    let launch_info = ctx.accounts.launch.to_account_info();
    let amount = claimable_sol(&launch_info)?;
    let mut rest = ctx.remaining_accounts.iter();
    let author = if ctx.accounts.launch.author_share_bps > 0 {
        Some(author_wallet_of(
            &ctx.accounts.launch,
            rest.next(),
            rest.next(),
        )?)
    } else {
        None
    };
    let mut partner = if ctx.accounts.launch.partner_share_bps() > 0 {
        Some(partner_wallet_of(
            &ctx.accounts.launch,
            rest.next(),
            rest.next(),
        )?)
    } else {
        None
    };
    let creator = ctx.accounts.creator.to_account_info();
    let paid = settle_sol(
        &launch_info,
        &mut ctx.accounts.launch,
        amount,
        &creator,
        author.as_ref().map(|(_, w)| w),
        partner.as_mut().map(|(pf, w)| (&mut **pf, &*w)),
    )?;
    let partner_ev = match partner {
        Some((pf, _)) => {
            pf.exit(&crate::ID)?;
            Some((pf.partner, pf.paid_total))
        }
        None => None,
    };
    emit_claim!(
        ctx,
        launch_key,
        paid,
        author.map(|(a, _)| a),
        partner_ev,
        clock
    );
    Ok(())
}

/// Accounts of `claim_author_fees_sol`: the author (its wallet receives its share), the native
/// launch, the listed config and the creator's wallet. A launch that pays a partner also takes,
/// as the remaining accounts, its `PartnerFee` (writable) and the partner's wallet (writable).
#[event_cpi]
#[derive(Accounts)]
pub struct ClaimAuthorFeesSol<'info> {
    #[account(mut)]
    pub author: Signer<'info>,
    #[account(
        mut,
        seeds = [LAUNCH_SEED, launch.mint.as_ref()],
        bump = launch.bump,
        constraint = launch.is_native() @ LaunchError::NotNativeLaunch
    )]
    pub launch: Account<'info, Launch>,
    #[account(address = launch.config @ LaunchError::NoAuthorShare, constraint = launch_config.creator == author.key() @ LaunchError::NotAuthor)]
    pub launch_config: Account<'info, LaunchConfig>,
    /// CHECK: address-checked: the launch's creator, paid its part.
    #[account(mut, address = launch.creator @ LaunchError::WrongHolding)]
    pub creator_wallet: UncheckedAccount<'info>,
}

/// `claim_author_fees_sol`.
pub fn process_claim_author_fees_sol<'info>(
    ctx: Context<'info, ClaimAuthorFeesSol<'info>>,
) -> Result<()> {
    let clock = Clock::get()?;
    let launch_key = ctx.accounts.launch.key();
    require!(
        ctx.accounts.launch.author_share_bps > 0,
        LaunchError::NoAuthorShare
    );
    let launch_info = ctx.accounts.launch.to_account_info();
    let amount = claimable_sol(&launch_info)?;
    let mut rest = ctx.remaining_accounts.iter();
    let mut partner = if ctx.accounts.launch.partner_share_bps() > 0 {
        Some(partner_wallet_of(
            &ctx.accounts.launch,
            rest.next(),
            rest.next(),
        )?)
    } else {
        None
    };
    let author = ctx.accounts.author.to_account_info();
    let creator = ctx.accounts.creator_wallet.to_account_info();
    let paid = settle_sol(
        &launch_info,
        &mut ctx.accounts.launch,
        amount,
        &creator,
        Some(&author),
        partner.as_mut().map(|(pf, w)| (&mut **pf, &*w)),
    )?;
    let partner_ev = match partner {
        Some((pf, _)) => {
            pf.exit(&crate::ID)?;
            Some((pf.partner, pf.paid_total))
        }
        None => None,
    };
    let author_key = ctx.accounts.author.key();
    emit_claim!(ctx, launch_key, paid, Some(author_key), partner_ev, clock);
    Ok(())
}

/// Accounts of `distribute_fees_sol`: anyone pays out a native partner launch's creator fees to
/// every party's wallet. A launch that also pays its config's author takes, as the remaining
/// accounts, the `LaunchConfig` and the author's wallet (writable).
#[event_cpi]
#[derive(Accounts)]
pub struct DistributeFeesSol<'info> {
    #[account(
        mut,
        seeds = [LAUNCH_SEED, launch.mint.as_ref()],
        bump = launch.bump,
        constraint = launch.partner_share_bps() > 0 @ LaunchError::NoPartnerShare,
        constraint = launch.is_native() @ LaunchError::NotNativeLaunch
    )]
    pub launch: Account<'info, Launch>,
    #[account(mut, seeds = [PARTNER_SEED, launch.config.as_ref()], bump = partner_fee.bump)]
    pub partner_fee: Account<'info, PartnerFee>,
    /// CHECK: address-checked: the launch's creator.
    #[account(mut, address = launch.creator @ LaunchError::WrongHolding)]
    pub creator_wallet: UncheckedAccount<'info>,
    /// CHECK: address-checked: the partner.
    #[account(mut, address = partner_fee.partner @ LaunchError::WrongHolding)]
    pub partner_wallet: UncheckedAccount<'info>,
}

/// `distribute_fees_sol`.
pub fn process_distribute_fees_sol<'info>(
    ctx: Context<'info, DistributeFeesSol<'info>>,
) -> Result<()> {
    let clock = Clock::get()?;
    let launch_key = ctx.accounts.launch.key();
    let launch_info = ctx.accounts.launch.to_account_info();
    let amount = claimable_sol(&launch_info)?;
    let mut rest = ctx.remaining_accounts.iter();
    let author = if ctx.accounts.launch.author_share_bps > 0 {
        Some(author_wallet_of(
            &ctx.accounts.launch,
            rest.next(),
            rest.next(),
        )?)
    } else {
        None
    };
    let creator = ctx.accounts.creator_wallet.to_account_info();
    let partner_wallet = ctx.accounts.partner_wallet.to_account_info();
    let paid = settle_sol(
        &launch_info,
        &mut ctx.accounts.launch,
        amount,
        &creator,
        author.as_ref().map(|(_, w)| w),
        Some((&mut *ctx.accounts.partner_fee, &partner_wallet)),
    )?;
    let partner_ev = Some((
        ctx.accounts.partner_fee.partner,
        ctx.accounts.partner_fee.paid_total,
    ));
    emit_claim!(
        ctx,
        launch_key,
        paid,
        author.map(|(a, _)| a),
        partner_ev,
        clock
    );
    Ok(())
}
