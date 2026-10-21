//! Launches quoted in tokens (`docs/token-quotes.md` §2.2): a coin launched against 1 to 6 real
//! SPL Token or Token-2022 mints instead of SOL, never both.
//!
//! `create_token_launch` makes the mint (the kit as its hook with kit rules, a launch config's
//! custom hook with one, none otherwise),
//! mints the whole supply to the launch's holding and records the `Launch` (`reserved[18] = 2`;
//! `pool` and `quote_mint` name leg 0) and its `Legs`: per quote mint (the remaining accounts, in
//! order, each held to the DEX's policy, none wrapped SOL, none repeated) its program, decimals,
//! virtual quote (at least [`MIN_LEG_VIRTUAL_QUOTE`]), share of the supply (an even split, the
//! last leg taking the remainder), pool and fee account. `open_leg(i)`, permissionless, creates
//! leg `i`'s fee account (a token account of the quote at `["leg-fees", mint, quote_mint]` owned by
//! the launch), its pool (DEX `create_spl_pool` with this program as the hook: no virtual base, the
//! leg's virtual quote, the leg's share deposited, the share fee model) and the pool's registry.
//! The pool hook refuses trades until every leg is open, then takes the creator fee on the quote
//! side of every trade into the leg's fee account (`crate::instructions::hooks`).
//! `claim_creator_fees_token(i)`, anyone, pays leg `i`'s fee account to the creator's associated
//! token account of that quote. Legs never graduate.
//!
//! Rules (`docs/token-quotes.md` §6.1): `create_token_launch` takes `create_launch`'s
//! `LaunchRules`, held to the same bounds, without a holder fee yet. The burn is every leg's pool
//! hook's; max wallet, the creator wallet lock and the early-buyer lock are the kit's, installed on
//! the mint by `init_legs` as `create_launch` installs a SOL coin's (the mint names the kit as its
//! hook from its first instruction). The kit takes the coin's `Legs` in its slice and treats every
//! leg's pool as the pool; max wallet lifts once the legs together have sold the supply's
//! `curve_bps`, which the pool hook counts on the `Legs` (`Legs::lift_at`, `Legs::sold`).
//!
//! Workshop items (`docs/token-quotes.md` §7, 21 Oct 2026, "builder decides"): the creator may be a
//! companion's creator address (`PDA(["creator", mint], COMPANION_ID)`), which only the companion
//! program can sign for (its `launch_token`); and the launch may be made from a `LaunchConfig`
//! (after the quote mints), whose rules and creator fee the arguments repeat, held to
//! `create_launch`'s hook rule (who may upgrade the hook was checked when the config was made; a
//! timelocked hook's timelock must hold no proposal), with no SOL cut and no author's or partner's
//! share (both are paid from SOL), and whose custom hook (no kit rules beside it) becomes the
//! coin's token hook. The hook learns the coin's legs from its registry, which must list the
//! coin's `Legs` (`Pda(LAUNCH_ID, ["legs", Account(1)])`, as the kit's registry for such a coin
//! does); `open_leg` passes the hook's slice on the deposit, and every leg trade carries it on the
//! coin's side.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_lang::system_program;
use anchor_lang::Discriminator;
use bordrless_hook::{
    hook_accounts_address, write_registry, AccountSource, ExtraAccount, HookAccountList, Seed,
    HOOK_AUTHORITY_SEED,
};
use bordrless_kit::KitInitArgs;
use bordrless_swap::instructions::CreatePoolArgs;
use bordrless_swap::spl;
use bordrless_token::client as token_client;
use bordrless_token::instructions::CreateMintArgs;
use bordrless_token::state::AuthorityKind;

use crate::constants::*;
use crate::cpi;
use crate::error::LaunchError;
use crate::events::{
    LegCreatorFeesClaimed, LegOpened, LegTerms, TokenLaunchConfigured, TokenLaunchCreated,
    TokenLaunchLocks, TokenLaunchPayouts, TokenLaunchRules,
};
use crate::instructions::launch::{
    check_custom_hook_accounts, check_rules, CustomHook, KitCallerSeeds, LaunchSeeds,
};
use crate::state::*;

/// Arguments of `create_token_launch`.
#[derive(AnchorSerialize, Clone, Debug)]
pub struct CreateTokenLaunchArgs {
    /// Name.
    pub name: String,
    /// Symbol.
    pub symbol: String,
    /// Metadata URI.
    pub uri: String,
    /// Creator fee in basis points of the quote side of every trade, within the config's bounds.
    pub creator_fee_bps: u16,
    /// Each leg's virtual quote, raw units of its token, in the order of the quote mints.
    pub virtual_quotes: Vec<u64>,
    /// The token rules, as `create_launch` takes them and within the same bounds
    /// (`docs/token-quotes.md` §6.1, §7.5.1): the holder fee (the kit's holder rewards, paid in the
    /// legs' tokens or in SOL by `payouts`), the burn on buys and sells (every leg's pool hook), max
    /// wallet, the creator wallet lock and the early-buyer lock (the kit). Optional in the data:
    /// data that ends after `virtual_quotes`, as every call made before rules, reads as no rules.
    pub rules: LaunchRules,
    /// How the coin pays out (`docs/token-quotes.md` §7.5): the SOL switch and the holder rounds.
    /// Optional in the data, after `rules`: absent reads as zero, and zero is written as nothing,
    /// so a launch without them sends exactly the data it always sent.
    pub payouts: TokenPayouts,
}

/// The payout settings of a coin quoted in tokens (`docs/token-quotes.md` §7.5): `flags` bit 0 the
/// SOL switch ([`PAYOUT_SOL`]: the launcher's part, a companion's holders' and launcher's parts and
/// the kit's holder rewards paid in SOL, sold on the token's reference pool by `swap_to_sol`), every
/// other bit 0 (`PayoutsInvalid`); `holder_round_secs` the round of holder rewards paid in every
/// leg's token on a coin of two legs or more (600 to 86,400 seconds; 0 is an hour; ignored on
/// other coins). Five bytes at the end of `create_token_launch`'s data when either is set, else
/// none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenPayouts {
    pub flags: u8,
    pub holder_round_secs: u32,
}

impl TokenPayouts {
    /// Whether the SOL switch is on.
    pub fn sol(&self) -> bool {
        self.flags & PAYOUT_SOL != 0
    }
}

impl AnchorSerialize for TokenPayouts {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        if *self != Self::default() {
            writer.write_all(&[self.flags])?;
            writer.write_all(&self.holder_round_secs.to_le_bytes())?;
        }
        Ok(())
    }
}

impl AnchorDeserialize for TokenPayouts {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let mut first = [0u8; 1];
        let read = loop {
            match reader.read(&mut first) {
                Err(e) if e.kind() == borsh::io::ErrorKind::Interrupted => continue,
                other => break other?,
            }
        };
        if read == 0 {
            return Ok(Self::default());
        }
        let mut secs = [0u8; 4];
        reader.read_exact(&mut secs)?;
        Ok(Self {
            flags: first[0],
            holder_round_secs: u32::from_le_bytes(secs),
        })
    }
}

#[cfg(feature = "idl-build")]
impl anchor_lang::IdlBuild for TokenPayouts {
    fn create_type() -> Option<anchor_lang::idl::types::IdlTypeDef> {
        use anchor_lang::idl::types::*;
        Some(IdlTypeDef {
            name: Self::get_full_path(),
            docs: vec![
                "Optional at the end of the data: 5 bytes (flags u8, holder_round_secs u32) when either is set, none when both are 0.".into(),
            ],
            serialization: IdlSerialization::default(),
            repr: None,
            generics: vec![],
            ty: IdlTypeDefTy::Struct {
                fields: Some(IdlDefinedFields::Named(vec![
                    IdlField {
                        name: "flags".into(),
                        docs: vec!["Bit 0: the SOL switch.".into()],
                        ty: IdlType::U8,
                    },
                    IdlField {
                        name: "holder_round_secs".into(),
                        docs: vec![],
                        ty: IdlType::U32,
                    },
                ])),
            },
        })
    }

    fn insert_types(
        types: &mut std::collections::BTreeMap<String, anchor_lang::idl::types::IdlTypeDef>,
    ) {
        if let Some(ty) = Self::create_type() {
            types.insert(Self::get_full_path(), ty);
        }
    }
}

impl AnchorDeserialize for CreateTokenLaunchArgs {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<Self> {
        Ok(Self {
            name: AnchorDeserialize::deserialize_reader(reader)?,
            symbol: AnchorDeserialize::deserialize_reader(reader)?,
            uri: AnchorDeserialize::deserialize_reader(reader)?,
            creator_fee_bps: AnchorDeserialize::deserialize_reader(reader)?,
            virtual_quotes: AnchorDeserialize::deserialize_reader(reader)?,
            rules: trailing_rules(reader)?,
            payouts: AnchorDeserialize::deserialize_reader(reader)?,
        })
    }
}

/// The rules at the end of `create_token_launch`'s data: none when the data ends before them, else
/// all of their bytes (a part of them fails).
fn trailing_rules<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<LaunchRules> {
    let mut first = [0u8; 1];
    let read = loop {
        match reader.read(&mut first) {
            Err(e) if e.kind() == borsh::io::ErrorKind::Interrupted => continue,
            other => break other?,
        }
    };
    if read == 0 {
        return Ok(LaunchRules::NONE);
    }
    let mut bytes = [0u8; LaunchRules::INIT_SPACE];
    bytes[0] = first[0];
    reader.read_exact(&mut bytes[1..])?;
    LaunchRules::try_from_slice(&bytes)
}

/// Accounts of `create_token_launch`. The remaining accounts are the quote mints, 1 to 6, in leg
/// order (read-only); with rules that install the kit, the kit's six accounts follow them
/// ([`TOKEN_LAUNCH_KIT_ACCOUNTS`]: the kit, the token program's signer of its callbacks, the kit
/// config (w), the kit's registry for the mint (w), the mint's kit-caller PDA, the kit's event
/// authority). A launch made from a `LaunchConfig` (`docs/token-quotes.md` §7) has it right after
/// the quote mints, then, when it names a custom hook, `create_launch`'s hook accounts (the hook,
/// the token program's signer for it, its registry for the mint, its extras, and a timelocked
/// hook's `Timelock`), else the kit's six when its rules install the kit.
#[event_cpi]
#[derive(Accounts)]
pub struct CreateTokenLaunch<'info> {
    /// The creator: a wallet, or a companion's creator address (`PDA(["creator", mint])` under the
    /// companion, signing through its `launch_token`). Pays the launch fee and the rent.
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(mut, address = CONFIG_ADDRESS)]
    pub config: Box<Account<'info, Config>>,
    /// CHECK: receives the launch fee (address-checked).
    #[account(mut, address = config.treasury @ LaunchError::WrongHolding)]
    pub treasury: UncheckedAccount<'info>,
    /// CHECK: the new mint; signs, created through the token program.
    #[account(mut)]
    pub mint: Signer<'info>,
    #[account(init, payer = creator, space = Launch::LEN, seeds = [LAUNCH_SEED, mint.key().as_ref()], bump)]
    pub launch: Box<Account<'info, Launch>>,
    #[account(init, payer = creator, space = Legs::LEN, seeds = [LEGS_SEED, mint.key().as_ref()], bump)]
    pub legs: Box<Account<'info, Legs>>,
    /// CHECK: the launch's holding of the coin, created here (the token program checks its
    /// address).
    #[account(mut)]
    pub launch_reserve: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = bordrless_token::ID @ LaunchError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority (address-checked).
    #[account(address = TOKEN_EVENT_AUTHORITY @ LaunchError::WrongProgram)]
    pub token_event_authority: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// The legs' terms from the quote mints and their virtual quotes, checked: 1 to 6, each an SPL
/// Token or Token-2022 mint the DEX's policy allows (`bordrless_swap::spl::check_quote_mint`, which
/// `create_spl_pool` repeats), none wrapped SOL, none repeated, each virtual quote at least the
/// minimum; the pools' and fee accounts' addresses derived.
fn leg_terms(
    quotes: &[AccountInfo],
    virtual_quotes: &[u64],
    mint: &Pubkey,
    supply: u64,
    lp_fee_bps: u16,
) -> Result<[Leg; MAX_LEGS]> {
    let n = quotes.len();
    require!(
        (1..=MAX_LEGS).contains(&n) && virtual_quotes.len() == n,
        LaunchError::InvalidLegCount
    );
    let count = n as u64;
    let even = supply / count;
    let mut legs = [Leg::default(); MAX_LEGS];
    for (i, info) in quotes.iter().enumerate() {
        require!(
            *info.key != spl::NATIVE_MINT && *info.key != spl::NATIVE_MINT_2022,
            LaunchError::SolQuoteToken
        );
        require!(
            !quotes[..i].iter().any(|q| q.key == info.key),
            LaunchError::DuplicateQuoteToken
        );
        let (quote_program, decimals) = spl::check_quote_mint(info).map_err(LaunchError::from)?;
        let virtual_quote = virtual_quotes[i];
        require!(
            virtual_quote >= MIN_LEG_VIRTUAL_QUOTE,
            LaunchError::LegVirtualQuoteTooSmall
        );
        // An even split; the last leg takes what the division leaves (`legShares`).
        let share = if i == n - 1 {
            supply - even * (count - 1)
        } else {
            even
        };
        let pool =
            bordrless_swap::state::Pool::address(mint, info.key, lp_fee_bps, Some(crate::ID)).0;
        let (fee_account, fee_bump) = Pubkey::find_program_address(
            &[LEG_FEES_SEED, mint.as_ref(), info.key.as_ref()],
            &crate::ID,
        );
        legs[i] = Leg {
            quote_mint: *info.key,
            quote_program,
            decimals,
            fee_bump,
            virtual_quote,
            share,
            pool,
            fee_account,
            creator_fees_accrued: 0,
            creator_fees_claimed: 0,
            opened_at: 0,
        };
    }
    Ok(legs)
}

/// The kit's accounts of a launch quoted in tokens whose rules install kit modules, checked.
struct TokenKit<'info> {
    program: AccountInfo<'info>,
    hook_signer: AccountInfo<'info>,
    config: AccountInfo<'info>,
    registry: AccountInfo<'info>,
    caller: AccountInfo<'info>,
    events: AccountInfo<'info>,
    caller_bump: u8,
}

/// The remaining accounts of `create_token_launch`: the quote mints, then, with kit `modules`, the
/// kit's [`TOKEN_LAUNCH_KIT_ACCOUNTS`], each at its address (the kit checks its registry's seeds).
/// Without kit modules every remaining account is a quote mint, as before rules.
#[allow(clippy::type_complexity)]
fn split_remaining<'a, 'info>(
    remaining: &'a [AccountInfo<'info>],
    modules: u8,
    mint: &Pubkey,
    kit_config: &Pubkey,
) -> Result<(&'a [AccountInfo<'info>], Option<TokenKit<'info>>)> {
    if modules == 0 {
        return Ok((remaining, None));
    }
    require!(
        remaining.len() > TOKEN_LAUNCH_KIT_ACCOUNTS,
        LaunchError::KitAccountsMissing
    );
    let (quotes, kit) = remaining.split_at(remaining.len() - TOKEN_LAUNCH_KIT_ACCOUNTS);
    Ok((quotes, Some(kit_of(kit, mint, kit_config)?)))
}

/// The kit's [`TOKEN_LAUNCH_KIT_ACCOUNTS`] accounts (`kit`), each at its address (the kit checks
/// its registry's seeds).
fn kit_of<'info>(
    kit: &[AccountInfo<'info>],
    mint: &Pubkey,
    kit_config: &Pubkey,
) -> Result<TokenKit<'info>> {
    let (caller, caller_bump) =
        Pubkey::find_program_address(&[KIT_CALLER_SEED, mint.as_ref()], &crate::ID);
    require!(
        *kit[0].key == KIT_ID
            && *kit[1].key == TOKEN_HOOK_AUTHORITY
            && kit[2].key == kit_config
            && *kit[4].key == caller
            && *kit[5].key == KIT_EVENT_AUTHORITY,
        LaunchError::WrongKitAccount
    );
    Ok(TokenKit {
        program: kit[0].clone(),
        hook_signer: kit[1].clone(),
        config: kit[2].clone(),
        registry: kit[3].clone(),
        caller: kit[4].clone(),
        events: kit[5].clone(),
        caller_bump,
    })
}

/// The registry entry through which a custom hook on a coin quoted in tokens learns the coin's
/// legs (`docs/token-quotes.md` §7): the launch's `Legs`, `Pda(LAUNCH_ID, ["legs", Account(1)])`
/// (the mint), as the kit's registry for such a coin lists it (§6.1). Read-only: only this program
/// writes the `Legs`.
pub fn legs_registry_entry() -> ExtraAccount {
    ExtraAccount {
        writable: false,
        source: AccountSource::Pda {
            program: crate::ID,
            seeds: vec![Seed::Literal(LEGS_SEED.to_vec()), Seed::Account(1)],
        },
    }
}

/// Whether a hook's registry lists the coin's `Legs` ([`legs_registry_entry`], writable or not).
pub fn lists_legs(list: &HookAccountList) -> bool {
    let wanted = legs_registry_entry().source;
    list.accounts.iter().any(|a| a.source == wanted)
}

/// Where the `LaunchConfig` stands among `create_token_launch`'s remaining accounts: the first
/// account this program owns that holds a `LaunchConfig`, right after the quote mints (a quote mint
/// never is one: only SPL Token and Token-2022 mints pass the DEX's policy). None: no config, and
/// every remaining account is read as before configs.
fn config_at(remaining: &[AccountInfo]) -> Option<usize> {
    remaining.iter().position(|a| {
        *a.owner == crate::ID
            && a.try_borrow_data()
                .is_ok_and(|d| d.starts_with(LaunchConfig::DISCRIMINATOR))
    })
}

/// A launch config on a coin quoted in tokens (`docs/token-quotes.md` §7): the arguments repeat its
/// rules and creator fee (`ConfigMismatch`); no SOL cut (`QuoteCutOnTokenLaunch`: a hook's SOL cut
/// would have nothing to be paid in); no partner and, unless the creator is its author, no author
/// share (`ConfigSharesOnTokenLaunch`: both are paid by the SOL claims, which a coin quoted in
/// tokens never has; the creator's whole fee goes to the creator, per leg); a custom hook with no
/// kit rules (`CustomHookWithKitRules`, as `create_launch`).
fn check_token_config(
    lc: &LaunchConfig,
    creator: &Pubkey,
    args: &CreateTokenLaunchArgs,
) -> Result<()> {
    require!(
        args.rules == lc.rules && args.creator_fee_bps == lc.creator_fee_bps,
        LaunchError::ConfigMismatch
    );
    require!(lc.quote_cut().is_none(), LaunchError::QuoteCutOnTokenLaunch);
    let author = lc.creator != *creator && lc.author_share_bps > 0;
    require!(
        !author && lc.partner_share_bps() == 0,
        LaunchError::ConfigSharesOnTokenLaunch
    );
    if lc.hook().is_some() {
        require!(lc.rules.modules() == 0, LaunchError::CustomHookWithKitRules);
    }
    Ok(())
}

/// What `create_token_launch`'s remaining accounts hold: the quote mints, the kit's accounts, the
/// config and its custom hook.
struct TokenRemaining<'a, 'info> {
    quotes: &'a [AccountInfo<'info>],
    kit: Option<TokenKit<'info>>,
    config: Option<Pubkey>,
    custom: Option<CustomHook<'info>>,
}

/// The remaining accounts of `create_token_launch`, checked: without a config as before configs
/// ([`split_remaining`]); with one ([`config_at`], [`check_token_config`]), the quote mints before
/// it and after it the custom hook's accounts (`create_launch`'s, checked as it checks them, the
/// hook's registry listing the coin's `Legs`: `HookMissingLegs`), or the kit's six when the rules
/// install the kit, or nothing.
fn token_remaining<'a, 'info>(
    remaining: &'a [AccountInfo<'info>],
    args: &CreateTokenLaunchArgs,
    creator: &Pubkey,
    mint: &Pubkey,
    kit_config: &Pubkey,
) -> Result<TokenRemaining<'a, 'info>> {
    let modules = args.rules.modules();
    let Some(at) = config_at(remaining) else {
        let (quotes, kit) = split_remaining(remaining, modules, mint, kit_config)?;
        return Ok(TokenRemaining {
            quotes,
            kit,
            config: None,
            custom: None,
        });
    };
    let info = &remaining[at];
    let lc = LaunchConfig::try_deserialize(&mut &info.try_borrow_data()?[..])?;
    check_token_config(&lc, creator, args)?;
    let (quotes, after) = (&remaining[..at], &remaining[at + 1..]);
    let (kit, custom) = match lc.hook() {
        Some(_) => {
            let custom = check_custom_hook_accounts(after, lc.hook(), lc.hook_timelocked(), mint)?;
            // `check_custom_hook_accounts` decoded the registry (`after[2]`) already.
            let list = HookAccountList::decode(&after[2].try_borrow_data()?)
                .ok_or(LaunchError::HookRegistryMissing)?;
            require!(lists_legs(&list), LaunchError::HookMissingLegs);
            (None, custom)
        }
        None if modules == 0 => {
            require!(after.is_empty(), LaunchError::UnexpectedCustomHookAccounts);
            (None, None)
        }
        None => {
            require!(
                after.len() == TOKEN_LAUNCH_KIT_ACCOUNTS,
                LaunchError::KitAccountsMissing
            );
            (Some(kit_of(after, mint, kit_config)?), None)
        }
    };
    Ok(TokenRemaining {
        quotes,
        kit,
        config: Some(*info.key),
        custom,
    })
}

/// The mint (the kit as its token hook when the rules install kit modules, a config's custom hook
/// with its flags when it names one, no hook otherwise; the launch its only authority), the
/// launch's holding of it, the whole supply into that holding (the kit does not subscribe to mints;
/// a custom hook may, so its extras go along, as `create_launch` passes them), then no more minting
/// ever.
#[inline(never)]
fn mint_coin<'info>(
    ctx: &Context<'info, CreateTokenLaunch<'info>>,
    args: &CreateTokenLaunchArgs,
    kit: Option<&TokenKit<'info>>,
    custom: Option<&CustomHook<'info>>,
    modules: u8,
) -> Result<()> {
    let a = &ctx.accounts;
    let config = &a.config;
    let (mint, launch) = (a.mint.key(), a.launch.key());
    let token_program = a.token_program.to_account_info();
    let events = a.token_event_authority.to_account_info();
    let (hook_program, hook_flags) = match custom {
        Some(c) => (Some(*c.program.key), c.flags),
        None => (kit.map(|_| KIT_ID), bordrless_kit::mint_flags(modules)),
    };
    let ix = cpi::token_create_mint(
        a.creator.key(),
        mint,
        CreateMintArgs {
            decimals: config.decimals,
            name: args.name.clone(),
            symbol: args.symbol.clone(),
            uri: args.uri.clone(),
            max_supply: config.supply,
            mint_authority: Some(launch),
            freeze_authority: None,
            hook_program,
            hook_flags,
            hook_authority: None,
            metadata_authority: None,
        },
    );
    invoke_signed(
        &ix,
        &[
            a.creator.to_account_info(),
            a.mint.to_account_info(),
            a.system_program.to_account_info(),
            events.clone(),
            token_program.clone(),
        ],
        &[],
    )?;
    let ix = cpi::token_create_holding(a.creator.key(), mint, launch, a.launch_reserve.key());
    invoke_signed(
        &ix,
        &[
            a.creator.to_account_info(),
            a.mint.to_account_info(),
            a.launch.to_account_info(),
            a.launch_reserve.to_account_info(),
            a.system_program.to_account_info(),
            events.clone(),
            token_program.clone(),
        ],
        &[],
    )?;
    let launch_seeds = LaunchSeeds::new(mint, ctx.bumps.launch);
    let seeds = launch_seeds.seeds();
    let extra_metas: Vec<AccountMeta> = custom.map_or_else(Vec::new, |c| {
        c.extras
            .iter()
            .map(|e| AccountMeta {
                pubkey: *e.key,
                is_signer: false,
                is_writable: e.is_writable,
            })
            .collect()
    });
    let ix = cpi::token_mint_to(
        launch,
        mint,
        a.launch_reserve.key(),
        match custom {
            Some(c) => Some(c.hook()),
            None => kit.map(|_| cpi::KIT_HOOK),
        },
        &extra_metas,
        config.supply,
    );
    let (hook_info, signer_info) = match (custom, kit) {
        (Some(c), _) => (c.program.clone(), c.signer.clone()),
        (None, Some(k)) => (k.program.clone(), k.hook_signer.clone()),
        (None, None) => (token_program.clone(), token_program.clone()),
    };
    let mut infos = vec![
        a.launch.to_account_info(),
        a.mint.to_account_info(),
        a.launch_reserve.to_account_info(),
        hook_info,
        signer_info,
        events.clone(),
        token_program.clone(),
    ];
    if let Some(c) = custom {
        infos.extend(c.extras.iter().cloned());
    }
    invoke_signed(&ix, &infos, &[&seeds])?;
    let ix = cpi::token_set_authority(launch, mint, AuthorityKind::Mint, None);
    invoke_signed(
        &ix,
        &[
            a.launch.to_account_info(),
            a.mint.to_account_info(),
            events,
            token_program,
        ],
        &[&seeds],
    )?;
    Ok(())
}

/// `create_token_launch`.
pub fn process_create_token_launch<'info>(
    mut ctx: Context<'info, CreateTokenLaunch<'info>>,
    args: CreateTokenLaunchArgs,
) -> Result<()> {
    let clock = Clock::get()?;
    let config = &ctx.accounts.config;
    require!(!config.paused, LaunchError::Paused);
    require!(
        !args.name.is_empty()
            && args.name.len() <= 32
            && !args.symbol.is_empty()
            && args.symbol.len() <= 10
            && args.uri.len() <= 200,
        LaunchError::InvalidMetadata
    );
    require!(
        args.creator_fee_bps <= config.max_creator_fee_bps,
        LaunchError::CreatorFeeTooHigh
    );
    // The rules within the bounds a SOL launch's are held to, the holder fee included
    // (`docs/token-quotes.md` §7.5.1: the kit's holder rewards in the legs' tokens or in SOL).
    let rules = args.rules;
    check_rules(&rules, args.creator_fee_bps, config)?;
    let payouts = args.payouts;
    require!(
        payouts.flags & !PAYOUT_SOL == 0
            && (payouts.holder_round_secs == 0
                || (MIN_HOLDER_ROUND_SECS..=MAX_HOLDER_ROUND_SECS)
                    .contains(&payouts.holder_round_secs)),
        LaunchError::PayoutsInvalid
    );
    let modules = rules.modules();
    let mint = ctx.accounts.mint.key();
    // A companion's creator address may be the creator (`docs/token-quotes.md` §7): only the
    // companion program signs for it, through its `launch_token` (a script companion's, whose
    // script spends each leg's creator fee in that leg's token).
    let (supply, lp_fee_bps, launch_fee) =
        (config.supply, config.lp_fee_bps, config.launch_fee_lamports);
    let (kit_config, _) = Pubkey::find_program_address(&[KIT_SEED, mint.as_ref()], &KIT_ID);
    let TokenRemaining {
        quotes,
        kit,
        config: launch_config,
        custom,
    } = token_remaining(
        ctx.remaining_accounts,
        &args,
        &ctx.accounts.creator.key(),
        &mint,
        &kit_config,
    )?;
    let legs = leg_terms(quotes, &args.virtual_quotes, &mint, supply, lp_fee_bps)?;
    let count = quotes.len();
    let terms = RuleTerms::of(&rules, clock.unix_timestamp, supply, config.curve_bps);
    // How holder rewards are paid (`Launch::reward_kind`, decided the same way here): SOL with the
    // switch, the leg's token on one leg, every leg's token in rounds on several.
    let rewards = holder_rewards(&rules, payouts, count);

    // The launch fee, as for any launch.
    if launch_fee > 0 {
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                system_program::Transfer {
                    from: ctx.accounts.creator.to_account_info(),
                    to: ctx.accounts.treasury.to_account_info(),
                },
            ),
            launch_fee,
        )?;
    }
    mint_coin(&ctx, &args, kit.as_ref(), custom.as_ref(), modules)?;
    if let Some(k) = &kit {
        install_kit(&ctx, k, &terms, legs[0].pool, rewards, &quotes[0])?;
    }
    let (custom_hook, custom_hook_flags) = custom
        .as_ref()
        .map_or((None, 0), |c| (Some(*c.program.key), c.flags));

    let a = &mut ctx.accounts;
    a.config.launches = a
        .config
        .launches
        .checked_add(1)
        .ok_or(LaunchError::MathOverflow)?;
    let decimals = a.config.decimals;
    let (launch_key, legs_key) = (a.launch.key(), a.legs.key());
    let creator = a.creator.key();
    let reserve_holding = a.launch_reserve.key();
    let launch = &mut a.launch;
    launch.version = VERSION;
    launch.bump = ctx.bumps.launch;
    launch.mint = mint;
    launch.creator = creator;
    launch.pool = legs[0].pool;
    launch.quote_mint = legs[0].quote_mint;
    launch.status = STATUS_CURVE;
    launch.creator_fee_bps = args.creator_fee_bps;
    launch.lp_fee_bps = lp_fee_bps;
    launch.sniper_window_secs = 0;
    launch.sniper_start_bps = 0;
    // Each leg's curve is in `Legs`; the launch never graduates.
    launch.virtual_quote = 0;
    launch.virtual_base = 0;
    launch.graduation_quote = u64::MAX;
    launch.curve_tokens = supply;
    launch.reserve_tokens = 0;
    launch.reserve_holding = reserve_holding;
    launch.quote_holding = legs[0].fee_account;
    launch.lp_holding = Pubkey::default();
    launch.created_at = clock.unix_timestamp;
    launch.graduated_at = 0;
    launch.creator_fees_accrued = 0;
    launch.creator_fees_claimed = 0;
    launch.graduation_topup = 0;
    launch.graduation_burned = 0;
    launch.rules = rules;
    launch.modules = modules;
    launch.kit_config = kit_config;
    launch.holder_vault = Pubkey::default();
    launch.kit_caller_bump = kit.as_ref().map_or(0, |k| k.caller_bump);
    launch.creator_unlock_at = terms.creator_unlock_at;
    launch.early_window_end = terms.early_window_end;
    launch.early_unlock_at = terms.early_unlock_at;
    launch.creator_bought = false;
    launch.holder_fees_accrued = 0;
    launch.burned_on_trades = 0;
    launch.config = launch_config.unwrap_or_default();
    launch.custom_hook = custom_hook;
    launch.custom_hook_flags = custom_hook_flags;
    // A config's author is paid nothing here: one that would be is refused
    // (`ConfigSharesOnTokenLaunch`).
    launch.author_share_bps = 0;
    launch.author_fees_paid = 0;
    launch.reserved = [0; 22];
    launch.set_token_quoted();
    launch.set_payouts(payouts.flags);
    let record = &mut a.legs;
    record.version = VERSION;
    record.bump = ctx.bumps.legs;
    record.mint = mint;
    record.launch = launch_key;
    record.count = count as u8;
    record.opened = 0;
    record.legs = legs;
    record.reserved = [0; 64];
    record.set_lift_at(terms.lift_at);

    let leg_list = legs[..count]
        .iter()
        .map(|l| LegTerms {
            quote_mint: l.quote_mint,
            quote_program: spl::program_id(l.quote_program),
            decimals: l.decimals,
            virtual_quote: l.virtual_quote,
            share: l.share,
            pool: l.pool,
            fee_account: l.fee_account,
        })
        .collect();
    emit_cpi!(TokenLaunchCreated {
        launch: launch_key,
        mint,
        creator,
        legs_account: legs_key,
        name: args.name,
        symbol: args.symbol,
        uri: args.uri,
        supply,
        decimals,
        creator_fee_bps: args.creator_fee_bps,
        lp_fee_bps,
        launch_fee_lamports: launch_fee,
        legs: leg_list,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    if let Some(config) = launch_config {
        emit_cpi!(TokenLaunchConfigured {
            launch: launch_key,
            mint,
            config,
            custom_hook,
            custom_hook_flags,
            ts: clock.unix_timestamp,
        });
    }
    if rules != LaunchRules::NONE {
        emit_cpi!(TokenLaunchRules {
            launch: launch_key,
            mint,
            rules,
            modules,
            kit_config: (modules != 0).then_some(kit_config),
            creator_unlock_at: terms.creator_unlock_at,
            early_window_end: terms.early_window_end,
            early_unlock_at: terms.early_unlock_at,
            lift_at: terms.lift_at,
            ts: clock.unix_timestamp,
        });
    }
    if payouts.flags != 0 || rewards.kind != 0 {
        emit_payouts(
            &ctx,
            launch_key,
            mint,
            payouts.flags,
            rewards,
            clock.unix_timestamp,
        )?;
    }
    Ok(())
}

/// `TokenLaunchPayouts`, out of `create_token_launch`'s frame (its stack is at the limit).
#[inline(never)]
fn emit_payouts<'info>(
    ctx: &Context<'info, CreateTokenLaunch<'info>>,
    launch: Pubkey,
    mint: Pubkey,
    flags: u8,
    rewards: bordrless_kit::LegsRewards,
    ts: i64,
) -> Result<()> {
    emit_cpi!(TokenLaunchPayouts {
        launch,
        mint,
        flags,
        reward_kind: rewards.kind,
        holder_round_secs: rewards.round_secs,
        ts,
    });
    Ok(())
}

/// The kit's holder rewards of a launch quoted in tokens (`docs/token-quotes.md` §7.5.1): none
/// without a holder fee; SOL (kind 3) with the switch; the leg's token (kind 1) on one leg; every
/// leg's token in rounds (kind 2, `holder_round_secs`, an hour when 0) on several.
#[inline(never)]
fn holder_rewards(
    rules: &LaunchRules,
    payouts: TokenPayouts,
    count: usize,
) -> bordrless_kit::LegsRewards {
    use bordrless_kit::constants::{REWARD_KIND_ROUNDS, REWARD_KIND_SOL, REWARD_KIND_TOKEN};
    let kind = if !rules.rewards_on() {
        0
    } else if payouts.sol() {
        REWARD_KIND_SOL
    } else if count == 1 {
        REWARD_KIND_TOKEN
    } else {
        REWARD_KIND_ROUNDS
    };
    let round_secs = if kind == REWARD_KIND_ROUNDS {
        match payouts.holder_round_secs {
            0 => DEFAULT_HOLDER_ROUND_SECS,
            secs => secs,
        }
    } else {
        0
    };
    bordrless_kit::LegsRewards { kind, round_secs }
}

/// The times and the max-wallet threshold a launch quoted in tokens fixes from its rules at `now`.
struct RuleTerms {
    rules: LaunchRules,
    creator_unlock_at: i64,
    early_window_end: i64,
    early_unlock_at: i64,
    /// With max wallet, the coins the legs together must sell for it to lift: `curve_bps` of the
    /// supply, where a SOL coin's curve sells out and graduates (at least 1); 0 without.
    lift_at: u64,
}

impl RuleTerms {
    fn of(rules: &LaunchRules, now: i64, supply: u64, curve_bps: u16) -> Self {
        let at = |secs: u32| if secs > 0 { now + i64::from(secs) } else { 0 };
        let (early_window_end, early_unlock_at) = if rules.early_window_secs > 0 {
            (at(rules.early_window_secs), at(rules.early_lock_secs))
        } else {
            (0, 0)
        };
        let lift_at = if rules.max_wallet_bps > 0 {
            let sold = u128::from(supply) * u128::from(curve_bps) / 10_000;
            u64::try_from(sold).unwrap_or(supply).max(1)
        } else {
            0
        };
        Self {
            rules: *rules,
            creator_unlock_at: at(rules.creator_lock_secs),
            early_window_end,
            early_unlock_at,
            lift_at,
        }
    }
}

/// The kit, installed through `init_legs` by the mint's kit-caller PDA (which signs nothing else),
/// the creator paying, after the supply is minted: as `create_launch` installs a SOL coin's, leg 0's
/// pool as its pool. With holder rewards (`docs/token-quotes.md` §7.5.1) their kind goes along
/// (`rewards`), and a one-leg coin paying in its token names that token (`quote`, leg 0's quote
/// mint) as the reward mint; without, no reward mint or vault, and the data is the call's of before.
#[inline(never)]
fn install_kit<'info>(
    ctx: &Context<'info, CreateTokenLaunch<'info>>,
    kit: &TokenKit<'info>,
    terms: &RuleTerms,
    pool: Pubkey,
    rewards: bordrless_kit::LegsRewards,
    quote: &AccountInfo<'info>,
) -> Result<()> {
    let a = &ctx.accounts;
    let mint = a.mint.key();
    let token = rewards.kind == bordrless_kit::constants::REWARD_KIND_TOKEN;
    let reward_mint = if token { *quote.key } else { Pubkey::default() };
    let ix = cpi::kit_init_legs_with(
        &cpi::KitInitKeys {
            kit_caller: kit.caller.key(),
            payer: a.creator.key(),
            mint,
            launch_reserve: a.launch_reserve.key(),
            kit_config: kit.config.key(),
            registry: kit.registry.key(),
            reward_mint,
            reward_vault: None,
        },
        KitInitArgs {
            launch: a.launch.key(),
            pool,
            creator: a.creator.key(),
            reward_mint,
            modules: terms.rules.modules(),
            max_wallet_bps: terms.rules.max_wallet_bps,
            creator_unlock_at: terms.creator_unlock_at,
            early_window_end: terms.early_window_end,
            early_unlock_at: terms.early_unlock_at,
            kit_caller_bump: kit.caller_bump,
        },
        rewards,
    );
    let caller_seeds = KitCallerSeeds::new(mint, kit.caller_bump);
    let mut infos = vec![
        kit.caller.clone(),
        a.creator.to_account_info(),
        a.mint.to_account_info(),
        a.launch_reserve.to_account_info(),
        kit.config.clone(),
        kit.registry.clone(),
        a.system_program.to_account_info(),
        kit.events.clone(),
        kit.program.clone(),
    ];
    if token {
        infos.push(quote.clone());
    }
    invoke_signed(&ix, &infos, &[&caller_seeds.seeds()])?;
    Ok(())
}

/// The extra accounts a leg pool's callbacks take, at indices 5 to 8 (`docs/token-quotes.md`
/// §2.4): the launch (w), the leg's fee account (w, `["leg-fees", base mint, quote mint]`), the
/// launch's `Legs` (w) and the kit config's fixed key (r, `launch.kit_config`, as a launch pool's).
pub fn leg_registry_list(kit_config: Pubkey) -> HookAccountList {
    HookAccountList::new(vec![
        ExtraAccount {
            writable: true,
            source: AccountSource::Pda {
                program: crate::ID,
                seeds: vec![Seed::Literal(LAUNCH_SEED.to_vec()), Seed::Account(2)],
            },
        },
        ExtraAccount {
            writable: true,
            source: AccountSource::Pda {
                program: crate::ID,
                seeds: vec![
                    Seed::Literal(LEG_FEES_SEED.to_vec()),
                    Seed::Account(2),
                    Seed::Account(3),
                ],
            },
        },
        ExtraAccount {
            writable: true,
            source: AccountSource::Pda {
                program: crate::ID,
                seeds: vec![Seed::Literal(LEGS_SEED.to_vec()), Seed::Account(2)],
            },
        },
        ExtraAccount {
            writable: false,
            source: AccountSource::Key(kit_config),
        },
    ])
}

/// Accounts of `open_leg(index)`: anyone opens leg `index` of a launch quoted in tokens, paying the
/// rent of its fee account, its pool, the pool's vaults and registry (and the DEX's pool creation
/// fee, if any). With kit rules the remaining accounts are the kit, the token program's signer of
/// its callbacks and the kit config (writable) ([`OPEN_LEG_KIT_ACCOUNTS`]); with a custom hook
/// (`docs/token-quotes.md` §7), the hook, the token program's signer for it and the hook's extras
/// for the deposit.
#[event_cpi]
#[derive(Accounts)]
pub struct OpenLeg<'info> {
    /// Pays the rent.
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(
        seeds = [LAUNCH_SEED, launch.mint.as_ref()],
        bump = launch.bump,
        constraint = launch.is_token_quoted() @ LaunchError::NotTokenLaunch
    )]
    pub launch: Box<Account<'info, Launch>>,
    #[account(mut, seeds = [LEGS_SEED, launch.mint.as_ref()], bump = legs.bump)]
    pub legs: Box<Account<'info, Legs>>,
    /// CHECK: address-checked: the coin.
    #[account(address = launch.mint @ LaunchError::WrongHolding)]
    pub mint: UncheckedAccount<'info>,
    /// CHECK: the leg's quote mint (checked in the handler).
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: the leg's fee account (checked in the handler), created here.
    #[account(mut)]
    pub leg_fees: UncheckedAccount<'info>,
    /// CHECK: address-checked: the launch's holding of the coin (the deposit).
    #[account(mut, address = launch.reserve_holding @ LaunchError::WrongHolding)]
    pub launch_reserve: UncheckedAccount<'info>,
    /// CHECK: this hook's registry for the pool (address checked when written).
    #[account(mut)]
    pub registry: UncheckedAccount<'info>,
    /// CHECK: address-checked: the DEX config.
    #[account(mut, address = DEX_CONFIG @ LaunchError::WrongProgram)]
    pub dex_config: UncheckedAccount<'info>,
    /// CHECK: the DEX config's treasury (the DEX checks it).
    #[account(mut)]
    pub dex_treasury: UncheckedAccount<'info>,
    /// CHECK: the leg's pool (checked in the handler), created by the DEX.
    #[account(mut)]
    pub pool: UncheckedAccount<'info>,
    /// CHECK: the pool's base vault, created by the DEX through the token program.
    #[account(mut)]
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: the pool's quote vault, created by the DEX through the ATA program.
    #[account(mut)]
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: this program's hook authority (address-checked).
    #[account(address = LAUNCH_HOOK_AUTHORITY @ LaunchError::WrongProgram)]
    pub hook_signer: UncheckedAccount<'info>,
    /// CHECK: the DEX's event authority (address-checked).
    #[account(address = DEX_EVENT_AUTHORITY @ LaunchError::WrongProgram)]
    pub dex_event_authority: UncheckedAccount<'info>,
    /// CHECK: the DEX.
    #[account(address = bordrless_swap::ID @ LaunchError::WrongProgram)]
    pub swap_program: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = bordrless_token::ID @ LaunchError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority (address-checked).
    #[account(address = TOKEN_EVENT_AUTHORITY @ LaunchError::WrongProgram)]
    pub token_event_authority: UncheckedAccount<'info>,
    /// CHECK: the leg's quote program (checked in the handler).
    pub quote_token_program: UncheckedAccount<'info>,
    /// CHECK: the associated token account program (address-checked).
    #[account(address = spl::ATA_PROGRAM_ID @ LaunchError::WrongProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// A program-owned account at a PDA of this program (`seeds`, which sign), `space` bytes owned by
/// `owner`, `payer` funding its rent. An address someone already sent lamports to is topped up,
/// allocated and assigned (a plain create would fail on it, and a leg that can't open would keep
/// the coin from ever trading).
pub(crate) fn create_pda_account<'info>(
    payer: &AccountInfo<'info>,
    account: &AccountInfo<'info>,
    system: &AccountInfo<'info>,
    space: usize,
    owner: &Pubkey,
    seeds: &[&[u8]],
) -> Result<()> {
    require_keys_eq!(
        *account.owner,
        system_program::ID,
        LaunchError::LegAlreadyOpen
    );
    let rent = Rent::get()?.minimum_balance(space);
    let current = account.lamports();
    if current == 0 {
        system_program::create_account(
            CpiContext::new_with_signer(
                system.key(),
                system_program::CreateAccount {
                    from: payer.clone(),
                    to: account.clone(),
                },
                &[seeds],
            ),
            rent,
            space as u64,
            owner,
        )?;
        return Ok(());
    }
    let top_up = rent.saturating_sub(current);
    if top_up > 0 {
        system_program::transfer(
            CpiContext::new(
                system.key(),
                system_program::Transfer {
                    from: payer.clone(),
                    to: account.clone(),
                },
            ),
            top_up,
        )?;
    }
    system_program::allocate(
        CpiContext::new_with_signer(
            system.key(),
            system_program::Allocate {
                account_to_allocate: account.clone(),
            },
            &[seeds],
        ),
        space as u64,
    )?;
    system_program::assign(
        CpiContext::new_with_signer(
            system.key(),
            system_program::Assign {
                account_to_assign: account.clone(),
            },
            &[seeds],
        ),
        owner,
    )?;
    Ok(())
}

/// 1. The leg's fee account: a token account of the quote at `["leg-fees", mint, quote_mint]`,
///    owned by the launch, sized for the mint's account extensions.
#[inline(never)]
fn create_leg_fees<'info>(
    ctx: &Context<'info, OpenLeg<'info>>,
    leg: &Leg,
    mint: &Pubkey,
) -> Result<()> {
    let a = &ctx.accounts;
    let program = a.quote_token_program.to_account_info();
    let quote_mint = a.quote_mint.to_account_info();
    let fees = a.leg_fees.to_account_info();
    let space = if leg.quote_program == spl::PROGRAM_TOKEN_2022 {
        usize::try_from(spl::account_data_size(&program, &quote_mint)?)
            .map_err(|_| LaunchError::MathOverflow)?
    } else {
        spl::ACCOUNT_LEN
    };
    let bump = [leg.fee_bump];
    let seeds: &[&[u8]] = &[LEG_FEES_SEED, mint.as_ref(), leg.quote_mint.as_ref(), &bump];
    create_pda_account(
        &a.payer.to_account_info(),
        &fees,
        &a.system_program.to_account_info(),
        space,
        program.key,
        seeds,
    )?;
    spl::initialize_account3(&program, &fees, &quote_mint, &a.launch.key())
}

/// The kit's accounts of `open_leg` for a launch with kit rules (its remaining accounts: the kit,
/// the token program's signer of its callbacks, the kit config), checked; none without a kit (any
/// remaining accounts are then ignored, as before rules).
fn open_leg_kit<'info>(
    launch: &Launch,
    remaining: &[AccountInfo<'info>],
) -> Result<Option<[AccountInfo<'info>; OPEN_LEG_KIT_ACCOUNTS]>> {
    if !launch.has_kit() {
        return Ok(None);
    }
    require!(
        remaining.len() >= OPEN_LEG_KIT_ACCOUNTS,
        LaunchError::KitAccountsMissing
    );
    require!(
        *remaining[0].key == KIT_ID
            && *remaining[1].key == TOKEN_HOOK_AUTHORITY
            && *remaining[2].key == launch.kit_config,
        LaunchError::WrongKitAccount
    );
    Ok(Some([
        remaining[0].clone(),
        remaining[1].clone(),
        remaining[2].clone(),
    ]))
}

/// The kit config's vault of a leg's token (its associated token account of it,
/// `docs/token-quotes.md` §7.5.1): where the leg's holder fees go, whatever the coin's reward kind.
pub fn kit_vault_address(kit_config: &Pubkey, leg: &Leg) -> Pubkey {
    spl::associated_token_address(
        kit_config,
        &leg.quote_mint,
        &spl::program_id(leg.quote_program),
    )
}

/// The kit's vault of the leg on a coin with holder rewards (`docs/token-quotes.md` §7.5.1): the
/// fourth remaining account of `open_leg` after the kit's three (`KitAccountsMissing` without it,
/// `WrongKitAccount` at another address), made here when missing (the opener paying), before the
/// deposit runs the kit (a one-leg coin paying in its token reads it on every transfer). None on a
/// coin without holder rewards.
#[inline(never)]
fn open_kit_vault<'info>(
    ctx: &Context<'info, OpenLeg<'info>>,
    leg: &Leg,
) -> Result<Option<AccountInfo<'info>>> {
    let a = &ctx.accounts;
    if !a.launch.rewards_on() {
        return Ok(None);
    }
    let remaining = ctx.remaining_accounts;
    require!(
        remaining.len() >= OPEN_LEG_KIT_REWARD_ACCOUNTS,
        LaunchError::KitAccountsMissing
    );
    let vault = &remaining[OPEN_LEG_KIT_ACCOUNTS];
    require_keys_eq!(
        *vault.key,
        kit_vault_address(&a.launch.kit_config, leg),
        LaunchError::WrongKitAccount
    );
    spl::create_ata_idempotent(
        &a.payer.to_account_info(),
        vault,
        &remaining[2],
        &a.quote_mint.to_account_info(),
        &a.system_program.to_account_info(),
        &a.quote_token_program.to_account_info(),
        &a.associated_token_program.to_account_info(),
    )?;
    Ok(Some(vault.clone()))
}

/// The custom hook's accounts of `open_leg` for a launch made from a config that names one
/// (`docs/token-quotes.md` §7): its remaining accounts are the hook (`InvalidCustomHook` for another
/// program), the token program's signer for it (`WrongHookSigner`) and the hook's extras for the
/// deposit (the launch's holding to the leg pool's, the launch signing), as a client resolves them
/// from the hook's registry for the mint; the hook checks its extras itself, as on every transfer.
/// None for a launch without a custom hook (its remaining accounts read as before).
fn open_leg_custom<'a, 'info>(
    launch: &Launch,
    remaining: &'a [AccountInfo<'info>],
) -> Result<Option<&'a [AccountInfo<'info>]>> {
    let Some(hook) = launch.custom_hook else {
        return Ok(None);
    };
    require!(remaining.len() >= 2, LaunchError::CustomHookAccountsMissing);
    require_keys_eq!(*remaining[0].key, hook, LaunchError::InvalidCustomHook);
    require_keys_eq!(
        *remaining[1].key,
        token_client::hook_signer(&hook),
        LaunchError::WrongHookSigner
    );
    Ok(Some(remaining))
}

/// 2. The leg's pool: DEX `create_spl_pool` as the pool's hook (this program's hook authority
///    signs), the launch depositing the leg's share (the launch signs). With kit rules the deposit
///    runs the kit (launch to the leg's pool: excluded to excluded), its slice the kit's with the
///    coin's `Legs`; with a custom hook, the hook's slice (`[hook, its signer, extras]`).
#[inline(never)]
fn create_leg_pool<'info>(
    ctx: &Context<'info, OpenLeg<'info>>,
    leg: &Leg,
    kit: Option<&[AccountInfo<'info>; OPEN_LEG_KIT_ACCOUNTS]>,
    custom: Option<&[AccountInfo<'info>]>,
    reward_vault: Option<&AccountInfo<'info>>,
) -> Result<()> {
    let a = &ctx.accounts;
    let launch = &a.launch;
    let launch_seeds = LaunchSeeds::new(launch.mint, launch.bump);
    let hook_seeds: &[&[u8]] = &[HOOK_AUTHORITY_SEED, &[HOOK_AUTHORITY_BUMP]];
    let slice = match (kit, custom) {
        // A one-leg coin paying holder rewards in its token: the kit reads its vault on the deposit.
        (Some(_), _) if reward_vault.is_some() => cpi::kit_legs_slice_with_vault(
            launch.kit_config,
            a.legs.key(),
            reward_vault.map(|v| *v.key).unwrap_or_default(),
        ),
        (Some(_), _) => cpi::kit_legs_slice(launch.kit_config, a.legs.key()),
        (None, Some(c)) => {
            let extras: Vec<AccountMeta> = c[2..]
                .iter()
                .map(|e| AccountMeta {
                    pubkey: *e.key,
                    is_signer: false,
                    is_writable: e.is_writable,
                })
                .collect();
            cpi::custom_slice((*c[0].key, *c[1].key), &extras)
        }
        (None, None) => vec![],
    };
    let base_hook_accounts = slice.len() as u8;
    let ix = cpi::dex_create_spl_pool(
        &cpi::CreateSplPoolKeys {
            payer: a.payer.key(),
            authority: launch.key(),
            treasury: a.dex_treasury.key(),
            base_mint: launch.mint,
            quote_mint: leg.quote_mint,
            pool: leg.pool,
            base_vault: a.base_vault.key(),
            quote_vault: a.quote_vault.key(),
            authority_base: a.launch_reserve.key(),
            quote_program: a.quote_token_program.key(),
        },
        CreatePoolArgs {
            lp_fee_bps: launch.lp_fee_bps,
            hook_program: crate::ID,
            hook_flags: LEG_HOOK_FLAGS,
            virtual_base: 0,
            virtual_quote: leg.virtual_quote,
            base_amount: leg.share,
            quote_amount: 0,
            base_hook_accounts,
            quote_hook_accounts: 0,
            hook_data: vec![],
        },
        slice,
    );
    let mut infos = vec![
        a.payer.to_account_info(),
        launch.to_account_info(),
        a.dex_config.to_account_info(),
        a.dex_treasury.to_account_info(),
        a.mint.to_account_info(),
        a.quote_mint.to_account_info(),
        a.pool.to_account_info(),
        a.base_vault.to_account_info(),
        a.quote_vault.to_account_info(),
        a.launch_reserve.to_account_info(),
        a.hook_signer.to_account_info(),
        a.token_program.to_account_info(),
        a.token_event_authority.to_account_info(),
        a.quote_token_program.to_account_info(),
        a.associated_token_program.to_account_info(),
        a.system_program.to_account_info(),
        a.dex_event_authority.to_account_info(),
        a.swap_program.to_account_info(),
    ];
    if let Some([program, signer, config]) = kit {
        infos.extend([
            program.clone(),
            signer.clone(),
            config.clone(),
            a.legs.to_account_info(),
        ]);
        if let Some(vault) = reward_vault {
            infos.push(vault.clone());
        }
    }
    if let Some(c) = custom {
        infos.extend(c.iter().cloned());
    }
    invoke_signed(&ix, &infos, &[&launch_seeds.seeds(), hook_seeds])?;
    Ok(())
}

/// `open_leg(index)`.
pub fn process_open_leg<'info>(ctx: Context<'info, OpenLeg<'info>>, index: u8) -> Result<()> {
    let clock = Clock::get()?;
    let i = usize::from(index);
    let leg = {
        let legs = &ctx.accounts.legs;
        require!(i < usize::from(legs.count), LaunchError::NoSuchLeg);
        legs.legs[i]
    };
    require!(leg.opened_at == 0, LaunchError::LegAlreadyOpen);
    let a = &ctx.accounts;
    require!(
        a.quote_mint.key() == leg.quote_mint
            && a.pool.key() == leg.pool
            && a.leg_fees.key() == leg.fee_account
            && a.quote_token_program.key() == spl::program_id(leg.quote_program),
        LaunchError::WrongLegAccount
    );
    let mint = a.launch.mint;
    let launch_key = a.launch.key();
    let kit = open_leg_kit(&a.launch, ctx.remaining_accounts)?;
    let custom = open_leg_custom(&a.launch, ctx.remaining_accounts)?;
    create_leg_fees(&ctx, &leg, &mint)?;
    // The kit's vault of the leg (holder rewards, §7.5.1), before the deposit runs the kit.
    let vault = open_kit_vault(&ctx, &leg)?;
    let count = ctx.accounts.legs.count;
    let reward_vault = vault.as_ref().filter(|_| {
        ctx.accounts.launch.reward_kind(count) == bordrless_kit::constants::REWARD_KIND_TOKEN
    });
    create_leg_pool(&ctx, &leg, kit.as_ref(), custom, reward_vault)?;
    // 3. The pool's registry: the launch, the leg's fee account, the `Legs`, the kit config; with
    //    holder rewards the kit's vault of the leg (index 9).
    let (registry, registry_bump) = hook_accounts_address(&crate::ID, &leg.pool);
    require_keys_eq!(
        ctx.accounts.registry.key(),
        registry,
        LaunchError::WrongHolding
    );
    let mut list = leg_registry_list(ctx.accounts.launch.kit_config);
    if let Some(v) = &vault {
        list.accounts.push(ExtraAccount {
            writable: true,
            source: AccountSource::Key(*v.key),
        });
    }
    write_registry(
        &ctx.accounts.payer.to_account_info(),
        &ctx.accounts.registry.to_account_info(),
        &ctx.accounts.system_program.to_account_info(),
        &crate::ID,
        &leg.pool,
        registry_bump,
        &list,
    )?;
    // 4. Opened.
    let payer = ctx.accounts.payer.key();
    let legs = &mut ctx.accounts.legs;
    legs.legs[i].opened_at = clock.unix_timestamp;
    legs.opened = legs
        .opened
        .checked_add(1)
        .ok_or(LaunchError::MathOverflow)?;
    let (opened, count) = (legs.opened, legs.count);
    emit_cpi!(LegOpened {
        launch: launch_key,
        mint,
        index,
        quote_mint: leg.quote_mint,
        pool: leg.pool,
        fee_account: leg.fee_account,
        virtual_quote: leg.virtual_quote,
        share: leg.share,
        opened,
        count,
        payer,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    // 5. The last leg: trading opens now, and the locks run from now (the kit moves their times by
    //    how long after the launch this is, from the `Legs`); said with the times in force.
    let l = &ctx.accounts.launch;
    let locks =
        bordrless_kit::modules::CREATOR_WALLET_LOCK | bordrless_kit::modules::EARLY_BUYER_LOCK;
    if opened == count && l.modules & locks != 0 {
        let now = clock.unix_timestamp;
        let delay = now.saturating_sub(l.created_at).max(0);
        let at = |t: i64| if t == 0 { 0 } else { t.saturating_add(delay) };
        emit_cpi!(TokenLaunchLocks {
            launch: launch_key,
            mint,
            trading_opened_at: now,
            delay,
            creator_unlock_at: at(l.creator_unlock_at),
            early_window_end: at(l.early_window_end),
            early_unlock_at: at(l.early_unlock_at),
            ts: now,
        });
    }
    Ok(())
}

/// Accounts of `claim_creator_fees_token(index)`: anyone pays leg `index`'s creator fees to the
/// creator's associated token account of the leg's quote (created if needed, `payer` paying). A
/// companion coin's creator is the companion's creator address, a PDA (`docs/token-quotes.md` §7):
/// its associated token account is derived and created as a wallet's (the ATA program takes any
/// owner key), and the companion's script spends it.
#[event_cpi]
#[derive(Accounts)]
pub struct ClaimCreatorFeesToken<'info> {
    /// Whoever sends it; pays the creator's token account's rent when it is created.
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(
        seeds = [LAUNCH_SEED, launch.mint.as_ref()],
        bump = launch.bump,
        constraint = launch.is_token_quoted() @ LaunchError::NotTokenLaunch
    )]
    pub launch: Box<Account<'info, Launch>>,
    #[account(mut, seeds = [LEGS_SEED, launch.mint.as_ref()], bump = legs.bump)]
    pub legs: Box<Account<'info, Legs>>,
    /// CHECK: the leg's quote mint (checked in the handler).
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: the leg's fee account (checked in the handler).
    #[account(mut)]
    pub leg_fees: UncheckedAccount<'info>,
    /// CHECK: address-checked: the launch's creator.
    #[account(address = launch.creator @ LaunchError::NotCreator)]
    pub creator: UncheckedAccount<'info>,
    /// CHECK: the creator's associated token account of the quote (the ATA program checks its
    /// address; created if needed).
    #[account(mut)]
    pub creator_quote: UncheckedAccount<'info>,
    /// CHECK: the leg's quote program (checked in the handler).
    pub quote_token_program: UncheckedAccount<'info>,
    /// CHECK: the associated token account program (address-checked).
    #[account(address = spl::ATA_PROGRAM_ID @ LaunchError::WrongProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// `claim_creator_fees_token(index)`: the whole balance of the leg's fee account (the launch
/// signs) to the creator's associated token account of the quote.
pub fn process_claim_creator_fees_token<'info>(
    ctx: Context<'info, ClaimCreatorFeesToken<'info>>,
    index: u8,
) -> Result<()> {
    let clock = Clock::get()?;
    let i = usize::from(index);
    let leg = {
        let legs = &ctx.accounts.legs;
        require!(i < usize::from(legs.count), LaunchError::NoSuchLeg);
        legs.legs[i]
    };
    let a = &ctx.accounts;
    require!(
        a.quote_mint.key() == leg.quote_mint
            && a.leg_fees.key() == leg.fee_account
            && a.quote_token_program.key() == spl::program_id(leg.quote_program),
        LaunchError::WrongLegAccount
    );
    // A leg not yet open has no fee account.
    require!(leg.opened_at != 0, LaunchError::NothingToClaim);
    // With the SOL switch (`docs/token-quotes.md` §7.5.2) nobody pushes tokens to a wallet creator
    // who chose SOL: only the creator takes them, signing. A companion's creator address is claimed
    // into by its own leg claim, which splits it (its parts' currencies are the companion's).
    require!(
        !a.launch.sol_switch()
            || a.creator.is_signer
            || is_companion_creator(&a.launch.creator, &a.launch.mint),
        LaunchError::CreatorSignsTokenClaim
    );
    let program = a.quote_token_program.to_account_info();
    let quote_mint = a.quote_mint.to_account_info();
    let creator_quote = a.creator_quote.to_account_info();
    spl::create_ata_idempotent(
        &a.payer.to_account_info(),
        &creator_quote,
        &a.creator.to_account_info(),
        &quote_mint,
        &a.system_program.to_account_info(),
        &program,
        &a.associated_token_program.to_account_info(),
    )?;
    let to =
        spl::read_token_account(&creator_quote, program.key).ok_or(LaunchError::WrongHolding)?;
    require!(
        to.owner == a.launch.creator && to.mint == leg.quote_mint,
        LaunchError::WrongHolding
    );
    let fees = a.leg_fees.to_account_info();
    let amount = spl::read_token_account(&fees, program.key)
        .ok_or(LaunchError::WrongLegAccount)?
        .amount;
    require!(amount > 0, LaunchError::NothingToClaim);
    let launch_seeds = LaunchSeeds::new(a.launch.mint, a.launch.bump);
    spl::transfer_checked(
        &program,
        &fees,
        &quote_mint,
        &creator_quote,
        &a.launch.to_account_info(),
        amount,
        leg.decimals,
        &[&launch_seeds.seeds()],
    )?;
    let (launch_key, mint, creator) = (a.launch.key(), a.launch.mint, a.launch.creator);
    // A running total for display (the claim pays the account's balance): saturating, so it never
    // blocks a claim.
    let legs = &mut ctx.accounts.legs;
    let claimed = legs.legs[i].creator_fees_claimed.saturating_add(amount);
    legs.legs[i].creator_fees_claimed = claimed;
    emit_cpi!(LegCreatorFeesClaimed {
        launch: launch_key,
        mint,
        index,
        quote_mint: leg.quote_mint,
        creator,
        destination: creator_quote.key(),
        amount,
        claimed_total: claimed,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    Ok(())
}
