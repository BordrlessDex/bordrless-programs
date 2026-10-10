//! Builders for the vault's instructions, as the tests and the SDK build them. Each step's remaining
//! accounts are the accounts of the instructions it invokes, built with the callees' own clients
//! exactly as the program builds them on chain (the program looks them up by key), listed once
//! each. A slot's owner signs only inside the program, so it is never marked a signer here.
//!
//! The coin's token instructions carry its custom hook: the builders take the hook's extra
//! accounts as the client resolved them from its registry for that very operation
//! (`CustomHookAccounts`), and add the registry, which the program resolves them from itself.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::{system_program, InstructionData};
use bordrless_hook::hook_accounts_address;
use bordrless_launch::client::{self as launch_client, CustomHookAccounts, LaunchKeys};
use bordrless_token::client::{self as token_client, Hook};

use crate::constants::*;
use crate::instructions::CreateVaultArgs;
use crate::state::Vault;

/// This program's event authority.
pub fn event_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &crate::ID).0
}

/// The vault of `mint`.
pub fn vault_address(mint: &Pubkey) -> Pubkey {
    Vault::address(mint).0
}

/// Slot `i`'s owner: its holding of the coin is where the coin's hook sends the slot's cut.
pub fn slot_owner(mint: &Pubkey, i: u8) -> Pubkey {
    Vault::slot_owner(mint, i).0
}

/// The holding of the coin a hook names for slot `i` (its delta target).
pub fn slot_holding(mint: &Pubkey, i: u8) -> Pubkey {
    token_client::holding_address(mint, &slot_owner(mint, i))
}

/// Appends `ix`'s accounts and program to `out`, each key once (writable if any listing is), and
/// nobody a signer.
fn add(out: &mut Vec<AccountMeta>, ix: &Instruction) {
    for meta in ix
        .accounts
        .iter()
        .cloned()
        .chain([AccountMeta::new_readonly(ix.program_id, false)])
    {
        push(out, meta.pubkey, meta.is_writable);
    }
}

/// Appends `key` once (writable if any listing is), not a signer.
fn push(out: &mut Vec<AccountMeta>, key: Pubkey, writable: bool) {
    match out.iter_mut().find(|m| m.pubkey == key) {
        Some(m) => m.is_writable |= writable,
        None => out.push(AccountMeta {
            pubkey: key,
            is_signer: false,
            is_writable: writable,
        }),
    }
}

fn build(named: Vec<AccountMeta>, extra: Vec<AccountMeta>, data: Vec<u8>) -> Instruction {
    let mut accounts = named;
    accounts.push(AccountMeta::new_readonly(event_authority(), false));
    accounts.push(AccountMeta::new_readonly(crate::ID, false));
    // The named accounts already carry their own flags: what they list again is dropped.
    let mut rest: Vec<AccountMeta> = Vec::new();
    for m in extra {
        match accounts.iter_mut().find(|a| a.pubkey == m.pubkey) {
            Some(a) => a.is_writable |= m.is_writable,
            None => push(&mut rest, m.pubkey, m.is_writable),
        }
    }
    accounts.extend(rest);
    Instruction {
        program_id: crate::ID,
        accounts,
        data,
    }
}

/// `create_vault`: the vault of `mint` (whose keypair signs, and which must not exist yet), `payer`
/// paying its rent. `buy_mints` are the tokens of the `SellBuyBurn` slots (whose pools are their
/// targets): their launches are read. Each `SellForSol` wallet is passed too (read: it must be
/// able to take SOL).
pub fn create_vault(
    payer: Pubkey,
    mint: Pubkey,
    args: CreateVaultArgs,
    buy_mints: &[Pubkey],
) -> Instruction {
    let named = vec![
        AccountMeta::new(payer, true),
        AccountMeta::new_readonly(mint, true),
        AccountMeta::new(vault_address(&mint), false),
        AccountMeta::new_readonly(system_program::ID, false),
    ];
    let mut extra = Vec::new();
    for a in &args.slots {
        if a.policy == policy::SELL_BUY_BURN {
            push(&mut extra, a.target, false);
        }
    }
    for x in buy_mints {
        push(&mut extra, launch_client::launch_address(x), false);
    }
    for a in &args.slots {
        if a.policy == policy::SELL_FOR_SOL {
            push(&mut extra, a.target, false);
        }
    }
    build(
        named,
        extra,
        crate::instruction::CreateVault { args }.data(),
    )
}

/// `open_vault` of `vault` (as read), the coin's launch keys giving its pool.
pub fn open_vault(sender: Pubkey, vault: &Vault, coin: &LaunchKeys) -> Instruction {
    let mint = vault.mint;
    let named = vec![
        AccountMeta::new(sender, true),
        AccountMeta::new(vault_address(&mint), false),
        AccountMeta::new_readonly(launch_client::launch_address(&mint), false),
        AccountMeta::new_readonly(mint, false),
        AccountMeta::new_readonly(system_program::ID, false),
    ];
    let mut extra = Vec::new();
    push(&mut extra, coin.pool(), false);
    for (i, slot) in vault.slots[..usize::from(vault.n_slots)].iter().enumerate() {
        let owner = slot_owner(&mint, i as u8);
        push(&mut extra, owner, slot.sells());
        add(
            &mut extra,
            &token_client::create_holding(sender, mint, owner),
        );
        if slot.sells() {
            add(
                &mut extra,
                &token_client::create_holding(sender, BRIDGED_SOL_MINT, owner),
            );
        }
        if slot.policy == policy::SELL_BUY_BURN {
            push(&mut extra, slot.target, false);
        }
    }
    build(named, extra, crate::instruction::OpenVault {}.data())
}

fn step(cranker: Pubkey, mint: &Pubkey, i: u8) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new(cranker, true),
        AccountMeta::new(vault_address(mint), false),
        AccountMeta::new(slot_owner(mint, i), false),
        AccountMeta::new_readonly(launch_client::launch_address(mint), false),
        AccountMeta::new_readonly(system_program::ID, false),
    ]
}

fn registry(out: &mut Vec<AccountMeta>, hook: &Pubkey, mint: &Pubkey) {
    push(out, hook_accounts_address(hook, mint).0, false);
}

fn unwrap_accounts(out: &mut Vec<AccountMeta>, owner: Pubkey) {
    add(out, &bordrless_bridge::client::unwrap_sol(owner, 0));
}

/// The coin's burn by slot `i`'s owner, as the program builds it; `hook` resolved for that burn.
fn coin_burn(owner: Pubkey, mint: Pubkey, hook: &CustomHookAccounts) -> Instruction {
    token_client::burn_with(
        owner,
        token_client::holding_address(&mint, &owner),
        mint,
        Some(Hook::of(hook.program)),
        hook.extras.clone(),
        0,
    )
}

/// `execute(i)` of `vault` (as read). `coin` is the coin's launch keys; `hook` the coin's hook with
/// its extras resolved for the operation the slot runs: the burn of the slot's holding (`Burn`), or
/// the transfer from the slot's holding to the pool's coin vault, the slot's owner signing
/// (`SellForSol`, `SellBuyBurn`).
pub fn execute(
    cranker: Pubkey,
    vault: &Vault,
    i: u8,
    coin: &LaunchKeys,
    hook: &CustomHookAccounts,
) -> Instruction {
    let mint = vault.mint;
    let owner = slot_owner(&mint, i);
    let slot = vault.slots[usize::from(i)];
    let mut extra = Vec::new();
    if slot.policy == policy::BURN {
        add(&mut extra, &coin_burn(owner, mint, hook));
    } else {
        add(
            &mut extra,
            &launch_client::swap_with_base_slice(coin, owner, owner, 0, 0, 0, hook.slice()),
        );
        unwrap_accounts(&mut extra, owner);
        if slot.policy == policy::SELL_FOR_SOL {
            push(&mut extra, slot.target, true);
        }
    }
    registry(&mut extra, &hook.program, &mint);
    build(
        step(cranker, &mint, i),
        extra,
        crate::instruction::Execute { i }.data(),
    )
}

/// The other token's hook as `execute_buy` passes it: none, the kit (with or without its reward
/// vault, `Launch.rules.rewards_on()`), or its custom hook resolved for the pool vault's transfer
/// to the slot's owner and for the owner's burn.
pub enum BuyHook<'a> {
    Kit {
        rewards: bool,
    },
    Custom {
        transfer: &'a CustomHookAccounts,
        burn: &'a CustomHookAccounts,
    },
}

/// `execute_buy(i)` of `vault` (as read): `x` is the launch keys of the token the slot buys.
pub fn execute_buy(
    cranker: Pubkey,
    vault: &Vault,
    i: u8,
    x: &LaunchKeys,
    hook: BuyHook,
) -> Instruction {
    let mint = vault.mint;
    let owner = slot_owner(&mint, i);
    let mut extra = Vec::new();
    let buy = match &hook {
        BuyHook::Kit { .. } => launch_client::swap(x, owner, owner, 1, 0, 0),
        BuyHook::Custom { transfer, .. } => {
            launch_client::swap_with_base_slice(x, owner, owner, 1, 0, 0, transfer.slice())
        }
    };
    add(&mut extra, &buy);
    push(&mut extra, launch_client::launch_address(&x.mint), false);
    add(
        &mut extra,
        &token_client::create_holding(cranker, x.mint, owner),
    );
    let holding = token_client::holding_address(&x.mint, &owner);
    let burn = match &hook {
        BuyHook::Kit { rewards } if x.modules != 0 => {
            let vault =
                rewards.then(|| launch_client::holder_vault_address(&x.mint, &x.quote_mint));
            token_client::burn_with(
                owner,
                holding,
                x.mint,
                Some(Hook::of(KIT_ID)),
                bordrless_launch::cpi::kit_extras(
                    launch_client::kit_config_address(&x.mint),
                    vault,
                )
                .to_vec(),
                0,
            )
        }
        BuyHook::Kit { .. } => token_client::burn_with(owner, holding, x.mint, None, vec![], 0),
        BuyHook::Custom { burn, .. } => coin_burn(owner, x.mint, burn),
    };
    add(&mut extra, &burn);
    unwrap_accounts(&mut extra, owner);
    if let BuyHook::Custom { transfer, .. } = &hook {
        registry(&mut extra, &transfer.program, &x.mint);
    }
    build(
        step(cranker, &mint, i),
        extra,
        crate::instruction::ExecuteBuy { i }.data(),
    )
}

/// `retire(i, burn_coin)` of `vault` (as read); `burn` is the coin's hook resolved for the slot's
/// burn (needed with `burn_coin`).
pub fn retire(
    cranker: Pubkey,
    vault: &Vault,
    i: u8,
    burn_coin: bool,
    burn: Option<&CustomHookAccounts>,
) -> Instruction {
    let mint = vault.mint;
    let owner = slot_owner(&mint, i);
    let mut extra = Vec::new();
    if vault.slots[usize::from(i)].pending_sol > 0 {
        unwrap_accounts(&mut extra, owner);
        push(&mut extra, INCINERATOR, true);
    }
    if let Some(hook) = burn.filter(|_| burn_coin) {
        add(&mut extra, &coin_burn(owner, mint, hook));
        registry(&mut extra, &hook.program, &mint);
    }
    build(
        step(cranker, &mint, i),
        extra,
        crate::instruction::Retire { i, burn_coin }.data(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_their_derivations() {
        let mint = Pubkey::new_unique();
        assert_eq!(
            vault_address(&mint),
            Pubkey::find_program_address(&[VAULT_SEED, mint.as_ref()], &crate::ID).0
        );
        for i in 0..3u8 {
            assert_eq!(
                slot_owner(&mint, i),
                Pubkey::find_program_address(&[SLOT_SEED, mint.as_ref(), &[i]], &crate::ID).0
            );
        }
        assert_ne!(slot_owner(&mint, 0), slot_owner(&mint, 1));
        assert_eq!(event_authority(), crate::EVENT_AUTHORITY_AND_BUMP.0);
    }

    #[test]
    fn remaining_accounts_are_listed_once() {
        let mut out = Vec::new();
        let k = Pubkey::new_unique();
        push(&mut out, k, false);
        push(&mut out, k, true);
        push(&mut out, k, false);
        assert_eq!(out, vec![AccountMeta::new(k, false)]);
        let ix = build(
            vec![AccountMeta::new_readonly(k, true)],
            vec![AccountMeta::new(k, false)],
            vec![],
        );
        assert_eq!(ix.accounts[0], AccountMeta::new(k, true));
        assert_eq!(ix.accounts.len(), 3);
    }
}
