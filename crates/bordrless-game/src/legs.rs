//! What a game hook reads of a coin quoted in tokens (`docs/token-quotes.md` §7.6): the
//! launchpad's `Legs` at `PDA(["legs", mint], launchpad)`, read at fixed offsets as the kit reads it
//! (a hook needs no dependency on the launchpad's crate). A coin quoted in tokens trades on up to
//! six pools, one a leg, each against its own token; none is ever on the curve of a SOL coin's
//! launch pool, none graduates, and the DEX refuses liquidity on every one of them (a kind-2 pool
//! is the launchpad's alone), so a transfer of the coin out of a leg's pool to a wallet is a buy.
//!
//! A hook lists the `Legs` in its registry ([`LEGS_SEED`], the mint as its second seed: the
//! launchpad refuses a hook on a coin quoted in tokens without it). On a SOL coin the same entry
//! names an address nothing lives at, and [`read_legs`] answers `None`: the hook keeps its SOL
//! rules.

use anchor_lang::prelude::*;

use crate::eligible;

/// The launchpad's `["legs", mint]` seed.
pub const LEGS_SEED: &[u8] = b"legs";
/// Anchor's discriminator of the launchpad's `Legs`: `sha256("account:Legs")[..8]`.
pub const LEGS_DISCRIMINATOR: [u8; 8] = [0x16, 0xbe, 0x5c, 0xdd, 0xc3, 0x3b, 0xab, 0xbd];
/// Size of a `Legs` account.
pub const LEGS_LEN: usize = 974;
/// The most legs a coin has.
pub const MAX_LEGS: usize = 6;

/// Where the fields a hook reads sit in `Legs` (absolute offsets, the discriminator first).
pub mod legs_offsets {
    /// `mint: Pubkey`.
    pub const MINT: usize = 10;
    /// `count: u8`: the legs in use.
    pub const COUNT: usize = 74;
    /// `legs[0]`; each leg is [`LEG_LEN`] bytes.
    pub const FIRST_LEG: usize = 76;
    /// Bytes of one leg.
    pub const LEG_LEN: usize = 139;
    /// A leg's `quote_mint` within the leg.
    pub const LEG_QUOTE_MINT: usize = 0;
    /// A leg's `virtual_quote: u64` (its opening value, raw units of its token) within the leg.
    pub const LEG_VIRTUAL_QUOTE: usize = 35;
    /// A leg's `pool` within the leg.
    pub const LEG_POOL: usize = 51;
}

/// A coin's legs as a hook sees them: each leg's pool, in leg order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LegsView {
    /// The legs' pools (`count` of them in use, the rest the default key).
    pub pools: [Pubkey; MAX_LEGS],
    /// The legs in use (1 to 6).
    pub count: u8,
}

impl LegsView {
    /// The legs' pools in use.
    pub fn in_use(&self) -> &[Pubkey] {
        &self.pools[..usize::from(self.count).min(MAX_LEGS)]
    }

    /// The leg whose pool is `owner`, if any.
    pub fn leg_of(&self, owner: &Pubkey) -> Option<usize> {
        self.in_use().iter().position(|p| p == owner)
    }
}

/// `PDA(["legs", mint], launchpad)`.
pub fn legs_address(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[LEGS_SEED, mint.as_ref()], &crate::LAUNCH_PROGRAM_ID).0
}

/// Reads the legs of `mint` from a `Legs` account's data: the launchpad's discriminator, at least
/// [`LEGS_LEN`] bytes, this mint, 1 to 6 legs. `None` for anything else.
pub fn parse_legs(data: &[u8], mint: &Pubkey) -> Option<LegsView> {
    use legs_offsets as o;
    if data.len() < LEGS_LEN || data[..8] != LEGS_DISCRIMINATOR {
        return None;
    }
    if data[o::MINT..o::MINT + 32] != mint.to_bytes() {
        return None;
    }
    let count = data[o::COUNT];
    if count == 0 || usize::from(count) > MAX_LEGS {
        return None;
    }
    let mut pools = [Pubkey::default(); MAX_LEGS];
    for (i, pool) in pools.iter_mut().enumerate().take(usize::from(count)) {
        let at = o::FIRST_LEG + i * o::LEG_LEN + o::LEG_POOL;
        let mut key = [0u8; 32];
        key.copy_from_slice(&data[at..at + 32]);
        *pool = Pubkey::new_from_array(key);
    }
    Some(LegsView { pools, count })
}

/// [`parse_legs`] from the account: owned by the launchpad, its data readable. `None` on a SOL
/// coin (nothing lives at its `Legs` address). Never panics.
pub fn read_legs(info: &AccountInfo, mint: &Pubkey) -> Option<LegsView> {
    if *info.owner != crate::LAUNCH_PROGRAM_ID {
        return None;
    }
    let data = info.try_borrow_data().ok()?;
    parse_legs(&data, mint)
}

/// Whether a transfer on a coin quoted in tokens is a qualifying buy: out of any leg's pool, of at
/// least `min_tokens` (and at least 1), to an owner who may hold tickets (none of `excluded`, a
/// wallet, never a leg's pool). Every leg is off the curve of a launch pool and refuses liquidity,
/// so no curve condition applies: a leg's sends to wallets are buys.
pub fn qualifying_buy_legs(
    legs: &LegsView,
    source_owner: &Pubkey,
    destination_owner: &Pubkey,
    amount: u64,
    min_tokens: u64,
    excluded: &[Pubkey],
) -> bool {
    legs.leg_of(source_owner).is_some()
        && legs.leg_of(destination_owner).is_none()
        && amount >= min_tokens.max(1)
        && eligible(destination_owner, excluded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legs_data(mint: &Pubkey, pools: &[Pubkey]) -> Vec<u8> {
        use legs_offsets as o;
        let mut d = vec![0u8; LEGS_LEN];
        d[..8].copy_from_slice(&LEGS_DISCRIMINATOR);
        d[o::MINT..o::MINT + 32].copy_from_slice(mint.as_ref());
        d[o::COUNT] = pools.len() as u8;
        for (i, p) in pools.iter().enumerate() {
            let at = o::FIRST_LEG + i * o::LEG_LEN + o::LEG_POOL;
            d[at..at + 32].copy_from_slice(p.as_ref());
        }
        d
    }

    #[test]
    fn the_legs_read_at_the_launchpads_offsets_and_nothing_else_reads() {
        let mint = Pubkey::new_unique();
        let pools = [
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        let d = legs_data(&mint, &pools);
        let v = parse_legs(&d, &mint).unwrap();
        assert_eq!((v.count, v.in_use()), (3, &pools[..]));
        assert_eq!(v.leg_of(&pools[2]), Some(2));
        assert_eq!(v.leg_of(&Pubkey::new_unique()), None);
        assert_eq!(
            parse_legs(&d, &Pubkey::new_unique()),
            None,
            "another mint's"
        );
        assert_eq!(parse_legs(&d[..LEGS_LEN - 1], &mint), None, "too short");
        let mut bad = d.clone();
        bad[0] ^= 1;
        assert_eq!(parse_legs(&bad, &mint), None, "another discriminator");
        let mut none = d.clone();
        none[legs_offsets::COUNT] = 0;
        assert_eq!(parse_legs(&none, &mint), None, "no legs");
        none[legs_offsets::COUNT] = 7;
        assert_eq!(parse_legs(&none, &mint), None, "seven legs");
    }

    #[test]
    fn a_buy_out_of_any_leg_qualifies_and_nothing_into_a_pool_does() {
        let mint = Pubkey::new_unique();
        // Leg pools are PDAs: off the curve.
        let pools: Vec<Pubkey> = (0..3u8)
            .map(|i| Pubkey::find_program_address(&[&[i]], &crate::LAUNCH_PROGRAM_ID).0)
            .collect();
        let v = parse_legs(&legs_data(&mint, &pools), &mint).unwrap();
        // A key on the curve: a wallet.
        let wallet = (1u8..=255)
            .map(|b| Pubkey::new_from_array([b; 32]))
            .find(|k| k.is_on_curve())
            .unwrap();
        for p in &pools {
            assert!(qualifying_buy_legs(&v, p, &wallet, 5, 5, &[]));
            assert!(
                !qualifying_buy_legs(&v, p, &wallet, 4, 5, &[]),
                "under the minimum"
            );
            assert!(!qualifying_buy_legs(&v, p, &wallet, 0, 0, &[]), "nothing");
            assert!(
                !qualifying_buy_legs(&v, p, &wallet, 5, 5, &[wallet]),
                "excluded"
            );
            assert!(
                !qualifying_buy_legs(&v, p, &pools[0], 5, 5, &[]),
                "into a pool"
            );
        }
        assert!(
            !qualifying_buy_legs(&v, &wallet, &wallet, 5, 5, &[]),
            "a wallet's send is no buy"
        );
    }
}
