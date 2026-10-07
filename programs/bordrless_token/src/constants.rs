//! Constants of the token standard.

/// Seed of a holding: `["holding", mint, owner]`.
pub const HOLDING_SEED: &[u8] = b"holding";
/// Layout version written into new accounts.
pub const VERSION: u8 = 1;
/// Longest name.
pub const MAX_NAME: usize = 32;
/// Longest symbol.
pub const MAX_SYMBOL: usize = 10;
/// Longest metadata URI.
pub const MAX_URI: usize = 200;
/// Most decimals a mint may have.
pub const MAX_DECIMALS: u8 = 12;
/// Discriminator length.
pub const DISCRIMINATOR_LEN: usize = 8;
