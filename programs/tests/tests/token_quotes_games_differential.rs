//! SOL coins' games and Lossback against the programs mainnet runs (`docs/token-quotes.md` §7.6):
//! each step sent to two forks of one world, one with this tree's programs, the other with the
//! deployed DEX, launchpad, kit, token, bridge, companion, `lottery_hook`, `loss_cover_hook` and
//! `loss_cover_script` (`solana program dump`ed, their ELF images, named by `DEPLOYED_76_SBF_DIR`).
//! Every step must end the same way, with the same inner instructions (CPIs and events) and every
//! account it names the same afterwards: a lottery coin's whole draw, a jackpot's settle, a streak's
//! epoch, and a Lossback coin's rounds. (The Studio starters are this tree's on both forks: Studio
//! deploys a new copy for each coin.)
//!
//! And coins quoted in tokens without games (§7.5's options: holder rewards, payees, the SOL
//! switch, a script taking the whole fee) against the tree before §7.6: the companion and the three
//! hooks built from it, named by `QUOTES_76_BASE_SBF_DIR` (every other program is the same bytes).
//!
//! Ignored unless run with the directories, e.g. `DEPLOYED_76_SBF_DIR=~/scratch/quotes-76-mainnet/deploy
//! QUOTES_76_BASE_SBF_DIR=~/scratch/quotes-76-base/target/deploy cargo test -p bordrless-program-tests
//! --test token_quotes_games_differential -- --ignored`.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{AnchorSerialize, InstructionData, ToAccountMetas};
use bordrless_companion::client::{self as companion, SeedSlot};
use bordrless_companion::constants::*;
use bordrless_companion::instructions::{
    CreateArgs, CreateGameArgs, CreateScriptArgs, GameKindArgs, PayoutTermsArgs,
};
use bordrless_companion::oracle as seeds;
use bordrless_companion::state::{Game, GameKind, Split};
use bordrless_core::policy;
use bordrless_game::{draw_index, round_of, round_start};
use bordrless_launch::client as launch;
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{Env, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::VQ;
use bordrless_program_tests::orao;
use bordrless_program_tests::program_bytes;
use bordrless_program_tests::quotes::{pack_mint, Quote, QUOTE_FUNDS, TOKEN, TOKEN_2022};
use bordrless_program_tests::timelock::programdata_hash;
use bordrless_token::client as token;
use loss_cover_script::CoverTerms;
use solana_keypair::Keypair;
use solana_signer::Signer;

const SOL: u64 = 1_000_000_000;
const FEE: u16 = 200;
const JACKPOT: Pubkey = studio_jackpot::ID;
const STREAK: Pubkey = studio_streak::ID;
const STUDIO_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[0];
const BORDRLESS_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[1];

/// The deployed programs this suite compares against, with the upgrade authority mainnet gives
/// each (the hooks Bordrless's hook key, the rest the deployer here).
const DEPLOYED: [(&str, Pubkey, bool); 9] = [
    ("bordrless_swap", bordrless_swap::ID, false),
    ("bordrless_launch", bordrless_launch::ID, false),
    ("bordrless_kit", bordrless_kit::ID, false),
    ("bordrless_token", bordrless_token::ID, false),
    ("bordrless_bridge", bordrless_bridge::ID, false),
    ("bordrless_companion", bordrless_companion::ID, false),
    ("lottery_hook", lottery_hook::ID, true),
    ("loss_cover_hook", loss_cover_hook::ID, true),
    ("loss_cover_script", loss_cover_script::ID, false),
];

fn deployed_dir() -> String {
    std::env::var("DEPLOYED_76_SBF_DIR").expect(
        "DEPLOYED_76_SBF_DIR: the deployed programs' ELF images (bordrless_companion.so, …)",
    )
}

/// `w` forked with the deployed programs, each upgradeable as on mainnet.
fn fork_deployed(w: &World) -> World {
    let mut env = Env {
        svm: w.env.svm.clone(),
        payer: w.env.payer.insecure_clone(),
        now: w.env.now,
        slot: w.env.slot,
        deployer: w.env.deployer.insecure_clone(),
        treasury: w.env.treasury.insecure_clone(),
    };
    let dir = deployed_dir();
    for (name, id, hook) in DEPLOYED {
        let path = format!("{dir}/{name}.so");
        let so = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        env.svm.add_program(id, &so).expect("load the program");
        // The hooks and the Lossback script are upgraded by Bordrless's hook key on mainnet.
        let authority = if hook || id == loss_cover_script::ID {
            BORDRLESS_KEY
        } else {
            env.deployer.pubkey()
        };
        env.set_upgrade_authority(id, Some(authority));
    }
    World { env, sol: w.sol }
}

struct Pair {
    new: World,
    old: World,
    steps: usize,
    /// Accounts that differ between the forks by construction (a script's approval names its
    /// code's hash), left out of the comparison.
    skip: Vec<Pubkey>,
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

    fn wallet(&mut self, sol: u64) -> Keypair {
        let k = Keypair::new();
        self.fund(k.pubkey(), 2 * sol + SOL);
        let ixs = [
            token::create_holding(k.pubkey(), self.new.sol, k.pubkey()),
            bordrless_bridge::client::wrap_sol(k.pubkey(), sol),
        ];
        self.send("wrap", &ixs, &k, &[]).ok();
        k
    }

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
            if self.skip.contains(key) {
                continue;
            }
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

    /// Writes the slot hashes sysvar on both forks, as `draw` reads it.
    fn set_slot_hash(&mut self) -> SeedSlot {
        let slot = self.new.env.slot - 1;
        let mut from = [0u8; 32];
        from[..8].copy_from_slice(&slot.to_le_bytes());
        let hash: [u8; 32] = orao::randomness_for(&from)[..32].try_into().unwrap();
        for w in [&mut self.new, &mut self.old] {
            let mut sysvar = w.env.account(&seeds::SLOT_HASHES).expect("slot hashes");
            sysvar.data[..8].copy_from_slice(&1u64.to_le_bytes());
            sysvar.data[8..16].copy_from_slice(&slot.to_le_bytes());
            sysvar.data[16..48].copy_from_slice(&hash);
            w.env.put(seeds::SLOT_HASHES, sysvar);
        }
        SeedSlot { slot, hash }
    }

    fn config(&mut self, creator: &Keypair, hook: Pubkey, flags: u16) -> Pubkey {
        let config = Keypair::new();
        let ix = launch::create_config(
            creator.pubkey(),
            config.pubkey(),
            CreateConfigArgs {
                rules: LaunchRules::NONE,
                creator_fee_bps: FEE,
                custom_hook: Some(hook),
                custom_hook_flags: flags,
                label: "Game".to_string(),
            },
        );
        self.send("create_config", &[ix], creator, &[&config]).ok();
        config.pubkey()
    }

    /// The companion's `launch` of `mint` from `config`.
    fn launch(&mut self, launcher: &Keypair, mint: &Keypair, config: &Pubkey, hook: Pubkey) {
        let m = mint.pubkey();
        let custom = self.new.custom_hook_accounts(&hook, &m);
        let args = World::launch_args("GAME", FEE, VQ, LaunchRules::NONE);
        let inner = launch::create_launch_with(
            companion::creator_address(&m),
            m,
            self.new.env.treasury.pubkey(),
            self.new.sol,
            policy::LP_FEE_BPS,
            args.clone(),
            Some(*config),
            Some(&custom),
        );
        let ix = companion::launch(launcher.pubkey(), m, &inner, args);
        self.send("launch", &[ix], launcher, &[mint]).ok();
    }

    /// A buy of `sol` of `mint` by a fresh wallet: answers it.
    fn buyer(&mut self, mint: &Pubkey, sol: u64) -> Keypair {
        let t = self.wallet(sol + SOL);
        let ixs = [
            token::create_holding(t.pubkey(), *mint, t.pubkey()),
            self.new.launch_swap_ix(&t.pubkey(), mint, 1, sol, 0),
        ];
        self.send("buy", &ixs, &t, &[]).ok();
        t
    }

    fn sell_all(&mut self, t: &Keypair, mint: &Pubkey) {
        let held = self.new.env.holding(mint, &t.pubkey());
        let ix = self.new.launch_swap_ix(&t.pubkey(), mint, 0, held, 0);
        self.send("sell", &[ix], t, &[]).ok();
    }
}

fn game_world() -> World {
    let mut w = World::new();
    orao::load(&mut w.env);
    for (name, id) in [("studio_jackpot", JACKPOT), ("studio_streak", STREAK)] {
        w.env
            .svm
            .add_program(id, &program_bytes(name))
            .expect("load");
        w.env.set_upgrade_authority(id, Some(STUDIO_KEY));
    }
    bordrless_program_tests::attest::attest_all(&mut w.env, &[JACKPOT, STREAK]);
    w
}

fn create_args(split: Split) -> CreateArgs {
    CreateArgs {
        split,
        bounty_bps: 50,
        max_buyback: SOL,
        buyback_interval: 60,
        vest_secs: 0,
        fund: SOL / 2,
    }
}

const BUYBACK_ONLY: Split = Split {
    buyback_bps: 10_000,
    holders_bps: 0,
    beneficiary_bps: 0,
};

const GAME_SPLIT: Split = Split {
    buyback_bps: 3_000,
    holders_bps: 0,
    beneficiary_bps: 0,
};

/// A SOL lottery coin's whole draw, a jackpot's settle and a streak's epoch: every step the same on
/// this tree and on the deployed companion and `lottery_hook`.
#[test]
#[ignore = "needs DEPLOYED_76_SBF_DIR (the deployed programs' ELF images)"]
fn sol_games_match_the_deployed_programs_byte_for_byte() {
    let new = game_world();
    let old = fork_deployed(&new);
    let mut p = Pair {
        new,
        old,
        steps: 0,
        skip: vec![],
    };
    let launcher = p.wallet(50 * SOL);
    let cranker = p.wallet(10 * SOL);

    // ---- A lottery.
    let mint = Keypair::new();
    let m = mint.pubkey();
    let round = 3_600u32;
    let game = CreateGameArgs {
        kind: GameKind::Lottery,
        hook: lottery_hook::ID,
        split: GAME_SPLIT,
        pot_bps: 7_000,
        round_secs: round,
        min_pot: 100_000_000,
        prize_bps: 10_000,
        claim_window_secs: 300,
        max_attempts: 6,
    };
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            m,
            create_args(BUYBACK_ONLY),
        ),
        lottery_hook::client::prepare(launcher.pubkey(), m, round),
        companion::create_game(launcher.pubkey(), m, game),
    ];
    p.send("lottery setup", &ixs, &launcher, &[&mint]).ok();
    let config = p.config(&launcher, lottery_hook::ID, lottery_hook::FLAGS);
    p.launch(&launcher, &mint, &config, lottery_hook::ID);
    p.warp(31);
    let a = p.buyer(&m, 5 * SOL);
    let next = round_of(p.new.env.now, round) + 1;
    p.warp(round_start(next, round) + 5 - p.new.env.now);
    let ix = lottery_hook::client::enter(m, a.pubkey());
    p.send("enter", &[ix], &cranker, &[]).ok();
    let v = p.buyer(&m, 20 * SOL);
    p.sell_all(&v, &m);
    let ix = companion::claim_fees_game(cranker.pubkey(), m, lottery_hook::ID);
    p.send("claim_fees", &[ix], &cranker, &[]).ok();
    p.warp(round_start(next + 1, round) + 5 - p.new.env.now);
    let at = p.set_slot_hash();
    let g: Game = p.new.env.read(&companion::game_address(&m));
    let ix = companion::draw_after(
        cranker.pubkey(),
        m,
        lottery_hook::ID,
        next,
        at,
        orao::TREASURY,
        g.paid_seed,
    );
    p.send("draw", &[ix], &cranker, &[]).ok();
    let g: Game = p.new.env.read(&companion::game_address(&m));
    let randomness = {
        let mut seed = [7u8; 32];
        seed[0] = 1;
        orao::randomness_for(&seed)
    };
    orao::fulfil(&mut p.new.env, &g.seed, &randomness);
    orao::fulfil(&mut p.old.env, &g.seed, &randomness);
    let ix = companion::reveal(cranker.pubkey(), m, lottery_hook::ID, g.request);
    p.send("reveal", &[ix], &cranker, &[]).ok();
    let g: Game = p.new.env.read(&companion::game_address(&m));
    let ticket = draw_index(&g.randomness, 0, g.total).unwrap();
    assert!(ticket < g.total);
    // Only `a` holds tickets: it wins.
    let ix = companion::claim_prize(cranker.pubkey(), m, lottery_hook::ID, 0, a.pubkey());
    p.send("claim_prize", &[ix], &cranker, &[]).ok();

    // ---- A jackpot.
    let mint = Keypair::new();
    let m = mint.pubkey();
    let jackpot = CreateGameArgs {
        kind: GameKind::Jackpot,
        hook: JACKPOT,
        split: GAME_SPLIT,
        pot_bps: 7_000,
        round_secs: 0,
        min_pot: 100_000_000,
        prize_bps: 5_000,
        claim_window_secs: 0,
        max_attempts: 0,
    };
    let k = GameKindArgs {
        timer_secs: studio_jackpot::TIMER_SECS,
        min_tokens: studio_jackpot::MIN_TOKENS,
        ..GameKindArgs::default()
    };
    let prepare = Instruction {
        program_id: JACKPOT,
        accounts: studio_jackpot::accounts::Prepare {
            payer: launcher.pubkey(),
            mint: m,
            state: bordrless_game::state_address(&JACKPOT, &m).0,
            registry: bordrless_hook::hook_accounts_address(&JACKPOT, &m).0,
            system_program: bordrless_program_tests::SYSTEM_PROGRAM_ID,
        }
        .to_account_metas(None),
        data: studio_jackpot::instruction::Prepare {}.data(),
    };
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            m,
            create_args(BUYBACK_ONLY),
        ),
        prepare,
        companion::create_game_v2_attested(launcher.pubkey(), m, jackpot, k, false),
    ];
    p.send("jackpot setup", &ixs, &launcher, &[&mint]).ok();
    let config = p.config(&launcher, JACKPOT, GameKind::Jackpot.hook_flags());
    p.launch(&launcher, &mint, &config, JACKPOT);
    p.warp(31);
    let v = p.buyer(&m, 20 * SOL);
    p.sell_all(&v, &m);
    let buyer = p.buyer(&m, 3 * SOL);
    let ix = companion::claim_fees_game(cranker.pubkey(), m, JACKPOT);
    p.send("claim_fees", &[ix], &cranker, &[]).ok();
    p.warp(i64::from(studio_jackpot::TIMER_SECS) + 1);
    let ix = companion::settle(cranker.pubkey(), m, JACKPOT, buyer.pubkey());
    p.send("settle", &[ix], &cranker, &[]).ok();

    // ---- A streak.
    let mint = Keypair::new();
    let m = mint.pubkey();
    let epoch = studio_streak::EPOCH_SECS;
    let streak = CreateGameArgs {
        kind: GameKind::Streak,
        hook: STREAK,
        split: GAME_SPLIT,
        pot_bps: 7_000,
        round_secs: epoch,
        min_pot: 100_000_000,
        prize_bps: 10_000,
        claim_window_secs: 3_600,
        max_attempts: 0,
    };
    let k = GameKindArgs {
        min_streak_secs: studio_streak::MIN_STREAK_SECS,
        min_weight: studio_streak::MIN_WEIGHT,
        ..GameKindArgs::default()
    };
    let prepare = Instruction {
        program_id: STREAK,
        accounts: studio_jackpot::accounts::Prepare {
            payer: launcher.pubkey(),
            mint: m,
            state: bordrless_game::state_address(&STREAK, &m).0,
            registry: bordrless_hook::hook_accounts_address(&STREAK, &m).0,
            system_program: bordrless_program_tests::SYSTEM_PROGRAM_ID,
        }
        .to_account_metas(None),
        data: studio_jackpot::instruction::Prepare {}.data(),
    };
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            m,
            create_args(BUYBACK_ONLY),
        ),
        prepare,
        companion::create_game_v2_attested(launcher.pubkey(), m, streak, k, false),
    ];
    p.send("streak setup", &ixs, &launcher, &[&mint]).ok();
    let config = p.config(&launcher, STREAK, GameKind::Streak.hook_flags());
    p.launch(&launcher, &mint, &config, STREAK);
    p.warp(31);
    let holder = p.buyer(&m, 5 * SOL);
    let next = round_of(p.new.env.now, epoch) + 1;
    p.warp(round_start(next, epoch) + 5 - p.new.env.now);
    let enter = Instruction {
        program_id: STREAK,
        accounts: studio_streak::accounts::Enter {
            state: bordrless_game::state_address(&STREAK, &m).0,
            mint: m,
            holding: token::holding_address(&m, &holder.pubkey()),
            hook_authority: bordrless_game::cpi::hook_authority_address(&STREAK).0,
            token_program: bordrless_token::ID,
            token_event_authority: token::event_authority(),
        }
        .to_account_metas(None),
        data: studio_streak::instruction::Enter {}.data(),
    };
    p.send("enter", &[enter], &cranker, &[]).ok();
    let v = p.buyer(&m, 20 * SOL);
    p.sell_all(&v, &m);
    let ix = companion::claim_fees_game(cranker.pubkey(), m, STREAK);
    p.send("claim_fees", &[ix], &cranker, &[]).ok();
    p.warp(round_start(next + 1, epoch) + 5 - p.new.env.now);
    let ix = companion::close_epoch(cranker.pubkey(), m, STREAK, next);
    p.send("close_epoch", &[ix], &cranker, &[]).ok();
    let ix = companion::claim_share(cranker.pubkey(), m, STREAK, next, holder.pubkey());
    p.send("claim_share", &[ix], &cranker, &[]).ok();
    println!(
        "{} transactions identical on this tree and on the deployed programs",
        p.steps
    );
}

/// A SOL Lossback coin's rounds: every step the same on this tree and on the deployed companion,
/// `loss_cover_hook` and `loss_cover_script` (the script's approval aside: it names its code).
#[test]
#[ignore = "needs DEPLOYED_76_SBF_DIR (the deployed programs' ELF images)"]
fn a_sol_lossback_coin_matches_the_deployed_programs_byte_for_byte() {
    let script = loss_cover_script::ID;
    let new = World::new();
    let old = fork_deployed(&new);
    let mut p = Pair {
        new,
        old,
        steps: 0,
        skip: vec![companion::approval_address(&script)],
    };
    let deployer = p.new.env.deployer.insecure_clone();
    for w in [&mut p.new, &mut p.old] {
        w.env.set_upgrade_authority(script, Some(BORDRLESS_KEY));
        let hash = programdata_hash(&w.env, &script);
        w.env
            .send(
                &[companion::approve(
                    deployer.pubkey(),
                    script,
                    APPROVAL_KIND_SCRIPT,
                    hash,
                    Pubkey::default(),
                )],
                &[&deployer],
            )
            .ok();
    }
    let launcher = p.wallet(50 * SOL);
    let cranker = p.wallet(10 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let terms = CoverTerms {
        cover_bps: 10_000,
        min_holding: 1,
        min_loss: 1,
        min_age_secs: 0,
        round_secs: 300,
        min_pot_lamports: 0,
    };
    let mut init = vec![];
    terms.serialize(&mut init).unwrap();
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            m,
            create_args(Split {
                buyback_bps: 0,
                holders_bps: 0,
                beneficiary_bps: 10_000,
            }),
        ),
        loss_cover_hook::client::prepare(launcher.pubkey(), m),
        companion::create_script_v2(
            launcher.pubkey(),
            m,
            CreateScriptArgs {
                split: Split {
                    buyback_bps: 0,
                    holders_bps: 0,
                    beneficiary_bps: 3_000,
                },
                pot_bps: 7_000,
                program: script,
                interval: 15,
                state_len: loss_cover_script::STATE_LEN,
                init_data: init,
            },
            PayoutTermsArgs {
                step_bps: 10_000,
                holding_bps: 10_000,
                window_bps: 10_000,
                window_secs: 300,
            },
            vec![],
        ),
    ];
    p.send("lossback setup", &ixs, &launcher, &[&mint]).ok();
    let config = p.config(&launcher, loss_cover_hook::ID, loss_cover_hook::FLAGS);
    p.launch(&launcher, &mint, &config, loss_cover_hook::ID);
    p.warp(31);
    // A whale pumps, holders buy high, the whale sells half: the holders are under water.
    let whale = p.buyer(&m, 25 * SOL);
    let holders = [p.buyer(&m, 3 * SOL), p.buyer(&m, SOL)];
    let held = p.new.env.holding(&m, &whale.pubkey());
    let ix = p.new.launch_swap_ix(&whale.pubkey(), &m, 0, held / 2, 0);
    p.send("sell half", &[ix], &whale, &[]).ok();
    let ix = companion::claim_fees_script(cranker.pubkey(), m, None);
    p.send("claim_fees", &[ix], &cranker, &[]).ok();
    let step = |p: &mut Pair, what: &str| {
        let mut extra = companion::payout_accounts(&m, &loss_cover_hook::ID);
        for h in &holders {
            extra.push(AccountMeta::new(
                token::holding_address(&m, &h.pubkey()),
                false,
            ));
            extra.push(AccountMeta::new(h.pubkey(), false));
        }
        let keys = launch::LaunchKeys::of(&p.new.launch(&m));
        let ix = companion::script_step(cranker.pubkey(), &keys, script, extra);
        p.send(what, &[ix], &cranker, &[]).ok();
    };
    for round in 0..2 {
        let now = p.new.env.now;
        p.warp(300 - now % 300);
        step(
            &mut p,
            if round == 0 {
                "measuring step"
            } else {
                "paying step"
            },
        );
    }
    println!(
        "{} transactions identical on this tree and on the deployed programs",
        p.steps
    );
}

// ---- Coins quoted in tokens without games against the tree before §7.6 -------------------------------

/// The programs §7.6 changes, built from the tree with §7.5 before it (`QUOTES_76_BASE_SBF_DIR`):
/// every other program is the same bytes on both trees.
const BASE_PROGRAMS: [(&str, Pubkey); 4] = [
    ("bordrless_companion", bordrless_companion::ID),
    ("lottery_hook", lottery_hook::ID),
    ("loss_cover_hook", loss_cover_hook::ID),
    ("loss_cover_script", loss_cover_script::ID),
];

fn base_dir() -> String {
    std::env::var("QUOTES_76_BASE_SBF_DIR").expect(
        "QUOTES_76_BASE_SBF_DIR: the companion, lottery_hook, loss_cover_hook and loss_cover_script built before §7.6",
    )
}

/// `program`'s upgrade authority in `env`.
fn authority_of(env: &Env, program: Pubkey) -> Option<Pubkey> {
    let loader = Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111");
    let (pd, _) = Pubkey::find_program_address(&[program.as_ref()], &loader);
    let data = env.account(&pd).expect("programdata").data;
    (data[12] == 1).then(|| Pubkey::new_from_array(data[13..45].try_into().unwrap()))
}

/// `w` forked with [`BASE_PROGRAMS`] from [`base_dir`], each with the authority it has in `w`.
fn fork_base(w: &World) -> World {
    let mut env = Env {
        svm: w.env.svm.clone(),
        payer: w.env.payer.insecure_clone(),
        now: w.env.now,
        slot: w.env.slot,
        deployer: w.env.deployer.insecure_clone(),
        treasury: w.env.treasury.insecure_clone(),
    };
    let dir = base_dir();
    for (name, id) in BASE_PROGRAMS {
        let path = format!("{dir}/{name}.so");
        let so = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let authority = authority_of(&w.env, id);
        env.svm.add_program(id, &so).expect("load the program");
        env.set_upgrade_authority(id, authority);
    }
    World { env, sol: w.sol }
}

/// A $5,000 opening in a token of `decimals`.
fn cap_raw(decimals: u8) -> u64 {
    match decimals {
        6 => 5_000_000_000,
        8 => 2_000_000_000,
        _ => 25_000_000_000,
    }
}

impl Pair {
    /// A wallet funded with `lamports` on both forks.
    fn funded(&mut self, lamports: u64) -> Keypair {
        let k = Keypair::new();
        self.fund(k.pubkey(), lamports);
        k
    }

    /// Quote mints of `specs` (program, decimals), each at an even share of a $5,000 opening, with
    /// token accounts of each for `owners`, the same on both forks.
    fn quotes(
        &mut self,
        specs: &[(Pubkey, u8)],
        owners: &[Pubkey],
    ) -> (Vec<(Pubkey, Pubkey)>, Vec<Quote>) {
        let n = specs.len() as u64;
        let supply = policy::TOKEN_SUPPLY;
        let issuer = self.new.env.deployer.pubkey();
        let mut quotes = vec![];
        let mut launch_quotes = vec![];
        for (i, (program, decimals)) in specs.iter().enumerate() {
            let key = Pubkey::new_unique();
            let data = pack_mint(Some(issuer), None, *decimals, u64::MAX / 2, &[]);
            for w in [&mut self.new, &mut self.old] {
                w.env.put_mint(key, *program, data.clone());
                for o in owners {
                    w.env.token_account(o, &key, program, QUOTE_FUNDS);
                }
            }
            let share = if i as u64 == n - 1 {
                supply - supply / n * (n - 1)
            } else {
                supply / n
            };
            let vq =
                (u128::from(cap_raw(*decimals)) * u128::from(share) / u128::from(supply)) as u64;
            quotes.push((key, *program));
            launch_quotes.push((key, vq));
        }
        (quotes, launch_quotes)
    }

    /// Opens every leg of `mint`, `payer` paying.
    fn open_legs(&mut self, payer: &Keypair, mint: &Pubkey) {
        for i in 0..usize::from(self.new.legs(mint).count) {
            let ix = self.new.open_leg_ix(&payer.pubkey(), mint, i);
            self.send("open_leg", &[ix], payer, &[]).ok();
        }
    }

    /// A trade on leg `i` of `mint` by `t` (a buy makes its holding first).
    fn leg_trade(
        &mut self,
        t: &Keypair,
        mint: &Pubkey,
        i: usize,
        direction: u8,
        amount: u64,
    ) -> Tx {
        let who = t.pubkey();
        let mut ixs = vec![];
        if direction == 1 {
            ixs.push(token::create_holding(who, *mint, who));
        }
        ixs.push(self.new.leg_swap_ix(&who, &who, mint, i, direction, amount));
        self.send("leg trade", &ixs, t, &[])
    }

    /// Writes `bytes` at the start of `key`'s data on both forks.
    fn poke(&mut self, key: &Pubkey, bytes: &[u8]) {
        for w in [&mut self.new, &mut self.old] {
            let mut a = w.env.account(key).expect("the account");
            a.data[..bytes.len()].copy_from_slice(bytes);
            w.env.put(*key, a);
        }
    }
}

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

/// Holder rewards 1% on buys and sells: the kit's holders' share in the legs' tokens (§7.5).
fn holder_rules() -> LaunchRules {
    LaunchRules {
        holder_fee_buy_bps: 100,
        holder_fee_sell_bps: 100,
        ..LaunchRules::NONE
    }
}

/// Coins quoted in tokens with §7.5's options and no game: every step the same on this tree and on
/// the companion and hooks before §7.6. A three-way split (a buyback, the holders' part, the
/// launcher's part with two payees) on a coin of three legs (SPL Token and Token-2022, 6, 8 and 9
/// decimals) with holder rewards: its legs opened and traded, each leg's fee claimed and split, the
/// holders' part shared with the kit and its round started, each leg's buyback, the payees and the
/// launcher paid in each leg's token, the SOL sale refused without the switch; the same split on a
/// one-leg coin with the SOL switch: its claims, the launcher's part kept for the sale, a pot
/// refused as a part to sell (only a game's is one); and a script companion taking the whole fee on
/// three legs: its claims into the creator address's token accounts, its script buying, paying and
/// selling on a leg. The intended differences come last, each on its own: a game and a script with
/// a launcher's part, refused before on a coin quoted in tokens, launched now.
#[test]
#[ignore = "needs QUOTES_76_BASE_SBF_DIR (the companion and the hooks with §7.5, before §7.6)"]
fn token_coins_without_games_match_the_programs_before_them_byte_for_byte() {
    use bordrless_companion::error::CompanionError;
    use bordrless_companion::instructions::PayeeArgs;
    use bordrless_launch::instructions::{CreateTokenLaunchArgs, TokenPayouts};
    use script_example as se;

    let mut new = World::new();
    new.env
        .svm
        .add_program(se::ID, &program_bytes("script_example"))
        .expect("load the script");
    new.env.set_upgrade_authority(se::ID, Some(BORDRLESS_KEY));
    let old = fork_base(&new);
    let mut p = Pair {
        new,
        old,
        steps: 0,
        skip: vec![],
    };
    let admin = p.new.env.deployer.insecure_clone();
    let hash = programdata_hash(&p.new.env, &se::ID);
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
    let launcher = p.funded(100 * SOL);
    let a = p.funded(100 * SOL);
    let b = p.funded(100 * SOL);
    let cranker = p.funded(10 * SOL);
    let owners = [a.pubkey(), b.pubkey()];
    let (quotes, three) = p.quotes(&[(TOKEN, 6), (TOKEN_2022, 8), (TOKEN, 9)], &owners);
    let mints: Vec<Pubkey> = quotes.iter().map(|q| q.0).collect();
    let split = Split {
        buyback_bps: 2_000,
        holders_bps: 4_000,
        beneficiary_bps: 4_000,
    };
    let companion_args = |split: Split| CreateArgs {
        split,
        bounty_bps: 50,
        max_buyback: SOL,
        buyback_interval: 60,
        vest_secs: 0,
        fund: 2 * SOL,
    };

    // ---- A three-way split with payees on three legs, holder rewards on.
    let mint = Keypair::new();
    let m = mint.pubkey();
    let (p1, p2) = (Pubkey::new_unique(), Pubkey::new_unique());
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            m,
            companion_args(split),
        ),
        companion::set_payees(
            launcher.pubkey(),
            m,
            vec![
                PayeeArgs {
                    wallet: p1,
                    bps: 3_000,
                },
                PayeeArgs {
                    wallet: p2,
                    bps: 2_000,
                },
            ],
        ),
    ];
    p.send("create + set_payees", &ixs, &launcher, &[&mint])
        .ok();
    let args = World::token_launch_args_with("SPLT", FEE, &three, holder_rules());
    let inner = launch::create_token_launch(
        companion::creator_address(&m),
        m,
        p.new.env.treasury.pubkey(),
        args.clone(),
        &mints,
    );
    let ix = companion::launch_token_with_payees(launcher.pubkey(), m, &inner, args);
    p.send("launch_token (payees)", &[ix], &launcher, &[&mint])
        .ok();
    p.open_legs(&cranker, &m);
    for (i, q) in three.iter().enumerate() {
        p.leg_trade(&a, &m, i, 1, q.1 / 20).ok();
        p.leg_trade(&b, &m, i, 1, q.1 / 20).ok();
        p.leg_trade(&a, &m, i, 1, q.1 / 40).ok();
        let coins = p.new.env.holding(&m, &b.pubkey());
        p.leg_trade(&b, &m, i, 0, coins / 3).ok();
    }
    for i in 0..3 {
        let keys = p.new.leg_keys(&m, i);
        let ix = companion::claim_fees_leg(cranker.pubkey(), &keys, i as u8, false);
        p.send("claim_fees_leg", &[ix], &cranker, &[]).ok();
        let ix = companion::share_leg(cranker.pubkey(), &keys, i as u8, false);
        p.send("share_leg", &[ix], &cranker, &[]).ok();
    }
    let ix = bordrless_kit::client::start_round(cranker.pubkey(), m, &quotes);
    p.send("start_round", &[ix], &cranker, &[]).ok();
    p.warp(61);
    for i in 0..3 {
        let keys = p.new.leg_keys(&m, i);
        let ix = companion::buyback_leg(
            cranker.pubkey(),
            &keys,
            i as u8,
            &holder_rules(),
            None,
            false,
        );
        p.send("buyback_leg", &[ix], &cranker, &[]).ok();
        let ix = companion::withdraw_leg_with_payees(
            cranker.pubkey(),
            launcher.pubkey(),
            &keys,
            i as u8,
            &[p1, p2],
        );
        p.send("withdraw_leg (payees)", &[ix], &cranker, &[]).ok();
    }
    let keys = p.new.leg_keys(&m, 0);
    let ix = companion::swap_bucket_to_sol(cranker.pubkey(), &keys, 0, 0, vec![]);
    let tx = p.send("swap_bucket_to_sol (switch off)", &[ix], &cranker, &[]);
    tx.expect_code(u32::from(CompanionError::SolSwitchOff));

    // ---- The same split on one leg with the SOL switch.
    let mint = Keypair::new();
    let m = mint.pubkey();
    let ix = companion::create(
        launcher.pubkey(),
        launcher.pubkey(),
        m,
        companion_args(split),
    );
    p.send("create", &[ix], &launcher, &[&mint]).ok();
    let one = [(quotes[0].0, cap_raw(6))];
    let args = CreateTokenLaunchArgs {
        payouts: TokenPayouts {
            flags: 1,
            holder_round_secs: 0,
        },
        ..World::token_launch_args_with("SWCH", FEE, &one, holder_rules())
    };
    let inner = launch::create_token_launch(
        companion::creator_address(&m),
        m,
        p.new.env.treasury.pubkey(),
        args.clone(),
        &[one[0].0],
    );
    let ix = companion::launch_token(launcher.pubkey(), m, &inner, args);
    p.send("launch_token (switch)", &[ix], &launcher, &[&mint])
        .ok();
    p.open_legs(&cranker, &m);
    p.leg_trade(&a, &m, 0, 1, cap_raw(6) / 10).ok();
    let coins = p.new.env.holding(&m, &a.pubkey());
    p.leg_trade(&a, &m, 0, 0, coins / 2).ok();
    let keys = p.new.leg_keys(&m, 0);
    let ix = companion::claim_fees_leg(cranker.pubkey(), &keys, 0, false);
    p.send("claim_fees_leg (switch)", &[ix], &cranker, &[]).ok();
    // The launcher's part waits for its sale; a pot is no part to sell but a game's.
    let ix = companion::withdraw_leg(cranker.pubkey(), launcher.pubkey(), &keys, 0);
    assert!(p
        .send("withdraw_leg (switch)", &[ix], &cranker, &[])
        .result
        .is_err());
    let ix = companion::swap_bucket_to_sol(cranker.pubkey(), &keys, 0, 2, vec![]);
    let tx = p.send("swap_bucket_to_sol (the pot)", &[ix], &cranker, &[]);
    tx.expect_code(u32::from(CompanionError::NotSwitchedPart));

    // ---- A script companion taking the whole fee on three legs.
    let mint = Keypair::new();
    let scripted = mint.pubkey();
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            scripted,
            companion_args(Split {
                buyback_bps: 0,
                holders_bps: 0,
                beneficiary_bps: 10_000,
            }),
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
    let args = World::token_launch_args_with("SCRQ", FEE, &three, LaunchRules::NONE);
    let inner = launch::create_token_launch(
        address,
        scripted,
        p.new.env.treasury.pubkey(),
        args.clone(),
        &mints,
    );
    let ix = companion::launch_token(launcher.pubkey(), scripted, &inner, args);
    p.send("launch_token (script)", &[ix], &launcher, &[&mint])
        .ok();
    p.open_legs(&cranker, &scripted);
    for (i, q) in three.iter().enumerate() {
        p.leg_trade(&a, &scripted, i, 1, q.1 / 50).ok();
        let coins = p.new.env.holding(&scripted, &a.pubkey());
        p.leg_trade(&a, &scripted, i, 0, coins / 3).ok();
        let keys = p.new.leg_keys(&scripted, i);
        let ix = launch::claim_creator_fees_token(cranker.pubkey(), address, &keys, i as u8);
        p.send("claim_creator_fees_token", &[ix], &cranker, &[])
            .ok();
    }
    // Its script, past its first minute: a buy on leg 1, a payment in leg 2's token, the sale of
    // what it bought.
    let holder = Pubkey::new_unique();
    let holder_account = {
        let x = p.new.env.token_account(&holder, &quotes[2].0, &TOKEN, 0);
        let y = p.old.env.token_account(&holder, &quotes[2].0, &TOKEN, 0);
        assert_eq!(x, y);
        x
    };
    let steps: [(u8, usize, Pubkey); 3] = [
        (se::MODE_LEG_BUY, 1, Pubkey::new_unique()),
        (se::MODE_LEG_PAY, 2, holder_account),
        (se::MODE_LEG_SELL, 1, Pubkey::new_unique()),
    ];
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
    assert!(p.new.env.token_amount(&holder_account) > 0);
    println!(
        "{} transactions identical on this tree and on the programs before §7.6",
        p.steps
    );

    // ---- The intended differences, each sent to each fork on its own.
    // A lottery on a coin quoted in tokens: refused before (`TokenLaunchKind`), launched now with
    // its token terms and the coin's `Legs` in its registry.
    let mint = Keypair::new();
    let m = mint.pubkey();
    let round = 3_600u32;
    let game = || CreateGameArgs {
        kind: GameKind::Lottery,
        hook: lottery_hook::ID,
        split: GAME_SPLIT,
        pot_bps: 7_000,
        round_secs: round,
        min_pot: 100_000_000,
        prize_bps: 10_000,
        claim_window_secs: 300,
        max_attempts: 6,
    };
    let create = companion::create(
        launcher.pubkey(),
        launcher.pubkey(),
        m,
        companion_args(BUYBACK_ONLY),
    );
    let ixs_new = [
        create.clone(),
        lottery_hook::client::prepare_legs(launcher.pubkey(), m, round),
        companion::create_game(launcher.pubkey(), m, game()),
        companion::set_game_token_terms(m, 100),
    ];
    let ixs_old = [
        create,
        lottery_hook::client::prepare(launcher.pubkey(), m, round),
        companion::create_game(launcher.pubkey(), m, game()),
    ];
    p.new.env.send_paid_by(&ixs_new, &launcher, &[&mint]).ok();
    p.old.env.send_paid_by(&ixs_old, &launcher, &[&mint]).ok();
    let config = Keypair::new();
    let ix = launch::create_config(
        launcher.pubkey(),
        config.pubkey(),
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: FEE,
            custom_hook: Some(lottery_hook::ID),
            custom_hook_flags: lottery_hook::FLAGS,
            label: "Game".to_string(),
        },
    );
    for w in [&mut p.new, &mut p.old] {
        w.env
            .send_paid_by(std::slice::from_ref(&ix), &launcher, &[&config])
            .ok();
    }
    let custom = launch::CustomHookAccounts {
        program: lottery_hook::ID,
        extras: vec![
            AccountMeta::new(
                bordrless_game::state_address(&lottery_hook::ID, &m).0,
                false,
            ),
            AccountMeta::new_readonly(launch::launch_address(&m), false),
            AccountMeta::new_readonly(launch::legs_address(&m), false),
        ],
    };
    let args = World::token_launch_args_with("LOTQ", FEE, &three, LaunchRules::NONE);
    let inner = launch::create_token_launch_with(
        companion::creator_address(&m),
        m,
        p.new.env.treasury.pubkey(),
        args.clone(),
        &mints,
        Some(config.pubkey()),
        Some(&custom),
    );
    let ix = companion::launch_token_game(launcher.pubkey(), m, &inner, args);
    p.new
        .env
        .send_paid_by(std::slice::from_ref(&ix), &launcher, &[&mint])
        .ok();
    p.old
        .env
        .send_paid_by(&[ix], &launcher, &[&mint])
        .expect_code(u32::from(CompanionError::TokenLaunchKind));
    // A script with a launcher's part: refused before (`TokenCompanionSplit`), its pot and the
    // launcher's part kept per leg now.
    let mint = Keypair::new();
    let m = mint.pubkey();
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            m,
            companion_args(BUYBACK_ONLY),
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
    p.new.env.send_paid_by(&ixs, &launcher, &[&mint]).ok();
    p.old.env.send_paid_by(&ixs, &launcher, &[&mint]).ok();
    let args = World::token_launch_args_with("SCRP", FEE, &three, LaunchRules::NONE);
    let inner = launch::create_token_launch(
        companion::creator_address(&m),
        m,
        p.new.env.treasury.pubkey(),
        args.clone(),
        &mints,
    );
    let ix = companion::launch_token(launcher.pubkey(), m, &inner, args);
    p.new
        .env
        .send_paid_by(std::slice::from_ref(&ix), &launcher, &[&mint])
        .ok();
    assert!(p
        .new
        .env
        .account(&companion::companion_legs_address(&m))
        .is_some());
    p.old
        .env
        .send_paid_by(&[ix], &launcher, &[&mint])
        .expect_code(u32::from(CompanionError::TokenCompanionSplit));
}
