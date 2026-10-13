//! Snowball's vectors for the SDK (`packages/sdk/src/snowball.test.ts`):
//! `programs/tests/vectors/snowball.json`, rendered from `bordrless_companion::client` and the
//! account layout. A stale file is rewritten and the test fails, so a change is always reviewed.

use std::path::PathBuf;

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{AccountSerialize, Space};
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::instructions::CreateSnowballArgs;
use bordrless_companion::state::{Snowball, Split};
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

fn terms() -> CreateSnowballArgs {
    CreateSnowballArgs {
        split: Split {
            buyback_bps: 0,
            holders_bps: 0,
            beneficiary_bps: 3_000,
        },
        pot_bps: 7_000,
        buy_step_bps: 200,
        sell_step_bps: 300,
        buy_bps: 2_500,
        sell_bps: 5_000,
        interval: 30,
        max_trade: 100_000_000_000,
        pool_bps: 200,
        flat_trades: true,
    }
}

fn keys(mint: Pubkey, modules: u8, burns: bool) -> LaunchKeys {
    LaunchKeys {
        mint,
        quote_mint: BRIDGED_SOL_MINT,
        lp_fee_bps: 30,
        modules,
        burns,
        custom_hook: None,
    }
}

fn sample() -> Snowball {
    Snowball {
        version: SNOWBALL_VERSION,
        bump: 254,
        mint: key(7),
        buy_step_bps: 200,
        sell_step_bps: 300,
        buy_bps: 2_500,
        sell_bps: 1_000,
        interval: 30,
        max_trade: 100_000_000_000,
        pool_bps: 200,
        flat_trades: true,
        reference_price: 123_456_789_012_345,
        last_at: 1_760_000_000,
        tokens_held: 42_000_000_000,
        buys: 3,
        sells: 2,
        sol_spent: 900_000_000,
        sol_received: 700_000_000,
        tokens_bought: 60_000_000_000,
        tokens_sold: 18_000_000_000,
        bounties_total: 8_000_000,
        created_at: 1_759_990_000,
        reserved: [0; 64],
    }
}

fn render() -> String {
    let (mint, payer, cranker) = (key(7), key(1), key(2));
    let mut data = Vec::new();
    sample().try_serialize(&mut data).expect("serialize");
    let instructions = [
        ix(
            "createSnowball",
            &companion::create_snowball(payer, mint, terms()),
        ),
        ix(
            "snowballStepPlain",
            &companion::snowball_step(cranker, &keys(mint, 0, false)),
        ),
        // Holder rewards and a burn (modules 1): the kit's extras, the holder vault among them.
        ix(
            "snowballStepKit",
            &companion::snowball_step(cranker, &keys(mint, 1, true)),
        ),
    ];
    format!(
        "{{\n  \"keys\": {{\"mint\": \"{mint}\", \"payer\": \"{payer}\", \"cranker\": \"{cranker}\"}},\n  \
         \"snowballAddress\": \"{}\",\n  \
         \"constants\": {{\"minStepBps\": {}, \"maxStepBps\": {}, \"minSizeBps\": {}, \"maxSizeBps\": {}, \
         \"minPoolBps\": {}, \"maxPoolBps\": {}, \"minInterval\": {}, \"maxInterval\": {}, \"minTrade\": \"{}\", \"minMaxTrade\": \"{}\", \"modeSnowball\": {}}},\n  \
         \"account\": {{\"len\": {}, \"data\": \"{}\"}},\n  \
         \"instructions\": [\n{}\n  ]\n}}\n",
        companion::snowball_address(&mint),
        MIN_SNOWBALL_STEP_BPS,
        MAX_SNOWBALL_STEP_BPS,
        MIN_SNOWBALL_SIZE_BPS,
        MAX_SNOWBALL_SIZE_BPS,
        MIN_SNOWBALL_POOL_BPS,
        MAX_SNOWBALL_POOL_BPS,
        MIN_SNOWBALL_INTERVAL,
        MAX_SNOWBALL_INTERVAL,
        MIN_SNOWBALL_TRADE,
        MIN_SNOWBALL_MAX_TRADE,
        MODE_SNOWBALL,
        8 + Snowball::INIT_SPACE,
        hex(&data),
        instructions.join(",\n")
    )
}

#[test]
fn the_snowball_vectors_are_current() {
    let json = render();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vectors/snowball.json");
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
