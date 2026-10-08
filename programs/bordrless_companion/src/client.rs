//! Builders for the companion's instructions, as the tests and the SDK build them. Each step's
//! remaining accounts are the accounts of the instructions it invokes, built with the callees'
//! own clients exactly as the program builds them on chain; the program looks them up by key. The
//! creator address signs only inside the program, so it is never marked a signer here.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::{system_program, InstructionData};
use bordrless_launch::client::{self as launch_client, LaunchKeys};
use bordrless_launch::instructions::CreateLaunchArgs;
use bordrless_token::client::{self as token_client, Hook};

use crate::constants::*;
use crate::instructions::CreateArgs;
use crate::state::Companion;

/// This program's event authority.
pub fn event_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &crate::ID).0
}

pub fn companion_address(mint: &Pubkey) -> Pubkey {
    Companion::address(mint).0
}

/// The launch's creator: `PDA(["creator", mint])`.
pub fn creator_address(mint: &Pubkey) -> Pubkey {
    Companion::creator(mint).0
}

/// `ix`'s accounts and program as remaining accounts: nobody marked a signer but `keep`.
fn remaining(ix: &Instruction, keep: &[Pubkey]) -> Vec<AccountMeta> {
    let mut metas: Vec<AccountMeta> = ix
        .accounts
        .iter()
        .map(|m| AccountMeta {
            pubkey: m.pubkey,
            is_signer: m.is_signer && keep.contains(&m.pubkey),
            is_writable: m.is_writable,
        })
        .collect();
    metas.push(AccountMeta::new_readonly(ix.program_id, false));
    metas
}

fn build(named: Vec<AccountMeta>, extra: Vec<AccountMeta>, data: Vec<u8>) -> Instruction {
    let mut accounts = named;
    accounts.push(AccountMeta::new_readonly(event_authority(), false));
    accounts.push(AccountMeta::new_readonly(crate::ID, false));
    accounts.extend(extra);
    Instruction {
        program_id: crate::ID,
        accounts,
        data,
    }
}

/// The launch's mint as token instructions take it (`steps.rs` `mint_hook`).
fn mint_hook(keys: &LaunchKeys, rewards: bool) -> (Option<Hook>, Vec<AccountMeta>) {
    if keys.modules == 0 {
        return (None, vec![]);
    }
    let vault = rewards.then(|| launch_client::holder_vault_address(&keys.mint, &keys.quote_mint));
    (
        Some(Hook::of(KIT_ID)),
        bordrless_launch::cpi::kit_extras(launch_client::kit_config_address(&keys.mint), vault)
            .to_vec(),
    )
}

/// `create`: a companion for `mint` (whose keypair signs), `payer` paying its rent and `args.fund`.
pub fn create(payer: Pubkey, beneficiary: Pubkey, mint: Pubkey, args: CreateArgs) -> Instruction {
    let creator = creator_address(&mint);
    let holding = token_client::create_holding(payer, BRIDGED_SOL_MINT, creator);
    let named = vec![
        AccountMeta::new(payer, true),
        AccountMeta::new_readonly(beneficiary, false),
        AccountMeta::new_readonly(mint, true),
        AccountMeta::new(companion_address(&mint), false),
        AccountMeta::new(creator, false),
        AccountMeta::new_readonly(BRIDGED_SOL_MINT, false),
        AccountMeta::new(
            token_client::holding_address(&BRIDGED_SOL_MINT, &creator),
            false,
        ),
        AccountMeta::new_readonly(TOKEN_ID, false),
        AccountMeta::new_readonly(token_client::event_authority(), false),
        AccountMeta::new_readonly(system_program::ID, false),
    ];
    build(
        named,
        remaining(&holding, &[]),
        crate::instruction::Create { args }.data(),
    )
}

/// `launch`: `create_launch` (built by the launchpad's client with the creator address as the
/// creator) through the companion. The mint signs.
pub fn launch(
    launcher: Pubkey,
    mint: Pubkey,
    create_launch: &Instruction,
    args: CreateLaunchArgs,
) -> Instruction {
    let named = vec![
        AccountMeta::new_readonly(launcher, true),
        AccountMeta::new(companion_address(&mint), false),
        AccountMeta::new(creator_address(&mint), false),
        AccountMeta::new_readonly(LAUNCH_ID, false),
    ];
    let inner: Vec<AccountMeta> = create_launch
        .accounts
        .iter()
        .map(|m| AccountMeta {
            pubkey: m.pubkey,
            is_signer: m.is_signer && m.pubkey == mint,
            is_writable: m.is_writable,
        })
        .collect();
    build(named, inner, crate::instruction::Launch { args }.data())
}

fn step(cranker: Pubkey, mint: Pubkey) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new(cranker, true),
        AccountMeta::new(companion_address(&mint), false),
        AccountMeta::new(creator_address(&mint), false),
        AccountMeta::new_readonly(launch_client::launch_address(&mint), false),
        AccountMeta::new_readonly(system_program::ID, false),
    ]
}

fn unwrap_accounts(creator: Pubkey) -> Vec<AccountMeta> {
    remaining(&bordrless_bridge::client::unwrap_sol(creator, 0), &[])
}

/// `dev_buy`.
pub fn dev_buy(beneficiary: Pubkey, keys: &LaunchKeys, lamports: u64, min_out: u64) -> Instruction {
    let creator = creator_address(&keys.mint);
    let mut extra = remaining(
        &token_client::create_holding(beneficiary, keys.mint, creator),
        &[],
    );
    extra.extend(remaining(
        &bordrless_bridge::client::wrap_sol(creator, lamports),
        &[],
    ));
    extra.extend(remaining(
        &launch_client::swap(keys, creator, creator, 1, lamports, min_out),
        &[],
    ));
    let named = vec![
        AccountMeta::new(beneficiary, true),
        AccountMeta::new(companion_address(&keys.mint), false),
        AccountMeta::new(creator, false),
        AccountMeta::new_readonly(launch_client::launch_address(&keys.mint), false),
        AccountMeta::new_readonly(system_program::ID, false),
    ];
    build(
        named,
        extra,
        crate::instruction::DevBuy { lamports, min_out }.data(),
    )
}

/// `claim_fees`; `author` is the listed config and its author when the launch pays one.
pub fn claim_fees(cranker: Pubkey, mint: Pubkey, author: Option<(Pubkey, Pubkey)>) -> Instruction {
    let creator = creator_address(&mint);
    let claim = match author {
        Some((config, author)) => launch_client::claim_creator_fees_shared(
            creator,
            mint,
            BRIDGED_SOL_MINT,
            config,
            author,
        ),
        None => launch_client::claim_creator_fees(creator, mint, BRIDGED_SOL_MINT),
    };
    let mut extra = remaining(&claim, &[]);
    extra.extend(unwrap_accounts(creator));
    build(
        step(cranker, mint),
        extra,
        crate::instruction::ClaimFees {}.data(),
    )
}

/// `buyback`; `rewards` says whether the launch has holder rewards (the kit's extras).
pub fn buyback(cranker: Pubkey, keys: &LaunchKeys, rewards: bool) -> Instruction {
    let creator = creator_address(&keys.mint);
    let mut extra = remaining(&launch_client::swap(keys, creator, creator, 1, 0, 0), &[]);
    extra.push(AccountMeta::new_readonly(
        launch_client::pool_address(&keys.mint, &keys.quote_mint, keys.lp_fee_bps),
        false,
    ));
    extra.extend(remaining(
        &token_client::create_holding(cranker, keys.mint, creator),
        &[],
    ));
    let (hook, extras) = mint_hook(keys, rewards);
    extra.extend(remaining(
        &token_client::burn_with(
            creator,
            token_client::holding_address(&keys.mint, &creator),
            keys.mint,
            hook,
            extras,
            0,
        ),
        &[],
    ));
    extra.extend(unwrap_accounts(creator));
    build(
        step(cranker, keys.mint),
        extra,
        crate::instruction::Buyback {}.data(),
    )
}

/// `share`.
pub fn share(cranker: Pubkey, mint: Pubkey) -> Instruction {
    let creator = creator_address(&mint);
    let source = token_client::holding_address(&BRIDGED_SOL_MINT, &creator);
    let mut extra = remaining(
        &bordrless_kit::client::share(creator, mint, source, BRIDGED_SOL_MINT, 0),
        &[],
    );
    extra.extend(unwrap_accounts(creator));
    build(
        step(cranker, mint),
        extra,
        crate::instruction::Share {}.data(),
    )
}

/// `withdraw`: pays `beneficiary` (the companion's).
pub fn withdraw(sender: Pubkey, mint: Pubkey, beneficiary: Pubkey) -> Instruction {
    let creator = creator_address(&mint);
    let named = vec![
        AccountMeta::new_readonly(sender, true),
        AccountMeta::new(companion_address(&mint), false),
        AccountMeta::new(creator, false),
        AccountMeta::new(beneficiary, false),
        AccountMeta::new_readonly(system_program::ID, false),
    ];
    build(
        named,
        unwrap_accounts(creator),
        crate::instruction::Withdraw {}.data(),
    )
}

/// `withdraw` before the launch: the creator address's funding back to `beneficiary`, signed by the
/// mint (a launch that never happened).
pub fn refund(sender: Pubkey, mint: Pubkey, beneficiary: Pubkey) -> Instruction {
    let mut ix = withdraw(sender, mint, beneficiary);
    ix.accounts.push(AccountMeta::new_readonly(mint, true));
    ix
}

/// `release`: the vested dev bag to `beneficiary` (the companion's).
pub fn release(
    cranker: Pubkey,
    keys: &LaunchKeys,
    rewards: bool,
    beneficiary: Pubkey,
) -> Instruction {
    let creator = creator_address(&keys.mint);
    let mut extra = remaining(
        &token_client::create_holding(cranker, keys.mint, beneficiary),
        &[],
    );
    let (hook, extras) = mint_hook(keys, rewards);
    let transfer = token_client::transfer_with(
        creator,
        token_client::holding_address(&keys.mint, &creator),
        token_client::holding_address(&keys.mint, &beneficiary),
        keys.mint,
        hook,
        extras,
        0,
    );
    extra.extend(remaining(&transfer, &[]));
    build(
        step(cranker, keys.mint),
        extra,
        crate::instruction::Release {}.data(),
    )
}
