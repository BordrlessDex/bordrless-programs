use anchor_lang::prelude::*;

use crate::state::Split;

#[event]
pub struct CompanionCreated {
    pub companion: Pubkey,
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub beneficiary: Pubkey,
    pub split: Split,
    pub bounty_bps: u16,
    pub max_buyback: u64,
    pub buyback_interval: i64,
    pub vest_secs: i64,
}

#[event]
pub struct CompanionLaunched {
    pub companion: Pubkey,
    pub mint: Pubkey,
    pub ts: i64,
}

#[event]
pub struct DevBought {
    pub companion: Pubkey,
    pub lamports: u64,
    pub tokens: u64,
    pub dev_tokens: u64,
}

#[event]
pub struct FeesClaimed {
    pub companion: Pubkey,
    pub claimed: u64,
    pub bounty: u64,
    pub to_buyback: u64,
    pub to_holders: u64,
    pub to_beneficiary: u64,
    pub cranker: Pubkey,
}

#[event]
pub struct BoughtBack {
    pub companion: Pubkey,
    pub spent: u64,
    pub burned: u64,
    pub bounty: u64,
    pub cranker: Pubkey,
}

/// A buyback that waited: the price was above the reference (which moved toward it, when due).
#[event]
pub struct BuybackWaited {
    pub companion: Pubkey,
    pub price: u128,
    pub reference: u128,
}

#[event]
pub struct SharedWithHolders {
    pub companion: Pubkey,
    pub amount: u64,
    pub bounty: u64,
    pub cranker: Pubkey,
}

#[event]
pub struct BeneficiaryPaid {
    pub companion: Pubkey,
    pub lamports: u64,
}

#[event]
pub struct DevReleased {
    pub companion: Pubkey,
    pub tokens: u64,
    pub released: u64,
}
