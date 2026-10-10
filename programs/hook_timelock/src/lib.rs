//! `hook_timelock`: a hook's (or a strategy's) upgrade authority behind a public delay
//! (`docs/phase3a.md` §3).
//!
//! A program registers once (`register`, signed by its current upgrade authority): its
//! `Timelock`, `PDA(["timelock", program])`, becomes its upgrade authority. From then on:
//!
//! - its author proposes new code (`propose`): a loader buffer whose authority is already the
//!   `Timelock` (so nobody but this program can write or close it again: its bytes are fixed), its
//!   executable hash computed on chain, live no earlier than `delay_secs` later (`eta`);
//! - anyone executes it (`execute`) from `eta` for `EXECUTE_WINDOW`; the buffer's lamports go to
//!   the author;
//! - the author cancels it (`cancel`), or anyone expires it after the window (`expire`): the buffer
//!   is closed to the author;
//! - the author lengthens the delay (`lengthen`, never shortens it), hands the role over
//!   (`propose_author`, `accept_author`), reclaims a stray buffer (`reclaim_buffer`), or makes the
//!   program immutable (`finalize`, at once: immutability only removes power).
//!
//! **The invariant:** no instruction, in any state, sets a program's or a buffer's authority to
//! any key other than the `Timelock` or none, or closes a program. There is no "unregister", and
//! Bordrless has no override. A lost author key leaves the program as it is.
//!
//! Readers class a program whose authority is its `Timelock` as "timelocked"
//! (`bordrless_hook::authority`), from the account's bytes at fixed offsets.

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use bordrless_hook::authority::{
    parse_buffer, parse_programdata, programdata_address, trimmed_len, BPF_LOADER_UPGRADEABLE_ID,
    BUFFER_HEADER_LEN, MAX_DELAY, MIN_ACCEPTED_DELAY, TIMELOCK_SEED,
};

pub mod client;
pub mod loader;

declare_id!("BBUzaamchPWZpKENmn7bopiuQWvRGm2Vg8TqLLgzgGGZ");

#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Bordrless hook timelock",
    project_url: "https://github.com/BordrlessDex/bordrless-programs",
    contacts: "link:https://github.com/BordrlessDex/bordrless-programs/security/advisories/new",
    policy: "https://github.com/BordrlessDex/bordrless-programs/blob/main/SECURITY.md",
    source_code: "https://github.com/BordrlessDex/bordrless-programs"
}

/// Layout version of [`Timelock`].
pub const VERSION: u8 = 1;
/// The shortest delay: 3 days (the owner's decision; every reader accepts no less).
pub const MIN_DELAY: u32 = MIN_ACCEPTED_DELAY;
/// How long a proposal stays executable after its `eta`: 30 days. After that it expires.
pub const EXECUTE_WINDOW: i64 = 30 * 86_400;
/// The longest code a proposal may carry: 2 MiB (about 1.05M compute units of hashing).
pub const MAX_CODE_LEN: u32 = 2 * 1024 * 1024;

#[program]
pub mod hook_timelock {
    use super::*;

    /// The program's current upgrade authority hands it to its new `Timelock` (delay `delay_secs`,
    /// author `author`): `SetAuthorityChecked`, the `Timelock` signing.
    pub fn register(ctx: Context<Register>, delay_secs: u32, author: Pubkey) -> Result<()> {
        process_register(ctx, delay_secs, author)
    }

    /// The author proposes the code in `buffer` (its authority already the `Timelock`), `len`
    /// bytes long without its trailing zeros: its hash is computed here, live from `now + delay`.
    pub fn propose(ctx: Context<Propose>, len: u32) -> Result<()> {
        process_propose(ctx, len)
    }

    /// The author cancels the proposal: its buffer is closed to them.
    pub fn cancel(ctx: Context<CloseProposal>) -> Result<()> {
        process_cancel(ctx)
    }

    /// Anyone, from the proposal's `eta` for `EXECUTE_WINDOW`: the program takes the proposed code.
    pub fn execute(ctx: Context<Execute>) -> Result<()> {
        process_execute(ctx)
    }

    /// Anyone, once the window has passed: the proposal is dropped and its buffer closed to the
    /// author.
    pub fn expire(ctx: Context<CloseProposal>) -> Result<()> {
        process_expire(ctx)
    }

    /// The author closes a buffer whose authority is the `Timelock` but which is not the one
    /// proposed (a mistake), to themselves.
    pub fn reclaim_buffer(ctx: Context<ReclaimBuffer>) -> Result<()> {
        process_reclaim_buffer(ctx)
    }

    /// The author lengthens the delay (never shortens it); a pending `eta` moves with it.
    pub fn lengthen(ctx: Context<AuthorOnly>, delay_secs: u32) -> Result<()> {
        process_lengthen(ctx, delay_secs)
    }

    /// The author names who may take the role (the default key clears it).
    pub fn propose_author(ctx: Context<AuthorOnly>, new_author: Pubkey) -> Result<()> {
        process_propose_author(ctx, new_author)
    }

    /// The named key takes the author role.
    pub fn accept_author(ctx: Context<AcceptAuthor>) -> Result<()> {
        process_accept_author(ctx)
    }

    /// The author makes the program immutable, at once (no proposal may be pending).
    pub fn finalize(ctx: Context<Finalize>) -> Result<()> {
        process_finalize(ctx)
    }
}

// ---- state --------------------------------------------------------------------------------------

/// `PDA(["timelock", program])`: a program's timelock and upgrade authority. Its fields sit at the
/// offsets `bordrless_hook::authority::timelock_offsets` names (no `Option`: every field has a
/// fixed place).
#[account]
#[derive(InitSpace)]
pub struct Timelock {
    pub version: u8,
    pub bump: u8,
    /// The program it holds, and its ProgramData.
    pub program: Pubkey,
    pub programdata: Pubkey,
    /// Who may propose, cancel, lengthen, finalize and hand the role over.
    pub author: Pubkey,
    /// Who may take the role (`accept_author`); the default key: nobody.
    pub pending_author: Pubkey,
    /// The delay between a proposal and its execution: `MIN_DELAY` to `MAX_DELAY`, never shortened.
    pub delay_secs: u32,
    /// The program was made immutable through this timelock.
    pub finalized: bool,
    /// The proposal waiting (the default buffer: none): its buffer, its code's executable hash
    /// and length, when it was proposed and when anyone may execute it.
    pub pending_buffer: Pubkey,
    pub pending_hash: [u8; 32],
    pub pending_len: u32,
    pub proposed_at: i64,
    pub eta: i64,
    /// Upgrades executed.
    pub upgrades: u32,
    pub created_at: i64,
    pub last_upgraded_at: i64,
    pub reserved: [u8; 32],
}

impl Timelock {
    pub const LEN: usize = 8 + Self::INIT_SPACE;

    pub fn address(program: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[TIMELOCK_SEED, program.as_ref()], &crate::ID)
    }

    pub fn has_pending(&self) -> bool {
        self.pending_buffer != Pubkey::default()
    }

    fn clear_pending(&mut self) {
        self.pending_buffer = Pubkey::default();
        self.pending_hash = [0; 32];
        self.pending_len = 0;
        self.proposed_at = 0;
        self.eta = 0;
    }
}

/// The signer seeds of the timelock of `program`.
struct TimelockSeeds {
    program: Pubkey,
    bump: [u8; 1],
}

impl TimelockSeeds {
    fn of(t: &Timelock) -> Self {
        Self {
            program: t.program,
            bump: [t.bump],
        }
    }

    fn seeds(&self) -> [&[u8]; 3] {
        [TIMELOCK_SEED, self.program.as_ref(), &self.bump]
    }
}

// ---- errors and events --------------------------------------------------------------------------

#[error_code]
pub enum TimelockError {
    #[msg("the delay is at least 3 days")]
    DelayTooShort,
    #[msg("the delay is at most 365 days")]
    DelayTooLong,
    #[msg("only the timelock's author may do this")]
    NotAuthor,
    #[msg("the buffer is not the one proposed")]
    WrongBuffer,
    #[msg("the buffer's authority must be the timelock (set it with the loader's SetAuthority first)")]
    BufferAuthority,
    #[msg("a proposal's code is at most 2 MiB")]
    BufferTooLarge,
    #[msg("the length must be the code's, without its trailing zero bytes")]
    BadLength,
    #[msg("a proposal is pending")]
    ProposalPending,
    #[msg("no proposal is pending")]
    NoProposal,
    #[msg("too early: the delay has not passed")]
    TooEarly,
    #[msg("the proposal's execution window has passed: expire it")]
    Expired,
    #[msg("a delay can only be lengthened")]
    DelayShortened,
    #[msg("the program is not upgradeable by this signer under the upgradeable loader")]
    NotUpgradeable,
    #[msg("the program data is not the program's")]
    WrongProgramData,
    #[msg("the program has been made immutable")]
    Finalized,
    #[msg("the author can't be the default key")]
    BadAuthor,
    #[msg("the proposal's window has not passed yet")]
    NotExpired,
    #[msg("the signer is not the author named to take the role")]
    NotPendingAuthor,
}

#[event]
pub struct TimelockRegistered {
    pub program: Pubkey,
    pub programdata: Pubkey,
    pub timelock: Pubkey,
    pub author: Pubkey,
    pub delay_secs: u32,
    /// Who held the authority before.
    pub previous_authority: Pubkey,
}

#[event]
pub struct UpgradeProposed {
    pub program: Pubkey,
    pub buffer: Pubkey,
    pub hash: [u8; 32],
    pub len: u32,
    pub proposed_at: i64,
    pub eta: i64,
}

#[event]
pub struct UpgradeCancelled {
    pub program: Pubkey,
    pub buffer: Pubkey,
    pub hash: [u8; 32],
}

#[event]
pub struct UpgradeExpired {
    pub program: Pubkey,
    pub buffer: Pubkey,
    pub hash: [u8; 32],
}

#[event]
pub struct Upgraded {
    pub program: Pubkey,
    pub buffer: Pubkey,
    pub hash: [u8; 32],
    pub slot: u64,
    pub upgrades: u32,
    pub sender: Pubkey,
}

#[event]
pub struct BufferReclaimed {
    pub program: Pubkey,
    pub buffer: Pubkey,
}

#[event]
pub struct DelayLengthened {
    pub program: Pubkey,
    pub old_delay_secs: u32,
    pub delay_secs: u32,
    /// The pending proposal's eta after it (0: none pending).
    pub eta: i64,
}

#[event]
pub struct AuthorProposed {
    pub program: Pubkey,
    pub author: Pubkey,
    pub pending_author: Pubkey,
}

#[event]
pub struct AuthorAccepted {
    pub program: Pubkey,
    pub old_author: Pubkey,
    pub author: Pubkey,
}

#[event]
pub struct Finalized {
    pub program: Pubkey,
    pub author: Pubkey,
}

// ---- accounts -----------------------------------------------------------------------------------

#[event_cpi]
#[derive(Accounts)]
pub struct Register<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// The program's current upgrade authority (a wallet, or a multisig signing by CPI).
    pub authority: Signer<'info>,
    /// CHECK: the program to timelock (owner and header checked in the handler).
    pub target: UncheckedAccount<'info>,
    /// CHECK: its ProgramData (address checked here; owner and header in the handler).
    #[account(mut, address = programdata_address(target.key))]
    pub programdata: UncheckedAccount<'info>,
    #[account(
        init,
        payer = payer,
        space = Timelock::LEN,
        seeds = [TIMELOCK_SEED, target.key().as_ref()],
        bump
    )]
    pub timelock: Box<Account<'info, Timelock>>,
    /// CHECK: the upgradeable loader.
    #[account(address = BPF_LOADER_UPGRADEABLE_ID)]
    pub loader: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Propose<'info> {
    pub author: Signer<'info>,
    #[account(
        mut,
        seeds = [TIMELOCK_SEED, timelock.program.as_ref()],
        bump = timelock.bump,
        has_one = author @ TimelockError::NotAuthor
    )]
    pub timelock: Box<Account<'info, Timelock>>,
    /// CHECK: the buffer (owner, header and authority checked in the handler).
    pub buffer: UncheckedAccount<'info>,
}

/// Accounts of `cancel` (the author signing) and `expire` (anyone).
#[event_cpi]
#[derive(Accounts)]
pub struct CloseProposal<'info> {
    /// The author (`cancel`) or anyone (`expire`).
    pub sender: Signer<'info>,
    #[account(
        mut,
        seeds = [TIMELOCK_SEED, timelock.program.as_ref()],
        bump = timelock.bump
    )]
    pub timelock: Box<Account<'info, Timelock>>,
    /// CHECK: the proposal's buffer (address-checked).
    #[account(mut, address = timelock.pending_buffer @ TimelockError::WrongBuffer)]
    pub buffer: UncheckedAccount<'info>,
    /// CHECK: the author, who gets the buffer's lamports (address-checked).
    #[account(mut, address = timelock.author @ TimelockError::NotAuthor)]
    pub author: UncheckedAccount<'info>,
    /// CHECK: the upgradeable loader.
    #[account(address = BPF_LOADER_UPGRADEABLE_ID)]
    pub loader: UncheckedAccount<'info>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Execute<'info> {
    /// Anyone.
    pub sender: Signer<'info>,
    #[account(
        mut,
        seeds = [TIMELOCK_SEED, timelock.program.as_ref()],
        bump = timelock.bump
    )]
    pub timelock: Box<Account<'info, Timelock>>,
    /// CHECK: the timelocked program (address-checked).
    #[account(mut, address = timelock.program)]
    pub target: UncheckedAccount<'info>,
    /// CHECK: its ProgramData (address-checked).
    #[account(mut, address = timelock.programdata)]
    pub programdata: UncheckedAccount<'info>,
    /// CHECK: the proposal's buffer (address-checked).
    #[account(mut, address = timelock.pending_buffer @ TimelockError::WrongBuffer)]
    pub buffer: UncheckedAccount<'info>,
    /// CHECK: the author, who gets the buffer's lamports (the loader's spill; address-checked).
    #[account(mut, address = timelock.author @ TimelockError::NotAuthor)]
    pub author: UncheckedAccount<'info>,
    /// CHECK: the rent sysvar.
    #[account(address = loader::RENT_SYSVAR)]
    pub rent: UncheckedAccount<'info>,
    /// CHECK: the clock sysvar.
    #[account(address = loader::CLOCK_SYSVAR)]
    pub clock: UncheckedAccount<'info>,
    /// CHECK: the upgradeable loader.
    #[account(address = BPF_LOADER_UPGRADEABLE_ID)]
    pub loader: UncheckedAccount<'info>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct ReclaimBuffer<'info> {
    #[account(mut)]
    pub author: Signer<'info>,
    #[account(
        seeds = [TIMELOCK_SEED, timelock.program.as_ref()],
        bump = timelock.bump,
        has_one = author @ TimelockError::NotAuthor
    )]
    pub timelock: Box<Account<'info, Timelock>>,
    /// CHECK: a buffer whose authority is the timelock (checked in the handler).
    #[account(mut)]
    pub buffer: UncheckedAccount<'info>,
    /// CHECK: the upgradeable loader.
    #[account(address = BPF_LOADER_UPGRADEABLE_ID)]
    pub loader: UncheckedAccount<'info>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct AuthorOnly<'info> {
    pub author: Signer<'info>,
    #[account(
        mut,
        seeds = [TIMELOCK_SEED, timelock.program.as_ref()],
        bump = timelock.bump,
        has_one = author @ TimelockError::NotAuthor
    )]
    pub timelock: Box<Account<'info, Timelock>>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct AcceptAuthor<'info> {
    pub new_author: Signer<'info>,
    #[account(
        mut,
        seeds = [TIMELOCK_SEED, timelock.program.as_ref()],
        bump = timelock.bump
    )]
    pub timelock: Box<Account<'info, Timelock>>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Finalize<'info> {
    pub author: Signer<'info>,
    #[account(
        mut,
        seeds = [TIMELOCK_SEED, timelock.program.as_ref()],
        bump = timelock.bump,
        has_one = author @ TimelockError::NotAuthor
    )]
    pub timelock: Box<Account<'info, Timelock>>,
    /// CHECK: the program's ProgramData (address-checked).
    #[account(mut, address = timelock.programdata)]
    pub programdata: UncheckedAccount<'info>,
    /// CHECK: the upgradeable loader.
    #[account(address = BPF_LOADER_UPGRADEABLE_ID)]
    pub loader: UncheckedAccount<'info>,
}

// ---- handlers -----------------------------------------------------------------------------------

/// The upgrade authority a ProgramData's header names (`WrongProgramData` for an account that is
/// not a ProgramData).
fn programdata_authority(programdata: &AccountInfo) -> Result<Option<Pubkey>> {
    require_keys_eq!(
        *programdata.owner,
        BPF_LOADER_UPGRADEABLE_ID,
        TimelockError::WrongProgramData
    );
    let data = programdata.try_borrow_data()?;
    Ok(parse_programdata(&data)
        .ok_or(TimelockError::WrongProgramData)?
        .1)
}

fn process_register(ctx: Context<Register>, delay_secs: u32, author: Pubkey) -> Result<()> {
    require!(delay_secs >= MIN_DELAY, TimelockError::DelayTooShort);
    require!(delay_secs <= MAX_DELAY, TimelockError::DelayTooLong);
    require!(author != Pubkey::default(), TimelockError::BadAuthor);
    let program = &ctx.accounts.target;
    let programdata = &ctx.accounts.programdata;
    // An upgradeable-loader program that names this ProgramData.
    require_keys_eq!(
        *program.owner,
        BPF_LOADER_UPGRADEABLE_ID,
        TimelockError::NotUpgradeable
    );
    {
        let data = program.try_borrow_data()?;
        require!(
            data.len() >= 36
                && data[..4] == 2u32.to_le_bytes()
                && data[4..36] == programdata.key.to_bytes(),
            TimelockError::WrongProgramData
        );
    }
    let previous = ctx.accounts.authority.key();
    require!(
        programdata_authority(programdata)? == Some(previous),
        TimelockError::NotUpgradeable
    );
    let now = Clock::get()?.unix_timestamp;
    let timelock_key = ctx.accounts.timelock.key();
    let t = &mut ctx.accounts.timelock;
    t.version = VERSION;
    t.bump = ctx.bumps.timelock;
    t.program = program.key();
    t.programdata = programdata.key();
    t.author = author;
    t.pending_author = Pubkey::default();
    t.delay_secs = delay_secs;
    t.finalized = false;
    t.clear_pending();
    t.upgrades = 0;
    t.created_at = now;
    t.last_upgraded_at = 0;
    t.reserved = [0; 32];
    let seeds = TimelockSeeds::of(t);
    invoke_signed(
        &loader::set_authority_checked(programdata.key(), previous, timelock_key),
        &[
            programdata.to_account_info(),
            ctx.accounts.authority.to_account_info(),
            ctx.accounts.timelock.to_account_info(),
            ctx.accounts.loader.to_account_info(),
        ],
        &[&seeds.seeds()],
    )?;
    // The loader moved it: the program's authority is now this timelock.
    require!(
        programdata_authority(programdata)? == Some(timelock_key),
        TimelockError::NotUpgradeable
    );
    emit_cpi!(TimelockRegistered {
        program: program.key(),
        programdata: programdata.key(),
        timelock: timelock_key,
        author,
        delay_secs,
        previous_authority: previous,
    });
    Ok(())
}

fn process_propose(ctx: Context<Propose>, len: u32) -> Result<()> {
    let t = &ctx.accounts.timelock;
    require!(!t.finalized, TimelockError::Finalized);
    require!(!t.has_pending(), TimelockError::ProposalPending);
    require!(len <= MAX_CODE_LEN, TimelockError::BufferTooLarge);
    require!(len > 0, TimelockError::BadLength);
    let buffer = &ctx.accounts.buffer;
    require_keys_eq!(
        *buffer.owner,
        BPF_LOADER_UPGRADEABLE_ID,
        TimelockError::BufferAuthority
    );
    let timelock_key = t.key();
    let hash = {
        let data = buffer.try_borrow_data()?;
        require!(
            parse_buffer(&data) == Some(Some(timelock_key)),
            TimelockError::BufferAuthority
        );
        let code = &data[BUFFER_HEADER_LEN..];
        // `len` is the code's length without its trailing zeros: the last byte it counts is not
        // zero, and every byte after it is (what `solana-verify get-executable-hash` hashes).
        require!(
            trimmed_len(code) == len as usize,
            TimelockError::BadLength
        );
        solana_sha256_hasher::hash(&code[..len as usize]).to_bytes()
    };
    let now = Clock::get()?.unix_timestamp;
    let eta = now
        .checked_add(i64::from(t.delay_secs))
        .ok_or(TimelockError::DelayTooLong)?;
    let t = &mut ctx.accounts.timelock;
    t.pending_buffer = buffer.key();
    t.pending_hash = hash;
    t.pending_len = len;
    t.proposed_at = now;
    t.eta = eta;
    emit_cpi!(UpgradeProposed {
        program: t.program,
        buffer: buffer.key(),
        hash,
        len,
        proposed_at: now,
        eta,
    });
    Ok(())
}

/// Closes the proposal's buffer to the author (the timelock signing as its authority).
fn close_pending(ctx: &Context<CloseProposal>) -> Result<()> {
    let t = &ctx.accounts.timelock;
    let seeds = TimelockSeeds::of(t);
    invoke_signed(
        &loader::close_buffer(ctx.accounts.buffer.key(), t.author, t.key()),
        &[
            ctx.accounts.buffer.to_account_info(),
            ctx.accounts.author.to_account_info(),
            ctx.accounts.timelock.to_account_info(),
            ctx.accounts.loader.to_account_info(),
        ],
        &[&seeds.seeds()],
    )?;
    Ok(())
}

fn process_cancel(ctx: Context<CloseProposal>) -> Result<()> {
    let t = &ctx.accounts.timelock;
    require_keys_eq!(
        ctx.accounts.sender.key(),
        t.author,
        TimelockError::NotAuthor
    );
    require!(t.has_pending(), TimelockError::NoProposal);
    close_pending(&ctx)?;
    let t = &mut ctx.accounts.timelock;
    let (buffer, hash) = (t.pending_buffer, t.pending_hash);
    t.clear_pending();
    emit_cpi!(UpgradeCancelled {
        program: t.program,
        buffer,
        hash,
    });
    Ok(())
}

fn process_expire(ctx: Context<CloseProposal>) -> Result<()> {
    let t = &ctx.accounts.timelock;
    require!(t.has_pending(), TimelockError::NoProposal);
    let now = Clock::get()?.unix_timestamp;
    require!(
        now >= t.eta.saturating_add(EXECUTE_WINDOW),
        TimelockError::NotExpired
    );
    close_pending(&ctx)?;
    let t = &mut ctx.accounts.timelock;
    let (buffer, hash) = (t.pending_buffer, t.pending_hash);
    t.clear_pending();
    emit_cpi!(UpgradeExpired {
        program: t.program,
        buffer,
        hash,
    });
    Ok(())
}

fn process_execute(ctx: Context<Execute>) -> Result<()> {
    let t = &ctx.accounts.timelock;
    require!(!t.finalized, TimelockError::Finalized);
    require!(t.has_pending(), TimelockError::NoProposal);
    let clock = Clock::get()?;
    require!(clock.unix_timestamp >= t.eta, TimelockError::TooEarly);
    require!(
        clock.unix_timestamp < t.eta.saturating_add(EXECUTE_WINDOW),
        TimelockError::Expired
    );
    let seeds = TimelockSeeds::of(t);
    let timelock_key = t.key();
    invoke_signed(
        &loader::upgrade(
            ctx.accounts.programdata.key(),
            ctx.accounts.target.key(),
            ctx.accounts.buffer.key(),
            t.author,
            timelock_key,
        ),
        &[
            ctx.accounts.programdata.to_account_info(),
            ctx.accounts.target.to_account_info(),
            ctx.accounts.buffer.to_account_info(),
            ctx.accounts.author.to_account_info(),
            ctx.accounts.rent.to_account_info(),
            ctx.accounts.clock.to_account_info(),
            ctx.accounts.timelock.to_account_info(),
            ctx.accounts.loader.to_account_info(),
        ],
        &[&seeds.seeds()],
    )?;
    let t = &mut ctx.accounts.timelock;
    let (buffer, hash) = (t.pending_buffer, t.pending_hash);
    t.clear_pending();
    t.upgrades = t.upgrades.saturating_add(1);
    t.last_upgraded_at = clock.unix_timestamp;
    emit_cpi!(Upgraded {
        program: t.program,
        buffer,
        hash,
        slot: clock.slot,
        upgrades: t.upgrades,
        sender: ctx.accounts.sender.key(),
    });
    Ok(())
}

fn process_reclaim_buffer(ctx: Context<ReclaimBuffer>) -> Result<()> {
    let t = &ctx.accounts.timelock;
    let buffer = &ctx.accounts.buffer;
    require_keys_neq!(buffer.key(), t.pending_buffer, TimelockError::WrongBuffer);
    require_keys_eq!(
        *buffer.owner,
        BPF_LOADER_UPGRADEABLE_ID,
        TimelockError::BufferAuthority
    );
    {
        let data = buffer.try_borrow_data()?;
        require!(
            parse_buffer(&data) == Some(Some(t.key())),
            TimelockError::BufferAuthority
        );
    }
    let seeds = TimelockSeeds::of(t);
    invoke_signed(
        &loader::close_buffer(buffer.key(), t.author, t.key()),
        &[
            buffer.to_account_info(),
            ctx.accounts.author.to_account_info(),
            ctx.accounts.timelock.to_account_info(),
            ctx.accounts.loader.to_account_info(),
        ],
        &[&seeds.seeds()],
    )?;
    emit_cpi!(BufferReclaimed {
        program: t.program,
        buffer: buffer.key(),
    });
    Ok(())
}

fn process_lengthen(ctx: Context<AuthorOnly>, delay_secs: u32) -> Result<()> {
    let t = &mut ctx.accounts.timelock;
    require!(!t.finalized, TimelockError::Finalized);
    require!(delay_secs >= t.delay_secs, TimelockError::DelayShortened);
    require!(delay_secs <= MAX_DELAY, TimelockError::DelayTooLong);
    let old = t.delay_secs;
    t.delay_secs = delay_secs;
    if t.has_pending() {
        t.eta = t
            .eta
            .max(t.proposed_at.saturating_add(i64::from(delay_secs)));
    }
    emit_cpi!(DelayLengthened {
        program: t.program,
        old_delay_secs: old,
        delay_secs,
        eta: t.eta,
    });
    Ok(())
}

fn process_propose_author(ctx: Context<AuthorOnly>, new_author: Pubkey) -> Result<()> {
    let t = &mut ctx.accounts.timelock;
    t.pending_author = new_author;
    emit_cpi!(AuthorProposed {
        program: t.program,
        author: t.author,
        pending_author: new_author,
    });
    Ok(())
}

fn process_accept_author(ctx: Context<AcceptAuthor>) -> Result<()> {
    let signer = ctx.accounts.new_author.key();
    let t = &mut ctx.accounts.timelock;
    require!(
        t.pending_author != Pubkey::default() && t.pending_author == signer,
        TimelockError::NotPendingAuthor
    );
    let old = t.author;
    t.author = signer;
    t.pending_author = Pubkey::default();
    emit_cpi!(AuthorAccepted {
        program: t.program,
        old_author: old,
        author: signer,
    });
    Ok(())
}

fn process_finalize(ctx: Context<Finalize>) -> Result<()> {
    let t = &ctx.accounts.timelock;
    require!(!t.finalized, TimelockError::Finalized);
    require!(!t.has_pending(), TimelockError::ProposalPending);
    let seeds = TimelockSeeds::of(t);
    invoke_signed(
        &loader::set_authority(ctx.accounts.programdata.key(), t.key(), None),
        &[
            ctx.accounts.programdata.to_account_info(),
            ctx.accounts.timelock.to_account_info(),
            ctx.accounts.loader.to_account_info(),
        ],
        &[&seeds.seeds()],
    )?;
    require!(
        programdata_authority(&ctx.accounts.programdata)?.is_none(),
        TimelockError::NotUpgradeable
    );
    let t = &mut ctx.accounts.timelock;
    t.finalized = true;
    emit_cpi!(Finalized {
        program: t.program,
        author: t.author,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_lang::Discriminator;
    use bordrless_hook::authority::{parse_timelock, timelock_offsets as o, TIMELOCK_DISCRIMINATOR};

    #[test]
    fn the_layout_is_the_one_readers_parse() {
        assert_eq!(Timelock::DISCRIMINATOR, TIMELOCK_DISCRIMINATOR);
        assert_eq!(Timelock::LEN, o::LEN);
        let t = Timelock {
            version: VERSION,
            bump: 251,
            program: Pubkey::new_unique(),
            programdata: Pubkey::new_unique(),
            author: Pubkey::new_unique(),
            pending_author: Pubkey::new_unique(),
            delay_secs: 0x0102_0304,
            finalized: true,
            pending_buffer: Pubkey::new_unique(),
            pending_hash: [7; 32],
            pending_len: 0x1112_1314,
            proposed_at: -5,
            eta: 0x2122_2324_2526_2728,
            upgrades: 9,
            created_at: 10,
            last_upgraded_at: 11,
            reserved: [0; 32],
        };
        let mut data = Vec::new();
        t.try_serialize(&mut data).unwrap();
        assert_eq!(data.len(), o::LEN);
        assert_eq!(data[o::VERSION], VERSION);
        assert_eq!(&data[o::AUTHOR..o::AUTHOR + 32], t.author.as_ref());
        assert_eq!(&data[o::PENDING_AUTHOR..o::PENDING_AUTHOR + 32], t.pending_author.as_ref());
        assert_eq!(&data[o::UPGRADES..o::UPGRADES + 4], &9u32.to_le_bytes());
        assert_eq!(&data[o::CREATED_AT..o::CREATED_AT + 8], &10i64.to_le_bytes());
        assert_eq!(
            &data[o::LAST_UPGRADED_AT..o::LAST_UPGRADED_AT + 8],
            &11i64.to_le_bytes()
        );
        let v = parse_timelock(&data).expect("parses");
        assert_eq!(
            (v.bump, v.program, v.programdata, v.author, v.delay_secs, v.finalized),
            (t.bump, t.program, t.programdata, t.author, t.delay_secs, true)
        );
        let p = v.pending.expect("pending");
        assert_eq!(
            (p.buffer, p.hash, p.len, p.proposed_at, p.eta),
            (t.pending_buffer, t.pending_hash, t.pending_len, t.proposed_at, t.eta)
        );
        let none = Timelock {
            pending_buffer: Pubkey::default(),
            ..t
        };
        let mut data = Vec::new();
        none.try_serialize(&mut data).unwrap();
        assert!(parse_timelock(&data).unwrap().pending.is_none());
        const { assert!(MIN_DELAY == 3 * 86_400 && MAX_CODE_LEN == 2_097_152) };
    }

    #[test]
    fn loader_instructions_are_the_loaders() {
        // bincode tags of UpgradeableLoaderInstruction.
        let pd = Pubkey::new_unique();
        let a = Pubkey::new_unique();
        assert_eq!(loader::set_authority_checked(pd, a, a).data, [7, 0, 0, 0]);
        assert_eq!(loader::set_authority(pd, a, None).data, [4, 0, 0, 0]);
        assert_eq!(loader::set_authority(pd, a, None).accounts.len(), 2);
        assert_eq!(loader::upgrade(pd, a, a, a, a).data, [3, 0, 0, 0]);
        assert_eq!(loader::close_buffer(pd, a, a).data, [5, 0, 0, 0]);
        let w = loader::write(pd, a, 3, vec![9, 9]);
        assert_eq!(w.data, [1, 0, 0, 0, 3, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 9, 9]);
        assert_eq!(
            loader::extend_program(pd, a, a, 10_240).data,
            [6, 0, 0, 0, 0, 40, 0, 0]
        );
    }
}
