//! Hook protocol v2 on the token standard, driven by the test-only `hook_tester` (answers scripted
//! per mint and per callback): up to three deltas, hook data by answer on transfer, mint and burn
//! and by `write_hook_data`, every rule an answer must follow, `close_holding` under hook data,
//! and registries created at addresses someone already funded.

use anchor_lang::prelude::Pubkey;
use bordrless_hook::{
    hook_accounts_address, token_flags, Delta, HookAccountList, HookReturn, Phase, TokenOp,
    HOOK_AUTHORITY_SEED,
};
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::hooks::{fixed, tax_install};
use bordrless_token::client as token;
use bordrless_token::error::TokenError;
use bordrless_token::events::{DeltaApplied, HookDataWritten, Transferred};
use bordrless_token::state::{Holding, Mint};
use hook_tester::client as tester;
use hook_tester::{callback, mode, Script};
use solana_keypair::Keypair;
use solana_signer::Signer;

const SUPPLY: u64 = 1_000_000_000;
const SOL: u64 = 1_000_000_000;
/// Index of the first test extra in a hook_tester callback: the prefix (5), then the script.
const X: u8 = 6;

fn code(e: TokenError) -> u32 {
    u32::from(e)
}

fn deltas(list: &[(u64, u8)]) -> HookReturn {
    HookReturn {
        deltas: list
            .iter()
            .map(|&(amount, account)| Delta { amount, account })
            .collect(),
        ..HookReturn::default()
    }
}

fn script(w: &World, mint: &Pubkey) -> Script {
    w.env.read(&tester::script_address(mint))
}

#[test]
fn three_deltas_credit_three_holdings() {
    let mut w = World::new();
    let alice = w.env.funded(10 * SOL);
    let bob = Pubkey::new_unique();
    let (r1, r2, r3) = (
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    );
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let h = |owner: &Pubkey| token::holding_address(&mint, owner);
    let flags = token_flags::BEFORE_TRANSFER
        | token_flags::AFTER_TRANSFER
        | token_flags::TRANSFER_RETURNS_DELTA;
    w.tester_mint(
        &mint_kp,
        &alice,
        flags,
        SUPPLY,
        vec![
            fixed(h(&r1), true),
            fixed(h(&r2), true),
            fixed(h(&r3), true),
        ],
    );
    w.holdings(&alice, mint, &[bob, r1, r2, r3]);
    let m: Mint = w.env.read(&mint);
    assert_eq!(
        (m.hook_program, m.hook_flags, m.supply),
        (Some(hook_tester::ID), flags, SUPPLY)
    );

    // Three cuts to three holdings the registry lists (indices 6, 7 and 8).
    w.script_answer(
        &alice,
        mint,
        callback::BEFORE_TRANSFER,
        &deltas(&[(10, X), (20, X + 1), (30, X + 2)]),
    );
    let tx = w.tester_transfer(&alice, mint, &alice.pubkey(), &bob, 1_000);
    println!(
        "transfer with three deltas (hook_tester) CU {} size {}",
        tx.cu(),
        tx.size
    );
    tx.ok();
    let ev: Transferred = tx.event();
    assert_eq!(
        (ev.amount, ev.source_post, ev.destination_post),
        (1_000, SUPPLY - 1_000, 940)
    );
    assert_eq!(
        ev.deltas,
        vec![
            DeltaApplied {
                holding: h(&r1),
                owner: r1,
                amount: 10,
                post: 10
            },
            DeltaApplied {
                holding: h(&r2),
                owner: r2,
                amount: 20,
                post: 20
            },
            DeltaApplied {
                holding: h(&r3),
                owner: r3,
                amount: 30,
                post: 30
            },
        ]
    );
    assert_eq!(
        (
            w.env.holding(&mint, &alice.pubkey()),
            w.env.holding(&mint, &bob),
            w.env.holding(&mint, &r1),
            w.env.holding(&mint, &r2),
            w.env.holding(&mint, &r3)
        ),
        (SUPPLY - 1_000, 940, 10, 20, 30)
    );
    // The mint never moves in a transfer.
    assert_eq!(w.env.read::<Mint>(&mint).supply, SUPPLY);

    // What the hook was told: before, the pre-state and no delta; after, the post-state and the sum.
    let s = script(&w, &mint);
    let before = s
        .told_token(callback::BEFORE_TRANSFER)
        .expect("before_transfer ran");
    assert_eq!(
        (
            before.op,
            before.phase,
            before.amount,
            before.delta,
            before.source_balance,
            before.destination_balance
        ),
        (TokenOp::Transfer, Phase::Before, 1_000, 0, SUPPLY, 0)
    );
    assert_eq!(
        (
            before.source_owner,
            before.destination_owner,
            before.authority,
            before.authority_is_delegate
        ),
        (alice.pubkey(), bob, alice.pubkey(), false)
    );
    let after = s
        .told_token(callback::AFTER_TRANSFER)
        .expect("after_transfer ran");
    assert_eq!(
        (
            after.phase,
            after.delta,
            after.source_balance,
            after.destination_balance
        ),
        (Phase::After, 60, SUPPLY - 1_000, 940)
    );

    // The deltas may take the whole amount: the destination then gains nothing.
    w.script_answer(
        &alice,
        mint,
        callback::BEFORE_TRANSFER,
        &deltas(&[(400, X + 2), (600, X)]),
    );
    let tx = w.tester_transfer(&alice, mint, &alice.pubkey(), &bob, 1_000);
    tx.ok();
    let ev: Transferred = tx.event();
    assert_eq!(ev.destination_post, 940);
    assert_eq!(
        ev.deltas,
        vec![
            DeltaApplied {
                holding: h(&r3),
                owner: r3,
                amount: 400,
                post: 430
            },
            DeltaApplied {
                holding: h(&r1),
                owner: r1,
                amount: 600,
                post: 610
            }
        ]
    );
    assert_eq!(
        script(&w, &mint)
            .told_token(callback::AFTER_TRANSFER)
            .unwrap()
            .delta,
        1_000
    );

    // No answer, no deltas.
    w.script_raw(&alice, mint, callback::BEFORE_TRANSFER, mode::NONE, vec![]);
    let tx = w.tester_transfer(&alice, mint, &alice.pubkey(), &bob, 1_000);
    tx.ok();
    assert!(tx.event::<Transferred>().deltas.is_empty());
    assert_eq!(w.env.holding(&mint, &bob), 1_940);

    // A refusing hook stops the transfer.
    w.script_raw(&alice, mint, callback::BEFORE_TRANSFER, mode::FAIL, vec![]);
    w.tester_transfer(&alice, mint, &alice.pubkey(), &bob, 1_000)
        .expect_code(u32::from(hook_tester::TesterError::Refused));
}

#[test]
fn every_delta_rule() {
    let mut w = World::new();
    let alice = w.env.funded(10 * SOL);
    let bob = Pubkey::new_unique();
    let (r1, r2, r3, r4, ro, cold) = (
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    );
    let other = w.mint_to_owner(&alice, 6, SUPPLY, "OTH");
    w.holdings(&alice, other, &[r1]);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let h = |owner: &Pubkey| token::holding_address(&mint, owner);
    // The registry: the script (5), then r1..r4 (6..9), r1 again (10), the source (11), the
    // destination (12), a read-only holding (13), r1's holding of another mint (14), a frozen
    // holding (15).
    let extras = vec![
        fixed(h(&r1), true),
        fixed(h(&r2), true),
        fixed(h(&r3), true),
        fixed(h(&r4), true),
        fixed(h(&r1), true),
        fixed(h(&alice.pubkey()), true),
        fixed(h(&bob), true),
        fixed(h(&ro), false),
        fixed(token::holding_address(&other, &r1), true),
        fixed(h(&cold), true),
    ];
    let flags = token_flags::BEFORE_TRANSFER | token_flags::TRANSFER_RETURNS_DELTA;
    w.tester_mint(&mint_kp, &alice, flags, SUPPLY, extras);
    w.holdings(&alice, mint, &[bob, r1, r2, r3, r4, ro, cold]);
    w.env
        .send_paid_by(
            &[token::set_frozen(alice.pubkey(), mint, h(&cold), true)],
            &alice,
            &[],
        )
        .ok();

    let mut refused = |answer: HookReturn, expected: TokenError| {
        w.script_answer(&alice, mint, callback::BEFORE_TRANSFER, &answer);
        let tx: Tx = w.tester_transfer(&alice, mint, &alice.pubkey(), &bob, 1_000);
        tx.expect_code(code(expected));
    };
    // The sum above the amount, and a sum that overflows.
    refused(deltas(&[(600, X), (401, X + 1)]), TokenError::DeltaTooLarge);
    refused(deltas(&[(1_001, X)]), TokenError::DeltaTooLarge);
    refused(
        deltas(&[(u64::MAX, X), (1, X + 1)]),
        TokenError::DeltaTooLarge,
    );
    refused(
        deltas(&[(u64::MAX - 5, X), (3, X + 1), (3, X + 2)]),
        TokenError::DeltaTooLarge,
    );
    // More than three.
    refused(
        deltas(&[(1, X), (1, X + 1), (1, X + 2), (1, X + 3)]),
        TokenError::TooManyDeltas,
    );
    // A delta of zero.
    refused(deltas(&[(1, X), (0, X + 1)]), TokenError::ZeroDelta);
    // The same account twice: by index, and by key at two indices.
    refused(deltas(&[(1, X), (1, X)]), TokenError::InvalidDeltaAccount);
    refused(
        deltas(&[(1, X), (1, X + 4)]),
        TokenError::InvalidDeltaAccount,
    );
    // To the source or the destination: as extras, and as prefix indices.
    refused(deltas(&[(1, X + 5)]), TokenError::InvalidDeltaAccount);
    refused(deltas(&[(1, X + 6)]), TokenError::InvalidDeltaAccount);
    refused(deltas(&[(1, 2)]), TokenError::InvalidDeltaAccount);
    refused(deltas(&[(1, 3)]), TokenError::InvalidDeltaAccount);
    // Not a holding (the script), past the extras, read-only, another mint's, frozen.
    refused(deltas(&[(1, 5)]), TokenError::InvalidDeltaAccount);
    refused(deltas(&[(1, X + 10)]), TokenError::InvalidDeltaAccount);
    refused(deltas(&[(1, X + 7)]), TokenError::InvalidDeltaAccount);
    refused(deltas(&[(1, X + 8)]), TokenError::InvalidDeltaAccount);
    refused(deltas(&[(1, X + 9)]), TokenError::InvalidDeltaAccount);
    // One bad delta refuses the good ones with it.
    refused(
        deltas(&[(1, X), (1, X + 1), (1, X + 9)]),
        TokenError::InvalidDeltaAccount,
    );
    // A burn, an LP fee or hook data (the mint has no WRITES_HOOK_DATA): nothing a transfer can take.
    refused(
        HookReturn {
            burn: 1,
            ..HookReturn::default()
        },
        TokenError::UnsupportedHookReturn,
    );
    refused(
        HookReturn {
            deltas: vec![Delta {
                amount: 1,
                account: X,
            }],
            burn: 1,
            ..HookReturn::default()
        },
        TokenError::UnsupportedHookReturn,
    );
    refused(
        HookReturn {
            lp_fee_bps: Some(0),
            ..HookReturn::default()
        },
        TokenError::UnsupportedHookReturn,
    );
    refused(
        HookReturn {
            source_hook_data: Some([1; 64]),
            ..HookReturn::default()
        },
        TokenError::UnsupportedHookReturn,
    );
    refused(
        HookReturn {
            destination_hook_data: Some([1; 64]),
            ..HookReturn::default()
        },
        TokenError::UnsupportedHookReturn,
    );

    // Bytes that do not decode as an answer.
    for bytes in [vec![1u8, 2, 3], {
        let mut b = Vec::new();
        anchor_lang::AnchorSerialize::serialize(&deltas(&[(1, X)]), &mut b).unwrap();
        b.push(0);
        b
    }] {
        w.script_raw(&alice, mint, callback::BEFORE_TRANSFER, mode::RETURN, bytes);
        w.tester_transfer(&alice, mint, &alice.pubkey(), &bob, 1_000)
            .expect_code(code(TokenError::InvalidHookReturn));
    }

    // Nothing moved; then a valid answer goes through.
    assert_eq!(
        (
            w.env.holding(&mint, &alice.pubkey()),
            w.env.holding(&mint, &bob),
            w.env.holding(&mint, &r1)
        ),
        (SUPPLY, 0, 0)
    );
    w.script_answer(
        &alice,
        mint,
        callback::BEFORE_TRANSFER,
        &deltas(&[(1, X + 3), (2, X + 1)]),
    );
    w.tester_transfer(&alice, mint, &alice.pubkey(), &bob, 1_000)
        .ok();
    assert_eq!(
        (
            w.env.holding(&mint, &bob),
            w.env.holding(&mint, &r4),
            w.env.holding(&mint, &r2)
        ),
        (997, 1, 2)
    );
}

#[test]
fn answers_are_read_only_when_the_flags_allow_them() {
    let mut w = World::new();
    let alice = w.env.funded(10 * SOL);
    let bob = Pubkey::new_unique();
    let r1 = Pubkey::new_unique();
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let h = |owner: &Pubkey| token::holding_address(&mint, owner);
    // before_transfer runs, but the mint lets it answer nothing.
    let flags = token_flags::BEFORE_TRANSFER | token_flags::BEFORE_MINT | token_flags::BEFORE_BURN;
    w.tester_mint(&mint_kp, &alice, flags, SUPPLY, vec![fixed(h(&r1), true)]);
    w.holdings(&alice, mint, &[bob, r1]);
    let everything = HookReturn {
        deltas: vec![Delta {
            amount: 5,
            account: X,
        }],
        burn: 7,
        lp_fee_bps: Some(1),
        source_hook_data: Some([9; 64]),
        destination_hook_data: Some([9; 64]),
    };
    w.script_answer(&alice, mint, callback::BEFORE_TRANSFER, &everything);
    let tx = w.tester_transfer(&alice, mint, &alice.pubkey(), &bob, 1_000);
    tx.ok();
    assert!(tx.event::<Transferred>().deltas.is_empty());
    assert_eq!(
        (w.env.holding(&mint, &bob), w.env.holding(&mint, &r1)),
        (1_000, 0)
    );
    assert_eq!(
        (
            w.env.hook_data(&mint, &alice.pubkey()),
            w.env.hook_data(&mint, &bob)
        ),
        ([0; 64], [0; 64])
    );
    assert_eq!(
        script(&w, &mint)
            .told_token(callback::BEFORE_TRANSFER)
            .unwrap()
            .amount,
        1_000
    );
    // Not even read: garbage passes.
    w.script_raw(
        &alice,
        mint,
        callback::BEFORE_TRANSFER,
        mode::RETURN,
        vec![0xff; 40],
    );
    w.tester_transfer(&alice, mint, &alice.pubkey(), &bob, 1_000)
        .ok();
    // Mint and burn callbacks without WRITES_HOOK_DATA are not read either.
    w.script_answer(&alice, mint, callback::BEFORE_MINT, &everything);
    w.script_answer(&alice, mint, callback::BEFORE_BURN, &everything);
    w.tester_mint_to(&alice, mint, &bob, 500).ok();
    w.tester_burn(&alice, mint, &alice.pubkey(), 500).ok();
    assert_eq!(
        (
            w.env.hook_data(&mint, &alice.pubkey()),
            w.env.hook_data(&mint, &bob)
        ),
        ([0; 64], [0; 64])
    );
    assert_eq!(w.env.read::<Mint>(&mint).supply, SUPPLY);
}

#[test]
fn hook_data_by_answer_on_transfer_mint_and_burn() {
    let mut w = World::new();
    let alice = w.env.funded(10 * SOL);
    let bob = w.env.funded(SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let all_callbacks = token_flags::BEFORE_TRANSFER
        | token_flags::AFTER_TRANSFER
        | token_flags::BEFORE_MINT
        | token_flags::AFTER_MINT
        | token_flags::BEFORE_BURN
        | token_flags::AFTER_BURN;
    let flags = all_callbacks | token_flags::WRITES_HOOK_DATA;
    w.tester_mint(&mint_kp, &alice, flags, 0, vec![]);
    w.holdings(&alice, mint, &[bob.pubkey()]);
    let (a, b) = (alice.pubkey(), bob.pubkey());

    // Mint: the destination's data, written with the balance.
    w.script_answer(
        &alice,
        mint,
        callback::BEFORE_MINT,
        &HookReturn {
            destination_hook_data: Some([1; 64]),
            ..HookReturn::default()
        },
    );
    let tx = w.tester_mint_to(&alice, mint, &a, SUPPLY);
    println!(
        "mint_to with hook data (hook_tester) CU {} size {}",
        tx.cu(),
        tx.size
    );
    tx.ok();
    assert_eq!(
        (w.env.holding(&mint, &a), w.env.hook_data(&mint, &a)),
        (SUPPLY, [1; 64])
    );
    let s = script(&w, &mint);
    let before = s.told_token(callback::BEFORE_MINT).unwrap();
    assert_eq!(
        (
            before.op,
            before.source_hook_data,
            before.destination_hook_data,
            before.destination_balance
        ),
        (TokenOp::Mint, [0; 64], [0; 64], 0)
    );
    let after = s.told_token(callback::AFTER_MINT).unwrap();
    assert_eq!(
        (
            after.phase,
            after.destination_hook_data,
            after.destination_balance,
            after.supply
        ),
        (Phase::After, [1; 64], SUPPLY, SUPPLY)
    );
    // A mint has no source to write, and answers no delta or burn.
    for answer in [
        HookReturn {
            source_hook_data: Some([2; 64]),
            ..HookReturn::default()
        },
        HookReturn {
            deltas: vec![Delta {
                amount: 1,
                account: X,
            }],
            ..HookReturn::default()
        },
        HookReturn {
            burn: 1,
            ..HookReturn::default()
        },
    ] {
        w.script_answer(&alice, mint, callback::BEFORE_MINT, &answer);
        w.tester_mint_to(&alice, mint, &a, 1)
            .expect_code(code(TokenError::UnsupportedHookReturn));
    }

    // Transfer: both sides' data, told to the hook before and after.
    w.script_answer(
        &alice,
        mint,
        callback::BEFORE_TRANSFER,
        &HookReturn {
            source_hook_data: Some([2; 64]),
            destination_hook_data: Some([3; 64]),
            ..HookReturn::default()
        },
    );
    let tx = w.tester_transfer(&alice, mint, &a, &b, 1_000);
    println!(
        "transfer with hook data on both sides (hook_tester) CU {} size {}",
        tx.cu(),
        tx.size
    );
    tx.ok();
    assert_eq!(
        (w.env.hook_data(&mint, &a), w.env.hook_data(&mint, &b)),
        ([2; 64], [3; 64])
    );
    let s = script(&w, &mint);
    let before = s.told_token(callback::BEFORE_TRANSFER).unwrap();
    assert_eq!(
        (before.source_hook_data, before.destination_hook_data),
        ([1; 64], [0; 64])
    );
    let after = s.told_token(callback::AFTER_TRANSFER).unwrap();
    assert_eq!(
        (
            after.source_hook_data,
            after.destination_hook_data,
            after.delta
        ),
        ([2; 64], [3; 64], 0)
    );
    // One side only: the other keeps its data.
    w.script_answer(
        &alice,
        mint,
        callback::BEFORE_TRANSFER,
        &HookReturn {
            destination_hook_data: Some([4; 64]),
            ..HookReturn::default()
        },
    );
    w.tester_transfer(&alice, mint, &a, &b, 1_000).ok();
    assert_eq!(
        (w.env.hook_data(&mint, &a), w.env.hook_data(&mint, &b)),
        ([2; 64], [4; 64])
    );
    // Without TRANSFER_RETURNS_DELTA a transfer answer carries no delta; it never carries a burn.
    w.script_answer(
        &alice,
        mint,
        callback::BEFORE_TRANSFER,
        &HookReturn {
            deltas: vec![Delta {
                amount: 1,
                account: X,
            }],
            ..HookReturn::default()
        },
    );
    w.tester_transfer(&alice, mint, &a, &b, 1_000)
        .expect_code(code(TokenError::UnsupportedHookReturn));
    w.script_answer(
        &alice,
        mint,
        callback::BEFORE_TRANSFER,
        &HookReturn {
            burn: 1,
            source_hook_data: Some([5; 64]),
            ..HookReturn::default()
        },
    );
    w.tester_transfer(&alice, mint, &a, &b, 1_000)
        .expect_code(code(TokenError::UnsupportedHookReturn));
    assert_eq!(
        (w.env.hook_data(&mint, &a), w.env.hook_data(&mint, &b)),
        ([2; 64], [4; 64])
    );

    // Burn: the source's data, written with the balance; never the destination's (there is none).
    w.script_answer(
        &alice,
        mint,
        callback::BEFORE_BURN,
        &HookReturn {
            source_hook_data: Some([6; 64]),
            ..HookReturn::default()
        },
    );
    let tx = w.tester_burn(&bob, mint, &b, 500);
    println!(
        "burn with hook data (hook_tester) CU {} size {}",
        tx.cu(),
        tx.size
    );
    tx.ok();
    assert_eq!(
        (w.env.holding(&mint, &b), w.env.hook_data(&mint, &b)),
        (1_500, [6; 64])
    );
    let s = script(&w, &mint);
    let before = s.told_token(callback::BEFORE_BURN).unwrap();
    assert_eq!(
        (
            before.op,
            before.source_hook_data,
            before.destination_hook_data,
            before.source_balance
        ),
        (TokenOp::Burn, [4; 64], [0; 64], 2_000)
    );
    let after = s.told_token(callback::AFTER_BURN).unwrap();
    assert_eq!(
        (after.source_hook_data, after.source_balance, after.supply),
        ([6; 64], 1_500, SUPPLY - 500)
    );
    for answer in [
        HookReturn {
            destination_hook_data: Some([7; 64]),
            ..HookReturn::default()
        },
        HookReturn {
            burn: 1,
            ..HookReturn::default()
        },
        HookReturn {
            deltas: vec![Delta {
                amount: 1,
                account: X,
            }],
            ..HookReturn::default()
        },
        HookReturn {
            lp_fee_bps: Some(1),
            ..HookReturn::default()
        },
    ] {
        w.script_answer(&alice, mint, callback::BEFORE_BURN, &answer);
        w.tester_burn(&bob, mint, &b, 1)
            .expect_code(code(TokenError::UnsupportedHookReturn));
    }
    assert_eq!(
        (
            w.env.holding(&mint, &b),
            w.env.hook_data(&mint, &b),
            w.env.read::<Mint>(&mint).supply
        ),
        (1_500, [6; 64], SUPPLY - 500)
    );

    // With deltas allowed too, one answer does both.
    let alice2 = w.env.funded(10 * SOL);
    let r1 = Pubkey::new_unique();
    let mint2_kp = Keypair::new();
    let mint2 = mint2_kp.pubkey();
    w.tester_mint(
        &mint2_kp,
        &alice2,
        token_flags::BEFORE_TRANSFER
            | token_flags::TRANSFER_RETURNS_DELTA
            | token_flags::WRITES_HOOK_DATA,
        SUPPLY,
        vec![fixed(token::holding_address(&mint2, &r1), true)],
    );
    w.holdings(&alice2, mint2, &[b, r1]);
    w.script_answer(
        &alice2,
        mint2,
        callback::BEFORE_TRANSFER,
        &HookReturn {
            deltas: vec![Delta {
                amount: 10,
                account: X,
            }],
            source_hook_data: Some([8; 64]),
            destination_hook_data: Some([9; 64]),
            ..HookReturn::default()
        },
    );
    let tx = w.tester_transfer(&alice2, mint2, &alice2.pubkey(), &b, 100);
    tx.ok();
    assert_eq!(tx.event::<Transferred>().deltas.len(), 1);
    assert_eq!(
        (w.env.holding(&mint2, &b), w.env.holding(&mint2, &r1)),
        (90, 10)
    );
    assert_eq!(
        (
            w.env.hook_data(&mint2, &alice2.pubkey()),
            w.env.hook_data(&mint2, &b),
            w.env.hook_data(&mint2, &r1)
        ),
        ([8; 64], [9; 64], [0; 64])
    );
}

#[test]
fn write_hook_data_only_for_the_mints_hook() {
    let mut w = World::new();
    let alice = w.env.funded(10 * SOL);
    let bob = Pubkey::new_unique();
    // A: hook_tester with the flag. C: hook_tester without it. B: another program's hook with
    // the flag. D: no hook.
    let a_kp = Keypair::new();
    let a = w.tester_mint(
        &a_kp,
        &alice,
        token_flags::WRITES_HOOK_DATA | token_flags::BEFORE_TRANSFER,
        SUPPLY,
        vec![],
    );
    let c_kp = Keypair::new();
    let c = w.tester_mint(&c_kp, &alice, token_flags::BEFORE_TRANSFER, SUPPLY, vec![]);
    let b_kp = Keypair::new();
    let args = bordrless_token::instructions::CreateMintArgs {
        decimals: 6,
        name: "Other hook".into(),
        symbol: "OTH".into(),
        uri: String::new(),
        max_supply: 0,
        mint_authority: Some(alice.pubkey()),
        freeze_authority: None,
        hook_program: Some(tax_hook::ID),
        hook_flags: token_flags::WRITES_HOOK_DATA,
        hook_authority: None,
        metadata_authority: None,
    };
    w.env
        .send_paid_by(
            &[token::create_mint(alice.pubkey(), b_kp.pubkey(), args)],
            &alice,
            &[&b_kp],
        )
        .ok();
    let b = b_kp.pubkey();
    let d = w.mint_to_owner(&alice, 6, SUPPLY, "NOH");
    for mint in [a, b, c, d] {
        w.holdings(&alice, mint, &[bob]);
    }
    let holding = |mint: &Pubkey| token::holding_address(mint, &bob);
    let data = [0x5a; 64];
    let calls = script(&w, &a).calls;

    // The mint's hook writes through its ["hook-authority"] PDA, without calling any hook.
    let tx = w
        .env
        .send(&[tester::write_hook_data(a, holding(&a), data)], &[]);
    println!(
        "write_hook_data by CPI (hook_tester) CU {} size {}",
        tx.cu(),
        tx.size
    );
    tx.ok();
    let ev: HookDataWritten = tx.event();
    assert_eq!(
        (ev.mint, ev.holding, ev.owner, ev.data),
        (a, holding(&a), bob, data)
    );
    assert_eq!(w.env.hook_data(&a, &bob), data);
    assert_eq!(script(&w, &a).calls, calls);
    let held: Holding = w.env.read(&holding(&a));
    assert_eq!((held.amount, held.owner, held.mint), (0, bob, a));

    // Another PDA of the same program: another seed, or ["hook-authority"] at a bump that is not
    // the canonical one.
    let other = Pubkey::find_program_address(&[b"not-the-hook-authority"], &hook_tester::ID);
    let tx = w.env.send(
        &[tester::write_hook_data_as(
            other.0,
            vec![b"not-the-hook-authority".to_vec(), vec![other.1]],
            a,
            holding(&a),
            [1; 64],
        )],
        &[],
    );
    tx.expect_code(code(TokenError::NotHookAuthority));
    let (_, canonical) = tester::hook_authority();
    let off = (0..canonical)
        .rev()
        .find_map(|bump| {
            Pubkey::create_program_address(&[HOOK_AUTHORITY_SEED, &[bump]], &hook_tester::ID)
                .ok()
                .map(|k| (k, bump))
        })
        .expect("a non-canonical bump");
    w.env
        .send(
            &[tester::write_hook_data_as(
                off.0,
                vec![HOOK_AUTHORITY_SEED.to_vec(), vec![off.1]],
                a,
                holding(&a),
                [1; 64],
            )],
            &[],
        )
        .expect_code(code(TokenError::NotHookAuthority));
    // A wallet signing for itself, even the mint's every authority.
    w.env
        .send_paid_by(
            &[token::write_hook_data(
                alice.pubkey(),
                a,
                holding(&a),
                [1; 64],
            )],
            &alice,
            &[],
        )
        .expect_code(code(TokenError::NotHookAuthority));
    // The hook authority of another mint's hook (hook_tester is A's hook, not B's).
    w.env
        .send(&[tester::write_hook_data(b, holding(&b), [1; 64])], &[])
        .expect_code(code(TokenError::NotHookAuthority));
    // A mint without the flag, a mint without a hook.
    w.env
        .send(&[tester::write_hook_data(c, holding(&c), [1; 64])], &[])
        .expect_code(code(TokenError::HookDataNotWritable));
    w.env
        .send(&[tester::write_hook_data(d, holding(&d), [1; 64])], &[])
        .expect_code(code(TokenError::HookDataNotWritable));
    // A holding of another mint, passed with the hook's own mint.
    for other_mint in [b, c, d] {
        w.env
            .send(
                &[tester::write_hook_data(a, holding(&other_mint), [1; 64])],
                &[],
            )
            .expect_code(code(TokenError::MintMismatch));
    }
    for mint in [b, c, d] {
        assert_eq!(w.env.hook_data(&mint, &bob), [0; 64]);
    }
    assert_eq!(w.env.hook_data(&a, &bob), data);
}

#[test]
fn close_holding_keeps_a_hooks_record() {
    let mut w = World::new();
    let alice = w.env.funded(10 * SOL);
    let bob = w.env.funded(SOL);
    let carol = w.env.funded(SOL);
    let a_kp = Keypair::new();
    let a = w.tester_mint(&a_kp, &alice, token_flags::WRITES_HOOK_DATA, SUPPLY, vec![]);
    w.holdings(&alice, a, &[bob.pubkey(), carol.pubkey()]);
    let bob_h = token::holding_address(&a, &bob.pubkey());
    let carol_h = token::holding_address(&a, &carol.pubkey());
    w.env
        .send(
            &[
                tester::write_hook_data(a, bob_h, [1; 64]),
                tester::write_hook_data(a, carol_h, [2; 64]),
            ],
            &[],
        )
        .ok();

    // Empty, but the hook still keeps data: refused.
    w.env
        .send_paid_by(
            &[token::close_holding(bob.pubkey(), a, bob_h, bob.pubkey())],
            &bob,
            &[],
        )
        .expect_code(code(TokenError::HookDataNotEmpty));
    // Once the hook writes it back to zero, it closes.
    let mut nearly = [0u8; 64];
    nearly[63] = 1;
    w.env
        .send(&[tester::write_hook_data(a, bob_h, nearly)], &[])
        .ok();
    w.env
        .send_paid_by(
            &[token::close_holding(bob.pubkey(), a, bob_h, bob.pubkey())],
            &bob,
            &[],
        )
        .expect_code(code(TokenError::HookDataNotEmpty));
    w.env
        .send(&[tester::write_hook_data(a, bob_h, [0; 64])], &[])
        .ok();
    let tx = w.env.send_paid_by(
        &[token::close_holding(bob.pubkey(), a, bob_h, bob.pubkey())],
        &bob,
        &[],
    );
    println!(
        "close_holding under a data-writing hook CU {} size {}",
        tx.cu(),
        tx.size
    );
    tx.ok();
    assert!(w.env.account(&bob_h).is_none());

    // With another mint's account: refused.
    let other = w.mint_to_owner(&alice, 6, SUPPLY, "OTH");
    w.env
        .send_paid_by(
            &[token::close_holding(
                carol.pubkey(),
                other,
                carol_h,
                carol.pubkey(),
            )],
            &carol,
            &[],
        )
        .expect_code(code(TokenError::MintMismatch));
    // Without a hook, nobody could ever clear it, so it closes regardless.
    w.env
        .send_paid_by(&[token::set_hook(alice.pubkey(), a, None, 0)], &alice, &[])
        .ok();
    assert_eq!(w.env.hook_data(&a, &carol.pubkey()), [2; 64]);
    w.env
        .send_paid_by(
            &[token::close_holding(
                carol.pubkey(),
                a,
                carol_h,
                carol.pubkey(),
            )],
            &carol,
            &[],
        )
        .ok();
    assert!(w.env.account(&carol_h).is_none());

    // The same with a hook that does not write hook data (any more).
    let e_kp = Keypair::new();
    let e = w.tester_mint(
        &e_kp,
        &alice,
        token_flags::WRITES_HOOK_DATA | token_flags::BEFORE_TRANSFER,
        SUPPLY,
        vec![],
    );
    w.holdings(&alice, e, &[carol.pubkey()]);
    let carol_e = token::holding_address(&e, &carol.pubkey());
    w.env
        .send(&[tester::write_hook_data(e, carol_e, [3; 64])], &[])
        .ok();
    w.env
        .send_paid_by(
            &[token::close_holding(
                carol.pubkey(),
                e,
                carol_e,
                carol.pubkey(),
            )],
            &carol,
            &[],
        )
        .expect_code(code(TokenError::HookDataNotEmpty));
    w.env
        .send_paid_by(
            &[token::set_hook(
                alice.pubkey(),
                e,
                Some(hook_tester::ID),
                token_flags::BEFORE_TRANSFER,
            )],
            &alice,
            &[],
        )
        .ok();
    w.env
        .send_paid_by(
            &[token::close_holding(
                carol.pubkey(),
                e,
                carol_e,
                carol.pubkey(),
            )],
            &carol,
            &[],
        )
        .ok();
    assert!(w.env.account(&carol_e).is_none());
}

#[test]
fn registries_accept_prefunded_addresses() {
    let mut w = World::new();
    let alice = w.env.funded(10 * SOL);
    let rent = |w: &World, len: usize| w.env.rent(len);

    // Someone sent the registry address one lamport, then much more than its rent: each is
    // topped up only as needed, allocated and assigned.
    for prefund in [1u64, 5 * SOL] {
        let key = Pubkey::new_unique();
        let registry = tester::registry_address(&key);
        w.env.fund(registry, prefund);
        let tx = w.env.send_paid_by(
            &[tester::init_script(
                alice.pubkey(),
                key,
                vec![fixed(Pubkey::new_unique(), true)],
            )],
            &alice,
            &[],
        );
        println!(
            "registry at an address holding {prefund} lamports: CU {}",
            tx.cu()
        );
        tx.ok();
        let account = w.env.account(&registry).expect("registry");
        assert_eq!(account.owner, hook_tester::ID);
        let list = HookAccountList::decode(&account.data).expect("registry decodes");
        assert_eq!(list.accounts.len(), 2);
        assert_eq!(account.data.len(), list.encode().len());
        assert_eq!(account.lamports, prefund.max(rent(&w, account.data.len())));
    }
    // A fresh address still works, and a second init of the same key is refused (the script exists).
    let key = Pubkey::new_unique();
    w.env
        .send_paid_by(
            &[tester::init_script(alice.pubkey(), key, vec![])],
            &alice,
            &[],
        )
        .ok();
    w.env
        .send_paid_by(
            &[tester::init_script(alice.pubkey(), key, vec![])],
            &alice,
            &[],
        )
        .expect_fail();

    // A real hook installing on a mint whose registry address was funded first.
    let collector = Pubkey::new_unique();
    let mint = w.mint_to_owner(&alice, 6, SUPPLY, "TAX");
    let registry = hook_accounts_address(&tax_hook::ID, &mint).0;
    w.env.fund(registry, 1);
    let collector_h = token::holding_address(&mint, &collector);
    let ixs = [
        token::create_holding(alice.pubkey(), mint, collector),
        tax_install(alice.pubkey(), mint, collector_h, 100, 0),
    ];
    w.env.send_paid_by(&ixs, &alice, &[]).ok();
    let account = w.env.account(&registry).expect("registry");
    assert_eq!(account.owner, tax_hook::ID);
    assert!(HookAccountList::decode(&account.data).is_some());
    assert!(account.lamports >= rent(&w, account.data.len()));
    // And its transfers work: 1% to the collector.
    let bob = Pubkey::new_unique();
    w.holdings(&alice, mint, &[bob]);
    let ix = w.hooked_transfer_ix(
        tax_hook::ID,
        mint,
        alice.pubkey(),
        &alice.pubkey(),
        &bob,
        10_000,
    );
    w.env.send_paid_by(&[ix], &alice, &[]).ok();
    assert_eq!(
        (w.env.holding(&mint, &bob), w.env.holding(&mint, &collector)),
        (9_900, 100)
    );
}
