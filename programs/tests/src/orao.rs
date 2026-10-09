//! ORAO VRF in the LiteSVM suites: the mainnet program and its network state, dumped into
//! `fixtures/`, and fulfilment written with `set_account` exactly as ORAO's own last `fulfill_v2`
//! leaves it (checked byte for byte against ORAO's `fulfill_v2` run in LiteSVM). The layouts are
//! written out here rather than taken from the companion's `oracle` module, so that the tests
//! check that module instead of trusting it.

use anchor_lang::prelude::Pubkey;
use solana_account::Account;

use crate::{Env, SYSTEM_PROGRAM_ID};

/// ORAO VRF (classic).
pub const ORAO_VRF_ID: Pubkey =
    Pubkey::from_str_const("VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y");
/// ORAO's network state, `PDA(["orao-vrf-network-configuration"])`.
pub const NETWORK_STATE: Pubkey =
    Pubkey::from_str_const("5ER1oENnV4srxYdAynUfRzWeQCPQaqMiAp4VqyMbSqnK");
/// ORAO's treasury in the fixture (also its upgrade and config authority on mainnet).
pub const TREASURY: Pubkey = Pubkey::from_str_const("9ZTHWWZDpB36UFe1vszf2KEpt83vwi27jDqtHQ7NSXyR");
/// The fixture's fee per request.
pub const FEE: u64 = 500_000;
/// A request account's size, pending and fulfilled.
pub const PENDING_LEN: usize = 749;
pub const FULFILLED_LEN: usize = 137;
/// `sha256("account:RandomnessV2")[..8]`.
pub const RANDOMNESS_V2: [u8; 8] = [139, 239, 184, 215, 227, 86, 191, 226];
/// The network state account's lamports on mainnet.
const NETWORK_STATE_LAMPORTS: u64 = 104_176_000;
/// Byte offsets: the network state's fee; a request's variant, client, seed and randomness.
pub const FEE_AT: usize = 72;
const VARIANT_AT: usize = 8;
const CLIENT_AT: usize = 9;
const SEED_AT: usize = 41;
const RANDOMNESS_AT: usize = 73;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// Loads ORAO at its address with its mainnet network state, and funds its treasury (a fee sent to
/// an empty treasury would leave it below the rent-exempt minimum and fail the request).
pub fn load(env: &mut Env) {
    env.svm
        .add_program(ORAO_VRF_ID, &fixture("orao_vrf.so"))
        .expect("load orao_vrf.so");
    env.put(
        NETWORK_STATE,
        Account {
            lamports: NETWORK_STATE_LAMPORTS,
            data: fixture("orao_network_state.bin"),
            owner: ORAO_VRF_ID,
            executable: false,
            rent_epoch: 0,
        },
    );
    env.fund(TREASURY, 1_000_000_000);
}

/// Sets ORAO's fee per request (to test the companion's cap, or a fee change between draws).
pub fn set_fee(env: &mut Env, fee: u64) {
    let mut ns = env.account(&NETWORK_STATE).expect("ORAO loaded");
    ns.data[FEE_AT..FEE_AT + 8].copy_from_slice(&fee.to_le_bytes());
    env.put(NETWORK_STATE, ns);
}

/// The request account for `seed`.
pub fn request_address(seed: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(&[b"orao-vrf-randomness-request", seed], &ORAO_VRF_ID).0
}

/// The client (payer) of the request for `seed` while it is pending; `None` once it is fulfilled
/// or when there is none.
pub fn pending_client(env: &Env, seed: &[u8; 32]) -> Option<Pubkey> {
    let acc = env.account(&request_address(seed))?;
    (acc.owner == ORAO_VRF_ID && acc.data[..8] == RANDOMNESS_V2 && acc.data[VARIANT_AT] == 0)
        .then(|| Pubkey::new_from_array(acc.data[CLIENT_AT..SEED_AT].try_into().unwrap()))
}

/// The fulfilled randomness for `seed`, if any.
pub fn fulfilled(env: &Env, seed: &[u8; 32]) -> Option<[u8; 64]> {
    let acc = env.account(&request_address(seed))?;
    (acc.owner == ORAO_VRF_ID && acc.data[..8] == RANDOMNESS_V2 && acc.data[VARIANT_AT] == 1)
        .then(|| acc.data[RANDOMNESS_AT..FULFILLED_LEN].try_into().unwrap())
}

/// Fulfils the pending request for `seed` with `randomness` as ORAO's last `fulfill_v2` does: the
/// account shrinks to 137 bytes holding their rent-exempt minimum, and the request's client gets
/// the rest of its lamports. Returns that refund.
pub fn fulfil(env: &mut Env, seed: &[u8; 32], randomness: &[u8; 64]) -> u64 {
    let key = request_address(seed);
    let pending = env.account(&key).expect("a request for this seed");
    assert_eq!(pending.owner, ORAO_VRF_ID);
    assert_eq!(pending.data[..8], RANDOMNESS_V2);
    assert_eq!(pending.data[VARIANT_AT], 0, "already fulfilled");
    assert_eq!(pending.data[SEED_AT..RANDOMNESS_AT], seed[..]);
    let client = Pubkey::new_from_array(pending.data[CLIENT_AT..SEED_AT].try_into().unwrap());
    let mut data = Vec::with_capacity(FULFILLED_LEN);
    data.extend_from_slice(&RANDOMNESS_V2);
    data.push(1); // RequestAccount::Fulfilled
    data.extend_from_slice(&pending.data[CLIENT_AT..RANDOMNESS_AT]); // client, seed
    data.extend_from_slice(randomness);
    let lamports = env.rent(FULFILLED_LEN);
    let refund = pending.lamports - lamports;
    env.put(
        key,
        Account {
            lamports,
            data,
            owner: ORAO_VRF_ID,
            executable: false,
            rent_epoch: 0,
        },
    );
    if env.account(&client).is_none() {
        env.put(
            client,
            Account {
                lamports: 0,
                data: vec![],
                owner: SYSTEM_PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
        );
    }
    let to = env.lamports(&client) + refund;
    env.fund(client, to);
    refund
}

/// A repeatable stand-in for ORAO's randomness for `seed`: splitmix64, seeded by mixing in the
/// seed's four words. Every run of a suite draws the same numbers.
pub fn randomness_for(seed: &[u8; 32]) -> [u8; 64] {
    fn mix(mut z: u64) -> u64 {
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut x = seed.chunks_exact(8).fold(GAMMA, |acc, w| {
        mix(acc.wrapping_add(GAMMA) ^ u64::from_le_bytes(w.try_into().unwrap()))
    });
    let mut out = [0u8; 64];
    for word in out.chunks_exact_mut(8) {
        x = x.wrapping_add(GAMMA);
        word.copy_from_slice(&mix(x).to_le_bytes());
    }
    out
}
