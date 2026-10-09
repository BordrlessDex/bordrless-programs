//! The randomness oracle: ORAO VRF (classic), `VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y`.
//! Everything the companion knows about it is in this file, so another oracle changes only this
//! file.
//!
//! ORAO's crate pins Anchor 0.32, so nothing here depends on it: the request is built by hand and
//! ORAO's accounts are read at fixed offsets. Layouts from orao-solana-vrf 0.7.0 (`src/lib.rs`,
//! `src/state/network_state.rs`, `src/state/randomness_v2.rs`), checked against mainnet accounts
//! and transactions on 2026-10-08.
//!
//! - `request_v2(seed)` creates the request account `PDA(["orao-vrf-randomness-request", seed])`
//!   (749 bytes) with the payer's lamports and sends `request_fee` lamports from the payer to the
//!   treasury: two system-program calls from the payer, which must therefore sign and be
//!   system-owned with no data. A PDA signing through `invoke_signed` qualifies. ORAO records the
//!   payer as the request's `client`.
//! - Each of ORAO's fulfilment authorities signs the seed (ed25519). Once a quorum has (2n/3 + 1
//!   of its n keys, rounded down: all 3 of today's 3), the randomness is the XOR of their
//!   signatures, the account shrinks to 137 bytes and the rent of the 612 bytes freed goes back to
//!   the client. On mainnet that took from 7 to 319 slots after the request (about 3 s to over 2
//!   minutes; 8 requests measured on 2026-10-09; one signer often lags by about 100 s), not the
//!   4 to 5 slots that 4 requests showed on 2026-10-08. Nobody ever gets the 137 bytes' rent back: ORAO has no instruction
//!   that closes a request account.
//! - Anyone may request any seed. A seed that already has a request account can't be requested
//!   again (the call fails), and a request someone else made names them as its client.
//!
//! Changed from the research draft (`orao.md` §7) after two facts checked on chain on 2026-10-08:
//!
//! - **ORAO's devnet answers with the mainnet signers' keys** (its network state lists the same three
//!   fulfilment authorities, plus a fourth), and their signatures of a seed are deterministic: anyone
//!   can learn what mainnet's randomness for a seed will be by requesting that seed on devnet. So
//!   nobody may learn the answer to a draw's seed while it can still be chosen whether the draw goes
//!   ahead: `draw` commits its seed and makes its request (or adopts one) in one instruction, so a
//!   seed is never on chain without its request. The client must know the seed to pass the
//!   request's account, so the seed is made from the hash of a slot the draw names, one of the last
//!   [`SEED_SLOTS`] slots when it lands ([`recent_slot_hash`], [`draw_seed`]). Nobody knows a slot's
//!   hash before the slot is done, and nobody can learn the answer to its seed on devnet before the
//!   slot is too old to draw: a preview needs a devnet request to land, then the three mainnet
//!   signers' responses on devnet, the last of which came 7 to 17 slots after the request (8
//!   devnet requests measured on 2026-10-09, read-only; the first response came 3 to 6 slots
//!   after). So a preview arrives at least 8 slots after the seed's slot, where a draw must land
//!   within 3 (about 5 should the seed slot's leader hold its block for the next leader's grace
//!   ticks): a margin of about 3 to 5 slots, from a small sample. A seed from a slot of
//!   the client's choosing, an older slot, or a nonce it picks could be ground against devnet's
//!   answers; a seed read from the parent slot when the draw lands could not be requested in the
//!   same instruction, and would stay public until a later one, whose sender could preview its
//!   answer and choose whether to request it at all.
//! - **ORAO's deprecated v1 `request` still works**, and makes its (legacy) account at the very
//!   address a v2 request for the same seed would use; ORAO still answers it (all 29,672 v1 accounts
//!   on mainnet are fulfilled). Anyone can compute the seeds of the last few slots and request one
//!   before a draw lands with it. So the draw adopts whatever pending request ORAO holds for its
//!   seed: a v2 request whoever paid for it, or a v1 one ([`randomness`]). Either way it is ORAO's
//!   answer to that seed alone (a v1 answer equals the v2 one: the XOR of the same authorities'
//!   signatures, checked on 20 mainnet accounts), made blind (nobody knew the answer when it was
//!   requested), and nobody can block a draw, or get it a new seed, by requesting its seed first.
//!
//! What this leaves to ORAO, by design:
//!
//! - **Its answer's timing.** ORAO's fulfilment carries no priority fee and only ORAO can send it,
//!   so anyone can keep it out of the blocks for a while (stuffing the request's write lock). A
//!   round's seed is therefore final: a late answer still decides the draw until its claims end,
//!   and a censor can only delay it or, kept up through the rest of the round, roll it over.
//! - **Answering v1.** A request someone else made with the deprecated v1 `request` is adopted, so
//!   the round waits on ORAO answering v1 (it answers every one today). Were ORAO to stop, each
//!   such squat would cost its round (a rollover at the claims' end, no new seed: a new seed would
//!   let the squatter, who can preview answers on devnet, pick among draws), and a game whose pot
//!   pays nothing for long is retired to the buyback (`retire`). Refusing v1 instead would make
//!   every squat a sure rollover today. Keepers should watch for v1 squats.
//! - **Its layouts, fee and program.** A layout changed under the same type name fails closed (the
//!   lengths are bound), and a fee above `MAX_REQUEST_FEE` is refused. Either way the pot can't pay
//!   for a request, so each draw rolls over with no seed committed (`OracleUnpaid`). The pot is
//!   never paid from, and is retired after two dormant periods. A change to `request_v2` that made
//!   the pot's own request fail fails every draw with it: no seed is committed either.
//! - **Its speed.** A draw's seed can be computed by anyone for up to `SEED_SLOTS` slots before
//!   the draw lands. Were ORAO ever to answer within that, a client could preview the seeds of the
//!   last few slots and draw only from one that wins: `SEED_SLOTS` must stay below ORAO's latency
//!   on devnet, the preview path (7 slots or more in the 8 requests measured on 2026-10-09; keepers
//!   should alert should it ever drop below about 5). A seed ORAO has already answered on mainnet
//!   is refused (`StaleSeed`), so a fast answer on chain can't be drawn. ORAO's callback VRF signs
//!   with the same keys but a 64-byte message, not the seed, and answers on devnet thousands of
//!   slots late: no faster preview. No other cluster checked (testnet, Eclipse, Sonic, SOON, Fogo)
//!   has ORAO's network state at its address.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};

use crate::error::CompanionError;

/// The ORAO VRF program (classic; not the callback variant `VRFCBe…`).
pub const ORAO_VRF_ID: Pubkey =
    Pubkey::from_str_const("VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y");
/// ORAO's configuration, `PDA(["orao-vrf-network-configuration"], ORAO_VRF_ID)`: the fee, the
/// treasury and the fulfilment authorities.
pub const NETWORK_STATE: Pubkey =
    Pubkey::from_str_const("5ER1oENnV4srxYdAynUfRzWeQCPQaqMiAp4VqyMbSqnK");
pub const NETWORK_STATE_SEED: &[u8] = b"orao-vrf-network-configuration";
/// A request account is `PDA([RANDOMNESS_SEED, seed], ORAO_VRF_ID)`.
pub const RANDOMNESS_SEED: &[u8] = b"orao-vrf-randomness-request";
/// The slot hashes sysvar: the most recent slots' hashes, the parent slot's first.
pub const SLOT_HASHES: Pubkey =
    Pubkey::from_str_const("SysvarS1otHashes111111111111111111111111111");
/// The owner of every sysvar.
pub const SYSVAR_OWNER: Pubkey =
    Pubkey::from_str_const("Sysvar1111111111111111111111111111111111111");

/// A pending request account: 8 + 1 + 32 + 32 + 4 + 7 * (32 + 64) bytes. Its payer funds the rent.
pub const PENDING_LEN: usize = 749;
/// A fulfilled one: 8 + 1 + 32 + 32 + 64 bytes (ORAO's last `fulfill_v2` reallocs it to this).
pub const FULFILLED_LEN: usize = 137;
/// A legacy v1 request, pending or answered (never reallocated): 8 + 32 + 64 + 4 + 7 * (32 + 64)
/// bytes. All 29,672 v1 accounts on mainnet were this long on 2026-10-08, and all 92,215 v2 ones
/// `FULFILLED_LEN`.
pub const V1_LEN: usize = 780;
/// The most one request may cost in fees, whatever ORAO's configuration says. It was 0.0005 SOL on
/// 2026-10-08, and ORAO's authority can change it at any time: above this, each draw rolls over
/// without committing its seed (`RolloverReason::OracleUnpaid`).
pub const MAX_REQUEST_FEE: u64 = 5_000_000;

/// `sha256("global:request_v2")[..8]`.
const REQUEST_V2: [u8; 8] = [38, 151, 209, 6, 195, 102, 28, 217];
/// `sha256("account:NetworkState")[..8]`.
const NETWORK_STATE_DISCRIMINATOR: [u8; 8] = [212, 237, 148, 56, 97, 245, 51, 169];
/// `sha256("account:RandomnessV2")[..8]`.
const RANDOMNESS_V2_DISCRIMINATOR: [u8; 8] = [139, 239, 184, 215, 227, 86, 191, 226];
/// `sha256("account:Randomness")[..8]`: the legacy v1 request (`bc60d8f85d5e3170`).
const RANDOMNESS_V1_DISCRIMINATOR: [u8; 8] = [188, 96, 216, 248, 93, 94, 49, 112];

// NetworkState (borsh): discriminator, authority, treasury, request_fee: u64, then fields of
// variable length (the fulfilment authorities, the token fee, the request count).
const TREASURY_AT: usize = 40;
const FEE_AT: usize = 72;

// RandomnessV2 (borsh): discriminator, the variant (0 pending, 1 fulfilled), client, seed, then
// for a pending request the responses so far (a Vec), for a fulfilled one the 64 random bytes.
const VARIANT_AT: usize = 8;
const SEED_AT: usize = 41;
const RANDOMNESS_AT: usize = 73;
const PENDING: u8 = 0;
const FULFILLED: u8 = 1;

// Randomness (v1, legacy; borsh): discriminator, the seed, the 64 random bytes (all zero while
// pending), then the responses so far. Checked against 200 mainnet accounts, each at
// `request_address(seed)`.
const V1_SEED_AT: usize = 8;
const V1_RANDOMNESS_AT: usize = 40;

// SlotHashes (bincode): the number of entries (u64), then each entry's slot (u64) and hash, the
// newest (the parent slot's) first.
const SLOT_HASHES_FIRST_SLOT_AT: usize = 8;
const SLOT_HASH_LEN: usize = 40;

/// `N` bytes of `data` at `at`, or `OracleAccount` when the account is too short.
fn bytes<const N: usize>(data: &[u8], at: usize) -> Result<[u8; N]> {
    data.get(at..at + N)
        .and_then(|s| <[u8; N]>::try_from(s).ok())
        .ok_or_else(|| error!(CompanionError::OracleAccount))
}

/// How old, in slots, the slot a draw's seed is made from may be when the draw lands: the parent
/// slot (1) up to 3. Long enough for a keeper to read the slot hashes sysvar through RPC and land
/// its draw; short enough that nobody can learn the answer to the slot's seed from ORAO's devnet
/// before the slot is too old: a devnet request must land, then the three mainnet signers answer
/// it, the last 7 to 17 slots after the request in the 8 devnet requests measured on 2026-10-09
/// (read-only), so a preview arrives 8 slots or more after the seed's slot.
pub const SEED_SLOTS: u64 = 3;

/// The seed of the `n`-th seed of round `round`'s draw, made from slot `slot` and its hash
/// `slot_hash` ([`recent_slot_hash`]): nobody knows it before that slot is done, and no client
/// chooses any other part of it. Never zero, as ORAO requires: it is a sha256.
pub fn draw_seed(mint: &Pubkey, round: u32, n: u32, slot: u64, slot_hash: &[u8; 32]) -> [u8; 32] {
    solana_sha256_hasher::hashv(&[
        b"bordrless-draw",
        mint.as_ref(),
        &round.to_le_bytes(),
        &n.to_le_bytes(),
        &slot.to_le_bytes(),
        slot_hash,
    ])
    .to_bytes()
}

/// The hash of `slot`, one of the last [`SEED_SLOTS`] slots before `current` (the slot that runs
/// the draw), from the slot hashes sysvar (address and owner checked; read in place, it is 20 KB):
/// `StaleSeed` when `slot` is older, not before `current`, or not in the sysvar (skipped).
pub fn recent_slot_hash(slot_hashes: &AccountInfo, slot: u64, current: u64) -> Result<[u8; 32]> {
    require_keys_eq!(*slot_hashes.key, SLOT_HASHES, CompanionError::OracleAccount);
    require_keys_eq!(
        *slot_hashes.owner,
        SYSVAR_OWNER,
        CompanionError::OracleAccount
    );
    require!(
        slot < current && current - slot <= SEED_SLOTS,
        CompanionError::StaleSeed
    );
    let data = slot_hashes.try_borrow_data()?;
    let entries = u64::from_le_bytes(bytes::<8>(&data, 0)?);
    // The newest first, one entry per slot that has a block: `slot` is among the first
    // `SEED_SLOTS` if it is there at all.
    for i in 0..entries.min(SEED_SLOTS) as usize {
        let at = SLOT_HASHES_FIRST_SLOT_AT + i * SLOT_HASH_LEN;
        let s = u64::from_le_bytes(bytes::<8>(&data, at)?);
        if s == slot {
            return bytes::<32>(&data, at + 8);
        }
        if s < slot {
            break;
        }
    }
    err!(CompanionError::StaleSeed)
}

/// The request account for `seed`.
pub fn request_address(seed: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(&[RANDOMNESS_SEED, seed], &ORAO_VRF_ID).0
}

/// What ORAO charges now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Terms {
    /// Where the fee goes. ORAO refuses a request naming another treasury.
    pub treasury: Pubkey,
    /// Lamports per request.
    pub fee: u64,
}

impl Terms {
    /// The lamports a request takes from its payer: the fee and the pending account's rent.
    pub fn cost(&self) -> Result<u64> {
        Rent::get()?
            .minimum_balance(PENDING_LEN)
            .checked_add(self.fee)
            .ok_or_else(|| error!(CompanionError::MathOverflow))
    }
}

/// ORAO's terms, from its network state (address, owner and discriminator checked). Refused while
/// the fee is above `MAX_REQUEST_FEE`.
pub fn terms(network_state: &AccountInfo) -> Result<Terms> {
    require_keys_eq!(
        *network_state.key,
        NETWORK_STATE,
        CompanionError::OracleAccount
    );
    require_keys_eq!(
        *network_state.owner,
        ORAO_VRF_ID,
        CompanionError::OracleAccount
    );
    let data = network_state.try_borrow_data()?;
    require!(
        bytes::<8>(&data, 0)? == NETWORK_STATE_DISCRIMINATOR,
        CompanionError::OracleAccount
    );
    let treasury = Pubkey::new_from_array(bytes::<32>(&data, TREASURY_AT)?);
    let fee = u64::from_le_bytes(bytes::<8>(&data, FEE_AT)?);
    require!(fee <= MAX_REQUEST_FEE, CompanionError::OracleFeeTooHigh);
    Ok(Terms { treasury, fee })
}

/// What a payer holding `payer_lamports` needs added before a request costing `cost`, so that it
/// still holds its own rent-exempt minimum after paying. A system account left with less than
/// that minimum but more than zero fails the whole transaction.
pub fn top_up(payer_lamports: u64, cost: u64) -> Result<u64> {
    let need = cost
        .checked_add(Rent::get()?.minimum_balance(0))
        .ok_or_else(|| error!(CompanionError::MathOverflow))?;
    Ok(need.saturating_sub(payer_lamports))
}

/// `request_v2(seed)`, `payer` paying `terms.cost()`. The payer must sign (`invoke_signed` with
/// its seeds) and be system-owned with no data. The caller passes the five accounts and the ORAO
/// program.
pub fn request_ix(payer: Pubkey, terms: &Terms, seed: [u8; 32]) -> Instruction {
    let mut data = Vec::with_capacity(40);
    data.extend_from_slice(&REQUEST_V2);
    data.extend_from_slice(&seed);
    Instruction {
        program_id: ORAO_VRF_ID,
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(NETWORK_STATE, false),
            AccountMeta::new(terms.treasury, false),
            AccountMeta::new(request_address(&seed), false),
            AccountMeta::new_readonly(anchor_lang::system_program::ID, false),
        ],
        data,
    }
}

/// Whether ORAO holds an account at `request` (anyone's request, v2 or v1; checked by
/// [`randomness`]). An address with no data (none yet, or only lamports someone sent) is free for
/// a new request.
pub fn is_requested(request: &AccountInfo) -> bool {
    *request.owner == ORAO_VRF_ID && !request.data_is_empty()
}

/// The randomness of ORAO's request for `seed`, or `None` while ORAO has not answered. Refused
/// unless it is ORAO's request for exactly that seed: owned by ORAO, at `request_address(seed)`,
/// a v2 request (whoever paid for it) or a legacy v1 one, naming that seed. Who made the request
/// does not matter: the randomness is ORAO's signers' answer to the seed alone (see the module's
/// notes).
///
/// Each layout is bound by its exact length as well as its discriminator and variant (a pending
/// v2 request `PENDING_LEN`, a fulfilled one `FULFILLED_LEN`, a v1 one `V1_LEN`), so a layout ORAO
/// changes under the same type name (a field moved before the randomness) fails closed
/// (`OracleAccount`) instead of being read at the old offsets.
pub fn randomness(request: &AccountInfo, seed: &[u8; 32]) -> Result<Option<[u8; 64]>> {
    require_keys_eq!(
        *request.key,
        request_address(seed),
        CompanionError::OracleAccount
    );
    require_keys_eq!(*request.owner, ORAO_VRF_ID, CompanionError::OracleAccount);
    let data = request.try_borrow_data()?;
    let discriminator = bytes::<8>(&data, 0)?;
    if discriminator == RANDOMNESS_V1_DISCRIMINATOR {
        require!(data.len() == V1_LEN, CompanionError::OracleAccount);
        require!(
            bytes::<32>(&data, V1_SEED_AT)? == *seed,
            CompanionError::OracleAccount
        );
        let r = bytes::<64>(&data, V1_RANDOMNESS_AT)?;
        // All zeroes: not answered yet.
        return Ok((r != [0u8; 64]).then_some(r));
    }
    require!(
        discriminator == RANDOMNESS_V2_DISCRIMINATOR,
        CompanionError::OracleAccount
    );
    require!(
        bytes::<32>(&data, SEED_AT)? == *seed,
        CompanionError::OracleAccount
    );
    match bytes::<1>(&data, VARIANT_AT)?[0] {
        PENDING => {
            require!(data.len() == PENDING_LEN, CompanionError::OracleAccount);
            Ok(None)
        }
        FULFILLED => {
            require!(data.len() == FULFILLED_LEN, CompanionError::OracleAccount);
            let r = bytes::<64>(&data, RANDOMNESS_AT)?;
            // ORAO's own SDK reads all zeroes as "not fulfilled".
            require!(r != [0u8; 64], CompanionError::OracleAccount);
            Ok(Some(r))
        }
        _ => err!(CompanionError::OracleAccount),
    }
}
