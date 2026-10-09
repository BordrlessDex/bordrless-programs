//! Builders for the companion's instructions, as the tests and the SDK build them. Each step's
//! remaining accounts are the accounts of the instructions it invokes, built with the callees'
//! own clients exactly as the program builds them on chain; the program looks them up by key. The
//! creator address signs only inside the program, so it is never marked a signer here.
//!
//! A game coin's token instructions carry its custom hook: the `_with` builders take the hook's
//! accounts as the client resolved them from its registry (`CustomHookAccounts`), and add the
//! registry, which the program resolves them from itself. Without one they build exactly what the
//! plain builders build.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::{system_program, InstructionData, ToAccountMetas};
use bordrless_hook::hook_accounts_address;
use bordrless_launch::client::{self as launch_client, CustomHookAccounts, LaunchKeys};
use bordrless_launch::instructions::CreateLaunchArgs;
use bordrless_token::client::{self as token_client, Hook};

use crate::constants::*;
use crate::instructions::{CreateArgs, CreateGameArgs, GameKindArgs, HookStatusArgs};
use crate::oracle;
use crate::state::{Companion, Game, HookStatus, ShareReceipt};

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
