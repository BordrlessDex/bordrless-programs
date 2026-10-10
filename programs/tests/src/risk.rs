//! The reference of the SDK's `riskLabelOf` (`docs/phase3a.md` §7): one label of "who can change
//! this code, and what Bordrless says of it", computed from the accounts a terminal reads in one
//! `getMultipleAccounts`, the same words on every surface. `phase3a_vectors.rs` renders it into
//! `vectors/risk-labels.json`, which the SDK's tests hold its TypeScript to.
//!
//! The class comes from `bordrless_hook::authority::classify` (the launchpad's and the companion's
//! own); the rest from the companion's `HookStatus` and `HookAttestation`, read at their offsets.

use anchor_lang::prelude::{AccountInfo, Pubkey};
use bordrless_hook::authority::{
    classify, parse_programdata, parse_timelock, AuthorityClass, ClassError,
    BPF_LOADER_UPGRADEABLE_ID, HOOK_TIMELOCK_ID,
};

/// An account as read: its owner, data and executable flag.
#[derive(Clone, Debug)]
pub struct RawAccount {
    /// Owner program.
    pub owner: Pubkey,
    /// Data.
    pub data: Vec<u8>,
    /// Executable.
    pub executable: bool,
}

/// What a label is computed from (each `None`: the account does not exist).
#[derive(Clone, Debug)]
pub struct RiskAccounts {
    /// The program's id.
    pub program_id: Pubkey,
    /// The program account.
    pub program: Option<RawAccount>,
    /// Its ProgramData (`PDA([program], loader)`).
    pub programdata: Option<RawAccount>,
    /// Its `Timelock` (`PDA(["timelock", program], hook_timelock)`).
    pub timelock: Option<RawAccount>,
    /// `hook_timelock`'s own ProgramData.
    pub timelock_programdata: Option<RawAccount>,
    /// The companion's `HookStatus` of it.
    pub status: Option<RawAccount>,
    /// The companion's `HookAttestation` of it.
    pub attestation: Option<RawAccount>,
}

/// A label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RiskLabel {
    /// `immutable`, `timelocked`, `managed`, `author` or `missing`.
    pub class: &'static str,
    /// The timelock's delay.
    pub delay_secs: Option<u32>,
    /// The timelock's author.
    pub author: Option<Pubkey>,
    /// A proposal waiting: its code's hash (hex), eta, buffer.
    pub pending: Option<(String, i64, Pubkey)>,
    /// Whether `hook_timelock` itself can still be upgraded (timelocked programs only).
    pub timelock_program_upgradeable: Option<bool>,
    /// `current`, `stale` or `false`.
    pub audited: &'static str,
    /// Blocked by the protocol (a companion game's kill switch).
    pub blocked: bool,
    /// The companion's pot cap for its games and strategies (`None`: audited, uncapped).
    pub pot_cap: Option<u64>,
    /// `protocol`, `studio`, `unattested` or `none`.
    pub provenance: &'static str,
    /// Studio's attestation, when there is one: build hash (hex), simulation passed, the largest
    /// cut seen (bps), whether it is current.
    pub studio: Option<(String, bool, u16, bool)>,
    /// `low`, `medium` or `high`.
    pub severity: &'static str,
    /// One sentence.
    pub words: String,
}

/// Bordrless's own hook programs: their provenance is the protocol.
pub const BORDRLESS_HOOKS: [Pubkey; 4] = [
    Pubkey::from_str_const("HqFWsCBQ416DAfevJ9TspyT5yXGGoYTCpcreiGkCgWcr"),
    Pubkey::from_str_const("8tjnVSreJGBRQFyDBf1SyyhBgLsdBxa2rHYh9sbxFyX7"),
    Pubkey::from_str_const("53SpmtkdPWQ63mWoDeXk8P9tuwiT4ed2Wx4fwfy5NSF8"),
    Pubkey::from_str_const("14RJQXPdJfkehit6ezktjd3xujamf8nVSKw2shKamaEH"),
];

/// The companion's `HookStatus`: `[disc 8][version][bump][hook 32][audited][pot_cap u64][blocked]
/// [updated_at i64][updated_by 32][audited_hash 32]`.
mod status_offsets {
    pub const AUDITED: usize = 8 + 1 + 1 + 32;
    pub const POT_CAP: usize = AUDITED + 1;
    pub const BLOCKED: usize = POT_CAP + 8;
    pub const AUDITED_HASH: usize = BLOCKED + 1 + 8 + 32;
    pub const LEN: usize = AUDITED_HASH + 32;
}

/// The companion's `HookAttestation`: `[disc 8][version][bump][program 32][build_hash 32]
/// [source_hash 32][template_commit 20][sim_version u16][sim_pass][cut_max_bps u16][cap_bps u16]
/// [review][kind][programdata_slot u64][attested_at i64][attester 32][revoked][revoked_at i64]
/// [reserved 32]`.
mod attestation_offsets {
    pub const BUILD_HASH: usize = 8 + 1 + 1 + 32;
    pub const SIM_PASS: usize = BUILD_HASH + 32 + 32 + 20 + 2;
    pub const CUT_MAX_BPS: usize = SIM_PASS + 1;
    pub const PROGRAMDATA_SLOT: usize = CUT_MAX_BPS + 2 + 2 + 1 + 1;
    pub const REVOKED: usize = PROGRAMDATA_SLOT + 8 + 8 + 32;
    pub const LEN: usize = REVOKED + 1 + 8 + 32;
}

/// The unaudited pot cap (`DEFAULT_POT_CAP`).
pub const DEFAULT_POT_CAP: u64 = 10_000_000_000;

fn hex32(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn u64_at(d: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(d[at..at + 8].try_into().unwrap())
}

/// `secs` since the epoch as `YYYY-MM-DDTHH:MM:SSZ` (UTC).
pub fn iso_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil from days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// "N days" (or hours, for a delay that is not a whole number of days).
pub fn delay_words(secs: u32) -> String {
    if secs.is_multiple_of(86_400) {
        let d = secs / 86_400;
        format!("{d} day{}", if d == 1 { "" } else { "s" })
    } else {
        let h = secs / 3_600;
        format!("{h} hour{}", if h == 1 { "" } else { "s" })
    }
}

fn info<'a>(key: &'a Pubkey, raw: &'a mut RawAccount, lamports: &'a mut u64) -> AccountInfo<'a> {
    AccountInfo::new(
        key,
        false,
        false,
        lamports,
        &mut raw.data,
        &raw.owner,
        raw.executable,
    )
}

/// The label of `a.program_id` at `now`, `executable_hash` being its code's hash when the caller
/// computed it (needed to tell an audit or an attestation current once the ProgramData's slot
/// moved).
pub fn risk_label_of(a: &RiskAccounts, now: i64, executable_hash: Option<[u8; 32]>) -> RiskLabel {
    let mut label = RiskLabel {
        class: "missing",
        delay_secs: None,
        author: None,
        pending: None,
        timelock_program_upgradeable: None,
        audited: "false",
        blocked: false,
        pot_cap: Some(DEFAULT_POT_CAP),
        provenance: "none",
        studio: None,
        severity: "high",
        words: String::new(),
    };
    // The companion's status.
    let mut audited_hash = None;
    if let Some(st) = a
        .status
        .as_ref()
        .filter(|s| s.data.len() >= status_offsets::LEN)
    {
        use status_offsets as o;
        let audited = st.data[o::AUDITED] != 0;
        label.blocked = st.data[o::BLOCKED] != 0;
        if audited {
            label.pot_cap = None;
            let mut h = [0u8; 32];
            h.copy_from_slice(&st.data[o::AUDITED_HASH..o::AUDITED_HASH + 32]);
            audited_hash = Some(h);
        } else {
            label.pot_cap = Some(u64_at(&st.data, o::POT_CAP).min(DEFAULT_POT_CAP));
        }
    }
    // The program and its class.
    let class = match a.program.as_ref().filter(|p| p.executable) {
        None => None,
        Some(p) => {
            let (mut p, mut pd) = (p.clone(), a.programdata.clone());
            let mut t = a.timelock.clone();
            let (mut l1, mut l2, mut l3) = (1u64, 1u64, 1u64);
            let pd_key = bordrless_hook::authority::programdata_address(&a.program_id);
            let tl_key = bordrless_hook::authority::timelock_address(&a.program_id).0;
            let program_info = info(&a.program_id, &mut p, &mut l1);
            let mut empty = RawAccount {
                owner: Pubkey::default(),
                data: vec![],
                executable: false,
            };
            let pd_info = match pd.as_mut() {
                Some(raw) => info(&pd_key, raw, &mut l2),
                None => info(&pd_key, &mut empty, &mut l2),
            };
            let t_info = t.as_mut().map(|raw| info(&tl_key, raw, &mut l3));
            Some(classify(&program_info, &pd_info, t_info.as_ref()))
        }
    };
    // An upgradeable-loader program whose ProgramData is gone was closed: it can't run, and
    // nobody can ever change it (round 1, F-S2).
    let closed = a
        .program
        .as_ref()
        .is_some_and(|p| p.owner == BPF_LOADER_UPGRADEABLE_ID)
        && a.programdata.is_none();
    if class.is_none() || closed {
        label.words = "This hook's program is gone: every transfer fails.".to_string();
        return label;
    }
    // Its upgrade authority handed to its timelock's address with no `Timelock` there (anyone's
    // `SetAuthority` needs no consent of the new key): nobody can sign for it today (round 1,
    // F-S3), but nothing Bordrless runs takes it (independent audit, finding 8): see below.
    let frozen = matches!(
        class,
        Some(Err(ClassError::TimelockMissing)) | Some(Err(ClassError::TimelockInvalid))
    ) && a
        .timelock
        .as_ref()
        .is_none_or(|t| t.owner != HOOK_TIMELOCK_ID);
    // Such a program is not frozen as far as Bordrless is concerned: the launchpad refuses it
    // (`HookTimelockInvalid`) and the companion can't class it (independent audit, finding 8), so
    // it reads as the class nobody accepts, `author` (high), with its own words.
    let class = match class {
        None => unreachable!("handled above"),
        Some(Ok(c)) => c,
        // A ProgramData that is not the program's, or a forged or missing timelock: nobody can
        // vouch for who upgrades it.
        Some(Err(ClassError::WrongProgramData))
        | Some(Err(ClassError::TimelockMissing))
        | Some(Err(ClassError::TimelockInvalid)) => AuthorityClass::Author(None),
    };
    label.class = match class {
        AuthorityClass::Immutable => "immutable",
        AuthorityClass::Timelocked { delay_secs } => {
            label.delay_secs = Some(delay_secs);
            "timelocked"
        }
        AuthorityClass::Protocol(_) => "managed",
        AuthorityClass::Author(_) => "author",
    };
    if let AuthorityClass::Timelocked { .. } = class {
        if let Some(view) = a.timelock.as_ref().and_then(|t| parse_timelock(&t.data)) {
            label.author = Some(view.author);
            label.pending = view.pending.map(|p| (hex32(&p.hash), p.eta, p.buffer));
        }
        label.timelock_program_upgradeable = Some(
            a.timelock_programdata
                .as_ref()
                .filter(|pd| pd.owner == BPF_LOADER_UPGRADEABLE_ID)
                .and_then(|pd| parse_programdata(&pd.data))
                .is_none_or(|(_, authority)| authority.is_some()),
        );
    }
    // The audit: current only while its hash is the code's.
    if let Some(h) = audited_hash {
        // As the companion counts it: only of code that can't change behind it (immutable, or
        // Bordrless's to change); a timelocked or author-upgradeable program's audit is stale.
        let fixed = matches!(
            class,
            AuthorityClass::Immutable | AuthorityClass::Protocol(_)
        );
        label.audited = if fixed && h != [0; 32] && executable_hash == Some(h) {
            "current"
        } else {
            "stale"
        };
    }
    // Studio's attestation: current while not revoked, passing, and of the code as it is.
    let pd_slot = a
        .programdata
        .as_ref()
        .filter(|pd| pd.owner == BPF_LOADER_UPGRADEABLE_ID)
        .and_then(|pd| parse_programdata(&pd.data))
        .map(|(slot, _)| slot);
    let mut attested = false;
    if let Some(at) = a
        .attestation
        .as_ref()
        .filter(|x| x.data.len() >= attestation_offsets::LEN)
    {
        use attestation_offsets as o;
        let d = &at.data;
        let build: [u8; 32] = d[o::BUILD_HASH..o::BUILD_HASH + 32].try_into().unwrap();
        let sim_pass = d[o::SIM_PASS] != 0;
        let cut = u16::from_le_bytes([d[o::CUT_MAX_BPS], d[o::CUT_MAX_BPS + 1]]);
        let slot = u64_at(d, o::PROGRAMDATA_SLOT);
        let revoked = d[o::REVOKED] != 0;
        // As the companion reads it: other code proposed in its timelock makes it not current.
        let other_code_pending = label
            .pending
            .as_ref()
            .is_some_and(|(hash, _, _)| *hash != hex32(&build));
        let current = !revoked
            && sim_pass
            && !other_code_pending
            && (pd_slot == Some(slot) || executable_hash == Some(build));
        attested = current;
        label.studio = Some((hex32(&build), sim_pass, cut, current));
    }
    label.provenance = if attested {
        "studio"
    } else if BORDRLESS_HOOKS.contains(&a.program_id) {
        "protocol"
    } else if matches!(class, AuthorityClass::Protocol(_)) {
        "unattested"
    } else {
        "none"
    };
    // Precedence (§7.2).
    let (severity, words) = if label.blocked {
        (
            "high",
            "Bordrless blocked this game's code: its pot goes to buyback and burn.".to_string(),
        )
    } else if frozen {
        (
            "high",
            "Bordrless refuses this hook: its upgrade key was handed to a timelock address that was \
             never set up."
                .to_string(),
        )
    } else if label.class == "author" {
        (
            "high",
            "Its owner can change this code at any time.".to_string(),
        )
    } else if let Some((hash, eta, _)) = label
        .pending
        .as_ref()
        .filter(|_| label.class == "timelocked")
    {
        let when = if now >= *eta {
            "executable now".to_string()
        } else {
            format!("live from {}", iso_utc(*eta))
        };
        (
            "high",
            format!(
                "Its author has proposed new code (hash {}…), {when}.",
                &hash[..8]
            ),
        )
    } else if label.audited == "current" {
        ("low", "Audited.".to_string())
    } else if label.class == "timelocked" {
        // Independent audit X5: a notice period says nothing of what the code does (it can read a
        // switch its author sets), so the words say whether anyone checked it, as for immutable code.
        let mut w = format!(
            "Its author can change this code with {} of public notice. {}",
            delay_words(label.delay_secs.unwrap_or(0)),
            if attested {
                "Checked automatically by Studio, not audited."
            } else {
                "Bordrless hasn't checked it."
            }
        );
        if label.timelock_program_upgradeable == Some(true) {
            w.push_str(" The timelock itself can still be changed by Bordrless.");
        }
        ("medium", w)
    } else if label.class == "managed" {
        match label.provenance {
            "studio" => (
                "medium",
                "Built and checked automatically by Bordrless Studio, not audited. Bordrless can \
                 change it."
                    .to_string(),
            ),
            "protocol" => (
                "medium",
                "Bordrless's own code, not audited here. Bordrless can change it.".to_string(),
            ),
            _ => (
                "medium",
                "Upgradeable by a Bordrless key; not built by Studio.".to_string(),
            ),
        }
    } else if attested {
        (
            "medium",
            "Nobody can change this code. Checked automatically by Studio, not audited."
                .to_string(),
        )
    } else {
        (
            "medium",
            "Nobody can change this code. Bordrless hasn't checked it.".to_string(),
        )
    };
    label.severity = severity;
    label.words = words;
    let _ = HOOK_TIMELOCK_ID;
    label
}
