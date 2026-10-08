//! Instruction builders for calling the launchpad (tests and the TypeScript SDK's reference). They
//! derive every address they need, off chain.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{system_program, InstructionData};
use bordrless_hook::{hook_accounts_address, HOOK_AUTHORITY_SEED};
use bordrless_kit::client as kit_client;
use bordrless_swap::client as swap_client;
use bordrless_swap::instructions::SwapArgs;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::instructions::{ConfigArgs, CreateConfigArgs, CreateLaunchArgs};
use crate::state::{Launch, LaunchConfig, LaunchRules};

/// This program's `["hook-authority"]` PDA.
pub fn hook_signer() -> Pubkey {
    Pubkey::create_program_address(&[HOOK_AUTHORITY_SEED, &[HOOK_AUTHORITY_BUMP]], &crate::ID)
        .expect("hook authority bump")
}

/// This program's event authority.
pub fn event_authority() -> Pubkey {
    crate::EVENT_AUTHORITY_AND_BUMP.0
}

/// `["config"]`.
pub fn config_address() -> Pubkey {
    Pubkey::create_program_address(&[CONFIG_SEED, &[CONFIG_BUMP]], &crate::ID).expect("config bump")
}

/// The launch of `mint`.
pub fn launch_address(mint: &Pubkey) -> Pubkey {
    Launch::address(mint).0
}

/// The pool of a launch.
pub fn pool_address(mint: &Pubkey, quote_mint: &Pubkey, lp_fee_bps: u16) -> Pubkey {
    swap_client::pool_address(mint, quote_mint, lp_fee_bps, Some(crate::ID))
}

/// The extra-accounts registry of a launch pool.
pub fn registry_address(pool: &Pubkey) -> Pubkey {
    hook_accounts_address(&crate::ID, pool).0
}

/// `PDA(["kit", mint], KIT_ID)`: a launch's kit config (it exists only with kit rules).
pub fn kit_config_address(mint: &Pubkey) -> Pubkey {
    kit_client::kit_config_address(mint)
}

/// `holding(quote_mint, kit_config)`: where a launch's holder fees go (the kit's reward vault; it
/// exists only with holder rewards).
pub fn holder_vault_address(mint: &Pubkey, quote_mint: &Pubkey) -> Pubkey {
    kit_client::reward_vault_address(mint, quote_mint)
}

/// `PDA(["kit-caller", mint], LAUNCH_ID)` and its bump.
pub fn kit_caller_address(mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[KIT_CALLER_SEED, mint.as_ref()], &crate::ID)
}

/// The extra accounts of a launch pool's callbacks, as the registry resolves them (§5.4): the
/// launch (w), its quote holding (w), the holder vault (w) and the kit config (r), whatever the
/// rules.
pub fn hook_extras(mint: &Pubkey, quote_mint: &Pubkey) -> Vec<AccountMeta> {
    let launch = launch_address(mint);
    vec![
        AccountMeta::new(launch, false),
        AccountMeta::new(token_client::holding_address(quote_mint, &launch), false),
        AccountMeta::new(holder_vault_address(mint, quote_mint), false),
        AccountMeta::new_readonly(kit_config_address(mint), false),
    ]
}

/// The token-hook slice of a launch's mint for a DEX instruction when its hook is the kit or
/// none: empty without kit modules, else the kit, its config and the reward vault (or the kit's
/// id without holder rewards). A custom-hook mint's slice is [`custom_hook_slice`].
pub fn base_hook_slice(mint: &Pubkey, quote_mint: &Pubkey, modules: u8) -> Vec<AccountMeta> {
    if modules == 0 {
        return vec![];
    }
    let rewards = modules & bordrless_kit::modules::HOLDER_REWARDS != 0;
    kit_client::hook_slice(
        mint,
        rewards.then(|| holder_vault_address(mint, quote_mint)),
    )
}

/// The token-hook slice of a custom-hook mint for a DEX instruction: the hook, the token
/// program's signer of its callbacks, then `extras`, the hook's extra accounts as the client
/// resolved them from the hook's registry for the mint.
pub fn custom_hook_slice(hook: Pubkey, extras: Vec<AccountMeta>) -> Vec<AccountMeta> {
    swap_client::token_hook_slice(Some(hook), extras)
}

/// A creator's own token hook, as a client passes it to `create_launch` and `graduate` (§5.8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomHookAccounts {
    /// The hook program (the `LaunchConfig`'s).
    pub program: Pubkey,
    /// The hook's extra accounts, resolved from its registry
    /// `["bordrless-hook-accounts", mint]` under the hook, with the writability it lists.
    pub extras: Vec<AccountMeta>,
}

impl CustomHookAccounts {
    /// The DEX slice: the hook, the token program's signer for it, the extras.
    pub fn slice(&self) -> Vec<AccountMeta> {
        custom_hook_slice(self.program, self.extras.clone())
    }

    /// The remaining accounts of `create_launch`: the hook, the token program's signer for it,
    /// its registry for `mint`, then the extras.
    pub fn launch_accounts(&self, mint: &Pubkey) -> Vec<AccountMeta> {
        let mut accounts = vec![
            AccountMeta::new_readonly(self.program, false),
            AccountMeta::new_readonly(token_client::hook_signer(&self.program), false),
            AccountMeta::new_readonly(hook_accounts_address(&self.program, mint).0, false),
        ];
        accounts.extend(self.extras.iter().cloned());
        accounts
    }
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

/// The token program, its signer of the kit's callbacks (the hook signer of a launch mint's token
/// instructions) and its event authority.
fn token_fixed() -> [AccountMeta; 3] {
    [
        AccountMeta::new_readonly(bordrless_token::ID, false),
        AccountMeta::new_readonly(TOKEN_HOOK_AUTHORITY, false),
        AccountMeta::new_readonly(token_client::event_authority(), false),
    ]
}

/// An absent optional account: this program's id.
fn absent() -> AccountMeta {
    AccountMeta::new_readonly(crate::ID, false)
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

/// `create_config`: `launch_config` is a fresh keypair that signs (its key is what a creator
/// pastes into the launch form); the hook program account is passed exactly when
/// `args.custom_hook` names one.
pub fn create_config(
    creator: Pubkey,
    launch_config: Pubkey,
    args: CreateConfigArgs,
) -> Instruction {
    let hook_program = match args.custom_hook {
        Some(hook) => AccountMeta::new_readonly(hook, false),
        None => absent(),
    };
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new(creator, true),
            AccountMeta::new_readonly(config_address(), false),
            AccountMeta::new(launch_config, true),
            hook_program,
            AccountMeta::new_readonly(system_program::ID, false),
        ]),
        data: crate::instruction::CreateConfig { args }.data(),
    }
}

/// `create_listed_config`: as [`create_config`], with the author's share of the creator fee
/// (basis points of it) on every launch someone else makes from it.
pub fn create_listed_config(
    creator: Pubkey,
    launch_config: Pubkey,
    args: CreateConfigArgs,
    author_share_bps: u16,
) -> Instruction {
    let mut ix = create_config(creator, launch_config, args.clone());
    ix.data = crate::instruction::CreateListedConfig {
        args,
        author_share_bps,
    }
    .data();
    ix
}

/// The kit accounts of `create_launch` for a launch of `mint` whose rules install `modules`:
/// the kit, its config, its registry, the reward vault (holder rewards only), the kit-caller PDA
/// and the kit's event authority; each absent (this program's id) without kit modules.
pub fn create_launch_kit_accounts(
    mint: &Pubkey,
    quote_mint: &Pubkey,
    modules: u8,
) -> Vec<AccountMeta> {
    if modules == 0 {
        return vec![absent(); 6];
    }
    let rewards = modules & bordrless_kit::modules::HOLDER_REWARDS != 0;
    vec![
        AccountMeta::new_readonly(KIT_ID, false),
        AccountMeta::new(kit_config_address(mint), false),
        AccountMeta::new(kit_client::registry_address(mint), false),
        if rewards {
            AccountMeta::new(holder_vault_address(mint, quote_mint), false)
        } else {
            absent()
        },
        AccountMeta::new_readonly(kit_caller_address(mint).0, false),
        AccountMeta::new_readonly(kit_client::event_authority(), false),
    ]
}

/// `create_launch` with inline rules. `treasury` is the config's treasury, `quote_mint` the
/// config's quote, `lp_fee_bps` the config's LP fee. The kit accounts follow `args.rules`.
pub fn create_launch(
    creator: Pubkey,
    mint: Pubkey,
    treasury: Pubkey,
    quote_mint: Pubkey,
    lp_fee_bps: u16,
    args: CreateLaunchArgs,
) -> Instruction {
    create_launch_with(
        creator, mint, treasury, quote_mint, lp_fee_bps, args, None, None,
    )
}

/// `create_launch`, from a `LaunchConfig` when `launch_config` is given (`args.rules` and
/// `args.creator_fee_bps` must then be the config's) and with the creator's own hook when the
/// config names one (`custom_hook`, whose extras the client resolved from the hook's registry
/// for `mint`; the hook must have been prepared for the mint first).
#[allow(clippy::too_many_arguments)]
pub fn create_launch_with(
    creator: Pubkey,
    mint: Pubkey,
    treasury: Pubkey,
    quote_mint: Pubkey,
    lp_fee_bps: u16,
    args: CreateLaunchArgs,
    launch_config: Option<Pubkey>,
    custom_hook: Option<&CustomHookAccounts>,
) -> Instruction {
    let launch = launch_address(&mint);
    let pool = pool_address(&mint, &quote_mint, lp_fee_bps);
    let lp_mint = swap_client::lp_mint_address(&pool);
    let mut accounts = vec![
        AccountMeta::new(creator, true),
        AccountMeta::new(config_address(), false),
        AccountMeta::new(treasury, false),
        AccountMeta::new(mint, true),
        AccountMeta::new(launch, false),
        AccountMeta::new(token_client::holding_address(&mint, &launch), false),
        AccountMeta::new(token_client::holding_address(&quote_mint, &launch), false),
        AccountMeta::new_readonly(quote_mint, false),
        AccountMeta::new(registry_address(&pool), false),
        AccountMeta::new(swap_client::config_address(), false),
        AccountMeta::new(pool, false),
        AccountMeta::new(lp_mint, false),
        AccountMeta::new(swap_client::vault_address(&pool, &mint), false),
        AccountMeta::new(swap_client::vault_address(&pool, &quote_mint), false),
        AccountMeta::new(token_client::holding_address(&lp_mint, &launch), false),
        AccountMeta::new_readonly(hook_signer(), false),
        AccountMeta::new_readonly(DEX_HOOK_AUTHORITY, false),
        AccountMeta::new_readonly(swap_client::event_authority(), false),
        AccountMeta::new_readonly(bordrless_swap::ID, false),
    ];
    accounts.extend(token_fixed());
    accounts.push(AccountMeta::new_readonly(system_program::ID, false));
    accounts.extend(create_launch_kit_accounts(
        &mint,
        &quote_mint,
        args.rules.modules(),
    ));
    accounts.push(match launch_config {
        Some(key) => AccountMeta::new_readonly(key, false),
        None => absent(),
    });
    let mut accounts = with_events(accounts);
    if let Some(hook) = custom_hook {
        accounts.extend(hook.launch_accounts(&mint));
    }
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::CreateLaunch { args }.data(),
    }
}

/// `graduate` of a launch whose rules installed `modules` (`Launch.modules`) and whose token has
/// no custom hook: with a kit, its accounts (the reward vault, or the kit's id without holder
/// rewards); without, each absent.
pub fn graduate(
    cranker: Pubkey,
    mint: Pubkey,
    quote_mint: Pubkey,
    lp_fee_bps: u16,
    modules: u8,
) -> Instruction {
    graduate_with(cranker, mint, quote_mint, lp_fee_bps, modules, None)
}

/// `graduate`, with the creator's own hook (its slice as remaining accounts) when the token has
/// one.
pub fn graduate_with(
    cranker: Pubkey,
    mint: Pubkey,
    quote_mint: Pubkey,
    lp_fee_bps: u16,
    modules: u8,
    custom_hook: Option<&CustomHookAccounts>,
) -> Instruction {
    let launch = launch_address(&mint);
    let pool = pool_address(&mint, &quote_mint, lp_fee_bps);
    let lp_mint = swap_client::lp_mint_address(&pool);
    let mut accounts = vec![
        AccountMeta::new_readonly(cranker, true),
        AccountMeta::new(launch, false),
        AccountMeta::new(mint, false),
        AccountMeta::new_readonly(quote_mint, false),
        AccountMeta::new(pool, false),
        AccountMeta::new(token_client::holding_address(&mint, &launch), false),
        AccountMeta::new(swap_client::vault_address(&pool, &mint), false),
        AccountMeta::new_readonly(swap_client::vault_address(&pool, &quote_mint), false),
        AccountMeta::new(lp_mint, false),
        AccountMeta::new(token_client::holding_address(&lp_mint, &launch), false),
        AccountMeta::new_readonly(hook_signer(), false),
        AccountMeta::new_readonly(bordrless_swap::ID, false),
        AccountMeta::new_readonly(swap_client::event_authority(), false),
    ];
    accounts.extend(token_fixed());
    if modules == 0 {
        accounts.extend(vec![absent(); 5]);
    } else {
        let rewards = modules & bordrless_kit::modules::HOLDER_REWARDS != 0;
        accounts.extend([
            AccountMeta::new_readonly(KIT_ID, false),
            AccountMeta::new(kit_config_address(&mint), false),
            AccountMeta::new_readonly(
                if rewards {
                    holder_vault_address(&mint, &quote_mint)
                } else {
                    KIT_ID
                },
                false,
            ),
            AccountMeta::new_readonly(kit_caller_address(&mint).0, false),
            AccountMeta::new_readonly(kit_client::event_authority(), false),
        ]);
    }
    let mut accounts = with_events(accounts);
    if let Some(hook) = custom_hook {
        accounts.extend(hook.slice());
    }
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::Graduate {}.data(),
    }
}

/// `claim_creator_fees`.
pub fn claim_creator_fees(creator: Pubkey, mint: Pubkey, quote_mint: Pubkey) -> Instruction {
    let launch = launch_address(&mint);
    let accounts = vec![
        AccountMeta::new_readonly(creator, true),
        AccountMeta::new(launch, false),
        AccountMeta::new_readonly(quote_mint, false),
        AccountMeta::new(token_client::holding_address(&quote_mint, &launch), false),
        AccountMeta::new(token_client::holding_address(&quote_mint, &creator), false),
        // Bridged SOL has no hook: no hook signer.
        AccountMeta::new_readonly(bordrless_token::ID, false),
        AccountMeta::new_readonly(token_client::event_authority(), false),
    ];
    Instruction {
        program_id: crate::ID,
        accounts: with_events(accounts),
        data: crate::instruction::ClaimCreatorFees {}.data(),
    }
}

/// `claim_creator_fees` for a launch made from a listed config by someone else: the config and
/// the author's holding of the quote follow, and the claim pays the author their share.
pub fn claim_creator_fees_shared(
    creator: Pubkey,
    mint: Pubkey,
    quote_mint: Pubkey,
    launch_config: Pubkey,
    author: Pubkey,
) -> Instruction {
    let mut ix = claim_creator_fees(creator, mint, quote_mint);
    ix.accounts
        .push(AccountMeta::new_readonly(launch_config, false));
    ix.accounts.push(AccountMeta::new(
        token_client::holding_address(&quote_mint, &author),
        false,
    ));
    ix
}

/// `claim_author_fees`: the listed config's author takes their share of the launch's creator
/// fees; the creator's part is paid to the creator at the same time.
pub fn claim_author_fees(
    author: Pubkey,
    mint: Pubkey,
    quote_mint: Pubkey,
    launch_config: Pubkey,
    creator: Pubkey,
) -> Instruction {
    let launch = launch_address(&mint);
    let accounts = vec![
        AccountMeta::new_readonly(author, true),
        AccountMeta::new(launch, false),
        AccountMeta::new_readonly(launch_config, false),
        AccountMeta::new_readonly(quote_mint, false),
        AccountMeta::new(token_client::holding_address(&quote_mint, &launch), false),
        AccountMeta::new(token_client::holding_address(&quote_mint, &creator), false),
        AccountMeta::new(token_client::holding_address(&quote_mint, &author), false),
        AccountMeta::new_readonly(bordrless_token::ID, false),
        AccountMeta::new_readonly(token_client::event_authority(), false),
    ];
    Instruction {
        program_id: crate::ID,
        accounts: with_events(accounts),
        data: crate::instruction::ClaimAuthorFees {}.data(),
    }
}

/// What a client needs to know of a launch to build its swaps and its graduation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaunchKeys {
    /// The token.
    pub mint: Pubkey,
    /// The quote (bridged SOL).
    pub quote_mint: Pubkey,
    /// The pool's LP fee (part of its address).
    pub lp_fee_bps: u16,
    /// The kit modules (`Launch.modules`).
    pub modules: u8,
    /// Whether the pool hook burns on either side (the base mint is then passed writable).
    pub burns: bool,
    /// The creator's own token hook (`Launch.custom_hook`), whose slice a client resolves from
    /// the hook's registry ([`swap_with_base_slice`]).
    pub custom_hook: Option<Pubkey>,
}

impl LaunchKeys {
    /// The keys of a launch of `mint` with `rules` and no custom hook.
    pub fn new(mint: Pubkey, quote_mint: Pubkey, lp_fee_bps: u16, rules: &LaunchRules) -> Self {
        Self {
            mint,
            quote_mint,
            lp_fee_bps,
            modules: rules.modules(),
            burns: rules.burns(),
            custom_hook: None,
        }
    }

    /// The keys of `launch`.
    pub fn of(launch: &Launch) -> Self {
        Self {
            custom_hook: launch.custom_hook,
            ..Self::new(
                launch.mint,
                launch.quote_mint,
                launch.lp_fee_bps,
                &launch.rules,
            )
        }
    }

    /// The launch's pool.
    pub fn pool(&self) -> Pubkey {
        pool_address(&self.mint, &self.quote_mint, self.lp_fee_bps)
    }
}

/// A swap on a launch pool by `trader`, delivered to `recipient`'s holding: direction 1 buys the
/// token with the quote, 0 sells it. For a token whose hook is the kit or none (a custom-hook
/// token's slice must be resolved by the client: [`swap_with_base_slice`]).
pub fn swap(
    keys: &LaunchKeys,
    trader: Pubkey,
    recipient: Pubkey,
    direction: u8,
    amount_in: u64,
    min_amount_out: u64,
) -> Instruction {
    assert!(
        keys.custom_hook.is_none(),
        "a custom-hook token's swap needs its hook slice: use swap_with_base_slice"
    );
    swap_with_base_slice(
        keys,
        trader,
        recipient,
        direction,
        amount_in,
        min_amount_out,
        base_hook_slice(&keys.mint, &keys.quote_mint, keys.modules),
    )
}

/// A swap on a launch pool with the token's token-hook slice given (`base_slice`: the kit's
/// [`base_hook_slice`], or a custom hook's [`custom_hook_slice`]). The base mint is passed
/// writable only when the launch burns (never the quote); the slice goes on the token's side;
/// the pool hook gets its four extras.
#[allow(clippy::too_many_arguments)]
pub fn swap_with_base_slice(
    keys: &LaunchKeys,
    trader: Pubkey,
    recipient: Pubkey,
    direction: u8,
    amount_in: u64,
    min_amount_out: u64,
    base_slice: Vec<AccountMeta>,
) -> Instruction {
    let buy = direction == 1;
    let (trader_base, trader_quote) = if buy {
        (
            token_client::holding_address(&keys.mint, &recipient),
            token_client::holding_address(&keys.quote_mint, &trader),
        )
    } else {
        (
            token_client::holding_address(&keys.mint, &trader),
            token_client::holding_address(&keys.quote_mint, &recipient),
        )
    };
    let (in_hook_accounts, out_hook_accounts) = if buy {
        (0, base_slice.len() as u8)
    } else {
        (base_slice.len() as u8, 0)
    };
    let mut extras = base_slice;
    extras.extend(hook_extras(&keys.mint, &keys.quote_mint));
    swap_client::swap(
        &swap_client::SwapKeys {
            trader,
            pool: keys.pool(),
            base_mint: keys.mint,
            quote_mint: keys.quote_mint,
            trader_base,
            trader_quote,
            hook_program: Some(crate::ID),
            base_mint_writable: keys.burns,
            quote_mint_writable: false,
        },
        SwapArgs {
            direction,
            amount_in,
            min_amount_out,
            in_hook_accounts,
            out_hook_accounts,
            hook_data: vec![],
        },
        extras,
    )
}

/// Reads a launch from its account data.
pub fn read_launch(info: &AccountInfo) -> Result<Launch> {
    require_keys_eq!(
        *info.owner,
        crate::ID,
        anchor_lang::error::ErrorCode::AccountOwnedByWrongProgram
    );
    let data = info.try_borrow_data()?;
    Launch::try_deserialize(&mut &data[..])
}

/// Reads a launch config from its account data.
pub fn read_launch_config(info: &AccountInfo) -> Result<LaunchConfig> {
    require_keys_eq!(
        *info.owner,
        crate::ID,
        anchor_lang::error::ErrorCode::AccountOwnedByWrongProgram
    );
    let data = info.try_borrow_data()?;
    LaunchConfig::try_deserialize(&mut &data[..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instructions::launch::registry_list;

    #[test]
    fn fixed_addresses_are_their_derivations() {
        assert_eq!(
            Pubkey::find_program_address(&[HOOK_AUTHORITY_SEED], &crate::ID),
            (hook_signer(), HOOK_AUTHORITY_BUMP)
        );
        assert_eq!(
            Pubkey::find_program_address(&[CONFIG_SEED], &crate::ID),
            (config_address(), CONFIG_BUMP)
        );
        assert_eq!(CONFIG_ADDRESS, config_address());
        assert_eq!(LAUNCH_HOOK_AUTHORITY, hook_signer());
        assert_eq!(DEX_CONFIG, swap_client::config_address());
        // The DEX's signer of this program's callbacks, and the token program's of the kit's:
        // one per hook program, so never another hook's.
        assert_eq!(DEX_HOOK_AUTHORITY, swap_client::hook_signer(&crate::ID));
        assert_eq!(
            DEX_HOOK_AUTHORITY,
            Pubkey::find_program_address(
                &[HOOK_AUTHORITY_SEED, crate::ID.as_ref()],
                &bordrless_swap::ID
            )
            .0
        );
        assert_ne!(DEX_HOOK_AUTHORITY, swap_client::hook_signer(&KIT_ID));
        assert_eq!(DEX_EVENT_AUTHORITY, swap_client::event_authority());
        assert_eq!(TOKEN_HOOK_AUTHORITY, token_client::hook_signer(&KIT_ID));
        assert_eq!(TOKEN_EVENT_AUTHORITY, token_client::event_authority());
        assert_eq!(KIT_EVENT_AUTHORITY, kit_client::event_authority());
        assert_eq!(KIT_ID, bordrless_kit::ID);
        assert_eq!(bordrless_kit::LAUNCH_ID, crate::ID);
        let mint = Pubkey::new_unique();
        assert_eq!(
            kit_caller_address(&mint),
            kit_client::kit_caller_address(&mint)
        );
        // The programs a config may not name as a custom hook: every protocol program, the
        // system program and the default key.
        assert_eq!(
            PROTOCOL_PROGRAMS,
            [
                bordrless_token::ID,
                bordrless_swap::ID,
                bordrless_kit::constants::BRIDGE_ID,
                crate::ID,
                KIT_ID,
                system_program::ID,
                Pubkey::default(),
            ]
        );
        assert_eq!(
            bordrless_kit::constants::BRIDGE_ID.to_string(),
            "CtLkuFVitoXHTa86Hfp8KmfSDfqJaMYFWr6EGmQVsKb7"
        );
    }

    #[test]
    fn ceilings_agree_with_the_kit() {
        use bordrless_kit::constants as kit;
        assert_eq!(ceilings::MAX_WALLET_BPS, 9_999);
        assert_eq!(
            i64::from(ceilings::CREATOR_LOCK_SECS),
            kit::MAX_CREATOR_LOCK_SECS
        );
        assert_eq!(
            i64::from(ceilings::EARLY_LOCK_SECS),
            kit::MAX_EARLY_LOCK_SECS
        );
        assert_eq!(ceilings::MIN_SUPPLY, 1_000);
        // The policy's bounds pass the ceilings `init_config` and `set_config` enforce; one
        // step over any ceiling does not.
        use crate::instructions::check_rule_bounds;
        use crate::state::RuleBounds;
        use bordrless_core::policy as p;
        let policy = RuleBounds {
            max_holder_fee_bps: p::MAX_HOLDER_FEE_BPS,
            max_burn_bps: p::MAX_BURN_BPS,
            max_rules_fee_bps: p::MAX_RULES_FEE_BPS,
            min_max_wallet_bps: p::MIN_MAX_WALLET_BPS,
            max_max_wallet_bps: p::MAX_MAX_WALLET_BPS,
            max_creator_lock_secs: p::MAX_CREATOR_LOCK_SECS,
            max_early_window_secs: p::MAX_EARLY_WINDOW_SECS,
            max_early_lock_secs: p::MAX_EARLY_LOCK_SECS,
        };
        assert!(check_rule_bounds(&policy).is_ok());
        let at_ceilings = RuleBounds {
            max_holder_fee_bps: ceilings::HOLDER_FEE_BPS,
            max_burn_bps: ceilings::BURN_BPS,
            max_rules_fee_bps: ceilings::RULES_FEE_BPS,
            min_max_wallet_bps: ceilings::MIN_MAX_WALLET_BPS,
            max_max_wallet_bps: ceilings::MAX_WALLET_BPS,
            max_creator_lock_secs: ceilings::CREATOR_LOCK_SECS,
            max_early_window_secs: ceilings::EARLY_WINDOW_SECS,
            max_early_lock_secs: ceilings::EARLY_LOCK_SECS,
        };
        assert!(check_rule_bounds(&at_ceilings).is_ok());
        let over: [fn(&mut RuleBounds); 8] = [
            |b| b.max_holder_fee_bps += 1,
            |b| b.max_burn_bps += 1,
            |b| b.max_rules_fee_bps += 1,
            |b| b.min_max_wallet_bps -= 1,
            |b| b.max_max_wallet_bps += 1,
            |b| b.max_creator_lock_secs += 1,
            |b| b.max_early_window_secs += 1,
            |b| b.max_early_lock_secs += 1,
        ];
        for (i, step) in over.iter().enumerate() {
            let mut b = at_ceilings;
            step(&mut b);
            assert!(check_rule_bounds(&b).is_err(), "ceiling {i}");
        }
    }

    #[test]
    fn registry_resolves_to_the_four_extras() {
        let mint = Pubkey::new_unique();
        let quote = Pubkey::new_unique();
        let prefix = [
            DEX_HOOK_AUTHORITY,
            pool_address(&mint, &quote, 30),
            mint,
            quote,
            Pubkey::new_unique(),
        ];
        let resolved = registry_list(
            holder_vault_address(&mint, &quote),
            kit_config_address(&mint),
        )
        .resolve(&prefix, &Pubkey::default(), &Pubkey::default())
        .unwrap();
        assert_eq!(resolved, hook_extras(&mint, &quote));
        // The indices the hook names its deltas by.
        assert_eq!(
            resolved[usize::from(QUOTE_HOLDING_INDEX) - 5].pubkey,
            token_client::holding_address(&quote, &launch_address(&mint))
        );
        assert_eq!(
            resolved[usize::from(HOLDER_VAULT_INDEX) - 5].pubkey,
            token_client::holding_address(&quote, &kit_config_address(&mint))
        );
        assert_eq!(
            resolved[usize::from(KIT_CONFIG_INDEX) - 5].pubkey,
            kit_config_address(&mint)
        );
        assert!(resolved[2].is_writable && !resolved[3].is_writable);
    }

    #[test]
    fn rules_install_their_modules() {
        let mut r = LaunchRules::NONE;
        assert_eq!(r.modules(), 0);
        r.burn_buy_bps = 50;
        r.burn_sell_bps = 50;
        assert_eq!((r.modules(), r.burns()), (0, true));
        r.holder_fee_sell_bps = 200;
        assert_eq!(r.modules(), 1);
        r.max_wallet_bps = 200;
        r.creator_lock_secs = 1;
        r.early_window_secs = 60;
        r.early_lock_secs = 3_600;
        assert_eq!(r.modules(), 15);
        assert_eq!(
            create_launch_kit_accounts(&Pubkey::new_unique(), &Pubkey::new_unique(), 0),
            vec![absent(); 6]
        );
    }

    #[test]
    fn a_custom_hooks_accounts_are_its_slice_plus_the_registry() {
        let (hook, mint) = (Pubkey::new_unique(), Pubkey::new_unique());
        let extra = AccountMeta::new(Pubkey::new_unique(), false);
        let custom = CustomHookAccounts {
            program: hook,
            extras: vec![extra.clone()],
        };
        let slice = custom.slice();
        assert_eq!(
            (slice[0].pubkey, slice[1].pubkey, slice[2].clone()),
            (hook, token_client::hook_signer(&hook), extra.clone())
        );
        let launch_accounts = custom.launch_accounts(&mint);
        assert_eq!(launch_accounts.len(), 4);
        assert_eq!(
            launch_accounts[2].pubkey,
            hook_accounts_address(&hook, &mint).0
        );
        assert_eq!(launch_accounts[3], extra);
        // `create_launch` from a config with a custom hook: the config after the kit accounts,
        // the hook's accounts after the event authority and the program.
        let config = Pubkey::new_unique();
        let args = CreateLaunchArgs {
            name: "x".into(),
            symbol: "X".into(),
            uri: String::new(),
            creator_fee_bps: 100,
            virtual_quote: 1,
            rules: LaunchRules::NONE,
        };
        let ix = create_launch_with(
            Pubkey::new_unique(),
            mint,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            30,
            args.clone(),
            Some(config),
            Some(&custom),
        );
        let n = ix.accounts.len();
        assert_eq!(ix.accounts[n - 7].pubkey, config);
        assert_eq!(ix.accounts[n - 5].pubkey, crate::ID);
        assert_eq!(ix.accounts[n - 4..], launch_accounts[..]);
        let plain = create_launch(
            Pubkey::new_unique(),
            mint,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            30,
            args,
        );
        assert_eq!(plain.accounts.len(), n - 4);
        assert_eq!(plain.accounts[n - 7], absent());
        // `graduate` with a custom hook: its slice after the event authority and the program.
        let g = graduate_with(
            Pubkey::new_unique(),
            mint,
            Pubkey::new_unique(),
            30,
            0,
            Some(&custom),
        );
        let m = g.accounts.len();
        assert_eq!(g.accounts[m - 3..], slice[..]);
        assert_eq!(
            graduate(Pubkey::new_unique(), mint, Pubkey::new_unique(), 30, 0)
                .accounts
                .len(),
            m - 3
        );
    }
}
