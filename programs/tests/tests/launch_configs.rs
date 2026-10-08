//! The fee share, launch configs and build-your-own tokens (`docs/hooks-v2.md` §3.1, §5.7,
//! §5.8): Bordrless takes a quarter of what each preset's rules collect, on buys and on sells,
//! and nothing from a launch whose rules collect nothing; a token launched from a `LaunchConfig`
//! made with the SDK; a launch whose token hook is the creator's own program (`hook_tester`, and
//! `tax_hook` prepared for the mint), its slice on the supply mint, the pool deposit, every swap
//! and the graduation, with the pool hook's rules still applied and the DEX sharing every cut;
//! every refusal; a honeypot hook (sells refused, buys through, the pool sound); and the runtime
//! budgets of `create_launch` with a config and with a custom hook.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::InstructionData;
use bordrless_core::policy::presets as core_presets;
use bordrless_core::{fee_amount, policy, protocol_share, quote_value, swap_out};
use bordrless_hook::{hook_accounts_address, token_flags};
use bordrless_launch::client as launch;
use bordrless_launch::constants::{KIT_ID, STATUS_GRADUATED};
use bordrless_launch::error::LaunchError;
use bordrless_launch::events::{LaunchConfigCreated, LaunchCreated};
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::{LaunchConfig, LaunchRules};
use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price, Tx, SYSTEM_PROGRAM_ID};
use bordrless_program_tests::fixture::{policy_launch_config, World};
use bordrless_program_tests::launch::*;
use bordrless_swap::events::{DeltaPaid, Swapped};
use bordrless_token::client as token;
use bordrless_token::state::Mint;
use hook_tester::client as tester;
use hook_tester::{callback, Script, TesterError};
use solana_keypair::Keypair;
use solana_message::AddressLookupTableAccount;
use solana_signer::Signer;

/// Bordrless's share of the hooks' cuts under the policy.
const SHARE: u16 = policy::LAUNCH_PROTOCOL_SHARE_BPS;

fn lcode(e: LaunchError) -> u32 {
    u32::from(e)
}

/// Asserts a failure with `code`, raised under `name` (codes of different programs overlap).
#[track_caller]
fn expect_err(tx: &Tx, code: u32, name: &str) {
    tx.expect_code(code);
    assert!(
        tx.logs()
            .iter()
            .any(|l| l.contains(&format!("Error Code: {name}"))),
        "expected {name}\n{}",
        tx.logs().join("\n")
    );
}

#[track_caller]
fn refused(tx: &Tx, e: LaunchError) {
    expect_err(tx, lcode(e), &format!("{e:?}"));
}

/// A quarter of `cuts`, rounded up.
fn share(cuts: u64) -> u64 {
    protocol_share(cuts, SHARE).unwrap()
}

/// A wallet holding `sol` lamports of bridged SOL and a holding of `mint`.
fn trader(w: &mut World, mint: &Pubkey, sol: u64) -> Keypair {
    let t = w.wallet_with_sol(sol);
    w.holdings(&t, *mint, &[t.pubkey()]);
    t
}

/// A swap of `amount` by `t` on the launch of `mint`, delivered to itself: it lands, and its
/// `Swapped` event is what the reference computed. Answers the reference and the transaction.
fn checked_swap(
    w: &mut World,
    mint: &Pubkey,
    t: &Keypair,
    buy: bool,
    amount: u64,
) -> (LaunchSwap, Tx) {
    let q = w.launch_quote(mint, &t.pubkey(), buy, amount);
    let lp = w.launch_lp_fee(mint, &t.pubkey(), &t.pubkey(), buy);
    let l = w.launch(mint);
    let ix = w.launch_swap_ix(&t.pubkey(), mint, u8::from(buy), amount, 0);
    let tx = w.env.send_paid_by(&[ix], t, &[]);
    tx.ok();
    expect_swapped(&tx.event::<Swapped>(), &q, &l, buy, lp);
    (q, tx)
}

/// The pool's books: the quote vault holds the reserve and the fees not yet collected, the base
/// vault the base reserve.
#[track_caller]
fn pool_is_sound(w: &World, mint: &Pubkey) {
    let p = w.launch_pool(mint);
    let pool = w.launch_pool_key(mint);
    assert_eq!(
        w.env.holding(&w.sol, &pool),
        p.quote_reserve + p.protocol_fees_quote,
        "the quote vault"
    );
    assert_eq!(w.env.holding(mint, &pool), p.base_reserve, "the base vault");
}

/// `CreateConfigArgs`.
fn config_args(
    rules: LaunchRules,
    creator_fee_bps: u16,
    custom_hook: Option<Pubkey>,
    custom_hook_flags: u16,
    label: &str,
) -> CreateConfigArgs {
    CreateConfigArgs {
        rules,
        creator_fee_bps,
        custom_hook,
        custom_hook_flags,
        label: label.to_string(),
    }
}

/// `tax_hook::prepare` for `mint` (which need not exist), the fee paid to `collector`'s holding.
fn tax_prepare(
    authority: Pubkey,
    mint: Pubkey,
    collector: Pubkey,
    fee_bps: u16,
    max_wallet_bps: u16,
) -> Instruction {
    let tax = Pubkey::find_program_address(&[tax_hook::TAX_SEED, mint.as_ref()], &tax_hook::ID).0;
    Instruction {
        program_id: tax_hook::ID,
        accounts: vec![
            AccountMeta::new(authority, true),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(collector, false),
            AccountMeta::new(tax, false),
            AccountMeta::new(hook_accounts_address(&tax_hook::ID, &mint).0, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data: tax_hook::instruction::Prepare {
            fee_bps,
            max_wallet_bps,
        }
        .data(),
    }
}

/// Prepares `hook_tester` for `mint`: its script and its registry (the script alone).
fn prepare_tester(w: &mut World, creator: &Keypair, mint: Pubkey) {
    w.env
        .send_paid_by(
            &[tester::init_script(creator.pubkey(), mint, vec![])],
            creator,
            &[],
        )
        .ok();
}

#[test]
fn bordrless_takes_a_quarter_of_what_each_preset_collects() {
    let mut w = World::new();
    for (i, p) in core_presets::ALL.iter().enumerate() {
        let r = presets::of(p);
        let creator = w.wallet_with_sol(20 * SOL);
        let (mint, tx) = w.create_launch_with(&creator, &format!("P{i}"), p.creator_fee_bps, VQ, r);
        tx.ok();
        assert_eq!(tx.event::<LaunchCreated>().rules, r);
        // Past the sniper window, the early window and the early-buyer unlock (Diamond hands
        // locks the first 5 minutes' buys for a day).
        w.env.warp(86_401);
        let (a, b) = (
            trader(&mut w, &mint, 5 * SOL),
            trader(&mut w, &mint, 5 * SOL),
        );
        // The first buy: nobody is eligible, so the creator fee alone is collected; Bordrless
        // takes a quarter of it and the LP fee, from the input, before the curve.
        let (q, tx) = checked_swap(&mut w, &mint, &a, true, SOL / 10);
        let ev: Swapped = tx.event();
        assert_eq!(q.holder_fee, 0);
        assert_eq!(ev.protocol_fee, share(q.creator_fee) + q.lp_fee);
        assert_eq!(ev.cuts_in, q.creator_fee);
        // The second buy: `a` holds above the threshold, so a buy-side holder fee is collected
        // too (not for Paid to hold, which pays holders on sells only).
        let (q2, tx2) = checked_swap(&mut w, &mint, &b, true, SOL / 10);
        assert_eq!(q2.holder_fee > 0, r.holder_fee_buy_bps > 0, "{}", p.name);
        assert_eq!(
            tx2.event::<Swapped>().protocol_fee,
            share(q2.creator_fee + q2.holder_fee) + q2.lp_fee
        );
        // A sell: the LP fee and the rules' fees come from the output, and Bordrless's share is
        // held back from the delivery.
        let held = w.env.holding(&mint, &a.pubkey());
        let (q3, tx3) = checked_swap(&mut w, &mint, &a, false, held / 2);
        let ev3: Swapped = tx3.event();
        assert_eq!(q3.holder_fee > 0, r.holder_fee_sell_bps > 0, "{}", p.name);
        assert_eq!(
            ev3.protocol_fee,
            share(q3.creator_fee + q3.holder_fee) + q3.lp_fee
        );
        assert_eq!(ev3.cuts_out, q3.creator_fee + q3.holder_fee);
        assert_eq!(
            ev3.delivered_out,
            ev3.amount_out - q3.creator_fee - q3.holder_fee - ev3.protocol_fee
        );
        pool_is_sound(&w, &mint);
        let total = ev.protocol_fee + tx2.event::<Swapped>().protocol_fee + ev3.protocol_fee;
        assert_eq!(w.launch_pool(&mint).protocol_fees_quote, total);
        println!(
            "{}: buy {} lamports of fees -> {} to Bordrless; buy {} -> {}; sell {} -> {}",
            p.name,
            q.creator_fee + q.holder_fee,
            ev.protocol_fee,
            q2.creator_fee + q2.holder_fee,
            tx2.event::<Swapped>().protocol_fee,
            q3.creator_fee + q3.holder_fee,
            ev3.protocol_fee
        );
    }

    // A Plain launch with no creator fee collects nothing, so Bordrless gets the LP fee alone.
    let creator = w.wallet_with_sol(20 * SOL);
    let (free, tx) = w.create_launch(&creator, "FREE", 0, VQ);
    tx.ok();
    w.env.warp(31);
    let t = trader(&mut w, &free, 5 * SOL);
    let (q, tx) = checked_swap(&mut w, &free, &t, true, SOL);
    let ev: Swapped = tx.event();
    assert_eq!((q.creator_fee, q.holder_fee), (0, 0));
    assert_eq!(
        ev.protocol_fee,
        fee_amount(SOL, policy::LP_FEE_BPS).unwrap()
    );
    assert!(ev.deltas_in.is_empty() && ev.cuts_in == 0 && ev.cuts_out == 0);
    let held = w.env.holding(&free, &t.pubkey());
    let (q_sell, tx) = checked_swap(&mut w, &free, &t, false, held / 2);
    let ev_sell: Swapped = tx.event();
    assert_eq!((ev_sell.cuts_in, ev_sell.cuts_out), (0, 0));
    assert_eq!(ev_sell.protocol_fee, q_sell.lp_fee);
    assert_eq!(ev_sell.delivered_out, ev_sell.amount_out - q_sell.lp_fee);
    assert_eq!(
        w.launch_pool(&free).protocol_fees_quote,
        ev.protocol_fee + ev_sell.protocol_fee
    );
    pool_is_sound(&w, &free);

    // A 1% creator fee: on a 1 SOL buy the creator gets 10,000,000 lamports and Bordrless
    // 2,500,000, a quarter of that and 0.25% of the trade, plus the 0.3% LP fee on the rest.
    let (one, tx) = w.create_launch(&creator, "ONE", 100, VQ);
    tx.ok();
    w.env.warp(31);
    let t = trader(&mut w, &one, 5 * SOL);
    let (q, tx) = checked_swap(&mut w, &one, &t, true, SOL);
    let ev: Swapped = tx.event();
    assert_eq!(q.creator_fee, 10_000_000);
    assert_eq!(share(q.creator_fee), 2_500_000);
    assert_eq!(share(q.creator_fee), SOL / 400);
    assert_eq!(
        q.lp_fee,
        fee_amount(SOL - 10_000_000, policy::LP_FEE_BPS).unwrap()
    );
    assert_eq!(ev.protocol_fee, 2_500_000 + q.lp_fee);
}

#[test]
fn a_token_launched_from_a_config() {
    let mut w = World::new();
    // Anyone makes a config: here someone other than the launch's creator, with the Burn preset.
    let maker = w.env.funded(5 * SOL);
    let burn = presets::burn();
    let args = config_args(burn, 50, None, 0, "Burn");
    let (config, tx) = w.create_config(&maker, args);
    tx.ok();
    let ev: LaunchConfigCreated = tx.event();
    assert_eq!(
        (
            ev.config,
            ev.creator,
            ev.rules,
            ev.creator_fee_bps,
            ev.custom_hook,
            ev.custom_hook_flags,
            ev.label.as_str()
        ),
        (config, maker.pubkey(), burn, 50, None, 0, "Burn")
    );
    let c = w.launch_config(&config);
    assert_eq!(
        (
            c.creator,
            c.rules,
            c.creator_fee_bps,
            c.custom_hook,
            c.custom_hook_flags
        ),
        (maker.pubkey(), burn, 50, None, 0)
    );
    assert_eq!((c.label.as_str(), c.created_at), ("Burn", w.env.now));
    assert_eq!(
        w.env.account(&config).unwrap().data.len(),
        LaunchConfig::LEN
    );
    println!("LaunchConfig {} bytes", LaunchConfig::LEN);

    // A launch from it: the rules, the fee and the kit come from the config.
    let creator = w.wallet_with_sol(20 * SOL);
    let (mint, tx) = w.create_launch_from_config(&creator, "CFG", VQ, &config);
    tx.ok();
    println!(
        "create_launch from a config (legacy-size v0, no table): CU {} size {} trace {}",
        tx.cu(),
        tx.size,
        tx.trace_len()
    );
    let ev: LaunchCreated = tx.event();
    assert_eq!(
        (
            ev.config,
            ev.custom_hook,
            ev.custom_hook_flags,
            ev.rules,
            ev.creator_fee_bps,
            ev.modules
        ),
        (Some(config), None, 0, burn, 50, 1)
    );
    let l = w.launch(&mint);
    assert_eq!(
        (
            l.config,
            l.custom_hook,
            l.custom_hook_flags,
            l.rules,
            l.creator_fee_bps,
            l.modules
        ),
        (config, None, 0, burn, 50, 1)
    );
    let m: Mint = w.env.read(&mint);
    assert_eq!((m.hook_program, m.hook_authority), (Some(KIT_ID), None));
    // Trades follow the config's rules: a burn on both sides, holder rewards, the creator fee,
    // and Bordrless's quarter of the two fees with the LP fee.
    w.env.warp(31);
    let t = trader(&mut w, &mint, 5 * SOL);
    let (q, tx) = checked_swap(&mut w, &mint, &t, true, SOL / 2);
    assert!(q.burn > 0 && q.creator_fee > 0);
    assert_eq!(
        tx.event::<Swapped>().protocol_fee,
        share(q.creator_fee) + q.lp_fee
    );
    // A config is reusable: a second launch from the same one.
    let (mint2, tx) = w.create_launch_from_config(&creator, "CFG2", VQ, &config);
    tx.ok();
    assert_eq!(w.launch(&mint2).config, config);

    // The arguments must repeat the config: another creator fee, or other rules, is refused.
    let try_args = |w: &mut World, fee: u16, rules: LaunchRules| -> Tx {
        let kp = Keypair::new();
        let ix = launch::create_launch_with(
            creator.pubkey(),
            kp.pubkey(),
            w.env.treasury.pubkey(),
            w.sol,
            policy::LP_FEE_BPS,
            World::launch_args("MIS", fee, VQ, rules),
            Some(config),
            None,
        );
        w.env.send_paid_by(&[ix], &creator, &[&kp])
    };
    refused(&try_args(&mut w, 100, burn), LaunchError::ConfigMismatch);
    refused(
        &try_args(&mut w, 50, presets::paid_to_hold()),
        LaunchError::ConfigMismatch,
    );
    // An account that is not a `LaunchConfig` in its place is refused by Anchor.
    {
        let kp = Keypair::new();
        let ix = launch::create_launch_with(
            creator.pubkey(),
            kp.pubkey(),
            w.env.treasury.pubkey(),
            w.sol,
            policy::LP_FEE_BPS,
            World::launch_args("NOT", 50, VQ, burn),
            Some(launch::config_address()),
            None,
        );
        w.env.send_paid_by(&[ix], &creator, &[&kp]).expect_fail();
    }

    // The bounds hold at creation...
    let over = |w: &mut World, args: CreateConfigArgs, e: LaunchError| {
        let (_, tx) = w.create_config(&maker, args);
        refused(&tx, e);
    };
    over(
        &mut w,
        config_args(rules(201, 0, 0, 0, 0, 0, 0, 0), 0, None, 0, "x"),
        LaunchError::HolderFeeTooHigh,
    );
    over(
        &mut w,
        config_args(rules(0, 0, 0, 101, 0, 0, 0, 0), 0, None, 0, "x"),
        LaunchError::BurnTooHigh,
    );
    over(
        &mut w,
        config_args(rules(200, 0, 100, 0, 0, 0, 0, 0), 1, None, 0, "x"),
        LaunchError::RulesFeeTooHigh,
    );
    over(
        &mut w,
        config_args(LaunchRules::NONE, 201, None, 0, "x"),
        LaunchError::CreatorFeeTooHigh,
    );
    over(
        &mut w,
        config_args(LaunchRules::NONE, 100, None, 0, &"l".repeat(33)),
        LaunchError::InvalidLabel,
    );
    let (_, tx) = w.create_config(
        &maker,
        config_args(LaunchRules::NONE, 100, None, 0, &"l".repeat(32)),
    );
    tx.ok();
    // ... and again at every launch: once the admin lowers the fee bound below the Burn
    // config's 1.5% a side, launching from it is refused.
    let deployer = w.env.deployer.insecure_clone();
    let mut tight = policy_launch_config(deployer.pubkey(), w.env.treasury.pubkey(), w.sol);
    tight.rule_bounds.max_rules_fee_bps = 100;
    w.env
        .send_paid_by(
            &[launch::set_config(deployer.pubkey(), tight)],
            &deployer,
            &[],
        )
        .ok();
    let (_, tx) = w.create_launch_from_config(&creator, "TGT", VQ, &config);
    refused(&tx, LaunchError::RulesFeeTooHigh);
    let back = policy_launch_config(deployer.pubkey(), w.env.treasury.pubkey(), w.sol);
    w.env
        .send_paid_by(
            &[launch::set_config(deployer.pubkey(), back)],
            &deployer,
            &[],
        )
        .ok();
    let (_, tx) = w.create_launch_from_config(&creator, "OK", VQ, &config);
    tx.ok();
}

#[test]
fn a_custom_hook_launch_runs_the_creators_hook_everywhere() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(20 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    // The hook is prepared for the mint before the launch: hook_tester's script and registry.
    prepare_tester(&mut w, &creator, mint);
    let flags = token_flags::BEFORE_TRANSFER | token_flags::BEFORE_MINT | token_flags::BEFORE_BURN;
    // A config naming it, with a burn and a creator fee (pool hook rules) and no kit rule.
    let r = rules(0, 0, 50, 50, 0, 0, 0, 0);
    let (config, tx) = w.create_config(
        &creator,
        config_args(r, 100, Some(hook_tester::ID), flags, "Own hook"),
    );
    tx.ok();
    let c = w.launch_config(&config);
    assert_eq!(c.hook(), Some((hook_tester::ID, flags)));

    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint, "OWN", VQ, &config);
    let tx = w.env.send_paid_by(&[ix], &creator, &[&mint_kp]);
    tx.ok();
    println!(
        "create_launch with a custom hook (legacy-size v0, no table): CU {} size {} trace {} \
         height {}",
        tx.cu(),
        tx.size,
        tx.trace_len(),
        tx.max_height()
    );
    // The mint: the creator's program as its hook from the first instruction, nobody to change
    // it.
    let m: Mint = w.env.read(&mint);
    assert_eq!(
        (
            m.hook_program,
            m.hook_authority,
            m.hook_flags,
            m.mint_authority
        ),
        (Some(hook_tester::ID), None, flags, None)
    );
    let l = w.launch(&mint);
    assert_eq!(
        (l.custom_hook, l.custom_hook_flags, l.modules, l.config),
        (Some(hook_tester::ID), flags, 0, config)
    );
    assert_eq!(l.token_hook(), Some(hook_tester::ID));
    assert!(l.has_custom_hook() && !l.has_kit());
    let ev: LaunchCreated = tx.event();
    assert_eq!(
        (
            ev.custom_hook,
            ev.custom_hook_flags,
            ev.config,
            ev.kit_config
        ),
        (Some(hook_tester::ID), flags, Some(config), None)
    );
    assert!(w.env.account(&launch::kit_config_address(&mint)).is_none());
    // The hook ran on the supply mint and on the pool deposit, with its slice passed by the
    // launch.
    let s: Script = w.env.read(&tester::script_address(&mint));
    let minted = s.told_token(callback::BEFORE_MINT).unwrap();
    assert_eq!(
        (minted.amount, minted.destination_owner),
        (policy::TOKEN_SUPPLY, launch::launch_address(&mint))
    );
    let deposit = s.told_token(callback::BEFORE_TRANSFER).unwrap();
    assert_eq!(
        (
            deposit.source_owner,
            deposit.destination_owner,
            deposit.amount
        ),
        (launch::launch_address(&mint), l.pool, l.curve_tokens)
    );
    assert_eq!(s.calls, 2);

    // Trades: the pool hook's rules apply (the creator fee, the burn), the DEX takes its share
    // of the fee, and the creator's hook sees every transfer and burn of the token.
    w.env.warp(31);
    let t = trader(&mut w, &mint, 10 * SOL);
    let (q, tx) = checked_swap(&mut w, &mint, &t, true, SOL / 2);
    assert!(q.burn > 0 && q.creator_fee == fee_amount(SOL / 2, 100).unwrap());
    assert_eq!(
        tx.event::<Swapped>().protocol_fee,
        share(q.creator_fee) + q.lp_fee
    );
    let s: Script = w.env.read(&tester::script_address(&mint));
    // The buy: the burn from the output, then the delivery.
    assert_eq!(s.calls, 4);
    let burn = s.told_token(callback::BEFORE_BURN).unwrap();
    assert_eq!((burn.amount, burn.source_owner), (q.burn, l.pool));
    let delivery = s.told_token(callback::BEFORE_TRANSFER).unwrap();
    assert_eq!(
        (
            delivery.source_owner,
            delivery.destination_owner,
            delivery.amount
        ),
        (l.pool, t.pubkey(), q.delivered.unwrap())
    );
    let held = w.env.holding(&mint, &t.pubkey());
    let (q2, tx2) = checked_swap(&mut w, &mint, &t, false, held / 2);
    assert!(q2.burn > 0);
    assert_eq!(
        tx2.event::<Swapped>().protocol_fee,
        share(q2.creator_fee) + q2.lp_fee
    );
    // The sell: the burn from the input, then the transfer in.
    let s: Script = w.env.read(&tester::script_address(&mint));
    assert_eq!(s.calls, 6);
    let sold = s.told_token(callback::BEFORE_TRANSFER).unwrap();
    assert_eq!(
        (sold.source_owner, sold.destination_owner, sold.amount),
        (t.pubkey(), l.pool, held / 2 - q2.burn)
    );
    pool_is_sound(&w, &mint);
    // A wallet-to-wallet transfer: the hook runs, the full amount moves.
    let friend = w.env.funded(SOL);
    w.holdings(&creator, mint, &[friend.pubkey()]);
    let rest = w.env.holding(&mint, &t.pubkey());
    w.send_tokens(&t, mint, &friend.pubkey(), rest / 3).ok();
    assert_eq!(w.env.holding(&mint, &friend.pubkey()), rest / 3);
    assert_eq!(
        w.env.read::<Script>(&tester::script_address(&mint)).calls,
        7
    );

    // Graduation needs the hook's slice (the reserve's top-up and burn run the hook); without
    // it the launch refuses before anything moves; with it the hook sees the burn.
    w.fill_curve(&mint, SOL / 10);
    let last = trader(&mut w, &mint, 10 * SOL);
    let amount = w.crossing_buy_amount(&mint, &last.pubkey());
    checked_swap(&mut w, &mint, &last, true, amount);
    let cranker = w.env.funded(SOL);
    let keys = w.launch_keys(&mint);
    let bare = launch::graduate(
        cranker.pubkey(),
        keys.mint,
        keys.quote_mint,
        keys.lp_fee_bps,
        keys.modules,
    );
    refused(
        &w.env.send_paid_by(&[bare], &cranker, &[]),
        LaunchError::CustomHookAccountsMissing,
    );
    let mut wrong = w.graduate_ix(&cranker.pubkey(), &mint);
    let n = wrong.accounts.len();
    wrong.accounts[n - 3] = AccountMeta::new_readonly(tax_hook::ID, false);
    refused(
        &w.env.send_paid_by(&[wrong], &cranker, &[]),
        LaunchError::WrongProgram,
    );
    let calls = w.env.read::<Script>(&tester::script_address(&mint)).calls;
    let reserve = w.env.holding(&mint, &launch::launch_address(&mint));
    let tx = w
        .env
        .send_paid_by(&[w.graduate_ix(&cranker.pubkey(), &mint)], &cranker, &[]);
    tx.ok();
    println!(
        "graduate with a custom hook: CU {} size {} trace {}",
        tx.cu(),
        tx.size,
        tx.trace_len()
    );
    assert_eq!(w.launch(&mint).status, STATUS_GRADUATED);
    let s: Script = w.env.read(&tester::script_address(&mint));
    assert_eq!(s.calls, calls + 2, "the top-up and the burn ran the hook");
    let burned = s.told_token(callback::BEFORE_BURN).unwrap();
    assert_eq!(burned.source_owner, launch::launch_address(&mint));
    assert_eq!(
        burned.amount + s.told_token(callback::BEFORE_TRANSFER).unwrap().amount,
        reserve
    );
    pool_is_sound(&w, &mint);
    // Trading goes on after graduation, the hook still running.
    checked_swap(&mut w, &mint, &last, true, SOL / 10);
}

#[test]
fn tax_hook_prepared_for_a_launch_mint_takes_its_cut_and_bordrless_a_quarter_of_it() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(20 * SOL);
    let collector = w.env.funded(SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    // The hook, prepared for the mint before it exists: 1% of every transfer to the collector.
    w.env
        .send_paid_by(
            &[tax_prepare(
                creator.pubkey(),
                mint,
                collector.pubkey(),
                100,
                0,
            )],
            &creator,
            &[],
        )
        .ok();
    let tax_key =
        Pubkey::find_program_address(&[tax_hook::TAX_SEED, mint.as_ref()], &tax_hook::ID).0;
    let tax: tax_hook::TaxConfig = w.env.read(&tax_key);
    assert_eq!(
        (
            tax.mint,
            tax.collector_owner,
            tax.collector_holding,
            tax.fee_bps
        ),
        (
            mint,
            collector.pubkey(),
            token::holding_address(&mint, &collector.pubkey()),
            100
        )
    );
    // A config naming it, Plain rules, creator fee 1%.
    let (config, tx) = w.create_config(
        &creator,
        config_args(
            LaunchRules::NONE,
            100,
            Some(tax_hook::ID),
            tax_hook::FLAGS,
            "Taxed",
        ),
    );
    tx.ok();
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint, "TAX", VQ, &config);
    let tx = w.env.send_paid_by(&[ix], &creator, &[&mint_kp]);
    tx.ok();
    let l = w.launch(&mint);
    assert_eq!(l.custom_hook, Some(tax_hook::ID));
    // The deposit paid no tax: the collector's holding did not exist yet.
    assert_eq!(w.env.read::<tax_hook::TaxConfig>(&tax_key).collected, 0);
    assert_eq!(w.env.holding(&mint, &l.pool), l.curve_tokens);
    // Now it does; trades pay the tax, and Bordrless takes a quarter of it as of the creator fee.
    w.holdings(&creator, mint, &[collector.pubkey()]);
    w.env.warp(31);
    let t = trader(&mut w, &mint, 10 * SOL);
    let quote_holding = token::holding_address(&w.sol, &launch::launch_address(&mint));

    // A buy: the creator fee from the input (its share before the curve); the delivery pays the
    // tax, a base-side cut valued at the swap's price, whose share is set aside from the quote
    // reserve after the swap.
    let p0 = w.launch_pool(&mint);
    let ix = w.launch_swap_ix(&t.pubkey(), &mint, 1, SOL, 0);
    let tx = w.env.send_paid_by(&[ix], &t, &[]);
    tx.ok();
    let ev: Swapped = tx.event();
    let creator_fee = fee_amount(SOL, 100).unwrap();
    let received = SOL - creator_fee;
    assert_eq!((ev.cuts_in, ev.received_in), (creator_fee, received));
    let p_in = share(creator_fee);
    let lp_fee = fee_amount(received, policy::LP_FEE_BPS).unwrap();
    let net_in = received - lp_fee - p_in;
    let out = swap_out(
        net_in,
        p0.quote_reserve,
        p0.virtual_quote,
        p0.base_reserve,
        p0.virtual_base,
    )
    .unwrap();
    assert_eq!((ev.lp_fee, ev.amount_out), (0, out));
    let tax_out = fee_amount(out, 100).unwrap();
    assert_eq!((ev.cuts_out, ev.delivered_out), (tax_out, out - tax_out));
    assert_eq!(w.env.holding(&mint, &collector.pubkey()), tax_out);
    assert_eq!(w.env.holding(&mint, &t.pubkey()), out - tax_out);
    let p_out = share(quote_value(tax_out, net_in, out).unwrap());
    assert!(p_out > 0);
    assert_eq!(ev.protocol_fee, p_in + lp_fee + p_out);
    let p1 = w.launch_pool(&mint);
    assert_eq!(
        p1.protocol_fees_quote,
        p0.protocol_fees_quote + p_in + lp_fee + p_out
    );
    assert_eq!(
        p1.quote_reserve,
        p0.quote_reserve + received - p_in - lp_fee - p_out
    );
    assert_eq!(p1.base_reserve, p0.base_reserve - out);
    pool_is_sound(&w, &mint);
    println!(
        "taxed buy of 1 SOL: creator fee {creator_fee}, tax {tax_out} tokens worth {} lamports, \
         Bordrless {p_in} + LP fee {lp_fee} + {p_out}",
        quote_value(tax_out, net_in, out).unwrap()
    );

    // A sell: the transfer in pays the tax (a base-side cut, valued, its share off the output
    // before the hook is told); the creator fee from what the hook is told; its share held back
    // from the delivery.
    let amount = w.env.holding(&mint, &t.pubkey()) / 2;
    let p0 = w.launch_pool(&mint);
    let collector_before = w.env.holding(&mint, &collector.pubkey());
    let sol_before = w.env.holding(&w.sol, &t.pubkey());
    let ix = w.launch_swap_ix(&t.pubkey(), &mint, 0, amount, 0);
    let tx = w.env.send_paid_by(&[ix], &t, &[]);
    tx.ok();
    let ev: Swapped = tx.event();
    let tax_in = fee_amount(amount, 100).unwrap();
    let received = amount - tax_in;
    assert_eq!(
        (ev.cuts_in, ev.received_in, ev.burn_in),
        (tax_in, received, 0)
    );
    let net_in = received;
    let out = swap_out(
        net_in,
        p0.base_reserve,
        p0.virtual_base,
        p0.quote_reserve,
        p0.virtual_quote,
    )
    .unwrap();
    assert_eq!(ev.amount_out, out);
    let p_in = share(quote_value(tax_in, out, net_in).unwrap());
    let lp_fee = fee_amount(out, policy::LP_FEE_BPS).unwrap();
    let told = out - p_in - lp_fee;
    let creator_fee = fee_amount(told, 100).unwrap();
    let p_out = share(creator_fee);
    assert_eq!(
        ev.deltas_out,
        vec![DeltaPaid {
            holding: quote_holding,
            amount: creator_fee
        }]
    );
    assert_eq!(ev.cuts_out, creator_fee);
    assert_eq!(ev.protocol_fee, p_in + lp_fee + p_out);
    assert_eq!(ev.delivered_out, told - creator_fee - p_out);
    assert_eq!(
        w.env.holding(&w.sol, &t.pubkey()),
        sol_before + ev.delivered_out
    );
    assert_eq!(
        w.env.holding(&mint, &collector.pubkey()),
        collector_before + tax_in
    );
    let p1 = w.launch_pool(&mint);
    assert_eq!(
        p1.protocol_fees_quote,
        p0.protocol_fees_quote + p_in + lp_fee + p_out
    );
    assert_eq!(p1.quote_reserve, p0.quote_reserve - out);
    assert_eq!(p1.base_reserve, p0.base_reserve + received);
    pool_is_sound(&w, &mint);
    assert_eq!(
        w.env.read::<tax_hook::TaxConfig>(&tax_key).collected,
        tax_out + tax_in
    );
    // The launch's own counters saw the creator fees only.
    assert_eq!(
        w.launch(&mint).creator_fees_accrued,
        fee_amount(SOL, 100).unwrap() + creator_fee
    );
    assert_eq!(
        w.env.holding(&w.sol, &launch::launch_address(&mint)),
        w.launch(&mint).creator_fees_accrued
    );
}

#[test]
fn every_refusal_of_a_custom_hook_config_and_launch() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(50 * SOL);
    let none = LaunchRules::NONE;
    let flags = token_flags::BEFORE_TRANSFER;
    let hook = hook_tester::ID;

    // Making the config.
    let config_refused = |w: &mut World, args: CreateConfigArgs, e: LaunchError| {
        let (_, tx) = w.create_config(&creator, args);
        refused(&tx, e);
    };
    // A kit rule with a custom hook: one token hook per mint.
    for r in [
        rules(100, 100, 0, 0, 0, 0, 0, 0),
        rules(0, 0, 0, 0, 200, 0, 0, 0),
        rules(0, 0, 0, 0, 0, 86_400, 0, 0),
        rules(0, 0, 0, 0, 0, 0, 60, 3_600),
    ] {
        config_refused(
            &mut w,
            config_args(r, 50, Some(hook), flags, "kit"),
            LaunchError::CustomHookWithKitRules,
        );
    }
    // The protocol's programs, the system program and the default key are not custom hooks.
    for program in [
        KIT_ID,
        bordrless_token::ID,
        bordrless_swap::ID,
        bordrless_bridge::ID,
        bordrless_launch::ID,
        SYSTEM_PROGRAM_ID,
        Pubkey::default(),
    ] {
        config_refused(
            &mut w,
            config_args(none, 50, Some(program), flags, "proto"),
            LaunchError::InvalidCustomHook,
        );
    }
    // Not a program.
    let wallet = w.env.funded(SOL).pubkey();
    config_refused(
        &mut w,
        config_args(none, 50, Some(wallet), flags, "wallet"),
        LaunchError::InvalidCustomHook,
    );
    // Flags: at least one callback, no unknown bit; none without a hook.
    config_refused(
        &mut w,
        config_args(none, 50, Some(hook), 0, "flags"),
        LaunchError::InvalidCustomHookFlags,
    );
    config_refused(
        &mut w,
        config_args(none, 50, Some(hook), 1 << 9, "flags"),
        LaunchError::InvalidCustomHookFlags,
    );
    config_refused(
        &mut w,
        config_args(none, 50, None, flags, "flags"),
        LaunchError::InvalidCustomHookFlags,
    );
    // The hook program account: present exactly with a hook, and the hook's.
    let config_with = |w: &mut World, args: CreateConfigArgs, account: AccountMeta| -> Tx {
        let kp = Keypair::new();
        let mut ix = launch::create_config(creator.pubkey(), kp.pubkey(), args);
        ix.accounts[3] = account;
        w.env.send_paid_by(&[ix], &creator, &[&kp])
    };
    refused(
        &config_with(
            &mut w,
            config_args(none, 50, Some(hook), flags, "absent"),
            AccountMeta::new_readonly(bordrless_launch::ID, false),
        ),
        LaunchError::CustomHookAccountsMissing,
    );
    refused(
        &config_with(
            &mut w,
            config_args(none, 50, Some(hook), flags, "other"),
            AccountMeta::new_readonly(tax_hook::ID, false),
        ),
        LaunchError::InvalidCustomHook,
    );
    refused(
        &config_with(
            &mut w,
            config_args(none, 50, None, 0, "stray"),
            AccountMeta::new_readonly(hook, false),
        ),
        LaunchError::UnexpectedCustomHookAccounts,
    );
    // A sound config, with a burn (a pool hook rule): accepted.
    let (config, tx) = w.create_config(
        &creator,
        config_args(rules(0, 0, 50, 50, 0, 0, 0, 0), 50, Some(hook), flags, "ok"),
    );
    tx.ok();

    // Launching from it.
    let launch_with = |w: &mut World,
                       mint: &Keypair,
                       launch_config: Option<Pubkey>,
                       custom: Option<&launch::CustomHookAccounts>,
                       fee: u16,
                       r: LaunchRules|
     -> Instruction {
        launch::create_launch_with(
            creator.pubkey(),
            mint.pubkey(),
            w.env.treasury.pubkey(),
            w.sol,
            policy::LP_FEE_BPS,
            World::launch_args("REF", fee, VQ, r),
            launch_config,
            custom,
        )
    };
    let r = rules(0, 0, 50, 50, 0, 0, 0, 0);
    let bare = launch::CustomHookAccounts {
        program: hook,
        extras: vec![],
    };
    // No registry for the mint (the hook was not prepared): refused before anything is made.
    let mint = Keypair::new();
    let treasury_before = w.env.lamports(&w.env.treasury.pubkey());
    let ix = launch_with(&mut w, &mint, Some(config), Some(&bare), 50, r);
    refused(
        &w.env.send_paid_by(&[ix], &creator, &[&mint]),
        LaunchError::HookRegistryMissing,
    );
    assert!(w.env.account(&mint.pubkey()).is_none());
    assert!(w
        .env
        .account(&launch::launch_address(&mint.pubkey()))
        .is_none());
    assert!(w.env.account(&w.launch_pool_key(&mint.pubkey())).is_none());
    assert_eq!(w.env.lamports(&w.env.treasury.pubkey()), treasury_before);
    // Prepared: the extras must be the registry's (the script), no more, no fewer.
    prepare_tester(&mut w, &creator, mint.pubkey());
    let ix = launch_with(&mut w, &mint, Some(config), Some(&bare), 50, r);
    refused(
        &w.env.send_paid_by(&[ix], &creator, &[&mint]),
        LaunchError::HookExtrasMismatch,
    );
    let resolved = w.custom_hook_accounts(&hook, &mint.pubkey());
    assert_eq!(resolved.extras.len(), 1);
    let mut too_many = resolved.clone();
    too_many
        .extras
        .push(AccountMeta::new_readonly(Pubkey::new_unique(), false));
    let ix = launch_with(&mut w, &mint, Some(config), Some(&too_many), 50, r);
    refused(
        &w.env.send_paid_by(&[ix], &creator, &[&mint]),
        LaunchError::HookExtrasMismatch,
    );
    // The hook's accounts missing altogether.
    let ix = launch_with(&mut w, &mint, Some(config), None, 50, r);
    refused(
        &w.env.send_paid_by(&[ix], &creator, &[&mint]),
        LaunchError::CustomHookAccountsMissing,
    );
    // The wrong program, the wrong signer, another mint's registry.
    let good = launch_with(&mut w, &mint, Some(config), Some(&resolved), 50, r);
    let n = good.accounts.len();
    let with = |i: usize, meta: AccountMeta| {
        let mut ix = good.clone();
        ix.accounts[i] = meta;
        ix
    };
    refused(
        &w.env.send_paid_by(
            &[with(n - 4, AccountMeta::new_readonly(tax_hook::ID, false))],
            &creator,
            &[&mint],
        ),
        LaunchError::InvalidCustomHook,
    );
    refused(
        &w.env.send_paid_by(
            &[with(
                n - 3,
                AccountMeta::new_readonly(token::hook_signer(&tax_hook::ID), false),
            )],
            &creator,
            &[&mint],
        ),
        LaunchError::WrongHookSigner,
    );
    let other = Keypair::new();
    prepare_tester(&mut w, &creator, other.pubkey());
    refused(
        &w.env.send_paid_by(
            &[with(
                n - 2,
                AccountMeta::new_readonly(tester::registry_address(&other.pubkey()), false),
            )],
            &creator,
            &[&mint],
        ),
        LaunchError::HookRegistryMissing,
    );
    // An inline launch (no config) with a hook's accounts.
    let plain = Keypair::new();
    let ix = launch_with(&mut w, &plain, None, Some(&resolved), 50, r);
    refused(
        &w.env.send_paid_by(&[ix], &creator, &[&plain]),
        LaunchError::UnexpectedCustomHookAccounts,
    );
    // Arguments that do not repeat the config.
    let ix = launch_with(&mut w, &mint, Some(config), Some(&resolved), 100, r);
    refused(
        &w.env.send_paid_by(&[ix], &creator, &[&mint]),
        LaunchError::ConfigMismatch,
    );
    // As built, it lands, and nothing of the kit exists for it.
    w.env.send_paid_by(&[good], &creator, &[&mint]).ok();
    let l = w.launch(&mint.pubkey());
    assert_eq!((l.custom_hook, l.modules), (Some(hook), 0));
    assert!(w
        .env
        .account(&launch::kit_config_address(&mint.pubkey()))
        .is_none());
}

#[test]
fn a_honeypot_hook_lets_buys_through_refuses_sells_and_leaves_the_pool_sound() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(20 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let pool = w.launch_pool_key(&mint);
    let launch_key = launch::launch_address(&mint);
    // The creator's hook refuses every transfer into the pool except the launch's own deposit:
    // a honeypot. The launchpad does not vet hook code; it labels the token.
    prepare_tester(&mut w, &creator, mint);
    w.env
        .send_paid_by(
            &[tester::honeypot(creator.pubkey(), mint, pool, launch_key)],
            &creator,
            &[],
        )
        .ok();
    let (config, tx) = w.create_config(
        &creator,
        config_args(
            LaunchRules::NONE,
            100,
            Some(hook_tester::ID),
            token_flags::BEFORE_TRANSFER,
            "Honeypot",
        ),
    );
    tx.ok();
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint, "POT", VQ, &config);
    w.env.send_paid_by(&[ix], &creator, &[&mint_kp]).ok();
    assert_eq!(w.launch(&mint).custom_hook, Some(hook_tester::ID));
    w.env.warp(31);

    // Buys land (the pool is the source) and pay the creator fee and Bordrless's share.
    let (buyer, other) = (
        trader(&mut w, &mint, 5 * SOL),
        trader(&mut w, &mint, 5 * SOL),
    );
    let (q, tx) = checked_swap(&mut w, &mint, &buyer, true, SOL);
    assert_eq!(
        tx.event::<Swapped>().protocol_fee,
        share(q.creator_fee) + q.lp_fee
    );
    let held = w.env.holding(&mint, &buyer.pubkey());
    assert!(held > 0);
    // A sell is refused by the creator's hook: nothing moves.
    let before = w.launch_pool(&mint);
    let sol_before = w.env.holding(&w.sol, &buyer.pubkey());
    let ix = w.launch_swap_ix(&buyer.pubkey(), &mint, 0, held / 2, 0);
    let tx = w.env.send_paid_by(&[ix], &buyer, &[]);
    tx.expect_code(u32::from(TesterError::Refused));
    assert!(tx.logs().iter().any(|l| l.contains("Error Code: Refused")));
    assert_eq!(w.env.holding(&mint, &buyer.pubkey()), held);
    assert_eq!(w.env.holding(&w.sol, &buyer.pubkey()), sol_before);
    let after = w.launch_pool(&mint);
    assert_eq!(
        (
            after.base_reserve,
            after.quote_reserve,
            after.protocol_fees_quote,
            after.swap_count
        ),
        (
            before.base_reserve,
            before.quote_reserve,
            before.protocol_fees_quote,
            before.swap_count
        )
    );
    pool_is_sound(&w, &mint);
    // A wallet-to-wallet transfer is not into the pool: it moves.
    w.send_tokens(&buyer, mint, &other.pubkey(), held / 4).ok();
    assert_eq!(w.env.holding(&mint, &other.pubkey()), held / 4);
    // The pool keeps working for buyers, and the admin still collects Bordrless's share.
    checked_swap(&mut w, &mint, &other, true, SOL / 2);
    pool_is_sound(&w, &mint);
    let deployer = w.env.deployer.insecure_clone();
    let admin = deployer.pubkey();
    w.holdings(&deployer, w.sol, &[admin]);
    let fees = w.launch_pool(&mint).protocol_fees_quote;
    assert!(fees > 0);
    w.env
        .send_paid_by(&[w.env.collect_ix(&admin, &pool, &admin)], &deployer, &[])
        .ok();
    assert_eq!(w.env.holding(&w.sol, &admin), fees);
    assert_eq!(w.launch_pool(&mint).protocol_fees_quote, 0);
    pool_is_sound(&w, &mint);
    println!(
        "honeypot: {} buys landed, every sell refused by the creator's hook",
        2
    );
}

/// A path's ceilings: compute units, instruction trace, CPI height, v0 bytes.
struct Ceiling {
    cu: u64,
    trace: usize,
    height: u8,
    bytes: usize,
}

/// Prints a path's figures and holds them under `c` (and under the runtime's own limits).
#[track_caller]
fn measure(name: &str, tx: &Tx, c: Ceiling) {
    tx.ok();
    println!(
        "| {name} | {} keys | {} v0 bytes | {} inner + {} top-level = {} trace | height {} | {} CU |",
        tx.keys.len(),
        tx.size,
        tx.inner_len(),
        tx.trace_len() - tx.inner_len(),
        tx.trace_len(),
        tx.max_height(),
        tx.cu()
    );
    assert!(tx.size <= c.bytes.min(1_232), "{name}: {} bytes", tx.size);
    assert!(
        tx.trace_len() <= c.trace.min(64),
        "{name}: trace {}",
        tx.trace_len()
    );
    assert!(
        tx.max_height() <= c.height.min(5),
        "{name}: height {}",
        tx.max_height()
    );
    assert!(tx.cu() <= c.cu, "{name}: {} CU", tx.cu());
}

#[test]
fn create_launch_budgets_with_a_config_and_with_a_custom_hook() {
    let mut w = World::new();
    let table: AddressLookupTableAccount = w
        .env
        .put_lookup_table(Pubkey::new_unique(), &protocol_lookup_table(&w));
    let tables = [table];
    let budget = [compute_unit_limit(1_400_000), compute_unit_price(20_000)];
    let creator = w.wallet_with_sol(20 * SOL);
    println!("| Path | Keys | v0 bytes | Trace | CPI height | Compute |");

    // From a config with every kit module (Diamond hands): the kit launch plus the config read.
    let p = core_presets::DIAMOND_HANDS;
    let (config, tx) = w.create_config(
        &creator,
        config_args(presets::of(&p), p.creator_fee_bps, None, 0, p.name),
    );
    tx.ok();
    let mint = Keypair::new();
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint.pubkey(), "DIAM", VQ, &config);
    let mut ixs = budget.to_vec();
    ixs.push(ix);
    let tx = w.env.send_v0(&ixs, &creator, &[&mint], &tables);
    measure(
        "create_launch from a config, every module",
        &tx,
        Ceiling {
            cu: 420_000,
            trace: 46,
            height: 4,
            bytes: 1_150,
        },
    );
    assert_eq!(w.launch(&mint.pubkey()).config, config);
    assert_eq!(w.env.kit_config(&mint.pubkey()).modules, 15);

    // With a custom hook that runs on the mint and the deposit (hook_tester): no kit, two hook
    // callbacks instead.
    let mint = Keypair::new();
    prepare_tester(&mut w, &creator, mint.pubkey());
    let (config, tx) = w.create_config(
        &creator,
        config_args(
            rules(0, 0, 50, 50, 0, 0, 0, 0),
            100,
            Some(hook_tester::ID),
            token_flags::BEFORE_TRANSFER | token_flags::BEFORE_MINT,
            "Own hook",
        ),
    );
    tx.ok();
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint.pubkey(), "HOOK", VQ, &config);
    let mut ixs = budget.to_vec();
    ixs.push(ix);
    let tx = w.env.send_v0(&ixs, &creator, &[&mint], &tables);
    measure(
        "create_launch with a custom hook",
        &tx,
        Ceiling {
            cu: 360_000,
            trace: 40,
            height: 4,
            bytes: 1_150,
        },
    );
    assert_eq!(w.launch(&mint.pubkey()).custom_hook, Some(hook_tester::ID));
}
