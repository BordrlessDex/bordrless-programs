//! `lottery_hook` on a real launch from a plain `LaunchConfig` (not through the companion: that is
//! the companion's own suite). Prepared for the mint before it exists (the mint's keypair signs),
//! launched, traded: buys, sells, wallet-to-wallet sends, burns, `enter`s, rounds passing.
//!
//! Every transaction is mirrored by a [`Model`] that applies the game ticket standard's rules
//! (`bordrless_game::on_send`, `on_receive`, `on_enter`) to the balances the chain shows, and after
//! each one the hook's header and every holding's 64 bytes of hook data must equal the model's,
//! byte for byte. The launch, its pool, the companion's creator address and any address off the
//! curve never get tickets; no transfer is ever refused.

use std::collections::BTreeMap;

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::InstructionData;
use bordrless_game::{
    eligible, header_offsets, on_enter, on_receive, on_send, wins, GameHeader, Range, Slots,
    HEADER_LEN, MAGIC,
};
use bordrless_hook::{
    hook_accounts_address, AccountSource, ExtraAccount, HookAccountList, Seed, TokenHookArgs,
};
use bordrless_launch::constants::STATUS_GRADUATED;
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_token::client as token;
use bordrless_token::state::Mint;
use lottery_hook::{client as lottery, Entered, LotteryError, LotteryState, Prepared, FLAGS};
use solana_keypair::Keypair;
use solana_signer::Signer;

/// An hour a round, the shortest the standard allows. `T0` starts a round.
const ROUND: u32 = 3_600;
const R: i64 = ROUND as i64;

#[track_caller]
fn refused(tx: &Tx, e: LotteryError) {
    tx.expect_code(u32::from(e));
    let name = format!("{e:?}");
    assert!(
        tx.logs()
            .iter()
            .any(|l| l.contains(&format!("Error Code: {name}"))),
        "expected {name}\n{}",
        tx.logs().join("\n")
    );
}

fn state(w: &World, mint: &Pubkey) -> LotteryState {
    w.env.read(&lottery::state_address(mint))
}

fn header(w: &World, mint: &Pubkey) -> GameHeader {
    let data = w.env.account(&lottery::state_address(mint)).unwrap().data;
    GameHeader::read(&data, mint).expect("a game header")
}

/// Prepares the hook for a fresh mint (its keypair signing), makes a config naming it (no kit
/// rules, creator fee 1%) and launches from it, then waits out the sniper window. Answers the
/// creator and the mint.
fn launch_lottery(w: &mut World) -> (Keypair, Pubkey) {
    let creator = w.wallet_with_sol(30 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    w.env
        .send_paid_by(
            &[lottery::prepare(creator.pubkey(), mint, ROUND)],
            &creator,
            &[&mint_kp],
        )
        .ok();
    let config = lottery_config(w, &creator);
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint, "LOTTO", VQ, &config);
    w.env.send_paid_by(&[ix], &creator, &[&mint_kp]).ok();
    assert_eq!(w.launch(&mint).custom_hook, Some(lottery_hook::ID));
    w.env.warp(31);
    (creator, mint)
}

fn lottery_config(w: &mut World, creator: &Keypair) -> Pubkey {
    let (config, tx) = w.create_config(
        creator,
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: 100,
            custom_hook: Some(lottery_hook::ID),
            custom_hook_flags: FLAGS,
            label: "Lottery".to_string(),
        },
    );
    tx.ok();
    config
}

/// The game as the standard says it must be: the header and every holding's slots, updated from
/// the balances the chain shows, through `bordrless_game`'s rules.
struct Model {
    mint: Pubkey,
    header: GameHeader,
    slots: BTreeMap<Pubkey, Slots>,
    /// The launch, its pool, the companion's creator address.
    excluded: Vec<Pubkey>,
    /// Owners whose hook data must stay zero, checked on every comparison.
    never: Vec<Pubkey>,
}

impl Model {
    fn new(w: &World, mint: &Pubkey) -> Self {
        let l = w.launch(mint);
        let creator = bordrless_companion::client::creator_address(mint);
        let launch = bordrless_launch::client::launch_address(mint);
        Self {
            mint: *mint,
            header: header(w, mint),
            slots: BTreeMap::new(),
            excluded: vec![launch, l.pool, creator],
            never: vec![launch, l.pool],
        }
    }

    fn slots_of(&mut self, owner: &Pubkey) -> &mut Slots {
        self.slots.entry(*owner).or_default()
    }

    /// A transfer of `amount` from `from` (holding `pre_from` before) to `to` (`pre_to`) at `now`.
    fn transfer(
        &mut self,
        from: &Pubkey,
        to: &Pubkey,
        pre_from: u64,
        pre_to: u64,
        amount: u64,
        now: i64,
    ) {
        self.header.roll(now);
        if eligible(from, &self.excluded) {
            let mut s = *self.slots_of(from);
            on_send(&mut self.header, &mut s, pre_from - amount, now);
            self.slots.insert(*from, s);
        }
        if eligible(to, &self.excluded) {
            let mut s = *self.slots_of(to);
            on_receive(&mut self.header, &mut s, pre_to, pre_to + amount, now);
            self.slots.insert(*to, s);
        }
    }

    fn burn(&mut self, from: &Pubkey, pre: u64, amount: u64, now: i64) {
        self.header.roll(now);
        if eligible(from, &self.excluded) {
            let mut s = *self.slots_of(from);
            on_send(&mut self.header, &mut s, pre - amount, now);
            self.slots.insert(*from, s);
        }
    }

    /// `enter`: answers whether the holding's data changes.
    fn enter(&mut self, owner: &Pubkey, balance: u64, now: i64) -> bool {
        let mut s = *self.slots_of(owner);
        let changed = on_enter(&mut self.header, &mut s, balance, now);
        self.slots.insert(*owner, s);
        changed
    }

    /// The chain equals the model, byte for byte: the header's bytes in the state account, every
    /// tracked holding's hook data, zeros for the owners that never hold tickets. And the
    /// standard's promises: no range above its balance, this round's ranges disjoint within the
    /// total.
    #[track_caller]
    fn check(&self, w: &World) {
        let data = w
            .env
            .account(&lottery::state_address(&self.mint))
            .unwrap()
            .data;
        assert_eq!(
            &data[8..header_offsets::END],
            &self.header.encode()[..],
            "the header\nchain {:?}\nmodel {:?}",
            GameHeader::parse(&data),
            self.header
        );
        for (owner, slots) in &self.slots {
            if w.env
                .account(&token::holding_address(&self.mint, owner))
                .is_none()
            {
                // Closed: only an empty holding with no hook data can be.
                assert_eq!(slots.encode(), [0u8; 64], "{owner} closed with tickets");
                continue;
            }
            let chain = w.env.hook_data(&self.mint, owner);
            assert_eq!(
                chain,
                slots.encode(),
                "{owner}'s hook data\nchain {:?}\nmodel {slots:?}",
                Slots::decode(&chain)
            );
            let balance = w.env.holding(&self.mint, owner);
            assert!(slots.current.weight <= balance && slots.previous.weight <= balance);
        }
        for owner in &self.never {
            let key = token::holding_address(&self.mint, owner);
            if w.env.account(&key).is_some() {
                assert_eq!(w.env.hook_data(&self.mint, owner), [0u8; 64], "{owner}");
            }
        }
        let mut ranges: Vec<Range> = self
            .slots
            .values()
            .filter_map(|s| s.range_in(self.header.round))
            .collect();
        ranges.sort_by_key(|r| r.start);
        let mut end = 0;
        for r in ranges {
            assert!(r.start >= end, "ranges overlap");
            end = r.end().unwrap();
        }
        assert!(end <= self.header.total);
    }
}

/// A buy by `t` on the launch pool, mirrored. Answers the tokens it got.
#[track_caller]
fn buy(w: &mut World, m: &mut Model, t: &Keypair, lamports: u64) -> u64 {
    let mint = m.mint;
    let pre = w.env.holding(&mint, &t.pubkey());
    w.buy(t, &mint, lamports).ok();
    let got = w.env.holding(&mint, &t.pubkey()) - pre;
    let pool = w.launch(&mint).pool;
    m.transfer(&pool, &t.pubkey(), 0, pre, got, w.env.now);
    m.check(w);
    got
}

/// A sell of `amount` by `t` into the launch pool, mirrored.
#[track_caller]
fn sell(w: &mut World, m: &mut Model, t: &Keypair, amount: u64) -> Tx {
    let mint = m.mint;
    let pre = w.env.holding(&mint, &t.pubkey());
    let tx = w.sell(t, &mint, amount);
    tx.ok();
    let pool = w.launch(&mint).pool;
    m.transfer(&t.pubkey(), &pool, pre, 0, amount, w.env.now);
    m.check(w);
    tx
}

/// A wallet-to-wallet send of `amount` from `from` to `to` (whose holding must exist), mirrored.
#[track_caller]
fn send(w: &mut World, m: &mut Model, from: &Keypair, to: &Pubkey, amount: u64) -> Tx {
    let mint = m.mint;
    let (pre_from, pre_to) = (
        w.env.holding(&mint, &from.pubkey()),
        w.env.holding(&mint, to),
    );
    let tx = w.send_tokens(from, mint, to, amount);
    tx.ok();
    m.transfer(&from.pubkey(), to, pre_from, pre_to, amount, w.env.now);
    m.check(w);
    tx
}

/// A burn of `amount` from `from`'s holding, with the hook's accounts, mirrored.
#[track_caller]
fn burn(w: &mut World, m: &mut Model, from: &Keypair, amount: u64) -> Tx {
    let mint = m.mint;
    let pre = w.env.holding(&mint, &from.pubkey());
    let ix = token::burn(
        from.pubkey(),
        token::holding_address(&mint, &from.pubkey()),
        mint,
        Some(lottery_hook::ID),
        lottery::extras(&mint),
        amount,
    );
    let tx = w.env.send_paid_by(&[ix], from, &[]);
    tx.ok();
    m.burn(&from.pubkey(), pre, amount, w.env.now);
    m.check(w);
    tx
}

/// `enter` for `owner`'s holding, sent by `sender`, mirrored. Answers the transaction.
#[track_caller]
fn enter(w: &mut World, m: &mut Model, sender: &Keypair, owner: &Pubkey) -> Tx {
    let mint = m.mint;
    let balance = w.env.holding(&mint, owner);
    let tx = w
        .env
        .send_paid_by(&[lottery::enter(mint, *owner)], sender, &[]);
    tx.ok();
    let changed = m.enter(owner, balance, w.env.now);
    assert_eq!(
        tx.events::<Entered>().len(),
        usize::from(changed),
        "an Entered event exactly when the holding's data changes"
    );
    m.check(w);
    tx
}

fn trader(w: &mut World, mint: &Pubkey, sol: u64) -> Keypair {
    let t = w.wallet_with_sol(sol);
    w.holdings(&t, *mint, &[t.pubkey()]);
    t
}

#[test]
fn prepare_writes_the_header_and_the_registry() {
    let mut w = World::new();
    let payer = w.wallet_with_sol(5 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let tx = w.env.send_paid_by(
        &[lottery::prepare(payer.pubkey(), mint, 6 * ROUND)],
        &payer,
        &[&mint_kp],
    );
    tx.ok();
    let now = w.env.now;
    // The header at the standard's offsets: a fresh game in the current round.
    let data = w.env.account(&lottery::state_address(&mint)).unwrap().data;
    assert_eq!(
        &data[header_offsets::MAGIC..header_offsets::MAGIC + 4],
        &MAGIC
    );
    let expected = GameHeader::new(mint, 6 * ROUND, now);
    assert_eq!(&data[8..8 + HEADER_LEN], &expected.encode()[..]);
    assert_eq!(header(&w, &mint).round, (now / (6 * R)) as u32);
    // The hook's own fields after it.
    let s = state(&w, &mint);
    let launch = bordrless_launch::client::launch_address(&mint);
    let creator = bordrless_companion::client::creator_address(&mint);
    assert_eq!(
        (s.launch, s.pool, s.creator, s.prepared_by, s.prepared_at),
        (launch, Pubkey::default(), creator, payer.pubkey(), now)
    );
    let ev: Prepared = tx.event();
    assert_eq!(
        (
            ev.mint,
            ev.state,
            ev.round_secs,
            ev.round,
            ev.launch,
            ev.creator,
            ev.prepared_by
        ),
        (
            mint,
            lottery::state_address(&mint),
            6 * ROUND,
            expected.round,
            launch,
            creator,
            payer.pubkey()
        )
    );
    // The standard's addresses are the programs' own: the state for the companion's reader, the
    // companion's creator address.
    assert_eq!(
        bordrless_game::state_address(&lottery_hook::ID, &mint).0,
        lottery::state_address(&mint)
    );
    assert_eq!(
        bordrless_game::COMPANION_PROGRAM_ID,
        bordrless_companion::ID
    );
    assert_eq!(
        bordrless_game::COMPANION_CREATOR_SEED,
        bordrless_companion::constants::CREATOR_SEED
    );
    assert_eq!(bordrless_game::LAUNCH_PROGRAM_ID, bordrless_launch::ID);
    // The registry: the state (writable), the launch (read-only), resolving to the client's list.
    let registry = w
        .env
        .account(&hook_accounts_address(&lottery_hook::ID, &mint).0)
        .unwrap();
    assert_eq!(registry.owner, lottery_hook::ID);
    let list = HookAccountList::decode(&registry.data).unwrap();
    assert_eq!(
        list.accounts,
        vec![
            ExtraAccount {
                writable: true,
                source: AccountSource::Pda {
                    program: lottery_hook::ID,
                    seeds: vec![Seed::Literal(b"state".to_vec()), Seed::Account(1)],
                },
            },
            ExtraAccount {
                writable: false,
                source: AccountSource::Pda {
                    program: bordrless_launch::ID,
                    seeds: vec![Seed::Literal(b"launch".to_vec()), Seed::Account(1)],
                },
            },
        ]
    );
    let resolved = w.env.token_hook_extras(
        &lottery_hook::ID,
        &mint,
        &Pubkey::new_unique(),
        &Pubkey::new_unique(),
        &Pubkey::new_unique(),
        &Pubkey::new_unique(),
        &Pubkey::new_unique(),
    );
    assert_eq!(resolved, lottery::extras(&mint));

    // Once per mint.
    let tx = w.env.send_paid_by(
        &[lottery::prepare(payer.pubkey(), mint, ROUND)],
        &payer,
        &[&mint_kp],
    );
    tx.expect_fail();
    assert_eq!(header(&w, &mint).round_secs, 6 * ROUND);
    // Rounds from an hour to 30 days.
    for bad in [0, ROUND - 1, 30 * 86_400 + 1] {
        let other = Keypair::new();
        let tx = w.env.send_paid_by(
            &[lottery::prepare(payer.pubkey(), other.pubkey(), bad)],
            &payer,
            &[&other],
        );
        refused(&tx, LotteryError::BadRoundSecs);
    }
    // Only the mint's keypair chooses its rounds: without its signature, nothing.
    let other = Keypair::new();
    let mut ix = lottery::prepare(payer.pubkey(), other.pubkey(), ROUND);
    ix.accounts[1].is_signer = false;
    let tx = w.env.send_paid_by(&[ix], &payer, &[]);
    tx.expect_code(3010); // AccountNotSigner
    assert!(w
        .env
        .account(&lottery::state_address(&other.pubkey()))
        .is_none());
}

#[test]
fn buys_sells_and_sends_follow_the_standard_byte_for_byte() {
    let mut w = World::new();
    let (_creator, mint) = launch_lottery(&mut w);
    let mut m = Model::new(&w, &mint);
    m.check(&w);
    let a = trader(&mut w, &mint, 20 * SOL);
    let b = trader(&mut w, &mint, 20 * SOL);
    let c = trader(&mut w, &mint, 20 * SOL);
    let crank = w.env.funded(SOL);
    let r0 = m.header.round;

    // A and B buy in round r0: they held nothing when it began, so no tickets in it; the slot
    // marks the round. The pool is read from the launch and remembered.
    let got_a = buy(&mut w, &mut m, &a, SOL);
    let got_b = buy(&mut w, &mut m, &b, 2 * SOL);
    assert_eq!(m.slots[&a.pubkey()].current, Range::none_in(r0));
    assert_eq!(m.header.total, 0);
    assert_eq!(state(&w, &mint).pool, w.launch(&mint).pool);
    // Entering them changes nothing this round.
    let tx = enter(&mut w, &mut m, &crank, &a.pubkey());
    assert!(tx.events::<Entered>().is_empty());

    // Round r0 + 1: A buys more; its first write of the round registers what it held when the
    // round began (not what it buys now). B is entered: its whole balance, the next range.
    w.env.warp(R);
    let r1 = r0 + 1;
    buy(&mut w, &mut m, &a, SOL / 10);
    assert_eq!(
        m.slots[&a.pubkey()].current,
        Range {
            round: r1,
            start: 0,
            weight: got_a
        }
    );
    enter(&mut w, &mut m, &crank, &b.pubkey());
    assert_eq!(
        m.slots[&b.pubkey()].current,
        Range {
            round: r1,
            start: got_a,
            weight: got_b
        }
    );
    // More buys add nothing this round: no range grows, the total stays.
    buy(&mut w, &mut m, &a, 3 * SOL);
    buy(&mut w, &mut m, &b, SOL / 10);
    assert_eq!(m.slots[&a.pubkey()].current.weight, got_a);
    assert_eq!(m.slots[&b.pubkey()].current.weight, got_b);
    assert_eq!(m.header.total, got_a + got_b);

    // B sells most: its range is cut to what it has left.
    let held = w.env.holding(&mint, &b.pubkey());
    let tx = sell(&mut w, &mut m, &b, held - got_b / 2);
    println!("sell CU {}", tx.cu());
    assert_eq!(m.slots[&b.pubkey()].current.weight, got_b / 2);
    assert_eq!(m.slots[&b.pubkey()].since, w.env.now);
    // A sends C a part (A's range only cut below its weight); C, which held nothing, gets no
    // tickets this round.
    let part = w.env.holding(&mint, &a.pubkey()) - got_a / 2;
    let tx = send(&mut w, &mut m, &a, &c.pubkey(), part);
    println!("send CU {}", tx.cu());
    assert_eq!(m.slots[&a.pubkey()].current.weight, got_a / 2);
    assert_eq!(m.slots[&c.pubkey()].current, Range::none_in(r1));
    // C sends everything back: C is cleared (it can close its holding); A's range stays as it was.
    send(&mut w, &mut m, &c, &a.pubkey(), part);
    assert_eq!(w.env.hook_data(&mint, &c.pubkey()), [0u8; 64]);
    assert_eq!(m.slots[&a.pubkey()].current.weight, got_a / 2);
    // C receives again: still nothing this round, and nothing to enter.
    send(&mut w, &mut m, &a, &c.pubkey(), 10);
    assert_eq!(m.slots[&c.pubkey()].current, Range::none_in(r1));
    enter(&mut w, &mut m, &crank, &c.pubkey());
    // Dust to B does not touch B's range.
    let before = m.slots[&b.pubkey()];
    send(&mut w, &mut m, &a, &b.pubkey(), 1);
    assert_eq!(m.slots[&b.pubkey()].current, before.current);
    assert_eq!(m.header.total, got_a + got_b);

    // Round r0 + 2: C's 10 count now.
    w.env.warp(R);
    enter(&mut w, &mut m, &crank, &c.pubkey());
    assert_eq!(m.slots[&c.pubkey()].current.weight, 10);
    // A buy's CU.
    let pre = w.env.holding(&mint, &c.pubkey());
    let tx = w.buy(&c, &mint, SOL / 2);
    tx.ok();
    println!("buy CU {}", tx.cu());
    let pool = w.launch(&mint).pool;
    let got = w.env.holding(&mint, &c.pubkey()) - pre;
    m.transfer(&pool, &c.pubkey(), 0, pre, got, w.env.now);
    m.check(&w);
}

#[test]
fn rounds_roll_and_enter_registers_for_the_new_round() {
    let mut w = World::new();
    let (_creator, mint) = launch_lottery(&mut w);
    let mut m = Model::new(&w, &mint);
    let a = trader(&mut w, &mint, 20 * SOL);
    let b = trader(&mut w, &mint, 20 * SOL);
    let crank = w.env.funded(SOL);
    buy(&mut w, &mut m, &a, SOL);
    buy(&mut w, &mut m, &b, SOL);
    // Round r: both entered by the keeper.
    w.env.warp(R);
    enter(&mut w, &mut m, &crank, &a.pubkey());
    enter(&mut w, &mut m, &crank, &b.pubkey());
    let r = m.header.round;
    let total_r = m.header.total;
    let a_r = m.slots[&a.pubkey()].current;
    assert_eq!(a_r.weight, w.env.holding(&mint, &a.pubkey()));

    // Entering again in the same round does nothing: no write, no event.
    let tx = enter(&mut w, &mut m, &crank, &a.pubkey());
    assert!(tx.events::<Entered>().is_empty());

    // The round ends. Nothing is written until something happens: the header still says r.
    w.env.warp(R);
    assert_eq!(header(&w, &mint).round, r);
    // A stranger enters A: the header rolls (r and its total move to prev), A's round-r range moves
    // to its previous slot, and its whole balance is its range for r + 1.
    let tx = enter(&mut w, &mut m, &crank, &a.pubkey());
    println!("enter CU {}", tx.cu());
    let ev: Entered = tx.event();
    let h = header(&w, &mint);
    assert_eq!((h.round, h.prev_round, h.prev_total), (r + 1, r, total_r));
    let balance_a = w.env.holding(&mint, &a.pubkey());
    assert_eq!(
        (ev.mint, ev.owner, ev.round, ev.start, ev.weight, ev.total),
        (mint, a.pubkey(), r + 1, 0, balance_a, balance_a)
    );
    let slots_a = Slots::decode(&w.env.hook_data(&mint, &a.pubkey()));
    assert_eq!(slots_a.previous, a_r);
    // Entering twice is a no-op, so nobody can grief A by entering it again.
    let tx = enter(&mut w, &mut m, &crank, &a.pubkey());
    assert!(tx.events::<Entered>().is_empty());

    // A won round r (its round-r tickets are in its previous slot); it sells part in r + 1: both
    // slots are cut to what it has left, so it can only claim what it still holds.
    let x = a_r.start + a_r.weight - 1;
    assert!(wins(&w.env.hook_data(&mint, &a.pubkey()), r, x, balance_a));
    sell(&mut w, &mut m, &a, balance_a / 2);
    let left = w.env.holding(&mint, &a.pubkey());
    let data = w.env.hook_data(&mint, &a.pubkey());
    assert!(!wins(&data, r, x, left), "the tickets it sold are dead");
    assert!(wins(&data, r, a_r.start + left - 1, left));
    assert_eq!(h.total_of(r), Some(total_r));

    // B enters in r + 1 too, then two rounds pass with no write at all.
    enter(&mut w, &mut m, &crank, &b.pubkey());
    let total_r1 = m.header.total;
    let balance_b = w.env.holding(&mint, &b.pubkey());
    w.env.warp(2 * R);
    buy(&mut w, &mut m, &b, SOL / 10);
    let h = header(&w, &mint);
    assert_eq!(
        (h.round, h.prev_round, h.prev_total),
        (r + 3, r + 1, total_r1)
    );
    assert_eq!(h.total_of(r + 1), Some(total_r1));
    assert_eq!(h.total_of(r + 2), Some(0), "no write in r + 2: no tickets");
    assert_eq!(h.total_of(r), None, "forgotten: round r rolls over");
    // B's buy registered what it held when r + 3 began, keeping its r + 1 range for the claim.
    let slots_b = Slots::decode(&w.env.hook_data(&mint, &b.pubkey()));
    assert_eq!(slots_b.previous.round, r + 1);
    assert_eq!(
        slots_b.current,
        Range {
            round: r + 3,
            start: 0,
            weight: balance_b
        }
    );
}

#[test]
fn the_pool_the_launch_and_the_companion_never_hold_tickets() {
    let mut w = World::new();
    let (creator, mint) = launch_lottery(&mut w);
    let mut m = Model::new(&w, &mint);
    let a = trader(&mut w, &mint, 20 * SOL);
    buy(&mut w, &mut m, &a, 2 * SOL);
    let l = w.launch(&mint);
    // The launch's reserve and the pool's vault: holdings the hook never writes.
    assert_eq!(w.env.hook_data(&mint, &l.pool), [0u8; 64]);
    assert_eq!(
        w.env
            .hook_data(&mint, &bordrless_launch::client::launch_address(&mint)),
        [0u8; 64]
    );
    // Tokens sent to the companion's creator address, or to any program's address, get no tickets
    // (and the sender's are cut as for any send).
    let companion_creator = bordrless_companion::client::creator_address(&mint);
    let escrow = Pubkey::find_program_address(&[b"escrow"], &Pubkey::new_unique()).0;
    w.holdings(&a, mint, &[companion_creator, escrow]);
    m.never.extend([companion_creator, escrow]);
    send(&mut w, &mut m, &a, &companion_creator, 1_000_000);
    send(&mut w, &mut m, &a, &escrow, 1_000_000);
    assert_eq!(w.env.hook_data(&mint, &companion_creator), [0u8; 64]);
    assert_eq!(w.env.hook_data(&mint, &escrow), [0u8; 64]);
    // Nobody can enter them either.
    for owner in [
        l.pool,
        bordrless_launch::client::launch_address(&mint),
        companion_creator,
        escrow,
    ] {
        let tx = w.env.send_paid_by(&[lottery::enter(mint, owner)], &a, &[]);
        refused(&tx, LotteryError::NotEligible);
    }
    // The launch's own creator is a wallet here (not a companion launch): an ordinary holder, with
    // tickets from the round after its buy.
    w.holdings(&creator, mint, &[creator.pubkey()]);
    buy(&mut w, &mut m, &creator, SOL);
    assert!(!m.slots[&creator.pubkey()].current.is_live());
    w.env.warp(R);
    enter(&mut w, &mut m, &a, &creator.pubkey());
    assert!(m.slots[&creator.pubkey()].current.is_live());

    // The curve fills and graduates through the hook (the reserve's top-up into the pool and its
    // burn): the launch and the pool still hold no tickets, and nothing was refused.
    let (wallets, tx) = w.graduate_launch(&mint);
    tx.ok();
    assert_eq!(w.launch(&mint).status, STATUS_GRADUATED);
    assert_eq!(w.env.hook_data(&mint, &l.pool), [0u8; 64]);
    assert_eq!(
        w.env
            .hook_data(&mint, &bordrless_launch::client::launch_address(&mint)),
        [0u8; 64]
    );
    let h = header(&w, &mint);
    let mut sum = 0;
    for wallet in &wallets {
        let balance = w.env.holding(&mint, &wallet.pubkey());
        let slots = Slots::decode(&w.env.hook_data(&mint, &wallet.pubkey()));
        assert!(slots.current.weight <= balance);
        if balance > 0 {
            assert_eq!(slots.current.round, h.round);
            assert_eq!(
                slots.current.weight, 0,
                "a fresh buyer's tokens count from the next round"
            );
        }
    }
    // The next round: each is entered, its range its balance (the graduation's buys were not
    // mirrored in the model, so the chain alone is checked).
    w.env.warp(R);
    for wallet in &wallets {
        let balance = w.env.holding(&mint, &wallet.pubkey());
        if balance > 0 {
            w.env
                .send_paid_by(&[lottery::enter(mint, wallet.pubkey())], &a, &[])
                .ok();
            let slots = Slots::decode(&w.env.hook_data(&mint, &wallet.pubkey()));
            assert_eq!(slots.current.weight, balance);
            sum += balance;
        }
    }
    assert!(sum <= header(&w, &mint).total);
}

#[test]
fn burns_cut_tickets_and_an_emptied_holding_closes() {
    let mut w = World::new();
    let (_creator, mint) = launch_lottery(&mut w);
    let mut m = Model::new(&w, &mint);
    let a = trader(&mut w, &mint, 20 * SOL);
    let b = trader(&mut w, &mint, 20 * SOL);
    buy(&mut w, &mut m, &a, SOL);
    buy(&mut w, &mut m, &b, SOL);
    w.env.warp(R);
    enter(&mut w, &mut m, &b, &a.pubkey());
    let supply = w.env.read::<Mint>(&mint).supply;
    let held = w.env.holding(&mint, &a.pubkey());
    let tx = burn(&mut w, &mut m, &a, held / 2);
    println!("burn CU {}", tx.cu());
    assert_eq!(w.env.read::<Mint>(&mint).supply, supply - held / 2);
    assert_eq!(
        m.slots[&a.pubkey()].current.weight,
        held - held / 2,
        "burned tickets are dead"
    );
    // Burning the rest clears the holding's data, so it closes.
    burn(&mut w, &mut m, &a, held - held / 2);
    assert_eq!(w.env.hook_data(&mint, &a.pubkey()), [0u8; 64]);
    let close = token::close_holding(
        a.pubkey(),
        mint,
        token::holding_address(&mint, &a.pubkey()),
        a.pubkey(),
    );
    w.env.send_paid_by(&[close], &a, &[]).ok();
    assert!(w
        .env
        .account(&token::holding_address(&mint, &a.pubkey()))
        .is_none());
    // So does a holding sold out to the pool.
    let all = w.env.holding(&mint, &b.pubkey());
    sell(&mut w, &mut m, &b, all);
    assert_eq!(w.env.hook_data(&mint, &b.pubkey()), [0u8; 64]);
    let close = token::close_holding(
        b.pubkey(),
        mint,
        token::holding_address(&mint, &b.pubkey()),
        b.pubkey(),
    );
    w.env.send_paid_by(&[close], &b, &[]).ok();
}

/// xorshift64*.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let v = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d);
        if n == 0 {
            0
        } else {
            v % n
        }
    }
}

#[test]
fn no_transfer_is_ever_refused() {
    let mut w = World::new();
    let (_creator, mint) = launch_lottery(&mut w);
    let mut m = Model::new(&w, &mint);
    let wallets: Vec<Keypair> = (0..6).map(|_| trader(&mut w, &mint, 60 * SOL)).collect();
    let crank = w.env.funded(10 * SOL);
    let companion_creator = bordrless_companion::client::creator_address(&mint);
    let escrow = Pubkey::find_program_address(&[b"vault"], &Pubkey::new_unique()).0;
    w.holdings(&crank, mint, &[companion_creator, escrow]);
    m.never.extend([companion_creator, escrow]);
    let mut rng = Rng(0x5eed_1077_e4e5_0001);
    let (mut buys, mut sells, mut sends, mut burns, mut enters, mut rounds) = (0, 0, 0, 0, 0, 0);
    for _ in 0..300 {
        let i = rng.below(wallets.len() as u64) as usize;
        let t = &wallets[i];
        let held = w.env.holding(&mint, &t.pubkey());
        match rng.below(100) {
            0..=29 => {
                let lamports = SOL / 100 + rng.below(SOL);
                if w.launch_quote(&mint, &t.pubkey(), true, lamports)
                    .failure
                    .is_none()
                {
                    buy(&mut w, &mut m, t, lamports);
                    buys += 1;
                }
            }
            30..=44 => {
                let amount = rng.below(held) + 1;
                if held > 0
                    && w.launch_quote(&mint, &t.pubkey(), false, amount)
                        .failure
                        .is_none()
                {
                    sell(&mut w, &mut m, t, amount);
                    sells += 1;
                }
            }
            45..=64 => {
                if held > 0 {
                    // To another wallet, dust or not, all or part; now and then to an address
                    // that never holds tickets.
                    let to = match rng.below(10) {
                        0 => companion_creator,
                        1 => escrow,
                        _ => wallets[rng.below(wallets.len() as u64) as usize].pubkey(),
                    };
                    let amount = match rng.below(4) {
                        0 => 1,
                        1 => held,
                        _ => rng.below(held) + 1,
                    };
                    if to != t.pubkey() {
                        send(&mut w, &mut m, t, &to, amount);
                        sends += 1;
                    }
                }
            }
            65..=69 => {
                if held > 0 {
                    burn(&mut w, &mut m, t, rng.below(held / 4 + 1).max(1));
                    burns += 1;
                }
            }
            70..=84 => {
                enter(&mut w, &mut m, &crank, &t.pubkey());
                enters += 1;
            }
            85..=94 => w.env.warp(rng.below(R as u64) as i64 + 1),
            _ => {
                w.env.warp(R * (1 + rng.below(2) as i64));
                rounds += 1;
            }
        }
    }
    println!(
        "{buys} buys, {sells} sells, {sends} sends, {burns} burns, {enters} enters, {rounds} round jumps; final round total {}",
        m.header.total
    );
    assert!(buys > 30 && sells > 10 && sends > 20 && burns > 3 && enters > 20 && rounds > 3);
    // The draw finds exactly the holding whose range contains the ticket, for every ticket checked.
    let round = m.header.round;
    let total = m.header.total;
    for k in 0..200u32 {
        let x = bordrless_game::draw_index(&[7u8; 64], k, total).unwrap();
        let winners: Vec<&Keypair> = wallets
            .iter()
            .filter(|t| {
                wins(
                    &w.env.hook_data(&mint, &t.pubkey()),
                    round,
                    x,
                    w.env.holding(&mint, &t.pubkey()),
                )
            })
            .collect();
        assert!(winners.len() <= 1);
        let holder = m
            .slots
            .iter()
            .find(|(_, s)| s.range_in(round).is_some_and(|r| r.contains(x)));
        assert_eq!(
            winners.first().map(|t| t.pubkey()),
            holder.map(|(o, _)| *o),
            "ticket {x}"
        );
    }
}

#[test]
fn the_callbacks_only_take_the_token_programs_signer() {
    let mut w = World::new();
    let (creator, mint) = launch_lottery(&mut w);
    let fake = Keypair::new();
    let t = w.env.funded(SOL);
    let args = TokenHookArgs {
        op: bordrless_hook::TokenOp::Transfer,
        phase: bordrless_hook::Phase::Before,
        mint,
        source: token::holding_address(&mint, &t.pubkey()),
        destination: token::holding_address(&mint, &creator.pubkey()),
        source_owner: t.pubkey(),
        destination_owner: creator.pubkey(),
        authority: t.pubkey(),
        authority_is_delegate: false,
        amount: 1,
        delta: 0,
        source_balance: 1,
        destination_balance: 0,
        decimals: 6,
        supply: 1,
        source_hook_data: [0; 64],
        destination_hook_data: [0; 64],
    };
    let metas = |signer: Pubkey, is_signer: bool| {
        let mut metas = vec![
            AccountMeta::new_readonly(signer, is_signer),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(args.source, false),
            AccountMeta::new_readonly(args.destination, false),
            AccountMeta::new_readonly(t.pubkey(), false),
        ];
        metas.extend(lottery::extras(&mint));
        metas
    };
    let before = w.env.account(&lottery::state_address(&mint)).unwrap().data;
    for data in [
        lottery_hook::instruction::BeforeTransfer { args: args.clone() }.data(),
        lottery_hook::instruction::BeforeBurn { args: args.clone() }.data(),
    ] {
        // Signed by someone else: refused.
        let ix = Instruction {
            program_id: lottery_hook::ID,
            accounts: metas(fake.pubkey(), true),
            data: data.clone(),
        };
        let tx = w.env.send_paid_by(&[ix], &t, &[&fake]);
        refused(&tx, LotteryError::BadHookSigner);
        // The token program's signer, unsigned (only the token program can sign for it): refused.
        let ix = Instruction {
            program_id: lottery_hook::ID,
            accounts: metas(lottery_hook::TOKEN_HOOK_SIGNER, false),
            data,
        };
        let tx = w.env.send_paid_by(&[ix], &t, &[]);
        tx.expect_fail();
    }
    // Nothing moved.
    assert_eq!(
        w.env.account(&lottery::state_address(&mint)).unwrap().data,
        before
    );
}

#[test]
fn enter_takes_only_this_mints_holdings() {
    let mut w = World::new();
    let (_creator, mint) = launch_lottery(&mut w);
    let mut m = Model::new(&w, &mint);
    let a = trader(&mut w, &mint, 20 * SOL);
    buy(&mut w, &mut m, &a, SOL);
    // A holding of another mint (A's bridged SOL) passed for this mint's: refused.
    let mut ix = lottery::enter(mint, a.pubkey());
    ix.accounts[2].pubkey = token::holding_address(&w.sol, &a.pubkey());
    let tx = w.env.send_paid_by(&[ix], &a, &[]);
    refused(&tx, LotteryError::WrongMint);
    // Another mint with no lottery state: refused (no state at its address).
    let ix = lottery::enter(w.sol, a.pubkey());
    let tx = w.env.send_paid_by(&[ix], &a, &[]);
    tx.expect_fail();
    // A mint whose hook is not this program, with a state for it made by `prepare`: refused.
    let plain = Keypair::new();
    w.env
        .send_paid_by(
            &[lottery::prepare(a.pubkey(), plain.pubkey(), ROUND)],
            &a,
            &[&plain],
        )
        .ok();
    let args = bordrless_token::instructions::CreateMintArgs {
        decimals: 6,
        name: "Plain".to_string(),
        symbol: "PLN".to_string(),
        uri: String::new(),
        max_supply: 0,
        mint_authority: Some(a.pubkey()),
        freeze_authority: None,
        hook_program: None,
        hook_flags: 0,
        hook_authority: None,
        metadata_authority: None,
    };
    w.env
        .send_paid_by(
            &[
                token::create_mint(a.pubkey(), plain.pubkey(), args),
                token::create_holding(a.pubkey(), plain.pubkey(), a.pubkey()),
            ],
            &a,
            &[&plain],
        )
        .ok();
    let tx = w
        .env
        .send_paid_by(&[lottery::enter(plain.pubkey(), a.pubkey())], &a, &[]);
    refused(&tx, LotteryError::NotThisHook);
    // The right accounts: fine (a no-op, A was written this round).
    enter(&mut w, &mut m, &a, &a.pubkey());
}

/// The site's flow for a lottery coin without a companion: `prepare` (with the mint's signature),
/// then `create_launch` from the config as a v0 transaction with the protocol lookup table and the
/// longest metadata the site sends, the hook's accounts built without reading the chain. It fits
/// mainnet's limits; the first buy registers the buyer.
#[test]
fn a_lottery_launch_fits_mainnet_limits() {
    use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price};
    let mut w = World::new();
    let table = w
        .env
        .put_lookup_table(Pubkey::new_unique(), &protocol_lookup_table(&w));
    let creator = w.wallet_with_sol(20 * SOL);
    let config = lottery_config(&mut w, &creator);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    w.env
        .send_paid_by(
            &[lottery::prepare(creator.pubkey(), mint, 6 * ROUND)],
            &creator,
            &[&mint_kp],
        )
        .ok();
    let custom = bordrless_launch::client::CustomHookAccounts {
        program: lottery_hook::ID,
        extras: lottery::extras(&mint),
    };
    assert_eq!(
        custom.extras,
        w.custom_hook_accounts(&lottery_hook::ID, &mint).extras
    );
    let c = w.launch_config(&config);
    let mut args = World::launch_args("LOTTERY123", c.creator_fee_bps, VQ, c.rules);
    args.name = "N".repeat(32);
    args.uri = format!("https://{}", "u".repeat(120));
    let launch_ix = bordrless_launch::client::create_launch_with(
        creator.pubkey(),
        mint,
        w.env.treasury.pubkey(),
        w.sol,
        bordrless_core::policy::LP_FEE_BPS,
        args,
        Some(config),
        Some(&custom),
    );
    let ixs = [
        compute_unit_limit(1_400_000),
        compute_unit_price(20_000),
        launch_ix,
    ];
    let tx = w.env.send_v0(&ixs, &creator, &[&mint_kp], &[table]);
    tx.ok();
    println!(
        "create_launch, lottery hook, the site's longest metadata: {} keys, {} v0 bytes, {} trace, height {}, {} CU",
        tx.keys.len(),
        tx.size,
        tx.trace_len(),
        tx.max_height(),
        tx.cu()
    );
    assert!(tx.size <= 1_232, "{} bytes", tx.size);
    assert!(tx.trace_len() <= 64, "trace {}", tx.trace_len());
    assert!(tx.max_height() <= 5, "height {}", tx.max_height());
    // The deposit's callback could not read the launch yet: the pool is remembered on the first
    // buy; the buyer's tokens count from the next round, where an `enter` registers them.
    assert_eq!(state(&w, &mint).pool, Pubkey::default());
    w.env.warp(31);
    let mut m = Model::new(&w, &mint);
    let t = trader(&mut w, &mint, 10 * SOL);
    let got = buy(&mut w, &mut m, &t, SOL);
    assert_eq!(state(&w, &mint).pool, w.launch(&mint).pool);
    let round = header(&w, &mint).round;
    assert_eq!(
        Slots::decode(&w.env.hook_data(&mint, &t.pubkey())).current,
        Range::none_in(round)
    );
    w.env.warp(6 * R);
    let tx = enter(&mut w, &mut m, &t, &t.pubkey());
    println!("enter: {} bytes, {} CU", tx.size, tx.cu());
    assert_eq!(
        Slots::decode(&w.env.hook_data(&mint, &t.pubkey())).current,
        Range {
            round: round + 1,
            start: 0,
            weight: got
        }
    );
}
