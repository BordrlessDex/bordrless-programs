//! The DEX under hook protocol v2 (`docs/hooks-v2.md` §3), with the test-only `hook_tester` as the
//! pool hook (its answers scripted per callback) and `tax_hook` on a mint: deltas and burns on
//! both sides of buys and sells, the protocol fee always in the quote token, every rule a swap
//! answer must follow, protocol fees collected for whoever the fee collector is now, the recipient
//! every callback is told, and a swap routed by CPI.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use bordrless_core::{fee_amount, policy, swap_out};
use bordrless_hook::{pool_flags, Delta, ExtraAccount, HookReturn, Phase, PoolHookArgs, PoolOp};
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::hooks::{fixed, NewPool, SwapSpec};
use bordrless_swap::client as swap;
use bordrless_swap::error::SwapError;
use bordrless_swap::events::{DeltaPaid, ProtocolFeesCollected, Swapped};
use bordrless_swap::instructions::{AddLiquidityArgs, ConfigArgs, RemoveLiquidityArgs};
use bordrless_swap::state::Pool;
use bordrless_token::client as token;
use bordrless_token::error::TokenError;
use bordrless_token::state::Mint;
use hook_tester::client as tester;
use hook_tester::{callback, mode, Script, TesterError};
use solana_keypair::Keypair;
use solana_signer::Signer;

const SOL: u64 = 1_000_000_000;
/// Supply of each test mint.
const SUPPLY: u64 = 1_000_000_000_000_000;
/// Each side of a pool's first deposit.
const DEPOSIT: u64 = 1_000_000_000_000;
/// What the owner sends the trader of each mint.
const STAKE: u64 = 100_000_000_000;
/// tax_hook's rate on the taxed mint.
const TAX_BPS: u16 = 100;
/// The DEX's protocol fee (the world's config).
const PROTOCOL_BPS: u16 = policy::PROTOCOL_FEE_BPS;
/// Every swap callback, and every answer a swap callback may give.
const SWAP_FLAGS: u16 = pool_flags::BEFORE_SWAP
    | pool_flags::AFTER_SWAP
    | pool_flags::BEFORE_SWAP_RETURNS_DELTA
    | pool_flags::AFTER_SWAP_RETURNS_DELTA
    | pool_flags::BEFORE_SWAP_OVERRIDES_FEE;
/// Index of the first test extra in a hook_tester callback: the prefix (5), then the script.
const X: u8 = 6;

fn code(e: SwapError) -> u32 {
    u32::from(e)
}

fn h(mint: &Pubkey, owner: &Pubkey) -> Pubkey {
    token::holding_address(mint, owner)
}

/// An answer of deltas (amount, account index) and a burn.
fn cut(deltas: &[(u64, u8)], burn: u64) -> HookReturn {
    HookReturn {
        deltas: deltas
            .iter()
            .map(|&(amount, account)| Delta { amount, account })
            .collect(),
        burn,
        ..HookReturn::default()
    }
}

/// An answer that sets the LP fee only.
fn fee(bps: u16) -> HookReturn {
    HookReturn {
        lp_fee_bps: Some(bps),
        ..HookReturn::default()
    }
}

fn paid(list: &[(Pubkey, u64)]) -> Vec<DeltaPaid> {
    list.iter()
        .map(|&(holding, amount)| DeltaPaid { holding, amount })
        .collect()
}

/// What arrives of a transfer of `amount` of a taxed mint.
fn taxed(amount: u64) -> u64 {
    amount - tax(amount)
}

/// The tax on a transfer of `amount` of a taxed mint.
fn tax(amount: u64) -> u64 {
    fee_amount(amount, TAX_BPS).unwrap()
}

/// The fees and the curve of a swap whose input vault received `received`, step by step in the
/// order of §3.1 (computed here, independently of `bordrless_core::swap_amounts`): the LP fee, the
/// protocol fee (in quote), what the curve gives, and what the output side hands on (the curve's
/// output, less a sell's protocol fee).
fn expect(buy: bool, received: u64, lp_fee_bps: u16, p: &Pool) -> (u64, u64, u64, u64) {
    let lp_fee = fee_amount(received, lp_fee_bps).unwrap();
    if buy {
        let protocol_fee = fee_amount(received, PROTOCOL_BPS).unwrap();
        let out = swap_out(
            received - lp_fee - protocol_fee,
            p.quote_reserve,
            p.virtual_quote,
            p.base_reserve,
            p.virtual_base,
        )
        .unwrap();
        (lp_fee, protocol_fee, out, out)
    } else {
        let out = swap_out(
            received - lp_fee,
            p.base_reserve,
            p.virtual_base,
            p.quote_reserve,
            p.virtual_quote,
        )
        .unwrap();
        let protocol_fee = fee_amount(out, PROTOCOL_BPS).unwrap();
        (lp_fee, protocol_fee, out, out - protocol_fee)
    }
}

/// A taxed base mint A (`tax_hook`: 1% of every transfer to `taxman`), a plain quote mint B, their
/// owner, a trader holding both, and three wallets holding each that a hook can pay.
struct Market {
    w: World,
    owner: Keypair,
    trader: Keypair,
    a: Pubkey,
    b: Pubkey,
    taxman: Pubkey,
    ra: [Pubkey; 3],
    rb: [Pubkey; 3],
}

impl Market {
    fn new() -> Self {
        let mut w = World::new();
        let owner = w.env.funded(100 * SOL);
        let trader = w.env.funded(100 * SOL);
        let taxman = Pubkey::new_unique();
        let a = w.tax_mint(&owner, 6, SUPPLY, "AAA", taxman, TAX_BPS);
        let b = w.mint_to_owner(&owner, 9, SUPPLY, "BBB");
        let ra = [
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        let rb = [
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        let t = trader.pubkey();
        w.holdings(&owner, a, &[t, ra[0], ra[1], ra[2]]);
        w.holdings(&owner, b, &[t, rb[0], rb[1], rb[2]]);
        w.send_tokens(&owner, a, &t, STAKE).ok();
        w.send_tokens(&owner, b, &t, STAKE).ok();
        Self {
            w,
            owner,
            trader,
            a,
            b,
            taxman,
            ra,
            rb,
        }
    }

    /// An A/B pool with the owner's first deposit; with `flags`, hook_tester is its hook and the
    /// registry lists the script, then `extras`.
    fn pool(&mut self, lp_fee_bps: u16, flags: Option<u16>, extras: Vec<ExtraAccount>) -> Pubkey {
        let owner = self.owner.insecure_clone();
        let spec = NewPool {
            base: self.a,
            quote: self.b,
            lp_fee_bps,
            tester_flags: flags,
            extras,
            base_amount: DEPOSIT,
            quote_amount: DEPOSIT,
        };
        let (pool, tx) = self.w.new_pool(&owner, &spec);
        tx.ok();
        pool
    }

    fn bal(&self, mint: &Pubkey, owner: &Pubkey) -> u64 {
        self.w.env.holding(mint, owner)
    }

    fn supply(&self, mint: &Pubkey) -> u64 {
        self.w.env.read::<Mint>(mint).supply
    }

    fn state(&self, pool: &Pubkey) -> Pool {
        self.w.env.read(pool)
    }

    fn script(&self, pool: &Pubkey) -> Script {
        self.w.env.read(&tester::script_address(pool))
    }

    /// Scripts what a callback of `pool`'s hook answers.
    fn answer(&mut self, pool: Pubkey, which: u8, answer: &HookReturn) {
        let owner = self.owner.insecure_clone();
        self.w.script_answer(&owner, pool, which, answer);
    }

    /// Scripts a callback of `pool`'s hook to return raw bytes, nothing or a failure.
    fn raw(&mut self, pool: Pubkey, which: u8, action: u8, data: Vec<u8>) {
        let owner = self.owner.insecure_clone();
        self.w.script_raw(&owner, pool, which, action, data);
    }

    /// Scripts a callback of `pool`'s hook to answer nothing.
    fn silence(&mut self, pool: Pubkey, which: u8) {
        self.raw(pool, which, mode::NONE, vec![]);
    }

    /// The trader's swap.
    fn swap(&mut self, spec: &SwapSpec) -> Tx {
        let ix = self.w.env.swap_ix(spec);
        let trader = self.trader.insecure_clone();
        self.w.env.send_paid_by(&[ix], &trader, &[])
    }

    /// The balances the refused answers of a pool must leave alone.
    fn watched(&self, pool: &Pubkey) -> Vec<u64> {
        let (a, b, t) = (self.a, self.b, self.trader.pubkey());
        let mut all = vec![
            self.bal(&a, &t),
            self.bal(&b, &t),
            self.bal(&a, pool),
            self.bal(&b, pool),
            self.bal(&a, &self.taxman),
            self.supply(&a),
            self.supply(&b),
        ];
        all.extend(self.ra.map(|o| self.bal(&a, &o)));
        all.extend(self.rb.map(|o| self.bal(&b, &o)));
        let p = self.state(pool);
        all.extend([
            p.base_reserve,
            p.quote_reserve,
            p.protocol_fees_quote,
            p.swap_count,
        ]);
        all
    }
}

#[test]
fn a_pool_hook_cuts_both_sides_of_buys_and_sells() {
    let mut m = Market::new();
    let (a, b, t) = (m.a, m.b, m.trader.pubkey());
    let (ha, hb) = (m.ra.map(|o| h(&a, &o)), m.rb.map(|o| h(&b, &o)));
    // The registry after the script (5): three A holdings (6, 7, 8), three B holdings (9, 10, 11).
    let mut extras: Vec<ExtraAccount> = ha.map(|k| fixed(k, true)).to_vec();
    extras.extend(hb.map(|k| fixed(k, true)));
    let pool = m.pool(30, Some(SWAP_FLAGS), extras);
    let p = m.state(&pool);
    // The taxed mint's first deposit arrived less its tax.
    assert_eq!(
        (p.base_reserve, p.quote_reserve, p.protocol_fees_quote),
        (taxed(DEPOSIT), DEPOSIT, 0)
    );

    // A buy of A with 1 B. Before: three deltas and a burn from the B input, and an LP fee of 0.5%
    // instead of the pool's 0.3%. After: three deltas and a burn from the A output.
    let amount_in = SOL;
    m.answer(
        pool,
        callback::BEFORE_SWAP,
        &HookReturn {
            lp_fee_bps: Some(50),
            ..cut(&[(1_000, X + 3), (2_000, X + 4), (3_000, X + 5)], 4_000)
        },
    );
    m.answer(
        pool,
        callback::AFTER_SWAP,
        &cut(&[(100, X), (200, X + 1), (300, X + 2)], 400),
    );
    let p0 = m.state(&pool);
    let received = amount_in - 10_000;
    let (lp_fee, protocol_fee, out_gross, amount_out) = expect(true, received, 50, &p0);
    assert_eq!(amount_out, out_gross);
    // The rest of the output, then the taxed mint's tax on its way to the trader.
    let delivered = taxed(amount_out - 1_000);
    let (a_supply, b_supply) = (m.supply(&a), m.supply(&b));
    let (t_a, t_b, taxman_a) = (m.bal(&a, &t), m.bal(&b, &t), m.bal(&a, &m.taxman));
    let (vault_a, vault_b) = (m.bal(&a, &pool), m.bal(&b, &pool));

    // The minimum is checked against what the trader's holding gains: one unit more fails, and
    // nothing moves.
    let spec = SwapSpec {
        min_out: delivered + 1,
        ..SwapSpec::new(t, pool, 1, amount_in).burnable()
    };
    m.swap(&spec).expect_code(code(SwapError::Slippage));
    assert_eq!(
        (m.bal(&b, &t), m.bal(&a, &pool), m.supply(&b), m.supply(&a)),
        (t_b, vault_a, b_supply, a_supply)
    );
    let tx = m.swap(&SwapSpec {
        min_out: delivered,
        ..spec
    });
    tx.ok();
    println!(
        "buy, 3 deltas + burn each side, taxed base (hook_tester) CU {} size {} trace {} height {}",
        tx.cu(),
        tx.size,
        tx.trace_len(),
        tx.max_height()
    );
    // The input: the trader paid it all; the hook's holdings got their deltas; the burn left the
    // supply; the rest reached the vault.
    assert_eq!(m.bal(&b, &t), t_b - amount_in);
    assert_eq!(m.rb.map(|o| m.bal(&b, &o)), [1_000, 2_000, 3_000]);
    assert_eq!(m.supply(&b), b_supply - 4_000);
    assert_eq!(m.bal(&b, &pool), vault_b + received);
    // The output: the curve's whole output left the vault: the deltas (each taxed on its way), the
    // burn, then the rest to the trader.
    assert_eq!(m.bal(&a, &pool), vault_a - out_gross);
    assert_eq!(
        m.ra.map(|o| m.bal(&a, &o)),
        [taxed(100), taxed(200), taxed(300)]
    );
    assert_eq!(m.supply(&a), a_supply - 400);
    assert_eq!(m.bal(&a, &t), t_a + delivered);
    assert_eq!(
        m.bal(&a, &m.taxman),
        taxman_a + tax(100) + tax(200) + tax(300) + tax(amount_out - 1_000)
    );
    // Reserves: what reached the vault less the protocol fee came in; the curve's output left; the
    // protocol fee, in B, is set aside in the B vault.
    let p1 = m.state(&pool);
    assert_eq!(
        (
            p1.quote_reserve,
            p1.base_reserve,
            p1.protocol_fees_quote,
            p1.swap_count
        ),
        (
            p0.quote_reserve + received - protocol_fee,
            p0.base_reserve - out_gross,
            p0.protocol_fees_quote + protocol_fee,
            p0.swap_count + 1
        )
    );
    assert_eq!(m.bal(&a, &pool), p1.base_reserve);
    assert_eq!(m.bal(&b, &pool), p1.quote_reserve + p1.protocol_fees_quote);
    // The event carries every cut with its destination.
    let ev: Swapped = tx.event();
    assert_eq!(
        (ev.pool, ev.trader, ev.recipient, ev.direction, ev.amount_in),
        (pool, t, t, 1, amount_in)
    );
    assert_eq!(
        ev.deltas_in,
        paid(&[(hb[0], 1_000), (hb[1], 2_000), (hb[2], 3_000)])
    );
    assert_eq!(
        (
            ev.burn_in,
            ev.received_in,
            ev.lp_fee,
            ev.protocol_fee,
            ev.lp_fee_bps
        ),
        (4_000, received, lp_fee, protocol_fee, 50)
    );
    assert_eq!(
        ev.deltas_out,
        paid(&[(ha[0], 100), (ha[1], 200), (ha[2], 300)])
    );
    assert_eq!(
        (ev.amount_out, ev.burn_out, ev.delivered_out),
        (out_gross, 400, delivered)
    );
    assert_eq!(
        (ev.base_reserve, ev.quote_reserve, ev.swap_count),
        (p1.base_reserve, p1.quote_reserve, p1.swap_count)
    );
    // What the hook was told: before, the trader's input and the pool as it stood; after, what
    // reached the vault, the curve's output, the fee applied and the pool as it now stands. Both
    // name the recipient.
    let s = m.script(&pool);
    let mut told = PoolHookArgs {
        op: PoolOp::Swap,
        phase: Phase::Before,
        pool,
        base_mint: a,
        quote_mint: b,
        actor: t,
        recipient: t,
        direction: 1,
        amount_in,
        amount_out: 0,
        base_reserve: p0.base_reserve,
        quote_reserve: p0.quote_reserve,
        virtual_base: 0,
        virtual_quote: 0,
        lp_fee_bps: 30,
        protocol_fee_bps: PROTOCOL_BPS,
        swap_count: p0.swap_count,
        created_at: p0.created_at,
        lp_amount: 0,
        hook_data: vec![],
    };
    assert_eq!(s.told_pool(callback::BEFORE_SWAP).unwrap(), told);
    told.phase = Phase::After;
    told.amount_in = received;
    told.amount_out = amount_out;
    told.lp_fee_bps = 50;
    told.base_reserve = p1.base_reserve;
    told.quote_reserve = p1.quote_reserve;
    assert_eq!(s.told_pool(callback::AFTER_SWAP).unwrap(), told);

    // A sell of 1,000 A. Before: three deltas (each taxed on its way) and a burn from the A input,
    // and an LP fee of 0.7%. After: three deltas and a burn from the B output that is left once
    // the protocol fee, in B, is set aside.
    let amount_in = 1_000_000_000;
    m.answer(
        pool,
        callback::BEFORE_SWAP,
        &HookReturn {
            lp_fee_bps: Some(70),
            ..cut(&[(1_000, X), (2_000, X + 1), (3_000, X + 2)], 4_000)
        },
    );
    m.answer(
        pool,
        callback::AFTER_SWAP,
        &cut(&[(100, X + 3), (200, X + 4), (300, X + 5)], 400),
    );
    let p0 = m.state(&pool);
    let sent = amount_in - 10_000;
    let received = taxed(sent);
    let (lp_fee, protocol_fee, out_gross, amount_out) = expect(false, received, 70, &p0);
    assert!(protocol_fee > 0 && amount_out == out_gross - protocol_fee);
    let delivered = amount_out - 1_000;
    let (a_supply, b_supply) = (m.supply(&a), m.supply(&b));
    let (t_a, t_b, taxman_a) = (m.bal(&a, &t), m.bal(&b, &t), m.bal(&a, &m.taxman));
    let (vault_a, vault_b) = (m.bal(&a, &pool), m.bal(&b, &pool));
    let (ra0, rb0) = (m.ra.map(|o| m.bal(&a, &o)), m.rb.map(|o| m.bal(&b, &o)));
    // Each burn needs its mint passed writable.
    m.swap(&SwapSpec {
        quote_mint_writable: true,
        ..SwapSpec::new(t, pool, 0, amount_in)
    })
    .expect_code(code(SwapError::MintNotWritable));
    m.swap(&SwapSpec {
        base_mint_writable: true,
        ..SwapSpec::new(t, pool, 0, amount_in)
    })
    .expect_code(code(SwapError::MintNotWritable));
    let spec = SwapSpec {
        min_out: delivered + 1,
        ..SwapSpec::new(t, pool, 0, amount_in).burnable()
    };
    m.swap(&spec).expect_code(code(SwapError::Slippage));
    assert_eq!(m.watched(&pool)[..2], [t_a, t_b]);
    let tx = m.swap(&SwapSpec {
        min_out: delivered,
        ..spec
    });
    tx.ok();
    println!(
        "sell, 3 deltas + burn each side, taxed base (hook_tester) CU {} size {} trace {} height {}",
        tx.cu(),
        tx.size,
        tx.trace_len(),
        tx.max_height()
    );
    assert_eq!(m.bal(&a, &t), t_a - amount_in);
    assert_eq!(
        m.ra.map(|o| m.bal(&a, &o)),
        [
            ra0[0] + taxed(1_000),
            ra0[1] + taxed(2_000),
            ra0[2] + taxed(3_000)
        ]
    );
    assert_eq!(m.supply(&a), a_supply - 4_000);
    assert_eq!(m.bal(&a, &pool), vault_a + received);
    assert_eq!(
        m.bal(&a, &m.taxman),
        taxman_a + tax(1_000) + tax(2_000) + tax(3_000) + tax(sent)
    );
    // The protocol fee stays in the B vault; what after_swap was told leaves it.
    assert_eq!(m.bal(&b, &pool), vault_b - amount_out);
    assert_eq!(
        m.rb.map(|o| m.bal(&b, &o)),
        [rb0[0] + 100, rb0[1] + 200, rb0[2] + 300]
    );
    assert_eq!(m.supply(&b), b_supply - 400);
    assert_eq!(m.bal(&b, &t), t_b + delivered);
    let p1 = m.state(&pool);
    assert_eq!(
        (p1.base_reserve, p1.quote_reserve, p1.protocol_fees_quote),
        (
            p0.base_reserve + received,
            p0.quote_reserve - out_gross,
            p0.protocol_fees_quote + protocol_fee
        )
    );
    assert_eq!(m.bal(&a, &pool), p1.base_reserve);
    assert_eq!(m.bal(&b, &pool), p1.quote_reserve + p1.protocol_fees_quote);
    let ev: Swapped = tx.event();
    assert_eq!(
        ev.deltas_in,
        paid(&[(ha[0], 1_000), (ha[1], 2_000), (ha[2], 3_000)])
    );
    assert_eq!(
        (
            ev.direction,
            ev.burn_in,
            ev.received_in,
            ev.lp_fee,
            ev.lp_fee_bps,
            ev.protocol_fee
        ),
        (0, 4_000, received, lp_fee, 70, protocol_fee)
    );
    assert_eq!(
        ev.deltas_out,
        paid(&[(hb[0], 100), (hb[1], 200), (hb[2], 300)])
    );
    assert_eq!(
        (ev.amount_out, ev.burn_out, ev.delivered_out),
        (out_gross, 400, delivered)
    );
    let s = m.script(&pool);
    let before = s.told_pool(callback::BEFORE_SWAP).unwrap();
    assert_eq!(
        (before.direction, before.amount_in, before.recipient),
        (0, amount_in, t)
    );
    let after = s.told_pool(callback::AFTER_SWAP).unwrap();
    // after_swap is told the output less the protocol fee.
    assert_eq!(
        (
            after.amount_in,
            after.amount_out,
            after.lp_fee_bps,
            after.base_reserve,
            after.quote_reserve
        ),
        (received, amount_out, 70, p1.base_reserve, p1.quote_reserve)
    );

    // The output can go to another wallet's holding; the hook is told that wallet.
    let carol = Pubkey::new_unique();
    m.w.holdings(&m.owner, a, &[carol]);
    m.silence(pool, callback::BEFORE_SWAP);
    m.silence(pool, callback::AFTER_SWAP);
    let t_a = m.bal(&a, &t);
    let tx = m.swap(&SwapSpec {
        recipient: carol,
        ..SwapSpec::new(t, pool, 1, SOL)
    });
    tx.ok();
    let ev: Swapped = tx.event();
    assert_eq!(
        (ev.recipient, m.bal(&a, &carol), m.bal(&a, &t)),
        (carol, ev.delivered_out, t_a)
    );
    assert!(ev.deltas_in.is_empty() && ev.deltas_out.is_empty());
    let s = m.script(&pool);
    for which in [callback::BEFORE_SWAP, callback::AFTER_SWAP] {
        let told = s.told_pool(which).unwrap();
        assert_eq!((told.actor, told.recipient), (t, carol));
    }
}

#[test]
fn the_dex_checks_every_swap_answer() {
    let mut m = Market::new();
    let (a, b, t) = (m.a, m.b, m.trader.pubkey());
    let (ro, ra3) = (Pubkey::new_unique(), Pubkey::new_unique());
    m.w.holdings(&m.owner, a, &[ro, ra3]);
    let pool = swap::pool_address(&a, &b, 31, Some(hook_tester::ID));
    // The registry after the script (5): an A and a B holding (6, 7), the A and B vaults (8, 9),
    // the trader's A and B holdings (10, 11), the A holding of 6 again (12), a read-only A holding
    // (13), three more A holdings (14, 15, 16).
    let extras = vec![
        fixed(h(&a, &m.ra[0]), true),
        fixed(h(&b, &m.rb[0]), true),
        fixed(swap::vault_address(&pool, &a), true),
        fixed(swap::vault_address(&pool, &b), true),
        fixed(h(&a, &t), true),
        fixed(h(&b, &t), true),
        fixed(h(&a, &m.ra[0]), true),
        fixed(h(&a, &ro), false),
        fixed(h(&a, &m.ra[1]), true),
        fixed(h(&a, &m.ra[2]), true),
        fixed(h(&a, &ra3), true),
    ];
    assert_eq!(m.pool(31, Some(SWAP_FLAGS), extras), pool);
    let start = m.watched(&pool);
    let buy = SwapSpec::new(t, pool, 1, SOL).burnable();
    let p0 = m.state(&pool);
    let (_, _, out, _) = expect(true, SOL, 31, &p0);

    // before_swap cuts the input, B on a buy.
    let refused = [
        // The deltas and the burn must leave some of the input: all of it, more, or a sum that
        // overflows is too large.
        (cut(&[(SOL - 1, X + 1)], 1), SwapError::DeltaTooLarge),
        (cut(&[(SOL, X + 1)], 0), SwapError::DeltaTooLarge),
        (cut(&[], SOL), SwapError::DeltaTooLarge),
        (cut(&[(u64::MAX, X + 1)], 1), SwapError::DeltaTooLarge),
        // Each delta goes to a writable holding of the input's mint that is neither vault nor
        // either of the trader's holdings, named once.
        (cut(&[(10, X)], 0), SwapError::InvalidDeltaAccount),
        (cut(&[(10, X + 3)], 0), SwapError::InvalidDeltaAccount),
        (cut(&[(10, X + 5)], 0), SwapError::InvalidDeltaAccount),
        (
            cut(&[(10, X + 1), (10, X + 1)], 0),
            SwapError::InvalidDeltaAccount,
        ),
        (cut(&[(10, 4)], 0), SwapError::InvalidDeltaAccount),
        (cut(&[(10, 5)], 0), SwapError::InvalidDeltaAccount),
        (cut(&[(10, 17)], 0), SwapError::InvalidDeltaAccount),
        // At most three deltas, each above zero.
        (
            cut(&[(1, X + 1), (1, X + 1), (1, X + 1), (1, X + 1)], 0),
            SwapError::TooManyDeltas,
        ),
        (cut(&[(0, X + 1)], 0), SwapError::ZeroDelta),
        // The LP fee is bounded.
        (fee(9_001), SwapError::FeeTooHigh),
    ];
    for (answer, error) in refused {
        m.answer(pool, callback::BEFORE_SWAP, &answer);
        let tx = m.swap(&buy);
        assert_eq!(
            tx.custom(),
            code(error),
            "before_swap {answer:?}\n{}",
            tx.logs().join("\n")
        );
    }
    m.silence(pool, callback::BEFORE_SWAP);

    // after_swap cuts the output, A on a buy.
    let refused = [
        // A B holding, the A vault, the trader's A holding (where the output goes), one holding
        // at two indices, a read-only holding.
        (cut(&[(10, X + 1)], 0), SwapError::InvalidDeltaAccount),
        (cut(&[(10, X + 2)], 0), SwapError::InvalidDeltaAccount),
        (cut(&[(10, X + 4)], 0), SwapError::InvalidDeltaAccount),
        (
            cut(&[(10, X), (10, X + 6)], 0),
            SwapError::InvalidDeltaAccount,
        ),
        (cut(&[(10, X + 7)], 0), SwapError::InvalidDeltaAccount),
        (
            cut(&[(1, X), (1, X + 8), (1, X + 9), (1, X + 10)], 0),
            SwapError::TooManyDeltas,
        ),
        // Nothing would be left for the trader.
        (cut(&[(out, X)], 0), SwapError::DeltaTooLarge),
        (cut(&[(out - 1, X)], 1), SwapError::DeltaTooLarge),
        // after_swap never sets the fee.
        (fee(30), SwapError::UnsupportedHookReturn),
    ];
    for (answer, error) in refused {
        m.answer(pool, callback::AFTER_SWAP, &answer);
        let tx = m.swap(&buy);
        assert_eq!(
            tx.custom(),
            code(error),
            "after_swap {answer:?}\n{}",
            tx.logs().join("\n")
        );
    }
    m.silence(pool, callback::AFTER_SWAP);

    // A burn needs its mint passed writable, on either side.
    m.answer(pool, callback::BEFORE_SWAP, &cut(&[], 10));
    m.swap(&SwapSpec {
        quote_mint_writable: false,
        ..buy.clone()
    })
    .expect_code(code(SwapError::MintNotWritable));
    m.silence(pool, callback::BEFORE_SWAP);
    m.answer(pool, callback::AFTER_SWAP, &cut(&[], 10));
    m.swap(&SwapSpec {
        base_mint_writable: false,
        ..buy.clone()
    })
    .expect_code(code(SwapError::MintNotWritable));
    m.silence(pool, callback::AFTER_SWAP);

    // An answer that does not decode, and a hook that refuses.
    m.raw(pool, callback::BEFORE_SWAP, mode::RETURN, vec![1, 2, 3]);
    m.swap(&buy).expect_code(code(SwapError::InvalidHookReturn));
    m.silence(pool, callback::BEFORE_SWAP);
    m.raw(pool, callback::AFTER_SWAP, mode::FAIL, vec![]);
    m.swap(&buy).expect_code(u32::from(TesterError::Refused));
    m.silence(pool, callback::AFTER_SWAP);
    // None of it moved anything.
    assert_eq!(m.watched(&pool), start);

    // On a sell, after_swap is told the output less the protocol fee. Taking all of that is too
    // much; taking all but one unit leaves the trader exactly one.
    let amount_in = 1_000_000_000;
    let p0 = m.state(&pool);
    let (_, protocol_fee, _, told) = expect(false, taxed(amount_in), 31, &p0);
    m.answer(pool, callback::AFTER_SWAP, &cut(&[(told, X + 1)], 0));
    let sell = SwapSpec {
        quote_mint_writable: true,
        ..SwapSpec::new(t, pool, 0, amount_in)
    };
    m.swap(&sell).expect_code(code(SwapError::DeltaTooLarge));
    m.answer(pool, callback::AFTER_SWAP, &cut(&[(told - 2, X + 1)], 1));
    let (t_b, rb) = (m.bal(&b, &t), m.bal(&b, &m.rb[0]));
    let tx = m.swap(&sell);
    tx.ok();
    assert_eq!(
        (m.bal(&b, &t), m.bal(&b, &m.rb[0])),
        (t_b + 1, rb + told - 2)
    );
    let ev: Swapped = tx.event();
    assert_eq!(
        (ev.protocol_fee, ev.delivered_out, ev.burn_out),
        (protocol_fee, 1, 1)
    );
    assert_eq!(
        m.script(&pool)
            .told_pool(callback::AFTER_SWAP)
            .unwrap()
            .amount_out,
        told
    );
}

#[test]
fn a_pool_answers_only_what_its_flags_allow() {
    let mut m = Market::new();
    let (a, b, t) = (m.a, m.b, m.trader.pubkey());
    // A hook that may set the LP fee and nothing else.
    let flags =
        pool_flags::BEFORE_SWAP | pool_flags::AFTER_SWAP | pool_flags::BEFORE_SWAP_OVERRIDES_FEE;
    let fee_only = m.pool(
        32,
        Some(flags),
        vec![fixed(h(&b, &m.rb[0]), true), fixed(h(&a, &m.ra[0]), true)],
    );
    let buy = SwapSpec::new(t, fee_only, 1, SOL).burnable();
    m.answer(fee_only, callback::BEFORE_SWAP, &cut(&[(10, X)], 0));
    m.swap(&buy)
        .expect_code(code(SwapError::UnsupportedHookReturn));
    m.answer(fee_only, callback::BEFORE_SWAP, &cut(&[], 10));
    m.swap(&buy)
        .expect_code(code(SwapError::UnsupportedHookReturn));
    // The fee is applied; whatever after_swap answers is never read.
    m.answer(fee_only, callback::BEFORE_SWAP, &fee(100));
    m.answer(fee_only, callback::AFTER_SWAP, &cut(&[(10, X + 1)], 10));
    let tx = m.swap(&buy);
    tx.ok();
    let ev: Swapped = tx.event();
    assert_eq!(
        (ev.lp_fee_bps, ev.lp_fee),
        (100, fee_amount(SOL, 100).unwrap())
    );
    assert!(ev.deltas_out.is_empty() && ev.burn_out == 0);
    assert_eq!(m.bal(&a, &m.ra[0]), 0);

    // A hook that may cut the input but not set the fee.
    let cut_only = m.pool(
        33,
        Some(pool_flags::BEFORE_SWAP | pool_flags::BEFORE_SWAP_RETURNS_DELTA),
        vec![fixed(h(&b, &m.rb[0]), true)],
    );
    m.answer(cut_only, callback::BEFORE_SWAP, &fee(100));
    m.swap(&SwapSpec::new(t, cut_only, 1, SOL))
        .expect_code(code(SwapError::UnsupportedHookReturn));
    m.answer(cut_only, callback::BEFORE_SWAP, &cut(&[(10, X)], 0));
    let tx = m.swap(&SwapSpec::new(t, cut_only, 1, SOL));
    tx.ok();
    let ev: Swapped = tx.event();
    assert_eq!(ev.deltas_in, paid(&[(h(&b, &m.rb[0]), 10)]));
    assert_eq!(ev.lp_fee_bps, 33);

    // A hook that only listens: nothing it returns is read, not even bytes that do not decode.
    let listens = m.pool(
        34,
        Some(pool_flags::BEFORE_SWAP | pool_flags::AFTER_SWAP),
        vec![],
    );
    m.raw(listens, callback::BEFORE_SWAP, mode::RETURN, vec![9, 9, 9]);
    m.raw(listens, callback::AFTER_SWAP, mode::RETURN, vec![9, 9, 9]);
    let tx = m.swap(&SwapSpec::new(t, listens, 1, SOL));
    tx.ok();
    assert_eq!(tx.event::<Swapped>().lp_fee_bps, 34);
    assert_eq!(m.script(&listens).calls, 2);
}

#[test]
fn every_pool_callback_is_told_who_receives() {
    let mut m = Market::new();
    let (a, b, t) = (m.a, m.b, m.trader.pubkey());
    let owner = m.owner.pubkey();
    let flags = pool_flags::BEFORE_INITIALIZE
        | pool_flags::AFTER_INITIALIZE
        | pool_flags::BEFORE_ADD_LIQUIDITY
        | pool_flags::AFTER_ADD_LIQUIDITY
        | pool_flags::BEFORE_REMOVE_LIQUIDITY
        | pool_flags::AFTER_REMOVE_LIQUIDITY;
    let pool = m.pool(30, Some(flags), vec![fixed(h(&b, &m.rb[0]), true)]);
    // Creation: nobody receives.
    let s = m.script(&pool);
    for (which, phase) in [
        (callback::BEFORE_INITIALIZE, Phase::Before),
        (callback::AFTER_INITIALIZE, Phase::After),
    ] {
        let told = s.told_pool(which).unwrap();
        assert_eq!(
            (told.op, told.phase, told.actor, told.recipient),
            (PoolOp::Initialize, phase, owner, Pubkey::default())
        );
    }
    // Liquidity callbacks answer nothing: a scripted delta is never read.
    m.answer(pool, callback::BEFORE_ADD_LIQUIDITY, &cut(&[(10, X)], 0));
    m.answer(pool, callback::AFTER_REMOVE_LIQUIDITY, &cut(&[(10, X)], 0));

    // The trader adds liquidity with the LP going to carol's holding: the hook is told carol.
    let carol = m.w.env.funded(10 * SOL);
    let c = carol.pubkey();
    let lp_mint = swap::lp_mint_address(&pool);
    m.w.holdings(&m.owner, lp_mint, &[c]);
    m.w.holdings(&m.owner, b, &[c]);
    let keys = swap::LiquidityKeys {
        provider: t,
        pool,
        base_mint: a,
        quote_mint: b,
        hook_program: Some(hook_tester::ID),
    };
    let base_slice = m.w.env.token_hook_slice(
        &a,
        &h(&a, &t),
        &swap::vault_address(&pool, &a),
        &t,
        &t,
        &pool,
    );
    let mut extras = base_slice.clone();
    extras.extend(
        m.w.env
            .pool_hook_extras(&hook_tester::ID, &pool, &a, &b, &t),
    );
    let mut ix = swap::add_liquidity(
        &keys,
        AddLiquidityArgs {
            base_desired: 10 * SOL,
            quote_desired: 10 * SOL,
            min_lp: 1,
            base_hook_accounts: base_slice.len() as u8,
            quote_hook_accounts: 0,
            hook_data: vec![],
        },
        extras,
    );
    // The provider's LP holding (account 10) is carol's.
    assert_eq!(ix.accounts[10].pubkey, h(&lp_mint, &t));
    ix.accounts[10].pubkey = h(&lp_mint, &c);
    let trader = m.trader.insecure_clone();
    let tx = m.w.env.send_paid_by(&[ix], &trader, &[]);
    tx.ok();
    println!(
        "add_liquidity, taxed base (hook_tester before + after) CU {} size {}",
        tx.cu(),
        tx.size
    );
    let lp = m.bal(&lp_mint, &c);
    assert!(lp > 0);
    assert_eq!(m.bal(&b, &m.rb[0]), 0);
    let s = m.script(&pool);
    for which in [
        callback::BEFORE_ADD_LIQUIDITY,
        callback::AFTER_ADD_LIQUIDITY,
    ] {
        let told = s.told_pool(which).unwrap();
        assert_eq!(
            (told.op, told.actor, told.recipient),
            (PoolOp::AddLiquidity, t, c)
        );
    }
    assert_eq!(
        s.told_pool(callback::AFTER_ADD_LIQUIDITY)
            .unwrap()
            .lp_amount,
        lp
    );

    // Carol removes it with the base going to dave's holding: the hook is told dave.
    let dave = Pubkey::new_unique();
    m.w.holdings(&m.owner, a, &[dave]);
    let keys = swap::LiquidityKeys {
        provider: c,
        ..keys
    };
    let base_slice = m.w.env.token_hook_slice(
        &a,
        &swap::vault_address(&pool, &a),
        &h(&a, &dave),
        &pool,
        &pool,
        &dave,
    );
    let mut extras = base_slice.clone();
    extras.extend(
        m.w.env
            .pool_hook_extras(&hook_tester::ID, &pool, &a, &b, &c),
    );
    let mut ix = swap::remove_liquidity(
        &keys,
        RemoveLiquidityArgs {
            lp_amount: lp,
            min_base: 0,
            min_quote: 0,
            base_hook_accounts: base_slice.len() as u8,
            quote_hook_accounts: 0,
            hook_data: vec![],
        },
        extras,
    );
    // The provider's base holding (account 8) is dave's.
    assert_eq!(ix.accounts[8].pubkey, h(&a, &c));
    ix.accounts[8].pubkey = h(&a, &dave);
    let tx = m.w.env.send_paid_by(&[ix], &carol, &[]);
    tx.ok();
    assert!(m.bal(&a, &dave) > 0 && m.bal(&b, &c) > 0);
    assert_eq!((m.bal(&lp_mint, &c), m.bal(&b, &m.rb[0])), (0, 0));
    let s = m.script(&pool);
    for which in [
        callback::BEFORE_REMOVE_LIQUIDITY,
        callback::AFTER_REMOVE_LIQUIDITY,
    ] {
        let told = s.told_pool(which).unwrap();
        assert_eq!(
            (told.op, told.actor, told.recipient, told.lp_amount),
            (PoolOp::RemoveLiquidity, c, dave, lp)
        );
    }
}

#[test]
fn protocol_fees_are_collected_in_quote_for_the_collector_of_the_day() {
    let mut w = World::new();
    let owner = w.env.funded(100 * SOL);
    let trader = w.env.funded(100 * SOL);
    let deployer = w.env.deployer.insecure_clone();
    let (t, admin) = (trader.pubkey(), deployer.pubkey());
    let (taxman, creator) = (Pubkey::new_unique(), Pubkey::new_unique());
    // A plain base X and a taxed quote Q: every protocol fee moves through Q's hook.
    let x = w.mint_to_owner(&owner, 6, SUPPLY, "XXX");
    let q = w.tax_mint(&owner, 9, SUPPLY, "QQQ", taxman, TAX_BPS);
    w.holdings(&owner, x, &[t]);
    w.holdings(&owner, q, &[t, creator]);
    w.send_tokens(&owner, x, &t, STAKE).ok();
    w.send_tokens(&owner, q, &t, STAKE).ok();
    // The pool's hook takes 1% for `creator`: from a buy's input, from a sell's output after the
    // protocol fee (a launch's creator fee).
    let (pool, tx) = w.new_pool(
        &owner,
        &NewPool {
            base: x,
            quote: q,
            lp_fee_bps: 30,
            tester_flags: Some(SWAP_FLAGS),
            extras: vec![fixed(h(&q, &creator), true)],
            base_amount: DEPOSIT,
            quote_amount: DEPOSIT,
        },
    );
    tx.ok();
    let read = |w: &World| -> Pool { w.env.read(&pool) };
    // A buy, then a sell of half of what it bought; answers the protocol fees they paid.
    let trade = |w: &mut World, amount_in: u64| -> u64 {
        // The buy: the hook's cut and the rest each pay Q's tax on their way in; the protocol fee
        // comes from what reached the vault.
        let p0: Pool = w.env.read(&pool);
        let creator_fee = fee_amount(amount_in, 100).unwrap();
        w.script_answer(
            &owner,
            pool,
            callback::BEFORE_SWAP,
            &cut(&[(creator_fee, X)], 0),
        );
        w.script_raw(&owner, pool, callback::AFTER_SWAP, mode::NONE, vec![]);
        let received = taxed(amount_in - creator_fee);
        let (_, buy_fee, out, _) = expect(true, received, 30, &p0);
        let x_before = w.env.holding(&x, &t);
        let ix = w.env.swap_ix(&SwapSpec::new(t, pool, 1, amount_in));
        let tx = w.env.send_paid_by(&[ix], &trader, &[]);
        tx.ok();
        assert_eq!(
            (tx.event::<Swapped>().protocol_fee, w.env.holding(&x, &t)),
            (buy_fee, x_before + out)
        );
        // The sell: the protocol fee from the curve's output, in Q; the hook's cut from the rest;
        // what is left reaches the trader less Q's tax.
        let p0: Pool = w.env.read(&pool);
        let tokens = out / 2;
        let (_, sell_fee, _, told) = expect(false, tokens, 30, &p0);
        let creator_fee = fee_amount(told, 100).unwrap();
        w.script_raw(&owner, pool, callback::BEFORE_SWAP, mode::NONE, vec![]);
        w.script_answer(
            &owner,
            pool,
            callback::AFTER_SWAP,
            &cut(&[(creator_fee, X)], 0),
        );
        let q_before = w.env.holding(&q, &t);
        let ix = w.env.swap_ix(&SwapSpec::new(t, pool, 0, tokens));
        let tx = w.env.send_paid_by(&[ix], &trader, &[]);
        tx.ok();
        assert_eq!(
            (tx.event::<Swapped>().protocol_fee, w.env.holding(&q, &t)),
            (sell_fee, q_before + taxed(told - creator_fee))
        );
        buy_fee + sell_fee
    };
    let mut fees = 0;
    for round in 1..=3 {
        fees += trade(&mut w, round * SOL);
        let p = read(&w);
        assert_eq!(p.protocol_fees_quote, fees);
        assert_eq!(w.env.holding(&q, &pool), p.quote_reserve + fees);
        assert_eq!(w.env.holding(&x, &pool), p.base_reserve);
    }

    // The admin collects into the fee collector's Q holding (the deployer at first). Without the
    // quote hook's accounts the token program refuses the transfer; with a count that does not
    // match the accounts the DEX does; into a holding that is not the collector's, too.
    w.holdings(&deployer, q, &[admin]);
    let tx = w.env.send_paid_by(
        &[swap::collect_protocol_fees(admin, pool, q, admin, vec![])],
        &deployer,
        &[],
    );
    // 6008 is also a DEX code, so the log names the token program's error.
    tx.expect_code(u32::from(TokenError::HookProgramMissing));
    assert!(tx
        .logs()
        .iter()
        .any(|l| l.contains("Error Code: HookProgramMissing")));
    let mut ix = w.env.collect_ix(&admin, &pool, &admin);
    ix.accounts
        .push(AccountMeta::new_readonly(Pubkey::new_unique(), false));
    w.env
        .send_paid_by(&[ix], &deployer, &[])
        .expect_code(code(SwapError::AccountCounts));
    let ix = w.env.collect_ix(&admin, &pool, &t);
    w.env
        .send_paid_by(&[ix], &deployer, &[])
        .expect_code(code(SwapError::WrongHolding));
    let taxman_q = w.env.holding(&q, &taxman);
    let ix = w.env.collect_ix(&admin, &pool, &admin);
    let tx = w.env.send_paid_by(&[ix], &deployer, &[]);
    tx.ok();
    println!(
        "collect_protocol_fees, taxed quote CU {} size {}",
        tx.cu(),
        tx.size
    );
    let ev: ProtocolFeesCollected = tx.event();
    assert_eq!(
        (ev.pool, ev.quote_amount, ev.collector),
        (pool, fees, admin)
    );
    assert_eq!(w.env.holding(&q, &admin), taxed(fees));
    assert_eq!(w.env.holding(&q, &taxman), taxman_q + tax(fees));
    assert_eq!(w.env.holding(&x, &admin), 0);
    let p = read(&w);
    assert_eq!(p.protocol_fees_quote, 0);
    assert_eq!(w.env.holding(&q, &pool), p.quote_reserve);
    assert_eq!(w.env.holding(&x, &pool), p.base_reserve);

    // The admin names another fee collector. What accrues from now on goes to it; the old
    // collector's holding is refused.
    let next = Pubkey::new_unique();
    w.holdings(&deployer, q, &[next]);
    let config = ConfigArgs {
        admin,
        protocol_fee_bps: PROTOCOL_BPS,
        launch_protocol_share_bps: policy::LAUNCH_PROTOCOL_SHARE_BPS,
        fee_collector: next,
        treasury: w.env.treasury.pubkey(),
        pool_creation_fee_lamports: 0,
        paused: false,
    };
    w.env
        .send_paid_by(&[swap::set_config(admin, config)], &deployer, &[])
        .ok();
    let fees = trade(&mut w, 2 * SOL);
    assert_eq!(read(&w).protocol_fees_quote, fees);
    let ix = w.env.collect_ix(&admin, &pool, &admin);
    w.env
        .send_paid_by(&[ix], &deployer, &[])
        .expect_code(code(SwapError::WrongHolding));
    let first = w.env.holding(&q, &admin);
    let ix = w.env.collect_ix(&admin, &pool, &next);
    let tx = w.env.send_paid_by(&[ix], &deployer, &[]);
    tx.ok();
    assert_eq!(tx.event::<ProtocolFeesCollected>().collector, next);
    assert_eq!(
        (w.env.holding(&q, &next), w.env.holding(&q, &admin)),
        (taxed(fees), first)
    );
    let p = read(&w);
    assert_eq!(p.protocol_fees_quote, 0);
    assert_eq!(w.env.holding(&q, &pool), p.quote_reserve);
    // Nothing accrued: a collection moves nothing.
    let ix = w.env.collect_ix(&admin, &pool, &next);
    let tx = w.env.send_paid_by(&[ix], &deployer, &[]);
    tx.ok();
    assert_eq!(tx.event::<ProtocolFeesCollected>().quote_amount, 0);
    assert_eq!(w.env.holding(&q, &next), taxed(fees));
}

#[test]
fn a_router_swaps_by_cpi() {
    let mut m = Market::new();
    let (a, t) = (m.a, m.trader.pubkey());
    // A pool without a hook: routed, the swap reaches the taxed mint's hook four levels down.
    let plain = m.pool(30, None, vec![]);
    let p0 = m.state(&plain);
    let (_, _, out, _) = expect(true, SOL, 30, &p0);
    let spec = SwapSpec {
        min_out: taxed(out),
        ..SwapSpec::new(t, plain, 1, SOL)
    };
    let t_a = m.bal(&a, &t);
    let ix = m.w.env.routed_swap_ix(&spec);
    let trader = m.trader.insecure_clone();
    let tx = m.w.env.send_paid_by(&[ix], &trader, &[]);
    tx.ok();
    println!(
        "routed buy (hook_tester > swap > token > tax_hook) CU {} size {} trace {} height {}",
        tx.cu(),
        tx.size,
        tx.trace_len(),
        tx.max_height()
    );
    assert_eq!(tx.max_height(), 4);
    assert_eq!(m.bal(&a, &t), t_a + taxed(out));
    let ev: Swapped = tx.event();
    assert_eq!(
        (ev.trader, ev.recipient, ev.delivered_out),
        (t, t, taxed(out))
    );
    // Directly, one level less.
    let tx = m.swap(&SwapSpec::new(t, plain, 1, SOL));
    tx.ok();
    assert_eq!(tx.max_height(), 3);
    // A pool whose hook is the router cannot be routed (the DEX would call back into it), but it
    // can be swapped directly.
    let hooked = m.pool(31, Some(SWAP_FLAGS), vec![]);
    let ix = m.w.env.routed_swap_ix(&SwapSpec::new(t, hooked, 1, SOL));
    m.w.env.send_paid_by(&[ix], &trader, &[]).expect_fail();
    m.swap(&SwapSpec::new(t, hooked, 1, SOL)).ok();
}
