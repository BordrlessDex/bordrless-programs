//! `half_life`: a Bordrless token hook whose exit fee has a half-life.
//!
//! Every holding remembers how old its tokens are: the average time they arrived, kept in the 64
//! bytes of hook data the token program stores inside each holding. Tokens bought from the pool
//! arrive "now"; tokens sent wallet to wallet arrive with the sender's age, so age travels with the
//! tokens and cannot be reset or laundered by moving them. Mixing new tokens into a wallet averages
//! its age down, by weight.
//!
//! Moving tokens out of a wallet (a sell into the pool, or a transfer to anyone) pays an exit fee
//! taken from the amount itself: 20% for tokens that arrived this instant, halving every six hours
//! they are held (10% at 6 h, 5% at 12 h, 2.5% at 18 h, ...), linear within each half-life, and
//! nothing after 48 hours. Buys never pay it. The fee goes to the mint's furnace, a holding this
//! program owns, which anyone can `stoke` to burn: early sellers shrink the supply for everyone
//! who stays.
//!
//! None of this is possible with a Token-2022 transfer hook: it cannot take part of the amount,
//! it has no state inside the holding (it would need an account per holder, created and funded
//! before use and carried by every transfer), and Token-2022's own transfer fee is one rate for
//! every holder alike.
//!
//! For a launch (`docs/hooks-v2.md` §5.8): `prepare(mint)` before the launch (the mint need not
//! exist), a `LaunchConfig` naming this program with [`FLAGS`], then `light(mint)` once the mint
//! exists. Until the furnace is lit, a transfer that owes a fee fails, so nobody exits free in the
//! first blocks; buys work from the start. Every instruction but the callback is permissionless.
//!
//! The launch's own accounts are exempt: the launch PDA (the supply and the graduation reserve)
//! and the launch pool (read from the `Launch` account, so buys come out of it free and sells into
//! it are charged on the seller's side only), and the furnace.

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_lang::AccountDeserialize;
use bordrless_hook::{
    hook_accounts_address, token_flags, write_registry, AccountSource, Delta, ExtraAccount,
    HookAccountList, HookReturn, Seed, TokenHookArgs, TokenOp, HOOK_DATA_LEN,
    TOKEN_PREFIX_ACCOUNTS,
};
use bordrless_launch::state::Launch;
use bordrless_token::client as token_client;

declare_id!("53SpmtkdPWQ63mWoDeXk8P9tuwiT4ed2Wx4fwfy5NSF8");

#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Bordrless Half-Life hook",
    project_url: "https://github.com/BordrlessDex/bordrless-programs",
    contacts: "link:https://github.com/BordrlessDex/bordrless-programs/security/advisories/new",
    policy: "https://github.com/BordrlessDex/bordrless-programs/blob/main/SECURITY.md",
    source_code: "https://github.com/BordrlessDex/bordrless-programs"
}

/// `["half-life", mint]`: the hook's state for a mint.
pub const STATE_SEED: &[u8] = b"half-life";
/// `["furnace", mint]`: the owner of the mint's furnace holding.
pub const FURNACE_SEED: &[u8] = b"furnace";
/// The launch program's `["launch", mint]` seed.
pub const LAUNCH_SEED: &[u8] = b"launch";
/// The launch program.
pub const LAUNCH_ID: Pubkey = bordrless_launch::ID;
/// The token program's signer of every callback to this hook: `["hook-authority", half_life]`
/// under the token program (checked against its derivation in the unit tests).
pub const TOKEN_HOOK_SIGNER: Pubkey =
    Pubkey::from_str_const("FBZPj9PmV9dXL8U4qmRffXdXm11e23KEBnNgEVXxhfhF");
/// The flags a `LaunchConfig` names for this hook: `before_transfer`, which may take a delta and
/// write hook data. No mint or burn callbacks.
pub const FLAGS: u16 = token_flags::BEFORE_TRANSFER
    | token_flags::TRANSFER_RETURNS_DELTA
    | token_flags::WRITES_HOOK_DATA;
/// Index of the furnace holding in a callback's account list (prefix of 5, the state, the furnace).
pub const FURNACE_INDEX: u8 = TOKEN_PREFIX_ACCOUNTS as u8 + 1;

/// The exit fee for tokens that arrived this instant, in parts per million (20%).
pub const MAX_FEE_PPM: u32 = 200_000;
/// The fee halves every this many seconds a token is held (six hours).
pub const HALF_LIFE_SECS: i64 = 21_600;
/// After this many half-lives (48 hours) the fee is zero.
pub const ZERO_AFTER_HALVINGS: i64 = 8;
/// Parts per million.
pub const PPM: u64 = 1_000_000;
/// The first bytes of a holding's hook data once this hook has stamped it: "HL", layout 1.
pub const MAGIC: [u8; 3] = *b"HL\x01";

/// The exit fee, in parts per million, for tokens `age` seconds old: [`MAX_FEE_PPM`] halved for
/// every [`HALF_LIFE_SECS`], linear within each half-life, zero from [`ZERO_AFTER_HALVINGS`] on.
pub fn fee_ppm(age: i64) -> u32 {
    if age <= 0 {
        return MAX_FEE_PPM;
    }
    let halvings = age / HALF_LIFE_SECS;
    if halvings >= ZERO_AFTER_HALVINGS {
        return 0;
    }
    let into = (age % HALF_LIFE_SECS) as u64;
    let hi = u64::from(MAX_FEE_PPM >> halvings);
    let lo = u64::from(MAX_FEE_PPM >> (halvings + 1));
    (hi - (hi - lo) * into / HALF_LIFE_SECS as u64) as u32
}

/// The fee on `amount` at `ppm`, rounded down.
pub fn fee_on(amount: u64, ppm: u32) -> u64 {
    (u128::from(amount) * u128::from(ppm) / u128::from(PPM)) as u64
}

/// When the tokens in a holding arrived, if this hook has stamped it.
pub fn read_since(data: &[u8; HOOK_DATA_LEN]) -> Option<i64> {
    if data[..3] != MAGIC {
        return None;
    }
    let mut b = [0u8; 8];
    b.copy_from_slice(&data[3..11]);
    Some(i64::from_le_bytes(b))
}

/// Hook data recording that a holding's tokens arrived at `since`.
pub fn stamp(since: i64) -> [u8; HOOK_DATA_LEN] {
    let mut d = [0u8; HOOK_DATA_LEN];
    d[..3].copy_from_slice(&MAGIC);
    d[3..11].copy_from_slice(&since.to_le_bytes());
    d
}

/// The arrival time of a holding of `balance` tokens that arrived at `since` once `added` tokens
/// that arrived at `added_since` join it: the average by weight (an empty holding takes the new
/// tokens' time).
pub fn blend(balance: u64, since: i64, added: u64, added_since: i64) -> i64 {
    if balance == 0 {
        return added_since;
    }
    let total = i128::from(balance) + i128::from(added);
    ((i128::from(balance) * i128::from(since) + i128::from(added) * i128::from(added_since))
        / total) as i64
}

/// The furnace owner of `mint` and its bump.
pub fn furnace_owner(mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[FURNACE_SEED, mint.as_ref()], &crate::ID)
}

/// The hook's state of `mint`.
pub fn state_address(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[STATE_SEED, mint.as_ref()], &crate::ID).0
}

/// Instructions of the Half-Life hook.
#[program]
pub mod half_life {
    use super::*;

    /// Prepares the hook for `mint`, which need not exist yet (a launch's mint: the launchpad
    /// creates it naming this program as its hook): the state and the extra-accounts registry.
    /// Anyone may call it, once per mint; it has no parameters, so what it prepares is the same
    /// for everyone.
    pub fn prepare(ctx: Context<Prepare>) -> Result<()> {
        let mint = ctx.accounts.mint.key();
        let (owner, furnace_bump) = furnace_owner(&mint);
        let furnace_holding = token_client::holding_address(&mint, &owner);
        let launch = Pubkey::find_program_address(&[LAUNCH_SEED, mint.as_ref()], &LAUNCH_ID).0;
        let state = &mut ctx.accounts.state;
        state.version = 1;
        state.bump = ctx.bumps.state;
        state.furnace_bump = furnace_bump;
        state.mint = mint;
        state.launch = launch;
        state.pool = Pubkey::default();
        state.furnace_owner = owner;
        state.furnace_holding = furnace_holding;
        state.fed = 0;
        state.burned = 0;
        state.prepared_by = ctx.accounts.payer.key();
        state.reserved = [0; 32];
        let (expected, bump) = hook_accounts_address(&crate::ID, &mint);
        require_keys_eq!(
            ctx.accounts.registry.key(),
            expected,
            HalfLifeError::WrongAccount
        );
        // The callback's extras: the state, the furnace holding, the launch account (read for its
        // pool until the state remembers it).
        let list = HookAccountList::new(vec![
            ExtraAccount {
                writable: true,
                source: AccountSource::Pda {
                    program: crate::ID,
                    seeds: vec![Seed::Literal(STATE_SEED.to_vec()), Seed::Account(1)],
                },
            },
            ExtraAccount {
                writable: true,
                source: AccountSource::Key(furnace_holding),
            },
            ExtraAccount {
                writable: false,
                source: AccountSource::Pda {
                    program: LAUNCH_ID,
                    seeds: vec![Seed::Literal(LAUNCH_SEED.to_vec()), Seed::Account(1)],
                },
            },
        ]);
        write_registry(
            &ctx.accounts.payer.to_account_info(),
            &ctx.accounts.registry.to_account_info(),
            &ctx.accounts.system_program.to_account_info(),
            &crate::ID,
            &mint,
            bump,
            &list,
        )
    }

    /// Lights the furnace of `mint`: creates the furnace's holding of the mint, which needs the
    /// mint to exist. Anyone may call it, and the payer pays the holding's rent. Until it is lit,
    /// transfers that owe a fee fail.
    pub fn light(ctx: Context<Light>) -> Result<()> {
        let ix = token_client::create_holding(
            ctx.accounts.payer.key(),
            ctx.accounts.mint.key(),
            ctx.accounts.furnace_owner.key(),
        );
        invoke_signed(
            &ix,
            &[
                ctx.accounts.payer.to_account_info(),
                ctx.accounts.mint.to_account_info(),
                ctx.accounts.furnace_owner.to_account_info(),
                ctx.accounts.furnace_holding.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
                ctx.accounts.token_event_authority.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
            ],
            &[],
        )?;
        Ok(())
    }

    /// Burns everything in the furnace. Anyone may call it.
    pub fn stoke(ctx: Context<Stoke>) -> Result<()> {
        let amount = token_client::read_holding(&ctx.accounts.furnace_holding)?.amount;
        require!(amount > 0, HalfLifeError::FurnaceEmpty);
        let mint = ctx.accounts.mint.key();
        let ix = token_client::burn(
            ctx.accounts.furnace_owner.key(),
            ctx.accounts.furnace_holding.key(),
            mint,
            Some(crate::ID),
            vec![],
            amount,
        );
        let bump = [ctx.accounts.state.furnace_bump];
        let seeds: &[&[u8]] = &[FURNACE_SEED, mint.as_ref(), &bump];
        invoke_signed(
            &ix,
            &[
                ctx.accounts.furnace_owner.to_account_info(),
                ctx.accounts.furnace_holding.to_account_info(),
                ctx.accounts.mint.to_account_info(),
                ctx.accounts.this_program.to_account_info(),
                ctx.accounts.hook_signer.to_account_info(),
                ctx.accounts.token_event_authority.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
            ],
            &[seeds],
        )?;
        let state = &mut ctx.accounts.state;
        state.burned = state.burned.saturating_add(amount);
        emit!(Stoked {
            mint,
            amount,
            burned_total: state.burned,
        });
        Ok(())
    }

    /// `before_transfer`: the exit fee to the furnace, and the two holdings' arrival times.
    pub fn before_transfer(
        ctx: Context<BeforeTransfer>,
        args: TokenHookArgs,
    ) -> Result<HookReturn> {
        require!(args.op == TokenOp::Transfer, HalfLifeError::NotATransfer);
        let state = &mut ctx.accounts.state;
        require_keys_eq!(args.mint, state.mint, HalfLifeError::WrongAccount);
        let now = Clock::get()?.unix_timestamp;

        // The launch pool, read once from the launch account and remembered.
        if state.pool == Pubkey::default() {
            if let Some(pool) = launch_pool(&ctx.accounts.launch, &state.mint) {
                state.pool = pool;
            }
        }
        let (launch, furnace, pool) = (state.launch, state.furnace_owner, state.pool);
        let exempt = |owner: &Pubkey| {
            *owner == launch || *owner == furnace || (pool != Pubkey::default() && *owner == pool)
        };
        if args.source == args.destination {
            return Ok(HookReturn::default());
        }
        let source_exempt = exempt(&args.source_owner);
        // Tokens out of the launch go only to its pool (the deposit, the graduation top-up).
        let destination_exempt = exempt(&args.destination_owner) || args.source_owner == launch;

        // The fee: the source's tokens' age, unless they leave the launch, its pool or the furnace.
        let source_since = read_since(&args.source_hook_data);
        let fee = if source_exempt {
            0
        } else {
            // A holding this hook never stamped holds tokens of unknown age: the full fee.
            let age = now.saturating_sub(source_since.unwrap_or(now));
            fee_on(args.amount, fee_ppm(age)).min(args.amount)
        };
        let lit = *ctx.accounts.furnace_holding.owner == bordrless_token::ID;
        require!(fee == 0 || lit, HalfLifeError::FurnaceNotLit);
        state.fed = state.fed.saturating_add(fee);

        // The destination's arrival time: the new tokens arrive with the source's age (now, out
        // of the launch or the pool), blended with what it already holds.
        let received = args.amount - fee;
        let destination_hook_data = if destination_exempt || received == 0 {
            None
        } else {
            let incoming = if source_exempt {
                now
            } else {
                source_since.unwrap_or(now)
            };
            let held_since = read_since(&args.destination_hook_data).unwrap_or(now);
            Some(stamp(blend(
                args.destination_balance,
                held_since,
                received,
                incoming,
            )))
        };
        // An emptied holding is cleared, so it can be closed (`HookDataNotEmpty` otherwise).
        let source_hook_data = if !source_exempt && args.source_balance == args.amount {
            Some([0u8; HOOK_DATA_LEN])
        } else {
            None
        };
        let deltas = if fee > 0 {
            vec![Delta {
                amount: fee,
                account: FURNACE_INDEX,
            }]
        } else {
            vec![]
        };
        Ok(HookReturn {
            deltas,
            source_hook_data,
            destination_hook_data,
            ..HookReturn::default()
        })
    }
}

/// The pool of the launch of `mint`, when `info` is that launch's account and is initialized.
fn launch_pool(info: &AccountInfo, mint: &Pubkey) -> Option<Pubkey> {
    if *info.owner != LAUNCH_ID {
        return None;
    }
    let data = info.try_borrow_data().ok()?;
    let launch = Launch::try_deserialize(&mut &data[..]).ok()?;
    (launch.mint == *mint && launch.pool != Pubkey::default()).then_some(launch.pool)
}

/// The hook's state for one mint, at `["half-life", mint]`.
#[account]
#[derive(InitSpace)]
pub struct HalfLifeState {
    /// Layout version.
    pub version: u8,
    /// Bump.
    pub bump: u8,
    /// Bump of the furnace owner.
    pub furnace_bump: u8,
    /// The mint.
    pub mint: Pubkey,
    /// The launch program's `["launch", mint]`: exempt.
    pub launch: Pubkey,
    /// The launch pool, once read from the launch account (default until then): exempt.
    pub pool: Pubkey,
    /// The furnace's owner, `["furnace", mint]` under this program: exempt.
    pub furnace_owner: Pubkey,
    /// The furnace's holding of the mint.
    pub furnace_holding: Pubkey,
    /// Exit fees sent to the furnace, all time.
    pub fed: u64,
    /// Tokens the furnace has burned, all time.
    pub burned: u64,
    /// Who prepared the hook for the mint.
    pub prepared_by: Pubkey,
    /// Reserved.
    pub reserved: [u8; 32],
}

/// Accounts of `prepare`.
#[derive(Accounts)]
pub struct Prepare<'info> {
    /// Pays the rent.
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: the mint the hook is prepared for; need not exist yet.
    pub mint: UncheckedAccount<'info>,
    #[account(init, payer = payer, space = 8 + HalfLifeState::INIT_SPACE, seeds = [STATE_SEED, mint.key().as_ref()], bump)]
    pub state: Account<'info, HalfLifeState>,
    /// CHECK: the registry PDA, created here (address-checked in the handler).
    #[account(mut)]
    pub registry: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Accounts of `light`.
#[derive(Accounts)]
pub struct Light<'info> {
    /// Pays the holding's rent.
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(seeds = [STATE_SEED, mint.key().as_ref()], bump = state.bump)]
    pub state: Account<'info, HalfLifeState>,
    /// CHECK: the mint (the token program checks it).
    pub mint: UncheckedAccount<'info>,
    /// CHECK: the furnace's owner (address-checked).
    #[account(address = state.furnace_owner @ HalfLifeError::WrongAccount)]
    pub furnace_owner: UncheckedAccount<'info>,
    /// CHECK: the furnace's holding, created here (address-checked).
    #[account(mut, address = state.furnace_holding @ HalfLifeError::WrongAccount)]
    pub furnace_holding: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = bordrless_token::ID @ HalfLifeError::WrongAccount)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: its event authority (the token program checks it).
    pub token_event_authority: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Accounts of `stoke`.
#[derive(Accounts)]
pub struct Stoke<'info> {
    #[account(mut, seeds = [STATE_SEED, mint.key().as_ref()], bump = state.bump)]
    pub state: Account<'info, HalfLifeState>,
    /// CHECK: the mint (the token program checks it).
    #[account(mut)]
    pub mint: UncheckedAccount<'info>,
    /// CHECK: the furnace's owner, signing by seeds (address-checked).
    #[account(address = state.furnace_owner @ HalfLifeError::WrongAccount)]
    pub furnace_owner: UncheckedAccount<'info>,
    /// CHECK: the furnace's holding (address-checked).
    #[account(mut, address = state.furnace_holding @ HalfLifeError::WrongAccount)]
    pub furnace_holding: UncheckedAccount<'info>,
    /// CHECK: this program, the mint's hook (the token program checks it).
    #[account(address = crate::ID @ HalfLifeError::WrongAccount)]
    pub this_program: UncheckedAccount<'info>,
    /// CHECK: the token program's signer for this hook (address-checked).
    #[account(address = TOKEN_HOOK_SIGNER @ HalfLifeError::WrongAccount)]
    pub hook_signer: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = bordrless_token::ID @ HalfLifeError::WrongAccount)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: its event authority (the token program checks it).
    pub token_event_authority: UncheckedAccount<'info>,
}

/// Accounts of `before_transfer`: the prefix, then the registry's extras.
#[derive(Accounts)]
pub struct BeforeTransfer<'info> {
    /// CHECK: the token program's signer for this hook (address- and signer-checked).
    #[account(signer, address = TOKEN_HOOK_SIGNER @ HalfLifeError::BadHookSigner)]
    pub hook_signer: UncheckedAccount<'info>,
    /// CHECK: the mint.
    pub mint: UncheckedAccount<'info>,
    /// CHECK: the source holding.
    pub source: UncheckedAccount<'info>,
    /// CHECK: the destination holding.
    pub destination: UncheckedAccount<'info>,
    /// CHECK: the authority.
    pub authority: UncheckedAccount<'info>,
    #[account(mut, seeds = [STATE_SEED, mint.key().as_ref()], bump = state.bump)]
    pub state: Account<'info, HalfLifeState>,
    /// CHECK: the furnace's holding (address-checked; not created until `light`).
    #[account(mut, address = state.furnace_holding @ HalfLifeError::WrongAccount)]
    pub furnace_holding: UncheckedAccount<'info>,
    /// CHECK: the launch account of the mint (address-checked; may not be initialized yet).
    #[account(address = state.launch @ HalfLifeError::WrongAccount)]
    pub launch: UncheckedAccount<'info>,
}

/// The furnace burned.
#[event]
pub struct Stoked {
    pub mint: Pubkey,
    pub amount: u64,
    pub burned_total: u64,
}

/// Errors.
#[error_code]
pub enum HalfLifeError {
    #[msg("an account is not the one this mint's hook expects")]
    WrongAccount,
    #[msg("the hook signer is not the token program's signer for this hook")]
    BadHookSigner,
    #[msg("only transfers call this hook")]
    NotATransfer,
    #[msg("this transfer owes an exit fee and the furnace is not lit yet: call light first")]
    FurnaceNotLit,
    #[msg("the furnace is empty")]
    FurnaceEmpty,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_programs_signer_for_this_hook() {
        assert_eq!(TOKEN_HOOK_SIGNER, token_client::hook_signer(&crate::ID));
    }

    #[test]
    fn the_launch_seed_is_the_launch_programs() {
        assert_eq!(LAUNCH_SEED, bordrless_launch::constants::LAUNCH_SEED);
    }

    #[test]
    fn the_fee_halves_every_six_hours_and_ends_at_48() {
        let h = HALF_LIFE_SECS;
        assert_eq!(fee_ppm(-5), 200_000);
        assert_eq!(fee_ppm(0), 200_000);
        assert_eq!(fee_ppm(h / 2), 150_000);
        assert_eq!(fee_ppm(h), 100_000);
        assert_eq!(fee_ppm(2 * h), 50_000);
        assert_eq!(fee_ppm(3 * h), 25_000);
        assert_eq!(fee_ppm(4 * h), 12_500);
        assert_eq!(fee_ppm(7 * h), 1_562);
        assert_eq!(fee_ppm(8 * h - 1), 782);
        assert_eq!(fee_ppm(8 * h), 0);
        assert_eq!(fee_ppm(i64::MAX), 0);
        // Never rises with age.
        let mut last = u32::MAX;
        for age in (0..9 * h).step_by(97) {
            let f = fee_ppm(age);
            assert!(f <= last, "fee rose at {age}");
            last = f;
        }
    }

    #[test]
    fn fees_round_down_and_never_exceed_the_amount() {
        assert_eq!(fee_on(1_000, 200_000), 200);
        assert_eq!(fee_on(4, 200_000), 0);
        assert_eq!(fee_on(u64::MAX, 200_000), u64::MAX / 5);
    }

    #[test]
    fn stamps_round_trip_and_unknown_data_reads_as_none() {
        assert_eq!(read_since(&stamp(1_791_419_498)), Some(1_791_419_498));
        assert_eq!(read_since(&[0u8; HOOK_DATA_LEN]), None);
        let mut other = [7u8; HOOK_DATA_LEN];
        other[..3].copy_from_slice(b"KT\x01");
        assert_eq!(read_since(&other), None);
    }

    #[test]
    fn age_blends_by_weight() {
        assert_eq!(blend(0, 999, 50, 100), 100);
        assert_eq!(blend(100, 0, 100, 1_000), 500);
        assert_eq!(blend(300, 0, 100, 1_000), 250);
        // A dust buy barely moves an old wallet's age; a large one dominates.
        assert_eq!(blend(1_000_000, 0, 1, 86_400), 0);
        assert_eq!(blend(1, 0, 1_000_000, 86_400), 86_399);
    }
}
