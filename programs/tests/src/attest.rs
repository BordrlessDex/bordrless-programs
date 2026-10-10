//! Bordrless Studio attestations (`docs/phase3a.md` §6) in the suites: the account as Studio's
//! attester writes it, put directly (the attester's key is a mainnet hot key the suites never
//! hold; `companion_attest.rs` drives the real `attest` instruction with sigverify off).

use anchor_lang::prelude::Pubkey;
use anchor_lang::AccountSerialize;
use bordrless_companion::state::HookAttestation;
use solana_account::Account;

use crate::env::Env;
use crate::timelock::{programdata_hash, programdata_slot};

/// What Studio's attester writes for `program` as it is now: its code's executable hash, the
/// ProgramData's slot, a passing simulation and review.
pub fn attestation_of(env: &Env, program: &Pubkey) -> HookAttestation {
    HookAttestation {
        version: bordrless_companion::constants::ATTESTATION_VERSION,
        bump: HookAttestation::address(program).1,
        program: *program,
        build_hash: programdata_hash(env, program),
        source_hash: [3; 32],
        template_commit: [4; 20],
        sim_version: 2,
        sim_pass: true,
        cut_max_bps: 0,
        cap_bps: 0,
        review: bordrless_companion::constants::REVIEW_PASS,
        kind: 1,
        programdata_slot: programdata_slot(env, program),
        attested_at: env.now,
        attester: bordrless_companion::constants::STUDIO_ATTESTER,
        revoked: false,
        revoked_at: 0,
        reserved: [0; 32],
    }
}

/// Writes `a` at its address, owned by the companion.
pub fn put_attestation_account(env: &mut Env, a: &HookAttestation) {
    let mut data = Vec::new();
    a.try_serialize(&mut data).expect("serialize");
    data.resize(HookAttestation::LEN, 0);
    let lamports = env.rent(data.len());
    env.put(
        HookAttestation::address(&a.program).0,
        Account {
            lamports,
            data,
            owner: bordrless_companion::ID,
            executable: false,
            rent_epoch: 0,
        },
    );
}

/// Each of `programs` attested by Studio as it is now.
pub fn attest_all(env: &mut Env, programs: &[Pubkey]) {
    for p in programs {
        let a = attestation_of(env, p);
        put_attestation_account(env, &a);
    }
}
