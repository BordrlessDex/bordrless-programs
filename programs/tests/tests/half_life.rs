//! `half_life` on a real launch: prepared for the mint before it exists, named by a
//! `LaunchConfig`, launched, lit. Buys are free and stamp the buyer's holding; sells pay an exit
//! fee by the age of the seller's tokens (20%, halving every six hours, nothing after 48), taken
//! from the amount into the furnace, which anyone can stoke to burn; wallet-to-wallet transfers
//! pay the same fee and carry the sender's age; an emptied holding is cleared; nobody exits before
//! the furnace is lit; the curve graduates through the hook.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{InstructionData, ToAccountMetas};
use bordrless_hook::hook_accounts_address;
use bordrless_launch::constants::STATUS_GRADUATED;
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{Tx, SYSTEM_PROGRAM_ID};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_swap::events::Swapped;
use bordrless_token::client as token;
use bordrless_token::state::Mint;
use half_life::{
    blend, fee_on, fee_ppm, furnace_owner, read_since, state_address, HalfLifeError, HalfLifeState,
    FLAGS, HALF_LIFE_SECS,
};
use solana_keypair::Keypair;
use solana_signer::Signer;

const H: i64 = HALF_LIFE_SECS;

#[track_caller]
fn refused(tx: &Tx, e: HalfLifeError) {
    tx.expect_code(u32::from(e));
    let name = format!("{e:?}");
    assert!(
        tx.logs()
            .iter()
            .any(|l| l.contains(&format!("Error Code: {name}"))),
        "expected {name}\n{}",
        tx.logs().join("\n")
    );
}

fn prepare_ix(payer: Pubkey, mint: Pubkey) -> Instruction {
    Instruction {
        program_id: half_life::ID,
        accounts: half_life::accounts::Prepare {
            payer,
            mint,
            state: state_address(&mint),
            registry: hook_accounts_address(&half_life::ID, &mint).0,
            system_program: SYSTEM_PROGRAM_ID,
        }
        .to_account_metas(None),
        data: half_life::instruction::Prepare {}.data(),
    }
}

fn furnace(mint: &Pubkey) -> (Pubkey, Pubkey) {
    let owner = furnace_owner(mint).0;
    (owner, token::holding_address(mint, &owner))
}

fn light_ix(payer: Pubkey, mint: Pubkey) -> Instruction {
    let (owner, holding) = furnace(&mint);
    Instruction {
        program_id: half_life::ID,
        accounts: half_life::accounts::Light {
            payer,
            state: state_address(&mint),
            mint,
            furnace_owner: owner,
            furnace_holding: holding,
            token_program: bordrless_token::ID,
            token_event_authority: token::event_authority(),
            system_program: SYSTEM_PROGRAM_ID,
        }
        .to_account_metas(None),
        data: half_life::instruction::Light {}.data(),
    }
}

fn stoke_ix(mint: Pubkey) -> Instruction {
    let (owner, holding) = furnace(&mint);
    Instruction {
        program_id: half_life::ID,
        accounts: half_life::accounts::Stoke {
            state: state_address(&mint),
            mint,
            furnace_owner: owner,
            furnace_holding: holding,
            this_program: half_life::ID,
            hook_signer: half_life::TOKEN_HOOK_SIGNER,
            token_program: bordrless_token::ID,
            token_event_authority: token::event_authority(),
        }
        .to_account_metas(None),
        data: half_life::instruction::Stoke {}.data(),
    }
}

/// A launch whose token runs Half-Life: prepared, a config naming it (no kit rules, creator fee
/// 1%), launched from it, past the sniper window. Not lit. Answers the creator and the mint.
fn launch_half_life(w: &mut World) -> (Keypair, Pubkey) {
    let creator = w.wallet_with_sol(20 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    w.env
        .send_paid_by(&[prepare_ix(creator.pubkey(), mint)], &creator, &[])
        .ok();
    let (config, tx) = w.create_config(
        &creator,
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: 100,
            custom_hook: Some(half_life::ID),
            custom_hook_flags: FLAGS,
            label: "Half-Life".to_string(),
        },
    );
    tx.ok();
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint, "HALF", VQ, &config);
    w.env.send_paid_by(&[ix], &creator, &[&mint_kp]).ok();
    assert_eq!(w.launch(&mint).custom_hook, Some(half_life::ID));
    w.env.warp(31);
    (creator, mint)
}

fn light(w: &mut World, payer: &Keypair, mint: &Pubkey) {
    w.env
        .send_paid_by(&[light_ix(payer.pubkey(), *mint)], payer, &[])
        .ok();
}

fn trader(w: &mut World, mint: &Pubkey, sol: u64) -> Keypair {
    let t = w.wallet_with_sol(sol);
    w.holdings(&t, *mint, &[t.pubkey()]);
    t
}

fn swap(w: &mut World, mint: &Pubkey, t: &Keypair, buy: bool, amount: u64) -> Tx {
    let ix = w.launch_swap_ix(&t.pubkey(), mint, u8::from(buy), amount, 0);
    w.env.send_paid_by(&[ix], t, &[])
}

fn since(w: &World, mint: &Pubkey, owner: &Pubkey) -> Option<i64> {
    read_since(&w.env.hook_data(mint, owner))
}

fn state(w: &World, mint: &Pubkey) -> HalfLifeState {
    w.env.read(&state_address(mint))
}

/// A sell of `amount` by `t`: it lands, the hook took `fee` from the transfer into the pool (the
/// DEX measures it as the input's cut), and the furnace gained it.
#[track_caller]
fn sell_paying(w: &mut World, mint: &Pubkey, t: &Keypair, amount: u64, fee: u64) {
    let furnace_before = w.env.holding(mint, &furnace(mint).0);
    let tx = swap(w, mint, t, false, amount);
    tx.ok();
    let ev: Swapped = tx.event();
    assert_eq!(
        (ev.cuts_in, ev.received_in),
        (fee, amount - fee),
        "the exit fee"
    );
    assert_eq!(w.env.holding(mint, &furnace(mint).0), furnace_before + fee);
}

#[test]
fn prepare_publishes_the_registry_and_the_state() {
    let mut w = World::new();
    let (creator, mint) = launch_half_life(&mut w);
    let s = state(&w, &mint);
    let (owner, holding) = furnace(&mint);
    assert_eq!(
        (
            s.mint,
            s.launch,
            s.furnace_owner,
            s.furnace_holding,
            s.prepared_by
        ),
        (
            mint,
            bordrless_launch::client::launch_address(&mint),
            owner,
            holding,
            creator.pubkey()
        )
    );
    // Preparing again is refused: what was prepared stays.
    let tx = w
        .env
        .send_paid_by(&[prepare_ix(creator.pubkey(), mint)], &creator, &[]);
    tx.expect_fail();
}

#[test]
fn sells_pay_an_exit_fee_that_halves_every_six_hours() {
    let mut w = World::new();
    let (creator, mint) = launch_half_life(&mut w);
    let t = trader(&mut w, &mint, 10 * SOL);

    // Buys are free, before the furnace is lit too, and stamp the buyer's holding with now.
    let tx = swap(&mut w, &mint, &t, true, SOL);
    tx.ok();
    let ev: Swapped = tx.event();
    assert_eq!(ev.cuts_out, 0, "no exit fee on a buy");
    let bought_at = w.env.now;
    assert_eq!(since(&w, &mint, &t.pubkey()), Some(bought_at));
    // The pool was read from the launch and is exempt: its vault is never stamped.
    assert_eq!(state(&w, &mint).pool, w.launch(&mint).pool);
    assert_eq!(since(&w, &mint, &w.launch(&mint).pool), None);
    let bal = w.env.holding(&mint, &t.pubkey());
    let part = bal / 4;

    // Nobody exits before the furnace is lit.
    let tx = swap(&mut w, &mint, &t, false, part);
    refused(&tx, HalfLifeError::FurnaceNotLit);
    light(&mut w, &creator, &mint);

    // Age 0: 20%.
    sell_paying(&mut w, &mint, &t, part, fee_on(part, 200_000));
    assert_eq!(fee_on(part, 200_000), part / 5);
    // Selling does not change when the remaining tokens arrived.
    assert_eq!(since(&w, &mint, &t.pubkey()), Some(bought_at));
    // Six hours: 10%. Nine: 7.5%.
    w.env.warp(H);
    sell_paying(&mut w, &mint, &t, part, fee_on(part, 100_000));
    w.env.warp(H / 2);
    sell_paying(&mut w, &mint, &t, part / 2, fee_on(part / 2, 75_000));
    // 48 hours: nothing. The last sell empties the holding and clears its data.
    w.env.warp(8 * H - H - H / 2);
    assert_eq!(fee_ppm(w.env.now - bought_at), 0);
    let rest = w.env.holding(&mint, &t.pubkey());
    sell_paying(&mut w, &mint, &t, rest, 0);
    assert_eq!(w.env.holding(&mint, &t.pubkey()), 0);
    assert_eq!(w.env.hook_data(&mint, &t.pubkey()), [0u8; 64]);
    let s = state(&w, &mint);
    assert_eq!(s.fed, w.env.holding(&mint, &furnace(&mint).0));
}

#[test]
fn age_travels_with_the_tokens_and_blends_by_weight() {
    let mut w = World::new();
    let (creator, mint) = launch_half_life(&mut w);
    light(&mut w, &creator, &mint);
    let a = trader(&mut w, &mint, 10 * SOL);
    let b = trader(&mut w, &mint, 10 * SOL);
    swap(&mut w, &mint, &a, true, SOL).ok();
    let a_since = w.env.now;
    w.env.warp(H);

    // A sends to B, six hours in: A pays 10%, and B's tokens arrive with A's age, not now.
    let amount = w.env.holding(&mint, &a.pubkey()) / 2;
    let ix = w.hooked_transfer_ix(
        half_life::ID,
        mint,
        a.pubkey(),
        &a.pubkey(),
        &b.pubkey(),
        amount,
    );
    w.env.send_paid_by(&[ix], &a, &[]).ok();
    let fee = fee_on(amount, 100_000);
    assert_eq!(w.env.holding(&mint, &b.pubkey()), amount - fee);
    assert_eq!(w.env.holding(&mint, &furnace(&mint).0), fee);
    assert_eq!(since(&w, &mint, &b.pubkey()), Some(a_since));
    // So B sells at 10% too: moving tokens to a fresh wallet resets nothing.
    let b_bal = w.env.holding(&mint, &b.pubkey());
    sell_paying(&mut w, &mint, &b, b_bal / 2, fee_on(b_bal / 2, 100_000));

    // B buys more now: its age blends by weight with the new tokens' arrival.
    let held = w.env.holding(&mint, &b.pubkey());
    swap(&mut w, &mint, &b, true, SOL).ok();
    let added = w.env.holding(&mint, &b.pubkey()) - held;
    assert_eq!(
        since(&w, &mint, &b.pubkey()),
        Some(blend(held, a_since, added, w.env.now))
    );
}

#[test]
fn the_furnace_burns_what_paper_hands_paid() {
    let mut w = World::new();
    let (creator, mint) = launch_half_life(&mut w);
    light(&mut w, &creator, &mint);
    let t = trader(&mut w, &mint, 10 * SOL);
    swap(&mut w, &mint, &t, true, SOL).ok();
    let part = w.env.holding(&mint, &t.pubkey()) / 2;
    sell_paying(&mut w, &mint, &t, part, fee_on(part, 200_000));

    let in_furnace = w.env.holding(&mint, &furnace(&mint).0);
    assert!(in_furnace > 0);
    let supply = w.env.read::<Mint>(&mint).supply;
    // Anyone stokes it: a stranger pays the fee.
    let stranger = w.env.funded(SOL);
    w.env.send_paid_by(&[stoke_ix(mint)], &stranger, &[]).ok();
    assert_eq!(w.env.holding(&mint, &furnace(&mint).0), 0);
    assert_eq!(w.env.read::<Mint>(&mint).supply, supply - in_furnace);
    assert_eq!(state(&w, &mint).burned, in_furnace);
    // An empty furnace has nothing to burn.
    let tx = w.env.send_paid_by(&[stoke_ix(mint)], &stranger, &[]);
    refused(&tx, HalfLifeError::FurnaceEmpty);
}

#[test]
fn a_half_life_launch_graduates_and_keeps_charging() {
    let mut w = World::new();
    let (creator, mint) = launch_half_life(&mut w);
    light(&mut w, &creator, &mint);
    let (wallets, tx) = w.graduate_launch(&mint);
    tx.ok();
    assert_eq!(w.launch(&mint).status, STATUS_GRADUATED);
    // Every buyer was stamped; the launch's reserve went to the pool free, and the pool stays
    // exempt: its vault was never stamped.
    for wallet in &wallets {
        if w.env.holding(&mint, &wallet.pubkey()) > 0 {
            assert!(since(&w, &mint, &wallet.pubkey()).is_some());
        }
    }
    assert_eq!(since(&w, &mint, &w.launch(&mint).pool), None);
    assert_eq!(w.env.holding(&mint, &furnace(&mint).0), 0);
    // After graduation a fresh seller still pays by age.
    let last = wallets.last().unwrap();
    let bought = since(&w, &mint, &last.pubkey()).unwrap();
    let amount = w.env.holding(&mint, &last.pubkey()) / 10;
    let fee = fee_on(amount, fee_ppm(w.env.now - bought));
    assert!(fee > 0);
    sell_paying(&mut w, &mint, last, amount, fee);
}

#[test]
fn the_callback_only_takes_the_token_programs_signer() {
    let mut w = World::new();
    let (creator, mint) = launch_half_life(&mut w);
    // A direct call to `before_transfer`, signed by someone else, is refused.
    let fake = Keypair::new();
    let t = w.env.funded(SOL);
    let mut metas = vec![
        AccountMeta::new_readonly(fake.pubkey(), true),
        AccountMeta::new_readonly(mint, false),
        AccountMeta::new_readonly(token::holding_address(&mint, &t.pubkey()), false),
        AccountMeta::new_readonly(token::holding_address(&mint, &creator.pubkey()), false),
        AccountMeta::new_readonly(t.pubkey(), false),
    ];
    metas.push(AccountMeta::new(state_address(&mint), false));
    metas.push(AccountMeta::new(furnace(&mint).1, false));
    metas.push(AccountMeta::new_readonly(
        bordrless_launch::client::launch_address(&mint),
        false,
    ));
    let args = bordrless_hook::TokenHookArgs {
        op: bordrless_hook::TokenOp::Transfer,
        phase: bordrless_hook::Phase::Before,
        mint,
        source: token::holding_address(&mint, &t.pubkey()),
        destination: token::holding_address(&mint, &creator.pubkey()),
        source_owner: t.pubkey(),
        destination_owner: creator.pubkey(),
        authority: t.pubkey(),
        authority_is_delegate: false,
        amount: 1,
        delta: 0,
        source_balance: 1,
        destination_balance: 0,
        decimals: 6,
        supply: 1,
        source_hook_data: [0; 64],
        destination_hook_data: [0; 64],
    };
    let ix = Instruction {
        program_id: half_life::ID,
        accounts: metas,
        data: half_life::instruction::BeforeTransfer { args }.data(),
    };
    let tx = w.env.send_paid_by(&[ix], &t, &[&fake]);
    refused(&tx, HalfLifeError::BadHookSigner);
}

/// The site's launch, three transactions: `prepare`; `create_launch` from the config, a v0
/// transaction with the protocol lookup table carrying the longest metadata the site sends (name
/// 32 and ticker 10 bytes, the most the form takes, and a 128-byte URI: the site's are
/// `<pinata gateway>/ipfs/<CIDv1>`, about 100; the launchpad would take 200, which with a custom
/// hook no longer fits a transaction), the hook's accounts built without reading the chain; then
/// `light`. The launch fits mainnet's limits; between it and `light` buys work and sells are
/// refused; after it the token trades with the fee from its first sell.
#[test]
fn prepare_launch_then_light_fit_mainnet_limits() {
    use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price};
    use solana_message::AddressLookupTableAccount;
    let mut w = World::new();
    let table: AddressLookupTableAccount = w
        .env
        .put_lookup_table(Pubkey::new_unique(), &protocol_lookup_table(&w));
    let creator = w.wallet_with_sol(20 * SOL);
    let (config, tx) = w.create_config(
        &creator,
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: 100,
            custom_hook: Some(half_life::ID),
            custom_hook_flags: FLAGS,
            label: "Half-Life".to_string(),
        },
    );
    tx.ok();
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    w.env
        .send_paid_by(&[prepare_ix(creator.pubkey(), mint)], &creator, &[])
        .ok();
    // The registry's extras, in its order: the state, the furnace holding, the launch account.
    let custom = bordrless_launch::client::CustomHookAccounts {
        program: half_life::ID,
        extras: vec![
            AccountMeta::new(state_address(&mint), false),
            AccountMeta::new(furnace(&mint).1, false),
            AccountMeta::new_readonly(bordrless_launch::client::launch_address(&mint), false),
        ],
    };
    // They are what a client resolves from the registry.
    assert_eq!(
        custom.extras,
        w.custom_hook_accounts(&half_life::ID, &mint).extras
    );
    let c = w.launch_config(&config);
    let mut args = World::launch_args("HALFLIFE10", c.creator_fee_bps, VQ, c.rules);
    args.name = "N".repeat(32);
    args.uri = format!("https://{}", "u".repeat(120));
    assert_eq!(
        (args.name.len(), args.symbol.len(), args.uri.len()),
        (32, 10, 128)
    );
    let launch_ix = bordrless_launch::client::create_launch_with(
        creator.pubkey(),
        mint,
        w.env.treasury.pubkey(),
        w.sol,
        bordrless_core::policy::LP_FEE_BPS,
        args,
        Some(config),
        Some(&custom),
    );
    let ixs = [
        compute_unit_limit(1_400_000),
        compute_unit_price(20_000),
        launch_ix,
    ];
    let tx = w.env.send_v0(&ixs, &creator, &[&mint_kp], &[table]);
    tx.ok();
    println!(
        "create_launch, Half-Life, the site's longest metadata: {} keys, {} v0 bytes, {} trace, height {}, {} CU",
        tx.keys.len(),
        tx.size,
        tx.trace_len(),
        tx.max_height(),
        tx.cu()
    );
    assert!(tx.size <= 1_232, "{} bytes", tx.size);
    assert!(tx.trace_len() <= 64, "trace {}", tx.trace_len());
    assert!(tx.max_height() <= 5, "height {}", tx.max_height());
    assert_eq!(w.launch(&mint).custom_hook, Some(half_life::ID));

    // Before `light`: a buy lands, a sell is refused.
    w.env.warp(31);
    let early = trader(&mut w, &mint, 10 * SOL);
    swap(&mut w, &mint, &early, true, SOL).ok();
    let tx = swap(&mut w, &mint, &early, false, 1_000);
    refused(&tx, HalfLifeError::FurnaceNotLit);
    let tx = w
        .env
        .send_paid_by(&[light_ix(creator.pubkey(), mint)], &creator, &[]);
    tx.ok();
    assert!(
        w.env.account(&furnace(&mint).1).is_some(),
        "the furnace is lit"
    );
    let part = w.env.holding(&mint, &early.pubkey()) / 2;
    sell_paying(&mut w, &mint, &early, part, fee_on(part, 200_000));
}
