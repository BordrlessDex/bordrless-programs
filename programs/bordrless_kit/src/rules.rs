//! What the callbacks do (`docs/hooks-v2.md` §4.9), as pure functions of the config, the token
//! program's arguments, the reward vault's balance and the time, so the order of the rules is
//! the same on chain and in host tests. The callbacks bind the accounts first
//! (`instructions::callbacks`); these make no CPI and emit nothing.

use anchor_lang::prelude::*;
use bordrless_hook::{HookReturn, Phase, TokenHookArgs, TokenOp};

use crate::constants::modules;
use crate::error::KitError;
use crate::math::settle;
use crate::state::{HolderData, KitConfig};

/// The answer for the hook data of the sides that changed. No deltas, no burn.
fn answer(
    source: Option<(&[u8; 64], &HolderData)>,
    destination: Option<(&[u8; 64], &HolderData)>,
) -> HookReturn {
    let changed = |side: Option<(&[u8; 64], &HolderData)>| {
        side.and_then(|(read, data)| {
            let bytes = data.to_bytes();
            (bytes != *read).then_some(bytes)
        })
    };
    HookReturn {
        source_hook_data: changed(source),
        destination_hook_data: changed(destination),
        ..HookReturn::default()
    }
}

/// The reward vault's balance when holder rewards are on (the callback passed it then).
fn vault_of(config: &KitConfig, vault_amount: Option<u64>) -> Result<Option<u64>> {
    match (config.rewards_on(), vault_amount) {
        (true, Some(amount)) => Ok(Some(amount)),
        (true, None) => err!(KitError::MissingRewardVault),
        (false, _) => Ok(None),
    }
}

/// `before_transfer` of a token whose config is `config` (at address `config_key`), at `now`:
///
/// 1. a refused destination, or with holder rewards a destination owner that is neither
///    excluded nor on the ed25519 curve: `DestinationNotAllowed`;
/// 2. sync (holder rewards);
/// 3. creator wallet lock: `CreatorLocked`;
/// 4. early-buyer lock on a holder source: `EarlyLocked`;
/// 5. and 6. settle the source, then the destination (holder rewards, holders only);
/// 7. max wallet on a holder destination until graduation: `MaxWalletExceeded`;
/// 8. early-buyer bookkeeping;
/// 9. `eligible`;
/// 10. the hook data of each side that changed.
pub fn before_transfer(
    config: &mut KitConfig,
    config_key: &Pubkey,
    args: &TokenHookArgs,
    vault_amount: Option<u64>,
    now: i64,
) -> Result<HookReturn> {
    require!(
        args.op == TokenOp::Transfer && args.phase == Phase::Before,
        KitError::UnsupportedOperation
    );
    let vault = vault_of(config, vault_amount)?;
    let source_excluded = config.is_excluded(&args.source_owner);
    let destination_excluded = config.is_excluded(&args.destination_owner);

    // 1. Where the token may go.
    require!(
        !config.is_refused_destination(config_key, &args.destination_owner),
        KitError::DestinationNotAllowed
    );
    if config.rewards_on() && !destination_excluded {
        // Wallets only: no program-owned account can hold the token.
        require!(
            args.destination_owner.is_on_curve(),
            KitError::DestinationNotAllowed
        );
    }

    // 2. What arrived for holders, at the eligible supply before this transfer.
    if let Some(vault) = vault {
        config.sync(vault, now)?;
    }

    // 3. The creator's wallet sends nothing until it unlocks, whoever signs.
    if config.has(modules::CREATOR_WALLET_LOCK)
        && now < config.creator_unlock_at
        && args.source_owner == config.creator
    {
        return err!(KitError::CreatorLocked);
    }

    let source_read = args.source_hook_data;
    let destination_read = args.destination_hook_data;
    let mut source = HolderData::read(&source_read);
    let mut destination = HolderData::read(&destination_read);
    let source_after = args
        .source_balance
        .checked_sub(args.amount)
        .ok_or(KitError::MathOverflow)?;
    let destination_after = args
        .destination_balance
        .checked_add(args.amount)
        .ok_or(KitError::MathOverflow)?;
    let early = config.has(modules::EARLY_BUYER_LOCK);

    // 4. Tokens bought in the early window stay until the unlock.
    if early && !source_excluded && now < config.early_unlock_at {
        require!(source_after >= source.early_locked, KitError::EarlyLocked);
    }

    // 5. and 6. Each holder's rewards up to now, at its balance before the transfer.
    if config.rewards_on() {
        if !source_excluded {
            settle(
                &mut source,
                args.source_balance,
                source_after,
                config.acc_per_share,
            )?;
        }
        if !destination_excluded {
            settle(
                &mut destination,
                args.destination_balance,
                destination_after,
                config.acc_per_share,
            )?;
        }
    }

    // 7. No holder above the cap until graduation; the pool and the launch are never capped, so a
    //    sell into the launch pool is never blocked.
    if config.has(modules::MAX_WALLET) && !config.graduated && !destination_excluded {
        require!(
            destination_after <= config.max_wallet_amount,
            KitError::MaxWalletExceeded
        );
    }

    // 8. What a holder buys from the pool in the early window is locked; after the unlock the
    //    lock is cleared on each side the transfer touches.
    if early {
        if args.source_owner == config.pool
            && now < config.early_window_end
            && !destination_excluded
        {
            destination.early_locked = destination
                .early_locked
                .checked_add(args.amount)
                .ok_or(KitError::MathOverflow)?;
        }
        if now >= config.early_unlock_at {
            if !source_excluded {
                source.early_locked = 0;
            }
            if !destination_excluded {
                destination.early_locked = 0;
            }
        }
    }

    // 9. The eligible supply: the kit answers no deltas, so what leaves is what arrives.
    match (source_excluded, destination_excluded) {
        (true, false) => {
            config.eligible = config
                .eligible
                .checked_add(args.amount)
                .ok_or(KitError::MathOverflow)?;
        }
        (false, true) => {
            config.eligible = config
                .eligible
                .checked_sub(args.amount)
                .ok_or(KitError::MathOverflow)?;
        }
        _ => {}
    }

    // 10. The data of each holder side that changed. Excluded owners' data is never written.
    Ok(answer(
        (!source_excluded).then_some((&source_read, &source)),
        (!destination_excluded).then_some((&destination_read, &destination)),
    ))
}

/// `before_burn` at `now`: sync (holder rewards); the early-buyer lock on a holder source; settle
/// the source when it is a holder; `eligible -= amount` when the source is a holder; the source's
/// data. The creator wallet lock does not apply: a burn extracts nothing.
pub fn before_burn(
    config: &mut KitConfig,
    args: &TokenHookArgs,
    vault_amount: Option<u64>,
    now: i64,
) -> Result<HookReturn> {
    require!(
        args.op == TokenOp::Burn && args.phase == Phase::Before,
        KitError::UnsupportedOperation
    );
    let vault = vault_of(config, vault_amount)?;
    if let Some(vault) = vault {
        config.sync(vault, now)?;
    }
    if config.is_excluded(&args.source_owner) {
        // The pool's and the launch's burns change nothing the kit keeps.
        return Ok(HookReturn::default());
    }
    let read = args.source_hook_data;
    let mut source = HolderData::read(&read);
    let after = args
        .source_balance
        .checked_sub(args.amount)
        .ok_or(KitError::MathOverflow)?;
    let early = config.has(modules::EARLY_BUYER_LOCK);
    if early && now < config.early_unlock_at {
        require!(after >= source.early_locked, KitError::EarlyLocked);
    }
    if config.rewards_on() {
        settle(
            &mut source,
            args.source_balance,
            after,
            config.acc_per_share,
        )?;
    }
    if early && now >= config.early_unlock_at {
        source.early_locked = 0;
    }
    config.eligible = config
        .eligible
        .checked_sub(args.amount)
        .ok_or(KitError::MathOverflow)?;
    Ok(answer(Some((&read, &source)), None))
}
