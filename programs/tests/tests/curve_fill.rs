//! A buy past the end of the curve takes what is left (`docs/hooks-v2.md` §3.1, §5.4): the
//! launchpad's `before_swap` ends its answer with the fill, the smallest input that sells the curve
//! out, the DEX uses that much (its fees of that part), sells the last token, holds the minimum
//! out to pro rata and leaves the rest of the input with the trader, and `graduate` follows in the
//! same transaction; a buy short of the end is unchanged. The DEX's rules for a fill, with
//! `hook_tester` as the pool hook: only a buy, only on a curve, never zero or above the input,
//! never from `after_swap`, never malformed.

use anchor_lang::prelude::Pubkey;
use bordrless_core::policy;
use bordrless_hook::{answer_bytes, pool_flags, HookReturn};
use bordrless_launch::client as launch;
use bordrless_launch::constants::STATUS_GRADUATED;
use bordrless_launch::events::Graduated;
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::hooks::{NewPool, SwapSpec};
use bordrless_program_tests::launch::{expect_swapped, presets, rules, SOL, VQ};
use bordrless_swap::client as swap;
use bordrless_swap::error::SwapError;
use bordrless_swap::events::Swapped;
use bordrless_swap::instructions::CreatePoolArgs;
use bordrless_swap::state::Pool;
use bordrless_token::client as token;
use hook_tester::client as tester;
use hook_tester::{callback, mode};
use solana_keypair::Keypair;
use solana_signer::Signer;

fn code(e: SwapError) -> u32 {
    u32::from(e)
}

/// `amount * num / den`, rounded down (a minimum out for the whole input from the fill's).
fn scaled(amount: u64, num: u64, den: u64) -> u64 {
    u64::try_from(u128::from(amount) * u128::from(num) / u128::from(den)).unwrap()
}

/// A launch with `rules` and a 1% creator fee, past the sniper window, its curve bought up to
/// `leave` lamports from graduation.
fn near_the_end(
    w: &mut World,
    symbol: &str,
    rules: bordrless_launch::state::LaunchRules,
    leave: u64,
) -> (Keypair, Pubkey) {
    let creator = w.wallet_with_sol(20 * SOL);
    let (mint, tx) = w.create_launch_with(&creator, symbol, 100, VQ, rules);
    tx.ok();
    w.env.warp(policy::SNIPER_WINDOW_SECS + 1);
    w.fill_curve(&mint, leave);
    (creator, mint)
}

/// The buy of `amount` by `buyer` (its holdings made first), `graduate` appended, with `min_out`.
fn crossing_buy(w: &mut World, buyer: &Keypair, mint: &Pubkey, amount: u64, min_out: u64) -> Tx {
    let ixs = [
        token::create_holding(buyer.pubkey(), *mint, buyer.pubkey()),
        w.launch_swap_ix(&buyer.pubkey(), mint, 1, amount, min_out),
        w.graduate_ix(&buyer.pubkey(), mint),
    ];
    w.env.send_paid_by(&ixs, buyer, &[])
}

#[test]
fn a_buy_past_the_end_takes_what_is_left_and_graduates() {
    let mut w = World::new();
    let (_, mint) = near_the_end(&mut w, "FILL", presets::plain(), SOL);
    let before = w.launch_pool(&mint);
    assert!(before.curve && before.base_reserve > 0);
    let buyer = w.wallet_with_sol(10 * SOL);
    let asked = 5 * SOL;
    // Five SOL is more than the curve has left: the reference fills it.
    let (fill, q) = w
        .launch_fill_quote(&mint, &buyer.pubkey(), asked)
        .expect("a fill");
    assert!(fill < asked, "fill {fill} of {asked}");
    assert_eq!(w.launch_fill_quote(&mint, &buyer.pubkey(), fill - 1), None);
    assert_eq!(q.amount_out, Some(before.base_reserve));
    // The minimum out is for the whole input: what the fill delivers, scaled up to it, less 1%.
    let min_out = scaled(q.delivered.unwrap() * 99 / 100, asked, fill);
    let sol_before = w.env.holding(&w.sol, &buyer.pubkey());
    let tx = crossing_buy(&mut w, &buyer, &mint, asked, min_out);
    println!("filling buy + graduate CU {} size {}", tx.cu(), tx.size);
    tx.ok();
    // The swap used the fill, took its fees of the fill, and sold the last token.
    let ev: Swapped = tx.event();
    assert_eq!(ev.amount_in, fill);
    expect_swapped(&ev, &q, &w.launch(&mint), true, policy::LP_FEE_BPS);
    assert_eq!(ev.amount_out, before.base_reserve);
    assert_eq!(ev.delivered_out, q.delivered.unwrap());
    assert_eq!(w.env.holding(&mint, &buyer.pubkey()), q.delivered.unwrap());
    // The rest of the input never left the buyer.
    assert_eq!(w.env.holding(&w.sol, &buyer.pubkey()), sol_before - fill);
    // Graduated in the same transaction: the curve sold out exactly.
    let g: Graduated = tx.event();
    assert_eq!(g.mint, mint);
    let l = w.launch(&mint);
    assert_eq!(l.status, STATUS_GRADUATED);
    let p = w.launch_pool(&mint);
    assert!(!p.curve);
    assert_eq!(
        l.creator_fees_accrued,
        w.env.holding(&w.sol, &launch::launch_address(&mint))
    );
}

#[test]
fn the_minimum_out_of_a_filled_buy_is_held_to_pro_rata() {
    let mut w = World::new();
    let (_, mint) = near_the_end(&mut w, "PRRT", presets::plain(), SOL / 2);
    let buyer = w.wallet_with_sol(10 * SOL);
    let asked = 3 * SOL;
    let (fill, q) = w
        .launch_fill_quote(&mint, &buyer.pubkey(), asked)
        .expect("a fill");
    let delivered = q.delivered.unwrap();
    // The least accepted for the whole input, pro rata to the part used, must be met: a unit of
    // tokens over what the fill delivers (scaled up) fails, exactly it lands.
    let too_much = scaled(delivered + 1, asked, fill) + asked / fill;
    crossing_buy(&mut w, &buyer, &mint, asked, too_much).expect_code(code(SwapError::Slippage));
    assert!(w.launch_pool(&mint).curve, "nothing moved");
    let exact = scaled(delivered, asked, fill);
    let tx = crossing_buy(&mut w, &buyer, &mint, asked, exact);
    tx.ok();
    let ev: Swapped = tx.event();
    assert_eq!((ev.amount_in, ev.delivered_out), (fill, delivered));
}

#[test]
fn a_buy_short_of_the_end_and_a_buy_after_graduation_are_not_filled() {
    let mut w = World::new();
    let (_, mint) = near_the_end(&mut w, "NOFL", presets::diamond_hands(), 5 * SOL);
    let buyer = w.wallet_with_sol(100 * SOL);
    w.holdings(&buyer, mint, &[buyer.pubkey()]);
    // Short of the end: the whole input, as ever.
    let amount = SOL / 10;
    assert_eq!(w.launch_fill_quote(&mint, &buyer.pubkey(), amount), None);
    let q = w.launch_quote(&mint, &buyer.pubkey(), true, amount);
    let tx = w.buy(&buyer, &mint, amount);
    tx.ok();
    let ev: Swapped = tx.event();
    assert_eq!(ev.amount_in, amount);
    expect_swapped(&ev, &q, &w.launch(&mint), true, policy::LP_FEE_BPS);
    // Graduated: an ordinary pool, which fills any buy, so a large one is never filled short.
    let (_, tx) = w.graduate_launch(&mint);
    tx.ok();
    let p = w.launch_pool(&mint);
    assert!(!p.curve && p.virtual_base == 0);
    let amount = 3 * SOL;
    let tx = w.buy(&buyer, &mint, amount);
    tx.ok();
    let ev: Swapped = tx.event();
    assert_eq!(ev.amount_in, amount);
    assert!(w.launch_pool(&mint).base_reserve > 0);
}

#[test]
fn a_filled_buy_with_holder_rewards_pays_the_holder_fee_of_the_fill() {
    let mut w = World::new();
    // Holder rewards 1% on both sides, no cap.
    let (_, mint) = near_the_end(&mut w, "HOLD", rules(100, 100, 0, 0, 0, 0, 0, 0), SOL);
    let buyer = w.wallet_with_sol(10 * SOL);
    let asked = 4 * SOL;
    let (fill, q) = w
        .launch_fill_quote(&mint, &buyer.pubkey(), asked)
        .expect("a fill");
    let l0 = w.launch(&mint);
    let tx = crossing_buy(&mut w, &buyer, &mint, asked, 0);
    tx.ok();
    let ev: Swapped = tx.event();
    assert_eq!(ev.amount_in, fill);
    expect_swapped(&ev, &q, &w.launch(&mint), true, policy::LP_FEE_BPS);
    let l = w.launch(&mint);
    assert_eq!(
        l.creator_fees_accrued - l0.creator_fees_accrued,
        q.creator_fee
    );
    assert_eq!(l.holder_fees_accrued - l0.holder_fees_accrued, q.holder_fee);
    assert_eq!(l.status, STATUS_GRADUATED);
}

/// The DEX's rules for a fill, with `hook_tester` scripting `before_swap`.
struct Tester {
    w: World,
    owner: Keypair,
    trader: Keypair,
    a: Pubkey,
    b: Pubkey,
}

const SUPPLY: u64 = 1_000_000_000_000_000;
const DEPOSIT: u64 = 1_000_000_000_000;
const SWAP_FLAGS: u16 = pool_flags::BEFORE_SWAP
    | pool_flags::AFTER_SWAP
    | pool_flags::BEFORE_SWAP_RETURNS_DELTA
    | pool_flags::AFTER_SWAP_RETURNS_DELTA
    | pool_flags::BEFORE_SWAP_OVERRIDES_FEE;

impl Tester {
    fn new() -> Self {
        let mut w = World::new();
        let owner = w.env.funded(100 * SOL);
        let trader = w.env.funded(100 * SOL);
        let a = w.mint_to_owner(&owner, 6, SUPPLY, "AAA");
        let b = w.mint_to_owner(&owner, 9, SUPPLY, "BBB");
        let t = trader.pubkey();
        w.holdings(&owner, a, &[t]);
        w.holdings(&owner, b, &[t]);
        w.send_tokens(&owner, a, &t, 100 * DEPOSIT).ok();
        w.send_tokens(&owner, b, &t, 100 * DEPOSIT).ok();
        Self {
            w,
            owner,
            trader,
            a,
            b,
        }
    }

    /// An ordinary A/B pool with hook_tester as its hook.
    fn pool(&mut self) -> Pubkey {
        let owner = self.owner.insecure_clone();
        let spec = NewPool {
            base: self.a,
            quote: self.b,
            lp_fee_bps: 30,
            tester_flags: Some(SWAP_FLAGS),
            extras: vec![],
            base_amount: DEPOSIT,
            quote_amount: DEPOSIT,
        };
        let (pool, tx) = self.w.new_pool(&owner, &spec);
        tx.ok();
        pool
    }

    /// A curve hook_tester opens as its own hook (base only, both virtual reserves, so the end of
    /// the curve can be reached), its script made first so the registry resolves. At an LP fee of
    /// 25 bps, so its address is not the ordinary pool's.
    fn curve(&mut self) -> Pubkey {
        let owner = self.owner.insecure_clone();
        let (authority, _) = tester::hook_authority();
        let pool = swap::pool_address(&self.a, &self.b, 25, Some(hook_tester::ID));
        self.w
            .env
            .send_paid_by(
                &[tester::init_script(owner.pubkey(), pool, vec![])],
                &owner,
                &[],
            )
            .ok();
        let keys = swap::CreatePoolKeys {
            payer: owner.pubkey(),
            authority: owner.pubkey(),
            treasury: self.w.env.treasury.pubkey(),
            base_mint: self.a,
            quote_mint: self.b,
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
        let tx = self.w.env.send_paid_by(
            &[tester::as_hook(swap::create_pool(&keys, args, vec![]))],
            &owner,
            &[],
        );
        tx.ok();
        let p: Pool = self.w.env.read(&pool);
        assert!(p.curve);
        pool
    }

    /// Scripts `before_swap` of `pool` to answer `answer` with `fill` after it.
    fn before(&mut self, pool: Pubkey, answer: &HookReturn, fill: Option<u64>) {
        let owner = self.owner.insecure_clone();
        self.w.script_raw(
            &owner,
            pool,
            callback::BEFORE_SWAP,
            mode::RETURN,
            answer_bytes(answer, fill),
        );
    }

    fn raw(&mut self, pool: Pubkey, which: u8, bytes: Vec<u8>) {
        let owner = self.owner.insecure_clone();
        self.w.script_raw(&owner, pool, which, mode::RETURN, bytes);
    }

    fn swap(&mut self, spec: &SwapSpec) -> Tx {
        let ix = self.w.env.swap_ix(spec);
        let trader = self.trader.insecure_clone();
        self.w.env.send_paid_by(&[ix], &trader, &[])
    }

    fn bal(&self, mint: &Pubkey, owner: &Pubkey) -> u64 {
        self.w.env.holding(mint, owner)
    }
}

#[test]
fn the_dex_takes_a_fill_only_for_a_buy_on_a_curve() {
    let mut m = Tester::new();
    let t = m.trader.pubkey();
    let fee = HookReturn {
        lp_fee_bps: Some(30),
        ..HookReturn::default()
    };
    // An ordinary pool: a fill is refused however sound.
    let plain = m.pool();
    m.before(plain, &fee, Some(SOL / 2));
    m.swap(&SwapSpec::new(t, plain, 1, SOL))
        .expect_code(code(SwapError::BadFill));
    m.before(plain, &fee, None);
    m.swap(&SwapSpec::new(t, plain, 1, SOL)).ok();

    // A curve another program opens as its hook (not the launchpad's): no fill at all, however
    // sound (it could keep a buyer's whole input for the last tokens); a fill of zero, above the
    // input, or for a sell is refused too; the answer's tail must be exactly a fill; `after_swap`
    // never fills. The launchpad's fills are in the tests above.
    let curve = m.curve();
    for bad in [0, SOL / 2, SOL, SOL + 1, u64::MAX] {
        m.before(curve, &fee, Some(bad));
        m.swap(&SwapSpec::new(t, curve, 1, SOL))
            .expect_code(code(SwapError::BadFill));
    }
    m.before(curve, &fee, None);
    m.swap(&SwapSpec::new(t, curve, 1, SOL)).ok();
    m.before(curve, &fee, Some(SOL / 2));
    let p: Pool = m.w.env.read(&curve);
    let sell = p.base_reserve / 10;
    m.swap(&SwapSpec::new(t, curve, 0, sell))
        .expect_code(code(SwapError::BadFill));
    let mut short = answer_bytes(&fee, Some(SOL / 2));
    short.pop();
    m.raw(curve, callback::BEFORE_SWAP, short);
    m.swap(&SwapSpec::new(t, curve, 1, SOL))
        .expect_code(code(SwapError::InvalidHookReturn));
    let mut long = answer_bytes(&fee, Some(SOL / 2));
    long.push(0);
    m.raw(curve, callback::BEFORE_SWAP, long);
    m.swap(&SwapSpec::new(t, curve, 1, SOL))
        .expect_code(code(SwapError::InvalidHookReturn));
    m.before(curve, &fee, None);
    m.raw(
        curve,
        callback::AFTER_SWAP,
        answer_bytes(&HookReturn::default(), Some(1)),
    );
    m.swap(&SwapSpec::new(t, curve, 1, SOL))
        .expect_code(code(SwapError::InvalidHookReturn));
    m.raw(curve, callback::AFTER_SWAP, vec![]);
}

#[test]
fn a_curve_another_program_opens_never_fills() {
    // The overpay the audit showed (a hook answering a fill of the whole input past the end, the
    // rest kept in the pool for earlier holders) is refused: only a launch pool's curve fills.
    let mut m = Tester::new();
    let (a, b, t) = (m.a, m.b, m.trader.pubkey());
    let curve = m.curve();
    let fee = HookReturn {
        lp_fee_bps: Some(30),
        ..HookReturn::default()
    };
    let huge = 50 * DEPOSIT;
    m.before(curve, &fee, Some(huge));
    let (ta, tb) = (m.bal(&a, &t), m.bal(&b, &t));
    m.swap(&SwapSpec::new(t, curve, 1, huge))
        .expect_code(code(SwapError::BadFill));
    assert_eq!((m.bal(&a, &t), m.bal(&b, &t)), (ta, tb));
    // Without a fill the same buy is refused past the end, as ever.
    m.before(curve, &fee, None);
    m.swap(&SwapSpec::new(t, curve, 1, huge))
        .expect_code(code(SwapError::InsufficientLiquidity));
}
