//! Native-SOL re-audit (branch `native-sol-reaudit`, `docs/native-sol-reaudit.md`): adversarial
//! LiteSVM tests of the audit fixes on `native-sol` (`d34148c`), chiefly the two new value-moving
//! paths, the old `swap`'s sell (audit N-5) and `remove_liquidity` (audit N-4) paying what the
//! buffer can't cover as lamports. Each test is `holds_` (an attack or failure ruled out). The four
//! that reproduced R-1 (twice), R-2 and R-3 now prove their fixes.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_hook::{pool_flags, Delta, HookReturn};
use bordrless_launch::client as launch;
use bordrless_launch::error::LaunchError;
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::{Launch, LaunchRules, QuoteCut};
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::hooks::{fixed, NewPool, SwapSpec};
use bordrless_program_tests::launch::{SOL, VQ};
use bordrless_swap::client as swap;
use bordrless_swap::error::SwapError;
use bordrless_swap::events::Swapped;
use bordrless_swap::instructions::{RemoveLiquidityArgs, SwapArgs};
use bordrless_swap::state::Pool;
use bordrless_token::client as token;
use hook_tester::callback;
use solana_keypair::Keypair;
use solana_signer::Signer;

const SYSTEM: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");
const SUPPLY: u64 = 1_000_000_000_000_000;
const BASE_DEPOSIT: u64 = 1_000_000_000_000;
const QUOTE_DEPOSIT: u64 = 100 * SOL;
const SWAP_FLAGS: u16 = pool_flags::BEFORE_SWAP
    | pool_flags::AFTER_SWAP
    | pool_flags::BEFORE_SWAP_RETURNS_DELTA
    | pool_flags::AFTER_SWAP_RETURNS_DELTA;
const X: u8 = 6;
/// A plain wallet (paid lamports).
const WALLET: u8 = X;
/// A holding of bridged SOL (paid wrapped).
const LEGACY: u8 = X + 1;

fn code(e: SwapError) -> u32 {
    u32::from(e)
}

fn h(mint: &Pubkey, owner: &Pubkey) -> Pubkey {
    token::holding_address(mint, owner)
}

fn cut(deltas: &[(u64, u8)]) -> HookReturn {
    HookReturn {
        deltas: deltas
            .iter()
            .map(|&(amount, account)| Delta { amount, account })
            .collect(),
        ..HookReturn::default()
    }
}

fn sys_transfer(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: SYSTEM,
        accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
        data,
    }
}

fn dex_pause_ix(w: &World, paused: bool) -> Instruction {
    let c: bordrless_swap::state::Config = w.env.read(&swap::config_address());
    swap::set_config(
        c.admin,
        bordrless_swap::instructions::ConfigArgs {
            admin: c.admin,
            protocol_fee_bps: c.protocol_fee_bps,
            fee_collector: c.fee_collector,
            treasury: c.treasury,
            pool_creation_fee_lamports: c.pool_creation_fee_lamports,
            paused,
            launch_protocol_share_bps: c.launch_protocol_share_bps,
        },
    )
}

fn as_admin(w: &mut World, ixs: &[Instruction]) -> Tx {
    let a = w.env.deployer.insecure_clone();
    let tx = w.env.send(ixs, &[&a]);
    w.env.warp(1);
    tx
}

/// A scripted-hook pool (hook_tester) over a fresh base mint and bridged SOL, as the audit's.
struct M {
    w: World,
    owner: Keypair,
    trader: Keypair,
    payer: Keypair,
    a: Pubkey,
    sol: Pubkey,
    wallet: Pubkey,
    legacy_owner: Pubkey,
}

impl M {
    fn new() -> Self {
        let mut w = World::new();
        let owner = w.wallet_with_sol(1_000 * SOL);
        let trader = w.wallet_with_sol(200 * SOL);
        let payer = w.env.funded(100 * SOL);
        let a = w.mint_to_owner(&owner, 6, SUPPLY, "AAA");
        let sol = w.sol;
        let wallet = Pubkey::new_unique();
        w.env.fund(wallet, SOL);
        let legacy_owner = Pubkey::new_unique();
        w.holdings(&owner, sol, &[legacy_owner]);
        w.holdings(&owner, a, &[trader.pubkey()]);
        w.send_tokens(&owner, a, &trader.pubkey(), BASE_DEPOSIT / 10)
            .ok();
        Self {
            w,
            owner,
            trader,
            payer,
            a,
            sol,
            wallet,
            legacy_owner,
        }
    }

    fn pool(&mut self) -> Pubkey {
        let owner = self.owner.insecure_clone();
        let spec = NewPool {
            base: self.a,
            quote: self.sol,
            lp_fee_bps: 30,
            tester_flags: Some(SWAP_FLAGS),
            extras: vec![
                fixed(self.wallet, true),
                fixed(h(&self.sol, &self.legacy_owner), true),
            ],
            base_amount: BASE_DEPOSIT,
            quote_amount: QUOTE_DEPOSIT,
        };
        let (pool, tx) = self.w.new_pool(&owner, &spec);
        tx.ok();
        pool
    }

    fn state(&self, pool: &Pubkey) -> Pool {
        self.w.env.read(pool)
    }

    fn lamports(&self, key: &Pubkey) -> u64 {
        self.w.env.lamports(key)
    }

    fn bal(&self, mint: &Pubkey, owner: &Pubkey) -> u64 {
        self.w.env.holding(mint, owner)
    }

    /// The invariant as an equality, with `donated` lamports and `vault_donated` bridged SOL sent
    /// from outside.
    #[track_caller]
    fn inv(&self, pool: &Pubkey, donated: u64, vault_donated: u64) {
        let p = self.state(pool);
        let rent = self.w.env.rent(Pool::LEN);
        let books = p.quote_reserve + p.protocol_fees_quote;
        assert!(p.quote_native());
        assert_eq!(
            self.lamports(pool),
            rent + books - p.buffer_quote() + donated,
            "native pool lamports"
        );
        assert_eq!(
            self.bal(&self.sol, pool),
            p.buffer_quote() + vault_donated,
            "native buffer"
        );
    }

    /// Lamports of `keys` plus the bridged SOL in circulation.
    fn value(&self, keys: &[Pubkey]) -> u128 {
        let supply = self
            .w
            .env
            .read::<bordrless_token::state::Mint>(&self.sol)
            .supply;
        keys.iter()
            .map(|k| u128::from(self.lamports(k)))
            .sum::<u128>()
            + u128::from(supply)
    }

    fn admin(&self) -> Keypair {
        self.w.env.deployer.insecure_clone()
    }

    fn send(&mut self, ixs: &[Instruction], signer: &Keypair) -> Tx {
        let payer = self.payer.insecure_clone();
        let tx = self.w.env.send_paid_by(ixs, &payer, &[signer]);
        self.w.env.warp(1);
        tx
    }

    fn migrate(&mut self, pool: &Pubkey, target: u64) {
        let admin = self.admin();
        let ixs = [
            dex_pause_ix(&self.w, true),
            swap::migrate_pool_to_native_with(admin.pubkey(), *pool, target, true),
            dex_pause_ix(&self.w, false),
        ];
        self.send(&ixs, &admin).ok();
    }

    /// The buffer brought to `target` (the admin's target, then a stranger's rebalance), or to the
    /// whole books when they are smaller. Answers the buffer.
    fn buffer_to(&mut self, pool: &Pubkey, target: u64) -> u64 {
        let admin = self.admin();
        self.send(
            &[swap::set_buffer_target(admin.pubkey(), *pool, target)],
            &admin,
        )
        .ok();
        let cranker = self.w.env.funded(SOL);
        self.send(&[swap::rebalance_pool(cranker.pubkey(), *pool)], &cranker)
            .ok();
        let p = self.state(pool);
        let buffer = target.min(p.quote_reserve + p.protocol_fees_quote);
        assert_eq!(p.buffer_quote(), buffer);
        buffer
    }

    fn answer(&mut self, pool: Pubkey, which: u8, answer: &HookReturn) {
        let owner = self.owner.insecure_clone();
        self.w.script_answer(&owner, pool, which, answer);
    }

    fn silence(&mut self, pool: Pubkey) {
        let owner = self.owner.insecure_clone();
        for which in [callback::BEFORE_SWAP, callback::AFTER_SWAP] {
            self.w
                .script_raw(&owner, pool, which, hook_tester::mode::NONE, vec![]);
        }
    }

    fn v1_ix(&self, trader: &Pubkey, pool: &Pubkey, direction: u8, amount_in: u64) -> Instruction {
        self.w
            .env
            .swap_ix(&SwapSpec::new(*trader, *pool, direction, amount_in))
    }

    fn v1(&mut self, pool: &Pubkey, direction: u8, amount_in: u64) -> Tx {
        let trader = self.trader.insecure_clone();
        let ix = self.v1_ix(&trader.pubkey(), pool, direction, amount_in);
        self.send(&[ix], &trader)
    }

    fn v2(&mut self, pool: &Pubkey, direction: u8, amount_in: u64) -> Tx {
        let trader = self.trader.insecure_clone();
        let t = trader.pubkey();
        let p = self.state(pool);
        let keys = swap::SwapV2Keys {
            trader: t,
            pool: *pool,
            base_mint: p.base_mint,
            trader_base: h(&p.base_mint, &t),
            trader_quote: t,
            hook_program: p.hook_program,
            base_mint_writable: false,
            with_bridge: true,
        };
        let extras = self.pool_extras(pool, &t);
        let args = SwapArgs {
            direction,
            amount_in,
            min_amount_out: 0,
            in_hook_accounts: 0,
            out_hook_accounts: 0,
            hook_data: vec![],
        };
        let ix = swap::swap_v2(&keys, args, extras);
        self.send(&[ix], &trader)
    }

    fn liquidity_keys(&self, pool: &Pubkey) -> swap::LiquidityKeys {
        swap::LiquidityKeys {
            provider: self.owner.pubkey(),
            pool: *pool,
            base_mint: self.a,
            quote_mint: self.sol,
            hook_program: Some(hook_tester::ID),
        }
    }

    fn pool_extras(&self, pool: &Pubkey, actor: &Pubkey) -> Vec<AccountMeta> {
        self.w
            .env
            .pool_hook_extras(&hook_tester::ID, pool, &self.a, &self.sol, actor)
    }

    fn remove_ix(&self, pool: &Pubkey, lp_amount: u64) -> Instruction {
        let o = self.owner.pubkey();
        swap::remove_liquidity(
            &self.liquidity_keys(pool),
            RemoveLiquidityArgs {
                lp_amount,
                min_base: 0,
                min_quote: 0,
                base_hook_accounts: 0,
                quote_hook_accounts: 0,
                hook_data: vec![],
            },
            self.pool_extras(pool, &o),
        )
    }
}

/// What a sell left: the trader's bridged-SOL gain, its lamport gain, the event's delivery, and the
/// pool's books after.
#[derive(Debug, PartialEq, Eq)]
struct Sold {
    bridged: u64,
    lamports: u64,
    delivered: u64,
    wallet: u64,
    legacy: u64,
    books: (u64, u64, u64),
}

/// One world: a native pool at target 0 with books in both forms and a buffer of `buffer`, a
/// quote-side cut on sells of `lamport_cut` lamports to a wallet and `wrapped_cut` wrapped into a
/// holding of bridged SOL; then one sell of `amount` through the old swap (`old`) or `swap_v2`.
fn sell_once(old: bool, buffer: u64, lamport_cut: u64, wrapped_cut: u64, amount: u64) -> Sold {
    let mut m = M::new();
    let pool = m.pool();
    m.silence(pool);
    m.migrate(&pool, 0);
    m.v2(&pool, 1, 20 * SOL).ok();
    m.buffer_to(&pool, buffer);
    m.inv(&pool, 0, 0);
    let mut d = vec![];
    if lamport_cut > 0 {
        d.push((lamport_cut, WALLET));
    }
    if wrapped_cut > 0 {
        d.push((wrapped_cut, LEGACY));
    }
    m.answer(pool, callback::AFTER_SWAP, &cut(&d));
    let t = m.trader.pubkey();
    let keys = [pool, t, m.wallet];
    let value = m.value(&keys);
    let (q0, l0, w0, g0) = (
        m.bal(&m.sol, &t),
        m.lamports(&t),
        m.lamports(&m.wallet),
        m.bal(&m.sol, &m.legacy_owner),
    );
    let tx = if old {
        m.v1(&pool, 0, amount)
    } else {
        m.v2(&pool, 0, amount)
    };
    tx.ok();
    let ev: Swapped = tx.event();
    m.inv(&pool, 0, 0);
    assert_eq!(m.value(&keys), value, "value conserved");
    let p = m.state(&pool);
    Sold {
        bridged: m.bal(&m.sol, &t) - q0,
        lamports: m.lamports(&t) - l0,
        delivered: ev.delivered_out,
        wallet: m.lamports(&m.wallet) - w0,
        legacy: m.bal(&m.sol, &m.legacy_owner) - g0,
        books: (p.quote_reserve, p.base_reserve, p.protocol_fees_quote),
    }
}

/// HOLDS (N-5, lamport exactness): an old-path sell whose delivery the buffer only partly covers
/// takes the wrapped cut from the buffer first, delivers the rest of the buffer as bridged SOL
/// and exactly the remainder as lamports to the trader; the trader's two gains add up to the
/// delivery, which, with every cut and the books, is identical to the same sell through
/// `swap_v2` (no arbitrage between the two). The buffer ends at 0, the invariant is an equality
/// and the value (lamports plus bridged supply) is conserved. With no cut and with a buffer that
/// covers the whole delivery, likewise.
#[test]
fn holds_old_sell_with_a_partial_buffer_is_lamport_exact() {
    let amount = BASE_DEPOSIT / 50;
    for (buffer, lc, wc) in [
        (SOL / 2, 3_000_000, 5_000_000),
        (SOL / 2, 0, 0),
        (1, 0, 0),
        (5_000_000, 0, 5_000_000),
        (0, 4_000_000, 0),
        (50 * SOL, 3_000_000, 5_000_000),
    ] {
        let old = sell_once(true, buffer, lc, wc, amount);
        let v2 = sell_once(false, buffer, lc, wc, amount);
        assert_eq!(old.delivered, v2.delivered, "same delivery ({buffer})");
        assert_eq!(old.books, v2.books, "same books ({buffer})");
        assert_eq!((old.wallet, old.legacy), (lc, wc));
        assert_eq!((v2.wallet, v2.legacy), (lc, wc));
        assert_eq!(old.bridged + old.lamports, old.delivered);
        let from_buffer = old.delivered.min(buffer - wc);
        assert_eq!(old.bridged, from_buffer, "bridged part ({buffer})");
        assert_eq!(old.lamports, old.delivered - from_buffer);
        assert_eq!(v2.lamports, v2.delivered);
    }
}

/// HOLDS (N-5, refusals move nothing): an old-path sell is refused, with nothing moved, when the
/// buffer can't pay its wrapped cut (`BufferShort`), when the delivery needs lamports and the
/// trader is passed read-only (`BufferShort`, as an old client that isn't the fee payer), and
/// when the lamports would leave a new trader account below rent (`RecipientBelowRent`). A
/// shortfall at or above rent opens that account.
#[test]
fn holds_old_sell_shortfall_refusals_move_nothing() {
    let mut m = M::new();
    let pool = m.pool();
    m.silence(pool);
    m.migrate(&pool, 0);
    m.v2(&pool, 1, 20 * SOL).ok();
    m.buffer_to(&pool, 1_000_000);
    let t = m.trader.pubkey();
    let snapshot = |m: &M| {
        (
            m.lamports(&pool),
            m.state(&pool).buffer_quote(),
            m.state(&pool).quote_reserve,
            m.bal(&m.sol, &pool),
            m.bal(&m.a, &t),
        )
    };
    // A wrapped cut above the buffer.
    m.answer(pool, callback::AFTER_SWAP, &cut(&[(2_000_000, LEGACY)]));
    let s0 = snapshot(&m);
    m.v1(&pool, 0, BASE_DEPOSIT / 100)
        .expect_code(code(SwapError::BufferShort));
    assert_eq!(snapshot(&m), s0);
    m.silence(pool);
    // The trader read-only (not the fee payer).
    let mut ix = m.v1_ix(&t, &pool, 0, BASE_DEPOSIT / 100);
    assert!(ix.accounts[0].is_writable, "the client passes it writable");
    ix.accounts[0].is_writable = false;
    let trader = m.trader.insecure_clone();
    m.send(&[ix], &trader)
        .expect_code(code(SwapError::BufferShort));
    assert_eq!(snapshot(&m), s0);
    // Read-only still works while the buffer covers the delivery.
    let mut ix = m.v1_ix(&t, &pool, 0, 1_000);
    ix.accounts[0].is_writable = false;
    m.send(&[ix], &trader).ok();
    m.inv(&pool, 0, 0);
    // A trader with no lamports (a fresh signer; the fee payer is another key).
    m.buffer_to(&pool, 0);
    let fresh = Keypair::new();
    let f = fresh.pubkey();
    let owner = m.owner.insecure_clone();
    let (a, sol) = (m.a, m.sol);
    m.w.holdings(&owner, a, &[f]);
    m.w.holdings(&owner, sol, &[f]);
    m.w.send_tokens(&owner, a, &f, BASE_DEPOSIT / 20).ok();
    assert_eq!(m.lamports(&f), 0);
    let s1 = (m.lamports(&pool), m.state(&pool).quote_reserve);
    // About 0.0005 SOL: below a wallet's rent-exempt minimum.
    let ix = m.v1_ix(&f, &pool, 0, 5_000_000);
    m.send(&[ix], &fresh)
        .expect_code(code(SwapError::RecipientBelowRent));
    assert_eq!((m.lamports(&pool), m.state(&pool).quote_reserve), s1);
    let ix = m.v1_ix(&f, &pool, 0, BASE_DEPOSIT / 100);
    let tx = m.send(&[ix], &fresh);
    tx.ok();
    let ev: Swapped = tx.event();
    assert_eq!(
        m.lamports(&f),
        ev.delivered_out,
        "the lamports open the account"
    );
    m.inv(&pool, 0, 0);
}

/// HOLDS (N-5, delivery to the pool's own quote vault): an old-path sell that names the pool's
/// quote vault as its delivery holding gains the trader nothing beyond its delivery (the token
/// program refuses the self-transfer, or the bridged part stays in the vault as a donation the
/// books never count); the pool never pays twice.
#[test]
fn holds_old_sell_delivered_to_the_quote_vault_pays_nothing_extra() {
    let mut m = M::new();
    let pool = m.pool();
    m.silence(pool);
    m.migrate(&pool, 0);
    m.v2(&pool, 1, 20 * SOL).ok();
    m.buffer_to(&pool, SOL / 2);
    let t = m.trader.pubkey();
    let p = m.state(&pool);
    let mut ix = m.v1_ix(&t, &pool, 0, BASE_DEPOSIT / 50);
    // trader_quote (index 8) is the delivery holding of a sell.
    assert_eq!(ix.accounts[8].pubkey, h(&m.sol, &t));
    ix.accounts[8].pubkey = p.quote_vault;
    let (l0, pool0) = (m.lamports(&t), m.lamports(&pool));
    let trader = m.trader.insecure_clone();
    let tx = m.send(&[ix], &trader);
    if tx.result.is_ok() {
        let ev: Swapped = tx.event();
        let gained = m.lamports(&t) - l0;
        assert!(gained <= ev.delivered_out);
        assert_eq!(pool0 - m.lamports(&pool), gained);
        // Whatever stayed in the vault is uncounted: the vault holds at least the buffer.
        assert!(m.bal(&m.sol, &pool) >= m.state(&pool).buffer_quote());
    } else {
        assert_eq!(m.lamports(&pool), pool0);
    }
}

/// HOLDS (N-4, lamport exactness): a withdrawal from a native pool whose buffer covers part of the
/// quote pays exactly the buffer as bridged SOL and the rest as lamports to the provider; the two
/// add up to the same quote a fully-covered withdrawal of the same LP pays; the invariant is an
/// equality and the value is conserved. A provider passed read-only is refused (`BufferShort`) only
/// when lamports are needed.
#[test]
fn holds_lp_withdrawal_with_a_partial_buffer_is_lamport_exact() {
    let run = |buffer: u64, readonly: bool| -> Option<(u64, u64, u64, u64)> {
        let mut m = M::new();
        let pool = m.pool();
        m.silence(pool);
        m.migrate(&pool, 0);
        m.v2(&pool, 1, 20 * SOL).ok();
        let buffer = m.buffer_to(&pool, buffer);
        let o = m.owner.pubkey();
        let lp_mint = swap::lp_mint_address(&pool);
        let lp = m.bal(&lp_mint, &o) / 4;
        let keys = [pool, o];
        let value = m.value(&keys);
        let (q0, l0, b0) = (m.bal(&m.sol, &o), m.lamports(&o), m.bal(&m.a, &o));
        let mut ix = m.remove_ix(&pool, lp);
        assert!(ix.accounts[0].is_writable);
        if readonly {
            ix.accounts[0].is_writable = false;
        }
        let owner = m.owner.insecure_clone();
        let tx = m.send(&[ix], &owner);
        if tx.result.is_err() {
            tx.expect_code(code(SwapError::BufferShort));
            assert_eq!(m.state(&pool).buffer_quote(), buffer);
            return None;
        }
        m.inv(&pool, 0, 0);
        assert_eq!(m.value(&keys), value);
        assert_eq!(
            m.state(&pool).buffer_quote(),
            buffer.saturating_sub(m.bal(&m.sol, &o) - q0)
        );
        Some((
            m.bal(&m.sol, &o) - q0,
            m.lamports(&o) - l0,
            m.bal(&m.a, &o) - b0,
            m.bal(&lp_mint, &o),
        ))
    };
    let full = run(200 * SOL, false).expect("covered");
    assert_eq!(full.1, 0, "a covered withdrawal is all bridged SOL");
    for buffer in [0, 1, SOL, 10 * SOL] {
        let part = run(buffer, false).expect("paid");
        assert_eq!(part.0, buffer.min(full.0), "bridged part = the buffer");
        assert_eq!(part.0 + part.1, full.0, "same quote in total");
        assert_eq!((part.2, part.3), (full.2, full.3), "same base and LP");
        assert_eq!(run(buffer, true), None, "read-only provider refused");
    }
    // Read-only and covered: still works, as before the fix.
    assert_eq!(run(200 * SOL, true), Some(full));
}

// ---------------------------------------------------------------------------------------------
// Launch pools (share model, kit holder fees, creator fee): old path against swap_v2.
// ---------------------------------------------------------------------------------------------

fn migrate_launch(w: &mut World, mint: &Pubkey) {
    let a = w.env.deployer.pubkey();
    let modules = w.launch(mint).modules;
    let pool = w.launch_pool_key(mint);
    let sol = w.sol;
    let ixs = [
        dex_pause_ix(w, true),
        swap::migrate_pool_to_native(a, pool, 0),
        launch::migrate_launch_to_native(a, *mint, sol, modules),
        dex_pause_ix(w, false),
    ];
    as_admin(w, &ixs).ok();
}

fn v2_launch_ix(
    w: &World,
    trader: &Pubkey,
    mint: &Pubkey,
    direction: u8,
    amount: u64,
) -> Instruction {
    let keys = w.launch_keys(mint);
    let slice = w.launch_base_slice(mint, trader, trader, direction == 1);
    let ix = launch::swap_v2_with_base_slice(&keys, *trader, *trader, direction, amount, 0, slice);
    w.with_quote_cut(ix, mint, trader)
}

/// One kit coin (holder fees and burn both ways, creator fee 1%), migrated, with a buffer of
/// `buffer`: `b` buys 4 SOL then sells half, each through the old swap (`old`) or swap_v2.
/// Answers what `b` paid and got (SOL in both forms), the pool's books, what reached the launch
/// and the kit config, and the lamports `b`'s sell paid it.
fn launch_round_trip(old: bool, buffer: u64) -> [u64; 8] {
    let mut w = World::new();
    let payer = w.env.funded(100 * SOL);
    let creator = w.wallet_with_sol(20 * SOL);
    let rules = bordrless_program_tests::launch::rules(100, 100, 50, 50, 0, 0, 0, 0);
    let (mint, tx) = w.create_launch_with(&creator, "KIT", 100, VQ, rules);
    tx.ok();
    w.env.warp(31);
    let (a, b) = (w.wallet_with_sol(30 * SOL), w.wallet_with_sol(30 * SOL));
    w.holdings(&a, mint, &[a.pubkey(), b.pubkey()]);
    w.buy(&a, &mint, 3 * SOL).ok();
    migrate_launch(&mut w, &mint);
    let ix = v2_launch_ix(&w, &a.pubkey(), &mint, 1, 2 * SOL);
    w.env.send_paid_by(&[ix], &payer, &[&a]).ok();
    let pool = w.launch_pool_key(&mint);
    let ad = w.env.deployer.pubkey();
    as_admin(&mut w, &[swap::set_buffer_target(ad, pool, buffer)]).ok();
    let cranker = w.env.funded(SOL);
    w.env
        .send_paid_by(
            &[swap::rebalance_pool(cranker.pubkey(), pool)],
            &cranker,
            &[],
        )
        .ok();
    w.env.warp(1);
    let kit = w.launch(&mint).kit_config;
    let launch_key = launch::launch_address(&mint);
    let bk = b.pubkey();
    let (q0, l0, k0, la0) = (
        w.env.holding(&w.sol, &bk),
        w.env.lamports(&bk),
        w.env.lamports(&kit),
        w.env.lamports(&launch_key),
    );
    let buy = if old {
        w.launch_swap_ix(&bk, &mint, 1, 4 * SOL, 0)
    } else {
        v2_launch_ix(&w, &bk, &mint, 1, 4 * SOL)
    };
    w.env.send_paid_by(&[buy], &payer, &[&b]).ok();
    w.env.warp(1);
    // Back to the target before the sell (an old buy filled the buffer): anyone may.
    w.env
        .send_paid_by(
            &[swap::rebalance_pool(cranker.pubkey(), pool)],
            &cranker,
            &[],
        )
        .ok();
    w.env.warp(1);
    let held = w.env.holding(&mint, &bk);
    let before_sell = w.env.lamports(&bk);
    let sell = if old {
        w.launch_swap_ix(&bk, &mint, 0, held / 2, 0)
    } else {
        v2_launch_ix(&w, &bk, &mint, 0, held / 2)
    };
    let tx = w.env.send_paid_by(&[sell], &payer, &[&b]);
    tx.ok();
    w.env.warp(1);
    let p: Pool = w.env.read(&pool);
    // b's net SOL (both forms): it paid 4 SOL and got the sale back.
    let paid_bridged = q0 as i128 - w.env.holding(&w.sol, &bk) as i128;
    let paid_lamports = l0 as i128 - w.env.lamports(&bk) as i128;
    let net = (paid_bridged + paid_lamports) as u64;
    // The pool's invariant, exact.
    assert_eq!(
        w.env.lamports(&pool),
        w.env.rent(Pool::LEN) + p.quote_reserve + p.protocol_fees_quote - p.buffer_quote()
    );
    assert_eq!(w.env.holding(&w.sol, &pool), p.buffer_quote());
    [
        net,
        w.env.holding(&mint, &bk),
        p.quote_reserve,
        p.base_reserve,
        p.protocol_fees_quote,
        w.env.lamports(&kit) - k0,
        w.env.lamports(&launch_key) - la0,
        w.env.lamports(&bk) - before_sell,
    ]
}

/// HOLDS (no arbitrage, N-2 and N-5 on a launch pool): a buy and a sell of a kit coin with holder
/// fees, a burn and a creator fee cost and pay exactly the same through the old swap as through
/// `swap_v2`, whatever the buffer (none, a part of the sell, more than the sell; a target at or
/// above the books is left out: there a native launch pool cannot pay its lamport cuts on either
/// path, a pre-existing limit, see the report), and leave the
/// same books, holder fees and creator fees: the old path's early holder-fee payment and its
/// lamport delivery change only the form.
#[test]
fn holds_old_and_v2_round_trips_match_on_a_kit_launch_pool() {
    for buffer in [0, SOL / 10, SOL, 3 * SOL] {
        let old = launch_round_trip(true, buffer);
        let v2 = launch_round_trip(false, buffer);
        assert_eq!(old[..7], v2[..7], "buffer {buffer}: old {old:?} v2 {v2:?}");
        // The old sell took lamports exactly when the buffer could not cover it.
        assert_eq!(old[7] > 0, buffer < 3 * SOL, "old-path lamports {}", old[7]);
    }
}

// ---------------------------------------------------------------------------------------------
// Finding R-1: a program that sells through the old swap with its own PDA as trader.
// ---------------------------------------------------------------------------------------------

/// FIXED R-1 (the N-5 fallback reached CPI callers that count only bridged SOL): a hook vault
/// left bridged on a native pool (opened before native launches, its `migrate_vault_to_native`
/// not yet run) sells its slot through the old swap by CPI with the slot's owner as trader. The
/// DEX pays the lamport fallback only to a top-level swap, so at buffer target 0 the step fails
/// safe (`BufferShort`) and nothing moves: no proceeds stranded on the slot owner, as before N-5.
#[test]
fn holds_a_bridged_vault_on_a_native_pool_fails_safe() {
    use bordrless_program_tests::vault::{sell_for_sol, vault_args};
    let mut w = World::new();
    let to = w.env.funded(SOL).pubkey();
    let c = w.vault_coin(vault_args(vec![sell_for_sol(to)]), &[], true);
    let m = c.mint;
    assert!(!w.launch(&m).is_native() && !w.vault(&m).is_native());
    w.env.warp(31);
    w.churn(&m, 4, 2 * SOL);
    w.env.warp(120);
    let owner = hook_vault::client::slot_owner(&m, 0);
    assert!(w.env.holding(&m, &owner) > 0, "the slot holds coin");
    // The window: pool and launch go native; the vault's step is left for later.
    migrate_launch(&mut w, &m);
    assert!(w.launch(&m).is_native() && !w.vault(&m).is_native());
    assert_eq!(w.launch_pool(&m).buffer_quote(), 0);
    w.set_sell_reference_to_price(&m, 0);
    let cranker = w.wallet_with_sol(SOL);
    let before = (
        w.env.holding(&m, &owner),
        w.env.holding(&w.sol, &owner),
        w.env.lamports(&owner),
        w.env.lamports(&to),
    );
    w.execute(&cranker, &m, 0)
        .expect_code(code(SwapError::BufferShort));
    let after = (
        w.env.holding(&m, &owner),
        w.env.holding(&w.sol, &owner),
        w.env.lamports(&owner),
        w.env.lamports(&to),
    );
    assert_eq!(before, after, "nothing moved");
}

/// FIXED R-1, the router: `bordrless_router::swap` measures a sell's delivery as the trader's
/// bridged-SOL gain and charges the integrator's fee on that. A routed old sell is a CPI, so the
/// DEX no longer pays it lamports the router can't see: at buffer target 0 it fails safe
/// (`BufferShort`), and the same sell sent to the DEX directly still sells, part as lamports.
#[test]
fn holds_a_routed_old_sell_on_a_native_pool_fails_safe() {
    use bordrless_router::client as router;
    let mut w = World::new();
    let creator = w.wallet_with_sol(20 * SOL);
    let (mint, tx) = w.create_launch_with(&creator, "RTR", 100, VQ, LaunchRules::NONE);
    tx.ok();
    w.env.warp(31);
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
    let (iq, tq) = (h(&w.sol, &integrator.pubkey()), h(&w.sol, &treasury));
    let t = w.wallet_with_sol(10 * SOL);
    let me = t.pubkey();
    w.holdings(&t, mint, &[me]);
    w.buy(&t, &mint, 4 * SOL).ok();
    migrate_launch(&mut w, &mint);
    // Some lamport quote in the pool (a swap_v2 buy), the buffer at its target of 0.
    let other = w.env.funded(5 * SOL);
    w.holdings(&t, mint, &[other.pubkey()]);
    let ix = v2_launch_ix(&w, &other.pubkey(), &mint, 1, 2 * SOL);
    w.env.send_paid_by(&[ix], &payer, &[&other]).ok();
    assert_eq!(w.launch_pool(&mint).buffer_quote(), 0);
    let (i0, r0, l0) = (
        w.env.holding(&w.sol, &integrator.pubkey()),
        w.env.holding(&w.sol, &treasury),
        w.env.lamports(&me),
    );
    let held = w.env.holding(&mint, &me);
    let dex = w.launch_swap_ix(&me, &mint, 0, held / 2, 0);
    let ix = router::swap(me, iq, tq, &dex, 100, 0);
    let fee_payer = w.env.funded(SOL);
    w.env
        .send_paid_by(&[ix], &fee_payer, &[&t])
        .expect_code(code(SwapError::BufferShort));
    assert_eq!(w.env.holding(&mint, &me), held, "nothing sold");
    assert_eq!(w.env.lamports(&me), l0);
    assert_eq!(w.env.holding(&w.sol, &integrator.pubkey()), i0);
    assert_eq!(w.env.holding(&w.sol, &treasury), r0);
    // Top level, the trader owning the delivery holding: the fallback pays.
    let tx = w.env.send_paid_by(&[dex], &fee_payer, &[&t]);
    tx.ok();
    let delivered = tx.event::<Swapped>().delivered_out;
    assert!(delivered > SOL, "the trader sold for {delivered}");
    assert_eq!(w.env.lamports(&me) - l0, delivered, "all of it as lamports");
}

/// FIXED R-3 (held coins): on a coin whose pool is native but whose launch stays bridged (the 13
/// held coins, or a half-migrated coin), the launchpad names its creator and holder fees as
/// holdings of bridged SOL, which the old sell can pay only from the buffer. At target 0 the old
/// sell fails safe (`BufferShort`, nothing moved); the SDK's `launch.swap` and the market maker
/// now build `swap_v2` with the bridge accounts for every native pool, which sells.
#[test]
fn holds_a_held_coin_sells_through_swap_v2_at_target_zero() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(20 * SOL);
    let rules = bordrless_program_tests::launch::rules(100, 100, 0, 0, 0, 0, 0, 0);
    let (mint, tx) = w.create_launch_with(&creator, "HLD", 100, VQ, rules);
    tx.ok();
    w.env.warp(31);
    let t = w.wallet_with_sol(10 * SOL);
    let me = t.pubkey();
    w.holdings(&t, mint, &[me]);
    w.buy(&t, &mint, 4 * SOL).ok();
    // The held coins' migration: the pool only.
    let pool = w.launch_pool_key(&mint);
    let ad = w.env.deployer.pubkey();
    let ixs = [
        dex_pause_ix(&w, true),
        swap::migrate_pool_to_native(ad, pool, 0),
        dex_pause_ix(&w, false),
    ];
    as_admin(&mut w, &ixs).ok();
    assert!(w.launch_pool(&mint).quote_native() && !w.launch(&mint).is_native());
    assert_eq!(w.launch_pool(&mint).buffer_quote(), 0);
    let held = w.env.holding(&mint, &me);
    let payer = w.env.funded(SOL);
    let ix = w.launch_swap_ix(&me, &mint, 0, held / 4, 0);
    w.env
        .send_paid_by(&[ix], &payer, &[&t])
        .expect_code(code(SwapError::BufferShort));
    assert_eq!(w.env.holding(&mint, &me), held, "nothing sold");
    // swap_v2 with the bridge (the wrapped cuts wrapped from the pool's lamports) sells.
    let keys = w.launch_keys(&mint);
    let slice = w.launch_base_slice(&mint, &me, &me, false);
    let ix = launch::swap_v2_with_bridge(&keys, me, me, 0, held / 4, 0, slice, true);
    w.env.send_paid_by(&[ix], &payer, &[&t]).ok();
    assert_eq!(w.env.holding(&mint, &me), held - held / 4, "sold");
}

// ---------------------------------------------------------------------------------------------
// Finding R-2: the launchpad's reserved-key list misses one key.
// ---------------------------------------------------------------------------------------------

/// A native launch from a partner config naming `partner`, with fees waiting, or `None` when
/// `create_partner_config` refuses the key.
fn partner_launch(w: &mut World, partner: Pubkey) -> Option<(Pubkey, Keypair, Pubkey)> {
    let ad = w.env.deployer.pubkey();
    as_admin(w, &[launch::set_native_launches(ad, true)]).ok();
    let author = w.wallet_with_sol(5 * SOL);
    let config = Keypair::new();
    let ix = launch::create_partner_config(
        author.pubkey(),
        config.pubkey(),
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: 200,
            custom_hook: None,
            custom_hook_flags: 0,
            label: "Partnered".to_string(),
        },
        0,
        QuoteCut::default(),
        partner,
        3_000,
    );
    let tx = w.env.send_paid_by(&[ix], &author, &[&config]);
    if tx.result.is_err() {
        tx.expect_code(u32::from(LaunchError::InvalidPartner));
        return None;
    }
    let creator = w.wallet_with_sol(5 * SOL);
    let mint = Keypair::new();
    let ix = w.create_launch_from_config_ix(
        &creator.pubkey(),
        &mint.pubkey(),
        "PRT",
        VQ,
        &config.pubkey(),
    );
    w.env.send_paid_by(&[ix], &creator, &[&mint]).ok();
    let mint = mint.pubkey();
    assert!(w.launch(&mint).is_native());
    w.env.warp(31);
    let t = w.env.funded(50 * SOL);
    let payer = w.env.funded(SOL);
    w.holdings(&t, mint, &[t.pubkey()]);
    let ix = v2_launch_ix(w, &t.pubkey(), &mint, 1, 5 * SOL);
    w.env.send_paid_by(&[ix], &payer, &[&t]).ok();
    w.env.warp(1);
    Some((mint, creator, config.pubkey()))
}

/// FIXED R-2 (N-7): `RESERVED_ACCOUNT_KEYS` has all 31 of the runtime's reserved keys
/// (agave-reserved-account-keys 4.3), the sysvar owner id
/// `Sysvar1111111111111111111111111111111111111` included, the same list as hook_vault's, so a
/// config naming it as the partner (which would lock every native claim) is refused.
#[test]
fn holds_the_sysvar_owner_key_is_refused_as_a_partner() {
    let mut w = World::new();
    // The listed keys are refused.
    let clock = Pubkey::from_str_const("SysvarC1ock11111111111111111111111111111111");
    assert!(partner_launch(&mut w, clock).is_none());
    // ... and so is the sysvar owner id, which the list lacked.
    let sysvar = Pubkey::from_str_const("Sysvar1111111111111111111111111111111111111");
    assert!(bordrless_launch::constants::RESERVED_ACCOUNT_KEYS.contains(&sysvar));
    assert!(partner_launch(&mut w, sysvar).is_none());
    // The same 31 keys as hook_vault's list.
    let mut ours = bordrless_launch::constants::RESERVED_ACCOUNT_KEYS.to_vec();
    let mut theirs = hook_vault::constants::RESERVED_KEYS.to_vec();
    ours.sort();
    theirs.sort();
    assert_eq!(ours, theirs);
}

// ---------------------------------------------------------------------------------------------
// N-8: the enforced revert order can't be griefed.
// ---------------------------------------------------------------------------------------------

/// HOLDS (N-8 griefing): with lamports donated to every account a revert reads or moves (the
/// launch, the kit config without holder rewards, the pool, bridged SOL to the pool's vault, and
/// the hook-vault and companion addresses, which then exist as system accounts), the enforced order
/// still runs to the end: the pool is refused before its launch (`LaunchStillNative`, also when the
/// launch isn't passed), the launch is refused without the vault's address (`VaultStillNative`),
/// and in order every step lands and the coin re-migrates. Nobody but the hook vault and companion
/// programs can create an account at those addresses, and a vault only turns native at its open
/// (the coin's own hook) or by the upgrade authority.
#[test]
fn holds_revert_order_survives_donations() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(20 * SOL);
    // Burn only: a kit without holder rewards.
    let rules = bordrless_program_tests::launch::rules(0, 0, 50, 50, 0, 0, 0, 0);
    let (mint, tx) = w.create_launch_with(&creator, "ORD", 100, VQ, rules);
    tx.ok();
    w.env.warp(31);
    let t = w.wallet_with_sol(30 * SOL);
    w.holdings(&t, mint, &[t.pubkey()]);
    w.buy(&t, &mint, 3 * SOL).ok();
    migrate_launch(&mut w, &mint);
    let l = w.launch(&mint);
    assert!(l.is_native() && !l.rewards_on());
    let pool = w.launch_pool_key(&mint);
    let launch_key = launch::launch_address(&mint);
    let vault_addr = hook_vault::client::vault_address(&mint);
    let companion_addr = bordrless_companion::state::Companion::address(&mint).0;
    let donor = w.wallet_with_sol(10 * SOL);
    let d = donor.pubkey();
    let mut ixs = vec![];
    for to in [launch_key, l.kit_config, pool, vault_addr, companion_addr] {
        ixs.push(sys_transfer(&d, &to, 1_000_000));
    }
    ixs.push(token::transfer(
        d,
        h(&w.sol, &d),
        swap::vault_address(&pool, &w.sol),
        w.sol,
        None,
        vec![],
        12_345,
    ));
    w.env.send_paid_by(&ixs, &donor, &[]).ok();
    assert_eq!(w.env.account(&vault_addr).unwrap().owner, SYSTEM);

    let ad = w.env.deployer.pubkey();
    let sol = w.sol;
    let modules = l.modules;
    let paused = |w: &mut World, ix: Instruction| -> Tx {
        let ixs = [dex_pause_ix(w, true), ix, dex_pause_ix(w, false)];
        as_admin(w, &ixs)
    };
    // Out of order: refused.
    paused(&mut w, swap::revert_launch_pool_to_bridged(ad, pool, mint))
        .expect_code(code(SwapError::LaunchStillNative));
    paused(&mut w, swap::revert_pool_to_bridged(ad, pool))
        .expect_code(code(SwapError::LaunchStillNative));
    let mut no_vault = launch::revert_launch_to_bridged(ad, mint, sol, modules);
    no_vault.accounts.retain(|a| a.pubkey != vault_addr);
    paused(&mut w, no_vault).expect_code(u32::from(LaunchError::VaultStillNative));
    // In order: every step lands.
    paused(
        &mut w,
        launch::revert_launch_to_bridged(ad, mint, sol, modules),
    )
    .ok();
    assert!(!w.launch(&mint).is_native());
    paused(&mut w, swap::revert_launch_pool_to_bridged(ad, pool, mint)).ok();
    assert!(!w.launch_pool(&mint).quote_native());
    // Even the launch passed bridged is still required for a launch pool (documented).
    // And the coin migrates again and trades.
    migrate_launch(&mut w, &mint);
    assert!(w.launch(&mint).is_native() && w.launch_pool(&mint).quote_native());
    let payer = w.env.funded(SOL);
    let ix = v2_launch_ix(&w, &t.pubkey(), &mint, 1, SOL);
    w.env.send_paid_by(&[ix], &payer, &[&t]).ok();
}

/// HOLDS (N-8, the launch flag parser): the DEX's fixed-offset read of a launch's native flag
/// agrees with the launch's own `is_native` for every byte pattern of the fields around it
/// (random reserved bytes, either hook tag, random counters), and refuses an account too short or
/// with an invalid option tag rather than reading another byte.
#[test]
fn holds_the_launch_flag_parser_matches_borsh() {
    use anchor_lang::AccountSerialize;
    use bordrless_program_tests::kit::Rng;
    let mut rng = Rng::new(0x5EED_0008);
    for _ in 0..2_000 {
        // SAFETY: a plain-data account struct; every byte pattern of these fields is valid.
        let mut l: Launch = unsafe { std::mem::zeroed() };
        l.custom_hook = if rng.below(2) == 0 {
            None
        } else {
            Some(Pubkey::new_unique())
        };
        l.custom_hook_flags = rng.below(u64::from(u16::MAX)) as u16;
        l.author_share_bps = rng.below(u64::from(u16::MAX)) as u16;
        l.author_fees_paid = rng.range(0, u64::MAX - 1);
        for b in l.reserved.iter_mut() {
            *b = rng.below(256) as u8;
        }
        l.reserved[17] = rng.below(3) as u8;
        let mut data = vec![];
        l.try_serialize(&mut data).unwrap();
        data.resize(Launch::LEN, 0);
        assert_eq!(
            bordrless_swap::instructions::launch_is_native(&data).unwrap(),
            l.is_native()
        );
        let short = &data[..bordrless_swap::constants::LAUNCH_CUSTOM_HOOK_OFFSET + 3];
        assert!(bordrless_swap::instructions::launch_is_native(short).is_err());
    }
    let mut data = vec![0u8; Launch::LEN];
    data[bordrless_swap::constants::LAUNCH_CUSTOM_HOOK_OFFSET] = 2;
    assert!(bordrless_swap::instructions::launch_is_native(&data).is_err());
}

// ---------------------------------------------------------------------------------------------
// N-5 and N-4 under donations and repetition: a seeded walk of the new paths.
// ---------------------------------------------------------------------------------------------

/// HOLDS (N-4/N-5 accounting under stress): 300 random steps that drive the new lamport paths hard
/// (old-path sells and LP withdrawals with random buffers from 0 to above the books, random wrapped
/// and lamport cuts, a trader that is sometimes read-only, rebalances, donations of lamports and of
/// bridged SOL, old and new buys). After every step, failed or not: the invariant is an equality
/// with the donations, and lamports plus the bridged supply are conserved over every account
/// involved (the trader and the provider included). A read-only trader or provider is only ever
/// refused with `BufferShort`, and only when its delivery needed lamports.
#[test]
fn holds_walk_of_the_lamport_fallbacks_conserves_value() {
    use bordrless_program_tests::kit::Rng;
    let mut m = M::new();
    let pool = m.pool();
    m.migrate(&pool, 0);
    m.silence(pool);
    let t = m.trader.pubkey();
    let o = m.owner.pubkey();
    let lp_mint = swap::lp_mint_address(&pool);
    let donor = m.w.wallet_with_sol(50 * SOL);
    let cranker = m.w.env.funded(10 * SOL);
    let admin = m.admin();
    // The owner pays the hook scripts' fees, so it stays out of the value (its gains from
    // withdrawals are counted on their own).
    let keys = [
        pool,
        t,
        m.wallet,
        donor.pubkey(),
        cranker.pubkey(),
        admin.pubkey(),
    ];
    let (mut donated, mut vault_donated) = (0u64, 0u64);
    let mut rng = Rng::new(0x0E5A_0D17);
    // Old sells that paid lamports, withdrawals that paid lamports, read-only refusals.
    let mut seen = [0u32; 3];
    for step in 0..300u32 {
        let op = rng.below(9);
        if op <= 1 {
            let mut d = vec![];
            if rng.below(2) == 0 {
                d.push((rng.range(1, 3_000_000), WALLET));
            }
            if rng.below(2) == 0 {
                d.push((rng.range(1, 3_000_000), LEGACY));
            }
            m.answer(pool, callback::AFTER_SWAP, &cut(&d));
        }
        let value = m.value(&keys);
        let mut owner_gain = 0u64;
        match op {
            0 | 1 => {
                let held = m.bal(&m.a, &t);
                let mut ix = m.v1_ix(&t, &pool, 0, rng.range(1, held / 8 + 2));
                let readonly = rng.below(4) == 0;
                if readonly {
                    ix.accounts[0].is_writable = false;
                }
                let l0 = m.lamports(&t);
                let trader = m.trader.insecure_clone();
                let tx = m.send(&[ix], &trader);
                if tx.result.is_ok() && m.lamports(&t) > l0 {
                    assert!(!readonly, "a read-only trader is never paid lamports");
                    seen[0] += 1;
                }
                if readonly && tx.result.is_err() && tx.custom() == code(SwapError::BufferShort) {
                    seen[2] += 1;
                }
            }
            2 => {
                let lp = m.bal(&lp_mint, &o);
                let mut ix = m.remove_ix(&pool, rng.range(1, lp / 20 + 2));
                let readonly = rng.below(4) == 0;
                if readonly {
                    ix.accounts[0].is_writable = false;
                }
                let l0 = m.lamports(&o);
                let owner = m.owner.insecure_clone();
                let tx = m.send(&[ix], &owner);
                owner_gain = m.lamports(&o) - l0;
                if tx.result.is_ok() && owner_gain > 0 {
                    assert!(!readonly, "a read-only provider is never paid lamports");
                    seen[1] += 1;
                }
                if readonly && tx.result.is_err() && tx.custom() == code(SwapError::BufferShort) {
                    seen[2] += 1;
                }
            }
            3 => {
                let p = m.state(&pool);
                let books = p.quote_reserve + p.protocol_fees_quote;
                let target = if rng.below(3) == 0 {
                    0
                } else {
                    rng.range(0, books + SOL)
                };
                m.send(
                    &[swap::set_buffer_target(admin.pubkey(), pool, target)],
                    &admin,
                )
                .ok();
            }
            4 => {
                let c = cranker.insecure_clone();
                m.send(&[swap::rebalance_pool(c.pubkey(), pool)], &c).ok();
            }
            5 => {
                let d = donor.insecure_clone();
                let amount = rng.range(1, SOL / 20);
                if rng.below(2) == 0 {
                    let tx = m.send(&[sys_transfer(&d.pubkey(), &pool, amount)], &d);
                    if tx.result.is_ok() {
                        donated += amount;
                    }
                } else {
                    let ix = token::transfer(
                        d.pubkey(),
                        h(&m.sol, &d.pubkey()),
                        swap::vault_address(&pool, &m.sol),
                        m.sol,
                        None,
                        vec![],
                        amount,
                    );
                    if m.send(&[ix], &d).result.is_ok() {
                        vault_donated += amount;
                    }
                }
            }
            6 => {
                let _ = m.v1(&pool, 1, rng.range(1_000_000, 2 * SOL));
            }
            7 => {
                let _ = m.v2(&pool, 1, rng.range(1_000_000, 2 * SOL));
            }
            _ => {
                let held = m.bal(&m.a, &t);
                let _ = m.v2(&pool, 0, rng.range(1, held / 8 + 2));
            }
        }
        m.inv(&pool, donated, vault_donated);
        assert_eq!(
            m.value(&keys) + u128::from(owner_gain),
            value,
            "value conserved at step {step} (op {op})"
        );
        if op <= 1 {
            m.silence(pool);
        }
        if m.lamports(&t) < 20 * SOL {
            let topped = m.lamports(&t) + 50 * SOL;
            m.w.env.fund(t, topped);
        }
        if m.bal(&m.a, &t) < BASE_DEPOSIT / 100 {
            let owner = m.owner.insecure_clone();
            let a = m.a;
            m.w.send_tokens(&owner, a, &t, BASE_DEPOSIT / 20).ok();
        }
    }
    println!(
        "lamport fallbacks: old sells {}, withdrawals {}, read-only refusals {}",
        seen[0], seen[1], seen[2]
    );
    assert!(seen.iter().all(|n| *n > 0), "every path ran: {seen:?}");
}
