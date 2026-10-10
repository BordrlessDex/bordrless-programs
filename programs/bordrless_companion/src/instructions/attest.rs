//! Phase 3a (`docs/phase3a.md` §2, §6): who can change a program's code, Bordrless Studio's
//! attestations, and audits tied to code.
//!
//! - `attest(args)`: Studio's attester (`STUDIO_ATTESTER`, a hot key of Studio's build worker)
//!   records that it rebuilt a program's source, matched the build's hash with the code on chain
//!   (checked again here: the hash is recomputed from the ProgramData), and passed the static
//!   checks, the simulator and the review. `PDA(["attest", program])`, rewritten by a later
//!   attestation (a new build of the same program).
//! - `revoke`: the attester or the protocol's upgrade authority marks it revoked. A revocation by
//!   the protocol is a hold on the program: `attest` refuses it from then on (`RevokedByProtocol`);
//!   the protocol can still give it a status.
//! - `set_hook_status_v2(hook, args, audited_hash)`: `set_hook_status` with the audit tied to the
//!   code: an audit needs the program immutable or Bordrless-managed (a timelocked program is
//!   finalized first) and `audited_hash` equal to its code's hash, recomputed here; the hash is
//!   kept in the status. `set_hook_status` (v1) refuses an audit of a program its remaining
//!   accounts show timelocked or author-upgradeable; as deployed, it audits without them (the
//!   protocol's operators audit through v2), records no hash, and keeps the hash an earlier v2
//!   audit recorded. Only an audit with a hash lifts a strategy's cap. v2 may lift an audit it shows
//!   stale (the program no longer auditable, its code not the recorded hash, or no hash recorded):
//!   an audit is otherwise final.
//!
//! An attestation is "current" (`attestation_current`) while it is of the kind asked (a game hook's
//! for a game hook), not revoked, has no other code proposed in the program's timelock, records a passing
//! simulation, and its build hash is the program's code: the same ProgramData slot as when attested,
//! else the hash recomputed (an `ExtendProgram`, which anyone may send, changes the slot but not
//! the code). The companion takes a game hook without a status (Studio's, the protocol's or a
//! timelock's to upgrade) only with a current attestation (owner decision 7).

use anchor_lang::prelude::*;
use bordrless_hook::authority::{
    classify, classify_programdata, code_of, parse_programdata, parse_timelock,
    programdata_address, timelock_address, trimmed_len_by, AuthorityClass, BPF_LOADER_UPGRADEABLE_ID,
    PROGRAMDATA_HEADER_LEN,
};

use crate::constants::*;
use crate::error::CompanionError;
use crate::events::*;
use crate::instructions::game::{emit_event, upgrade_authority, HookStatusArgs, SetHookStatus};
use crate::state::*;

/// `solana-verify`'s executable hash of an executable account's code (`code` its code: after the
/// loader's header), its trailing zeros stripped.
pub(crate) fn code_hash(code: &[u8]) -> [u8; 32] {
    solana_sha256_hasher::hash(&code[..trimmed_len_by(code, zeros_like)]).to_bytes()
}

/// Whether two slices of equal length are equal, by the `sol_memcmp` syscall on chain (about 1 unit
/// per 250 bytes): what the zero tail of a ProgramData anyone can grow is scanned with.
fn zeros_like(chunk: &[u8], zeros: &[u8]) -> bool {
    if chunk.len() != zeros.len() {
        return false;
    }
    // SAFETY: `n` is the length of both slices, so the syscall reads only within them.
    unsafe {
        anchor_lang::solana_program::program_memory::sol_memcmp(chunk, zeros, chunk.len()) == 0
    }
}

/// The executable hash of the upgradeable-loader program `program` from its ProgramData (address,
/// owner and header checked), and the ProgramData's slot.
pub(crate) fn programdata_hash(program: &Pubkey, info: &AccountInfo) -> Result<([u8; 32], u64)> {
    require_keys_eq!(
        *info.key,
        programdata_address(program),
        CompanionError::ProgramAccounts
    );
    require_keys_eq!(
        *info.owner,
        BPF_LOADER_UPGRADEABLE_ID,
        CompanionError::ProgramAccounts
    );
    let data = info.try_borrow_data()?;
    let (slot, _) = parse_programdata(&data).ok_or(CompanionError::ProgramAccounts)?;
    let code = data
        .get(PROGRAMDATA_HEADER_LEN..)
        .ok_or(CompanionError::ProgramAccounts)?;
    Ok((code_hash(code), slot))
}

/// The class of the upgradeable-loader program `program` from its ProgramData and, when it is
/// timelocked, its `Timelock`, both looked up among `available` by address. `None` when its
/// ProgramData was not passed or it can't be classed (a missing or invalid timelock).
pub(crate) fn class_among(available: &[AccountInfo], program: &Pubkey) -> Option<AuthorityClass> {
    let pd = programdata_address(program);
    let info = available.iter().find(|a| *a.key == pd)?;
    let lock = timelock_address(program).0;
    let timelock = available.iter().find(|a| *a.key == lock);
    classify_programdata(program, info, timelock).ok()
}

/// Whether `program` has a current Studio attestation of `kind` among `available` (its
/// `PDA(["attest", program])` and its ProgramData, looked up by address): of that kind, not
/// revoked, a passing simulation, and its build hash the code on chain (the slot unchanged since,
/// or the hash recomputed). When the program's `Timelock` is among `available` with a proposal
/// waiting, the proposed code must be the attested code too: a game is never made on code that
/// other code can replace without notice to its holders.
pub(crate) fn attestation_current(
    available: &[AccountInfo],
    program: &Pubkey,
    kind: u8,
) -> Result<bool> {
    let key = HookAttestation::address(program).0;
    let Some(info) = available.iter().find(|a| *a.key == key) else {
        return Ok(false);
    };
    if *info.owner != crate::ID {
        return Ok(false);
    }
    let a = HookAttestation::try_deserialize(&mut &info.try_borrow_data()?[..])?;
    if a.program != *program || a.revoked || !a.sim_pass || a.kind != kind {
        return Ok(false);
    }
    let lock = timelock_address(program).0;
    if let Some(t) = available.iter().find(|x| *x.key == lock) {
        if *t.owner == HOOK_TIMELOCK_ID {
            let pending = parse_timelock(&t.try_borrow_data()?).and_then(|v| v.pending);
            if pending.is_some_and(|p| p.hash != a.build_hash) {
                return Ok(false);
            }
        }
    }
    let pd = programdata_address(program);
    let Some(pd_info) = available.iter().find(|x| *x.key == pd) else {
        return Ok(false);
    };
    {
        let data = pd_info.try_borrow_data()?;
        if *pd_info.owner != BPF_LOADER_UPGRADEABLE_ID {
            return Ok(false);
        }
        let Some((slot, _)) = parse_programdata(&data) else {
            return Ok(false);
        };
        if slot == a.programdata_slot {
            return Ok(true);
        }
    }
    let (hash, _) = programdata_hash(program, pd_info)?;
    Ok(hash == a.build_hash)
}

/// Whether a game hook's hashed audit holds for the code it runs now (independent audit X4): the
/// hook immutable or Bordrless-managed (its ProgramData's authority none or one of
/// `HOOK_UPGRADE_AUTHORITIES`; a loader-v4 program finalized or under one of those keys; a loader-2
/// program) and its code `recorded`. The hook's code is read from its ProgramData among `available`
/// (by address), or, for a loader-2 or loader-v4 program, from its program account there:
/// `ProgramAccounts` when neither is passed, so leaving them out never keeps an audit (nor trims an
/// audited pot). `memo` is the deploy slot it last held for (`Game::hook_audit_memo`): the hash is
/// recomputed only when the code's slot moved since (an upgrade, or anyone's `ExtendProgram`).
/// Answers the slot it holds for (`None`: it doesn't).
pub(crate) fn hook_audit_holds(
    available: &[AccountInfo],
    hook: &Pubkey,
    recorded: [u8; 32],
    memo: Option<u64>,
) -> Result<Option<u64>> {
    use bordrless_hook::authority::{BPF_LOADER_2_ID, HOOK_UPGRADE_AUTHORITIES, LOADER_V4_ID};
    let pd = programdata_address(hook);
    let info = available
        .iter()
        .find(|a| *a.key == pd)
        .or_else(|| {
            available
                .iter()
                .find(|a| a.key == hook && *a.owner != BPF_LOADER_UPGRADEABLE_ID)
        })
        .ok_or(CompanionError::ProgramAccounts)?;
    let data = info.try_borrow_data()?;
    let managed = |key: Option<Pubkey>| key.is_none_or(|k| HOOK_UPGRADE_AUTHORITIES.contains(&k));
    let (slot, auditable) = if *info.owner == BPF_LOADER_UPGRADEABLE_ID && *info.key == pd {
        let (slot, authority) = parse_programdata(&data).ok_or(CompanionError::ProgramAccounts)?;
        (slot, managed(authority))
    } else if *info.owner == LOADER_V4_ID {
        let word = |at: usize| {
            data.get(at..at + 8)
                .map(|b| u64::from_le_bytes(b.try_into().unwrap_or([0; 8])))
        };
        let (Some(slot), Some(status), Some(key)) = (
            word(0),
            word(40),
            data.get(8..40)
                .map(|b| Pubkey::new_from_array(b.try_into().unwrap_or([0; 32]))),
        ) else {
            return err!(CompanionError::ProgramAccounts);
        };
        (slot, status == 2 || managed(Some(key)))
    } else if *info.owner == BPF_LOADER_2_ID {
        (0, true)
    } else {
        return err!(CompanionError::ProgramAccounts);
    };
    if !auditable {
        return Ok(None);
    }
    if memo == Some(slot) {
        return Ok(Some(slot));
    }
    let code = code_of(info.owner, &data).ok_or(CompanionError::ProgramAccounts)?;
    Ok((code_hash(code) == recorded).then_some(slot))
}

// ---- attest ----------------------------------------------------------------------------------------

/// Arguments of `attest`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct AttestArgs {
    /// `solana-verify`'s executable hash of the code Studio built: must be the code on chain.
    pub build_hash: [u8; 32],
    pub source_hash: [u8; 32],
    pub template_commit: [u8; 20],
    pub sim_version: u16,
    /// Must be true: a failing simulation gets no attestation.
    pub sim_pass: bool,
    pub cut_max_bps: u16,
    pub cap_bps: u16,
    /// `REVIEW_PASS` or `REVIEW_WARN`.
    pub review: u8,
    /// 0 a token hook, 1 a game hook, 2 a strategy.
    pub kind: u8,
}

/// Accounts of `attest`.
#[event_cpi]
#[derive(Accounts)]
pub struct Attest<'info> {
    /// Studio's attester; pays the rent the first time.
    #[account(mut, address = STUDIO_ATTESTER @ CompanionError::NotAttester)]
    pub attester: Signer<'info>,
    /// CHECK: the attested program (an upgradeable-loader program naming `programdata`).
    pub target: UncheckedAccount<'info>,
    /// CHECK: its ProgramData (address, owner and header checked in the handler).
    pub programdata: UncheckedAccount<'info>,
    #[account(
        init_if_needed,
        payer = attester,
        space = HookAttestation::LEN,
        seeds = [ATTEST_SEED, target.key().as_ref()],
        bump
    )]
    pub attestation: Box<Account<'info, HookAttestation>>,
    pub system_program: Program<'info, System>,
}

pub fn process_attest(ctx: Context<Attest>, args: AttestArgs) -> Result<()> {
    require!(
        args.sim_pass && (args.review == REVIEW_PASS || args.review == REVIEW_WARN) && args.kind <= ATTEST_KIND_STRATEGY,
        CompanionError::BadAttestation
    );
    let program = ctx.accounts.target.key();
    {
        let p = &ctx.accounts.target;
        require_keys_eq!(
            *p.owner,
            BPF_LOADER_UPGRADEABLE_ID,
            CompanionError::ProgramAccounts
        );
        let data = p.try_borrow_data()?;
        require!(
            data.len() >= 36
                && data[..4] == 2u32.to_le_bytes()
                && data[4..36] == ctx.accounts.programdata.key.to_bytes(),
            CompanionError::ProgramAccounts
        );
    }
    let (hash, slot) = programdata_hash(&program, &ctx.accounts.programdata)?;
    require!(
        hash == args.build_hash,
        CompanionError::AttestationHashMismatch
    );
    let now = Clock::get()?.unix_timestamp;
    let attester = ctx.accounts.attester.key();
    let a = &mut ctx.accounts.attestation;
    // A program the protocol revoked stays revoked, whatever code it runs later (a revoked build
    // could otherwise come back by way of another): the attester can't attest it again.
    require!(
        !(a.version != 0 && a.reserved[0] == REVOKED_BY_PROTOCOL),
        CompanionError::RevokedByProtocol
    );
    a.version = ATTESTATION_VERSION;
    a.bump = ctx.bumps.attestation;
    a.program = program;
    a.build_hash = args.build_hash;
    a.source_hash = args.source_hash;
    a.template_commit = args.template_commit;
    a.sim_version = args.sim_version;
    a.sim_pass = args.sim_pass;
    a.cut_max_bps = args.cut_max_bps;
    a.cap_bps = args.cap_bps;
    a.review = args.review;
    a.kind = args.kind;
    a.programdata_slot = slot;
    a.attested_at = now;
    a.attester = attester;
    a.revoked = false;
    a.revoked_at = 0;
    a.reserved = [0; 32];
    emit_cpi!(HookAttested {
        program,
        build_hash: args.build_hash,
        source_hash: args.source_hash,
        template_commit: args.template_commit,
        sim_version: args.sim_version,
        cut_max_bps: args.cut_max_bps,
        cap_bps: args.cap_bps,
        review: args.review,
        kind: args.kind,
        programdata_slot: slot,
        attester,
    });
    Ok(())
}

// ---- revoke ----------------------------------------------------------------------------------------

/// Accounts of `revoke`.
#[event_cpi]
#[derive(Accounts)]
pub struct Revoke<'info> {
    /// Studio's attester, or the companion's upgrade authority.
    pub authority: Signer<'info>,
    /// CHECK: this program's ProgramData (read only when the signer is not the attester).
    pub program_data: UncheckedAccount<'info>,
    #[account(mut, seeds = [ATTEST_SEED, attestation.program.as_ref()], bump = attestation.bump)]
    pub attestation: Box<Account<'info, HookAttestation>>,
}

pub fn process_revoke(ctx: Context<Revoke>) -> Result<()> {
    let by = ctx.accounts.authority.key();
    if by != STUDIO_ATTESTER {
        require!(
            upgrade_authority(&ctx.accounts.program_data)? == Some(by),
            CompanionError::NotAttester
        );
    }
    let a = &mut ctx.accounts.attestation;
    if !a.revoked {
        a.revoked = true;
        a.revoked_at = Clock::get()?.unix_timestamp;
    }
    // The protocol's revocation sticks to the build (`attest` refuses it again).
    if by != STUDIO_ATTESTER {
        a.reserved[0] = REVOKED_BY_PROTOCOL;
    }
    emit_cpi!(AttestationRevoked {
        program: a.program,
        build_hash: a.build_hash,
        by,
    });
    Ok(())
}

// ---- audits tied to code ------------------------------------------------------------------------

/// The class of the hook whose program account is among `remaining` (with its ProgramData and its
/// timelock, by address), and the executable hash of its code: what an audit is recorded against.
fn audited_program(remaining: &[AccountInfo], hook: &Pubkey) -> Result<(AuthorityClass, [u8; 32])> {
    let program = remaining
        .iter()
        .find(|a| a.key == hook)
        .ok_or(CompanionError::ProgramAccounts)?;
    let pd_key = programdata_address(hook);
    let lock = timelock_address(hook).0;
    let timelock = remaining.iter().find(|a| *a.key == lock);
    if *program.owner == BPF_LOADER_UPGRADEABLE_ID {
        let pd = remaining
            .iter()
            .find(|a| *a.key == pd_key)
            .ok_or(CompanionError::ProgramAccounts)?;
        let class =
            classify(program, pd, timelock).map_err(|_| error!(CompanionError::ProgramAccounts))?;
        let (hash, _) = programdata_hash(hook, pd)?;
        return Ok((class, hash));
    }
    // Loader 2 or loader v4: the program account holds its code. (Its ProgramData address is an
    // unused one, passed for nothing.)
    let class = classify(program, program, timelock)
        .map_err(|_| error!(CompanionError::ProgramAccounts))?;
    let data = program.try_borrow_data()?;
    let code = code_of(program.owner, &data).ok_or(CompanionError::ProgramAccounts)?;
    Ok((class, code_hash(code)))
}

/// What `set_hook_status` and `set_hook_status_v2` share: the rules of v1 (an audit is final, a
/// block only of a hook not audited and lifted only by an audit, a cap of 0.1 to 10 SOL), and the
/// audited code's hash kept where `reserved` was (zeros when none is recorded).
pub(crate) fn write_status(
    ctx: Context<SetHookStatus>,
    hook: Pubkey,
    args: HookStatusArgs,
    audited_hash: Option<[u8; 32]>,
    unaudit: bool,
) -> Result<()> {
    let authority = ctx.accounts.authority.key();
    require!(
        upgrade_authority(&ctx.accounts.program_data)? == Some(authority),
        CompanionError::NotProtocolAuthority
    );
    let s = &mut ctx.accounts.hook_status;
    // A status made just now reads all zeros: not audited, not blocked.
    let made_before = s.version != 0;
    if made_before {
        require_keys_eq!(s.hook, hook, CompanionError::HookStatusAccount);
    }
    // An audit is final, unless `set_hook_status_v2` has just shown it stale (`unaudit`).
    let (was_audited, was_blocked) = (s.audited && !unaudit, s.blocked);
    // An audit is final.
    if was_audited {
        require!(args.audited, CompanionError::BadHookStatus);
    }
    if args.audited {
        require!(!args.blocked, CompanionError::BadHookStatus);
    } else {
        require!(
            (MIN_POT_CAP..=DEFAULT_POT_CAP).contains(&args.pot_cap),
            CompanionError::BadHookStatus
        );
    }
    if args.blocked {
        require!(!was_audited, CompanionError::BadHookStatus);
    }
    if was_blocked && !args.blocked {
        require!(args.audited, CompanionError::BadHookStatus);
    }
    s.version = HOOK_STATUS_VERSION;
    s.bump = ctx.bumps.hook_status;
    s.hook = hook;
    s.audited = args.audited;
    s.pot_cap = args.pot_cap;
    s.blocked = args.blocked;
    s.updated_at = Clock::get()?.unix_timestamp;
    s.updated_by = authority;
    // v1 (`None`) keeps the hash an earlier audit recorded (an audit is final, so the status stays
    // audited); v2 records its own.
    if let Some(hash) = audited_hash {
        s.reserved = hash;
    } else if !was_audited {
        s.reserved = [0; 32];
    }
    emit_cpi!(HookStatusSet {
        hook,
        audited: args.audited,
        pot_cap: args.pot_cap,
        blocked: args.blocked,
        authority,
    });
    Ok(())
}

/// `set_hook_status` (v1): as deployed, with one refusal added (`docs/phase3a.md` §2.3): an audit of
/// a hook whose ProgramData is among the remaining accounts and shows it timelocked or upgradeable
/// by someone outside Bordrless (`AuditNeedsFixedCode`). It records no audited hash.
pub fn process_set_hook_status(
    ctx: Context<SetHookStatus>,
    hook: Pubkey,
    args: HookStatusArgs,
) -> Result<()> {
    if args.audited {
        if let Some(class) = class_among(ctx.remaining_accounts, &hook) {
            require!(class.auditable(), CompanionError::AuditNeedsFixedCode);
        } else {
            // A ProgramData passed that can't be classed (a timelock missing or forged) is refused.
            let pd = programdata_address(&hook);
            require!(
                !ctx.remaining_accounts.iter().any(|a| *a.key == pd),
                CompanionError::AuditNeedsFixedCode
            );
        }
    }
    write_status(ctx, hook, args, None, false)
}

/// `set_hook_status_v2(hook, args, audited_hash)`: with `args.audited`, the hook (its program
/// account among the remaining accounts, then its ProgramData for an upgradeable-loader program,
/// and its `Timelock` when it has one) must be immutable or Bordrless-managed
/// (`AuditNeedsFixedCode`), and `audited_hash` its code's executable hash, recomputed here
/// (`AuditHashMismatch`); the status keeps it. Without an audit, `audited_hash` must be zeros.
pub fn process_set_hook_status_v2(
    ctx: Context<SetHookStatus>,
    hook: Pubkey,
    args: HookStatusArgs,
    audited_hash: [u8; 32],
) -> Result<()> {
    let mut unaudit = false;
    if args.audited {
        let (class, hash) = audited_program(ctx.remaining_accounts, &hook)?;
        require!(class.auditable(), CompanionError::AuditNeedsFixedCode);
        require!(hash == audited_hash, CompanionError::AuditHashMismatch);
    } else {
        require!(audited_hash == [0; 32], CompanionError::BadHookStatus);
        let s = &ctx.accounts.hook_status;
        if s.version != 0 && s.audited {
            // An audit outlives no change of the code it was given for: shown stale here (the
            // program no longer auditable, as when handed to a timelock, or its code not the
            // recorded hash, or no hash recorded), it may be lifted, and the hook capped or blocked.
            let (class, hash) = audited_program(ctx.remaining_accounts, &hook)?;
            let recorded = s.audited_hash();
            require!(
                !class.auditable() || recorded == [0; 32] || recorded != hash,
                CompanionError::BadHookStatus
            );
            unaudit = true;
        }
    }
    let authority = ctx.accounts.authority.key();
    let audited = args.audited;
    let event_authority = ctx.accounts.event_authority.to_account_info();
    write_status(ctx, hook, args, Some(audited_hash), unaudit)?;
    // After `HookStatusSet`: the hash the audit is tied to.
    emit_event(
        &event_authority,
        &HookAuditRecorded {
            hook,
            audited,
            audited_hash,
            authority,
        },
    )
}
