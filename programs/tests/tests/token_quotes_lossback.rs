//! Lossback on a coin quoted in tokens (`docs/token-quotes.md` §7.6): `loss_cover_hook` prepared
//! with the legs' pools (`prepare_legs`), each holding's basis kept in the token of the leg it first
//! bought on (`Record.leg`); `loss_cover_script` in token mode, its rounds per leg, its answers
//! version 3; the companion keeping the script's pot and the launcher's part per leg
//! (`CompanionLegs`) and paying each leg's payouts in its token within the terms per leg.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::AnchorSerialize;
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{CreateArgs, CreateScriptArgs, PayoutTermsArgs};
use bordrless_companion::state::{CompanionLegs, Split};
use bordrless_core::policy;
use bordrless_launch::client::{self as launch, CustomHookAccounts, LegKeys};
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::quotes::{pack_mint, Quote, QUOTE_FUNDS, TOKEN, TOKEN_2022};
use bordrless_program_tests::timelock::programdata_hash;
use bordrless_strategy::pool_reserves;
use bordrless_token::client as token;
use loss_cover_hook::{client as hook, spot_of, trade_price, Record};
use loss_cover_script::{CoverTerms, LegRound, STATE_LEN_LEGS};
use solana_keypair::Keypair;
use solana_signer::Signer;

const SOL: u64 = 1_000_000_000;
const SUPPLY: u64 = policy::TOKEN_SUPPLY;
const SCRIPT: Pubkey = loss_cover_script::ID;
const HOOK: Pubkey = loss_cover_hook::ID;
const FEE: u16 = 200;
const BORDRLESS_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[1];
const ROUND: u32 = 300;
const R: i64 = ROUND as i64;
const STEP: i64 = 15;

fn borsh<T: AnchorSerialize>(t: &T) -> Vec<u8> {
    let mut out = Vec::new();
    t.serialize(&mut out).unwrap();
    out
}

fn world() -> World {
    let mut w = World::new();
    w.env.set_upgrade_authority(SCRIPT, Some(BORDRLESS_KEY));
    let deployer = w.env.deployer.insecure_clone();
    let hash = programdata_hash(&w.env, &SCRIPT);
    w.env
        .send(
            &[companion::approve(
                deployer.pubkey(),
                SCRIPT,
                APPROVAL_KIND_SCRIPT,
                hash,
                Pubkey::default(),
            )],
            &[&deployer],
        )
        .ok();
    w
}

/// Everything covered, no wait, five-minute rounds; the least loss 50 basis points of a holding's
/// cost (the site's).
fn terms() -> CoverTerms {
    CoverTerms {
        cover_bps: 10_000,
        min_holding: 1,
        min_loss: 50,
        min_age_secs: 0,
        round_secs: ROUND,
        min_pot_lamports: 0,
    }
}

struct Coin {
    w: World,
    mint: Pubkey,
    quotes: Vec<(Pubkey, Pubkey)>,
    pools: Vec<Pubkey>,
    custom: CustomHookAccounts,
    cranker: Keypair,
}

impl Coin {
    fn new(specs: &[(Pubkey, u8)]) -> Self {
        let mut w = world();
        let launcher = w.env.funded(100 * SOL);
        let issuer = w.env.deployer.pubkey();
        let n = specs.len() as u64;
        let mut quotes = vec![];
        let mut launch_quotes: Vec<Quote> = vec![];
        for (i, (program, decimals)) in specs.iter().enumerate() {
            let key = Pubkey::new_unique();
            w.env.put_mint(
                key,
                *program,
                pack_mint(Some(issuer), None, *decimals, u64::MAX / 2, &[]),
            );
            let share = if i as u64 == n - 1 {
                SUPPLY - SUPPLY / n * (n - 1)
            } else {
                SUPPLY / n
            };
            let cap = if *decimals == 9 {
                25_000_000_000u64
            } else {
                5_000_000_000
            };
            let vq = (u128::from(cap) * u128::from(share) / u128::from(SUPPLY)) as u64;
            quotes.push((key, *program));
            launch_quotes.push((key, vq));
        }
        let mint_kp = Keypair::new();
        let m = mint_kp.pubkey();
        let pools: Vec<Pubkey> = quotes
            .iter()
            .map(|q| launch::pool_address(&m, &q.0, policy::LP_FEE_BPS))
            .collect();
        let (config, tx) = w.create_config(
            &launcher,
            CreateConfigArgs {
                rules: LaunchRules::NONE,
                creator_fee_bps: FEE,
                custom_hook: Some(HOOK),
                custom_hook_flags: loss_cover_hook::FLAGS,
                label: "Lossback".to_string(),
            },
        );
        tx.ok();
        let ixs = [
            companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                m,
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
                    fund: SOL / 2,
                },
            ),
            hook::prepare_legs(launcher.pubkey(), m, &pools),
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
                    program: SCRIPT,
                    interval: STEP,
                    state_len: STATE_LEN_LEGS,
                    init_data: borsh(&terms()),
                },
                PayoutTermsArgs {
                    step_bps: 10_000,
                    holding_bps: 10_000,
                    window_bps: 10_000,
                    window_secs: ROUND,
                },
                vec![],
            ),
        ];
        w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]).ok();
        let custom = CustomHookAccounts {
            program: HOOK,
            extras: hook::extras_legs(&m, &pools),
        };
        let mints: Vec<Pubkey> = quotes.iter().map(|q| q.0).collect();
        let largs = World::token_launch_args_with("LOSS", FEE, &launch_quotes, LaunchRules::NONE);
        let inner = launch::create_token_launch_with(
            companion::creator_address(&m),
            m,
            w.env.treasury.pubkey(),
            largs.clone(),
            &mints,
            Some(config),
            Some(&custom),
        );
        let ix = companion::launch_token(launcher.pubkey(), m, &inner, largs);
        let tx = w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]);
        tx.ok();
        let payer = w.env.payer.insecure_clone();
        for i in 0..quotes.len() {
            let ix = launch::open_leg_custom(
                payer.pubkey(),
                w.env.treasury.pubkey(),
                &w.leg_keys(&m, i),
                i as u8,
                &custom,
            );
            w.env.send(&[ix], &[]).ok();
        }
        w.env.warp(61);
        let cranker = w.env.funded(10 * SOL);
        Self {
            w,
            mint: m,
            quotes,
            pools,
            custom,
            cranker,
        }
    }

    fn keys(&self, i: usize) -> LegKeys {
        self.w.leg_keys(&self.mint, i)
    }

    fn legs(&self) -> CompanionLegs {
        self.w
            .env
            .read(&companion::companion_legs_address(&self.mint))
    }

    fn trader(&mut self) -> Keypair {
        let kp = self.w.env.funded(10 * SOL);
        for (q, p) in &self.quotes {
            self.w.env.token_account(&kp.pubkey(), q, p, QUOTE_FUNDS);
        }
        kp
    }

    fn swap(&self, who: &Pubkey, i: usize, direction: u8, amount: u64) -> Instruction {
        let keys = self.keys(i);
        launch::leg_swap_with_slice(
            &keys,
            *who,
            token::holding_address(&self.mint, who),
            keys.quote_account(who),
            direction,
            amount,
            0,
            self.custom.slice(),
            false,
        )
    }

    fn buy(&mut self, t: &Keypair, i: usize, amount: u64) -> Tx {
        let who = t.pubkey();
        let ixs = [
            token::create_holding(who, self.mint, who),
            self.swap(&who, i, 1, amount),
        ];
        self.w.env.send_paid_by(&ixs, t, &[])
    }

    fn sell(&mut self, t: &Keypair, i: usize, coins: u64) -> Tx {
        let ix = self.swap(&t.pubkey(), i, 0, coins);
        self.w.env.send_paid_by(&[ix], t, &[])
    }

    fn record(&self, owner: &Pubkey) -> Record {
        Record::decode(&self.w.env.hook_data(&self.mint, owner))
    }

    fn reserves(&self, i: usize) -> bordrless_strategy::PoolReserves {
        pool_reserves(&self.w.env.account(&self.pools[i]).unwrap().data).unwrap()
    }

    fn spot(&self, i: usize) -> u64 {
        spot_of(&self.reserves(i)).unwrap()
    }

    fn claim_leg(&mut self, i: usize) {
        let ix = companion::claim_fees_leg(self.cranker.pubkey(), &self.keys(i), i as u8, false);
        let cranker = self.cranker.insecure_clone();
        self.w.env.send_paid_by(&[ix], &cranker, &[]).ok();
    }

    /// The step's accounts after the companion's six: the payout accounts, the pots on the legs and
    /// the coin's `Legs`, every leg's pool, quote mint and the creator address's account of it, the
    /// token programs, then each candidate's holding and, when `paid` on leg `i`, its owner's
    /// account of leg `i`'s token.
    fn step(&mut self, list: &[(Pubkey, Option<usize>)]) -> Tx {
        let ix = self.step_ix(list);
        let cranker = self.cranker.insecure_clone();
        self.w.env.send_paid_by(&[ix], &cranker, &[])
    }

    fn step_ix(&self, list: &[(Pubkey, Option<usize>)]) -> Instruction {
        let creator = companion::creator_address(&self.mint);
        let mut extra = companion::payout_accounts(&self.mint, &HOOK);
        extra.extend(companion::token_pot_accounts(&self.mint));
        for (i, (q, p)) in self.quotes.iter().enumerate() {
            extra.push(AccountMeta::new_readonly(self.pools[i], false));
            extra.push(AccountMeta::new_readonly(*q, false));
            extra.push(AccountMeta::new(
                bordrless_swap::spl::associated_token_address(&creator, q, p),
                false,
            ));
        }
        extra.push(AccountMeta::new_readonly(TOKEN, false));
        extra.push(AccountMeta::new_readonly(TOKEN_2022, false));
        for (owner, paid) in list {
            extra.push(AccountMeta::new(
                token::holding_address(&self.mint, owner),
                false,
            ));
            if let Some(i) = paid {
                let (q, p) = self.quotes[*i];
                extra.push(AccountMeta::new(
                    bordrless_swap::spl::associated_token_address(owner, &q, &p),
                    false,
                ));
            }
        }
        let keys = launch::LaunchKeys::of(&self.w.launch(&self.mint));
        companion::script_step(self.cranker.pubkey(), &keys, SCRIPT, extra)
    }

    /// The v0 size of a step of `list` through a lookup table of the coin's accounts (all but the
    /// cranker and the candidates' accounts, the keeper's per-coin table).
    fn step_size_through_table(&mut self, list: &[(Pubkey, Option<usize>)]) -> usize {
        let ix = self.step_ix(list);
        let bare = self.step_ix(&[]);
        let cranker = self.cranker.pubkey();
        let mut keys: Vec<Pubkey> = bare
            .accounts
            .iter()
            .map(|m| m.pubkey)
            .filter(|k| *k != cranker)
            .collect();
        keys.sort();
        keys.dedup();
        let table = self.w.env.put_lookup_table(Pubkey::new_unique(), &keys);
        let payer = self.cranker.insecure_clone();
        self.w.env.v0_size(&[ix], &payer, &[], &[table])
    }

    fn next_round(&mut self) {
        let now = self.w.env.now;
        self.w.env.warp(R - now % R);
    }

    fn quote_balance(&self, owner: &Pubkey, i: usize) -> u64 {
        let (q, p) = self.quotes[i];
        let key = bordrless_swap::spl::associated_token_address(owner, &q, &p);
        self.w
            .env
            .account(&key)
            .map_or(0, |_| self.w.env.token_amount(&key))
    }
}

const THREE: [(Pubkey, u8); 3] = [(TOKEN, 6), (TOKEN_2022, 6), (TOKEN, 9)];
const SIX: [(Pubkey, u8); 6] = [
    (TOKEN, 6),
    (TOKEN_2022, 6),
    (TOKEN, 9),
    (TOKEN, 6),
    (TOKEN_2022, 9),
    (TOKEN, 6),
];

/// A holding's basis is kept in the token of the leg it first bought on: a buy on that leg blends at
/// the trade's price, a buy on another leg and a transfer in at the record leg's spot; a holding
/// with no record that receives a transfer takes leg 0.
#[test]
fn a_record_keeps_its_basis_on_the_leg_of_its_first_buy() {
    let mut c = Coin::new(&THREE);
    let a = c.trader();
    let vq1 = c.w.legs(&c.mint).legs[1].virtual_quote;
    let tx = c.buy(&a, 1, vq1 / 50);
    tx.ok();
    let got = c.w.env.holding(&c.mint, &a.pubkey());
    let r = c.record(&a.pubkey());
    assert_eq!((r.version, r.leg, r.amount), (1, 1, got));
    // The trade's price on leg 1: its reserves after the trade, the coins delivered.
    assert_eq!(r.entry, trade_price(&c.reserves(1), got).unwrap());
    // A buy on leg 2 blends at leg 1's spot, not at leg 2's price.
    let spot1 = c.spot(1);
    let vq2 = c.w.legs(&c.mint).legs[2].virtual_quote;
    c.buy(&a, 2, vq2 / 100).ok();
    let more = c.w.env.holding(&c.mint, &a.pubkey()) - got;
    let r2 = c.record(&a.pubkey());
    let blended = ((u128::from(r.entry) * u128::from(got) + u128::from(spot1) * u128::from(more))
        / u128::from(got + more)) as u64;
    assert_eq!((r2.leg, r2.entry), (1, blended));
    // A transfer to a wallet with no record: leg 0's spot.
    let b = Keypair::new();
    c.w.env.fund(b.pubkey(), SOL);
    let half = c.w.env.holding(&c.mint, &a.pubkey()) / 2;
    let spot0 = c.spot(0);
    let ixs = [
        token::create_holding(a.pubkey(), c.mint, b.pubkey()),
        token::transfer_with(
            a.pubkey(),
            token::holding_address(&c.mint, &a.pubkey()),
            token::holding_address(&c.mint, &b.pubkey()),
            c.mint,
            Some(token::Hook::of(HOOK)),
            c.custom.extras.clone(),
            half,
        ),
    ];
    c.w.env.send_paid_by(&ixs, &a, &[]).ok();
    let rb = c.record(&b.pubkey());
    assert_eq!((rb.leg, rb.entry, rb.amount), (0, spot0, half));
    // A first buy on leg 0 is on leg 0.
    let d = c.trader();
    let vq0 = c.w.legs(&c.mint).legs[0].virtual_quote;
    c.buy(&d, 0, vq0 / 100).ok();
    assert_eq!(c.record(&d.pubkey()).leg, 0);
}

/// Each leg's pot pays only holders whose basis is on that leg, in its token: the first round
/// measures, the next pays every holder under water the leg's fraction of its loss, each into its
/// account of its leg's token, recorded by the hook; the launcher's part stays.
#[test]
fn each_legs_pot_pays_its_holders_in_its_token() {
    let mut c = Coin::new(&THREE);
    let m = c.mint;
    // Holders who bought high on legs 0 and 2: a whale on each leg pumps, the holder buys, the
    // whale sells half.
    let whale = c.trader();
    let holders = [c.trader(), c.trader()];
    for (k, leg) in [0usize, 2].iter().enumerate() {
        let vq = c.w.legs(&m).legs[*leg].virtual_quote;
        c.buy(&whale, *leg, vq / 4).ok();
        let h = holders[k].insecure_clone();
        c.buy(&h, *leg, vq / 50).ok();
        let coins = c.w.env.holding(&m, &whale.pubkey());
        c.sell(&whale, *leg, coins / 2).ok();
    }
    // Leg 1 saw no trade: nothing to claim there.
    for i in [0, 2] {
        c.claim_leg(i);
    }
    let pots: Vec<u64> = c.legs().legs.iter().map(|p| p.pending_pot).collect();
    assert!(pots[0] > 0 && pots[2] > 0);
    let launcher_parts: Vec<u64> = c
        .legs()
        .legs
        .iter()
        .map(|p| p.pending_beneficiary)
        .collect();
    assert!(launcher_parts[0] > 0, "the launcher's part, per leg");
    let list: Vec<(Pubkey, Option<usize>)> = holders
        .iter()
        .zip([0usize, 2])
        .map(|(h, leg)| (h.pubkey(), Some(leg)))
        .collect();
    // The first round measures: touches only.
    c.next_round();
    let tx = c.step(&list);
    tx.ok();
    let paid: Vec<LegScriptPayout> = tx.events();
    assert_eq!(paid.len(), 2);
    assert!(paid.iter().all(|p| p.amount == 0));
    // The next round pays each from its leg's pot.
    c.next_round();
    let before: Vec<u64> = holders
        .iter()
        .zip([0usize, 2])
        .map(|(h, leg)| c.quote_balance(&h.pubkey(), leg))
        .collect();
    let through_table = c.step_size_through_table(&list);
    let tx = c.step(&list);
    tx.ok();
    println!(
        "lossback step in tokens (2 payouts, 3 legs): CU {} size {} through a table {}",
        tx.cu(),
        tx.size,
        through_table
    );
    let paid: Vec<LegScriptPayout> = tx.events();
    assert_eq!(paid.len(), 2);
    let legs = c.legs();
    for (k, (h, leg)) in holders.iter().zip([0usize, 2]).enumerate() {
        let e = paid.iter().find(|p| p.owner == h.pubkey()).unwrap();
        assert_eq!(usize::from(e.index), leg);
        assert!(e.amount > 0, "under water: paid");
        assert_eq!(c.quote_balance(&h.pubkey(), leg) - before[k], e.amount);
        assert_eq!(
            c.record(&h.pubkey()).paid,
            e.amount,
            "recorded in the leg's token"
        );
        assert!(legs.legs[leg].pending_pot <= pots[leg] - e.amount);
    }
    // Leg 1's pot paid nobody: none of its holders is under water.
    assert_eq!(legs.legs[1].pending_pot, pots[1]);
    assert_eq!(
        legs.legs[0].pending_beneficiary, launcher_parts[0],
        "the launcher's part stays"
    );
    // The legs' rounds in the script's state.
    let state =
        c.w.env
            .account(&companion::script_state_address(&m))
            .unwrap()
            .data;
    let r0 = LegRound::read(&state, 0);
    assert!(r0.round_paid > 0 && r0.b == pots[0]);
}

/// A six-leg Lossback coin's trades, measured: a buy on the last leg (its holding made first) and a
/// sale, each carrying the hook's nine extras (the state, the launch, the six pools and the coin's
/// `Legs`); their size through a lookup table of the coin's accounts (the trader's aside), as the
/// site's per-coin table would carry them.
#[test]
fn measures_a_six_leg_coins_trades() {
    let mut c = Coin::new(&SIX);
    let m = c.mint;
    let t = c.trader();
    let who = t.pubkey();
    let theirs = [
        who,
        token::holding_address(&m, &who),
        c.keys(5).quote_account(&who),
    ];
    let through_table = |c: &mut Coin, ixs: &[Instruction]| -> usize {
        let mut keys: Vec<Pubkey> = ixs
            .iter()
            .flat_map(|ix| ix.accounts.iter().map(|a| a.pubkey).chain([ix.program_id]))
            .filter(|k| !theirs.contains(k))
            .collect();
        keys.sort();
        keys.dedup();
        let table = c.w.env.put_lookup_table(Pubkey::new_unique(), &keys);
        c.w.env.v0_size(ixs, &t, &[], &[table])
    };
    let vq = c.w.legs(&m).legs[5].virtual_quote;
    let buy = [
        token::create_holding(who, m, who),
        c.swap(&who, 5, 1, vq / 50),
    ];
    let size = through_table(&mut c, &buy);
    let tx = c.w.env.send_paid_by(&buy, &t, &[]);
    tx.ok();
    println!(
        "six-leg Lossback buy on leg 5 (with create_holding): CU {} size {} accounts {} through a table {}",
        tx.cu(),
        tx.size,
        tx.keys.len(),
        size
    );
    assert_eq!(c.record(&who).leg, 5, "the basis on the leg it bought on");
    let coins = c.w.env.holding(&m, &who);
    let sell = [c.swap(&who, 5, 0, coins / 2)];
    let size = through_table(&mut c, &sell);
    let tx = c.w.env.send_paid_by(&sell, &t, &[]);
    tx.ok();
    println!(
        "six-leg Lossback sell on leg 5: CU {} size {} accounts {} through a table {}",
        tx.cu(),
        tx.size,
        tx.keys.len(),
        size
    );
}

/// A coin quoted in tokens' transfer must carry the hook's `Legs` and the record leg's pool (the
/// token program hands the hook whatever the sender passes): without the `Legs` an arrival would be
/// priced at leg 0's spot, without the record leg's pool it would keep the old entry. Both refused
/// (`WrongAccount`); with the registry's extras whole it lands.
#[test]
fn a_transfer_without_the_legs_or_the_record_legs_pool_is_refused() {
    let mut c = Coin::new(&THREE);
    let m = c.mint;
    let b = c.trader();
    let vq1 = c.w.legs(&m).legs[1].virtual_quote;
    c.buy(&b, 1, vq1 / 50).ok();
    assert_eq!(c.record(&b.pubkey()).leg, 1);
    let a = c.trader();
    let vq0 = c.w.legs(&m).legs[0].virtual_quote;
    c.buy(&a, 0, vq0 / 50).ok();
    let moved = c.w.env.holding(&m, &a.pubkey()) / 4;
    let send = |c: &mut Coin, extras: Vec<AccountMeta>| -> Tx {
        let ixs = [
            token::create_holding(a.pubkey(), m, b.pubkey()),
            token::transfer_with(
                a.pubkey(),
                token::holding_address(&m, &a.pubkey()),
                token::holding_address(&m, &b.pubkey()),
                m,
                Some(token::Hook::of(HOOK)),
                extras,
                moved,
            ),
        ];
        c.w.env.send_paid_by(&ixs, &a, &[])
    };
    let wrong_account = 6000 + loss_cover_hook::LossCoverError::WrongAccount as u32;
    // The registry's first three extras (state, launch, leg 0's pool): no `Legs`.
    let short = c.custom.extras[..3].to_vec();
    send(&mut c, short).expect_code(wrong_account);
    // The `Legs` but not leg 1's pool.
    let pool1 = c.pools[1];
    let without: Vec<AccountMeta> = c
        .custom
        .extras
        .iter()
        .filter(|e| e.pubkey != pool1)
        .cloned()
        .collect();
    send(&mut c, without).expect_code(wrong_account);
    let whole = c.custom.extras.clone();
    send(&mut c, whole).ok();
}
