use super::*;

/// A header with a distinct value in every field.
fn sample_header() -> GameHeader {
    GameHeader {
        magic: MAGIC,
        mint: Pubkey::new_from_array([7; 32]),
        round_secs: 21_600,
        round: 83_334,
        total: 0x0102_0304_0506_0708,
        prev_round: 83_331,
        prev_total: 0x1112_1314_1516_1718,
        last_buyer: Pubkey::new_from_array([9; 32]),
        last_amount: 0x2122_2324_2526_2728,
        last_buy_at: -5,
    }
}

/// A state account's data: a discriminator, the header, then the hook's own bytes.
fn state_data(header: &GameHeader) -> Vec<u8> {
    let mut data = vec![0xee; DISCRIMINATOR_LEN];
    header.serialize(&mut data).unwrap();
    data.extend_from_slice(&[0xab; 40]);
    data
}

#[test]
fn the_header_sits_at_the_documented_offsets() {
    use header_offsets as o;
    let h = sample_header();
    let data = state_data(&h);
    // Borsh lays the struct out exactly as the offsets say, and `encode` agrees with it.
    assert_eq!(GameHeader::INIT_SPACE, HEADER_LEN);
    assert_eq!(o::END, DISCRIMINATOR_LEN + HEADER_LEN);
    assert_eq!(&data[DISCRIMINATOR_LEN..o::END], &h.encode()[..]);
    assert_eq!(&data[o::MAGIC..o::MAGIC + 4], b"BRG1");
    assert_eq!(&data[o::MINT..o::MINT + 32], &[7; 32]);
    assert_eq!(
        &data[o::ROUND_SECS..o::ROUND_SECS + 4],
        &21_600u32.to_le_bytes()
    );
    assert_eq!(&data[o::ROUND..o::ROUND + 4], &83_334u32.to_le_bytes());
    assert_eq!(&data[o::TOTAL..o::TOTAL + 8], &h.total.to_le_bytes());
    assert_eq!(
        &data[o::PREV_ROUND..o::PREV_ROUND + 4],
        &83_331u32.to_le_bytes()
    );
    assert_eq!(
        &data[o::PREV_TOTAL..o::PREV_TOTAL + 8],
        &h.prev_total.to_le_bytes()
    );
    assert_eq!(&data[o::LAST_BUYER..o::LAST_BUYER + 32], &[9; 32]);
    assert_eq!(
        &data[o::LAST_AMOUNT..o::LAST_AMOUNT + 8],
        &h.last_amount.to_le_bytes()
    );
    assert_eq!(
        &data[o::LAST_BUY_AT..o::LAST_BUY_AT + 8],
        &(-5i64).to_le_bytes()
    );
    assert_eq!(
        data[o::END],
        0xab,
        "the hook's own fields start after the header"
    );
    // Parsing raw bytes gives the header back.
    assert_eq!(GameHeader::parse(&data), Ok(h));
    assert_eq!(GameHeader::read(&data, &h.mint), Ok(h));
}

#[test]
fn parsing_checks_length_magic_and_mint() {
    let h = sample_header();
    let data = state_data(&h);
    assert_eq!(
        GameHeader::parse(&data[..header_offsets::END - 1]),
        Err(GameError::TooShort)
    );
    assert_eq!(GameHeader::parse(&[]), Err(GameError::TooShort));
    let mut bad = data.clone();
    bad[header_offsets::MAGIC + 3] = b'2';
    assert_eq!(GameHeader::parse(&bad), Err(GameError::BadMagic));
    assert_eq!(
        GameHeader::read(&data, &Pubkey::new_from_array([8; 32])),
        Err(GameError::WrongMint)
    );
    // An all-zero account (not prepared) is not a header.
    assert_eq!(GameHeader::parse(&[0u8; 200]), Err(GameError::BadMagic));
}

#[test]
fn read_state_checks_the_owner_and_the_address() {
    let hook = Pubkey::new_unique();
    let mint = Pubkey::new_from_array([7; 32]);
    let (state, bump) = state_address(&hook, &mint);
    assert_eq!(state_address_at(&hook, &mint, bump), Some(state));
    let h = sample_header();
    let mut data = state_data(&h);
    let mut lamports = 1u64;
    let info = AccountInfo::new(&state, false, false, &mut lamports, &mut data, &hook, false);
    assert_eq!(read_state(&info, &hook, &mint), Ok(h));
    assert_eq!(read_state_at(&info, &hook, &mint, bump), Ok(h));
    // Another program's account, or the right owner at another address, is refused.
    let other = Pubkey::new_unique();
    assert_eq!(read_state(&info, &other, &mint), Err(GameError::WrongOwner));
    let elsewhere = Pubkey::new_unique();
    let mut data2 = state_data(&h);
    let mut lamports2 = 1u64;
    let info2 = AccountInfo::new(
        &elsewhere,
        false,
        false,
        &mut lamports2,
        &mut data2,
        &hook,
        false,
    );
    assert_eq!(
        read_state(&info2, &hook, &mint),
        Err(GameError::WrongAddress)
    );
    // The right address for another mint is the wrong address for this one.
    assert_eq!(
        read_state(&info, &hook, &Pubkey::new_from_array([8; 32])),
        Err(GameError::WrongAddress)
    );
}

#[test]
fn rounds_are_the_clock_divided_and_never_panic() {
    assert_eq!(round_of(1_800_000_000, 3_600), 500_000);
    assert_eq!(round_of(1_800_003_599, 3_600), 500_000);
    assert_eq!(round_of(1_800_003_600, 3_600), 500_001);
    assert_eq!(round_of(0, 3_600), 0);
    assert_eq!(round_of(-1, 3_600), 0);
    assert_eq!(round_of(1_800_000_000, 0), 0);
    assert_eq!(round_of(i64::MAX, 1), u32::MAX);
    assert_eq!(round_start(500_001, 3_600), 1_800_003_600);
    assert_eq!(round_end(500_000, 3_600), 1_800_003_600);
    assert_eq!(round_end(u32::MAX, u32::MAX), i64::MAX, "saturates");
    assert!(valid_round_secs(3_600) && valid_round_secs(30 * 86_400));
    assert!(!valid_round_secs(3_599) && !valid_round_secs(30 * 86_400 + 1) && !valid_round_secs(0));
}

#[test]
fn the_header_rolls_on_a_new_round_and_remembers_the_last_two() {
    let t = 1_800_000_000i64;
    let mut h = GameHeader::new(Pubkey::new_unique(), 3_600, t + 10);
    assert_eq!(
        (h.round, h.total, h.prev_round, h.prev_total),
        (500_000, 0, 0, 0)
    );
    h.total = 40;
    // Same round: nothing. A clock that reads earlier never moves it back.
    assert!(!h.roll(t + 3_599));
    assert!(!h.roll(t - 7_200));
    assert_eq!(h.round_at(t - 7_200), 500_000);
    assert_eq!((h.round, h.total), (500_000, 40));
    // The next round: the old one moves to prev.
    assert!(h.roll(t + 3_600));
    assert_eq!(
        (h.round, h.total, h.prev_round, h.prev_total),
        (500_001, 0, 500_000, 40)
    );
    h.total = 9;
    // Two rounds later with no write in between: prev is the round that had a write, by number.
    assert!(h.roll(t + 4 * 3_600));
    assert_eq!(
        (h.round, h.total, h.prev_round, h.prev_total),
        (500_004, 0, 500_001, 9)
    );
    // What the header knows of each finished round.
    assert_eq!(h.total_of(500_004), Some(0));
    assert_eq!(h.total_of(500_001), Some(9));
    assert_eq!(h.total_of(500_002), Some(0), "no write in it");
    assert_eq!(h.total_of(500_003), Some(0), "no write in it");
    assert_eq!(h.total_of(500_009), Some(0), "nothing written yet");
    assert_eq!(
        h.total_of(500_000),
        None,
        "forgotten: two rounds with writes came after"
    );
    // A fresh header knows its first round and everything before it held nothing.
    let f = GameHeader::new(Pubkey::new_unique(), 3_600, t);
    assert_eq!(f.total_of(500_000), Some(0));
    assert_eq!(f.total_of(499_000), Some(0));
}

#[test]
fn slots_round_trip_at_the_documented_offsets() {
    use slot_offsets as o;
    let s = Slots {
        current: Range {
            round: 9,
            start: 100,
            weight: 50,
        },
        previous: Range {
            round: 8,
            start: 7,
            weight: 3,
        },
        since: 1_800_000_123,
        free: [0x5a; 16],
    };
    let d = s.encode();
    assert_eq!(&d[o::ROUND..o::ROUND + 4], &9u32.to_le_bytes());
    assert_eq!(&d[o::START..o::START + 8], &100u64.to_le_bytes());
    assert_eq!(&d[o::WEIGHT..o::WEIGHT + 8], &50u64.to_le_bytes());
    assert_eq!(&d[o::PREV_ROUND..o::PREV_ROUND + 4], &8u32.to_le_bytes());
    assert_eq!(&d[o::PREV_START..o::PREV_START + 8], &7u64.to_le_bytes());
    assert_eq!(&d[o::PREV_WEIGHT..o::PREV_WEIGHT + 8], &3u64.to_le_bytes());
    assert_eq!(&d[o::SINCE..o::SINCE + 8], &1_800_000_123i64.to_le_bytes());
    assert_eq!(&d[o::FREE..], &[0x5a; 16]);
    assert_eq!(o::FREE + o::FREE_LEN, HOOK_DATA_LEN);
    assert_eq!(Slots::decode(&d), s);
    // No tickets is all zeros, both ways. A current slot without tickets keeps only its round (the
    // round the holding was last written in); a previous slot without tickets is zeros.
    assert_eq!(Slots::default().encode(), [0u8; 64]);
    assert_eq!(Slots::decode(&[0u8; 64]), Slots::default());
    let ghost = Slots {
        current: Range {
            round: 9,
            start: 100,
            weight: 0,
        },
        previous: Range {
            round: 8,
            start: 7,
            weight: 0,
        },
        ..Slots::default()
    };
    let mut marked = [0u8; 64];
    marked[..4].copy_from_slice(&9u32.to_le_bytes());
    assert_eq!(ghost.encode(), marked);
    assert_eq!(
        Slots::decode(&marked),
        Slots {
            current: Range::none_in(9),
            ..Slots::default()
        }
    );
    assert!(written_in(&Slots::decode(&marked), 9));
    assert!(!written_in(&Slots::decode(&marked), 10));
    assert!(
        !written_in(&Slots::default(), 0),
        "round 0 is never written"
    );
    assert_eq!(Slots::decode(&marked).range_in(9), None, "no tickets");
    assert_eq!(s.range_in(9), Some(s.current));
    assert_eq!(s.range_in(8), Some(s.previous));
    assert_eq!(s.range_in(7), None);
}

fn header_at(round: u32, total: u64) -> GameHeader {
    GameHeader {
        magic: MAGIC,
        round_secs: 3_600,
        round,
        total,
        ..GameHeader::default()
    }
}

#[test]
fn register_takes_fresh_tickets_at_the_end() {
    let mut h = header_at(5, 30);
    let mut s = Slots::default();
    assert!(register(&mut h, &mut s, 20));
    assert_eq!(
        s.current,
        Range {
            round: 5,
            start: 30,
            weight: 20
        }
    );
    assert_eq!(h.total, 50);
    // Again: a new range at the end, the old one dead.
    assert!(register(&mut h, &mut s, 25));
    assert_eq!(
        s.current,
        Range {
            round: 5,
            start: 50,
            weight: 25
        }
    );
    assert_eq!(h.total, 75);
    // Nothing for nothing, nothing past u64::MAX.
    assert!(!register(&mut h, &mut s, 0));
    let mut full = header_at(5, u64::MAX - 3);
    let mut t = Slots::default();
    assert!(!register(&mut full, &mut t, 4));
    assert_eq!((full.total, t), (u64::MAX - 3, Slots::default()));
    assert!(register(&mut full, &mut t, 3));
    assert_eq!(full.total, u64::MAX);
}

#[test]
fn a_holding_registers_once_a_round_what_it_held_since_it_began() {
    let t = 1_800_000_000i64;
    let mut h = GameHeader::new(Pubkey::new_unique(), 3_600, t);
    let r = h.round;
    let (mut a, mut b, mut c) = (Slots::default(), Slots::default(), Slots::default());
    // A buys 100 in round r: it held nothing when the round began, so no tickets this round; the
    // slot marks the round.
    on_receive(&mut h, &mut a, 0, 100, t + 1);
    assert_eq!(a.current, Range::none_in(r));
    assert_eq!((a.since, h.total), (t + 1, 0));
    // More arrives: still nothing this round, and `enter` can't register it either.
    on_receive(&mut h, &mut a, 100, 150, t + 2);
    assert!(!on_enter(&mut h, &mut a, 150, t + 3));
    assert_eq!((a.current, h.total), (Range::none_in(r), 0));

    // B and C buy too.
    on_receive(&mut h, &mut b, 0, 70, t + 10);
    on_receive(&mut h, &mut c, 0, 90, t + 20);
    // Round r + 1: A's first write registers what it held when the round began, and nothing more.
    let t1 = t + 3_600;
    on_receive(&mut h, &mut a, 150, 400, t1);
    assert_eq!(
        (a.current, h.total),
        (
            Range {
                round: r + 1,
                start: 0,
                weight: 150
            },
            150
        )
    );
    // B is entered (its whole balance, held since before the round): the next range.
    assert!(on_enter(&mut h, &mut b, 70, t1 + 1));
    assert_eq!(
        b.current,
        Range {
            round: r + 1,
            start: 150,
            weight: 70
        }
    );
    // A second enter, or more arriving, changes nothing: one range a round.
    assert!(!on_enter(&mut h, &mut b, 70, t1 + 2));
    on_receive(&mut h, &mut b, 70, 500, t1 + 3);
    assert_eq!((b.current.weight, h.total), (70, 220));
    // C's first write of the round is a send: its range is what it keeps, nothing dies.
    on_send(&mut h, &mut c, 30, t1 + 4);
    assert_eq!(
        (c.current, h.total),
        (
            Range {
                round: r + 1,
                start: 220,
                weight: 30
            },
            250
        )
    );
    // A later send cuts it (those tickets are dead); what comes back does not revive them.
    on_send(&mut h, &mut c, 10, t1 + 5);
    on_receive(&mut h, &mut c, 10, 90, t1 + 6);
    assert!(!on_enter(&mut h, &mut c, 90, t1 + 7));
    assert_eq!((c.current.weight, h.total), (10, 250));
    // A receive of nothing writes nothing.
    let mut d = Slots::default();
    on_receive(&mut h, &mut d, 0, 0, t1 + 8);
    assert_eq!(d, Slots::default());
}

#[test]
fn moving_tokens_between_wallets_adds_no_tickets() {
    // The dead-ticket flood: a stash bounced between two wallets of the same owner, as many times
    // as fees allow. Each hop's receiver held nothing of it when the round began, so no hop adds
    // a ticket; the stash's own range is cut by its first send.
    let t = 1_800_000_000i64;
    let mut h = GameHeader::new(Pubkey::new_unique(), 3_600, t);
    let (mut e1, mut e2, mut honest) = (Slots::default(), Slots::default(), Slots::default());
    on_receive(&mut h, &mut e1, 0, 1_000, t);
    on_receive(&mut h, &mut honest, 0, 5_000, t);
    let t1 = t + 3_600;
    assert!(on_enter(&mut h, &mut honest, 5_000, t1));
    assert!(on_enter(&mut h, &mut e1, 1_000, t1));
    assert_eq!(h.total, 6_000);
    let (mut b1, mut b2) = (1_000u64, 0u64);
    for i in 0..1_000 {
        let now = t1 + 1 + i;
        if i % 2 == 0 {
            on_send(&mut h, &mut e1, 0, now);
            on_receive(&mut h, &mut e2, b2, b2 + b1, now);
            (b1, b2) = (0, b2 + b1);
        } else {
            on_send(&mut h, &mut e2, 0, now);
            on_receive(&mut h, &mut e1, b1, b1 + b2, now);
            (b1, b2) = (b1 + b2, 0);
        }
        assert!(!on_enter(&mut h, &mut e1, b1, now));
        assert!(!on_enter(&mut h, &mut e2, b2, now));
    }
    assert_eq!(h.total, 6_000, "not one ticket added");
    assert_eq!(honest.range_in(h.round).unwrap().weight, 5_000);
    // The dead tickets are the stash's own range, once.
    assert!(e1.range_in(h.round).is_none() && e2.range_in(h.round).is_none());
    // Dust, the same: a holder's range never dies or grows from what others send it.
    for i in 0..100 {
        on_receive(
            &mut h,
            &mut honest,
            5_000 + i,
            5_001 + i,
            t1 + 2_000 + i as i64,
        );
    }
    assert_eq!(honest.range_in(h.round).unwrap().weight, 5_000);
    assert_eq!(h.total, 6_000);
}

#[test]
fn sends_shrink_both_slots_to_what_is_left() {
    let t = 1_800_000_000i64;
    let mut h = GameHeader::new(Pubkey::new_unique(), 3_600, t);
    let mut a = Slots::default();
    on_receive(&mut h, &mut a, 0, 100, t + 5);
    assert_eq!(a.since, t + 5);
    // Next round: A's 100 are its range; then the round after, A receives again and its
    // round-500001 range moves to previous.
    assert!(on_enter(&mut h, &mut a, 100, t + 3_600));
    on_receive(&mut h, &mut a, 100, 150, t + 7_200);
    assert_eq!(
        a.previous,
        Range {
            round: 500_001,
            start: 0,
            weight: 100
        }
    );
    assert_eq!(
        a.current,
        Range {
            round: 500_002,
            start: 0,
            weight: 100
        }
    );
    assert_eq!(a.since, t + 5, "since is set once on receive");
    // A send leaving 60 cuts both slots to 60 and stamps since.
    on_send(&mut h, &mut a, 60, t + 7_300);
    assert_eq!(
        (a.previous.weight, a.current.weight, a.since),
        (60, 60, t + 7_300)
    );
    // A send leaving 80 (can't happen after the cut, but the rule is a min) changes no weight up.
    on_send(&mut h, &mut a, 80, t + 7_301);
    assert_eq!((a.previous.weight, a.current.weight), (60, 60));
    // A send leaving nothing clears everything, free bytes too: the holding can close.
    a.free = [1; 16];
    on_send(&mut h, &mut a, 0, t + 7_302);
    assert_eq!(a.encode(), [0u8; 64]);
}

#[test]
fn a_winner_who_trades_in_the_next_round_still_claims() {
    let t = 1_800_000_000i64;
    let mut h = GameHeader::new(Pubkey::new_unique(), 3_600, t);
    let (mut a, mut b) = (Slots::default(), Slots::default());
    on_receive(&mut h, &mut a, 0, 100, t);
    on_receive(&mut h, &mut b, 0, 50, t);
    // Round r: both entered.
    let t0 = t + 3_600;
    assert!(on_enter(&mut h, &mut a, 100, t0)); // [0, 100)
    assert!(on_enter(&mut h, &mut b, 50, t0)); // [100, 150)
    let r = h.round;
    let total_r = h.total;
    // Round r ends; A buys more in r + 1.
    on_receive(&mut h, &mut a, 100, 300, t0 + 3_600);
    assert_eq!(h.total_of(r), Some(total_r));
    assert!(wins(&a.encode(), r, 99, 300));
    assert!(!wins(&a.encode(), r, 100, 300), "B's ticket");
    assert!(wins(&b.encode(), r, 100, 50));
    // However often anyone writes A in r + 1 (enters, dust), its round-r range stays.
    assert!(!on_enter(&mut h, &mut a, 300, t0 + 3_601));
    on_receive(&mut h, &mut a, 300, 301, t0 + 3_602);
    assert!(wins(&a.encode(), r, 99, 301));
    // A sells down to 40 in r + 1: its round-r range is cut to 40, ticket 99 is dead.
    on_send(&mut h, &mut a, 40, t0 + 3_700);
    assert!(wins(&a.encode(), r, 39, 40));
    assert!(!wins(&a.encode(), r, 40, 40));
    // The claim checks the balance too: hook data claiming more than the balance wins nothing.
    assert!(!wins(&b.encode(), r, 100, 49));
    // Written again in r + 2, A forgets round r: the previous slot now holds r + 1. (So the
    // companion ends every claim of round r with round r + 1.)
    on_receive(&mut h, &mut a, 40, 41, t0 + 2 * 3_600);
    assert!(!wins(&a.encode(), r, 0, 41));
    assert_eq!(a.previous.round, r + 1);
    // B, not written in r + 1, still has round r in r + 2.
    on_receive(&mut h, &mut b, 50, 51, t0 + 2 * 3_600 + 1);
    assert!(wins(&b.encode(), r, 100, 51));
}

#[test]
fn enter_registers_the_whole_balance_once_a_round() {
    let t = 1_800_000_000i64;
    let mut h = GameHeader::new(Pubkey::new_unique(), 3_600, t);
    let (mut a, mut b) = (Slots::default(), Slots::default());
    on_receive(&mut h, &mut a, 0, 100, t);
    // Written this round (the buy): nothing to write.
    let before = a;
    assert!(!on_enter(&mut h, &mut a, 100, t + 1));
    assert_eq!(a, before);
    // Next round: the whole balance.
    assert!(on_enter(&mut h, &mut a, 100, t + 3_600));
    assert_eq!(
        a.current,
        Range {
            round: 500_001,
            start: 0,
            weight: 100
        }
    );
    assert_eq!(a.since, t, "since is not reset by an enter");
    // The round after: the whole balance again, the old range kept for its claim.
    assert!(on_enter(&mut h, &mut a, 100, t + 7_200));
    assert_eq!(a.previous.round, 500_001);
    assert_eq!(a.current.round, 500_002);
    // A second enter in the round is a no-op, so nobody can grief A by re-entering it.
    let before = a;
    assert!(!on_enter(&mut h, &mut a, 100, t + 7_201));
    assert_eq!(a, before);
    on_receive(&mut h, &mut b, 0, 10, t + 7_202);
    assert!(!on_enter(&mut h, &mut a, 101, t + 7_203));
    assert_eq!(h.total, 100);
    // Nothing to enter with nothing held: an empty holding stays all zeros (so it can close).
    let mut empty = Slots::default();
    assert!(!on_enter(&mut h, &mut empty, 0, t + 7_204));
    assert_eq!(empty, Slots::default());
    assert_eq!(h.total, 100);
}

#[test]
fn draw_index_is_sha256_of_the_randomness_and_k() {
    // Vectors from Python's hashlib: sha256(R || u32_le(k)), first 8 bytes little-endian.
    assert_eq!(
        draw_index(&[0u8; 64], 0, u64::MAX),
        Some(12_976_294_286_951_469_335)
    );
    let r: Vec<u8> = (0..64).collect();
    assert_eq!(draw_index(&r, 7, 1_000_003), Some(359_196));
    assert_eq!(draw_index(&r, 7, 1), Some(0));
    assert_eq!(draw_index(&r, 7, 0), None);
    assert_ne!(draw_index(&r, 7, u64::MAX), draw_index(&r, 8, u64::MAX));
    // A 32-byte randomness works the same way.
    assert!(draw_index(&[1u8; 32], 0, 10).unwrap() < 10);
    // Uniform enough: 70,000 draws over 7 tickets land within 5% of 10,000 each.
    let mut counts = [0u32; 7];
    for i in 0u32..70_000 {
        let mut seed = [0u8; 64];
        seed[..4].copy_from_slice(&i.to_le_bytes());
        counts[draw_index(&seed, i % 3, 7).unwrap() as usize] += 1;
    }
    for c in counts {
        assert!((9_500..=10_500).contains(&c), "{counts:?}");
    }
}

#[test]
fn only_wallets_outside_the_launch_hold_tickets() {
    let mint = Pubkey::new_unique();
    let launch = launch_address(&mint);
    let creator = companion_creator_address(&mint);
    // The launch and the creator address are PDAs: off the curve, so excluded even unlisted.
    assert!(!launch.is_on_curve() && !creator.is_on_curve());
    assert!(!eligible(&launch, &[]));
    assert!(!eligible(&creator, &[]));
    assert!(!eligible(&Pubkey::default(), &[]));
    // A wallet (an ed25519 public key) is eligible unless listed.
    let wallet = Pubkey::new_from_array([
        0x3b, 0x6a, 0x27, 0xbc, 0xce, 0xb6, 0xa4, 0x2d, 0x62, 0xa3, 0xa8, 0xd0, 0x2a, 0x6f, 0x0d,
        0x73, 0x65, 0x32, 0x15, 0x77, 0x1d, 0xe2, 0x43, 0xa6, 0x3a, 0xc0, 0x48, 0xa1, 0x8b, 0x59,
        0xda, 0x29,
    ]);
    assert!(wallet.is_on_curve(), "RFC 8032 test 1's public key");
    assert!(eligible(&wallet, &[launch, creator]));
    assert!(!eligible(&wallet, &[launch, wallet]));
    // The seeds are the programs' own.
    assert_eq!(
        launch,
        Pubkey::find_program_address(&[b"launch", mint.as_ref()], &LAUNCH_PROGRAM_ID).0
    );
    assert_eq!(
        creator,
        Pubkey::find_program_address(&[b"creator", mint.as_ref()], &COMPANION_PROGRAM_ID).0
    );
}

// ------------------------------------------------------------------------------- random walk

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

const HOLDERS: usize = 6;
const ROUND: u32 = 3_600;

/// A game as the standard writes it: `HOLDERS` eligible holdings and one excluded holding (the
/// pool), every move through the `on_*` rules exactly as a hook applies them. It also follows, by
/// the clock, what each holding has held since the clock's round began (`held`) and what all of
/// them held when it began (`opening`).
#[derive(Clone)]
struct Walk {
    header: GameHeader,
    balances: [u64; HOLDERS],
    slots: [Slots; HOLDERS],
    pool: u64,
    now: i64,
    clock_round: u32,
    held: [u64; HOLDERS],
    opening: u64,
}

impl Walk {
    fn new(now: i64) -> Self {
        Self {
            header: GameHeader::new(Pubkey::new_unique(), ROUND, now),
            balances: [0; HOLDERS],
            slots: [Slots::default(); HOLDERS],
            pool: 10_000,
            now,
            clock_round: round_of(now, ROUND),
            held: [0; HOLDERS],
            opening: 0,
        }
    }

    /// The clock moved by `secs`.
    fn wait(&mut self, secs: i64) {
        self.now += secs;
        let round = round_of(self.now, ROUND);
        if round != self.clock_round {
            self.clock_round = round;
            self.held = self.balances;
            self.opening = self.balances.iter().sum();
        }
    }

    fn lost(&mut self, h: usize) {
        self.held[h] = self.held[h].min(self.balances[h]);
    }

    fn buy(&mut self, h: usize, amount: u64) {
        let amount = amount.min(self.pool);
        if amount == 0 {
            return;
        }
        self.pool -= amount;
        let before = self.balances[h];
        self.balances[h] += amount;
        on_receive(
            &mut self.header,
            &mut self.slots[h],
            before,
            self.balances[h],
            self.now,
        );
    }

    fn sell(&mut self, h: usize, amount: u64) {
        let amount = amount.min(self.balances[h]);
        if amount == 0 {
            return;
        }
        self.balances[h] -= amount;
        self.lost(h);
        self.pool += amount;
        on_send(
            &mut self.header,
            &mut self.slots[h],
            self.balances[h],
            self.now,
        );
    }

    fn send(&mut self, from: usize, to: usize, amount: u64) {
        let amount = amount.min(self.balances[from]);
        if amount == 0 || from == to {
            return;
        }
        self.balances[from] -= amount;
        self.lost(from);
        on_send(
            &mut self.header,
            &mut self.slots[from],
            self.balances[from],
            self.now,
        );
        let before = self.balances[to];
        self.balances[to] += amount;
        on_receive(
            &mut self.header,
            &mut self.slots[to],
            before,
            self.balances[to],
            self.now,
        );
    }

    fn burn(&mut self, h: usize, amount: u64) {
        let amount = amount.min(self.balances[h]);
        if amount == 0 {
            return;
        }
        self.balances[h] -= amount;
        self.lost(h);
        on_send(
            &mut self.header,
            &mut self.slots[h],
            self.balances[h],
            self.now,
        );
    }

    fn enter(&mut self, h: usize) {
        let before = self.slots[h];
        let changed = on_enter(
            &mut self.header,
            &mut self.slots[h],
            self.balances[h],
            self.now,
        );
        if !changed {
            assert_eq!(self.slots[h], before, "a no-op enter writes nothing");
        }
    }

    /// Every holding's live range in `round`.
    fn ranges_in(&self, round: u32) -> Vec<(usize, Range)> {
        (0..HOLDERS)
            .filter_map(|h| self.slots[h].range_in(round).map(|r| (h, r)))
            .collect()
    }
}

/// The invariants the standard promises, between two states one step apart.
fn check(before: &Walk, after: &Walk) {
    let h = &after.header;
    for i in 0..HOLDERS {
        let (s, bal) = (&after.slots[i], after.balances[i]);
        // No ticket ever exceeds the balance.
        assert!(
            s.current.weight <= bal && s.previous.weight <= bal,
            "holder {i}"
        );
        // Nothing held, nothing kept.
        if bal == 0 {
            assert_eq!(s.encode(), [0u8; 64], "holder {i} emptied");
        }
        // Slots never run ahead of the header, and the previous one is older.
        if s.current.is_live() {
            assert!(s.current.round <= h.round);
            if s.previous.is_live() {
                assert!(s.previous.round < s.current.round);
            }
        }
    }
    // A round's tickets are tokens held since it began: no range above what its holder has held
    // since then, and no more tickets in the round than its holders held when it began.
    if h.round == after.clock_round {
        assert!(
            h.total <= after.opening,
            "round {}: {} tickets, {} tokens held when it began",
            h.round,
            h.total,
            after.opening
        );
        for i in 0..HOLDERS {
            if let Some(r) = after.slots[i].range_in(h.round) {
                assert!(
                    r.weight <= after.held[i],
                    "holder {i} holds tickets it bought this round"
                );
            }
        }
    }
    // A holding is written at most once a round with a range: one range a round, never re-made.
    for i in 0..HOLDERS {
        let (old, new) = (
            before.slots[i].range_in(h.round),
            after.slots[i].range_in(h.round),
        );
        if let (Some(old), Some(new)) = (old, new) {
            assert_eq!(old.start, new.start, "holder {i} got a second range");
        }
        if old.is_none() && new.is_some() {
            assert!(
                !written_in(&before.slots[i], h.round),
                "holder {i} registered after its first write of the round"
            );
        }
    }
    // The header only rolls forward, keeping what it rolls.
    if h.round == before.header.round {
        assert!(
            h.total >= before.header.total,
            "a round's total never decreases"
        );
        assert_eq!(
            (h.prev_round, h.prev_total),
            (before.header.prev_round, before.header.prev_total)
        );
    } else {
        assert!(h.round > before.header.round);
        assert_eq!(
            (h.prev_round, h.prev_total),
            (before.header.round, before.header.total)
        );
    }
    // Each round the header still knows: ranges within its total, disjoint, summing to at most it.
    for round in [h.round, h.prev_round] {
        let total = h.total_of(round).unwrap();
        let mut ranges = after.ranges_in(round);
        ranges.sort_by_key(|(_, r)| r.start);
        let (mut sum, mut end) = (0u64, 0u64);
        for (i, r) in &ranges {
            assert!(
                r.start >= end,
                "holder {i}'s range overlaps another in round {round}"
            );
            end = r.end().unwrap();
            assert!(
                end <= total,
                "holder {i}'s range ends past the total of round {round}"
            );
            sum += r.weight;
        }
        assert!(sum <= total);
    }
    // Ranges only shrink: a holding's range of a round after the step is what it had of that round
    // (the same start, never more of it), plus at most the fresh tickets past the round's old total.
    // With the ranges disjoint, no ticket ever changes hands or comes back once dead.
    for i in 0..HOLDERS {
        for new in [after.slots[i].current, after.slots[i].previous] {
            if !new.is_live() {
                continue;
            }
            let old = before.slots[i].range_in(new.round);
            assert!(new.round <= h.round);
            if new.round < h.round {
                // A past round: nothing new at all, only what was there, never more of it.
                let old = old.expect("a range of a past round appeared");
                assert!(
                    new.start == old.start && new.weight <= old.weight,
                    "holder {i} grew in a past round"
                );
                continue;
            }
            // The current round: its old total, 0 if it just started.
            let old_total = if before.header.round == h.round {
                before.header.total
            } else {
                0
            };
            assert!(new.end().unwrap() <= h.total);
            if new.start < old_total {
                let old = old.expect("a range below the old total that was not there");
                assert_eq!(new.start, old.start, "holder {i}");
                assert!(
                    new.end().unwrap().min(old_total) <= old.end().unwrap(),
                    "holder {i} regained dead tickets"
                );
            }
        }
    }
}

/// Exactly the holdings' ranges win: at every ticket checked (each range's edges and the tickets
/// just outside them, and a stride of at most 300 through the round), [`wins`] holds for exactly
/// the holding whose range contains it, and for nobody at a dead ticket.
fn check_odds(w: &Walk) {
    let round = w.header.round;
    let total = w.header.total;
    let data: Vec<[u8; 64]> = w.slots.iter().map(Slots::encode).collect();
    let mut xs: Vec<u64> = (0..total).step_by((total / 300).max(1) as usize).collect();
    for (_, r) in w.ranges_in(round) {
        let end = r.end().unwrap();
        xs.extend([r.start, end - 1, end, r.start.wrapping_sub(1)]);
    }
    for x in xs.into_iter().filter(|x| *x < total) {
        let mut winners = 0;
        for (i, hook_data) in data.iter().enumerate() {
            let won = wins(hook_data, round, x, w.balances[i]);
            let holds = w.slots[i].range_in(round).is_some_and(|r| r.contains(x));
            assert_eq!(won, holds, "holder {i}, ticket {x}");
            winners += usize::from(won);
        }
        assert!(winners <= 1, "ticket {x}");
    }
}

#[test]
fn a_random_walk_keeps_every_promise() {
    for seed in 1..=10u64 {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ seed);
        let mut w = Walk::new(1_800_000_000);
        let mut rolled = 0;
        for _ in 0..1_500 {
            let before = w.clone();
            let h = rng.below(HOLDERS as u64) as usize;
            match rng.below(100) {
                0..=24 => {
                    let size = rng.below(400) + 1;
                    w.buy(h, size);
                }
                25..=34 => w.buy(h, 1), // dust
                35..=49 => {
                    let size = rng.below(w.balances[h] + 1);
                    w.sell(h, size);
                }
                50..=64 => {
                    let to = rng.below(HOLDERS as u64) as usize;
                    let size = rng.below(w.balances[h] + 1);
                    w.send(h, to, size);
                }
                65..=69 => {
                    let size = rng.below(w.balances[h] / 4 + 1);
                    w.burn(h, size);
                }
                70..=89 => w.enter(h),
                90..=94 => {
                    let secs = rng.below(i64::from(ROUND) as u64) as i64;
                    w.wait(secs);
                }
                _ => {
                    // Rounds pass, sometimes several with nothing written.
                    w.wait(i64::from(ROUND) * (1 + rng.below(3) as i64));
                    rolled += 1;
                }
            }
            check(&before, &w);
            if w.header.round != before.header.round {
                check_odds(&before);
            }
        }
        check_odds(&w);
        assert!(rolled > 10, "the walk crossed rounds");
        // Every holder still holding something ends with at most its balance in tickets.
        let live: u64 = w
            .ranges_in(w.header.round)
            .iter()
            .map(|(_, r)| r.weight)
            .sum();
        assert!(live <= w.header.total);
    }
}

#[test]
fn dust_cannot_kill_a_holders_tickets() {
    // Two whales entered for the round; then dust, over and over, to both. Neither range dies or
    // grows, and the round's total never moves: what arrives counts from the next round.
    let mut w = Walk::new(1_800_000_000);
    w.pool = 1_000_000;
    w.buy(0, 400_000);
    w.buy(1, 400_000);
    w.wait(i64::from(ROUND));
    w.enter(0);
    w.enter(1);
    let total = w.header.total;
    assert_eq!(total, 800_000);
    for i in 0..1_000 {
        w.buy(i % 2, 1);
        w.enter(i % 2);
    }
    assert_eq!(w.header.total, total);
    assert_eq!(w.slots[0].current.start, 0);
    assert_eq!(w.slots[0].current.weight, 400_000);
    assert_eq!(w.slots[1].current.weight, 400_000);
    // The next round counts the dust too.
    w.wait(i64::from(ROUND));
    w.enter(0);
    assert_eq!(w.slots[0].current.weight, 400_500);
}
