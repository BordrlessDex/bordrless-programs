//! The instructions this program invokes, built from keys it already holds. Nothing is derived
//! here: the `client` builders of each program derive the addresses they need, which on chain
//! costs a PDA search per address. Account order is each callee's `Accounts` struct, with its
//! event authority and program appended as `#[event_cpi]` does.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{system_program, InstructionData};
use bordrless_kit::KitInitArgs;
use bordrless_swap::instructions::CreatePoolArgs;
use bordrless_token::instructions::CreateMintArgs;
use bordrless_token::state::AuthorityKind;

use crate::constants::*;

fn ro(key: Pubkey) -> AccountMeta {
    AccountMeta::new_readonly(key, false)
}

fn rw(key: Pubkey) -> AccountMeta {
    AccountMeta::new(key, false)
}

/// The token program's event authority and the program, as `#[event_cpi]` appends them.
fn token_events(mut accounts: Vec<AccountMeta>) -> Vec<AccountMeta> {
    accounts.push(ro(TOKEN_EVENT_AUTHORITY));
    accounts.push(ro(bordrless_token::ID));
    accounts
}

/// A token-program optional account: the token program's id stands for none.
fn token_optional(key: Option<Pubkey>) -> AccountMeta {
    ro(key.unwrap_or(bordrless_token::ID))
}

/// A launch mint's token hook as the token instructions take it: the hook program and the token
/// program's signer of its callbacks (`["hook-authority", hook]` under the token program).
pub type TokenHook = (Pubkey, Pubkey);

/// The kit as a launch mint's token hook.
pub const KIT_HOOK: TokenHook = (KIT_ID, TOKEN_HOOK_AUTHORITY);

/// The two hook accounts of a token instruction on a launch's mint: the hook program and the
/// token program's signer for it when the mint has a hook (the kit, or the creator's own program
/// from a `LaunchConfig`), the token program's id for both when it has none.
fn token_hook(hook: Option<TokenHook>) -> [AccountMeta; 2] {
    [
        token_optional(hook.map(|h| h.0)),
        token_optional(hook.map(|h| h.1)),
    ]
}

/// Token `create_mint`: the mint signs (a keypair).
pub fn token_create_mint(payer: Pubkey, mint: Pubkey, args: CreateMintArgs) -> Instruction {
    Instruction {
        program_id: bordrless_token::ID,
        accounts: token_events(vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(mint, true),
            ro(system_program::ID),
        ]),
        data: bordrless_token::instruction::CreateMint { args }.data(),
    }
}

/// Token `create_holding` of `owner` for `mint` at `holding`.
pub fn token_create_holding(
    payer: Pubkey,
    mint: Pubkey,
    owner: Pubkey,
    holding: Pubkey,
) -> Instruction {
    Instruction {
        program_id: bordrless_token::ID,
        accounts: token_events(vec![
            AccountMeta::new(payer, true),
            ro(mint),
            ro(owner),
            rw(holding),
            ro(system_program::ID),
        ]),
        data: bordrless_token::instruction::CreateHolding {}.data(),
    }
}

/// Token `mint_to`, passing the mint's hook (`hook`) and its extra accounts (`extras`: none for
/// the kit, which does not subscribe to mints; a custom hook's registry extras).
pub fn token_mint_to(
    authority: Pubkey,
    mint: Pubkey,
    destination: Pubkey,
    hook: Option<TokenHook>,
    extras: &[AccountMeta],
    amount: u64,
) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new_readonly(authority, true),
        rw(mint),
        rw(destination),
    ];
    accounts.extend(token_hook(hook));
    let mut accounts = token_events(accounts);
    accounts.extend_from_slice(extras);
    Instruction {
        program_id: bordrless_token::ID,
        accounts,
        data: bordrless_token::instruction::MintTo { amount }.data(),
    }
}

/// Token `set_authority`.
pub fn token_set_authority(
    authority: Pubkey,
    mint: Pubkey,
    kind: AuthorityKind,
    new_authority: Option<Pubkey>,
) -> Instruction {
    Instruction {
        program_id: bordrless_token::ID,
        accounts: token_events(vec![AccountMeta::new_readonly(authority, true), rw(mint)]),
        data: bordrless_token::instruction::SetAuthority {
            kind,
            new_authority,
        }
        .data(),
    }
}

/// Token `transfer` on a launch's mint, with its hook (`hook`) and the hook's extra accounts when
/// it has one.
pub fn token_transfer(
    authority: Pubkey,
    source: Pubkey,
    destination: Pubkey,
    mint: Pubkey,
    hook: Option<TokenHook>,
    extras: &[AccountMeta],
    amount: u64,
) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new_readonly(authority, true),
        rw(source),
        rw(destination),
        ro(mint),
    ];
    accounts.extend(token_hook(hook));
    let mut accounts = token_events(accounts);
    accounts.extend_from_slice(extras);
    Instruction {
        program_id: bordrless_token::ID,
        accounts,
        data: bordrless_token::instruction::Transfer { amount }.data(),
    }
}

/// Token `burn` on a launch's mint, with its hook (`hook`) and the hook's extra accounts when it
/// has one.
pub fn token_burn(
    authority: Pubkey,
    source: Pubkey,
    mint: Pubkey,
    hook: Option<TokenHook>,
    extras: &[AccountMeta],
    amount: u64,
) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new_readonly(authority, true),
        rw(source),
        rw(mint),
    ];
    accounts.extend(token_hook(hook));
    let mut accounts = token_events(accounts);
    accounts.extend_from_slice(extras);
    Instruction {
        program_id: bordrless_token::ID,
        accounts,
        data: bordrless_token::instruction::Burn { amount }.data(),
    }
}

/// The keys of a launch pool's `create_pool`.
pub struct CreatePoolKeys {
    /// Pays the rent (the creator).
    pub payer: Pubkey,
    /// Deposits and receives the LP (the launch).
    pub authority: Pubkey,
    /// The DEX config's treasury.
    pub treasury: Pubkey,
    /// The token.
    pub base_mint: Pubkey,
    /// The quote.
    pub quote_mint: Pubkey,
    /// The pool PDA.
    pub pool: Pubkey,
    /// Its LP mint PDA.
    pub lp_mint: Pubkey,
    /// Its base vault.
    pub base_vault: Pubkey,
    /// Its quote vault.
    pub quote_vault: Pubkey,
    /// The launch's holding of the token (the deposit).
    pub authority_base: Pubkey,
    /// The launch's holding of the quote.
    pub authority_quote: Pubkey,
    /// The launch's holding of the LP mint, created by the DEX.
    pub authority_lp: Pubkey,
}

/// DEX `create_pool` with this program as the pool's hook and its hook authority signing, so
/// the DEX skips the initialize callbacks. `extras` are the base mint's token-hook slice (the
/// hook, the token program's signer for it, the hook's extras: the kit's two, or a custom hook's
/// registry extras; none without a hook).
pub fn dex_create_pool(
    keys: &CreatePoolKeys,
    args: CreatePoolArgs,
    extras: Vec<AccountMeta>,
) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new(keys.payer, true),
        AccountMeta::new_readonly(keys.authority, true),
        rw(DEX_CONFIG),
        rw(keys.treasury),
        ro(keys.base_mint),
        ro(keys.quote_mint),
        rw(keys.pool),
        rw(keys.lp_mint),
        rw(keys.base_vault),
        rw(keys.quote_vault),
        rw(keys.authority_base),
        rw(keys.authority_quote),
        rw(keys.authority_lp),
        ro(crate::ID),
        AccountMeta::new_readonly(LAUNCH_HOOK_AUTHORITY, true),
        ro(DEX_HOOK_AUTHORITY),
        ro(bordrless_token::ID),
        ro(TOKEN_EVENT_AUTHORITY),
        ro(system_program::ID),
        ro(DEX_EVENT_AUTHORITY),
        ro(bordrless_swap::ID),
    ];
    accounts.extend(extras);
    Instruction {
        program_id: bordrless_swap::ID,
        accounts,
        data: bordrless_swap::instruction::CreatePool { args }.data(),
    }
}

/// DEX `finalize_curve`, signed by this program's hook authority.
pub fn dex_finalize_curve(
    pool: Pubkey,
    base_vault: Pubkey,
    quote_vault: Pubkey,
    lp_mint: Pubkey,
    lp_recipient: Pubkey,
) -> Instruction {
    Instruction {
        program_id: bordrless_swap::ID,
        accounts: vec![
            AccountMeta::new_readonly(LAUNCH_HOOK_AUTHORITY, true),
            rw(pool),
            ro(base_vault),
            ro(quote_vault),
            rw(lp_mint),
            rw(lp_recipient),
            ro(bordrless_token::ID),
            ro(TOKEN_EVENT_AUTHORITY),
            ro(DEX_EVENT_AUTHORITY),
            ro(bordrless_swap::ID),
        ],
        data: bordrless_swap::instruction::FinalizeCurve {
            hook_caller_bump: HOOK_AUTHORITY_BUMP,
        }
        .data(),
    }
}

/// The keys of the kit's `init`.
pub struct KitInitKeys {
    /// `PDA(["kit-caller", mint], LAUNCH_ID)`: signs.
    pub kit_caller: Pubkey,
    /// Pays the rent (the creator).
    pub payer: Pubkey,
    /// The token.
    pub mint: Pubkey,
    /// The launch's holding of the token, holding the whole supply.
    pub launch_reserve: Pubkey,
    /// `PDA(["kit", mint], KIT_ID)`, created by the kit.
    pub kit_config: Pubkey,
    /// `PDA(["bordrless-hook-accounts", mint], KIT_ID)`, written by the kit.
    pub registry: Pubkey,
    /// The quote mint.
    pub reward_mint: Pubkey,
    /// The kit config's holding of the quote, with holder rewards.
    pub reward_vault: Option<Pubkey>,
}

/// The kit's `init`.
pub fn kit_init(keys: &KitInitKeys, args: KitInitArgs) -> Instruction {
    let reward_vault = match keys.reward_vault {
        Some(vault) => rw(vault),
        None => ro(KIT_ID),
    };
    Instruction {
        program_id: KIT_ID,
        accounts: vec![
            AccountMeta::new_readonly(keys.kit_caller, true),
            AccountMeta::new(keys.payer, true),
            ro(keys.mint),
            ro(keys.launch_reserve),
            rw(keys.kit_config),
            rw(keys.registry),
            ro(keys.reward_mint),
            reward_vault,
            ro(bordrless_token::ID),
            ro(TOKEN_EVENT_AUTHORITY),
            ro(system_program::ID),
            ro(KIT_EVENT_AUTHORITY),
            ro(KIT_ID),
        ],
        data: bordrless_kit::instruction::Init { args }.data(),
    }
}

/// The kit's `graduate`.
pub fn kit_graduate(kit_caller: Pubkey, kit_config: Pubkey) -> Instruction {
    Instruction {
        program_id: KIT_ID,
        accounts: vec![
            AccountMeta::new_readonly(kit_caller, true),
            rw(kit_config),
            ro(KIT_EVENT_AUTHORITY),
            ro(KIT_ID),
        ],
        data: bordrless_kit::instruction::Graduate {}.data(),
    }
}

/// The kit hook's extra accounts on every token operation of a kit mint (the registry's two
/// extras): the config (writable), then the reward vault (read-only) with holder rewards, or the
/// kit's id without.
pub fn kit_extras(kit_config: Pubkey, reward_vault: Option<Pubkey>) -> [AccountMeta; 2] {
    [rw(kit_config), ro(reward_vault.unwrap_or(KIT_ID))]
}

/// A kit mint's token-hook slice for a DEX instruction: the kit, the token program's signer of
/// its callbacks, then [`kit_extras`].
pub fn kit_slice(kit_config: Pubkey, reward_vault: Option<Pubkey>) -> Vec<AccountMeta> {
    let mut slice = vec![ro(KIT_ID), ro(TOKEN_HOOK_AUTHORITY)];
    slice.extend(kit_extras(kit_config, reward_vault));
    slice
}

/// A custom-hook mint's token-hook slice for a DEX instruction: the hook, the token program's
/// signer of its callbacks, then the hook's registry extras as the client resolved them.
pub fn custom_slice(hook: TokenHook, extras: &[AccountMeta]) -> Vec<AccountMeta> {
    let mut slice = vec![ro(hook.0), ro(hook.1)];
    slice.extend_from_slice(extras);
    slice
}
