//! Approvals' and script companions' vectors for the SDK (2026-10-12):
//! `programs/tests/vectors/scripts.json`, rendered from `bordrless_companion::client` and the account
//! layouts (`Approval`, `Script`), with a script companion's fee claim (its author paid) and a
//! Snowball step of a custom-hook token. A stale file is
//! rewritten and the test fails, so a change is always reviewed.

use std::path::PathBuf;

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{AccountSerialize, Discriminator, Space};
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::events::{
    ApprovalRevoked, AuthorPaid, ProgramApproved, ScriptCreated, ScriptStepped,
};
use bordrless_companion::instructions::CreateScriptArgs;
use bordrless_companion::state::{Approval, Script, Split};
use bordrless_launch::client::{CustomHookAccounts, LaunchKeys};

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

fn keys(mint: Pubkey, modules: u8, burns: bool, custom_hook: Option<Pubkey>) -> LaunchKeys {
    LaunchKeys {
        mint,
        quote_mint: BRIDGED_SOL_MINT,
        lp_fee_bps: 30,
        modules,
        burns,
        custom_hook,
    }
}

fn approval() -> Approval {
    Approval {
        version: APPROVAL_VERSION,
        bump: 253,
        program: key(9),
        kind: APPROVAL_KIND_SCRIPT,
        code_hash: [0xab; 32],
        approved_at: 1_760_000_000,
        revoked: false,
        updated_by: key(3),
        programdata_slot: 400_000_123,
        author: key(5),
        reserved: [0; 24],
    }
}

fn script() -> Script {
    Script {
        version: SCRIPT_VERSION,
        bump: 252,
        mint: key(7),
        program: key(9),
        interval: 60,
        last_at: 1_760_000_600,
        steps: 42,
        created_at: 1_759_990_000,
        author: key(5),
        author_paid: 1_234_567_890,
        state_len: 1_024,
        reserved: [0; 20],
    }
}

fn terms() -> CreateScriptArgs {
    CreateScriptArgs {
        split: Split {
            buyback_bps: 1_000,
            holders_bps: 0,
            beneficiary_bps: 2_000,
        },
        pot_bps: 7_000,
        program: key(9),
        interval: 60,
        state_len: 1_024,
        init_data: vec![1, 2, 3],
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
    let (mint, payer, cranker, authority, program, author) =
        (key(7), key(1), key(2), key(3), key(9), key(5));
    let (mut a, mut s) = (Vec::new(), Vec::new());
    approval().try_serialize(&mut a).expect("serialize");
    script().try_serialize(&mut s).expect("serialize");
    // A custom hook's accounts for a Snowball's buys and for its sells (resolved by the client from
    // the hook's registry; here, made up).
    let hook = key(12);
    let buy = CustomHookAccounts {
        program: hook,
        extras: vec![
            AccountMeta::new(key(13), false),
            AccountMeta::new_readonly(key(14), false),
        ],
    };
    let sell = CustomHookAccounts {
        program: hook,
        extras: vec![AccountMeta::new(key(15), false)],
    };
    let instructions = [
        ix(
            "approve",
            &companion::approve(authority, program, APPROVAL_KIND_SCRIPT, [0xab; 32], author),
        ),
        ix(
            "revokeApproval",
            &companion::revoke_approval(authority, program),
        ),
        // The script's init takes one account of its own (`key(10)`) after its four (`[creator,
        // mint, state, system program]`); forwarded unsigned.
        ix(
            "createScript",
            &companion::create_script(payer, mint, terms(), vec![AccountMeta::new(key(10), false)]),
        ),
        // A script that asks for no state: the system program's id stands in its place.
        ix(
            "createScriptNoState",
            &companion::create_script(
                payer,
                mint,
                CreateScriptArgs {
                    state_len: 0,
                    ..terms()
                },
                vec![],
            ),
        ),
        // The script's step takes two accounts of its own after its six (`[creator, mint, launch,
        // companion, pool, state]`); forwarded unsigned.
        ix(
            "scriptStep",
            &companion::script_step(
                cranker,
                &keys(mint, 0, false, None),
                program,
                vec![
                    AccountMeta::new_readonly(key(10), false),
                    AccountMeta::new(key(11), false),
                ],
            ),
        ),
        ix(
            "scriptStepKit",
            &companion::script_step(cranker, &keys(mint, 1, true, None), program, vec![]),
        ),
        // A script companion's fee claim: the script (writable), then the author's wallet when the
        // claim may pay them; without it when the author is the launcher (or none).
        ix(
            "claimFeesScript",
            &companion::claim_fees_script(cranker, mint, Some(author)),
        ),
        ix(
            "claimFeesScriptNoAuthor",
            &companion::claim_fees_script(cranker, mint, None),
        ),
        ix(
            "snowballStepCustomHook",
            &companion::snowball_step_with(
                cranker,
                &keys(mint, 0, false, Some(hook)),
                Some(&buy),
                Some(&sell),
            ),
        ),
    ];
    let events = [
        ("ProgramApproved", ProgramApproved::DISCRIMINATOR),
        ("ApprovalRevoked", ApprovalRevoked::DISCRIMINATOR),
        ("ScriptCreated", ScriptCreated::DISCRIMINATOR),
        ("ScriptStepped", ScriptStepped::DISCRIMINATOR),
        ("AuthorPaid", AuthorPaid::DISCRIMINATOR),
    ]
    .iter()
    .map(|(name, d)| format!("\"{name}\": \"{}\"", hex(d)))
    .collect::<Vec<_>>()
    .join(", ");
    format!(
        "{{\n  \"keys\": {{\"mint\": \"{mint}\", \"payer\": \"{payer}\", \"cranker\": \"{cranker}\", \"authority\": \"{authority}\", \"program\": \"{program}\", \"author\": \"{author}\"}},\n  \
         \"approvalAddress\": \"{}\",\n  \
         \"scriptAddress\": \"{}\",\n  \
         \"scriptStateAddress\": \"{}\",\n  \
         \"constants\": {{\"modeScript\": {}, \"approvalKindTokenHook\": {}, \"approvalKindScript\": {}, \"minInterval\": {}, \"maxInterval\": {}, \"maxInitLen\": {}, \"maxStateLen\": {}, \"bounty\": \"{}\", \"afterLaunch\": {}, \"authorShareBps\": {}, \"initDiscriminator\": \"{}\", \"stepDiscriminator\": \"{}\"}},\n  \
         \"events\": {{{events}}},\n  \
         \"accounts\": {{\n{},\n{}\n  }},\n  \
         \"instructions\": [\n{}\n  ]\n}}\n",
        companion::approval_address(&program),
        companion::script_address(&mint),
        companion::script_state_address(&mint),
        MODE_SCRIPT,
        APPROVAL_KIND_TOKEN_HOOK,
        APPROVAL_KIND_SCRIPT,
        MIN_SCRIPT_INTERVAL,
        MAX_SCRIPT_INTERVAL,
        MAX_SCRIPT_INIT_LEN,
        MAX_SCRIPT_STATE_LEN,
        SCRIPT_BOUNTY,
        SCRIPT_AFTER_LAUNCH,
        AUTHOR_SHARE_BPS,
        hex(&COMPANION_INIT_DISCRIMINATOR),
        hex(&COMPANION_STEP_DISCRIMINATOR),
        account("approval", &a, 8 + Approval::INIT_SPACE),
        account("script", &s, 8 + Script::INIT_SPACE),
        instructions.join(",\n")
    )
}

#[test]
fn the_script_vectors_are_current() {
    let json = render();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vectors/scripts.json");
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
