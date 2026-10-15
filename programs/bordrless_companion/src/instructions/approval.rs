//! Approvals (2026-10-12; the owner: "anyone can create their own companions and hooks with
//! complete freedom", gated by Bordrless's approval): the protocol's records of the programs builders
//! may use with a companion, each bound to the program's code.
//!
//! - `approve(program, kind, code_hash, author)`: the companion program's upgrade authority, or the
//!   script approver (`SCRIPT_APPROVER`: a hot key of Bordrless's backend that writes and revokes
//!   approvals and can do nothing else here), approves
//!   `program`, an upgradeable-loader program (its program account and its ProgramData passed, and
//!   its `Timelock` when it has one), as a token hook the site offers beside a companion (kind 0,
//!   read off chain) or as a companion script (kind 1, `create_script` and `script_step`). Only a
//!   program nobody but Bordrless can change is approved (the owner, 2026-10-12: Bordrless builds and
//!   deploys every approved script itself): immutable, upgradeable only by Bordrless's keys
//!   (`HOOK_UPGRADE_AUTHORITIES`, as the launchpad and strategies take), or behind a valid hook
//!   timelock (`ProgramNotBounded` otherwise). `code_hash` must be the program's executable hash
//!   (`solana-verify`'s), recomputed here from its ProgramData. `author` is who wrote it (a script's
//!   author earns `AUTHOR_SHARE_BPS` of the launcher's part of the creator fees of the companions
//!   made with it).
//!   `PDA(["approval", program])` is made or rewritten, and is no longer revoked.
//! - `revoke_approval(program)`: either of them marks it revoked.
//! - [`approval_of`]: what every check of an approval asks: the approval exists, is not revoked, is
//!   of the kind asked, and the program still runs the approved code (its ProgramData passed). An
//!   upgrade of an approved program voids its approval until Bordrless approves the new code.
//! - [`script_approval`]: what a script needs of its program (the owner, 2026-10-12: the rule the
//!   launchpad applies to hooks): nothing when the program is immutable (its upgrade authority
//!   given up, so nobody can change its code; its approval account, if any, is not read, and the
//!   script has no author), else a current approval of kind `APPROVAL_KIND_SCRIPT`.

use anchor_lang::prelude::*;
use bordrless_hook::authority::{
    classify, parse_programdata, programdata_address, timelock_address, BPF_LOADER_UPGRADEABLE_ID,
};

use crate::constants::*;
use crate::error::CompanionError;
use crate::events::*;
use crate::instructions::attest::programdata_hash;
use crate::instructions::game::upgrade_authority;
use crate::state::*;

/// The protocol's own programs (and the system program): never a script, which runs with a
/// creator address signing.
pub(crate) fn is_protocol_program(program: &Pubkey) -> bool {
    [
        crate::ID,
        LAUNCH_ID,
        SWAP_ID,
        TOKEN_ID,
        KIT_ID,
        BRIDGE_ID,
        anchor_lang::system_program::ID,
    ]
    .contains(program)
}

/// Checks that `program` is a deployed upgradeable-loader program whose ProgramData is
/// `programdata` (`ProgramAccounts` otherwise), and answers the ProgramData's header: the slot of
/// the program's last deploy, upgrade or extension, and its upgrade authority (`None`: immutable).
pub(crate) fn programdata_header(
    program: &AccountInfo,
    programdata: &AccountInfo,
) -> Result<(u64, Option<Pubkey>)> {
    require!(
        *program.owner == BPF_LOADER_UPGRADEABLE_ID && program.executable,
        CompanionError::ProgramAccounts
    );
    {
        let data = program.try_borrow_data()?;
        require!(
            data.len() >= 36
                && data[..4] == 2u32.to_le_bytes()
                && data[4..36] == programdata.key.to_bytes(),
            CompanionError::ProgramAccounts
        );
    }
    require_keys_eq!(
        *programdata.key,
        programdata_address(program.key),
        CompanionError::ProgramAccounts
    );
    require_keys_eq!(
        *programdata.owner,
        BPF_LOADER_UPGRADEABLE_ID,
        CompanionError::ProgramAccounts
    );
    let data = programdata.try_borrow_data()?;
    Ok(parse_programdata(&data).ok_or(CompanionError::ProgramAccounts)?)
}

/// The ProgramData's slot ([`programdata_header`]).
pub(crate) fn programdata_slot(program: &AccountInfo, programdata: &AccountInfo) -> Result<u64> {
    Ok(programdata_header(program, programdata)?.0)
}

/// Whether `program` is immutable ([`programdata_header`]): its upgrade authority given up, so
/// nobody can change its code.
pub(crate) fn is_immutable(program: &AccountInfo, programdata: &AccountInfo) -> Result<bool> {
    Ok(programdata_header(program, programdata)?.1.is_none())
}

/// The approval `approval` holds for `program` (its program account and ProgramData checked:
/// `ProgramAccounts` otherwise) and `kind` as the program runs now, if it holds: it is the account
/// at `PDA(["approval", program])`, exists, is not revoked, is of `kind`, and the program's code is
/// the approved code. The code is the approved code while the ProgramData's slot is the one the
/// approval recorded (the loader refuses a second deploy, upgrade or extension in one slot, so the
/// code can't have changed since it was hashed); once the slot moved (an upgrade, or anyone's
/// `ExtendProgram`, which changes the slot but not the code), the code is hashed again.
pub(crate) fn approval_of(
    approval: &AccountInfo,
    program: &AccountInfo,
    programdata: &AccountInfo,
    kind: u8,
) -> Result<Option<Approval>> {
    let slot = programdata_slot(program, programdata)?;
    if *approval.key != Approval::address(program.key).0 || *approval.owner != crate::ID {
        return Ok(None);
    }
    let a = {
        let data = approval.try_borrow_data()?;
        match Approval::try_deserialize(&mut &data[..]) {
            Ok(a) => a,
            Err(_) => return Ok(None),
        }
    };
    if a.program != *program.key || a.revoked || a.kind != kind {
        return Ok(None);
    }
    if slot == a.programdata_slot {
        return Ok(Some(a));
    }
    let (hash, _) = programdata_hash(program.key, programdata)?;
    Ok((hash == a.code_hash).then_some(a))
}

/// What a script needs of its program `program` (its ProgramData `programdata`, both checked as
/// [`programdata_slot`] does): nothing when the program is immutable (`Ok(None)`: no approval is
/// read, and the script has no author), else its current approval of kind `APPROVAL_KIND_SCRIPT`
/// ([`approval_of`]; `NotApproved` without one).
pub(crate) fn script_approval(
    approval: &AccountInfo,
    program: &AccountInfo,
    programdata: &AccountInfo,
) -> Result<Option<Approval>> {
    if is_immutable(program, programdata)? {
        return Ok(None);
    }
    match approval_of(approval, program, programdata, APPROVAL_KIND_SCRIPT)? {
        Some(a) => Ok(Some(a)),
        None => err!(CompanionError::NotApproved),
    }
}

/// Whether `signer` writes approvals: the protocol authority (this program's upgrade authority,
/// read from its ProgramData, `program_data`) or the script approver, whose signature is all that
/// is checked (`program_data` is not read for it).
fn may_approve(signer: &Pubkey, program_data: &AccountInfo) -> Result<bool> {
    if *signer == SCRIPT_APPROVER {
        return Ok(true);
    }
    Ok(upgrade_authority(program_data)? == Some(*signer))
}

// ---- approve ---------------------------------------------------------------------------------------

/// Accounts of `approve`. Remaining: the program's `Timelock` (`PDA(["timelock", program],
/// hook_timelock)`), read only when the program's upgrade authority is its timelock.
#[event_cpi]
#[derive(Accounts)]
pub struct Approve<'info> {
    /// The companion program's upgrade authority (the protocol's) or the script approver; pays the
    /// rent the first time.
    #[account(mut)]
    pub authority: Signer<'info>,
    /// CHECK: this program's ProgramData (address and owner checked in the handler).
    pub program_data: UncheckedAccount<'info>,
    /// CHECK: the approved program, the instruction's `program` (checked in the handler, with its
    /// being an upgradeable-loader program naming `target_programdata`).
    pub target: UncheckedAccount<'info>,
    /// CHECK: its ProgramData (address, owner and header checked in the handler).
    pub target_programdata: UncheckedAccount<'info>,
    #[account(
        init_if_needed,
        payer = authority,
        space = Approval::LEN,
        seeds = [APPROVAL_SEED, target.key().as_ref()],
        bump
    )]
    pub approval: Box<Account<'info, Approval>>,
    pub system_program: Program<'info, System>,
}

/// `approve(program, kind, code_hash, author)`: see the module.
pub fn process_approve(
    ctx: Context<Approve>,
    program: Pubkey,
    kind: u8,
    code_hash: [u8; 32],
    author: Pubkey,
) -> Result<()> {
    let authority = ctx.accounts.authority.key();
    require!(
        may_approve(&authority, &ctx.accounts.program_data)?,
        CompanionError::NotProtocolAuthority
    );
    require_keys_eq!(
        ctx.accounts.target.key(),
        program,
        CompanionError::ProgramAccounts
    );
    require!(
        kind == APPROVAL_KIND_TOKEN_HOOK
            || (kind == APPROVAL_KIND_SCRIPT && !is_protocol_program(&program)),
        CompanionError::BadApproval
    );
    let slot = programdata_slot(&ctx.accounts.target, &ctx.accounts.target_programdata)?;
    // Who can change the code: only Bordrless (or nobody, or a public timelock).
    let lock = timelock_address(&program).0;
    let timelock = ctx.remaining_accounts.iter().find(|a| *a.key == lock);
    let class = classify(
        &ctx.accounts.target,
        &ctx.accounts.target_programdata,
        timelock,
    )
    .map_err(|_| error!(CompanionError::ProgramAccounts))?;
    require!(class.is_bounded(), CompanionError::ProgramNotBounded);
    let (hash, _) = programdata_hash(&program, &ctx.accounts.target_programdata)?;
    require!(hash == code_hash, CompanionError::BadApproval);
    let now = Clock::get()?.unix_timestamp;
    let a = &mut ctx.accounts.approval;
    a.version = APPROVAL_VERSION;
    a.bump = ctx.bumps.approval;
    a.program = program;
    a.kind = kind;
    a.code_hash = code_hash;
    a.approved_at = now;
    a.revoked = false;
    a.updated_by = authority;
    a.programdata_slot = slot;
    a.author = author;
    a.reserved = [0; 24];
    emit_cpi!(ProgramApproved {
        program,
        kind,
        code_hash,
        programdata_slot: slot,
        authority,
        author,
    });
    Ok(())
}

// ---- revoke_approval --------------------------------------------------------------------------------

/// Accounts of `revoke_approval`.
#[event_cpi]
#[derive(Accounts)]
pub struct RevokeApproval<'info> {
    /// The companion program's upgrade authority, or the script approver.
    pub authority: Signer<'info>,
    /// CHECK: this program's ProgramData (address and owner checked in the handler).
    pub program_data: UncheckedAccount<'info>,
    /// The approval of the instruction's `program` (checked in the handler).
    #[account(mut, seeds = [APPROVAL_SEED, approval.program.as_ref()], bump = approval.bump)]
    pub approval: Box<Account<'info, Approval>>,
}

/// `revoke_approval(program)`: the approval of `program` is revoked (until `approve` writes it
/// again).
pub fn process_revoke_approval(ctx: Context<RevokeApproval>, program: Pubkey) -> Result<()> {
    let authority = ctx.accounts.authority.key();
    require!(
        may_approve(&authority, &ctx.accounts.program_data)?,
        CompanionError::NotProtocolAuthority
    );
    require_keys_eq!(
        ctx.accounts.approval.program,
        program,
        CompanionError::ProgramAccounts
    );
    let a = &mut ctx.accounts.approval;
    a.revoked = true;
    a.updated_by = authority;
    emit_cpi!(ApprovalRevoked {
        program,
        kind: a.kind,
        code_hash: a.code_hash,
        authority,
    });
    Ok(())
}
