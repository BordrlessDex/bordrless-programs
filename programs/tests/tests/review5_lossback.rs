//! Lossback on a coin quoted in tokens (`docs/token-quotes.md` §7.6): `loss_cover_hook` prepared
//! with the legs' pools (`prepare_legs`), each holding's basis kept in the token of the leg it first
//! bought on (`Record.leg`); `loss_cover_script` in token mode, its rounds per leg, its answers
//! version 3; the companion keeping the script's pot and the launcher's part per leg
//! (`CompanionLegs`) and paying each leg's payouts in its token within the terms per leg.
//!
//! Review 5's regressions (the adversarial review of §7.6), as the reviewer wrote them: they keep
//! their suite's helpers whole, some unused here.

#![allow(dead_code, unused_imports, unused_variables)]

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

// ---- review5 (adversarial review of §7.6): fake losses through the hook's caller-chosen extras ----

use loss_cover_script::uncovered_loss;

/// Leg 0 in a 9-decimal token, leg 1 in a 6-decimal one: leg 0's raw units are worth less, so its
/// spot (raw units of its token per base unit) reads about five times leg 1's.
const LOPSIDED: [(Pubkey, u8); 2] = [(TOKEN, 9), (TOKEN, 6)];

/// The record's blend of `held` at `entry` with `amount` at `price` (`on_receive`).
fn blend(entry: u64, held: u64, price: u64, amount: u64) -> u64 {
    ((u128::from(entry) * u128::from(held) + u128::from(price) * u128::from(amount))
        / u128::from(held + amount)) as u64
}

impl Coin {
    /// Round trips on leg `i` (fees for its pot), its fee claimed and split.
    fn fees_on(&mut self, i: usize, trips: usize) {
        let t = self.trader();
        let vq = self.w.legs(&self.mint).legs[i].virtual_quote;
        for _ in 0..trips {
            self.buy(&t, i, vq / 8).ok();
            let coins = self.w.env.holding(&self.mint, &t.pubkey());
            self.sell(&t, i, coins).ok();
        }
        self.claim_leg(i);
    }

    /// `from` sends `amount` coins to `to` with exactly `extras` as the hook's extras.
    fn send_with(
        &mut self,
        from: &Keypair,
        to: &Pubkey,
        amount: u64,
        extras: Vec<AccountMeta>,
    ) -> Tx {
        let ixs = [
            token::create_holding(from.pubkey(), self.mint, *to),
            token::transfer_with(
                from.pubkey(),
                token::holding_address(&self.mint, &from.pubkey()),
                token::holding_address(&self.mint, to),
                self.mint,
                Some(token::Hook::of(HOOK)),
                extras,
                amount,
            ),
        ];
        self.w.env.send_paid_by(&ixs, from, &[])
    }
}

/// F1 (a): the hook's extras are whatever the sender passes (the token program forwards its
/// remaining accounts unchecked). Leaving out the `Legs` sends a coin quoted in tokens down the SOL
/// path: an arrival on a record of leg 1 is blended at leg 0's spot, in leg 0's units. Here that is
/// five times leg 1's price, so the receiver is "under water" without any price having fallen, and
/// leg 1's pot pays it.
#[test]
fn review5_without_the_legs_a_leg_records_arrival_blends_at_leg_0s_spot() {
    let mut c = Coin::new(&LOPSIDED);
    let m = c.mint;
    assert!(
        c.spot(0) > 3 * c.spot(1),
        "leg 0's spot reads higher in raw units"
    );
    // b's record on leg 1.
    let b = c.trader();
    let vq1 = c.w.legs(&m).legs[1].virtual_quote;
    c.buy(&b, 1, vq1 / 50).ok();
    let rb = c.record(&b.pubkey());
    assert_eq!(rb.leg, 1);
    // a buys on leg 0 and sends b its coins, the hook's extras cut to the three it names.
    let a = c.trader();
    let vq0 = c.w.legs(&m).legs[0].virtual_quote;
    c.buy(&a, 0, vq0 / 20).ok();
    let moved = c.w.env.holding(&m, &a.pubkey());
    let before = c.record(&b.pubkey());
    let short = c.custom.extras[..3].to_vec();
    let tx = c.send_with(&a, &b.pubkey(), moved, short);
    // EXPECTED (fixed): on a token coin (leg_count > 0) the hook must be handed the `Legs`; a
    // transfer that omits it must be refused, never silently priced down the SOL path at leg 0's
    // spot. FAILS on the build under review (the transfer succeeds and re-prices b's leg-1 basis
    // at leg 0's spot).
    assert!(
        tx.result.is_err(),
        "a transfer that omits the Legs on a token coin must be refused; it succeeded and the \
         record went from {} to {} (leg {})",
        before.entry,
        c.record(&b.pubkey()).entry,
        c.record(&b.pubkey()).leg
    );
}

/// F1 (b): with the `Legs` passed but the record leg's pool left out, an arrival finds no price and
/// "the basis stays as it was": new coins bought cheaply on another leg are recorded at the
/// holding's old (high) entry. After a real fall, a holder averages down on a deep leg and its
/// recorded loss grows with every coin it adds.
#[test]
fn review5_without_the_record_legs_pool_new_coins_take_the_old_entry() {
    let mut c = Coin::new(&THREE);
    let m = c.mint;
    let whale = c.trader();
    let b = c.trader();
    let vq1 = c.w.legs(&m).legs[1].virtual_quote;
    // A pump on leg 1, b buys high there, the whale sells everything: a real fall.
    c.buy(&whale, 1, vq1 / 2).ok();
    c.buy(&b, 1, vq1 / 50).ok();
    let r = c.record(&b.pubkey());
    assert_eq!(r.leg, 1);
    let coins = c.w.env.holding(&m, &whale.pubkey());
    c.sell(&whale, 1, coins).ok();
    let spot1 = c.spot(1);
    assert!(r.entry > 2 * spot1);
    let honest_before = uncovered_loss(r.entry, spot1, r.amount, 10_000, 0);
    // b buys a lot on leg 0 with the hook's slice minus leg 1's pool.
    let vq0 = c.w.legs(&m).legs[0].virtual_quote;
    let keys0 = c.keys(0);
    let who = b.pubkey();
    let slice: Vec<AccountMeta> = c
        .custom
        .slice()
        .into_iter()
        .filter(|a| a.pubkey != c.pools[1])
        .collect();
    let _ = honest_before;
    let ix = launch::leg_swap_with_slice(
        &keys0,
        who,
        token::holding_address(&m, &who),
        keys0.quote_account(&who),
        1,
        vq0 / 4,
        0,
        slice,
        false,
    );
    let tx = c.w.env.send_paid_by(&[ix], &b, &[]);
    // EXPECTED (fixed): the hook must be handed the record leg's pool to price an arrival; a trade
    // that omits it must be refused, never record the new coins at the old (high) entry. FAILS on
    // the build under review (the buy succeeds and keeps the old entry for the cheap new coins).
    let r2 = c.record(&who);
    assert!(
        tx.result.is_err(),
        "a trade that omits the record leg's pool must be refused; it succeeded and kept entry {} \
         for {} new coins",
        r2.entry,
        r2.amount - r.amount
    );
}

/// Control: the same moves with the registry's extras whole are priced as the design says.
#[test]
fn review5_control_with_the_whole_extras() {
    let mut c = Coin::new(&LOPSIDED);
    let m = c.mint;
    let b = c.trader();
    let vq1 = c.w.legs(&m).legs[1].virtual_quote;
    c.buy(&b, 1, vq1 / 50).ok();
    let rb = c.record(&b.pubkey());
    let a = c.trader();
    let vq0 = c.w.legs(&m).legs[0].virtual_quote;
    c.buy(&a, 0, vq0 / 20).ok();
    let moved = c.w.env.holding(&m, &a.pubkey());
    let spot1 = c.spot(1);
    let extras = c.custom.extras.clone();
    c.send_with(&a, &b.pubkey(), moved, extras).ok();
    assert_eq!(
        c.record(&b.pubkey()).entry,
        blend(rb.entry, rb.amount, spot1, moved)
    );
}
