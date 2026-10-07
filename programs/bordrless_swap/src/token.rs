//! CPIs into the token standard, and reads of its accounts.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use bordrless_token::client as token_client;
pub use bordrless_token::client::{read_holding, read_mint};
use bordrless_token::instructions::CreateMintArgs;

use crate::error::SwapError;

/// One side's token hook in a DEX instruction. A mint with a hook has a slice of the remaining
/// accounts: the hook program, the token program's signer of its callbacks
/// (`["hook-authority", hook_program]`, one per hook program), then the hook's extra accounts. A
/// mint without one has an empty slice. The token program checks the hook program and the signer
/// against the mint; this program only passes them on.
#[derive(Clone, Copy)]
pub struct TokenSide<'a, 'info> {
    /// The hook program, when the mint has a hook.
    pub program: Option<&'a AccountInfo<'info>>,
    /// The token program's signer of its callbacks.
    pub signer: Option<&'a AccountInfo<'info>>,
    /// The hook's extra accounts.
    pub extras: &'a [AccountInfo<'info>],
}

impl<'a, 'info> TokenSide<'a, 'info> {
    /// The side whose slice is `slice`, for a mint that has a hook or not (`has_hook`). A slice too
    /// short for its hook leaves the hook program or the signer out, and the token program
    /// refuses the transfer (`HookProgramMissing`, `BadHookSigner`).
    pub fn of(slice: &'a [AccountInfo<'info>], has_hook: bool) -> Self {
        if !has_hook {
            return Self {
                program: None,
                signer: None,
                extras: slice,
            };
        }
        Self {
            program: slice.first(),
            signer: slice.get(1),
            extras: slice.get(2..).unwrap_or(&[]),
        }
    }

    /// A mint without a hook (an LP mint).
    pub fn none() -> Self {
        Self {
            program: None,
            signer: None,
            extras: &[],
        }
    }
}

/// The fixed accounts every token CPI needs.
pub struct TokenAccounts<'info> {
    /// The token program.
    pub program: AccountInfo<'info>,
    /// Its event authority.
    pub event_authority: AccountInfo<'info>,
}

impl<'info> TokenAccounts<'info> {
    /// Checks the two fixed accounts are the token program's.
    pub fn check(&self) -> Result<()> {
        require_keys_eq!(
            *self.program.key,
            bordrless_token::ID,
            anchor_lang::error::ErrorCode::InvalidProgramId
        );
        require_keys_eq!(
            *self.event_authority.key,
            token_client::event_authority(),
            SwapError::WrongHolding
        );
        Ok(())
    }

    /// The hook of a token instruction for `side` (the token program's id stands for an absent
    /// hook program or signer), and the two account infos that go with it.
    fn hook_of(
        &self,
        side: &TokenSide<'_, 'info>,
    ) -> (
        Option<token_client::Hook>,
        AccountInfo<'info>,
        AccountInfo<'info>,
    ) {
        let program = side
            .program
            .cloned()
            .unwrap_or_else(|| self.program.clone());
        let signer = side.signer.cloned().unwrap_or_else(|| self.program.clone());
        let hook = side.program.map(|p| token_client::Hook {
            program: *p.key,
            signer: *signer.key,
        });
        (hook, program, signer)
    }

    /// `transfer` of `amount` from `source` to `destination`, signed by `authority` (a signer of
    /// the instruction, or a PDA with `signer_seeds`), with the mint's hook as `side` gives it.
    #[allow(clippy::too_many_arguments)]
    pub fn transfer(
        &self,
        authority: &AccountInfo<'info>,
        source: &AccountInfo<'info>,
        destination: &AccountInfo<'info>,
        mint: &AccountInfo<'info>,
        side: &TokenSide<'_, 'info>,
        amount: u64,
        signer_seeds: &[&[&[u8]]],
    ) -> Result<()> {
        let (hook, hook_program, hook_signer) = self.hook_of(side);
        let ix = token_client::transfer_with(
            *authority.key,
            *source.key,
            *destination.key,
            *mint.key,
            hook,
            extra_metas(side.extras),
            amount,
        );
        let mut infos = Vec::with_capacity(8 + side.extras.len());
        infos.push(authority.clone());
        infos.push(source.clone());
        infos.push(destination.clone());
        infos.push(mint.clone());
        infos.push(hook_program);
        infos.push(hook_signer);
        infos.push(self.event_authority.clone());
        infos.push(self.program.clone());
        infos.extend(side.extras.iter().cloned());
        invoke_signed(&ix, &infos, signer_seeds).map_err(Into::into)
    }

    /// `create_mint` of a mint that signs through `signer_seeds` (the LP mint PDA).
    pub fn create_mint(
        &self,
        payer: &AccountInfo<'info>,
        mint: &AccountInfo<'info>,
        system_program: &AccountInfo<'info>,
        args: CreateMintArgs,
        signer_seeds: &[&[&[u8]]],
    ) -> Result<()> {
        let ix = token_client::create_mint(*payer.key, *mint.key, args);
        invoke_signed(
            &ix,
            &[
                payer.clone(),
                mint.clone(),
                system_program.clone(),
                self.event_authority.clone(),
                self.program.clone(),
            ],
            signer_seeds,
        )
        .map_err(Into::into)
    }

    /// `create_holding` of `owner` for `mint`, paid by `payer`.
    pub fn create_holding(
        &self,
        payer: &AccountInfo<'info>,
        mint: &AccountInfo<'info>,
        owner: &AccountInfo<'info>,
        holding: &AccountInfo<'info>,
        system_program: &AccountInfo<'info>,
    ) -> Result<()> {
        let ix = token_client::create_holding(*payer.key, *mint.key, *owner.key);
        require_keys_eq!(ix.accounts[3].pubkey, *holding.key, SwapError::WrongHolding);
        invoke_signed(
            &ix,
            &[
                payer.clone(),
                mint.clone(),
                owner.clone(),
                holding.clone(),
                system_program.clone(),
                self.event_authority.clone(),
                self.program.clone(),
            ],
            &[],
        )
        .map_err(Into::into)
    }

    /// `mint_to` on a hook-less mint (an LP mint) whose authority signs through `signer_seeds`.
    pub fn mint_to(
        &self,
        authority: &AccountInfo<'info>,
        mint: &AccountInfo<'info>,
        destination: &AccountInfo<'info>,
        amount: u64,
        signer_seeds: &[&[&[u8]]],
    ) -> Result<()> {
        let ix = token_client::mint_to_with(
            *authority.key,
            *mint.key,
            *destination.key,
            None,
            vec![],
            amount,
        );
        invoke_signed(
            &ix,
            &[
                authority.clone(),
                mint.clone(),
                destination.clone(),
                self.program.clone(),
                self.program.clone(),
                self.event_authority.clone(),
                self.program.clone(),
            ],
            signer_seeds,
        )
        .map_err(Into::into)
    }

    /// `burn` of `amount` from `source`, signed by `authority` (a signer of the instruction, or a
    /// PDA with `signer_seeds`: the pool burning from its vault), with the mint's hook as `side`
    /// gives it. `mint` must be writable here.
    #[allow(clippy::too_many_arguments)]
    pub fn burn(
        &self,
        authority: &AccountInfo<'info>,
        source: &AccountInfo<'info>,
        mint: &AccountInfo<'info>,
        side: &TokenSide<'_, 'info>,
        amount: u64,
        signer_seeds: &[&[&[u8]]],
    ) -> Result<()> {
        let (hook, hook_program, hook_signer) = self.hook_of(side);
        let ix = token_client::burn_with(
            *authority.key,
            *source.key,
            *mint.key,
            hook,
            extra_metas(side.extras),
            amount,
        );
        let mut infos = Vec::with_capacity(7 + side.extras.len());
        infos.push(authority.clone());
        infos.push(source.clone());
        infos.push(mint.clone());
        infos.push(hook_program);
        infos.push(hook_signer);
        infos.push(self.event_authority.clone());
        infos.push(self.program.clone());
        infos.extend(side.extras.iter().cloned());
        invoke_signed(&ix, &infos, signer_seeds).map_err(Into::into)
    }
}

/// A token hook's extra accounts as metas, with the writability they were given.
fn extra_metas(extras: &[AccountInfo]) -> Vec<AccountMeta> {
    extras
        .iter()
        .map(|a| AccountMeta {
            pubkey: *a.key,
            is_signer: false,
            is_writable: a.is_writable,
        })
        .collect()
}

/// Balance of a holding.
pub fn balance(info: &AccountInfo) -> Result<u64> {
    Ok(read_holding(info)?.amount)
}
