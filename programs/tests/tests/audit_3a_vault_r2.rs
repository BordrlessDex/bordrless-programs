//! Independent audit of `hook_vault` (phase 3a, round 2): proofs of concept against the round-1
//! fixes (see `bordrless-games-work/log-3a-audit-r2-vault-integration.md`). The round-2 fixes
//! flipped the vault's tests: each now asserts the fixed behaviour, in the proof of concept's
//! scenario.
//!
//! Run: `cargo test -p bordrless-program-tests --test audit_3a_vault_r2 -- --nocapture`.

use anchor_lang::prelude::Pubkey;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::SOL;
use bordrless_program_tests::vault::*;
use hook_vault::constants::*;
use hook_vault::error::VaultError;
use hook_vault::events::*;
use solana_keypair::Keypair;
use solana_signer::Signer;

fn code(e: VaultError) -> u32 {
    u32::from(e)
}

fn wealth(w: &World, who: &Pubkey) -> i128 {
    w.env.holding(&w.sol, who) as i128 + w.env.lamports(who) as i128
}

/// Coin bought by a fresh wallet, half of it sent to slot `i`'s holding.
fn fund_slot(w: &mut World, mint: &Pubkey, i: u8, lamports: u64) -> u64 {
    let h = w.wallet_with_sol(lamports + SOL);
    w.buy(&h, mint, lamports).ok();
    let got = w.env.holding(mint, &h.pubkey()) / 2;
    w.send_tokens(&h, *mint, &slot_owner(mint, i), got).ok();
    got
}

/// Ordinary trading between two cranks: a fresh wallet buys `lamports` of the coin (keeps the price
/// above the slots' references, as a live market would).
fn trade(w: &mut World, mint: &Pubkey, lamports: u64) {
    let t = w.wallet_with_sol(lamports + SOL);
    w.buy(&t, mint, lamports).ok();
}

// =====================================================================================================
// R2-V1 (Medium). The round-1 fix of F1 made the sell slice the vault's, first come first served: the
// first selling slot cranked in a window took all of it, the others got `SliceUsed` (no state change,
// so no run) and were retired 60 days on. Fixed: each of the `n` selling slots sells at most `1/n`
// of the slice a sale (once an interval), so the vault still sells at most one slice an interval,
// no slot's part can be taken by another, and every cranked slot sells. `SliceUsed` is gone.
// =====================================================================================================

/// Two `SellForSol` slots (the creator's wallet and a treasury), both with a backlog of several
/// slices, cranked every `interval` for 60 days in `order`. Answers what each wallet was paid and
/// how many sales each slot made; checks that neither slot can be retired at the end.
fn crank_two_slots(order: [u8; 2], interval: i64) -> ([u64; 2], [u32; 2]) {
    let mut w = World::new();
    let wallets = [w.env.funded(SOL), w.env.funded(SOL)];
    let mut args = vault_args(vec![
        sell_for_sol(wallets[0].pubkey()),
        sell_for_sol(wallets[1].pubkey()),
    ]);
    args.interval = interval;
    let c = w.vault_coin_spec(
        args,
        &[],
        &CoinSpec {
            tax_bps: 0,
            ..CoinSpec::default()
        },
    );
    let m = c.mint;
    fund_slot(&mut w, &m, 0, 8 * SOL);
    fund_slot(&mut w, &m, 1, 4 * SOL);
    let opened = w.vault(&m).opened_at;
    w.env.warp(61);
    let cranker = w.wallet_with_sol(SOL);
    let before = wallets.each_ref().map(|k| w.env.lamports(&k.pubkey()));
    let mut sales = [0u32; 2];
    loop {
        for i in order {
            let ev = w.execute(&cranker, &m, i).event::<SlotSold>();
            assert!(ev.sold > 0);
            sales[usize::from(i)] += 1;
        }
        if w.env.now + interval >= opened + RETIRE_SECS {
            break;
        }
        trade(&mut w, &m, SOL / 2);
        w.env.warp(interval);
    }
    // 60 days after the opening: both slots ran, neither can be retired.
    w.env.warp(opened + RETIRE_SECS - w.env.now);
    let griefer = w.wallet_with_sol(SOL);
    for i in 0..2 {
        w.retire(&griefer, &m, i, true)
            .expect_code(code(VaultError::NotRetirable));
    }
    let paid = [0, 1].map(|i| w.env.lamports(&wallets[i].pubkey()) - before[i]);
    println!(
        "order {order:?}, interval {interval}: sales {sales:?}, paid {paid:?} lamports; neither slot retirable at 60 days"
    );
    (paid, sales)
}

#[test]
fn r2_v1_two_selling_slots_both_sell_and_neither_is_retired() {
    // Every week for 60 days, in the natural order 0 then 1: both slots sell every week.
    let (paid, sales) = crank_two_slots([0, 1], 7 * 86_400);
    assert_eq!(sales[0], sales[1]);
    assert!(sales[1] >= 8);
    assert!(paid[0] > 0 && paid[1] > 0, "the treasury is paid too");
}

/// The same vault cranked in the other order, at the longest interval: the crank order decides
/// nothing any more.
#[test]
fn r2_v1_the_crank_order_no_longer_decides_which_slot_is_paid() {
    for order in [[0u8, 1], [1, 0]] {
        let (paid, sales) = crank_two_slots(order, MAX_INTERVAL);
        assert_eq!(sales, [2, 2]);
        assert!(paid[0] > 0 && paid[1] > 0);
    }
}

// =====================================================================================================
// R2-V2 (Low). The round-1 fix of F3 made a wait use up the slot's whole interval, so a one-block dip
// sandwiched around the due crank pushed the sale a whole interval away (up to 30 days). Fixed: a
// wait holds the slot back a minute (`waited_at` + `MIN_INTERVAL`): still never a wait and a sale in
// one transaction or block (the F3 PoCs fail `NotDue`), but the next minute's crank sells.
// =====================================================================================================

#[test]
fn r2_v2_a_one_block_dip_costs_the_slot_a_minute_not_an_interval() {
    let mut w = World::new();
    let to = w.env.funded(SOL);
    let interval = 7 * 86_400;
    let mut args = vault_args(vec![sell_for_sol(to.pubkey())]);
    args.interval = interval;
    let c = w.vault_coin_spec(
        args,
        &[],
        &CoinSpec {
            tax_bps: 0,
            open: false,
            ..CoinSpec::default()
        },
    );
    let m = c.mint;
    // A holder buys; the vault opens at that price; the holder sends some coin to the slot (a
    // backlog of cuts, without moving the price).
    let attacker: Keypair = w.wallet_with_sol(15 * SOL);
    w.buy(&attacker, &m, 10 * SOL).ok();
    w.open_vault(&c.creator, &m).ok();
    // The reference at that price, as sales there leave it (a vault opens at most at the opening
    // price since X3).
    w.set_sell_reference_to_price(&m, 0);
    let backlog = w.env.holding(&m, &attacker.pubkey()) / 4;
    w.send_tokens(&attacker, m, &slot_owner(&m, 0), backlog)
        .ok();
    w.env.warp(61);
    let cranker = w.wallet_with_sol(SOL);
    w.execute(&cranker, &m, 0).event::<SlotSold>();
    w.env.warp(interval);
    // The slot is due, and an honest crank would sell now: the price is above the floor.
    let s = w.vault(&m).slots[0];
    let floor = s.reference_price * u128::from(BPS - MAX_DISCOUNT_BPS) / u128::from(BPS);
    let price0 = w.coin_price(&m);
    assert!(price0 >= floor, "due and above the floor");
    // The attacker's bundle: dump to 4% below the reference, crank (a wait), buy back.
    let a = attacker.pubkey();
    let p = w.launch_pool(&m);
    let base = (p.base_reserve + p.virtual_base) as f64;
    let target = s.reference_price as f64 * 0.96;
    let dump = (base * ((price0 as f64 / target).sqrt() - 1.0) * 1.02) as u64;
    let (wealth0, coin0) = (wealth(&w, &a), w.env.holding(&m, &a));
    assert!(coin0 >= dump);
    let sol0 = w.env.holding(&w.sol, &a);
    w.sell(&attacker, &m, dump).ok();
    let got = w.env.holding(&w.sol, &a) - sol0;
    assert!(w.coin_price(&m) < floor);
    let waited = w.execute(&attacker, &m, 0).event::<SellWaited>();
    w.env
        .send_paid_by(&[w.launch_swap_ix(&a, &m, 1, got, 0)], &attacker, &[])
        .ok();
    let price1 = w.coin_price(&m);
    let coins = w.env.holding(&m, &a) as i128 - coin0 as i128;
    let cost = wealth0 - wealth(&w, &a) - coins * price1 as i128 / PRICE_SCALE as i128;
    println!(
        "the wait moved the reference {:.4} -> {:.4}; price after the buy-back {:.4} of the old floor; attacker's cost {cost} lamports",
        1.0,
        waited.new_reference as f64 / waited.reference as f64,
        price1 as f64 / floor as f64,
    );
    assert!(price1 >= floor);
    // Still no sale in the same block as the wait, nor within the minute.
    w.execute(&cranker, &m, 0)
        .expect_code(code(VaultError::NotDue));
    w.env.warp(MIN_INTERVAL - 1);
    w.execute(&cranker, &m, 0)
        .expect_code(code(VaultError::NotDue));
    // A minute after the dip the honest crank sells: the dip cost the slot a minute.
    w.env.warp(1);
    let ev = w.execute(&cranker, &m, 0).event::<SlotSold>();
    assert!(ev.sold > 0 && ev.paid > 0);
    println!("sold {} a minute after the dip, paid {}", ev.sold, ev.paid);
}

// =====================================================================================================
// Checked, fine (keeper <-> program): the keeper flags a strategy `over_compute` (and leaves its game
// alone for 6 hours) when a dry run uses more than `PLAN_BASE_UNITS (80k) + planCuMax`, or
// `PAY_BASE_UNITS (150k) + n * (PAY_UNITS_PER_CANDIDATE (30k) + entitleCuMax)`. This measures the
// companion's own compute in those instructions (the transaction's units less the strategy's own,
// from the runtime's `consumed` log lines): it stays under the allowances, so a strategy within its
// declared caps is never flagged. The harness below is `companion_strategy.rs`'s, copied as is.
// =====================================================================================================

#[allow(dead_code, unused_imports, clippy::all)]
mod strategy_cu {
    use anchor_lang::prelude::{AccountMeta, Pubkey};
    use anchor_lang::solana_program::instruction::Instruction;
    use bordrless_companion::client as companion;
    use bordrless_companion::constants::*;
    use bordrless_companion::error::CompanionError;
    use bordrless_companion::events::*;
    use bordrless_companion::instructions::{
        CreateArgs, CreateGameArgs, GameKindArgs, HookStatusArgs, StrategyArgs,
    };
    use bordrless_companion::state::{
        Companion, DrawStatus, Game, GameKind, ShareReceipt, Split, StrategyTerms,
    };
    use bordrless_core::policy;
    use bordrless_game::{round_of, GameHeader, Slots};
    use bordrless_hook::{AccountSource, ExtraAccount, HookAccountList, Seed};
    use bordrless_launch::client as launch;
    use bordrless_launch::instructions::CreateConfigArgs;
    use bordrless_launch::state::LaunchRules;
    use bordrless_program_tests::env::{compute_unit_limit, Tx};
    use bordrless_program_tests::fixture::World;
    use bordrless_program_tests::launch::*;
    use bordrless_program_tests::program_bytes;
    use bordrless_program_tests::timelock::{register, timelock_of};
    use lottery_hook::client as lottery;
    use solana_account::Account;
    use solana_keypair::Keypair;
    use solana_signer::Signer;
    use strategy_tester as st;

    const ROUND: u32 = 3_600;
    const R: i64 = ROUND as i64;
    const CREATOR_FEE: u16 = 200;
    const BOUNTY_BPS: u16 = 50;
    const SPLIT: Split = Split {
        buyback_bps: 3_000,
        holders_bps: 0,
        beneficiary_bps: 0,
    };
    const POT_BPS: u16 = 7_000;
    const MIN_POT: u64 = 100_000_000;
    const WINDOW: u32 = 600;
    const HOOK: Pubkey = lottery_hook::ID;
    const STRATEGY: Pubkey = st::ID;
    const STUDIO_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[0];
    const PACKET: usize = 1_232;

    fn code(e: CompanionError) -> u32 {
        u32::from(e)
    }

    #[track_caller]
    fn refused(tx: &Tx, e: CompanionError) {
        tx.expect_code(code(e));
        let name = format!("{e:?}");
        assert!(
            tx.logs()
                .iter()
                .any(|l| l.contains(&format!("Error Code: {name}"))),
            "expected {name}\n{}",
            tx.logs().join("\n")
        );
    }

    fn create_args() -> CreateArgs {
        CreateArgs {
            split: Split {
                buyback_bps: 10_000,
                holders_bps: 0,
                beneficiary_bps: 0,
            },
            bounty_bps: BOUNTY_BPS,
            max_buyback: SOL,
            buyback_interval: 60,
            vest_secs: 0,
            fund: SOL / 2,
        }
    }

    fn game_args() -> CreateGameArgs {
        CreateGameArgs {
            kind: GameKind::Strategy,
            hook: HOOK,
            split: SPLIT,
            pot_bps: POT_BPS,
            round_secs: ROUND,
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

    /// The tester's config for `mint`: `PDA(["config", mint], tester)`.
    fn config_address(mint: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[b"config", mint.as_ref()], &STRATEGY).0
    }

    /// A second extra of the tester (to measure transactions with 2): `PDA(["aux", mint], tester)`.
    fn aux_address(mint: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[b"aux", mint.as_ref()], &STRATEGY).0
    }

    fn pda_seeds(literal: &[u8]) -> ExtraAccount {
        ExtraAccount {
            writable: false,
            source: AccountSource::Pda {
                program: STRATEGY,
                seeds: vec![Seed::Literal(literal.to_vec()), Seed::Account(0)],
            },
        }
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

    /// The tester's config and registry for `mint` (the config, and the aux account when `two`).
    fn prepare_strategy(w: &mut World, mint: &Pubkey, two: bool) {
        put_owned(
            w,
            config_address(mint),
            STRATEGY,
            st::config_bytes(st::MODE_PRO_RATA, 0, st::MODE_PRO_RATA, 0),
        );
        let mut list = vec![pda_seeds(b"config")];
        if two {
            put_owned(w, aux_address(mint), STRATEGY, vec![1; 8]);
            list.push(pda_seeds(b"aux"));
        }
        put_owned(
            w,
            bordrless_strategy::registry_address(&STRATEGY, mint).0,
            STRATEGY,
            HookAccountList::new(list).encode(),
        );
    }

    /// A world with the tester loaded, upgradeable by `authority` (Studio's key: managed).
    fn world(authority: Option<Pubkey>) -> World {
        let mut w = World::new();
        w.env
            .svm
            .add_program(STRATEGY, &program_bytes("strategy_tester"))
            .expect("load the tester");
        w.env.set_upgrade_authority(STRATEGY, authority);
        w
    }

    fn hook_config(w: &mut World, creator: &Keypair) -> Pubkey {
        let (config, tx) = w.create_config(
            creator,
            CreateConfigArgs {
                rules: LaunchRules::NONE,
                creator_fee_bps: CREATOR_FEE,
                custom_hook: Some(HOOK),
                custom_hook_flags: lottery_hook::FLAGS,
                label: "Strategy".to_string(),
            },
        );
        tx.ok();
        config
    }

    fn launch_ix(w: &World, launcher: &Pubkey, mint: &Pubkey, config: &Pubkey) -> Instruction {
        let c = w.launch_config(config);
        let custom = c.custom_hook.map(|h| w.custom_hook_accounts(&h, mint));
        let mut args = World::launch_args("STRAT", c.creator_fee_bps, VQ, c.rules);
        args.name = "Strategy".to_string();
        let inner = launch::create_launch_with(
            companion::creator_address(mint),
            *mint,
            w.env.treasury.pubkey(),
            w.sol,
            policy::LP_FEE_BPS,
            args.clone(),
            Some(*config),
            custom.as_ref(),
        );
        companion::launch(*launcher, *mint, &inner, args)
    }

    fn extras(mint: &Pubkey, two: bool) -> Vec<Pubkey> {
        let mut e = vec![config_address(mint)];
        if two {
            e.push(aux_address(mint));
        }
        e
    }

    fn setup_ixs(
        launcher: &Pubkey,
        mint: &Pubkey,
        g: CreateGameArgs,
        s: StrategyArgs,
        two: bool,
    ) -> Vec<Instruction> {
        vec![
            companion::create(*launcher, *launcher, *mint, create_args()),
            lottery::prepare(*launcher, *mint, g.round_secs),
            companion::create_strategy_game(
                *launcher,
                *mint,
                g,
                s,
                vec![],
                false,
                &extras(mint, two),
            ),
        ]
    }

    /// The protocol's 22-address lookup table, as the SDK lists it.
    fn table_22(w: &World) -> Vec<Pubkey> {
        let mut addresses = protocol_lookup_table(w);
        addresses.extend([
            companion::event_authority(),
            bordrless_launch::ID,
            bordrless_swap::ID,
            bordrless_token::ID,
        ]);
        assert_eq!(addresses.len(), 22);
        addresses
    }

    /// A strategy coin launched through its companion, past the sniper window.
    struct Strat {
        w: World,
        mint: Pubkey,
        cranker: Keypair,
        two: bool,
    }

    impl Strat {
        fn new() -> Self {
            Self::with(Some(STUDIO_KEY), |_, _| {}, false)
        }

        fn with(
            authority: Option<Pubkey>,
            f: impl FnOnce(&mut CreateGameArgs, &mut StrategyArgs),
            two: bool,
        ) -> Self {
            let mut w = world(authority);
            let launcher = w.wallet_with_sol(50 * SOL);
            let mint_kp = Keypair::new();
            let mint = mint_kp.pubkey();
            prepare_strategy(&mut w, &mint, two);
            let (mut g, mut s) = (game_args(), strategy_args());
            f(&mut g, &mut s);
            let tx = w.env.send_paid_by(
                &setup_ixs(&launcher.pubkey(), &mint, g, s, two),
                &launcher,
                &[&mint_kp],
            );
            tx.ok();
            let set: StrategySet = tx.event();
            assert_eq!(set.strategy, STRATEGY);
            let config = hook_config(&mut w, &launcher);
            let ix = launch_ix(&w, &launcher.pubkey(), &mint, &config);
            w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]).ok();
            w.env.warp(31);
            let cranker = w.wallet_with_sol(5 * SOL);
            Self {
                w,
                mint,
                cranker,
                two,
            }
        }

        fn round(&self) -> u32 {
            round_of(self.w.env.now, ROUND)
        }

        fn warp_into(&mut self, round: u32, secs: i64) {
            let t = i64::from(round) * R + secs;
            assert!(t >= self.w.env.now);
            self.w.env.warp(t - self.w.env.now);
        }

        fn send(&mut self, ix: Instruction) -> Tx {
            let cranker = self.cranker.insecure_clone();
            self.w.env.send_paid_by(&[ix], &cranker, &[])
        }

        fn game(&self) -> Game {
            self.w.env.read(&companion::game_address(&self.mint))
        }

        fn companion(&self) -> Companion {
            self.w.env.read(&companion::companion_address(&self.mint))
        }

        fn terms(&self) -> StrategyTerms {
            self.w
                .env
                .read(&companion::strategy_terms_address(&self.mint))
        }

        fn header(&self) -> GameHeader {
            let data = self
                .w
                .env
                .account(&lottery::state_address(&self.mint))
                .unwrap()
                .data;
            GameHeader::read(&data, &self.mint).unwrap()
        }

        fn balance(&self, owner: &Pubkey) -> u64 {
            self.w.env.holding(&self.mint, owner)
        }

        fn weight(&self, owner: &Pubkey, round: u32) -> u64 {
            Slots::decode(&self.w.env.hook_data(&self.mint, owner))
                .range_in(round)
                .map_or(0, |r| r.weight)
        }

        fn buyer(&mut self, sol: u64) -> Keypair {
            let t = self.w.wallet_with_sol(sol + SOL);
            self.w.buy(&t, &self.mint, sol).ok();
            t
        }

        fn volume(&mut self, wallets: usize, sol: u64) {
            for _ in 0..wallets {
                let t = self.w.wallet_with_sol(sol + SOL);
                self.w.buy(&t, &self.mint, sol).ok();
                let held = self.balance(&t.pubkey());
                self.w.sell(&t, &self.mint, held).ok();
            }
        }

        fn enter(&mut self, holders: &[&Keypair]) {
            for h in holders {
                let ix = lottery::enter(self.mint, h.pubkey());
                self.send(ix).ok();
            }
        }

        /// The next round, 5 seconds in, with `holders` entered.
        fn start_round(&mut self, holders: &[&Keypair]) -> u32 {
            let next = self.round() + 1;
            self.warp_into(next, 5);
            self.enter(holders);
            next
        }

        fn claim_fees(&mut self) -> Tx {
            let ix = companion::claim_fees_game(self.cranker.pubkey(), self.mint, HOOK);
            self.send(ix)
        }

        fn fund_pot(&mut self) {
            self.volume(1, 20 * SOL);
            self.claim_fees().ok();
            assert!(self.companion().pending_pot >= MIN_POT);
        }

        fn set_config(&mut self, plan: (u8, u64), entitle: (u8, u64)) {
            let mint = self.mint;
            put_owned(
                &mut self.w,
                config_address(&mint),
                STRATEGY,
                st::config_bytes(plan.0, plan.1, entitle.0, entitle.1),
            );
        }

        fn plan_ix(&self, period: u32) -> Instruction {
            companion::plan_period(
                self.cranker.pubkey(),
                self.mint,
                HOOK,
                STRATEGY,
                self.w.launch(&self.mint).pool,
                &extras(&self.mint, self.two),
                period,
            )
        }

        fn plan(&mut self, period: u32) -> Tx {
            let ix = self.plan_ix(period);
            self.send(ix)
        }

        fn pay_ix(&self, period: u32, owners: &[Pubkey]) -> Instruction {
            companion::pay_strategy(
                self.cranker.pubkey(),
                self.mint,
                HOOK,
                STRATEGY,
                &extras(&self.mint, self.two),
                period,
                owners,
            )
        }

        fn pay(&mut self, period: u32, owners: &[Pubkey]) -> Tx {
            let ix = self.pay_ix(period, owners);
            let cranker = self.cranker.insecure_clone();
            self.w
                .env
                .send_v0(&[compute_unit_limit(1_400_000), ix], &cranker, &[], &[])
        }

        fn set_status(
            &mut self,
            program: Pubkey,
            audited: bool,
            pot_cap: u64,
            blocked: bool,
        ) -> Tx {
            let deployer = self.w.env.deployer.insecure_clone();
            let ix = companion::set_hook_status(
                deployer.pubkey(),
                program,
                HookStatusArgs {
                    audited,
                    pot_cap,
                    blocked,
                },
            );
            self.w.env.send_paid_by(&[ix], &deployer, &[])
        }

        /// Holders A (5 SOL of tokens), B (1 SOL) and C (2 SOL) entered in a fresh period, a funded pot,
        /// and that period over: answers the holders and the period.
        fn period_with_holders(&mut self) -> (Keypair, Keypair, Keypair, u32) {
            let a = self.buyer(5 * SOL);
            let b = self.buyer(SOL);
            let c = self.buyer(2 * SOL);
            let p = self.start_round(&[&a, &b, &c]);
            self.fund_pot();
            self.warp_into(p + 1, 10);
            (a, b, c, p)
        }
    }

    /// The heap a transaction's companion instruction peaked at, when the companion was built with the
    /// instrumented allocator (`custom-heap` and the heap probe, which logs `0x4ea9` and the bytes used at
    /// each new 256-byte high-water mark); `None` with the deployed build.
    fn heap_peak(tx: &Tx) -> Option<u64> {
        tx.logs()
            .iter()
            .filter_map(|l| l.strip_prefix("Program log: 0x4ea9, 0x"))
            .filter_map(|rest| u64::from_str_radix(rest.split(',').next()?, 16).ok())
            .max()
    }

    fn reasons(tx: &Tx) -> Vec<(Pubkey, CandidateReason)> {
        tx.events::<CandidateRejected>()
            .into_iter()
            .map(|e| (e.owner, e.reason))
            .collect()
    }

    // =============================================================================== creation

    /// The compute the strategy itself used in `tx` (its `consumed` log lines), and how many calls.
    fn strategy_units(tx: &Tx) -> (u64, usize) {
        let prefix = format!("Program {} consumed ", STRATEGY);
        let used: Vec<u64> = tx
            .logs()
            .iter()
            .filter_map(|l| l.strip_prefix(prefix.as_str()))
            .filter_map(|rest| rest.split(' ').next()?.parse().ok())
            .collect();
        (used.iter().sum(), used.len())
    }

    /// The keeper's allowances (`apps/server/src/keeper/games.ts`).
    const PLAN_BASE_UNITS: u64 = 80_000;
    const PAY_BASE_UNITS: u64 = 150_000;
    const PAY_UNITS_PER_CANDIDATE: u64 = 30_000;

    #[test]
    fn ok_the_keepers_compute_allowances_cover_the_companions_own() {
        let mut worst_plan = 0u64;
        let mut worst_pay = i64::MIN;
        for (plan_burn, entitle_burn) in [
            (0u64, 0u64),
            (50_000, 20_000),
            (MAX_PLAN_CU as u64 - 20_000, MAX_ENTITLE_CU as u64 - 15_000),
        ] {
            let mut s = Strat::with(Some(STUDIO_KEY), |_, _| {}, true);
            let a = s.buyer(5 * SOL);
            let b = s.buyer(SOL);
            let c = s.buyer(2 * SOL);
            let d = s.buyer(3 * SOL);
            let p = s.start_round(&[&a, &b, &c, &d]);
            s.fund_pot();
            s.warp_into(p + 1, 10);
            s.set_config((st::MODE_BURN, plan_burn), (st::MODE_BURN, entitle_burn));
            let cranker = s.cranker.insecure_clone();
            let plan = s.plan_ix(p);
            let tx =
                s.w.env
                    .send_v0(&[compute_unit_limit(1_400_000), plan], &cranker, &[], &[]);
            tx.ok();
            assert!(tx.events::<PeriodPlanned>().len() == 1);
            let (strategy, calls) = strategy_units(&tx);
            assert_eq!(calls, 1);
            let own = tx.cu() - strategy;
            println!(
                "plan_period: {} CU in all, the strategy {strategy}, the companion's own {own} (the keeper allows {PLAN_BASE_UNITS} + planCuMax)",
                tx.cu()
            );
            worst_plan = worst_plan.max(own);
            // A strategy that declares exactly what it uses (planCuMax = its use) is over the
            // keeper's cap: flagged over_compute, its game left alone for 6 hours, its plan unsent.
            let owners = [a.pubkey(), b.pubkey(), c.pubkey(), d.pubkey()];
            let pay = s.pay_ix(p, &owners);
            let tx =
                s.w.env
                    .send_v0(&[compute_unit_limit(1_400_000), pay], &cranker, &[], &[]);
            tx.ok();
            assert_eq!(tx.event::<StrategyPaid>().payments.len(), 4);
            let (strategy, calls) = strategy_units(&tx);
            assert_eq!(calls, 4);
            let own = tx.cu() - strategy;
            let allowance = PAY_BASE_UNITS + 4 * PAY_UNITS_PER_CANDIDATE;
            println!(
                "pay_strategy x4: {} CU in all, the strategy {strategy} ({} a call), the companion's own {own} (the keeper allows {allowance} + 4 x entitleCuMax): margin {}",
                tx.cu(),
                strategy / 4,
                allowance as i64 - own as i64
            );
            worst_pay = worst_pay.max(own as i64 - allowance as i64);
        }
        println!("worst: plan overhead {worst_plan} vs {PLAN_BASE_UNITS}; pay overhead over the allowance by {worst_pay}");
        assert!(worst_plan < PLAN_BASE_UNITS);
        assert!(worst_pay < 0);
    }
}
