//! Accounts of the DEX.

use anchor_lang::prelude::*;

use crate::constants::*;

/// Program configuration at `["config"]`.
#[account]
#[derive(InitSpace, Debug)]
pub struct Config {
    /// Layout version.
    pub version: u8,
    /// Bump.
    pub bump: u8,
    /// May change the config and collect protocol fees.
    pub admin: Pubkey,
    /// Protocol fee of ordinary pools created from now on, in basis points (anyone's pool: no
    /// curve).
    pub protocol_fee_bps: u16,
    /// Whose holdings receive collected protocol fees.
    pub fee_collector: Pubkey,
    /// Receives pool creation fees (lamports).
    pub treasury: Pubkey,
    /// Lamports paid to create a pool.
    pub pool_creation_fee_lamports: u64,
    /// Stops swaps and liquidity changes.
    pub paused: bool,
    /// Pools created so far.
    pub pools_created: u64,
    /// Bordrless's share of what the hooks cut on each swap of a launch pool created from now on,
    /// in basis points of the cuts (2,500 = a quarter): the curve the launchpad creates as its hook
    /// (`FEE_MODEL_SHARE`), which keeps the model after graduation. A launch whose rules collect
    /// nothing pays nothing.
    pub launch_protocol_share_bps: u16,
    /// Reserved.
    pub reserved: [u8; 62],
}

impl Config {
    /// Account size.
    pub const LEN: usize = DISCRIMINATOR_LEN + Self::INIT_SPACE;
}

/// A pool at `["pool", base_mint, quote_mint, lp_fee_bps, hook_program or default]`.
#[account]
#[derive(InitSpace, Debug)]
pub struct Pool {
    /// Layout version.
    pub version: u8,
    /// Bump of the pool PDA.
    pub bump: u8,
    /// Bump of the LP mint PDA `["lp", pool]`.
    pub lp_mint_bump: u8,
    /// Base mint.
    pub base_mint: Pubkey,
    /// Quote mint.
    pub quote_mint: Pubkey,
    /// LP mint (a BTS mint whose authority is the pool).
    pub lp_mint: Pubkey,
    /// The pool's holding of the base mint.
    pub base_vault: Pubkey,
    /// The pool's holding of the quote mint.
    pub quote_vault: Pubkey,
    /// Hook program, if any.
    pub hook_program: Option<Pubkey>,
    /// Which callbacks run (`bordrless_hook::pool_flags`).
    pub hook_flags: u16,
    /// LP fee in basis points of the input; stays in the pool.
    pub lp_fee_bps: u16,
    /// Flat protocol fee in basis points (`FEE_MODEL_FLAT`), always charged in the quote token: of
    /// what reaches the vault on a buy, of the curve's output on a sell. Accrued for the fee
    /// collector. 0 under the share model.
    pub protocol_fee_bps: u16,
    /// Real base reserve (the base vault's balance).
    pub base_reserve: u64,
    /// Real quote reserve (the quote vault's balance minus accrued protocol fees).
    pub quote_reserve: u64,
    /// Virtual base offset used for pricing.
    pub virtual_base: u64,
    /// Virtual quote offset used for pricing.
    pub virtual_quote: u64,
    /// LP shares outstanding, including the minimum liquidity the pool keeps.
    pub lp_supply: u64,
    /// Protocol fees accrued (always in the quote token), sitting in the quote vault apart from
    /// the reserve until `collect_protocol_fees`.
    pub protocol_fees_quote: u64,
    /// A curve: virtual offsets live, no LP minted yet, liquidity locked until finalized.
    pub curve: bool,
    /// Who created the pool (the deposit authority).
    pub creator: Pubkey,
    /// Creation time.
    pub created_at: i64,
    /// Time of the last swap.
    pub last_swap_at: i64,
    /// Swaps so far.
    pub swap_count: u64,
    /// Base traded so far (both directions).
    pub base_volume: u128,
    /// Quote traded so far (both directions).
    pub quote_volume: u128,
    /// Bump of `["hook-authority", hook_program]`, this program's signer of every callback to the
    /// pool's hook (one per hook program, so a hook can tell its own callbacks from a signer
    /// another hook passed on); 0 without a hook.
    pub hook_signer_bump: u8,
    /// How the protocol fee is taken: `FEE_MODEL_FLAT` (a rate of the quote, ordinary pools) or
    /// `FEE_MODEL_SHARE` (a share of what the hooks cut, launch pools). Fixed at creation.
    pub fee_model: u8,
    /// Under `FEE_MODEL_SHARE`: Bordrless's share of what the hooks cut on each swap, in basis
    /// points of the cuts, taken in the quote (base-side cuts valued at the swap's own price). 0
    /// under the flat model.
    pub protocol_share_bps: u16,
    /// Reserved.
    pub reserved: [u8; 60],
}

impl Pool {
    /// Account size.
    pub const LEN: usize = DISCRIMINATOR_LEN + Self::INIT_SPACE;

    /// The hook program as a seed (default when there is none).
    pub fn hook_key(hook_program: Option<Pubkey>) -> Pubkey {
        hook_program.unwrap_or_default()
    }

    /// This program's signer of the callbacks to `hook_program`, `["hook-authority",
    /// hook_program]`, and its bump (a search: for creating a pool).
    pub fn hook_signer(hook_program: &Pubkey) -> (Pubkey, u8) {
        bordrless_hook::hook_signer(&crate::ID, hook_program)
    }

    /// The pool address.
    pub fn address(
        base_mint: &Pubkey,
        quote_mint: &Pubkey,
        lp_fee_bps: u16,
        hook_program: Option<Pubkey>,
    ) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[
                POOL_SEED,
                base_mint.as_ref(),
                quote_mint.as_ref(),
                &lp_fee_bps.to_le_bytes(),
                Self::hook_key(hook_program).as_ref(),
            ],
            &crate::ID,
        )
    }

    /// The LP mint address of `pool`.
    pub fn lp_mint_address(pool: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[LP_SEED, pool.as_ref()], &crate::ID)
    }

    /// Whether the hook runs `flag`.
    pub fn runs(&self, flag: u16) -> bool {
        self.hook_program.is_some() && self.hook_flags & flag != 0
    }

    /// Whether the protocol fee is a share of the hooks' cuts (a launch pool).
    pub fn shares_cuts(&self) -> bool {
        self.fee_model == FEE_MODEL_SHARE
    }

    /// Whether the pool holds its quote as lamports on its own account (a native pool). Its
    /// `quote_mint` stays bridged SOL, so its address and seeds never change.
    pub fn quote_native(&self) -> bool {
        self.reserved[NATIVE_FLAG_OFFSET] == 1
    }

    /// Sets or clears the native flag.
    pub fn set_quote_native(&mut self, native: bool) {
        self.reserved[NATIVE_FLAG_OFFSET] = u8::from(native);
    }

    /// Whether the pool's quote is a real SPL Token or Token-2022 mint (quote kind 2, `crate::spl`),
    /// held in its associated token account. Such a pool trades only through `swap_spl`.
    pub fn quote_spl(&self) -> bool {
        self.reserved[NATIVE_FLAG_OFFSET] == QUOTE_KIND_SPL
    }

    /// A kind-2 pool's quote program (SPL Token or Token-2022).
    pub fn spl_program(&self) -> Pubkey {
        crate::spl::program_id(self.reserved[SPL_PROGRAM_OFFSET])
    }

    /// A kind-2 pool's quote decimals.
    pub fn spl_decimals(&self) -> u8 {
        self.reserved[SPL_DECIMALS_OFFSET]
    }

    /// Marks the pool kind 2 with its quote mint's program index and decimals.
    pub fn set_quote_spl(&mut self, program: u8, decimals: u8) {
        self.reserved[NATIVE_FLAG_OFFSET] = QUOTE_KIND_SPL;
        self.reserved[SPL_PROGRAM_OFFSET] = program;
        self.reserved[SPL_DECIMALS_OFFSET] = decimals;
    }

    fn reserved_u64(&self, offset: usize) -> u64 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.reserved[offset..offset + 8]);
        u64::from_le_bytes(b)
    }

    fn set_reserved_u64(&mut self, offset: usize, value: u64) {
        self.reserved[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    /// A native pool's buffer: the bridged SOL in its quote vault that its books count (0 for a
    /// bridged pool, whose whole quote is in the vault).
    pub fn buffer_quote(&self) -> u64 {
        self.reserved_u64(BUFFER_OFFSET)
    }

    /// Sets the buffer.
    pub fn set_buffer_quote(&mut self, value: u64) {
        self.set_reserved_u64(BUFFER_OFFSET, value)
    }

    /// The buffer `rebalance` moves a native pool toward.
    pub fn buffer_target(&self) -> u64 {
        self.reserved_u64(BUFFER_TARGET_OFFSET)
    }

    /// Sets the buffer target.
    pub fn set_buffer_target(&mut self, value: u64) {
        self.set_reserved_u64(BUFFER_TARGET_OFFSET, value)
    }

    /// The quote the pool's books hold: `quote_reserve + protocol_fees_quote`.
    pub fn quote_books(&self) -> Option<u64> {
        self.quote_reserve.checked_add(self.protocol_fees_quote)
    }

    /// A native pool's quote held as lamports on its account, above its rent-exempt minimum:
    /// `quote_reserve + protocol_fees_quote - buffer`. The invariant of a native pool is
    /// `lamports >= rent_exempt_minimum + lamports_quote` (equal unless someone sent the pool SOL
    /// or rent fell), and `quote vault balance >= buffer`.
    pub fn lamports_quote(&self) -> Option<u64> {
        self.quote_books()?.checked_sub(self.buffer_quote())
    }
}
