//! Native-SOL pools (`crate::native`): a pool quoted in bridged SOL (by its mint and seeds,
//! which never change) that holds its quote as lamports on the pool account itself.
//!
//! The books of a native pool: `quote_reserve + protocol_fees_quote` is its whole quote, held in
//! two forms. The buffer (`Pool::buffer_quote`, in `reserved`) is bridged SOL in the pool's quote
//! vault, which the old `swap` and the liquidity instructions still move; the rest
//! (`Pool::lamports_quote`) is lamports on the pool account. The invariant, checked at the end of
//! every instruction that touches a native pool's quote:
//!
//! ```text
//! pool lamports >= rent_exempt_minimum(Pool::LEN) + quote_reserve + protocol_fees_quote - buffer
//! quote vault balance >= buffer
//! ```
//!
//! Both are equalities unless someone sent the pool SOL or bridged SOL from outside (a donation the
//! books never count) or rent fell. Payouts are always computed from the books, never from the
//! balances, so a donation can never be taken out by a trader.
//!
//! The bridge moves value between the two forms: `unwrap_sol` with the pool as its user (the pool
//! signs with its seeds, the lamports land on it) and `wrap_sol`, which takes a system transfer
//! from its user, so the pool, which owns data, cannot be that user: a plain wallet that signed
//! the transaction is credited the lamports from the pool first and wraps them straight into the
//! destination holding, ending where it started.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::{invoke, invoke_signed};
use anchor_lang::system_program;

use crate::constants::*;
use crate::error::SwapError;
use crate::state::Pool;
use crate::token::{balance, read_holding};

/// The bridge's accounts for `wrap_sol` and `unwrap_sol` (the bridge checks every one of them but
/// its program id and the bridged SOL mint, checked here).
pub struct BridgeSol<'info> {
    /// The bridge.
    pub program: AccountInfo<'info>,
    /// Its config.
    pub config: AccountInfo<'info>,
    /// The SOL wrapper.
    pub wrapper: AccountInfo<'info>,
    /// The SOL vault.
    pub sol_vault: AccountInfo<'info>,
    /// Bridged SOL (writable).
    pub wrapped_mint: AccountInfo<'info>,
    /// The bridge's event authority.
    pub event_authority: AccountInfo<'info>,
    /// The token program.
    pub token_program: AccountInfo<'info>,
    /// The token program's event authority.
    pub token_event_authority: AccountInfo<'info>,
    /// The System Program.
    pub system_program: AccountInfo<'info>,
}

impl<'info> BridgeSol<'info> {
    /// Checks the bridge's program id and that the bridged SOL mint is passed writable.
    pub fn check(&self) -> Result<()> {
        require_keys_eq!(
            *self.program.key,
            BRIDGE_ID,
            SwapError::BridgeAccountsMissing
        );
        require_keys_eq!(
            *self.wrapped_mint.key,
            BRIDGED_SOL_MINT,
            SwapError::BridgeAccountsMissing
        );
        require!(
            self.wrapped_mint.is_writable,
            SwapError::BridgeAccountsMissing
        );
        require_keys_eq!(
            *self.token_program.key,
            bordrless_token::ID,
            SwapError::BridgeAccountsMissing
        );
        Ok(())
    }

    fn call(
        &self,
        discriminator: [u8; 8],
        user: &AccountInfo<'info>,
        holding: &AccountInfo<'info>,
        amount: u64,
        signer_seeds: &[&[&[u8]]],
        trailing: Option<&AccountInfo<'info>>,
    ) -> Result<()> {
        let mut data = discriminator.to_vec();
        data.extend_from_slice(&amount.to_le_bytes());
        let ro = |info: &AccountInfo| AccountMeta::new_readonly(info.key(), false);
        let rw = |info: &AccountInfo| AccountMeta::new(info.key(), false);
        let mut ix = anchor_lang::solana_program::instruction::Instruction {
            program_id: BRIDGE_ID,
            accounts: vec![
                AccountMeta::new(user.key(), true),
                ro(&self.config),
                rw(&self.wrapper),
                rw(&self.sol_vault),
                rw(&self.wrapped_mint),
                rw(holding),
                ro(&self.token_program),
                ro(&self.token_event_authority),
                ro(&self.system_program),
                ro(&self.event_authority),
                ro(&self.program),
            ],
            data,
        };
        let mut infos = vec![
            user.clone(),
            self.config.clone(),
            self.wrapper.clone(),
            self.sol_vault.clone(),
            self.wrapped_mint.clone(),
            holding.clone(),
            self.token_program.clone(),
            self.token_event_authority.clone(),
            self.system_program.clone(),
            self.event_authority.clone(),
            self.program.clone(),
        ];
        // An account whose lamports this program changed before the CPI (the pool, which credited
        // the wallet): passed on (the bridge ignores it) so the runtime sees both sides of that
        // move at the CPI, not only the wallet's.
        if let Some(extra) = trailing {
            ix.accounts.push(AccountMeta::new(extra.key(), false));
            infos.push(extra.clone());
        }
        if signer_seeds.is_empty() {
            invoke(&ix, &infos)?;
        } else {
            invoke_signed(&ix, &infos, signer_seeds)?;
        }
        Ok(())
    }

    /// `unwrap_sol` of `amount` from the pool's quote vault, the pool its user (signing with
    /// `pool_seeds`): the lamports land on the pool account.
    pub fn unwrap_to_pool(
        &self,
        pool: &AccountInfo<'info>,
        quote_vault: &AccountInfo<'info>,
        amount: u64,
        pool_seeds: &[&[u8]],
    ) -> Result<()> {
        if amount == 0 {
            return Ok(());
        }
        self.call(
            UNWRAP_SOL_DISCRIMINATOR,
            pool,
            quote_vault,
            amount,
            &[pool_seeds],
            None,
        )
    }

    /// `wrap_sol` of `amount` lamports from `wallet` (a system account that signed the
    /// transaction) into `holding`, any holding of bridged SOL.
    pub fn wrap_from_wallet(
        &self,
        wallet: &AccountInfo<'info>,
        holding: &AccountInfo<'info>,
        amount: u64,
    ) -> Result<()> {
        if amount == 0 {
            return Ok(());
        }
        require!(
            wallet.is_signer && wallet.is_writable,
            SwapError::WrongNativeAccount
        );
        self.call(WRAP_SOL_DISCRIMINATOR, wallet, holding, amount, &[], None)
    }

    /// Wraps `amount` of the pool's lamports into `holding`: the pool's lamports go to `wallet`
    /// (a writable system account that signed the transaction), which wraps them at once and
    /// ends where it started.
    pub fn wrap_from_pool(
        &self,
        pool: &AccountInfo<'info>,
        wallet: &AccountInfo<'info>,
        holding: &AccountInfo<'info>,
        amount: u64,
    ) -> Result<()> {
        if amount == 0 {
            return Ok(());
        }
        require!(
            wallet.is_signer && wallet.is_writable,
            SwapError::WrongNativeAccount
        );
        move_lamports(pool, wallet, amount)?;
        self.call(
            WRAP_SOL_DISCRIMINATOR,
            wallet,
            holding,
            amount,
            &[],
            Some(pool),
        )
    }
}

/// The rent-exempt minimum of an account of `info`'s size.
pub fn rent_minimum(info: &AccountInfo) -> Result<u64> {
    Ok(Rent::get()?.minimum_balance(info.data_len()))
}

/// Moves `amount` lamports from `from` (an account this program owns: a pool) to `to`, directly.
/// The pool is never left below its rent-exempt minimum plus its books (checked again by
/// [`check_native`] at the end of the instruction).
pub fn move_lamports(from: &AccountInfo, to: &AccountInfo, amount: u64) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    require!(to.is_writable, SwapError::WrongNativeAccount);
    require!(!to.executable, SwapError::WrongNativeAccount);
    let from_lamports = from.lamports();
    **from.try_borrow_mut_lamports()? = from_lamports
        .checked_sub(amount)
        .ok_or(SwapError::LamportsShort)?;
    let to_lamports = to.lamports();
    **to.try_borrow_mut_lamports()? = to_lamports
        .checked_add(amount)
        .ok_or(SwapError::MathOverflow)?;
    Ok(())
}

/// A system transfer of `amount` lamports from `from` (a wallet that signed) to `to`.
pub fn transfer_from_wallet<'info>(
    system: &AccountInfo<'info>,
    from: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    amount: u64,
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    system_program::transfer(
        CpiContext::new(
            system.key(),
            system_program::Transfer {
                from: from.clone(),
                to: to.clone(),
            },
        ),
        amount,
    )
}

/// An account about to be credited `amount` lamports must end at or above the rent-exempt
/// minimum of its size (the runtime would otherwise fail the transaction with an opaque
/// `InsufficientFundsForRent`): a clear `RecipientBelowRent` instead.
pub fn check_credit_rent(to: &AccountInfo, amount: u64) -> Result<()> {
    let after = to
        .lamports()
        .checked_add(amount)
        .ok_or(SwapError::MathOverflow)?;
    require!(after >= rent_minimum(to)?, SwapError::RecipientBelowRent);
    Ok(())
}

/// How a native quote-side cut is paid to the account a hook names: lamports, or, to a holding of
/// bridged SOL (an old-style destination), bridged SOL wrapped by the bridge.
pub fn wraps_into(info: &AccountInfo) -> Result<bool> {
    require!(!info.executable, SwapError::InvalidDeltaAccount);
    if *info.owner == bordrless_token::ID {
        let holding = read_holding(info).map_err(|_| SwapError::InvalidDeltaAccount)?;
        require_keys_eq!(
            holding.mint,
            BRIDGED_SOL_MINT,
            SwapError::InvalidDeltaAccount
        );
        return Ok(true);
    }
    Ok(false)
}

/// The invariant of a native pool (see the module): its lamports cover its rent-exempt minimum
/// plus the quote its books hold as lamports, and its quote vault (when passed) covers the
/// buffer. Nothing for a bridged pool.
pub fn check_native(
    pool_info: &AccountInfo,
    pool: &Pool,
    quote_vault: Option<&AccountInfo>,
) -> Result<()> {
    if !pool.quote_native() {
        return Ok(());
    }
    let needed = rent_minimum(pool_info)?
        .checked_add(pool.lamports_quote().ok_or(SwapError::NativeInvariant)?)
        .ok_or(SwapError::MathOverflow)?;
    require!(pool_info.lamports() >= needed, SwapError::NativeInvariant);
    if let Some(vault) = quote_vault {
        require!(
            balance(vault)? >= pool.buffer_quote(),
            SwapError::NativeInvariant
        );
    }
    Ok(())
}

/// Adds `amount` to a native pool's buffer (bridged SOL that reached its quote vault).
pub fn buffer_in(pool: &mut Pool, amount: u64) -> Result<()> {
    let b = pool
        .buffer_quote()
        .checked_add(amount)
        .ok_or(SwapError::MathOverflow)?;
    pool.set_buffer_quote(b);
    Ok(())
}

/// Takes `amount` from a native pool's buffer (bridged SOL that left its quote vault), else
/// `BufferShort`.
pub fn buffer_out(pool: &mut Pool, amount: u64) -> Result<()> {
    let b = pool
        .buffer_quote()
        .checked_sub(amount)
        .ok_or(SwapError::BufferShort)?;
    pool.set_buffer_quote(b);
    Ok(())
}

/// Whether the running instruction is one the transaction sends directly (not a CPI).
pub fn top_level() -> bool {
    anchor_lang::solana_program::instruction::get_stack_height()
        == anchor_lang::solana_program::instruction::TRANSACTION_LEVEL_STACK_HEIGHT
}
