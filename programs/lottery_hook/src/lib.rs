//! `lottery_hook`: a lottery coin's token hook, under the game ticket standard (`bordrless-game`).
//!
//! Every round (`round_secs`, an hour to 30 days, fixed at `prepare`), the tokens a holder has
//! held since the round began are their tickets: a range of the round's ticket space kept in the
//! 64 bytes of hook data the token program stores inside each holding. The Bordrless companion
//! holds the pot, draws a verifiable ticket once the round has ended, and pays the holding whose
//! range contains it, if that holding still holds the tokens. This program only keeps the tickets:
//! it never sees SOL, never refuses a transfer and takes no cut.
//!
//! A holding gets its range of a round at its first write that round, for what it has held since
//! the round began (`bordrless_game`'s rules): tokens bought or received during a round count from
//! the next round, so moving tokens around never adds tickets.
//!
//! - **Transfers** (`before_transfer`): the header rolls when a new round has begun; at the
//!   sender's first write of the round its range is what it keeps, and every send cuts its ranges
//!   (this round's and the previous round's) to the balance it has left and stamps its `since`; at
//!   the receiver's first write of the round its range is what it held before the transfer.
//! - **Burns** (`before_burn`): like a send. Mints give no tickets: a launch's only mint is its
//!   supply, into the launch, which never holds tickets. The hook does not subscribe to mints.
//! - **`enter(holding)`**: anyone, for any holding not written yet this round (so it has held its
//!   whole balance since the round began): registers that balance for the round. It does nothing
//!   for a holding written this round already. It writes through the token program's
//!   `write_hook_data`, signed by this program's `["hook-authority"]`. A holder (or the keeper, for
//!   everyone) enters once a round to hold tickets in a round it does not trade in.
//!
//! Who never holds tickets: the launch (`["launch", mint]` under the launchpad), its pool (read from
//! the `Launch` account and remembered), the companion's creator address
//! (`["creator", mint]` under the companion), the default key, and every address off the ed25519
//! curve (any program's account: a pool, a vault, an escrow). Their holdings' hook data is never
//! written.
//!
//! The callbacks make no CPI and emit no event: under a companion launch they run at stack height 5,
//! Solana's limit. Their effect is a pure function of the token program's `Transferred` and
//! `Burned` events and the clock, so an indexer replays it exactly.
//!
//! For a launch (`docs/hooks-v2.md` §5.8): `prepare(round_secs)` for the new mint, signed by the
//! mint's keypair (so nobody else can choose its rounds); a `LaunchConfig` naming this program with
//! [`FLAGS`]; then the launch. Registry extras: the state (writable), the launch (read-only).

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_lang::AccountDeserialize;
use bordrless_game::{
    companion_creator_address, eligible, on_enter, on_receive, on_send, valid_round_secs,
    GameHeader, Slots, STATE_SEED,
};
use bordrless_hook::{
    hook_accounts_address, token_flags, write_registry, AccountSource, ExtraAccount,
    HookAccountList, HookReturn, Seed, TokenHookArgs, TokenOp, HOOK_AUTHORITY_SEED,
};
use bordrless_launch::state::Launch;
use bordrless_token::client as token_client;

declare_id!("HqFWsCBQ416DAfevJ9TspyT5yXGGoYTCpcreiGkCgWcr");

#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Bordrless lottery hook",
    project_url: "https://github.com/BordrlessDex/bordrless-programs",
    contacts: "link:https://github.com/BordrlessDex/bordrless-programs/security/advisories/new",
    policy: "https://github.com/BordrlessDex/bordrless-programs/blob/main/SECURITY.md",
    source_code: "https://github.com/BordrlessDex/bordrless-programs"
}

/// Layout version of [`LotteryState`] (after the standard's header).
pub const VERSION: u8 = 1;
/// The launch program's `["launch", mint]` seed.
pub const LAUNCH_SEED: &[u8] = b"launch";
/// The launch program.
pub const LAUNCH_ID: Pubkey = bordrless_launch::ID;
/// The token program's signer of every callback to this hook: `["hook-authority", lottery_hook]`
/// under the token program (checked against its derivation in the unit tests).
pub const TOKEN_HOOK_SIGNER: Pubkey =
    Pubkey::from_str_const("CFyuaxvKmpgnSMCoNqDKCcwxnTUeW8Mm1go1t3UMsvLH");
/// This program's `["hook-authority"]`, which signs the token program's `write_hook_data` (checked
/// against its derivation in the unit tests).
pub const HOOK_AUTHORITY: Pubkey =
    Pubkey::from_str_const("6oZ9LkAfgmhmPYjXj3okjYK5sderddEo8fp8MR4H6Gx1");
/// Its canonical bump.
pub const HOOK_AUTHORITY_BUMP: u8 = 255;
/// The token program's event authority.
pub const TOKEN_EVENT_AUTHORITY: Pubkey = bordrless_token::EVENT_AUTHORITY_AND_BUMP.0;
/// The flags a `LaunchConfig` names for this hook: `before_transfer` and `before_burn`, which write
/// hook data. No deltas (the hook takes no cut), no mint callbacks.
pub const FLAGS: u16 =
    token_flags::BEFORE_TRANSFER | token_flags::BEFORE_BURN | token_flags::WRITES_HOOK_DATA;

/// Instructions of the lottery hook.
#[program]
pub mod lottery_hook {
    use super::*;

    /// Prepares the hook for `mint`, which need not exist yet (a launch's mint: the launchpad
    /// creates it naming this program as its hook): the state, with the standard's header and
    /// rounds of `round_secs`, and the extra-accounts registry. The mint's keypair signs, so only
    /// whoever launches it chooses its rounds; once per mint.
    pub fn prepare(ctx: Context<Prepare>, round_secs: u32) -> Result<()> {
        require!(valid_round_secs(round_secs), LotteryError::BadRoundSecs);
        let mint = ctx.accounts.mint.key();
        let (expected, registry_bump) = hook_accounts_address(&crate::ID, &mint);
        require_keys_eq!(
            ctx.accounts.registry.key(),
            expected,
            LotteryError::WrongAccount
        );
        let now = Clock::get()?.unix_timestamp;
        let launch = Pubkey::find_program_address(&[LAUNCH_SEED, mint.as_ref()], &LAUNCH_ID).0;
        let creator = companion_creator_address(&mint);
        let payer = ctx.accounts.payer.key();
        let state_key = ctx.accounts.state.key();
        let state = &mut ctx.accounts.state;
        state.header = GameHeader::new(mint, round_secs, now);
        state.version = VERSION;
        state.bump = ctx.bumps.state;
        state.launch = launch;
        state.pool = Pubkey::default();
        state.creator = creator;
        state.prepared_by = payer;
        state.prepared_at = now;
        state.reserved = [0; 64];
        let round = state.header.round;
        // The callbacks' extras: the state (written every transfer), the launch account (read for
        // its pool until the state remembers it).
        let list = HookAccountList::new(vec![
            ExtraAccount {
                writable: true,
                source: AccountSource::Pda {
                    program: crate::ID,
                    seeds: vec![Seed::Literal(STATE_SEED.to_vec()), Seed::Account(1)],
                },
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
            registry_bump,
            &list,
        )?;
        emit_cpi!(Prepared {
            mint,
            state: state_key,
            round_secs,
            round,
            launch,
            creator,
            prepared_by: payer,
        });
        Ok(())
    }

    /// Anyone, for any holding of the mint: registers its whole balance for the current round when
    /// it has not been written this round (`bordrless_game::on_enter`). Does nothing for a holding
    /// written this round already, or empty (so nobody can grief a holder by entering it again),
    /// and refuses a holding whose owner can't hold tickets.
    pub fn enter(ctx: Context<Enter>) -> Result<()> {
        let mint_key = ctx.accounts.mint.key();
        let mint = token_client::read_mint(&ctx.accounts.mint)?;
        require!(
            mint.hook_program == Some(crate::ID) && mint.hook_writes_data(),
            LotteryError::NotThisHook
        );
        let holding_key = ctx.accounts.holding.key();
        let holding = token_client::read_holding(&ctx.accounts.holding)?;
        require_keys_eq!(holding.mint, mint_key, LotteryError::WrongMint);
        let now = Clock::get()?.unix_timestamp;
        let state = &mut ctx.accounts.state;
        require!(
            eligible(&holding.owner, &state.excluded()),
            LotteryError::NotEligible
        );
        let mut slots = Slots::decode(&holding.hook_data);
        if !on_enter(&mut state.header, &mut slots, holding.amount, now) {
            return Ok(());
        }
        let ix =
            token_client::write_hook_data(HOOK_AUTHORITY, mint_key, holding_key, slots.encode());
        invoke_signed(
            &ix,
            &[
                ctx.accounts.hook_authority.to_account_info(),
                ctx.accounts.mint.to_account_info(),
                ctx.accounts.holding.to_account_info(),
                ctx.accounts.token_event_authority.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
            ],
            &[&[HOOK_AUTHORITY_SEED, &[HOOK_AUTHORITY_BUMP]]],
        )?;
        let (round, total) = (state.header.round, state.header.total);
        emit_cpi!(Entered {
            mint: mint_key,
            holding: holding_key,
            owner: holding.owner,
            round,
            start: slots.current.start,
            weight: slots.current.weight,
            total,
        });
        Ok(())
    }

    /// `before_transfer`: the header rolled, the sender's tickets cut to what it has left, the
    /// receiver's range of the round opened at its first write (for what it held before); each
    /// answered as the holding's hook data when its owner may hold tickets. Never refuses a
    /// transfer, never takes a cut.
    pub fn before_transfer(ctx: Context<Callback>, args: TokenHookArgs) -> Result<HookReturn> {
        require_keys_eq!(
            args.mint,
            ctx.accounts.mint.key(),
            LotteryError::WrongAccount
        );
        let now = Clock::get()?.unix_timestamp;
        let state = &mut ctx.accounts.state;
        state.header.roll(now);
        remember_pool(state, &ctx.accounts.launch);
        if args.op != TokenOp::Transfer {
            return Ok(HookReturn::default());
        }
        let excluded = state.excluded();
        let mut answer = HookReturn::default();
        if eligible(&args.source_owner, &excluded) {
            let mut slots = Slots::decode(&args.source_hook_data);
            let left = args.source_balance.saturating_sub(args.amount);
            on_send(&mut state.header, &mut slots, left, now);
            answer.source_hook_data = Some(slots.encode());
        }
        if eligible(&args.destination_owner, &excluded) {
            let mut slots = Slots::decode(&args.destination_hook_data);
            let after = args.destination_balance.saturating_add(args.amount);
            on_receive(
                &mut state.header,
                &mut slots,
                args.destination_balance,
                after,
                now,
            );
            answer.destination_hook_data = Some(slots.encode());
        }
        Ok(answer)
    }

    /// `before_burn`: the header rolled and the burner's tickets cut to what it has left, as for a
    /// send.
    pub fn before_burn(ctx: Context<Callback>, args: TokenHookArgs) -> Result<HookReturn> {
        require_keys_eq!(
            args.mint,
            ctx.accounts.mint.key(),
            LotteryError::WrongAccount
        );
        let now = Clock::get()?.unix_timestamp;
        let state = &mut ctx.accounts.state;
        state.header.roll(now);
        remember_pool(state, &ctx.accounts.launch);
        if args.op != TokenOp::Burn {
            return Ok(HookReturn::default());
        }
        let mut answer = HookReturn::default();
        if eligible(&args.source_owner, &state.excluded()) {
            let mut slots = Slots::decode(&args.source_hook_data);
            let left = args.source_balance.saturating_sub(args.amount);
            on_send(&mut state.header, &mut slots, left, now);
            answer.source_hook_data = Some(slots.encode());
        }
        Ok(answer)
    }
}

/// Remembers the launch's pool once the `Launch` account can be read (it is not serialized until
/// `create_launch` ends, so the deposit inside it finds nothing; the first transfer after does).
fn remember_pool(state: &mut LotteryState, launch: &AccountInfo) {
    if state.pool != Pubkey::default() || *launch.owner != LAUNCH_ID {
        return;
    }
    let Ok(data) = launch.try_borrow_data() else {
        return;
    };
    let Ok(launch) = Launch::try_deserialize(&mut &data[..]) else {
        return;
    };
    if launch.mint == state.header.mint && launch.pool != Pubkey::default() {
        state.pool = launch.pool;
    }
}

/// The hook's state for one mint, at `["state", mint]`: the game ticket standard's header first
/// (the companion reads it at the standard's offsets), then the hook's own fields.
#[account]
#[derive(InitSpace)]
pub struct LotteryState {
    /// The standard's header: rounds, the current and previous rounds' ticket totals. The jackpot
    /// fields (`last_buyer`, `last_amount`, `last_buy_at`) stay zero: this hook runs no jackpot.
    pub header: GameHeader,
    /// Layout version.
    pub version: u8,
    /// Bump of `["state", mint]`.
    pub bump: u8,
    /// The launch program's `["launch", mint]`: never holds tickets.
    pub launch: Pubkey,
    /// The launch pool, once read from the launch account (default until then): never holds
    /// tickets.
    pub pool: Pubkey,
    /// The companion's `["creator", mint]`: never holds tickets.
    pub creator: Pubkey,
    /// Who paid for `prepare`.
    pub prepared_by: Pubkey,
    /// When.
    pub prepared_at: i64,
    /// Reserved.
    pub reserved: [u8; 64],
}

impl LotteryState {
    /// The owners that never hold tickets, besides the default key and every address off the curve.
    pub fn excluded(&self) -> [Pubkey; 3] {
        [self.launch, self.pool, self.creator]
    }
}

/// Accounts of `prepare`.
#[event_cpi]
#[derive(Accounts)]
pub struct Prepare<'info> {
    /// Pays the rent.
    #[account(mut)]
    pub payer: Signer<'info>,
    /// The mint the hook is prepared for (not created yet); its keypair signs.
    pub mint: Signer<'info>,
    #[account(init, payer = payer, space = 8 + LotteryState::INIT_SPACE, seeds = [STATE_SEED, mint.key().as_ref()], bump)]
    pub state: Box<Account<'info, LotteryState>>,
    /// CHECK: the registry PDA, created here (address-checked in the handler).
    #[account(mut)]
    pub registry: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Accounts of `enter`.
#[event_cpi]
#[derive(Accounts)]
pub struct Enter<'info> {
    #[account(mut, seeds = [STATE_SEED, mint.key().as_ref()], bump = state.bump)]
    pub state: Box<Account<'info, LotteryState>>,
    /// CHECK: the mint, read as the token program's (owner and discriminator checked), whose hook
    /// must be this program.
    pub mint: UncheckedAccount<'info>,
    /// CHECK: the holding, read as the token program's (owner and discriminator checked) and of
    /// `mint`; the token program writes its hook data.
    #[account(mut)]
    pub holding: UncheckedAccount<'info>,
    /// CHECK: this program's `["hook-authority"]` (address-checked), which signs `write_hook_data`.
    #[account(address = HOOK_AUTHORITY @ LotteryError::WrongAccount)]
    pub hook_authority: UncheckedAccount<'info>,
    /// CHECK: the token program (address-checked).
    #[account(address = bordrless_token::ID @ LotteryError::WrongAccount)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: its event authority (address-checked).
    #[account(address = TOKEN_EVENT_AUTHORITY @ LotteryError::WrongAccount)]
    pub token_event_authority: UncheckedAccount<'info>,
}

/// Accounts of the callbacks: the token program's prefix, then the registry's extras.
#[derive(Accounts)]
pub struct Callback<'info> {
    /// CHECK: the token program's signer for this hook (address- and signer-checked).
    #[account(signer, address = TOKEN_HOOK_SIGNER @ LotteryError::BadHookSigner)]
    pub hook_signer: UncheckedAccount<'info>,
    /// CHECK: the mint (the token program's prefix; equals `args.mint`).
    pub mint: UncheckedAccount<'info>,
    /// CHECK: the source holding (balances, owners and hook data come from the arguments).
    pub source: UncheckedAccount<'info>,
    /// CHECK: the destination holding (the mint for a burn).
    pub destination: UncheckedAccount<'info>,
    /// CHECK: the authority.
    pub authority: UncheckedAccount<'info>,
    #[account(mut, seeds = [STATE_SEED, mint.key().as_ref()], bump = state.bump)]
    pub state: Box<Account<'info, LotteryState>>,
    /// CHECK: the launch account of the mint (address-checked; not readable during the launch).
    #[account(address = state.launch @ LotteryError::WrongAccount)]
    pub launch: UncheckedAccount<'info>,
}

/// The hook was prepared for a mint.
#[event]
pub struct Prepared {
    pub mint: Pubkey,
    pub state: Pubkey,
    pub round_secs: u32,
    /// The round it was prepared in.
    pub round: u32,
    pub launch: Pubkey,
    /// The companion's creator address for the mint.
    pub creator: Pubkey,
    pub prepared_by: Pubkey,
}

/// A holding was entered for the round: its range now, and the round's total.
#[event]
pub struct Entered {
    pub mint: Pubkey,
    pub holding: Pubkey,
    pub owner: Pubkey,
    pub round: u32,
    pub start: u64,
    pub weight: u64,
    pub total: u64,
}

/// Errors.
#[error_code]
pub enum LotteryError {
    #[msg("an account is not the one this mint's hook expects")]
    WrongAccount,
    #[msg("the hook signer is not the token program's signer for this hook")]
    BadHookSigner,
    #[msg("a round lasts from an hour to 30 days")]
    BadRoundSecs,
    #[msg("this mint's hook is not the lottery hook, writing hook data")]
    NotThisHook,
    #[msg("the holding is of another mint")]
    WrongMint,
    #[msg("this owner can't hold tickets: the launch, its pool, the companion's creator address, or an address off the ed25519 curve")]
    NotEligible,
}

/// Instruction builders and addresses, as the tests, the SDK and the companion's clients build them.
pub mod client {
    use super::*;
    use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
    use anchor_lang::{system_program, InstructionData, ToAccountMetas};

    /// This program's event authority.
    pub fn event_authority() -> Pubkey {
        crate::EVENT_AUTHORITY_AND_BUMP.0
    }

    /// The state of `mint`: `["state", mint]`.
    pub fn state_address(mint: &Pubkey) -> Pubkey {
        bordrless_game::state_address(&crate::ID, mint).0
    }

    /// The registry of `mint`: `["bordrless-hook-accounts", mint]`.
    pub fn registry_address(mint: &Pubkey) -> Pubkey {
        hook_accounts_address(&crate::ID, mint).0
    }

    /// The launch of `mint` (the launch program's `["launch", mint]`).
    pub fn launch_address(mint: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[LAUNCH_SEED, mint.as_ref()], &LAUNCH_ID).0
    }

    /// The registry's extras, in its order, for every token instruction on `mint` (after the hook
    /// program and the token program's signer for it): the state, writable; the launch, read-only.
    pub fn extras(mint: &Pubkey) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new(state_address(mint), false),
            AccountMeta::new_readonly(launch_address(mint), false),
        ]
    }

    /// `prepare(round_secs)` for `mint` (whose keypair signs), `payer` paying.
    pub fn prepare(payer: Pubkey, mint: Pubkey, round_secs: u32) -> Instruction {
        Instruction {
            program_id: crate::ID,
            accounts: crate::accounts::Prepare {
                payer,
                mint,
                state: state_address(&mint),
                registry: registry_address(&mint),
                system_program: system_program::ID,
                event_authority: event_authority(),
                program: crate::ID,
            }
            .to_account_metas(None),
            data: crate::instruction::Prepare { round_secs }.data(),
        }
    }

    /// `enter` for `owner`'s holding of `mint`. No signer: whoever pays the fee sends it.
    pub fn enter(mint: Pubkey, owner: Pubkey) -> Instruction {
        Instruction {
            program_id: crate::ID,
            accounts: crate::accounts::Enter {
                state: state_address(&mint),
                mint,
                holding: token_client::holding_address(&mint, &owner),
                hook_authority: HOOK_AUTHORITY,
                token_program: bordrless_token::ID,
                token_event_authority: TOKEN_EVENT_AUTHORITY,
                event_authority: event_authority(),
                program: crate::ID,
            }
            .to_account_metas(None),
            data: crate::instruction::Enter {}.data(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_programs_signer_for_this_hook() {
        assert_eq!(TOKEN_HOOK_SIGNER, token_client::hook_signer(&crate::ID));
    }

    #[test]
    fn this_programs_hook_authority() {
        assert_eq!(
            (HOOK_AUTHORITY, HOOK_AUTHORITY_BUMP),
            bordrless_hook::hook_authority(&crate::ID)
        );
    }

    #[test]
    fn the_seeds_and_programs_are_the_launchpads() {
        assert_eq!(LAUNCH_SEED, bordrless_launch::constants::LAUNCH_SEED);
        assert_eq!(bordrless_game::LAUNCH_SEED, LAUNCH_SEED);
        assert_eq!(bordrless_game::LAUNCH_PROGRAM_ID, LAUNCH_ID);
        assert_eq!(bordrless_game::HOOK_DATA_LEN, bordrless_hook::HOOK_DATA_LEN);
        let mint = Pubkey::new_unique();
        assert_eq!(
            client::launch_address(&mint),
            bordrless_game::launch_address(&mint)
        );
    }

    #[test]
    fn the_header_comes_first() {
        let header = GameHeader::new(Pubkey::new_unique(), 3_600, 1_800_000_000);
        let state = LotteryState {
            header,
            version: VERSION,
            bump: 254,
            launch: Pubkey::new_unique(),
            pool: Pubkey::default(),
            creator: Pubkey::new_unique(),
            prepared_by: Pubkey::new_unique(),
            prepared_at: 1_800_000_000,
            reserved: [0; 64],
        };
        let mut data = Vec::new();
        state.try_serialize(&mut data).unwrap();
        assert_eq!(data.len(), 8 + LotteryState::INIT_SPACE);
        assert_eq!(GameHeader::read(&data, &header.mint), Ok(header));
        // The registry's magic is no state's discriminator.
        assert_ne!(
            &data[..8],
            &bordrless_hook::HOOK_ACCOUNTS_MAGIC[..],
            "a registry can never pass for a state"
        );
    }

    #[test]
    fn flags_write_hook_data_and_take_no_cut() {
        assert_eq!(FLAGS, 1 | 16 | 128);
        assert_eq!(FLAGS & token_flags::TRANSFER_RETURNS_DELTA, 0);
        assert_eq!(
            FLAGS & (token_flags::BEFORE_MINT | token_flags::AFTER_MINT),
            0
        );
    }
}
