//! Phase 2: the jackpot and streak headers, their rules, the launch view, and random walks that
//! check every promise the rules make against a plain model.

use super::jackpot::{jackpot_offsets, MARK_AT};
use super::launch::{launch_offsets, LAUNCH_DISCRIMINATOR};
use super::streak::{streak_offsets, streak_weight_for};
use super::*;

/// xorshift64*: deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
}

fn base_header(round_secs: u32) -> GameHeader {
    GameHeader::new(Pubkey::new_from_array([7; 32]), round_secs, 1_800_000_000)
}

/// A state account's data: a discriminator, the base header, a kind's header, then the hook's own.
fn state_with(base: &GameHeader, kind: &[u8]) -> Vec<u8> {
    let mut data = vec![0xee; DISCRIMINATOR_LEN];
    base.serialize(&mut data).unwrap();
    data.extend_from_slice(kind);
    data.extend_from_slice(&[0xab; 24]);
    data
}

#[test]
fn the_kind_headers_sit_after_the_base_header_at_the_documented_offsets() {
    // Jackpot.
    let j = JackpotHeader {
        magic: JACKPOT_MAGIC,
        timer_secs: 0x0102_0304,
        min_tokens: 0x1112_1314_1516_1718,
        buys: 0x2122_2324_2526_2728,
        ended_buyer: Pubkey::new_from_array([5; 32]),
        ended_amount: 0x3132_3334_3536_3738,
        ended_at: -9,
        ended_buys: 0x4142_4344_4546_4748,
        earlier: core::array::from_fn(|i| EndedRound {
            buyer: Pubkey::new_from_array([0x60 + i as u8; 32]),
            amount: 0x5100 + i as u64,
            at: -(i as i64) - 100,
            number: 0x7100 + i as u64,
        }),
    };
    let mut borsh = Vec::new();
    j.serialize(&mut borsh).unwrap();
    assert_eq!(
        borsh,
        j.encode().to_vec(),
        "Borsh lays it out as encode does"
    );
    assert_eq!(JackpotHeader::INIT_SPACE, jackpot::JACKPOT_HEADER_LEN);
    let data = state_with(&base_header(0), &borsh);
    use jackpot_offsets as o;
    assert_eq!(o::MAGIC, header_offsets::END);
    assert_eq!(o::END, o::MAGIC + jackpot::JACKPOT_HEADER_LEN);
    assert_eq!(&data[o::MAGIC..o::MAGIC + 4], b"BRJ1");
    assert_eq!(
        &data[o::TIMER_SECS..o::TIMER_SECS + 4],
        &0x0102_0304u32.to_le_bytes()
    );
    assert_eq!(
        &data[o::MIN_TOKENS..o::MIN_TOKENS + 8],
        &j.min_tokens.to_le_bytes()
    );
    assert_eq!(&data[o::BUYS..o::BUYS + 8], &j.buys.to_le_bytes());
    assert_eq!(&data[o::ENDED_BUYER..o::ENDED_BUYER + 32], &[5; 32]);
    assert_eq!(
        &data[o::ENDED_AMOUNT..o::ENDED_AMOUNT + 8],
        &j.ended_amount.to_le_bytes()
    );
    assert_eq!(&data[o::ENDED_AT..o::ENDED_AT + 8], &(-9i64).to_le_bytes());
    assert_eq!(
        &data[o::ENDED_BUYS..o::ENDED_BUYS + 8],
        &j.ended_buys.to_le_bytes()
    );
    // The earlier ended rounds, additively after the first 80 bytes: 56 bytes each.
    assert_eq!(o::EARLIER, o::ENDED_BUYS + 8);
    assert_eq!(
        jackpot::JACKPOT_HEADER_LEN,
        80 + 7 * jackpot::ENDED_ROUND_LEN
    );
    assert_eq!(EndedRound::INIT_SPACE, jackpot::ENDED_ROUND_LEN);
    for (i, r) in j.earlier.iter().enumerate() {
        let at = o::EARLIER + i * jackpot::ENDED_ROUND_LEN;
        assert_eq!(&data[at..at + 32], &[0x60 + i as u8; 32]);
        assert_eq!(&data[at + 32..at + 40], &r.amount.to_le_bytes());
        assert_eq!(&data[at + 40..at + 48], &r.at.to_le_bytes());
        assert_eq!(&data[at + 48..at + 56], &r.number.to_le_bytes());
        assert_eq!(j.ended(i + 1), Some(*r));
    }
    assert_eq!(j.ended(0).unwrap().number, j.ended_buys);
    assert_eq!(j.ended(JACKPOT_ENDED_ROUNDS), None);
    assert_eq!(JackpotHeader::parse(&data), Some(j));
    // The base header still reads, at its own offsets.
    assert_eq!(GameHeader::parse(&data).unwrap().round_secs, 0);

    // Streak.
    let s = StreakHeader::new(0x0a0b_0c0d, 0x1a1b_1c1d_1e1f_2021);
    let mut borsh = Vec::new();
    s.serialize(&mut borsh).unwrap();
    assert_eq!(borsh, s.encode().to_vec());
    assert_eq!(StreakHeader::INIT_SPACE, streak::STREAK_HEADER_LEN);
    let data = state_with(&base_header(86_400), &borsh);
    use streak_offsets as so;
    assert_eq!(so::MAGIC, header_offsets::END);
    assert_eq!(&data[so::MAGIC..so::MAGIC + 4], b"BRS1");
    assert_eq!(
        &data[so::MIN_STREAK_SECS..so::MIN_STREAK_SECS + 4],
        &0x0a0b_0c0du32.to_le_bytes()
    );
    assert_eq!(
        &data[so::MIN_WEIGHT..so::MIN_WEIGHT + 8],
        &s.min_weight.to_le_bytes()
    );
    assert_eq!(StreakHeader::parse(&data), Some(s));

    // Each kind's magic is checked, and so is the length; neither header reads as the other, and a
    // lottery hook's own fields (which start at the same offset) read as neither.
    assert_eq!(JackpotHeader::parse(&data), None);
    let jackpot_data = state_with(&base_header(0), &j.encode());
    assert_eq!(StreakHeader::parse(&jackpot_data), None);
    assert_eq!(JackpotHeader::parse(&jackpot_data[..o::END - 1]), None);
    assert_eq!(StreakHeader::parse(&data[..so::END - 1]), None);
    let lottery_like = state_with(&base_header(3_600), &[1, 255, 0, 0, 9, 9, 9, 9]);
    assert_eq!(JackpotHeader::parse(&lottery_like), None);
    assert_eq!(StreakHeader::parse(&lottery_like), None);
    for len in 0..o::END + 2 {
        let _ = JackpotHeader::parse(&jackpot_data[..len.min(jackpot_data.len())]);
        let _ = StreakHeader::parse(&data[..len.min(data.len())]);
    }
}

#[test]
fn the_jackpot_mark_is_in_the_free_bytes_and_leaves_the_phase_1_slots_alone() {
    assert_eq!(MARK_AT, slot_offsets::FREE);
    let mut slots = Slots {
        current: Range {
            round: 3,
            start: 4,
            weight: 5,
        },
        previous: Range {
            round: 2,
            start: 6,
            weight: 7,
        },
        since: 99,
        free: [0xcc; 16],
    };
    set_jackpot_mark(&mut slots, 0x0102_0304_0506_0708);
    let data = slots.encode();
    assert_eq!(
        &data[MARK_AT..MARK_AT + 8],
        &0x0102_0304_0506_0708u64.to_le_bytes()
    );
    assert_eq!(&data[MARK_AT + 8..], &[0xcc; 8]);
    let back = Slots::decode(&data);
    assert_eq!(jackpot_mark(&back), 0x0102_0304_0506_0708);
    assert_eq!(
        (back.current, back.previous, back.since),
        (slots.current, slots.previous, 99)
    );
}

fn launch_data(mint: &Pubkey, pool: &Pubkey, status: u8) -> Vec<u8> {
    let mut data = vec![0u8; 700];
    data[..8].copy_from_slice(&LAUNCH_DISCRIMINATOR);
    data[launch_offsets::MINT..launch_offsets::MINT + 32].copy_from_slice(mint.as_ref());
    data[launch_offsets::POOL..launch_offsets::POOL + 32].copy_from_slice(pool.as_ref());
    data[launch_offsets::STATUS] = status;
    data
}

#[test]
fn the_launch_view_reads_the_pool_and_the_curve() {
    let mint = Pubkey::new_from_array([1; 32]);
    let pool = Pubkey::new_from_array([2; 32]);
    let curve = launch_data(&mint, &pool, 0);
    assert_eq!(
        parse_launch(&curve, &mint),
        Some(LaunchView {
            pool,
            on_curve: true
        })
    );
    let graduated = launch_data(&mint, &pool, 1);
    assert!(!parse_launch(&graduated, &mint).unwrap().on_curve);
    // Another mint's launch, a launch not written yet, another account, a short one: nothing.
    assert_eq!(parse_launch(&curve, &Pubkey::new_from_array([3; 32])), None);
    assert_eq!(parse_launch(&vec![0u8; 700], &mint), None);
    let mut other = curve.clone();
    other[0] ^= 1;
    assert_eq!(parse_launch(&other, &mint), None);
    assert_eq!(parse_launch(&curve[..launch_offsets::END - 1], &mint), None);
    assert_eq!(
        parse_launch(&launch_data(&mint, &Pubkey::default(), 0), &mint),
        None
    );
    for len in 0..launch_offsets::END + 1 {
        let _ = parse_launch(&curve[..len], &mint);
    }

    // The account form checks the owner.
    let key = Pubkey::new_unique();
    let mut lamports = 1u64;
    let mut bytes = curve.clone();
    let owner = LAUNCH_PROGRAM_ID;
    let info = AccountInfo::new(&key, false, false, &mut lamports, &mut bytes, &owner, false);
    assert!(read_launch(&info, &mint).is_some());
    let mut lamports = 1u64;
    let mut bytes = curve.clone();
    let not_launch = Pubkey::new_unique();
    let info = AccountInfo::new(
        &key,
        false,
        false,
        &mut lamports,
        &mut bytes,
        &not_launch,
        false,
    );
    assert_eq!(read_launch(&info, &mint), None);
}

// ---------------------------------------------------------------------------------- jackpot

const POOL: Pubkey = Pubkey::new_from_array([0x50; 32]);

fn wallet(i: u8) -> Pubkey {
    // Addresses on the curve: the public keys of fixed secret keys.
    let mut secret = [0u8; 32];
    secret[0] = i;
    secret[1] = 0x77;
    let point = curve25519_dalek_free_point(secret);
    Pubkey::new_from_array(point)
}

/// An on-curve address without a curve dependency: search keys whose bytes decode as a point.
fn curve25519_dalek_free_point(seed: [u8; 32]) -> [u8; 32] {
    let mut candidate = seed;
    for i in 0u16..=u16::MAX {
        candidate[30..32].copy_from_slice(&i.to_le_bytes());
        if Pubkey::new_from_array(candidate).is_on_curve() {
            return candidate;
        }
    }
    unreachable!("about half of all keys are on the curve")
}

#[test]
fn a_qualifying_buy_is_out_of_the_launch_pool_on_the_curve_to_a_wallet() {
    let buyer = wallet(1);
    let curve = LaunchView {
        pool: POOL,
        on_curve: true,
    };
    let graduated = LaunchView {
        pool: POOL,
        on_curve: false,
    };
    let creator = wallet(9);
    let excluded = [Pubkey::new_from_array([0x11; 32]), creator];
    assert!(qualifying_buy(
        Some(&curve),
        &POOL,
        &buyer,
        100,
        100,
        &excluded
    ));
    // Below the minimum, at least 1 token, from another pool or a wallet, to an excluded owner,
    // off the curve, to the pool itself, once graduated, or with the launch unreadable: no.
    assert!(!qualifying_buy(
        Some(&curve),
        &POOL,
        &buyer,
        99,
        100,
        &excluded
    ));
    assert!(!qualifying_buy(
        Some(&curve),
        &POOL,
        &buyer,
        0,
        0,
        &excluded
    ));
    assert!(qualifying_buy(Some(&curve), &POOL, &buyer, 1, 0, &excluded));
    assert!(!qualifying_buy(
        Some(&curve),
        &wallet(2),
        &buyer,
        100,
        1,
        &excluded
    ));
    assert!(!qualifying_buy(
        Some(&curve),
        &Pubkey::new_from_array([0x51; 32]),
        &buyer,
        100,
        1,
        &excluded
    ));
    assert!(!qualifying_buy(
        Some(&curve),
        &POOL,
        &creator,
        100,
        1,
        &excluded
    ));
    let off_curve = (0u8..=255)
        .map(|i| Pubkey::new_from_array([i; 32]))
        .find(|k| !k.is_on_curve())
        .unwrap();
    assert!(!qualifying_buy(
        Some(&curve),
        &POOL,
        &off_curve,
        100,
        1,
        &excluded
    ));
    assert!(!qualifying_buy(Some(&curve), &POOL, &POOL, 100, 1, &[]));
    assert!(!qualifying_buy(
        Some(&graduated),
        &POOL,
        &buyer,
        100,
        1,
        &excluded
    ));
    assert!(!qualifying_buy(None, &POOL, &buyer, 100, 1, &excluded));
    assert!(!qualifying_buy(
        Some(&LaunchView {
            pool: Pubkey::default(),
            on_curve: true
        }),
        &Pubkey::default(),
        &buyer,
        100,
        1,
        &[]
    ));
}

fn jackpot_state(timer: u32, min: u64) -> (GameHeader, JackpotHeader) {
    (base_header(0), JackpotHeader::new(timer, min))
}

#[test]
fn a_buy_after_the_timer_keeps_the_ended_rounds_winner() {
    let (mut h, mut j) = jackpot_state(300, 10);
    let (a, b) = (wallet(1), wallet(2));
    let (mut sa, mut sb) = (Slots::default(), Slots::default());
    let t = 1_800_000_000;
    // No round yet.
    assert_eq!(settle_round(&h, &j, 0, 300, t), None);
    jackpot_on_receive(&mut sa, 50, t);
    assert_eq!(jackpot_on_buy(&mut h, &mut j, &mut sa, &a, 50, t), Some(1));
    assert_eq!(
        (h.last_buyer, h.last_amount, h.last_buy_at, j.buys),
        (a, 50, t, 1)
    );
    assert_eq!(jackpot_mark(&sa), 1);
    // The timer has not run out: nothing to settle.
    assert_eq!(settle_round(&h, &j, 0, 300, t + 299), None);
    // B buys within the timer: B's round now, A's restarted.
    jackpot_on_receive(&mut sb, 20, t + 200);
    jackpot_on_buy(&mut h, &mut j, &mut sb, &b, 20, t + 200);
    assert_eq!((j.buys, j.ended_buys), (2, 0));
    // The timer runs out at t + 500: round 2 (B's) is over.
    let r2 = settle_round(&h, &j, 0, 300, t + 500).unwrap();
    assert_eq!((r2.number, r2.buyer, r2.amount), (2, b, 20));
    // A buys again before anyone settled: round 2 is kept as the ended round, and is still the
    // one to settle first.
    jackpot_on_buy(&mut h, &mut j, &mut sa, &a, 30, t + 600);
    assert_eq!(
        (j.buys, j.ended_buys, j.ended_buyer, j.ended_at),
        (3, 2, b, t + 200)
    );
    assert_eq!(jackpot_mark(&sa), 1, "A has held since its first buy");
    assert_eq!(settle_round(&h, &j, 0, 300, t + 600), Some(r2));
    // B still holds what it bought: B wins round 2.
    assert!(jackpot_winner_holds(&sb.encode(), &r2, 20));
    // Settled (paid 2), round 3 is next once its timer runs out.
    assert_eq!(settle_round(&h, &j, 2, 300, t + 899), None);
    let r3 = settle_round(&h, &j, 2, 300, t + 900).unwrap();
    assert_eq!((r3.number, r3.buyer, r3.amount), (3, a, 30));
    assert!(jackpot_winner_holds(&sa.encode(), &r3, 80));
    // Settled (paid 3): nothing is open until a new round ends.
    assert_eq!(settle_round(&h, &j, 3, 300, t + 10_000), None);
}

/// The ring: an ended round nobody settled stays the one `settle` pays first while 8 later rounds
/// end (8 timers after it ended, at the least), and only the 9th drops it; each of the remembered
/// ones is then paid in order, oldest first.
#[test]
fn an_unsettled_round_is_remembered_for_eight_timers() {
    let timer = 300u32;
    let (mut h, mut j) = jackpot_state(timer, 10);
    let t = 1_800_000_000i64;
    let mut slots: Vec<Slots> = vec![Slots::default(); 12];
    // Round k (k = 1..=10) is wallet k's buy at t + (k - 1) * timer: each buy ends the round before.
    for k in 1..=10u64 {
        let at = t + (k as i64 - 1) * i64::from(timer);
        let s = &mut slots[k as usize];
        jackpot_on_receive(s, 50, at);
        assert_eq!(
            jackpot_on_buy(&mut h, &mut j, s, &wallet(k as u8), 50, at),
            Some(k)
        );
        let oldest = settle_round(&h, &j, 0, timer, at).map(|r| r.number);
        if k == 1 {
            assert_eq!(oldest, None, "round 1 is still running");
        } else if k <= 9 {
            // Rounds 1..k-1 have ended; round 1 is remembered (k - 1 <= 8 of them).
            assert_eq!(oldest, Some(1), "round 1 still first at buy {k}");
            assert_eq!(j.ended(0).unwrap().number, k - 1);
        } else {
            // The 9th ended round drops round 1: round 2 is the oldest remembered.
            assert_eq!(oldest, Some(2));
        }
    }
    // 8 timers after round 1 ended (t + timer), round 1 was still payable; at the 9th it is not.
    // Paying in order: rounds 2..=9 from the ring, then 10 once its timer runs out.
    let end = t + 9 * i64::from(timer);
    let mut paid = 0;
    let mut order = Vec::new();
    while let Some(r) = settle_round(&h, &j, paid, timer, end + i64::from(timer)) {
        assert_eq!(r.buyer, wallet(r.number as u8));
        assert!(jackpot_winner_holds(
            &slots[r.number as usize].encode(),
            &r,
            50
        ));
        order.push(r.number);
        paid = r.number;
    }
    assert_eq!(order, (2..=10).collect::<Vec<_>>());
    // Paid up to a remembered round: the ones after it are still open, in order.
    assert_eq!(
        settle_round(&h, &j, 5, timer, end).map(|r| r.number),
        Some(6)
    );
    // A hook that broke the chain (a newer round numbered below an older one) is read only up to
    // the break.
    let mut broken = j;
    broken.earlier[2].number = 99;
    let first = settle_round(&h, &broken, 0, timer, end).map(|r| r.number);
    assert_eq!(first, Some(j.ended(2).unwrap().number));
    // An empty ring with a current round over pays the current round.
    let mut empty = j;
    empty.ended_buys = 0;
    assert_eq!(settle_round(&h, &empty, 0, timer, end), None);
    assert_eq!(
        settle_round(&h, &empty, 0, timer, end + i64::from(timer)).map(|r| r.number),
        Some(10)
    );
}

#[test]
fn the_last_buyer_who_sells_or_rebuys_does_not_win() {
    let (mut h, mut j) = jackpot_state(300, 10);
    let a = wallet(1);
    let mut sa = Slots::default();
    let t = 1_800_000_000;
    jackpot_on_receive(&mut sa, 100, t);
    jackpot_on_buy(&mut h, &mut j, &mut sa, &a, 100, t);
    let r = settle_round(&h, &j, 0, 300, t + 300).unwrap();
    assert!(jackpot_winner_holds(&sa.encode(), &r, 100));
    // Not enough held.
    assert!(!jackpot_winner_holds(&sa.encode(), &r, 99));
    // A sends one token: the mark is gone, even once it buys back (below the minimum, which is
    // not a qualifying buy, so the round stays the same).
    jackpot_on_send(&mut sa, 99, t + 10);
    assert_eq!(jackpot_mark(&sa), 0);
    jackpot_on_receive(&mut sa, 200, t + 20);
    assert!(!jackpot_winner_holds(&sa.encode(), &r, 200));
    // A buys back with a qualifying buy: a new round (2), which A wins only for itself.
    jackpot_on_buy(&mut h, &mut j, &mut sa, &a, 100, t + 30);
    assert_eq!(jackpot_mark(&sa), 2);
    let r2 = settle_round(&h, &j, 0, 300, t + 330).unwrap();
    assert_eq!(r2.number, 2);
    let r1 = JackpotRound { number: 1, ..r2 };
    assert!(
        !jackpot_winner_holds(&sa.encode(), &r1, 300),
        "mark 2 > round 1"
    );
    assert!(jackpot_winner_holds(&sa.encode(), &r2, 300));
    // Selling everything clears the holding.
    jackpot_on_send(&mut sa, 0, t + 40);
    assert_eq!(sa, Slots::default());
    // A zero amount never wins.
    let zero = JackpotRound { amount: 0, ..r2 };
    let mut held = Slots::default();
    set_jackpot_mark(&mut held, 1);
    assert!(!jackpot_winner_holds(&held.encode(), &zero, 5));
}

#[test]
fn settle_round_never_panics_and_never_goes_back() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for _ in 0..20_000 {
        let h = GameHeader {
            last_buyer: wallet(1),
            last_amount: rng.next(),
            last_buy_at: rng.next() as i64,
            ..base_header(0)
        };
        let mut j = JackpotHeader {
            buys: rng.below(12),
            ended_buys: rng.below(12),
            ended_at: rng.next() as i64,
            ended_amount: rng.next(),
            timer_secs: rng.next() as u32,
            ..JackpotHeader::new(0, 0)
        };
        for r in j.earlier.iter_mut() {
            *r = EndedRound {
                buyer: wallet(rng.below(4) as u8),
                amount: rng.next(),
                at: [rng.next() as i64, rng.below(10_000) as i64][rng.below(2) as usize],
                number: rng.below(12),
            };
        }
        let paid = rng.below(12);
        let now = rng.next() as i64;
        if let Some(r) = settle_round(&h, &j, paid, rng.next() as u32, now) {
            assert!(r.number > paid);
            let remembered = (0..JACKPOT_ENDED_ROUNDS)
                .filter_map(|i| j.ended(i))
                .any(|e| e.number == r.number && e.number < j.buys);
            assert!(r.number == j.buys || remembered);
        }
    }
}

/// A jackpot walk: holders buy from the pool (some below the minimum, some after graduation),
/// send, sell and burn; the clock jumps. A plain model of who bought what when, and who sent
/// since, must agree with the hook's header and marks at every step.
#[derive(Default, Clone)]
struct JackpotModel {
    balances: Vec<u64>,
    slots: Vec<Slots>,
    /// The step of each holder's last send (or burn).
    last_send: Vec<u64>,
    /// Each qualifying buy: (buyer, amount, at, step).
    buys: Vec<(usize, u64, i64, u64)>,
    step: u64,
}

/// The rounds that ended, oldest first: a round is numbered by its last buy, and it ended when the
/// next qualifying buy came a whole timer or more after it.
fn model_ended(buys: &[(usize, u64, i64, u64)], timer: u32) -> Vec<u64> {
    (1..buys.len())
        .filter(|&i| buys[i].2 >= buys[i - 1].2 + i64::from(timer))
        .map(|i| i as u64)
        .collect()
}

#[test]
fn a_jackpot_walk_keeps_every_promise() {
    const N: usize = 6;
    let (mut won, mut forfeited, mut ended_settles) = (0u32, 0u32, 0u32);
    // Settles of a round older than the newest ended one, and settles that skipped dropped rounds.
    let (mut deep_settles, mut dropped) = (0u32, 0u32);
    for seed in 0..200u64 {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ seed);
        let timer = [300u32, 600, 3_600][rng.below(3) as usize];
        let min = 1 + rng.below(50);
        let (mut h, mut j) = jackpot_state(timer, min);
        let owners: Vec<Pubkey> = (0..N as u8).map(wallet).collect();
        let mut m = JackpotModel {
            balances: vec![0; N],
            slots: vec![Slots::default(); N],
            last_send: vec![0; N],
            ..Default::default()
        };
        let mut now = 1_800_000_000i64;
        let mut on_curve = true;
        let mut paid = 0u64;
        // The rounds the model says ended (a qualifying buy came after their timer ran out).
        for _ in 0..300 {
            m.step += 1;
            now += [0i64, 1, 60, 299, 300, 301, 3_600][rng.below(7) as usize];
            if rng.below(60) == 0 {
                on_curve = false;
            }
            let launch = LaunchView {
                pool: POOL,
                on_curve,
            };
            let who = rng.below(N as u64) as usize;
            match rng.below(5) {
                0 | 1 => {
                    // A buy from the pool.
                    let amount = 1 + rng.below(3 * min);
                    let before = m.balances[who];
                    m.balances[who] += amount;
                    let s = &mut m.slots[who];
                    jackpot_on_receive(s, m.balances[who], now);
                    let qualifies =
                        qualifying_buy(Some(&launch), &POOL, &owners[who], amount, min, &[]);
                    assert_eq!(qualifies, on_curve && amount >= min);
                    if qualifies {
                        let n = jackpot_on_buy(&mut h, &mut j, s, &owners[who], amount, now);
                        m.buys.push((who, amount, now, m.step));
                        assert_eq!(n, Some(m.buys.len() as u64));
                    }
                    let _ = before;
                }
                2 => {
                    // A send to another holder (a receive that is no buy).
                    let to = rng.below(N as u64) as usize;
                    if to != who && m.balances[who] > 0 {
                        let amount = 1 + rng.below(m.balances[who]);
                        m.balances[who] -= amount;
                        m.last_send[who] = m.step;
                        jackpot_on_send(&mut m.slots[who], m.balances[who], now);
                        m.balances[to] += amount;
                        jackpot_on_receive(&mut m.slots[to], m.balances[to], now);
                    }
                }
                3 => {
                    // A sell or a burn.
                    if m.balances[who] > 0 {
                        let amount = 1 + rng.below(m.balances[who]);
                        m.balances[who] -= amount;
                        m.last_send[who] = m.step;
                        jackpot_on_send(&mut m.slots[who], m.balances[who], now);
                    }
                }
                _ => {
                    // Someone settles whatever is open: the oldest remembered ended round later
                    // than `paid` (the model's last 8 ended rounds), else the current one once over.
                    let ended_model = model_ended(&m.buys, timer);
                    let remembered =
                        &ended_model[ended_model.len().saturating_sub(JACKPOT_ENDED_ROUNDS)..];
                    let expected = remembered.iter().copied().find(|&n| n > paid).or_else(|| {
                        let current = m.buys.len() as u64;
                        let over = m.buys.last().is_some_and(|b| now >= b.2 + i64::from(timer));
                        (current > paid && over).then_some(current)
                    });
                    let got = settle_round(&h, &j, paid, timer, now);
                    assert_eq!(got.map(|r| r.number), expected, "seed {seed}");
                    if let Some(r) = got {
                        assert!(r.number > paid);
                        let (buyer, amount, at, step) = m.buys[(r.number - 1) as usize];
                        assert_eq!((r.buyer, r.amount, r.at), (owners[buyer], amount, at));
                        // Over: the next buy came after its timer, or its timer ran out now.
                        let next_at = m.buys.get(r.number as usize).map(|b| b.2);
                        assert!(next_at.map_or(now, |n| n.min(now)) >= at + i64::from(timer));
                        let wins =
                            jackpot_winner_holds(&m.slots[buyer].encode(), &r, m.balances[buyer]);
                        let model = m.last_send[buyer] < step && m.balances[buyer] >= amount;
                        assert_eq!(wins, model, "seed {seed} round {}", r.number);
                        if wins {
                            won += 1;
                        } else {
                            forfeited += 1;
                        }
                        if r.number < j.buys {
                            ended_settles += 1;
                        }
                        if r.number < j.ended_buys {
                            deep_settles += 1;
                        }
                        if ended_model.iter().any(|&n| n > paid && n < r.number) {
                            dropped += 1;
                        }
                        paid = r.number;
                    }
                }
            }
            // Every step: the header names the last qualifying buy.
            assert_eq!(j.buys, m.buys.len() as u64);
            if let Some(&(buyer, amount, at, _)) = m.buys.last() {
                assert_eq!(
                    (h.last_buyer, h.last_amount, h.last_buy_at),
                    (owners[buyer], amount, at)
                );
            }
            // The remembered ended rounds are the last 8 a qualifying buy found over, newest
            // first, with their buyers, amounts and times.
            let ended = model_ended(&m.buys, timer);
            for i in 0..JACKPOT_ENDED_ROUNDS {
                let e = j.ended(i).unwrap();
                match ended.len().checked_sub(i + 1).map(|at| ended[at]) {
                    Some(n) => {
                        let (buyer, amount, at, _) = m.buys[(n - 1) as usize];
                        assert_eq!(
                            e,
                            EndedRound {
                                buyer: owners[buyer],
                                amount,
                                at,
                                number: n
                            }
                        );
                    }
                    None => assert_eq!(e, EndedRound::default()),
                }
            }
            // Every mark is the holder's first qualifying buy since its last send.
            for (i, s) in m.slots.iter().enumerate() {
                let first = m
                    .buys
                    .iter()
                    .position(|b| b.0 == i && b.3 > m.last_send[i])
                    .map_or(0, |p| p as u64 + 1);
                let expected = if m.balances[i] == 0 { 0 } else { first };
                assert_eq!(jackpot_mark(s), expected, "seed {seed} holder {i}");
            }
        }
    }
    // The walk reaches every outcome.
    assert!(
        won > 200 && forfeited > 200 && ended_settles > 50 && deep_settles > 50 && dropped > 5,
        "{won} {forfeited} {ended_settles} {deep_settles} {dropped}"
    );
}

// ---------------------------------------------------------------------------------- streak

const EPOCH: u32 = 86_400;

#[test]
fn a_streak_counts_only_what_was_held_through_the_epoch_without_a_send() {
    let streak = StreakHeader::new(0, 10);
    let t0 = i64::from(EPOCH) * 20_000;
    let mut h = GameHeader::new(Pubkey::new_from_array([7; 32]), EPOCH, t0);
    let e0 = h.round;
    let (mut a, mut b) = (Slots::default(), Slots::default());
    // Both receive in epoch e0: nothing counts in it (they held nothing when it began).
    streak_on_receive(&mut h, &streak, &mut a, 0, 100, t0 + 10);
    streak_on_receive(&mut h, &streak, &mut b, 0, 5, t0 + 20);
    assert_eq!(h.total, 0);
    assert_eq!((a.since, b.since), (t0 + 10, t0 + 20));
    // Next epoch: the keeper enters both. A registers 100; B is below the minimum weight, so
    // nothing is written for it (and it can still be written by a transfer later this epoch).
    let t1 = t0 + i64::from(EPOCH) + 5;
    assert!(streak_on_enter(&mut h, &streak, &mut a, 100, t1));
    assert!(!streak_on_enter(&mut h, &streak, &mut b, 5, t1));
    assert_eq!(h.round, e0 + 1);
    assert_eq!(h.total, 100);
    assert_eq!(
        a.current,
        Range {
            round: e0 + 1,
            start: 0,
            weight: 100
        }
    );
    // Entering again does nothing.
    assert!(!streak_on_enter(&mut h, &streak, &mut a, 100, t1 + 1));
    // A receives more: still 100 (what arrives counts from the next epoch).
    streak_on_receive(&mut h, &streak, &mut a, 100, 150, t1 + 2);
    assert_eq!((h.total, a.current.weight), (100, 100));
    // B receives 20: its first write of the epoch registers what it held before (5): below the
    // minimum, so 0.
    streak_on_receive(&mut h, &streak, &mut b, 5, 25, t1 + 3);
    assert_eq!(
        (h.total, b.current.weight, b.current.round),
        (100, 0, e0 + 1)
    );
    // The epoch ends: its total is final (100). In e0 + 2, A sends one token: its share of e0 + 1
    // is forfeited, and its weight in e0 + 2 is 0. The final total stays.
    let t2 = t1 + i64::from(EPOCH);
    assert_eq!(
        streak_weight(&a.encode(), e0 + 1, EPOCH, 0, 10, 150),
        100,
        "claimable before the send"
    );
    streak_on_send(&mut h, &mut a, 149, t2);
    assert_eq!(h.round, e0 + 2);
    assert_eq!(h.total_of(e0 + 1), Some(100));
    assert_eq!(streak_weight(&a.encode(), e0 + 1, EPOCH, 0, 10, 149), 0);
    assert_eq!(a.since, t2);
    // B, entered in e0 + 2, holds 25 since it began.
    assert!(streak_on_enter(&mut h, &streak, &mut b, 25, t2 + 1));
    assert_eq!(h.total, 25);
    // B sends: its weight leaves the total at once (exact).
    streak_on_send(&mut h, &mut b, 20, t2 + 2);
    assert_eq!(h.total, 0);
    assert_eq!(b.current, Range::none_in(e0 + 2));
}

#[test]
fn the_minimum_streak_is_judged_at_the_epochs_end() {
    let week = 7 * 86_400u32;
    let streak = StreakHeader::new(week, 1);
    let t0 = i64::from(EPOCH) * 20_000;
    let mut h = GameHeader::new(Pubkey::new_from_array([7; 32]), EPOCH, t0);
    let mut a = Slots::default();
    streak_on_receive(&mut h, &streak, &mut a, 0, 100, t0);
    // Until the end of the sixth epoch after, A has not gone a week by the epoch's end; from then on it qualifies,
    // whenever in the epoch it is entered.
    for k in 1..=8u32 {
        let start = round_start(h.round + 1, EPOCH);
        let qualifies = streak_qualifies(a.since, h.round + 1, EPOCH, week);
        assert_eq!(qualifies, k >= 6, "epoch +{k}");
        let entered = streak_on_enter(&mut h, &streak, &mut a, 100, start + 1);
        assert_eq!(entered, qualifies);
        if entered {
            assert_eq!(
                streak_weight(&a.encode(), h.round, EPOCH, week, 1, 100),
                100
            );
        }
        // The same verdict late in the epoch.
        assert_eq!(streak_qualifies(a.since, h.round, EPOCH, week), qualifies);
    }
    // `since` of 0 never qualifies; a streak longer than the time since 1970 never does.
    assert!(!streak_qualifies(0, 100, EPOCH, 0));
    assert!(!streak_qualifies(1, 0, EPOCH, u32::MAX));
    assert_eq!(streak_weight_for(&h, &streak, 0, 100), 0);
}

#[test]
fn shares_round_down_and_never_exceed_the_pot() {
    assert_eq!(share_of(1_000, 1, 3), 333);
    assert_eq!(share_of(1_000, 3, 3), 1_000);
    assert_eq!(
        share_of(1_000, 4, 3),
        1_000,
        "a weight above the total is cut to it"
    );
    assert_eq!(share_of(1_000, 1, 0), 0);
    assert_eq!(share_of(u64::MAX, u64::MAX, u64::MAX), u64::MAX);
    assert_eq!(share_of(u64::MAX, 1, u64::MAX), 1);
    let mut rng = Rng(77);
    for _ in 0..10_000 {
        let total = 1 + rng.next() % 1_000_000;
        let pot = rng.next();
        let mut left = total;
        let mut paid = 0u128;
        while left > 0 {
            let w = 1 + rng.below(left);
            left -= w;
            paid += u128::from(share_of(pot, w, total));
        }
        assert!(paid <= u128::from(pot), "the shares never exceed the pot");
    }
}

/// A streak walk: holders receive (buys), send between themselves, sell, burn and are entered,
/// across epochs. A plain model of balances and sends must agree with the hook at every step.
#[test]
fn a_streak_walk_keeps_every_promise() {
    const N: usize = 6;
    let (mut claims, mut exact_checks) = (0u32, 0u32);
    for seed in 0..200u64 {
        let mut rng = Rng(0x51ee_d0f5_7ea4 ^ seed);
        let epoch_secs = [3_600u32, 86_400][rng.below(2) as usize];
        let streak = StreakHeader::new(
            [0u32, 3_600, 2 * 86_400][rng.below(3) as usize],
            1 + rng.below(20),
        );
        let mut now = i64::from(epoch_secs) * 500_000 + rng.below(1_000) as i64;
        let mut h = GameHeader::new(Pubkey::new_from_array([7; 32]), epoch_secs, now);
        let mut slots = vec![Slots::default(); N];
        let mut bal = vec![0u64; N];
        // The model: per holder, the balance at the start of the current epoch and whether it sent
        // during it; per epoch, the final weights.
        let mut epoch = h.round;
        let mut start_bal = vec![0u64; N];
        let mut sent_in = vec![false; N];
        let mut prev_weights: Option<(u32, Vec<u64>)> = None;
        for _ in 0..400 {
            now += [
                0i64,
                1,
                600,
                i64::from(epoch_secs) / 2,
                i64::from(epoch_secs),
            ][rng.below(5) as usize];
            let who = rng.below(N as u64) as usize;
            // A new epoch begins for the model when the clock crosses it (the header rolls at the
            // first write; the model rolls with the clock).
            let clock_epoch = round_of(now, epoch_secs);
            if clock_epoch > epoch {
                let weights: Vec<u64> = (0..N)
                    .map(|i| slots[i].range_in(epoch).map_or(0, |r| r.weight))
                    .collect();
                prev_weights = Some((epoch, weights));
                epoch = clock_epoch;
                start_bal = bal.clone();
                sent_in = vec![false; N];
            }
            match rng.below(5) {
                0 => {
                    let amount = 1 + rng.below(40);
                    let before = bal[who];
                    bal[who] += amount;
                    streak_on_receive(&mut h, &streak, &mut slots[who], before, bal[who], now);
                }
                1 => {
                    let to = rng.below(N as u64) as usize;
                    if to != who && bal[who] > 0 {
                        let amount = 1 + rng.below(bal[who]);
                        bal[who] -= amount;
                        sent_in[who] = true;
                        streak_on_send(&mut h, &mut slots[who], bal[who], now);
                        let before = bal[to];
                        bal[to] += amount;
                        streak_on_receive(&mut h, &streak, &mut slots[to], before, bal[to], now);
                    }
                }
                2 => {
                    if bal[who] > 0 {
                        let amount = 1 + rng.below(bal[who]);
                        bal[who] -= amount;
                        sent_in[who] = true;
                        streak_on_send(&mut h, &mut slots[who], bal[who], now);
                    }
                }
                _ => {
                    streak_on_enter(&mut h, &streak, &mut slots[who], bal[who], now);
                }
            }
            // The header's epoch never runs ahead of the clock.
            assert!(h.round <= epoch);
            // 1. The total is exact: the sum of the live weights of the header's epoch.
            let live: u64 = slots
                .iter()
                .filter_map(|s| s.range_in(h.round))
                .map(|r| r.weight)
                .sum();
            if h.round == epoch {
                assert_eq!(h.total, live, "seed {seed}");
                exact_checks += u32::from(live > 0);
            }
            for i in 0..N {
                if let Some(r) = slots[i].range_in(epoch) {
                    // 2. No weight above the balance, nor above what was held since the epoch began.
                    assert!(
                        r.weight <= bal[i] && r.weight <= start_bal[i],
                        "seed {seed}"
                    );
                    // 3. A holder that sent this epoch holds no weight in it.
                    assert!(!sent_in[i], "seed {seed}");
                    // 4. It qualifies, and is at least the floor.
                    assert!(r.weight >= streak.floor());
                    assert!(streak_qualifies(
                        slots[i].since,
                        epoch,
                        epoch_secs,
                        streak.min_streak_secs
                    ));
                }
            }
            // 5. What can be claimed for the previous epoch never exceeds its final total, and each
            //    claim is that epoch's final weight of a holder that has not sent since.
            if let Some((pe, weights)) = &prev_weights {
                if *pe + 1 == epoch {
                    let mut claimable = 0u64;
                    for i in 0..N {
                        let w = streak_weight(
                            &slots[i].encode(),
                            *pe,
                            epoch_secs,
                            streak.min_streak_secs,
                            streak.min_weight,
                            bal[i],
                        );
                        if w > 0 {
                            assert_eq!(w, weights[i], "seed {seed}");
                            assert!(!sent_in[i]);
                            claims += 1;
                        }
                        claimable += w;
                    }
                    if let Some(total) = h.total_of(*pe) {
                        if h.round > *pe {
                            assert!(claimable <= total, "seed {seed}");
                            assert_eq!(total, weights.iter().sum::<u64>(), "seed {seed}");
                        }
                    }
                }
            }
        }
    }
    assert!(
        claims > 1_000 && exact_checks > 1_000,
        "{claims} {exact_checks}"
    );
}

#[test]
fn the_rules_never_panic_on_any_bytes() {
    let mut rng = Rng(0xdead_beef);
    for _ in 0..50_000 {
        let mut data = [0u8; HOOK_DATA_LEN];
        for b in data.iter_mut() {
            *b = rng.next() as u8;
        }
        let mut s = Slots::decode(&data);
        let mut h = GameHeader {
            round_secs: rng.next() as u32,
            round: rng.next() as u32,
            total: rng.next(),
            prev_round: rng.next() as u32,
            prev_total: rng.next(),
            last_buy_at: rng.next() as i64,
            ..base_header(0)
        };
        let streak = StreakHeader::new(rng.next() as u32, rng.next());
        let mut j = JackpotHeader {
            buys: rng.next(),
            timer_secs: rng.next() as u32,
            ..JackpotHeader::new(0, 0)
        };
        let now = rng.next() as i64;
        match rng.below(6) {
            0 => streak_on_send(&mut h, &mut s, rng.next(), now),
            1 => streak_on_receive(&mut h, &streak, &mut s, rng.next(), rng.next(), now),
            2 => {
                streak_on_enter(&mut h, &streak, &mut s, rng.next(), now);
            }
            3 => {
                jackpot_on_buy(&mut h, &mut j, &mut s, &wallet(1), rng.next(), now);
            }
            4 => jackpot_on_send(&mut s, rng.next(), now),
            _ => jackpot_on_receive(&mut s, rng.next(), now),
        }
        let _ = streak_weight(
            &s.encode(),
            rng.next() as u32,
            rng.next() as u32,
            rng.next() as u32,
            rng.next(),
            rng.next(),
        );
        let r = JackpotRound {
            number: rng.next(),
            buyer: wallet(1),
            amount: rng.next(),
            at: rng.next() as i64,
        };
        let _ = jackpot_winner_holds(&s.encode(), &r, rng.next());
        let _ = timer_over(rng.next() as i64, rng.next() as u32, rng.next() as i64);
    }
}
