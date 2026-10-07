//! Hook CPIs of the token standard, the reading of a hook's answer and the application of its
//! deltas.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{instruction::Instruction, program::invoke_signed};
use bordrless_hook::{
    clear_return_data, hook_signer_at, read_answer, Allowed, Answer, Delta, TokenHookArgs,
    HOOK_AUTHORITY_SEED, TOKEN_PREFIX_ACCOUNTS,
};

use crate::client::read_holding;
use crate::error::{answer_error, TokenError};
use crate::events::DeltaApplied;
use crate::state::{Holding, Mint};

/// The accounts a token hook CPI is built from. Owned `AccountInfo`s (cheap clones), so the
/// handler can go on mutating its Anchor accounts while the call is alive.
pub struct HookCall<'a, 'info> {
    /// The hook program.
    pub hook_program: AccountInfo<'info>,
    /// This program's signer of the hook's callbacks, `["hook-authority", hook_program]`.
    pub hook_signer: AccountInfo<'info>,
    /// Bump of `hook_signer`.
    pub bump: u8,
    /// `mint`, `source`, `destination`, `authority` (the mint stands in for a missing side).
    pub prefix: [AccountInfo<'info>; 4],
    /// The hook's extra accounts (the instruction's remaining accounts).
    pub extras: &'a [AccountInfo<'info>],
}

impl<'a, 'info> HookCall<'a, 'info> {
    /// The call to `mint`'s hook, when it has one: the hook program passed must be the mint's
    /// (`HookProgramMissing`, `WrongHookProgram`), and the hook signer this program's
    /// `["hook-authority", hook_program]` at the mint's bump (`BadHookSigner`). One signer per
    /// hook program: a hook that passes on the signer it receives can only vouch for itself.
    pub fn of(
        mint: &Mint,
        hook_program: Option<AccountInfo<'info>>,
        hook_signer: Option<AccountInfo<'info>>,
        prefix: [AccountInfo<'info>; 4],
        extras: &'a [AccountInfo<'info>],
    ) -> Result<Option<Self>> {
        let Some(program) = mint.hook_program else {
            return Ok(None);
        };
        let info = hook_program.ok_or(TokenError::HookProgramMissing)?;
        require_keys_eq!(info.key(), program, TokenError::WrongHookProgram);
        let signer = hook_signer.ok_or(TokenError::BadHookSigner)?;
        let expected = hook_signer_at(&crate::ID, &program, mint.hook_signer_bump)
            .ok_or(TokenError::BadHookSigner)?;
        require_keys_eq!(signer.key(), expected, TokenError::BadHookSigner);
        Ok(Some(Self {
            hook_program: info,
            hook_signer: signer,
            bump: mint.hook_signer_bump,
            prefix,
            extras,
        }))
    }

    /// Invokes one callback. Return data is cleared first so only the hook's own answer is read.
    pub fn invoke(&self, discriminator: [u8; 8], args: &TokenHookArgs) -> Result<()> {
        let mut data = Vec::with_capacity(8 + 364);
        data.extend_from_slice(&discriminator);
        args.serialize(&mut data)?;
        let mut metas = Vec::with_capacity(TOKEN_PREFIX_ACCOUNTS + self.extras.len());
        metas.push(AccountMeta::new_readonly(*self.hook_signer.key, true));
        for info in &self.prefix {
            metas.push(AccountMeta::new_readonly(*info.key, false));
        }
        let mut infos = Vec::with_capacity(TOKEN_PREFIX_ACCOUNTS + self.extras.len() + 1);
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
        )
        .map_err(Into::into)
    }

    /// Invokes a `before_*` callback and reads its answer, when `allowed` (the callback and the
    /// mint's flags) lets it answer anything. The answer is checked by the protocol's rules: no
    /// field outside `allowed`, at most three deltas, each above zero, no account index twice, sums
    /// in checked arithmetic. Answers it with the sum of its deltas.
    pub fn invoke_for_answer(
        &self,
        discriminator: [u8; 8],
        args: &TokenHookArgs,
        allowed: Allowed,
    ) -> Result<Option<Answer>> {
        self.invoke(discriminator, args)?;
        read_answer(self.hook_program.key, allowed).map_err(answer_error)
    }
}

/// Credits each delta to the extra account it names (an index over prefix then extras). Every
/// target must be an extra that is a writable holding of `mint`, not frozen, none of `forbidden`
/// (the holdings the instruction itself manages) and named once, else `InvalidDeltaAccount`. All
/// are checked before any is written. Answers what was applied, in order.
pub fn apply_deltas(
    extras: &[AccountInfo],
    deltas: &[Delta],
    mint: &Pubkey,
    forbidden: &[Pubkey],
) -> Result<Vec<DeltaApplied>> {
    let mut targets: Vec<(&AccountInfo, Holding, u64)> = Vec::with_capacity(deltas.len());
    for delta in deltas {
        let i = usize::from(delta.account);
        require!(i >= TOKEN_PREFIX_ACCOUNTS, TokenError::InvalidDeltaAccount);
        let info = extras
            .get(i - TOKEN_PREFIX_ACCOUNTS)
            .ok_or(TokenError::InvalidDeltaAccount)?;
        require!(info.is_writable, TokenError::InvalidDeltaAccount);
        require!(
            !forbidden.contains(info.key),
            TokenError::InvalidDeltaAccount
        );
        require!(
            !targets.iter().any(|(t, _, _)| t.key == info.key),
            TokenError::InvalidDeltaAccount
        );
        let holding = read_holding(info).map_err(|_| TokenError::InvalidDeltaAccount)?;
        require_keys_eq!(holding.mint, *mint, TokenError::InvalidDeltaAccount);
        require!(!holding.frozen, TokenError::InvalidDeltaAccount);
        targets.push((info, holding, delta.amount));
    }
    let mut applied = Vec::with_capacity(targets.len());
    for (info, mut holding, amount) in targets {
        holding.amount = holding
            .amount
            .checked_add(amount)
            .ok_or(TokenError::MathOverflow)?;
        let mut data = info.try_borrow_mut_data()?;
        let mut cursor: &mut [u8] = &mut data;
        holding.try_serialize(&mut cursor)?;
        applied.push(DeltaApplied {
            holding: *info.key,
            owner: holding.owner,
            amount,
            post: holding.amount,
        });
    }
    Ok(applied)
}
