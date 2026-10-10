//! Independent phase-3a audit, integration / compatibility / deploy lens. The PoC now asserts the fix.
//!
//! - Finding 6: the SDK CLI's `bordrless hook|strategy propose` sent `hook_timelock::propose` with no
//!   `SetComputeUnitLimit`, so it got the default 200k units; `propose` hashes the buffer on chain
//!   (~0.5 unit a byte), so any program over ~380 KB of code could not be proposed through the CLI,
//!   though the program takes up to 2 MiB. The CLI (`packages/sdk/src/cli/bordrless.ts`) now puts a
//!   limit in front of every transaction: `proposeUnits(len)` for `propose`, held here to the
//!   program up to the 2 MiB cap.

use anchor_lang::prelude::Pubkey;
use bordrless_hook::authority::trimmed_len;
use bordrless_program_tests::env::{compute_unit_limit, Env};
use bordrless_program_tests::timelock::*;
use hook_timelock::client as tl;
use hook_timelock::{loader, MIN_DELAY};
use solana_keypair::Keypair;
use solana_signer::Signer;

const PROGRAM: Pubkey = tax_hook::ID;

/// tax_hook registered under a 3-day timelock, and a buffer of `len` bytes of non-zero code handed
/// to the timelock: what `bordrless hook propose <so> --buffer <key>` starts from.
fn proposal_ready(len: usize) -> (Env, Keypair, Pubkey, Vec<u8>) {
    let mut env = Env::new();
    let author = env.funded(10_000_000_000);
    env.set_upgrade_authority(PROGRAM, Some(author.pubkey()));
    register(&mut env, &author, PROGRAM, MIN_DELAY, author.pubkey()).ok();
    let code: Vec<u8> = (0..len).map(|i| (i % 251) as u8 | 1).collect();
    let buffer = Pubkey::new_unique();
    put_buffer(&mut env, buffer, author.pubkey(), &code, 0);
    env.send(
        &[loader::set_authority(
            buffer,
            author.pubkey(),
            Some(timelock_of(&PROGRAM)),
        )],
        &[&author],
    )
    .ok();
    (env, author, buffer, code)
}

/// The CLI's compute limit for `propose` of a buffer of `len` bytes: `proposeUnits` in
/// `packages/sdk/src/cli/bordrless.ts` (monorepo), `min(1.4M, ceil((15,000 + ceil(len / 2)) × 1.15))`.
fn cli_propose_units(len: usize) -> u32 {
    let base = 15_000u64 + (len as u64).div_ceil(2);
    (base * 115).div_ceil(100).min(1_400_000) as u32
}

#[test]
fn propose_with_the_clis_compute_limit_lands_up_to_2_mib() {
    // 360 KB (a hook_vault-sized program): fits even the default 200k units.
    let (mut env, author, buffer, code) = proposal_ready(360_000);
    let ix = tl::propose(author.pubkey(), PROGRAM, buffer, trimmed_len(&code) as u32);
    let tx = env.send_bare(&[ix], &[&author]);
    tx.ok();
    println!(
        "propose of 360,000 B with the default budget: {} CU",
        tx.cu()
    );

    // Control: 450 KB (bordrless_swap is 447,792 B) without a compute budget instruction (what the
    // CLI sent before the fix) runs out of the default 200k units.
    let (mut env, author, buffer, code) = proposal_ready(450_000);
    let ix = tl::propose(author.pubkey(), PROGRAM, buffer, trimmed_len(&code) as u32);
    let bare = env.send_bare(std::slice::from_ref(&ix), &[&author]);
    bare.expect_fail();
    assert!(
        bare.logs()
            .iter()
            .any(|l| l.contains("exceeded CUs meter") || l.contains("consumed 200000 of 200000")),
        "without a limit propose runs out of the default 200k units: {:?}",
        bare.logs()
    );
    // The same instruction behind the CLI's limit lands, within it.
    env.svm.expire_blockhash();
    let limit = cli_propose_units(code.len());
    let payer = env.payer.insecure_clone();
    let budgeted = env.send_v0(&[compute_unit_limit(limit), ix], &payer, &[&author], &[]);
    budgeted.ok();
    println!(
        "propose of 450,000 B: default budget fails; the CLI's limit {limit}, used {} CU",
        budgeted.cu()
    );
    assert!(budgeted.cu() > 200_000 && budgeted.cu() < u64::from(limit));

    // 2 MiB, the program's cap: the CLI's limit (within 1.4M) is enough.
    let len = 2 * 1024 * 1024;
    let (mut env, author, buffer, code) = proposal_ready(len);
    let ix = tl::propose(author.pubkey(), PROGRAM, buffer, trimmed_len(&code) as u32);
    let limit = cli_propose_units(code.len());
    assert!(limit <= 1_400_000);
    let payer = env.payer.insecure_clone();
    let tx = env.send_v0(&[compute_unit_limit(limit), ix], &payer, &[&author], &[]);
    tx.ok();
    println!(
        "propose of 2 MiB: the CLI's limit {limit}, used {} CU",
        tx.cu()
    );
    assert!(tx.cu() < u64::from(limit));
}
