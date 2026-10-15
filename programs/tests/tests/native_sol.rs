//! Native-SOL pools (`bordrless_swap::native`): a pool quoted in bridged SOL by its mint and seeds
//! that holds its quote as lamports on the pool account. `swap_v2` buys and sells in SOL with the
//! pool hook's quote-side cuts paid as lamports or, to an old-style holding of bridged SOL, wrapped;
//! the old `swap`, the liquidity instructions and `collect_protocol_fees` go through the buffer
//! (bridged SOL in the quote vault); `rebalance_pool` moves the buffer toward the admin's target;
//! `migrate_pool_to_native` and `revert_pool_to_bridged` round-trip a pool with reserves and fees.
//! After every instruction the invariant is asserted:
//!
//! native:  pool lamports == rent minimum + quote_reserve + protocol_fees_quote - buffer, and
//!          quote vault == buffer
//! bridged: pool lamports == rent minimum, and quote vault == quote_reserve + protocol_fees_quote
//!
//! (plus whatever was donated, where a test donates).

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{AnchorDeserialize, Discriminator};
use bordrless_hook::{pool_flags, Delta, ExtraAccount, HookReturn};
use bordrless_launch::client as launch;
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::hooks::{fixed, NewPool, SwapSpec};
use bordrless_program_tests::launch::VQ;
use bordrless_swap::client as swap;
use bordrless_swap::error::SwapError;
use bordrless_swap::events::{
    NativeDeltaPaid, PoolQuoteFormChanged, ProtocolFeesCollected, Swapped, SwappedNative,
};
use bordrless_swap::instructions::{
    AddLiquidityArgs, CreatePoolArgs, RemoveLiquidityArgs, SwapArgs,
};
use bordrless_swap::state::Pool;
use bordrless_token::client as token;
use bordrless_token::state::Mint;
use hook_tester::callback;
use solana_keypair::Keypair;
use solana_signer::Signer;

const SOL: u64 = 1_000_000_000;
const SUPPLY: u64 = 1_000_000_000_000_000;
const BASE_DEPOSIT: u64 = 1_000_000_000_000;
const QUOTE_DEPOSIT: u64 = 100 * SOL;
const SWAP_FLAGS: u16 = pool_flags::BEFORE_SWAP
    | pool_flags::AFTER_SWAP
    | pool_flags::BEFORE_SWAP_RETURNS_DELTA
    | pool_flags::AFTER_SWAP_RETURNS_DELTA;
/// The first registry extra of a hook_tester callback.
const X: u8 = 6;
/// Registry extras: a funded wallet, an old-style holding of bridged SOL, an unfunded wallet, a
/// holding of the base mint.
const WALLET: u8 = X;
const LEGACY: u8 = X + 1;
const EMPTY: u8 = X + 2;
const BASE_HOLDING: u8 = X + 3;

fn code(e: SwapError) -> u32 {
    u32::from(e)
}

fn h(mint: &Pubkey, owner: &Pubkey) -> Pubkey {
    token::holding_address(mint, owner)
}

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

/// A market: a plain base mint A, bridged SOL, an owner holding both, a trader, a fee payer (so
/// the trader's lamports move only by the swap), and the accounts the pool hook may pay.
struct M {
    w: World,
    owner: Keypair,
    trader: Keypair,
    payer: Keypair,
    a: Pubkey,
    sol: Pubkey,
    wallet: Pubkey,
    legacy_owner: Pubkey,
    empty: Pubkey,
    base_owner: Pubkey,
}

impl M {
    fn new() -> Self {
        let mut w = World::new();
        let owner = w.wallet_with_sol(1_000 * SOL);
        let trader = w.wallet_with_sol(50 * SOL);
        let payer = w.env.funded(100 * SOL);
        let a = w.mint_to_owner(&owner, 6, SUPPLY, "AAA");
        let sol = w.sol;
        let wallet = Pubkey::new_unique();
        w.env.fund(wallet, SOL);
        let legacy_owner = Pubkey::new_unique();
        let empty = Pubkey::new_unique();
        let base_owner = Pubkey::new_unique();
        w.holdings(&owner, sol, &[legacy_owner]);
        w.holdings(&owner, a, &[trader.pubkey(), base_owner]);
        w.send_tokens(&owner, a, &trader.pubkey(), 10 * BASE_DEPOSIT / 100)
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
            empty,
            base_owner,
        }
    }

    fn extras(&self) -> Vec<ExtraAccount> {
        vec![
            fixed(self.wallet, true),
            fixed(h(&self.sol, &self.legacy_owner), true),
            fixed(self.empty, true),
            fixed(h(&self.a, &self.base_owner), true),
        ]
    }

    /// An A/bridged-SOL pool with hook_tester as its hook, created bridged (the old way).
    fn pool(&mut self) -> Pubkey {
        let owner = self.owner.insecure_clone();
        let spec = NewPool {
            base: self.a,
            quote: self.sol,
            lp_fee_bps: 30,
            tester_flags: Some(SWAP_FLAGS),
            extras: self.extras(),
            base_amount: BASE_DEPOSIT,
            quote_amount: QUOTE_DEPOSIT,
        };
        let (pool, tx) = self.w.new_pool(&owner, &spec);
        tx.ok();
        self.inv(&pool, 0);
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

    fn sol_supply(&self) -> u64 {
        self.w.env.read::<Mint>(&self.sol).supply
    }

    /// The invariant (see the module), with `donated` lamports someone sent the pool.
    #[track_caller]
    fn inv(&self, pool: &Pubkey, donated: u64) {
        let p = self.state(pool);
        let rent = self.w.env.rent(Pool::LEN);
        let lamports = self.lamports(pool);
        let vault = self.bal(&self.sol, pool);
        let books = p.quote_reserve + p.protocol_fees_quote;
        if p.quote_native() {
            assert_eq!(
                lamports,
                rent + books - p.buffer_quote() + donated,
                "native pool lamports"
            );
            assert_eq!(vault, p.buffer_quote(), "native pool buffer");
            assert_eq!(p.lamports_quote(), Some(books - p.buffer_quote()));
        } else {
            assert_eq!(lamports, rent + donated, "bridged pool lamports");
            assert_eq!(vault, books, "bridged pool vault");
            assert_eq!(p.buffer_quote(), 0);
            assert_eq!(p.buffer_target(), 0);
        }
    }

    fn admin(&self) -> Keypair {
        self.w.env.deployer.insecure_clone()
    }

    /// Sends `ixs` paid by the fee payer, `signer` signing.
    fn send(&mut self, ixs: &[Instruction], signer: &Keypair) -> Tx {
        let payer = self.payer.insecure_clone();
        self.w.env.send_paid_by(ixs, &payer, &[signer])
    }

    /// The DEX's `set_config` with `paused`, everything else as it stands.
    fn pause_ix(&self, paused: bool) -> Instruction {
        let c: bordrless_swap::state::Config = self.w.env.read(&swap::config_address());
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

    /// `ix` signed by the admin inside a pause: pause, `ix`, unpause, in one transaction.
    fn paused(&mut self, ix: Instruction) -> Tx {
        let admin = self.admin();
        let ixs = [self.pause_ix(true), ix, self.pause_ix(false)];
        self.send(&ixs, &admin)
    }

    fn migrate(&mut self, pool: &Pubkey, target: u64) -> Tx {
        let admin = self.admin();
        let tx = self.paused(swap::migrate_pool_to_native(admin.pubkey(), *pool, target));
        self.w.env.warp(1);
        tx
    }

    fn revert(&mut self, pool: &Pubkey) -> Tx {
        let admin = self.admin();
        let tx = self.paused(swap::revert_pool_to_bridged(admin.pubkey(), *pool));
        self.w.env.warp(1);
        tx
    }

    fn rebalance(&mut self, pool: &Pubkey, cranker: &Keypair) -> Tx {
        let tx = self.send(&[swap::rebalance_pool(cranker.pubkey(), *pool)], cranker);
        self.w.env.warp(1);
        tx
    }

    fn set_target(&mut self, pool: &Pubkey, target: u64) -> Tx {
        let admin = self.admin();
        let tx = self.send(
            &[swap::set_buffer_target(admin.pubkey(), *pool, target)],
            &admin,
        );
        self.w.env.warp(1);
        tx
    }

    /// Scripts what a callback of `pool`'s hook answers.
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

    /// The `swap_v2` of `trader` on `pool`: a buy pays SOL from the trader's wallet and delivers
    /// base to `recipient`'s holding; a sell delivers SOL to `recipient`.
    fn v2_ix(
        &self,
        trader: &Pubkey,
        pool: &Pubkey,
        direction: u8,
        amount_in: u64,
        recipient: &Pubkey,
        with_bridge: bool,
    ) -> Instruction {
        let p = self.state(pool);
        let buy = direction == 1;
        let keys = swap::SwapV2Keys {
            trader: *trader,
            pool: *pool,
            base_mint: p.base_mint,
            trader_base: h(&p.base_mint, if buy { recipient } else { trader }),
            trader_quote: if buy { *trader } else { *recipient },
            hook_program: p.hook_program,
            base_mint_writable: false,
            with_bridge,
        };
        let extras = p
            .hook_program
            .map(|hook| {
                self.w
                    .env
                    .pool_hook_extras(&hook, pool, &p.base_mint, &p.quote_mint, trader)
            })
            .unwrap_or_default();
        let args = SwapArgs {
            direction,
            amount_in,
            min_amount_out: 0,
            in_hook_accounts: 0,
            out_hook_accounts: 0,
            hook_data: vec![],
        };
        swap::swap_v2(&keys, args, extras)
    }

    fn v2(&mut self, pool: &Pubkey, direction: u8, amount_in: u64, recipient: &Pubkey) -> Tx {
        let trader = self.trader.insecure_clone();
        let ix = self.v2_ix(
            &trader.pubkey(),
            pool,
            direction,
            amount_in,
            recipient,
            true,
        );
        let tx = self.send(&[ix], &trader);
        self.w.env.warp(1);
        tx
    }

    /// The old `swap` of the trader on `pool`.
    fn v1(&mut self, pool: &Pubkey, direction: u8, amount_in: u64) -> Tx {
        let trader = self.trader.insecure_clone();
        let ix = self
            .w
            .env
            .swap_ix(&SwapSpec::new(trader.pubkey(), *pool, direction, amount_in));
        let tx = self.send(&[ix], &trader);
        self.w.env.warp(1);
        tx
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
}

/// A pool with reserves and fees migrates (unwrapping its books onto its account, keeping the
/// buffer it is told), is idempotent, trades, takes a donation it never counts, and reverts (its
/// lamports wrapped back), idempotently, to a pool the old swap trades exactly as before.
#[test]
fn migrate_and_revert_round_trip_a_pool_with_reserves_and_fees() {
    let mut m = M::new();
    let pool = m.pool();
    let t = m.trader.pubkey();
    m.v1(&pool, 1, 3 * SOL).ok();
    m.inv(&pool, 0);
    m.v1(&pool, 0, m.bal(&m.a, &t) / 3).ok();
    m.inv(&pool, 0);
    let before = m.state(&pool);
    assert!(before.protocol_fees_quote > 0);
    let books = before.quote_reserve + before.protocol_fees_quote;
    let supply = m.sol_supply();
    let lamports = m.lamports(&pool);

    // Nobody but the admin migrates or reverts.
    let stranger = m.w.env.funded(SOL);
    let tx = m.send(
        &[swap::migrate_pool_to_native(stranger.pubkey(), pool, 0)],
        &stranger,
    );
    tx.expect_code(code(SwapError::NotAdmin));
    let tx = m.send(
        &[swap::revert_pool_to_bridged(stranger.pubkey(), pool)],
        &stranger,
    );
    tx.expect_code(code(SwapError::NotAdmin));
    // A bridged pool is refused by swap_v2 and rebalance.
    m.v2(&pool, 1, SOL, &t)
        .expect_code(code(SwapError::NotNativePool));
    m.rebalance(&pool, &stranger)
        .expect_code(code(SwapError::NotNativePool));
    m.set_target(&pool, SOL)
        .expect_code(code(SwapError::NotNativePool));

    // Not while the DEX trades.
    let admin = m.admin();
    m.send(
        &[swap::migrate_pool_to_native(admin.pubkey(), pool, 0)],
        &admin,
    )
    .expect_code(code(SwapError::NotPaused));

    // Migrate keeping 2 SOL as the buffer.
    let tx = m.migrate(&pool, 2 * SOL);
    tx.ok();
    let ev: PoolQuoteFormChanged = tx.event();
    assert_eq!(
        (
            ev.kind,
            ev.native,
            ev.unwrapped,
            ev.buffer_quote,
            ev.buffer_target
        ),
        (0, true, books - 2 * SOL, 2 * SOL, 2 * SOL)
    );
    let after = m.state(&pool);
    assert!(after.quote_native());
    assert_eq!(after.quote_reserve, before.quote_reserve);
    assert_eq!(after.protocol_fees_quote, before.protocol_fees_quote);
    assert_eq!(after.buffer_quote(), 2 * SOL);
    assert_eq!(m.lamports(&pool), lamports + books - 2 * SOL);
    assert_eq!(m.bal(&m.sol, &pool), 2 * SOL);
    assert_eq!(m.sol_supply(), supply - (books - 2 * SOL));
    m.inv(&pool, 0);
    // The data around the reserved bytes is unchanged (same layout, same size).
    assert_eq!(m.w.env.account(&pool).unwrap().data.len(), Pool::LEN);
    assert_eq!(
        (after.base_reserve, after.lp_supply, after.swap_count),
        (before.base_reserve, before.lp_supply, before.swap_count)
    );

    // Again: nothing changes.
    m.migrate(&pool, 0).ok();
    assert_eq!(m.state(&pool).buffer_quote(), 2 * SOL);
    assert_eq!(m.state(&pool).buffer_target(), 2 * SOL);
    m.inv(&pool, 0);

    // Trades both ways in SOL.
    m.v2(&pool, 1, 2 * SOL, &t).ok();
    m.inv(&pool, 0);
    m.v2(&pool, 0, m.bal(&m.a, &t) / 4, &t).ok();
    m.inv(&pool, 0);

    // Someone sends the pool SOL: it is never counted.
    let donor = m.w.env.funded(10 * SOL);
    m.w.env
        .send_paid_by(
            &[anchor_lang::solana_program::system_instruction::transfer(
                &donor.pubkey(),
                &pool,
                SOL,
            )],
            &donor,
            &[],
        )
        .ok();
    m.inv(&pool, SOL);
    m.v2(&pool, 1, SOL, &t).ok();
    m.inv(&pool, SOL);

    // Revert: the quote lamports are wrapped back into the vault; the admin's wallet, which wraps
    // them, ends where it started.
    let p = m.state(&pool);
    let lamports_quote = p.lamports_quote().unwrap();
    let admin_key = m.admin().pubkey();
    let admin_lamports = m.lamports(&admin_key);
    let supply = m.sol_supply();
    let tx = m.revert(&pool);
    tx.ok();
    let ev: PoolQuoteFormChanged = tx.event();
    assert_eq!((ev.kind, ev.native, ev.wrapped), (1, false, lamports_quote));
    assert_eq!(m.lamports(&admin_key), admin_lamports);
    assert_eq!(m.sol_supply(), supply + lamports_quote);
    assert!(!m.state(&pool).quote_native());
    m.inv(&pool, SOL);
    // Not while the DEX trades.
    m.send(
        &[swap::revert_pool_to_bridged(admin.pubkey(), pool)],
        &admin,
    )
    .expect_code(code(SwapError::NotPaused));
    // Again: nothing changes.
    m.revert(&pool).ok();
    m.inv(&pool, SOL);

    // The old swap trades it as a bridged pool again.
    m.v1(&pool, 1, SOL).ok();
    m.inv(&pool, SOL);
    m.v1(&pool, 0, m.bal(&m.a, &t) / 4).ok();
    m.inv(&pool, SOL);
    m.v2(&pool, 1, SOL, &t)
        .expect_code(code(SwapError::NotNativePool));
}

/// A `swap_v2` buy: the trader's wallet pays the cuts (lamports to a wallet, wrapped into an
/// old-style holding of bridged SOL) and the rest to the pool; the base side is as ever.
#[test]
fn swap_v2_buys_with_native_and_wrapped_cuts() {
    let mut m = M::new();
    let pool = m.pool();
    m.migrate(&pool, 0).ok();
    m.inv(&pool, 0);
    let t = m.trader.pubkey();
    let (c1, c2) = (10_000_000, 20_000_000);
    m.answer(
        pool,
        callback::BEFORE_SWAP,
        &cut(&[(c1, WALLET), (c2, LEGACY)], 0),
    );
    m.answer(
        pool,
        callback::AFTER_SWAP,
        &cut(&[(5_000, BASE_HOLDING)], 0),
    );
    let amount = 3 * SOL;
    let p0 = m.state(&pool);
    let (tl, wl, pl) = (m.lamports(&t), m.lamports(&m.wallet), m.lamports(&pool));
    let (legacy, supply, tb) = (
        m.bal(&m.sol, &m.legacy_owner),
        m.sol_supply(),
        m.bal(&m.a, &t),
    );
    let based = m.bal(&m.a, &m.base_owner);
    let tx = m.v2(&pool, 1, amount, &t);
    tx.ok();
    let received = amount - c1 - c2;
    assert_eq!(m.lamports(&t), tl - amount);
    assert_eq!(m.lamports(&m.wallet), wl + c1);
    assert_eq!(m.bal(&m.sol, &m.legacy_owner), legacy + c2);
    assert_eq!(m.sol_supply(), supply + c2);
    assert_eq!(m.lamports(&pool), pl + received);
    assert_eq!(m.bal(&m.a, &m.base_owner), based + 5_000);
    let ev: Swapped = tx.event();
    assert_eq!(ev.received_in, received);
    assert_eq!(ev.cuts_in, c1 + c2);
    assert_eq!(ev.deltas_in.len(), 2);
    assert_eq!(m.bal(&m.a, &t), tb + ev.delivered_out);
    let p1 = m.state(&pool);
    assert_eq!(
        p1.quote_reserve + p1.protocol_fees_quote,
        p0.quote_reserve + p0.protocol_fees_quote + received
    );
    let nev: SwappedNative = tx.event();
    assert_eq!(nev.quote_lamports, received);
    assert_eq!(
        nev.quote_deltas,
        vec![
            NativeDeltaPaid {
                account: m.wallet,
                amount: c1,
                wrapped: false
            },
            NativeDeltaPaid {
                account: h(&m.sol, &m.legacy_owner),
                amount: c2,
                wrapped: true
            },
        ]
    );
    assert_eq!(nev.pool_lamports, m.lamports(&pool));
    m.inv(&pool, 0);
}

/// A `swap_v2` sell: the pool's lamports pay the cuts and the delivery, to any account (here a
/// wallet that did not exist); the protocol fee stays on the pool.
#[test]
fn swap_v2_sells_with_native_and_wrapped_cuts() {
    let mut m = M::new();
    let pool = m.pool();
    m.migrate(&pool, SOL).ok();
    let t = m.trader.pubkey();
    let (d1, d2) = (3_000_000, 4_000_000);
    m.answer(
        pool,
        callback::AFTER_SWAP,
        &cut(&[(d1, WALLET), (d2, LEGACY)], 0),
    );
    let recipient = Pubkey::new_unique();
    let sell = m.bal(&m.a, &t) / 2;
    let p0 = m.state(&pool);
    let (tl, wl, pl) = (m.lamports(&t), m.lamports(&m.wallet), m.lamports(&pool));
    let (legacy, supply, tb) = (
        m.bal(&m.sol, &m.legacy_owner),
        m.sol_supply(),
        m.bal(&m.a, &t),
    );
    let tx = m.v2(&pool, 0, sell, &recipient);
    tx.ok();
    let ev: Swapped = tx.event();
    let nev: SwappedNative = tx.event();
    assert_eq!(ev.recipient, recipient);
    assert_eq!(m.bal(&m.a, &t), tb - sell);
    assert_eq!(m.lamports(&recipient), ev.delivered_out);
    assert_eq!(nev.quote_lamports, ev.delivered_out);
    assert_eq!(ev.delivered_out, ev.amount_out - ev.protocol_fee - d1 - d2);
    // The trader wrapped the old-style cut with the pool's lamports and ends where it started.
    assert_eq!(m.lamports(&t), tl);
    assert_eq!(m.lamports(&m.wallet), wl + d1);
    assert_eq!(m.bal(&m.sol, &m.legacy_owner), legacy + d2);
    assert_eq!(m.sol_supply(), supply + d2);
    assert_eq!(m.lamports(&pool), pl - d1 - d2 - ev.delivered_out);
    let p1 = m.state(&pool);
    assert_eq!(
        p1.protocol_fees_quote,
        p0.protocol_fees_quote + ev.protocol_fee
    );
    assert_eq!(p1.buffer_quote(), SOL);
    assert_eq!(
        nev.quote_deltas
            .iter()
            .map(|d| d.wrapped)
            .collect::<Vec<_>>(),
        vec![false, true]
    );
    m.inv(&pool, 0);
}

/// The old `swap` on a native pool, with its very accounts: a buy's bridged SOL joins the buffer,
/// a sell is paid from it, and a sell the buffer cannot cover fails with `BufferShort` and moves
/// nothing.
#[test]
fn the_old_swap_trades_a_native_pool_through_its_buffer() {
    let mut m = M::new();
    let pool = m.pool();
    m.migrate(&pool, 0).ok();
    m.inv(&pool, 0);
    let t = m.trader.pubkey();
    assert_eq!(m.state(&pool).buffer_quote(), 0);

    // A sell with an empty buffer.
    let sell = m.bal(&m.a, &t) / 10;
    m.v1(&pool, 0, sell)
        .expect_code(code(SwapError::BufferShort));
    m.inv(&pool, 0);

    // A buy with bridged SOL fills it.
    let pl = m.lamports(&pool);
    let tx = m.v1(&pool, 1, 5 * SOL);
    tx.ok();
    let ev: Swapped = tx.event();
    assert_eq!(m.state(&pool).buffer_quote(), ev.received_in);
    assert_eq!(m.bal(&m.sol, &pool), ev.received_in);
    assert_eq!(m.lamports(&pool), pl);
    m.inv(&pool, 0);

    // A sell it covers is paid from it.
    let tq = m.bal(&m.sol, &t);
    let tx = m.v1(&pool, 0, m.bal(&m.a, &t) / 20);
    tx.ok();
    let ev2: Swapped = tx.event();
    assert_eq!(m.bal(&m.sol, &t), tq + ev2.delivered_out);
    assert_eq!(
        m.state(&pool).buffer_quote(),
        ev.received_in - ev2.delivered_out
    );
    assert_eq!(m.lamports(&pool), pl);
    m.inv(&pool, 0);

    // One it cannot cover fails, whatever the lamports hold.
    let snapshot = (
        m.state(&pool).buffer_quote(),
        m.lamports(&pool),
        m.bal(&m.a, &t),
    );
    m.v1(&pool, 0, m.bal(&m.a, &t))
        .expect_code(code(SwapError::BufferShort));
    assert_eq!(
        snapshot,
        (
            m.state(&pool).buffer_quote(),
            m.lamports(&pool),
            m.bal(&m.a, &t)
        )
    );
    m.inv(&pool, 0);

    // Old-style cuts to holdings of bridged SOL on a sell leave the buffer too.
    m.answer(pool, callback::AFTER_SWAP, &cut(&[(1_000_000, LEGACY)], 0));
    let (b0, legacy) = (
        m.state(&pool).buffer_quote(),
        m.bal(&m.sol, &m.legacy_owner),
    );
    let tx = m.v1(&pool, 0, m.bal(&m.a, &t) / 50);
    tx.ok();
    let ev3: Swapped = tx.event();
    assert_eq!(m.bal(&m.sol, &m.legacy_owner), legacy + 1_000_000);
    assert_eq!(
        m.state(&pool).buffer_quote(),
        b0 - ev3.delivered_out - 1_000_000
    );
    m.inv(&pool, 0);
}

/// `rebalance_pool`: anyone moves the buffer toward the admin's target, never past it, and pays
/// nothing; a buffer above the target is unwrapped onto the pool, one below it filled from the
/// pool's lamports.
#[test]
fn rebalance_moves_the_buffer_toward_its_target() {
    let mut m = M::new();
    let pool = m.pool();
    m.migrate(&pool, SOL).ok();
    let cranker = m.w.env.funded(SOL);
    let cl = m.lamports(&cranker.pubkey());

    // At the target: nothing moves.
    let tx = m.rebalance(&pool, &cranker);
    tx.ok();
    let ev: PoolQuoteFormChanged = tx.event();
    assert_eq!((ev.kind, ev.unwrapped, ev.wrapped), (2, 0, 0));
    m.inv(&pool, 0);

    // Old buys raise the buffer above it; a rebalance unwraps the excess.
    m.v1(&pool, 1, 4 * SOL).ok();
    let b = m.state(&pool).buffer_quote();
    assert!(b > SOL);
    let pl = m.lamports(&pool);
    let tx = m.rebalance(&pool, &cranker);
    tx.ok();
    let ev: PoolQuoteFormChanged = tx.event();
    assert_eq!((ev.unwrapped, ev.wrapped), (b - SOL, 0));
    assert_eq!(m.state(&pool).buffer_quote(), SOL);
    assert_eq!(m.lamports(&pool), pl + b - SOL);
    m.inv(&pool, 0);

    // Nobody but the admin sets the target.
    let tx = m.send(
        &[swap::set_buffer_target(cranker.pubkey(), pool, 0)],
        &cranker,
    );
    tx.expect_code(code(SwapError::NotAdmin));

    // A higher target: the cranker wraps the pool's lamports into the vault and pays nothing.
    m.set_target(&pool, 10 * SOL).ok();
    let pl = m.lamports(&pool);
    let tx = m.rebalance(&pool, &cranker);
    tx.ok();
    let ev: PoolQuoteFormChanged = tx.event();
    assert_eq!((ev.unwrapped, ev.wrapped), (0, 9 * SOL));
    assert_eq!(m.state(&pool).buffer_quote(), 10 * SOL);
    assert_eq!(m.lamports(&pool), pl - 9 * SOL);
    assert_eq!(m.lamports(&cranker.pubkey()), cl);
    m.inv(&pool, 0);

    // A target above the whole quote: the buffer takes all of it, never more, and the pool keeps
    // its rent; swap_v2 sells then fail cleanly.
    m.set_target(&pool, u64::MAX).ok();
    m.rebalance(&pool, &cranker).ok();
    let p = m.state(&pool);
    assert_eq!(p.buffer_quote(), p.quote_reserve + p.protocol_fees_quote);
    assert_eq!(m.lamports(&pool), m.w.env.rent(Pool::LEN));
    m.inv(&pool, 0);
    let t = m.trader.pubkey();
    m.v2(&pool, 0, m.bal(&m.a, &t) / 10, &t)
        .expect_code(code(SwapError::LamportsShort));
    // Even a sell smaller than the rent the pool keeps cannot dip into it.
    m.v2(&pool, 0, 1_000, &t)
        .expect_code(code(SwapError::NativeInvariant));
    assert_eq!(m.lamports(&pool), m.w.env.rent(Pool::LEN));
    // And back to zero: everything unwrapped.
    m.set_target(&pool, 0).ok();
    m.rebalance(&pool, &cranker).ok();
    assert_eq!(m.state(&pool).buffer_quote(), 0);
    assert_eq!(m.lamports(&cranker.pubkey()), cl);
    m.inv(&pool, 0);
    m.v2(&pool, 0, m.bal(&m.a, &t) / 10, &t).ok();
    m.inv(&pool, 0);
}

/// Rent: a lamport recipient (the delivery or a cut) that would be left below the rent-exempt
/// minimum is refused with `RecipientBelowRent`, before the runtime fails it opaquely; at or above
/// it, an account that did not exist is created by the payment.
#[test]
fn recipients_below_rent_are_refused_clearly() {
    let mut m = M::new();
    let pool = m.pool();
    m.migrate(&pool, 0).ok();
    let t = m.trader.pubkey();
    let rent0 = m.w.env.rent(0);

    // A tiny sell to a wallet that does not exist.
    let fresh = Pubkey::new_unique();
    m.v2(&pool, 0, 1_000, &fresh)
        .expect_code(code(SwapError::RecipientBelowRent));
    assert_eq!(m.lamports(&fresh), 0);
    // The same to the trader's own wallet is fine.
    m.v2(&pool, 0, 1_000, &t).ok();
    m.inv(&pool, 0);

    // A cut too small for an account that does not exist, on a buy and on a sell.
    m.answer(pool, callback::BEFORE_SWAP, &cut(&[(1_000, EMPTY)], 0));
    m.v2(&pool, 1, SOL, &t)
        .expect_code(code(SwapError::RecipientBelowRent));
    m.silence(pool);
    m.answer(pool, callback::AFTER_SWAP, &cut(&[(1_000, EMPTY)], 0));
    m.v2(&pool, 0, m.bal(&m.a, &t) / 10, &t)
        .expect_code(code(SwapError::RecipientBelowRent));
    // At the minimum it is paid and the account exists.
    m.answer(pool, callback::AFTER_SWAP, &cut(&[(rent0, EMPTY)], 0));
    m.v2(&pool, 0, m.bal(&m.a, &t) / 10, &t).ok();
    assert_eq!(m.lamports(&m.empty), rent0);
    m.inv(&pool, 0);
    // Then any amount.
    m.answer(pool, callback::AFTER_SWAP, &cut(&[(1, EMPTY)], 0));
    m.v2(&pool, 0, m.bal(&m.a, &t) / 10, &t).ok();
    assert_eq!(m.lamports(&m.empty), rent0 + 1);
    m.inv(&pool, 0);
}

/// What `swap_v2` refuses: a buy paid by anyone but the trader, a cut to a holding of another
/// mint, a burn of the native side, an old-style cut without the bridge's accounts.
#[test]
fn swap_v2_refusals() {
    let mut m = M::new();
    let pool = m.pool();
    m.migrate(&pool, 0).ok();
    let t = m.trader.pubkey();
    let trader = m.trader.insecure_clone();

    // A buy whose quote account is not the trader.
    let mut ix = m.v2_ix(&t, &pool, 1, SOL, &t, false);
    ix.accounts[7] = AccountMeta::new(m.wallet, false);
    m.send(&[ix], &trader)
        .expect_code(code(SwapError::WrongNativeAccount));

    // A quote-side cut to a holding of the base mint.
    m.answer(
        pool,
        callback::BEFORE_SWAP,
        &cut(&[(1_000, BASE_HOLDING)], 0),
    );
    m.v2(&pool, 1, SOL, &t)
        .expect_code(code(SwapError::InvalidDeltaAccount));
    // A burn of lamports.
    m.answer(pool, callback::BEFORE_SWAP, &cut(&[], 1_000));
    let ix = m.v2_ix(&t, &pool, 1, SOL, &t, true);
    m.send(&[ix], &trader)
        .expect_code(code(SwapError::NativeBurn));
    // A sell delivering lamports to the pool, a vault or a holding (bridged SOL passed out of
    // habit).
    m.silence(pool);
    let sell = m.bal(&m.a, &t) / 10;
    for to in [pool, swap::vault_address(&pool, &m.sol), h(&m.sol, &t)] {
        let mut ix = m.v2_ix(&t, &pool, 0, sell, &t, false);
        ix.accounts[7] = AccountMeta::new(to, false);
        m.send(&[ix], &trader)
            .expect_code(code(SwapError::WrongNativeAccount));
    }
    // To an account another program owns, with data (a hook's script), it is paid.
    let script = hook_tester::client::script_address(&pool);
    let sl = m.lamports(&script);
    let mut ix = m.v2_ix(&t, &pool, 0, sell, &t, false);
    ix.accounts[7] = AccountMeta::new(script, false);
    let tx = m.send(&[ix], &trader);
    tx.ok();
    assert_eq!(
        m.lamports(&script),
        sl + tx.event::<Swapped>().delivered_out
    );
    m.w.env.warp(1);
    m.inv(&pool, 0);
    // An old-style cut without the bridge.
    m.answer(pool, callback::BEFORE_SWAP, &cut(&[(1_000_000, LEGACY)], 0));
    let ix = m.v2_ix(&t, &pool, 1, SOL, &t, false);
    m.send(&[ix], &trader)
        .expect_code(code(SwapError::BridgeAccountsMissing));
    // A lamport cut needs no bridge.
    m.answer(pool, callback::BEFORE_SWAP, &cut(&[(1_000_000, WALLET)], 0));
    let ix = m.v2_ix(&t, &pool, 1, SOL, &t, false);
    m.send(&[ix], &trader).ok();
    m.inv(&pool, 0);
}

/// Liquidity on a native pool goes through the buffer: an add's bridged SOL joins it, a removal is
/// paid from it (`BufferShort` when it cannot cover, until a rebalance fills it).
#[test]
fn liquidity_on_a_native_pool_goes_through_the_buffer() {
    let mut m = M::new();
    let pool = m.pool();
    m.migrate(&pool, 0).ok();
    let owner = m.owner.insecure_clone();
    let o = owner.pubkey();
    let lp_mint = swap::lp_mint_address(&pool);

    let tx = m.send(
        &[swap::add_liquidity(
            &m.liquidity_keys(&pool),
            AddLiquidityArgs {
                base_desired: BASE_DEPOSIT / 10,
                quote_desired: 50 * SOL,
                min_lp: 0,
                base_hook_accounts: 0,
                quote_hook_accounts: 0,
                hook_data: vec![],
            },
            m.pool_extras(&pool, &o),
        )],
        &owner,
    );
    tx.ok();
    let added = m.state(&pool).buffer_quote();
    assert!(added > 0);
    m.inv(&pool, 0);

    // Removing every LP the owner has asks for more quote than the buffer holds.
    let lp = m.bal(&lp_mint, &o);
    let remove = |m: &M, lp_amount| {
        swap::remove_liquidity(
            &m.liquidity_keys(&pool),
            RemoveLiquidityArgs {
                lp_amount,
                min_base: 0,
                min_quote: 0,
                base_hook_accounts: 0,
                quote_hook_accounts: 0,
                hook_data: vec![],
            },
            m.pool_extras(&pool, &o),
        )
    };
    let ix = remove(&m, lp);
    m.send(&[ix], &owner)
        .expect_code(code(SwapError::BufferShort));
    // A twentieth of it fits.
    let q0 = m.bal(&m.sol, &o);
    let ix = remove(&m, lp / 20);
    m.send(&[ix], &owner).ok();
    assert!(m.bal(&m.sol, &o) > q0);
    m.inv(&pool, 0);
    // A rebalance to a target above the whole quote lets the rest out.
    m.set_target(&pool, u64::MAX).ok();
    let cranker = m.w.env.funded(SOL);
    m.rebalance(&pool, &cranker).ok();
    let ix = remove(&m, m.bal(&lp_mint, &o));
    m.send(&[ix], &owner).ok();
    m.inv(&pool, 0);
}

/// Protocol fees of a native pool: `collect_protocol_fees_sol` pays the collector from the pool's
/// lamports (unwrapping from the buffer only what they lack); `collect_protocol_fees` pays bridged
/// SOL from the buffer, which must cover it.
#[test]
fn protocol_fees_of_a_native_pool() {
    let mut m = M::new();
    let pool = m.pool();
    let t = m.trader.pubkey();
    m.v1(&pool, 1, 5 * SOL).ok();
    m.migrate(&pool, 0).ok();
    m.v2(&pool, 1, 5 * SOL, &t).ok();
    m.v2(&pool, 0, m.bal(&m.a, &t) / 3, &t).ok();
    let fees = m.state(&pool).protocol_fees_quote;
    assert!(fees > 0);
    let admin = m.admin();
    let collector = admin.pubkey();

    // As SOL, from lamports.
    let cl = m.lamports(&collector);
    let stranger = m.w.env.funded(SOL);
    let pl = m.lamports(&pool);
    let tx = m.send(
        &[swap::collect_protocol_fees_sol(
            stranger.pubkey(),
            pool,
            collector,
        )],
        &stranger,
    );
    tx.ok();
    assert_eq!(tx.event::<ProtocolFeesCollected>().quote_amount, fees);
    assert_eq!(m.lamports(&collector), cl + fees);
    assert_eq!(m.lamports(&pool), pl - fees);
    assert_eq!(m.state(&pool).protocol_fees_quote, 0);
    m.inv(&pool, 0);

    // As SOL when the lamports hold none of the quote: the whole fee comes from the buffer.
    m.v2(&pool, 1, 5 * SOL, &t).ok();
    let fees = m.state(&pool).protocol_fees_quote;
    m.set_target(&pool, u64::MAX).ok();
    m.rebalance(&pool, &stranger).ok();
    assert_eq!(m.state(&pool).lamports_quote(), Some(0));
    let (cl, b) = (m.lamports(&collector), m.state(&pool).buffer_quote());
    let tx = m.send(
        &[swap::collect_protocol_fees_sol(
            stranger.pubkey(),
            pool,
            collector,
        )],
        &stranger,
    );
    tx.ok();
    assert_eq!(m.lamports(&collector), cl + fees);
    assert_eq!(m.state(&pool).buffer_quote(), b - fees);
    m.inv(&pool, 0);

    // In bridged SOL, from the buffer: refused while it is short, paid once it holds them.
    m.set_target(&pool, 0).ok();
    m.rebalance(&pool, &stranger).ok();
    m.v2(&pool, 1, 5 * SOL, &t).ok();
    let fees = m.state(&pool).protocol_fees_quote;
    m.holdings(&admin, &[collector]);
    let ix = m.w.env.collect_ix(&collector, &pool, &collector);
    m.send(std::slice::from_ref(&ix), &admin)
        .expect_code(code(SwapError::BufferShort));
    m.set_target(&pool, fees).ok();
    m.rebalance(&pool, &stranger).ok();
    let held = m.bal(&m.sol, &collector);
    m.w.env.warp(1);
    m.send(&[ix], &admin).ok();
    assert_eq!(m.bal(&m.sol, &collector), held + fees);
    assert_eq!(m.state(&pool).buffer_quote(), 0);
    m.inv(&pool, 0);
}

impl M {
    fn holdings(&mut self, payer: &Keypair, owners: &[Pubkey]) {
        let sol = self.sol;
        self.w.holdings(payer, sol, owners);
    }
}

/// `create_native_pool`: a pool native from the start, its quote deposit paid in SOL by the payer,
/// its quote vault created empty as the buffer; a quote that is not bridged SOL is refused.
#[test]
fn a_pool_can_be_created_native() {
    let mut m = M::new();
    let owner = m.owner.insecure_clone();
    let o = owner.pubkey();
    let b = m.w.mint_to_owner(&owner, 9, SUPPLY, "BBB");
    let args = |quote| CreatePoolArgs {
        lp_fee_bps: 25,
        hook_program: Pubkey::default(),
        hook_flags: 0,
        virtual_base: 0,
        virtual_quote: 0,
        base_amount: BASE_DEPOSIT,
        quote_amount: quote,
        base_hook_accounts: 0,
        quote_hook_accounts: 0,
        hook_data: vec![],
    };
    let (treasury, a) = (m.w.env.treasury.pubkey(), m.a);
    let keys = |quote_mint| swap::CreatePoolKeys {
        payer: o,
        authority: o,
        treasury,
        base_mint: a,
        quote_mint,
        hook_caller: None,
    };
    let tx = m.w.env.send_paid_by(
        &[swap::create_native_pool(&keys(b), args(SOL), vec![])],
        &owner,
        &[],
    );
    tx.expect_code(code(SwapError::NotBridgedSol));

    let pool = swap::pool_address(&m.a, &m.sol, 25, None);
    let (ol, oq) = (m.lamports(&o), m.bal(&m.sol, &o));
    let tx = m.w.env.send_paid_by(
        &[swap::create_native_pool(
            &keys(m.sol),
            args(10 * SOL),
            vec![],
        )],
        &owner,
        &[],
    );
    tx.ok();
    let p = m.state(&pool);
    assert!(p.quote_native());
    assert_eq!(p.quote_reserve, 10 * SOL);
    assert_eq!(m.bal(&m.sol, &o), oq);
    assert!(m.lamports(&o) < ol - 10 * SOL);
    assert_eq!(m.lamports(&pool), m.w.env.rent(Pool::LEN) + 10 * SOL);
    assert_eq!(
        m.w.env
            .account(&swap::vault_address(&pool, &m.sol))
            .map(|a| a.owner),
        Some(bordrless_token::ID)
    );
    m.inv(&pool, 0);
    let t = m.trader.pubkey();
    m.v2(&pool, 1, SOL, &t).ok();
    m.inv(&pool, 0);
    m.v2(&pool, 0, m.bal(&m.a, &t) / 10, &t).ok();
    m.inv(&pool, 0);
    m.v1(&pool, 1, SOL).ok();
    m.inv(&pool, 0);
}

/// The `swap_v2` form of a DEX `swap` instruction (a launch client's), the SOL going to and from
/// `trader_quote`.
fn to_v2(ix: &Instruction, trader_quote: Pubkey) -> Instruction {
    let a = &ix.accounts;
    let args = SwapArgs::deserialize(&mut &ix.data[8..]).unwrap();
    let keys = swap::SwapV2Keys {
        trader: a[0].pubkey,
        pool: a[2].pubkey,
        base_mint: a[3].pubkey,
        trader_base: a[7].pubkey,
        trader_quote,
        hook_program: Some(a[9].pubkey),
        base_mint_writable: a[3].is_writable,
        with_bridge: true,
    };
    assert_eq!(
        &ix.data[..8],
        bordrless_swap::instruction::Swap::DISCRIMINATOR
    );
    swap::swap_v2(&keys, args, a[15..].to_vec())
}

/// A live launch pool (the launchpad as its hook, its creator fee paid to the launch's holding of
/// bridged SOL) migrates mid-curve, trades through both swaps (the launchpad's old-style cuts
/// wrapped on `swap_v2`), graduates through the buffer (`finalize_curve` counting lamports and
/// buffer) and keeps trading.
#[test]
fn a_migrated_launch_pool_trades_and_graduates() {
    let mut m = M::new();
    let creator = m.w.wallet_with_sol(20 * SOL);
    let (mint, tx) = m.w.create_launch(&creator, "NAT", 100, VQ);
    tx.ok();
    m.w.env.warp(31);
    let trader = m.w.wallet_with_sol(30 * SOL);
    let t = trader.pubkey();
    m.w.holdings(&trader, mint, &[t]);
    m.w.buy(&trader, &mint, SOL).ok();
    let pool = launch::pool_address(&mint, &m.sol, bordrless_core::policy::LP_FEE_BPS);
    m.inv(&pool, 0);
    m.migrate(&pool, 0).ok();
    m.inv(&pool, 0);
    assert!(m.state(&pool).curve);

    // A swap_v2 buy: the trader pays SOL; the launchpad's cuts land wrapped where they always did.
    let supply = m.sol_supply();
    let tl = m.lamports(&t);
    let ix = to_v2(&m.w.launch_swap_ix(&t, &mint, 1, 2 * SOL, 0), t);
    let tx = m.send(&[ix], &trader);
    tx.ok();
    m.w.env.warp(1);
    let nev: SwappedNative = tx.event();
    let wrapped: u64 = nev
        .quote_deltas
        .iter()
        .filter(|d| d.wrapped)
        .map(|d| d.amount)
        .sum();
    assert!(
        wrapped > 0,
        "the creator fee is paid to the launch's holding"
    );
    assert_eq!(m.sol_supply(), supply + wrapped);
    assert_eq!(m.lamports(&t), tl - 2 * SOL);
    m.inv(&pool, 0);

    // A swap_v2 sell.
    let held = m.bal(&mint, &t);
    let ix = to_v2(&m.w.launch_swap_ix(&t, &mint, 0, held / 2, 0), t);
    m.send(&[ix], &trader).ok();
    m.w.env.warp(1);
    m.inv(&pool, 0);

    // The old buys (bridged SOL into the buffer) fill the curve, graduate, and the pool keeps
    // trading both ways.
    let (_, tx) = m.w.graduate_launch(&mint);
    tx.ok();
    let p = m.state(&pool);
    assert!(!p.curve);
    assert!(p.quote_native());
    assert!(p.lp_supply > 0);
    m.inv(&pool, 0);
    // Its quote is now mostly buffer: a large SOL sell waits for a rebalance.
    let held = m.bal(&mint, &t);
    let ix = to_v2(&m.w.launch_swap_ix(&t, &mint, 0, held / 2, 0), t);
    m.send(&[ix], &trader)
        .expect_code(code(SwapError::LamportsShort));
    let cranker = m.w.env.funded(SOL);
    m.rebalance(&pool, &cranker).ok();
    assert_eq!(m.state(&pool).buffer_quote(), 0);
    m.inv(&pool, 0);
    let ix = to_v2(&m.w.launch_swap_ix(&t, &mint, 1, SOL, 0), t);
    m.send(&[ix], &trader).ok();
    m.inv(&pool, 0);
    m.w.env.warp(1);
    let held = m.bal(&mint, &t);
    let ix = to_v2(&m.w.launch_swap_ix(&t, &mint, 0, held / 2, 0), t);
    m.send(&[ix], &trader).ok();
    m.inv(&pool, 0);
}
