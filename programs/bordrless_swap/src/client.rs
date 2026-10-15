//! Instruction builders for calling the DEX: used by the launchpad (CPI), the tests and as the
//! reference for the TypeScript SDK. Account order is that of each `Accounts` struct, with the
//! event authority and the program appended.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{system_program, InstructionData};
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::instructions::{
    AddLiquidityArgs, ConfigArgs, CreatePoolArgs, RemoveLiquidityArgs, SwapArgs,
};
use crate::state::Pool;

/// This program's signer of every callback to the pool hook `hook_program`: `["hook-authority",
/// hook_program]`. One per hook program, so a hook accepts only its own.
pub fn hook_signer(hook_program: &Pubkey) -> Pubkey {
    Pool::hook_signer(hook_program).0
}

/// The pool hook signer of an instruction: the hook's signer, or this program's id (Anchor's
/// `None`) for a pool without a hook.
fn hook_signer_meta(hook_program: Option<Pubkey>) -> AccountMeta {
    optional(hook_program.map(|p| hook_signer(&p)))
}

/// A mint's token-hook slice for a DEX instruction: its hook program, the token program's signer
/// of its callbacks, then `extras` (the hook's accounts, resolved from its registry); empty for a
/// mint without a hook.
pub fn token_hook_slice(
    hook_program: Option<Pubkey>,
    extras: Vec<AccountMeta>,
) -> Vec<AccountMeta> {
    match hook_program {
        Some(program) => {
            let mut slice = vec![
                AccountMeta::new_readonly(program, false),
                AccountMeta::new_readonly(token_client::hook_signer(&program), false),
            ];
            slice.extend(extras);
            slice
        }
        None => vec![],
    }
}

/// This program's event authority.
pub fn event_authority() -> Pubkey {
    crate::EVENT_AUTHORITY_AND_BUMP.0
}

/// `["config"]`.
pub fn config_address() -> Pubkey {
    Pubkey::create_program_address(&[CONFIG_SEED, &[CONFIG_BUMP]], &crate::ID).expect("config bump")
}

/// The pool of a pair with a fee and a hook.
pub fn pool_address(
    base_mint: &Pubkey,
    quote_mint: &Pubkey,
    lp_fee_bps: u16,
    hook_program: Option<Pubkey>,
) -> Pubkey {
    Pool::address(base_mint, quote_mint, lp_fee_bps, hook_program).0
}

/// The LP mint of a pool.
pub fn lp_mint_address(pool: &Pubkey) -> Pubkey {
    Pool::lp_mint_address(pool).0
}

/// The vault of a pool for `mint`: its holding.
pub fn vault_address(pool: &Pubkey, mint: &Pubkey) -> Pubkey {
    token_client::holding_address(mint, pool)
}

/// The ProgramData account of this program.
pub fn program_data_address() -> Pubkey {
    Pubkey::find_program_address(&[crate::ID.as_ref()], &BPF_LOADER_UPGRADEABLE_ID).0
}

fn with_events(mut accounts: Vec<AccountMeta>) -> Vec<AccountMeta> {
    accounts.push(AccountMeta::new_readonly(event_authority(), false));
    accounts.push(AccountMeta::new_readonly(crate::ID, false));
    accounts
}

fn optional(key: Option<Pubkey>) -> AccountMeta {
    AccountMeta::new_readonly(key.unwrap_or(crate::ID), false)
}

fn optional_signer(key: Option<Pubkey>) -> AccountMeta {
    match key {
        Some(k) => AccountMeta::new_readonly(k, true),
        None => AccountMeta::new_readonly(crate::ID, false),
    }
}

/// The token program's fixed accounts, in the order every instruction here takes them (its
/// signer of a mint's hook travels in that mint's token-hook slice).
fn token_fixed() -> [AccountMeta; 2] {
    [
        AccountMeta::new_readonly(bordrless_token::ID, false),
        AccountMeta::new_readonly(token_client::event_authority(), false),
    ]
}

/// `init_config`.
pub fn init_config(authority: Pubkey, args: ConfigArgs) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new(authority, true),
            AccountMeta::new(config_address(), false),
            AccountMeta::new_readonly(program_data_address(), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ]),
        data: crate::instruction::InitConfig { args }.data(),
    }
}

/// `set_config`.
pub fn set_config(admin: Pubkey, args: ConfigArgs) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new_readonly(admin, true),
            AccountMeta::new(config_address(), false),
        ]),
        data: crate::instruction::SetConfig { args }.data(),
    }
}

/// The keys of `create_pool` that the caller chooses.
pub struct CreatePoolKeys {
    /// Pays rent and the creation fee.
    pub payer: Pubkey,
    /// Owns the deposit holdings, receives the LP.
    pub authority: Pubkey,
    /// The config's treasury.
    pub treasury: Pubkey,
    /// Base mint.
    pub base_mint: Pubkey,
    /// Quote mint.
    pub quote_mint: Pubkey,
    /// The hook program's hook authority, when the hook creates the pool.
    pub hook_caller: Option<Pubkey>,
}

/// `create_pool`. `extras` are the remaining accounts (base hook, quote hook, pool hook).
pub fn create_pool(
    keys: &CreatePoolKeys,
    args: CreatePoolArgs,
    extras: Vec<AccountMeta>,
) -> Instruction {
    let hook_program = if args.hook_program == Pubkey::default() {
        None
    } else {
        Some(args.hook_program)
    };
    let pool = pool_address(
        &keys.base_mint,
        &keys.quote_mint,
        args.lp_fee_bps,
        hook_program,
    );
    let lp_mint = lp_mint_address(&pool);
    let mut accounts = vec![
        AccountMeta::new(keys.payer, true),
        AccountMeta::new_readonly(keys.authority, true),
        AccountMeta::new(config_address(), false),
        AccountMeta::new(keys.treasury, false),
        AccountMeta::new_readonly(keys.base_mint, false),
        AccountMeta::new_readonly(keys.quote_mint, false),
        AccountMeta::new(pool, false),
        AccountMeta::new(lp_mint, false),
        AccountMeta::new(vault_address(&pool, &keys.base_mint), false),
        AccountMeta::new(vault_address(&pool, &keys.quote_mint), false),
        AccountMeta::new(
            token_client::holding_address(&keys.base_mint, &keys.authority),
            false,
        ),
        AccountMeta::new(
            token_client::holding_address(&keys.quote_mint, &keys.authority),
            false,
        ),
        AccountMeta::new(
            token_client::holding_address(&lp_mint, &keys.authority),
            false,
        ),
        optional(hook_program),
        optional_signer(keys.hook_caller),
        hook_signer_meta(hook_program),
    ];
    accounts.extend(token_fixed());
    accounts.push(AccountMeta::new_readonly(system_program::ID, false));
    let mut accounts = with_events(accounts);
    accounts.extend(extras);
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::CreatePool { args }.data(),
    }
}

/// The fixed keys of a swap.
pub struct SwapKeys {
    /// The trader.
    pub trader: Pubkey,
    /// The pool.
    pub pool: Pubkey,
    /// Base mint.
    pub base_mint: Pubkey,
    /// Quote mint.
    pub quote_mint: Pubkey,
    /// The trader's base holding (or, on a buy, any holding of the base mint to deliver to).
    pub trader_base: Pubkey,
    /// The trader's quote holding (or, on a sell, any holding of the quote mint to deliver to).
    pub trader_quote: Pubkey,
    /// The pool's hook program.
    pub hook_program: Option<Pubkey>,
    /// Pass the base mint writable: only when the pool's hook may burn base (a launch with a burn
    /// rate). A transfer never needs it, and a burn without it fails with `MintNotWritable`.
    pub base_mint_writable: bool,
    /// Pass the quote mint writable: only when the pool's hook may burn quote (never bridged SOL).
    pub quote_mint_writable: bool,
}

/// A mint, writable only when asked.
fn mint_meta(mint: Pubkey, writable: bool) -> AccountMeta {
    if writable {
        AccountMeta::new(mint, false)
    } else {
        AccountMeta::new_readonly(mint, false)
    }
}

/// `swap`. `extras` are the remaining accounts: the input mint's token-hook slice, the output
/// mint's (each its program, the token program's signer for it, then its accounts:
/// [`token_hook_slice`]; `args` gives their lengths), then the pool hook's extras.
pub fn swap(keys: &SwapKeys, args: SwapArgs, extras: Vec<AccountMeta>) -> Instruction {
    let mut accounts = vec![
        // Writable: on a native pool an old-path sell the buffer can't cover is paid to the
        // trader as lamports.
        AccountMeta::new(keys.trader, true),
        AccountMeta::new_readonly(config_address(), false),
        AccountMeta::new(keys.pool, false),
        mint_meta(keys.base_mint, keys.base_mint_writable),
        mint_meta(keys.quote_mint, keys.quote_mint_writable),
        AccountMeta::new(vault_address(&keys.pool, &keys.base_mint), false),
        AccountMeta::new(vault_address(&keys.pool, &keys.quote_mint), false),
        AccountMeta::new(keys.trader_base, false),
        AccountMeta::new(keys.trader_quote, false),
        optional(keys.hook_program),
        hook_signer_meta(keys.hook_program),
    ];
    accounts.extend(token_fixed());
    let mut accounts = with_events(accounts);
    accounts.extend(extras);
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::Swap { args }.data(),
    }
}

/// The fixed keys of a liquidity change.
pub struct LiquidityKeys {
    /// The provider.
    pub provider: Pubkey,
    /// The pool.
    pub pool: Pubkey,
    /// Base mint.
    pub base_mint: Pubkey,
    /// Quote mint.
    pub quote_mint: Pubkey,
    /// The pool's hook program.
    pub hook_program: Option<Pubkey>,
}

fn liquidity_accounts(keys: &LiquidityKeys) -> Vec<AccountMeta> {
    let lp_mint = lp_mint_address(&keys.pool);
    let mut accounts = vec![
        // Writable: a native pool pays a withdrawal its buffer can't cover as lamports.
        AccountMeta::new(keys.provider, true),
        AccountMeta::new_readonly(config_address(), false),
        AccountMeta::new(keys.pool, false),
        AccountMeta::new_readonly(keys.base_mint, false),
        AccountMeta::new_readonly(keys.quote_mint, false),
        AccountMeta::new(lp_mint, false),
        AccountMeta::new(vault_address(&keys.pool, &keys.base_mint), false),
        AccountMeta::new(vault_address(&keys.pool, &keys.quote_mint), false),
        AccountMeta::new(
            token_client::holding_address(&keys.base_mint, &keys.provider),
            false,
        ),
        AccountMeta::new(
            token_client::holding_address(&keys.quote_mint, &keys.provider),
            false,
        ),
        AccountMeta::new(
            token_client::holding_address(&lp_mint, &keys.provider),
            false,
        ),
        optional(keys.hook_program),
        hook_signer_meta(keys.hook_program),
    ];
    accounts.extend(token_fixed());
    with_events(accounts)
}

/// `add_liquidity`.
pub fn add_liquidity(
    keys: &LiquidityKeys,
    args: AddLiquidityArgs,
    extras: Vec<AccountMeta>,
) -> Instruction {
    let mut accounts = liquidity_accounts(keys);
    accounts.extend(extras);
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::AddLiquidity { args }.data(),
    }
}

/// `remove_liquidity`.
pub fn remove_liquidity(
    keys: &LiquidityKeys,
    args: RemoveLiquidityArgs,
    extras: Vec<AccountMeta>,
) -> Instruction {
    let mut accounts = liquidity_accounts(keys);
    accounts.extend(extras);
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::RemoveLiquidity { args }.data(),
    }
}

/// `finalize_curve`, called by the hook program's hook authority.
pub fn finalize_curve(
    hook_caller: Pubkey,
    hook_caller_bump: u8,
    pool: Pubkey,
    base_mint: Pubkey,
    quote_mint: Pubkey,
    lp_recipient: Pubkey,
) -> Instruction {
    let lp_mint = lp_mint_address(&pool);
    let mut accounts = vec![
        AccountMeta::new_readonly(hook_caller, true),
        AccountMeta::new(pool, false),
        AccountMeta::new_readonly(vault_address(&pool, &base_mint), false),
        AccountMeta::new_readonly(vault_address(&pool, &quote_mint), false),
        AccountMeta::new(lp_mint, false),
        AccountMeta::new(lp_recipient, false),
    ];
    accounts.extend(token_fixed());
    Instruction {
        program_id: crate::ID,
        accounts: with_events(accounts),
        data: crate::instruction::FinalizeCurve { hook_caller_bump }.data(),
    }
}

/// The bridge's accounts of `wrap_sol` and `unwrap_sol` that a DEX instruction passes after the
/// pool's: the bridge, its config, the SOL wrapper, the SOL vault (bridged SOL and the event
/// authority are placed by each builder).
fn bridge_keys() -> (Pubkey, Pubkey, Pubkey, Pubkey, Pubkey) {
    use crate::constants::BRIDGE_ID;
    let native = Pubkey::from_str_const("So11111111111111111111111111111111111111112");
    let bridge = |seeds: &[&[u8]]| Pubkey::find_program_address(seeds, &BRIDGE_ID).0;
    (
        BRIDGE_ID,
        bridge(&[b"config"]),
        bridge(&[b"wrapper", native.as_ref()]),
        bridge(&[b"sol-vault"]),
        bridge(&[b"__event_authority"]),
    )
}

/// `create_native_pool`: as [`create_pool`] (the quote mint must be bridged SOL; the payer pays
/// the quote deposit in SOL; the authority's quote holding is passed but unused).
pub fn create_native_pool(
    keys: &CreatePoolKeys,
    args: CreatePoolArgs,
    extras: Vec<AccountMeta>,
) -> Instruction {
    let mut ix = create_pool(keys, args.clone(), extras);
    ix.data = crate::instruction::CreateNativePool { args }.data();
    ix
}

/// The fixed keys of a `swap_v2`.
pub struct SwapV2Keys {
    /// The trader (pays a buy's SOL).
    pub trader: Pubkey,
    /// The pool (a native one).
    pub pool: Pubkey,
    /// Base mint.
    pub base_mint: Pubkey,
    /// The trader's base holding (or, on a buy, any holding of the base mint to deliver to).
    pub trader_base: Pubkey,
    /// On a buy the trader; on a sell any account to deliver the SOL to.
    pub trader_quote: Pubkey,
    /// The pool's hook program.
    pub hook_program: Option<Pubkey>,
    /// Pass the base mint writable (a hook may burn base).
    pub base_mint_writable: bool,
    /// Pass the bridge's accounts (and bridged SOL writable): needed when a quote-side cut names a
    /// holding of bridged SOL, which is paid wrapped.
    pub with_bridge: bool,
}

/// `swap_v2`. `extras` as for [`swap`].
pub fn swap_v2(keys: &SwapV2Keys, args: SwapArgs, extras: Vec<AccountMeta>) -> Instruction {
    use crate::constants::BRIDGED_SOL_MINT;
    let (bridge, bridge_config, wrapper, sol_vault, bridge_events) = bridge_keys();
    let mut accounts = vec![
        AccountMeta::new(keys.trader, true),
        AccountMeta::new_readonly(config_address(), false),
        AccountMeta::new(keys.pool, false),
        mint_meta(keys.base_mint, keys.base_mint_writable),
        mint_meta(BRIDGED_SOL_MINT, keys.with_bridge),
        AccountMeta::new(vault_address(&keys.pool, &keys.base_mint), false),
        AccountMeta::new(keys.trader_base, false),
        AccountMeta::new(keys.trader_quote, false),
        optional(keys.hook_program),
        hook_signer_meta(keys.hook_program),
    ];
    accounts.extend(token_fixed());
    accounts.push(AccountMeta::new_readonly(system_program::ID, false));
    if keys.with_bridge {
        accounts.extend([
            AccountMeta::new_readonly(bridge, false),
            AccountMeta::new_readonly(bridge_config, false),
            AccountMeta::new(wrapper, false),
            AccountMeta::new(sol_vault, false),
            AccountMeta::new_readonly(bridge_events, false),
        ]);
    } else {
        accounts.extend((0..5).map(|_| optional(None)));
    }
    let mut accounts = with_events(accounts);
    accounts.extend(extras);
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::SwapV2 { args }.data(),
    }
}

/// The accounts of `migrate_pool_to_native`, `revert_pool_to_bridged`, `rebalance_pool` and
/// `set_buffer_target`.
fn quote_form_accounts(signer: Pubkey, pool: Pubkey) -> Vec<AccountMeta> {
    use crate::constants::BRIDGED_SOL_MINT;
    let (bridge, bridge_config, wrapper, sol_vault, bridge_events) = bridge_keys();
    with_events(vec![
        AccountMeta::new(signer, true),
        AccountMeta::new_readonly(config_address(), false),
        AccountMeta::new(pool, false),
        AccountMeta::new(vault_address(&pool, &BRIDGED_SOL_MINT), false),
        AccountMeta::new_readonly(bridge, false),
        AccountMeta::new_readonly(bridge_config, false),
        AccountMeta::new(wrapper, false),
        AccountMeta::new(sol_vault, false),
        AccountMeta::new(BRIDGED_SOL_MINT, false),
        AccountMeta::new_readonly(bridge_events, false),
        AccountMeta::new_readonly(bordrless_token::ID, false),
        AccountMeta::new_readonly(token_client::event_authority(), false),
        AccountMeta::new_readonly(anchor_lang::system_program::ID, false),
    ])
}

/// `migrate_pool_to_native`, signed by the admin.
pub fn migrate_pool_to_native(admin: Pubkey, pool: Pubkey, buffer_target: u64) -> Instruction {
    migrate_pool_to_native_with(admin, pool, buffer_target, false)
}

/// `migrate_pool_to_native` with `allow_foreign_hook`: a pool whose hook is another program than
/// the launchpad (refused otherwise).
pub fn migrate_pool_to_native_with(
    admin: Pubkey,
    pool: Pubkey,
    buffer_target: u64,
    allow_foreign_hook: bool,
) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: quote_form_accounts(admin, pool),
        data: crate::instruction::MigratePoolToNative {
            buffer_target,
            allow_foreign_hook,
        }
        .data(),
    }
}

/// `revert_pool_to_bridged`, signed by the admin (a plain wallet: it wraps the pool's SOL).
pub fn revert_pool_to_bridged(admin: Pubkey, pool: Pubkey) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: quote_form_accounts(admin, pool),
        data: crate::instruction::RevertPoolToBridged {}.data(),
    }
}

/// `revert_pool_to_bridged` of a launch pool (its hook the launchpad): the launch of `base_mint`
/// is passed (read) so the DEX can check it reverted first.
pub fn revert_launch_pool_to_bridged(
    admin: Pubkey,
    pool: Pubkey,
    base_mint: Pubkey,
) -> Instruction {
    let mut ix = revert_pool_to_bridged(admin, pool);
    let (launch, _) = Pubkey::find_program_address(
        &[crate::constants::LAUNCH_SEED, base_mint.as_ref()],
        &crate::constants::LAUNCHPAD_ID,
    );
    ix.accounts.push(AccountMeta::new_readonly(launch, false));
    ix
}

/// `rebalance_pool`, signed by anyone (a plain wallet: it may wrap the pool's SOL).
pub fn rebalance_pool(cranker: Pubkey, pool: Pubkey) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: quote_form_accounts(cranker, pool),
        data: crate::instruction::RebalancePool {}.data(),
    }
}

/// `buffer_all`, signed by anyone (a plain wallet: it wraps the pool's SOL). The same transaction
/// must send `rebalance_pool` of the same pool after it.
pub fn buffer_all(cranker: Pubkey, pool: Pubkey) -> Instruction {
    let mut accounts = quote_form_accounts(cranker, pool);
    accounts.push(AccountMeta::new_readonly(
        Pubkey::from_str_const("Sysvar1nstructions1111111111111111111111111"),
        false,
    ));
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::BufferAll {}.data(),
    }
}

/// `set_buffer_target`, signed by the admin.
pub fn set_buffer_target(admin: Pubkey, pool: Pubkey, buffer_target: u64) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: quote_form_accounts(admin, pool),
        data: crate::instruction::SetBufferTarget { buffer_target }.data(),
    }
}

/// `collect_protocol_fees_sol`: anyone (`cranker`) unwraps a pool's accrued protocol fees (the pool's
/// quote must be bridged SOL) and pays them to `fee_collector`, the wallet the config names, as SOL.
pub fn collect_protocol_fees_sol(
    cranker: Pubkey,
    pool: Pubkey,
    fee_collector: Pubkey,
) -> Instruction {
    use crate::constants::{BRIDGED_SOL_MINT, BRIDGE_ID};
    let native = Pubkey::from_str_const("So11111111111111111111111111111111111111112");
    let bridge = |seeds: &[&[u8]]| Pubkey::find_program_address(seeds, &BRIDGE_ID).0;
    let accounts = with_events(vec![
        AccountMeta::new_readonly(cranker, true),
        AccountMeta::new_readonly(config_address(), false),
        AccountMeta::new(pool, false),
        AccountMeta::new(vault_address(&pool, &BRIDGED_SOL_MINT), false),
        AccountMeta::new(fee_collector, false),
        AccountMeta::new_readonly(BRIDGE_ID, false),
        AccountMeta::new_readonly(bridge(&[b"config"]), false),
        AccountMeta::new(bridge(&[b"wrapper", native.as_ref()]), false),
        AccountMeta::new(bridge(&[b"sol-vault"]), false),
        AccountMeta::new(BRIDGED_SOL_MINT, false),
        AccountMeta::new_readonly(bridge(&[b"__event_authority"]), false),
        AccountMeta::new_readonly(bordrless_token::ID, false),
        AccountMeta::new_readonly(token_client::event_authority(), false),
        AccountMeta::new_readonly(anchor_lang::system_program::ID, false),
    ]);
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::CollectProtocolFeesSol {}.data(),
    }
}

/// `collect_protocol_fees`: the pool's protocol fees (always in its quote token) to the holding of
/// `fee_collector` (the config's) for the quote mint. `quote_hook` is the quote mint's token-hook
/// slice ([`token_hook_slice`]; empty for a mint without a hook), as a swap passes it.
pub fn collect_protocol_fees(
    admin: Pubkey,
    pool: Pubkey,
    quote_mint: Pubkey,
    fee_collector: Pubkey,
    quote_hook: Vec<AccountMeta>,
) -> Instruction {
    let quote_hook_accounts = u8::try_from(quote_hook.len()).expect("at most 255 hook accounts");
    let mut accounts = vec![
        AccountMeta::new_readonly(admin, true),
        AccountMeta::new_readonly(config_address(), false),
        AccountMeta::new(pool, false),
        AccountMeta::new_readonly(quote_mint, false),
        AccountMeta::new(vault_address(&pool, &quote_mint), false),
        AccountMeta::new(
            token_client::holding_address(&quote_mint, &fee_collector),
            false,
        ),
    ];
    accounts.extend(token_fixed());
    let mut accounts = with_events(accounts);
    accounts.extend(quote_hook);
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::CollectProtocolFees {
            quote_hook_accounts,
        }
        .data(),
    }
}

/// Reads a pool from its account data.
pub fn read_pool(info: &AccountInfo) -> Result<Pool> {
    require_keys_eq!(
        *info.owner,
        crate::ID,
        anchor_lang::error::ErrorCode::AccountOwnedByWrongProgram
    );
    let data = info.try_borrow_data()?;
    Pool::try_deserialize(&mut &data[..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bumps_are_canonical() {
        use bordrless_hook::HOOK_AUTHORITY_SEED;
        assert_eq!(
            Pubkey::find_program_address(&[CONFIG_SEED], &crate::ID),
            (config_address(), CONFIG_BUMP)
        );
        // One pool hook signer per hook program.
        let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
        assert_eq!(
            hook_signer(&a),
            Pubkey::find_program_address(&[HOOK_AUTHORITY_SEED, a.as_ref()], &crate::ID).0
        );
        assert_ne!(hook_signer(&a), hook_signer(&b));
        assert_eq!(hook_signer_meta(None).pubkey, crate::ID);
        // A token-hook slice: the hook, the token program's signer for it, then its accounts.
        let extra = AccountMeta::new(Pubkey::new_unique(), false);
        let slice = token_hook_slice(Some(a), vec![extra.clone()]);
        assert_eq!(slice.len(), 3);
        assert_eq!(
            (slice[0].pubkey, slice[1].pubkey),
            (a, token_client::hook_signer(&a))
        );
        assert_eq!(slice[2], extra);
        assert!(token_hook_slice(None, vec![extra]).is_empty());
    }
}
