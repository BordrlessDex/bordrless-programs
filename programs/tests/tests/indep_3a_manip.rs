//! Independent phase 3a audit, manipulation and fairness lens.
//!
//! The auditor's PoCs asserted each weakness; since the fixes (bordrless-games-work/log-3a-fix.md)
//! each `x<N>_...` test asserts the fixed behaviour, and the residuals left by design are named as
//! such. Only the `.so` files in `target/deploy` run.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::InstructionData;
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{CreateArgs, CreateGameArgs, HookStatusArgs, StrategyArgs};
use bordrless_companion::state::{Companion, GameKind, Split};
use bordrless_core::policy;
use bordrless_game::round_of;
use bordrless_hook::authority::{programdata_address, trimmed_len, MIN_ACCEPTED_DELAY};
use bordrless_hook::{hook_accounts_address, AccountSource, ExtraAccount, HookAccountList, Seed};
use bordrless_launch::client as launch;
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{Env, Tx, SYSTEM_PROGRAM_ID};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::program_bytes;
use bordrless_program_tests::risk::{risk_label_of, RawAccount, RiskAccounts};
use bordrless_program_tests::timelock::*;
use bordrless_program_tests::vault::*;
use hook_timelock::client as tl;
use hook_timelock::loader;
use hook_vault::events::*;
use lottery_hook::client as lottery;
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;
use strategy_tester as st;

const DAY: i64 = 86_400;

fn raw(env: &Env, key: &Pubkey) -> Option<RawAccount> {
    env.account(key).map(|a| RawAccount {
        owner: a.owner,
        data: a.data,
        executable: a.executable,
    })
}

fn label_accounts(env: &Env, program: &Pubkey) -> RiskAccounts {
    RiskAccounts {
        program_id: *program,
        program: raw(env, program),
        programdata: raw(env, &programdata_address(program)),
        timelock: raw(env, &timelock_of(program)),
        timelock_programdata: raw(env, &programdata_address(&hook_timelock::ID)),
        status: raw(env, &companion::hook_status_address(program)),
        attestation: raw(env, &companion::attestation_address(program)),
    }
}

/// Grows `program`'s ProgramData so `code` fits (anyone may: `ExtendProgram`).
fn grow_for(env: &mut Env, program: &Pubkey, code: &[u8]) {
    let pd = programdata_address(program);
    let have = env.account(&pd).unwrap().data.len() - 45;
    if code.len() <= have {
        return;
    }
    let grow = ((code.len() - have) as u32).max(10_240);
    let payer = env.payer.pubkey();
    env.send(&[loader::extend_program(pd, *program, payer, grow)], &[])
        .ok();
    env.warp(1);
}

/// Stages `code` for `program` in its timelock: a buffer whose authority is the timelock, proposed
/// by `author`. Answers the buffer.
fn stage(env: &mut Env, author: &Keypair, program: Pubkey, code: &[u8]) -> Pubkey {
    let buffer = Pubkey::new_unique();
    put_buffer(env, buffer, timelock_of(&program), code, 0);
    env.send(
        &[tl::propose(
            author.pubkey(),
            program,
            buffer,
            trimmed_len(code) as u32,
        )],
        &[author],
    )
    .ok();
    buffer
}

// ================================================================================== X1: launch

fn tax_args() -> CreateConfigArgs {
    CreateConfigArgs {
        rules: LaunchRules::NONE,
        creator_fee_bps: 100,
        custom_hook: Some(tax_hook::ID),
        custom_hook_flags: tax_hook::FLAGS,
        label: "Taxed".to_string(),
    }
}

/// tax_hook prepared for `mint`: 1% of transfers to `collector`.
fn tax_prepare_ix(payer: &Pubkey, mint: Pubkey, collector: Pubkey) -> Instruction {
    let tax = Pubkey::find_program_address(&[tax_hook::TAX_SEED, mint.as_ref()], &tax_hook::ID).0;
    Instruction {
        program_id: tax_hook::ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(collector, false),
            AccountMeta::new(tax, false),
            AccountMeta::new(hook_accounts_address(&tax_hook::ID, &mint).0, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data: tax_hook::instruction::Prepare {
            fee_bps: 100,
            max_wallet_bps: 0,
        }
        .data(),
    }
}

/// X1 (launch), fixed. A timelocked custom hook is taken only while its `Timelock` holds no
/// proposal: at `create_config` (`HookTimelockPending`), and again at every launch from a config made
/// on a timelocked hook (the config records it, `LaunchConfig::hook_timelocked`; the launch passes
/// the hook's `Timelock` after its extras). The auditor's honeypot (stage other code, wait out the
/// delay, then make the config and launch with the switch executable) is refused at both steps; a
/// config made while nothing was pending can't launch once something is (staged before or after
/// the delay ran out); a cancelled proposal lets the launch through.
#[test]
fn x1_a_timelocked_hook_with_other_code_staged_gets_no_config_and_no_launch() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(30 * SOL);
    w.env
        .set_upgrade_authority(tax_hook::ID, Some(creator.pubkey()));
    register(
        &mut w.env,
        &creator,
        tax_hook::ID,
        MIN_ACCEPTED_DELAY,
        creator.pubkey(),
    )
    .ok();
    let pending = u32::from(bordrless_launch::error::LaunchError::HookTimelockPending);
    // Staged before the coin exists; the delay runs out with nobody holding it.
    let new_code = program_bytes("half_life");
    let buffer = stage(&mut w.env, &creator, tax_hook::ID, &new_code);
    w.env.warp(i64::from(MIN_ACCEPTED_DELAY) + 1);
    // The auditor's sequence: the config is refused.
    let config = Keypair::new();
    let ix = launch::create_config_timelocked(creator.pubkey(), config.pubkey(), tax_args());
    w.env
        .send_paid_by(&[ix], &creator, &[&config])
        .expect_code(pending);
    // The label says it too (high: "executable now").
    let label = risk_label_of(&label_accounts(&w.env, &tax_hook::ID), w.env.now, None);
    assert_eq!((label.class, label.severity), ("timelocked", "high"));
    assert!(label.words.contains("executable now"), "{}", label.words);
    // The author cancels: the config is made (and records the timelocked hook).
    w.env
        .send_paid_by(
            &[tl::cancel(creator.pubkey(), tax_hook::ID, buffer)],
            &creator,
            &[],
        )
        .ok();
    w.env.svm.expire_blockhash();
    let config = Keypair::new();
    let ix = launch::create_config_timelocked(creator.pubkey(), config.pubkey(), tax_args());
    w.env.send_paid_by(&[ix], &creator, &[&config]).ok();
    assert!(w.launch_config(&config.pubkey()).hook_timelocked());
    // Then other code is staged again, and waited out: no launch from the clean config.
    let buffer = stage(&mut w.env, &creator, tax_hook::ID, &new_code);
    let mint = Keypair::new();
    let collector = Keypair::new().pubkey();
    let ix = tax_prepare_ix(&creator.pubkey(), mint.pubkey(), collector);
    w.env.send_paid_by(&[ix], &creator, &[]).ok();
    let launch_ix = |w: &World| {
        w.create_launch_from_config_ix(
            &creator.pubkey(),
            &mint.pubkey(),
            "RUG",
            VQ,
            &config.pubkey(),
        )
    };
    // Pending, not yet executable: refused.
    w.env
        .send_paid_by(&[launch_ix(&w)], &creator, &[&mint])
        .expect_code(pending);
    // Executable now: refused.
    w.env.warp(i64::from(MIN_ACCEPTED_DELAY) + 1);
    w.env.svm.expire_blockhash();
    w.env
        .send_paid_by(&[launch_ix(&w)], &creator, &[&mint])
        .expect_code(pending);
    // An old client's accounts (no `Timelock`) don't get around it.
    let mut bare = launch_ix(&w);
    bare.accounts.pop();
    w.env.svm.expire_blockhash();
    w.env
        .send_paid_by(&[bare], &creator, &[&mint])
        .expect_code(u32::from(
            bordrless_launch::error::LaunchError::HookTimelockInvalid,
        ));
    // A forged account in the timelock's place neither.
    let mut forged = launch_ix(&w);
    forged.accounts.last_mut().unwrap().pubkey = creator.pubkey();
    w.env.svm.expire_blockhash();
    w.env
        .send_paid_by(&[forged], &creator, &[&mint])
        .expect_code(u32::from(
            bordrless_launch::error::LaunchError::HookTimelockInvalid,
        ));
    // Cancelled: the coin launches, its holders get the whole notice for any later proposal.
    w.env
        .send_paid_by(
            &[tl::cancel(creator.pubkey(), tax_hook::ID, buffer)],
            &creator,
            &[],
        )
        .ok();
    w.env.svm.expire_blockhash();
    w.env
        .send_paid_by(&[launch_ix(&w)], &creator, &[&mint])
        .ok();
    let m = mint.pubkey();
    assert_eq!(w.launch(&m).custom_hook, Some(tax_hook::ID));
    let buyer = w.wallet_with_sol(5 * SOL);
    w.env.warp(61);
    w.buy(&buyer, &m, SOL).ok();
    let held = w.env.holding(&m, &buyer.pubkey());
    w.sell(&buyer, &m, held / 2).ok();
}

/// X1, the residual (documented, `docs/phase3a.md` §17): a config made while the hook was
/// Bordrless-managed (Studio's key) is not flagged, so if that key later hands the hook to a
/// timelock (owner decision 3's "timelocked by me", done in the wrong order), launches from the old
/// config don't look at the timelock. Only Bordrless's own key can make this transition (an
/// immutable hook stays immutable; an author's hook never gets a config). Studio registers the
/// timelock before making any config, the server refuses to build a launch while the hook's timelock
/// holds a proposal, and the label reads high ("executable now").
#[test]
fn x1_residual_a_config_made_before_studio_handed_its_hook_to_a_timelock_is_not_flagged() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(30 * SOL);
    w.env.set_upgrade_authority(tax_hook::ID, Some(STUDIO_KEY));
    let config = Keypair::new();
    let ix = launch::create_config_timelocked(creator.pubkey(), config.pubkey(), tax_args());
    w.env.send_paid_by(&[ix], &creator, &[&config]).ok();
    assert!(!w.launch_config(&config.pubkey()).hook_timelocked());
    // Studio's key hands it to a timelock (sigverify off: the key is Studio's), the author stages.
    w.env.without_sigverify();
    let author = w.env.funded(5 * SOL);
    send_as(
        &mut w.env,
        &[tl::register(
            author.pubkey(),
            STUDIO_KEY,
            tax_hook::ID,
            MIN_ACCEPTED_DELAY,
            author.pubkey(),
        )],
        &[&author],
        &[STUDIO_KEY],
    )
    .ok();
    w.env.warp(1);
    let new_code = program_bytes("half_life");
    stage(&mut w.env, &author, tax_hook::ID, &new_code);
    let label = risk_label_of(&label_accounts(&w.env, &tax_hook::ID), w.env.now, None);
    assert_eq!((label.class, label.severity), ("timelocked", "high"));
    let mint = Keypair::new();
    let collector = Keypair::new().pubkey();
    let ix = tax_prepare_ix(&creator.pubkey(), mint.pubkey(), collector);
    w.env.send_paid_by(&[ix], &creator, &[]).ok();
    let ix = w.create_launch_from_config_ix(
        &creator.pubkey(),
        &mint.pubkey(),
        "OLD",
        VQ,
        &config.pubkey(),
    );
    w.env.send_paid_by(&[ix], &creator, &[&mint]).ok();
}

// ================================================================================ X2: strategy

const STRATEGY: Pubkey = st::ID;
const HOOK: Pubkey = lottery_hook::ID;
const HOUR: u32 = 3_600;
const WINDOW: u32 = 600;
const MIN_POT: u64 = 100_000_000;

fn create_args() -> CreateArgs {
    CreateArgs {
        split: Split {
            buyback_bps: 10_000,
            holders_bps: 0,
            beneficiary_bps: 0,
        },
        bounty_bps: 50,
        max_buyback: SOL,
        buyback_interval: 60,
        vest_secs: 0,
        fund: SOL / 2,
    }
}

fn game_args(round_secs: u32) -> CreateGameArgs {
    CreateGameArgs {
        kind: GameKind::Strategy,
        hook: HOOK,
        split: Split {
            buyback_bps: 3_000,
            holders_bps: 0,
            beneficiary_bps: 0,
        },
        pot_bps: 7_000,
        round_secs,
        min_pot: MIN_POT,
        prize_bps: 0,
        claim_window_secs: WINDOW,
        max_attempts: 0,
    }
}

fn strategy_args() -> StrategyArgs {
    StrategyArgs {
        strategy: STRATEGY,
        budget_bps: MAX_STRATEGY_BUDGET_BPS,
        max_share_bps: MAX_STRATEGY_SHARE_BPS,
        max_per_tx: MAX_STRATEGY_PER_TX,
        plan_cu_max: MAX_PLAN_CU,
        entitle_cu_max: MAX_ENTITLE_CU,
        min_weight: 1,
    }
}

fn config_address(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"config", mint.as_ref()], &STRATEGY).0
}

fn put_owned(w: &mut World, key: Pubkey, owner: Pubkey, data: Vec<u8>) {
    let lamports = w.env.rent(data.len());
    w.env.put(
        key,
        Account {
            lamports,
            data,
            owner,
            executable: false,
            rent_epoch: 0,
        },
    );
}

fn prepare_strategy(w: &mut World, mint: &Pubkey) {
    put_owned(
        w,
        config_address(mint),
        STRATEGY,
        st::config_bytes(st::MODE_PRO_RATA, 0, st::MODE_PRO_RATA, 0),
    );
    put_owned(
        w,
        bordrless_strategy::registry_address(&STRATEGY, mint).0,
        STRATEGY,
        HookAccountList::new(vec![ExtraAccount {
            writable: false,
            source: AccountSource::Pda {
                program: STRATEGY,
                seeds: vec![Seed::Literal(b"config".to_vec()), Seed::Account(0)],
            },
        }])
        .encode(),
    );
}

fn extras(mint: &Pubkey) -> Vec<Pubkey> {
    vec![config_address(mint)]
}

struct Strat {
    w: World,
    mint: Pubkey,
    cranker: Keypair,
    round_secs: u32,
    created: Tx,
}

impl Strat {
    fn new(w: World, round_secs: u32, timelocked: bool) -> Self {
        Self::with_vetting(w, round_secs, timelocked, vec![])
    }

    /// [`Strat::new`] with the ticket hook's vetting accounts (its ProgramData when its status
    /// records an audit hash).
    fn with_vetting(
        mut w: World,
        round_secs: u32,
        timelocked: bool,
        hook_vetting: Vec<AccountMeta>,
    ) -> Self {
        let launcher = w.wallet_with_sol(50 * SOL);
        let mint_kp = Keypair::new();
        let mint = mint_kp.pubkey();
        prepare_strategy(&mut w, &mint);
        let ixs = vec![
            companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
            lottery::prepare(launcher.pubkey(), mint, round_secs),
            companion::create_strategy_game(
                launcher.pubkey(),
                mint,
                game_args(round_secs),
                strategy_args(),
                hook_vetting,
                timelocked,
                &extras(&mint),
            ),
        ];
        let created = w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]);
        if created.result.is_ok() {
            Self::launch(&mut w, &launcher, &mint_kp);
        }
        w.env.warp(31);
        let cranker = w.wallet_with_sol(5 * SOL);
        Self {
            w,
            mint,
            cranker,
            round_secs,
            created,
        }
    }

    /// The coin's launch through its companion, once its game is made.
    fn launch(w: &mut World, launcher: &Keypair, mint_kp: &Keypair) {
        let mint = mint_kp.pubkey();
        let (config, tx) = w.create_config(
            launcher,
            CreateConfigArgs {
                rules: LaunchRules::NONE,
                creator_fee_bps: 200,
                custom_hook: Some(HOOK),
                custom_hook_flags: lottery_hook::FLAGS,
                label: "Strategy".to_string(),
            },
        );
        tx.ok();
        let c = w.launch_config(&config);
        let custom = c.custom_hook.map(|h| w.custom_hook_accounts(&h, &mint));
        let mut args = World::launch_args("STRAT", c.creator_fee_bps, VQ, c.rules);
        args.name = "Strategy".to_string();
        let inner = launch::create_launch_with(
            companion::creator_address(&mint),
            mint,
            w.env.treasury.pubkey(),
            w.sol,
            policy::LP_FEE_BPS,
            args.clone(),
            Some(config),
            custom.as_ref(),
        );
        let ix = companion::launch(launcher.pubkey(), mint, &inner, args);
        w.env.send_paid_by(&[ix], launcher, &[mint_kp]).ok();
    }

    fn round(&self) -> u32 {
        round_of(self.w.env.now, self.round_secs)
    }

    fn warp_into(&mut self, round: u32, secs: i64) {
        let t = i64::from(round) * i64::from(self.round_secs) + secs;
        assert!(t >= self.w.env.now);
        self.w.env.warp(t - self.w.env.now);
    }

    fn send(&mut self, ix: Instruction) -> Tx {
        let cranker = self.cranker.insecure_clone();
        self.w.env.svm.expire_blockhash();
        self.w.env.send_paid_by(&[ix], &cranker, &[])
    }

    fn companion(&self) -> Companion {
        self.w.env.read(&companion::companion_address(&self.mint))
    }

    fn grow_pot_above(&mut self, lamports: u64) {
        for _ in 0..80 {
            if self.companion().pending_pot > lamports {
                return;
            }
            let t = self.w.wallet_with_sol(21 * SOL);
            self.w.buy(&t, &self.mint, 20 * SOL).ok();
            let held = self.w.env.holding(&self.mint, &t.pubkey());
            self.w.sell(&t, &self.mint, held).ok();
            let ix = companion::with_hook_code(
                companion::claim_fees_game(self.cranker.pubkey(), self.mint, HOOK),
                &HOOK,
            );
            self.send(ix).ok();
        }
        assert!(self.companion().pending_pot > lamports, "pot did not grow");
    }

    fn plan_ix(&self, period: u32) -> Instruction {
        companion::plan_period(
            self.cranker.pubkey(),
            self.mint,
            HOOK,
            STRATEGY,
            self.w.launch(&self.mint).pool,
            &extras(&self.mint),
            period,
        )
    }

    /// [`Strat::plan_ix`] with the ticket hook's ProgramData (needed once its status records an
    /// audit hash).
    fn plan_ix_with_hook_code(&self, period: u32) -> Instruction {
        companion::with_hook_code(self.plan_ix(period), &HOOK)
    }
}

/// X2 (strategy), fixed. `create_strategy_game` takes a timelocked strategy only while its
/// `Timelock` holds no proposal (`TimelockPending`): a game is never made on code whose replacement
/// is staged (pending or already executable), so its players get the whole notice for any later
/// proposal. Here the auditor's sequence (stage, wait out the delay, make the game) is refused, and
/// so is a proposal still within its delay; once cancelled, the game is made.
#[test]
fn x2_a_strategy_game_is_refused_while_its_timelocked_strategy_has_code_staged() {
    let world = || {
        let mut w = World::new();
        w.env
            .svm
            .add_program(STRATEGY, &program_bytes("strategy_tester"))
            .expect("load the tester");
        let author = w.env.funded(10 * SOL);
        w.env.set_upgrade_authority(STRATEGY, Some(author.pubkey()));
        register(
            &mut w.env,
            &author,
            STRATEGY,
            MIN_ACCEPTED_DELAY,
            author.pubkey(),
        )
        .ok();
        (w, author)
    };
    let pending = u32::from(bordrless_companion::error::CompanionError::TimelockPending);
    let new_code = program_bytes("strategy_pro_rata");
    // Staged and waited out (the auditor's sequence): refused.
    let (mut w, author) = world();
    stage(&mut w.env, &author, STRATEGY, &new_code);
    w.env.warp(i64::from(MIN_ACCEPTED_DELAY) + 1);
    let label = risk_label_of(&label_accounts(&w.env, &STRATEGY), w.env.now, None);
    assert_eq!((label.class, label.severity), ("timelocked", "high"));
    assert!(label.words.contains("executable now"), "{}", label.words);
    let s = Strat::new(w, HOUR, true);
    s.created.expect_code(pending);
    // Staged a minute ago (its delay still running): refused too.
    let (mut w, author) = world();
    stage(&mut w.env, &author, STRATEGY, &new_code);
    w.env.warp(60);
    let s = Strat::new(w, HOUR, true);
    s.created.expect_code(pending);
    // Cancelled first: the game is made, timelocked.
    let (mut w, author) = world();
    let buffer = stage(&mut w.env, &author, STRATEGY, &new_code);
    w.env
        .send(&[tl::cancel(author.pubkey(), STRATEGY, buffer)], &[&author])
        .ok();
    let s = Strat::new(w, HOUR, true);
    let set: StrategySet = s.created.event();
    assert_eq!(set.class, strategy_class::TIMELOCKED);
}

// ==================================================================================== X3: vault

/// X3 (vault), fixed. `open_vault` is the vault creator's (or a sender with the mint's keypair):
/// a stranger can't pick the moment (`NotOpener`), and the sells' reference opens at most at the
/// coin's opening price (its launch's curve start), so a pump around the open, a sandwich of the
/// creator's own open included, never leaves a reference the sells must wait under. Here the
/// attacker's own open is refused; the attacker then sandwiches the creator's open with the same
/// 30 SOL pump: the reference is the opening price, and the slot sells at the first crank, as the
/// fairly opened control does.
#[test]
fn x3_nobody_but_the_creator_opens_and_a_pumped_open_leaves_no_poisoned_reference() {
    let mut w = World::new();
    let to = w.env.funded(SOL);
    let mut args = vault_args(vec![sell_for_sol(to.pubkey())]);
    args.interval = DAY;
    let fair = w.vault_coin_spec(
        args.clone(),
        &[],
        &CoinSpec {
            open: true,
            ..CoinSpec::default()
        },
    );
    let target = w.vault_coin_spec(
        args,
        &[],
        &CoinSpec {
            open: false,
            ..CoinSpec::default()
        },
    );
    let m = target.mint;
    let opening = w.coin_price(&m);
    assert_eq!(opening, w.opening_price(&m));
    w.env.warp(120);
    // The attack: pump, open, dump. The attacker's open is refused.
    let attacker = w.wallet_with_sol(40 * SOL);
    w.buy(&attacker, &m, 30 * SOL).ok();
    w.open_vault(&attacker, &m)
        .expect_code(u32::from(hook_vault::error::VaultError::NotOpener));
    // The creator's open lands inside the pump (a sandwich of it): the reference is the opening
    // price, not the pumped one.
    let pumped = w.coin_price(&m);
    let creator = target.creator.insecure_clone();
    w.open_vault(&creator, &m).ok();
    let held = w.env.holding(&m, &attacker.pubkey());
    w.sell(&attacker, &m, held).ok();
    let reference = w.vault(&m).slots[0].reference_price;
    println!(
        "pumped to {:.2}x the opening price; the reference opened at {:.2}x",
        pumped as f64 / opening as f64,
        reference as f64 / opening as f64
    );
    assert!(pumped > opening * 2);
    assert_eq!(reference, opening);
    // Both coins trade the same; their slot 0 fills with the 1% tax; both sell at the first crank.
    w.churn(&fair.mint, 6, 2 * SOL);
    w.churn(&m, 6, 2 * SOL);
    assert!(w.slot_coin(&m, 0) > 0 && w.slot_coin(&fair.mint, 0) > 0);
    w.env.warp(61);
    let cranker = w.wallet_with_sol(SOL);
    w.execute(&cranker, &fair.mint, 0).event::<SlotSold>();
    w.execute(&cranker, &m, 0).event::<SlotSold>();
}

/// X3, measured: the launch from a config and `open_vault` do not fit one transaction, even for a
/// one-slot vault and with the protocol's lookup table, so `open_vault` is the creator's next
/// transaction (the fix makes that gap harmless: only the creator opens, and the sells' reference
/// opens at most at the opening price).
#[test]
fn x3_open_vault_does_not_fit_in_the_launch_transaction() {
    use solana_message::{v0, AddressLookupTableAccount, VersionedMessage};
    use solana_transaction::versioned::VersionedTransaction;
    let mut w = World::new();
    let to = w.env.funded(SOL);
    let creator = w.wallet_with_sol(50 * SOL);
    let config = w.tax_hook_config(&creator, 100, LaunchRules::NONE);
    let mint = Keypair::new();
    let spec = CoinSpec::default();
    let setup = w.vault_setup_ixs(
        &creator.pubkey(),
        &mint.pubkey(),
        vault_args(vec![sell_for_sol(to.pubkey())]),
        &[],
        &spec,
    );
    w.env.send_paid_by(&setup, &creator, &[&mint]).ok();
    let launch_ix =
        w.create_launch_from_config_ix(&creator.pubkey(), &mint.pubkey(), "VLT", VQ, &config);
    let tx = w
        .env
        .send_paid_by(std::slice::from_ref(&launch_ix), &creator, &[&mint]);
    tx.ok();
    let launch_alone = tx.size;
    let open_ix = w.open_vault_ix(&creator.pubkey(), &mint.pubkey());
    let table = AddressLookupTableAccount {
        key: Pubkey::new_unique(),
        addresses: protocol_lookup_table(&w),
    };
    let size = |tables: &[AddressLookupTableAccount]| {
        let ixs = [
            bordrless_program_tests::env::compute_unit_limit(1_400_000),
            launch_ix.clone(),
            open_ix.clone(),
        ];
        let msg = v0::Message::try_compile(
            &creator.pubkey(),
            &ixs,
            tables,
            w.env.svm.latest_blockhash(),
        )
        .expect("compile");
        let n = msg.header.num_required_signatures as usize;
        let tx = VersionedTransaction {
            signatures: vec![Default::default(); n],
            message: VersionedMessage::V0(msg),
        };
        bordrless_program_tests::env::wire_size(&tx)
    };
    let (plain, with_table) = (size(&[]), size(&[table]));
    println!("launch alone {launch_alone} B; launch + open_vault {plain} B, {with_table} B with the table");
    assert!(with_table > 1_232 && plain > 1_232);
}

// =========================================================================== X4: game-hook audit

const STUDIO_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[0];

/// Studio's key upgrades `program` to `code` (sigverify off), then the clock moves a slot.
fn studio_upgrade(env: &mut Env, program: Pubkey, code: &[u8]) {
    grow_for(env, &program, code);
    let buffer = Pubkey::new_unique();
    put_buffer(env, buffer, STUDIO_KEY, code, 0);
    let spill = env.payer.pubkey();
    env.svm.expire_blockhash();
    send_as(
        env,
        &[loader::upgrade(
            programdata_address(&program),
            program,
            buffer,
            spill,
            STUDIO_KEY,
        )],
        &[],
        &[STUDIO_KEY],
    )
    .ok();
    env.warp(1);
}

/// X4 (audited label for changed code; Studio's hot key), fixed. A game hook's audit recorded with
/// its code's hash (`set_hook_status_v2`) lifts the cap only while the hook runs that code and is
/// immutable or Bordrless-managed, as a strategy's (R3-F2): every step that applies the hook's
/// terms takes the hook's ProgramData (refused without it, so leaving it out neither keeps the audit
/// nor trims an audited pot), rehashes the code when its deploy slot moved since the game's memo
/// (`Game::hook_audit_slot`), and otherwise takes the memo. Here Studio's key replaces the audited
/// ticket hook's code: the next plan trims the pot to 10 SOL, exactly as the same change to the
/// strategy does (control).
#[test]
fn x4_a_managed_game_hooks_audit_stops_lifting_the_cap_once_its_code_changes() {
    let mut w = World::new();
    w.env
        .svm
        .add_program(STRATEGY, &program_bytes("strategy_tester"))
        .expect("load the tester");
    w.env.set_upgrade_authority(STRATEGY, Some(STUDIO_KEY));
    w.env.set_upgrade_authority(HOOK, Some(STUDIO_KEY));
    let deployer = w.env.deployer.insecure_clone();
    let audited = HookStatusArgs {
        audited: true,
        pot_cap: 0,
        blocked: false,
    };
    for program in [STRATEGY, HOOK] {
        let hash = programdata_hash(&w.env, &program);
        w.env
            .send(
                &[companion::set_hook_status_v2(
                    deployer.pubkey(),
                    program,
                    audited,
                    hash,
                )],
                &[&deployer],
            )
            .ok();
    }
    w.env.without_sigverify();
    let hook_code = vec![AccountMeta::new_readonly(programdata_address(&HOOK), false)];
    let mut s = Strat::with_vetting(w, HOUR, false, hook_code);
    let set: StrategySet = s.created.event();
    assert_eq!((set.audited, set.pot_cap), (true, 0), "an uncapped game");
    assert!(
        s.w.env
            .read::<bordrless_companion::state::Game>(&companion::game_address(&s.mint))
            .hook_audit_ok
    );
    s.grow_pot_above(DEFAULT_POT_CAP + 2 * SOL);
    let pot = s.companion().pending_pot;
    // Without the hook's ProgramData, a plan is refused (it can neither keep nor drop the audit).
    let p = s.round();
    s.warp_into(p + 1, 10);
    let tx = s.send(s.plan_ix(p));
    tx.expect_code(u32::from(
        bordrless_companion::error::CompanionError::ProgramAccounts,
    ));
    // With it, and the code unchanged: the audit holds, nothing is trimmed.
    let tx = s.send(s.plan_ix_with_hook_code(p));
    tx.ok();
    assert_eq!(tx.event::<RolledOver>().reason, RolloverReason::NoTickets);
    assert!(tx.events::<PotToBuyback>().is_empty());
    assert_eq!(s.companion().pending_pot, pot);
    // Studio's key replaces the game hook's code (any code: here another program's).
    let before = programdata_hash(&s.w.env, &HOOK);
    studio_upgrade(&mut s.w.env, HOOK, &program_bytes("half_life"));
    assert_ne!(programdata_hash(&s.w.env, &HOOK), before);
    let now_hash = programdata_hash(&s.w.env, &HOOK);
    let label = risk_label_of(
        &label_accounts(&s.w.env, &HOOK),
        s.w.env.now,
        Some(now_hash),
    );
    assert_eq!((label.class, label.audited), ("managed", "stale"));
    // The next plan sees it: the pot is trimmed to the unaudited cap.
    s.warp_into(p + 2, 10);
    let tx = s.send(s.plan_ix_with_hook_code(p + 1));
    tx.ok();
    assert_eq!(tx.event::<PotToBuyback>().lamports, pot - DEFAULT_POT_CAP);
    assert!(
        !s.w.env
            .read::<bordrless_companion::state::Game>(&companion::game_address(&s.mint))
            .hook_audit_ok
    );
    // Control: the same change to the strategy (R3-F2's fix) is caught the same way.
    studio_upgrade(&mut s.w.env, STRATEGY, &program_bytes("strategy_pro_rata"));
    s.warp_into(p + 3, 10);
    let tx = s.send(s.plan_ix_with_hook_code(p + 2));
    tx.ok();
    assert!(tx.events::<PotToBuyback>().is_empty(), "already at the cap");
}

/// X4, `claim_fees`: a game whose hook carries a hashed audit. The claim needs the hook's
/// ProgramData (the game, passed read-only, lends its memo) and rehashes the code when it moved: an
/// upgrade by the managing key caps the pot at the next claim. (The draw's, the jackpot's and the
/// streak's steps go through the same `apply_status`.)
#[test]
fn x4_claim_fees_follows_the_hooks_code() {
    let mut w = World::new();
    w.env
        .svm
        .add_program(STRATEGY, &program_bytes("strategy_tester"))
        .expect("load the tester");
    w.env.set_upgrade_authority(STRATEGY, Some(STUDIO_KEY));
    w.env.set_upgrade_authority(HOOK, Some(STUDIO_KEY));
    let deployer = w.env.deployer.insecure_clone();
    let hash = programdata_hash(&w.env, &HOOK);
    w.env
        .send(
            &[companion::set_hook_status_v2(
                deployer.pubkey(),
                HOOK,
                HookStatusArgs {
                    audited: true,
                    pot_cap: 0,
                    blocked: false,
                },
                hash,
            )],
            &[&deployer],
        )
        .ok();
    w.env.without_sigverify();
    let hook_code = vec![AccountMeta::new_readonly(programdata_address(&HOOK), false)];
    let mut s = Strat::with_vetting(w, HOUR, false, hook_code);
    s.created.ok();
    // A claim of fees without the hook's ProgramData: refused; with it: the pot grows uncapped.
    let t = s.w.wallet_with_sol(21 * SOL);
    s.w.buy(&t, &s.mint, 20 * SOL).ok();
    let bare = companion::claim_fees_game(s.cranker.pubkey(), s.mint, HOOK);
    s.send(bare.clone()).expect_code(u32::from(
        bordrless_companion::error::CompanionError::ProgramAccounts,
    ));
    let mut with_game = companion::with_hook_code(bare, &HOOK);
    with_game.accounts.push(AccountMeta::new_readonly(
        companion::game_address(&s.mint),
        false,
    ));
    let tx = s.send(with_game);
    tx.ok();
    assert!(tx.events::<PotToBuyback>().is_empty());
    s.grow_pot_above(DEFAULT_POT_CAP + SOL);
    let pot = s.companion().pending_pot;
    assert!(pot > DEFAULT_POT_CAP);
    // Fees wait to be claimed; Studio's key replaces the hook's code: the next claim caps the pot.
    let t = s.w.wallet_with_sol(3 * SOL);
    s.w.buy(&t, &s.mint, 2 * SOL).ok();
    studio_upgrade(&mut s.w.env, HOOK, &program_bytes("half_life"));
    let ix = companion::with_hook_code(
        companion::claim_fees_game(s.cranker.pubkey(), s.mint, HOOK),
        &HOOK,
    );
    let tx = s.send(ix);
    tx.ok();
    assert!(tx.event::<PotToBuyback>().lamports >= pot - DEFAULT_POT_CAP);
    assert!(s.companion().pending_pot <= DEFAULT_POT_CAP);
}

// ============================================================== X5: notice that isn't one

/// X5 (labels and acceptance read code, not behaviour): the residual, with the label fixed. The
/// launchpad takes a timelocked custom hook as "changed only with N days of public notice", and
/// nothing on chain looks at what the code lets its author do without an upgrade: a hook that reads
/// a switch its author can set (here `hook_tester`'s script, whose authority sets answers at any
/// time), or that calls a program its registry names (a proxy to author-upgradeable code), changes
/// behaviour at once. That stays (no program can tell behaviour from code). What changed is the
/// label: a timelocked hook's words now say Bordrless hasn't checked it (unless a current Studio
/// attestation exists), as the immutable words always did; and the SDK's `hookRiskLabel(…, { mint })`
/// flags a registry naming another program (its class becomes the weakest of the programs it can
/// call), as Studio's static checks do. Here the author still turns the coin into a honeypot with
/// no proposal and no hash change, under a label that never promised more than the notice.
#[test]
fn x5_residual_a_timelocked_hook_can_still_switch_behaviour_and_its_label_says_unchecked() {
    use bordrless_hook::token_flags;
    use hook_tester::client as tester;
    let mut w = World::new();
    let creator = w.wallet_with_sol(30 * SOL);
    w.env
        .set_upgrade_authority(hook_tester::ID, Some(creator.pubkey()));
    register(
        &mut w.env,
        &creator,
        hook_tester::ID,
        MIN_ACCEPTED_DELAY,
        creator.pubkey(),
    )
    .ok();
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    w.env
        .send_paid_by(
            &[tester::init_script(creator.pubkey(), mint, vec![])],
            &creator,
            &[],
        )
        .ok();
    let config = Keypair::new();
    let args = CreateConfigArgs {
        rules: LaunchRules::NONE,
        creator_fee_bps: 100,
        custom_hook: Some(hook_tester::ID),
        custom_hook_flags: token_flags::BEFORE_TRANSFER,
        label: "Noticed".to_string(),
    };
    let ix = launch::create_config_timelocked(creator.pubkey(), config.pubkey(), args);
    w.env.send_paid_by(&[ix], &creator, &[&config]).ok();
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint, "NTC", VQ, &config.pubkey());
    w.env.send_paid_by(&[ix], &creator, &[&mint_kp]).ok();
    w.env.warp(31);
    let label = risk_label_of(&label_accounts(&w.env, &hook_tester::ID), w.env.now, None);
    assert_eq!((label.class, label.severity), ("timelocked", "medium"));
    assert!(
        label.words.starts_with(
            "Its author can change this code with 3 days of public notice. Bordrless hasn't \
             checked it."
        ),
        "{}",
        label.words
    );
    let hash = programdata_hash(&w.env, &hook_tester::ID);
    let buyer = w.wallet_with_sol(5 * SOL);
    w.buy(&buyer, &mint, SOL).ok();
    let held = w.env.holding(&mint, &buyer.pubkey());
    assert!(held > 0);
    // The author flips the switch: no proposal, same code, same (unchecked) label.
    let pool = w.launch_pool_key(&mint);
    let ix = tester::honeypot(creator.pubkey(), mint, pool, launch::launch_address(&mint));
    w.env.send_paid_by(&[ix], &creator, &[]).ok();
    w.sell(&buyer, &mint, held)
        .expect_code(u32::from(hook_tester::TesterError::Refused));
    assert_eq!(programdata_hash(&w.env, &hook_tester::ID), hash);
    let lock: hook_timelock::Timelock = w.env.read(&timelock_of(&hook_tester::ID));
    assert!(!lock.has_pending());
    let after = risk_label_of(&label_accounts(&w.env, &hook_tester::ID), w.env.now, None);
    assert_eq!(after.words, label.words);
}
