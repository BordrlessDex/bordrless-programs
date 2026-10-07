//! Events of the token standard, emitted by self-CPI. Transfers, mints and burns carry the
//! post-balances of the holdings they touched, so an indexer keeps exact balances.

#![allow(missing_docs)]

use anchor_lang::prelude::*;

#[event]
pub struct MintCreated {
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub decimals: u8,
    pub max_supply: u64,
    pub mint_authority: Option<Pubkey>,
    pub freeze_authority: Option<Pubkey>,
    pub hook_authority: Option<Pubkey>,
    pub metadata_authority: Option<Pubkey>,
    pub hook_program: Option<Pubkey>,
    pub hook_flags: u16,
    pub name: String,
    pub symbol: String,
    pub uri: String,
    pub ts: i64,
}

#[event]
pub struct HoldingCreated {
    pub mint: Pubkey,
    pub holding: Pubkey,
    pub owner: Pubkey,
    pub payer: Pubkey,
    pub ts: i64,
}

/// One delta a `before_transfer` answer took: the holding it credited, that holding's owner, the
/// amount and the holding's balance after.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct DeltaApplied {
    pub holding: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
    pub post: u64,
}

/// A transfer. The source lost `amount`; the destination gained `amount` less the deltas; each
/// delta holding gained its amount. Hook data is not in events.
#[event]
pub struct Transferred {
    pub mint: Pubkey,
    pub source: Pubkey,
    pub destination: Pubkey,
    pub source_owner: Pubkey,
    pub destination_owner: Pubkey,
    pub authority: Pubkey,
    pub amount: u64,
    pub deltas: Vec<DeltaApplied>,
    pub source_post: u64,
    pub destination_post: u64,
    pub slot: u64,
    pub ts: i64,
}

/// The mint's hook program wrote a holding's hook data through `write_hook_data`.
#[event]
pub struct HookDataWritten {
    pub mint: Pubkey,
    pub holding: Pubkey,
    pub owner: Pubkey,
    pub data: [u8; 64],
}

#[event]
pub struct Minted {
    pub mint: Pubkey,
    pub destination: Pubkey,
    pub destination_owner: Pubkey,
    pub authority: Pubkey,
    pub amount: u64,
    pub destination_post: u64,
    pub supply_post: u64,
    pub slot: u64,
    pub ts: i64,
}

#[event]
pub struct Burned {
    pub mint: Pubkey,
    pub source: Pubkey,
    pub source_owner: Pubkey,
    pub authority: Pubkey,
    pub amount: u64,
    pub source_post: u64,
    pub supply_post: u64,
    pub slot: u64,
    pub ts: i64,
}

#[event]
pub struct HookSet {
    pub mint: Pubkey,
    pub hook_program: Option<Pubkey>,
    pub hook_flags: u16,
    pub ts: i64,
}

#[event]
pub struct AuthoritySet {
    pub mint: Pubkey,
    pub kind: u8,
    pub new_authority: Option<Pubkey>,
    pub ts: i64,
}

#[event]
pub struct MetadataUpdated {
    pub mint: Pubkey,
    pub name: String,
    pub symbol: String,
    pub uri: String,
    pub ts: i64,
}

#[event]
pub struct FrozenSet {
    pub mint: Pubkey,
    pub holding: Pubkey,
    pub frozen: bool,
    pub ts: i64,
}

#[event]
pub struct HoldingClosed {
    pub mint: Pubkey,
    pub holding: Pubkey,
    pub owner: Pubkey,
    pub ts: i64,
}

#[event]
pub struct DelegateSet {
    pub mint: Pubkey,
    pub holding: Pubkey,
    pub delegate: Option<Pubkey>,
    pub amount: u64,
    pub ts: i64,
}
