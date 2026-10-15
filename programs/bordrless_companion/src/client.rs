//! Builders for the companion's instructions, as the tests and the SDK build them. Each step's
//! remaining accounts are the accounts of the instructions it invokes, built with the callees'
//! own clients exactly as the program builds them on chain; the program looks them up by key. The
//! creator address signs only inside the program, so it is never marked a signer here.
//!
//! A token with a custom hook (a game coin's, or since 2026-10-12 any companion's) has its token
//! instructions carry the hook: the `_with` builders take the hook's accounts as the client
//! resolved them from its registry (`CustomHookAccounts`), and add the registry, which the program
//! resolves them from itself. Without one they build exactly what the plain builders build.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::{system_program, InstructionData, ToAccountMetas};
use bordrless_hook::hook_accounts_address;
use bordrless_launch::client::{self as launch_client, CustomHookAccounts, LaunchKeys};
use bordrless_launch::instructions::CreateLaunchArgs;
use bordrless_token::client::{self as token_client, Hook};

use crate::constants::*;
use crate::instructions::{
    AttestArgs, CreateArgs, CreateGameArgs, CreateScriptArgs, CreateSnowballArgs, GameKindArgs,
    HookStatusArgs, PayoutTermsArgs, StrategyArgs,
};
use crate::oracle;
use crate::state::{
    Approval, Companion, Game, HookAttestation, HookStatus, PayoutTerms, Script, ShareReceipt,
    Snowball, StrategyTerms,
};

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

/// The mint's hook for a token instruction: the kit's ([`mint_hook`]) without a custom hook, else
/// the custom hook and its extras.
fn token_hook(
    keys: &LaunchKeys,
    rewards: bool,
    custom: Option<&CustomHookAccounts>,
) -> (Option<Hook>, Vec<AccountMeta>) {
    match custom {
        None => mint_hook(keys, rewards),
        Some(h) => (Some(Hook::of(h.program)), h.extras.clone()),
    }
}

/// A custom hook's registry for `mint`, which the program resolves the hook's extras from.
fn registry(mint: &Pubkey, custom: Option<&CustomHookAccounts>) -> Vec<AccountMeta> {
    custom
        .map(|h| {
            vec![AccountMeta::new_readonly(
                hook_accounts_address(&h.program, mint).0,
                false,
            )]
        })
        .unwrap_or_default()
}

/// The creator address's buy on the launch pool, as the program builds it.
fn creator_buy(
    keys: &LaunchKeys,
    lamports: u64,
    min_out: u64,
    custom: Option<&CustomHookAccounts>,
) -> Instruction {
    let creator = creator_address(&keys.mint);
    match custom {
        None => launch_client::swap(keys, creator, creator, 1, lamports, min_out),
        Some(h) => launch_client::swap_with_base_slice(
            keys,
            creator,
            creator,
            1,
            lamports,
            min_out,
            h.slice(),
        ),
    }
}

/// The accounts a native companion's steps call besides a bridged one's: the launchpad's
/// `claim_creator_fees_sol` (the launch writable), the creator address's `swap_v2` on the launch
/// pool (the System Program and the DEX), and the kit's `share_sol`. Every step looks its accounts
/// up by key, so appending these to any step serves a native companion; a bridged one ignores them.
pub fn native_accounts(keys: &LaunchKeys) -> Vec<AccountMeta> {
    let creator = creator_address(&keys.mint);
    let mut extra = remaining(
        &launch_client::claim_creator_fees_sol(creator, keys.mint, None, None),
        &[],
    );
    extra.extend(remaining(
        &launch_client::swap_v2_with_bridge(
            keys,
            creator,
            creator,
            1,
            0,
            0,
            launch_client::base_hook_slice(&keys.mint, &keys.quote_mint, keys.modules),
            false,
        ),
        &[],
    ));
    extra.extend(remaining(
        &bordrless_kit::client::share_sol(creator, keys.mint, 0),
        &[],
    ));
    extra
}

/// `ix` (a step of a native companion) with [`native_accounts`] appended.
pub fn native(mut ix: Instruction, keys: &LaunchKeys) -> Instruction {
    ix.accounts.extend(native_accounts(keys));
    ix
}

/// `migrate_companion_to_native` of `mint`'s companion, signed by the program's upgrade authority.
pub fn migrate_companion_to_native(admin: Pubkey, mint: Pubkey) -> Instruction {
    companion_form(
        admin,
        mint,
        crate::instruction::MigrateCompanionToNative {}.data(),
    )
}

/// `revert_companion_to_bridged`, as [`migrate_companion_to_native`].
pub fn revert_companion_to_bridged(admin: Pubkey, mint: Pubkey) -> Instruction {
    companion_form(
        admin,
        mint,
        crate::instruction::RevertCompanionToBridged {}.data(),
    )
}

fn companion_form(admin: Pubkey, mint: Pubkey, data: Vec<u8>) -> Instruction {
    let creator = creator_address(&mint);
    let named = vec![
        AccountMeta::new(admin, true),
        AccountMeta::new_readonly(program_data_address(), false),
        AccountMeta::new_readonly(bordrless_swap::client::config_address(), false),
        AccountMeta::new(companion_address(&mint), false),
        AccountMeta::new(creator, false),
        AccountMeta::new_readonly(launch_client::launch_address(&mint), false),
        AccountMeta::new_readonly(system_program::ID, false),
    ];
    build(named, unwrap_accounts(creator), data)
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

/// [`launch`] of a script companion: its script's address appended last, which the companion
/// reads (a script that is not native-ready can't launch native) and never forwards. A script
/// companion's launch while native launches are on needs it.
pub fn launch_script(
    launcher: Pubkey,
    mint: Pubkey,
    create_launch: &Instruction,
    args: CreateLaunchArgs,
) -> Instruction {
    let mut ix = launch(launcher, mint, create_launch, args);
    ix.accounts
        .push(AccountMeta::new_readonly(script_address(&mint), false));
    ix
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
    dev_buy_with(beneficiary, keys, lamports, min_out, None)
}

/// `dev_buy`, with the token's custom hook when it has one.
pub fn dev_buy_with(
    beneficiary: Pubkey,
    keys: &LaunchKeys,
    lamports: u64,
    min_out: u64,
    custom: Option<&CustomHookAccounts>,
) -> Instruction {
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
        &creator_buy(keys, lamports, min_out, custom),
        &[],
    ));
    extra.extend(registry(&keys.mint, custom));
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
    buyback_with(cranker, keys, rewards, None)
}

/// `buyback`, with the token's custom hook when it has one.
pub fn buyback_with(
    cranker: Pubkey,
    keys: &LaunchKeys,
    rewards: bool,
    custom: Option<&CustomHookAccounts>,
) -> Instruction {
    let creator = creator_address(&keys.mint);
    let mut extra = remaining(&creator_buy(keys, 0, 0, custom), &[]);
    extra.push(AccountMeta::new_readonly(
        launch_client::pool_address(&keys.mint, &keys.quote_mint, keys.lp_fee_bps),
        false,
    ));
    extra.extend(remaining(
        &token_client::create_holding(cranker, keys.mint, creator),
        &[],
    ));
    let (hook, extras) = token_hook(keys, rewards, custom);
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
    extra.extend(registry(&keys.mint, custom));
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
    release_with(cranker, keys, rewards, beneficiary, None)
}

/// `release`, with the token's custom hook when it has one.
pub fn release_with(
    cranker: Pubkey,
    keys: &LaunchKeys,
    rewards: bool,
    beneficiary: Pubkey,
    custom: Option<&CustomHookAccounts>,
) -> Instruction {
    let creator = creator_address(&keys.mint);
    let mut extra = remaining(
        &token_client::create_holding(cranker, keys.mint, beneficiary),
        &[],
    );
    let (hook, extras) = token_hook(keys, rewards, custom);
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
    extra.extend(registry(&keys.mint, custom));
    build(
        step(cranker, keys.mint),
        extra,
        crate::instruction::Release {}.data(),
    )
}

// ---- Games (v2) -----------------------------------------------------------------------------------

/// The game of `mint`: `PDA(["game", mint])`.
pub fn game_address(mint: &Pubkey) -> Pubkey {
    Game::address(mint).0
}

/// What the protocol says of `hook`: `PDA(["hook-status", hook])`.
pub fn hook_status_address(hook: &Pubkey) -> Pubkey {
    HookStatus::address(hook).0
}

/// The payer of a game's oracle requests: `PDA(["oracle", mint])`.
pub fn oracle_payer_address(mint: &Pubkey) -> Pubkey {
    Game::oracle_payer(mint).0
}

/// This program's ProgramData (its upgrade authority writes hook statuses).
pub fn program_data_address() -> Pubkey {
    Pubkey::find_program_address(&[crate::ID.as_ref()], &BPF_LOADER_UPGRADEABLE_ID).0
}

/// `claim_fees` of a game companion: the plain claim, and the game hook's status account (which the
/// program requires, whether or not it exists).
pub fn claim_fees_game(cranker: Pubkey, mint: Pubkey, hook: Pubkey) -> Instruction {
    let mut ix = claim_fees(cranker, mint, None);
    ix.accounts
        .push(AccountMeta::new_readonly(hook_status_address(&hook), false));
    ix
}

/// `create_game`: the companion of `mint` (whose keypair signs) gets its game, `payer` paying the
/// rent. `args.hook` must have been prepared for the mint (its state is read), and be Bordrless's
/// lottery hook or have a status the protocol wrote (its status account is passed either way).
pub fn create_game(payer: Pubkey, mint: Pubkey, args: CreateGameArgs) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: crate::accounts::CreateGame {
            payer,
            mint,
            companion: companion_address(&mint),
            game: game_address(&mint),
            hook_state: bordrless_game::state_address(&args.hook, &mint).0,
            hook_registry: hook_accounts_address(&args.hook, &mint).0,
            hook_status: hook_status_address(&args.hook),
            system_program: system_program::ID,
            event_authority: event_authority(),
            program: crate::ID,
        }
        .to_account_metas(None),
        data: crate::instruction::CreateGame { args }.data(),
    }
}

/// The named accounts of `draw`, `reveal`, `expire` and `retire`.
fn game_step(
    cranker: Pubkey,
    mint: Pubkey,
    hook: Pubkey,
    extra: Vec<AccountMeta>,
) -> Vec<AccountMeta> {
    let mut accounts = crate::accounts::GameStep {
        cranker,
        companion: companion_address(&mint),
        creator: creator_address(&mint),
        game: game_address(&mint),
        hook_status: hook_status_address(&hook),
        oracle_payer: oracle_payer_address(&mint),
        system_program: system_program::ID,
        event_authority: event_authority(),
        program: crate::ID,
    }
    .to_account_metas(None);
    accounts.extend(extra);
    accounts
}

/// The oracle's `request_v2` accounts for `seed`, the game's oracle payer paying, with `treasury`
/// (the oracle's, read from its network state).
fn request_accounts(mint: &Pubkey, seed: [u8; 32], treasury: Pubkey) -> Vec<AccountMeta> {
    let terms = oracle::Terms { treasury, fee: 0 };
    remaining(
        &oracle::request_ix(oracle_payer_address(mint), &terms, seed),
        &[],
    )
}

/// The slot a draw's seed is made from, and its hash: an entry of the slot hashes sysvar (its
/// first, the newest, read just before sending), which must still be one of the last
/// `oracle::SEED_SLOTS` slots when the draw lands (else it fails with `StaleSeed`: build it again).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeedSlot {
    pub slot: u64,
    pub hash: [u8; 32],
}

impl SeedSlot {
    /// The seed of the draw of `round` of `mint`'s game made from this slot (`oracle::draw_seed`).
    pub fn seed(&self, mint: &Pubkey, round: u32) -> [u8; 32] {
        oracle::draw_seed(mint, round, 0, self.slot, &self.hash)
    }
}

/// `draw(round, slot)`: the draw of `round` (the round that just ended) of `mint`'s game, whose
/// hook is `hook`. It commits the draw's seed, made from `at` (`SeedSlot::seed`), and asks ORAO
/// for it in the same instruction, the pot paying and the sender paid the bounty (or adopts a
/// pending request for it); or it rolls the round over (no tickets, too late, or the pot unable to
/// pay for the oracle's request). `treasury` is the oracle's (from its network state). For a game
/// whose `Game.paid_seed` is set, use [`draw_after`].
pub fn draw(
    cranker: Pubkey,
    mint: Pubkey,
    hook: Pubkey,
    round: u32,
    at: SeedSlot,
    treasury: Pubkey,
) -> Instruction {
    draw_after(cranker, mint, hook, round, at, treasury, [0; 32])
}

/// [`draw`] with the game's `Game.paid_seed` (zeros: none): its request is passed, so the program
/// can tell whether the oracle's breaker lets the pot pay for this draw's request.
pub fn draw_after(
    cranker: Pubkey,
    mint: Pubkey,
    hook: Pubkey,
    round: u32,
    at: SeedSlot,
    treasury: Pubkey,
    paid_seed: [u8; 32],
) -> Instruction {
    let mut extra = vec![
        AccountMeta::new_readonly(bordrless_game::state_address(&hook, &mint).0, false),
        AccountMeta::new_readonly(oracle::SLOT_HASHES, false),
    ];
    extra.extend(request_accounts(&mint, at.seed(&mint, round), treasury));
    extra.extend(unwrap_accounts(creator_address(&mint)));
    if let Some(paid) = paid_request_address(&paid_seed) {
        extra.push(AccountMeta::new_readonly(paid, false));
    }
    Instruction {
        program_id: crate::ID,
        accounts: game_step(cranker, mint, hook, extra),
        data: crate::instruction::Draw {
            round,
            slot: at.slot,
        }
        .data(),
    }
}

/// The request of the last seed the pot paid for (`Game.paid_seed`), which `draw` reads before the
/// pot pays for another (none when the seed is zeros).
pub fn paid_request_address(paid_seed: &[u8; 32]) -> Option<Pubkey> {
    (*paid_seed != [0u8; 32]).then(|| oracle::request_address(paid_seed))
}

/// `reveal`: the game's pending `request` (`Game.request`) read.
pub fn reveal(cranker: Pubkey, mint: Pubkey, hook: Pubkey, request: Pubkey) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: game_step(
            cranker,
            mint,
            hook,
            vec![AccountMeta::new_readonly(request, false)],
        ),
        data: crate::instruction::Reveal {}.data(),
    }
}

/// `expire`: `request` is the draw's request (`Game.request`, read to tell a silent oracle from one
/// that answered).
pub fn expire(cranker: Pubkey, mint: Pubkey, hook: Pubkey, request: Pubkey) -> Instruction {
    let extra = vec![AccountMeta::new_readonly(request, false)];
    Instruction {
        program_id: crate::ID,
        accounts: game_step(cranker, mint, hook, extra),
        data: crate::instruction::Expire {}.data(),
    }
}

/// `retire`: the pot of `mint`'s game (hook `hook`) sent to the buyback, once it has paid no prize
/// for two dormant periods.
pub fn retire(cranker: Pubkey, mint: Pubkey, hook: Pubkey) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: game_step(cranker, mint, hook, vec![]),
        data: crate::instruction::Retire {}.data(),
    }
}

/// `burn_stranded`: the stranded buyback of `mint`'s blocked game hook (`hook`) burned as SOL.
pub fn burn_stranded(cranker: Pubkey, mint: Pubkey, hook: Pubkey) -> Instruction {
    let creator = creator_address(&mint);
    let mut accounts = crate::accounts::BurnStranded {
        cranker,
        companion: companion_address(&mint),
        creator,
        hook_status: hook_status_address(&hook),
        incinerator: INCINERATOR,
        system_program: system_program::ID,
        event_authority: event_authority(),
        program: crate::ID,
    }
    .to_account_metas(None);
    accounts.extend(unwrap_accounts(creator));
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::BurnStranded {}.data(),
    }
}

/// `claim_prize(attempt)`: `winner`'s holding of `mint` said to hold attempt `attempt`'s ticket;
/// `winner` is paid the prize.
pub fn claim_prize(
    cranker: Pubkey,
    mint: Pubkey,
    hook: Pubkey,
    attempt: u8,
    winner: Pubkey,
) -> Instruction {
    let creator = creator_address(&mint);
    let mut accounts = crate::accounts::ClaimPrize {
        cranker,
        companion: companion_address(&mint),
        creator,
        game: game_address(&mint),
        hook_status: hook_status_address(&hook),
        launch: launch_client::launch_address(&mint),
        holding: token_client::holding_address(&mint, &winner),
        system_program: system_program::ID,
        event_authority: event_authority(),
        program: crate::ID,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new(winner, false));
    accounts.extend(unwrap_accounts(creator));
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::ClaimPrize { attempt }.data(),
    }
}

/// `set_hook_status(hook, args)`, signed by the program's upgrade authority.
pub fn set_hook_status(authority: Pubkey, hook: Pubkey, args: HookStatusArgs) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: crate::accounts::SetHookStatus {
            authority,
            program_data: program_data_address(),
            hook_status: hook_status_address(&hook),
            system_program: system_program::ID,
            event_authority: event_authority(),
            program: crate::ID,
        }
        .to_account_metas(None),
        data: crate::instruction::SetHookStatus { hook, args }.data(),
    }
}

// ---- Phase 2: the jackpot, the streak, Studio hooks -------------------------------------------------

/// A program's ProgramData under the upgradeable loader: `PDA([program], loader)`.
pub fn hook_program_data_address(hook: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[hook.as_ref()], &BPF_LOADER_UPGRADEABLE_ID).0
}

/// [`create_game`] with the hook's ProgramData passed (read-only, as a remaining account): what
/// lets the program take a hook upgradeable only by the protocol's keys (a Studio hook) without a
/// status. Harmless for any other hook.
pub fn create_game_with_program_data(
    payer: Pubkey,
    mint: Pubkey,
    args: CreateGameArgs,
) -> Instruction {
    let hook = args.hook;
    let mut ix = create_game(payer, mint, args);
    ix.accounts.push(AccountMeta::new_readonly(
        hook_program_data_address(&hook),
        false,
    ));
    ix
}

/// `create_game_v2(args, kind)`: a game of any kind for `mint` (whose keypair signs), `payer`
/// paying the rent; the hook's ProgramData is passed (see [`create_game_with_program_data`]).
pub fn create_game_v2(
    payer: Pubkey,
    mint: Pubkey,
    args: CreateGameArgs,
    kind: GameKindArgs,
) -> Instruction {
    let hook = args.hook;
    let mut accounts = crate::accounts::CreateGame {
        payer,
        mint,
        companion: companion_address(&mint),
        game: game_address(&mint),
        hook_state: bordrless_game::state_address(&args.hook, &mint).0,
        hook_registry: hook_accounts_address(&args.hook, &mint).0,
        hook_status: hook_status_address(&args.hook),
        system_program: system_program::ID,
        event_authority: event_authority(),
        program: crate::ID,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(
        hook_program_data_address(&hook),
        false,
    ));
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::CreateGameV2 { args, kind }.data(),
    }
}

/// `settle`: the oldest jackpot round of `mint`'s game (hook `hook`) that is over, whose last
/// qualifying buyer is `buyer` (the header's `last_buyer`, or its `ended_buyer` while that round is
/// open): paid if their holding still holds what they bought (nothing when the pot is below its
/// minimum), else forfeited. When the launch holds unclaimed fees that could fund the prize, send
/// [`claim_fees_game`] first, in the same transaction.
pub fn settle(cranker: Pubkey, mint: Pubkey, hook: Pubkey, buyer: Pubkey) -> Instruction {
    let creator = creator_address(&mint);
    let mut accounts = crate::accounts::ClaimPrize {
        cranker,
        companion: companion_address(&mint),
        creator,
        game: game_address(&mint),
        hook_status: hook_status_address(&hook),
        launch: launch_client::launch_address(&mint),
        holding: token_client::holding_address(&mint, &buyer),
        system_program: system_program::ID,
        event_authority: event_authority(),
        program: crate::ID,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(
        bordrless_game::state_address(&hook, &mint).0,
        false,
    ));
    accounts.push(AccountMeta::new(buyer, false));
    // The launch's holding of creator fees: a prize they could fund is never closed unfunded.
    accounts.push(AccountMeta::new_readonly(
        token_client::holding_address(&BRIDGED_SOL_MINT, &launch_client::launch_address(&mint)),
        false,
    ));
    accounts.extend(unwrap_accounts(creator));
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::Settle {}.data(),
    }
}

/// `close_epoch(epoch)`: streak epoch `epoch` of `mint`'s game (hook `hook`) closed.
pub fn close_epoch(cranker: Pubkey, mint: Pubkey, hook: Pubkey, epoch: u32) -> Instruction {
    let extra = vec![AccountMeta::new_readonly(
        bordrless_game::state_address(&hook, &mint).0,
        false,
    )];
    Instruction {
        program_id: crate::ID,
        accounts: game_step(cranker, mint, hook, extra),
        data: crate::instruction::CloseEpoch { epoch }.data(),
    }
}

/// The receipt of `owner`'s claim of streak epoch `epoch` of `mint`'s game:
/// `PDA(["claimed", game, epoch_le, owner])`.
pub fn receipt_address(mint: &Pubkey, epoch: u32, owner: &Pubkey) -> Pubkey {
    ShareReceipt::address(&game_address(mint), epoch, owner).0
}

/// `claim_share(epoch)`: `owner`'s share of streak epoch `epoch` of `mint`'s game (hook `hook`),
/// paid to `owner`; `cranker` pays the receipt's rent and is paid the bounty.
pub fn claim_share(
    cranker: Pubkey,
    mint: Pubkey,
    hook: Pubkey,
    epoch: u32,
    owner: Pubkey,
) -> Instruction {
    let creator = creator_address(&mint);
    let mut accounts = crate::accounts::ClaimShare {
        cranker,
        companion: companion_address(&mint),
        creator,
        game: game_address(&mint),
        hook_status: hook_status_address(&hook),
        launch: launch_client::launch_address(&mint),
        holding: token_client::holding_address(&mint, &owner),
        owner,
        receipt: receipt_address(&mint, epoch, &owner),
        system_program: system_program::ID,
        event_authority: event_authority(),
        program: crate::ID,
    }
    .to_account_metas(None);
    accounts.extend(unwrap_accounts(creator));
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::ClaimShare { epoch }.data(),
    }
}

/// `close_receipt`: `owner`'s receipt of streak epoch `epoch` of `mint`'s game closed, its rent
/// to `payer` (who paid it: `ShareReceipt.payer`).
pub fn close_receipt(mint: Pubkey, epoch: u32, owner: Pubkey, payer: Pubkey) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: crate::accounts::CloseReceipt {
            receipt: receipt_address(&mint, epoch, &owner),
            payer,
            game: game_address(&mint),
        }
        .to_account_metas(None),
        data: crate::instruction::CloseReceipt {}.data(),
    }
}

/// `retire` of a jackpot or a streak: the hook's state is passed, so the program can tell whether
/// a round or an epoch the pot can pay now is still to be settled or closed (it then waits).
pub fn retire_game(cranker: Pubkey, mint: Pubkey, hook: Pubkey) -> Instruction {
    let mut ix = retire(cranker, mint, hook);
    ix.accounts.push(AccountMeta::new_readonly(
        bordrless_game::state_address(&hook, &mint).0,
        false,
    ));
    // The fees a claim would bring the pot (the launch's, and any surplus in the creator's
    // holding): a prize they would fund is due too.
    ix.accounts.push(AccountMeta::new_readonly(
        token_client::holding_address(&BRIDGED_SOL_MINT, &launch_client::launch_address(&mint)),
        false,
    ));
    ix.accounts.push(AccountMeta::new_readonly(
        token_client::holding_address(&BRIDGED_SOL_MINT, &creator_address(&mint)),
        false,
    ));
    ix
}

// ---- Phase 3a: attestations, audits tied to code ---------------------------------------------------

/// Studio's attestation of `program`: `PDA(["attest", program])`.
pub fn attestation_address(program: &Pubkey) -> Pubkey {
    HookAttestation::address(program).0
}

/// `program`'s `hook_timelock` account (its upgrade authority, when timelocked).
pub fn timelock_address(program: &Pubkey) -> Pubkey {
    bordrless_hook::authority::timelock_address(program).0
}

/// Any game step (or `create_game*`, `create_strategy_game`, `claim_fees`, `plan_period`) of a game
/// whose hook's status records an audit with its code's hash (`set_hook_status_v2`): the hook's
/// ProgramData appended, read-only. The audit then lifts the cap only while the hook runs that code
/// (immutable or Bordrless-managed); without the ProgramData such a step is refused
/// (`ProgramAccounts`). Harmless for any other game. For `claim_fees`, also append the game
/// (read-only: its memo spares a rehash).
pub fn with_hook_code(mut ix: Instruction, hook: &Pubkey) -> Instruction {
    ix.accounts.push(AccountMeta::new_readonly(
        hook_program_data_address(hook),
        false,
    ));
    ix
}

/// What the program needs to vet a game hook (or a strategy) without a status: its ProgramData, its
/// attestation and, when `timelocked`, its `Timelock` (read-only, appended as remaining accounts).
pub fn vetting_accounts(program: &Pubkey, timelocked: bool) -> Vec<AccountMeta> {
    let mut metas = vec![
        AccountMeta::new_readonly(hook_program_data_address(program), false),
        AccountMeta::new_readonly(attestation_address(program), false),
    ];
    if timelocked {
        metas.push(AccountMeta::new_readonly(timelock_address(program), false));
    }
    metas
}

/// [`create_game`] for a hook taken without a status (Studio's, the protocol's or a timelock's to
/// upgrade, with a current Studio attestation): its [`vetting_accounts`] appended.
pub fn create_game_attested(
    payer: Pubkey,
    mint: Pubkey,
    args: CreateGameArgs,
    timelocked: bool,
) -> Instruction {
    let hook = args.hook;
    let mut ix = create_game(payer, mint, args);
    ix.accounts.extend(vetting_accounts(&hook, timelocked));
    ix
}

/// [`create_game_v2`] with the hook's attestation (and, when `timelocked`, its `Timelock`) after
/// its ProgramData.
pub fn create_game_v2_attested(
    payer: Pubkey,
    mint: Pubkey,
    args: CreateGameArgs,
    kind: GameKindArgs,
    timelocked: bool,
) -> Instruction {
    let hook = args.hook;
    let mut ix = create_game_v2(payer, mint, args, kind);
    ix.accounts
        .push(AccountMeta::new_readonly(attestation_address(&hook), false));
    if timelocked {
        ix.accounts
            .push(AccountMeta::new_readonly(timelock_address(&hook), false));
    }
    ix
}

/// `attest(args)`: Studio's attester attests `program`'s code (its ProgramData read and hashed).
pub fn attest(attester: Pubkey, program: Pubkey, args: AttestArgs) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: crate::accounts::Attest {
            attester,
            target: program,
            programdata: hook_program_data_address(&program),
            attestation: attestation_address(&program),
            system_program: system_program::ID,
            event_authority: event_authority(),
            program: crate::ID,
        }
        .to_account_metas(None),
        data: crate::instruction::Attest { args }.data(),
    }
}

/// `revoke`: the attester or the companion's upgrade authority revokes `program`'s attestation.
pub fn revoke(authority: Pubkey, program: Pubkey) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: crate::accounts::Revoke {
            authority,
            program_data: program_data_address(),
            attestation: attestation_address(&program),
            event_authority: event_authority(),
            program: crate::ID,
        }
        .to_account_metas(None),
        data: crate::instruction::Revoke {}.data(),
    }
}

/// The accounts an audit of `hook` is checked against: the program, its ProgramData and its
/// `Timelock` (read-only; unused addresses for a program that has none).
pub fn audit_accounts(hook: &Pubkey) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(*hook, false),
        AccountMeta::new_readonly(hook_program_data_address(hook), false),
        AccountMeta::new_readonly(timelock_address(hook), false),
    ]
}

/// `set_hook_status_v2(hook, args, audited_hash)`, signed by the program's upgrade authority, with
/// the hook's [`audit_accounts`].
pub fn set_hook_status_v2(
    authority: Pubkey,
    hook: Pubkey,
    args: HookStatusArgs,
    audited_hash: [u8; 32],
) -> Instruction {
    let mut ix = set_hook_status(authority, hook, args);
    ix.data = crate::instruction::SetHookStatusV2 {
        hook,
        args,
        audited_hash,
    }
    .data();
    ix.accounts.extend(audit_accounts(&hook));
    ix
}

/// [`set_hook_status`] (v1) with the hook's [`audit_accounts`], so the program can refuse an audit
/// of a timelocked or author-upgradeable hook (what the SDK sends; the bare builder is the deployed
/// instruction).
pub fn set_hook_status_checked(
    authority: Pubkey,
    hook: Pubkey,
    args: HookStatusArgs,
) -> Instruction {
    let mut ix = set_hook_status(authority, hook, args);
    ix.accounts.extend(audit_accounts(&hook));
    ix
}

// ---- Phase 3a: strategy games --------------------------------------------------------------------

/// A strategy game's terms: `PDA(["strategy", mint])`.
pub fn strategy_terms_address(mint: &Pubkey) -> Pubkey {
    StrategyTerms::address(mint).0
}

/// What `create_strategy_game` needs to vet the ticket hook and the strategy, and the strategy's
/// registry accounts: `hook_vetting` (the hook's [`vetting_accounts`], none for a hook with a
/// status or Bordrless's lottery hook), the strategy's ProgramData and, when `strategy_timelocked`,
/// its `Timelock`, then `extras` (what its registry names, resolved by the client).
pub fn create_strategy_game(
    payer: Pubkey,
    mint: Pubkey,
    args: CreateGameArgs,
    s: StrategyArgs,
    hook_vetting: Vec<AccountMeta>,
    strategy_timelocked: bool,
    extras: &[Pubkey],
) -> Instruction {
    let mut accounts = crate::accounts::CreateStrategyGame {
        payer,
        mint,
        companion: companion_address(&mint),
        game: game_address(&mint),
        terms: strategy_terms_address(&mint),
        hook_state: bordrless_game::state_address(&args.hook, &mint).0,
        hook_registry: hook_accounts_address(&args.hook, &mint).0,
        hook_status: hook_status_address(&args.hook),
        strategy: s.strategy,
        strategy_status: hook_status_address(&s.strategy),
        strategy_registry: bordrless_strategy::registry_address(&s.strategy, &mint).0,
        system_program: system_program::ID,
        event_authority: event_authority(),
        program: crate::ID,
    }
    .to_account_metas(None);
    accounts.extend(hook_vetting);
    accounts.push(AccountMeta::new_readonly(
        hook_program_data_address(&s.strategy),
        false,
    ));
    if strategy_timelocked {
        accounts.push(AccountMeta::new_readonly(
            timelock_address(&s.strategy),
            false,
        ));
    }
    accounts.extend(extras.iter().map(|k| AccountMeta::new_readonly(*k, false)));
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::CreateStrategyGame { args, s }.data(),
    }
}

/// What `plan_period` and `pay_strategy` read the strategy's class by (it is checked before every
/// question): its ProgramData and its timelock's address (an account that need not exist).
pub fn strategy_class_accounts(strategy: &Pubkey) -> [AccountMeta; 2] {
    [
        AccountMeta::new_readonly(hook_program_data_address(strategy), false),
        AccountMeta::new_readonly(timelock_address(strategy), false),
    ]
}

/// `plan_period(period)` of `mint`'s strategy game: `hook` its ticket hook, `strategy` its
/// strategy, `pool` the launch's pool, `extras` the terms' extras.
pub fn plan_period(
    cranker: Pubkey,
    mint: Pubkey,
    hook: Pubkey,
    strategy: Pubkey,
    pool: Pubkey,
    extras: &[Pubkey],
    period: u32,
) -> Instruction {
    let mut accounts = crate::accounts::PlanPeriod {
        cranker,
        companion: companion_address(&mint),
        game: game_address(&mint),
        terms: strategy_terms_address(&mint),
        hook_status: hook_status_address(&hook),
        strategy_status: hook_status_address(&strategy),
        launch: launch_client::launch_address(&mint),
        pool,
        hook_state: bordrless_game::state_address(&hook, &mint).0,
        strategy,
        event_authority: event_authority(),
        program: crate::ID,
    }
    .to_account_metas(None);
    accounts.extend(strategy_class_accounts(&strategy));
    accounts.extend(extras.iter().map(|k| AccountMeta::new_readonly(*k, false)));
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::PlanPeriod { period }.data(),
    }
}

/// `pay_strategy(period, n)` of `mint`'s strategy game for `owners` (each paid its entitlement;
/// `cranker` pays the receipts' rent and is paid the bounties).
pub fn pay_strategy(
    cranker: Pubkey,
    mint: Pubkey,
    hook: Pubkey,
    strategy: Pubkey,
    extras: &[Pubkey],
    period: u32,
    owners: &[Pubkey],
) -> Instruction {
    let creator = creator_address(&mint);
    let mut accounts = crate::accounts::PayStrategy {
        cranker,
        companion: companion_address(&mint),
        creator,
        game: game_address(&mint),
        terms: strategy_terms_address(&mint),
        hook_status: hook_status_address(&hook),
        strategy_status: hook_status_address(&strategy),
        launch: launch_client::launch_address(&mint),
        hook_state: bordrless_game::state_address(&hook, &mint).0,
        strategy,
        system_program: system_program::ID,
        event_authority: event_authority(),
        program: crate::ID,
    }
    .to_account_metas(None);
    accounts.extend(strategy_class_accounts(&strategy));
    accounts.extend(extras.iter().map(|k| AccountMeta::new_readonly(*k, false)));
    accounts.extend(unwrap_accounts(creator));
    for owner in owners {
        accounts.push(AccountMeta::new_readonly(
            token_client::holding_address(&mint, owner),
            false,
        ));
        accounts.push(AccountMeta::new(*owner, false));
        accounts.push(AccountMeta::new(
            receipt_address(&mint, period, owner),
            false,
        ));
    }
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::PayStrategy {
            period,
            n: owners.len() as u8,
        }
        .data(),
    }
}

// ---- Snowball (2026-10-12) ----------------------------------------------------------------------------

/// A Snowball's market maker: `PDA(["snowball", mint])`.
pub fn snowball_address(mint: &Pubkey) -> Pubkey {
    Snowball::address(mint).0
}

/// `create_snowball`: the companion of `mint` (whose keypair signs) becomes a Snowball, `payer`
/// paying the rent. After `create`, before `launch`.
pub fn create_snowball(payer: Pubkey, mint: Pubkey, args: CreateSnowballArgs) -> Instruction {
    let accounts = crate::accounts::CreateSnowball {
        payer,
        mint,
        companion: companion_address(&mint),
        snowball: snowball_address(&mint),
        system_program: system_program::ID,
        event_authority: event_authority(),
        program: crate::ID,
    }
    .to_account_metas(None);
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::CreateSnowball { args }.data(),
    }
}

/// `snowball_step` (anyone): the swap's accounts (a buy's, which a sell uses too), the pool, the
/// creator's token holding, the bridge's `unwrap_sol`'s. The swap's accounts carry the kit's
/// extras as the launch's modules name them (`LaunchKeys`), holder vault included when on. For a
/// custom-hook token, [`snowball_step_with`].
pub fn snowball_step(cranker: Pubkey, keys: &LaunchKeys) -> Instruction {
    snowball_step_with(cranker, keys, None, None)
}

/// `snowball_step` of a token with a custom hook: `buy` the hook's accounts resolved for a buy (the
/// pool vault's transfer to the creator address) and `sell` for a sell (the creator address's
/// transfer to the vault), as the client resolved them from the hook's registry; one given alone
/// serves both directions (a hook whose extras do not depend on them). The step trades one side,
/// which the program decides; it resolves that side's extras itself from the registry, passed too.
/// Without a hook, exactly [`snowball_step`].
pub fn snowball_step_with(
    cranker: Pubkey,
    keys: &LaunchKeys,
    buy: Option<&CustomHookAccounts>,
    sell: Option<&CustomHookAccounts>,
) -> Instruction {
    let (buy, sell) = (buy.or(sell), sell.or(buy));
    let creator = creator_address(&keys.mint);
    let mut extra = remaining(&creator_buy(keys, 0, 0, buy), &[]);
    if let Some(s) = sell {
        extra.extend(s.extras.iter().cloned());
    }
    extra.push(AccountMeta::new_readonly(
        launch_client::pool_address(&keys.mint, &keys.quote_mint, keys.lp_fee_bps),
        false,
    ));
    extra.extend(remaining(
        &token_client::create_holding(cranker, keys.mint, creator),
        &[],
    ));
    extra.extend(unwrap_accounts(creator));
    extra.extend(registry(&keys.mint, buy));
    let named = vec![
        AccountMeta::new(cranker, true),
        AccountMeta::new(companion_address(&keys.mint), false),
        AccountMeta::new(creator, false),
        AccountMeta::new_readonly(launch_client::launch_address(&keys.mint), false),
        AccountMeta::new(snowball_address(&keys.mint), false),
        AccountMeta::new_readonly(system_program::ID, false),
    ];
    build(named, extra, crate::instruction::SnowballStep {}.data())
}

// ---- Approvals and script companions (2026-10-12) ----------------------------------------------------

/// Bordrless's approval of `program`: `PDA(["approval", program])`.
pub fn approval_address(program: &Pubkey) -> Pubkey {
    Approval::address(program).0
}

/// A script companion's script: `PDA(["script", mint])`.
pub fn script_address(mint: &Pubkey) -> Pubkey {
    Script::address(mint).0
}

/// A script's state, made by `create_script` when it asks for one: `PDA(["script-state", mint])`.
pub fn script_state_address(mint: &Pubkey) -> Pubkey {
    Script::state_address(mint).0
}

/// `approve(program, kind, code_hash, author)`, signed (and paid the first time) by the companion
/// program's upgrade authority: `program`'s account and ProgramData are passed, its code hashed on
/// chain, and its `Timelock`'s address after them (read only when the program is timelocked; an
/// unused address otherwise), so its upgrade authority can be classed.
pub fn approve(
    authority: Pubkey,
    program: Pubkey,
    kind: u8,
    code_hash: [u8; 32],
    author: Pubkey,
) -> Instruction {
    let mut accounts = crate::accounts::Approve {
        authority,
        program_data: program_data_address(),
        target: program,
        target_programdata: hook_program_data_address(&program),
        approval: approval_address(&program),
        system_program: system_program::ID,
        event_authority: event_authority(),
        program: crate::ID,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(timelock_address(&program), false));
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::Approve {
            program,
            kind,
            code_hash,
            author,
        }
        .data(),
    }
}

/// `claim_fees` of a script companion: the plain claim, then its `Script` (writable: it keeps its
/// author's running total) and, when the claim may pay the script's author (`Script.author`, unless
/// it is the default key or the launch's beneficiary), the author's wallet (writable).
pub fn claim_fees_script(cranker: Pubkey, mint: Pubkey, author: Option<Pubkey>) -> Instruction {
    let mut ix = claim_fees(cranker, mint, None);
    ix.accounts
        .push(AccountMeta::new(script_address(&mint), false));
    if let Some(author) = author {
        ix.accounts.push(AccountMeta::new(author, false));
    }
    ix
}

/// `revoke_approval(program)`, signed by the companion program's upgrade authority.
pub fn revoke_approval(authority: Pubkey, program: Pubkey) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: crate::accounts::RevokeApproval {
            authority,
            program_data: program_data_address(),
            approval: approval_address(&program),
            event_authority: event_authority(),
            program: crate::ID,
        }
        .to_account_metas(None),
        data: crate::instruction::RevokeApproval { program }.data(),
    }
}

/// `create_script`: the companion of `mint` (whose keypair signs) becomes a script companion running
/// `args.program`, `payer` paying the rent of the script account and of the script's state
/// (`args.state_len` bytes at [`script_state_address`], made by the companion; none when 0). After
/// `create`, before `launch`. `script_extra`: the accounts the script's `companion_init` takes after
/// its four (`[creator, mint, state (or the system program's id), system program]`), as the script
/// documents them; forwarded with their writability, never as signers (the payer is never passed).
pub fn create_script(
    payer: Pubkey,
    mint: Pubkey,
    args: CreateScriptArgs,
    script_extra: Vec<AccountMeta>,
) -> Instruction {
    let creator = creator_address(&mint);
    let program = args.program;
    let mut accounts = crate::accounts::CreateScript {
        payer,
        mint,
        companion: companion_address(&mint),
        script: script_address(&mint),
        script_state: script_state_address(&mint),
        creator,
        creator_quote: token_client::holding_address(&BRIDGED_SOL_MINT, &creator),
        approval: approval_address(&program),
        script_program: program,
        script_programdata: hook_program_data_address(&program),
        system_program: system_program::ID,
        event_authority: event_authority(),
        program: crate::ID,
    }
    .to_account_metas(None);
    accounts.extend(script_extra);
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::CreateScript { args }.data(),
    }
}

/// `script_step` (anyone) of the script companion of `keys.mint`, whose script is `script_program`:
/// `extra` are the accounts the script's `companion_step` takes after its six (`[creator, mint,
/// launch, companion, pool, state (or the system program's id)]`), as the script documents them;
/// then the creator's token holding's `create_holding` accounts (the sender pays its rent the first
/// time) and the bridge's `unwrap_sol` accounts (the bounty). Every remaining account is forwarded
/// to the script with its writability, never as a signer.
pub fn script_step(
    cranker: Pubkey,
    keys: &LaunchKeys,
    script_program: Pubkey,
    extra: Vec<AccountMeta>,
) -> Instruction {
    let mint = keys.mint;
    let creator = creator_address(&mint);
    let mut accounts = crate::accounts::ScriptStep {
        cranker,
        companion: companion_address(&mint),
        creator,
        launch: launch_client::launch_address(&mint),
        mint,
        pool: keys.pool(),
        script: script_address(&mint),
        script_state: script_state_address(&mint),
        approval: approval_address(&script_program),
        script_program,
        script_programdata: hook_program_data_address(&script_program),
        system_program: system_program::ID,
        event_authority: event_authority(),
        program: crate::ID,
    }
    .to_account_metas(None);
    accounts.extend(extra);
    accounts.extend(remaining(
        &token_client::create_holding(cranker, mint, creator),
        &[],
    ));
    accounts.extend(unwrap_accounts(creator));
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::ScriptStep {}.data(),
    }
}

// ---- Payouts (Loss cover, 2026-10-12) ----------------------------------------------------------------

/// A script companion's payout terms: `PDA(["payout-terms", mint])`.
pub fn payout_terms_address(mint: &Pubkey) -> Pubkey {
    PayoutTerms::address(mint).0
}

/// The one signer of a hook's `record_payout`: `PDA(["payout-authority"])`, a program-wide address.
pub fn payout_authority_address() -> Pubkey {
    PAYOUT_AUTHORITY
}

/// `create_script_v2`: [`create_script`] with the script's payout terms, made at
/// [`payout_terms_address`], `payer` paying their rent too.
pub fn create_script_v2(
    payer: Pubkey,
    mint: Pubkey,
    args: CreateScriptArgs,
    payout: PayoutTermsArgs,
    script_extra: Vec<AccountMeta>,
) -> Instruction {
    let creator = creator_address(&mint);
    let program = args.program;
    let mut accounts = crate::accounts::CreateScriptV2 {
        payer,
        mint,
        companion: companion_address(&mint),
        script: script_address(&mint),
        script_state: script_state_address(&mint),
        creator,
        creator_quote: token_client::holding_address(&BRIDGED_SOL_MINT, &creator),
        approval: approval_address(&program),
        script_program: program,
        script_programdata: hook_program_data_address(&program),
        terms: payout_terms_address(&mint),
        system_program: system_program::ID,
        event_authority: event_authority(),
        program: crate::ID,
    }
    .to_account_metas(None);
    accounts.extend(script_extra);
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::CreateScriptV2 { args, payout }.data(),
    }
}

/// The accounts a step that answers payouts needs beside the script's own, as the program looks
/// them up: the terms (writable), the launch's hook program `hook`, the payout authority, the hook's
/// state for the mint (writable) and its `["hook-authority"]`, the token program and its event
/// authority. The candidates follow: each holding (writable: the hook records into it) and, when
/// it is paid, its owner (writable).
pub fn payout_accounts(mint: &Pubkey, hook: &Pubkey) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new(payout_terms_address(mint), false),
        AccountMeta::new_readonly(*hook, false),
        AccountMeta::new_readonly(PAYOUT_AUTHORITY, false),
        AccountMeta::new(bordrless_game::state_address(hook, mint).0, false),
        AccountMeta::new_readonly(bordrless_hook::hook_authority(hook).0, false),
        AccountMeta::new_readonly(TOKEN_ID, false),
        AccountMeta::new_readonly(token_client::event_authority(), false),
    ]
}

/// The hook's `record_payout(lamports, tag)` for `holding` of `mint`, exactly as `script_step`
/// builds and signs it (the SDK's reference; never sent on its own: only the companion signs for
/// the payout authority).
pub fn record_payout(
    hook: Pubkey,
    mint: Pubkey,
    holding: Pubkey,
    lamports: u64,
    tag: u32,
) -> Instruction {
    let mut data = Vec::with_capacity(8 + 8 + 4);
    data.extend_from_slice(&RECORD_PAYOUT_DISCRIMINATOR);
    data.extend_from_slice(&lamports.to_le_bytes());
    data.extend_from_slice(&tag.to_le_bytes());
    Instruction {
        program_id: hook,
        accounts: vec![
            AccountMeta::new_readonly(PAYOUT_AUTHORITY, true),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new(holding, false),
            AccountMeta::new(bordrless_game::state_address(&hook, &mint).0, false),
            AccountMeta::new_readonly(bordrless_hook::hook_authority(&hook).0, false),
            AccountMeta::new_readonly(TOKEN_ID, false),
            AccountMeta::new_readonly(token_client::event_authority(), false),
        ],
        data,
    }
}

/// The payees of `mint`'s companion: `PDA(["payees", mint])`.
pub fn payees_address(mint: &Pubkey) -> Pubkey {
    crate::state::Payees::address(mint).0
}

/// `set_payees`: before the launch, `mint` (the launch's keypair) signing, `payer` paying the rent.
pub fn set_payees(
    payer: Pubkey,
    mint: Pubkey,
    entries: Vec<crate::instructions::PayeeArgs>,
) -> Instruction {
    let accounts = crate::accounts::SetPayees {
        payer,
        mint,
        companion: companion_address(&mint),
        payees: payees_address(&mint),
        system_program: system_program::ID,
        event_authority: event_authority(),
        program: crate::ID,
    }
    .to_account_metas(None);
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::SetPayees { entries }.data(),
    }
}

/// `withdraw` of a companion with payees: [`withdraw`], then its payees account and each payee
/// (writable), whom it pays their parts first.
pub fn withdraw_with_payees(
    sender: Pubkey,
    mint: Pubkey,
    beneficiary: Pubkey,
    payees: &[Pubkey],
) -> Instruction {
    let mut ix = withdraw(sender, mint, beneficiary);
    ix.accounts
        .push(AccountMeta::new(payees_address(&mint), false));
    ix.accounts
        .extend(payees.iter().map(|p| AccountMeta::new(*p, false)));
    ix
}

/// `allow_wallet_payouts`: before the launch, `mint` (the launch's keypair) signing, the script may
/// pay any wallet as well as holders, within its payout terms.
pub fn allow_wallet_payouts(mint: Pubkey) -> Instruction {
    let accounts = crate::accounts::AllowWalletPayouts {
        mint,
        companion: companion_address(&mint),
        terms: payout_terms_address(&mint),
        script: script_address(&mint),
        event_authority: event_authority(),
        program: crate::ID,
    }
    .to_account_metas(None);
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::AllowWalletPayouts {}.data(),
    }
}

/// `set_payout_terms`: `authority` (the launch's beneficiary or the script approver) replaces the
/// payout terms of `mint`'s script.
pub fn set_payout_terms(authority: Pubkey, mint: Pubkey, payout: PayoutTermsArgs) -> Instruction {
    let accounts = crate::accounts::SetPayoutTerms {
        authority,
        companion: companion_address(&mint),
        terms: payout_terms_address(&mint),
        event_authority: event_authority(),
        program: crate::ID,
    }
    .to_account_metas(None);
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::SetPayoutTerms { payout }.data(),
    }
}
