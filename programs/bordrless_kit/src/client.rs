//! Addresses, the hook accounts of a kit mint and instruction builders for calling the kit: used
//! by the launch program (CPI), the tests and as the reference for the TypeScript SDK. Account
//! order is that of each `Accounts` struct, with the event authority and the program appended as
//! `#[event_cpi]` does.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{system_program, InstructionData};
use bordrless_hook::hook_accounts_address;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::state::{KitConfig, KitInitArgs};

/// This program's event authority.
pub fn event_authority() -> Pubkey {
    crate::EVENT_AUTHORITY_AND_BUMP.0
}

/// This program's `["hook-authority"]` PDA (it signs `write_hook_data` in `claim`).
pub fn hook_authority() -> Pubkey {
    HOOK_AUTHORITY
}

/// The `KitConfig` of `mint`.
pub fn kit_config_address(mint: &Pubkey) -> Pubkey {
    KitConfig::address(mint).0
}

/// The kit's registry for `mint`.
pub fn registry_address(mint: &Pubkey) -> Pubkey {
    hook_accounts_address(&crate::ID, mint).0
}

/// The launch program's kit-caller PDA for `mint`, and its bump.
pub fn kit_caller_address(mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[KIT_CALLER_SEED, mint.as_ref()], &LAUNCH_ID)
}

/// The reward vault of `mint` for `reward_mint`: the config's holding of it.
pub fn reward_vault_address(mint: &Pubkey, reward_mint: &Pubkey) -> Pubkey {
    token_client::holding_address(reward_mint, &kit_config_address(mint))
}

/// The two extra accounts every callback of `mint` takes, as the registry lists them: the config
/// (writable), then the reward vault (read-only) with holder rewards, or the kit's id without.
pub fn hook_extras(mint: &Pubkey, reward_vault: Option<Pubkey>) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new(kit_config_address(mint), false),
        AccountMeta::new_readonly(reward_vault.unwrap_or(crate::ID), false),
    ]
}

/// A kit mint's token-hook slice for a DEX instruction: the kit's id, the token program's signer
/// of the kit's callbacks ([`TOKEN_HOOK_AUTHORITY`]), then [`hook_extras`].
pub fn hook_slice(mint: &Pubkey, reward_vault: Option<Pubkey>) -> Vec<AccountMeta> {
    let mut slice = vec![
        AccountMeta::new_readonly(crate::ID, false),
        AccountMeta::new_readonly(TOKEN_HOOK_AUTHORITY, false),
    ];
    slice.extend(hook_extras(mint, reward_vault));
    slice
}

fn with_events(mut accounts: Vec<AccountMeta>) -> Vec<AccountMeta> {
    accounts.push(AccountMeta::new_readonly(event_authority(), false));
    accounts.push(AccountMeta::new_readonly(crate::ID, false));
    accounts
}

/// `init`, signed by `kit_caller` (the launch's kit-caller PDA for `mint`) and paid by `payer`.
/// The reward vault is passed (and created) when `args.modules` has holder rewards.
pub fn init(kit_caller: Pubkey, payer: Pubkey, mint: Pubkey, args: KitInitArgs) -> Instruction {
    let kit_config = kit_config_address(&mint);
    let reward_vault = if args.modules & modules::HOLDER_REWARDS != 0 {
        AccountMeta::new(reward_vault_address(&mint, &args.reward_mint), false)
    } else {
        AccountMeta::new_readonly(crate::ID, false)
    };
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new_readonly(kit_caller, true),
            AccountMeta::new(payer, true),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(token_client::holding_address(&mint, &args.launch), false),
            AccountMeta::new(kit_config, false),
            AccountMeta::new(registry_address(&mint), false),
            AccountMeta::new_readonly(args.reward_mint, false),
            reward_vault,
            AccountMeta::new_readonly(TOKEN_ID, false),
            AccountMeta::new_readonly(TOKEN_EVENT_AUTHORITY, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ]),
        data: crate::instruction::Init { args }.data(),
    }
}

/// `graduate` of `mint`, signed by `kit_caller`.
pub fn graduate(kit_caller: Pubkey, mint: Pubkey) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new_readonly(kit_caller, true),
            AccountMeta::new(kit_config_address(&mint), false),
        ]),
        data: crate::instruction::Graduate {}.data(),
    }
}

/// `claim` by `owner` of its rewards on `mint`, paid in `reward_mint` into its own holding of it.
pub fn claim(owner: Pubkey, mint: Pubkey, reward_mint: Pubkey) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new_readonly(owner, true),
            AccountMeta::new(kit_config_address(&mint), false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new(token_client::holding_address(&mint, &owner), false),
            AccountMeta::new_readonly(reward_mint, false),
            AccountMeta::new(reward_vault_address(&mint, &reward_mint), false),
            AccountMeta::new(token_client::holding_address(&reward_mint, &owner), false),
            AccountMeta::new_readonly(HOOK_AUTHORITY, false),
            AccountMeta::new_readonly(TOKEN_ID, false),
            AccountMeta::new_readonly(TOKEN_EVENT_AUTHORITY, false),
        ]),
        data: crate::instruction::Claim {}.data(),
    }
}

/// `share(amount)` by `sharer` from `source` (a holding of `reward_mint` it may spend) with the
/// holders of `mint`.
pub fn share(
    sharer: Pubkey,
    mint: Pubkey,
    source: Pubkey,
    reward_mint: Pubkey,
    amount: u64,
) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new_readonly(sharer, true),
            AccountMeta::new(kit_config_address(&mint), false),
            AccountMeta::new(source, false),
            AccountMeta::new_readonly(reward_mint, false),
            AccountMeta::new(reward_vault_address(&mint, &reward_mint), false),
            AccountMeta::new_readonly(TOKEN_ID, false),
            AccountMeta::new_readonly(TOKEN_EVENT_AUTHORITY, false),
        ]),
        data: crate::instruction::Share { amount }.data(),
    }
}

/// Reads a `KitConfig` from its account data (owner and discriminator checked): what the launch's
/// pool hook reads `eligible` and `min_eligible` from.
pub fn read_kit_config(info: &AccountInfo) -> Result<KitConfig> {
    require_keys_eq!(
        *info.owner,
        crate::ID,
        anchor_lang::error::ErrorCode::AccountOwnedByWrongProgram
    );
    let data = info.try_borrow_data()?;
    KitConfig::try_deserialize(&mut &data[..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instructions::{holding_amount, registry_list, HOLDING_AMOUNT_OFFSET};
    use crate::state::{check_params, HolderData};
    use bordrless_hook::{token_flags, HOOK_AUTHORITY_SEED};

    #[test]
    fn fixed_addresses_are_their_derivations() {
        assert_eq!(
            Pubkey::find_program_address(&[HOOK_AUTHORITY_SEED], &crate::ID),
            (HOOK_AUTHORITY, HOOK_AUTHORITY_BUMP)
        );
        // The token program's signer of the kit's callbacks, `["hook-authority", KIT_ID]` under
        // the token program: the kit's own, not the token program's signer of another hook.
        assert_eq!(TOKEN_HOOK_AUTHORITY, token_client::hook_signer(&crate::ID));
        assert_eq!(
            TOKEN_HOOK_AUTHORITY,
            Pubkey::find_program_address(&[HOOK_AUTHORITY_SEED, crate::ID.as_ref()], &TOKEN_ID).0
        );
        assert_ne!(
            TOKEN_HOOK_AUTHORITY,
            token_client::hook_signer(&Pubkey::new_unique())
        );
        assert_eq!(TOKEN_EVENT_AUTHORITY, token_client::event_authority());
        assert_eq!(TOKEN_ID, bordrless_token::ID);
        assert_eq!(crate::KIT_ID, crate::ID);
        assert_eq!(
            crate::ID.to_string(),
            "14RJQXPdJfkehit6ezktjd3xujamf8nVSKw2shKamaEH"
        );
    }

    #[test]
    fn flags_of_each_module_set() {
        let t = token_flags::BEFORE_TRANSFER;
        let data =
            token_flags::BEFORE_TRANSFER | token_flags::BEFORE_BURN | token_flags::WRITES_HOOK_DATA;
        assert_eq!(mint_flags(0), 0);
        for m in 1..=15u8 {
            let expected = if m & (modules::HOLDER_REWARDS | modules::EARLY_BUYER_LOCK) != 0 {
                data
            } else {
                t
            };
            assert_eq!(mint_flags(m), expected, "modules {m}");
        }
    }

    #[test]
    fn registry_resolves_to_the_two_extras() {
        let mint = Pubkey::new_unique();
        let vault = reward_vault_address(&mint, &Pubkey::new_unique());
        let prefix = [
            TOKEN_HOOK_AUTHORITY,
            mint,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        for reward_vault in [Some(vault), None] {
            let resolved = registry_list(kit_config_address(&mint), reward_vault)
                .resolve(&prefix, &Pubkey::default(), &Pubkey::default())
                .unwrap();
            assert_eq!(resolved, hook_extras(&mint, reward_vault));
        }
        assert_eq!(hook_extras(&mint, None)[1].pubkey, crate::ID);
        assert!(hook_extras(&mint, None)[0].is_writable);
        assert!(!hook_extras(&mint, Some(vault))[1].is_writable);
        // A DEX slice: the kit, the token program's signer for it, then the two extras.
        let slice = hook_slice(&mint, None);
        assert_eq!(
            (slice[0].pubkey, slice[1].pubkey),
            (crate::ID, TOKEN_HOOK_AUTHORITY)
        );
        assert_eq!(slice[2..], hook_extras(&mint, None)[..]);
    }

    #[test]
    fn holding_amount_reads_the_amount() {
        let holding = bordrless_token::state::Holding {
            version: 1,
            bump: 254,
            mint: Pubkey::new_unique(),
            owner: Pubkey::new_unique(),
            amount: 0x0102_0304_0506_0708,
            delegate: Some(Pubkey::new_unique()),
            delegated_amount: 9,
            frozen: false,
            hook_data: [7; 64],
            reserved: [0; 16],
        };
        let mut data = Vec::new();
        holding.try_serialize(&mut data).unwrap();
        assert_eq!(
            data[HOLDING_AMOUNT_OFFSET..HOLDING_AMOUNT_OFFSET + 8],
            holding.amount.to_le_bytes()
        );
        let key = Pubkey::new_unique();
        let mut lamports = 1u64;
        let info = AccountInfo::new(
            &key,
            false,
            false,
            &mut lamports,
            &mut data,
            &TOKEN_ID,
            false,
        );
        assert_eq!(holding_amount(&info).unwrap(), holding.amount);
        // Another owner, or another account type, is refused.
        let other = Pubkey::new_unique();
        let mut lamports = 1u64;
        let mut copy = data.clone();
        let info = AccountInfo::new(&key, false, false, &mut lamports, &mut copy, &other, false);
        assert!(holding_amount(&info).is_err());
        let mut lamports = 1u64;
        let mut copy = data.clone();
        copy[0] ^= 1;
        let info = AccountInfo::new(
            &key,
            false,
            false,
            &mut lamports,
            &mut copy,
            &TOKEN_ID,
            false,
        );
        assert!(holding_amount(&info).is_err());
    }

    #[test]
    fn hook_data_layout() {
        let data = HolderData {
            snapshot: 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10,
            owed: 0x1112_1314_1516_1718,
            early_locked: 0x2122_2324_2526_2728,
        };
        let bytes = data.to_bytes();
        assert_eq!(bytes[0], 0x10);
        assert_eq!(bytes[15], 0x01);
        assert_eq!(bytes[16], 0x18);
        assert_eq!(bytes[24], 0x28);
        assert!(bytes[32..].iter().all(|b| *b == 0));
        assert_eq!(HolderData::read(&bytes), data);
        assert_eq!(HolderData::default().to_bytes(), [0; 64]);
        // A claim writes bytes 0..24 and keeps 24..64 exactly as read.
        let mut read = [0xAAu8; 64];
        read[0] = 1;
        let claimed = HolderData {
            snapshot: 5,
            owed: 6,
            early_locked: 0,
        }
        .with_rewards_of(&read);
        assert_eq!(&claimed[24..], &read[24..]);
        assert_eq!(HolderData::read(&claimed).snapshot, 5);
        assert_eq!(HolderData::read(&claimed).owed, 6);
    }

    fn args(modules: u8) -> KitInitArgs {
        KitInitArgs {
            launch: Pubkey::new_unique(),
            pool: Pubkey::new_unique(),
            creator: Pubkey::new_unique(),
            reward_mint: Pubkey::new_unique(),
            modules,
            max_wallet_bps: 0,
            creator_unlock_at: 0,
            early_window_end: 0,
            early_unlock_at: 0,
            kit_caller_bump: 255,
        }
    }

    #[test]
    fn init_bounds() {
        use crate::error::KitError;
        let now = 1_800_000_000i64;
        let code = |r: Result<()>| match r {
            Ok(()) => None,
            Err(Error::AnchorError(e)) => Some(e.error_code_number),
            Err(other) => panic!("{other:?}"),
        };
        let e = |k: KitError| Some(u32::from(k));
        assert_eq!(
            code(check_params(&args(0), now)),
            e(KitError::InvalidModules)
        );
        assert_eq!(
            code(check_params(&args(16), now)),
            e(KitError::InvalidModules)
        );
        assert_eq!(code(check_params(&args(1), now)), None);
        // Max wallet: 1..=9_999 exactly when on.
        let mut a = args(modules::MAX_WALLET);
        assert_eq!(code(check_params(&a, now)), e(KitError::InvalidMaxWallet));
        for (bps, ok) in [(1, true), (9_999, true), (10_000, false)] {
            a.max_wallet_bps = bps;
            assert_eq!(code(check_params(&a, now)).is_none(), ok, "bps {bps}");
        }
        let mut a = args(modules::HOLDER_REWARDS);
        a.max_wallet_bps = 100;
        assert_eq!(code(check_params(&a, now)), e(KitError::InvalidMaxWallet));
        // Creator lock: now < unlock <= now + 365 days exactly when on.
        let mut a = args(modules::CREATOR_WALLET_LOCK);
        for (at, ok) in [
            (now, false),
            (now + 1, true),
            (now + MAX_CREATOR_LOCK_SECS, true),
            (now + MAX_CREATOR_LOCK_SECS + 1, false),
        ] {
            a.creator_unlock_at = at;
            assert_eq!(code(check_params(&a, now)).is_none(), ok, "unlock {at}");
        }
        let mut a = args(modules::HOLDER_REWARDS);
        a.creator_unlock_at = now + 10;
        assert_eq!(code(check_params(&a, now)), e(KitError::InvalidCreatorLock));
        // Early lock: now < window end < unlock <= now + 30 days exactly when on.
        let mut a = args(modules::EARLY_BUYER_LOCK);
        for (end, unlock, ok) in [
            (now + 60, now + 3_600, true),
            (now, now + 3_600, false),
            (now + 60, now + 60, false),
            (now + 60, now + MAX_EARLY_LOCK_SECS, true),
            (now + 60, now + MAX_EARLY_LOCK_SECS + 1, false),
        ] {
            a.early_window_end = end;
            a.early_unlock_at = unlock;
            assert_eq!(code(check_params(&a, now)).is_none(), ok, "{end} {unlock}");
        }
        let mut a = args(modules::HOLDER_REWARDS);
        a.early_unlock_at = now + 10;
        assert_eq!(code(check_params(&a, now)), e(KitError::InvalidEarlyLock));

        // The supply and the reward vault.
        let mint = Pubkey::new_unique();
        let vault_key = Pubkey::new_unique();
        let vault = Some(vault_key);
        let install = |a: &KitInitArgs, supply: u64, vault: Option<Pubkey>| -> Option<u32> {
            code(KitConfig::install(a, mint, supply, 254, vault, now).map(|_| ()))
        };
        assert_eq!(
            install(&args(1), 999, vault),
            e(KitError::SupplyOutOfBounds)
        );
        assert_eq!(install(&args(1), 1_000, vault), None);
        assert_eq!(install(&args(1), MAX_SUPPLY, vault), None);
        assert_eq!(
            install(&args(1), MAX_SUPPLY + 1, vault),
            e(KitError::SupplyOutOfBounds)
        );
        assert_eq!(
            install(&args(1), 1_000, None),
            e(KitError::MissingRewardVault)
        );
        assert_eq!(
            install(&args(2 | 4), 1_000, vault),
            e(KitError::InvalidMaxWallet)
        );
        let mut a = args(modules::MAX_WALLET);
        a.max_wallet_bps = 1;
        // 1,000 units at 1 bps would cap every wallet at nothing.
        assert_eq!(install(&a, 1_000, None), e(KitError::InvalidMaxWallet));
        assert_eq!(install(&a, 10_000, None), None);
        assert_eq!(install(&a, 1_000, vault), e(KitError::WrongRewardVault));

        // What init writes.
        let mut a = args(modules::ALL);
        a.max_wallet_bps = 200;
        a.creator_unlock_at = now + 86_400;
        a.early_window_end = now + 60;
        a.early_unlock_at = now + 3_600;
        let c = KitConfig::install(&a, mint, 1_000_000_000_000_000, 251, vault, now).unwrap();
        assert_eq!(
            (c.min_eligible, c.max_wallet_amount, c.eligible, c.seen),
            (1_000_000_000_000, 20_000_000_000_000, 0, 0)
        );
        assert_eq!((c.bump, c.kit_caller_bump, c.modules), (251, 255, 15));
        assert_eq!(
            (c.reward_vault, c.supply_at_init, c.created_at),
            (vault_key, 1_000_000_000_000_000, now)
        );
        let off = KitConfig::install(&args(modules::MAX_WALLET | 4), mint, 5_000, 1, None, now);
        assert!(off.is_err());
        let mut b = args(modules::CREATOR_WALLET_LOCK);
        b.creator_unlock_at = now + 5;
        let c = KitConfig::install(&b, mint, 5_000, 1, None, now).unwrap();
        assert_eq!((c.reward_vault, c.min_eligible), (Pubkey::default(), 5));
    }
}
