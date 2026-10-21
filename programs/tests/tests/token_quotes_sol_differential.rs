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
//!
//! Workshop items on coins quoted in tokens (`docs/token-quotes.md` §7) changed the launchpad and
//! the companion. Their test runs against the programs mainnet runs since 21 Oct (the DEX, the
//! launchpad and the kit with rules, the companion), named by `BUILDERS_BASELINE_SBF_DIR`
//! (`bordrless_swap.so`, `bordrless_launch.so`, `bordrless_kit.so`, `bordrless_companion.so`):
//! companion launches (a script companion's and a classic one's, bridged and native) and custom-hook
//! launches on SOL coins, and coins quoted in tokens made by a wallet with kit rules and without,
//! the same byte for byte.

use anchor_lang::prelude::{AccountMeta, Pubkey};
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

// ---- Workshop items on coins quoted in tokens (`docs/token-quotes.md` §7), against the programs
// mainnet runs since 21 Oct: the DEX, the launchpad and the kit with rules on coins quoted in
// tokens, and the companion. ----

/// The deployed binaries' directory (`bordrless_swap.so`, `bordrless_launch.so`,
/// `bordrless_kit.so`, `bordrless_companion.so`: built from the tree before §7, or dumped).
fn builders_baseline_dir() -> String {
    std::env::var("BUILDERS_BASELINE_SBF_DIR").expect(
        "BUILDERS_BASELINE_SBF_DIR: a directory with the deployed bordrless_swap.so, bordrless_launch.so, bordrless_kit.so and bordrless_companion.so",
    )
}

/// The programs §7 touches (the launchpad and the companion), with the DEX and the kit they run
/// against.
const BUILDERS_PROGRAMS: [(&str, Pubkey); 4] = [
    ("bordrless_swap", bordrless_swap::ID),
    ("bordrless_launch", bordrless_launch::ID),
    ("bordrless_kit", bordrless_kit::ID),
    ("bordrless_companion", bordrless_companion::ID),
];

/// `ix`'s accounts and program as remaining accounts, nobody a signer.
fn as_remaining(ix: &Instruction) -> Vec<AccountMeta> {
    let mut metas: Vec<AccountMeta> = ix
        .accounts
        .iter()
        .map(|a| AccountMeta {
            pubkey: a.pubkey,
            is_signer: false,
            is_writable: a.is_writable,
        })
        .collect();
    metas.push(AccountMeta::new_readonly(ix.program_id, false));
    metas
}

impl Pair {
    /// Writes `bytes` at the start of `key`'s data on both forks.
    fn poke(&mut self, key: &Pubkey, bytes: &[u8]) {
        for w in [&mut self.new, &mut self.old] {
            let mut a = w.env.account(key).expect("the account");
            a.data[..bytes.len()].copy_from_slice(bytes);
            w.env.put(*key, a);
        }
    }

    /// A trade of `amount` on the launch pool of `mint` by `t`, delivered to itself (`swap_v2` on a
    /// native launch), with the token's hook slice; a buy creates `t`'s holding first.
    fn launch_trade(&mut self, t: &Keypair, mint: &Pubkey, direction: u8, amount: u64) -> Tx {
        let who = t.pubkey();
        let ix = if self.new.launch(mint).is_native() {
            let slice = self.new.launch_base_slice(mint, &who, &who, direction == 1);
            launch::swap_v2_with_base_slice(
                &self.new.launch_keys(mint),
                who,
                who,
                direction,
                amount,
                0,
                slice,
            )
        } else {
            self.new.launch_swap_ix(&who, mint, direction, amount, 0)
        };
        let mut ixs = vec![];
        if direction == 1 {
            ixs.push(token::create_holding(who, *mint, who));
        }
        ixs.push(ix);
        self.send("launch pool trade", &ixs, t, &[])
    }

    /// A step of the example script for `mint` in `mode` with `param`, `extra` its accounts after
    /// its six, sent by `cranker`, a minute after the last.
    fn script_step(
        &mut self,
        cranker: &Keypair,
        mint: &Pubkey,
        mode: u8,
        param: u64,
        extra: Vec<AccountMeta>,
    ) -> Tx {
        self.warp(61);
        self.poke(
            &script_example::state_address(mint),
            &script_example::state_bytes(mode, param),
        );
        let ix = bordrless_companion::client::script_step(
            cranker.pubkey(),
            &self.new.launch_keys(mint),
            script_example::ID,
            extra,
        );
        self.send("script_step", &[ix], cranker, &[])
    }
}

/// Companion launches and custom-hook launches on SOL coins, bridged and native, and coins quoted
/// in tokens made as phase 1 makes them (a wallet the creator, kit rules and none): every step the
/// same on this tree and on the deployed DEX, launchpad, kit and companion. A script companion's
/// init, launch, trades, fee claim, steps (the bounty, spending within the pot, a step over it
/// refused) and withdrawal; a classic companion's dev buy, claim, buyback and withdrawal; the
/// presets' (Holders, Snowball, Buyback, a three-way Split) claims, shares, buybacks, steps and
/// withdrawals; a launch
/// from a `tax_hook` config, its trades, a transfer and its claim; a coin quoted in tokens opened,
/// traded and claimed.
#[test]
#[ignore = "needs BUILDERS_BASELINE_SBF_DIR (the deployed DEX, launchpad, kit and companion .so)"]
fn companion_and_custom_hook_launches_match_the_deployed_programs_byte_for_byte() {
    use bordrless_companion::client as companion;
    use bordrless_companion::constants::APPROVAL_KIND_SCRIPT;
    use bordrless_companion::instructions::{CreateArgs, CreateScriptArgs};
    use bordrless_companion::state::{Companion, Split};
    use bordrless_launch::instructions::CreateConfigArgs;
    use script_example as se;

    let mut new = World::new();
    new.env
        .svm
        .add_program(
            se::ID,
            &bordrless_program_tests::program_bytes("script_example"),
        )
        .expect("load the script");
    new.env.set_upgrade_authority(
        se::ID,
        Some(bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[1]),
    );
    let old = fork_with(&new, &builders_baseline_dir(), &BUILDERS_PROGRAMS);
    let mut p = Pair { new, old, steps: 0 };
    let admin = p.new.env.deployer.insecure_clone();
    let sol = p.new.sol;
    let hash = bordrless_program_tests::timelock::programdata_hash(&p.new.env, &se::ID);
    p.send(
        "approve the script",
        &[companion::approve(
            admin.pubkey(),
            se::ID,
            APPROVAL_KIND_SCRIPT,
            hash,
            Pubkey::default(),
        )],
        &admin,
        &[],
    )
    .ok();
    let launcher = p.sol_wallet(20 * SOL);
    let t1 = p.sol_wallet(100 * SOL);
    let t2 = p.sol_wallet(100 * SOL);
    let cranker = p.sol_wallet(10 * SOL);
    let create_args = |split: Split| CreateArgs {
        split,
        bounty_bps: 50,
        max_buyback: SOL,
        buyback_interval: 60,
        vest_secs: 0,
        fund: SOL,
    };
    let launcher_only = Split {
        buyback_bps: 0,
        holders_bps: 0,
        beneficiary_bps: 10_000,
    };
    let companion_of =
        |w: &World, m: &Pubkey| -> Companion { w.env.read(&companion::companion_address(m)) };
    for native in [false, true] {
        if native {
            p.send(
                "set_native_launches",
                &[launch::set_native_launches(admin.pubkey(), true)],
                &admin,
                &[],
            )
            .ok();
            p.warp(1);
        }
        // A script companion: 70% of every claim its pot, 30% the launcher's.
        let mint = Keypair::new();
        let m = mint.pubkey();
        let ixs = [
            companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                m,
                create_args(launcher_only),
            ),
            companion::create_script(
                launcher.pubkey(),
                m,
                CreateScriptArgs {
                    split: Split {
                        buyback_bps: 0,
                        holders_bps: 0,
                        beneficiary_bps: 3_000,
                    },
                    pot_bps: 7_000,
                    program: se::ID,
                    interval: 60,
                    state_len: 64,
                    init_data: vec![],
                },
                vec![],
            ),
        ];
        p.send("create + create_script", &ixs, &launcher, &[&mint])
            .ok();
        let creator = companion::creator_address(&m);
        let args = World::launch_args("SCRP", FEE, VQ, LaunchRules::NONE);
        let inner = launch::create_launch_with(
            creator,
            m,
            p.new.env.treasury.pubkey(),
            sol,
            policy::LP_FEE_BPS,
            args.clone(),
            None,
            None,
        );
        p.send(
            "companion launch",
            &[companion::launch_script(launcher.pubkey(), m, &inner, args)],
            &launcher,
            &[&mint],
        )
        .ok();
        assert_eq!(p.new.launch(&m).is_native(), native);
        assert_eq!(companion_of(&p.new, &m).is_native(), native);
        for (t, amt) in [(&t1, 2 * SOL), (&t2, 3 * SOL)] {
            p.launch_trade(t, &m, 1, amt).ok();
            let coins = p.new.env.holding(&m, &t.pubkey());
            p.launch_trade(t, &m, 0, coins / 3).ok();
        }
        p.warp(61);
        let keys = p.new.launch_keys(&m);
        let claim = companion::claim_fees_script(cranker.pubkey(), m, None);
        let claim = if native {
            companion::native(claim, &keys)
        } else {
            claim
        };
        p.send("claim_fees (script)", &[claim], &cranker, &[]).ok();
        let pot = companion_of(&p.new, &m).pending_pot;
        assert!(pot > 0);
        p.script_step(
            &cranker,
            &m,
            se::MODE_NOTHING,
            0,
            vec![AccountMeta::new(t1.pubkey(), false)],
        )
        .ok();
        // Spending within the pot, then beyond it (refused): lamports on a native coin, bridged
        // SOL on a bridged one.
        let (mode, extra) = if native {
            (
                se::MODE_TAKE_LAMPORTS,
                vec![AccountMeta::new(t1.pubkey(), false)],
            )
        } else {
            let aux = token::holding_address(&sol, &t1.pubkey());
            let mut extra = vec![AccountMeta::new(aux, false)];
            extra.extend(as_remaining(&token::transfer_with(
                creator,
                token::holding_address(&sol, &creator),
                aux,
                sol,
                None,
                vec![],
                0,
            )));
            (se::MODE_TAKE_SOL, extra)
        };
        p.script_step(&cranker, &m, mode, pot / 4, extra.clone())
            .ok();
        p.script_step(&cranker, &m, mode, pot * 10, extra)
            .expect_fail();
        p.send(
            "withdraw",
            &[companion::withdraw(cranker.pubkey(), m, launcher.pubkey())],
            &cranker,
            &[],
        )
        .ok();

        // A classic companion: half to buybacks, half to the launcher; its dev buy.
        let mint = Keypair::new();
        let m = mint.pubkey();
        p.send(
            "create (classic)",
            &[companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                m,
                create_args(Split {
                    buyback_bps: 5_000,
                    holders_bps: 0,
                    beneficiary_bps: 5_000,
                }),
            )],
            &launcher,
            &[&mint],
        )
        .ok();
        let creator = companion::creator_address(&m);
        let args = World::launch_args("CLSC", FEE, VQ, LaunchRules::NONE);
        let inner = launch::create_launch_with(
            creator,
            m,
            p.new.env.treasury.pubkey(),
            sol,
            policy::LP_FEE_BPS,
            args.clone(),
            None,
            None,
        );
        p.send(
            "companion launch (classic)",
            &[companion::launch(launcher.pubkey(), m, &inner, args)],
            &launcher,
            &[&mint],
        )
        .ok();
        let keys = p.new.launch_keys(&m);
        let dev = companion::dev_buy(launcher.pubkey(), &keys, SOL / 5, 1);
        let dev = if native {
            companion::native(dev, &keys)
        } else {
            dev
        };
        p.send("dev_buy", &[dev], &launcher, &[]).ok();
        for (t, amt) in [(&t1, 2 * SOL), (&t2, SOL)] {
            p.launch_trade(t, &m, 1, amt).ok();
            let coins = p.new.env.holding(&m, &t.pubkey());
            p.launch_trade(t, &m, 0, coins / 4).ok();
        }
        p.warp(120);
        let claim = companion::claim_fees(cranker.pubkey(), m, None);
        let claim = if native {
            companion::native(claim, &keys)
        } else {
            claim
        };
        p.send("claim_fees (classic)", &[claim], &cranker, &[]).ok();
        let buyback = companion::buyback(cranker.pubkey(), &keys, false);
        let buyback = if native {
            companion::native(buyback, &keys)
        } else {
            buyback
        };
        p.send("buyback", &[buyback], &cranker, &[]).ok();
        p.send(
            "withdraw (classic)",
            &[companion::withdraw(cranker.pubkey(), m, launcher.pubkey())],
            &cranker,
            &[],
        )
        .ok();

        // The presets (the launch form's Holders, Snowball, Buyback and Split): a holders' companion
        // (half to holders through the kit's rewards, half to the launcher), a Snowball (70% of
        // every claim its pot, traded on the pool), a buyback companion (the whole fee to buybacks)
        // and a three-way split (40% buybacks, 30% holders, 30% the launcher): launched, traded,
        // claimed, shared, bought back and stepped. A share, a buyback or a step that waits (too
        // little held, a price within its steps) waits the same on both.
        let rewards = LaunchRules {
            holder_fee_buy_bps: 100,
            holder_fee_sell_bps: 100,
            ..LaunchRules::NONE
        };
        for kind in ["holders", "snowball", "buyback", "split"] {
            let snowball = kind == "snowball";
            let holders = matches!(kind, "holders" | "split");
            let buys_back = matches!(kind, "buyback" | "split");
            let mint = Keypair::new();
            let m = mint.pubkey();
            let mut ixs = vec![companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                m,
                create_args(match kind {
                    "snowball" => launcher_only,
                    "holders" => Split {
                        buyback_bps: 0,
                        holders_bps: 5_000,
                        beneficiary_bps: 5_000,
                    },
                    "buyback" => Split {
                        buyback_bps: 10_000,
                        holders_bps: 0,
                        beneficiary_bps: 0,
                    },
                    _ => Split {
                        buyback_bps: 4_000,
                        holders_bps: 3_000,
                        beneficiary_bps: 3_000,
                    },
                }),
            )];
            if snowball {
                ixs.push(companion::create_snowball(
                    launcher.pubkey(),
                    m,
                    bordrless_companion::instructions::CreateSnowballArgs {
                        split: Split {
                            buyback_bps: 0,
                            holders_bps: 0,
                            beneficiary_bps: 3_000,
                        },
                        pot_bps: 7_000,
                        buy_step_bps: 100,
                        sell_step_bps: 100,
                        buy_bps: 2_000,
                        sell_bps: 2_000,
                        interval: 60,
                        max_trade: 100 * SOL,
                        pool_bps: 500,
                        flat_trades: true,
                    },
                ));
            }
            let what = |step: &str| format!("{step} ({kind})");
            p.send(&what("create"), &ixs, &launcher, &[&mint]).ok();
            let creator = companion::creator_address(&m);
            let rules = if holders { rewards } else { LaunchRules::NONE };
            let args = World::launch_args("HLDR", FEE, VQ, rules);
            let inner = launch::create_launch_with(
                creator,
                m,
                p.new.env.treasury.pubkey(),
                sol,
                policy::LP_FEE_BPS,
                args.clone(),
                None,
                None,
            );
            p.send(
                &what("companion launch"),
                &[companion::launch(launcher.pubkey(), m, &inner, args)],
                &launcher,
                &[&mint],
            )
            .ok();
            for (t, amt) in [(&t1, 2 * SOL), (&t2, 3 * SOL)] {
                p.launch_trade(t, &m, 1, amt).ok();
                let coins = p.new.env.holding(&m, &t.pubkey());
                p.launch_trade(t, &m, 0, coins / 4).ok();
            }
            p.warp(120);
            let keys = p.new.launch_keys(&m);
            let native_of = |ix: Instruction| {
                if native {
                    companion::native(ix, &keys)
                } else {
                    ix
                }
            };
            let claim = native_of(companion::claim_fees(cranker.pubkey(), m, None));
            p.send(&what("claim_fees"), &[claim], &cranker, &[]).ok();
            // A buyback lands (a buy, or a wait above its reference); a share or a step may wait.
            let mut steps = vec![];
            if snowball {
                steps.push((companion::snowball_step(cranker.pubkey(), &keys), false));
            }
            if holders {
                steps.push((companion::share(cranker.pubkey(), m), false));
            }
            if buys_back {
                steps.push((companion::buyback(cranker.pubkey(), &keys, holders), true));
            }
            for (step, lands) in steps {
                let tx = p.send(
                    &what("share, buyback or snowball_step"),
                    &[native_of(step)],
                    &cranker,
                    &[],
                );
                if lands {
                    tx.ok();
                }
            }
            // A buyback companion's launcher part is nothing: its withdrawal is refused on both.
            p.send(
                &what("withdraw"),
                &[companion::withdraw(cranker.pubkey(), m, launcher.pubkey())],
                &cranker,
                &[],
            );
        }

        // A launch from a config naming tax_hook (1% of every transfer to a collector).
        let config = Keypair::new();
        p.send(
            "create_config (tax_hook)",
            &[launch::create_config(
                launcher.pubkey(),
                config.pubkey(),
                CreateConfigArgs {
                    rules: LaunchRules::NONE,
                    creator_fee_bps: FEE,
                    custom_hook: Some(tax_hook::ID),
                    custom_hook_flags: tax_hook::FLAGS,
                    label: "Taxed".to_string(),
                },
            )],
            &launcher,
            &[&config],
        )
        .ok();
        let mint = Keypair::new();
        let m = mint.pubkey();
        let collector = Pubkey::new_unique();
        let tax = Pubkey::find_program_address(&[tax_hook::TAX_SEED, m.as_ref()], &tax_hook::ID).0;
        let prepare = Instruction {
            program_id: tax_hook::ID,
            accounts: vec![
                AccountMeta::new(launcher.pubkey(), true),
                AccountMeta::new_readonly(m, false),
                AccountMeta::new_readonly(collector, false),
                AccountMeta::new(tax, false),
                AccountMeta::new(
                    bordrless_hook::hook_accounts_address(&tax_hook::ID, &m).0,
                    false,
                ),
                AccountMeta::new_readonly(bordrless_program_tests::env::SYSTEM_PROGRAM_ID, false),
            ],
            data: anchor_lang::InstructionData::data(&tax_hook::instruction::Prepare {
                fee_bps: 100,
                max_wallet_bps: 0,
            }),
        };
        p.send("prepare tax_hook", &[prepare], &launcher, &[]).ok();
        let ix = p.new.create_launch_from_config_ix(
            &launcher.pubkey(),
            &m,
            "TAXD",
            VQ,
            &config.pubkey(),
        );
        p.send(
            "create_launch from a hook's config",
            &[ix],
            &launcher,
            &[&mint],
        )
        .ok();
        let payer = p.new.env.payer.insecure_clone();
        p.send(
            "the collector's holding",
            &[token::create_holding(payer.pubkey(), m, collector)],
            &payer,
            &[],
        )
        .ok();
        for (t, amt) in [(&t1, SOL), (&t2, 2 * SOL)] {
            p.launch_trade(t, &m, 1, amt).ok();
            let coins = p.new.env.holding(&m, &t.pubkey());
            p.launch_trade(t, &m, 0, coins / 3).ok();
        }
        let src = token::holding_address(&m, &t1.pubkey());
        let dst = token::holding_address(&m, &t2.pubkey());
        let extras = p.new.env.token_hook_extras(
            &tax_hook::ID,
            &m,
            &src,
            &dst,
            &t1.pubkey(),
            &t1.pubkey(),
            &t2.pubkey(),
        );
        let coins = p.new.env.holding(&m, &t1.pubkey());
        let ix = token::transfer_with(
            t1.pubkey(),
            src,
            dst,
            m,
            Some(token::Hook::of(tax_hook::ID)),
            extras,
            coins / 5,
        );
        p.send("transfer (tax_hook)", &[ix], &t1, &[]).ok();
        let claim = if native {
            launch::claim_creator_fees_sol(launcher.pubkey(), m, None, None)
        } else {
            launch::claim_creator_fees(launcher.pubkey(), m, sol)
        };
        p.send("claim_creator_fees (tax_hook)", &[claim], &launcher, &[])
            .ok();
    }
    // Coins quoted in tokens as phase 1 makes them (a wallet the creator): kit rules and none.
    let quote = p.quote_mint(&[launcher.pubkey(), t1.pubkey(), t2.pubkey()]);
    let quote2 = p.quote_mint(&[t1.pubkey(), t2.pubkey()]);
    let kit_rules = LaunchRules {
        burn_buy_bps: 50,
        max_wallet_bps: 500,
        creator_lock_secs: 86_400,
        early_window_secs: 60,
        early_lock_secs: 3_600,
        ..LaunchRules::NONE
    };
    for rules in [kit_rules, LaunchRules::NONE] {
        let m = Keypair::new();
        let ix = p.new.token_launch_ix_with(
            &launcher.pubkey(),
            &m.pubkey(),
            FEE,
            &[(quote, 5_000_000_000 / 2), (quote2, 5_000_000_000 / 2)],
            rules,
        );
        p.send("create_token_launch", &[ix], &launcher, &[&m]).ok();
        let tq = m.pubkey();
        for i in 0..2 {
            let ix = p.new.open_leg_ix(&launcher.pubkey(), &tq, i);
            p.send("open_leg", &[ix], &launcher, &[]).ok();
        }
        p.warp(3_601);
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
                launcher.pubkey(),
                &p.new.leg_keys(&tq, i),
                i as u8,
            );
            p.send("claim_creator_fees_token", &[ix], &t1, &[]).ok();
        }
    }
    println!(
        "{} transactions identical on this tree and on the deployed programs",
        p.steps
    );
}

// ---- Holder rewards in the quote token and the SOL switch (`docs/token-quotes.md` §7.5), against
// the tree before them: the DEX, the launchpad, the kit and the companion with §7 and the presets
// (built, not deployed), and against the programs mainnet runs. ----

/// The binaries before §7.5 (`bordrless_swap.so`, `bordrless_launch.so`, `bordrless_kit.so`,
/// `bordrless_companion.so`, built from the tree with §7 and the presets).
fn payouts_baseline_dir() -> String {
    std::env::var("QUOTES_75_BASELINE_SBF_DIR").expect(
        "QUOTES_75_BASELINE_SBF_DIR: a directory with bordrless_swap.so, bordrless_launch.so, bordrless_kit.so and bordrless_companion.so built before §7.5",
    )
}

impl Pair {
    /// A wallet funded on both forks.
    fn wallet(&mut self, lamports: u64) -> Keypair {
        let k = Keypair::new();
        self.fund(k.pubkey(), lamports);
        k
    }

    /// A quote mint of `program` with `decimals`, and token accounts of it for `owners`, the same
    /// on both forks.
    fn quote_mint_of(&mut self, program: Pubkey, decimals: u8, owners: &[Pubkey]) -> Pubkey {
        let key = Pubkey::new_unique();
        let issuer = self.new.env.deployer.pubkey();
        let data = pack_mint(Some(issuer), None, decimals, u64::MAX / 2, &[]);
        for w in [&mut self.new, &mut self.old] {
            w.env.put_mint(key, program, data.clone());
            for o in owners {
                w.env.token_account(o, &key, &program, QUOTE_FUNDS);
            }
        }
        key
    }

    /// An empty token account of `mint` (of `program`) for `owner`, the same on both forks.
    fn empty_token_account(&mut self, owner: &Pubkey, mint: &Pubkey, program: &Pubkey) -> Pubkey {
        let a = self.new.env.token_account(owner, mint, program, 0);
        let b = self.old.env.token_account(owner, mint, program, 0);
        assert_eq!(a, b);
        a
    }

    /// Opens every leg of `mint`, `payer` paying.
    fn open_legs(&mut self, payer: &Keypair, mint: &Pubkey) {
        for i in 0..usize::from(self.new.legs(mint).count) {
            let ix = self.new.open_leg_ix(&payer.pubkey(), mint, i);
            self.send("open_leg", &[ix], payer, &[]).ok();
        }
    }

    /// A buy of `amount` on leg `i` of `mint` by `t`, then a sell of a third of its coins.
    fn leg_round_trip(&mut self, t: &Keypair, mint: &Pubkey, i: usize, amount: u64) {
        let who = t.pubkey();
        let ixs = [
            token::create_holding(who, *mint, who),
            self.new.leg_swap_ix(&who, &who, mint, i, 1, amount),
        ];
        self.send("leg buy", &ixs, t, &[]).ok();
        let coins = self.new.env.holding(mint, &who);
        let ix = self.new.leg_swap_ix(&who, &who, mint, i, 0, coins / 3);
        self.send("leg sell", &[ix], t, &[]).ok();
    }

    /// `claim_creator_fees_token(i)` of `mint`, sent by `cranker`.
    fn leg_claim(&mut self, cranker: &Keypair, mint: &Pubkey, i: usize) -> Tx {
        let creator = self.new.launch(mint).creator;
        let ix = launch::claim_creator_fees_token(
            cranker.pubkey(),
            creator,
            &self.new.leg_keys(mint, i),
            i as u8,
        );
        self.send("claim_creator_fees_token", &[ix], cranker, &[])
    }
}

/// Coins quoted in tokens without the options of §7.5 (no holder fees, no SOL switch, no payees):
/// every step the same on this tree and on the programs before §7.5. A wallet's coin of one leg
/// without rules and one of three legs (SPL Token and Token-2022, 6 and 9 decimals) with the kit's
/// burn, max wallet and both locks: their legs opened, traded (the kit's refusals included) and
/// claimed; a script companion's coin of three legs through `launch_token`: its legs traded and
/// claimed into the creator address's token accounts, its script buying, paying and selling on a
/// leg. The one intended difference, holder fees on a coin quoted in tokens, refused before and
/// launched now, is checked last on its own.
///
/// The presets' coins are left out: review3's F1 grows `LegPot` (`snowball_reference`,
/// `snowball_last_at`) and §7.5 adds `shared_total` and `swapped_total`, so `CompanionLegs` is
/// 1,293 bytes where it was smaller, on purpose (`token_quotes_presets.rs` holds them).
#[test]
#[ignore = "needs QUOTES_75_BASELINE_SBF_DIR (the DEX, launchpad, kit and companion before §7.5)"]
fn token_coins_without_the_new_options_match_the_programs_before_them_byte_for_byte() {
    use bordrless_companion::client as companion;
    use bordrless_companion::constants::APPROVAL_KIND_SCRIPT;
    use bordrless_companion::instructions::{CreateArgs, CreateScriptArgs};
    use bordrless_companion::state::Split;
    use bordrless_launch::error::LaunchError;
    use script_example as se;

    let mut new = World::new();
    new.env
        .svm
        .add_program(
            se::ID,
            &bordrless_program_tests::program_bytes("script_example"),
        )
        .expect("load the script");
    new.env.set_upgrade_authority(
        se::ID,
        Some(bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[1]),
    );
    let old = fork_with(&new, &payouts_baseline_dir(), &BUILDERS_PROGRAMS);
    let mut p = Pair { new, old, steps: 0 };
    let admin = p.new.env.deployer.insecure_clone();
    let hash = bordrless_program_tests::timelock::programdata_hash(&p.new.env, &se::ID);
    p.send(
        "approve the script",
        &[companion::approve(
            admin.pubkey(),
            se::ID,
            APPROVAL_KIND_SCRIPT,
            hash,
            Pubkey::default(),
        )],
        &admin,
        &[],
    )
    .ok();
    let creator = p.wallet(100 * SOL);
    let launcher = p.wallet(100 * SOL);
    let t1 = p.wallet(100 * SOL);
    let t2 = p.wallet(100 * SOL);
    let cranker = p.wallet(10 * SOL);
    let owners = [creator.pubkey(), t1.pubkey(), t2.pubkey()];
    let spl6 = p.quote_mint_of(TOKEN, 6, &owners);
    let t22 = p.quote_mint_of(TOKEN_2022, 6, &owners);
    let spl9 = p.quote_mint_of(TOKEN, 9, &owners);
    let three = [
        (spl6, 1_666_666_666u64),
        (t22, 1_666_666_666),
        (spl9, 8_333_333_333),
    ];

    // A wallet's coin of one leg, no rules.
    let mint = Keypair::new();
    let one = mint.pubkey();
    let ix = p
        .new
        .token_launch_ix(&creator.pubkey(), &one, FEE, &[(spl6, 5_000_000_000)]);
    p.send("create_token_launch (one leg)", &[ix], &creator, &[&mint])
        .ok();
    p.open_legs(&cranker, &one);
    p.leg_round_trip(&t1, &one, 0, 400_000_000);
    p.leg_round_trip(&t2, &one, 0, 150_000_000);
    p.leg_claim(&cranker, &one, 0).ok();
    assert!(p.leg_claim(&cranker, &one, 0).result.is_err());

    // A wallet's coin of three legs with the kit's burn, max wallet and both locks.
    let rules = LaunchRules {
        burn_buy_bps: 50,
        burn_sell_bps: 50,
        max_wallet_bps: 500,
        creator_lock_secs: 7 * 86_400,
        early_window_secs: 60,
        early_lock_secs: 3_600,
        ..LaunchRules::NONE
    };
    let mint = Keypair::new();
    let ruled = mint.pubkey();
    let ix = p
        .new
        .token_launch_ix_with(&creator.pubkey(), &ruled, FEE, &three, rules);
    p.send(
        "create_token_launch (three legs, rules)",
        &[ix],
        &creator,
        &[&mint],
    )
    .ok();
    p.open_legs(&cranker, &ruled);
    // An early buyer: locked for the hour, its sell refused on both.
    let who = t2.pubkey();
    let ixs = [
        token::create_holding(who, ruled, who),
        p.new.leg_swap_ix(&who, &who, &ruled, 0, 1, 50_000_000),
    ];
    p.send("early leg buy", &ixs, &t2, &[]).ok();
    let coins = p.new.env.holding(&ruled, &who);
    let ix = p.new.leg_swap_ix(&who, &who, &ruled, 0, 0, coins / 2);
    assert!(p.send("early leg sell", &[ix], &t2, &[]).result.is_err());
    p.warp(61);
    for (i, q) in three.iter().enumerate() {
        p.leg_round_trip(&t1, &ruled, i, q.1 / 40);
    }
    // Over max wallet, refused on both.
    let ix = p
        .new
        .leg_swap_ix(&t1.pubkey(), &t1.pubkey(), &ruled, 2, 1, three[2].1 / 2);
    assert!(p
        .send("leg buy over max wallet", &[ix], &t1, &[])
        .result
        .is_err());
    // The creator's own buy, then its sell under the creator lock, refused on both.
    let who = creator.pubkey();
    let ixs = [
        token::create_holding(who, ruled, who),
        p.new.leg_swap_ix(&who, &who, &ruled, 1, 1, 20_000_000),
    ];
    p.send("creator leg buy", &ixs, &creator, &[]).ok();
    let coins = p.new.env.holding(&ruled, &who);
    let ix = p.new.leg_swap_ix(&who, &who, &ruled, 1, 0, coins / 2);
    assert!(p
        .send("creator leg sell", &[ix], &creator, &[])
        .result
        .is_err());
    p.warp(3_600);
    p.leg_round_trip(&t2, &ruled, 0, 30_000_000);
    for i in 0..3 {
        p.leg_claim(&cranker, &ruled, i).ok();
    }

    // A script companion's coin of three legs: every claim the pot (a script on a coin quoted in
    // tokens takes the whole creator fee).
    let mint = Keypair::new();
    let scripted = mint.pubkey();
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            scripted,
            CreateArgs {
                split: Split {
                    buyback_bps: 0,
                    holders_bps: 0,
                    beneficiary_bps: 10_000,
                },
                bounty_bps: 50,
                max_buyback: SOL,
                buyback_interval: 60,
                vest_secs: 0,
                fund: SOL,
            },
        ),
        companion::create_script(
            launcher.pubkey(),
            scripted,
            CreateScriptArgs {
                split: Split {
                    buyback_bps: 0,
                    holders_bps: 0,
                    beneficiary_bps: 0,
                },
                pot_bps: 10_000,
                program: se::ID,
                interval: 60,
                state_len: 64,
                init_data: vec![],
            },
            vec![],
        ),
    ];
    p.send("create + create_script", &ixs, &launcher, &[&mint])
        .ok();
    let address = companion::creator_address(&scripted);
    let mints: Vec<Pubkey> = three.iter().map(|q| q.0).collect();
    let args = World::token_launch_args_with("SCRQ", FEE, &three, LaunchRules::NONE);
    let inner = launch::create_token_launch_with(
        address,
        scripted,
        p.new.env.treasury.pubkey(),
        args.clone(),
        &mints,
        None,
        None,
    );
    p.send(
        "companion launch_token",
        &[companion::launch_token(
            launcher.pubkey(),
            scripted,
            &inner,
            args,
        )],
        &launcher,
        &[&mint],
    )
    .ok();
    p.open_legs(&cranker, &scripted);
    for (i, q) in three.iter().enumerate() {
        p.leg_round_trip(&t1, &scripted, i, q.1 / 50);
        p.leg_claim(&cranker, &scripted, i).ok();
    }
    // Its script, past its first minute: a buy on leg 1, a payment in leg 2's token, the sale of
    // what it bought.
    let holder = Pubkey::new_unique();
    let holder_account = p.empty_token_account(&holder, &spl9, &TOKEN);
    let steps: [(u8, usize, Pubkey); 3] = [
        (se::MODE_LEG_BUY, 1, Pubkey::new_unique()),
        (se::MODE_LEG_PAY, 2, holder_account),
        (se::MODE_LEG_SELL, 1, Pubkey::new_unique()),
    ];
    let mut paid = 0;
    for (mode, i, aux) in steps {
        p.warp(61);
        let keys = p.new.leg_keys(&scripted, i);
        let account = keys.quote_account(&address);
        let held = p.new.env.token_amount(&account);
        let param = match mode {
            se::MODE_LEG_BUY => held / 2,
            se::MODE_LEG_PAY => held / 4,
            _ => 0,
        };
        assert!(held > 0 && (param > 0 || mode == se::MODE_LEG_SELL));
        if mode == se::MODE_LEG_PAY {
            paid = param;
        }
        let trade = mode != se::MODE_LEG_PAY;
        let slice = if trade {
            launch::leg_base_slice(&scripted, 0)
        } else {
            vec![]
        };
        let mut extra = vec![AccountMeta::new(aux, false)];
        extra.extend(slice.clone());
        if trade {
            extra.extend(as_remaining(&launch::leg_swap_with_slice(
                &keys,
                address,
                token::holding_address(&scripted, &address),
                account,
                1,
                0,
                0,
                slice.clone(),
                false,
            )));
        } else {
            extra.extend([
                AccountMeta::new(account, false),
                AccountMeta::new_readonly(keys.quote_mint, false),
                AccountMeta::new_readonly(keys.quote_program, false),
                AccountMeta::new_readonly(launch::legs_address(&scripted), false),
            ]);
        }
        p.poke(
            &se::state_address(&scripted),
            &se::leg_state_bytes(mode, param, i as u8, slice.len() as u8),
        );
        let ix = companion::script_step(
            cranker.pubkey(),
            &p.new.launch_keys(&scripted),
            se::ID,
            extra,
        );
        p.send("script leg step", &[ix], &cranker, &[]).ok();
    }
    assert_eq!(p.new.env.token_amount(&holder_account), paid);

    // The intended difference, on its own wallet and last (the treasury's lamports part from here):
    // holder fees on a coin quoted in tokens, refused before (6082) and launched now.
    let other = p.wallet(100 * SOL);
    let quote = p.quote_mint_of(TOKEN, 6, &[other.pubkey()]);
    let mint = Keypair::new();
    let ix = p.new.token_launch_ix_with(
        &other.pubkey(),
        &mint.pubkey(),
        FEE,
        &[(quote, 5_000_000_000)],
        presets::every_rule(),
    );
    p.new
        .env
        .send_paid_by(std::slice::from_ref(&ix), &other, &[&mint])
        .ok();
    let refused = p.old.env.send_paid_by(&[ix], &other, &[&mint]);
    let code = 6000 + LaunchError::HolderFeeOnTokenLaunch as u32;
    assert_eq!(code, 6082);
    let logs = refused.logs().join("\n");
    assert!(
        refused.result.is_err() && logs.contains(&format!("Error Number: {code}")),
        "the programs before §7.5 refuse holder fees on a coin quoted in tokens:\n{logs}"
    );
    println!(
        "{} transactions identical on this tree and on the programs before §7.5",
        p.steps
    );
}

/// A coin quoted in tokens with the kit's rules made by the programs mainnet runs (the launchpad
/// and the kit with rules on coins quoted in tokens, `BUILDERS_BASELINE_SBF_DIR`): the upgrade to
/// this tree writes nothing; this tree reads it as made without §7.5's options (no reward kind, no
/// payouts, the SOL switch off); it trades on, its creator fees are claimed by anyone, and the SOL
/// switch's sale is refused.
#[test]
#[ignore = "needs BUILDERS_BASELINE_SBF_DIR (the deployed DEX, launchpad, kit and companion .so)"]
fn a_token_coin_with_rules_made_before_the_payouts_upgrade_trades_after_it() {
    use bordrless_kit::constants::REWARD_KIND_OFFSET;
    use bordrless_launch::error::LaunchError;
    let mut w = fork_with(&World::new(), &builders_baseline_dir(), &BUILDERS_PROGRAMS);
    let creator = w.wallet_with_sol(100 * SOL);
    let t1 = w.env.funded(100 * SOL);
    let issuer = w.env.deployer.pubkey();
    let mut quotes = vec![];
    for (program, decimals, vq) in [
        (TOKEN, 6u8, 2_500_000_000u64),
        (TOKEN_2022, 9, 12_500_000_000),
    ] {
        let key = Pubkey::new_unique();
        w.env.put_mint(
            key,
            program,
            pack_mint(Some(issuer), None, decimals, u64::MAX / 2, &[]),
        );
        w.env
            .token_account(&t1.pubkey(), &key, &program, QUOTE_FUNDS);
        quotes.push((key, vq));
    }
    let rules = LaunchRules {
        burn_buy_bps: 50,
        burn_sell_bps: 50,
        max_wallet_bps: 500,
        ..LaunchRules::NONE
    };
    let (mint, tx) = w.create_token_launch_with(&creator, FEE, &quotes, rules);
    tx.ok();
    w.open_all_legs(&mint);
    w.leg_buy(&t1, &mint, 0, 40_000_000).ok();
    let keys = [
        launch::launch_address(&mint),
        launch::legs_address(&mint),
        launch::kit_config_address(&mint),
    ];
    let before: Vec<Vec<u8>> = keys
        .iter()
        .map(|k| w.env.account(k).unwrap().data)
        .collect();
    load(&mut w.env, &tree_dir(), &BUILDERS_PROGRAMS);
    for (k, data) in keys.iter().zip(&before) {
        assert_eq!(
            &w.env.account(k).unwrap().data,
            data,
            "{k}: the upgrade writes nothing"
        );
    }
    let l: Launch = w.launch(&mint);
    assert!(l.is_token_quoted());
    assert_eq!((l.payouts(), l.sol_switch()), (0, false));
    let c: KitConfig = w.env.kit_config(&mint);
    assert!(c.is_token_quoted());
    assert_eq!(c.reserved[REWARD_KIND_OFFSET], 0);
    assert!(!c.accumulates() && !c.pays_token() && !c.pays_rounds());
    for i in 0..2 {
        let vq = w.legs(&mint).legs[i].virtual_quote;
        w.leg_buy(&t1, &mint, i, vq / 60).ok();
        let coins = w.env.holding(&mint, &t1.pubkey());
        w.leg_sell(&t1, &mint, i, coins / 4).ok();
        w.env.assert_spl_invariant(&w.leg_keys(&mint, i).pool());
    }
    let cranker = w.env.funded(SOL);
    for i in 0..2 {
        let ix = launch::claim_creator_fees_token(
            cranker.pubkey(),
            creator.pubkey(),
            &w.leg_keys(&mint, i),
            i as u8,
        );
        w.env.send_paid_by(&[ix], &cranker, &[]).ok();
    }
    // The SOL switch is off: its sale is refused before any venue is read (a reference written in
    // for the leg's token, so the launchpad's own check is what refuses).
    let quote = quotes[0].0;
    let (key, bump) = bordrless_launch::state::QuoteReference::address(&quote);
    let reference = bordrless_launch::state::QuoteReference {
        version: 1,
        bump,
        quote_mint: quote,
        venue: 0,
        via: 0,
        pool: Pubkey::new_unique(),
        registrant: cranker.pubkey(),
        registered_at: w.env.now,
        depth_at: 1_000 * SOL,
        window: 0,
        window_value: 0,
        paused: false,
        sold_total: 0,
        lamports_total: 0,
        reserved: [0; 32],
    };
    let mut data = vec![];
    anchor_lang::AccountSerialize::try_serialize(&reference, &mut data).unwrap();
    w.env.put(
        key,
        solana_account::Account {
            lamports: w.env.rent(data.len()),
            data,
            owner: bordrless_launch::ID,
            executable: false,
            rent_epoch: 0,
        },
    );
    let ix = launch::swap_creator_fees(cranker.pubkey(), &w.leg_keys(&mint, 0), 0, 1, vec![]);
    let tx = w.env.send_paid_by(&[ix], &cranker, &[]);
    let code = 6000 + LaunchError::SolSwitchOff as u32;
    assert!(
        tx.result.is_err()
            && tx
                .logs()
                .join("\n")
                .contains(&format!("Error Number: {code}")),
        "{}",
        tx.logs().join("\n")
    );
}
