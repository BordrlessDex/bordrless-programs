//! Independent audit of the curve fill (2026-10-14): adversarial proofs.
//!
//! - The router charges its integrator fee on the whole input of a buy before the DEX runs; a
//!   filled buy uses only part of it, so the fee is charged on input that never swaps.
//! - A companion's `dev_buy` past the end of the curve: the unused SOL stays in the companion's
//!   creator holding (not the beneficiary's), where `claim_fees` splits it as fees.
//! - A third-party curve whose hook answers a fill of the whole input past the end: the trader's
//!   overpay stays in the pool's quote reserve, where earlier holders sell into it.
//! - Compute: an ordinary buy and filled buys of growing size (bisection), patched vs mainnet.

use anchor_lang::prelude::Pubkey;
use bordrless_companion::client as companion;
use bordrless_companion::instructions::CreateArgs;
use bordrless_companion::state::Split;
use bordrless_core::policy;
use bordrless_hook::{answer_bytes, pool_flags, HookReturn};
use bordrless_launch::client::{self as launch, LaunchKeys};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::hooks::{NewPool, SwapSpec};
use bordrless_program_tests::launch::*;
use bordrless_router::{client as router, fee_split, IntegratorFeePaid};
use bordrless_swap::client as swap;
use bordrless_swap::events::Swapped;
use bordrless_swap::instructions::CreatePoolArgs;
use bordrless_swap::state::Pool;
use bordrless_token::client as token;
use hook_tester::client as tester;
use hook_tester::{callback, mode};
use solana_keypair::Keypair;
use solana_signer::Signer;

const MAINNET_DIR: &str =
    "/tmp/claude-1000/-home-ubuntu-BORDRLESS/afd29fc2-f987-4995-8401-9e5be2ffe62e/scratchpad/mainnet";

fn near_the_end(w: &mut World, symbol: &str, leave: u64) -> Pubkey {
    let creator = w.wallet_with_sol(20 * SOL);
    let (mint, tx) = w.create_launch_with(&creator, symbol, 100, VQ, presets::plain());
    tx.ok();
    w.env.warp(policy::SNIPER_WINDOW_SECS + 1);
    w.fill_curve(&mint, leave);
    mint
}

// ------------------------------------------------------------------------------------------------
// 1. Router: the fee on unused input.
// ------------------------------------------------------------------------------------------------

#[test]
fn fixed_a_routed_buy_past_the_end_is_not_filled_and_pays_no_fee() {
    let mut w = World::new();
    let mint = near_the_end(&mut w, "RTR", SOL / 2);
    // Holdings for the integrator and the treasury.
    let integrator = w.wallet_with_sol(SOL);
    let treasury = w.env.treasury.pubkey();
    let payer = w.env.funded(SOL);
    w.env
        .send_paid_by(
            &[token::create_holding(payer.pubkey(), w.sol, treasury)],
            &payer,
            &[],
        )
        .ok();
    let integrator_quote = token::holding_address(&w.sol, &integrator.pubkey());
    let treasury_quote = token::holding_address(&w.sol, &treasury);

    let asked = 50 * SOL;
    let t = w.wallet_with_sol(asked);
    w.holdings(&t, mint, &[t.pubkey()]);
    let me = t.pubkey();
    let sol0 = w.env.holding(&w.sol, &me);
    let dex = w.launch_swap_ix(&me, &mint, 1, asked, 0);
    let fee_bps = 100;
    let ix = router::swap(me, integrator_quote, treasury_quote, &dex, fee_bps, 0);
    // Fixed: a buy the router sends the DEX (a CPI) is never filled, so it is refused past the end
    // as it always was (InsufficientLiquidity, 6016) and no fee is charged on anything.
    let tx = w.env.send_paid_by(&[ix], &t, &[]);
    tx.expect_code(6016);
    assert_eq!(w.env.holding(&w.sol, &me), sol0);
    let _ = (
        fee_split(asked, fee_bps),
        std::any::type_name::<IntegratorFeePaid>(),
    );
    assert!(w.launch_pool(&mint).base_reserve > 0);
}

// ------------------------------------------------------------------------------------------------
// 2. Companion dev_buy past the end.
// ------------------------------------------------------------------------------------------------

#[test]
fn fixed_a_companion_dev_buy_past_the_end_is_not_filled() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(10 * SOL);
    let split = Split {
        buyback_bps: 5_000,
        holders_bps: 0,
        beneficiary_bps: 5_000,
    };
    let mint_kp = Keypair::new();
    let ix = companion::create(
        launcher.pubkey(),
        launcher.pubkey(),
        mint_kp.pubkey(),
        CreateArgs {
            split,
            bounty_bps: 50,
            max_buyback: SOL,
            buyback_interval: 60,
            vest_secs: 86_400,
            fund: SOL / 2,
        },
    );
    w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]).ok();
    let mint = mint_kp.pubkey();
    let creator = companion::creator_address(&mint);
    let launch_args = World::launch_args("CMPF", 200, VQ, presets::plain());
    let inner = launch::create_launch_with(
        creator,
        mint,
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        launch_args.clone(),
        None,
        None,
    );
    let ix = companion::launch(launcher.pubkey(), mint, &inner, launch_args);
    w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]).ok();
    // Others buy the curve up to 1 SOL from the end within the dev-buy window.
    w.fill_curve(&mint, SOL);
    let held_before = w.env.holding(&w.sol, &creator);
    let k = LaunchKeys::of(&w.launch(&mint));
    let lamports = 8 * SOL;
    let beneficiary = w.env.funded(20 * SOL);
    // The beneficiary is the launcher.
    w.env
        .send_paid_by(
            &[token::create_holding(
                beneficiary.pubkey(),
                w.sol,
                beneficiary.pubkey(),
            )],
            &beneficiary,
            &[],
        )
        .ok();
    w.env.fund(launcher.pubkey(), 20 * SOL);
    let tx = w.env.send_paid_by(
        &[companion::dev_buy(launcher.pubkey(), &k, lamports, 1)],
        &launcher,
        &[],
    );
    // Fixed: the companion's buy is a CPI, never filled: refused past the end as before, and
    // nothing is left in the companion's creator holding to be split as fees.
    tx.expect_code(6016);
    assert_eq!(w.env.holding(&w.sol, &creator), held_before);
    let _ = std::any::type_name::<Swapped>();
}

// ------------------------------------------------------------------------------------------------
// 3. A third-party curve whose hook fills the whole input past the end.
// ------------------------------------------------------------------------------------------------

const SUPPLY: u64 = 1_000_000_000_000_000;
const DEPOSIT: u64 = 1_000_000_000_000;
const SWAP_FLAGS: u16 = pool_flags::BEFORE_SWAP
    | pool_flags::BEFORE_SWAP_RETURNS_DELTA
    | pool_flags::BEFORE_SWAP_OVERRIDES_FEE;

#[test]
fn fixed_a_third_party_curve_hook_cannot_fill() {
    let mut w = World::new();
    let owner = w.env.funded(100 * SOL);
    let victim = w.env.funded(100 * SOL);
    let early = w.env.funded(100 * SOL);
    let a = w.mint_to_owner(&owner, 6, SUPPLY, "AAA");
    let b = w.mint_to_owner(&owner, 9, SUPPLY, "BBB");
    for t in [victim.pubkey(), early.pubkey()] {
        w.holdings(&owner, a, &[t]);
        w.holdings(&owner, b, &[t]);
        w.send_tokens(&owner, b, &t, 100 * DEPOSIT).ok();
    }
    let (authority, _) = tester::hook_authority();
    let pool = swap::pool_address(&a, &b, 25, Some(hook_tester::ID));
    w.env
        .send_paid_by(
            &[tester::init_script(owner.pubkey(), pool, vec![])],
            &owner,
            &[],
        )
        .ok();
    let keys = swap::CreatePoolKeys {
        payer: owner.pubkey(),
        authority: owner.pubkey(),
        treasury: w.env.treasury.pubkey(),
        base_mint: a,
        quote_mint: b,
        hook_caller: Some(authority),
    };
    let args = CreatePoolArgs {
        lp_fee_bps: 25,
        hook_program: hook_tester::ID,
        hook_flags: SWAP_FLAGS,
        virtual_base: DEPOSIT,
        virtual_quote: 10 * DEPOSIT,
        base_amount: DEPOSIT,
        quote_amount: 0,
        base_hook_accounts: 0,
        quote_hook_accounts: 0,
        hook_data: vec![],
    };
    w.env
        .send_paid_by(
            &[tester::as_hook(swap::create_pool(&keys, args, vec![]))],
            &owner,
            &[],
        )
        .ok();
    let _ = NewPool {
        base: a,
        quote: b,
        lp_fee_bps: 0,
        tester_flags: None,
        extras: vec![],
        base_amount: 0,
        quote_amount: 0,
    };
    // The early holder buys half the curve with no answer.
    let tx = w.env.send_paid_by(
        &[w.env
            .swap_ix(&SwapSpec::new(early.pubkey(), pool, 1, 5 * DEPOSIT))],
        &early,
        &[],
    );
    tx.ok();
    let early_paid = 5 * DEPOSIT;
    let early_tokens = w.env.holding(&a, &early.pubkey());
    // The hook now answers a fill of the whole input; the victim buys 60 DEPOSIT with a minimum of
    // what is left (it thinks it buys the rest of the curve).
    let p: Pool = w.env.read(&pool);
    let huge = 60 * DEPOSIT;
    w.script_raw(
        &owner,
        pool,
        callback::BEFORE_SWAP,
        mode::RETURN,
        answer_bytes(&HookReturn::default(), Some(huge)),
    );
    let tx = w.env.send_paid_by(
        &[w.env.swap_ix(&SwapSpec {
            min_out: p.base_reserve,
            ..SwapSpec::new(victim.pubkey(), pool, 1, huge)
        })],
        &victim,
        &[],
    );
    // Fixed: only a launch pool's curve fills; this hook's fill is refused (BadFill, 6038) and the
    // victim keeps everything.
    tx.expect_code(6038);
    let _ = (
        early_paid,
        early_tokens,
        b,
        std::any::type_name::<Swapped>(),
    );
}

// ------------------------------------------------------------------------------------------------
// 4. Compute.
// ------------------------------------------------------------------------------------------------

fn cu_of_buy(w: &mut World, mint: &Pubkey, amount: u64) -> (u64, u64) {
    let t = w.wallet_with_sol(amount);
    w.holdings(&t, *mint, &[t.pubkey()]);
    let ix = w.launch_swap_ix(&t.pubkey(), mint, 1, amount, 0);
    let tx = w.env.send_paid_by(&[ix], &t, &[]);
    tx.ok();
    (tx.cu(), tx.event::<Swapped>().amount_in)
}

#[test]
fn compute_of_filled_buys_with_holder_rewards() {
    for amount in [SOL / 10, 5 * SOL, 1_000 * SOL, 1_000_000 * SOL] {
        let mut w = World::new();
        let creator = w.wallet_with_sol(20 * SOL);
        let (mint, tx) =
            w.create_launch_with(&creator, "KCU", 100, VQ, rules(100, 100, 0, 0, 0, 0, 0, 0));
        tx.ok();
        w.env.warp(policy::SNIPER_WINDOW_SECS + 1);
        w.fill_curve(&mint, SOL);
        let t = w.wallet_with_sol(amount);
        w.holdings(&t, mint, &[t.pubkey()]);
        let ixs = [
            w.launch_swap_ix(&t.pubkey(), &mint, 1, amount, 0),
            w.graduate_ix(&t.pubkey(), &mint),
        ];
        let tx = w.env.send_paid_by(&ixs[..1], &t, &[]);
        match &tx.result {
            Ok(_) => {
                let ev: Swapped = tx.event();
                println!(
                    "kit holder rewards: buy {amount} -> CU {} (used {})",
                    tx.cu(),
                    ev.amount_in
                );
            }
            Err(f) => println!("kit: buy {amount} -> failed {:?} CU {}", f.err, tx.cu()),
        }
        let _ = ixs;
    }
}

#[test]
fn compute_of_ordinary_and_filled_buys() {
    // The mainnet arm needs `solana program dump` of the DEX and launchpad in MAINNET_DIR.
    let have_mainnet = std::path::Path::new(&format!("{MAINNET_DIR}/swap_main.so")).exists();
    for (label, mainnet) in [("patched", false), ("mainnet", true)] {
        if mainnet && !have_mainnet {
            continue;
        }
        for amount in [SOL / 10, 5 * SOL, 1_000 * SOL, 1_000_000 * SOL] {
            let mut w = World::new();
            if mainnet {
                let s = std::fs::read(format!("{MAINNET_DIR}/swap_main.so")).unwrap();
                let l = std::fs::read(format!("{MAINNET_DIR}/launch_main.so")).unwrap();
                w.env.svm.add_program(bordrless_swap::ID, &s).unwrap();
                w.env.svm.add_program(bordrless_launch::ID, &l).unwrap();
            }
            let mint = near_the_end(&mut w, "CU", SOL);
            let t = w.wallet_with_sol(amount);
            w.holdings(&t, mint, &[t.pubkey()]);
            let ix = w.launch_swap_ix(&t.pubkey(), &mint, 1, amount, 0);
            let tx = w.env.send_paid_by(&[ix], &t, &[]);
            match &tx.result {
                Ok(_) => {
                    let ev: Swapped = tx.event();
                    println!(
                        "{label}: buy {amount} -> CU {} (used {})",
                        tx.cu(),
                        ev.amount_in
                    );
                }
                Err(f) => println!("{label}: buy {amount} -> failed {:?} CU {}", f.err, tx.cu()),
            }
        }
        // An ordinary buy far from the end, at the start of a curve.
        let mut w = World::new();
        if mainnet {
            let s = std::fs::read(format!("{MAINNET_DIR}/swap_main.so")).unwrap();
            let l = std::fs::read(format!("{MAINNET_DIR}/launch_main.so")).unwrap();
            w.env.svm.add_program(bordrless_swap::ID, &s).unwrap();
            w.env.svm.add_program(bordrless_launch::ID, &l).unwrap();
        }
        let creator = w.wallet_with_sol(20 * SOL);
        let (mint, tx) = w.create_launch_with(&creator, "CU2", 100, VQ, presets::plain());
        tx.ok();
        w.env.warp(policy::SNIPER_WINDOW_SECS + 1);
        let (cu, _) = cu_of_buy(&mut w, &mint, SOL / 10);
        println!("{label}: ordinary buy at the start of a curve -> CU {cu}");
    }
}

// ------------------------------------------------------------------------------------------------
// 5. Host-side property checks: the parser and the fill math.
// ------------------------------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

#[test]
fn a_plain_answer_parses_exactly_as_before_and_only_an_exact_tail_is_a_fill() {
    use anchor_lang::AnchorSerialize;
    use bordrless_hook::{
        answer_from, filled_answer_from, Allowed, AnswerError, Delta, Phase, PoolOp,
    };
    let hook = Pubkey::new_unique();
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let flag_sets = [
        pool_flags::ALL,
        pool_flags::BEFORE_SWAP | pool_flags::BEFORE_SWAP_OVERRIDES_FEE,
        pool_flags::BEFORE_SWAP | pool_flags::BEFORE_SWAP_RETURNS_DELTA,
        pool_flags::BEFORE_SWAP,
    ];
    let mut fills = 0;
    for i in 0..400_000u32 {
        // Half: a structured answer (possibly with a tail and mutations); half: random bytes.
        let mut data: Vec<u8> = if i % 2 == 0 {
            let n = rng.below(5) as usize;
            let ret = HookReturn {
                deltas: (0..n)
                    .map(|_| Delta {
                        amount: rng.below(3),
                        account: rng.below(8) as u8,
                    })
                    .collect(),
                burn: if rng.below(2) == 0 { 0 } else { rng.next() },
                lp_fee_bps: if rng.below(2) == 0 {
                    None
                } else {
                    Some(rng.below(20_000) as u16)
                },
                source_hook_data: if rng.below(4) == 0 {
                    Some([7; 64])
                } else {
                    None
                },
                destination_hook_data: None,
            };
            let mut v = Vec::new();
            ret.serialize(&mut v).unwrap();
            match rng.below(4) {
                0 => v.extend_from_slice(
                    &answer_bytes(&HookReturn::default(), Some(rng.next()))[..0],
                ),
                1 => {
                    v.push(1);
                    v.extend_from_slice(&rng.next().to_le_bytes());
                }
                2 => {
                    for _ in 0..rng.below(12) {
                        v.push(rng.below(3) as u8);
                    }
                }
                _ => {}
            }
            if rng.below(5) == 0 && !v.is_empty() {
                let at = rng.below(v.len() as u64) as usize;
                v[at] = rng.next() as u8;
            }
            v
        } else {
            (0..rng.below(200)).map(|_| rng.below(4) as u8).collect()
        };
        if rng.below(10) == 0 {
            data.truncate(rng.below(data.len() as u64 + 1) as usize);
        }
        for flags in flag_sets {
            let allowed = Allowed::pool(PoolOp::Swap, Phase::Before, flags);
            for program in [hook, Pubkey::new_unique()] {
                let old = answer_from(Some((program, data.clone())), &hook, allowed);
                let new = filled_answer_from(
                    Some((program, data.clone())),
                    &hook,
                    allowed,
                    allowed.deltas,
                );
                match (&old, &new) {
                    // A plain answer (or none, or a refusal for a reason other than its length)
                    // reads exactly as before.
                    (Ok(o), Ok(n)) => {
                        assert_eq!(o.clone().map(|(a, t)| (a, t, None)), n.clone(), "{data:?}");
                    }
                    (Err(AnswerError::Malformed), Ok(Some((_, _, Some(_))))) => {
                        // Only an exact [1, u64] tail after a valid answer, with deltas allowed.
                        assert!(allowed.deltas);
                        let n = data.len() - 9;
                        assert_eq!(data[n], 1);
                        assert!(HookReturn::try_from_slice_exact(&data[..n]));
                        fills += 1;
                    }
                    (Err(AnswerError::Malformed), Err(_)) => {}
                    (Err(a), Err(b)) => assert_eq!(a, b, "{data:?}"),
                    _ => panic!("divergence {old:?} vs {new:?} on {data:?} flags {flags}"),
                }
            }
        }
    }
    println!("fills accepted: {fills}");
}

trait Exact {
    fn try_from_slice_exact(b: &[u8]) -> bool;
}
impl Exact for HookReturn {
    fn try_from_slice_exact(b: &[u8]) -> bool {
        use anchor_lang::AnchorDeserialize;
        HookReturn::try_from_slice(b).is_ok()
    }
}

#[test]
fn the_fill_always_sells_out_and_overpays_by_dust_against_the_dex_math() {
    use bordrless_core::{
        launch_before_swap, launch_buy_net_in, launch_fill, quote_cut_fee, swap_amounts_shared,
        swap_amounts_shared_with, LaunchFeeRates, Reserves, SwapFailure,
    };
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    let (mut filled, mut none_but_past) = (0u32, 0u32);
    let mut worst_over = 0u64;
    for _ in 0..200_000 {
        let vb = 1 + rng.below(1 << 50);
        let vq = 1 + rng.below(1 << 40);
        let r = Reserves {
            base_reserve: rng.below(vb),
            quote_reserve: rng.below(1 << 40),
            virtual_base: vb,
            virtual_quote: vq,
        };
        let rates = LaunchFeeRates {
            creator_fee_bps: rng.below(1_001) as u16,
            holder_fee_buy_bps: rng.below(300) as u16,
            ..Default::default()
        };
        let (eligible, min_eligible) = (rng.below(3), rng.below(3));
        let lp = rng.below(5_001) as u16;
        let share = rng.below(10_001) as u16;
        let qc = rng.below(1_001) as u16;
        let amount = 1 + match rng.below(3) {
            0 => rng.below(1 << 20),
            1 => rng.below(1 << 40),
            _ => rng.below(u64::MAX),
        };
        // The DEX's view of a buy of `a` with this launch's answer.
        let dex = |a: u64, fill: bool| {
            let cut = launch_before_swap(true, a, &rates, eligible, min_eligible);
            let taken = cut.creator_fee + cut.holder_fee;
            let cuts = taken + quote_cut_fee(a, qc, taken);
            if cuts >= a {
                return Err(SwapFailure::FeesExceedInput);
            }
            let received = a - cuts;
            if fill {
                swap_amounts_shared_with(true, received, lp, share, cuts, &r, true)
            } else {
                swap_amounts_shared(true, received, lp, share, cuts, &r)
            }
        };
        match launch_fill(&r, amount, lp, share, &rates, eligible, min_eligible, qc) {
            Some(f) => {
                filled += 1;
                assert!(f > 0 && f <= amount);
                let a = dex(f, true).expect("a fill lands");
                assert_eq!(a.out_gross, r.base_reserve, "sells out");
                // Overpay: net into the curve beyond the least that sells out.
                let net =
                    launch_buy_net_in(f, lp, share, &rates, eligible, min_eligible, qc).unwrap();
                let x = u128::from(r.quote_reserve) + u128::from(r.virtual_quote);
                let y = u128::from(r.base_reserve) + u128::from(r.virtual_base);
                let left = u128::from(r.base_reserve);
                // least n with n*y/(x+n) >= left  <=> n >= left*x/(y-left)
                let least = if y > left {
                    (left * x).div_ceil(y - left)
                } else {
                    u128::MAX
                };
                let over = u128::from(net).saturating_sub(least) as u64;
                worst_over = worst_over.max(over);
            }
            None => {
                // No fill: the buy is the ordinary one; it is never one the DEX would cap.
                if dex(amount, false) == Err(SwapFailure::InsufficientLiquidity) {
                    if let Ok(a) = dex(amount, true) {
                        if a.out_gross == r.base_reserve && r.base_reserve > 0 {
                            none_but_past += 1;
                        }
                    }
                }
            }
        }
    }
    println!("filled {filled}, no fill but past the end {none_but_past}, worst overpay into the curve {worst_over} lamports");
    assert!(worst_over <= 4);
}
