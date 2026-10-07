//! Who may call a hook (the review fixes of `docs/hooks-v2.md`, implementation notes). The token
//! program and the DEX sign every callback with a PDA of the hook program they call,
//! `["hook-authority", hook_program]`: a callback gets that signer as a signer and could pass it on
//! in a CPI of its own, so each hook accepts only the signer made for it. Here `hook_tester`, as a
//! token hook and as a pool hook, passes on the signer it receives to the kit, `tax_hook` and the
//! launch's pool hook with forged arguments (a ghost transfer that zeroes `eligible`, a fee that
//! runs `collected` to its limit, a swap that runs the launch's counters up and spends the
//! creator's first-buy exemption): each refuses it and nothing changes. Before the fix the token
//! program and the DEX signed every callback with one PDA, and each of these calls went through.
//! Then the callers' own checks: the token program and the DEX refuse a hook signer that is not
//! the one for the hook they call.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::InstructionData;
use bordrless_hook::{
    pool_flags, token_flags, Phase, PoolHookArgs, PoolOp, TokenHookArgs, TokenOp,
};
use bordrless_kit::{modules, HolderData, KitConfig};
use bordrless_launch::client as launch;
use bordrless_launch::state::Launch;
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::hooks::{fixed, NewPool, SwapSpec};
use bordrless_program_tests::kit::{check_kit, code_of, KitSpec};
use bordrless_program_tests::launch::{presets, SOL, VQ};
use bordrless_swap::client as swap;
use bordrless_token::client as token;
use hook_tester::client as tester;
use hook_tester::{callback, mode};
use solana_keypair::Keypair;
use solana_signer::Signer;

const SUPPLY: u64 = 1_000_000_000_000_000;
const MIN_ELIGIBLE: u64 = SUPPLY / 1_000;

/// Whether `tx` failed because `program` itself refused it with error `code`, named `name` in
/// Anchor's log (a code alone proves nothing: the programs share their numbers).
fn refused_by(tx: &Tx, program: &Pubkey, code: u32, name: &str) -> bool {
    if code_of(tx) != Some(code) {
        return false;
    }
    let failed = format!("Program {program} failed: custom program error: {code:#x}");
    let named = format!("Error Code: {name}.");
    let logs = tx.logs();
    logs.iter().any(|l| l.contains(&failed)) && logs.iter().any(|l| l.contains(&named))
}

#[track_caller]
fn expect_refused(tx: &Tx, program: &Pubkey, code: u32, name: &str) {
    assert!(
        refused_by(tx, program, code, name),
        "expected {name} ({code}) from {program}\n{}",
        tx.logs().join("\n")
    );
}

/// The token arguments of a transfer of `mint` the attacker makes up.
#[allow(clippy::too_many_arguments)]
fn forged_transfer(
    mint: Pubkey,
    source_owner: Pubkey,
    destination_owner: Pubkey,
    amount: u64,
    source_balance: u64,
    source_hook_data: [u8; 64],
) -> TokenHookArgs {
    TokenHookArgs {
        op: TokenOp::Transfer,
        phase: Phase::Before,
        mint,
        source: Pubkey::new_unique(),
        destination: Pubkey::new_unique(),
        source_owner,
        destination_owner,
        authority: source_owner,
        authority_is_delegate: false,
        amount,
        delta: 0,
        source_balance,
        destination_balance: 0,
        decimals: 6,
        supply: SUPPLY,
        source_hook_data,
        destination_hook_data: [0; 64],
    }
}

/// An attacker's mint whose hook is `hook_tester`, its `before_transfer` scripted to call
/// `target` with `data` and `accounts`, passing on the token program's signer it receives. Answers
/// the attacker (holding the whole supply), the mint, and a second holder to transfer to.
fn attacker_mint(
    w: &mut World,
    target: Pubkey,
    accounts: Vec<(Pubkey, bool)>,
    data: Vec<u8>,
) -> (Keypair, Pubkey, Pubkey) {
    let attacker = w.env.funded(10 * SOL);
    let mint = Keypair::new();
    let mut extras = vec![fixed(target, false)];
    extras.extend(
        accounts
            .into_iter()
            .map(|(key, writable)| fixed(key, writable)),
    );
    let a = w.tester_mint(
        &mint,
        &attacker,
        token_flags::BEFORE_TRANSFER,
        1_000_000,
        extras,
    );
    w.env
        .send_paid_by(
            &[tester::forward(
                attacker.pubkey(),
                a,
                callback::BEFORE_TRANSFER,
                data,
            )],
            &attacker,
            &[],
        )
        .ok();
    let other = Keypair::new().pubkey();
    w.holdings(&attacker, a, &[other]);
    (attacker, a, other)
}

#[test]
fn a_token_hook_cannot_pass_its_signer_on_to_the_kit() {
    let mut w = World::new();
    // The victim: a kit token with holder rewards, three holders and 10 SOL that arrived for them.
    let d = w.direct_kit(&KitSpec::new(modules::HOLDER_REWARDS));
    let t = d.token.clone();
    let holders: Vec<Keypair> = (0..3).map(|_| w.kit_holder(&t, SOL)).collect();
    for (h, share) in holders.iter().zip([30u64, 20, 10]) {
        w.send_tokens(&d.pool, t.mint, &h.pubkey(), SUPPLY / 1_000 * share)
            .ok();
    }
    let donor = w.wallet_with_sol(20 * SOL);
    w.donate(&donor, &t, 10 * SOL).ok();
    w.send_tokens(&holders[0], t.mint, &holders[1].pubkey(), 1)
        .ok();
    let before: KitConfig = w.env.kit_config(&t.mint);
    assert!(before.eligible > before.min_eligible && before.acc_per_share > 0);

    // The forged callback of the review: a ghost holder "sells" eligible - min_eligible into the
    // pool with a snapshot at the accumulator, so it earns nothing and `eligible` drops to the
    // threshold (then one holder's donation takes everyone's rewards); or all of it, which makes
    // every sell revert.
    for amount in [before.eligible - before.min_eligible, before.eligible] {
        let mut data = [0u8; 64];
        data[..16].copy_from_slice(&before.acc_per_share.to_le_bytes());
        let args = forged_transfer(
            t.mint,
            Keypair::new().pubkey(),
            t.pool,
            amount,
            amount,
            data,
        );
        let ix = bordrless_kit::instruction::BeforeTransfer { args }.data();
        let (attacker, a, other) = attacker_mint(
            &mut w,
            bordrless_kit::ID,
            vec![
                (t.mint, false),
                (Pubkey::new_unique(), false),
                (Pubkey::new_unique(), false),
                (Pubkey::new_unique(), false),
                (t.kit_config, true),
                (t.reward_vault.unwrap(), false),
            ],
            ix,
        );
        // The attacker moves its own token; its hook passes the token program's signer on.
        let tx = w.tester_transfer(&attacker, a, &attacker.pubkey(), &other, 1);
        expect_refused(
            &tx,
            &bordrless_kit::ID,
            u32::from(bordrless_kit::error::KitError::BadHookSigner),
            "BadHookSigner",
        );
        assert_eq!(
            w.env.kit_config(&t.mint),
            before,
            "the victim's config is untouched"
        );
        // The forward itself works: scripted to answer nothing, the attacker's token moves.
        w.script_raw(&attacker, a, callback::BEFORE_TRANSFER, mode::NONE, vec![]);
        w.tester_transfer(&attacker, a, &attacker.pubkey(), &other, 1)
            .ok();
    }
    // The victim trades on: a holder sells into the pool.
    w.send_tokens(&holders[2], t.mint, &t.pool, SUPPLY / 1_000)
        .ok();
    let owners: Vec<Pubkey> = holders.iter().map(|h| h.pubkey()).collect();
    check_kit(&w.env, &t, &owners);
}

#[test]
fn a_token_hook_cannot_pass_its_signer_on_to_tax_hook() {
    let mut w = World::new();
    let owner = w.env.funded(10 * SOL);
    let collector = Keypair::new().pubkey();
    let taxed = w.tax_mint(&owner, 6, SUPPLY, "TAX", collector, 100);
    let tax = Pubkey::find_program_address(&[tax_hook::TAX_SEED, taxed.as_ref()], &tax_hook::ID).0;
    let collector_holding = token::holding_address(&taxed, &collector);
    let before: tax_hook::TaxConfig = w.env.read(&tax);
    // A fee on an amount near the limit, again and again, would run `collected` to u64::MAX.
    let ghost = Keypair::new().pubkey();
    let args = forged_transfer(
        taxed,
        ghost,
        Keypair::new().pubkey(),
        u64::MAX / 2,
        u64::MAX / 2,
        [0; 64],
    );
    let ix = tax_hook::instruction::BeforeTransfer { args }.data();
    let (attacker, a, other) = attacker_mint(
        &mut w,
        tax_hook::ID,
        vec![
            (taxed, false),
            (Pubkey::new_unique(), false),
            (Pubkey::new_unique(), false),
            (Pubkey::new_unique(), false),
            (tax, true),
            (collector_holding, true),
        ],
        ix,
    );
    let tx = w.tester_transfer(&attacker, a, &attacker.pubkey(), &other, 1);
    expect_refused(
        &tx,
        &tax_hook::ID,
        u32::from(tax_hook::TaxError::BadHookSigner),
        "BadHookSigner",
    );
    let after: tax_hook::TaxConfig = w.env.read(&tax);
    assert_eq!(after.collected, before.collected);
    // The taxed token still moves, and pays its tax.
    let bob = Keypair::new().pubkey();
    w.holdings(&owner, taxed, &[bob]);
    w.send_tokens(&owner, taxed, &bob, 10_000).ok();
    assert_eq!(w.env.holding(&taxed, &bob), 9_900);
    assert_eq!(w.env.read::<tax_hook::TaxConfig>(&tax).collected, 100);
}

#[test]
fn a_pool_hook_cannot_pass_its_signer_on_to_the_launch() {
    let mut w = World::new();
    // The victim: a launch with holder rewards, inside its sniper window.
    let creator = w.wallet_with_sol(5 * SOL);
    let (mint, tx) = w.create_launch_with(&creator, "VIC", 50, VQ, presets::rewards_and_cap());
    tx.ok();
    let launch_key = launch::launch_address(&mint);
    let before: Launch = w.env.read(&launch_key);
    let victim_pool = before.pool;

    // The attacker's pool: two plain mints, `hook_tester` as the pool hook with `before_swap`,
    // which calls the launch's `before_swap` with the DEX's signer it receives: a buy by the
    // creator into the creator's wallet of nearly all of u64, in the sniper window.
    let attacker = w.env.funded(10 * SOL);
    let x = w.mint_to_owner(&attacker, 6, 1_000_000_000_000, "XX");
    let y = w.mint_to_owner(&attacker, 6, 1_000_000_000_000, "YY");
    let args = PoolHookArgs {
        op: PoolOp::Swap,
        phase: Phase::Before,
        pool: victim_pool,
        base_mint: mint,
        quote_mint: w.sol,
        actor: creator.pubkey(),
        recipient: creator.pubkey(),
        direction: 1,
        amount_in: u64::MAX / 4,
        amount_out: 0,
        base_reserve: 0,
        quote_reserve: 0,
        virtual_base: 0,
        virtual_quote: 0,
        lp_fee_bps: 100,
        protocol_fee_bps: 100,
        swap_count: 0,
        created_at: 0,
        lp_amount: 0,
        hook_data: vec![],
    };
    let data = bordrless_launch::instruction::BeforeSwap { args }.data();
    let extras = vec![
        fixed(bordrless_launch::ID, false),
        fixed(victim_pool, false),
        fixed(mint, false),
        fixed(w.sol, false),
        fixed(creator.pubkey(), false),
        fixed(launch_key, true),
        fixed(before.quote_holding, true),
        fixed(before.holder_vault, false),
        fixed(before.kit_config, false),
    ];
    let (pool, tx) = w.new_pool(
        &attacker,
        &NewPool {
            base: x,
            quote: y,
            lp_fee_bps: 30,
            tester_flags: Some(pool_flags::BEFORE_SWAP),
            extras,
            base_amount: 1_000_000_000,
            quote_amount: 1_000_000_000,
        },
    );
    tx.ok();
    w.env
        .send_paid_by(
            &[tester::forward(
                attacker.pubkey(),
                pool,
                callback::BEFORE_SWAP,
                data,
            )],
            &attacker,
            &[],
        )
        .ok();
    let tx = w.env.send_paid_by(
        &[w.env
            .swap_ix(&SwapSpec::new(attacker.pubkey(), pool, 1, 1_000))],
        &attacker,
        &[],
    );
    expect_refused(
        &tx,
        &bordrless_launch::ID,
        u32::from(bordrless_launch::error::LaunchError::BadHookSigner),
        "BadHookSigner",
    );
    let after: Launch = w.env.read(&launch_key);
    assert_eq!(
        (
            after.creator_fees_accrued,
            after.holder_fees_accrued,
            after.burned_on_trades,
            after.creator_bought
        ),
        (
            before.creator_fees_accrued,
            before.holder_fees_accrued,
            before.burned_on_trades,
            before.creator_bought
        )
    );
    // Scripted to answer nothing, the attacker's pool swaps; the creator's own first buy still
    // pays the normal LP fee.
    w.script_raw(&attacker, pool, callback::BEFORE_SWAP, mode::NONE, vec![]);
    w.env
        .send_paid_by(
            &[w.env
                .swap_ix(&SwapSpec::new(attacker.pubkey(), pool, 1, 1_000))],
            &attacker,
            &[],
        )
        .ok();
    w.holdings(&creator, mint, &[creator.pubkey()]);
    let ix = w.launch_swap_ix(&creator.pubkey(), &mint, 1, SOL / 100, 0);
    let tx = w.env.send_paid_by(&[ix], &creator, &[]);
    tx.ok();
    let swapped: bordrless_swap::events::Swapped = tx.event();
    assert_eq!(swapped.lp_fee_bps, before.lp_fee_bps);
    assert!(w.launch(&mint).creator_bought);
}

#[test]
fn the_callers_check_the_hook_signer_they_are_given() {
    let mut w = World::new();
    let d = w.direct_kit(&KitSpec::new(modules::HOLDER_REWARDS));
    let t = d.token.clone();
    let alice = w.kit_holder(&t, SOL);
    let bob = w.kit_holder(&t, SOL);
    w.send_tokens(&d.pool, t.mint, &alice.pubkey(), MIN_ELIGIBLE)
        .ok();
    let token_code = |e: bordrless_token::error::TokenError| u32::from(e);
    let bad_token_signer = token_code(bordrless_token::error::TokenError::BadHookSigner);
    // The token program: a transfer of a kit token takes the token program's signer for the kit
    // in its hook-signer slot (index 5); another hook's, none (the token program's id) or the
    // old single authority are refused.
    let old =
        Pubkey::find_program_address(&[bordrless_hook::HOOK_AUTHORITY_SEED], &bordrless_token::ID)
            .0;
    for wrong in [
        token::hook_signer(&tax_hook::ID),
        token::hook_signer(&hook_tester::ID),
        bordrless_token::ID,
        old,
    ] {
        let mut ix = w.kit_transfer_ix(t.mint, alice.pubkey(), &alice.pubkey(), &bob.pubkey(), 1);
        assert_eq!(
            ix.accounts[5].pubkey,
            bordrless_kit::constants::TOKEN_HOOK_AUTHORITY
        );
        ix.accounts[5] = AccountMeta::new_readonly(wrong, false);
        let tx = w.env.send_paid_by(&[ix], &alice, &[]);
        expect_refused(&tx, &bordrless_token::ID, bad_token_signer, "BadHookSigner");
    }
    // A burn and a mint likewise (index 4).
    let mut ix = w.kit_burn_ix(t.mint, alice.pubkey(), &alice.pubkey(), 1);
    ix.accounts[4] = AccountMeta::new_readonly(token::hook_signer(&tax_hook::ID), false);
    let tx = w.env.send_paid_by(&[ix], &alice, &[]);
    expect_refused(&tx, &bordrless_token::ID, bad_token_signer, "BadHookSigner");
    // A mint without a hook takes anything there: it is never used.
    let plain = w.mint_to_owner(&alice, 6, 1_000, "PLN");
    w.holdings(&alice, plain, &[bob.pubkey()]);
    let mut ix = token::transfer(
        alice.pubkey(),
        token::holding_address(&plain, &alice.pubkey()),
        token::holding_address(&plain, &bob.pubkey()),
        plain,
        None,
        vec![],
        1,
    );
    ix.accounts[5] = AccountMeta::new_readonly(old, false);
    w.env.send_paid_by(&[ix], &alice, &[]).ok();
    // As built, the kit transfer lands.
    let ix = w.kit_transfer_ix(t.mint, alice.pubkey(), &alice.pubkey(), &bob.pubkey(), 1);
    w.env.send_paid_by(&[ix], &alice, &[]).ok();

    // The DEX: a launch-pool swap takes the DEX's signer for the launch (index 10), and in the
    // kit slice the token program's signer for the kit (the slice's second account).
    let creator = w.wallet_with_sol(5 * SOL);
    let (mint, tx) = w.create_launch_with(&creator, "SGN", 50, VQ, presets::rewards_and_cap());
    tx.ok();
    let trader = w.wallet_with_sol(2 * SOL);
    w.holdings(&trader, mint, &[trader.pubkey()]);
    let swap_code = u32::from(bordrless_swap::error::SwapError::BadHookSigner);
    for wrong in [
        swap::hook_signer(&hook_tester::ID),
        bordrless_swap::ID,
        Pubkey::find_program_address(&[bordrless_hook::HOOK_AUTHORITY_SEED], &bordrless_swap::ID).0,
    ] {
        let mut ix = w.launch_swap_ix(&trader.pubkey(), &mint, 1, SOL / 100, 0);
        assert_eq!(
            ix.accounts[10].pubkey,
            bordrless_launch::constants::DEX_HOOK_AUTHORITY
        );
        ix.accounts[10] = AccountMeta::new_readonly(wrong, false);
        let tx = w.env.send_paid_by(&[ix], &trader, &[]);
        expect_refused(&tx, &bordrless_swap::ID, swap_code, "BadHookSigner");
    }
    let mut ix = w.launch_swap_ix(&trader.pubkey(), &mint, 1, SOL / 100, 0);
    let slice_signer = ix
        .accounts
        .iter()
        .position(|m| m.pubkey == bordrless_kit::constants::TOKEN_HOOK_AUTHORITY)
        .expect("the kit slice carries the token program's signer for the kit");
    assert_eq!(ix.accounts[slice_signer - 1].pubkey, bordrless_kit::ID);
    ix.accounts[slice_signer] =
        AccountMeta::new_readonly(token::hook_signer(&hook_tester::ID), false);
    let tx = w.env.send_paid_by(&[ix], &trader, &[]);
    expect_refused(&tx, &bordrless_token::ID, bad_token_signer, "BadHookSigner");
    // As built, the swap lands.
    let ix = w.launch_swap_ix(&trader.pubkey(), &mint, 1, SOL / 100, 0);
    w.env.send_paid_by(&[ix], &trader, &[]).ok();
    // The kit's hook data of the trader was written by the kit's own callback.
    let data = HolderData::read(&w.env.hook_data(&mint, &trader.pubkey()));
    assert_eq!(data.owed, 0);
}
