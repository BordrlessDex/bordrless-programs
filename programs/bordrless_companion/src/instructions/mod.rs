pub mod create;
pub mod game;
pub mod kinds;
pub mod steps;

pub use create::*;
pub use game::*;
pub use kinds::*;
pub use steps::*;

use anchor_lang::prelude::Pubkey;

use crate::constants::CREATOR_SEED;

/// The creator address's signer seeds, `["creator", mint, bump]`.
pub struct CreatorSeeds {
    mint: Pubkey,
    bump: [u8; 1],
}

impl CreatorSeeds {
    pub fn new(mint: Pubkey, bump: u8) -> Self {
        Self { mint, bump: [bump] }
    }

    pub fn seeds(&self) -> [&[u8]; 3] {
        [CREATOR_SEED, self.mint.as_ref(), &self.bump]
    }
}
