//! Accounts of the token standard.

use anchor_lang::prelude::*;

use crate::constants::*;

/// A mint. Address: any signer at creation (a keypair, or a PDA of the creating program).
#[account]
#[derive(InitSpace, Debug)]
pub struct Mint {
    /// Layout version.
    pub version: u8,
    /// Decimals.
    pub decimals: u8,
    /// Current supply.
    pub supply: u64,
    /// Largest supply ever allowed; 0 means unlimited.
    pub max_supply: u64,
    /// May mint; `None` once revoked.
    pub mint_authority: Option<Pubkey>,
    /// May freeze and thaw holdings; `None` once revoked.
    pub freeze_authority: Option<Pubkey>,
    /// May change the hook; `None` locks the hook for ever.
    pub hook_authority: Option<Pubkey>,
    /// May change the metadata; `None` fixes it for ever.
    pub metadata_authority: Option<Pubkey>,
    /// The hook program, if any.
    pub hook_program: Option<Pubkey>,
    /// Which callbacks run (`bordrless_hook::token_flags`).
    pub hook_flags: u16,
    /// Name.
    #[max_len(32)]
    pub name: String,
    /// Symbol.
    #[max_len(10)]
    pub symbol: String,
    /// Metadata URI.
    #[max_len(200)]
    pub uri: String,
    /// Creation time.
    pub created_at: i64,
    /// Who paid for the creation.
    pub creator: Pubkey,
    /// Bump of `["hook-authority", hook_program]`, this program's signer of every callback to the
    /// hook (one per hook program, so a hook can tell its own callbacks from a signer another hook
    /// passed on); 0 without a hook. Set with the hook.
    pub hook_signer_bump: u8,
    /// Reserved.
    pub reserved: [u8; 31],
}

impl Mint {
    /// Account size.
    pub const LEN: usize = DISCRIMINATOR_LEN + Self::INIT_SPACE;

    /// The hook, if one is set.
    pub fn hook(&self) -> Option<(Pubkey, u16)> {
        self.hook_program.map(|p| (p, self.hook_flags))
    }

    /// This program's signer of the callbacks to `hook_program`, `["hook-authority",
    /// hook_program]`, and its bump (a search: for creating a mint or setting its hook).
    pub fn hook_signer(hook_program: &Pubkey) -> (Pubkey, u8) {
        bordrless_hook::hook_signer(&crate::ID, hook_program)
    }

    /// Whether the mint's hook keeps hook data in its holdings (a hook with `WRITES_HOOK_DATA`).
    pub fn hook_writes_data(&self) -> bool {
        self.hook_program.is_some()
            && self.hook_flags & bordrless_hook::token_flags::WRITES_HOOK_DATA != 0
    }
}

/// A holding: the token account of `owner` for `mint`, at `["holding", mint, owner]`.
#[account]
#[derive(InitSpace, Debug)]
pub struct Holding {
    /// Layout version.
    pub version: u8,
    /// Bump of the PDA.
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
    /// State the mint's hook keeps for this holder (`bordrless_hook::HOOK_DATA_LEN` bytes). Only
    /// the mint's hook program changes it: by answering a `before_*` callback, or through
    /// `write_hook_data`, and only while the mint has `WRITES_HOOK_DATA`. Zero until then.
    pub hook_data: [u8; 64],
    /// Reserved.
    pub reserved: [u8; 16],
}

impl Holding {
    /// Account size.
    pub const LEN: usize = DISCRIMINATOR_LEN + Self::INIT_SPACE;

    /// Whether the hook data is all zero (nothing kept for this holder).
    pub fn hook_data_is_empty(&self) -> bool {
        self.hook_data.iter().all(|b| *b == 0)
    }

    /// The holding address of `owner` for `mint`.
    pub fn address(mint: &Pubkey, owner: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[HOLDING_SEED, mint.as_ref(), owner.as_ref()], &crate::ID)
    }
}

/// Which authority `set_authority` replaces.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthorityKind {
    /// `mint_authority`.
    Mint,
    /// `freeze_authority`.
    Freeze,
    /// `hook_authority`.
    Hook,
    /// `metadata_authority`.
    Metadata,
}
