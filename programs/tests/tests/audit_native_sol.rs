//! Native-SOL upgrade audit (branch `native-sol-audit`, `docs/native-sol-audit.md`): adversarial
//! LiteSVM tests against the native-SOL paths of the DEX, the launchpad, the kit, the companion,
//! hook_vault and the router. Each test either proves a finding (its name starts `finding_`) or
//! rules an attack out (`ruled_out_`). A finding test asserts today's (faulty) behaviour, so it
//! passes on `native-sol` and is expected to fail once the finding is fixed.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_hook::{pool_flags, Delta, ExtraAccount, HookReturn};
use bordrless_kit::state::KitConfig;
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
use bordrless_swap::instructions::SwapArgs;
use bordrless_swap::state::Pool;
use bordrless_token::client as token;
use hook_tester::callback;
use solana_keypair::Keypair;
use solana_signer::Signer;

const SYSTEM: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");

/// A plain system transfer.
fn sys_transfer(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: SYSTEM,
        accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
        data,
    }
}

fn admin(w: &World) -> Keypair {
    w.env.deployer.insecure_clone()
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
    let a = admin(w);
    let tx = w.env.send(ixs, &[&a]);
    w.env.warp(1);
    tx
}

fn pause(w: &mut World, paused: bool) {
    let ix = dex_pause_ix(w, paused);
    as_admin(w, &[ix]).ok();
}

fn pool_of(w: &World, mint: &Pubkey) -> Pubkey {
    w.launch_pool_key(mint)
}

/// The runbook's first two steps for one coin, the DEX paused around them.
fn migrate(w: &mut World, mint: &Pubkey) -> Tx {
    let a = admin(w).pubkey();
    let modules = w.launch(mint).modules;
    let pool = pool_of(w, mint);
    let sol = w.sol;
    pause(w, true);
    as_admin(w, &[swap::migrate_pool_to_native(a, pool, 0)]).ok();
    let tx = as_admin(
        w,
        &[launch::migrate_launch_to_native(a, *mint, sol, modules)],
    );
    pause(w, false);
    tx
}

fn v2_ix(w: &World, trader: &Pubkey, mint: &Pubkey, direction: u8, amount: u64) -> Instruction {
    let keys = w.launch_keys(mint);
    let slice = w.launch_base_slice(mint, trader, trader, direction == 1);
    let ix = launch::swap_v2_with_base_slice(&keys, *trader, *trader, direction, amount, 0, slice);
    w.with_quote_cut(ix, mint, trader)
}

fn send(w: &mut World, payer: &Keypair, ix: Instruction, signer: &Keypair) -> Tx {
    let tx = w.env.send_paid_by(&[ix], payer, &[signer]);
    w.env.warp(1);
    tx
}

#[track_caller]
fn pool_inv(w: &World, pool: &Pubkey) {
    let p: Pool = w.env.read(pool);
    let vault = w.env.holding(&w.sol, pool);
    if p.quote_native() {
        assert_eq!(
            w.env.lamports(pool),
            w.env.rent(Pool::LEN) + p.quote_reserve + p.protocol_fees_quote - p.buffer_quote(),
            "native pool lamports"
        );
        assert_eq!(vault, p.buffer_quote(), "native pool buffer");
    } else {
        assert_eq!(w.env.lamports(pool), w.env.rent(Pool::LEN));
        assert_eq!(vault, p.quote_reserve + p.protocol_fees_quote);
    }
}

fn launch_fees(w: &World, mint: &Pubkey) -> (u64, u64) {
    let key = launch::launch_address(mint);
    (
        w.env.lamports(&key) - w.env.rent(Launch::LEN),
        w.env.holding(&w.sol, &key),
    )
}

// ---------------------------------------------------------------------------------------------
// Kit: a lamport donation to a native kit without holder rewards blocks the rollback.
// ---------------------------------------------------------------------------------------------

/// FINDING (kit `set_native(false)`): a native kit whose config holds lamports above its base and
/// has no holder rewards (no reward vault) can never turn back: `set_native(false)` debits the
/// lamports and then asks for the vault (`MissingRewardVault`). Anyone can send the kit config a
/// lamport, so anyone can permanently block `revert_launch_to_bridged` for every native coin whose
/// kit has no holder rewards (11 of the 66 kit configs in the cloned-mainnet dump).
#[test]
fn fixed_kit_donation_no_longer_blocks_launch_revert_without_holder_rewards() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(20 * SOL);
    let no_rewards = bordrless_program_tests::launch::rules(0, 0, 0, 0, 200, 86_400, 0, 0);
    let (mint, tx) = w.create_launch_with(&creator, "NRW", 100, VQ, no_rewards);
    tx.ok();
    assert_eq!(
        w.launch(&mint).modules & bordrless_kit::modules::HOLDER_REWARDS,
        0
    );
    w.env.warp(31);
    let a = w.wallet_with_sol(30 * SOL);
    w.holdings(&a, mint, &[a.pubkey(), creator.pubkey()]);
    w.buy(&a, &mint, SOL / 10).ok();
    migrate(&mut w, &mint).ok();
    let kit_key = launch::kit_config_address(&mint);
    let kit: KitConfig = w.env.read(&kit_key);
    assert!(kit.is_native());

    // A griefer sends the kit config one lamport.
    let griefer = w.env.funded(SOL);
    w.env
        .send(&[sys_transfer(&griefer.pubkey(), &kit_key, 1)], &[&griefer])
        .ok();
    w.env.warp(1);

    // The rollback of the launch now fails, for good.
    let ad = admin(&w).pubkey();
    let modules = w.launch(&mint).modules;
    let sol = w.sol;
    pause(&mut w, true);
    let tx = as_admin(
        &mut w,
        &[launch::revert_launch_to_bridged(ad, mint, sol, modules)],
    );
    // Fixed (N-1): the revert skips the move without holder rewards; the donation stays on the
    // config, uncounted.
    tx.ok();
    assert!(!w.launch(&mint).is_native());
    let kit: KitConfig = w.env.read(&kit_key);
    assert!(!kit.is_native());
}

// ---------------------------------------------------------------------------------------------
// Launchpad: the partner key of a partner config receives lamports directly.
// ---------------------------------------------------------------------------------------------

/// A native launch from a partner config naming `partner`, with fees waiting. Answers the mint,
/// the creator and the config.
fn partner_launch(w: &mut World, partner: Pubkey) -> (Pubkey, Keypair, Pubkey) {
    let ad = admin(w).pubkey();
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
    w.env.send_paid_by(&[ix], &author, &[&config]).ok();
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
    let ix = v2_ix(w, &t.pubkey(), &mint, 1, 5 * SOL);
    send(w, &payer, ix, &t).ok();
    assert!(launch_fees(w, &mint).0 > 0);
    (mint, creator, config.pubkey())
}

/// FINDING (launchpad `pay_sol` to `PartnerFee.partner`): the `_sol` claims pay the partner's part
/// as lamports straight to the partner key, which `create_partner_config` takes unchecked (any key
/// but the default). A partner key that can't take lamports (an executable program, or a key the
/// runtime always demotes to read-only, like a sysvar) makes every SOL claim of every native
/// launch from that config fail, the creator's included: its fees are locked until the admin
/// reverts the launch to bridged (which needs the DEX paused). The bridged claims paid the
/// partner's holding of bridged SOL, which exists for any owner key.
#[test]
fn finding_partner_key_that_cannot_take_lamports_locks_native_claims() {
    // A sysvar: the runtime demotes it to read-only in every transaction, so `pay_sol` refuses it.
    let partner = Pubkey::from_str_const("SysvarC1ock11111111111111111111111111111111");
    let mut w = World::new();
    let (mint, creator, cfg) = partner_launch(&mut w, partner);
    let payer = w.env.funded(SOL);
    let (fees, _) = launch_fees(&w, &mint);
    let tx = send(
        &mut w,
        &payer,
        launch::claim_creator_fees_sol(creator.pubkey(), mint, None, Some((cfg, partner))),
        &creator,
    );
    tx.expect_code(u32::from(LaunchError::PartnerAccountsMissing));
    let anyone = w.env.funded(SOL);
    let tx = w.env.send_paid_by(
        &[launch::distribute_fees_sol(
            mint,
            cfg,
            creator.pubkey(),
            partner,
            None,
        )],
        &anyone,
        &[],
    );
    // Anchor's `mut` constraint on `partner_wallet` (ConstraintMut).
    tx.expect_code(2000);
    assert_eq!(
        launch_fees(&w, &mint).0,
        fees,
        "the creator's fees stay locked"
    );
}

// ---------------------------------------------------------------------------------------------
// The DEX: a hook_tester pool (scripted cuts), as `native_sol.rs` builds it.
// ---------------------------------------------------------------------------------------------

const SUPPLY: u64 = 1_000_000_000_000_000;
const BASE_DEPOSIT: u64 = 1_000_000_000_000;
const QUOTE_DEPOSIT: u64 = 100 * SOL;
const SWAP_FLAGS: u16 = pool_flags::BEFORE_SWAP
    | pool_flags::AFTER_SWAP
    | pool_flags::BEFORE_SWAP_RETURNS_DELTA
    | pool_flags::AFTER_SWAP_RETURNS_DELTA;
const X: u8 = 6;
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
        let trader = w.wallet_with_sol(200 * SOL);
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

    fn pool_with(&mut self, extras: Vec<ExtraAccount>) -> Pubkey {
        let owner = self.owner.insecure_clone();
        let spec = NewPool {
            base: self.a,
            quote: self.sol,
            lp_fee_bps: 30,
            tester_flags: Some(SWAP_FLAGS),
            extras,
            base_amount: BASE_DEPOSIT,
            quote_amount: QUOTE_DEPOSIT,
        };
        let (pool, tx) = self.w.new_pool(&owner, &spec);
        tx.ok();
        pool
    }

    fn pool(&mut self) -> Pubkey {
        let extras = self.extras();
        self.pool_with(extras)
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

    /// The invariant with `donated` lamports and `vault_donated` bridged SOL sent from outside.
    #[track_caller]
    fn inv2(&self, pool: &Pubkey, donated: u64, vault_donated: u64) {
        let p = self.state(pool);
        let rent = self.w.env.rent(Pool::LEN);
        let books = p.quote_reserve + p.protocol_fees_quote;
        let vault = self.bal(&self.sol, pool);
        if p.quote_native() {
            assert_eq!(
                self.lamports(pool),
                rent + books - p.buffer_quote() + donated,
                "native pool lamports"
            );
            assert_eq!(vault, p.buffer_quote() + vault_donated, "native buffer");
        } else {
            assert_eq!(self.lamports(pool), rent + donated, "bridged pool lamports");
            assert_eq!(vault, books + vault_donated, "bridged vault");
        }
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

    fn pause_ix(&self, paused: bool) -> Instruction {
        dex_pause_ix(&self.w, paused)
    }

    fn paused(&mut self, ix: Instruction) -> Tx {
        let admin = self.admin();
        let ixs = [self.pause_ix(true), ix, self.pause_ix(false)];
        self.send(&ixs, &admin)
    }

    fn migrate(&mut self, pool: &Pubkey, target: u64) -> Tx {
        let admin = self.admin();
        self.paused(swap::migrate_pool_to_native(admin.pubkey(), *pool, target))
    }

    fn revert(&mut self, pool: &Pubkey) -> Tx {
        let admin = self.admin();
        self.paused(swap::revert_pool_to_bridged(admin.pubkey(), *pool))
    }

    fn rebalance(&mut self, pool: &Pubkey, cranker: &Keypair) -> Tx {
        self.send(&[swap::rebalance_pool(cranker.pubkey(), *pool)], cranker)
    }

    fn set_target(&mut self, pool: &Pubkey, target: u64) -> Tx {
        let admin = self.admin();
        self.send(
            &[swap::set_buffer_target(admin.pubkey(), *pool, target)],
            &admin,
        )
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

    fn v2_ix_by(
        &self,
        trader: &Pubkey,
        pool: &Pubkey,
        direction: u8,
        amount_in: u64,
        recipient: &Pubkey,
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
            with_bridge: true,
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
        let ix = self.v2_ix_by(&trader.pubkey(), pool, direction, amount_in, recipient);
        self.send(&[ix], &trader)
    }

    fn v1(&mut self, pool: &Pubkey, direction: u8, amount_in: u64) -> Tx {
        let trader = self.trader.insecure_clone();
        let ix = self
            .w
            .env
            .swap_ix(&SwapSpec::new(trader.pubkey(), *pool, direction, amount_in));
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

    /// Value outside the bridge: every account's lamports but the bridge's SOL vault, plus the
    /// bridged SOL in circulation. Wrapping and unwrapping never change it; only fees paid by the
    /// fee payer do (the payer is left out).
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
}

/// RULED OUT (lamport theft or creation, rounding, double counting of buffer vs lamports): a seeded
/// walk of 400 random operations on one native pool, every kind that touches its quote (both swaps
/// in both directions with random lamport and wrapped quote-side cuts and base-side cuts, rebalances
/// by strangers, random buffer targets, liquidity in and out, both protocol-fee collections,
/// donations of lamports and of bridged SOL, migrate/revert round trips). After every instruction,
/// failed or not, the invariant holds with *equality* (lamports = rent + books - buffer + donations;
/// vault = buffer + bridged donations), and the value of every account involved plus the bridged
/// supply is exactly conserved (the bridge backing lamport for lamport). So no instruction mints or
/// burns a lamport, and the pool never pays out anything its books did not count.
#[test]
fn ruled_out_seeded_walk_conserves_every_lamport_on_a_native_pool() {
    use bordrless_program_tests::kit::Rng;
    use bordrless_swap::instructions::{AddLiquidityArgs, RemoveLiquidityArgs};
    let mut m = M::new();
    let pool = m.pool();
    let t = m.trader.pubkey();
    let owner = m.owner.pubkey();
    let admin = m.admin().pubkey();
    let lp_mint = swap::lp_mint_address(&pool);
    let cranker = m.w.env.funded(10 * SOL);
    let donor = m.w.wallet_with_sol(50 * SOL);
    let recipients: Vec<Pubkey> = (0..3).map(|_| Pubkey::new_unique()).collect();
    {
        let o = m.owner.insecure_clone();
        let sol = m.sol;
        m.w.holdings(&o, sol, &[admin, t]);
        let a = m.a;
        m.w.holdings(&o, a, &recipients);
    }
    m.migrate(&pool, 0).ok();
    let (mut donated, mut vault_donated) = (0u64, 0u64);
    m.inv2(&pool, donated, vault_donated);
    let sol_vault = Pubkey::find_program_address(&[b"sol-vault"], &bordrless_bridge::ID).0;
    // The owner pays the hook scripts' fees, so it stays out of the value.
    let mut keys = vec![
        pool,
        t,
        admin,
        m.wallet,
        m.empty,
        cranker.pubkey(),
        donor.pubkey(),
    ];
    keys.extend(recipients.iter().copied());
    let _ = sol_vault;
    let mut rng = Rng::new(0xA0D1_7001);
    let mut ok = [0u32; 16];
    let rent0 = m.w.env.rent(0);
    for step in 0..400u32 {
        let op = rng.below(16) as usize;
        // Random hook answers for this step.
        let quote_cut = |rng: &mut Rng| -> HookReturn {
            let mut d = vec![];
            if rng.below(2) == 0 {
                d.push((rng.range(1, 5_000_000), WALLET));
            }
            if rng.below(2) == 0 {
                d.push((rng.range(1, 5_000_000), LEGACY));
            }
            if rng.below(4) == 0 {
                d.push((rng.range(rent0, rent0 + 1_000_000), EMPTY));
            }
            cut(&d, 0)
        };
        let base_cut = |rng: &mut Rng| -> HookReturn {
            if rng.below(2) == 0 {
                cut(&[(rng.range(1, 1_000_000), BASE_HOLDING)], 0)
            } else {
                cut(&[], 0)
            }
        };
        let (qb, qa, bb, ba) = (
            quote_cut(&mut rng),
            quote_cut(&mut rng),
            base_cut(&mut rng),
            base_cut(&mut rng),
        );
        let buy = rng.below(2) == 1;
        // A buy cuts the quote before and the base after; a sell the base before, the quote after.
        if buy {
            m.answer(pool, callback::BEFORE_SWAP, &qb);
            m.answer(pool, callback::AFTER_SWAP, &ba);
        } else {
            m.answer(pool, callback::BEFORE_SWAP, &bb);
            m.answer(pool, callback::AFTER_SWAP, &qa);
        }
        let held = m.bal(&m.a, &t);
        let value = m.value(&keys);
        let mut owner_gain = 0u128;
        let tx = match op {
            0..=2 => {
                let amount = rng.range(1_000_000, 3 * SOL);
                if buy {
                    m.v2(&pool, 1, amount, &t)
                } else {
                    let to = recipients[rng.below(3) as usize];
                    m.v2(&pool, 0, rng.range(1, held / 5 + 1), &to)
                }
            }
            3..=5 => {
                if buy {
                    m.v1(&pool, 1, rng.range(1_000_000, 3 * SOL))
                } else {
                    m.v1(&pool, 0, rng.range(1, held / 5 + 1))
                }
            }
            6 | 7 => m.rebalance(&pool, &cranker),
            8 => {
                let target = rng.range(0, 30 * SOL);
                m.set_target(&pool, target)
            }
            9 => {
                let o = m.owner.insecure_clone();
                let ix = swap::add_liquidity(
                    &m.liquidity_keys(&pool),
                    AddLiquidityArgs {
                        base_desired: rng.range(1, BASE_DEPOSIT / 50),
                        quote_desired: rng.range(1_000_000, 5 * SOL),
                        min_lp: 0,
                        base_hook_accounts: 0,
                        quote_hook_accounts: 0,
                        hook_data: vec![],
                    },
                    m.pool_extras(&pool, &o.pubkey()),
                );
                m.send(&[ix], &o)
            }
            10 => {
                let o = m.owner.insecure_clone();
                let lp = m.bal(&lp_mint, &o.pubkey());
                let ix = swap::remove_liquidity(
                    &m.liquidity_keys(&pool),
                    RemoveLiquidityArgs {
                        lp_amount: rng.range(1, lp / 10 + 1),
                        min_base: 0,
                        min_quote: 0,
                        base_hook_accounts: 0,
                        quote_hook_accounts: 0,
                        hook_data: vec![],
                    },
                    m.pool_extras(&pool, &o.pubkey()),
                );
                // A withdrawal the buffer can't cover pays the owner lamports (N-4 fix): count
                // what the owner gained (it is otherwise outside the value).
                let before = m.lamports(&owner);
                let tx = m.send(&[ix], &o);
                owner_gain = u128::from(m.lamports(&owner) - before);
                tx
            }
            11 => {
                let c = cranker.insecure_clone();
                m.send(
                    &[swap::collect_protocol_fees_sol(c.pubkey(), pool, admin)],
                    &c,
                )
            }
            12 => {
                let a = m.admin();
                let ix = m.w.env.collect_ix(&admin, &pool, &admin);
                m.send(&[ix], &a)
            }
            13 => {
                // A donation of lamports to the pool, or of bridged SOL to its vault.
                let d = donor.insecure_clone();
                let amount = rng.range(1, SOL / 10);
                if rng.below(2) == 0 {
                    let tx = m.send(&[sys_transfer(&d.pubkey(), &pool, amount)], &d);
                    if tx.result.is_ok() {
                        donated += amount;
                    }
                    tx
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
                    let tx = m.send(&[ix], &d);
                    if tx.result.is_ok() {
                        vault_donated += amount;
                    }
                    tx
                }
            }
            14 => {
                // A round trip through bridged and back.
                let r = m.revert(&pool);
                if r.result.is_ok() {
                    m.inv2(&pool, donated, vault_donated);
                    // The revert wraps the books, not the donations: a re-migration keeps the
                    // donated lamports on the pool, uncounted, and the bridged donations in the
                    // vault (only `migrate`'s `min(target, books)` counts as buffer).
                }
                m.migrate(&pool, rng.range(0, 10 * SOL))
            }
            _ => {
                // Old and new swaps by a second signer with the cut accounts aliased to it is
                // covered elsewhere; here a plain buy by the trader to a recipient's holding.
                let to = recipients[rng.below(3) as usize];
                let trader = m.trader.insecure_clone();
                let ix = m.v2_ix_by(&t, &pool, 1, rng.range(1_000_000, SOL), &to);
                m.send(&[ix], &trader)
            }
        };
        if tx.result.is_ok() {
            ok[op] += 1;
        }
        m.inv2(&pool, donated, vault_donated);
        assert_eq!(
            m.value(&keys) + owner_gain,
            value,
            "value conserved at step {step} (op {op})"
        );
        if m.lamports(&t) < 20 * SOL {
            // Keep the trader solvent (outside the books, and outside the value).
            m.w.env.fund(t, m.lamports(&t) + 50 * SOL);
        }
    }
    // Every kind of operation succeeded at least once.
    for (op, n) in ok.iter().enumerate() {
        assert!(*n > 0, "op {op} never succeeded");
    }
    m.silence(pool);
}

// ---------------------------------------------------------------------------------------------
// Account substitution and aliasing on `swap_v2` and the old swap of a native pool.
// ---------------------------------------------------------------------------------------------

/// RULED OUT (account substitution, duplicate accounts): a pool hook (here a scripted one, the
/// worst case: it names whatever the trader passes at an index) can't make the DEX pay a native
/// cut to the pool itself, its quote or base vault, the trader's wallet, the trader's delivery
/// account, an executable account, or a token-program account that is not a holding of bridged
/// SOL (the bridged-SOL mint); nor can a sell deliver to an executable account. Every such swap
/// fails and moves nothing; both swaps, both directions.
#[test]
fn ruled_out_native_cuts_to_the_pool_vaults_trader_or_executables() {
    let mut m = M::new();
    let t = m.trader.pubkey();
    let (pool_key, _) = Pool::address(&m.a, &m.sol, 30, Some(hook_tester::ID));
    let quote_vault = swap::vault_address(&pool_key, &m.sol);
    let base_vault = swap::vault_address(&pool_key, &m.a);
    let recipient = Pubkey::new_unique();
    m.w.env.fund(recipient, SOL);
    // Extras at X..: the pool, its quote vault, its base vault, the trader's wallet, an executable
    // (the hook program), the bridged-SOL mint, the recipient's wallet, the swap config.
    let extras = vec![
        fixed(pool_key, true),
        fixed(quote_vault, true),
        fixed(base_vault, true),
        fixed(t, true),
        fixed(hook_tester::ID, false),
        fixed(m.sol, true),
        fixed(recipient, true),
        fixed(swap::config_address(), true),
    ];
    let pool = m.pool_with(extras);
    assert_eq!(pool, pool_key);
    m.migrate(&pool, 0).ok();
    // Some lamports for the old swap's sells to test against.
    m.set_target(&pool, 10 * SOL).ok();
    let cranker = m.w.env.funded(SOL);
    m.rebalance(&pool, &cranker).ok();
    m.inv2(&pool, 0, 0);
    let snapshot = |m: &M| {
        (
            m.lamports(&pool),
            m.bal(&m.sol, &pool),
            m.lamports(&t),
            m.lamports(&recipient),
            m.state(&pool).quote_reserve,
        )
    };
    let sell = m.bal(&m.a, &t) / 50;
    for index in 0..8u8 {
        let delta = cut(&[(1_000_000, X + index)], 0);
        for buy in [true, false] {
            m.silence(pool);
            if buy {
                m.answer(pool, callback::BEFORE_SWAP, &delta);
            } else {
                m.answer(pool, callback::AFTER_SWAP, &delta);
            }
            // The swap config (index 7) is a fair lamport destination (a donation): skipped here.
            if index == 7 {
                continue;
            }
            let before = snapshot(&m);
            let to = if index == 6 { recipient } else { t };
            let tx = if buy {
                m.v2(&pool, 1, SOL, &t)
            } else {
                m.v2(&pool, 0, sell, &to)
            };
            // The recipient's wallet as a cut is refused only when it is also the delivery.
            if index == 6 && buy {
                tx.ok();
                m.inv2(&pool, 0, 0);
                continue;
            }
            assert!(tx.result.is_err(), "v2 index {index} buy {buy} must fail");
            assert_eq!(before, snapshot(&m));
            let tx = if buy {
                m.v1(&pool, 1, SOL)
            } else {
                m.v1(&pool, 0, sell)
            };
            if index == 6 {
                // The old swap delivers to a holding: the recipient wallet is a fair cut there.
                tx.ok();
                m.inv2(&pool, 0, 0);
                continue;
            }
            assert!(tx.result.is_err(), "v1 index {index} buy {buy} must fail");
            assert_eq!(before, snapshot(&m));
        }
    }
    // A sell delivering lamports to an executable account.
    m.silence(pool);
    let trader = m.trader.insecure_clone();
    let mut ix = m.v2_ix_by(&t, &pool, 0, sell, &t);
    ix.accounts[7] = AccountMeta::new(hook_tester::ID, false);
    m.send(&[ix], &trader)
        .expect_code(code(SwapError::WrongNativeAccount));
    m.inv2(&pool, 0, 0);
}

// ---------------------------------------------------------------------------------------------
// The buffer: the old swap's sells, and who moves the buffer.
// ---------------------------------------------------------------------------------------------

/// FINDING (compatibility, griefing): at the recommended buffer target of 0, the old `swap`'s
/// sells on a native pool work only while bridged SOL from earlier old-path buys sits in the
/// buffer, and `rebalance_pool` is permissionless: anyone can empty the buffer (toward the target)
/// right before an old-path sell, which then fails with `BufferShort`. Integrators still on the old
/// instruction can't sell reliably. No value moves (the books never change), so it is liveness only.
#[test]
fn fixed_emptying_the_buffer_no_longer_fails_an_old_path_sell() {
    let mut m = M::new();
    let pool = m.pool();
    m.silence(pool);
    m.migrate(&pool, 0).ok();
    let t = m.trader.pubkey();
    // An old-path buy fills the buffer; a small old-path sell would be covered.
    m.v1(&pool, 1, 5 * SOL).ok();
    assert!(m.state(&pool).buffer_quote() > 0);
    let sell = m.bal(&m.a, &t) / 100;
    // A stranger's rebalance first.
    let griefer = m.w.env.funded(SOL);
    m.rebalance(&pool, &griefer).ok();
    assert_eq!(m.state(&pool).buffer_quote(), 0);
    // Fixed (N-5): the old sell still works; what the buffer can't cover is paid as lamports to
    // the trader, and the invariant stays exact.
    let lamports = m.w.env.lamports(&t);
    let e: bordrless_swap::events::Swapped = m.v1(&pool, 0, sell).event();
    assert!(e.delivered_out > 0);
    assert!(m.w.env.lamports(&t) > lamports);
    assert_eq!(m.state(&pool).buffer_quote(), 0);
    m.inv2(&pool, 0, 0);
    // The same sell through swap_v2 is fine.
    m.v2(&pool, 0, sell, &t).ok();
    m.inv2(&pool, 0, 0);
}

/// RULED OUT (value extraction by moving the buffer; sandwiching swap against swap_v2): both swaps
/// price off the same books, so the same trade through either costs and pays exactly the same, and
/// a rebalance between them changes nothing a trader receives.
#[test]
fn ruled_out_swap_and_swap_v2_price_identically_whatever_the_buffer() {
    // Two identical worlds; in one, trade through the old swap, in the other through swap_v2, with
    // rebalances to different targets in between.
    let mut m1 = M::new();
    let mut m2 = M::new();
    let p1 = m1.pool();
    let p2 = m2.pool();
    m1.silence(p1);
    m2.silence(p2);
    m1.migrate(&p1, 0).ok();
    m2.migrate(&p2, 50 * SOL).ok();
    let t1 = m1.trader.pubkey();
    let t2 = m2.trader.pubkey();
    for (i, amount) in [3 * SOL, SOL / 3, 7 * SOL].into_iter().enumerate() {
        let e1: bordrless_swap::events::Swapped = m1.v1(&p1, 1, amount).event();
        let e2: bordrless_swap::events::Swapped = m2.v2(&p2, 1, amount, &t2).event();
        assert_eq!(e1.delivered_out, e2.delivered_out, "buy {i}");
        assert_eq!(e1.protocol_fee, e2.protocol_fee);
        let c = m2.w.env.funded(SOL);
        m2.set_target(&p2, (i as u64) * 20 * SOL).ok();
        m2.rebalance(&p2, &c).ok();
        let s1 = m1.bal(&m1.a, &t1) / 7;
        let s2 = m2.bal(&m2.a, &t2) / 7;
        assert_eq!(s1, s2);
        // The old path's sell needs the buffer: give m1 one from its own lamports.
        m1.set_target(&p1, 100 * SOL).ok();
        let c1 = m1.w.env.funded(SOL);
        m1.rebalance(&p1, &c1).ok();
        let e1: bordrless_swap::events::Swapped = m1.v1(&p1, 0, s1).event();
        let e2: bordrless_swap::events::Swapped = m2.v2(&p2, 0, s2, &t2).event();
        assert_eq!(e1.delivered_out, e2.delivered_out, "sell {i}");
        assert_eq!(
            (
                m1.state(&p1).quote_reserve,
                m1.state(&p1).protocol_fees_quote
            ),
            (
                m2.state(&p2).quote_reserve,
                m2.state(&p2).protocol_fees_quote
            )
        );
        m1.inv2(&p1, 0, 0);
        m2.inv2(&p2, 0, 0);
    }
}

// ---------------------------------------------------------------------------------------------
// Migration ordering.
// ---------------------------------------------------------------------------------------------

/// FINDING (ordering not enforced on the way back): `revert_pool_to_bridged` does not check that
/// the pool's launch was reverted first. A pool reverted while its launch is still native makes
/// the launch hook name the launch account (lamports) as a bridged pool's quote cut, so every swap
/// of the coin, old and new, fails (`InvalidDeltaAccount` / `NotNativePool`) until the pool is
/// migrated again. No value moves; the runbook's order is the only guard.
#[test]
fn finding_reverting_a_pool_before_its_launch_stops_every_trade() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(20 * SOL);
    let (mint, tx) = w.create_launch(&creator, "ORD", 100, VQ);
    tx.ok();
    w.env.warp(31);
    let a = w.wallet_with_sol(30 * SOL);
    w.holdings(&a, mint, &[a.pubkey()]);
    w.buy(&a, &mint, SOL).ok();
    migrate(&mut w, &mint).ok();
    let pool = pool_of(&w, &mint);
    let ad = admin(&w).pubkey();
    pause(&mut w, true);
    as_admin(&mut w, &[swap::revert_pool_to_bridged(ad, pool)]).ok();
    pause(&mut w, false);
    assert!(w.launch(&mint).is_native());
    pool_inv(&w, &pool);
    w.buy(&a, &mint, SOL / 10)
        .expect_code(code(SwapError::InvalidDeltaAccount));
    let held = w.env.holding(&mint, &a.pubkey());
    w.sell(&a, &mint, held / 10)
        .expect_code(code(SwapError::InvalidDeltaAccount));
    let payer = w.env.funded(SOL);
    let ix = v2_ix(&w, &a.pubkey(), &mint, 1, SOL / 10);
    send(&mut w, &payer, ix, &a).expect_code(code(SwapError::NotNativePool));
}

/// RULED OUT (partial migration, unpause mid-way): with the pool native and its launch still
/// bridged (the window stopped between steps 1 and 2 and the DEX reopened), the launch hook names
/// its bridged-SOL holdings, which a native pool pays in bridged SOL: swap_v2 buys (the client
/// passing the bridge) and sells work, and every lamport is where the books say.
#[test]
fn ruled_out_pool_native_launch_bridged_still_trades_exactly() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(20 * SOL);
    let rules = bordrless_program_tests::launch::rules(100, 100, 0, 0, 0, 0, 0, 0);
    let (mint, tx) = w.create_launch_with(&creator, "HLF", 100, VQ, rules);
    tx.ok();
    w.env.warp(31);
    let a = w.wallet_with_sol(30 * SOL);
    w.holdings(&a, mint, &[a.pubkey(), creator.pubkey()]);
    w.buy(&a, &mint, SOL).ok();
    let pool = pool_of(&w, &mint);
    let ad = admin(&w).pubkey();
    pause(&mut w, true);
    as_admin(&mut w, &[swap::migrate_pool_to_native(ad, pool, 0)]).ok();
    pause(&mut w, false);
    assert!(!w.launch(&mint).is_native());
    let (_, fees0) = launch_fees(&w, &mint);
    let payer = w.env.funded(SOL);
    let keys = w.launch_keys(&mint);
    let slice = w.launch_base_slice(&mint, &a.pubkey(), &a.pubkey(), true);
    let ix = launch::swap_v2_with_bridge(&keys, a.pubkey(), a.pubkey(), 1, SOL, 0, slice, true);
    send(&mut w, &payer, ix, &a).ok();
    pool_inv(&w, &pool);
    let (_, fees1) = launch_fees(&w, &mint);
    assert!(
        fees1 > fees0,
        "the creator fee reached the old holding, wrapped"
    );
    let held = w.env.holding(&mint, &a.pubkey());
    let slice = w.launch_base_slice(&mint, &a.pubkey(), &a.pubkey(), false);
    let ix =
        launch::swap_v2_with_bridge(&keys, a.pubkey(), a.pubkey(), 0, held / 3, 0, slice, true);
    send(&mut w, &payer, ix, &a).ok();
    pool_inv(&w, &pool);
}

// ---------------------------------------------------------------------------------------------
// Companion.
// ---------------------------------------------------------------------------------------------

/// FINDING (companion `native_at_launch`): a companion created before `quote_cut_claimed`
/// (300 bytes) that has not launched yet can't launch once native launches are on:
/// `grow_for_native` requires `quote_cut_claimed` stored and `process_launch` never grows it
/// first (`MissingAccount`). The cloned-mainnet dump has three such companions. Their funding is
/// not lost (`withdraw` with the mint's signature), but they can launch only while native launches
/// are off.
#[test]
fn finding_an_old_unlaunched_companion_cannot_launch_once_launches_are_native() {
    use bordrless_companion::client as companion;
    use bordrless_companion::error::CompanionError;
    use bordrless_companion::instructions::CreateArgs;
    use bordrless_companion::state::{Companion, Split};
    let mut w = World::new();
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint = Keypair::new();
    let split = Split {
        buyback_bps: 0,
        holders_bps: 0,
        beneficiary_bps: 10_000,
    };
    let args = CreateArgs {
        split,
        bounty_bps: 50,
        max_buyback: SOL,
        buyback_interval: 60,
        vest_secs: 0,
        fund: SOL / 2,
    };
    w.env
        .send_paid_by(
            &[companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                mint.pubkey(),
                args,
            )],
            &launcher,
            &[&mint],
        )
        .ok();
    // As made before the upgrade: 300 bytes.
    let key = companion::companion_address(&mint.pubkey());
    let mut account = w.env.account(&key).unwrap();
    assert_eq!(account.data.len(), Companion::LEN);
    account.data.truncate(300);
    account.lamports = w.env.rent(300);
    w.env.put(key, account);
    let c: Companion = w.env.read(&key);
    assert!(!c.quote_cut_claimed.stored && !c.launched);

    let ad = admin(&w).pubkey();
    as_admin(&mut w, &[launch::set_native_launches(ad, true)]).ok();
    let creator = companion::creator_address(&mint.pubkey());
    let launch_args = World::launch_args("OLD", 200, VQ, LaunchRules::NONE);
    let inner = launch::create_launch_with(
        creator,
        mint.pubkey(),
        w.env.treasury.pubkey(),
        w.sol,
        bordrless_core::policy::LP_FEE_BPS,
        launch_args.clone(),
        None,
        None,
    );
    let ix = companion::launch(
        launcher.pubkey(),
        mint.pubkey(),
        &inner,
        launch_args.clone(),
    );
    w.env
        .send_paid_by(&[ix], &launcher, &[&mint])
        .expect_code(u32::from(CompanionError::MissingAccount));
    // With native launches off it launches (bridged).
    as_admin(&mut w, &[launch::set_native_launches(ad, false)]).ok();
    let ix = companion::launch(launcher.pubkey(), mint.pubkey(), &inner, launch_args);
    w.env.send_paid_by(&[ix], &launcher, &[&mint]).ok();
}

// ---------------------------------------------------------------------------------------------
// hook_vault: a vault made for a coin launched native.
// ---------------------------------------------------------------------------------------------

/// FINDING (hook_vault has no native-at-creation path): once native launches are on, a coin made
/// with a hook vault launches native (pool, launch, kit) but its vault is created bridged, and only
/// `migrate_vault_to_native` (the upgrade authority, with the whole DEX paused) turns it native.
/// Until then every slot sale goes through the old swap's buffer and fails with `BufferShort`
/// (nothing ever fills a fresh native pool's buffer at target 0): the coin's hook cut piles up in
/// the slot unsold.
#[test]
fn finding_a_new_vault_coin_on_native_launches_cannot_sell() {
    use bordrless_program_tests::vault::{sell_for_sol, vault_args};
    let mut w = World::new();
    let ad = admin(&w).pubkey();
    as_admin(&mut w, &[launch::set_native_launches(ad, true)]).ok();
    let to = Keypair::new().pubkey();
    let c = w.vault_coin(vault_args(vec![sell_for_sol(to)]), &[], true);
    let m = c.mint;
    assert!(w.launch(&m).is_native());
    assert!(!w.vault(&m).is_native(), "the vault starts bridged");
    // Trade the coin through swap_v2 (the old buy can't, on a fresh native pool).
    let payer = w.env.funded(10 * SOL);
    for _ in 0..3 {
        let t = w.env.funded(3 * SOL);
        w.holdings(&t, m, &[t.pubkey()]);
        let ix = v2_ix(&w, &t.pubkey(), &m, 1, SOL);
        send(&mut w, &payer, ix, &t).ok();
        let got = w.env.holding(&m, &t.pubkey());
        let ix = v2_ix(&w, &t.pubkey(), &m, 0, got / 2);
        send(&mut w, &payer, ix, &t).ok();
    }
    w.env.warp(61);
    let owner = hook_vault::client::slot_owner(&m, 0);
    assert!(
        w.env.holding(&m, &owner) > 0,
        "the slot holds the hook's cut"
    );
    let cranker = w.wallet_with_sol(SOL);
    w.execute(&cranker, &m, 0)
        .expect_code(code(SwapError::BufferShort));
}

// ---------------------------------------------------------------------------------------------
// Kit: when a native pool's holder fee reaches the kit, against the buyer's settle.
// ---------------------------------------------------------------------------------------------

/// A migrated launch with holder rewards (1% each side), `a` holding, the pool holding lamports.
/// Answers the world, the mint, a fee payer, and a second wallet `b` (bridged SOL and lamports).
fn kit_world() -> (World, Pubkey, Keypair, Keypair) {
    let mut w = World::new();
    let payer = w.env.funded(100 * SOL);
    let creator = w.wallet_with_sol(20 * SOL);
    let rules = bordrless_program_tests::launch::rules(100, 100, 0, 0, 0, 0, 0, 0);
    let (mint, tx) = w.create_launch_with(&creator, "KIT", 100, VQ, rules);
    tx.ok();
    w.env.warp(31);
    let (a, b) = (w.wallet_with_sol(30 * SOL), w.wallet_with_sol(30 * SOL));
    w.holdings(&a, mint, &[a.pubkey(), b.pubkey(), creator.pubkey()]);
    w.buy(&a, &mint, 2 * SOL).ok();
    migrate(&mut w, &mint).ok();
    let ix = v2_ix(&w, &a.pubkey(), &mint, 1, SOL);
    send(&mut w, &payer, ix, &a).ok();
    (w, mint, payer, b)
}

/// FINDING (fairness, old swap on a native launch pool): the old swap pays a native pool's
/// lamport cuts only after every CPI (`pending`), so on an old-path buy the holder fee reaches the
/// kit config after the kit's transfer hook has already settled the buyer at its new balance; the
/// next sync then shares the fee with the buyer too. On `swap_v2` (and on bridged pools) the fee
/// is in the vault before the buyer's tokens arrive, so the buyer gets none of its own fee. A buyer
/// that will hold a share `s` of the eligible supply recovers about `s` of its own holder fee from
/// the other holders by buying through the old swap.
#[test]
fn fixed_old_path_buyer_no_longer_shares_its_own_holder_fee() {
    let claim = |old_path: bool| -> u64 {
        let (mut w, mint, payer, b) = kit_world();
        if old_path {
            w.buy(&b, &mint, 10 * SOL).ok();
        } else {
            let ix = v2_ix(&w, &b.pubkey(), &mint, 1, 10 * SOL);
            send(&mut w, &payer, ix, &b).ok();
        }
        w.env.warp(1);
        let before = w.env.lamports(&b.pubkey());
        let tx = send(
            &mut w,
            &payer,
            bordrless_kit::client::claim_sol(b.pubkey(), mint),
            &b,
        );
        if tx.result.is_err() {
            return 0;
        }
        w.env.lamports(&b.pubkey()) - before
    };
    let (old, v2) = (claim(true), claim(false));
    println!("buyer's claim of its own 0.1 SOL holder fee: old path {old}, swap_v2 {v2}");
    assert_eq!(
        v2, 0,
        "through swap_v2 the buyer earns nothing from its own fee"
    );
    // Fixed (N-2): the old path pays the holder fee before the output transfer, as swap_v2 does.
    assert_eq!(old, 0, "through the old swap neither: {old}");
}

// ---------------------------------------------------------------------------------------------
// Liquidity providers of a native pool.
// ---------------------------------------------------------------------------------------------

/// FINDING (LPs locked at buffer target 0): `remove_liquidity` on a native pool pays the quote in
/// bridged SOL from the buffer only. A third party's deposit joins the buffer, the permissionless
/// `rebalance_pool` (or the keeper) unwraps it to lamports toward the target of 0, and the LP can
/// no longer withdraw (`BufferShort`); only the admin's `set_buffer_target` (or the LP buying
/// through the old swap first, to refill the buffer, and selling back) gets it out. On mainnet this
/// is every LP of a graduated launch pool or a hook-less pool.
#[test]
fn fixed_an_lp_of_a_native_pool_withdraws_at_target_zero() {
    use bordrless_swap::instructions::{AddLiquidityArgs, RemoveLiquidityArgs};
    let mut m = M::new();
    let pool = m.pool();
    m.silence(pool);
    m.migrate(&pool, 0).ok();
    let owner = m.owner.insecure_clone();
    let o = owner.pubkey();
    let lp_mint = swap::lp_mint_address(&pool);
    let lp0 = m.bal(&lp_mint, &o);
    let ix = swap::add_liquidity(
        &m.liquidity_keys(&pool),
        AddLiquidityArgs {
            base_desired: BASE_DEPOSIT / 10,
            quote_desired: 10 * SOL,
            min_lp: 0,
            base_hook_accounts: 0,
            quote_hook_accounts: 0,
            hook_data: vec![],
        },
        m.pool_extras(&pool, &o),
    );
    m.send(&[ix], &owner).ok();
    let minted = m.bal(&lp_mint, &o) - lp0;
    assert!(minted > 0 && m.state(&pool).buffer_quote() > 0);
    // Anyone's rebalance unwraps the deposit (target 0).
    let stranger = m.w.env.funded(SOL);
    m.rebalance(&pool, &stranger).ok();
    assert_eq!(m.state(&pool).buffer_quote(), 0);
    let ix = swap::remove_liquidity(
        &m.liquidity_keys(&pool),
        RemoveLiquidityArgs {
            lp_amount: minted,
            min_base: 0,
            min_quote: 0,
            base_hook_accounts: 0,
            quote_hook_accounts: 0,
            hook_data: vec![],
        },
        m.pool_extras(&pool, &o),
    );
    // Fixed (N-4): the withdrawal pays what the buffer lacks as lamports to the provider.
    let lamports = m.w.env.lamports(&o);
    m.send(&[ix], &owner).ok();
    assert!(m.w.env.lamports(&o) > lamports + SOL);
    assert_eq!(m.bal(&lp_mint, &o), lp0);
    m.inv2(&pool, 0, 0);
}
// ---------------------------------------------------------------------------------------------
// `buffer_all` (branch `native-sol-held`, 963ad6d).
// ---------------------------------------------------------------------------------------------


/// RULED OUT (introspection bypasses): `buffer_all` is refused alone, with a rebalance of another
/// pool after it, with a rebalance only before it, without the instructions sysvar or with another
/// account in its place, on a bridged pool, and for a second pool whose rebalance is missing. With
/// a later `rebalance_pool` of the same pool it runs, the window's state is the whole books in the
/// vault, and the transaction ends with the pool back at its target: the invariant exact, the
/// signer's lamports unchanged (it only wraps what the pool credited it), the value conserved.
#[test]
fn ruled_out_buffer_all_needs_a_later_rebalance_of_the_same_pool() {
    let mut m = M::new();
    let pool = m.pool();
    m.silence(pool);
    m.migrate(&pool, 0).ok();
    let t = m.trader.pubkey();
    // Some books in both forms: a v2 buy, an old buy.
    m.v2(&pool, 1, 3 * SOL, &t).ok();
    m.v1(&pool, 1, 2 * SOL).ok();
    m.set_target(&pool, SOL).ok();
    let cranker = m.w.env.funded(10 * SOL);
    let c = cranker.pubkey();
    m.rebalance(&pool, &cranker).ok();
    m.inv2(&pool, 0, 0);
    // A second native pool (another base mint).
    let mut m2_extras = m.extras();
    m2_extras.truncate(0);
    let owner = m.owner.insecure_clone();
    let b = m.w.mint_to_owner(&owner, 6, SUPPLY, "BBB");
    let spec = NewPool {
        base: b,
        quote: m.sol,
        lp_fee_bps: 30,
        tester_flags: None,
        extras: m2_extras,
        base_amount: BASE_DEPOSIT,
        quote_amount: QUOTE_DEPOSIT,
    };
    let (other, tx) = m.w.new_pool(&owner, &spec);
    tx.ok();
    m.migrate(&other, 0).ok();

    let ba = |p: Pubkey| swap::buffer_all(c, p);
    let rb = |p: Pubkey| swap::rebalance_pool(c, p);
    let refused = code(SwapError::NoRebalanceAfter);
    let before = (m.state(&pool), m.lamports(&pool), m.lamports(&c));
    m.send(&[ba(pool)], &cranker).expect_code(refused);
    m.send(&[ba(pool), rb(other)], &cranker).expect_code(refused);
    m.send(&[rb(pool), ba(pool)], &cranker).expect_code(refused);
    m.send(&[ba(pool), rb(pool), ba(pool)], &cranker)
        .expect_code(refused);
    m.send(&[ba(pool), ba(other), rb(pool)], &cranker)
        .expect_code(refused);
    // No sysvar, or another account where it goes.
    let mut no_sysvar = ba(pool);
    no_sysvar.accounts.pop();
    m.send(&[no_sysvar, rb(pool)], &cranker).expect_code(refused);
    let mut fake = ba(pool);
    let n = fake.accounts.len();
    fake.accounts[n - 1] = AccountMeta::new_readonly(swap::config_address(), false);
    m.send(&[fake, rb(pool)], &cranker).expect_code(refused);
    // A rebalance of the pool built by another program id (a look-alike): refused.
    let mut lookalike = rb(pool);
    lookalike.program_id = hook_tester::ID;
    m.send(&[ba(pool), lookalike], &cranker).expect_fail();
    let after = (m.state(&pool), m.lamports(&pool), m.lamports(&c));
    assert_eq!(before.0.buffer_quote(), after.0.buffer_quote());
    assert_eq!((before.1, before.2), (after.1, after.2));

    // The real thing, with the window's state checked by a probe in between: the whole books in
    // the vault, none of them as lamports.
    let p0 = m.state(&pool);
    let books = p0.quote_reserve + p0.protocol_fees_quote;
    let vault0 = m.bal(&m.sol, &pool);
    let tx = m.send(&[ba(pool), ba(pool), rb(pool)], &cranker);
    tx.ok();
    let evs = tx.events::<bordrless_swap::events::PoolQuoteFormChanged>();
    assert_eq!(evs.iter().map(|e| e.kind).collect::<Vec<_>>(), vec![4, 4, 2]);
    assert_eq!(evs[0].buffer_quote, books, "the window holds the whole books in the vault");
    assert_eq!(evs[0].wrapped, books - p0.buffer_quote());
    assert_eq!(evs[1].wrapped, 0, "a second buffer_all has nothing left to wrap");
    assert_eq!(evs[2].unwrapped, books - SOL);
    let p1 = m.state(&pool);
    assert_eq!(p1.buffer_quote(), SOL, "back at the target");
    assert_eq!(m.bal(&m.sol, &pool), vault0);
    assert_eq!(m.lamports(&c), before.2, "the signer ends where it started");
    m.inv2(&pool, 0, 0);
    // On a bridged pool: refused.
    m.revert(&other).ok();
    m.send(&[ba(other), rb(other)], &cranker)
        .expect_code(code(SwapError::NotNativePool));
}

/// RULED OUT (abuse inside the window): between `buffer_all` and the rebalance the pool's quote is
/// all bridged SOL in its vault; the books, the reserves and the price never move by the form, so
/// trades inside the window price exactly as outside it. The old swap's sells now work (the buffer
/// covers them) and a `swap_v2` sell can't be paid (no lamports: the whole transaction fails); a
/// `swap_v2` buy and LP withdrawals work. Every combination leaves the invariant exact and the value
/// conserved; nothing is left unbalanced.
#[test]
fn ruled_out_trades_inside_the_buffer_all_window_are_value_neutral() {
    use bordrless_swap::instructions::RemoveLiquidityArgs;
    // Two identical worlds: the same trades outside a window and inside one.
    let run = |inside: bool| -> (u64, u64, Pool) {
        let mut m = M::new();
        let pool = m.pool();
        m.silence(pool);
        m.migrate(&pool, 0).ok();
        let t = m.trader.pubkey();
        m.v2(&pool, 1, 5 * SOL, &t).ok();
        let cranker = m.w.env.funded(10 * SOL);
        let c = cranker.pubkey();
        let trader = m.trader.insecure_clone();
        let sell = m.bal(&m.a, &t) / 3;
        let sell_ix = m
            .w
            .env
            .swap_ix(&SwapSpec::new(t, pool, 0, sell));
        let buy_ix = m.v2_ix_by(&t, &pool, 1, SOL, &t);
        let q0 = m.bal(&m.sol, &t);
        let l0 = m.lamports(&t);
        if inside {
            let payer = m.payer.insecure_clone();
            let tx = m.w.env.send_paid_by(
                &[
                    swap::buffer_all(c, pool),
                    sell_ix,
                    buy_ix,
                    swap::rebalance_pool(c, pool),
                ],
                &payer,
                &[&cranker, &trader],
            );
            tx.ok();
        } else {
            // Outside a window the old sell needs a buffer: the admin's target for this one sell.
            m.set_target(&pool, u64::MAX).ok();
            m.rebalance(&pool, &cranker).ok();
            m.send(&[sell_ix], &trader).ok();
            m.send(&[buy_ix], &trader).ok();
            m.set_target(&pool, 0).ok();
            m.rebalance(&pool, &cranker).ok();
        }
        m.inv2(&pool, 0, 0);
        // A v2 sell inside a window fails, and with it the whole transaction.
        let payer = m.payer.insecure_clone();
        let v2_sell = m.v2_ix_by(&t, &pool, 0, m.bal(&m.a, &t) / 4, &t);
        let snapshot = (m.state(&pool).buffer_quote(), m.lamports(&pool), m.lamports(&t));
        m.w.env
            .send_paid_by(
                &[
                    swap::buffer_all(c, pool),
                    v2_sell,
                    swap::rebalance_pool(c, pool),
                ],
                &payer,
                &[&cranker, &trader],
            )
            .expect_fail();
        assert_eq!(
            snapshot,
            (m.state(&pool).buffer_quote(), m.lamports(&pool), m.lamports(&t))
        );
        // An LP withdraws inside a window (the N-4 way out): its quote is covered.
        let o = m.owner.insecure_clone();
        let lp = m.bal(&swap::lp_mint_address(&pool), &o.pubkey());
        let remove = swap::remove_liquidity(
            &m.liquidity_keys(&pool),
            RemoveLiquidityArgs {
                lp_amount: lp / 10,
                min_base: 0,
                min_quote: 0,
                base_hook_accounts: 0,
                quote_hook_accounts: 0,
                hook_data: vec![],
            },
            m.pool_extras(&pool, &o.pubkey()),
        );
        m.w.env
            .send_paid_by(
                &[
                    swap::buffer_all(c, pool),
                    remove,
                    swap::rebalance_pool(c, pool),
                ],
                &payer,
                &[&cranker, &o],
            )
            .ok();
        m.inv2(&pool, 0, 0);
        (
            m.bal(&m.sol, &t) - q0,
            l0 - m.lamports(&t),
            m.state(&pool),
        )
    };
    let (q_in, l_in, p_in) = run(true);
    let (q_out, l_out, p_out) = run(false);
    assert_eq!((q_in, l_in), (q_out, l_out), "the trader's result is the same");
    assert_eq!(
        (p_in.quote_reserve, p_in.base_reserve, p_in.protocol_fees_quote),
        (p_out.quote_reserve, p_out.base_reserve, p_out.protocol_fees_quote)
    );
}
