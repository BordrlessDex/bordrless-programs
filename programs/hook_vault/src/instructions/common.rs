//! What every step shares: the slot owner's signer seeds, the coin's (and the other token's) hook
//! accounts for each token instruction, account lookups, the fee bounds and the slice cap. Most of
//! it is ported from the companion (`bordrless_companion::instructions::steps`), whose buyback
//! guards were audited; each port names its original.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::AccountMeta;
use anchor_lang::system_program;
use bordrless_hook::{hook_accounts_address, HOOK_ACCOUNTS_MAGIC};
use bordrless_launch::state::Launch;
use bordrless_swap::state::Pool;
use bordrless_token::client::{self as token_client, Hook};

use crate::constants::*;
use crate::error::VaultError;
use crate::invoke::invoke_built;
use crate::state::*;

/// Slot `i`'s owner signer seeds, `["slot", mint, [i], bump]`. They sign only for the holdings
/// of that one owner: every instruction a step builds names the slot's own holdings, derived here
/// from the owner's key, so `execute(i)` can never move slot `j`'s tokens.
pub struct SlotSeeds {
    mint: Pubkey,
    index: [u8; 1],
    bump: [u8; 1],
}

impl SlotSeeds {
    pub fn new(mint: Pubkey, i: u8, bump: u8) -> Self {
        Self {
            mint,
            index: [i],
            bump: [bump],
        }
    }

    pub fn seeds(&self) -> [&[u8]; 4] {
        [SLOT_SEED, self.mint.as_ref(), &self.index, &self.bump]
    }

    /// The owner's address (from the stored bump: no search).
    pub fn address(&self) -> Result<Pubkey> {
        Pubkey::create_program_address(&self.seeds(), &crate::ID)
            .map_err(|_| error!(VaultError::WrongSlotOwner))
    }
}

/// The slot `i` of `vault`, checked to be in use, and its owner's seeds checked against `owner`.
pub fn slot_of(vault: &Vault, i: u8, owner: &Pubkey) -> Result<(Slot, SlotSeeds)> {
    require!(
        usize::from(i) < usize::from(vault.n_slots),
        VaultError::WrongSlot
    );
    let slot = vault.slots[usize::from(i)];
    require!(slot.policy != policy::UNUSED, VaultError::WrongSlot);
    let seeds = SlotSeeds::new(vault.mint, i, slot.owner_bump);
    require_keys_eq!(seeds.address()?, *owner, VaultError::WrongSlotOwner);
    Ok((slot, seeds))
}

/// The keys a token operation hands the mint's hook (its callback's prefix, and the owners): what a
/// custom hook's registry resolves its extra accounts from (the companion's `OpKeys`).
pub struct OpKeys {
    pub source: Pubkey,
    pub destination: Pubkey,
    pub authority: Pubkey,
    pub source_owner: Pubkey,
    pub destination_owner: Pubkey,
}

impl OpKeys {
    /// A burn by `owner` of its holding of `mint`.
    pub fn burn(mint: &Pubkey, owner: Pubkey) -> Self {
        Self {
            source: token_client::holding_address(mint, &owner),
            destination: *mint,
            authority: owner,
            source_owner: owner,
            destination_owner: Pubkey::default(),
        }
    }
}

/// A custom hook's extra accounts for a token operation on `mint`, resolved from the hook's
/// registry `PDA(["bordrless-hook-accounts", mint], hook)` (owner and address checked), with the
/// token program's callback prefix: its signer for the hook, the mint, the source, the destination
/// and the authority (the companion's `custom_extras`).
pub fn custom_extras(
    available: &[AccountInfo],
    hook: &Pubkey,
    mint: &Pubkey,
    op: &OpKeys,
) -> Result<Vec<AccountMeta>> {
    let info = find(available, &hook_accounts_address(hook, mint).0)?;
    require_keys_eq!(*info.owner, *hook, VaultError::HookRegistry);
    let prefix = [
        token_client::hook_signer(hook),
        *mint,
        op.source,
        op.destination,
        op.authority,
    ];
    let data = info.try_borrow_data()?;
    resolve_registry(&data, &prefix, &op.source_owner, &op.destination_owner)
        .ok_or_else(|| error!(VaultError::HookRegistry))
}

/// A hook's extra accounts for an operation, resolved straight from its registry's bytes:
/// `HookAccountList::decode(data)?.resolve(prefix, source_owner, destination_owner)`, the same
/// accounts in the same order, without decoding the list. The decode and the resolve built a vector
/// for every PDA's seeds and every seed (about 10 KiB for a registry at the bounds), on a heap that
/// never frees: twice in one buy (the bought token's transfer and burn) that exhausted the 32 KiB
/// heap, an abort the bounds were meant to rule out. Here the seeds are slices of the registry and
/// of a fixed array of keys, and the answer is the only allocation. `None` for a registry out of
/// bounds (`MAX_REGISTRY_*`) or malformed, a seed naming an account not resolved yet, or a PDA no
/// bump makes (`try_find_program_address`: an error, not the panic `find_program_address` raises).
pub fn resolve_registry(
    data: &[u8],
    prefix: &[Pubkey; bordrless_hook::TOKEN_PREFIX_ACCOUNTS],
    source_owner: &Pubkey,
    destination_owner: &Pubkey,
) -> Option<Vec<AccountMeta>> {
    const PREFIX: usize = bordrless_hook::TOKEN_PREFIX_ACCOUNTS;
    if !registry_within_bounds(data) || data.get(..8)? != HOOK_ACCOUNTS_MAGIC {
        return None;
    }
    let mut cur = data.get(8..)?;
    take(&mut cur, 1)?;
    let n = len(&mut cur, MAX_REGISTRY_ACCOUNTS)?;
    let mut keys = [Pubkey::default(); PREFIX + MAX_REGISTRY_ACCOUNTS];
    keys[..PREFIX].copy_from_slice(prefix);
    let mut metas = Vec::with_capacity(n);
    for i in 0..n {
        let writable = match take(&mut cur, 1)?[0] {
            0 => false,
            1 => true,
            _ => return None,
        };
        let key = match take(&mut cur, 1)?[0] {
            0 => Pubkey::try_from(take(&mut cur, 32)?).ok()?,
            1 => {
                let program = Pubkey::try_from(take(&mut cur, 32)?).ok()?;
                let count = len(&mut cur, MAX_REGISTRY_SEEDS)?;
                let mut seeds: [&[u8]; MAX_REGISTRY_SEEDS] = [&[]; MAX_REGISTRY_SEEDS];
                for seed in seeds.iter_mut().take(count) {
                    *seed = match take(&mut cur, 1)?[0] {
                        0 => {
                            let l = len(&mut cur, MAX_REGISTRY_SEED_LEN)?;
                            take(&mut cur, l)?
                        }
                        1 => {
                            let j = usize::from(take(&mut cur, 1)?[0]);
                            keys.get(..PREFIX + i)?.get(j)?.as_ref()
                        }
                        2 => source_owner.as_ref(),
                        3 => destination_owner.as_ref(),
                        _ => return None,
                    };
                }
                Pubkey::try_find_program_address(&seeds[..count], &program)?.0
            }
            _ => return None,
        };
        keys[PREFIX + i] = key;
        metas.push(AccountMeta {
            pubkey: key,
            is_signer: false,
            is_writable: writable,
        });
    }
    cur.is_empty().then_some(metas)
}

/// The next `n` bytes of a registry being walked.
fn take<'a>(data: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    let (head, rest) = (data.get(..n)?, data.get(n..)?);
    *data = rest;
    Some(head)
}

/// A Borsh length (u32), at most `max`.
fn len(data: &mut &[u8], max: usize) -> Option<usize> {
    let mut b = [0u8; 4];
    b.copy_from_slice(take(data, 4)?);
    let n = usize::try_from(u32::from_le_bytes(b)).ok()?;
    (n <= max).then_some(n)
}

/// Walks a registry's Borsh bytes without allocating (the companion's `registry_within_bounds`): at
/// most `MAX_REGISTRY_LEN` bytes, `MAX_REGISTRY_ACCOUNTS` accounts, `MAX_REGISTRY_SEEDS` seeds a PDA,
/// `MAX_REGISTRY_SEED_LEN` bytes a literal seed, and no byte after the last account.
pub fn registry_within_bounds(data: &[u8]) -> bool {
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

/// A launch's mint as token instructions take it: the kit and its two extras with kit rules,
/// nothing without (the companion's `mint_hook`).
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

/// A launch's mint as a token instruction for `op` takes it: the kit's ([`mint_hook`]), or the
/// custom hook's with its extras resolved from its registry (the companion's `token_hook`).
pub fn token_hook(
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

/// The room the kit's max wallet leaves the slot's owner (which holds none of the token: the buy
/// burns what was there first) in a token it buys: the cap, while the token's kit caps wallets
/// and the token has not graduated; `None` without a cap. The kit's config is read at the launch's
/// `kit_config` (owner and discriminator checked), which the buy's burn names anyway.
pub fn wallet_room(available: &[AccountInfo], launch: &Launch) -> Result<Option<u64>> {
    use bordrless_kit::constants::modules::MAX_WALLET;
    if launch.modules & MAX_WALLET == 0 || launch.graduated_at != 0 {
        return Ok(None);
    }
    let config = read::<bordrless_kit::state::KitConfig>(find(available, &launch.kit_config)?)?;
    if !config.has(MAX_WALLET) || config.graduated {
        return Ok(None);
    }
    Ok(Some(config.max_wallet_amount))
}

/// An account of `T`'s program, deserialized (owner and discriminator checked).
pub fn read<T: AccountDeserialize + Owner>(info: &AccountInfo) -> Result<T> {
    require_keys_eq!(*info.owner, T::owner(), VaultError::MissingAccount);
    let data = info.try_borrow_data()?;
    T::try_deserialize(&mut &data[..])
}

pub fn find<'a, 'info>(
    available: &'a [AccountInfo<'info>],
    key: &Pubkey,
) -> Result<&'a AccountInfo<'info>> {
    available
        .iter()
        .find(|a| a.key == key)
        .ok_or_else(|| error!(VaultError::MissingAccount))
}

/// The DEX pool at `key` among `available` (owner checked).
pub fn read_pool(available: &[AccountInfo], key: &Pubkey) -> Result<Pool> {
    let info = find(available, key)?;
    require_keys_eq!(*info.owner, SWAP_ID, VaultError::MissingAccount);
    read::<Pool>(info)
}

/// A pool's spot price.
pub fn pool_price(pool: &Pool) -> Result<u128> {
    spot_price(
        pool.quote_reserve,
        pool.virtual_quote,
        pool.base_reserve,
        pool.virtual_base,
    )
    .ok_or_else(|| error!(VaultError::NoQuote))
}

/// A holding's balance; 0 for one that does not exist yet.
pub fn balance(available: &[AccountInfo], mint: &Pubkey, owner: &Pubkey) -> Result<u64> {
    let info = find(available, &token_client::holding_address(mint, owner))?;
    if info.owner != &TOKEN_ID {
        return Ok(0);
    }
    Ok(token_client::read_holding(info)?.amount)
}

/// Creates `owner`'s holding of `mint` when it does not exist, `payer` (a signer) paying.
pub fn ensure_holding(
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

/// The slot's bridged SOL unwrapped to its owner (`total` of it), then paid on as SOL: each
/// `(to, lamports)` in turn, the owner signing. The owner ends with the lamports it had.
pub fn pay_sol<'info>(
    available: &[AccountInfo<'info>],
    seeds: &SlotSeeds,
    owner: Pubkey,
    payments: &[(&AccountInfo<'info>, u64)],
) -> Result<()> {
    let total = payments
        .iter()
        .try_fold(0u64, |sum, (_, l)| sum.checked_add(*l))
        .ok_or(VaultError::MathOverflow)?;
    if total == 0 {
        return Ok(());
    }
    invoke_built(
        &bordrless_bridge::client::unwrap_sol(owner, total),
        available,
        &[&seeds.seeds()],
    )?;
    let from = find(available, &owner)?.clone();
    let system = find(available, &system_program::ID)?.clone();
    for (to, lamports) in payments {
        if *lamports == 0 {
            continue;
        }
        system_program::transfer(
            CpiContext::new_with_signer(
                *system.key,
                system_program::Transfer {
                    from: from.clone(),
                    to: (*to).clone(),
                },
                &[&seeds.seeds()],
            ),
            *lamports,
        )?;
    }
    Ok(())
}

pub fn available<'info>(
    named: Vec<AccountInfo<'info>>,
    remaining: &[AccountInfo<'info>],
) -> Vec<AccountInfo<'info>> {
    let mut all = named;
    all.extend_from_slice(remaining);
    all
}

// ---- The guards' fee arithmetic (the companion's, and its mirror for sells) ----------------------

/// The fee bound of a buy on a launch's pool, in basis points of the input: the LP fee, the creator
/// and holder fees, Bordrless's share of them (rounded up), the burn on buys (the companion's
/// `buy_fee_bound`, with the pool's own share in place of the constant quarter).
pub fn buy_fee_bound(launch: &Launch, share_bps: u64) -> u64 {
    let r = &launch.rules;
    let cuts = u64::from(launch.creator_fee_bps) + u64::from(r.holder_fee_buy_bps);
    u64::from(launch.lp_fee_bps) + cuts + share_of(cuts, share_bps) + u64::from(r.burn_buy_bps)
}

/// [`buy_fee_bound`] with the most the bought token's own hook may cut from the buy
/// (`max_cut_bps`, declared at `create_vault` for a custom-hook token) and Bordrless's share of that
/// cut, which the DEX takes in SOL from the input (the buy's mirror of [`sell_fee_bound`]). Never
/// above 10,000.
pub fn buy_bound_with_cut(launch: &Launch, share_bps: u64, max_cut_bps: u16) -> u64 {
    let cut = u64::from(max_cut_bps);
    (buy_fee_bound(launch, share_bps) + cut + share_of(cut, share_bps)).min(BPS)
}

/// The fee bound of a sell on a launch's pool, in basis points of the input, the mirror of
/// [`buy_fee_bound`]: the burn on sells (from the coin in), the LP fee, the creator and holder sell
/// fees and Bordrless's share of them (from the SOL out), and, for the coin's own hook, the most it
/// may cut from the vault's transfer into the pool (`max_hook_cut_bps`, declared at
/// `create_vault`) with Bordrless's share of that cut, which the DEX takes in SOL from the output
/// (`docs/hooks-v2.md` §3.1). Never above 10,000.
pub fn sell_fee_bound(launch: &Launch, share_bps: u64, max_hook_cut_bps: u16) -> u64 {
    let r = &launch.rules;
    let cuts = u64::from(launch.creator_fee_bps) + u64::from(r.holder_fee_sell_bps);
    let hook = u64::from(max_hook_cut_bps);
    let bound = u64::from(launch.lp_fee_bps)
        + cuts
        + share_of(cuts, share_bps)
        + u64::from(r.burn_sell_bps)
        + hook
        + share_of(hook, share_bps);
    bound.min(BPS)
}

/// The most a buy (or a sell) may move, in basis points of the pool's quote side: a quarter of what
/// a trader pays in fees for a buy and a sell (LP, creator, holder and burn fees; Bordrless's share
/// not counted), and never more than `POOL_SHARE_BPS` (the companion's `pool_share_bps`, as is).
/// Someone who moves the price by `p` before a trade of `v` and trades back after it gains about
/// `p * v` and pays about `p * Q * (buy + sell) / 2` in fees (`Q` the quote side), so at half that
/// bound the sandwich always costs more than it makes. A sell is the buy's mirror: the attacker
/// dumps first and buys back after, and the same fees bound it.
pub fn pool_share_bps(launch: &Launch) -> u64 {
    let r = &launch.rules;
    let side = u64::from(launch.lp_fee_bps) + u64::from(launch.creator_fee_bps);
    let buy = side + u64::from(r.holder_fee_buy_bps) + u64::from(r.burn_buy_bps);
    let sell = side + u64::from(r.holder_fee_sell_bps) + u64::from(r.burn_sell_bps);
    ((buy + sell) / 4).min(POOL_SHARE_BPS)
}

/// The vault's slice, in basis points of a pool's quote side: [`pool_share_bps`] with the creator
/// fee replaced by Bordrless's share of it (`share_bps`, the pool's protocol share). The creator
/// fee comes back to the launch's creator, so to the creator a sandwich around the vault's trade
/// costs only the LP fee, the burns, the holder fees and Bordrless's share of the creator fee; the
/// slice is a quarter of that round trip, so the creator's own sandwich does not pay either
/// (audit 3a vault r1, F2). Never more than [`pool_share_bps`].
pub fn vault_share_bps(launch: &Launch, share_bps: u64) -> u64 {
    let r = &launch.rules;
    let creator = share_of(u64::from(launch.creator_fee_bps), share_bps);
    let side = u64::from(launch.lp_fee_bps) + creator;
    let buy = side + u64::from(r.holder_fee_buy_bps) + u64::from(r.burn_buy_bps);
    let sell = side + u64::from(r.holder_fee_sell_bps) + u64::from(r.burn_sell_bps);
    ((buy + sell) / 4).min(pool_share_bps(launch))
}

/// How many of the vault's slots sell (at least 1, for a divisor).
pub fn selling_slots(v: &Vault) -> u64 {
    let n = v.slots[..usize::from(v.n_slots)]
        .iter()
        .filter(|s| s.sells())
        .count();
    (n as u64).max(1)
}

/// The pool's quote side (real and virtual), and `bps` of it, capped at `u64::MAX`.
pub fn quote_cap(pool: &Pool, bps: u64) -> u64 {
    let side = u128::from(pool.quote_reserve) + u128::from(pool.virtual_quote);
    (side * u128::from(bps) / u128::from(BPS)).min(u128::from(u64::MAX)) as u64
}

/// What `base` of the coin is worth at the pool's spot price (rounded up), capped at `u64::MAX`.
pub fn quote_worth(pool: &Pool, base: u64) -> u64 {
    let q = u128::from(pool.quote_reserve) + u128::from(pool.virtual_quote);
    let b = u128::from(pool.base_reserve) + u128::from(pool.virtual_base);
    if b == 0 {
        return u64::MAX;
    }
    (u128::from(base) * q).div_ceil(b).min(u128::from(u64::MAX)) as u64
}

/// The most quote a buy on `pool` may spend so that, with no fee at all, it delivers at most `room`
/// of the token: `room * Q / (B - room)` on the effective reserves, rounded down. Fees only lower
/// what a buy delivers, so the real buy stays within `room` too. `None` when `room` is at least
/// the whole base side (no cap).
pub fn spend_for_at_most(pool: &Pool, room: u64) -> Option<u64> {
    let q = u128::from(pool.quote_reserve) + u128::from(pool.virtual_quote);
    let b = u128::from(pool.base_reserve) + u128::from(pool.virtual_base);
    let room = u128::from(room);
    (room < b).then(|| (room * q / (b - room)).min(u128::from(u64::MAX)) as u64)
}

/// How much of the coin is worth at most `quote` at the pool's spot price (rounded down).
pub fn base_worth(pool: &Pool, quote: u64) -> u64 {
    let q = u128::from(pool.quote_reserve) + u128::from(pool.virtual_quote);
    let b = u128::from(pool.base_reserve) + u128::from(pool.virtual_base);
    if q == 0 {
        return 0;
    }
    (u128::from(quote) * b / q).min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use bordrless_hook::{AccountSource, ExtraAccount, HookAccountList, Seed};

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
    fn the_registry_resolves_in_place_as_the_list_does() {
        let prefix = [(); 5].map(|_| Pubkey::new_unique());
        let (so, dst) = (Pubkey::new_unique(), Pubkey::new_unique());
        let lists = [
            HookAccountList::new(vec![]),
            HookAccountList::new(vec![
                pda(vec![Seed::Literal(b"tax".to_vec()), Seed::Account(1)]),
                key(),
                pda(vec![
                    Seed::Account(5),
                    Seed::Account(6),
                    Seed::SourceOwner,
                    Seed::DestinationOwner,
                    Seed::Literal(vec![]),
                ]),
            ]),
            HookAccountList::new(vec![
                pda(vec![Seed::SourceOwner; 15]);
                MAX_REGISTRY_ACCOUNTS
            ]),
            HookAccountList::new(vec![
                pda(vec![Seed::Literal(vec![9; MAX_REGISTRY_SEED_LEN]); 15]),
                key(),
            ]),
            // A seed naming an account not resolved yet.
            HookAccountList::new(vec![pda(vec![Seed::Account(5)])]),
            // 16 seeds: no bump fits (17 seeds), refused rather than a panic.
            HookAccountList::new(vec![pda(vec![Seed::SourceOwner; 16])]),
        ];
        for list in &lists {
            let data = list.encode();
            let want =
                if list.accounts.iter().any(
                    |a| matches!(&a.source, AccountSource::Pda { seeds, .. } if seeds.len() > 15),
                ) {
                    None
                } else {
                    list.resolve(&prefix, &so, &dst)
                };
            assert_eq!(
                resolve_registry(&data, &prefix, &so, &dst),
                want,
                "{list:?}"
            );
        }
        // Malformed: a bool other than 0 or 1, a wrong magic, a trailing byte.
        let mut data = lists[1].encode();
        assert!(resolve_registry(&data, &prefix, &so, &dst).is_some());
        data[8 + 1 + 4] = 2;
        assert_eq!(resolve_registry(&data, &prefix, &so, &dst), None);
        let mut data = lists[1].encode();
        data[0] ^= 1;
        assert_eq!(resolve_registry(&data, &prefix, &so, &dst), None);
        let mut data = lists[1].encode();
        data.push(0);
        assert_eq!(resolve_registry(&data, &prefix, &so, &dst), None);
    }

    #[test]
    fn a_registry_within_bounds_is_walked_as_borsh_lays_it_out() {
        let seed = |n: usize| Seed::Literal(vec![7; n]);
        let ok = [
            HookAccountList::new(vec![]),
            HookAccountList::new(vec![
                pda(vec![
                    Seed::Literal(b"tax".to_vec()),
                    Seed::Account(1),
                    Seed::SourceOwner,
                    Seed::DestinationOwner,
                ]),
                key(),
            ]),
            HookAccountList::new(vec![pda(vec![seed(MAX_REGISTRY_SEED_LEN)]); 2]),
            HookAccountList::new(vec![key(); MAX_REGISTRY_ACCOUNTS]),
        ];
        for list in &ok {
            let data = list.encode();
            assert!(registry_within_bounds(&data), "{list:?}");
            assert!(!registry_within_bounds(&data[..data.len() - 1]));
            let mut longer = data.clone();
            longer.push(0);
            assert!(!registry_within_bounds(&longer));
        }
        let over = [
            HookAccountList::new(vec![key(); MAX_REGISTRY_ACCOUNTS + 1]),
            HookAccountList::new(vec![pda(vec![Seed::SourceOwner; MAX_REGISTRY_SEEDS + 1])]),
            HookAccountList::new(vec![pda(vec![seed(MAX_REGISTRY_SEED_LEN + 1)])]),
        ];
        for list in &over {
            assert!(!registry_within_bounds(&list.encode()), "{list:?}");
        }
        assert!(!registry_within_bounds(&[]));
    }

    fn launch(creator: u16, lp: u16, burn_sell: u16) -> Launch {
        let mut l = Launch {
            version: 1,
            bump: 0,
            mint: Pubkey::default(),
            creator: Pubkey::default(),
            pool: Pubkey::default(),
            quote_mint: Pubkey::default(),
            status: 0,
            creator_fee_bps: creator,
            lp_fee_bps: lp,
            sniper_window_secs: 0,
            sniper_start_bps: 0,
            virtual_quote: 0,
            virtual_base: 0,
            graduation_quote: 0,
            curve_tokens: 0,
            reserve_tokens: 0,
            reserve_holding: Pubkey::default(),
            quote_holding: Pubkey::default(),
            lp_holding: Pubkey::default(),
            created_at: 0,
            graduated_at: 0,
            creator_fees_accrued: 0,
            creator_fees_claimed: 0,
            graduation_topup: 0,
            graduation_burned: 0,
            rules: bordrless_launch::state::LaunchRules::NONE,
            modules: 0,
            kit_config: Pubkey::default(),
            holder_vault: Pubkey::default(),
            kit_caller_bump: 0,
            creator_unlock_at: 0,
            early_window_end: 0,
            early_unlock_at: 0,
            creator_bought: false,
            holder_fees_accrued: 0,
            burned_on_trades: 0,
            config: Pubkey::default(),
            custom_hook: None,
            custom_hook_flags: 0,
            author_share_bps: 0,
            author_fees_paid: 0,
            reserved: [0; 22],
        };
        l.rules.burn_sell_bps = burn_sell;
        l
    }

    #[test]
    fn the_fee_bounds_and_the_slice() {
        // 0.3% LP, 1% creator, 1% burn on sells; a quarter shared.
        let l = launch(100, 30, 100);
        assert_eq!(buy_fee_bound(&l, 2_500), 30 + 100 + 25);
        assert_eq!(sell_fee_bound(&l, 2_500, 0), 30 + 100 + 25 + 100);
        // The hook's declared cut and Bordrless's quarter of it.
        assert_eq!(sell_fee_bound(&l, 2_500, 1_000), 255 + 1_000 + 250);
        assert_eq!(
            sell_fee_bound(&l, 2_500, MAX_HOOK_CUT_BPS),
            255 + 5_000 + 1_250
        );
        // (130 + 230) / 4.
        assert_eq!(pool_share_bps(&l), 90);
        assert_eq!(pool_share_bps(&launch(200, 30, 500)), 100);
        // A fee-free pool gets the smallest slice.
        assert_eq!(pool_share_bps(&launch(0, 30, 0)), 15);
        // The vault's slice counts only Bordrless's quarter of the creator fee: (2 * (30 + 25)) / 4,
        // and with 1% burns on sells (110 + 100) / 4.
        assert_eq!(vault_share_bps(&launch(100, 30, 0), 2_500), 27);
        assert_eq!(vault_share_bps(&launch(100, 30, 100), 2_500), 52);
        assert_eq!(vault_share_bps(&launch(0, 30, 0), 2_500), 15);
        assert_eq!(vault_share_bps(&launch(200, 30, 500), 2_500), 100);
        for (c, lp, b) in [(0, 30, 0), (100, 30, 100), (200, 30, 500), (1_000, 100, 0)] {
            let l = launch(c, lp, b);
            assert!(vault_share_bps(&l, 2_500) <= pool_share_bps(&l));
        }
        // The bought token's declared cut and Bordrless's quarter of it.
        assert_eq!(buy_bound_with_cut(&l, 2_500, 0), buy_fee_bound(&l, 2_500));
        assert_eq!(buy_bound_with_cut(&l, 2_500, 500), 155 + 500 + 125);
        assert_eq!(buy_bound_with_cut(&l, 2_500, u16::MAX), BPS);
    }

    fn pool(quote: u64, base: u64) -> Pool {
        let zeros = vec![0u8; Pool::INIT_SPACE];
        let mut p = Pool::deserialize(&mut &zeros[..]).unwrap();
        p.quote_reserve = quote;
        p.base_reserve = base;
        p
    }

    #[test]
    fn a_capped_spend_delivers_at_most_the_room() {
        let p = pool(30_000_000_000, 800_000_000_000_000);
        for room in [1u64, 1_000, 8_000_000_000_000, 400_000_000_000_000] {
            let spend = spend_for_at_most(&p, room).unwrap();
            let out =
                bordrless_core::swap_out(spend, p.quote_reserve, 0, p.base_reserve, 0).unwrap_or(0);
            assert!(out <= room, "room {room}: {out}");
            // Tight: a little more would cross it (within the rounding of a few base units).
            let more = bordrless_core::swap_out(spend + 1, p.quote_reserve, 0, p.base_reserve, 0)
                .unwrap_or(0);
            assert!(more + 30_000 > room, "room {room}: {more}");
        }
        assert_eq!(spend_for_at_most(&p, 800_000_000_000_000), None);
        assert_eq!(quote_worth(&p, 800_000_000), 30_000);
        assert_eq!(quote_worth(&p, 1), 1);
        assert_eq!(base_worth(&p, quote_worth(&p, 8_000_000)), 8_000_000);
    }
}
