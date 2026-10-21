//! SOL launches against the programs before quote tokens, byte for byte (the independent review of
//! 19 Oct, its differential tests). Each step is sent to two forks of one world: one runs the DEX
//! and the launchpad built from this tree, the other the DEX and the launchpad before the change
//! (7fde540, what mainnet runs: built from that commit, or `solana program dump`ed). Every step must
//! end the same way, with the same inner instructions (CPIs and events) and every account it names
//! the same afterwards. The one intended difference, the launchpad's pools created only by the
//! launchpad, is checked on its own.
//!
//! The baseline binaries are not in the tree: these tests are ignored unless run with
//! `QUOTES_BASELINE_SBF_DIR` naming a directory that holds `bordrless_swap.so` and
//! `bordrless_launch.so` of the baseline, e.g.
//! `QUOTES_BASELINE_SBF_DIR=/path/to/7fde540/deploy programs.sh test -p bordrless-program-tests
//! --test token_quotes_sol_differential -- --ignored`.
//!
//! Rules on coins quoted in tokens (`docs/token-quotes.md` §6.1) changed the launchpad and the kit
//! again. Their tests run against the programs as deployed with quote tokens (20 Oct: the DEX,
//! launchpad and kit that mainnet runs, built from the tree before rules, or dumped), named by
//! `RULES_BASELINE_SBF_DIR` (`bordrless_swap.so`, `bordrless_launch.so`, `bordrless_kit.so`): SOL
//! kit launches with every rule, bridged and native, and a coin quoted in tokens without rules, the
//! same byte for byte; and accounts made by the deployed programs read and trade after the upgrade.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_core::policy;
use bordrless_kit::constants::QUOTE_KIND_OFFSET;
use bordrless_kit::state::KitConfig;
use bordrless_launch::client as launch;
use bordrless_launch::state::{Launch, LaunchRules, Legs};
use bordrless_program_tests::env::{Env, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::{presets, VQ};
use bordrless_program_tests::quotes::{pack_mint, QUOTE_FUNDS, TOKEN, TOKEN_2022};
use bordrless_swap::client as swap;
use bordrless_swap::error::SwapError;
use bordrless_swap::instructions::CreatePoolArgs;
use bordrless_token::client as token;
use solana_keypair::Keypair;
use solana_signer::Signer;

const FEE: u16 = 100;
const SOL: u64 = 1_000_000_000;

/// The baseline binaries' directory.
fn baseline_dir() -> String {
    std::env::var("QUOTES_BASELINE_SBF_DIR").expect(
        "QUOTES_BASELINE_SBF_DIR: a directory with the baseline bordrless_swap.so and bordrless_launch.so",
    )
}

/// The programs the quote-token change touched.
const QUOTE_PROGRAMS: [(&str, Pubkey); 2] = [
    ("bordrless_swap", bordrless_swap::ID),
    ("bordrless_launch", bordrless_launch::ID),
];

/// `w` forked with the baseline's DEX and launchpad (upgradeable by the deployer, as `Env::new`
/// leaves them).
fn fork_baseline(w: &World) -> World {
    fork_with(w, &baseline_dir(), &QUOTE_PROGRAMS)
}

/// `w` forked with `programs` loaded from `dir` (each upgradeable by the deployer).
fn fork_with(w: &World, dir: &str, programs: &[(&str, Pubkey)]) -> World {
    let mut env = Env {
        svm: w.env.svm.clone(),
        payer: w.env.payer.insecure_clone(),
        now: w.env.now,
        slot: w.env.slot,
        deployer: w.env.deployer.insecure_clone(),
        treasury: w.env.treasury.insecure_clone(),
    };
    load(&mut env, dir, programs);
    World { env, sol: w.sol }
}

/// Replaces `programs` in `env` with the `.so` files of `dir`, as an upgrade would.
fn load(env: &mut Env, dir: &str, programs: &[(&str, Pubkey)]) {
    let deployer = env.deployer.pubkey();
    for (name, id) in programs {
        let path = format!("{dir}/{name}.so");
        let so = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        env.svm.add_program(*id, &so).expect("load the program");
        env.set_upgrade_authority(*id, Some(deployer));
    }
}

/// The two forks.
struct Pair {
    new: World,
    old: World,
    steps: usize,
}

impl Pair {
    fn fund(&mut self, key: Pubkey, lamports: u64) {
        self.new.env.fund(key, lamports);
        self.old.env.fund(key, lamports);
    }

    fn warp(&mut self, secs: i64) {
        self.new.env.warp(secs);
        self.old.env.warp(secs);
    }

    /// Sends `ixs` to both: the same outcome, the same inner instructions (CPIs and events) and
    /// every account the transaction names the same afterwards (programs aside).
    #[track_caller]
    fn send(
        &mut self,
        what: &str,
        ixs: &[Instruction],
        payer: &Keypair,
        signers: &[&Keypair],
    ) -> Tx {
        self.steps += 1;
        let a = self.new.env.send_paid_by(ixs, payer, signers);
        let b = self.old.env.send_paid_by(ixs, payer, signers);
        assert_eq!(
            a.result.as_ref().err().map(|e| e.err.clone()),
            b.result.as_ref().err().map(|e| e.err.clone()),
            "{what}: the outcome differs\nnew:\n{}\nold:\n{}",
            a.logs().join("\n"),
            b.logs().join("\n")
        );
        if let (Ok(ma), Ok(mb)) = (&a.result, &b.result) {
            type Inner = Vec<(u8, Vec<u8>, Vec<u8>)>;
            let strip = |m: &litesvm::types::TransactionMetadata| -> Inner {
                m.inner_instructions
                    .iter()
                    .flatten()
                    .map(|i| {
                        (
                            i.instruction.program_id_index,
                            i.instruction.accounts.clone(),
                            i.instruction.data.clone(),
                        )
                    })
                    .collect()
            };
            assert_eq!(
                strip(ma),
                strip(mb),
                "{what}: the inner instructions differ"
            );
        }
        let loader = Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111");
        for key in &a.keys {
            let (x, y) = (self.new.env.account(key), self.old.env.account(key));
            if let Some(acc) = &x {
                if acc.executable || acc.owner == loader {
                    continue;
                }
            }
            assert_eq!(
                x.as_ref().map(|a| (a.lamports, &a.owner, &a.data)),
                y.as_ref().map(|a| (a.lamports, &a.owner, &a.data)),
                "{what}: account {key} differs"
            );
        }
        a
    }
}

fn sol_buy_ixs(w: &World, trader: &Pubkey, mint: &Pubkey, amount: u64) -> Vec<Instruction> {
    vec![
        token::create_holding(*trader, *mint, *trader),
        w.launch_swap_ix(trader, mint, 1, amount, 0),
    ]
}

/// Bridged launches with and without kit rules (in the sniper window and after it), their
/// creator claims, a graduation on a crossing buy and a trade after it; a native launch's
/// `swap_v2` both ways, the old swap on its pool and its SOL claim: the same on both programs.
#[test]
#[ignore = "needs QUOTES_BASELINE_SBF_DIR (the baseline DEX and launchpad .so)"]
fn sol_launches_match_the_deployed_programs_byte_for_byte() {
    let new = World::new();
    let old = fork_baseline(&new);
    let mut p = Pair { new, old, steps: 0 };
    let admin = p.new.env.deployer.insecure_clone();
    let creator = Keypair::new();
    let t1 = Keypair::new();
    let t2 = Keypair::new();
    let whale = Keypair::new();
    for k in [&creator, &t1, &t2, &whale] {
        p.fund(k.pubkey(), 1_000 * SOL);
    }
    let sol = p.new.sol;
    for k in [&creator, &t1, &t2, &whale] {
        let amount = if k.pubkey() == whale.pubkey() {
            400 * SOL
        } else {
            50 * SOL
        };
        let ixs = [
            token::create_holding(k.pubkey(), sol, k.pubkey()),
            bordrless_bridge::client::wrap_sol(k.pubkey(), amount),
        ];
        p.send("wrap", &ixs, k, &[]);
    }
    let kit_rules = LaunchRules {
        holder_fee_buy_bps: 100,
        holder_fee_sell_bps: 100,
        burn_buy_bps: 50,
        burn_sell_bps: 50,
        max_wallet_bps: 0,
        ..LaunchRules::NONE
    };
    let mut mints = vec![];
    for (sym, rules) in [("PLAIN", LaunchRules::NONE), ("KIT", kit_rules)] {
        let m = Keypair::new();
        let ix = p
            .new
            .create_launch_ix(&creator.pubkey(), &m.pubkey(), sym, FEE, VQ, rules);
        p.send("create_launch", &[ix], &creator, &[&m]).ok();
        mints.push(m.pubkey());
    }
    for round in 0..2 {
        for mint in &mints {
            for (t, amt) in [(&t1, 2 * SOL), (&t2, 3 * SOL)] {
                let ixs = sol_buy_ixs(&p.new, &t.pubkey(), mint, amt);
                p.send("buy", &ixs, t, &[]).ok();
                let coins = p.new.env.holding(mint, &t.pubkey());
                let ix = p.new.launch_swap_ix(&t.pubkey(), mint, 0, coins / 3, 0);
                p.send("sell", &[ix], t, &[]).ok();
            }
        }
        if round == 0 {
            p.warp(policy::SNIPER_WINDOW_SECS + 1);
        }
    }
    for mint in &mints {
        let ix = launch::claim_creator_fees(creator.pubkey(), *mint, sol);
        p.send("claim_creator_fees", &[ix], &creator, &[]).ok();
    }
    let plain = mints[0];
    let mut ixs = sol_buy_ixs(&p.new, &whale.pubkey(), &plain, 300 * SOL);
    ixs.push(p.new.graduate_ix(&whale.pubkey(), &plain));
    p.send("crossing buy + graduate", &ixs, &whale, &[]).ok();
    assert!(p.new.launch(&plain).status != 0);
    let coins = p.new.env.holding(&plain, &whale.pubkey());
    let ix = p
        .new
        .launch_swap_ix(&whale.pubkey(), &plain, 0, coins / 4, 0);
    p.send("sell after graduation", &[ix], &whale, &[]).ok();
    p.send(
        "set_native_launches",
        &[launch::set_native_launches(admin.pubkey(), true)],
        &admin,
        &[],
    )
    .ok();
    p.warp(1);
    let m = Keypair::new();
    let ix = p
        .new
        .create_launch_ix(&creator.pubkey(), &m.pubkey(), "NAT", FEE, VQ, kit_rules);
    p.send("create native launch", &[ix], &creator, &[&m]).ok();
    let native = m.pubkey();
    assert!(p.new.launch(&native).is_native());
    p.warp(policy::SNIPER_WINDOW_SECS + 1);
    for (t, amt) in [(&t1, 2 * SOL), (&t2, 5 * SOL)] {
        let keys = p.new.launch_keys(&native);
        let ixs = [
            token::create_holding(t.pubkey(), native, t.pubkey()),
            launch::swap_v2(&keys, t.pubkey(), t.pubkey(), 1, amt, 0),
        ];
        p.send("swap_v2 buy", &ixs, t, &[]).ok();
        let coins = p.new.env.holding(&native, &t.pubkey());
        let ix = launch::swap_v2(&keys, t.pubkey(), t.pubkey(), 0, coins / 2, 0);
        p.send("swap_v2 sell", &[ix], t, &[]).ok();
        let ix = p.new.launch_swap_ix(&t.pubkey(), &native, 0, coins / 4, 0);
        p.send("old swap sell on native", &[ix], t, &[]).ok();
    }
    let ix = launch::claim_creator_fees_sol(creator.pubkey(), native, None, None);
    p.send("claim_creator_fees_sol", &[ix], &creator, &[]).ok();
    println!(
        "{} transactions identical on this tree and on the baseline",
        p.steps
    );
}

/// The intended difference: on the baseline anyone could create a pool naming the launchpad as its
/// hook and asking it nothing (taking the address of a launch's future pool); now only the
/// launchpad can.
#[test]
#[ignore = "needs QUOTES_BASELINE_SBF_DIR (the baseline DEX and launchpad .so)"]
fn a_launchpad_squat_lands_on_the_baseline_and_is_refused_now() {
    let mut new = World::new();
    let mut old = fork_baseline(&new);
    let mut outcomes = vec![];
    for w in [&mut new, &mut old] {
        let squat = w.env.funded(10 * SOL);
        let base = w.mint_to_owner(&squat, 6, 1_000_000_000, "SQ");
        let sol = w.sol;
        let ixs = [
            token::create_holding(squat.pubkey(), sol, squat.pubkey()),
            bordrless_bridge::client::wrap_sol(squat.pubkey(), SOL),
        ];
        w.env.send_paid_by(&ixs, &squat, &[]).ok();
        let ix = swap::create_pool(
            &swap::CreatePoolKeys {
                payer: squat.pubkey(),
                authority: squat.pubkey(),
                treasury: w.env.treasury.pubkey(),
                base_mint: base,
                quote_mint: sol,
                hook_caller: None,
            },
            CreatePoolArgs {
                lp_fee_bps: policy::LP_FEE_BPS,
                hook_program: bordrless_launch::ID,
                hook_flags: 0,
                virtual_base: 0,
                virtual_quote: 0,
                base_amount: 1_000_000,
                quote_amount: 1_000_000,
                base_hook_accounts: 0,
                quote_hook_accounts: 0,
                hook_data: vec![],
            },
            vec![],
        );
        outcomes.push(w.env.send_paid_by(&[ix], &squat, &[]));
    }
    outcomes[0].expect_code(u32::from(SwapError::LaunchpadPoolNeedsLaunchpad));
    outcomes[1].ok();
}

// ---- Rules on coins quoted in tokens (`docs/token-quotes.md` §6.1), against the programs as
// deployed with quote tokens. ----

/// The deployed (pre-rules) binaries' directory.
fn rules_baseline_dir() -> String {
    std::env::var("RULES_BASELINE_SBF_DIR").expect(
        "RULES_BASELINE_SBF_DIR: a directory with the deployed bordrless_swap.so, bordrless_launch.so and bordrless_kit.so",
    )
}

/// The programs rules on coins quoted in tokens touch, with the DEX they run against.
const RULES_PROGRAMS: [(&str, Pubkey); 3] = [
    ("bordrless_swap", bordrless_swap::ID),
    ("bordrless_launch", bordrless_launch::ID),
    ("bordrless_kit", bordrless_kit::ID),
];

/// This tree's builds of [`RULES_PROGRAMS`].
fn tree_dir() -> String {
    bordrless_program_tests::sbf_out_dir()
        .to_string_lossy()
        .into_owned()
}

/// Every rule of the kit and the pool hook at once (holder rewards 1% both sides, burn 0.5% both
/// sides, max wallet 5%, creator wallet lock 30 days, early-buyer lock in the first 60 s until 1 h)
/// and the kit without holder rewards (max wallet and both locks).
fn kit_rule_sets() -> [(&'static str, LaunchRules); 2] {
    [
        ("EVERY", presets::every_rule()),
        (
            "LOCKS",
            LaunchRules {
                max_wallet_bps: 500,
                creator_lock_secs: 7 * 86_400,
                early_window_secs: 60,
                early_lock_secs: 3_600,
                ..LaunchRules::NONE
            },
        ),
    ]
}

impl Pair {
    /// A wallet funded on both forks holding `sol` bridged SOL.
    fn sol_wallet(&mut self, sol: u64) -> Keypair {
        let k = Keypair::new();
        self.fund(k.pubkey(), sol + 100 * SOL);
        let ixs = [
            token::create_holding(k.pubkey(), self.new.sol, k.pubkey()),
            bordrless_bridge::client::wrap_sol(k.pubkey(), sol),
        ];
        self.send("wrap", &ixs, &k, &[]).ok();
        k
    }

    /// Buys the curve of `mint` within max wallet with fresh wallets, then graduates it on a
    /// crossing buy (as the site sends it), on both forks.
    fn graduate(&mut self, mint: &Pubkey) {
        let mut n = 0;
        loop {
            let l = self.new.launch(mint);
            let pool = self.new.launch_pool(mint);
            let missing = l.graduation_quote.saturating_sub(pool.quote_reserve);
            if missing <= SOL / 10 {
                break;
            }
            n += 1;
            assert!(n < 100, "the curve does not fill");
            let probe = Keypair::new();
            let mut amount = self.new.max_buy_for(mint, &probe.pubkey());
            let target = missing - SOL / 10;
            if self
                .new
                .launch_quote(mint, &probe.pubkey(), true, amount)
                .to_reserve_in(true)
                > target
            {
                let (mut lo, mut hi) = (0u64, amount);
                while hi - lo > 1 {
                    let mid = lo + (hi - lo) / 2;
                    let q = self.new.launch_quote(mint, &probe.pubkey(), true, mid);
                    if q.failure.is_none() && q.to_reserve_in(true) <= target {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                amount = lo.max(1);
            }
            let b = self.sol_wallet(amount);
            let ixs = sol_buy_ixs(&self.new, &b.pubkey(), mint, amount);
            self.send("fill buy", &ixs, &b, &[]).ok();
        }
        let probe = Keypair::new();
        let amount = self.new.crossing_buy_amount(mint, &probe.pubkey());
        let last = self.sol_wallet(amount);
        let mut ixs = sol_buy_ixs(&self.new, &last.pubkey(), mint, amount);
        ixs.push(self.new.graduate_ix(&last.pubkey(), mint));
        self.send("crossing buy + graduate", &ixs, &last, &[]).ok();
        assert!(self.new.launch(mint).status != 0);
        assert!(self.new.env.kit_config(mint).graduated);
    }

    /// A quote mint and token accounts of it, the same on both forks.
    fn quote_mint(&mut self, owners: &[Pubkey]) -> Pubkey {
        let key = Pubkey::new_unique();
        let issuer = self.new.env.deployer.pubkey();
        let data = pack_mint(Some(issuer), None, 6, u64::MAX / 2, &[]);
        for w in [&mut self.new, &mut self.old] {
            w.env.put_mint(key, TOKEN, data.clone());
            for o in owners {
                w.env.token_account(o, &key, &TOKEN, QUOTE_FUNDS);
            }
        }
        key
    }
}

/// SOL kit launches with every rule (bridged and native) and a coin quoted in tokens without
/// rules: every step the same on this tree and on the deployed DEX, launchpad and kit, the kit's
/// refusals included (the early-buyer lock, the creator wallet lock, max wallet), through
/// graduation, which lifts max wallet, the kit's claims and shares, and a leg's trades and claims.
#[test]
#[ignore = "needs RULES_BASELINE_SBF_DIR (the deployed DEX, launchpad and kit .so)"]
fn kit_launches_with_every_rule_match_the_deployed_programs_byte_for_byte() {
    let new = World::new();
    let old = fork_with(&new, &rules_baseline_dir(), &RULES_PROGRAMS);
    let mut p = Pair { new, old, steps: 0 };
    let admin = p.new.env.deployer.insecure_clone();
    let creator = p.sol_wallet(100 * SOL);
    let t1 = p.sol_wallet(100 * SOL);
    let t2 = p.sol_wallet(100 * SOL);
    let whale = p.sol_wallet(400 * SOL);
    let sol = p.new.sol;
    let mut mints = vec![];
    for (sym, rules) in kit_rule_sets() {
        let m = Keypair::new();
        let ix = p
            .new
            .create_launch_ix(&creator.pubkey(), &m.pubkey(), sym, FEE, VQ, rules);
        p.send("create_launch", &[ix], &creator, &[&m]).ok();
        mints.push(m.pubkey());
    }
    // In the early window (and the sniper window): buys locked, sells and transfers refused, the
    // creator locked, max wallet holding.
    // (Max wallet is 5% of the supply, about 1.25 SOL at the opening price.)
    for mint in &mints {
        let ixs = sol_buy_ixs(&p.new, &creator.pubkey(), mint, SOL / 2);
        p.send("creator's first buy", &ixs, &creator, &[]).ok();
        for (t, amt) in [(&t1, 3 * SOL / 10), (&t2, SOL / 2)] {
            let ixs = sol_buy_ixs(&p.new, &t.pubkey(), mint, amt);
            p.send("early buy", &ixs, t, &[]).ok();
            let coins = p.new.env.holding(mint, &t.pubkey());
            let ix = p.new.launch_swap_ix(&t.pubkey(), mint, 0, coins / 3, 0);
            p.send("early sell (EarlyLocked)", &[ix], t, &[])
                .expect_fail();
        }
        let ix = p
            .new
            .kit_transfer_ix(*mint, t1.pubkey(), &t1.pubkey(), &t2.pubkey(), 1);
        p.send("early transfer (EarlyLocked)", &[ix], &t1, &[])
            .expect_fail();
        let coins = p.new.env.holding(mint, &creator.pubkey());
        let ix = p
            .new
            .launch_swap_ix(&creator.pubkey(), mint, 0, coins / 2, 0);
        p.send("creator sell (CreatorLocked)", &[ix], &creator, &[])
            .expect_fail();
        let cap = p.new.max_buy_for(mint, &whale.pubkey());
        let ixs = sol_buy_ixs(&p.new, &whale.pubkey(), mint, cap * 3);
        p.send("whale above the cap (MaxWalletExceeded)", &ixs, &whale, &[])
            .expect_fail();
        let ixs = sol_buy_ixs(&p.new, &whale.pubkey(), mint, cap);
        p.send("whale at the cap", &ixs, &whale, &[]).ok();
    }
    // Past the early unlock: trades, transfers, holder rewards claimed and shared.
    p.warp(3_600);
    for round in 0..2 {
        for mint in &mints {
            for (t, amt) in [(&t1, SOL / 5), (&t2, 3 * SOL / 10)] {
                let ixs = sol_buy_ixs(&p.new, &t.pubkey(), mint, amt);
                p.send("buy", &ixs, t, &[]).ok();
                let coins = p.new.env.holding(mint, &t.pubkey());
                let ix = p.new.launch_swap_ix(&t.pubkey(), mint, 0, coins / 4, 0);
                p.send("sell", &[ix], t, &[]).ok();
            }
            let coins = p.new.env.holding(mint, &t1.pubkey());
            let ix =
                p.new
                    .kit_transfer_ix(*mint, t1.pubkey(), &t1.pubkey(), &t2.pubkey(), coins / 5);
            p.send("transfer", &[ix], &t1, &[]).ok();
        }
        if round == 0 {
            p.warp(600);
        }
    }
    let every = mints[0];
    let ix = bordrless_kit::client::claim(t1.pubkey(), every, sol);
    p.send("kit claim", &[ix], &t1, &[]).ok();
    let ix = bordrless_kit::client::share(
        t2.pubkey(),
        every,
        token::holding_address(&sol, &t2.pubkey()),
        sol,
        SOL / 100,
    );
    p.send("kit share", &[ix], &t2, &[]).ok();
    p.warp(1_800);
    let ix = bordrless_kit::client::claim(t2.pubkey(), every, sol);
    p.send("kit claim after the share", &[ix], &t2, &[]).ok();
    for mint in &mints {
        let ix = launch::claim_creator_fees(creator.pubkey(), *mint, sol);
        p.send("claim_creator_fees", &[ix], &creator, &[]).ok();
    }
    // Graduation lifts max wallet: the whale buys above the cap on the graduated pool.
    for mint in &mints {
        p.graduate(mint);
        let ixs = sol_buy_ixs(&p.new, &whale.pubkey(), mint, 50 * SOL);
        p.send("whale above the cap after graduation", &ixs, &whale, &[])
            .ok();
        let coins = p.new.env.holding(mint, &whale.pubkey());
        let ix = p.new.launch_swap_ix(&whale.pubkey(), mint, 0, coins / 4, 0);
        p.send("sell after graduation", &[ix], &whale, &[]).ok();
    }
    // The creator wallet lock to its end.
    p.warp(30 * 86_400);
    for mint in &mints {
        let coins = p.new.env.holding(mint, &creator.pubkey());
        let ix = p
            .new
            .launch_swap_ix(&creator.pubkey(), mint, 0, coins / 2, 0);
        p.send("creator sell after the lock", &[ix], &creator, &[])
            .ok();
    }
    // Native launches with the same rules: swap_v2 in the early window and after, the old swap on
    // the native pool, SOL claims, pushes and shares.
    p.send(
        "set_native_launches",
        &[launch::set_native_launches(admin.pubkey(), true)],
        &admin,
        &[],
    )
    .ok();
    let mut natives = vec![];
    for (sym, rules) in kit_rule_sets() {
        let m = Keypair::new();
        let ix = p
            .new
            .create_launch_ix(&creator.pubkey(), &m.pubkey(), sym, FEE, VQ, rules);
        p.send("create native launch", &[ix], &creator, &[&m]).ok();
        assert!(p.new.launch(&m.pubkey()).is_native());
        natives.push(m.pubkey());
    }
    for mint in &natives {
        let keys = p.new.launch_keys(mint);
        for (t, amt) in [(&t1, 3 * SOL / 10), (&t2, SOL / 2)] {
            let ixs = [
                token::create_holding(t.pubkey(), *mint, t.pubkey()),
                launch::swap_v2(&keys, t.pubkey(), t.pubkey(), 1, amt, 0),
            ];
            p.send("swap_v2 early buy", &ixs, t, &[]).ok();
            let coins = p.new.env.holding(mint, &t.pubkey());
            let ix = launch::swap_v2(&keys, t.pubkey(), t.pubkey(), 0, coins / 2, 0);
            p.send("swap_v2 early sell (EarlyLocked)", &[ix], t, &[])
                .expect_fail();
        }
    }
    p.warp(3_600);
    for mint in &natives {
        let keys = p.new.launch_keys(mint);
        for (t, amt) in [(&t1, SOL / 5), (&t2, 3 * SOL / 10)] {
            let ix = launch::swap_v2(&keys, t.pubkey(), t.pubkey(), 1, amt, 0);
            p.send("swap_v2 buy", &[ix], t, &[]).ok();
            let coins = p.new.env.holding(mint, &t.pubkey());
            let ix = launch::swap_v2(&keys, t.pubkey(), t.pubkey(), 0, coins / 3, 0);
            p.send("swap_v2 sell", &[ix], t, &[]).ok();
            let ix = p.new.launch_swap_ix(&t.pubkey(), mint, 0, coins / 5, 0);
            p.send("old swap sell on native", &[ix], t, &[]).ok();
        }
    }
    let native = natives[0];
    let ix = bordrless_kit::client::claim_sol(t1.pubkey(), native);
    p.send("claim_sol", &[ix], &t1, &[]).ok();
    let ix = bordrless_kit::client::share_sol(t1.pubkey(), native, SOL / 50);
    p.send("share_sol", &[ix], &t1, &[]).ok();
    p.warp(3_600);
    let ix = bordrless_kit::client::claim_sol_for(whale.pubkey(), t2.pubkey(), native);
    p.send("claim_sol_for", &[ix], &whale, &[]).ok();
    for mint in &natives {
        let ix = launch::claim_creator_fees_sol(creator.pubkey(), *mint, None, None);
        p.send("claim_creator_fees_sol", &[ix], &creator, &[]).ok();
    }
    // A coin quoted in tokens without rules: created with the rules' bytes the deployed
    // launchpad ignores, opened, traded and claimed the same on both.
    let quote = p.quote_mint(&[creator.pubkey(), t1.pubkey(), t2.pubkey()]);
    let quote2 = p.quote_mint(&[t1.pubkey(), t2.pubkey()]);
    let m = Keypair::new();
    let ix = p.new.token_launch_ix(
        &creator.pubkey(),
        &m.pubkey(),
        FEE,
        &[(quote, 5_000_000_000 / 2), (quote2, 5_000_000_000 / 2)],
    );
    p.send("create_token_launch", &[ix], &creator, &[&m]).ok();
    let tq = m.pubkey();
    for i in 0..2 {
        let ix = p.new.open_leg_ix(&creator.pubkey(), &tq, i);
        p.send("open_leg", &[ix], &creator, &[]).ok();
    }
    for i in 0..2 {
        for (t, amt) in [(&t1, 20_000_000u64), (&t2, 35_000_000)] {
            let ixs = [
                token::create_holding(t.pubkey(), tq, t.pubkey()),
                p.new.leg_swap_ix(&t.pubkey(), &t.pubkey(), &tq, i, 1, amt),
            ];
            p.send("leg buy", &ixs, t, &[]).ok();
            let coins = p.new.env.holding(&tq, &t.pubkey());
            let ix = p
                .new
                .leg_swap_ix(&t.pubkey(), &t.pubkey(), &tq, i, 0, coins / 3);
            p.send("leg sell", &[ix], t, &[]).ok();
        }
        let ix = launch::claim_creator_fees_token(
            t1.pubkey(),
            creator.pubkey(),
            &p.new.leg_keys(&tq, i),
            i as u8,
        );
        p.send("claim_creator_fees_token", &[ix], &t1, &[]).ok();
    }
    println!(
        "{} transactions identical on this tree and on the deployed programs",
        p.steps
    );
}

/// Accounts the deployed programs made (a SOL kit launch with every rule, bridged and native, its
/// `KitConfig`s, a coin quoted in tokens and its `Legs`, one leg still unopened) read as they were
/// after the upgrade to this tree, and every one of them trades on: the new fields read as zeros
/// (no rules, nothing counted, not lifted, a SOL coin's kit), the leg opens, the legs count nothing.
#[test]
#[ignore = "needs RULES_BASELINE_SBF_DIR (the deployed DEX, launchpad and kit .so)"]
fn accounts_made_before_the_upgrade_read_and_trade_after_it() {
    let mut w = fork_with(&World::new(), &rules_baseline_dir(), &RULES_PROGRAMS);
    let creator = w.wallet_with_sol(100 * SOL);
    let t1 = w.wallet_with_sol(100 * SOL);
    let (sol_kit, tx) = w.create_launch_with(&creator, "EVERY", FEE, VQ, presets::every_rule());
    tx.ok();
    w.buy(&t1, &sol_kit, SOL / 2).ok();
    let admin = w.admin();
    w.as_admin(launch::set_native_launches(admin.pubkey(), true))
        .ok();
    let (native, tx) = w.create_launch_with(&creator, "NAT", FEE, VQ, presets::every_rule());
    tx.ok();
    let issuer = w.env.deployer.pubkey();
    let mut quotes = vec![];
    for program in [TOKEN, TOKEN_2022] {
        let key = Pubkey::new_unique();
        w.env.put_mint(
            key,
            program,
            pack_mint(Some(issuer), None, 6, u64::MAX / 2, &[]),
        );
        w.env
            .token_account(&t1.pubkey(), &key, &program, QUOTE_FUNDS);
        quotes.push((key, 2_500_000_000u64));
    }
    let (tq, tx) = w.create_token_launch(&creator, FEE, &quotes);
    tx.ok();
    w.open_leg(&tq, 0).ok();
    // Before the upgrade, every account as the deployed programs wrote it.
    let keys = [
        launch::launch_address(&sol_kit),
        launch::kit_config_address(&sol_kit),
        launch::launch_address(&native),
        launch::kit_config_address(&native),
        launch::launch_address(&tq),
        launch::legs_address(&tq),
    ];
    let before: Vec<Vec<u8>> = keys
        .iter()
        .map(|k| w.env.account(k).unwrap().data)
        .collect();
    load(&mut w.env, &tree_dir(), &RULES_PROGRAMS);
    for (k, data) in keys.iter().zip(&before) {
        assert_eq!(
            &w.env.account(k).unwrap().data,
            data,
            "{k}: the upgrade writes nothing"
        );
    }
    // Read by this tree: the deployed layouts, the new fields zero.
    let l: Launch = w.launch(&sol_kit);
    assert_eq!((l.rules, l.modules), (presets::every_rule(), 15));
    assert!(!l.is_token_quoted() && !l.is_native());
    for (mint, native) in [(sol_kit, false), (native, true)] {
        let c: KitConfig = w.env.kit_config(&mint);
        assert!(!c.is_token_quoted(), "a SOL coin's kit");
        assert_eq!(c.is_native(), native);
        assert_eq!(c.reserved[QUOTE_KIND_OFFSET..], [0u8; 46][..]);
    }
    let l: Launch = w.launch(&tq);
    assert!(l.is_token_quoted());
    assert_eq!((l.rules, l.modules), (LaunchRules::NONE, 0));
    let legs: Legs = w.legs(&tq);
    assert_eq!((legs.count, legs.opened), (2, 1));
    assert_eq!(
        (legs.lift_at(), legs.lifted(), legs.total_sold()),
        (0, false, 0)
    );
    assert_eq!(legs.reserved, [0; 64]);
    // They trade on under this tree.
    w.buy(&t1, &sol_kit, SOL / 4).ok();
    let coins = w.env.holding(&sol_kit, &t1.pubkey());
    w.env.warp(3_600);
    w.sell(&t1, &sol_kit, coins / 3).ok();
    let ix = bordrless_kit::client::claim(t1.pubkey(), sol_kit, w.sol);
    w.env.send_paid_by(&[ix], &t1, &[]).ok();
    let keys = w.launch_keys(&native);
    let ixs = [
        token::create_holding(t1.pubkey(), native, t1.pubkey()),
        launch::swap_v2(&keys, t1.pubkey(), t1.pubkey(), 1, SOL, 0),
    ];
    w.env.send_paid_by(&ixs, &t1, &[]).ok();
    // The deployed launchpad's `Legs` opens its last leg under this tree, then both legs trade.
    w.open_leg(&tq, 1).ok();
    for i in 0..2 {
        w.leg_buy(&t1, &tq, i, 25_000_000).ok();
        let coins = w.env.holding(&tq, &t1.pubkey());
        w.leg_sell(&t1, &tq, i, coins / 4).ok();
        w.env.assert_spl_invariant(&w.leg_keys(&tq, i).pool());
    }
    let legs: Legs = w.legs(&tq);
    assert_eq!(legs.opened, 2);
    assert_eq!(legs.reserved, [0; 64], "nothing counted without max wallet");
}
