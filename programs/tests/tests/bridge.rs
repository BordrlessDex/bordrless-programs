//! The bridge: SPL tokens and SOL in and out of the token standard.

use anchor_lang::prelude::Pubkey;
use bordrless_bridge::client as bridge;
use bordrless_bridge::constants::{NATIVE_MINT, TOKEN_PROGRAM_ID};
use bordrless_bridge::events::{Unwrapped, Wrapped};
use bordrless_bridge::instructions::RegisterArgs;
use bordrless_bridge::state::Wrapper;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::spl;
use bordrless_token::client as token;
use bordrless_token::state::Mint;
use solana_signer::Signer;

#[test]
fn spl_round_trip() {
    let mut w = World::new();
    let user = w.env.funded(10_000_000_000);
    let underlying = Pubkey::new_unique();
    w.env.set_spl_mint(underlying, None, 6, 1_000_000_000);
    let user_ata = w
        .env
        .set_spl_ata(&user.pubkey(), &underlying, 1_000_000_000);
    let wrapped = bridge::wrapped_mint_address(&underlying);
    let wrapper = bridge::wrapper_address(&underlying);
    let vault = bridge::vault_address(&underlying, &TOKEN_PROGRAM_ID);

    let tx = w.env.send_paid_by(
        &[bridge::register(
            user.pubkey(),
            underlying,
            TOKEN_PROGRAM_ID,
            RegisterArgs {
                name: "Some Token".into(),
                symbol: "SOME".into(),
                uri: "https://example.com/some.json".into(),
            },
        )],
        &user,
        &[],
    );
    println!("register CU {} size {}", tx.cu(), tx.size);
    tx.ok();
    let wr: Wrapper = w.env.read(&wrapper);
    assert_eq!(
        (
            wr.underlying_mint,
            wr.wrapped_mint,
            wr.vault,
            wr.decimals,
            wr.native,
            wr.total_wrapped
        ),
        (underlying, wrapped, vault, 6, false, 0)
    );
    let m: Mint = w.env.read(&wrapped);
    assert_eq!(
        (m.decimals, m.symbol.as_str(), m.mint_authority),
        (6, "SOME", Some(wrapper))
    );
    // Registering twice fails (the wrapper exists).
    w.env
        .send_paid_by(
            &[bridge::register(
                user.pubkey(),
                underlying,
                TOKEN_PROGRAM_ID,
                RegisterArgs {
                    name: "x".into(),
                    symbol: "X".into(),
                    uri: String::new(),
                },
            )],
            &user,
            &[],
        )
        .expect_fail();

    let ixs = [
        token::create_holding(user.pubkey(), wrapped, user.pubkey()),
        bridge::wrap(
            user.pubkey(),
            underlying,
            TOKEN_PROGRAM_ID,
            user_ata,
            400_000_000,
        ),
    ];
    let tx = w.env.send_paid_by(&ixs, &user, &[]);
    println!("wrap CU {} size {}", tx.cu(), tx.size);
    tx.ok();
    let ev: Wrapped = tx.event();
    assert_eq!(
        (ev.amount_sent, ev.amount_minted, ev.total_wrapped),
        (400_000_000, 400_000_000, 400_000_000)
    );
    assert_eq!(w.env.holding(&wrapped, &user.pubkey()), 400_000_000);
    assert_eq!(spl::amount(&w.env, &vault), 400_000_000);
    assert_eq!(spl::amount(&w.env, &user_ata), 600_000_000);

    let tx = w.env.send_paid_by(
        &[bridge::unwrap(
            user.pubkey(),
            underlying,
            TOKEN_PROGRAM_ID,
            user_ata,
            150_000_000,
        )],
        &user,
        &[],
    );
    tx.ok();
    assert_eq!(tx.event::<Unwrapped>().total_wrapped, 250_000_000);
    assert_eq!(w.env.holding(&wrapped, &user.pubkey()), 250_000_000);
    assert_eq!(spl::amount(&w.env, &vault), 250_000_000);
    assert_eq!(spl::amount(&w.env, &user_ata), 750_000_000);
    // More than held.
    w.env
        .send_paid_by(
            &[bridge::unwrap(
                user.pubkey(),
                underlying,
                TOKEN_PROGRAM_ID,
                user_ata,
                250_000_001,
            )],
            &user,
            &[],
        )
        .expect_fail();
}

#[test]
fn sol_round_trip() {
    let mut w = World::new();
    let user = w.env.funded(5_000_000_000);
    let wrapped = bridge::wrapped_mint_address(&NATIVE_MINT);
    assert_eq!(w.sol, wrapped);
    let before = w.env.lamports(&user.pubkey());
    let tx = w.wrap_sol(&user, 2_000_000_000);
    println!("wrap_sol CU {} size {}", tx.cu(), tx.size);
    tx.ok();
    assert_eq!(w.env.holding(&wrapped, &user.pubkey()), 2_000_000_000);
    let spent = before - w.env.lamports(&user.pubkey());
    assert!(
        (2_000_000_000..2_010_000_000).contains(&spent),
        "spent {spent}"
    );
    assert_eq!(
        w.env
            .read::<Wrapper>(&bridge::wrapper_address(&NATIVE_MINT))
            .total_wrapped,
        2_000_000_000
    );

    let before = w.env.lamports(&user.pubkey());
    let tx = w
        .env
        .send_paid_by(&[bridge::unwrap_sol(user.pubkey(), u64::MAX)], &user, &[]);
    tx.ok();
    assert_eq!(w.env.holding(&wrapped, &user.pubkey()), 0);
    let got = w.env.lamports(&user.pubkey()) - before;
    assert!((1_990_000_001..=2_000_000_000).contains(&got), "got {got}");
    assert_eq!(
        w.env
            .read::<Wrapper>(&bridge::wrapper_address(&NATIVE_MINT))
            .total_wrapped,
        0
    );
    // Nothing left to unwrap.
    w.env
        .send_paid_by(&[bridge::unwrap_sol(user.pubkey(), u64::MAX)], &user, &[])
        .expect_fail();
}
