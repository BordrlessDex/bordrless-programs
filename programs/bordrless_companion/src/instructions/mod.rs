pub mod approval;
pub mod attest;
pub mod create;
pub mod game;
pub mod kinds;
pub mod native;
pub mod payees;
pub mod payout;
pub mod script;
pub mod snowball;
pub mod steps;
pub mod strategy;

pub use approval::*;
pub use attest::*;
pub use create::*;
pub use game::*;
pub use kinds::*;
pub use native::*;
pub use payees::*;
pub use payout::*;
pub use script::*;
pub use snowball::*;
pub use steps::*;
pub use strategy::*;

use anchor_lang::prelude::Pubkey;

use crate::constants::CREATOR_SEED;

/// The creator address's signer seeds, `["creator", mint, bump]`.
pub struct CreatorSeeds {
    mint: Pubkey,
    bump: [u8; 1],
    /// The companion is native: its SOL is the creator address's lamports, not bridged SOL.
    pub native: bool,
}

impl CreatorSeeds {
    pub fn new(mint: Pubkey, bump: u8) -> Self {
        Self {
            mint,
            bump: [bump],
            native: false,
        }
    }

    /// The seeds of `c`'s creator address, with its native flag.
    pub fn of(c: &crate::state::Companion) -> Self {
        Self {
            mint: c.mint,
            bump: [c.creator_bump],
            native: c.is_native(),
        }
    }

    pub fn seeds(&self) -> [&[u8]; 3] {
        [CREATOR_SEED, self.mint.as_ref(), &self.bump]
    }
}
