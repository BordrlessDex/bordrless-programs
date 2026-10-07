//! Pool hook CPIs, the reading of a hook's answer and the cut it takes from one side of a swap.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{instruction::Instruction, program::invoke_signed};
use bordrless_hook::{
    clear_return_data, hook_signer_at, read_answer, Allowed, Answer, HookReturn, PoolHookArgs,
    HOOK_AUTHORITY_SEED, POOL_PREFIX_ACCOUNTS,
};

use crate::error::{answer_error, SwapError};
use crate::events::DeltaPaid;
use crate::token::read_holding;

/// What a pool hook's answer takes from one side of a swap: deltas, each sent to the holding it
/// names, then a burn. Built by [`PoolHookCall::cut`], which checks it.
pub struct Cut<'a, 'info> {
    /// Each delta's holding and amount, in the answer's order.
    pub deltas: Vec<(&'a AccountInfo<'info>, u64)>,
    /// The burn.
    pub burn: u64,
    /// The deltas and the burn together.
    pub taken: u64,
}

impl<'a, 'info> Cut<'a, 'info> {
    /// No cut: the hook did not run, answered nothing, or may not answer.
    pub fn none() -> Self {
        Self {
            deltas: Vec::new(),
            burn: 0,
            taken: 0,
        }
    }

    /// The deltas as the `Swapped` event carries them.
    pub fn paid(&self) -> Vec<DeltaPaid> {
        self.deltas
            .iter()
            .map(|(holding, amount)| DeltaPaid {
                holding: *holding.key,
                amount: *amount,
            })
            .collect()
    }
}

/// The accounts a pool hook CPI is built from.
pub struct PoolHookCall<'a, 'info> {
    /// The hook program.
    pub hook_program: AccountInfo<'info>,
    /// This program's signer of the hook's callbacks, `["hook-authority", hook_program]`.
    pub hook_signer: AccountInfo<'info>,
    /// Bump of `hook_signer`.
    pub bump: u8,
    /// `pool`, `base_mint`, `quote_mint`, `actor`.
    pub prefix: [AccountInfo<'info>; 4],
    /// The hook's extra accounts.
    pub extras: &'a [AccountInfo<'info>],
}

impl<'a, 'info> PoolHookCall<'a, 'info> {
    /// The call to a pool's hook, when it has one (`hook`, its signer's bump `bump`): the hook
    /// program passed must be the pool's (`HookProgramMissing`, `WrongHookProgram`), and the hook
    /// signer this program's `["hook-authority", hook_program]` at that bump (`BadHookSigner`).
    /// One signer per hook program: a hook that passes on the signer it receives can only vouch
    /// for itself.
    pub fn of(
        hook: Option<Pubkey>,
        bump: u8,
        hook_program: Option<AccountInfo<'info>>,
        hook_signer: Option<AccountInfo<'info>>,
        prefix: [AccountInfo<'info>; 4],
        extras: &'a [AccountInfo<'info>],
    ) -> Result<Option<Self>> {
        let Some(program) = hook else {
            return Ok(None);
        };
        let info = hook_program.ok_or(SwapError::HookProgramMissing)?;
        require_keys_eq!(info.key(), program, SwapError::WrongHookProgram);
        let signer = hook_signer.ok_or(SwapError::BadHookSigner)?;
        let expected =
            hook_signer_at(&crate::ID, &program, bump).ok_or(SwapError::BadHookSigner)?;
        require_keys_eq!(signer.key(), expected, SwapError::BadHookSigner);
        Ok(Some(Self {
            hook_program: info,
            hook_signer: signer,
            bump,
            prefix,
            extras,
        }))
    }

    /// Invokes one callback and reads its answer when `allowed` (the callback and the pool's flags)
    /// lets it answer anything, checked by the protocol's rules (no field outside `allowed`, at most
    /// three deltas, each above zero, no account index twice, sums in checked arithmetic). Answers
    /// it with the sum of its deltas and burn.
    pub fn invoke(
        &self,
        discriminator: [u8; 8],
        args: &PoolHookArgs,
        allowed: Allowed,
    ) -> Result<Option<Answer>> {
        let mut data = Vec::with_capacity(8 + 256);
        data.extend_from_slice(&discriminator);
        args.serialize(&mut data)?;
        let mut metas = Vec::with_capacity(POOL_PREFIX_ACCOUNTS + self.extras.len());
        metas.push(AccountMeta::new_readonly(*self.hook_signer.key, true));
        for info in &self.prefix {
            metas.push(AccountMeta::new_readonly(*info.key, false));
        }
        let mut infos = Vec::with_capacity(POOL_PREFIX_ACCOUNTS + self.extras.len() + 1);
        infos.push(self.hook_signer.clone());
        for info in &self.prefix {
            infos.push(info.clone());
        }
        for info in self.extras {
            metas.push(AccountMeta {
                pubkey: *info.key,
                is_signer: false,
                is_writable: info.is_writable,
            });
            infos.push(info.clone());
        }
        infos.push(self.hook_program.clone());
        let ix = Instruction {
            program_id: *self.hook_program.key,
            accounts: metas,
            data,
        };
        clear_return_data();
        invoke_signed(
            &ix,
            &infos,
            &[&[
                HOOK_AUTHORITY_SEED,
                self.hook_program.key.as_ref(),
                &[self.bump],
            ]],
        )?;
        read_answer(self.hook_program.key, allowed).map_err(answer_error)
    }

    /// The extra account a return names (an index over prefix then extras), checked to be a
    /// writable holding of `mint` that is none of `forbidden`.
    pub fn delta_account(
        &self,
        index: u8,
        mint: &Pubkey,
        forbidden: &[Pubkey],
    ) -> Result<&'a AccountInfo<'info>> {
        let i = usize::from(index);
        require!(i >= POOL_PREFIX_ACCOUNTS, SwapError::InvalidDeltaAccount);
        let info = self
            .extras
            .get(i - POOL_PREFIX_ACCOUNTS)
            .ok_or(SwapError::InvalidDeltaAccount)?;
        require!(info.is_writable, SwapError::InvalidDeltaAccount);
        require!(
            !forbidden.contains(info.key),
            SwapError::InvalidDeltaAccount
        );
        let holding = read_holding(info).map_err(|_| SwapError::InvalidDeltaAccount)?;
        require_keys_eq!(holding.mint, *mint, SwapError::InvalidDeltaAccount);
        Ok(info)
    }

    /// The cut a swap callback's answer takes from `amount` of `mint` (one side of the swap),
    /// checked by the DEX's rules before anything moves: the deltas and the burn together
    /// (`taken`, summed in checked arithmetic by the protocol crate) below `amount`, else
    /// `DeltaTooLarge`; each delta account an extra that is a writable holding of `mint`, none of
    /// `forbidden` (the two vaults and the trader's two holdings) and named once, by index and by
    /// key, else `InvalidDeltaAccount`; a burn only with `mint` passed writable, else
    /// `MintNotWritable`.
    pub fn cut(
        &self,
        answer: &HookReturn,
        taken: u64,
        amount: u64,
        mint: &AccountInfo<'info>,
        forbidden: &[Pubkey],
    ) -> Result<Cut<'a, 'info>> {
        require!(taken < amount, SwapError::DeltaTooLarge);
        let mut deltas: Vec<(&'a AccountInfo<'info>, u64)> =
            Vec::with_capacity(answer.deltas.len());
        for delta in &answer.deltas {
            let info = self.delta_account(delta.account, mint.key, forbidden)?;
            require!(
                !deltas.iter().any(|(named, _)| named.key == info.key),
                SwapError::InvalidDeltaAccount
            );
            deltas.push((info, delta.amount));
        }
        if answer.burn > 0 {
            require!(mint.is_writable, SwapError::MintNotWritable);
        }
        Ok(Cut {
            deltas,
            burn: answer.burn,
            taken,
        })
    }
}

/// Splits the remaining accounts of a swap or deposit into the two token hooks' extras and the pool
/// hook's extras, by the counts the caller gave.
pub fn split_extras<'a, 'info>(
    remaining: &'a [AccountInfo<'info>],
    first: u8,
    second: u8,
) -> Result<(
    &'a [AccountInfo<'info>],
    &'a [AccountInfo<'info>],
    &'a [AccountInfo<'info>],
)> {
    let (f, s) = (usize::from(first), usize::from(second));
    require!(remaining.len() >= f + s, SwapError::AccountCounts);
    Ok((&remaining[..f], &remaining[f..f + s], &remaining[f + s..]))
}
