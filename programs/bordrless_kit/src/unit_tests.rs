//! Host tests of the kit's pure parts: the reward math (§4.7, §4.8), the callbacks' rules in the
//! order of §4.9, and the mirror (§4.13) against the program's own sync.

use anchor_lang::prelude::*;
use bordrless_hook::{HookReturn, Phase, TokenHookArgs, TokenOp};

use crate::constants::*;
use crate::error::KitError;
use crate::math::{settle, stream_release};
use crate::mirror;
use crate::rules;
use crate::state::{HolderData, KitConfig, KitInitArgs};

const NOW: i64 = 1_800_000_000;
const SUPPLY: u64 = 1_000_000_000_000_000;

fn code<T: std::fmt::Debug>(r: Result<T>) -> u32 {
    match r {
        Err(Error::AnchorError(e)) => e.error_code_number,
        other => panic!("expected an Anchor error, got {other:?}"),
    }
}

fn kit(e: KitError) -> u32 {
    u32::from(e)
}

/// A key on the ed25519 curve (a wallet).
fn wallet() -> Pubkey {
    loop {
        let key = Pubkey::new_unique();
        if key.is_on_curve() {
            return key;
        }
    }
}

/// A key off the curve (a PDA).
fn pda() -> Pubkey {
    Pubkey::find_program_address(&[b"somewhere", Pubkey::new_unique().as_ref()], &crate::ID).0
}

struct Kit {
    config: KitConfig,
    key: Pubkey,
}

fn setup(modules: u8) -> Kit {
    let args = KitInitArgs {
        launch: pda(),
        pool: pda(),
        creator: wallet(),
        reward_mint: Pubkey::new_unique(),
        modules,
        max_wallet_bps: if modules & modules::MAX_WALLET != 0 {
            200
        } else {
            0
        },
        creator_unlock_at: if modules & modules::CREATOR_WALLET_LOCK != 0 {
            NOW + 86_400
        } else {
            0
        },
        early_window_end: if modules & modules::EARLY_BUYER_LOCK != 0 {
            NOW + 60
        } else {
            0
        },
        early_unlock_at: if modules & modules::EARLY_BUYER_LOCK != 0 {
            NOW + 3_600
        } else {
            0
        },
        kit_caller_bump: 255,
    };
    let vault = (modules & modules::HOLDER_REWARDS != 0).then(Pubkey::new_unique);
    let mint = Pubkey::new_unique();
    let config = KitConfig::install(&args, mint, SUPPLY, 255, vault, NOW).unwrap();
    Kit {
        config,
        key: KitConfig::address(&mint).0,
    }
}

/// The arguments of a transfer as the token program builds them.
#[allow(clippy::too_many_arguments)]
fn transfer(
    config: &KitConfig,
    from: Pubkey,
    to: Pubkey,
    amount: u64,
    from_balance: u64,
    to_balance: u64,
    from_data: [u8; 64],
    to_data: [u8; 64],
) -> TokenHookArgs {
    TokenHookArgs {
        op: TokenOp::Transfer,
        phase: Phase::Before,
        mint: config.mint,
        source: Pubkey::new_unique(),
        destination: Pubkey::new_unique(),
        source_owner: from,
        destination_owner: to,
        authority: from,
        authority_is_delegate: false,
        amount,
        delta: 0,
        source_balance: from_balance,
        destination_balance: to_balance,
        decimals: 6,
        supply: SUPPLY,
        source_hook_data: from_data,
        destination_hook_data: to_data,
    }
}

fn burn(
    config: &KitConfig,
    from: Pubkey,
    amount: u64,
    balance: u64,
    data: [u8; 64],
) -> TokenHookArgs {
    TokenHookArgs {
        op: TokenOp::Burn,
        destination: config.mint,
        destination_owner: Pubkey::default(),
        destination_balance: 0,
        destination_hook_data: [0; 64],
        ..transfer(
            config,
            from,
            Pubkey::default(),
            amount,
            balance,
            0,
            data,
            [0; 64],
        )
    }
}

/// A small ledger of balances and hook data, moved through the rules as the token program would.
struct Ledger {
    kit: Kit,
    vault: u64,
    balances: std::collections::HashMap<Pubkey, (u64, [u8; 64])>,
    now: i64,
}

impl Ledger {
    fn new(modules: u8) -> Self {
        let kit = setup(modules);
        let mut balances = std::collections::HashMap::new();
        // The whole supply in the launch, three quarters moved to the pool.
        balances.insert(kit.config.launch, (SUPPLY, [0; 64]));
        let mut ledger = Self {
            kit,
            vault: 0,
            balances,
            now: NOW,
        };
        let (launch, pool) = (ledger.kit.config.launch, ledger.kit.config.pool);
        ledger.send(launch, pool, SUPPLY / 4 * 3).unwrap();
        ledger
    }

    fn get(&self, owner: &Pubkey) -> (u64, [u8; 64]) {
        self.balances.get(owner).copied().unwrap_or((0, [0; 64]))
    }

    fn send(&mut self, from: Pubkey, to: Pubkey, amount: u64) -> Result<HookReturn> {
        let (fb, fd) = self.get(&from);
        let (tb, td) = self.get(&to);
        let args = transfer(&self.kit.config, from, to, amount, fb, tb, fd, td);
        let vault = self.kit.config.rewards_on().then_some(self.vault);
        let mut config = self.kit.config.clone();
        let ret = rules::before_transfer(&mut config, &self.kit.key, &args, vault, self.now)?;
        self.kit.config = config;
        let fd = ret.source_hook_data.unwrap_or(fd);
        let td = ret.destination_hook_data.unwrap_or(td);
        self.balances.insert(from, (fb - amount, fd));
        self.balances.insert(to, (tb + amount, td));
        Ok(ret)
    }

    fn burn(&mut self, from: Pubkey, amount: u64) -> Result<HookReturn> {
        let (fb, fd) = self.get(&from);
        let args = burn(&self.kit.config, from, amount, fb, fd);
        let vault = self.kit.config.rewards_on().then_some(self.vault);
        let mut config = self.kit.config.clone();
        let ret = rules::before_burn(&mut config, &args, vault, self.now)?;
        self.kit.config = config;
        self.balances
            .insert(from, (fb - amount, ret.source_hook_data.unwrap_or(fd)));
        Ok(ret)
    }

    /// What `share` does to the accounting: sync, the lamports into the vault, then the stream.
    fn share(&mut self, amount: u64) -> Result<()> {
        require!(self.kit.config.divides(), KitError::NoEligibleHolders);
        self.kit.config.sync(self.vault, self.now)?;
        self.vault += amount;
        self.kit.config.add_share(amount, self.now)
    }

    /// What `claim` does to the accounting, with the vault paying.
    fn claim(&mut self, owner: Pubkey) -> u64 {
        let (balance, read) = self.get(&owner);
        let expected = mirror::claimable(
            &self.kit.config,
            self.vault,
            &owner,
            balance,
            &read,
            self.now,
        )
        .unwrap()
        .min(self.vault);
        let config = &mut self.kit.config;
        config.sync(self.vault, self.now).unwrap();
        let mut data = HolderData::read(&read);
        settle(&mut data, balance, balance, config.acc_per_share).unwrap();
        let pay = data.owed.min(self.vault);
        assert_eq!(pay, expected, "a claim pays what the mirror computed");
        data.owed -= pay;
        config.total_claimed += pay;
        self.vault -= pay;
        self.balances
            .insert(owner, (balance, data.with_rewards_of(&read)));
        pay
    }

    fn holders(&self) -> Vec<Pubkey> {
        self.balances
            .keys()
            .copied()
            .filter(|o| !self.kit.config.is_excluded(o))
            .collect()
    }

    /// The vault covers what every holder could claim, what is held and what still streams (the
    /// running stream and the shares waiting for it); the eligible counter is the holders'
    /// balances.
    fn check(&self) -> u64 {
        let c = &self.kit.config;
        let after = mirror::synced(c, self.vault, self.now).unwrap();
        let mut owed = 0u64;
        let mut sum = 0u64;
        for o in self.holders() {
            let (b, d) = self.get(&o);
            sum += b;
            owed += mirror::claimable(c, self.vault, &o, b, &d, self.now).unwrap();
        }
        assert_eq!(c.eligible, sum, "eligible is the holders' balances");
        let covered = owed + after.held + after.streaming();
        assert!(
            self.vault >= covered,
            "vault {} < claimable {owed} + held {} + stream {} + waiting {}",
            self.vault,
            after.held,
            after.stream_remaining,
            after.stream_next
        );
        self.vault - covered
    }
}

#[test]
fn the_hand_example_pays_at_most_what_came_in() {
    // One holder of 1.5e12 (min_eligible 1e12), inflows of 2, 0, 1, 1, 1 lamports.
    for claim_each_time in [true, false] {
        let mut l = Ledger::new(modules::HOLDER_REWARDS);
        let h = wallet();
        let pool = l.kit.config.pool;
        l.send(pool, h, 1_500_000_000_000).unwrap();
        let mut paid = 0;
        for inflow in [2u64, 0, 1, 1, 1] {
            l.vault += inflow;
            if claim_each_time {
                paid += l.claim(h);
            } else {
                // A sync that settles nobody: the launch sends to the pool.
                let (launch, pool) = (l.kit.config.launch, l.kit.config.pool);
                l.send(launch, pool, 1).unwrap();
            }
            l.check();
        }
        paid += l.claim(h);
        assert!(paid <= 5);
        // Settling after every inflow loses each half lamport; once at the end loses one.
        assert_eq!(paid, if claim_each_time { 3 } else { 4 });
        assert_eq!(l.kit.config.total_claimed, paid);
        assert_eq!(l.kit.config.total_distributed, 5);
    }
}

#[test]
fn the_remainder_is_exact() {
    let mut c = setup(modules::HOLDER_REWARDS).config;
    c.eligible = 3_000_000_000_000;
    // 1 lamport over 3e12 units: 1e12 / 3e12 is 0, the whole scaled lamport is kept.
    c.sync(1, NOW).unwrap();
    assert_eq!((c.acc_per_share, c.rem), (0, 1_000_000_000_000));
    c.sync(3, NOW).unwrap();
    assert_eq!((c.acc_per_share, c.rem), (1, 0));
    c.sync(4, NOW).unwrap();
    assert_eq!((c.acc_per_share, c.rem), (1, 1_000_000_000_000));
    assert_eq!((c.seen, c.total_distributed, c.held), (4, 4, 0));
}

#[test]
fn below_min_eligible_holds_then_divides() {
    let mut c = setup(modules::HOLDER_REWARDS).config;
    c.eligible = c.min_eligible - 1;
    c.sync(500, NOW).unwrap();
    assert_eq!((c.held, c.acc_per_share, c.total_distributed), (500, 0, 0));
    c.eligible = 0;
    c.sync(700, NOW).unwrap();
    assert_eq!(c.held, 700);
    c.eligible = c.min_eligible;
    c.sync(1_000, NOW).unwrap();
    assert_eq!((c.held, c.total_distributed, c.seen), (0, 1_000, 1_000));
    assert_eq!(c.acc_per_share, 1_000 * SCALE / u128::from(c.min_eligible));
}

#[test]
fn the_stream_releases_linearly() {
    assert_eq!(stream_release(0, NOW, NOW + 10, NOW + 5), Some(0));
    assert_eq!(stream_release(3_600, NOW, NOW + 3_600, NOW + 1), Some(1));
    assert_eq!(stream_release(1_000, NOW, NOW + 3_600, NOW + 1), Some(0));
    assert_eq!(
        stream_release(1_000, NOW, NOW + 3_600, NOW + 3_600),
        Some(1_000)
    );
    assert_eq!(stream_release(1_000, NOW, NOW + 3_600, NOW - 5), Some(0));
    // Released step by step, the floors only delay: everything is out at the end.
    let mut c = setup(modules::HOLDER_REWARDS).config;
    c.eligible = c.min_eligible;
    c.stream_remaining = 1_000_003;
    c.stream_last = NOW;
    c.stream_end = NOW + SHARE_STREAM_SECS;
    c.seen = 1_000_003;
    let mut released = 0;
    let mut t = NOW;
    while c.stream_remaining > 0 {
        t += 7;
        let before = c.stream_remaining;
        c.sync(1_000_003, t).unwrap();
        let step = before - c.stream_remaining;
        // Never more than the linear schedule allows.
        released += step;
        let elapsed = (t - NOW).min(SHARE_STREAM_SECS) as u128;
        assert!(u128::from(released) <= 1_000_003 * elapsed / SHARE_STREAM_SECS as u128 + 1);
    }
    assert_eq!(released, 1_000_003);
    assert_eq!(c.total_distributed, 1_000_003);
    assert!(t >= NOW + SHARE_STREAM_SECS);
}

#[test]
fn the_stream_pauses_while_nobody_is_eligible() {
    let mut c = setup(modules::HOLDER_REWARDS).config;
    c.eligible = c.min_eligible;
    c.add_share(3_600_000, NOW).unwrap();
    assert_eq!(
        (c.stream_last, c.stream_end),
        (NOW, NOW + SHARE_STREAM_SECS)
    );
    c.sync(3_600_000, NOW + 600).unwrap();
    assert_eq!(
        (c.stream_remaining, c.total_distributed),
        (3_000_000, 600_000)
    );
    // Below min_eligible: ten minutes, then an hour, release nothing; the end moves on with the
    // clock, so the stream keeps its rate and the time it had left.
    c.eligible = c.min_eligible - 1;
    c.sync(3_600_000, NOW + 1_200).unwrap();
    c.sync(3_600_000, NOW + 4_800).unwrap();
    assert_eq!(c.stream_remaining, 3_000_000);
    assert_eq!((c.held, c.total_distributed), (0, 600_000));
    assert_eq!(
        (c.stream_last, c.stream_end),
        (NOW + 4_800, NOW + 4_800 + 3_000)
    );
    // The mirror agrees: nothing released while paused.
    assert_eq!(mirror::released_at(&c, NOW + 9_000), Some(0));
    // Eligible again: the clock that passed while paused releases nothing (the sync that sees
    // holders back runs at the same moment), then the stream resumes at its rate.
    c.eligible = c.min_eligible;
    c.sync(3_600_000, NOW + 4_800).unwrap();
    assert_eq!(c.stream_remaining, 3_000_000);
    assert_eq!(mirror::released_at(&c, NOW + 5_400), Some(600_000));
    c.sync(3_600_000, NOW + 5_400).unwrap();
    assert_eq!(
        (c.stream_remaining, c.total_distributed),
        (2_400_000, 1_200_000)
    );
    c.sync(3_600_000, NOW + 4_800 + 3_000).unwrap();
    assert_eq!((c.stream_remaining, c.total_distributed), (0, 3_600_000));
}

#[test]
fn a_newcomer_cannot_take_a_paused_share_in_one_transaction() {
    // The review's sequence (§4.10's promise): two holders above the threshold and a small one
    // below it on its own; 10 SOL shared; the two sell out after ten minutes; the small holder
    // holds alone, below the threshold, for the rest of the hour. A newcomer then buys past the
    // threshold, claims and sells back at the same moment: before the fix the claim took what the
    // stream had released into `held` while nobody was eligible.
    let mut l = Ledger::new(modules::HOLDER_REWARDS);
    let c = l.kit.config.clone();
    let (a, b, small, newcomer) = (wallet(), wallet(), wallet(), wallet());
    let big = 2 * c.min_eligible;
    l.send(c.pool, a, big).unwrap();
    l.send(c.pool, b, big).unwrap();
    l.send(c.pool, small, c.min_eligible / 3).unwrap();
    let shared = 10_000_000_000u64;
    l.share(shared).unwrap();
    l.now += 600;
    l.send(a, c.pool, big).unwrap();
    l.send(b, c.pool, big).unwrap();
    let paid = l.claim(a) + l.claim(b);
    assert!(paid > 0);
    l.check();
    assert!(l.kit.config.eligible < l.kit.config.min_eligible);
    l.now += 3_000;
    let small_before = mirror::claimable(
        &l.kit.config,
        l.vault,
        &small,
        l.get(&small).0,
        &l.get(&small).1,
        l.now,
    )
    .unwrap();
    // Buy, claim, sell, all at one moment.
    l.send(c.pool, newcomer, c.min_eligible).unwrap();
    assert_eq!(l.claim(newcomer), 0, "nothing reached the newcomer");
    l.send(newcomer, c.pool, c.min_eligible).unwrap();
    // The stream waited for holders: what the two did not hold through is still streaming.
    let c2 = &l.kit.config;
    assert_eq!(c2.held, 0);
    assert!(c2.stream_remaining >= shared / 6 * 5 - 1);
    let small_after = mirror::claimable(
        &l.kit.config,
        l.vault,
        &small,
        l.get(&small).0,
        &l.get(&small).1,
        l.now,
    )
    .unwrap();
    assert_eq!(small_after, small_before);
    // A newcomer who holds for ten minutes gets its share of those ten minutes only.
    l.send(c.pool, newcomer, c.min_eligible).unwrap();
    l.now += 600;
    let stream = l.kit.config.stream_remaining;
    let got = l.claim(newcomer);
    let ten_minutes = stream / 5; // 600 of the 3,000 seconds the stream had left
    let fair = (u128::from(ten_minutes) * u128::from(c.min_eligible)
        / u128::from(c.min_eligible + c.min_eligible / 3)) as u64;
    // The scaled remainder kept from the first division (below the 4.3e12 eligible then, so up
    // to three lamports over today's 1.3e12) can add a few lamports; floors can take two.
    assert!(got <= fair + 4 && got + 2 >= fair, "got {got}, fair {fair}");
    l.check();
}

/// A share at `now` as `share` makes it: the sync, the lamports into the vault, then the stream.
fn share_at(c: &mut KitConfig, vault: &mut u64, amount: u64, now: i64) {
    c.sync(*vault, now).unwrap();
    *vault += amount;
    c.add_share(amount, now).unwrap();
}

/// The stream: what runs, what waits for it, the last release and the running stream's end.
fn stream(c: &KitConfig) -> (u64, u64, i64, i64) {
    (
        c.stream_remaining,
        c.stream_next,
        c.stream_last,
        c.stream_end,
    )
}

#[test]
fn a_share_waits_for_the_running_stream_then_streams_its_own_hour() {
    let mut c = setup(modules::HOLDER_REWARDS).config;
    c.eligible = c.min_eligible;
    let mut vault = 0u64;
    let hour = SHARE_STREAM_SECS;
    // Nothing streams: a share streams over the next hour.
    share_at(&mut c, &mut vault, 3_600_000, NOW);
    assert_eq!(stream(&c), (3_600_000, 0, NOW, NOW + hour));
    // Another in the same second joins it: the same hour, each at its own rate.
    share_at(&mut c, &mut vault, 1_800_000, NOW);
    assert_eq!(stream(&c), (5_400_000, 0, NOW, NOW + hour));
    // Half an hour on, a share waits for the running stream: the stream's end does not move, and
    // the share does not take on its rate.
    share_at(&mut c, &mut vault, 7_200_000, NOW + 1_800);
    assert_eq!(stream(&c), (2_700_000, 7_200_000, NOW + 1_800, NOW + hour));
    // A later one waits with it, for the same hour.
    share_at(&mut c, &mut vault, 3_600_000, NOW + 3_000);
    assert_eq!(stream(&c), (900_000, 10_800_000, NOW + 3_000, NOW + hour));
    assert_eq!(c.total_distributed, 4_500_000);
    // The running stream is out at its end; the waiting shares stream over the hour after it.
    assert_eq!(
        mirror::stream_at(&c, NOW + hour - 1),
        Some((898_500, 1_500, 10_800_000))
    );
    assert_eq!(
        mirror::stream_at(&c, NOW + hour),
        Some((900_000, 10_800_000, 0))
    );
    assert_eq!(
        mirror::stream_at(&c, NOW + hour + 360),
        Some((1_980_000, 9_720_000, 0))
    );
    c.sync(vault, NOW + hour + 360).unwrap();
    assert_eq!(c.total_distributed, 4_500_000 + 1_980_000);
    assert_eq!(stream(&c), (9_720_000, 0, NOW + hour + 360, NOW + 2 * hour));
    // A share now waits for that hour and streams over the one after it.
    share_at(&mut c, &mut vault, 360_000, NOW + hour + 400);
    assert_eq!(
        stream(&c),
        (9_600_000, 360_000, NOW + hour + 400, NOW + 2 * hour)
    );
    c.sync(vault, NOW + 2 * hour + 1_800).unwrap();
    assert_eq!(
        stream(&c),
        (180_000, 0, NOW + 2 * hour + 1_800, NOW + 3 * hour)
    );
    c.sync(vault, NOW + 3 * hour).unwrap();
    assert_eq!((c.stream_remaining, c.stream_next), (0, 0));
    assert_eq!(
        (c.total_distributed, c.total_shared, c.seen),
        (vault, vault, vault)
    );
    // A share without the sync at its time would wait behind a stream that may already be over,
    // and be released at once: refused, as a bug.
    let mut d = setup(modules::HOLDER_REWARDS).config;
    d.eligible = d.min_eligible;
    d.add_share(3_600_000, NOW).unwrap();
    assert_eq!(
        code(d.add_share(1_000_000, NOW + 10)),
        kit(KitError::MathOverflow)
    );
    d.sync(3_600_000, NOW + 10).unwrap();
    d.add_share(1_000_000, NOW + 10).unwrap();
    assert_eq!((d.stream_next, d.stream_end), (1_000_000, NOW + hour));
}

#[test]
fn a_waiting_share_waits_through_a_pause() {
    let mut c = setup(modules::HOLDER_REWARDS).config;
    c.eligible = c.min_eligible;
    let mut vault = 0u64;
    let hour = SHARE_STREAM_SECS;
    share_at(&mut c, &mut vault, 3_600_000, NOW);
    share_at(&mut c, &mut vault, 1_800_000, NOW + 600);
    assert_eq!(stream(&c), (3_000_000, 1_800_000, NOW + 600, NOW + hour));
    // Nobody eligible for an hour and a half: nothing is released, and the running stream's end,
    // and with it the waiting share's hour, moves on with the clock.
    c.eligible = c.min_eligible - 1;
    c.sync(vault, NOW + 1_200).unwrap();
    c.sync(vault, NOW + 6_000).unwrap();
    assert_eq!(stream(&c), (3_000_000, 1_800_000, NOW + 6_000, NOW + 9_000));
    assert_eq!(
        mirror::stream_at(&c, NOW + 50_000),
        Some((0, 3_000_000, 1_800_000))
    );
    assert_eq!((c.total_distributed, c.held), (600_000, 0));
    // Eligible again: the running stream's 3,000 s, then the waiting share's hour.
    c.eligible = c.min_eligible;
    c.sync(vault, NOW + 6_000).unwrap();
    let end = NOW + 9_000;
    assert_eq!(mirror::stream_at(&c, end), Some((3_000_000, 1_800_000, 0)));
    assert_eq!(
        mirror::stream_at(&c, end + 1_800),
        Some((3_900_000, 900_000, 0))
    );
    c.sync(vault, end + 1_800).unwrap();
    assert_eq!(stream(&c), (900_000, 0, end + 1_800, end + hour));
    // Paused again half way through that hour: it keeps the half it had left.
    c.eligible = 0;
    c.sync(vault, end + 10_000).unwrap();
    assert_eq!(stream(&c), (900_000, 0, end + 10_000, end + 11_800));
    c.eligible = c.min_eligible;
    c.sync(vault, end + 10_000).unwrap();
    c.sync(vault, end + 11_800).unwrap();
    assert_eq!(
        (c.stream_remaining, c.total_distributed, c.held),
        (0, 5_400_000, 0)
    );
}

/// fix1's `add_share`: a share merged into the running stream, released at the sum of the two
/// rates until the stream was empty (`math::merged_duration`, now gone). Kept to show that the
/// bound in `a_share_made_as_another_ends_keeps_its_own_rate` catches what the review found.
fn merged_add_share(c: &mut KitConfig, received: u64, now: i64) {
    let start = c.stream_last.max(now);
    let left = c.stream_end - start;
    let duration = if c.stream_remaining == 0 || left <= 0 {
        SHARE_STREAM_SECS
    } else {
        let (r, x) = (u128::from(c.stream_remaining), u128::from(received));
        let (d, s) = (left as u128, SHARE_STREAM_SECS as u128);
        ((r + x) * d * s / (r * s + x * d)) as i64
    };
    c.seen += received;
    c.stream_remaining += received;
    c.stream_last = start;
    c.stream_end = start + duration;
    c.total_shared += received;
}

#[test]
fn a_share_made_as_another_ends_keeps_its_own_rate() {
    // The review of fix1: 36 SOL shared, then 1 SOL one second before the 36 SOL's hour ends.
    // Merged into one stream at the sum of the two rates, the 1 SOL was out within 98 s, 37 times
    // its own rate, so a trader who read the stream's end, bought a second later and sold at it
    // kept about 0.4 SOL after every fee. Now it waits for the 36 SOL and streams its own hour.
    const SOL: u64 = 1_000_000_000;
    let (big, small) = (36 * SOL, SOL);
    let hour = SHARE_STREAM_SECS;
    let at = NOW + hour - 1;
    // The two shares (`merge`: the second added as fix1 did), and what was distributed by then.
    let run = |merge: bool| -> (KitConfig, u64, u64) {
        let mut c = setup(modules::HOLDER_REWARDS).config;
        c.eligible = c.min_eligible;
        let mut vault = 0u64;
        share_at(&mut c, &mut vault, big, NOW);
        c.sync(vault, at).unwrap();
        vault += small;
        if merge {
            merged_add_share(&mut c, small, at);
        } else {
            c.add_share(small, at).unwrap();
        }
        let distributed = c.total_distributed;
        (c, vault, distributed)
    };
    let released_after = |c: &KitConfig, vault: u64, from: u64, secs: i64| {
        let mut probe = c.clone();
        probe.sync(vault, at + secs).unwrap();
        probe.total_distributed - from
    };
    let (c, vault, d0) = run(false);
    // One second of the 36 SOL is left; the 1 SOL waits for it.
    let tail = c.stream_remaining;
    assert_eq!(tail, big / hour as u64);
    assert_eq!(stream(&c), (tail, small, at, NOW + hour));
    // The review's bound, times an hour: in the next L seconds at most the 36 SOL's tail and L
    // seconds of the 1 SOL at its own rate.
    let bound = |secs: i64| u128::from(tail) * hour as u128 + u128::from(small) * secs as u128;
    for secs in [
        1i64, 2, 10, 60, 98, 155, 600, 1_800, 3_599, 3_600, 3_601, 7_200,
    ] {
        let released = released_after(&c, vault, d0, secs);
        assert!(
            u128::from(released) * hour as u128 <= bound(secs),
            "{secs} s: {released}"
        );
        // Exactly the two hours: the tail by its end, then the 1 SOL from it, rounded down.
        let own = (u128::from(small) * (secs - 1).clamp(0, hour) as u128 / hour as u128) as u64;
        assert_eq!(released, tail + own, "{secs} s");
    }
    // Synced every second: never above the bound, and all out an hour after the 36 SOL's end.
    let mut stepped = c.clone();
    for secs in 1..=hour + 1 {
        stepped.sync(vault, at + secs).unwrap();
        let released = stepped.total_distributed - d0;
        assert!(
            u128::from(released) * hour as u128 <= bound(secs),
            "{secs} s"
        );
    }
    assert_eq!(stepped.total_distributed, big + small);
    assert_eq!((stepped.stream_remaining, stepped.stream_next), (0, 0));
    // fix1's merge breaks the bound: the stream ends 98 s after the 1 SOL, and all of it is out.
    let (m, vault, d0) = run(true);
    assert_eq!(m.stream_end, at + 98);
    let released = released_after(&m, vault, d0, 98);
    assert_eq!(released, tail + small);
    assert!(u128::from(released) * hour as u128 > bound(98));
}

/// Shares at random times (several in one second among them) and of random sizes, the stream
/// synced at random moments while holders stay eligible, held to a schedule of hours written
/// apart from the program: a share made while nothing streams streams over the hour from it; one
/// made in the second an hour starts, or while the next hour is still ahead, joins that hour; one
/// made while an hour runs starts the hour after it. Each share is released linearly over its
/// hour, at its own rate. After every sync the stream has released no more than those hours have
/// by then (never ahead of them; the floors only delay it, by under a lamport a sync), every hour
/// that is over is out, every share starts within the hour, the program's state names the same
/// hours, and every shared lamport is released, running or waiting.
#[test]
fn every_share_streams_its_own_hour() {
    let hour = SHARE_STREAM_SECS;
    let secs = hour as u128;
    for seed in 1..=6u64 {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n.max(1)
        };
        let mut c = setup(modules::HOLDER_REWARDS).config;
        c.eligible = c.min_eligible;
        let mut now = NOW;
        let mut vault = 0u64;
        // The schedule: each hour's start and the lamports shared for it, in order.
        let mut hours: Vec<(i64, u64)> = Vec::new();
        let mut released = 0u64;
        let mut syncs = 0u64;
        let (mut started, mut waited) = (0u32, 0u32);
        for _ in 0..2_000 {
            let long = next(4) == 0;
            now += next(if long { 900 } else { 120 }) as i64;
            let before = c.total_distributed;
            c.sync(vault, now).unwrap();
            syncs += 1;
            released += c.total_distributed - before;
            // What the hours have released by now, times an hour.
            let due: u128 = hours
                .iter()
                .map(|(start, amount)| u128::from(*amount) * (now - start).clamp(0, hour) as u128)
                .sum();
            assert!(
                u128::from(released) * secs <= due,
                "seed {seed}: {released} released at {now}, ahead of the shares' hours"
            );
            assert!(
                u128::from(released + syncs) * secs >= due,
                "seed {seed}: {released} released at {now}, behind the shares' hours"
            );
            let over: u64 = hours
                .iter()
                .filter(|(start, _)| start + hour <= now)
                .map(|(_, amount)| amount)
                .sum();
            assert!(
                released >= over,
                "seed {seed}: an hour over at {now} is not out"
            );
            if next(3) == 0 {
                let amount = if next(4) == 0 {
                    MIN_SHARE_LAMPORTS
                } else {
                    MIN_SHARE_LAMPORTS + next(20_000_000_000)
                };
                vault += amount;
                c.add_share(amount, now).unwrap();
                let start = match hours.last() {
                    Some(&(last, _)) if now < last + hour => {
                        if now <= last {
                            last
                        } else {
                            last + hour
                        }
                    }
                    _ => now,
                };
                assert!(start - now <= hour, "a share starts within the hour");
                match hours.last_mut() {
                    Some(last) if last.0 == start => last.1 += amount,
                    _ => hours.push((start, amount)),
                }
                // The program names the same hour: the running stream's, from now, or the one
                // waiting at its end.
                if start == now {
                    started += 1;
                    assert_eq!((c.stream_end, c.stream_next), (now + hour, 0));
                } else {
                    waited += 1;
                    assert_eq!(
                        (c.stream_end, c.stream_next),
                        (start, hours.last().unwrap().1)
                    );
                }
            }
            let total: u64 = hours.iter().map(|(_, amount)| amount).sum();
            assert_eq!(released + c.stream_remaining + c.stream_next, total);
            assert_eq!(c.seen, total);
        }
        // Everything is out by the end of the last hour.
        let end = hours.last().map_or(now, |(start, _)| start + hour).max(now);
        let before = c.total_distributed;
        c.sync(vault, end).unwrap();
        released += c.total_distributed - before;
        assert_eq!((released, c.stream_remaining, c.stream_next), (vault, 0, 0));
        assert!(started > 0 && waited > 0);
    }
}

/// The mirror (§4.13), which the TypeScript one follows, against the program's own sync on random
/// states: a stream running, over, empty, with shares waiting for it or not, paused or not, with
/// donations and held lamports.
#[test]
fn the_mirror_matches_the_sync() {
    let mut rng = 0x5DEE_CE66_D1CE_4E5Bu64;
    let mut next = |n: u64| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng % n.max(1)
    };
    let base = setup(modules::HOLDER_REWARDS).config;
    let mut started_waiting = 0;
    for _ in 0..20_000 {
        let mut c = base.clone();
        c.eligible = match next(4) {
            0 => 0,
            1 => c.min_eligible - 1 - next(1_000),
            _ => c.min_eligible + next(1 << 50),
        };
        c.acc_per_share = u128::from(next(1 << 40));
        c.rem = u128::from(next(c.eligible.max(1)));
        c.held = next(1 << 30);
        c.stream_last = NOW + next(1_000) as i64;
        c.stream_end = c.stream_last + 1 + next(SHARE_STREAM_SECS as u64) as i64;
        c.stream_remaining = if next(6) == 0 { 0 } else { 1 + next(1 << 40) };
        c.stream_next = if next(3) == 0 { 0 } else { next(1 << 40) };
        c.total_claimed = next(1 << 40);
        c.seen = next(1 << 41);
        c.total_distributed = next(1 << 42);
        let vault = c.seen.saturating_sub(c.total_claimed) + next(1 << 30);
        let now = c.stream_last - 100 + next(2 * SHARE_STREAM_SECS as u64 + 4_000) as i64;
        let m = mirror::synced(&c, vault, now).unwrap();
        let mut s = c.clone();
        s.sync(vault, now).unwrap();
        assert_eq!(
            (
                m.acc_per_share,
                m.held,
                m.stream_remaining,
                m.stream_next,
                m.distributed
            ),
            (
                s.acc_per_share,
                s.held,
                s.stream_remaining,
                s.stream_next,
                s.total_distributed
            ),
            "{c:?} at {now}"
        );
        assert_eq!(
            m.released,
            c.stream_remaining + c.stream_next - s.stream_remaining - s.stream_next
        );
        if c.stream_next > 0 && s.stream_next == 0 {
            started_waiting += 1;
        }
    }
    assert!(started_waiting > 1_000);
}

#[test]
fn settle_rounds_down_and_forgets_an_empty_holding() {
    let mut d = HolderData {
        snapshot: 10,
        owed: 7,
        early_locked: 3,
    };
    settle(&mut d, 1_500_000_000_000, 1, 11).unwrap();
    assert_eq!((d.owed, d.snapshot, d.early_locked), (8, 11, 3));
    settle(&mut d, 5, 0, 1_000_000_000_011).unwrap();
    assert_eq!((d.owed, d.snapshot), (13, 0));
    // A snapshot above the accumulator is impossible: a bug, refused.
    let mut d = HolderData {
        snapshot: 5,
        ..HolderData::default()
    };
    assert_eq!(code(settle(&mut d, 1, 1, 4)), kit(KitError::MathOverflow));
}

#[test]
fn rules_refuse_the_wrong_operation() {
    let mut l = Ledger::new(modules::ALL);
    let h = wallet();
    let c = l.kit.config.clone();
    let mut args = transfer(&c, c.pool, h, 1, 10, 0, [0; 64], [0; 64]);
    args.op = TokenOp::Mint;
    assert_eq!(
        code(rules::before_transfer(
            &mut l.kit.config,
            &l.kit.key,
            &args,
            Some(0),
            NOW
        )),
        kit(KitError::UnsupportedOperation)
    );
    args.op = TokenOp::Transfer;
    args.phase = Phase::After;
    assert_eq!(
        code(rules::before_transfer(
            &mut l.kit.config,
            &l.kit.key,
            &args,
            Some(0),
            NOW
        )),
        kit(KitError::UnsupportedOperation)
    );
    let args = transfer(&c, c.pool, h, 1, 10, 0, [0; 64], [0; 64]);
    assert_eq!(
        code(rules::before_burn(&mut l.kit.config, &args, Some(0), NOW)),
        kit(KitError::UnsupportedOperation)
    );
    // Holder rewards without the vault's balance.
    assert_eq!(
        code(rules::before_transfer(
            &mut l.kit.config,
            &l.kit.key,
            &args,
            None,
            NOW
        )),
        kit(KitError::MissingRewardVault)
    );
}

#[test]
fn destinations_are_checked_first() {
    let mut l = Ledger::new(modules::ALL);
    let c = l.kit.config.clone();
    let creator = c.creator;
    l.send(c.pool, creator, 1_000).unwrap();
    // The creator is locked, but a refused destination is refused first.
    for to in [
        c.launch,
        l.kit.key,
        Pubkey::default(),
        TOKEN_ID,
        SWAP_ID,
        BRIDGE_ID,
        LAUNCH_ID,
        crate::ID,
        pda(),
    ] {
        assert_eq!(
            code(l.send(creator, to, 1)),
            kit(KitError::DestinationNotAllowed),
            "{to}"
        );
    }
    assert_eq!(
        code(l.send(creator, wallet(), 1)),
        kit(KitError::CreatorLocked)
    );
    // The pool is excluded: an off-curve owner the token may reach (a sell).
    l.now = c.creator_unlock_at;
    l.send(creator, c.pool, 1).unwrap();
    // Without holder rewards a PDA may hold the token; the refused list still applies.
    let mut l = Ledger::new(modules::MAX_WALLET);
    let c = l.kit.config.clone();
    let somewhere = pda();
    l.send(c.pool, somewhere, 10).unwrap();
    assert_eq!(
        code(l.send(somewhere, Pubkey::default(), 1)),
        kit(KitError::DestinationNotAllowed)
    );
    assert_eq!(
        code(l.send(somewhere, c.launch, 1)),
        kit(KitError::DestinationNotAllowed)
    );
}

#[test]
fn the_creator_lock_holds_whoever_signs_and_never_applies_to_burns() {
    let mut l = Ledger::new(modules::CREATOR_WALLET_LOCK | modules::EARLY_BUYER_LOCK);
    let c = l.kit.config.clone();
    l.now = c.early_window_end;
    l.send(c.pool, c.creator, 1_000).unwrap();
    let (b, d) = l.get(&c.creator);
    let mut args = transfer(&c, c.creator, wallet(), 10, b, 0, d, [0; 64]);
    args.authority = wallet();
    args.authority_is_delegate = true;
    assert_eq!(
        code(rules::before_transfer(
            &mut l.kit.config,
            &l.kit.key,
            &args,
            None,
            l.now
        )),
        kit(KitError::CreatorLocked)
    );
    assert_eq!(
        code(l.send(c.creator, c.pool, 10)),
        kit(KitError::CreatorLocked)
    );
    // Receiving is fine; burning is fine.
    l.send(c.pool, c.creator, 5).unwrap();
    l.burn(c.creator, 5).unwrap();
    l.now = c.creator_unlock_at - 1;
    assert_eq!(
        code(l.send(c.creator, wallet(), 1)),
        kit(KitError::CreatorLocked)
    );
    l.now = c.creator_unlock_at;
    l.send(c.creator, wallet(), 1).unwrap();
}

#[test]
fn the_early_lock_keeps_what_was_bought_in_the_window() {
    let mut l = Ledger::new(modules::EARLY_BUYER_LOCK);
    let c = l.kit.config.clone();
    let (e, x, f, g) = (wallet(), wallet(), wallet(), wallet());
    // Bought in the window: locked (the pool's own data is never written).
    let ret = l.send(c.pool, e, 600).unwrap();
    assert_eq!(
        HolderData::read(&ret.destination_hook_data.unwrap()).early_locked,
        600
    );
    assert!(ret.source_hook_data.is_none());
    l.send(c.pool, x, 30).unwrap();
    assert_eq!(code(l.send(x, f, 1)), kit(KitError::EarlyLocked));
    // Bought after the window: free, and free to pass on.
    l.now = c.early_window_end;
    l.send(c.pool, f, 500).unwrap();
    l.send(c.pool, g, 50).unwrap();
    assert_eq!((l.get(&f).1, l.get(&g).1), ([0; 64], [0; 64]));
    l.send(f, e, 100).unwrap();
    // e holds 700, 600 locked: 100 can move, 101 cannot; nor can a burn or a sell take more.
    assert_eq!(code(l.send(e, g, 101)), kit(KitError::EarlyLocked));
    assert_eq!(code(l.send(e, c.pool, 101)), kit(KitError::EarlyLocked));
    assert_eq!(code(l.burn(e, 101)), kit(KitError::EarlyLocked));
    l.send(e, g, 60).unwrap();
    l.burn(e, 40).unwrap();
    assert_eq!(code(l.send(e, g, 1)), kit(KitError::EarlyLocked));
    assert_eq!(HolderData::read(&l.get(&e).1).early_locked, 600);
    // After the unlock everything moves, and each side the transfer touches is cleared.
    l.now = c.early_unlock_at;
    let ret = l.send(f, e, 1).unwrap();
    assert_eq!(ret.source_hook_data, None);
    assert_eq!(ret.destination_hook_data, Some([0; 64]));
    let ret = l.send(x, g, 30).unwrap();
    assert_eq!(ret.source_hook_data, Some([0; 64]));
    assert_eq!(l.get(&e), (601, [0; 64]));
    l.send(e, f, 601).unwrap();
    let ret = l.burn(g, 1).unwrap();
    assert_eq!(ret.source_hook_data, None);
    // A buy after the unlock locks nothing.
    l.send(c.pool, e, 5).unwrap();
    assert_eq!(l.get(&e).1, [0; 64]);
}

#[test]
fn max_wallet_caps_holders_until_graduation() {
    let mut l = Ledger::new(modules::MAX_WALLET);
    let c = l.kit.config.clone();
    assert_eq!(c.max_wallet_amount, SUPPLY / 50);
    let m = wallet();
    l.send(c.pool, m, c.max_wallet_amount - 1).unwrap();
    l.send(c.pool, m, 1).unwrap();
    assert_eq!(code(l.send(c.pool, m, 1)), kit(KitError::MaxWalletExceeded));
    let n = wallet();
    l.send(c.pool, n, 5).unwrap();
    assert_eq!(code(l.send(n, m, 1)), kit(KitError::MaxWalletExceeded));
    // The pool is never capped: a sell lands whatever the pool holds.
    l.send(m, c.pool, c.max_wallet_amount).unwrap();
    l.send(c.launch, c.pool, SUPPLY / 4).unwrap();
    l.kit.config.graduated = true;
    l.send(c.pool, m, c.max_wallet_amount * 3).unwrap();
}

#[test]
fn eligible_follows_holders_and_only_holders() {
    let mut l = Ledger::new(modules::HOLDER_REWARDS | modules::EARLY_BUYER_LOCK);
    let c = l.kit.config.clone();
    l.now = c.early_unlock_at;
    assert_eq!(l.kit.config.eligible, 0);
    let (a, b) = (wallet(), wallet());
    l.send(c.pool, a, 1_000).unwrap();
    assert_eq!(l.kit.config.eligible, 1_000);
    l.send(a, b, 300).unwrap();
    assert_eq!(l.kit.config.eligible, 1_000);
    l.send(b, c.pool, 100).unwrap();
    assert_eq!(l.kit.config.eligible, 900);
    l.burn(a, 200).unwrap();
    assert_eq!(l.kit.config.eligible, 700);
    l.burn(c.pool, 200).unwrap();
    l.send(c.launch, c.pool, 200).unwrap();
    assert_eq!(l.kit.config.eligible, 700);
    l.check();
}

#[test]
fn hook_data_is_answered_only_when_it_changes() {
    // Max wallet and the creator lock keep nothing per holder.
    let mut l = Ledger::new(modules::MAX_WALLET | modules::CREATOR_WALLET_LOCK);
    let c = l.kit.config.clone();
    let a = wallet();
    let ret = l.send(c.pool, a, 10).unwrap();
    assert_eq!(ret, HookReturn::default());
    // Holder rewards: a holder's snapshot is written when it holds, zero when it holds nothing.
    let mut l = Ledger::new(modules::HOLDER_REWARDS);
    let c = l.kit.config.clone();
    let (a, b) = (wallet(), wallet());
    l.send(c.pool, a, 2_000_000_000_000).unwrap();
    l.vault += 1_000;
    let ret = l.send(a, b, 1_000_000_000_000).unwrap();
    let da = HolderData::read(&ret.source_hook_data.unwrap());
    let db = HolderData::read(&ret.destination_hook_data.unwrap());
    assert_eq!((da.owed, da.snapshot), (1_000, l.kit.config.acc_per_share));
    assert_eq!((db.owed, db.snapshot), (0, l.kit.config.acc_per_share));
    let ret = l.send(a, c.pool, 1_000_000_000_000).unwrap();
    let da = HolderData::read(&ret.source_hook_data.unwrap());
    assert_eq!((da.owed, da.snapshot), (1_000, 0));
    assert!(ret.destination_hook_data.is_none());
    assert_eq!(l.claim(a), 1_000);
    assert_eq!(l.get(&a), (0, [0; 64]));
    l.check();
}

#[test]
fn wallets_only_with_holder_rewards() {
    let mut l = Ledger::new(modules::HOLDER_REWARDS);
    let c = l.kit.config.clone();
    let a = wallet();
    l.send(c.pool, a, 100).unwrap();
    assert_eq!(
        code(l.send(a, pda(), 1)),
        kit(KitError::DestinationNotAllowed)
    );
    assert_eq!(
        code(l.send(c.pool, pda(), 1)),
        kit(KitError::DestinationNotAllowed)
    );
    l.send(a, wallet(), 1).unwrap();
    l.send(a, c.pool, 1).unwrap();
}

#[test]
fn only_a_companion_creator_is_excluded() {
    let mut l = Ledger::new(modules::HOLDER_REWARDS | modules::MAX_WALLET);
    // A companion's creator address: it may hold, uncapped, outside `eligible`.
    let companion = pda();
    l.kit.config.creator = companion;
    l.kit.config.creator_is_companion = true;
    let c = l.kit.config.clone();
    assert!(c.is_excluded(&companion));
    let eligible = l.kit.config.eligible;
    l.send(c.pool, companion, c.max_wallet_amount + 1).unwrap();
    assert_eq!(l.kit.config.eligible, eligible);
    // What it releases to a wallet counts from then on.
    let dev = wallet();
    l.send(companion, dev, 10).unwrap();
    assert_eq!(l.kit.config.eligible, eligible + 10);
    // Any other creator, a program's address included, is not excluded: it can't hold, as before.
    let other = pda();
    l.kit.config.creator = other;
    l.kit.config.creator_is_companion = false;
    assert!(!l.kit.config.is_excluded(&other));
    assert_eq!(
        code(l.send(dev, other, 1)),
        kit(KitError::DestinationNotAllowed)
    );
}

/// A seeded walk through every operation on the pure rules, with the invariants after each step
/// and each claim paying what the mirror computed. The LiteSVM suite runs the same walk on chain.
#[test]
fn seeded_walk_keeps_the_vault_solvent() {
    for (seed, modules) in [(1u64, 1u8), (2, 1), (3, 9), (4, 15), (5, 1), (6, 9)] {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n.max(1)
        };
        let mut l = Ledger::new(modules);
        let c = l.kit.config.clone();
        let holders: Vec<Pubkey> = (0..6).map(|_| wallet()).collect();
        let mut inflows = 0u64;
        let mut claimed = 0u64;
        let mut below = 0;
        let mut max_dust = 0;
        for step in 0..3_000 {
            let thin = (step / 200) % 2 == 1;
            let h = holders[next(holders.len() as u64) as usize];
            match next(10) {
                0 | 1 => {
                    let pool = l.get(&c.pool).0;
                    let cap = if thin {
                        c.min_eligible / 8
                    } else {
                        c.min_eligible * 2
                    };
                    let amount = 1 + next(cap.min(pool));
                    let _ = l.send(c.pool, h, amount);
                }
                2 => {
                    let have = l.get(&h).0;
                    if have > 0 {
                        let amount = if thin { have } else { 1 + next(have) };
                        let _ = l.send(h, c.pool, amount);
                    }
                }
                3 => {
                    let have = l.get(&h).0;
                    let to = holders[next(holders.len() as u64) as usize];
                    if have > 0 && to != h {
                        let _ = l.send(h, to, 1 + next(have));
                    }
                }
                4 => {
                    let have = l.get(&h).0;
                    if have > 0 {
                        let _ = l.burn(h, 1 + next(have));
                    }
                }
                5 | 6 => {
                    let amount = if next(3) == 0 {
                        1 + next(3)
                    } else {
                        1 + next(10_000_000)
                    };
                    l.vault += amount;
                    inflows += amount;
                }
                7 => {
                    if c.rewards_on() {
                        claimed += l.claim(h);
                    }
                }
                8 => {
                    // A share: counted as seen, streamed at its own rate.
                    if c.rewards_on() && l.kit.config.divides() {
                        let amount = 1_000_000 + next(50_000_000);
                        l.share(amount).unwrap();
                        inflows += amount;
                    }
                }
                _ => l.now += next(900) as i64,
            }
            if l.kit.config.eligible < l.kit.config.min_eligible {
                below += 1;
            }
            max_dust = max_dust.max(l.check());
        }
        // Holders eligible (the stream only runs while they are), the stream out (the running
        // stream within an hour, then the hour of the shares waiting for it), then everyone
        // claims everything.
        let pool = l.get(&c.pool).0;
        let _ = l.send(c.pool, holders[0], c.min_eligible.min(pool));
        l.now += 2 * SHARE_STREAM_SECS;
        if c.rewards_on() {
            for h in &holders {
                claimed += l.claim(*h);
            }
            for h in &holders {
                let (b, d) = l.get(h);
                let left = mirror::claimable(&l.kit.config, l.vault, h, b, &d, l.now).unwrap();
                assert_eq!(left, 0);
            }
            if l.kit.config.divides() {
                let k = &l.kit.config;
                assert_eq!(
                    (k.stream_remaining, k.stream_next),
                    (0, 0),
                    "the stream is out"
                );
            }
        }
        let dust = l.check();
        assert_eq!(l.vault + claimed, inflows, "nothing is created or lost");
        if c.rewards_on() {
            // What stays behind is rounding: under a lamport per settle, plus the scaled
            // remainder (below the eligible supply over SCALE).
            let bound = 2 * 3_000 + SUPPLY / SCALE as u64 + 2;
            assert!(dust <= bound, "dust {dust} above {bound}");
            assert!(claimed > 0);
        }
        assert!(below > 0, "a stretch below min_eligible");
        println!(
            "seed {seed} modules {modules}: inflows {inflows} claimed {claimed} dust {dust} \
             max dust {max_dust} below-min steps {below}"
        );
    }
}
