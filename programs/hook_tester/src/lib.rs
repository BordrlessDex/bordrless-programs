//! `hook_tester`: a test-only hook program for the LiteSVM suites. It is never deployed (it is not
//! in `scripts/solana/deploy.sh` nor in the local validator's program list).
//!
//! - A token hook and a pool hook whose answers a test scripts per key (a mint for the token
//!   callbacks, a pool for the pool callbacks) and per callback: the [`Script`] PDA at
//!   `["script", key]` holds, for each callback, what to do (answer nothing, return given bytes as
//!   the return data, or fail) and records the arguments each callback was last told (so a test
//!   reads, for example, the recipient a swap's hook was told). The registry at
//!   `["bordrless-hook-accounts", key]` lists the script (writable) and then any accounts the test
//!   chose, so a scripted answer can name them as delta recipients. In a pool callback the
//!   registry's `Seed::Account(1)` is the pool, as in a token callback it is the mint.
//! - [`hook_tester::write_hook_data_as`] calls the token program's `write_hook_data` signed by any
//!   PDA of this program (its `["hook-authority"]`, another seed, a non-canonical bump) on any mint
//!   and holding, for the cross-mint attack tests.
//! - [`hook_tester::route`] performs a DEX swap by CPI, as a router would (for the CPI-depth tests).
//!   It cannot route a pool whose hook is this program: the DEX would call back into it, and a
//!   program cannot appear twice on the CPI stack.
//! - [`hook_tester::invoke_as_hook`] calls any program with this program's `["hook-authority"]`
//!   signing, as any deployed program can with its own: the fee-model test creates a curve pool on
//!   the DEX as its own hook and finalizes it, and shows it pays the flat rate, not the
//!   launchpad's share.
//! - [`hook_tester::probe_curve`] runs `Pubkey::is_on_curve` (the curve25519 validate-point
//!   syscall the kit's wallets-only rule uses) on given keys, or skips it, so a test measures its
//!   compute and checks it agrees with the host's curve arithmetic.
//! - A scripted callback can also pass on the signer it was given ([`mode::FORWARD`]): it calls
//!   another program with that signer as a signer, as a malicious hook would, so a test shows
//!   another hook (the kit, the launch's pool hook, `tax_hook`) refusing it.
//!
//! The answers are raw bytes, not a `HookReturn`, so a test can also return malformed data.

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::program::{invoke, invoke_signed, set_return_data};
use anchor_lang::InstructionData;
use bordrless_hook::{
    hook_accounts_address, write_registry, AccountSource, ExtraAccount, HookAccountList,
    PoolHookArgs, Seed, TokenHookArgs, HOOK_AUTHORITY_SEED,
};
use bordrless_swap::instructions::SwapArgs;
use bordrless_token::client as token_client;

pub mod client;

declare_id!("9xrQXAxhbRRCVfE5E7DFnxYHESmy4hrtcKpSvZUNmiWM");

/// `["script", key]`.
pub const SCRIPT_SEED: &[u8] = b"script";
/// Bytes allocated for a script (it is rewritten in place; Borsh only uses what it needs).
pub const SCRIPT_SPACE: usize = 10_000;
/// Longest scripted answer (a forwarded token callback is 8 + 364 bytes).
pub const MAX_ANSWER_LEN: usize = 512;
/// The token program's signer of every callback to this program: `["hook-authority",
/// hook_tester]` under the token program.
pub const TOKEN_HOOK_SIGNER: Pubkey =
    Pubkey::from_str_const("DQk365tAAtrmejMvdfpVgfxzJTBtxCdfmxEPkoaEZEoG");
/// The DEX's signer of every pool callback to this program: `["hook-authority", hook_tester]`
/// under the DEX.
pub const DEX_HOOK_SIGNER: Pubkey =
    Pubkey::from_str_const("8KDaHfzhTVX3mv8CsuBmtybBz8QrhUW8q8oZH1CYdVoF");
/// Most extra accounts a test may add to the registry after the script.
pub const MAX_EXTRAS: usize = 16;

/// Callback ids: which callback a scripted answer and a record belong to.
pub mod callback {
    /// `before_transfer`.
    pub const BEFORE_TRANSFER: u8 = 0;
    /// `after_transfer`.
    pub const AFTER_TRANSFER: u8 = 1;
    /// `before_mint`.
    pub const BEFORE_MINT: u8 = 2;
    /// `after_mint`.
    pub const AFTER_MINT: u8 = 3;
    /// `before_burn`.
    pub const BEFORE_BURN: u8 = 4;
    /// `after_burn`.
    pub const AFTER_BURN: u8 = 5;
    /// `before_initialize`.
    pub const BEFORE_INITIALIZE: u8 = 6;
    /// `after_initialize`.
    pub const AFTER_INITIALIZE: u8 = 7;
    /// `before_add_liquidity`.
    pub const BEFORE_ADD_LIQUIDITY: u8 = 8;
    /// `after_add_liquidity`.
    pub const AFTER_ADD_LIQUIDITY: u8 = 9;
    /// `before_remove_liquidity`.
    pub const BEFORE_REMOVE_LIQUIDITY: u8 = 10;
    /// `after_remove_liquidity`.
    pub const AFTER_REMOVE_LIQUIDITY: u8 = 11;
    /// `before_swap`.
    pub const BEFORE_SWAP: u8 = 12;
    /// `after_swap`.
    pub const AFTER_SWAP: u8 = 13;
    /// Number of callback ids.
    pub const COUNT: u8 = 14;
}

/// What a scripted callback does.
pub mod mode {
    /// Answers nothing (no return data).
    pub const NONE: u8 = 0;
    /// Returns the scripted bytes as its return data.
    pub const RETURN: u8 = 1;
    /// Fails with `TesterError::Refused`.
    pub const FAIL: u8 = 2;
    /// Passes on the signer the callback was given: calls the first of the callback's remaining
    /// accounts (after the script) as a program, with the scripted bytes as instruction data and
    /// as accounts the signer (a signer), then the remaining accounts after the program, as
    /// writable as they arrived. What a malicious hook would do with the calling program's
    /// signer.
    pub const FORWARD: u8 = 3;
    /// A honeypot (token callbacks only): the scripted bytes are a pool's owner key (32 bytes)
    /// and a launch's key (32 bytes); a transfer whose destination owner is the pool and whose
    /// source owner is not the launch is refused (`TesterError::Refused`), so a launch's deposit
    /// and every buy go through and every sell fails. What a creator's own hook can do to a
    /// launch, which the site must label.
    pub const HONEYPOT: u8 = 4;
}

/// Instructions of the test hook.
#[program]
pub mod hook_tester {
    use super::*;

    /// Creates the script of `key` (a mint, or a pool, which need not exist yet) and its registry:
    /// the script (writable), then `extras`. The payer becomes the script's authority. An address
    /// someone already funded is accepted for the registry.
    pub fn init_script(ctx: Context<InitScript>, extras: Vec<ExtraAccount>) -> Result<()> {
        require!(extras.len() <= MAX_EXTRAS, TesterError::BadParams);
        let key = ctx.accounts.key.key();
        let script = &mut ctx.accounts.script;
        script.version = 1;
        script.bump = ctx.bumps.script;
        script.authority = ctx.accounts.payer.key();
        script.key = key;
        script.calls = 0;
        script.answers = Vec::new();
        script.told = Vec::new();
        let (registry, bump) = hook_accounts_address(&crate::ID, &key);
        require_keys_eq!(
            ctx.accounts.registry.key(),
            registry,
            TesterError::WrongRegistry
        );
        let mut accounts = vec![ExtraAccount {
            writable: true,
            source: AccountSource::Pda {
                program: crate::ID,
                seeds: vec![Seed::Literal(SCRIPT_SEED.to_vec()), Seed::Account(1)],
            },
        }];
        accounts.extend(extras);
        write_registry(
            &ctx.accounts.payer.to_account_info(),
            &ctx.accounts.registry.to_account_info(),
            &ctx.accounts.system_program.to_account_info(),
            &crate::ID,
            &key,
            bump,
            &HookAccountList::new(accounts),
        )
    }

    /// Scripts what callback `which` ([`callback`]) does for the script's key: `action` ([`mode`])
    /// and, for `mode::RETURN`, the bytes it returns.
    pub fn set_answer(ctx: Context<SetAnswer>, which: u8, action: u8, data: Vec<u8>) -> Result<()> {
        require!(
            which < callback::COUNT && action <= mode::HONEYPOT && data.len() <= MAX_ANSWER_LEN,
            TesterError::BadParams
        );
        require!(
            action != mode::HONEYPOT || (data.len() == 64 && which <= callback::AFTER_BURN),
            TesterError::BadParams
        );
        let script = &mut ctx.accounts.script;
        match script.answers.iter_mut().find(|a| a.callback == which) {
            Some(answer) => {
                answer.mode = action;
                answer.data = data;
            }
            None => script.answers.push(ScriptedAnswer {
                callback: which,
                mode: action,
                data,
            }),
        }
        Ok(())
    }

    /// Token hook: `before_transfer`, as scripted.
    pub fn before_transfer<'info>(
        ctx: Context<'info, TokenCallback<'info>>,
        args: TokenHookArgs,
    ) -> Result<()> {
        run_token(ctx, callback::BEFORE_TRANSFER, &args)
    }

    /// Token hook: `after_transfer`, as scripted.
    pub fn after_transfer<'info>(
        ctx: Context<'info, TokenCallback<'info>>,
        args: TokenHookArgs,
    ) -> Result<()> {
        run_token(ctx, callback::AFTER_TRANSFER, &args)
    }

    /// Token hook: `before_mint`, as scripted.
    pub fn before_mint<'info>(
        ctx: Context<'info, TokenCallback<'info>>,
        args: TokenHookArgs,
    ) -> Result<()> {
        run_token(ctx, callback::BEFORE_MINT, &args)
    }

    /// Token hook: `after_mint`, as scripted.
    pub fn after_mint<'info>(
        ctx: Context<'info, TokenCallback<'info>>,
        args: TokenHookArgs,
    ) -> Result<()> {
        run_token(ctx, callback::AFTER_MINT, &args)
    }

    /// Token hook: `before_burn`, as scripted.
    pub fn before_burn<'info>(
        ctx: Context<'info, TokenCallback<'info>>,
        args: TokenHookArgs,
    ) -> Result<()> {
        run_token(ctx, callback::BEFORE_BURN, &args)
    }

    /// Token hook: `after_burn`, as scripted.
    pub fn after_burn<'info>(
        ctx: Context<'info, TokenCallback<'info>>,
        args: TokenHookArgs,
    ) -> Result<()> {
        run_token(ctx, callback::AFTER_BURN, &args)
    }

    /// Pool hook: `before_initialize`, as scripted.
    pub fn before_initialize<'info>(
        ctx: Context<'info, PoolCallback<'info>>,
        args: PoolHookArgs,
    ) -> Result<()> {
        run_pool(ctx, callback::BEFORE_INITIALIZE, &args)
    }

    /// Pool hook: `after_initialize`, as scripted.
    pub fn after_initialize<'info>(
        ctx: Context<'info, PoolCallback<'info>>,
        args: PoolHookArgs,
    ) -> Result<()> {
        run_pool(ctx, callback::AFTER_INITIALIZE, &args)
    }

    /// Pool hook: `before_add_liquidity`, as scripted.
    pub fn before_add_liquidity<'info>(
        ctx: Context<'info, PoolCallback<'info>>,
        args: PoolHookArgs,
    ) -> Result<()> {
        run_pool(ctx, callback::BEFORE_ADD_LIQUIDITY, &args)
    }

    /// Pool hook: `after_add_liquidity`, as scripted.
    pub fn after_add_liquidity<'info>(
        ctx: Context<'info, PoolCallback<'info>>,
        args: PoolHookArgs,
    ) -> Result<()> {
        run_pool(ctx, callback::AFTER_ADD_LIQUIDITY, &args)
    }

    /// Pool hook: `before_remove_liquidity`, as scripted.
    pub fn before_remove_liquidity<'info>(
        ctx: Context<'info, PoolCallback<'info>>,
        args: PoolHookArgs,
    ) -> Result<()> {
        run_pool(ctx, callback::BEFORE_REMOVE_LIQUIDITY, &args)
    }

    /// Pool hook: `after_remove_liquidity`, as scripted.
    pub fn after_remove_liquidity<'info>(
        ctx: Context<'info, PoolCallback<'info>>,
        args: PoolHookArgs,
    ) -> Result<()> {
        run_pool(ctx, callback::AFTER_REMOVE_LIQUIDITY, &args)
    }

    /// Pool hook: `before_swap`, as scripted (deltas and a burn from the input, an LP fee).
    pub fn before_swap<'info>(
        ctx: Context<'info, PoolCallback<'info>>,
        args: PoolHookArgs,
    ) -> Result<()> {
        run_pool(ctx, callback::BEFORE_SWAP, &args)
    }

    /// Pool hook: `after_swap`, as scripted (deltas and a burn from the output).
    pub fn after_swap<'info>(
        ctx: Context<'info, PoolCallback<'info>>,
        args: PoolHookArgs,
    ) -> Result<()> {
        run_pool(ctx, callback::AFTER_SWAP, &args)
    }

    /// A router: calls the DEX's `swap(args)` by CPI. The remaining accounts are the swap's
    /// accounts in its order (as `bordrless_swap::client::swap` lists them, its event authority,
    /// program and extras included), passed on with the signer and writable flags they arrived
    /// with, so the trader's signature reaches the DEX.
    pub fn route<'info>(ctx: Context<'info, Route<'info>>, args: SwapArgs) -> Result<()> {
        let ix = Instruction {
            program_id: bordrless_swap::ID,
            accounts: ctx
                .remaining_accounts
                .iter()
                .map(|info| AccountMeta {
                    pubkey: *info.key,
                    is_signer: info.is_signer,
                    is_writable: info.is_writable,
                })
                .collect(),
            data: bordrless_swap::instruction::Swap { args }.data(),
        };
        let mut infos = ctx.remaining_accounts.to_vec();
        infos.push(ctx.accounts.swap_program.to_account_info());
        invoke(&ix, &infos)?;
        Ok(())
    }

    /// Calls the first of the remaining accounts as a program with `data`, passing the rest on as
    /// they arrived, with this program's `["hook-authority"]` (at `bump`) signing wherever it
    /// appears among them. What any deployed program can do with its own hook authority: create a
    /// curve pool on the DEX as its own hook (`create_pool` with `hook_caller`), finalize it. The
    /// fee-model test shows such a pool pays the flat rate, not the launchpad's share.
    pub fn invoke_as_hook<'info>(
        ctx: Context<'info, InvokeAsHook<'info>>,
        bump: u8,
        data: Vec<u8>,
    ) -> Result<()> {
        let authority = Pubkey::create_program_address(&[HOOK_AUTHORITY_SEED, &[bump]], &crate::ID)
            .map_err(|_| TesterError::BadParams)?;
        require_keys_eq!(
            ctx.accounts.hook_authority.key(),
            authority,
            TesterError::BadParams
        );
        let (program, accounts) = ctx
            .remaining_accounts
            .split_first()
            .ok_or(TesterError::BadParams)?;
        let metas: Vec<AccountMeta> = accounts
            .iter()
            .map(|info| AccountMeta {
                pubkey: *info.key,
                is_signer: info.is_signer || *info.key == authority,
                is_writable: info.is_writable,
            })
            .collect();
        let mut infos = vec![ctx.accounts.hook_authority.to_account_info()];
        infos.extend(accounts.iter().cloned());
        infos.push(program.clone());
        let ix = Instruction {
            program_id: *program.key,
            accounts: metas,
            data,
        };
        invoke_signed(&ix, &infos, &[&[HOOK_AUTHORITY_SEED, &[bump]]])?;
        Ok(())
    }

    /// Answers, one byte per key, whether each key is on the ed25519 curve (`Pubkey::is_on_curve`,
    /// which compiles to the curve25519 validate-point syscall on chain). With `check` false it
    /// answers zeros without checking, so the difference in compute units measures the check.
    pub fn probe_curve(_ctx: Context<ProbeCurve>, keys: Vec<Pubkey>, check: bool) -> Result<()> {
        let answer: Vec<u8> = keys
            .iter()
            .map(|key| {
                if check {
                    u8::from(key.is_on_curve())
                } else {
                    0
                }
            })
            .collect();
        set_return_data(&answer);
        Ok(())
    }

    /// Calls the token program's `write_hook_data(data)` on `mint` and `holding`, signed by the PDA
    /// of this program that `seeds` (bump included) give.
    pub fn write_hook_data_as(
        ctx: Context<WriteHookDataAs>,
        seeds: Vec<Vec<u8>>,
        data: [u8; 64],
    ) -> Result<()> {
        let refs: Vec<&[u8]> = seeds.iter().map(Vec::as_slice).collect();
        let pda = Pubkey::create_program_address(&refs, &crate::ID)
            .map_err(|_| TesterError::BadParams)?;
        require_keys_eq!(ctx.accounts.signer.key(), pda, TesterError::BadParams);
        let ix = token_client::write_hook_data(
            pda,
            ctx.accounts.mint.key(),
            ctx.accounts.holding.key(),
            data,
        );
        invoke_signed(
            &ix,
            &[
                ctx.accounts.signer.to_account_info(),
                ctx.accounts.mint.to_account_info(),
                ctx.accounts.holding.to_account_info(),
                ctx.accounts.token_event_authority.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
            ],
            &[refs.as_slice()],
        )?;
        Ok(())
    }
}

/// Records what callback `which` was told, then does what the script says.
fn run_token<'info>(
    ctx: Context<'info, TokenCallback<'info>>,
    which: u8,
    args: &TokenHookArgs,
) -> Result<()> {
    let mut told = Vec::with_capacity(364);
    args.serialize(&mut told)?;
    // The honeypot: a transfer into the pool from anyone but the launch is refused.
    if let Some(answer) = ctx
        .accounts
        .script
        .answers
        .iter()
        .find(|a| a.callback == which && a.mode == mode::HONEYPOT)
    {
        let pool = Pubkey::try_from(&answer.data[..32]).map_err(|_| TesterError::BadParams)?;
        let launch = Pubkey::try_from(&answer.data[32..]).map_err(|_| TesterError::BadParams)?;
        require!(
            !(args.destination_owner == pool && args.source_owner != launch),
            TesterError::Refused
        );
    }
    if let Some(data) = run(&mut ctx.accounts.script, which, told)? {
        forward(
            &ctx.accounts.hook_signer.to_account_info(),
            ctx.remaining_accounts,
            data,
        )?;
    }
    Ok(())
}

/// Records what pool callback `which` was told, then does what the script says.
fn run_pool<'info>(
    ctx: Context<'info, PoolCallback<'info>>,
    which: u8,
    args: &PoolHookArgs,
) -> Result<()> {
    let mut told = Vec::with_capacity(256 + args.hook_data.len());
    args.serialize(&mut told)?;
    if let Some(data) = run(&mut ctx.accounts.script, which, told)? {
        forward(
            &ctx.accounts.hook_signer.to_account_info(),
            ctx.remaining_accounts,
            data,
        )?;
    }
    Ok(())
}

/// Records the arguments and does what the script says; answers the instruction data to
/// forward for `mode::FORWARD`.
fn run(script: &mut Script, which: u8, told: Vec<u8>) -> Result<Option<Vec<u8>>> {
    script.calls = script.calls.checked_add(1).ok_or(TesterError::Overflow)?;
    match script.told.iter_mut().find(|t| t.callback == which) {
        Some(record) => record.args = told,
        None => script.told.push(Told {
            callback: which,
            args: told,
        }),
    }
    match script.answers.iter().find(|a| a.callback == which) {
        Some(answer) if answer.mode == mode::RETURN => set_return_data(&answer.data),
        Some(answer) if answer.mode == mode::FAIL => return err!(TesterError::Refused),
        Some(answer) if answer.mode == mode::FORWARD => return Ok(Some(answer.data.clone())),
        _ => {}
    }
    Ok(None)
}

/// `mode::FORWARD`: calls the first of `remaining` as a program with `data`, passing on `signer`
/// (a signer of this callback) as a signer, then the rest of `remaining` as they arrived.
fn forward<'info>(
    signer: &AccountInfo<'info>,
    remaining: &[AccountInfo<'info>],
    data: Vec<u8>,
) -> Result<()> {
    let (program, accounts) = remaining.split_first().ok_or(TesterError::BadParams)?;
    let mut metas = vec![AccountMeta::new_readonly(*signer.key, true)];
    metas.extend(accounts.iter().map(|info| AccountMeta {
        pubkey: *info.key,
        is_signer: false,
        is_writable: info.is_writable,
    }));
    let mut infos = vec![signer.clone()];
    infos.extend(accounts.iter().cloned());
    infos.push(program.clone());
    let ix = Instruction {
        program_id: *program.key,
        accounts: metas,
        data,
    };
    invoke(&ix, &infos)?;
    Ok(())
}

/// The script of one key (a mint, or a pool), at `["script", key]`.
#[account]
#[derive(Debug)]
pub struct Script {
    /// Layout version.
    pub version: u8,
    /// Bump.
    pub bump: u8,
    /// May change the answers.
    pub authority: Pubkey,
    /// The mint or pool.
    pub key: Pubkey,
    /// Callbacks run so far.
    pub calls: u64,
    /// What each scripted callback does.
    pub answers: Vec<ScriptedAnswer>,
    /// The Borsh arguments each callback was last told (`TokenHookArgs` for token callbacks,
    /// `PoolHookArgs` for pool callbacks).
    pub told: Vec<Told>,
}

/// What one callback does.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct ScriptedAnswer {
    /// The callback ([`callback`]).
    pub callback: u8,
    /// [`mode`].
    pub mode: u8,
    /// The return data for `mode::RETURN`.
    pub data: Vec<u8>,
}

/// What one callback was last told.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct Told {
    /// The callback ([`callback`]).
    pub callback: u8,
    /// Its Borsh arguments.
    pub args: Vec<u8>,
}

impl Script {
    /// The arguments callback `which` was last told, if it ran.
    pub fn told_args(&self, which: u8) -> Option<&[u8]> {
        self.told
            .iter()
            .find(|t| t.callback == which)
            .map(|t| t.args.as_slice())
    }

    /// The token arguments callback `which` was last told, if it ran.
    pub fn told_token(&self, which: u8) -> Option<TokenHookArgs> {
        self.told_args(which)
            .and_then(|mut bytes| TokenHookArgs::deserialize(&mut bytes).ok())
    }

    /// The pool arguments callback `which` was last told, if it ran.
    pub fn told_pool(&self, which: u8) -> Option<PoolHookArgs> {
        self.told_args(which)
            .and_then(|mut bytes| PoolHookArgs::deserialize(&mut bytes).ok())
    }
}

/// Accounts of `init_script`.
#[derive(Accounts)]
pub struct InitScript<'info> {
    /// Pays the rent; becomes the script's authority.
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: the mint (or pool) the script is for; any key.
    pub key: UncheckedAccount<'info>,
    #[account(init, payer = payer, space = SCRIPT_SPACE, seeds = [SCRIPT_SEED, key.key().as_ref()], bump)]
    pub script: Account<'info, Script>,
    /// CHECK: the registry PDA of `key` (checked in the handler), created here.
    #[account(mut)]
    pub registry: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Accounts of `set_answer`.
#[derive(Accounts)]
pub struct SetAnswer<'info> {
    /// The script's authority.
    pub authority: Signer<'info>,
    #[account(mut, has_one = authority @ TesterError::BadParams)]
    pub script: Account<'info, Script>,
}

/// Accounts of a token callback: the prefix, then the registry's extras (the script first).
#[derive(Accounts)]
pub struct TokenCallback<'info> {
    /// CHECK: the token program's signer for this hook (address- and signer-checked).
    #[account(signer, address = TOKEN_HOOK_SIGNER @ TesterError::BadHookSigner)]
    pub hook_signer: UncheckedAccount<'info>,
    /// CHECK: the mint.
    pub mint: UncheckedAccount<'info>,
    /// CHECK: the source holding (the mint for a mint).
    pub source: UncheckedAccount<'info>,
    /// CHECK: the destination holding (the mint for a burn).
    pub destination: UncheckedAccount<'info>,
    /// CHECK: the authority.
    pub authority: UncheckedAccount<'info>,
    #[account(mut, seeds = [SCRIPT_SEED, mint.key().as_ref()], bump = script.bump)]
    pub script: Account<'info, Script>,
}

/// Accounts of a pool callback: the DEX's prefix, then the registry's extras (the script first).
#[derive(Accounts)]
pub struct PoolCallback<'info> {
    /// CHECK: the DEX's signer for this hook (address- and signer-checked).
    #[account(signer, address = DEX_HOOK_SIGNER @ TesterError::BadHookSigner)]
    pub hook_signer: UncheckedAccount<'info>,
    /// CHECK: the pool.
    pub pool: UncheckedAccount<'info>,
    /// CHECK: the base mint.
    pub base_mint: UncheckedAccount<'info>,
    /// CHECK: the quote mint.
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: the trader or liquidity provider.
    pub actor: UncheckedAccount<'info>,
    #[account(mut, seeds = [SCRIPT_SEED, pool.key().as_ref()], bump = script.bump)]
    pub script: Account<'info, Script>,
}

/// Accounts of `route`: the DEX, then (as remaining accounts) the swap's own accounts.
#[derive(Accounts)]
pub struct Route<'info> {
    /// CHECK: the DEX (address-checked).
    #[account(address = bordrless_swap::ID @ TesterError::BadParams)]
    pub swap_program: UncheckedAccount<'info>,
}

/// Accounts of `invoke_as_hook`: this program's hook authority (checked in the handler), then (as
/// remaining accounts) the program to call and its accounts.
#[derive(Accounts)]
pub struct InvokeAsHook<'info> {
    /// CHECK: `["hook-authority", bump]` of this program (checked in the handler); signs the CPI.
    pub hook_authority: UncheckedAccount<'info>,
}

/// Accounts of `probe_curve`: none.
#[derive(Accounts)]
pub struct ProbeCurve {}

/// Accounts of `write_hook_data_as`.
#[derive(Accounts)]
pub struct WriteHookDataAs<'info> {
    /// CHECK: the PDA of this program the seeds give (checked in the handler); signs the CPI.
    pub signer: UncheckedAccount<'info>,
    /// CHECK: any mint (the token program checks it).
    pub mint: UncheckedAccount<'info>,
    /// CHECK: any holding (the token program checks it).
    #[account(mut)]
    pub holding: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = bordrless_token::ID @ TesterError::BadParams)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority.
    #[account(address = token_client::event_authority() @ TesterError::BadParams)]
    pub token_event_authority: UncheckedAccount<'info>,
}

/// Errors.
#[error_code]
pub enum TesterError {
    #[msg("bad parameters")]
    BadParams,
    #[msg("the hook signer is not the calling program's signer for this hook")]
    BadHookSigner,
    #[msg("the registry passed is not this key's")]
    WrongRegistry,
    #[msg("the scripted callback refused")]
    Refused,
    #[msg("math overflow")]
    Overflow,
}
