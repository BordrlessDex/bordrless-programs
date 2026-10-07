//! The token standard: mints, holdings, transfers, delegation, freezing, authorities, metadata,
//! and a token hook taking a fee.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use bordrless_hook::{hook_accounts_address, HookAccountList};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::hooks::tax_install;
use bordrless_token::client as token;
use bordrless_token::error::TokenError;
use bordrless_token::events::{Burned, DeltaApplied, Minted, Transferred};
use bordrless_token::state::{AuthorityKind, Holding, Mint};
use solana_signer::Signer;

const SUPPLY: u64 = 1_000_000_000;

#[test]
fn mint_transfer_delegate_freeze_burn() {
    let mut w = World::new();
    let alice = w.env.funded(10_000_000_000);
    let bob = w.env.funded(10_000_000_000);
    let mint = w.mint_to_owner(&alice, 6, SUPPLY, "TST");
    let alice_h = token::holding_address(&mint, &alice.pubkey());
    let bob_h = token::holding_address(&mint, &bob.pubkey());
    assert_eq!(w.env.holding(&mint, &alice.pubkey()), SUPPLY);
    let m: Mint = w.env.read(&mint);
    assert_eq!(
        (m.supply, m.decimals, m.symbol.as_str(), m.name.as_str()),
        (SUPPLY, 6, "TST", "Test TST")
    );
    assert_eq!(m.mint_authority, Some(alice.pubkey()));

    // A transfer to a wallet that has no holding yet: create it in the same transaction.
    let ixs = [
        token::create_holding(alice.pubkey(), mint, bob.pubkey()),
        token::transfer(
            alice.pubkey(),
            alice_h,
            bob_h,
            mint,
            None,
            vec![],
            250_000_000,
        ),
    ];
    let tx = w.env.send_paid_by(&ixs, &alice, &[]);
    println!("transfer CU {} size {}", tx.cu(), tx.size);
    tx.ok();
    let ev: Transferred = tx.event();
    assert_eq!(
        (ev.amount, ev.source_post, ev.destination_post),
        (250_000_000, 750_000_000, 250_000_000)
    );
    assert!(ev.deltas.is_empty());
    assert_eq!(
        (ev.source_owner, ev.destination_owner),
        (alice.pubkey(), bob.pubkey())
    );
    assert_eq!(w.env.holding(&mint, &bob.pubkey()), 250_000_000);
    // A new holding keeps no hook data.
    assert_eq!(w.env.read::<Holding>(&bob_h).hook_data, [0; 64]);

    // Creating it again changes nothing.
    w.env
        .send_paid_by(
            &[token::create_holding(alice.pubkey(), mint, bob.pubkey())],
            &alice,
            &[],
        )
        .ok();
    assert_eq!(w.env.holding(&mint, &bob.pubkey()), 250_000_000);

    // More than held.
    w.env
        .send_paid_by(
            &[token::transfer(
                bob.pubkey(),
                bob_h,
                alice_h,
                mint,
                None,
                vec![],
                300_000_000,
            )],
            &bob,
            &[],
        )
        .expect_code(u32::from(TokenError::InsufficientFunds));
    // Not the owner.
    w.env
        .send_paid_by(
            &[token::transfer(
                bob.pubkey(),
                alice_h,
                bob_h,
                mint,
                None,
                vec![],
                1,
            )],
            &bob,
            &[],
        )
        .expect_code(u32::from(TokenError::NotAuthorized));

    // A delegate may move up to its allowance.
    w.env
        .send_paid_by(
            &[token::approve(
                alice.pubkey(),
                alice_h,
                bob.pubkey(),
                100_000_000,
            )],
            &alice,
            &[],
        )
        .ok();
    let tx = w.env.send_paid_by(
        &[token::transfer(
            bob.pubkey(),
            alice_h,
            bob_h,
            mint,
            None,
            vec![],
            60_000_000,
        )],
        &bob,
        &[],
    );
    println!(
        "transfer by a delegate, no hook (alone) CU {} size {}",
        tx.cu(),
        tx.size
    );
    tx.ok();
    assert!(tx.event::<Transferred>().authority == bob.pubkey());
    let h: Holding = w.env.read(&alice_h);
    assert_eq!(
        (h.amount, h.delegate, h.delegated_amount),
        (690_000_000, Some(bob.pubkey()), 40_000_000)
    );
    w.env
        .send_paid_by(
            &[token::transfer(
                bob.pubkey(),
                alice_h,
                bob_h,
                mint,
                None,
                vec![],
                50_000_000,
            )],
            &bob,
            &[],
        )
        .expect_code(u32::from(TokenError::InsufficientDelegation));
    w.env
        .send_paid_by(&[token::revoke(alice.pubkey(), alice_h)], &alice, &[])
        .ok();
    w.env
        .send_paid_by(
            &[token::transfer(
                bob.pubkey(),
                alice_h,
                bob_h,
                mint,
                None,
                vec![],
                1,
            )],
            &bob,
            &[],
        )
        .expect_code(u32::from(TokenError::NotAuthorized));

    // Frozen holdings neither send nor receive.
    w.env
        .send_paid_by(
            &[token::set_frozen(alice.pubkey(), mint, bob_h, true)],
            &alice,
            &[],
        )
        .ok();
    w.env
        .send_paid_by(
            &[token::transfer(
                bob.pubkey(),
                bob_h,
                alice_h,
                mint,
                None,
                vec![],
                1,
            )],
            &bob,
            &[],
        )
        .expect_code(u32::from(TokenError::Frozen));
    w.env
        .send_paid_by(
            &[token::transfer(
                alice.pubkey(),
                alice_h,
                bob_h,
                mint,
                None,
                vec![],
                1,
            )],
            &alice,
            &[],
        )
        .expect_code(u32::from(TokenError::Frozen));
    w.env
        .send_paid_by(
            &[token::set_frozen(bob.pubkey(), mint, bob_h, false)],
            &bob,
            &[],
        )
        .expect_code(u32::from(TokenError::NotAuthorized));
    w.env
        .send_paid_by(
            &[token::set_frozen(alice.pubkey(), mint, bob_h, false)],
            &alice,
            &[],
        )
        .ok();

    // Burning lowers the supply.
    let tx = w.env.send_paid_by(
        &[token::burn(
            bob.pubkey(),
            bob_h,
            mint,
            None,
            vec![],
            10_000_000,
        )],
        &bob,
        &[],
    );
    tx.ok();
    let ev: Burned = tx.event();
    assert_eq!(
        (ev.amount, ev.source_post, ev.supply_post),
        (10_000_000, 300_000_000, SUPPLY - 10_000_000)
    );
    assert_eq!(w.env.read::<Mint>(&mint).supply, SUPPLY - 10_000_000);

    // Minting respects the authority; revoking it is final.
    let tx = w.env.send_paid_by(
        &[token::mint_to(
            alice.pubkey(),
            mint,
            bob_h,
            None,
            vec![],
            5_000_000,
        )],
        &alice,
        &[],
    );
    tx.ok();
    assert_eq!(tx.event::<Minted>().supply_post, SUPPLY - 5_000_000);
    w.env
        .send_paid_by(
            &[token::mint_to(bob.pubkey(), mint, bob_h, None, vec![], 1)],
            &bob,
            &[],
        )
        .expect_code(u32::from(TokenError::NotAuthorized));
    w.env
        .send_paid_by(
            &[token::set_authority(
                alice.pubkey(),
                mint,
                AuthorityKind::Mint,
                None,
            )],
            &alice,
            &[],
        )
        .ok();
    w.env
        .send_paid_by(
            &[token::mint_to(alice.pubkey(), mint, bob_h, None, vec![], 1)],
            &alice,
            &[],
        )
        .expect_code(u32::from(TokenError::AuthorityRevoked));

    // Metadata changes until its authority is revoked.
    w.env
        .send_paid_by(
            &[token::update_metadata(
                alice.pubkey(),
                mint,
                None,
                None,
                Some("ipfs://new".to_string()),
            )],
            &alice,
            &[],
        )
        .ok();
    assert_eq!(w.env.read::<Mint>(&mint).uri, "ipfs://new");
    w.env
        .send_paid_by(
            &[token::set_authority(
                alice.pubkey(),
                mint,
                AuthorityKind::Metadata,
                None,
            )],
            &alice,
            &[],
        )
        .ok();
    w.env
        .send_paid_by(
            &[token::update_metadata(
                alice.pubkey(),
                mint,
                Some("x".to_string()),
                None,
                None,
            )],
            &alice,
            &[],
        )
        .expect_code(u32::from(TokenError::AuthorityRevoked));

    // An empty holding closes and returns its rent; a non-empty one does not.
    w.env
        .send_paid_by(
            &[token::close_holding(
                bob.pubkey(),
                mint,
                bob_h,
                bob.pubkey(),
            )],
            &bob,
            &[],
        )
        .expect_code(u32::from(TokenError::HoldingNotEmpty));
    let bob_amount = w.env.holding(&mint, &bob.pubkey());
    w.env
        .send_paid_by(
            &[token::burn(
                bob.pubkey(),
                bob_h,
                mint,
                None,
                vec![],
                bob_amount,
            )],
            &bob,
            &[],
        )
        .ok();
    let before = w.env.lamports(&bob.pubkey());
    let tx = w.env.send_paid_by(
        &[token::close_holding(
            bob.pubkey(),
            mint,
            bob_h,
            bob.pubkey(),
        )],
        &bob,
        &[],
    );
    println!("close_holding CU {} size {}", tx.cu(), tx.size);
    tx.ok();
    assert!(w.env.account(&bob_h).is_none());
    assert!(w.env.lamports(&bob.pubkey()) > before);
}

#[test]
fn a_hook_takes_a_fee_and_caps_wallets() {
    let mut w = World::new();
    let alice = w.env.funded(10_000_000_000);
    let bob = w.env.funded(10_000_000_000);
    let collector = w.env.funded(1_000_000_000);
    let mint = w.mint_to_owner(&alice, 6, SUPPLY, "TAX");
    let alice_h = token::holding_address(&mint, &alice.pubkey());
    let bob_h = token::holding_address(&mint, &bob.pubkey());
    let collector_h = token::holding_address(&mint, &collector.pubkey());
    let tax = Pubkey::find_program_address(&[tax_hook::TAX_SEED, mint.as_ref()], &tax_hook::ID).0;
    let (registry, _) = hook_accounts_address(&tax_hook::ID, &mint);

    // Install: 5% to the collector, nobody above 20% of the supply.
    let install = tax_install(alice.pubkey(), mint, collector_h, 500, 2_000);
    let ixs = [
        token::create_holding(alice.pubkey(), mint, collector.pubkey()),
        token::create_holding(alice.pubkey(), mint, bob.pubkey()),
        install,
    ];
    w.env.send_paid_by(&ixs, &alice, &[]).ok();
    let m: Mint = w.env.read(&mint);
    assert_eq!(
        (m.hook_program, m.hook_flags),
        (Some(tax_hook::ID), tax_hook::FLAGS)
    );

    // The client resolves the hook's accounts from its registry.
    let list = HookAccountList::decode(&w.env.account(&registry).expect("registry").data)
        .expect("decodes");
    let extras =
        |source: Pubkey, destination: Pubkey, source_owner: Pubkey, destination_owner: Pubkey| {
            list.resolve(
                &[
                    token::hook_signer(&tax_hook::ID),
                    mint,
                    source,
                    destination,
                    alice.pubkey(),
                ],
                &source_owner,
                &destination_owner,
            )
            .expect("resolves")
        };
    assert_eq!(
        extras(alice_h, bob_h, alice.pubkey(), bob.pubkey()),
        vec![
            AccountMeta::new(tax, false),
            AccountMeta::new(collector_h, false)
        ]
    );

    // Without the hook's accounts the transfer cannot run.
    w.env
        .send_paid_by(
            &[token::transfer(
                alice.pubkey(),
                alice_h,
                bob_h,
                mint,
                None,
                vec![],
                100_000_000,
            )],
            &alice,
            &[],
        )
        .expect_fail();
    w.env
        .send_paid_by(
            &[token::transfer(
                alice.pubkey(),
                alice_h,
                bob_h,
                mint,
                Some(tax_hook::ID),
                vec![],
                100_000_000,
            )],
            &alice,
            &[],
        )
        .expect_fail();

    // 100 sent: 95 arrive, 5 go to the collector through one delta; the event says so.
    let tx = w.env.send_paid_by(
        &[token::transfer(
            alice.pubkey(),
            alice_h,
            bob_h,
            mint,
            Some(tax_hook::ID),
            extras(alice_h, bob_h, alice.pubkey(), bob.pubkey()),
            100_000_000,
        )],
        &alice,
        &[],
    );
    println!("hooked transfer (tax_hook) CU {} size {}", tx.cu(), tx.size);
    tx.ok();
    let ev: Transferred = tx.event();
    assert_eq!((ev.amount, ev.destination_post), (100_000_000, 95_000_000));
    assert_eq!(
        ev.deltas,
        vec![DeltaApplied {
            holding: collector_h,
            owner: collector.pubkey(),
            amount: 5_000_000,
            post: 5_000_000
        }]
    );
    assert_eq!(w.env.holding(&mint, &bob.pubkey()), 95_000_000);
    assert_eq!(w.env.holding(&mint, &collector.pubkey()), 5_000_000);
    assert_eq!(w.env.read::<tax_hook::TaxConfig>(&tax).collected, 5_000_000);

    // A transfer that would put Bob above 20% of the supply fails in the hook.
    w.env
        .send_paid_by(
            &[token::transfer(
                alice.pubkey(),
                alice_h,
                bob_h,
                mint,
                Some(tax_hook::ID),
                extras(alice_h, bob_h, alice.pubkey(), bob.pubkey()),
                120_000_000,
            )],
            &alice,
            &[],
        )
        .expect_code(u32::from(tax_hook::TaxError::WalletTooLarge));

    // The collector is exempt on both sides.
    let tx = w.env.send_paid_by(
        &[token::transfer(
            alice.pubkey(),
            alice_h,
            collector_h,
            mint,
            Some(tax_hook::ID),
            extras(alice_h, collector_h, alice.pubkey(), collector.pubkey()),
            300_000_000,
        )],
        &alice,
        &[],
    );
    tx.ok();
    assert!(tx.event::<Transferred>().deltas.is_empty());
    assert_eq!(w.env.holding(&mint, &collector.pubkey()), 305_000_000);
}
