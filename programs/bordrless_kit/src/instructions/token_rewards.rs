//! Holder rewards on coins quoted in tokens (`docs/token-quotes.md` §7.5.1). The launchpad's leg
//! hook takes the holder fee on the quote side of every leg trade into the kit config's associated
//! token account of that leg's token (its vault of the leg). What the kit does with it depends on
//! its reward kind (`KitConfig::reward_kind`, fixed by `init_legs`):
//!
//! - **Kind 1, a one-leg coin's token.** The kit's accumulator as on a SOL coin, the vault being the
//!   leg token's account (`reward_vault`): the callbacks sync and settle, `claim_token` pays the
//!   holder's token account (made when missing, the holder paying), `claim_token_for` pushes the
//!   same claim to an account that exists, `share_token` streams a share over the hour.
//! - **Kind 2, every leg's token in rounds.** `start_round` (anyone, once a round) takes the eligible
//!   supply and every leg vault's balance; `pay_round` (anyone, for any holding not excluded) pays a
//!   holding its share of each leg's amount on the least it held since the round before began
//!   (`RoundData`), once a round, into its associated token account of each leg's token (made
//!   with `create_missing`, the sender paying; a leg whose account is missing or can't receive is
//!   skipped for that holding this round). What a round leaves rolls into the next one's amounts.
//!   `share_token` adds to a leg's vault (the next round counts it).
//! - **Kind 3, SOL.** A native kit from birth; `swap_rewards_to_sol` sells a leg vault's tokens
//!   through the launchpad's `swap_to_sol` (the config signing as the tokens' owner) and streams
//!   what it brought over the hour as `share_sol` would (or leaves it to the next sync's division
//!   when it is too little or nobody is eligible); holders `claim_sol`, the keeper `claim_sol_for`.
//!
//! Every claim and payment excludes every leg's pool (the coin's `Legs`), the launch and a companion
//! creator, as the callbacks do.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_lang::system_program;
use bordrless_hook::HOOK_AUTHORITY_SEED;
use bordrless_token::client as token_client;
use bordrless_token::state::Holding;

use crate::constants::*;
use crate::error::KitError;
use crate::events::*;
use crate::instructions::callbacks::token_vault_amount;
use crate::legs::LegPools;
use crate::math::settle;
use crate::spl;
use crate::state::*;

/// The config's signer seeds.
struct ConfigSeeds {
    mint: Pubkey,
    bump: [u8; 1],
}

impl ConfigSeeds {
    fn of(config: &KitConfig) -> Self {
        Self {
            mint: config.mint,
            bump: [config.bump],
        }
    }

    fn seeds(&self) -> [&[u8]; 3] {
        [KIT_SEED, self.mint.as_ref(), &self.bump]
    }
}

/// Writes `data` as `holding`'s hook data through the token program, the kit's hook authority
/// signing.
fn write_hook_data<'info>(
    mint: &AccountInfo<'info>,
    holding: &AccountInfo<'info>,
    kit_hook_authority: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
    token_event_authority: &AccountInfo<'info>,
    data: [u8; 64],
) -> Result<()> {
    let ix = token_client::write_hook_data(HOOK_AUTHORITY, *mint.key, *holding.key, data);
    invoke_signed(
        &ix,
        &[
            kit_hook_authority.clone(),
            mint.clone(),
            holding.clone(),
            token_event_authority.clone(),
            token_program.clone(),
        ],
        &[&[HOOK_AUTHORITY_SEED, &[HOOK_AUTHORITY_BUMP]]],
    )?;
    Ok(())
}

// ---- Kind 1: claim_token, claim_token_for ------------------------------------------------------------

/// Accounts of `claim_token`: the holder claims its rewards in the leg's token.
#[event_cpi]
#[derive(Accounts)]
pub struct ClaimToken<'info> {
    /// The holder; pays its token account's rent when it is made.
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut, constraint = kit_config.pays_token() @ KitError::WrongRewardKind)]
    pub kit_config: Account<'info, KitConfig>,
    /// CHECK: the config's mint (address-checked).
    #[account(address = kit_config.mint @ KitError::WrongMint)]
    pub mint: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = holding.mint == kit_config.mint @ KitError::WrongHolding,
        constraint = holding.owner == owner.key() @ KitError::WrongHolding
    )]
    pub holding: Account<'info, Holding>,
    /// CHECK: the config's reward mint, the leg's token (address-checked).
    #[account(address = kit_config.reward_mint @ KitError::WrongRewardMint)]
    pub reward_mint: UncheckedAccount<'info>,
    /// CHECK: the config's vault (address-checked; read as a token account in the handler).
    #[account(mut, address = kit_config.reward_vault @ KitError::WrongRewardVault)]
    pub reward_vault: UncheckedAccount<'info>,
    /// CHECK: a token account of the reward mint the owner owns; its associated account is made
    /// here when it is that and missing.
    #[account(mut)]
    pub destination: UncheckedAccount<'info>,
    /// CHECK: the coin's `Legs` (read in place: owner, discriminator, size, mint).
    pub legs: UncheckedAccount<'info>,
    /// CHECK: this program's `["hook-authority"]` PDA, which signs `write_hook_data`.
    #[account(address = HOOK_AUTHORITY @ KitError::WrongProgram)]
    pub kit_hook_authority: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = TOKEN_ID @ KitError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority.
    #[account(address = TOKEN_EVENT_AUTHORITY @ KitError::WrongProgram)]
    pub token_event_authority: UncheckedAccount<'info>,
    /// CHECK: the reward mint's token program (checked in the handler).
    pub reward_token_program: UncheckedAccount<'info>,
    /// CHECK: the associated token account program.
    #[account(address = spl::ATA_PROGRAM_ID @ KitError::WrongProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Accounts of `claim_token_for`: `claim_token` pushed by anyone (`payer` signs and pays the fee);
/// the tokens go only to a token account the holding's owner owns, which must exist.
#[event_cpi]
#[derive(Accounts)]
pub struct ClaimTokenFor<'info> {
    pub payer: Signer<'info>,
    /// CHECK: the holding's owner (constrained by `holding`).
    pub owner: UncheckedAccount<'info>,
    #[account(mut, constraint = kit_config.pays_token() @ KitError::WrongRewardKind)]
    pub kit_config: Account<'info, KitConfig>,
    /// CHECK: the config's mint (address-checked).
    #[account(address = kit_config.mint @ KitError::WrongMint)]
    pub mint: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = holding.mint == kit_config.mint @ KitError::WrongHolding,
        constraint = holding.owner == owner.key() @ KitError::WrongHolding
    )]
    pub holding: Account<'info, Holding>,
    /// CHECK: the config's reward mint (address-checked).
    #[account(address = kit_config.reward_mint @ KitError::WrongRewardMint)]
    pub reward_mint: UncheckedAccount<'info>,
    /// CHECK: the config's vault (address-checked).
    #[account(mut, address = kit_config.reward_vault @ KitError::WrongRewardVault)]
    pub reward_vault: UncheckedAccount<'info>,
    /// CHECK: a token account of the reward mint the owner owns (checked in the handler).
    #[account(mut)]
    pub destination: UncheckedAccount<'info>,
    /// CHECK: the coin's `Legs`.
    pub legs: UncheckedAccount<'info>,
    /// CHECK: this program's `["hook-authority"]` PDA.
    #[account(address = HOOK_AUTHORITY @ KitError::WrongProgram)]
    pub kit_hook_authority: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = TOKEN_ID @ KitError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority.
    #[account(address = TOKEN_EVENT_AUTHORITY @ KitError::WrongProgram)]
    pub token_event_authority: UncheckedAccount<'info>,
    /// CHECK: the reward mint's token program (checked in the handler).
    pub reward_token_program: UncheckedAccount<'info>,
}

/// The accounts a token claim reads and moves.
struct TokenClaim<'a, 'info> {
    kit_config: &'a mut Account<'info, KitConfig>,
    mint: AccountInfo<'info>,
    holding: &'a Account<'info, Holding>,
    owner: Pubkey,
    reward_mint: AccountInfo<'info>,
    reward_vault: AccountInfo<'info>,
    destination: AccountInfo<'info>,
    legs: AccountInfo<'info>,
    kit_hook_authority: AccountInfo<'info>,
    token_program: AccountInfo<'info>,
    token_event_authority: AccountInfo<'info>,
    reward_token_program: AccountInfo<'info>,
}

/// A kind-1 claim: the owner not excluded (every leg's pool included); sync at the vault's balance
/// (a shortfall taken as the new base, `KitConfig::sync`); settle at the holding's balance; pay
/// `min(owed, vault)` (`NothingToClaim` at 0) to `destination` (`NoTokenAccount` unless it can
/// receive the token for the owner) by `TransferChecked`, the config signing; write the holder's
/// bytes 0..24, the rest as read. Answers the event.
fn claim_token(c: TokenClaim) -> Result<RewardsClaimed> {
    let now = Clock::get()?.unix_timestamp;
    let config_key = c.kit_config.key();
    let (program, decimals) = c.kit_config.reward_token();
    require_keys_eq!(
        *c.reward_token_program.key,
        spl::program_id(program),
        KitError::WrongProgram
    );
    let legs = LegPools::read(&c.legs, &c.kit_config.mint)?;
    require!(
        !c.kit_config.is_excluded_with(&c.owner, Some(&legs)),
        KitError::NotAHolder
    );
    require!(
        spl::receivable(
            &c.destination,
            c.reward_token_program.key,
            c.reward_mint.key,
            &c.owner
        ),
        KitError::NoTokenAccount
    );
    let vault_amount = token_vault_amount(c.kit_config, &config_key, &c.reward_vault)?;
    let balance = c.holding.amount;
    let read = c.holding.hook_data;
    let config = &mut **c.kit_config;
    config.sync(vault_amount, now)?;
    let mut data = HolderData::read(&read);
    settle(&mut data, balance, balance, config.acc_per_share)?;
    let pay = data.owed.min(vault_amount);
    require!(pay > 0, KitError::NothingToClaim);
    data.owed -= pay;
    config.total_claimed = config
        .total_claimed
        .checked_add(pay)
        .ok_or(KitError::MathOverflow)?;
    let total_claimed = config.total_claimed;
    let mint = config.mint;
    let seeds = ConfigSeeds::of(config);
    spl::transfer_checked(
        &c.reward_token_program,
        &c.reward_vault,
        &c.reward_mint,
        &c.destination,
        &c.kit_config.to_account_info(),
        pay,
        decimals,
        &[&seeds.seeds()],
    )?;
    write_hook_data(
        &c.mint,
        &c.holding.to_account_info(),
        &c.kit_hook_authority,
        &c.token_program,
        &c.token_event_authority,
        data.with_rewards_of(&read),
    )?;
    Ok(RewardsClaimed {
        mint,
        owner: c.owner,
        amount: pay,
        owed_left: data.owed,
        total_claimed,
    })
}

/// `claim_token`: the holder's own claim of a kind-1 kit; its associated token account of the
/// reward mint is made first when it is the destination and missing (the holder paying).
pub fn process_claim_token(ctx: Context<ClaimToken>) -> Result<()> {
    let a = &mut *ctx.accounts;
    let owner = a.owner.key();
    let ata = spl::associated_address(&owner, a.reward_mint.key, a.reward_token_program.key);
    if a.destination.key() == ata {
        require!(
            spl::program_index(a.reward_token_program.key).is_some(),
            KitError::WrongProgram
        );
        spl::create_ata_idempotent(
            &a.owner.to_account_info(),
            &a.destination.to_account_info(),
            &a.owner.to_account_info(),
            &a.reward_mint.to_account_info(),
            &a.system_program.to_account_info(),
            &a.reward_token_program.to_account_info(),
            &a.associated_token_program.to_account_info(),
        )?;
    }
    let event = claim_token(TokenClaim {
        kit_config: &mut a.kit_config,
        mint: a.mint.to_account_info(),
        holding: &a.holding,
        owner,
        reward_mint: a.reward_mint.to_account_info(),
        reward_vault: a.reward_vault.to_account_info(),
        destination: a.destination.to_account_info(),
        legs: a.legs.to_account_info(),
        kit_hook_authority: a.kit_hook_authority.to_account_info(),
        token_program: a.token_program.to_account_info(),
        token_event_authority: a.token_event_authority.to_account_info(),
        reward_token_program: a.reward_token_program.to_account_info(),
    })?;
    emit_cpi!(event);
    Ok(())
}

/// `claim_token_for`: `claim_token` pushed by anyone, paid exactly as the holder's own claim into a
/// token account of the owner's that exists.
pub fn process_claim_token_for(ctx: Context<ClaimTokenFor>) -> Result<()> {
    let a = &mut *ctx.accounts;
    let owner = a.owner.key();
    let event = claim_token(TokenClaim {
        kit_config: &mut a.kit_config,
        mint: a.mint.to_account_info(),
        holding: &a.holding,
        owner,
        reward_mint: a.reward_mint.to_account_info(),
        reward_vault: a.reward_vault.to_account_info(),
        destination: a.destination.to_account_info(),
        legs: a.legs.to_account_info(),
        kit_hook_authority: a.kit_hook_authority.to_account_info(),
        token_program: a.token_program.to_account_info(),
        token_event_authority: a.token_event_authority.to_account_info(),
        reward_token_program: a.reward_token_program.to_account_info(),
    })?;
    emit_cpi!(event);
    Ok(())
}

// ---- Kinds 1 and 2: share_token --------------------------------------------------------------------

/// Accounts of `share_token(index, amount)`.
#[event_cpi]
#[derive(Accounts)]
pub struct ShareToken<'info> {
    /// Shares from `source`, as its owner or delegate (the token program checks).
    pub sharer: Signer<'info>,
    #[account(mut, constraint = kit_config.rewards_on() @ KitError::RewardsOff)]
    pub kit_config: Account<'info, KitConfig>,
    /// CHECK: the coin's `Legs` (read in place).
    pub legs: UncheckedAccount<'info>,
    /// CHECK: leg `index`'s quote mint (checked in the handler).
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: a token account of it the sharer may spend (the token program checks).
    #[account(mut)]
    pub source: UncheckedAccount<'info>,
    /// CHECK: the kit's vault of the leg (checked in the handler).
    #[account(mut)]
    pub vault: UncheckedAccount<'info>,
    /// CHECK: the leg token's program (checked in the handler).
    pub quote_token_program: UncheckedAccount<'info>,
}

/// `share_token(index, amount)`: `amount` of leg `index`'s token shared with the holders. Kind 1
/// (leg 0, the reward mint): sync, the transfer into the vault, what arrived streamed over the hour
/// as `share` streams SOL (`NoEligibleHolders` while holders hold less than the threshold). Kind 2:
/// into the leg's vault, which the next round shares. Kind 3 takes SOL (`share_sol`).
pub fn process_share_token(ctx: Context<ShareToken>, index: u8, amount: u64) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    require!(amount > 0, KitError::ShareTooSmall);
    let a = &ctx.accounts;
    let config_key = a.kit_config.key();
    let kind = a.kit_config.reward_kind();
    require!(
        kind == REWARD_KIND_TOKEN || kind == REWARD_KIND_ROUNDS,
        KitError::WrongRewardKind
    );
    LegPools::read(&a.legs, &a.kit_config.mint)?;
    let leg = LegPools::token(&a.legs, usize::from(index))?;
    require!(
        *a.quote_mint.key == leg.quote_mint
            && *a.quote_token_program.key == leg.program_id()
            && *a.vault.key == leg.vault(&config_key),
        KitError::WrongRewardVault
    );
    let accumulate = kind == REWARD_KIND_TOKEN;
    let before = if accumulate {
        // Kind 1 has one leg, whose token is the reward mint and whose vault is `reward_vault`.
        require!(
            *a.vault.key == a.kit_config.reward_vault,
            KitError::WrongRewardVault
        );
        require!(a.kit_config.divides(), KitError::NoEligibleHolders);
        token_vault_amount(&a.kit_config, &config_key, &a.vault)?
    } else {
        spl::read(&a.vault, &leg.program_id()).map_or(0, |t| t.amount)
    };
    if accumulate {
        ctx.accounts.kit_config.sync(before, now)?;
    }
    let a = &ctx.accounts;
    spl::transfer_checked(
        &a.quote_token_program,
        &a.source,
        &a.quote_mint,
        &a.vault,
        &a.sharer,
        amount,
        leg.decimals,
        &[],
    )?;
    let after = spl::read(&a.vault, &leg.program_id()).map_or(0, |t| t.amount);
    let received = after.checked_sub(before).ok_or(KitError::MathOverflow)?;
    let config = &mut ctx.accounts.kit_config;
    if accumulate {
        config.add_share(received, now)?;
    }
    let (mint, total_shared) = (config.mint, config.total_shared);
    emit_cpi!(RewardsShared {
        mint,
        from: ctx.accounts.sharer.key(),
        amount: received,
        total_shared,
    });
    Ok(())
}

// ---- Kind 2: start_round, pay_round -----------------------------------------------------------------

/// Accounts of `start_round`. Remaining: each leg's vault (the kit config's associated token account
/// of the leg's token), in leg order.
#[event_cpi]
#[derive(Accounts)]
pub struct StartRound<'info> {
    /// Pays the `KitRounds` rent the first time.
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(constraint = kit_config.pays_rounds() @ KitError::WrongRewardKind)]
    pub kit_config: Account<'info, KitConfig>,
    /// CHECK: `["kit-rounds", mint]` (seeds-checked; made here the first time).
    #[account(mut, seeds = [KIT_ROUNDS_SEED, kit_config.mint.as_ref()], bump)]
    pub kit_rounds: UncheckedAccount<'info>,
    /// CHECK: the coin's `Legs` (read in place).
    pub legs: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// `KitRounds` at `info`, read (`None` while it is not made yet).
fn read_rounds(info: &AccountInfo) -> Result<Option<KitRounds>> {
    if *info.owner != crate::ID {
        return Ok(None);
    }
    let data = info.try_borrow_data()?;
    Ok(Some(KitRounds::try_deserialize(&mut &data[..])?))
}

/// Writes `rounds` into `info`.
fn write_rounds(info: &AccountInfo, rounds: &KitRounds) -> Result<()> {
    let mut data = info.try_borrow_mut_data()?;
    let mut w: &mut [u8] = &mut data;
    rounds.try_serialize(&mut w)?;
    Ok(())
}

/// `start_round()`: anyone, once a round (`RoundAlreadyStarted`): the round `⌊now / round_secs⌋`,
/// the eligible supply now, and each leg vault's balance (0 for a leg whose vault is not made yet);
/// nothing paid yet. Makes `KitRounds` the first time (the sender paying).
pub fn process_start_round(ctx: Context<StartRound>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let a = &ctx.accounts;
    let config = &a.kit_config;
    let config_key = config.key();
    let round = config.round_at(now);
    let info = a.kit_rounds.to_account_info();
    let existing = read_rounds(&info)?;
    if let Some(r) = &existing {
        require!(
            !(r.round == round && r.started_at > 0),
            KitError::RoundAlreadyStarted
        );
    } else {
        // Made here: a PDA of this program (its seeds sign), the sender paying the rent (an address
        // someone funded first is topped up).
        let bump = [ctx.bumps.kit_rounds];
        let seeds: &[&[u8]] = &[KIT_ROUNDS_SEED, config.mint.as_ref(), &bump];
        let rent = Rent::get()?.minimum_balance(KitRounds::LEN);
        let have = info.lamports();
        if have < rent {
            system_program::transfer(
                CpiContext::new(
                    a.system_program.key(),
                    system_program::Transfer {
                        from: a.payer.to_account_info(),
                        to: info.clone(),
                    },
                ),
                rent - have,
            )?;
        }
        system_program::allocate(
            CpiContext::new_with_signer(
                a.system_program.key(),
                system_program::Allocate {
                    account_to_allocate: info.clone(),
                },
                &[seeds],
            ),
            KitRounds::LEN as u64,
        )?;
        system_program::assign(
            CpiContext::new_with_signer(
                a.system_program.key(),
                system_program::Assign {
                    account_to_assign: info.clone(),
                },
                &[seeds],
            ),
            &crate::ID,
        )?;
    }
    LegPools::read(&a.legs, &config.mint)?;
    let count = usize::from(a.legs.try_borrow_data()?[LEGS_COUNT_OFFSET]).min(LEGS_MAX);
    require!(
        ctx.remaining_accounts.len() >= count,
        KitError::WrongRewardVault
    );
    let mut legs = [RoundLeg::default(); LEGS_MAX];
    let mut amounts = Vec::with_capacity(count);
    for (i, slot) in legs.iter_mut().enumerate().take(count) {
        let leg = LegPools::token(&a.legs, i)?;
        let vault = &ctx.remaining_accounts[i];
        require_keys_eq!(
            *vault.key,
            leg.vault(&config_key),
            KitError::WrongRewardVault
        );
        let amount = spl::read(vault, &leg.program_id()).map_or(0, |t| t.amount);
        slot.amount = amount;
        amounts.push(amount);
    }
    let rounds = KitRounds {
        version: VERSION,
        bump: ctx.bumps.kit_rounds,
        mint: config.mint,
        round,
        started_at: now,
        supply: config.eligible,
        count: count as u8,
        legs,
        reserved: [0; 32],
    };
    write_rounds(&info, &rounds)?;
    emit_cpi!(RoundStarted {
        mint: config.mint,
        round,
        supply: rounds.supply,
        amounts,
        ts: now,
    });
    Ok(())
}

/// Accounts of `pay_round(create_missing)`. Remaining: per leg, in leg order, the kit's vault of the
/// leg (w), the leg's quote mint and the owner's associated token account of it (w); then the token
/// programs the legs use and, with `create_missing`, the associated token account program.
#[event_cpi]
#[derive(Accounts)]
pub struct PayRound<'info> {
    /// Sends it; pays the owner's token accounts made with `create_missing`.
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(constraint = kit_config.pays_rounds() @ KitError::WrongRewardKind)]
    pub kit_config: Account<'info, KitConfig>,
    /// CHECK: `["kit-rounds", mint]` (seeds-checked, read and written in the handler).
    #[account(mut, seeds = [KIT_ROUNDS_SEED, kit_config.mint.as_ref()], bump)]
    pub kit_rounds: UncheckedAccount<'info>,
    /// CHECK: the config's mint (address-checked).
    #[account(address = kit_config.mint @ KitError::WrongMint)]
    pub mint: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = holding.mint == kit_config.mint @ KitError::WrongHolding,
        constraint = holding.owner == owner.key() @ KitError::WrongHolding
    )]
    pub holding: Account<'info, Holding>,
    /// CHECK: the holding's owner (constrained by `holding`).
    pub owner: UncheckedAccount<'info>,
    /// CHECK: the coin's `Legs`.
    pub legs: UncheckedAccount<'info>,
    /// CHECK: this program's `["hook-authority"]` PDA.
    #[account(address = HOOK_AUTHORITY @ KitError::WrongProgram)]
    pub kit_hook_authority: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = TOKEN_ID @ KitError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority.
    #[account(address = TOKEN_EVENT_AUTHORITY @ KitError::WrongProgram)]
    pub token_event_authority: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// The account among `all` with `key` (`NoTokenAccount` when it was not passed).
fn find<'a, 'info>(all: &'a [AccountInfo<'info>], key: &Pubkey) -> Result<&'a AccountInfo<'info>> {
    all.iter()
        .find(|a| a.key == key)
        .ok_or_else(|| error!(KitError::NoTokenAccount))
}

/// `pay_round(create_missing)`: see the module. Refused unless this round is started
/// (`RoundNotStarted`), the owner is a holder (`NotAHolder`), the holding was not paid this round
/// (`AlreadyPaidThisRound`) and held something through the round before (`NotHeldThroughRound`);
/// `NothingToClaim` when no leg pays it anything (nothing is then written).
pub fn process_pay_round<'info>(
    ctx: Context<'info, PayRound<'info>>,
    create_missing: bool,
) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let a = &ctx.accounts;
    let config = &a.kit_config;
    let config_key = config.key();
    let round = config.round_at(now);
    let info = a.kit_rounds.to_account_info();
    let mut rounds = read_rounds(&info)?.ok_or(KitError::RoundNotStarted)?;
    require!(
        rounds.round == round && rounds.started_at > 0,
        KitError::RoundNotStarted
    );
    let owner = a.owner.key();
    let legs = LegPools::read(&a.legs, &config.mint)?;
    require!(
        !config.is_excluded_with(&owner, Some(&legs)),
        KitError::NotAHolder
    );
    let read = a.holding.hook_data;
    let mut record = RoundData::read(&read);
    require!(record.paid != round, KitError::AlreadyPaidThisRound);
    let balance = record.eligible_balance(round, a.holding.amount);
    require!(
        balance > 0 && rounds.supply > 0,
        KitError::NotHeldThroughRound
    );
    let seeds = ConfigSeeds::of(config);
    let config_info = a.kit_config.to_account_info();
    let rest = ctx.remaining_accounts;
    let count = usize::from(rounds.count).min(LEGS_MAX);
    require!(rest.len() >= 3 * count, KitError::NoTokenAccount);
    let mut amounts = vec![0u64; count];
    let mut total = 0u64;
    for (i, paid_now) in amounts.iter_mut().enumerate() {
        let leg = LegPools::token(&a.legs, i)?;
        let [vault, quote_mint, destination] = [&rest[3 * i], &rest[3 * i + 1], &rest[3 * i + 2]];
        require!(
            *vault.key == leg.vault(&config_key) && *quote_mint.key == leg.quote_mint,
            KitError::WrongRewardVault
        );
        let program_id = leg.program_id();
        require_keys_eq!(
            *destination.key,
            spl::associated_address(&owner, &leg.quote_mint, &program_id),
            KitError::NoTokenAccount
        );
        let slot = rounds.legs[i];
        let share = u128::from(slot.amount) * u128::from(balance) / u128::from(rounds.supply);
        let share = u64::try_from(share)
            .unwrap_or(u64::MAX)
            .min(slot.amount.saturating_sub(slot.paid));
        if share == 0 {
            continue;
        }
        let program = find(rest, &program_id)?;
        if *destination.owner == anchor_lang::system_program::ID && destination.data_is_empty() {
            if !create_missing {
                continue;
            }
            spl::create_ata_idempotent(
                &a.payer.to_account_info(),
                destination,
                &a.owner.to_account_info(),
                quote_mint,
                &a.system_program.to_account_info(),
                program,
                find(rest, &spl::ATA_PROGRAM_ID)?,
            )?;
        }
        if !spl::receivable(destination, &program_id, &leg.quote_mint, &owner) {
            continue;
        }
        spl::transfer_checked(
            program,
            vault,
            quote_mint,
            destination,
            &config_info,
            share,
            leg.decimals,
            &[&seeds.seeds()],
        )?;
        rounds.legs[i].paid = slot.paid + share;
        *paid_now = share;
        total = total.saturating_add(share);
    }
    require!(total > 0, KitError::NothingToClaim);
    record.paid = round;
    let mut data = read;
    record.write(&mut data);
    write_hook_data(
        &a.mint.to_account_info(),
        &a.holding.to_account_info(),
        &a.kit_hook_authority.to_account_info(),
        &a.token_program.to_account_info(),
        &a.token_event_authority.to_account_info(),
        data,
    )?;
    write_rounds(&info, &rounds)?;
    emit_cpi!(RoundPaid {
        mint: rounds.mint,
        round,
        owner,
        balance,
        amounts,
    });
    Ok(())
}

// ---- Kind 3: swap_rewards_to_sol ------------------------------------------------------------------------

/// Accounts of `swap_rewards_to_sol(index, amount_in)`. Remaining: the launchpad's `swap_to_sol`
/// accounts from its third (the launch) on, with its fourth to sixth (the `Legs`, the quote mint,
/// the source) left out: the kit names those itself (`docs/token-quotes.md` §7.5.5).
#[event_cpi]
#[derive(Accounts)]
pub struct SwapRewardsToSol<'info> {
    /// Sends it; pays the wrapped-SOL account's rent for the swap (returned by it).
    #[account(mut)]
    pub crank: Signer<'info>,
    #[account(
        mut,
        constraint = kit_config.rewards_on()
            && kit_config.reward_kind() == REWARD_KIND_SOL
            && kit_config.is_native() @ KitError::WrongRewardKind
    )]
    pub kit_config: Account<'info, KitConfig>,
    /// CHECK: the coin's `Legs` (read in place).
    pub legs: UncheckedAccount<'info>,
    /// CHECK: leg `index`'s quote mint (checked in the handler).
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: the kit's vault of the leg (checked in the handler).
    #[account(mut)]
    pub vault: UncheckedAccount<'info>,
    /// CHECK: the launchpad.
    #[account(address = LAUNCH_ID @ KitError::WrongProgram)]
    pub launch_program: UncheckedAccount<'info>,
}

/// `swap_rewards_to_sol(index, amount_in)`: sync at the native vault; the launchpad's `swap_to_sol`
/// of `amount_in` (0: the vault's whole balance) of leg `index`'s vault, the config signing as its
/// owner (the launchpad takes at most what the token's reference allows this window and its
/// minimum out from the reference pool's price); what landed on the config streamed over the hour
/// as a share when holders hold the threshold and it is at least `MIN_SHARE_LAMPORTS`, else left
/// for the next sync to divide (or hold).
pub fn process_swap_rewards_to_sol<'info>(
    ctx: Context<'info, SwapRewardsToSol<'info>>,
    index: u8,
    amount_in: u64,
) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let a = &ctx.accounts;
    let config_key = a.kit_config.key();
    LegPools::read(&a.legs, &a.kit_config.mint)?;
    let leg = LegPools::token(&a.legs, usize::from(index))?;
    require!(
        *a.quote_mint.key == leg.quote_mint && *a.vault.key == leg.vault(&config_key),
        KitError::WrongRewardVault
    );
    let amount = if amount_in == 0 {
        spl::read(&a.vault, &leg.program_id()).map_or(0, |t| t.amount)
    } else {
        amount_in
    };
    require!(amount > 0, KitError::NothingToClaim);
    let config_info = a.kit_config.to_account_info();
    let before = a.kit_config.native_vault(config_info.lamports());
    ctx.accounts.kit_config.sync(before, now)?;

    // The launchpad's swap_to_sol: crank, the config (the tokens' owner, receiving the SOL), the
    // launch, the `Legs`, the quote mint, the vault, then the rest as passed.
    let a = &ctx.accounts;
    let rest = ctx.remaining_accounts;
    require!(!rest.is_empty(), KitError::WrongSwap);
    let meta = |i: &AccountInfo| AccountMeta {
        pubkey: *i.key,
        is_signer: false,
        is_writable: i.is_writable,
    };
    let mut metas = vec![
        AccountMeta::new(a.crank.key(), true),
        AccountMeta::new(config_key, true),
        meta(&rest[0]),
        AccountMeta::new_readonly(a.legs.key(), false),
        AccountMeta::new_readonly(a.quote_mint.key(), false),
        AccountMeta::new(a.vault.key(), false),
    ];
    metas.extend(rest[1..].iter().map(meta));
    let mut infos = vec![
        a.crank.to_account_info(),
        config_info.clone(),
        rest[0].clone(),
        a.legs.to_account_info(),
        a.quote_mint.to_account_info(),
        a.vault.to_account_info(),
    ];
    infos.extend(rest[1..].iter().cloned());
    infos.push(a.launch_program.to_account_info());
    let mut data = SWAP_TO_SOL_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&amount.to_le_bytes());
    let seeds = ConfigSeeds::of(&a.kit_config);
    invoke_signed(
        &Instruction {
            program_id: LAUNCH_ID,
            accounts: metas,
            data,
        },
        &infos,
        &[&seeds.seeds()],
    )?;
    let after = ctx.accounts.kit_config.native_vault(config_info.lamports());
    let lamports = after.checked_sub(before).ok_or(KitError::MathOverflow)?;
    let config = &mut ctx.accounts.kit_config;
    let shared = config.divides() && lamports >= MIN_SHARE_LAMPORTS;
    if shared {
        config.add_share(lamports, now)?;
    }
    emit_cpi!(RewardsSwappedToSol {
        mint: config.mint,
        index,
        quote_mint: leg.quote_mint,
        amount_in: amount,
        lamports,
        shared,
    });
    Ok(())
}
