//! Independent audit of `hook_vault` (phase 3a, round 1): proofs of concept.
//!
//! Each test is named after the finding it demonstrates (see
//! `bordrless-games-work/log-3a-audit-vault-r1.md`). The round-1 fixes flipped every finding's
//! test: each now asserts the fixed behaviour (the name says what holds now), with the same
//! scenario as the proof of concept; the "checked, fine" tests assert the property holds.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::InstructionData;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{compute_unit_limit, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::{protocol_lookup_table, SOL, VQ};
use bordrless_program_tests::vault::*;
use bordrless_token::client as token;
use hook_vault::client as vault_client;
use hook_vault::constants::*;
use hook_vault::events::*;
use hook_vault::instructions::{CreateVaultArgs, SlotArgs};
use solana_keypair::Keypair;
use solana_signer::Signer;

const PACKET: usize = 1_232;

fn wealth(w: &World, who: &Pubkey) -> i128 {
    w.env.holding(&w.sol, who) as i128 + w.env.lamports(who) as i128
}

/// Coin bought by a fresh wallet, half of it sent to slot `i`'s holding.
fn fund_slot(w: &mut World, mint: &Pubkey, i: u8, lamports: u64) -> u64 {
    let h = w.wallet_with_sol(lamports + SOL);
    w.buy(&h, mint, lamports).ok();
    let got = w.env.holding(mint, &h.pubkey()) / 2;
    w.send_tokens(&h, *mint, &slot_owner(mint, i), got).ok();
    got
}

fn rules(burn: u16) -> LaunchRules {
    LaunchRules {
        burn_buy_bps: burn,
        burn_sell_bps: burn,
        ..LaunchRules::NONE
    }
}

fn price_of(w: &World, pool: &Pubkey) -> u128 {
    let p: bordrless_swap::state::Pool = w.env.read(pool);
    hook_vault::state::spot_price(
        p.quote_reserve,
        p.virtual_quote,
        p.base_reserve,
        p.virtual_base,
    )
    .unwrap()
}

/// Sends `ixs` in ONE v0 transaction through a lookup table holding every non-signer,
/// non-program key they use (what an attacker builds for an atomic bundle).
fn atomic(w: &mut World, payer: &Keypair, ixs: &[Instruction]) -> Tx {
    let mut keys: Vec<Pubkey> = Vec::new();
    let programs: Vec<Pubkey> = ixs.iter().map(|i| i.program_id).collect();
    for ix in ixs {
        for m in &ix.accounts {
            if m.pubkey != payer.pubkey()
                && !programs.contains(&m.pubkey)
                && !keys.contains(&m.pubkey)
            {
                keys.push(m.pubkey);
            }
        }
    }
    assert!(keys.len() <= 256);
    let lut = w.env.put_lookup_table(Pubkey::new_unique(), &keys);
    let mut all = vec![compute_unit_limit(1_400_000)];
    all.extend_from_slice(ixs);
    w.env.send_v0(&all, payer, &[], std::slice::from_ref(&lut))
}

// =====================================================================================================
// F1. Several selling slots of one vault stacked into one sandwich (the slice was per slot). Fixed:
// each of the `n` selling slots sells at most `1/n` of the vault's slice a sale (round 2, V1: split,
// not first come), so slots cranked together sell one slice, and the sandwich loses as a single
// slot's does.
// =====================================================================================================

struct Sandwich {
    /// How many of the slots sold.
    sales: usize,
    sold: u64,
    bounties: u64,
    /// Attacker's SOL + bridged SOL change, plus its coin change valued at the final price.
    profit: i128,
    /// Creator fees that accrued to the launch during the sandwich (the creator's, claimable).
    creator_fees: i128,
    /// Of those, the creator fees of the vault's own sales (earned by an honest crank too).
    vault_sale_fees: i128,
}

/// A coin with `n` SellForSol slots (each to its own funded wallet), tax 0, `creator_fee`, burn
/// rules `burn`. The attacker (the creator when `creator`) holds coin, then in one bundle dumps
/// `front` lamports' worth, cranks every slot, and buys back with all the dump brought.
fn sale_sandwich_n(n: usize, creator_fee: u16, burn: u16, front: u64, creator: bool) -> Sandwich {
    let mut w = World::new();
    let slots: Vec<SlotArgs> = (0..n)
        .map(|_| sell_for_sol(w.env.funded(SOL).pubkey()))
        .collect();
    let c = w.vault_coin_spec(
        vault_args(slots),
        &[],
        &CoinSpec {
            creator_fee_bps: creator_fee,
            rules: rules(burn),
            tax_bps: 0,
            ..CoinSpec::default()
        },
    );
    let m = c.mint;
    let attacker = if creator {
        c.creator.insecure_clone()
    } else {
        w.wallet_with_sol(40 * SOL)
    };
    w.buy(&attacker, &m, 30 * SOL).ok();
    for i in 0..n {
        fund_slot(&mut w, &m, i as u8, 5 * SOL);
    }
    w.env.warp(61);
    let a = attacker.pubkey();
    let (coin0, wealth0) = (w.env.holding(&m, &a), wealth(&w, &a));
    let fees0 = w.launch(&m).creator_fees_accrued as i128;
    let p = w.launch_pool(&m);
    let dump = (u128::from(front) * u128::from(p.base_reserve + p.virtual_base)
        / u128::from(p.quote_reserve + p.virtual_quote)) as u64;
    let sol0 = w.env.holding(&w.sol, &a);
    w.sell(&attacker, &m, dump.min(coin0)).ok();
    let got = w.env.holding(&w.sol, &a) - sol0;
    let (mut sold, mut bounties, mut sales) = (0, 0, 0);
    let before_sales = w.launch(&m).creator_fees_accrued as i128;
    for i in 0..n {
        let tx = w.execute(&attacker, &m, i as u8);
        tx.ok();
        if let Some(e) = tx.events::<SlotSold>().first() {
            sold += e.sold;
            bounties += e.bounty;
            sales += 1;
        }
    }
    let vault_sale_fees = w.launch(&m).creator_fees_accrued as i128 - before_sales;
    w.env
        .send_paid_by(&[w.launch_swap_ix(&a, &m, 1, got, 0)], &attacker, &[])
        .ok();
    let price = w.coin_price(&m);
    let coins = w.env.holding(&m, &a) as i128 - coin0 as i128;
    let value = coins * price as i128 / PRICE_SCALE as i128;
    let fees1 = w.launch(&m).creator_fees_accrued as i128;
    Sandwich {
        sales,
        sold,
        bounties,
        profit: wealth(&w, &a) - wealth0 + value,
        creator_fees: fees1 - fees0,
        vault_sale_fees,
    }
}

#[test]
fn f1_three_selling_slots_share_one_slice_and_the_sandwich_loses() {
    // (creator fee, burn): the cheapest pool, the common 1%, and the most a vault coin can charge
    // short of the 1% slice cap mattering less.
    let mut stacked_paid = Vec::new();
    for (fee, burn) in [(0u16, 0u16), (100, 0), (200, 0)] {
        for front in [SOL / 4, SOL / 2, SOL, 2 * SOL, 4 * SOL, 8 * SOL] {
            let one = sale_sandwich_n(1, fee, burn, front, false);
            let three = sale_sandwich_n(3, fee, burn, front, false);
            let ex1 = one.profit - one.bounties as i128;
            let ex3 = three.profit - three.bounties as i128;
            println!(
                "fee {fee} burn {burn} front {front}: 1 slot: sold {} bounty {} profit {} \
                 (beyond the bounty {ex1}); 3 slots: sold {} bounties {} profit {} (beyond {ex3})",
                one.sold, one.bounties, one.profit, three.sold, three.bounties, three.profit
            );
            // One slot: the port's bound holds, the sandwich itself loses.
            assert!(ex1 < 0, "single slot fee {fee} front {front}: {ex1}");
            // Three slots sell a third of the slice each: one slice together.
            assert_eq!((one.sales, three.sales), (1, 3), "fee {fee} front {front}");
            if ex3 >= 0 {
                stacked_paid.push((fee, front, ex3));
            }
        }
    }
    println!("stacked sandwiches that paid beyond the bounties: {stacked_paid:?}");
    assert!(stacked_paid.is_empty(), "{stacked_paid:?}");
}

// =====================================================================================================
// F2. The creator sandwiched its own vault's sells: the creator fee comes back to the creator, so its
// round trip cost only the LP fee and Bordrless's share. Fixed: the vault's slice counts only the
// fees nobody gets back (`vault_share_bps`: Bordrless's share of the creator fee, not the fee).
// =====================================================================================================

#[test]
fn f2_the_creators_sandwich_of_its_own_vaults_sale_loses() {
    let mut paid = Vec::new();
    for front in [SOL / 4, SOL / 2, SOL, 2 * SOL, 4 * SOL] {
        let s = sale_sandwich_n(1, 200, 0, front, true);
        // What the creator gains beyond an honest crank: its wallet (with the coin at the final
        // price) and the creator fees its own two trades paid it; not the bounty, not the creator
        // fee of the vault's sale, which it earns either way.
        let beyond = s.profit + (s.creator_fees - s.vault_sale_fees) - s.bounties as i128;
        println!(
            "creator fee 2%, creator attacks, front {front}: sold {}, bounty {}, wallet profit {}, \
             creator fees accrued {} (of which the vault's sale {}), net beyond an honest crank {beyond}",
            s.sold, s.bounties, s.profit, s.creator_fees, s.vault_sale_fees
        );
        assert!(s.sold > 0);
        if beyond >= 0 {
            paid.push((front, beyond));
        }
    }
    assert!(paid.is_empty(), "the creator's sandwich paid: {paid:?}");
}

// =====================================================================================================
// F3. A wait did not use up the slot's turn and caught up every idle interval at once: one
// transaction dumped, cranked a wait and cranked a sale at the dumped price. Fixed: a wait sets
// `waited_at` (`buy_waited_at`), and the next attempt is a minute later (round 2, V2: not a whole
// interval), so nothing follows it in the same transaction or block, and every trade restarts
// the reference's clock. (The companion's audited buyback has the same pattern: not changed here.)
// =====================================================================================================

#[test]
fn f3_a_wait_and_a_sale_can_no_longer_share_a_transaction() {
    let mut w = World::new();
    let to = w.env.funded(SOL);
    let c = w.vault_coin_spec(
        vault_args(vec![sell_for_sol(to.pubkey())]),
        &[],
        &CoinSpec {
            tax_bps: 0,
            open: false,
            ..CoinSpec::default()
        },
    );
    let m = c.mint;
    let whale = w.wallet_with_sol(25 * SOL);
    w.buy(&whale, &m, 20 * SOL).ok();
    // Opened after the whale's buy; the reference is the price now (as sales at that price leave
    // it: since X3 a vault opens at most at the opening price).
    w.open_vault(&c.creator, &m).ok();
    w.set_sell_reference_to_price(&m, 0);
    fund_slot(&mut w, &m, 0, 5 * SOL);
    w.env.warp(61);
    let cranker = w.wallet_with_sol(SOL);
    // One ordinary sale: the reference's clock restarts.
    w.execute(&cranker, &m, 0).event::<SlotSold>();
    let reference = w.vault(&m).slots[0].reference_price;
    assert_eq!(w.vault(&m).slots[0].reference_at, w.env.now);
    // A quiet hour: nobody cranks (the slot still holds coin).
    w.env.warp(3_600);
    // The whale dumps everything: far more than 3% below the reference.
    let held = w.env.holding(&m, &whale.pubkey());
    w.sell(&whale, &m, held).ok();
    let dumped = w.coin_price(&m);
    println!(
        "dumped price {:.4} of the reference",
        dumped as f64 / reference as f64
    );
    assert!(dumped < reference * 80 / 100);
    // The proof of concept's transaction (wait, then sale) now fails as a whole: after a wait the
    // slot's next attempt is a minute later.
    let sold0 = w.vault(&m).slots[0].sold;
    let ix = w.execute_ix(&cranker.pubkey(), &m, 0);
    let tx = atomic(&mut w, &cranker, &[ix.clone(), ix.clone()]);
    tx.expect_code(u32::from(hook_vault::error::VaultError::NotDue));
    println!(
        "wait + sale in one transaction: refused (NotDue), {} bytes",
        tx.size
    );
    // The wait alone lands; a sale in the same block (or within the next minute) does not.
    let waited = w.execute(&cranker, &m, 0).event::<SellWaited>();
    println!(
        "the wait: reference {:.4} -> {:.4} of the old reference",
        1.0,
        waited.new_reference as f64 / reference as f64
    );
    w.execute(&cranker, &m, 0)
        .expect_code(u32::from(hook_vault::error::VaultError::NotDue));
    w.env.warp(59);
    w.execute(&cranker, &m, 0)
        .expect_code(u32::from(hook_vault::error::VaultError::NotDue));
    assert_eq!(
        w.vault(&m).slots[0].sold,
        sold0,
        "nothing sold at the dumped price"
    );
}

#[test]
fn f3_the_buy_leg_can_no_longer_wait_and_buy_in_one_transaction() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(5 * SOL);
    let (x, tx) = w.create_launch_with(&creator, "XX", 100, VQ, LaunchRules::NONE);
    tx.ok();
    let x_pool = w.launch_pool_key(&x);
    let c = w.vault_coin(vault_args(vec![sell_buy_burn(x_pool)]), &[x], true);
    let m = c.mint;
    fund_slot(&mut w, &m, 0, 3 * SOL);
    w.env.warp(61);
    let cranker = w.wallet_with_sol(SOL);
    w.execute(&cranker, &m, 0).event::<SlotSold>();
    let reference = w.vault(&m).slots[0].buy_reference;
    w.env.warp(3_600);
    // Someone pumps the token bought by far more than 3%.
    let pumper = w.wallet_with_sol(20 * SOL);
    w.buy(&pumper, &x, 15 * SOL).ok();
    let pumped = price_of(&w, &x_pool);
    println!(
        "pumped price {:.4} of the buy reference",
        pumped as f64 / reference as f64
    );
    assert!(pumped > reference * 150 / 100);
    let ix = w.execute_buy_ix(&cranker.pubkey(), &m, 0);
    let tx = atomic(&mut w, &cranker, &[ix.clone(), ix]);
    tx.expect_code(u32::from(hook_vault::error::VaultError::NotDue));
    // The wait alone lands and uses up the turn to buy.
    w.execute_buy(&cranker, &m, 0).event::<BuyWaited>();
    assert_eq!(w.vault(&m).slots[0].buy_waited_at, w.env.now);
    w.execute_buy(&cranker, &m, 0)
        .expect_code(u32::from(hook_vault::error::VaultError::NotDue));
    println!("wait + buy in one transaction: refused; after the wait alone the next attempt is a minute later");
}

// =====================================================================================================
// F4. `retire` ignored waits: with a long interval anyone burned a live slot's coin though it was
// cranked and waiting. Fixed: a wait is a run (it sets `last_at`), so a slot cranked while it waits
// is never retired, and its reference comes down until it sells.
// =====================================================================================================

#[test]
fn f4_retire_refuses_a_live_slot_that_is_cranked_while_it_waits() {
    let mut w = World::new();
    let to = w.env.funded(SOL);
    let mut a = vault_args(vec![sell_for_sol(to.pubkey())]);
    a.interval = MAX_INTERVAL;
    let c = w.vault_coin_spec(
        a,
        &[],
        &CoinSpec {
            tax_bps: 0,
            open: false,
            ..CoinSpec::default()
        },
    );
    let m = c.mint;
    let whale = w.wallet_with_sol(15 * SOL);
    w.buy(&whale, &m, 10 * SOL).ok();
    w.open_vault(&c.creator, &m).ok();
    w.set_sell_reference_to_price(&m, 0);
    fund_slot(&mut w, &m, 0, 5 * SOL);
    w.env.warp(61);
    let cranker = w.wallet_with_sol(SOL);
    w.execute(&cranker, &m, 0).event::<SlotSold>();
    let left = w.slot_coin(&m, 0);
    assert!(left > 0, "the slot still holds coin after one slice");
    // The whale takes its profit: the price falls by a third or so.
    let held = w.env.holding(&m, &whale.pubkey());
    w.sell(&whale, &m, held).ok();
    // Cranked every interval: each crank waits (the reference comes down 5% an interval).
    let to_before = w.env.lamports(&to.pubkey());
    for k in 0..2 {
        w.env.warp(MAX_INTERVAL);
        let ev = w.execute(&cranker, &m, 0).event::<SellWaited>();
        println!(
            "crank {k}: waited, reference {:.4} -> {:.4} of the price",
            ev.reference as f64 / ev.price as f64,
            ev.new_reference as f64 / ev.price as f64
        );
    }
    // 60 days after the sale: the slot waited a moment ago, so it is live and can't be retired.
    let griefer = w.wallet_with_sol(SOL);
    w.retire(&griefer, &m, 0, true)
        .expect_code(u32::from(hook_vault::error::VaultError::NotRetirable));
    assert_eq!(w.slot_coin(&m, 0), left);
    // Kept cranking, the reference reaches the price and the slot sells to its wallet.
    let mut cranks = 0;
    loop {
        w.env.warp(MAX_INTERVAL);
        cranks += 1;
        let tx = w.execute(&cranker, &m, 0);
        if let Some(ev) = tx.events::<SlotSold>().first() {
            println!("crank {cranks} after: sold {} for {}", ev.sold, ev.paid);
            break;
        }
        tx.event::<SellWaited>();
        assert!(cranks < 6, "the reference never came down");
    }
    assert!(w.env.lamports(&to.pubkey()) > to_before);
    // A slot nobody runs for 60 days is still retired.
    w.env.warp(RETIRE_SECS);
    let held = w.slot_coin(&m, 0);
    let ev = w.retire(&griefer, &m, 0, true).event::<SlotRetired>();
    assert_eq!(ev.coin_burned, held);
}

// =====================================================================================================
// F5. A buy target with max wallet: one buy slice delivered more than the cap, so every buy failed
// (and `retire` would incinerate the SOL). Fixed: the buy is cut to what the kit's cap leaves room
// for, until the token graduates.
// =====================================================================================================

/// A vault whose slot 0 buys a token with kit max wallet `max_wallet_bps` (creator fee 2%, burns 1%:
/// the most fees the launch policy allows, so the vault's largest slice of its pool), its slot sold
/// four times. Answers the world, the coin, the token and the cranker.
fn max_wallet_world(max_wallet_bps: u16) -> (World, Pubkey, Pubkey, Keypair) {
    let mut w = World::new();
    let creator = w.wallet_with_sol(5 * SOL);
    let x_rules = LaunchRules {
        max_wallet_bps,
        burn_buy_bps: 100,
        burn_sell_bps: 100,
        ..LaunchRules::NONE
    };
    let (x, tx) = w.create_launch_with(&creator, "XX", 200, VQ, x_rules);
    tx.ok();
    let x_pool = w.launch_pool_key(&x);
    let c = w.vault_coin(vault_args(vec![sell_buy_burn(x_pool)]), &[x], true);
    let m = c.mint;
    let whale = w.wallet_with_sol(25 * SOL);
    w.buy(&whale, &m, 20 * SOL).ok();
    fund_slot(&mut w, &m, 0, 15 * SOL);
    let cranker = w.wallet_with_sol(SOL);
    for _ in 0..4 {
        w.env.warp(61);
        w.execute(&cranker, &m, 0).event::<SlotSold>();
    }
    (w, m, x, cranker)
}

/// Rewrites the kit's wallet cap of `x` (as a token launched with a tighter cap would have it).
fn set_wallet_cap(w: &mut World, x: &Pubkey, cap: u64) {
    let key = w.launch(x).kit_config;
    let mut kit: bordrless_kit::state::KitConfig = w.env.read(&key);
    kit.max_wallet_amount = cap;
    let mut account = w.env.account(&key).expect("kit config");
    let mut data = Vec::new();
    anchor_lang::AccountSerialize::try_serialize(&kit, &mut data).expect("serialize");
    account.data[..data.len()].copy_from_slice(&data);
    w.env.put(key, account);
}

/// Buys until the slot's SOL is spent (at most `n` buys), checking each lands within the cap.
/// Answers (buys, buys the cap cut).
fn buy_within_cap(
    w: &mut World,
    m: &Pubkey,
    x: &Pubkey,
    cranker: &Keypair,
    n: usize,
) -> (u32, u32) {
    use hook_vault::instructions::common::{quote_cap, vault_share_bps};
    let xl = w.launch(x);
    let cap = w
        .env
        .read::<bordrless_kit::state::KitConfig>(&xl.kit_config)
        .max_wallet_amount;
    let (mut buys, mut capped) = (0, 0);
    for _ in 0..n {
        let pending = w.vault(m).slots[0].pending_sol;
        if pending == 0 {
            break;
        }
        let p = w.launch_pool(x);
        let slice = pending.min(quote_cap(
            &p,
            vault_share_bps(&xl, u64::from(p.protocol_share_bps)),
        ));
        let ev = w.execute_buy(cranker, m, 0).event::<SlotBought>();
        println!(
            "cap {cap}: spent {} (+ bounty {}) of {slice} the slice allows, bought and burned {}",
            ev.spent, ev.bounty, ev.burned
        );
        assert!(ev.burned > 0 && ev.burned <= cap);
        assert_eq!(w.env.holding(x, &slot_owner(m, 0)), 0);
        buys += 1;
        if ev.spent + ev.bounty < slice {
            capped += 1;
        }
        w.env.warp(61);
    }
    (buys, capped)
}

#[test]
fn f5_a_max_wallet_token_is_bought_within_its_cap() {
    // The proof of concept's token (max wallet 1%, the policy's smallest; the most fees): the buys
    // land. (Since F2's fix the vault's slice of a launch pool is at most 0.9%, below 1% of the
    // supply; the cap guards it all the same.)
    let (mut w, m, x, cranker) = max_wallet_world(100);
    let (buys, _) = buy_within_cap(&mut w, &m, &x, &cranker, 3);
    assert!(buys > 0);
    // A tighter cap (a quarter of it): every buy is cut to what it leaves room for, and lands.
    let (mut w, m, x, cranker) = max_wallet_world(100);
    let cap = w
        .env
        .read::<bordrless_kit::state::KitConfig>(&w.launch(&x).kit_config)
        .max_wallet_amount;
    set_wallet_cap(&mut w, &x, cap / 4);
    let incinerator = w.env.lamports(&INCINERATOR);
    let (buys, capped) = buy_within_cap(&mut w, &m, &x, &cranker, 6);
    assert!(buys >= 2 && capped >= 1, "{buys} buys, {capped} capped");
    // Someone fills the slot owner's holding of the token up to the cap: what it sent is burned
    // first, and the buy goes on.
    let filler = w.wallet_with_sol(10 * SOL);
    w.buy(&filler, &x, SOL / 20).ok();
    let sent = w.env.holding(&x, &filler.pubkey()).min(cap / 4);
    w.send_tokens(&filler, x, &slot_owner(&m, 0), sent).ok();
    assert_eq!(w.env.holding(&x, &slot_owner(&m, 0)), sent);
    fund_slot(&mut w, &m, 0, 2 * SOL);
    w.execute(&cranker, &m, 0).event::<SlotSold>();
    let supply0 = w.env.read::<bordrless_token::state::Mint>(&x).supply;
    let ev = w.execute_buy(&cranker, &m, 0).event::<SlotBought>();
    assert!(ev.burned > sent, "the donation and the buy were burned");
    // (The token's own burn on buys takes a little more.)
    assert!(w.env.read::<bordrless_token::state::Mint>(&x).supply <= supply0 - ev.burned);
    assert_eq!(w.env.holding(&x, &slot_owner(&m, 0)), 0);
    // Nothing went to the incinerator.
    assert_eq!(w.env.lamports(&INCINERATOR), incinerator);
}

// =====================================================================================================
// F6. The token bought's own custom hook may take a cut on the way to the slot; `execute_buy`'s
// min_out allowed no hook cut, so a hook raising its cut blocked the buy leg for good. Fixed: a
// `SellBuyBurn` slot declares the most the bought token's hook may cut (`max_cut_bps`, at most 50%,
// only for a custom-hook token), included in the buy's min_out. A cut above the declared one still
// blocks the buy (the creator declared it; the label shows it).
// =====================================================================================================

fn hook_cut_buy(declared: u16, tax: u16) -> Tx {
    let mut w = World::new();
    let y = w.vault_coin(vault_args(vec![burn()]), &[], true).mint;
    let y_pool = w.launch_pool_key(&y);
    let c = w.vault_coin(
        vault_args(vec![sell_buy_burn_cut(y_pool, declared)]),
        &[y],
        true,
    );
    let m = c.mint;
    fund_slot(&mut w, &m, 0, 4 * SOL);
    w.env.warp(61);
    let cranker = w.wallet_with_sol(SOL);
    w.execute(&cranker, &m, 0).event::<SlotSold>();
    w.set_tax(&y, tax, 0);
    w.execute_buy(&cranker, &m, 0)
}

#[test]
fn f6_a_declared_cut_lets_the_buy_leg_pass_the_bought_tokens_hook() {
    // Y's hook (tax_hook) goes from 1% to 3%: a slot that declared 5% still buys.
    let tx = hook_cut_buy(500, 300);
    let ev = tx.event::<SlotBought>();
    println!(
        "declared 5%, the hook takes 3%: bought and burned {}",
        ev.burned
    );
    assert!(ev.burned > 0);
    // Up to the declared cut: 5%.
    hook_cut_buy(500, 500).event::<SlotBought>();
    // A slot that declared nothing is still blocked by a 3% cut (min_out), as is one whose token's
    // hook takes more than declared.
    for (declared, tax) in [(0u16, 300u16), (500, 900)] {
        let tx = hook_cut_buy(declared, tax);
        tx.expect_fail();
        assert!(
            tx.logs().iter().any(|l| l.contains("Slippage")),
            "{}",
            tx.logs().join("\n")
        );
        println!("declared {declared}, the hook takes {tax}: the buy fails (min_out)");
    }
}

// =====================================================================================================
// F7. The vault could be created after the launch. Fixed: `create_vault` requires the mint to have no
// data and be the system program's (not created yet).
// =====================================================================================================

#[test]
fn f7_the_mint_keypair_can_no_longer_create_the_vault_after_launch() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(50 * SOL);
    let config = w.tax_hook_config(&creator, 100, LaunchRules::NONE);
    let mint = Keypair::new();
    let m = mint.pubkey();
    // The hook sends 1% of every transfer to slot 0's owner; no vault exists.
    let prep = tax_prepare(creator.pubkey(), m, slot_owner(&m, 0), 100, 0);
    w.env.send_paid_by(&[prep], &creator, &[]).ok();
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &m, "VLT", VQ, &config);
    w.env.send_paid_by(&[ix], &creator, &[&mint]).ok();
    // Anyone may create slot 0's holding; from then on cuts accrue to a slot with no policy.
    let ix = token::create_holding(creator.pubkey(), m, slot_owner(&m, 0));
    w.env.send_paid_by(&[ix], &creator, &[]).ok();
    w.churn(&m, 3, 2 * SOL);
    let accrued = w.slot_coin(&m, 0);
    assert!(accrued > 0);
    w.env.warp(3_600);
    // An hour after the launch the creator tries to pick the policy: refused.
    let ix = vault_client::create_vault(
        creator.pubkey(),
        m,
        vault_args(vec![sell_for_sol(creator.pubkey())]),
        &[],
    );
    w.env
        .send_paid_by(&[ix], &creator, &[&mint])
        .expect_code(u32::from(hook_vault::error::VaultError::MintExists));
    assert!(w.env.account(&vault_client::vault_address(&m)).is_none());
    println!(
        "vault creation after the launch refused; {accrued} coin stays in a slot no vault can move"
    );
}

// =====================================================================================================
// F8. A SellForSol wallet that was a program (or a PDA nobody signs for): every sale failed, the slot
// was retired later. Fixed: `create_vault` refuses such a wallet.
// =====================================================================================================

#[test]
fn f8_a_sale_to_a_program_address_is_refused_at_creation() {
    let mut w = World::new();
    let payer = w.wallet_with_sol(5 * SOL);
    let mint = Keypair::new();
    let ix = vault_client::create_vault(
        payer.pubkey(),
        mint.pubkey(),
        vault_args(vec![sell_for_sol(tax_hook::ID)]),
        &[],
    );
    w.env
        .send_paid_by(&[ix], &payer, &[&mint])
        .expect_code(u32::from(hook_vault::error::VaultError::BadWallet));
}

// =====================================================================================================
// Checked, fine: custody and substitution.
// =====================================================================================================

/// The SellForSol wallet substituted in `execute`: the program pays only `slot.target`.
#[test]
fn ok_the_paid_wallet_cannot_be_substituted() {
    let mut w = World::new();
    let to = w.env.funded(SOL);
    let c = w.vault_coin_spec(
        vault_args(vec![sell_for_sol(to.pubkey())]),
        &[],
        &CoinSpec {
            tax_bps: 0,
            ..CoinSpec::default()
        },
    );
    let m = c.mint;
    fund_slot(&mut w, &m, 0, 2 * SOL);
    w.env.warp(61);
    let thief = w.wallet_with_sol(SOL);
    let mut ix = w.execute_ix(&thief.pubkey(), &m, 0);
    for a in ix.accounts.iter_mut() {
        if a.pubkey == to.pubkey() {
            a.pubkey = thief.pubkey();
        }
    }
    w.env
        .send_paid_by(&[ix], &thief, &[])
        .expect_code(u32::from(hook_vault::error::VaultError::MissingAccount));
}

/// A fake launch (another coin's launch) in the step: Anchor's address constraint.
#[test]
fn ok_another_coins_launch_or_pool_is_refused() {
    let mut w = World::new();
    let c = w.vault_coin(vault_args(vec![burn(), burn()]), &[], true);
    let d = w.vault_coin(vault_args(vec![burn()]), &[], true);
    w.churn(&c.mint, 1, SOL);
    let cranker = w.wallet_with_sol(SOL);
    let mut ix = w.execute_ix(&cranker.pubkey(), &c.mint, 0);
    ix.accounts[3].pubkey = launch_of(&d.mint);
    // ConstraintAddress.
    w.env.send_paid_by(&[ix], &cranker, &[]).expect_code(2012);
    // Vault of coin D with slot owner of coin C.
    let mut ix = w.execute_ix(&cranker.pubkey(), &d.mint, 0);
    ix.accounts[2].pubkey = slot_owner(&c.mint, 0);
    w.env
        .send_paid_by(&[ix], &cranker, &[])
        .expect_code(u32::from(hook_vault::error::VaultError::WrongSlotOwner));
}

/// Retire never pays the cranker, and leaves other slots alone.
#[test]
fn ok_retire_touches_only_its_slot_and_pays_nobody() {
    let mut w = World::new();
    let c = w.vault_coin(vault_args(vec![burn(), burn()]), &[], true);
    let m = c.mint;
    w.churn(&m, 2, SOL);
    fund_slot(&mut w, &m, 1, SOL);
    let (zero, one) = (w.slot_coin(&m, 0), w.slot_coin(&m, 1));
    w.env.warp(RETIRE_SECS);
    let cranker = w.wallet_with_sol(SOL);
    let l = w.env.lamports(&cranker.pubkey());
    w.retire(&cranker, &m, 1, true).event::<SlotRetired>();
    assert_eq!((w.slot_coin(&m, 0), w.slot_coin(&m, 1)), (zero, 0));
    assert_eq!(w.env.lamports(&cranker.pubkey()), l - 5_000);
    let _ = one;
}

// =====================================================================================================
// Limits: every step through a router (hook_tester's `invoke_as_hook`), with burn rules on the
// coin (the launch hook's burn) and a custom-hook token bought.
// =====================================================================================================

fn table_22(w: &World) -> Vec<Pubkey> {
    let mut addresses = protocol_lookup_table(w);
    addresses.extend([
        vault_client::event_authority(),
        bordrless_launch::ID,
        bordrless_swap::ID,
        bordrless_token::ID,
    ]);
    assert_eq!(addresses.len(), 22);
    addresses
}

fn via_router(ix: Instruction) -> Instruction {
    let (authority, bump) = bordrless_hook::hook_authority(&hook_tester::ID);
    let mut accounts = vec![
        AccountMeta::new_readonly(authority, false),
        AccountMeta::new_readonly(ix.program_id, false),
    ];
    accounts.extend(ix.accounts.iter().cloned());
    Instruction {
        program_id: hook_tester::ID,
        accounts,
        data: hook_tester::instruction::InvokeAsHook {
            bump,
            data: ix.data,
        }
        .data(),
    }
}

fn routed(w: &mut World, name: &str, cranker: &Keypair, ix: Instruction) -> Tx {
    let table = table_22(w);
    let lut = w.env.put_lookup_table(Pubkey::new_unique(), &table);
    let tx = w.env.send_v0(
        &[compute_unit_limit(1_400_000), via_router(ix)],
        cranker,
        &[],
        std::slice::from_ref(&lut),
    );
    match &tx.result {
        Ok(_) => println!(
            "routed {name}: {} bytes, {} CU, height {}, trace {}{}",
            tx.size,
            tx.cu(),
            tx.max_height(),
            tx.trace_len(),
            tx.logs()
                .iter()
                .filter_map(|l| l.split("heap used: ").nth(1))
                .map(|h| format!(", heap {h}"))
                .collect::<String>()
        ),
        Err(e) => println!(
            "routed {name}: FAILED {:?} ({} bytes)\n{}",
            e.err,
            tx.size,
            tx.logs().join("\n")
        ),
    }
    tx
}

#[test]
fn limits_every_step_through_a_router_with_burn_rules() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(5 * SOL);
    let (x, tx) = w.create_launch_with(
        &creator,
        "XX",
        200,
        VQ,
        LaunchRules {
            max_wallet_bps: 500,
            burn_buy_bps: 100,
            burn_sell_bps: 100,
            ..LaunchRules::NONE
        },
    );
    tx.ok();
    let x_pool = w.launch_pool_key(&x);
    let to = w.env.funded(SOL);
    let c = w.vault_coin_spec(
        vault_args(vec![
            burn(),
            sell_for_sol(to.pubkey()),
            sell_buy_burn(x_pool),
        ]),
        &[x],
        &CoinSpec {
            creator_fee_bps: 200,
            rules: rules(100),
            open: false,
            ..CoinSpec::default()
        },
    );
    let m = c.mint;
    let cranker = w.wallet_with_sol(5 * SOL);
    // The vault's creator opens it (X3).
    let creator = c.creator.insecure_clone();
    let ix = w.open_vault_ix(&creator.pubkey(), &m);
    let mut fails = Vec::new();
    let mut check = |name: &str, tx: &Tx| {
        if tx.result.is_err() {
            fails.push(name.to_string());
        } else {
            assert!(tx.size <= PACKET && tx.max_height() <= 5 && tx.trace_len() <= 64);
        }
    };
    let tx = routed(&mut w, "open_vault", &creator, ix);
    check("open_vault", &tx);
    if tx.result.is_err() {
        w.open_vault(&creator, &m).ok();
    }
    w.churn(&m, 3, SOL);
    fund_slot(&mut w, &m, 1, SOL);
    fund_slot(&mut w, &m, 2, 2 * SOL);
    w.env.warp(61);
    for (i, name) in [(0u8, "burn"), (1, "sell for SOL"), (2, "sell to buy")] {
        // (An interval apart: not needed since round 2, kept so the numbers stay comparable.)
        if i == 2 {
            w.env.warp(61);
        }
        let ix = w.execute_ix(&cranker.pubkey(), &m, i);
        let tx = routed(&mut w, name, &cranker, ix);
        check(name, &tx);
    }
    if w.vault(&m).slots[2].pending_sol == 0 {
        w.execute(&cranker, &m, 2).ok();
    }
    let ix = w.execute_buy_ix(&cranker.pubkey(), &m, 2);
    let tx = routed(&mut w, "execute_buy (kit token)", &cranker, ix);
    check("execute_buy kit", &tx);
    fund_slot(&mut w, &m, 2, SOL);
    w.env.warp(61);
    w.execute(&cranker, &m, 2).ok();
    fund_slot(&mut w, &m, 2, SOL);
    w.env.warp(RETIRE_SECS);
    let ix = w.retire_ix(&cranker.pubkey(), &m, 2, true);
    let tx = routed(&mut w, "retire", &cranker, ix);
    check("retire", &tx);

    // A custom-hook token bought, through the router.
    let mut w = World::new();
    let y = w.vault_coin(vault_args(vec![burn()]), &[], true).mint;
    let y_pool = w.launch_pool_key(&y);
    let c = w.vault_coin_spec(
        vault_args(vec![sell_buy_burn(y_pool)]),
        &[y],
        &CoinSpec {
            rules: rules(100),
            ..CoinSpec::default()
        },
    );
    let m = c.mint;
    let cranker = w.wallet_with_sol(5 * SOL);
    fund_slot(&mut w, &m, 0, 2 * SOL);
    w.env.warp(61);
    w.execute(&cranker, &m, 0).ok();
    let ix = w.execute_buy_ix(&cranker.pubkey(), &m, 0);
    let tx = routed(&mut w, "execute_buy (custom-hook token)", &cranker, ix);
    check("execute_buy custom", &tx);
    println!("routed failures: {fails:?}");
    assert!(fails.is_empty(), "{fails:?}");
}

/// `create_vault` refuses a SellForSol wallet that is one of the vault's slot owners, and (fixed in
/// round 1, F8) the owner of an index the vault does not use too (a PDA nobody signs for).
#[test]
fn info_a_sale_to_an_unused_slot_index_owner_is_refused() {
    let mut w = World::new();
    let payer = w.wallet_with_sol(5 * SOL);
    let mint = Keypair::new();
    let a = CreateVaultArgs {
        slots: vec![burn(), sell_for_sol(slot_owner(&mint.pubkey(), 2))],
        ..vault_args(vec![])
    };
    let ix = vault_client::create_vault(payer.pubkey(), mint.pubkey(), a, &[]);
    // Slot 2 doesn't exist in a two-slot vault, but its owner is refused all the same: nobody
    // could ever spend what is paid there.
    let tx = w.env.send_paid_by(&[ix], &payer, &[&mint]);
    tx.expect_code(u32::from(hook_vault::error::VaultError::BadWallet));
    println!("a sale to an unused slot index's owner is refused");
}

// =====================================================================================================
// F1 (buy side). Several SellBuyBurn slots buying the same token stacked the same way. Fixed: a
// vault has at most one buy slot a pool (`DuplicatePool`), and a wait can no longer be followed by a
// buy in one bundle (F3). Across vaults the residual stays (documented).
// =====================================================================================================

/// A SellBuyBurn slot on one hook-less token X (creator fee `x_fee`); the attacker pumps X by
/// `front` lamports, cranks the slot's buy once, and sells all it bought. Answers (spent by the
/// vault, bounty, attacker profit).
fn buy_sandwich_one(x_fee: u16, front: u64) -> (u64, u64, i128) {
    let mut w = World::new();
    let creator = w.wallet_with_sol(5 * SOL);
    let (x, tx) = w.create_launch_with(&creator, "XX", x_fee, VQ, LaunchRules::NONE);
    tx.ok();
    let x_pool = w.launch_pool_key(&x);
    let c = w.vault_coin(vault_args(vec![sell_buy_burn(x_pool)]), &[x], true);
    let m = c.mint;
    fund_slot(&mut w, &m, 0, 3 * SOL);
    w.env.warp(61);
    let cranker = w.wallet_with_sol(SOL);
    w.execute(&cranker, &m, 0).event::<SlotSold>();
    w.env.warp(3_600);
    let attacker = w.wallet_with_sol(30 * SOL);
    let a = attacker.pubkey();
    let start = wealth(&w, &a);
    let ixs = [
        token::create_holding(a, x, a),
        w.launch_swap_ix(&a, &x, 1, front, 0),
    ];
    w.env.send_paid_by(&ixs, &attacker, &[]).ok();
    let tx = w.execute_buy(&attacker, &m, 0);
    tx.ok();
    let (spent, bounty) = tx
        .events::<SlotBought>()
        .first()
        .map_or((0, 0), |e| (e.spent, e.bounty));
    let got = w.env.holding(&x, &a);
    w.sell(&attacker, &x, got).ok();
    // The attacker's X holding's rent is its own (it closes it later).
    let rent = w.env.lamports(&token::holding_address(&x, &a)) as i128;
    (spent, bounty, wealth(&w, &a) + rent - start)
}

#[test]
fn f1_three_buy_slots_on_one_token_are_refused_and_one_slot_loses() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(5 * SOL);
    let (x, tx) = w.create_launch_with(&creator, "XX", 0, VQ, LaunchRules::NONE);
    tx.ok();
    let x_pool = w.launch_pool_key(&x);
    let payer = w.wallet_with_sol(5 * SOL);
    let mint = Keypair::new();
    let ix = vault_client::create_vault(
        payer.pubkey(),
        mint.pubkey(),
        vault_args(vec![sell_buy_burn(x_pool); 3]),
        &[x],
    );
    w.env
        .send_paid_by(&[ix], &payer, &[&mint])
        .expect_code(u32::from(hook_vault::error::VaultError::DuplicatePool));
    // The one slot a vault may have on X: its sandwich loses at every pump.
    for x_fee in [0u16, 100] {
        for front in [SOL / 2, SOL, 2 * SOL, 4 * SOL] {
            let (spent, bounty, profit) = buy_sandwich_one(x_fee, front);
            let beyond = profit - bounty as i128;
            println!(
                "buy, x fee {x_fee}, front {front}: one slot spent {spent} profit {profit} \
                 (beyond the bounty {beyond})"
            );
            assert!(beyond < 0, "x fee {x_fee} front {front}: {beyond}");
        }
    }
}

// =====================================================================================================
// Heap: a maximum registry (8 PDAs of 15 seeds, as many literal bytes as 1,024 bytes allow) on the
// token bought, decoded twice in one `execute_buy` (its transfer and its burn). Measured with a
// `heap-probe` build of the vault (it logs `heap used: N`); with the ordinary build the test only
// checks the buy lands.
// =====================================================================================================

/// Rewrites the registry of `hook` for `mint`: its own accounts first (what the hook reads), then
/// PDAs of a filler program up to `MAX_REGISTRY_ACCOUNTS`, each of 15 seeds (`find_program_address`
/// adds the bump: 16 in all), owner seeds mostly, filled up with literal bytes to `MAX_REGISTRY_LEN`.
fn max_registry(w: &mut World, hook: &Pubkey, mint: &Pubkey) -> usize {
    use bordrless_hook::{
        hook_accounts_address, AccountSource, ExtraAccount, HookAccountList, Seed,
    };
    let key = hook_accounts_address(hook, mint).0;
    let mut account = w.env.account(&key).expect("registry");
    let mut list = HookAccountList::decode(&account.data).expect("decodes");
    let filler = Pubkey::new_unique();
    let own = list.accounts.len();
    while list.accounts.len() < MAX_REGISTRY_ACCOUNTS {
        list.accounts.push(ExtraAccount {
            writable: false,
            source: AccountSource::Pda {
                program: filler,
                seeds: vec![Seed::SourceOwner; 15],
            },
        });
    }
    // Widen literal seeds while the registry stays within its bounds.
    'fill: for a in own..MAX_REGISTRY_ACCOUNTS {
        for k in 0..15 {
            for n in (1..=MAX_REGISTRY_SEED_LEN).rev() {
                let mut l = list.clone();
                if let AccountSource::Pda { seeds, .. } = &mut l.accounts[a].source {
                    seeds[k] = Seed::Literal(vec![a as u8 + k as u8; n]);
                }
                if l.encode().len() <= MAX_REGISTRY_LEN {
                    list = l;
                    break;
                }
                if n == 1 {
                    break 'fill;
                }
            }
        }
    }
    let data = list.encode();
    assert!(data.len() <= MAX_REGISTRY_LEN);
    assert!(hook_vault::instructions::common::registry_within_bounds(
        &data
    ));
    account.data = data.clone();
    account.lamports = account.lamports.max(w.env.rent(data.len()));
    w.env.put(key, account);
    data.len()
}

#[test]
fn heap_a_maximum_registry_decoded_twice_in_one_buy() {
    let mut w = World::new();
    let y = w.vault_coin(vault_args(vec![burn()]), &[], true).mint;
    let len = max_registry(&mut w, &tax_hook::ID, &y);
    let y_pool = w.launch_pool_key(&y);
    let c = w.vault_coin(vault_args(vec![sell_buy_burn_cut(y_pool, 100)]), &[y], true);
    let m = c.mint;
    fund_slot(&mut w, &m, 0, 2 * SOL);
    w.env.warp(61);
    let cranker = w.wallet_with_sol(5 * SOL);
    w.execute(&cranker, &m, 0).event::<SlotSold>();
    let ix = w.execute_buy_ix(&cranker.pubkey(), &m, 0);
    let accounts = ix.accounts.len();
    let tx = atomic(&mut w, &cranker, &[ix]);
    let heap: Vec<&String> = tx
        .logs()
        .iter()
        .filter(|l| l.contains("heap used:"))
        .collect();
    println!(
        "execute_buy with a {len}-byte registry (8 accounts) on the token bought: {} accounts, \
         {} CU, {} bytes; {}",
        accounts,
        tx.cu(),
        tx.size,
        if heap.is_empty() {
            "heap not instrumented (ordinary build)".to_string()
        } else {
            format!("{heap:?} of 32,768")
        }
    );
    if tx.result.is_err() {
        println!("{}", tx.logs().join("\n"));
    }
    tx.event::<SlotBought>();
    for l in heap {
        let n: usize = l.rsplit(' ').next().unwrap().parse().unwrap();
        assert!(n < 32 * 1024);
    }
}
