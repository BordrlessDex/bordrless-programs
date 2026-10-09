//! A token holding as a game hook reads it (its `enter` registers a holding that has not traded):
//! the token program's `Holding` account, read without the token program's crate.

use anchor_lang::prelude::*;

use crate::cpi::TOKEN_PROGRAM_ID;
use crate::HOOK_DATA_LEN;

/// Anchor's discriminator of the token program's `Holding`: `sha256("account:Holding")[..8]`.
pub const HOLDING_DISCRIMINATOR: [u8; 8] = [23, 96, 64, 250, 235, 191, 0, 144];

/// The token program's `Holding`, field for field (Borsh), after its discriminator.
#[derive(AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct HoldingView {
    /// Layout version.
    pub version: u8,
    /// Bump of the holding's PDA.
    pub bump: u8,
    /// The mint.
    pub mint: Pubkey,
    /// The owner.
    pub owner: Pubkey,
    /// Balance.
    pub amount: u64,
    /// Delegate, if any.
    pub delegate: Option<Pubkey>,
    /// What the delegate may still move.
    pub delegated_amount: u64,
    /// Frozen holdings cannot send, receive or burn.
    pub frozen: bool,
    /// The hook data (the game's slots).
    pub hook_data: [u8; HOOK_DATA_LEN],
}

/// Reads a holding from its account data: the token program's discriminator, then its fields.
/// `None` for anything else; never panics.
pub fn parse_holding(data: &[u8]) -> Option<HoldingView> {
    if data.len() < 8 || data[..8] != HOLDING_DISCRIMINATOR {
        return None;
    }
    HoldingView::deserialize(&mut &data[8..]).ok()
}

/// [`parse_holding`] from the account, owned by the token program.
pub fn read_holding(info: &AccountInfo) -> Option<HoldingView> {
    if *info.owner != TOKEN_PROGRAM_ID {
        return None;
    }
    let data = info.try_borrow_data().ok()?;
    parse_holding(&data)
}
