//! The token hook callbacks the token program calls, signing with its signer for the kit,
//! `["hook-authority", KIT_ID]` ([`TOKEN_HOOK_AUTHORITY`]). Only the token program can sign for
//! it, and only when it calls the kit: the signer it gives any other hook is that hook's own, so
//! a hook that passes its signer on in a CPI to the kit is refused (`BadHookSigner`), and the
//! arguments a callback trusts (balances, owners, amounts, hook data) are the token program's.
//! Account binding (`docs/hooks-v2.md` §4.6), then the rules of §4.9 ([`crate::rules`]). No CPI
//! and no event: while a callback runs the token program is on the stack, so the kit could not
//! call it.

use anchor_lang::prelude::*;
use bordrless_hook::{HookReturn, TokenHookArgs};

use crate::constants::*;
use crate::error::KitError;
use crate::rules;
use crate::state::KitConfig;

/// Accounts of `before_transfer` and `before_burn`: the token program's prefix, then the two
/// extras the kit's registry always lists.
#[derive(Accounts)]
pub struct Callback<'info> {
    /// CHECK: the token program's signer for the kit: a signer, compared with the constant.
    #[account(signer, address = TOKEN_HOOK_AUTHORITY @ KitError::BadHookSigner)]
    pub hook_signer: UncheckedAccount<'info>,
    /// CHECK: the mint (compared with the arguments' mint).
    pub mint: UncheckedAccount<'info>,
    /// CHECK: the source holding; its balance, owner and hook data come from the arguments.
    pub source: UncheckedAccount<'info>,
    /// CHECK: the destination holding (the mint for a burn).
    pub destination: UncheckedAccount<'info>,
    /// CHECK: whoever signed the operation.
    pub authority: UncheckedAccount<'info>,
    /// The token's config (owner and discriminator checked); its mint is checked in the handler.
    /// Only `init` creates a config, and only at `["kit", mint]`, so this binds it.
    #[account(mut)]
    pub kit_config: Account<'info, KitConfig>,
    /// CHECK: the reward vault, present exactly when holder rewards are on (the kit's id stands
    /// for none); address-checked in the handler.
    pub reward_vault: Option<UncheckedAccount<'info>>,
}

/// The balance of a token-program holding: owner and discriminator checked, the amount read at its
/// fixed offset (discriminator, version, bump, mint, owner, then the amount).
pub fn holding_amount(info: &AccountInfo) -> Result<u64> {
    require_keys_eq!(*info.owner, TOKEN_ID, KitError::WrongRewardVault);
    let data = info.try_borrow_data()?;
    use anchor_lang::Discriminator;
    let discriminator = bordrless_token::state::Holding::DISCRIMINATOR;
    require!(
        data.len() >= HOLDING_AMOUNT_OFFSET + 8 && data[..discriminator.len()] == *discriminator,
        KitError::WrongRewardVault
    );
    let mut amount = [0u8; 8];
    amount.copy_from_slice(&data[HOLDING_AMOUNT_OFFSET..HOLDING_AMOUNT_OFFSET + 8]);
    Ok(u64::from_le_bytes(amount))
}

/// Offset of `amount` in a holding's data.
pub const HOLDING_AMOUNT_OFFSET: usize = DISCRIMINATOR_LEN + 1 + 1 + 32 + 32;

/// Binds the callback's accounts to the arguments (the prefix mint and the config's mint are the
/// arguments' mint; the reward vault is passed exactly when holder rewards are on, at the
/// config's address) and answers the vault's balance when it is.
fn bind(accounts: &Callback, args: &TokenHookArgs) -> Result<Option<u64>> {
    let config = &accounts.kit_config;
    require_keys_eq!(accounts.mint.key(), args.mint, KitError::WrongMint);
    require_keys_eq!(config.mint, args.mint, KitError::WrongMint);
    match (&accounts.reward_vault, config.rewards_on()) {
        (Some(vault), true) => {
            require_keys_eq!(vault.key(), config.reward_vault, KitError::WrongRewardVault);
            Ok(Some(holding_amount(vault)?))
        }
        (None, true) => err!(KitError::MissingRewardVault),
        (Some(_), false) => err!(KitError::WrongRewardVault),
        (None, false) => Ok(None),
    }
}

/// `before_transfer`.
pub fn process_before_transfer(ctx: Context<Callback>, args: TokenHookArgs) -> Result<HookReturn> {
    let vault = bind(ctx.accounts, &args)?;
    let now = Clock::get()?.unix_timestamp;
    let key = ctx.accounts.kit_config.key();
    rules::before_transfer(&mut ctx.accounts.kit_config, &key, &args, vault, now)
}

/// `before_burn`.
pub fn process_before_burn(ctx: Context<Callback>, args: TokenHookArgs) -> Result<HookReturn> {
    let vault = bind(ctx.accounts, &args)?;
    let now = Clock::get()?.unix_timestamp;
    rules::before_burn(&mut ctx.accounts.kit_config, &args, vault, now)
}
