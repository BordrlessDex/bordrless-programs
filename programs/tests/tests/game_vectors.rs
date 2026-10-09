//! Vectors for the TypeScript SDK's mirror of companion v2 games (the monorepo's `packages/sdk`:
//! `companion.ts`, `game.ts`, `lotteryHook.ts`, `orao.ts`): `programs/tests/vectors/companion-games.json`
//! holds, for fixed keys, every game instruction exactly as the Rust clients build it
//! (`bordrless_companion::client`, `lottery_hook::client`; the kit path's steps too, which the
//! game path must leave unchanged), the accounts as the programs serialize them (`Game`,
//! `HookStatus`, `Companion` with its v2 fields, `LotteryState`), the game ticket standard
//! (`bordrless-game`: the header, the slots, `total_of`, `wins`, `eligible`, the round arithmetic,
//! `draw_index`), the game's clocks (`Game::claims_end`, the attempts' windows, dormancy, the
//! oracle's backoff, the hook terms) and the oracle module's seed, addresses and network state.
//!
//! As with `vectors.rs`: when the file differs from what the Rust reference computes, this test
//! rewrites it and fails, so a stale file never passes. The SDK keeps a copy next to its IDLs.

use std::path::PathBuf;

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::{AccountSerialize, Space};
use bordrless_companion::client::{self as companion, SeedSlot};
use bordrless_companion::constants::*;
use bordrless_companion::instructions::{CreateArgs, CreateGameArgs, HookStatusArgs};
use bordrless_companion::oracle;
use bordrless_companion::state::{
    claims_end, Companion, DrawStatus, Game, GameKind, HookStatus, HookTerms, Split,
};
use bordrless_game::{
    draw_index, eligible, round_end, round_of, round_start, valid_round_secs, wins, GameHeader,
    Range, Slots,
};
use bordrless_launch::client::{self as launch_client, CustomHookAccounts, LaunchKeys};
use bordrless_launch::instructions::CreateLaunchArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::launch::presets;
use lottery_hook::{client as lottery, LotteryState};

// ------------------------------------------------------------------------------------------ JSON

/// A JSON value, rendered deterministically. Integers that may exceed 2^53 are strings.
enum J {
    Null,
    Bool(bool),
    Num(i64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(&'static str, J)>),
}

fn num(n: impl Into<i64>) -> J {
    J::Num(n.into())
}

fn big(n: impl Into<i128>) -> J {
    J::Str(n.into().to_string())
}

fn s(text: impl Into<String>) -> J {
    J::Str(text.into())
}

fn key(k: &Pubkey) -> J {
    s(k.to_string())
}

fn hex(bytes: &[u8]) -> J {
    s(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

fn opt(v: Option<J>) -> J {
    v.unwrap_or(J::Null)
}

fn inline(j: &J, out: &mut String) {
    match j {
        J::Null => out.push_str("null"),
        J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        J::Num(n) => out.push_str(&n.to_string()),
        J::Str(text) => {
            out.push('"');
            out.push_str(text);
            out.push('"');
        }
        J::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                inline(item, out);
            }
            out.push(']');
        }
        J::Obj(fields) => {
            out.push('{');
            for (i, (k, v)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push('"');
                out.push_str(k);
                out.push_str("\": ");
                inline(v, out);
            }
            out.push('}');
        }
    }
}

/// The file: the top-level fields one per line, each section's entries one per line.
fn render(fields: &[(&'static str, J)]) -> String {
    let mut out = String::from("{\n");
    for (i, (k, v)) in fields.iter().enumerate() {
        out.push_str(&format!("  \"{k}\": "));
        match v {
            J::Arr(items) if !items.is_empty() => {
                out.push_str("[\n");
                for (n, item) in items.iter().enumerate() {
                    out.push_str("    ");
                    inline(item, &mut out);
                    if n + 1 < items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str("  ]");
            }
            J::Obj(entries) if !entries.is_empty() => {
                out.push_str("{\n");
                for (n, (ek, ev)) in entries.iter().enumerate() {
                    out.push_str(&format!("    \"{ek}\": "));
                    inline(ev, &mut out);
                    if n + 1 < entries.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str("  }");
            }
            other => inline(other, &mut out),
        }
        if i + 1 < fields.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("}\n");
    out
}

// ------------------------------------------------------------------------------------------ keys

/// A fixed key: 32 bytes of `n` (a key for addresses only; nobody signs with it).
fn fixed(n: u8) -> Pubkey {
    Pubkey::new_from_array([n; 32])
}

/// An address on the ed25519 curve (a wallet), for `eligible`: ORAO's treasury on mainnet, an
/// ordinary key.
const ON_CURVE: Pubkey = Pubkey::from_str_const("9ZTHWWZDpB36UFe1vszf2KEpt83vwi27jDqtHQ7NSXyR");

struct Keys {
    mint: Pubkey,
    payer: Pubkey,
    cranker: Pubkey,
    beneficiary: Pubkey,
    winner: Pubkey,
    authority: Pubkey,
    treasury: Pubkey,
    seed: [u8; 32],
    paid_seed: [u8; 32],
    /// The slot a draw names for its seed, and its hash.
    seed_slot: u64,
    slot_hash: [u8; 32],
}

fn keys() -> Keys {
    Keys {
        mint: fixed(1),
        payer: fixed(2),
        cranker: fixed(3),
        beneficiary: fixed(4),
        winner: fixed(5),
        authority: fixed(6),
        treasury: ON_CURVE,
        seed: [7; 32],
        paid_seed: [8; 32],
        seed_slot: 454_000_123,
        slot_hash: [9; 32],
    }
}

fn keys_json(k: &Keys) -> J {
    J::Obj(vec![
        ("mint", key(&k.mint)),
        ("payer", key(&k.payer)),
        ("cranker", key(&k.cranker)),
        ("beneficiary", key(&k.beneficiary)),
        ("winner", key(&k.winner)),
        ("authority", key(&k.authority)),
        ("treasury", key(&k.treasury)),
        ("seed", hex(&k.seed)),
        ("paidSeed", hex(&k.paid_seed)),
        ("seedSlot", big(k.seed_slot)),
        ("slotHash", hex(&k.slot_hash)),
        ("onCurve", key(&ON_CURVE)),
    ])
}

// ------------------------------------------------------------------------------------------ inputs

fn create_args() -> CreateArgs {
    CreateArgs {
        split: Split {
            buyback_bps: 10_000,
            holders_bps: 0,
            beneficiary_bps: 0,
        },
        bounty_bps: 50,
        max_buyback: 1_000_000_000,
        buyback_interval: 60,
        vest_secs: 30 * 86_400,
        fund: 500_000_000,
    }
}

fn game_args() -> CreateGameArgs {
    CreateGameArgs {
        kind: GameKind::Lottery,
        hook: LOTTERY_HOOK_ID,
        split: Split {
            buyback_bps: 3_000,
            holders_bps: 0,
            beneficiary_bps: 0,
        },
        pot_bps: 7_000,
        round_secs: 21_600,
        min_pot: 500_000_000,
        prize_bps: 10_000,
        claim_window_secs: 600,
        max_attempts: 8,
    }
}

fn status_args() -> HookStatusArgs {
    HookStatusArgs {
        audited: false,
        pot_cap: 5_000_000_000,
        blocked: true,
    }
}

fn launch_args() -> CreateLaunchArgs {
    CreateLaunchArgs {
        name: "Lottery coin".to_string(),
        symbol: "LOTTO".to_string(),
        uri: "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi".to_string(),
        creator_fee_bps: 200,
        virtual_quote: 28_125_000_000,
        rules: LaunchRules::NONE,
    }
}

/// A lottery coin's keys: no kit, the lottery hook; `burns` when its config burns on trades.
fn game_keys(mint: Pubkey, burns: bool) -> LaunchKeys {
    LaunchKeys {
        mint,
        quote_mint: BRIDGED_SOL_MINT,
        lp_fee_bps: 30,
        modules: 0,
        burns,
        custom_hook: Some(LOTTERY_HOOK_ID),
    }
}

fn custom(mint: &Pubkey) -> CustomHookAccounts {
    CustomHookAccounts {
        program: LOTTERY_HOOK_ID,
        extras: lottery::extras(mint),
    }
}

// ------------------------------------------------------------------------------------------ sections

fn ix(name: &str, i: &Instruction) -> J {
    J::Obj(vec![
        ("name", s(name)),
        ("program", key(&i.program_id)),
        (
            "accounts",
            J::Arr(
                i.accounts
                    .iter()
                    .map(|m: &AccountMeta| {
                        J::Arr(vec![
                            key(&m.pubkey),
                            J::Bool(m.is_signer),
                            J::Bool(m.is_writable),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("data", hex(&i.data)),
    ])
}

fn instructions(k: &Keys) -> J {
    let mint = k.mint;
    let hook = LOTTERY_HOOK_ID;
    let request = oracle::request_address(&k.seed);
    let at = SeedSlot {
        slot: k.seed_slot,
        hash: k.slot_hash,
    };
    let game = game_keys(mint, false);
    let burning = game_keys(mint, true);
    let lotto = custom(&mint);
    let kit_keys = LaunchKeys::new(mint, BRIDGED_SOL_MINT, 30, &presets::every_rule());
    let config = fixed(9);
    let create_launch = launch_client::create_launch_with(
        companion::creator_address(&mint),
        mint,
        fixed(10),
        BRIDGED_SOL_MINT,
        30,
        launch_args(),
        Some(config),
        Some(&lotto),
    );
    let all: Vec<(&str, Instruction)> = vec![
        // The setup transaction of a lottery coin, in order: the companion, the hook, the game.
        (
            "create",
            companion::create(k.payer, k.beneficiary, mint, create_args()),
        ),
        (
            "lotteryPrepare",
            lottery::prepare(k.payer, mint, game_args().round_secs),
        ),
        (
            "createGame",
            companion::create_game(k.payer, mint, game_args()),
        ),
        // The launch from a config naming the lottery hook, through the companion.
        (
            "launch",
            companion::launch(k.payer, mint, &create_launch, launch_args()),
        ),
        // The token's moves with the custom hook.
        (
            "devBuyGame",
            companion::dev_buy_with(k.beneficiary, &game, 1_000_000_000, 7, Some(&lotto)),
        ),
        (
            "buybackGame",
            companion::buyback_with(k.cranker, &game, false, Some(&lotto)),
        ),
        (
            "buybackGameBurns",
            companion::buyback_with(k.cranker, &burning, false, Some(&lotto)),
        ),
        (
            "releaseGame",
            companion::release_with(k.cranker, &game, false, k.beneficiary, Some(&lotto)),
        ),
        (
            "claimFeesGame",
            companion::claim_fees_game(k.cranker, mint, hook),
        ),
        // The draw: its seed committed from the named slot and requested, in one instruction.
        (
            "draw",
            companion::draw(k.cranker, mint, hook, 81_234, at, k.treasury),
        ),
        (
            "drawAfter",
            companion::draw_after(k.cranker, mint, hook, 81_234, at, k.treasury, k.paid_seed),
        ),
        ("reveal", companion::reveal(k.cranker, mint, hook, request)),
        (
            "claimPrize",
            companion::claim_prize(k.cranker, mint, hook, 3, k.winner),
        ),
        ("expire", companion::expire(k.cranker, mint, hook, request)),
        ("retire", companion::retire(k.cranker, mint, hook)),
        (
            "burnStranded",
            companion::burn_stranded(k.cranker, mint, hook),
        ),
        (
            "setHookStatus",
            companion::set_hook_status(k.authority, fixed(11), status_args()),
        ),
        // The lottery hook's own.
        ("lotteryEnter", lottery::enter(mint, k.winner)),
        // The kit path, which the game path must leave as it was.
        (
            "devBuyKit",
            companion::dev_buy(k.beneficiary, &kit_keys, 1_000_000_000, 7),
        ),
        ("buybackKit", companion::buyback(k.cranker, &kit_keys, true)),
        (
            "releaseKit",
            companion::release(k.cranker, &kit_keys, true, k.beneficiary),
        ),
        ("claimFeesKit", companion::claim_fees(k.cranker, mint, None)),
    ];
    J::Arr(all.iter().map(|(name, i)| ix(name, i)).collect())
}

fn serialize<T: AccountSerialize>(account: &T) -> Vec<u8> {
    let mut data = Vec::new();
    account.try_serialize(&mut data).expect("serialize");
    data
}

fn sample_game() -> Game {
    Game {
        version: 1,
        bump: 253,
        kind: GameKind::Lottery,
        mint: fixed(1),
        hook: LOTTERY_HOOK_ID,
        state_bump: 254,
        status_bump: 252,
        oracle_bump: 251,
        round_secs: 21_600,
        min_pot: 500_000_000,
        prize_bps: 5_000,
        claim_window_secs: 600,
        max_attempts: 8,
        created_at: 1_790_000_000,
        status: DrawStatus::Revealed,
        next_round: 82_871,
        round: 82_870,
        total: 987_654_321_012_345,
        n: 0,
        seed: [7; 32],
        request: oracle::request_address(&[7; 32]),
        committed_at: 1_790_000_100,
        requested_at: 1_790_000_101,
        randomness: core::array::from_fn(|i| i as u8),
        revealed_at: 1_790_000_160,
        prize: 1_234_567_890,
        draws: 41,
        prizes_paid: 37,
        prizes_total: 98_765_432_100,
        rollovers: 4,
        oracle_total: 76_543_210,
        last_winner: fixed(5),
        settled_at: 1_789_999_000,
        paid_seed: [8; 32],
        paid_round: 82_866,
        paid_streak: 3,
        reserved: [0; 83],
    }
}

fn sample_status() -> HookStatus {
    HookStatus {
        version: 1,
        bump: 250,
        hook: fixed(11),
        audited: false,
        pot_cap: 5_000_000_000,
        blocked: true,
        updated_at: 1_790_000_500,
        updated_by: fixed(6),
        reserved: [0; 32],
    }
}

fn sample_companion() -> Companion {
    Companion {
        version: 1,
        bump: 255,
        creator_bump: 254,
        mint: fixed(1),
        beneficiary: fixed(4),
        split: Split {
            buyback_bps: 3_000,
            holders_bps: 0,
            beneficiary_bps: 0,
        },
        bounty_bps: 50,
        max_buyback: 1_000_000_000,
        buyback_interval: 60,
        vest_secs: 2_592_000,
        launched: true,
        launched_at: 1_790_000_000,
        dev_tokens: 11,
        dev_released: 12,
        pending_buyback: 13,
        pending_holders: 0,
        pending_beneficiary: 0,
        last_buyback_at: 1_790_000_060,
        claimed_total: 14,
        spent_total: 15,
        burned_total: 16,
        shared_total: 0,
        paid_beneficiary_total: 0,
        bounties_total: 17,
        reference_price: 340_282_366_920_938_463_463,
        reference_at: 1_790_000_061,
        game_hook: LOTTERY_HOOK_ID,
        pot_bps: 7_000,
        pending_pot: 9_876_543_210,
        round_secs: 21_600,
        stranded_burned_at: 1_792_600_000,
        reserved: [0; 10],
    }
}

fn sample_header() -> GameHeader {
    GameHeader {
        magic: bordrless_game::MAGIC,
        mint: fixed(1),
        round_secs: 21_600,
        round: 82_871,
        total: 123_456_789_012,
        prev_round: 82_868,
        prev_total: 98_765,
        last_buyer: fixed(12),
        last_amount: 5,
        last_buy_at: 1_790_000_123,
    }
}

fn sample_lottery_state() -> LotteryState {
    LotteryState {
        header: sample_header(),
        version: 1,
        bump: 249,
        launch: bordrless_game::launch_address(&fixed(1)),
        pool: fixed(13),
        creator: bordrless_game::companion_creator_address(&fixed(1)),
        prepared_by: fixed(2),
        prepared_at: 1_789_990_000,
        reserved: [0; 64],
    }
}

fn accounts() -> J {
    J::Obj(vec![
        ("game", hex(&serialize(&sample_game()))),
        ("hookStatus", hex(&serialize(&sample_status()))),
        ("companion", hex(&serialize(&sample_companion()))),
        ("lotteryState", hex(&serialize(&sample_lottery_state()))),
        ("gameLen", num(Game::LEN as i64)),
        ("hookStatusLen", num(HookStatus::LEN as i64)),
        ("companionLen", num(Companion::LEN as i64)),
        ("lotteryStateLen", num(8 + LotteryState::INIT_SPACE as i64)),
    ])
}

fn header_json(h: &GameHeader) -> J {
    J::Obj(vec![
        ("mint", key(&h.mint)),
        ("roundSecs", num(h.round_secs)),
        ("round", num(h.round)),
        ("total", big(h.total)),
        ("prevRound", num(h.prev_round)),
        ("prevTotal", big(h.prev_total)),
        ("lastBuyer", key(&h.last_buyer)),
        ("lastAmount", big(h.last_amount)),
        ("lastBuyAt", big(h.last_buy_at)),
    ])
}

fn range_json(r: &Range) -> J {
    J::Obj(vec![
        ("round", num(r.round)),
        ("start", big(r.start)),
        ("weight", big(r.weight)),
    ])
}

fn slots_json(sl: &Slots) -> J {
    J::Obj(vec![
        ("current", range_json(&sl.current)),
        ("previous", range_json(&sl.previous)),
        ("since", big(sl.since)),
        ("free", hex(&sl.free)),
    ])
}

/// Raw hook data: `round`, `start`, `weight` of each slot, `since`, the free bytes.
fn raw(cur: (u32, u64, u64), prev: (u32, u64, u64), since: i64, free: u8) -> [u8; 64] {
    let mut d = [0u8; 64];
    d[0..4].copy_from_slice(&cur.0.to_le_bytes());
    d[4..12].copy_from_slice(&cur.1.to_le_bytes());
    d[12..20].copy_from_slice(&cur.2.to_le_bytes());
    d[20..24].copy_from_slice(&prev.0.to_le_bytes());
    d[24..32].copy_from_slice(&prev.1.to_le_bytes());
    d[32..40].copy_from_slice(&prev.2.to_le_bytes());
    d[40..48].copy_from_slice(&since.to_le_bytes());
    d[48..64].fill(free);
    d
}

fn standard() -> J {
    let header = sample_header();
    let state = serialize(&sample_lottery_state());
    let totals: Vec<J> = [82_872, 82_871, 82_870, 82_869, 82_868, 82_867, 0]
        .iter()
        .map(|&r| J::Arr(vec![num(r), opt(header.total_of(r).map(big))]))
        .collect();
    // Hook data as written (decoded, and written back by `encode`), and as anyone could write it.
    let samples = [
        raw((82_871, 100, 50), (82_870, 7, 300), 1_790_000_000, 0),
        raw((82_871, 0, 0), (82_870, 7, 300), 1_790_000_000, 0xab),
        raw((82_871, 99, 0), (82_870, 7, 0), -1, 0),
        raw((0, 0, 0), (0, 0, 0), 0, 0),
        raw(
            (u32::MAX, u64::MAX - 1, 2),
            (u32::MAX - 1, u64::MAX, u64::MAX),
            i64::MAX,
            0xff,
        ),
    ];
    let slots: Vec<J> = samples
        .iter()
        .map(|d| {
            let decoded = Slots::decode(d);
            J::Obj(vec![
                ("data", hex(d)),
                ("slots", slots_json(&decoded)),
                ("encoded", hex(&decoded.encode())),
                (
                    "rangeIn",
                    J::Arr(
                        [82_871, 82_870, 0, u32::MAX, u32::MAX - 1]
                            .iter()
                            .map(|&r| opt(decoded.range_in(r).map(|x| range_json(&x))))
                            .collect(),
                    ),
                ),
            ])
        })
        .collect();
    let win_cases: Vec<J> = [
        (0usize, 82_871u32, 100u64, 50u64),
        (0, 82_871, 149, 50),
        (0, 82_871, 150, 1_000),
        (0, 82_871, 99, 1_000),
        (0, 82_871, 120, 49),
        (0, 82_870, 7, 300),
        (0, 82_870, 306, 300),
        (0, 82_870, 307, 300),
        (0, 82_870, 10, 299),
        (1, 82_871, 0, 1_000),
        (1, 82_870, 100, 300),
        (2, 82_871, 99, 1_000),
        (3, 0, 0, 1_000),
        (4, u32::MAX, u64::MAX - 1, 2),
        (4, u32::MAX, u64::MAX, 2),
        (4, u32::MAX - 1, u64::MAX - 1, u64::MAX),
    ]
    .iter()
    .map(|&(i, round, x, balance)| {
        J::Arr(vec![
            num(i as i64),
            num(round),
            big(x),
            big(balance),
            J::Bool(wins(&samples[i], round, x, balance)),
        ])
    })
    .collect();
    let mut ascending = [0u8; 64];
    for (i, b) in ascending.iter_mut().enumerate() {
        *b = i as u8;
    }
    let randomness: [(&str, Vec<u8>); 4] = [
        ("zeros", vec![0u8; 64]),
        ("ascending", ascending.to_vec()),
        ("ones", vec![0xff; 64]),
        ("short", vec![1u8; 32]),
    ];
    let totals_drawn = [0u64, 1, 7, 1_000_003, 1_000_000_000_000_000, u64::MAX];
    let mut draws = Vec::new();
    for (name, r) in &randomness {
        for &k in &[0u32, 1, 7, 15, 255, u32::MAX] {
            for &total in &totals_drawn {
                draws.push(J::Arr(vec![
                    s(*name),
                    num(k),
                    big(total),
                    opt(draw_index(r, k, total).map(big)),
                ]));
            }
        }
    }
    let rounds: Vec<J> = [
        (0i64, 3_600u32),
        (-5, 3_600),
        (3_599, 3_600),
        (3_600, 3_600),
        (1_790_000_000, 21_600),
        (1_790_000_000, 0),
        (i64::MAX, 3_600),
        (i64::MAX, 1),
    ]
    .iter()
    .map(|&(now, secs)| J::Arr(vec![big(now), num(secs), num(round_of(now, secs))]))
    .collect();
    let ends: Vec<J> = [
        (0u32, 3_600u32),
        (82_870, 21_600),
        (u32::MAX, u32::MAX),
        (u32::MAX - 1, 3_600),
    ]
    .iter()
    .map(|&(r, secs)| {
        J::Arr(vec![
            num(r),
            num(secs),
            big(round_start(r, secs)),
            big(round_end(r, secs)),
            big(claims_end(r, secs)),
        ])
    })
    .collect();
    let valid: Vec<J> = [0u32, 3_599, 3_600, 2_592_000, 2_592_001]
        .iter()
        .map(|&secs| J::Arr(vec![num(secs), J::Bool(valid_round_secs(secs))]))
        .collect();
    let launch = bordrless_game::launch_address(&fixed(1));
    let creator = bordrless_game::companion_creator_address(&fixed(1));
    let eligibility: Vec<J> = [
        (ON_CURVE, vec![]),
        (ON_CURVE, vec![launch, creator]),
        (ON_CURVE, vec![ON_CURVE]),
        (Pubkey::default(), vec![]),
        (launch, vec![]),
        (creator, vec![]),
    ]
    .iter()
    .map(|(owner, excluded)| {
        J::Arr(vec![
            key(owner),
            J::Arr(excluded.iter().map(key).collect()),
            J::Bool(eligible(owner, excluded)),
        ])
    })
    .collect();
    J::Obj(vec![
        ("state", hex(&state)),
        ("header", header_json(&header)),
        ("headerBytes", hex(&header.encode())),
        ("totalOf", J::Arr(totals)),
        ("slots", J::Arr(slots)),
        ("wins", J::Arr(win_cases)),
        (
            "randomness",
            J::Obj(
                randomness
                    .iter()
                    .map(|(name, r)| (*name, hex(r)))
                    .collect::<Vec<_>>(),
            ),
        ),
        ("drawIndex", J::Arr(draws)),
        ("roundOf", J::Arr(rounds)),
        ("roundEnds", J::Arr(ends)),
        ("validRoundSecs", J::Arr(valid)),
        ("eligible", J::Arr(eligibility)),
        ("launch", key(&launch)),
        ("companionCreator", key(&creator)),
        ("stateAddress", key(&lottery::state_address(&fixed(1)))),
    ])
}

/// The game's clocks as the steps read them, for a sample game (`sample_game`) and variations.
fn clocks() -> J {
    let g = sample_game();
    let launched_at = 1_790_000_000i64;
    let attempts: Vec<J> = (0..=g.max_attempts)
        .map(|a| {
            J::Arr(vec![
                num(a),
                opt(g.attempt_opens(a).map(big)),
                opt(g.attempt_closes(a).map(big)),
            ])
        })
        .collect();
    let late = Game {
        revealed_at: claims_end(g.round, g.round_secs) - 1_000,
        ..g.clone()
    };
    let late_attempts: Vec<J> = (0..=late.max_attempts)
        .map(|a| {
            J::Arr(vec![
                num(a),
                opt(late.attempt_opens(a).map(big)),
                opt(late.attempt_closes(a).map(big)),
            ])
        })
        .collect();
    let dormant_at = g.idle_since(launched_at) + g.dormant_secs();
    let min_pots: Vec<J> = [dormant_at - 1, dormant_at, dormant_at + 1]
        .iter()
        .map(|&now| J::Arr(vec![big(now), big(g.min_pot_at(now, launched_at))]))
        .collect();
    let mut b = g.clone();
    let backoff: Vec<J> = (0..=13u8)
        .chain([255])
        .map(|streak| {
            b.paid_streak = streak;
            J::Arr(vec![
                num(streak),
                big(b.oracle_backoff_rounds()),
                J::Bool(b.oracle_backoff_over()),
            ])
        })
        .collect();
    let long = Game {
        round_secs: 30 * 86_400,
        settled_at: 0,
        ..g.clone()
    };
    let terms = [
        HookTerms::DEFAULT,
        HookTerms {
            audited: true,
            ..HookTerms::DEFAULT
        },
        HookTerms {
            pot_cap: u64::MAX,
            ..HookTerms::DEFAULT
        },
        HookTerms {
            pot_cap: MIN_POT_CAP,
            ..HookTerms::DEFAULT
        },
        HookTerms {
            blocked: true,
            pot_cap: 2_000_000_000,
            ..HookTerms::DEFAULT
        },
    ];
    let terms_json: Vec<J> = terms
        .iter()
        .map(|t| {
            J::Arr(vec![
                J::Bool(t.audited),
                big(t.pot_cap),
                J::Bool(t.blocked),
                opt(t.cap().map(big)),
                big(t.draw_threshold(MIN_MIN_POT)),
                big(t.draw_threshold(MAX_MIN_POT)),
                big(t.draw_threshold(3_000_000_000)),
            ])
        })
        .collect();
    // `burn_stranded`'s clock, for the sample companion with its buybacks a minute or 30 days
    // apart and the status written at three times.
    let mut stranded = Vec::new();
    for interval in [60i64, 30 * 86_400] {
        let c = Companion {
            buyback_interval: interval,
            ..sample_companion()
        };
        for updated_at in [0i64, 1_790_000_000, 1_795_000_000] {
            stranded.push(J::Arr(vec![
                big(interval),
                big(updated_at),
                big(c.stranded_since(updated_at)),
                big(c.stranded_at(updated_at)),
            ]));
        }
    }
    J::Obj(vec![
        ("launchedAt", big(launched_at)),
        ("claimsEnd", big(g.claims_end())),
        ("lastDraw", big(g.last_draw(g.round))),
        ("attempts", J::Arr(attempts)),
        ("lateRevealedAt", big(late.revealed_at)),
        ("lateAttempts", J::Arr(late_attempts)),
        ("dormantSecs", big(g.dormant_secs())),
        ("idleSince", big(g.idle_since(launched_at))),
        ("minPotAt", J::Arr(min_pots)),
        ("retirableAt", big(g.retirable_at(launched_at))),
        ("longDormantSecs", big(long.dormant_secs())),
        ("longRetirableAt", big(long.retirable_at(launched_at))),
        ("backoff", J::Arr(backoff)),
        ("terms", J::Arr(terms_json)),
        ("stranded", J::Arr(stranded)),
    ])
}

fn oracle_section() -> J {
    let seeds: Vec<J> = [
        (fixed(1), 82_870u32, 0u32, 454_000_000u64, [3u8; 32]),
        (fixed(9), 0, 0, 0, [0u8; 32]),
        (fixed(1), u32::MAX, u32::MAX, u64::MAX, [0xffu8; 32]),
    ]
    .iter()
    .map(|(mint, round, n, slot, hash)| {
        let seed = oracle::draw_seed(mint, *round, *n, *slot, hash);
        J::Arr(vec![
            key(mint),
            num(*round),
            num(*n),
            big(*slot),
            hex(hash),
            hex(&seed),
            key(&oracle::request_address(&seed)),
        ])
    })
    .collect();
    let path = format!(
        "{}/fixtures/orao_network_state.bin",
        env!("CARGO_MANIFEST_DIR")
    );
    let network_state = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    J::Obj(vec![
        ("program", key(&oracle::ORAO_VRF_ID)),
        ("networkState", key(&oracle::NETWORK_STATE)),
        (
            "networkStateDerived",
            key(
                &Pubkey::find_program_address(&[oracle::NETWORK_STATE_SEED], &oracle::ORAO_VRF_ID)
                    .0,
            ),
        ),
        ("slotHashes", key(&oracle::SLOT_HASHES)),
        ("pendingLen", num(oracle::PENDING_LEN as i64)),
        ("fulfilledLen", num(oracle::FULFILLED_LEN as i64)),
        ("v1Len", num(oracle::V1_LEN as i64)),
        ("maxRequestFee", big(oracle::MAX_REQUEST_FEE)),
        ("networkStateData", hex(&network_state)),
        (
            "requestIx",
            ix(
                "requestV2",
                &oracle::request_ix(
                    fixed(14),
                    &oracle::Terms {
                        treasury: ON_CURVE,
                        fee: 500_000,
                    },
                    [7; 32],
                ),
            ),
        ),
        ("drawSeeds", J::Arr(seeds)),
    ])
}

fn constants() -> J {
    let mint = fixed(1);
    J::Obj(vec![
        ("companionProgram", key(&bordrless_companion::ID)),
        ("lotteryHook", key(&LOTTERY_HOOK_ID)),
        ("lotteryHookFlags", num(LOTTERY_HOOK_FLAGS)),
        (
            "lotteryTokenHookSigner",
            key(&lottery_hook::TOKEN_HOOK_SIGNER),
        ),
        ("lotteryHookAuthority", key(&lottery_hook::HOOK_AUTHORITY)),
        ("lotteryEventAuthority", key(&lottery::event_authority())),
        (
            "companionProgramData",
            key(&companion::program_data_address()),
        ),
        ("incinerator", key(&INCINERATOR)),
        ("gameAddress", key(&companion::game_address(&mint))),
        (
            "hookStatusAddress",
            key(&companion::hook_status_address(&LOTTERY_HOOK_ID)),
        ),
        (
            "oraclePayerAddress",
            key(&companion::oracle_payer_address(&mint)),
        ),
        ("lotteryRegistry", key(&lottery::registry_address(&mint))),
        ("maxBountyBps", num(MAX_BOUNTY_BPS)),
        ("defaultPotCap", big(DEFAULT_POT_CAP)),
        ("minPotCap", big(MIN_POT_CAP)),
        ("minMinPot", big(MIN_MIN_POT)),
        ("maxMinPot", big(MAX_MIN_POT)),
        ("minPrizeBps", num(MIN_PRIZE_BPS)),
        ("minClaimWindow", num(MIN_CLAIM_WINDOW)),
        ("maxClaimWindow", num(MAX_CLAIM_WINDOW)),
        ("maxAttempts", num(MAX_ATTEMPTS)),
        ("claimsPerRound", num(CLAIMS_PER_ROUND)),
        ("revealSecs", big(REVEAL_SECS)),
        ("seedSlots", big(oracle::SEED_SLOTS)),
        ("dormantSecs", big(DORMANT_SECS)),
        ("dormantRounds", big(DORMANT_ROUNDS)),
        ("retireDormantPeriods", big(RETIRE_DORMANT_PERIODS)),
        ("maxGameHookExtras", num(MAX_GAME_HOOK_EXTRAS as i64)),
        ("strandedSecs", big(STRANDED_SECS)),
        ("strandedIntervals", big(STRANDED_INTERVALS)),
        ("minRoundSecs", num(bordrless_game::MIN_ROUND_SECS)),
        ("maxRoundSecs", num(bordrless_game::MAX_ROUND_SECS)),
    ])
}

fn vectors() -> Vec<(&'static str, J)> {
    let k = keys();
    vec![
        ("keys", keys_json(&k)),
        ("constants", constants()),
        ("accounts", accounts()),
        ("standard", standard()),
        ("clocks", clocks()),
        ("oracle", oracle_section()),
        ("instructions", instructions(&k)),
    ]
}

fn vectors_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vectors/companion-games.json")
}

#[test]
fn the_companion_game_vectors_are_current() {
    let json = render(&vectors());
    let path = vectors_path();
    let on_disk = std::fs::read_to_string(&path).ok();
    println!("{}: {} bytes", path.display(), json.len());
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

/// The samples are what they say: a game serialized at the account's size, the standard's header
/// at its offsets inside the hook's state, and the draw index the crate's own vectors give.
#[test]
fn the_samples_hold_together() {
    assert_eq!(serialize(&sample_game()).len(), Game::LEN);
    assert_eq!(serialize(&sample_status()).len(), HookStatus::LEN);
    assert_eq!(serialize(&sample_companion()).len(), Companion::LEN);
    let state = serialize(&sample_lottery_state());
    assert_eq!(
        GameHeader::read(&state, &fixed(1)).expect("a header"),
        sample_header()
    );
    assert_eq!(
        draw_index(&[0u8; 64], 0, u64::MAX),
        Some(12_976_294_286_951_469_335)
    );
    let mut ascending = [0u8; 64];
    for (i, b) in ascending.iter_mut().enumerate() {
        *b = i as u8;
    }
    assert_eq!(draw_index(&ascending, 7, 1_000_003), Some(359_196));
}
