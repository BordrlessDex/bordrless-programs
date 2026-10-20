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

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_core::policy;
use bordrless_launch::client as launch;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{Env, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::VQ;
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

/// `w` forked with the baseline's DEX and launchpad (upgradeable by the deployer, as `Env::new`
/// leaves them).
fn fork_baseline(w: &World) -> World {
    let mut env = Env {
        svm: w.env.svm.clone(),
        payer: w.env.payer.insecure_clone(),
        now: w.env.now,
        slot: w.env.slot,
        deployer: w.env.deployer.insecure_clone(),
        treasury: w.env.treasury.insecure_clone(),
    };
    let deployer = env.deployer.pubkey();
    for (name, id) in [
        ("bordrless_swap", bordrless_swap::ID),
        ("bordrless_launch", bordrless_launch::ID),
    ] {
        let path = format!("{}/{name}.so", baseline_dir());
        let so = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        env.svm.add_program(id, &so).expect("load the baseline");
        env.set_upgrade_authority(id, Some(deployer));
    }
    World { env, sol: w.sol }
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
