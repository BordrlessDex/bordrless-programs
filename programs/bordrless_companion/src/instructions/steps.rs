//! A launched companion's steps. `dev_buy` and `withdraw` serve the beneficiary; `claim_fees`,
//! `buyback`, `share` and `release` anyone may send, each paying its sender `bounty_bps` of what it
//! moves (as SOL). Every step is one call into a Bordrless program (plus the bridge's `unwrap_sol`
//! and a system transfer for a bounty), built here with that program's own client (`invoke.rs`).
//!
//! A game coin's mint has a custom token hook (`Launch.custom_hook`, the game's), and since
//! 2026-10-12 any companion's mint may have one: every token instruction a step builds then carries
//! that hook and its extra accounts, resolved from the hook's registry exactly as the token
//! program's callers resolve them. A mint whose hook is the kit, or none, takes the very
//! instructions it always did.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::system_program;
use bordrless_core::swap_out;
use bordrless_hook::{hook_accounts_address, HookAccountList};
use bordrless_kit::state::KitConfig;
use bordrless_launch::client as launch_client;
use bordrless_launch::state::Launch;
use bordrless_swap::state::Pool;
use bordrless_token::client::{self as token_client, Hook};

use crate::constants::*;
use crate::error::CompanionError;
use crate::events::*;
use crate::instructions::CreatorSeeds;
use crate::invoke::invoke_built;
use crate::state::*;

/// The launch's mint as token instructions take it: the kit and its two extras with kit rules,
/// nothing without.
fn mint_hook(launch: &Launch) -> (Option<Hook>, Vec<AccountMeta>) {
    if launch.modules == 0 {
        return (None, vec![]);
    }
    let vault = launch.rules.rewards_on().then_some(launch.holder_vault);
    (
        Some(Hook::of(KIT_ID)),
        bordrless_launch::cpi::kit_extras(launch.kit_config, vault).to_vec(),
    )
}

/// The keys a token operation hands the mint's hook (its callback's prefix, and the owners): what a
/// custom hook's registry resolves its extra accounts from.
pub(crate) struct OpKeys {
    pub source: Pubkey,
    pub destination: Pubkey,
    pub authority: Pubkey,
    pub source_owner: Pubkey,
    pub destination_owner: Pubkey,
}

/// A custom hook's extra accounts for a token operation on `mint`, resolved from the hook's
/// registry `PDA(["bordrless-hook-accounts", mint], hook)` (owner and address checked), with the
/// token program's callback prefix: its signer for the hook, the mint, the source, the destination
/// and the authority.
fn custom_extras(
    available: &[AccountInfo],
    hook: &Pubkey,
    mint: &Pubkey,
    op: &OpKeys,
) -> Result<Vec<AccountMeta>> {
    let info = find(available, &hook_accounts_address(hook, mint).0)?;
    require_keys_eq!(*info.owner, *hook, CompanionError::HookRegistry);
    let list = decode_registry(info).ok_or_else(|| error!(CompanionError::HookRegistry))?;
    let prefix = [
        token_client::hook_signer(hook),
        *mint,
        op.source,
        op.destination,
        op.authority,
    ];
    list.resolve(&prefix, &op.source_owner, &op.destination_owner)
        .ok_or_else(|| error!(CompanionError::HookRegistry))
}

/// A hook's registry (`HookAccountList`), decoded only once its bytes are seen to stay within the
/// companion's bounds (`MAX_REGISTRY_*`): the decode builds vectors on the heap, sized by counts the
/// hook wrote, which unchecked could exhaust the 32 KiB heap (an abort, not an error). `None` for
/// a registry out of bounds or malformed.
pub(crate) fn decode_registry(info: &AccountInfo) -> Option<HookAccountList> {
    let data = info.try_borrow_data().ok()?;
    if !registry_within_bounds(&data) {
        return None;
    }
    HookAccountList::decode(&data)
}

/// Walks a registry's Borsh bytes (the magic, the version, then each account: `writable`, a source
/// tag, a key or a PDA's program and seeds) without allocating: at most `MAX_REGISTRY_LEN` bytes,
/// `MAX_REGISTRY_ACCOUNTS` accounts, `MAX_REGISTRY_SEEDS` seeds a PDA, `MAX_REGISTRY_SEED_LEN` bytes a
/// literal seed, and no byte after the last account.
pub(crate) fn registry_within_bounds(data: &[u8]) -> bool {
    fn take<'a>(data: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        let (head, rest) = (data.get(..n)?, data.get(n..)?);
        *data = rest;
        Some(head)
    }
    fn len(data: &mut &[u8], max: usize) -> Option<usize> {
        let mut b = [0u8; 4];
        b.copy_from_slice(take(data, 4)?);
        let n = usize::try_from(u32::from_le_bytes(b)).ok()?;
        (n <= max).then_some(n)
    }
    fn walk(mut data: &[u8]) -> Option<()> {
        if data.len() > MAX_REGISTRY_LEN {
            return None;
        }
        take(&mut data, 8 + 1)?;
        for _ in 0..len(&mut data, MAX_REGISTRY_ACCOUNTS)? {
            take(&mut data, 1)?;
            match take(&mut data, 1)?[0] {
                0 => {
                    take(&mut data, 32)?;
                }
                1 => {
                    take(&mut data, 32)?;
                    for _ in 0..len(&mut data, MAX_REGISTRY_SEEDS)? {
                        match take(&mut data, 1)?[0] {
                            0 => {
                                let n = len(&mut data, MAX_REGISTRY_SEED_LEN)?;
                                take(&mut data, n)?;
                            }
                            1 => {
                                take(&mut data, 1)?;
                            }
                            2 | 3 => {}
                            _ => return None,
                        }
                    }
                }
                _ => return None,
            }
        }
        data.is_empty().then_some(())
    }
    walk(data).is_some()
}

/// The launch's mint as a token instruction for `op` takes it: the kit's ([`mint_hook`], as it
/// always was), or the custom hook's with its extras resolved from its registry.
fn token_hook(
    available: &[AccountInfo],
    launch: &Launch,
    op: &OpKeys,
) -> Result<(Option<Hook>, Vec<AccountMeta>)> {
    match launch.custom_hook {
        None => Ok(mint_hook(launch)),
        Some(hook) => Ok((
            Some(Hook::of(hook)),
            custom_extras(available, &hook, &launch.mint, op)?,
        )),
    }
}

/// A buy on the launch's pool by the creator address, delivered to it: the launchpad client's
/// `swap` for a kit (or hook-less) token, as it always was; for a custom-hook token, with the
/// hook's slice on the token's side, resolved for the pool vault's transfer to the creator.
pub(crate) fn creator_buy(
    available: &[AccountInfo],
    launch: &Launch,
    creator: Pubkey,
    amount_in: u64,
    min_out: u64,
    native: bool,
) -> Result<Instruction> {
    let keys = launch_client::LaunchKeys::of(launch);
    let Some(hook) = launch.custom_hook else {
        if native {
            return Ok(creator_swap_v2(
                &keys,
                creator,
                1,
                amount_in,
                min_out,
                launch_client::base_hook_slice(&keys.mint, &keys.quote_mint, keys.modules),
            ));
        }
        return Ok(launch_client::swap(
            &keys, creator, creator, 1, amount_in, min_out,
        ));
    };
    let op = OpKeys {
        source: bordrless_swap::client::vault_address(&launch.pool, &launch.mint),
        destination: token_client::holding_address(&launch.mint, &creator),
        authority: launch.pool,
        source_owner: launch.pool,
        destination_owner: creator,
    };
    let extras = custom_extras(available, &hook, &launch.mint, &op)?;
    let slice = launch_client::custom_hook_slice(hook, extras);
    if native {
        return Ok(creator_swap_v2(
            &keys, creator, 1, amount_in, min_out, slice,
        ));
    }
    Ok(launch_client::swap_with_base_slice(
        &keys, creator, creator, 1, amount_in, min_out, slice,
    ))
}

/// A native companion's trade: the DEX `swap_v2`, the creator address the trader and the
/// recipient, paying a buy and taking a sell's SOL as its own lamports. No bridge accounts: a
/// native launch's cuts are lamports.
fn creator_swap_v2(
    keys: &launch_client::LaunchKeys,
    creator: Pubkey,
    direction: u8,
    amount_in: u64,
    min_out: u64,
    base_slice: Vec<AccountMeta>,
) -> Instruction {
    launch_client::swap_v2_with_bridge(
        keys, creator, creator, direction, amount_in, min_out, base_slice, false,
    )
}

/// A sell on the launch's pool by the creator address, the SOL delivered to it: the launchpad
/// client's `swap` for a kit (or hook-less) token; for a custom-hook token, with the hook's slice on
/// the token's side, resolved for the creator's transfer to the pool's vault (the creator signing).
pub(crate) fn creator_sell(
    available: &[AccountInfo],
    launch: &Launch,
    creator: Pubkey,
    amount_in: u64,
    min_out: u64,
    native: bool,
) -> Result<Instruction> {
    let keys = launch_client::LaunchKeys::of(launch);
    let Some(hook) = launch.custom_hook else {
        if native {
            return Ok(creator_swap_v2(
                &keys,
                creator,
                0,
                amount_in,
                min_out,
                launch_client::base_hook_slice(&keys.mint, &keys.quote_mint, keys.modules),
            ));
        }
        return Ok(launch_client::swap(
            &keys, creator, creator, 0, amount_in, min_out,
        ));
    };
    let op = OpKeys {
        source: token_client::holding_address(&launch.mint, &creator),
        destination: bordrless_swap::client::vault_address(&launch.pool, &launch.mint),
        authority: creator,
        source_owner: creator,
        destination_owner: launch.pool,
    };
    let extras = custom_extras(available, &hook, &launch.mint, &op)?;
    let slice = launch_client::custom_hook_slice(hook, extras);
    if native {
        return Ok(creator_swap_v2(
            &keys, creator, 0, amount_in, min_out, slice,
        ));
    }
    Ok(launch_client::swap_with_base_slice(
        &keys, creator, creator, 0, amount_in, min_out, slice,
    ))
}

/// An account of `T`'s program, deserialized (owner and discriminator checked).
pub(crate) fn read<T: AccountDeserialize + Owner>(info: &AccountInfo) -> Result<T> {
    require_keys_eq!(*info.owner, T::owner(), CompanionError::MissingAccount);
    let data = info.try_borrow_data()?;
    T::try_deserialize(&mut &data[..])
}

pub(crate) fn find<'a, 'info>(
    available: &'a [AccountInfo<'info>],
    key: &Pubkey,
) -> Result<&'a AccountInfo<'info>> {
    available
        .iter()
        .find(|a| a.key == key)
        .ok_or_else(|| error!(CompanionError::MissingAccount))
}

/// A script companion's `Script` of `mint` among `available` (address and owner checked), which
/// must be passed writable: a fee claim keeps its author's running total in it.
fn script_of<'a, 'info>(
    available: &'a [AccountInfo<'info>],
    mint: &Pubkey,
) -> Result<(&'a AccountInfo<'info>, Script)> {
    let info = find(available, &Script::address(mint).0)?;
    require!(info.is_writable, CompanionError::MissingAccount);
    Ok((info, read::<Script>(info)?))
}

/// The launch's kit config, when it has kit rules.
fn kit_config(available: &[AccountInfo], launch: &Launch) -> Result<Option<KitConfig>> {
    if launch.modules == 0 {
        return Ok(None);
    }
    Ok(Some(read::<KitConfig>(find(
        available,
        &launch.kit_config,
    )?)?))
}

/// What the kit's max wallet still lets `holder` (now holding `held`) receive, while it applies.
fn max_wallet_room(kit: &Option<KitConfig>, held: u64) -> Option<u64> {
    let kit = kit.as_ref()?;
    (kit.has(bordrless_kit::constants::modules::MAX_WALLET) && !kit.graduated)
        .then(|| kit.max_wallet_amount.saturating_sub(held))
}

/// A holding's balance; 0 for one that does not exist yet.
pub(crate) fn balance(available: &[AccountInfo], mint: &Pubkey, owner: &Pubkey) -> Result<u64> {
    let info = find(available, &token_client::holding_address(mint, owner))?;
    if info.owner != &TOKEN_ID {
        return Ok(0);
    }
    Ok(token_client::read_holding(info)?.amount)
}

/// Creates `owner`'s holding of `mint` when it does not exist, `payer` (a signer) paying.
pub(crate) fn ensure_holding(
    available: &[AccountInfo],
    payer: Pubkey,
    mint: Pubkey,
    owner: Pubkey,
) -> Result<()> {
    let info = find(available, &token_client::holding_address(&mint, &owner))?;
    if info.owner == &TOKEN_ID {
        return Ok(());
    }
    invoke_built(
        &token_client::create_holding(payer, mint, owner),
        available,
        &[],
    )
}

/// Pays `to` `lamports` of the creator's bridged SOL as SOL: unwrapped to the creator address,
/// then sent on (the creator address ends with what it had).
pub(crate) fn pay_sol<'info>(
    available: &[AccountInfo<'info>],
    seeds: &CreatorSeeds,
    creator: Pubkey,
    to: &AccountInfo<'info>,
    lamports: u64,
) -> Result<()> {
    if lamports == 0 {
        return Ok(());
    }
    // A native companion's SOL is already the creator address's lamports.
    if !seeds.native {
        invoke_built(
            &bordrless_bridge::client::unwrap_sol(creator, lamports),
            available,
            &[&seeds.seeds()],
        )?;
    }
    let from = find(available, &creator)?.clone();
    let system = find(available, &system_program::ID)?.clone();
    system_program::transfer(
        CpiContext::new_with_signer(
            *system.key,
            system_program::Transfer {
                from,
                to: to.clone(),
            },
            &[&seeds.seeds()],
        ),
        lamports,
    )
}

/// The SOL the creator holds: its bridged SOL, or for a native companion its lamports above the
/// rent-exempt minimum of an account with no data (what every pot and bucket is counted against).
pub(crate) fn sol_held(
    available: &[AccountInfo],
    seeds: &CreatorSeeds,
    creator: &Pubkey,
) -> Result<u64> {
    if seeds.native {
        let info = find(available, creator)?;
        return Ok(info
            .lamports()
            .saturating_sub(Rent::get()?.minimum_balance(0)));
    }
    balance(available, &BRIDGED_SOL_MINT, creator)
}

/// The creator fees the launch holds unclaimed: its bridged-SOL holding's, or for a native
/// companion (whose launch is native) the launch's lamports above its rent-exempt minimum.
pub(crate) fn fees_waiting(
    available: &[AccountInfo],
    seeds: &CreatorSeeds,
    launch: &Pubkey,
) -> Result<u64> {
    if seeds.native {
        let info = find(available, launch)?;
        return Ok(info
            .lamports()
            .saturating_sub(Rent::get()?.minimum_balance(info.data_len())));
    }
    balance(available, &BRIDGED_SOL_MINT, launch)
}

pub(crate) fn available<'info>(
    named: Vec<AccountInfo<'info>>,
    remaining: &[AccountInfo<'info>],
) -> Vec<AccountInfo<'info>> {
    let mut all = named;
    all.extend_from_slice(remaining);
    all
}

// ---- dev_buy -----------------------------------------------------------------------------------

/// Accounts of `dev_buy`. Remaining: the bridge's `wrap_sol` accounts and the DEX `swap`'s, the
/// creator's token holding (created here when missing), the token program's.
#[event_cpi]
#[derive(Accounts)]
pub struct DevBuy<'info> {
    #[account(mut, address = companion.beneficiary)]
    pub beneficiary: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked).
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    #[account(address = launch_client::launch_address(&companion.mint))]
    pub launch: Box<Account<'info, Launch>>,
    pub system_program: Program<'info, System>,
}

/// `dev_buy(lamports, min_out)`: the beneficiary's buy, made by the companion and held by it,
/// vesting with the dev bag; within `DEV_BUY_WINDOW` of the launch, and never above what max wallet
/// allows a wallet before graduation.
pub fn process_dev_buy<'info>(
    ctx: Context<'info, DevBuy<'info>>,
    lamports: u64,
    min_out: u64,
) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let c = &ctx.accounts.companion;
    require!(c.launched, CompanionError::NotLaunched);
    require!(lamports > 0, CompanionError::NothingToDo);
    require!(
        now <= c.launched_at.saturating_add(DEV_BUY_WINDOW),
        CompanionError::DevBuyWindowClosed
    );
    let (mint, creator) = (c.mint, ctx.accounts.creator.key());
    let seeds = CreatorSeeds::of(c);
    system_program::transfer(
        CpiContext::new(
            ctx.accounts.system_program.key(),
            system_program::Transfer {
                from: ctx.accounts.beneficiary.to_account_info(),
                to: ctx.accounts.creator.to_account_info(),
            },
        ),
        lamports,
    )?;
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    ensure_holding(&all, ctx.accounts.beneficiary.key(), mint, creator)?;
    // A native companion buys with the lamports as they are.
    if !seeds.native {
        invoke_built(
            &bordrless_bridge::client::wrap_sol(creator, lamports),
            &all,
            &[&seeds.seeds()],
        )?;
    }
    let before = balance(&all, &mint, &creator)?;
    let buy = creator_buy(
        &all,
        &ctx.accounts.launch,
        creator,
        lamports,
        min_out,
        seeds.native,
    )?;
    invoke_built(&buy, &all, &[&seeds.seeds()])?;
    let got = balance(&all, &mint, &creator)?
        .checked_sub(before)
        .ok_or(CompanionError::MathOverflow)?;
    let dev_tokens = c
        .dev_tokens
        .checked_add(got)
        .ok_or(CompanionError::MathOverflow)?;
    // The companion is uncapped (the kit excludes it): the bag is held to what one wallet could hold.
    let kit = kit_config(&all, &ctx.accounts.launch)?;
    if let Some(room) = max_wallet_room(&kit, 0) {
        require!(dev_tokens <= room, CompanionError::DevBagOverMaxWallet);
    }
    let c = &mut ctx.accounts.companion;
    c.dev_tokens = dev_tokens;
    emit_cpi!(DevBought {
        companion: c.key(),
        lamports,
        tokens: got,
        dev_tokens
    });
    Ok(())
}

// ---- claim_fees ----------------------------------------------------------------------------------

/// Accounts of the permissionless steps.
#[event_cpi]
#[derive(Accounts)]
pub struct Step<'info> {
    /// Whoever sends it; paid the bounty.
    #[account(mut)]
    pub cranker: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked).
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    #[account(address = launch_client::launch_address(&companion.mint))]
    pub launch: Box<Account<'info, Launch>>,
    pub system_program: Program<'info, System>,
}

/// `claim_fees`: the launch's creator fees claimed into the creator's holding (when it holds any),
/// the bounty paid, the rest split. What is split is everything the holding holds above what is
/// already set aside, so bridged SOL that reached it any other way is split too, never stranded.
/// Remaining: `claim_creator_fees`'s accounts and the bridge's `unwrap_sol` accounts; for a game,
/// also its hook's status account (`PDA(["hook-status", hook])`, which need not exist).
///
/// A game's split has a fourth part, the pot, which takes the rounding. While the game's hook is not
/// audited the pot holds at most the hook's cap, and the pot's share above it goes to the buyback;
/// once the hook is blocked, all of it does, and the pot with it. Under a block, a claim that
/// credits the buyback at least what it held restarts the stranded wait
/// ([`Companion::restart_stranded_wait`]). A Snowball's or a script's pot has no hook: its part is
/// credited whole ([`Companion::has_own_pot`]).
///
/// A script companion's claim (2026-10-12; 2026-10-16: out of the launcher's part alone) pays the
/// script's author (`Script.author`, copied from the script's approval) `AUTHOR_SHARE_BPS` of the
/// launcher's part of the creator fee this claim took from the launch, as SOL to their wallet: out
/// of that part, never the pot's, the buyback's or the holders', never the hook's SOL cut; not when
/// the author is the launch's beneficiary (who gets no extra share), none, or an account no transfer
/// can pay (a program, a reserved key) (`FeesClaimed.claimed` and `claimed_total` count the claim
/// less the author's share). Its remaining accounts also
/// hold its `Script` (writable: the author's running total) and, when the author is to be paid,
/// the author's wallet (writable).
///
/// The hook's SOL cut (2026-10-15): a launch whose custom hook takes a SOL cut pays it into the
/// same vault as the creator fee (`Launch::quote_cut_accrued` counts it). Of each claim, the cut
/// no claim has taken yet (`accrued - Companion.quote_cut_claimed`, at most the whole claim; any
/// rest waits for the next claim) is the hook's: after the bounty (of the whole claim) its part of
/// what is left, `floor(rest * hook / claim)`, goes whole to the pot's
/// bucket (`FeesClaimed.from_hook`), and only the rest is split by the shares. A companion made
/// before is grown by 8 bytes on its first claim since, the cranker paying their rent, and counts
/// every cut taken before as claimed ([`Companion::grow_for_quote_cut`]).
pub fn process_claim_fees<'info>(ctx: Context<'info, Step<'info>>) -> Result<()> {
    // The hook's SOL cut so far. The launchpad's claim below moves the vault, never this counter.
    let accrued = ctx.accounts.launch.quote_cut_accrued();
    {
        // A companion made before `quote_cut_claimed`: grown, and counted as having taken all the
        // cut so far (the cranker pays the 8 bytes' rent).
        let info = ctx.accounts.companion.to_account_info();
        let payer = ctx.accounts.cranker.to_account_info();
        let system = ctx.accounts.system_program.to_account_info();
        ctx.accounts
            .companion
            .grow_for_quote_cut(&info, &payer, &system, accrued, &[])?;
    }
    let c = &ctx.accounts.companion;
    require!(c.launched, CompanionError::NotLaunched);
    let (mint, creator) = (c.mint, ctx.accounts.creator.key());
    let seeds = CreatorSeeds::of(c);
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let terms = if c.is_game() {
        Some(hook_terms(&all, &c.game_hook, &mint)?)
    } else {
        None
    };
    let script = if c.is_script() {
        Some(script_of(&all, &mint)?)
    } else {
        None
    };
    let launch_key = launch_client::launch_address(&mint);
    let before = sol_held(&all, &seeds, &creator)?;
    if fees_waiting(&all, &seeds, &launch_key)? > 0 {
        // A native companion's launch pays its fees as lamports to the creator address.
        let claim = if seeds.native {
            launch_client::claim_creator_fees_sol(creator, mint, None, None)
        } else {
            launch_client::claim_creator_fees(creator, mint, BRIDGED_SOL_MINT)
        };
        invoke_built(&claim, &all, &[&seeds.seeds()])?;
    }
    let holding = sol_held(&all, &seeds, &creator)?;
    let set_aside = c.set_aside().ok_or(CompanionError::MathOverflow)?;
    let got = holding.saturating_sub(set_aside);
    require!(got > 0, CompanionError::NothingToDo);
    // The hook's part of this claim: the cut the launch took that no claim has taken yet, up to
    // the whole claim (the rest of it waits for the next one).
    let hook_gross = got.min(accrued.saturating_sub(c.quote_cut_claimed.amount));
    let claimed_gross = got;
    // A script's author (2026-10-16, the owner: "it should only come from the creator fee; the
    // companion fee is only for the pot"): `AUTHOR_SHARE_BPS` of the launcher's part of the creator
    // fee this claim took from the launch, taken out of that part alone: never of the pot's, the
    // buyback's or the holders' parts, never of the hook's SOL cut (which goes whole to the pot),
    // never of SOL that reached the holding any other way. No author (the default key: an approval
    // naming nobody, or an immutable script, which has none), the launch's beneficiary, or an
    // author no transfer can pay (a program, or one of the runtime's reserved keys, which no
    // transaction can write) is paid nothing, so no claim is ever stuck on it.
    let wallet = match &script {
        Some((_, s)) if s.author != Pubkey::default() && s.author != c.beneficiary => {
            let wallet = find(&all, &s.author)?;
            (!wallet.executable && !RESERVED_KEYS.contains(&s.author)).then(|| wallet.clone())
        }
        _ => None,
    };
    // The creator fee this claim took from the launch: what it took, less the hook's cut in it.
    let fee_taken = holding
        .saturating_sub(before)
        .saturating_sub(hook_gross);
    let to_author = match wallet {
        Some(_) => bps_of(
            bps_of(fee_taken, u64::from(c.split.beneficiary_bps)),
            u64::from(AUTHOR_SHARE_BPS),
        ),
        None => 0,
    };
    let bounty = bps_of(got, u64::from(c.bounty_bps));
    let rest = got - bounty;
    // The bounty came from the whole claim, so the hook's part of what is left is its share of the
    // claim: `rest * hook_gross / claimed_gross` (rounded down; the rounding stays with the split).
    // `base` is the rest, split by the shares as ever.
    let from_hook = hook_part(rest, hook_gross, claimed_gross)?;
    let base = rest - from_hook;
    let rest = base;
    let to_holders = bps_of(rest, u64::from(c.split.holders_bps));
    let to_beneficiary = bps_of(rest, u64::from(c.split.beneficiary_bps));
    // The rounding goes to the pot when there is one, else to buybacks when there are any, else to
    // the beneficiary.
    let (to_buyback, to_beneficiary, to_pot) = if c.pot_bps > 0 {
        let to_buyback = bps_of(rest, u64::from(c.split.buyback_bps));
        let to_pot = rest
            .checked_sub(to_holders)
            .and_then(|v| v.checked_sub(to_beneficiary))
            .and_then(|v| v.checked_sub(to_buyback))
            .ok_or(CompanionError::MathOverflow)?;
        (to_buyback, to_beneficiary, to_pot)
    } else if c.split.buyback_bps > 0 {
        (rest - to_holders - to_beneficiary, to_beneficiary, 0)
    } else {
        (0, rest - to_holders, 0)
    };
    // The hook's part, whole, to the pot's bucket: the pot (a game's, a Snowball's or a script's),
    // else the buyback, else the holders. A split of the beneficiary alone has no other bucket
    // anything could ever spend from (no buyback limits, no reward pool): there it goes to the
    // beneficiary, as the whole claim did before (`FeesClaimed.from_hook` still tells it apart).
    // The author's share, out of the launcher's part alone (before the hook's part is added to any
    // bucket, so never of the cut): at most half of the launcher's part of the fee taken, so within
    // what the launcher's part holds.
    // Capped at the launcher's part, so a claim is never stuck on it (the fee taken is measured
    // from the creator address's balance, the split from what is above its set-aside).
    let to_author = to_author.min(to_beneficiary);
    let to_beneficiary = to_beneficiary - to_author;
    if let (Some((info, mut s)), Some(wallet)) = (script.filter(|_| to_author > 0), wallet) {
        pay_sol(&all, &seeds, creator, &wallet, to_author)?;
        s.author_paid = s.author_paid.saturating_add(to_author);
        let mut data = info.try_borrow_mut_data()?;
        let mut writer: &mut [u8] = &mut data;
        s.try_serialize(&mut writer)?;
        emit_cpi!(AuthorPaid {
            companion: c.key(),
            author: s.author,
            amount: to_author,
        });
    }
    // What the claim kept to split (`FeesClaimed.claimed`, `claimed_total`): all of it but the author's share.
    let got = got
        .checked_sub(to_author)
        .ok_or(CompanionError::MathOverflow)?;
    let (mut to_buyback, mut to_holders, mut to_beneficiary, mut to_pot) =
        (to_buyback, to_holders, to_beneficiary, to_pot);
    let bucket = if c.is_game() || c.has_own_pot() {
        &mut to_pot
    } else if c.split.buyback_bps > 0 {
        &mut to_buyback
    } else if c.split.holders_bps > 0 {
        &mut to_holders
    } else {
        &mut to_beneficiary
    };
    *bucket = bucket
        .checked_add(from_hook)
        .ok_or(CompanionError::MathOverflow)?;
    pay_sol(
        &all,
        &seeds,
        creator,
        &ctx.accounts.cranker.to_account_info(),
        bounty,
    )?;
    let c = &mut ctx.accounts.companion;
    c.quote_cut_claimed.amount = c
        .quote_cut_claimed
        .amount
        .checked_add(hook_gross)
        .ok_or(CompanionError::MathOverflow)?;
    // What the buyback held before this claim credited it.
    let held = c.pending_buyback;
    c.pending_buyback = c
        .pending_buyback
        .checked_add(to_buyback)
        .ok_or(CompanionError::MathOverflow)?;
    c.pending_holders = c
        .pending_holders
        .checked_add(to_holders)
        .ok_or(CompanionError::MathOverflow)?;
    c.pending_beneficiary = c
        .pending_beneficiary
        .checked_add(to_beneficiary)
        .ok_or(CompanionError::MathOverflow)?;
    c.claimed_total = c.claimed_total.saturating_add(got);
    c.bounties_total = c.bounties_total.saturating_add(bounty);
    emit_cpi!(FeesClaimed {
        companion: c.key(),
        claimed: got,
        bounty,
        to_buyback,
        to_holders,
        to_beneficiary,
        cranker: ctx.accounts.cranker.key(),
        from_hook,
    });
    if let Some(terms) = terms {
        let now = Clock::get()?.unix_timestamp;
        // What the pot held beyond what the hook's status now allows goes first, then the share.
        let retired = enforce_terms(c, &terms, now)?;
        if retired > 0 {
            emit_cpi!(PotToBuyback {
                companion: c.key(),
                lamports: retired,
                blocked: terms.blocked,
                pending_pot: c.pending_pot,
            });
        }
        let (kept, over) = fund_pot(c, to_pot, &terms)?;
        emit_cpi!(PotFunded {
            companion: c.key(),
            to_pot: kept,
            to_buyback: over,
            pending_pot: c.pending_pot,
        });
        if terms.blocked {
            c.restart_stranded_wait(held, now);
        }
    } else if c.has_own_pot() && to_pot > 0 {
        // A Snowball's pot (what its market maker trades with) or a script's (what the script
        // spends): no game hook, so no cap and no block.
        c.pending_pot = c
            .pending_pot
            .checked_add(to_pot)
            .ok_or(CompanionError::MathOverflow)?;
        emit_cpi!(PotFunded {
            companion: c.key(),
            to_pot,
            to_buyback: 0,
            pending_pot: c.pending_pot,
        });
    }
    Ok(())
}

/// The hook's part of `rest`, what is left of a claim of `claimed` after the bounty, when `hook` of the claim was the hook's cut: `floor(rest * hook / claimed)`, so at
/// most `rest` (`hook <= claimed`) and exactly `rest` when the whole claim was the cut.
pub(crate) fn hook_part(rest: u64, hook: u64, claimed: u64) -> Result<u64> {
    if hook == 0 || claimed == 0 {
        return Ok(0);
    }
    require!(
        hook <= claimed && rest <= claimed,
        CompanionError::MathOverflow
    );
    Ok((u128::from(rest) * u128::from(hook) / u128::from(claimed)) as u64)
}

/// What a game hook's status says, from its `PDA(["hook-status", hook])` among `available`: the
/// account must be passed; one that does not exist (no data, owned by the system program) means
/// the defaults. Leaving it out can therefore never lift a cap or a block. An audit recorded with
/// the code's hash counts only while it holds for the code ([`game_hook_terms`]: the hook's
/// ProgramData passed too); the game of `mint`, when passed (read-only), lends its memo, so the
/// code is rehashed only when its deploy slot moved since a game step last checked it.
pub(crate) fn hook_terms(
    available: &[AccountInfo],
    hook: &Pubkey,
    mint: &Pubkey,
) -> Result<HookTerms> {
    let info = find(available, &HookStatus::address(hook).0)?;
    let memo = available
        .iter()
        .filter(|a| *a.owner == crate::ID)
        .find_map(|a| {
            let data = a.try_borrow_data().ok()?;
            if !data.starts_with(Game::DISCRIMINATOR) {
                return None;
            }
            let g = Game::try_deserialize(&mut &data[..]).ok()?;
            (g.mint == *mint).then(|| g.hook_audit_memo())
        })
        .flatten();
    Ok(game_hook_terms(available, hook, info, memo, true)?.0)
}

/// [`hook_terms`] from the status account itself (its address checked by the caller).
pub(crate) fn read_hook_terms(info: &AccountInfo, hook: &Pubkey) -> Result<HookTerms> {
    if *info.owner == crate::ID {
        let status = HookStatus::try_deserialize(&mut &info.try_borrow_data()?[..])?;
        require_keys_eq!(status.hook, *hook, CompanionError::HookStatusAccount);
        return Ok(status.terms());
    }
    require!(
        *info.owner == system_program::ID && info.data_is_empty(),
        CompanionError::HookStatusAccount
    );
    Ok(HookTerms::DEFAULT)
}

/// A game hook's terms in a step (independent audit X4): its status's ([`read_hook_terms`]), except
/// that an audit the protocol recorded with the code's hash (`set_hook_status_v2`) lifts the cap
/// only while it holds for the code the hook runs now
/// ([`hook_audit_holds`](crate::instructions::attest::hook_audit_holds): the hook immutable or
/// Bordrless-managed and its code the recorded hash, the hook's ProgramData among `available`).
/// Otherwise the hook is taken as not audited, under the unaudited ceiling (`DEFAULT_POT_CAP`), as a
/// strategy's stale audit is. An audit with no hash (v1, as deployed) is taken as before.
///
/// `memo` is the game's (`Game::hook_audit_memo`). With `recompute`, the audit is checked (the hash
/// only when the code's deploy slot moved since the memo); without, the memo is taken as it is (a
/// strategy payment takes its plan's check, as for the strategy's own audit). Answers the terms
/// and the memo to keep.
pub(crate) fn game_hook_terms(
    available: &[AccountInfo],
    hook: &Pubkey,
    status: &AccountInfo,
    memo: Option<u64>,
    recompute: bool,
) -> Result<(HookTerms, Option<u64>)> {
    let terms = read_hook_terms(status, hook)?;
    if !terms.audited {
        return Ok((terms, None));
    }
    let recorded = HookStatus::try_deserialize(&mut &status.try_borrow_data()?[..])?.audited_hash();
    if recorded == [0; 32] {
        return Ok((terms, memo));
    }
    let held = if recompute {
        crate::instructions::attest::hook_audit_holds(available, hook, recorded, memo)?
    } else {
        memo
    };
    if held.is_some() {
        return Ok((terms, held));
    }
    Ok((
        HookTerms {
            audited: false,
            pot_cap: DEFAULT_POT_CAP,
            blocked: terms.blocked,
        },
        None,
    ))
}

/// Moves to the buyback what the pot holds beyond what `terms` allow: all of it for a blocked hook,
/// what is above the cap for one that is not audited. Answers what moved.
///
/// A blocked hook's pot moved at `now` restarts the stranded wait (`Companion.stranded_burned_at`):
/// a buyback has its whole 30 days to spend it before `burn_stranded` can take it, whichever step
/// moved it (a step and a burn sent in one transaction included). Under a block a pot is funded no
/// more, so this happens once.
///
/// What a closed streak epoch still owes its holders (`Companion.pot_locked`, zero for every other
/// kind) stays above a cap lowered since: a holder's share, fixed when the epoch closed, is never
/// trimmed by a later cap. A block takes it with the rest (no prize is paid under a block), and
/// nothing is locked after.
pub(crate) fn enforce_terms(c: &mut Companion, terms: &HookTerms, now: i64) -> Result<u64> {
    let keep = if terms.blocked {
        0
    } else {
        terms.cap().map_or(c.pending_pot, |cap| {
            c.pending_pot.min(cap.max(c.pot_locked))
        })
    };
    if terms.blocked {
        c.pot_locked = 0;
    }
    let moved = c.pending_pot - keep;
    if moved > 0 {
        c.pending_pot = keep;
        c.pending_buyback = c
            .pending_buyback
            .checked_add(moved)
            .ok_or(CompanionError::MathOverflow)?;
        if terms.blocked {
            c.stranded_burned_at = c.stranded_burned_at.max(now);
        }
    }
    Ok(moved)
}

/// Adds `amount` to the pot within what `terms` allow; the rest to the buyback. Answers both parts.
fn fund_pot(c: &mut Companion, amount: u64, terms: &HookTerms) -> Result<(u64, u64)> {
    let room = if terms.blocked {
        0
    } else {
        terms
            .cap()
            .map_or(u64::MAX, |cap| cap.saturating_sub(c.pending_pot))
    };
    let kept = amount.min(room);
    let over = amount - kept;
    c.pending_pot = c
        .pending_pot
        .checked_add(kept)
        .ok_or(CompanionError::MathOverflow)?;
    c.pending_buyback = c
        .pending_buyback
        .checked_add(over)
        .ok_or(CompanionError::MathOverflow)?;
    Ok((kept, over))
}

// ---- buyback ----------------------------------------------------------------------------------------

/// The fee bound of a buy on the launch's pool, in basis points of the input: the LP fee, the
/// creator and holder fees, Bordrless's share of them (rounded up), the burn on buys.
pub(crate) fn buy_fee_bound(launch: &Launch) -> u64 {
    let r = &launch.rules;
    let cuts = u64::from(launch.creator_fee_bps) + u64::from(r.holder_fee_buy_bps);
    u64::from(launch.lp_fee_bps) + cuts + cuts.div_ceil(4) + u64::from(r.burn_buy_bps)
}

/// The most a buyback may spend, in basis points of the pool's quote side: a quarter of what a
/// trader pays in fees for a buy and a sell (LP, creator, holder and burn fees; Bordrless's share not
/// counted), and never more than `BUYBACK_POOL_SHARE_BPS`. Someone who pumps the price by `p` before
/// a buyback of `v` and sells after it gains about `p * v` and pays about `p * Q * (buy + sell) / 2`
/// in fees (`Q` the quote side), so at half that bound the sandwich always costs more than it makes.
pub fn pool_share_bps(launch: &Launch) -> u64 {
    let r = &launch.rules;
    let side = u64::from(launch.lp_fee_bps) + u64::from(launch.creator_fee_bps);
    let buy = side + u64::from(r.holder_fee_buy_bps) + u64::from(r.burn_buy_bps);
    let sell = side + u64::from(r.holder_fee_sell_bps) + u64::from(r.burn_sell_bps);
    ((buy + sell) / 4).min(BUYBACK_POOL_SHARE_BPS)
}

/// `reference` moved toward `spot` once for every `interval` since `reference_at` (at least once,
/// at most `MAX_REFERENCE_STEPS`), so one crank after a long quiet catches up.
fn catch_up(reference: u128, spot: u128, reference_at: i64, interval: i64, now: i64) -> u128 {
    let steps = (now.saturating_sub(reference_at) / interval.max(1)).clamp(1, MAX_REFERENCE_STEPS);
    let mut r = reference;
    for _ in 0..steps {
        let next = step_toward(r, spot);
        if next == r {
            break;
        }
        r = next;
    }
    r
}

/// `buyback`: at most `max_buyback` of the pending buyback, and at most `pool_share_bps` of the
/// pool's quote side, at least `buyback_interval` after the last, bought on the launch's pool and
/// burned. It waits (without buying) while the pool's price is more than `MAX_PREMIUM_BPS` above the
/// reference price. The reference is set at the launch; a wait raises it toward the price by
/// `REFERENCE_STEP_BPS` for each interval since it last moved, and a buy only ever lowers it (so the
/// price a buy leaves never raises it). Remaining: the DEX `swap`'s accounts, the pool, the
/// creator's token holding (created when missing, the sender paying), the token `burn`'s and the
/// bridge's `unwrap_sol`'s.
pub fn process_buyback<'info>(ctx: Context<'info, Step<'info>>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let c = &ctx.accounts.companion;
    require!(c.launched, CompanionError::NotLaunched);
    let due_at = c
        .last_buyback_at
        .checked_add(c.buyback_interval)
        .ok_or(CompanionError::MathOverflow)?;
    require!(
        now >= c.launched_at.saturating_add(BUYBACK_AFTER_LAUNCH) && now >= due_at,
        CompanionError::BuybackNotDue
    );
    require!(c.pending_buyback > 0, CompanionError::NothingToDo);
    let (mint, creator) = (c.mint, ctx.accounts.creator.key());
    let seeds = CreatorSeeds::of(c);
    let launch = &ctx.accounts.launch;
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let pool_info = find(&all, &launch.pool)?;
    require_keys_eq!(*pool_info.owner, SWAP_ID, CompanionError::MissingAccount);
    let pool = read::<Pool>(pool_info)?;
    let spot = spot_price(
        pool.quote_reserve,
        pool.virtual_quote,
        pool.base_reserve,
        pool.virtual_base,
    )
    .ok_or(CompanionError::NoQuote)?;
    let ceiling = c
        .reference_price
        .saturating_mul(u128::from(BPS + MAX_PREMIUM_BPS))
        / u128::from(BPS);
    if spot > ceiling {
        // Above the reference: no buy. The reference catches up toward the price, a step for each
        // interval since it last moved.
        let reference = c.reference_price;
        let c = &mut ctx.accounts.companion;
        if now >= c.reference_at.saturating_add(c.buyback_interval) {
            c.reference_price = catch_up(
                c.reference_price,
                spot,
                c.reference_at,
                c.buyback_interval,
                now,
            );
            c.reference_at = now;
        }
        emit_cpi!(BuybackWaited {
            companion: c.key(),
            price: spot,
            reference
        });
        return Ok(());
    }
    let pool_side = u128::from(pool.quote_reserve) + u128::from(pool.virtual_quote);
    let pool_cap = (pool_side * u128::from(pool_share_bps(launch)) / u128::from(BPS))
        .min(u128::from(u64::MAX)) as u64;
    let total = c.pending_buyback.min(c.max_buyback).min(pool_cap);
    require!(total > 0, CompanionError::NothingToDo);
    let bounty = bps_of(total, u64::from(c.bounty_bps));
    let spend = total - bounty;
    // No less than the pool's own quote now, less the fees and the slippage: never at any price.
    let net = bps_of(spend, BPS.saturating_sub(buy_fee_bound(launch)));
    let quote = swap_out(
        net,
        pool.quote_reserve,
        pool.virtual_quote,
        pool.base_reserve,
        pool.virtual_base,
    )
    .ok_or(CompanionError::NoQuote)?;
    let min_out = bps_of(quote, BPS - BUYBACK_SLIPPAGE_BPS);
    require!(min_out > 0, CompanionError::NoQuote);
    ensure_holding(&all, ctx.accounts.cranker.key(), mint, creator)?;
    let before = balance(&all, &mint, &creator)?;
    let buy = creator_buy(&all, launch, creator, spend, min_out, seeds.native)?;
    invoke_built(&buy, &all, &[&seeds.seeds()])?;
    let bought = balance(&all, &mint, &creator)?
        .checked_sub(before)
        .ok_or(CompanionError::MathOverflow)?;
    let holding = token_client::holding_address(&mint, &creator);
    let (hook, extras) = token_hook(
        &all,
        launch,
        &OpKeys {
            source: holding,
            destination: mint,
            authority: creator,
            source_owner: creator,
            destination_owner: Pubkey::default(),
        },
    )?;
    invoke_built(
        &token_client::burn_with(creator, holding, mint, hook, extras, bought),
        &all,
        &[&seeds.seeds()],
    )?;
    pay_sol(
        &all,
        &seeds,
        creator,
        &ctx.accounts.cranker.to_account_info(),
        bounty,
    )?;
    let after = read::<Pool>(find(&all, &launch.pool)?)?;
    let c = &mut ctx.accounts.companion;
    c.pending_buyback -= total;
    c.last_buyback_at = now;
    if let Some(price) = spot_price(
        after.quote_reserve,
        after.virtual_quote,
        after.base_reserve,
        after.virtual_base,
    ) {
        // Down only: the price this buy leaves is partly the buy itself (or a front-runner's).
        c.reference_price = c.reference_price.min(step_toward(c.reference_price, price));
    }
    c.spent_total = c.spent_total.saturating_add(spend);
    c.burned_total = c.burned_total.saturating_add(bought);
    c.bounties_total = c.bounties_total.saturating_add(bounty);
    emit_cpi!(BoughtBack {
        companion: c.key(),
        spent: spend,
        burned: bought,
        bounty,
        cranker: ctx.accounts.cranker.key()
    });
    Ok(())
}

// ---- share ------------------------------------------------------------------------------------------

/// `share`: the holders' part, streamed to holders through the kit's reward pool (the kit takes a
/// share only while holders hold enough, so this waits until then). Remaining: the kit `share`'s
/// accounts and the bridge's `unwrap_sol`'s.
pub fn process_share<'info>(ctx: Context<'info, Step<'info>>) -> Result<()> {
    let c = &ctx.accounts.companion;
    require!(c.launched, CompanionError::NotLaunched);
    let total = c.pending_holders;
    let bounty = bps_of(total, u64::from(c.bounty_bps));
    let amount = total - bounty;
    require!(
        amount >= bordrless_kit::constants::MIN_SHARE_LAMPORTS,
        CompanionError::NothingToDo
    );
    let (mint, creator) = (c.mint, ctx.accounts.creator.key());
    let seeds = CreatorSeeds::of(c);
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    // A native companion shares its lamports with a native kit.
    let ix = if seeds.native {
        bordrless_kit::client::share_sol(creator, mint, amount)
    } else {
        let source = token_client::holding_address(&BRIDGED_SOL_MINT, &creator);
        bordrless_kit::client::share(creator, mint, source, BRIDGED_SOL_MINT, amount)
    };
    invoke_built(&ix, &all, &[&seeds.seeds()])?;
    pay_sol(
        &all,
        &seeds,
        creator,
        &ctx.accounts.cranker.to_account_info(),
        bounty,
    )?;
    let c = &mut ctx.accounts.companion;
    c.pending_holders = 0;
    c.shared_total = c.shared_total.saturating_add(amount);
    c.bounties_total = c.bounties_total.saturating_add(bounty);
    emit_cpi!(SharedWithHolders {
        companion: c.key(),
        amount,
        bounty,
        cranker: ctx.accounts.cranker.key()
    });
    Ok(())
}

// ---- withdraw -----------------------------------------------------------------------------------------

/// Accounts of `withdraw`.
#[event_cpi]
#[derive(Accounts)]
pub struct Withdraw<'info> {
    pub sender: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked).
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    /// CHECK: the beneficiary (address-checked); paid here.
    #[account(mut, address = companion.beneficiary)]
    pub beneficiary: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// `withdraw`: the beneficiary's part paid to them as SOL, with whatever SOL the creator address
/// holds above its rent-exempt minimum (what the launch did not spend of its funding). Anyone may
/// send it: it pays only the beneficiary, and, for a companion with payees (`set_payees`), each
/// payee its part first (`payees.rs`; its payees account and each payee among the remaining). Before the launch, only with the mint's signature (as a
/// remaining account): the funding returned to a launch that never happened. Remaining: the
/// bridge's `unwrap_sol` accounts.
pub fn process_withdraw<'info>(ctx: Context<'info, Withdraw<'info>>) -> Result<()> {
    let c = &ctx.accounts.companion;
    // Before the launch the creator address's funding pays for it: only the mint's holder refunds it.
    let mint_signed = ctx
        .remaining_accounts
        .iter()
        .any(|a| a.key() == c.mint && a.is_signer);
    require!(c.launched || mint_signed, CompanionError::NotLaunched);
    let (mint, creator) = (c.mint, ctx.accounts.creator.key());
    let seeds = CreatorSeeds::of(c);
    let pending = c.pending_beneficiary;
    // A native companion has no spare SOL apart from its SOL: its lamports above the minimum are
    // what its buckets count (the spare it had when it turned native went to the beneficiary's).
    let spare = if seeds.native {
        0
    } else {
        ctx.accounts
            .creator
            .lamports()
            .saturating_sub(Rent::get()?.minimum_balance(0))
    };
    require!(pending > 0 || spare > 0, CompanionError::NothingToDo);
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let beneficiary = ctx.accounts.beneficiary.to_account_info();
    // A companion with payees pays them their parts first (`payees.rs`); what they can't take yet,
    // or the beneficiary can't, stays owed. Before the launch (or with nothing pending) there is
    // nothing to split: the spare SOL goes back as it always did.
    let (to_beneficiary, spare, owed, paid_payees) =
        if ctx.accounts.companion.version == VERSION_WITH_PAYEES && pending > 0 {
            let o = crate::instructions::payees::pay_payees(
                &all,
                &seeds,
                creator,
                ctx.accounts.companion.key(),
                &mint,
                pending,
                &beneficiary,
                spare,
            )?;
            (
                o.to_beneficiary,
                if o.pay_spare { spare } else { 0 },
                o.owed,
                o.paid,
            )
        } else {
            (pending, spare, 0, 0)
        };
    // A withdraw that would pay nobody (every part owed to an account that can't take it yet) is
    // refused, so nobody pays fees to send it again and again.
    require!(
        to_beneficiary > 0 || spare > 0 || paid_payees > 0,
        CompanionError::NothingToDo
    );
    if spare > 0 {
        let from = ctx.accounts.creator.to_account_info();
        system_program::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.system_program.key(),
                system_program::Transfer {
                    from,
                    to: beneficiary.clone(),
                },
                &[&seeds.seeds()],
            ),
            spare,
        )?;
    }
    pay_sol(&all, &seeds, creator, &beneficiary, to_beneficiary)?;
    let c = &mut ctx.accounts.companion;
    c.pending_beneficiary = owed;
    c.paid_beneficiary_total = c
        .paid_beneficiary_total
        .saturating_add(to_beneficiary + spare);
    emit_cpi!(BeneficiaryPaid {
        companion: c.key(),
        lamports: to_beneficiary + spare
    });
    Ok(())
}

// ---- release ------------------------------------------------------------------------------------------

/// `release`: the dev bag's vested tokens sent to the beneficiary (their holding created when
/// missing, the sender paying): not before the early-buyer lock ends (the dev's buy would have
/// been locked like anyone's), and no more than max wallet still lets them hold before graduation
/// (the rest waits). Remaining: the token `transfer`'s accounts, the beneficiary's holding.
pub fn process_release<'info>(ctx: Context<'info, Step<'info>>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let c = &ctx.accounts.companion;
    let vested = c.vested(now).saturating_sub(c.dev_released);
    require!(vested > 0, CompanionError::NothingToDo);
    let (mint, creator, beneficiary) = (c.mint, ctx.accounts.creator.key(), c.beneficiary);
    let seeds = CreatorSeeds::of(c);
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let kit = kit_config(&all, &ctx.accounts.launch)?;
    if let Some(k) = &kit {
        if k.has(bordrless_kit::constants::modules::EARLY_BUYER_LOCK) {
            require!(now >= k.early_unlock_at, CompanionError::EarlyLocked);
        }
    }
    ensure_holding(&all, ctx.accounts.cranker.key(), mint, beneficiary)?;
    let held = balance(&all, &mint, &beneficiary)?;
    let amount = match max_wallet_room(&kit, held) {
        Some(room) => vested.min(room),
        None => vested,
    };
    require!(amount > 0, CompanionError::NothingToDo);
    let (source, destination) = (
        token_client::holding_address(&mint, &creator),
        token_client::holding_address(&mint, &beneficiary),
    );
    let (hook, extras) = token_hook(
        &all,
        &ctx.accounts.launch,
        &OpKeys {
            source,
            destination,
            authority: creator,
            source_owner: creator,
            destination_owner: beneficiary,
        },
    )?;
    let ix: Instruction =
        token_client::transfer_with(creator, source, destination, mint, hook, extras, amount);
    invoke_built(&ix, &all, &[&seeds.seeds()])?;
    let c = &mut ctx.accounts.companion;
    c.dev_released = c
        .dev_released
        .checked_add(amount)
        .ok_or(CompanionError::MathOverflow)?;
    emit_cpi!(DevReleased {
        companion: c.key(),
        tokens: amount,
        released: c.dev_released
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bordrless_hook::{AccountSource, ExtraAccount, Seed};

    fn pda(seeds: Vec<Seed>) -> ExtraAccount {
        ExtraAccount {
            writable: true,
            source: AccountSource::Pda {
                program: Pubkey::new_unique(),
                seeds,
            },
        }
    }

    fn key() -> ExtraAccount {
        ExtraAccount {
            writable: false,
            source: AccountSource::Key(Pubkey::new_unique()),
        }
    }

    #[test]
    fn a_registry_within_bounds_is_walked_as_borsh_lays_it_out() {
        let seed = |n: usize| Seed::Literal(vec![7; n]);
        let all = vec![
            Seed::Literal(b"state".to_vec()),
            Seed::Account(1),
            Seed::SourceOwner,
            Seed::DestinationOwner,
        ];
        let ok = [
            HookAccountList::new(vec![]),
            HookAccountList::new(vec![pda(all.clone()), key()]),
            HookAccountList::new(vec![pda(vec![seed(MAX_REGISTRY_SEED_LEN)]); 2]),
            HookAccountList::new(vec![pda(vec![Seed::Account(0); MAX_REGISTRY_SEEDS])]),
            HookAccountList::new(vec![key(); MAX_REGISTRY_ACCOUNTS]),
        ];
        for list in &ok {
            let data = list.encode();
            assert!(registry_within_bounds(&data), "{list:?}");
            assert_eq!(HookAccountList::decode(&data).as_ref(), Some(list));
            // Any byte short, or one more, is not a registry.
            assert!(!registry_within_bounds(&data[..data.len() - 1]));
            let mut longer = data.clone();
            longer.push(0);
            assert!(!registry_within_bounds(&longer));
        }
        let over = [
            HookAccountList::new(vec![key(); MAX_REGISTRY_ACCOUNTS + 1]),
            HookAccountList::new(vec![pda(vec![Seed::SourceOwner; MAX_REGISTRY_SEEDS + 1])]),
            HookAccountList::new(vec![pda(vec![seed(MAX_REGISTRY_SEED_LEN + 1)])]),
            HookAccountList::new(vec![pda(vec![seed(32); 16]); 2]),
        ];
        for list in &over {
            assert!(!registry_within_bounds(&list.encode()), "{list:?}");
        }
        // Bad tags, and nothing at all.
        let mut data = HookAccountList::new(vec![key()]).encode();
        data[8 + 1 + 4 + 1] = 2;
        assert!(!registry_within_bounds(&data));
        assert!(!registry_within_bounds(&[]));
    }

    #[test]
    fn the_walk_never_panics_on_any_bytes() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let base =
            HookAccountList::new(vec![pda(vec![Seed::Literal(b"s".to_vec())]), key()]).encode();
        for _ in 0..20_000 {
            let mut data = base.clone();
            let flips = 1 + next() % 4;
            for _ in 0..flips {
                let at = (next() as usize) % data.len();
                data[at] = next() as u8;
            }
            data.truncate(1 + (next() as usize) % (data.len() + 1));
            if registry_within_bounds(&data) {
                // What passes the walk decodes as Borsh (or fails cleanly), within the bounds.
                if let Some(list) = HookAccountList::decode(&data) {
                    assert!(list.accounts.len() <= MAX_REGISTRY_ACCOUNTS);
                }
            }
        }
    }
}
