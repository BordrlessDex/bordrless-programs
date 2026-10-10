use anchor_lang::prelude::*;

/// A slot as `create_vault` fixed it.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotPolicy {
    pub policy: u8,
    pub target: Pubkey,
    /// `SellBuyBurn` of a custom-hook token: the most that token's hook may cut from the buy.
    pub max_cut_bps: u16,
}

#[event]
pub struct VaultCreated {
    pub vault: Pubkey,
    pub mint: Pubkey,
    pub hook: Pubkey,
    pub creator: Pubkey,
    pub slots: Vec<SlotPolicy>,
    /// The slots' owners, `PDA(["slot", mint, [i]])`: their holdings of the coin are what the hook
    /// names as delta targets once the vault is open.
    pub slot_owners: Vec<Pubkey>,
    pub bounty_bps: u16,
    pub max_sell_bps: u16,
    pub interval: i64,
    pub max_hook_cut_bps: u16,
}

#[event]
pub struct VaultOpened {
    pub vault: Pubkey,
    pub mint: Pubkey,
    /// The price the sells' references start at: the coin's pool price, or its launch's opening
    /// price when the pool is above it (so nobody can open a vault at a pumped price).
    pub price: u128,
    pub ts: i64,
}

#[event]
pub struct SlotBurned {
    pub vault: Pubkey,
    pub slot: u8,
    pub amount: u64,
    pub cranker: Pubkey,
}

#[event]
pub struct SlotSold {
    pub vault: Pubkey,
    pub slot: u8,
    /// Coin sold.
    pub sold: u64,
    /// SOL it brought (bridged), before the bounty.
    pub got: u64,
    pub bounty: u64,
    /// SOL paid to the slot's wallet (`SellForSol`); 0 for `SellBuyBurn`, or while it waits.
    pub paid: u64,
    /// What the slot holds for later after this sell.
    pub pending_sol: u64,
    /// The pool's price before the sell, and the reference after it.
    pub price: u128,
    pub reference: u128,
    pub cranker: Pubkey,
}

/// A sell that waited: the price was more than 3% below the reference. The wait uses up the slot's
/// turn: its next sell (or wait) is an interval later.
#[event]
pub struct SellWaited {
    pub vault: Pubkey,
    pub slot: u8,
    pub price: u128,
    /// The reference the price was held to, and where it is now.
    pub reference: u128,
    pub new_reference: u128,
}

/// `SellForSol`: a payment held back earlier, paid now.
#[event]
pub struct PendingPaid {
    pub vault: Pubkey,
    pub slot: u8,
    pub paid: u64,
}

#[event]
pub struct SlotBought {
    pub vault: Pubkey,
    pub slot: u8,
    /// SOL spent (after the bounty).
    pub spent: u64,
    /// The other token bought and burned (with any of it someone had sent the slot's owner, burned
    /// first).
    pub burned: u64,
    pub bounty: u64,
    pub pending_sol: u64,
    pub cranker: Pubkey,
}

/// A buy that waited: the token's price was more than 3% above the buy's reference. The wait uses
/// up the slot's turn to buy.
#[event]
pub struct BuyWaited {
    pub vault: Pubkey,
    pub slot: u8,
    pub price: u128,
    pub reference: u128,
    pub new_reference: u128,
}

#[event]
pub struct SlotRetired {
    pub vault: Pubkey,
    pub slot: u8,
    /// SOL sent to the incinerator.
    pub sol_burned: u64,
    /// Coin burned.
    pub coin_burned: u64,
}
