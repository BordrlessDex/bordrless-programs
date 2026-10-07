//! Events of the DEX, emitted by self-CPI. A `Swapped` event carries the reserves after the swap
//! (virtual offsets included), which is what the indexer prices from.

#![allow(missing_docs)]

use anchor_lang::prelude::*;

#[event]
pub struct ConfigSet {
    pub admin: Pubkey,
    pub protocol_fee_bps: u16,
    pub launch_protocol_share_bps: u16,
    pub fee_collector: Pubkey,
    pub treasury: Pubkey,
    pub pool_creation_fee_lamports: u64,
    pub paused: bool,
    pub ts: i64,
}

#[event]
pub struct PoolCreated {
    pub pool: Pubkey,
    pub payer: Pubkey,
    pub creator: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub lp_mint: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub lp_fee_bps: u16,
    /// The flat protocol fee (`FEE_MODEL_FLAT`); 0 under the share model.
    pub protocol_fee_bps: u16,
    /// `FEE_MODEL_FLAT` or `FEE_MODEL_SHARE`.
    pub fee_model: u8,
    /// Bordrless's share of the hooks' cuts (`FEE_MODEL_SHARE`); 0 under the flat model.
    pub protocol_share_bps: u16,
    pub hook_program: Option<Pubkey>,
    pub hook_flags: u16,
    pub virtual_base: u64,
    pub virtual_quote: u64,
    pub base_reserve: u64,
    pub quote_reserve: u64,
    pub lp_supply: u64,
    pub lp_minted: u64,
    pub curve: bool,
    pub slot: u64,
    pub ts: i64,
}

/// One cut a pool hook took from a swap: the holding it was sent to and the amount sent (a token
/// hook on that mint may deliver less).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct DeltaPaid {
    pub holding: Pubkey,
    pub amount: u64,
}

/// A swap. The input side: the hook's `deltas_in` and `burn_in` left the trader's input holding
/// first, then the rest, of which `received_in` reached the vault (`cuts_in` is what the hooks
/// took on the way: the pool hook's deltas and any token-hook cut). The output side: the curve
/// gave `amount_out`; the protocol fee stayed in the quote vault; the hook's `deltas_out` and
/// `burn_out` left the output vault; the rest went to the recipient's holding, which gained
/// `delivered_out` (`cuts_out` is what the hooks took on the way).
#[event]
pub struct Swapped {
    pub pool: Pubkey,
    pub trader: Pubkey,
    /// The owner of the holding the output was delivered to.
    pub recipient: Pubkey,
    /// 0 base → quote (sell), 1 quote → base (buy).
    pub direction: u8,
    /// What the trader put in, before the hook's cuts.
    pub amount_in: u64,
    /// Sent from the trader's input holding by the hook's `before_swap` answer, in order.
    pub deltas_in: Vec<DeltaPaid>,
    /// Burned from the trader's input holding by the hook's `before_swap` answer.
    pub burn_in: u64,
    /// What the hooks took from the input and someone received, in the input token:
    /// `amount_in - burn_in - received_in` (the pool hook's deltas, plus any cut the input mint's
    /// own hook took from those transfers).
    pub cuts_in: u64,
    /// What arrived in the input vault.
    pub received_in: u64,
    /// In the input token; stays in the pool.
    pub lp_fee: u64,
    /// Always in the quote token. Flat model: on a buy from `received_in`, on a sell from
    /// `amount_out`. Share model: the pool's share of `cuts_in` and `cuts_out` (base-side cuts
    /// valued at this swap's price), each rounded up.
    pub protocol_fee: u64,
    pub lp_fee_bps: u16,
    /// What the curve gave (before any protocol fee taken from the output).
    pub amount_out: u64,
    /// Sent from the output vault by the hook's `after_swap` answer, in order.
    pub deltas_out: Vec<DeltaPaid>,
    /// Burned from the output vault by the hook's `after_swap` answer.
    pub burn_out: u64,
    /// What the hooks took from the output and someone received, in the output token: the pool
    /// hook's deltas, plus any cut the output mint's own hook took from those transfers and from
    /// the delivery.
    pub cuts_out: u64,
    /// What the recipient's holding actually gained.
    pub delivered_out: u64,
    pub base_reserve: u64,
    pub quote_reserve: u64,
    pub virtual_base: u64,
    pub virtual_quote: u64,
    pub swap_count: u64,
    pub slot: u64,
    pub ts: i64,
}

#[event]
pub struct LiquidityAdded {
    pub pool: Pubkey,
    pub provider: Pubkey,
    pub base_amount: u64,
    pub quote_amount: u64,
    pub lp_minted: u64,
    pub base_reserve: u64,
    pub quote_reserve: u64,
    pub lp_supply: u64,
    pub slot: u64,
    pub ts: i64,
}

#[event]
pub struct LiquidityRemoved {
    pub pool: Pubkey,
    pub provider: Pubkey,
    pub base_amount: u64,
    pub quote_amount: u64,
    pub lp_burned: u64,
    pub base_reserve: u64,
    pub quote_reserve: u64,
    pub lp_supply: u64,
    pub slot: u64,
    pub ts: i64,
}

#[event]
pub struct CurveFinalized {
    pub pool: Pubkey,
    pub base_reserve: u64,
    pub quote_reserve: u64,
    pub lp_supply: u64,
    pub lp_minted: u64,
    pub lp_recipient: Pubkey,
    pub slot: u64,
    pub ts: i64,
}

/// Protocol fees (always in the quote token) moved from a pool's quote vault to the fee
/// collector's holding.
#[event]
pub struct ProtocolFeesCollected {
    pub pool: Pubkey,
    pub quote_amount: u64,
    pub collector: Pubkey,
    pub ts: i64,
}
