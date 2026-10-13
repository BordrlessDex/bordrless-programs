//! The payout primitive's vectors for the SDK (Loss cover, 2026-10-12):
//! `programs/tests/vectors/payouts.json`, rendered from `bordrless_companion::client` and the
//! layouts (`PayoutTerms`, `PayoutAnswer`): `create_script_v2`, a `script_step` with the payout
//! accounts and two candidates, the hook's `record_payout` as the companion builds it, the budget
//! and bounty arithmetic. A stale file is rewritten and the test fails, so a change is always
//! reviewed.

use std::path::PathBuf;

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{AccountSerialize, AnchorSerialize, Discriminator, Space};
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::events::{PayoutTermsSet, ScriptPayout};
use bordrless_companion::instructions::{
    bounty_for, step_budget, CreateScriptArgs, PayoutTermsArgs,
};
use bordrless_companion::state::{PayoutAnswer, PayoutEntry, PayoutTerms, Split};
use bordrless_launch::client::LaunchKeys;

fn key(n: u8) -> Pubkey {
    Pubkey::new_from_array([n; 32])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn ix(name: &str, i: &Instruction) -> String {
    let accounts: Vec<String> = i
        .accounts
        .iter()
        .map(|m| format!("[\"{}\", {}, {}]", m.pubkey, m.is_signer, m.is_writable))
        .collect();
    format!(
        "    {{\"name\": \"{name}\", \"program\": \"{}\", \"accounts\": [{}], \"data\": \"{}\"}}",
        i.program_id,
        accounts.join(", "),
        hex(&i.data)
    )
}

fn terms() -> PayoutTerms {
    PayoutTerms {
        version: PAYOUT_TERMS_VERSION,
        bump: 251,
        mint: key(7),
        step_bps: 10_000,
        holding_bps: 10_000,
        window_bps: 10_000,
        window_secs: 300,
        window_start: 1_800_000_000,
        window_pot: 5_000_000_000,
        window_paid: 123_456_789,
        paid_total: 9_876_543_210,
        payouts: 42,
        last_paid_at: 1_800_000_123,
        reserved: [0; 28],
    }
}

fn args() -> CreateScriptArgs {
    CreateScriptArgs {
        split: Split {
            buyback_bps: 0,
            holders_bps: 0,
            beneficiary_bps: 0,
        },
        pot_bps: 10_000,
        program: key(9),
        interval: 15,
        state_len: 128,
        init_data: vec![1, 2, 3],
    }
}

fn payout() -> PayoutTermsArgs {
    PayoutTermsArgs {
        step_bps: 10_000,
        holding_bps: 10_000,
        window_bps: 10_000,
        window_secs: 300,
    }
}

fn account(name: &str, data: &[u8], len: usize) -> String {
    format!(
        "    \"{name}\": {{\"len\": {len}, \"discriminator\": \"{}\", \"data\": \"{}\"}}",
        hex(&data[..8]),
        hex(data)
    )
}

fn render() -> String {
    let (mint, payer, cranker, hook, holder_a, owner_a, holder_b) =
        (key(7), key(1), key(2), key(12), key(20), key(21), key(22));
    let mut t = Vec::new();
    terms().try_serialize(&mut t).expect("serialize");
    let mut answer = Vec::new();
    PayoutAnswer {
        version: PAYOUT_ANSWER_VERSION,
        entries: vec![
            PayoutEntry {
                account: 7,
                lamports: 1_000_000,
                tag: 6_000_001,
            },
            PayoutEntry {
                account: 9,
                lamports: 0,
                tag: 6_000_001,
            },
        ],
    }
    .serialize(&mut answer)
    .expect("serialize");
    let keys = LaunchKeys {
        mint,
        quote_mint: BRIDGED_SOL_MINT,
        lp_fee_bps: 30,
        modules: 0,
        burns: false,
        custom_hook: Some(hook),
    };
    // A step with two candidates: the first paid (its owner follows its holding), the second
    // touched (its holding alone); every holding writable (the hook records into it); the answer
    // above names them at indices 7 and 9.
    let mut extra = companion::payout_accounts(&mint, &hook);
    extra.push(AccountMeta::new(holder_a, false));
    extra.push(AccountMeta::new(owner_a, false));
    extra.push(AccountMeta::new(holder_b, false));
    let instructions = [
        ix(
            "createScriptV2",
            &companion::create_script_v2(payer, mint, args(), payout(), vec![]),
        ),
        ix(
            "scriptStepPayouts",
            &companion::script_step(cranker, &keys, key(9), extra),
        ),
        ix(
            "recordPayout",
            &companion::record_payout(hook, mint, holder_a, 1_000_000, 6_000_001),
        ),
    ];
    let payout_accounts: Vec<String> = companion::payout_accounts(&mint, &hook)
        .iter()
        .map(|m| format!("[\"{}\", {}, {}]", m.pubkey, m.is_signer, m.is_writable))
        .collect();
    let events = [
        ("PayoutTermsSet", PayoutTermsSet::DISCRIMINATOR),
        ("ScriptPayout", ScriptPayout::DISCRIMINATOR),
    ]
    .iter()
    .map(|(name, d)| format!("\"{name}\": \"{}\"", hex(d)))
    .collect::<Vec<_>>()
    .join(", ");
    // The budget: every cap at 100%, a pot of 5 SOL at the step's start, the window's first step,
    // a two-entry answer's bounty.
    let bounty = bounty_for(Some(2));
    let budget = step_budget(
        5_000_000_000,
        5_000_000_000,
        bounty,
        5_000_000_000,
        0,
        10_000,
        10_000,
    );
    let budget_bounded = step_budget(
        5_000_000_000,
        5_000_000_000,
        bounty,
        4_000_000_000,
        100_000_000,
        1_000,
        2_000,
    );
    format!(
        "{{\n  \"keys\": {{\"mint\": \"{mint}\", \"payer\": \"{payer}\", \"cranker\": \"{cranker}\", \"program\": \"{}\", \"hook\": \"{hook}\", \"holdingA\": \"{holder_a}\", \"ownerA\": \"{owner_a}\", \"holdingB\": \"{holder_b}\"}},\n  \
         \"payoutTermsAddress\": \"{}\",\n  \
         \"payoutAuthority\": \"{}\",\n  \
         \"payoutAuthorityBump\": {},\n  \
         \"hookStateAddress\": \"{}\",\n  \
         \"hookAuthority\": \"{}\",\n  \
         \"constants\": {{\"termsSeed\": \"payout-terms\", \"authoritySeed\": \"payout-authority\", \"termsVersion\": {}, \"answerVersion\": {}, \"maxPayoutsPerStep\": {}, \"bountyFull\": {}, \"minWindowSecs\": {}, \"maxWindowSecs\": {}, \"recordPayoutDiscriminator\": \"{}\", \"scriptBounty\": \"{}\"}},\n  \
         \"events\": {{{events}}},\n  \
         \"payoutAccounts\": [{}],\n  \
         \"accounts\": {{\n{}\n  }},\n  \
         \"answer\": {{\"len\": {}, \"data\": \"{}\"}},\n  \
         \"budget\": [{{\"potBefore\": \"5000000000\", \"potAfterScript\": \"5000000000\", \"entries\": 2, \"bounty\": \"{bounty}\", \"windowPot\": \"5000000000\", \"windowPaid\": \"0\", \"stepBps\": 10000, \"windowBps\": 10000, \"budget\": \"{budget}\"}}, {{\"potBefore\": \"5000000000\", \"potAfterScript\": \"5000000000\", \"entries\": 2, \"bounty\": \"{bounty}\", \"windowPot\": \"4000000000\", \"windowPaid\": \"100000000\", \"stepBps\": 1000, \"windowBps\": 2000, \"budget\": \"{budget_bounded}\"}}],\n  \
         \"bounty\": {{\"none\": \"{}\", \"0\": \"{}\", \"1\": \"{}\", \"4\": \"{}\", \"8\": \"{}\", \"16\": \"{}\"}},\n  \
         \"instructions\": [\n{}\n  ]\n}}\n",
        key(9),
        companion::payout_terms_address(&mint),
        companion::payout_authority_address(),
        PAYOUT_AUTHORITY_BUMP,
        bordrless_game::state_address(&hook, &mint).0,
        bordrless_hook::hook_authority(&hook).0,
        PAYOUT_TERMS_VERSION,
        PAYOUT_ANSWER_VERSION,
        MAX_PAYOUTS_PER_STEP,
        PAYOUT_BOUNTY_FULL,
        MIN_PAYOUT_WINDOW_SECS,
        MAX_PAYOUT_WINDOW_SECS,
        hex(&RECORD_PAYOUT_DISCRIMINATOR),
        SCRIPT_BOUNTY,
        payout_accounts.join(", "),
        account("payoutTerms", &t, 8 + PayoutTerms::INIT_SPACE),
        answer.len(),
        hex(&answer),
        bounty_for(None),
        bounty_for(Some(0)),
        bounty_for(Some(1)),
        bounty_for(Some(4)),
        bounty_for(Some(8)),
        bounty_for(Some(16)),
        instructions.join(",\n")
    )
}

#[test]
fn the_payout_vectors_are_current() {
    let json = render();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vectors/payouts.json");
    let on_disk = std::fs::read_to_string(&path).ok();
    if on_disk.as_deref() != Some(json.as_str()) {
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("vectors directory");
        std::fs::write(&path, &json).expect("write the vectors");
        panic!(
            "{} was stale (or missing) and has been rewritten from the Rust reference: review the \
             change, then run the tests again",
            path.display()
        );
    }
}
