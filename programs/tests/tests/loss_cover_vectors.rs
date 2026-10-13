//! Loss cover's vectors for the SDK (2026-10-12): `programs/tests/vectors/loss-cover.json`,
//! rendered from the hook's and the script's clients and layouts: the program ids and addresses,
//! the constants, the state, record and terms layouts with sample bytes, `prepare` and
//! `record_payout`, the registry's extras, the `CoverRound` event, and worked examples of the
//! arithmetic the SDK mirrors (`lossCoverOwed`, `lossCoverFraction`, the entry price, the cap, the
//! reference). A stale file is rewritten and the test fails, so a change is always reviewed.

use std::path::PathBuf;

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{AccountSerialize, AnchorSerialize, Discriminator, Space};
use bordrless_strategy::PoolReserves;
use loss_cover_hook::{
    capped, client as hook, record_offsets, spot_of, step_toward, trade_price, LossCoverState,
    Record, ENTRY_CAP_BPS, FLAGS, FLAG_SOLD, HOOK_AUTHORITY, HOOK_AUTHORITY_BUMP, PAYOUT_AUTHORITY,
    PRICE_SCALE, REFERENCE_SECS, REFERENCE_STEP_BPS, TOKEN_HOOK_SIGNER,
};
use loss_cover_script::{
    fraction_of, uncovered_loss, CoverRound, CoverState, CoverTerms, MAX_COVER_BPS,
    MAX_MIN_AGE_SECS, MAX_MIN_POT_LAMPORTS, MAX_ROUND_SECS, MIN_COVER_BPS, MIN_ROUND_SECS,
    STATE_LEN,
};

/// The keeper's batches: what a step carries within a packet and the runtime's instruction trace
/// of 64 (`loss_cover.rs`: a touch costs four inner instructions, a payment five, on a base of
/// eleven with the compute budget's two instructions): 7 paid holdings (a holding and its owner
/// each; 10 at most), 13 touched (a holding each; the most).
pub const BATCH_PAYING: usize = 7;
pub const BATCH_MEASURING: usize = 13;

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

fn account(name: &str, data: &[u8], len: usize) -> String {
    format!(
        "    \"{name}\": {{\"len\": {len}, \"discriminator\": \"{}\", \"data\": \"{}\"}}",
        hex(&data[..8]),
        hex(data)
    )
}

fn state() -> LossCoverState {
    LossCoverState {
        version: loss_cover_hook::VERSION,
        bump: 254,
        mint: key(7),
        launch: key(3),
        pool: key(4),
        creator: key(5),
        reference: 40_166_666,
        reference_at: 1_800_000_000,
        paid_total: 123_456_789,
        records: 42,
        prepared_by: key(1),
        prepared_at: 1_799_990_000,
        reserved: [0; 64],
    }
}

fn record() -> Record {
    Record {
        version: 1,
        flags: FLAG_SOLD,
        entry: 40_166_666,
        amount: 100_000_000_000,
        paid: 1_000_000,
        updated_slot: 400_000_123,
        paid_at: 1_800_000_100,
        basis_at: 1_799_999_000,
        round: 6_000_000,
    }
}

fn terms() -> CoverTerms {
    CoverTerms {
        cover_bps: 10_000,
        min_holding: 100_000_000_000,
        min_loss: 1_000_000,
        min_age_secs: 1_800,
        round_secs: 300,
        min_pot_lamports: 1_000_000_000,
    }
}

fn cover_state() -> CoverState {
    CoverState {
        version: loss_cover_script::VERSION,
        mint: key(7),
        cover_bps: 10_000,
        min_holding: 100_000_000_000,
        min_loss: 1_000_000,
        min_age_secs: 1_800,
        round_secs: 300,
        min_pot_lamports: 1_000_000_000,
        round: 6_000_002,
        b: 500_000_000,
        l_prev: 5_000_000_000,
        l_cur: 1_234_567_890,
        round_paid: 450_000_000,
        measured: 12,
        rounds: 3,
        paid_total: 2_000_000_000,
        reserved: [0; 1],
    }
}

fn render() -> String {
    let (mint, payer, owner) = (key(7), key(1), key(20));
    let (mut s, mut cs, mut t) = (Vec::new(), Vec::new(), Vec::new());
    state().try_serialize(&mut s).expect("serialize");
    cover_state().try_serialize(&mut cs).expect("serialize");
    terms().serialize(&mut t).expect("serialize");
    let rec = record().encode();
    let mut round = Vec::new();
    CoverRound {
        mint,
        round: 6_000_002,
        b: 500_000_000,
        l_prev: 5_000_000_000,
    }
    .serialize(&mut round)
    .expect("serialize");
    let extras: Vec<String> = hook::extras(&mint, 30)
        .iter()
        .map(|m| format!("[\"{}\", {}, {}]", m.pubkey, m.is_signer, m.is_writable))
        .collect();
    let instructions = [
        ix("prepare", &hook::prepare(payer, mint)),
        ix(
            "recordPayout",
            &hook::record_payout(mint, owner, 1_000_000, 6_000_001),
        ),
    ];
    // The arithmetic: a pool after a buy, the entry it sets, the spot, the cap, the reference's
    // step, what a holding is owed and what a round pays it.
    let pool = PoolReserves {
        base_mint: mint,
        quote_mint: key(8),
        lp_fee_bps: 30,
        base_reserve: 700_000_000_000_000,
        quote_reserve: 2_000_000_000,
        virtual_base: 50_000_000_000_000,
        virtual_quote: 28_125_000_000,
    };
    let delivered = 50_000_000_000_000u64;
    let spot = spot_of(&pool).unwrap();
    let entry = trade_price(&pool, delivered).unwrap();
    let math = format!(
        "{{\"pool\": {{\"baseReserve\": \"{}\", \"quoteReserve\": \"{}\", \"virtualBase\": \"{}\", \"virtualQuote\": \"{}\"}}, \"delivered\": \"{delivered}\", \"spot\": \"{spot}\", \"entry\": \"{entry}\", \"capped\": [{{\"price\": \"1300\", \"reference\": \"1000\", \"out\": \"{}\"}}, {{\"price\": \"1300\", \"reference\": \"0\", \"out\": \"{}\"}}], \"stepToward\": [{{\"reference\": \"1000\", \"spot\": \"2000\", \"out\": \"{}\"}}, {{\"reference\": \"1000\", \"spot\": \"10\", \"out\": \"{}\"}}, {{\"reference\": \"0\", \"spot\": \"777\", \"out\": \"{}\"}}], \"owed\": [{{\"entry\": \"20000000\", \"spot\": \"10000000\", \"balance\": \"1000000000000\", \"coverBps\": 5000, \"paid\": \"1000000\", \"out\": \"{}\"}}, {{\"entry\": \"10000000\", \"spot\": \"10000000\", \"balance\": \"1\", \"coverBps\": 10000, \"paid\": \"0\", \"out\": \"{}\"}}], \"fraction\": [{{\"uncovered\": \"1000\", \"b\": \"100\", \"lPrev\": \"1000\", \"out\": \"{}\"}}, {{\"uncovered\": \"1000\", \"b\": \"3000\", \"lPrev\": \"1000\", \"out\": \"{}\"}}, {{\"uncovered\": \"1000\", \"b\": \"500\", \"lPrev\": \"0\", \"out\": \"{}\"}}]}}",
        pool.base_reserve,
        pool.quote_reserve,
        pool.virtual_base,
        pool.virtual_quote,
        capped(1_300, 1_000),
        capped(1_300, 0),
        step_toward(1_000, 2_000),
        step_toward(1_000, 10),
        step_toward(0, 777),
        uncovered_loss(20_000_000, 10_000_000, 1_000_000_000_000, 5_000, 1_000_000),
        uncovered_loss(10_000_000, 10_000_000, 1, 10_000, 0),
        fraction_of(1_000, 100, 1_000),
        fraction_of(1_000, 3_000, 1_000),
        fraction_of(1_000, 500, 0),
    );
    format!(
        "{{\n  \"programs\": {{\"hook\": \"{}\", \"script\": \"{}\"}},\n  \
         \"keys\": {{\"mint\": \"{mint}\", \"payer\": \"{payer}\", \"owner\": \"{owner}\"}},\n  \
         \"addresses\": {{\"state\": \"{}\", \"registry\": \"{}\", \"launch\": \"{}\", \"pool30\": \"{}\", \"hookAuthority\": \"{}\", \"hookAuthorityBump\": {}, \"tokenHookSigner\": \"{}\", \"payoutAuthority\": \"{}\", \"eventAuthority\": \"{}\"}},\n  \
         \"constants\": {{\"flags\": {}, \"priceScale\": \"{}\", \"entryCapBps\": {}, \"referenceSecs\": {}, \"referenceStepBps\": {}, \"flagSold\": {}, \"stateLen\": {}, \"recordLen\": 64, \"coverStateLen\": {}, \"coverTermsLen\": {}, \"minCoverBps\": {}, \"maxCoverBps\": {}, \"minRoundSecs\": {}, \"maxRoundSecs\": {}, \"maxMinAgeSecs\": {}, \"maxMinPotLamports\": \"{}\", \"batchPaying\": {}, \"batchMeasuring\": {}}},\n  \
         \"recordOffsets\": {{\"version\": {}, \"flags\": {}, \"entry\": {}, \"amount\": {}, \"paid\": {}, \"updatedSlot\": {}, \"paidAt\": {}, \"basisAt\": {}, \"round\": {}, \"reserved\": {}}},\n  \
         \"coverStateOffsets\": {{\"version\": 8, \"mint\": 9, \"coverBps\": 41, \"minHolding\": 43, \"minLoss\": 51, \"minAgeSecs\": 59, \"roundSecs\": 63, \"minPotLamports\": 67, \"round\": 75, \"b\": 79, \"lPrev\": 87, \"lCur\": 95, \"roundPaid\": 103, \"measured\": 111, \"rounds\": 115, \"paidTotal\": 119}},\n  \
         \"events\": {{\"CoverRound\": \"{}\"}},\n  \
         \"coverRound\": {{\"data\": \"{}\"}},\n  \
         \"record\": {{\"data\": \"{}\"}},\n  \
         \"coverTerms\": {{\"len\": {}, \"data\": \"{}\"}},\n  \
         \"accounts\": {{\n{},\n{}\n  }},\n  \
         \"extras\": [{}],\n  \
         \"math\": {math},\n  \
         \"instructions\": [\n{}\n  ]\n}}\n",
        loss_cover_hook::ID,
        loss_cover_script::ID,
        hook::state_address(&mint),
        hook::registry_address(&mint),
        hook::launch_address(&mint),
        hook::pool_address(&mint, 30),
        HOOK_AUTHORITY,
        HOOK_AUTHORITY_BUMP,
        TOKEN_HOOK_SIGNER,
        PAYOUT_AUTHORITY,
        hook::event_authority(),
        FLAGS,
        PRICE_SCALE,
        ENTRY_CAP_BPS,
        REFERENCE_SECS,
        REFERENCE_STEP_BPS,
        FLAG_SOLD,
        LossCoverState::LEN,
        STATE_LEN,
        CoverTerms::LEN,
        MIN_COVER_BPS,
        MAX_COVER_BPS,
        MIN_ROUND_SECS,
        MAX_ROUND_SECS,
        MAX_MIN_AGE_SECS,
        MAX_MIN_POT_LAMPORTS,
        BATCH_PAYING,
        BATCH_MEASURING,
        record_offsets::VERSION,
        record_offsets::FLAGS,
        record_offsets::ENTRY,
        record_offsets::AMOUNT,
        record_offsets::PAID,
        record_offsets::UPDATED_SLOT,
        record_offsets::PAID_AT,
        record_offsets::BASIS_AT,
        record_offsets::ROUND,
        record_offsets::RESERVED,
        hex(CoverRound::DISCRIMINATOR),
        hex(&round),
        hex(&rec),
        t.len(),
        hex(&t),
        account("lossCoverState", &s, 8 + LossCoverState::INIT_SPACE),
        account("coverState", &cs, 8 + CoverState::INIT_SPACE),
        extras.join(", "),
        instructions.join(",\n")
    )
}

#[test]
fn the_loss_cover_vectors_are_current() {
    let json = render();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vectors/loss-cover.json");
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
