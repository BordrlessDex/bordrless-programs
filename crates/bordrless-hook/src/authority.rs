//! Who can change a program's code (`docs/phase3a.md` §2): one classification shared by the
//! launchpad (a custom token hook), the companion (a game hook, a strategy), `hook_vault` and the
//! SDK's risk labels. The class is computed from on-chain accounts every time and never stored: a
//! stored label goes stale.
//!
//! - [`AuthorityClass::Immutable`]: nobody (BPF loader 2; the upgradeable loader with no upgrade
//!   authority; a finalized loader-v4 program).
//! - [`AuthorityClass::Timelocked`]: the upgradeable loader's authority is the program's own
//!   `hook_timelock` account (`PDA(["timelock", program], HOOK_TIMELOCK_ID)`), which only ever
//!   upgrades it after a public delay of at least [`MIN_ACCEPTED_DELAY`], or makes it immutable.
//! - [`AuthorityClass::Protocol`]: one of Bordrless's keys ([`HOOK_UPGRADE_AUTHORITIES`]: Studio's
//!   upgrade key, the protocol's).
//! - [`AuthorityClass::Author`]: anyone else, or an account this module can't read.
//!
//! Byte layouts read (all little-endian):
//!
//! - upgradeable loader (v3) program: `[2u32, programdata: Pubkey]`; ProgramData:
//!   `[3u32, slot: u64, Option<Pubkey>]` (a 1-byte tag, then the key), the code from byte 45;
//!   buffer: `[1u32, Option<Pubkey>]`, the code from byte 37;
//! - loader v4: `[slot: u64, authority: Pubkey, status: u64]` (status 2: finalized), the code from
//!   byte 48;
//! - `hook_timelock`'s `Timelock` ([`timelock_offsets`]).

use anchor_lang::prelude::*;

/// The upgradeable loader (v3).
pub const BPF_LOADER_UPGRADEABLE_ID: Pubkey =
    Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111");
/// The BPF loader 2: its programs are immutable.
pub const BPF_LOADER_2_ID: Pubkey =
    Pubkey::from_str_const("BPFLoader2111111111111111111111111111111111");
/// Loader v4.
pub const LOADER_V4_ID: Pubkey =
    Pubkey::from_str_const("LoaderV411111111111111111111111111111111111");

/// Bordrless Studio's upgrade key: every hook Studio deploys "managed" is upgradeable by it.
pub const STUDIO_UPGRADE_AUTHORITY: Pubkey =
    Pubkey::from_str_const("CS1NRyXNCPxEUP4CRoa26cHQSeSJCxXh5SPijwFhDW6W");
/// The protocol's own upgrade authority (Half-Life, tax_hook, lottery_hook).
pub const PROTOCOL_UPGRADE_AUTHORITY: Pubkey =
    Pubkey::from_str_const("5xsibKwtiN6ruxsYrEyWVpV3KcwuzSPbQd1n28a7spEd");
/// The keys a program may be upgradeable by and still count as "Bordrless-managed"
/// ([`AuthorityClass::Protocol`]). The launchpad's `HOOK_UPGRADE_AUTHORITIES` is this list.
pub const HOOK_UPGRADE_AUTHORITIES: [Pubkey; 2] =
    [STUDIO_UPGRADE_AUTHORITY, PROTOCOL_UPGRADE_AUTHORITY];

/// `hook_timelock`: holds a program's upgrade authority behind a public delay
/// (`docs/phase3a.md` §3).
pub const HOOK_TIMELOCK_ID: Pubkey =
    Pubkey::from_str_const("BBUzaamchPWZpKENmn7bopiuQWvRGm2Vg8TqLLgzgGGZ");
/// `PDA(["timelock", program], HOOK_TIMELOCK_ID)`: a program's `Timelock`, which is also its
/// upgrade authority.
pub const TIMELOCK_SEED: &[u8] = b"timelock";
/// The shortest delay a timelock may have, and the shortest every reader accepts: 3 days.
pub const MIN_ACCEPTED_DELAY: u32 = 3 * 86_400;
/// The longest: 365 days.
pub const MAX_DELAY: u32 = 365 * 86_400;
/// `sha256("account:Timelock")[..8]`, Anchor's discriminator of `hook_timelock`'s `Timelock`.
pub const TIMELOCK_DISCRIMINATOR: [u8; 8] = [0xbd, 0x21, 0x4e, 0x4b, 0xcd, 0x1f, 0x04, 0xb1];

/// Where `hook_timelock`'s `Timelock` keeps each field (absolute offsets, the discriminator
/// first). `hook_timelock`'s tests hold its account to these.
pub mod timelock_offsets {
    /// `version: u8`.
    pub const VERSION: usize = 8;
    /// `bump: u8` (of `PDA(["timelock", program])`).
    pub const BUMP: usize = 9;
    /// `program: Pubkey`.
    pub const PROGRAM: usize = 10;
    /// `programdata: Pubkey`.
    pub const PROGRAMDATA: usize = 42;
    /// `author: Pubkey`: who may propose, cancel, lengthen, finalize and hand the role over.
    pub const AUTHOR: usize = 74;
    /// `pending_author: Pubkey` (the default key: none).
    pub const PENDING_AUTHOR: usize = 106;
    /// `delay_secs: u32`.
    pub const DELAY_SECS: usize = 138;
    /// `finalized: bool`: the program was made immutable through the timelock.
    pub const FINALIZED: usize = 142;
    /// `pending_buffer: Pubkey` (the default key: no proposal).
    pub const PENDING_BUFFER: usize = 143;
    /// `pending_hash: [u8; 32]`: the proposed code's executable hash.
    pub const PENDING_HASH: usize = 175;
    /// `pending_len: u32`.
    pub const PENDING_LEN: usize = 207;
    /// `proposed_at: i64`.
    pub const PROPOSED_AT: usize = 211;
    /// `eta: i64`: when anyone may execute it.
    pub const ETA: usize = 219;
    /// `upgrades: u32`.
    pub const UPGRADES: usize = 227;
    /// `created_at: i64`.
    pub const CREATED_AT: usize = 231;
    /// `last_upgraded_at: i64`.
    pub const LAST_UPGRADED_AT: usize = 239;
    /// `reserved: [u8; 32]`.
    pub const RESERVED: usize = 247;
    /// The account's length.
    pub const LEN: usize = 279;
}

/// Bytes of a ProgramData's header: the code starts here.
pub const PROGRAMDATA_HEADER_LEN: usize = 45;
/// Bytes of a buffer's header.
pub const BUFFER_HEADER_LEN: usize = 37;
/// Bytes of a loader-v4 program's header.
pub const LOADER_V4_HEADER_LEN: usize = 48;

/// Who can change a program's code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthorityClass {
    /// Nobody.
    Immutable,
    /// Its `hook_timelock` account, after `delay_secs` of public notice.
    Timelocked {
        /// The timelock's delay (at least [`MIN_ACCEPTED_DELAY`]).
        delay_secs: u32,
    },
    /// One of [`HOOK_UPGRADE_AUTHORITIES`].
    Protocol(Pubkey),
    /// Anyone else (`None`: a header this module can't read).
    Author(Option<Pubkey>),
}

impl AuthorityClass {
    /// Whether its code can never change without Bordrless or a public delay: immutable,
    /// timelocked or Bordrless-managed.
    pub fn is_bounded(&self) -> bool {
        !matches!(self, AuthorityClass::Author(_))
    }

    /// Whether an audit of its code stays an audit of what runs: immutable or Bordrless-managed
    /// (`docs/phase3a.md` §2.3; a timelocked program is finalized first).
    pub fn auditable(&self) -> bool {
        matches!(self, AuthorityClass::Immutable | AuthorityClass::Protocol(_))
    }
}

/// Why a program could not be classified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClassError {
    /// The ProgramData is not the program's (address, owner or header).
    WrongProgramData,
    /// The program's authority is its timelock, and the `Timelock` was not passed.
    TimelockMissing,
    /// The program's authority is its timelock, and the account passed is not a valid `Timelock`
    /// for it (owner, discriminator, program, ProgramData, bump) or its delay is below
    /// [`MIN_ACCEPTED_DELAY`].
    TimelockInvalid,
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

fn key_at(data: &[u8], at: usize) -> Option<Pubkey> {
    Some(Pubkey::new_from_array(data.get(at..at + 32)?.try_into().ok()?))
}

/// A program's ProgramData address under the upgradeable loader: `PDA([program], loader)`.
pub fn programdata_address(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[program.as_ref()], &BPF_LOADER_UPGRADEABLE_ID).0
}

/// A program's `Timelock` (and upgrade authority, once registered): `PDA(["timelock", program],
/// HOOK_TIMELOCK_ID)`.
pub fn timelock_address(program: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[TIMELOCK_SEED, program.as_ref()], &HOOK_TIMELOCK_ID)
}

/// What a ProgramData header says: the slot of its last deploy, upgrade or extension, and its
/// upgrade authority (`None`: immutable). `None` for bytes that are not a ProgramData header.
pub fn parse_programdata(data: &[u8]) -> Option<(u64, Option<Pubkey>)> {
    if u32_at(data, 0)? != 3 {
        return None;
    }
    let slot = u64_at(data, 4)?;
    match *data.get(12)? {
        0 => Some((slot, None)),
        1 => Some((slot, Some(key_at(data, 13)?))),
        _ => None,
    }
}

/// The upgrade authority of a buffer (`[1u32, Option<Pubkey>]`); `None` for bytes that are not a
/// buffer, `Some(None)` for a buffer with no authority.
pub fn parse_buffer(data: &[u8]) -> Option<Option<Pubkey>> {
    if u32_at(data, 0)? != 1 {
        return None;
    }
    match *data.get(4)? {
        0 => Some(None),
        1 => Some(Some(key_at(data, 5)?)),
        _ => None,
    }
}

/// What a `Timelock` account says, read from its bytes ([`timelock_offsets`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimelockView {
    /// The bump of its address.
    pub bump: u8,
    /// The program it holds.
    pub program: Pubkey,
    /// That program's ProgramData.
    pub programdata: Pubkey,
    /// Who may propose, cancel, lengthen, finalize and hand over.
    pub author: Pubkey,
    /// The delay.
    pub delay_secs: u32,
    /// Made immutable through it.
    pub finalized: bool,
    /// The proposal waiting, if any: its buffer, executable hash, length and eta.
    pub pending: Option<PendingView>,
}

/// A proposal waiting in a `Timelock`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingView {
    /// The buffer holding the proposed code (its authority the timelock).
    pub buffer: Pubkey,
    /// sha256 of the proposed code, trailing zeros stripped (`solana-verify`'s executable hash).
    pub hash: [u8; 32],
    /// The code's length, trailing zeros stripped.
    pub len: u32,
    /// When it was proposed.
    pub proposed_at: i64,
    /// When anyone may execute it.
    pub eta: i64,
}

/// Parses a `Timelock` account's data (discriminator and length checked; owner and address are
/// the caller's to check).
pub fn parse_timelock(data: &[u8]) -> Option<TimelockView> {
    use timelock_offsets as o;
    if data.len() < o::LEN || data[..8] != TIMELOCK_DISCRIMINATOR {
        return None;
    }
    let buffer = key_at(data, o::PENDING_BUFFER)?;
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&data[o::PENDING_HASH..o::PENDING_HASH + 32]);
    let pending = (buffer != Pubkey::default()).then(|| PendingView {
        buffer,
        hash,
        len: u32_at(data, o::PENDING_LEN).unwrap_or(0),
        proposed_at: u64_at(data, o::PROPOSED_AT).unwrap_or(0) as i64,
        eta: u64_at(data, o::ETA).unwrap_or(0) as i64,
    });
    Some(TimelockView {
        bump: data[o::BUMP],
        program: key_at(data, o::PROGRAM)?,
        programdata: key_at(data, o::PROGRAMDATA)?,
        author: key_at(data, o::AUTHOR)?,
        delay_secs: u32_at(data, o::DELAY_SECS)?,
        finalized: data[o::FINALIZED] != 0,
        pending,
    })
}

/// The class of an upgradeable-loader program whose upgrade authority is `authority`.
///
/// A key of [`HOOK_UPGRADE_AUTHORITIES`] is `Protocol`. A key equal to the program's timelock
/// address is `Timelocked` when `timelock` is that account, owned by [`HOOK_TIMELOCK_ID`], a
/// `Timelock` for `program` and `programdata`, at the address its bump gives, with a delay of at
/// least [`MIN_ACCEPTED_DELAY`] (else `TimelockInvalid`, or `TimelockMissing` when not passed).
/// Any other key is `Author`. The timelock address is derived only for a key that is neither a
/// protocol key nor the passed account's own key.
pub fn class_of_authority(
    program: &Pubkey,
    programdata: &Pubkey,
    authority: Option<Pubkey>,
    timelock: Option<&AccountInfo>,
) -> core::result::Result<AuthorityClass, ClassError> {
    let Some(key) = authority else {
        return Ok(AuthorityClass::Immutable);
    };
    if HOOK_UPGRADE_AUTHORITIES.contains(&key) {
        return Ok(AuthorityClass::Protocol(key));
    }
    match timelock {
        // Only an account of `hook_timelock`'s is read as one: any other account passed (the
        // author's own key, say) leaves an author's program `Author`.
        Some(info) if *info.key == key && *info.owner == HOOK_TIMELOCK_ID => {
            let view = info
                .try_borrow_data()
                .ok()
                .and_then(|d| parse_timelock(&d))
                .ok_or(ClassError::TimelockInvalid)?;
            let at = Pubkey::create_program_address(
                &[TIMELOCK_SEED, program.as_ref(), &[view.bump]],
                &HOOK_TIMELOCK_ID,
            )
            .map_err(|_| ClassError::TimelockInvalid)?;
            if at != key
                || view.program != *program
                || view.programdata != *programdata
                || view.delay_secs < MIN_ACCEPTED_DELAY
            {
                return Err(ClassError::TimelockInvalid);
            }
            Ok(AuthorityClass::Timelocked {
                delay_secs: view.delay_secs,
            })
        }
        other => {
            if timelock_address(program).0 == key {
                Err(if other.is_some() {
                    ClassError::TimelockInvalid
                } else {
                    ClassError::TimelockMissing
                })
            } else {
                Ok(AuthorityClass::Author(Some(key)))
            }
        }
    }
}

/// The class of the upgradeable-loader program `program` from its ProgramData alone (no program
/// account): `programdata` must be at `PDA([program], loader)` and owned by the loader (which alone
/// writes accounts it owns, and creates one at that address only for a program deployed there),
/// with a ProgramData header. What the companion reads a hook or a strategy by.
pub fn classify_programdata(
    program: &Pubkey,
    programdata: &AccountInfo,
    timelock: Option<&AccountInfo>,
) -> core::result::Result<AuthorityClass, ClassError> {
    if *programdata.owner != BPF_LOADER_UPGRADEABLE_ID
        || *programdata.key != programdata_address(program)
    {
        return Err(ClassError::WrongProgramData);
    }
    let authority = {
        let data = programdata
            .try_borrow_data()
            .map_err(|_| ClassError::WrongProgramData)?;
        parse_programdata(&data).ok_or(ClassError::WrongProgramData)?.1
    };
    class_of_authority(program, programdata.key, authority, timelock)
}

/// The class of `program` (its account passed; for an upgradeable-loader program, `programdata`
/// is its ProgramData at `PDA([program], loader)`):
///
/// - owner BPF loader 2: `Immutable`;
/// - owner loader v4: `Immutable` when finalized, else `Protocol` or `Author` by its authority (the
///   timelock serves only the upgradeable loader);
/// - owner the upgradeable loader: the program names `programdata`, which the loader owns, and
///   its header's authority classes it ([`class_of_authority`]); a header byte this module can't
///   read is `Author(None)`;
/// - any other owner: `Author(None)`.
///
/// The launchpad's `check_hook_authority` takes `Immutable`, `Protocol` and `Timelocked` (and
/// checks `programdata`'s address first, for every loader).
pub fn classify(
    program: &AccountInfo,
    programdata: &AccountInfo,
    timelock: Option<&AccountInfo>,
) -> core::result::Result<AuthorityClass, ClassError> {
    if *program.owner == BPF_LOADER_2_ID {
        return Ok(AuthorityClass::Immutable);
    }
    if *program.owner == LOADER_V4_ID {
        let data = program
            .try_borrow_data()
            .map_err(|_| ClassError::WrongProgramData)?;
        let (Some(status), Some(key)) = (u64_at(&data, 40), key_at(&data, 8)) else {
            return Ok(AuthorityClass::Author(None));
        };
        if status == 2 {
            return Ok(AuthorityClass::Immutable);
        }
        return Ok(if HOOK_UPGRADE_AUTHORITIES.contains(&key) {
            AuthorityClass::Protocol(key)
        } else {
            AuthorityClass::Author(Some(key))
        });
    }
    if *program.owner != BPF_LOADER_UPGRADEABLE_ID {
        return Ok(AuthorityClass::Author(None));
    }
    {
        let data = program
            .try_borrow_data()
            .map_err(|_| ClassError::WrongProgramData)?;
        if data.len() < 36
            || data[..4] != 2u32.to_le_bytes()
            || data[4..36] != programdata.key.to_bytes()
        {
            return Err(ClassError::WrongProgramData);
        }
    }
    if *programdata.owner != BPF_LOADER_UPGRADEABLE_ID {
        return Err(ClassError::WrongProgramData);
    }
    let header = {
        let data = programdata
            .try_borrow_data()
            .map_err(|_| ClassError::WrongProgramData)?;
        if data.len() < 13 || data[..4] != 3u32.to_le_bytes() {
            return Err(ClassError::WrongProgramData);
        }
        parse_programdata(&data)
    };
    match header {
        Some((_, authority)) => class_of_authority(program.key, programdata.key, authority, timelock),
        None => Ok(AuthorityClass::Author(None)),
    }
}

/// The length of `code` without its trailing zero bytes: what `solana-verify` hashes (the
/// executable hash is `sha256(code[..trimmed_len(code)])`). [`trimmed_len_by`] with plain slice
/// equality.
pub fn trimmed_len(code: &[u8]) -> usize {
    trimmed_len_by(code, |a, b| a == b)
}

/// [`trimmed_len`], comparing chunks with `eq` (two slices of equal length).
///
/// Anyone can grow a ProgramData with zeros (`ExtendProgram` is permissionless, up to 10 MiB), so the
/// zero tail is scanned in chunks, each compared with the zeros already found after it: a chunk is
/// the size of the tail known so far (up to [`TRIM_CHUNK_MAX`]), halved on a mismatch down to
/// [`TRIM_CHUNK_MIN`] bytes, then the rest byte by byte. On chain, pass the `sol_memcmp` syscall as
/// `eq` (about 1 unit per 250 bytes: some 45k units for a 10 MiB tail, against 1.2M at 8 bytes a
/// step); this crate forbids `unsafe`, which the syscall's binding needs, so the caller supplies it.
pub fn trimmed_len_by(code: &[u8], eq: impl Fn(&[u8], &[u8]) -> bool) -> usize {
    let len = code.len();
    let mut end = len;
    // A first few zeros the plain way: what later chunks are compared with.
    while end > 0 && len - end < TRIM_CHUNK_MIN && code[end - 1] == 0 {
        end -= 1;
    }
    if len - end == TRIM_CHUNK_MIN {
        let mut size = TRIM_CHUNK_MIN;
        while end > 0 {
            let n = size.min(len - end).min(end);
            if eq(&code[end - n..end], &code[end..end + n]) {
                end -= n;
                size = size.saturating_mul(2).min(TRIM_CHUNK_MAX);
            } else if n <= TRIM_CHUNK_MIN {
                break;
            } else {
                size = n / 2;
            }
        }
        while end > 0 && code[end - 1] == 0 {
            end -= 1;
        }
    }
    end
}

/// The smallest chunk [`trimmed_len_by`] compares at once (bytes).
pub const TRIM_CHUNK_MIN: usize = 32;
/// The largest chunk [`trimmed_len_by`] compares at once (bytes).
pub const TRIM_CHUNK_MAX: usize = 1 << 20;

/// The code of an executable account's data for its loader: after the ProgramData header (v3), the
/// loader-v4 header, or all of it (loader 2). `None` for another owner or too short a header.
pub fn code_of<'a>(owner: &Pubkey, data: &'a [u8]) -> Option<&'a [u8]> {
    if *owner == BPF_LOADER_UPGRADEABLE_ID {
        data.get(PROGRAMDATA_HEADER_LEN..)
    } else if *owner == LOADER_V4_ID {
        data.get(LOADER_V4_HEADER_LEN..)
    } else if *owner == BPF_LOADER_2_ID {
        Some(data)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info<'a>(
        key: &'a Pubkey,
        owner: &'a Pubkey,
        lamports: &'a mut u64,
        data: &'a mut [u8],
    ) -> AccountInfo<'a> {
        AccountInfo::new(key, false, false, lamports, data, owner, false)
    }

    fn programdata_bytes(authority: Option<Pubkey>, slot: u64) -> Vec<u8> {
        let mut d = vec![0u8; 45 + 16];
        d[..4].copy_from_slice(&3u32.to_le_bytes());
        d[4..12].copy_from_slice(&slot.to_le_bytes());
        if let Some(a) = authority {
            d[12] = 1;
            d[13..45].copy_from_slice(a.as_ref());
        }
        d[45] = 0x7f;
        d
    }

    fn timelock_bytes(program: &Pubkey, programdata: &Pubkey, bump: u8, delay: u32) -> Vec<u8> {
        use timelock_offsets as o;
        let mut d = vec![0u8; o::LEN];
        d[..8].copy_from_slice(&TIMELOCK_DISCRIMINATOR);
        d[o::VERSION] = 1;
        d[o::BUMP] = bump;
        d[o::PROGRAM..o::PROGRAM + 32].copy_from_slice(program.as_ref());
        d[o::PROGRAMDATA..o::PROGRAMDATA + 32].copy_from_slice(programdata.as_ref());
        d[o::DELAY_SECS..o::DELAY_SECS + 4].copy_from_slice(&delay.to_le_bytes());
        d
    }

    #[test]
    fn the_constants_hold_together() {
        // `hook_timelock`'s tests hold TIMELOCK_DISCRIMINATOR to its `Timelock`'s.
        assert_eq!(
            HOOK_UPGRADE_AUTHORITIES,
            [STUDIO_UPGRADE_AUTHORITY, PROTOCOL_UPGRADE_AUTHORITY]
        );
        const { assert!(MIN_ACCEPTED_DELAY == 259_200 && MAX_DELAY > MIN_ACCEPTED_DELAY) };
    }

    #[test]
    fn upgradeable_programs_class_by_their_authority() {
        let program = Pubkey::new_unique();
        let pd_key = programdata_address(&program);
        let (tl_key, bump) = timelock_address(&program);
        let mut program_data = vec![0u8; 36];
        program_data[..4].copy_from_slice(&2u32.to_le_bytes());
        program_data[4..].copy_from_slice(pd_key.as_ref());
        let cases: Vec<(Option<Pubkey>, core::result::Result<AuthorityClass, ClassError>)> = vec![
            (None, Ok(AuthorityClass::Immutable)),
            (
                Some(STUDIO_UPGRADE_AUTHORITY),
                Ok(AuthorityClass::Protocol(STUDIO_UPGRADE_AUTHORITY)),
            ),
            (
                Some(PROTOCOL_UPGRADE_AUTHORITY),
                Ok(AuthorityClass::Protocol(PROTOCOL_UPGRADE_AUTHORITY)),
            ),
        ];
        for (authority, want) in cases {
            let (mut l1, mut l2) = (1u64, 1u64);
            let mut pd = programdata_bytes(authority, 9);
            let mut pr = program_data.clone();
            let p = info(&program, &BPF_LOADER_UPGRADEABLE_ID, &mut l1, &mut pr);
            let d = info(&pd_key, &BPF_LOADER_UPGRADEABLE_ID, &mut l2, &mut pd);
            assert_eq!(classify(&p, &d, None), want);
            assert_eq!(classify_programdata(&program, &d, None), want);
        }
        // A stranger: Author. A timelock: Timelocked with its account, an error without.
        let stranger = Pubkey::new_unique();
        let (mut l1, mut l2, mut l3) = (1u64, 1u64, 1u64);
        let mut pd = programdata_bytes(Some(stranger), 9);
        let d = info(&pd_key, &BPF_LOADER_UPGRADEABLE_ID, &mut l2, &mut pd);
        let mut pr = program_data.clone();
        let p = info(&program, &BPF_LOADER_UPGRADEABLE_ID, &mut l1, &mut pr);
        assert_eq!(
            classify(&p, &d, None),
            Ok(AuthorityClass::Author(Some(stranger)))
        );
        let mut pd2 = programdata_bytes(Some(tl_key), 9);
        let d2 = info(&pd_key, &BPF_LOADER_UPGRADEABLE_ID, &mut l3, &mut pd2);
        assert_eq!(classify(&p, &d2, None), Err(ClassError::TimelockMissing));
        for (delay, owner, prog, pdk, b, want) in [
            (
                MIN_ACCEPTED_DELAY,
                HOOK_TIMELOCK_ID,
                program,
                pd_key,
                bump,
                Ok(AuthorityClass::Timelocked {
                    delay_secs: MIN_ACCEPTED_DELAY,
                }),
            ),
            (
                MIN_ACCEPTED_DELAY - 1,
                HOOK_TIMELOCK_ID,
                program,
                pd_key,
                bump,
                Err(ClassError::TimelockInvalid),
            ),
            (
                MAX_DELAY,
                Pubkey::new_unique(),
                program,
                pd_key,
                bump,
                Err(ClassError::TimelockInvalid),
            ),
            (
                MAX_DELAY,
                HOOK_TIMELOCK_ID,
                Pubkey::new_unique(),
                pd_key,
                bump,
                Err(ClassError::TimelockInvalid),
            ),
            (
                MAX_DELAY,
                HOOK_TIMELOCK_ID,
                program,
                Pubkey::new_unique(),
                bump,
                Err(ClassError::TimelockInvalid),
            ),
            (
                MAX_DELAY,
                HOOK_TIMELOCK_ID,
                program,
                pd_key,
                bump.wrapping_sub(1),
                Err(ClassError::TimelockInvalid),
            ),
        ] {
            let mut l = 1u64;
            let mut tl = timelock_bytes(&prog, &pdk, b, delay);
            let t = info(&tl_key, &owner, &mut l, &mut tl);
            assert_eq!(classify(&p, &d2, Some(&t)), want, "{delay} {owner} {b}");
            assert_eq!(classify_programdata(&program, &d2, Some(&t)), want);
        }
        // Another account passed as the timelock: the authority is still the timelock's address.
        let other = Pubkey::new_unique();
        let mut l = 1u64;
        let mut tl = timelock_bytes(&program, &pd_key, bump, MAX_DELAY);
        let t = info(&other, &HOOK_TIMELOCK_ID, &mut l, &mut tl);
        assert_eq!(
            classify(&p, &d2, Some(&t)),
            Err(ClassError::TimelockInvalid)
        );
        // A stranger's program with any account passed stays Author.
        assert_eq!(
            classify(&p, &d, Some(&t)),
            Ok(AuthorityClass::Author(Some(stranger)))
        );
        // A discriminator off by a bit.
        let mut l = 1u64;
        let mut tl = timelock_bytes(&program, &pd_key, bump, MAX_DELAY);
        tl[0] ^= 1;
        let t = info(&tl_key, &HOOK_TIMELOCK_ID, &mut l, &mut tl);
        assert_eq!(
            classify(&p, &d2, Some(&t)),
            Err(ClassError::TimelockInvalid)
        );
    }

    #[test]
    fn wrong_programdata_and_other_loaders() {
        let program = Pubkey::new_unique();
        let pd_key = programdata_address(&program);
        let mut pr = vec![0u8; 36];
        pr[..4].copy_from_slice(&2u32.to_le_bytes());
        pr[4..].copy_from_slice(pd_key.as_ref());
        let (mut l1, mut l2) = (1u64, 1u64);
        let p = info(&program, &BPF_LOADER_UPGRADEABLE_ID, &mut l1, &mut pr);
        // Not owned by the loader.
        let mut pd = programdata_bytes(None, 1);
        let fake_owner = Pubkey::new_unique();
        let d = info(&pd_key, &fake_owner, &mut l2, &mut pd);
        assert_eq!(classify(&p, &d, None), Err(ClassError::WrongProgramData));
        assert_eq!(
            classify_programdata(&program, &d, None),
            Err(ClassError::WrongProgramData)
        );
        // At another address.
        let other = Pubkey::new_unique();
        let mut l3 = 1u64;
        let mut pd3 = programdata_bytes(None, 1);
        let d3 = info(&other, &BPF_LOADER_UPGRADEABLE_ID, &mut l3, &mut pd3);
        assert_eq!(classify(&p, &d3, None), Err(ClassError::WrongProgramData));
        assert_eq!(
            classify_programdata(&program, &d3, None),
            Err(ClassError::WrongProgramData)
        );
        // A header tag this module can't read: Author(None).
        let mut l4 = 1u64;
        let mut pd4 = programdata_bytes(None, 1);
        pd4[12] = 2;
        let d4 = info(&pd_key, &BPF_LOADER_UPGRADEABLE_ID, &mut l4, &mut pd4);
        assert_eq!(classify(&p, &d4, None), Ok(AuthorityClass::Author(None)));
        // Loader 2: immutable. Loader v4: finalized or by its key.
        let (mut l5, mut l6) = (1u64, 1u64);
        let mut empty = vec![];
        let mut pd5 = programdata_bytes(None, 1);
        let p2 = info(&program, &BPF_LOADER_2_ID, &mut l5, &mut empty);
        let d5 = info(&pd_key, &BPF_LOADER_UPGRADEABLE_ID, &mut l6, &mut pd5);
        assert_eq!(classify(&p2, &d5, None), Ok(AuthorityClass::Immutable));
        for (status, key, want) in [
            (2u64, Pubkey::new_unique(), None),
            (
                0,
                STUDIO_UPGRADE_AUTHORITY,
                Some(AuthorityClass::Protocol(STUDIO_UPGRADE_AUTHORITY)),
            ),
            (0, other, Some(AuthorityClass::Author(Some(other)))),
        ] {
            let mut v4 = vec![0u8; 48];
            v4[8..40].copy_from_slice(key.as_ref());
            v4[40..48].copy_from_slice(&status.to_le_bytes());
            let mut l7 = 1u64;
            let p4 = info(&program, &LOADER_V4_ID, &mut l7, &mut v4);
            assert_eq!(
                classify(&p4, &d5, None),
                Ok(want.unwrap_or(AuthorityClass::Immutable))
            );
        }
        // Any other owner.
        let mut l8 = 1u64;
        let mut e = vec![];
        let sys = Pubkey::default();
        let p8 = info(&program, &sys, &mut l8, &mut e);
        assert_eq!(classify(&p8, &d5, None), Ok(AuthorityClass::Author(None)));
    }

    #[test]
    fn trimmed_lengths() {
        assert_eq!(trimmed_len(&[]), 0);
        assert_eq!(trimmed_len(&[0; 20]), 0);
        assert_eq!(trimmed_len(&[1, 0, 0]), 1);
        let mut v = vec![7u8; 33];
        v.extend([0u8; 100]);
        assert_eq!(trimmed_len(&v), 33);
        v[5] = 0;
        assert_eq!(trimmed_len(&v), 33);
        for n in 0..40 {
            let mut w = vec![0u8; 40];
            if n > 0 {
                w[n - 1] = 9;
            }
            assert_eq!(trimmed_len(&w), n);
        }
        // The chunked scan against the plain one: every nonzero position over tails of every size.
        let naive = |c: &[u8]| c.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
        for len in [0usize, 1, 31, 32, 33, 63, 64, 65, 100, 1000, 4097, 70_000] {
            let mut positions: Vec<usize> = (0..len.min(80)).collect();
            positions.extend((len.saturating_sub(80)..len).step_by(1));
            positions.extend((0..len).step_by(997));
            for at in positions {
                for prefix_zeros in [false, true] {
                    let mut w = vec![0u8; len];
                    w[at] = 1 + (at % 200) as u8;
                    if !prefix_zeros && at > 0 {
                        w[0] = 3;
                    }
                    assert_eq!(trimmed_len(&w), naive(&w), "len {len} at {at}");
                }
            }
            assert_eq!(trimmed_len(&vec![0u8; len]), 0);
        }
        let mut big = vec![0u8; 10 << 20];
        big[12_345] = 1;
        assert_eq!(trimmed_len(&big), 12_346);
        assert_eq!(parse_buffer(&[1, 0, 0, 0, 0]), Some(None));
        assert_eq!(parse_buffer(&[3, 0, 0, 0, 0]), None);
        let k = Pubkey::new_unique();
        let mut b = vec![1, 0, 0, 0, 1];
        b.extend(k.to_bytes());
        assert_eq!(parse_buffer(&b), Some(Some(k)));
        assert_eq!(parse_programdata(&programdata_bytes(Some(k), 5)), Some((5, Some(k))));
    }
}
