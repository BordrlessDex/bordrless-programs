#![allow(clippy::too_many_arguments)]
//! Audit (partner fee, integration and upgrade safety): the undeployed `create_partner_config` /
//! `distribute_fees` launchpad change against the program live on mainnet.
//!
//! The deployed launchpad's bytes (mainnet `1jcBym…`, `solana program dump`, sha256 of the trimmed
//! ELF `17604ad1…` = commit 22932e5) are loaded from `MAINNET_LAUNCH_SO` (default:
//! `target/mainnet/bordrless_launch.so`; without it the two upgrade tests say so and pass). With them, configs and launches are made exactly as mainnet makes them, then the
//! program is swapped for the new build (as the upgrade does) and every old account is claimed with
//! the instructions today's clients build.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::AccountSerialize;
use bordrless_companion::client as companion;
use bordrless_hook::token_flags;
use bordrless_launch::client as launch;
use bordrless_launch::error::LaunchError;
use bordrless_launch::events::{AuthorFeesPaid, CreatorFeesClaimed, PartnerFeesPaid};
use bordrless_launch::instructions::{author_part, CreateConfigArgs};
use bordrless_launch::state::{Launch, LaunchConfig, LaunchRules, QuoteCut};
use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::program_bytes;
use bordrless_token::client as token;
use hook_tester::client as tester;
use hook_tester::quote_mode;
use solana_keypair::Keypair;
use solana_signer::Signer;

const DEFAULT_OLD: &str = "target/mainnet/bordrless_launch.so";
const NO_CUT: QuoteCut = QuoteCut {
    max_buy_bps: 0,
    max_sell_bps: 0,
};
const PACKET: usize = 1_232;

/// The deployed launchpad's bytes, or None (the test is skipped) when no dump is at hand.
fn old_bytes() -> Option<Vec<u8>> {
    let path = std::env::var("MAINNET_LAUNCH_SO").unwrap_or_else(|_| {
        bordrless_program_tests::workspace_root()
            .join(DEFAULT_OLD)
            .display()
            .to_string()
    });
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("skipped: no mainnet launchpad at {path} (`solana program dump 1jcBymHxBjniZDhNPy51Vgm5Nz7pLUdxa9UBHc4TavC {path} --url mainnet-beta`)");
        return None;
    };
    // The deployed program is commit 22932e5's verified build.
    let trimmed = {
        let mut end = bytes.len();
        while end > 0 && bytes[end - 1] == 0 {
            end -= 1;
        }
        &bytes[..end]
    };
    let hash = solana_sha256_hasher::hash(trimmed).to_bytes();
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex, "17604ad167bebed7e0a7569903babe66fd06a6405fcd7e8c2d21c018d2bea836",
        "not the deployed launchpad"
    );
    Some(bytes)
}

fn use_program(w: &mut World, bytes: &[u8]) {
    w.env
        .svm
        .add_program(bordrless_launch::ID, bytes)
        .expect("load the launchpad");
    let d = w.env.deployer.pubkey();
    w.env.set_upgrade_authority(bordrless_launch::ID, Some(d));
}

fn code(e: LaunchError) -> u32 {
    u32::from(e)
}

fn cargs(hook: Option<Pubkey>, fee: u16, label: &str) -> CreateConfigArgs {
    CreateConfigArgs {
        rules: LaunchRules::NONE,
        creator_fee_bps: fee,
        custom_hook: hook,
        custom_hook_flags: if hook.is_some() {
            token_flags::BEFORE_TRANSFER
        } else {
            0
        },
        label: label.to_string(),
    }
}

fn send_config(w: &mut World, creator: &Keypair, ix: Instruction, config: &Keypair) -> Tx {
    w.env.send_paid_by(&[ix], creator, &[config])
}

fn sol_of(w: &World, owner: &Pubkey) -> u64 {
    w.env.holding(&w.sol, owner)
}

fn fees_of(w: &World, mint: &Pubkey) -> u64 {
    w.env.holding(&w.sol, &launch::launch_address(mint))
}

/// Trades on `mint`: a buy and a partial sell by a fresh wallet.
fn trade(w: &mut World, mint: &Pubkey, sol: u64) {
    let t = w.wallet_with_sol(sol + SOL);
    w.buy(&t, mint, sol).ok();
    let held = w.env.holding(mint, &t.pubkey());
    w.sell(&t, mint, held / 3).ok();
}

/// The 22-address table the SDK and keepers send with.
fn table_22(w: &World) -> Vec<Pubkey> {
    let mut t = protocol_lookup_table(w);
    t.extend([
        companion::event_authority(),
        bordrless_launch::ID,
        bordrless_swap::ID,
        bordrless_token::ID,
    ]);
    t
}

fn budget() -> Vec<Instruction> {
    vec![compute_unit_limit(1_400_000), compute_unit_price(20_000)]
}

/// Sends `ixs` (budget in front) as v0 with the 22-address table; prints and checks the packet.
#[track_caller]
fn measured(
    w: &mut World,
    name: &str,
    ixs: &[Instruction],
    payer: &Keypair,
    signers: &[&Keypair],
) -> Tx {
    let lut = w.env.put_lookup_table(Pubkey::new_unique(), &table_22(w));
    let mut all = budget();
    all.extend_from_slice(ixs);
    let tx = w.env.send_v0(&all, payer, signers, &[lut]);
    tx.ok();
    println!(
        "| {name} | {} keys | {} B | trace {} | height {} | {} CU |",
        tx.keys.len(),
        tx.size,
        tx.trace_len(),
        tx.max_height(),
        tx.cu()
    );
    assert!(tx.size <= PACKET, "{name}: {} bytes", tx.size);
    assert!(tx.trace_len() <= 64, "{name}: trace {}", tx.trace_len());
    tx
}

// =================================================================================================
// 3. The fill_config refactor: every old config instruction writes the same bytes and events.
// =================================================================================================

#[derive(Debug, PartialEq)]
enum Outcome {
    Made {
        data: Vec<u8>,
        lamports: u64,
        events: Vec<(Pubkey, [u8; 8], Vec<u8>)>,
    },
    Refused(String),
}

fn outcome(
    w: &mut World,
    creator: &Keypair,
    build: &dyn Fn(Pubkey) -> Instruction,
) -> (Outcome, u64) {
    let config = Keypair::new();
    let ix = build(config.pubkey());
    let tx = send_config(w, creator, ix, &config);
    let cu = tx.cu();
    let out = match &tx.result {
        Ok(_) => {
            let acc = w.env.account(&config.pubkey()).expect("made");
            let key = config.pubkey().to_bytes();
            let events = tx
                .raw_events()
                .into_iter()
                .map(|e| {
                    let mut body = e.body.clone();
                    // The config's own key, normalized (each run makes its own).
                    for i in 0..body.len().saturating_sub(31) {
                        if body[i..i + 32] == key {
                            body[i..i + 32].copy_from_slice(&[0; 32]);
                        }
                    }
                    (e.program, e.discriminator, body)
                })
                .collect();
            Outcome::Made {
                data: acc.data,
                lamports: acc.lamports,
                events,
            }
        }
        Err(f) => Outcome::Refused(format!("{:?}", f.err)),
    };
    (out, cu)
}

#[test]
fn the_config_instructions_write_what_the_deployed_program_writes() {
    let mut w = World::new();
    let _sol = w.sol;
    let Some(old) = old_bytes() else { return };
    let new = program_bytes("bordrless_launch");
    let author = w.wallet_with_sol(50 * SOL);
    let a = author.pubkey();
    let hook = Some(hook_tester::ID);
    type Case = (&'static str, Box<dyn Fn(Pubkey) -> Instruction>);
    let cases: Vec<Case> = vec![
        (
            "create_config plain",
            Box::new(move |c| launch::create_config(a, c, cargs(None, 100, "Plain"))),
        ),
        (
            "create_config custom hook",
            Box::new(move |c| launch::create_config(a, c, cargs(hook, 50, "Own hook"))),
        ),
        (
            "create_config label 32",
            Box::new(move |c| launch::create_config(a, c, cargs(None, 0, &"L".repeat(32)))),
        ),
        (
            "create_listed_config 5000",
            Box::new(move |c| {
                launch::create_listed_config(a, c, cargs(None, 100, "Listed"), 5_000)
            }),
        ),
        (
            "create_listed_config hook 1234",
            Box::new(move |c| {
                launch::create_listed_config(a, c, cargs(hook, 200, "Listed hook"), 1_234)
            }),
        ),
        (
            "refused: listed with 0",
            Box::new(move |c| launch::create_listed_config(a, c, cargs(None, 100, "Listed 0"), 0)),
        ),
        (
            "create_quote_cut_config 300/0",
            Box::new(move |c| {
                launch::create_quote_cut_config(
                    a,
                    c,
                    cargs(hook, 100, "Cut"),
                    0,
                    QuoteCut {
                        max_buy_bps: 300,
                        max_sell_bps: 0,
                    },
                )
            }),
        ),
        (
            "create_quote_cut_config listed 1000/1000",
            Box::new(move |c| {
                launch::create_quote_cut_config(
                    a,
                    c,
                    cargs(hook, 100, "Cut listed"),
                    2_500,
                    QuoteCut {
                        max_buy_bps: 1_000,
                        max_sell_bps: 1_000,
                    },
                )
            }),
        ),
        // Refusals: the same error, in the same order of checks.
        (
            "refused: label 33",
            Box::new(move |c| launch::create_config(a, c, cargs(None, 100, &"L".repeat(33)))),
        ),
        (
            "refused: fee too high",
            Box::new(move |c| launch::create_config(a, c, cargs(None, 5_000, "Fee"))),
        ),
        (
            "refused: author 5001",
            Box::new(move |c| launch::create_listed_config(a, c, cargs(None, 100, "A"), 5_001)),
        ),
        (
            "refused: cut without a hook",
            Box::new(move |c| {
                launch::create_quote_cut_config(
                    a,
                    c,
                    cargs(None, 100, "Q"),
                    0,
                    QuoteCut {
                        max_buy_bps: 100,
                        max_sell_bps: 0,
                    },
                )
            }),
        ),
        (
            "refused: cut 1001",
            Box::new(move |c| {
                launch::create_quote_cut_config(
                    a,
                    c,
                    cargs(hook, 100, "Q"),
                    0,
                    QuoteCut {
                        max_buy_bps: 1_001,
                        max_sell_bps: 0,
                    },
                )
            }),
        ),
        (
            "refused: cut sell only",
            Box::new(move |c| {
                launch::create_quote_cut_config(
                    a,
                    c,
                    cargs(hook, 100, "Q"),
                    0,
                    QuoteCut {
                        max_buy_bps: 0,
                        max_sell_bps: 100,
                    },
                )
            }),
        ),
        (
            "refused: no programdata",
            Box::new(move |c| {
                let mut ix = launch::create_config(a, c, cargs(hook, 100, "PD"));
                ix.accounts.pop();
                ix
            }),
        ),
        (
            "refused: hook flags 0",
            Box::new(move |c| {
                let mut args = cargs(hook, 100, "F");
                args.custom_hook_flags = 0;
                launch::create_config(a, c, args)
            }),
        ),
        (
            "hook with burn rules (allowed by both)",
            Box::new(move |c| {
                let mut args = cargs(hook, 100, "K");
                args.rules = rules(0, 0, 50, 50, 0, 0, 0, 0);
                launch::create_config(a, c, args)
            }),
        ),
    ];
    println!("| case | old CU | new CU |");
    for (name, build) in &cases {
        use_program(&mut w, &old);
        let (o, cu_old) = outcome(&mut w, &author, build.as_ref());
        use_program(&mut w, &new);
        let (n, cu_new) = outcome(&mut w, &author, build.as_ref());
        println!("| {name} | {cu_old} | {cu_new} |");
        assert_eq!(o, n, "{name}: the new program writes something else");
        if name.starts_with("refused") {
            assert!(matches!(o, Outcome::Refused(_)), "{name}: {o:?}");
        } else {
            assert!(matches!(o, Outcome::Made { .. }), "{name}: {o:?}");
        }
    }
}

// =================================================================================================
// 1. Configs and launches made by the deployed program, claimed after the upgrade.
// =================================================================================================

struct Old {
    author: Keypair,
    plain: Pubkey,
    listed: Pubkey,
    cut: Pubkey,
    /// (creator, mint): inline, from plain, from listed by another, from listed by the author,
    /// from the SOL-cut listed config by another.
    inline: (Keypair, Pubkey),
    from_plain: (Keypair, Pubkey),
    from_listed: (Keypair, Pubkey),
    own_listed: Pubkey,
    from_cut: (Keypair, Pubkey),
}

/// Everything made with the deployed launchpad, traded, partly claimed by it.
fn made_by_the_deployed_program(w: &mut World) -> Option<Old> {
    let old = old_bytes()?;
    use_program(w, &old);
    // The deployed program has no partner configs.
    let author = w.wallet_with_sol(50 * SOL);
    let probe = Keypair::new();
    let ix = launch::create_partner_config(
        author.pubkey(),
        probe.pubkey(),
        cargs(None, 100, "P"),
        0,
        NO_CUT,
        author.pubkey(),
        1_000,
    );
    assert_eq!(
        send_config(w, &author, ix, &probe).custom(),
        101,
        "InstructionFallbackNotFound"
    );

    let k = Keypair::new();
    send_config(
        w,
        &author,
        launch::create_config(author.pubkey(), k.pubkey(), cargs(None, 100, "Plain")),
        &k,
    )
    .ok();
    let plain = k.pubkey();
    let k = Keypair::new();
    send_config(
        w,
        &author,
        launch::create_listed_config(
            author.pubkey(),
            k.pubkey(),
            cargs(None, 100, "Listed"),
            5_000,
        ),
        &k,
    )
    .ok();
    let listed = k.pubkey();
    let k = Keypair::new();
    let hook = Some(hook_tester::ID);
    send_config(
        w,
        &author,
        launch::create_quote_cut_config(
            author.pubkey(),
            k.pubkey(),
            cargs(hook, 100, "Cut"),
            2_000,
            QuoteCut {
                max_buy_bps: 500,
                max_sell_bps: 500,
            },
        ),
        &k,
    )
    .ok();
    let cut = k.pubkey();

    let c = w.wallet_with_sol(20 * SOL);
    let (m, tx) = w.create_launch(&c, "INL", 100, VQ);
    tx.ok();
    let inline = (c, m);
    let c = w.wallet_with_sol(20 * SOL);
    let (m, tx) = w.create_launch_from_config(&c, "PLN", VQ, &plain);
    tx.ok();
    let from_plain = (c, m);
    let c = w.wallet_with_sol(20 * SOL);
    let (m, tx) = w.create_launch_from_config(&c, "LST", VQ, &listed);
    tx.ok();
    let from_listed = (c, m);
    let (own_listed, tx) = w.create_launch_from_config(&author, "OWN", VQ, &listed);
    tx.ok();
    // The SOL-cut coin: hook_tester answers 300 bps on every trade.
    let c = w.wallet_with_sol(20 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    w.env
        .send_paid_by(
            &[
                tester::init_script(c.pubkey(), mint, vec![]),
                tester::init_quote_state(c.pubkey(), mint),
            ],
            &c,
            &[],
        )
        .ok();
    let ix = w.create_launch_from_config_ix(&c.pubkey(), &mint, "CUT", VQ, &cut);
    w.env.send_paid_by(&[ix], &c, &[&mint_kp]).ok();
    w.env
        .send_paid_by(
            &[tester::set_quote_answer(
                c.pubkey(),
                mint,
                quote_mode::RETURN,
                300u16.to_le_bytes().to_vec(),
            )],
            &c,
            &[],
        )
        .ok();
    let from_cut = (c, mint);

    w.env.warp(31);
    for m in [
        inline.1,
        from_plain.1,
        from_listed.1,
        own_listed,
        from_cut.1,
    ] {
        trade(w, &m, 2 * SOL);
        assert!(fees_of(w, &m) > 0);
    }
    assert!(
        w.launch(&from_cut.1).quote_cut_accrued() > 0,
        "the SOL cut was taken"
    );
    // The deployed program's own claims, before the upgrade: the author's (counters move).
    w.env
        .send_paid_by(
            &[launch::claim_author_fees(
                author.pubkey(),
                from_listed.1,
                w.sol,
                listed,
                from_listed.0.pubkey(),
            )],
            &author,
            &[],
        )
        .ok();
    assert!(w.launch(&from_listed.1).author_fees_paid > 0);
    for m in [
        inline.1,
        from_plain.1,
        from_listed.1,
        own_listed,
        from_cut.1,
    ] {
        trade(w, &m, SOL);
    }
    Some(Old {
        author,
        plain,
        listed,
        cut,
        inline,
        from_plain,
        from_listed,
        own_listed,
        from_cut,
    })
}

/// `before` and `after` agree on everything but the claim counters.
#[track_caller]
fn only_counters_moved(before: &Launch, after: &Launch) {
    let mut a = after.clone();
    a.creator_fees_claimed = before.creator_fees_claimed;
    a.author_fees_paid = before.author_fees_paid;
    a.creator_fees_accrued = before.creator_fees_accrued;
    let (mut x, mut y) = (Vec::new(), Vec::new());
    before.try_serialize(&mut x).unwrap();
    a.try_serialize(&mut y).unwrap();
    assert_eq!(x, y, "a claim touched more than its counters");
}

#[test]
fn old_configs_and_launches_claim_exactly_as_before_after_the_upgrade() {
    let mut w = World::new();
    let sol = w.sol;
    let Some(o) = made_by_the_deployed_program(&mut w) else {
        return;
    };
    let keys = [
        o.plain,
        o.listed,
        o.cut,
        launch::launch_address(&o.inline.1),
        launch::launch_address(&o.from_plain.1),
        launch::launch_address(&o.from_listed.1),
        launch::launch_address(&o.own_listed),
        launch::launch_address(&o.from_cut.1),
    ];
    let snapshot: Vec<Vec<u8>> = keys
        .iter()
        .map(|k| w.env.account(k).unwrap().data)
        .collect();

    // The upgrade.
    let new = program_bytes("bordrless_launch");
    use_program(&mut w, &new);
    for (k, d) in keys.iter().zip(&snapshot) {
        assert_eq!(&w.env.account(k).unwrap().data, d);
    }
    for c in [o.plain, o.listed, o.cut] {
        let lc: LaunchConfig = w.env.read(&c);
        assert_eq!((lc.partner_share_bps(), lc.partner_bump()), (0, 0));
        assert!(lc.reserved[5..].iter().all(|b| *b == 0));
    }
    let lc: LaunchConfig = w.env.read(&o.cut);
    assert_eq!(
        lc.quote_cut().map(|c| (c.max_buy_bps, c.max_sell_bps)),
        Some((500, 500))
    );
    for m in [
        o.inline.1,
        o.from_plain.1,
        o.from_listed.1,
        o.own_listed,
        o.from_cut.1,
    ] {
        let l = w.launch(&m);
        assert_eq!((l.partner_share_bps(), l.partner_bump()), (0, 0));
        assert!(l.reserved[14..].iter().all(|b| *b == 0));
    }
    let cut_launch = w.launch(&o.from_cut.1);
    assert!(cut_launch.quote_cut().is_some() && cut_launch.author_share_bps == 2_000);

    // The new program trades them (the pool hook) and keeps taking the old SOL cut.
    let accrued = cut_launch.quote_cut_accrued();
    for m in [
        o.inline.1,
        o.from_plain.1,
        o.from_listed.1,
        o.own_listed,
        o.from_cut.1,
    ] {
        trade(&mut w, &m, SOL);
    }
    assert!(w.launch(&o.from_cut.1).quote_cut_accrued() > accrued);

    // Today's clients' claims: the creator alone.
    for (creator, mint) in [
        (&o.inline.0, o.inline.1),
        (&o.from_plain.0, o.from_plain.1),
        (&o.author, o.own_listed),
    ] {
        let before = w.launch(&mint);
        let fees = fees_of(&w, &mint);
        let c0 = sol_of(&w, &creator.pubkey());
        let tx = w.env.send_paid_by(
            &[launch::claim_creator_fees(creator.pubkey(), mint, sol)],
            creator,
            &[],
        );
        tx.ok();
        assert_eq!(sol_of(&w, &creator.pubkey()), c0 + fees);
        let ev: CreatorFeesClaimed = tx.event();
        assert_eq!(
            (ev.amount, ev.claimed_total),
            (fees, before.creator_fees_claimed + fees)
        );
        assert!(
            tx.events::<AuthorFeesPaid>().is_empty() && tx.events::<PartnerFeesPaid>().is_empty()
        );
        let after = w.launch(&mint);
        only_counters_moved(&before, &after);
        // distribute_fees is refused for them (no PartnerFee).
        trade(&mut w, &mint, SOL);
        let ix = launch::distribute_fees(
            mint,
            sol,
            before.config,
            creator.pubkey(),
            creator.pubkey(),
            None,
        );
        let tx = w.env.send(&[ix], &[]);
        println!("distribute_fees on an old launch: {:?}", tx.err());
    }

    // The creator's shared claim of the listed launch (config and author's holding), as before.
    let (c3, m3) = (&o.from_listed.0, o.from_listed.1);
    // Without the author's accounts: refused as before.
    w.env
        .send_paid_by(&[launch::claim_creator_fees(c3.pubkey(), m3, sol)], c3, &[])
        .expect_code(code(LaunchError::AuthorAccountsMissing));
    let before = w.launch(&m3);
    let fees = fees_of(&w, &m3);
    let (c0, a0) = (sol_of(&w, &c3.pubkey()), sol_of(&w, &o.author.pubkey()));
    let tx = w.env.send_paid_by(
        &[launch::claim_creator_fees_shared(
            c3.pubkey(),
            m3,
            sol,
            o.listed,
            o.author.pubkey(),
        )],
        c3,
        &[],
    );
    tx.ok();
    let to_author = author_part(fees, 5_000);
    assert_eq!(sol_of(&w, &o.author.pubkey()), a0 + to_author);
    assert_eq!(sol_of(&w, &c3.pubkey()), c0 + fees - to_author);
    assert_eq!(tx.event::<AuthorFeesPaid>().amount, to_author);
    assert!(tx.events::<PartnerFeesPaid>().is_empty());
    only_counters_moved(&before, &w.launch(&m3));

    // The author's claim of the SOL-cut launch (2,000 bps), as before.
    let (c4, m4) = (&o.from_cut.0, o.from_cut.1);
    let before = w.launch(&m4);
    let fees = fees_of(&w, &m4);
    let (c0, a0) = (sol_of(&w, &c4.pubkey()), sol_of(&w, &o.author.pubkey()));
    let tx = w.env.send_paid_by(
        &[launch::claim_author_fees(
            o.author.pubkey(),
            m4,
            sol,
            o.cut,
            c4.pubkey(),
        )],
        &o.author,
        &[],
    );
    tx.ok();
    let to_author = author_part(fees, 2_000);
    assert_eq!(sol_of(&w, &o.author.pubkey()), a0 + to_author);
    assert_eq!(sol_of(&w, &c4.pubkey()), c0 + fees - to_author);
    only_counters_moved(&before, &w.launch(&m4));
}

/// A stray non-zero byte in `Launch.reserved[14..17]` (none on mainnet: 110 of 110 launches are
/// zero there) would make the new program read a partner the launch never had: every claim then
/// wants a `PartnerFee` that does not exist, so its fees would be stuck until another upgrade.
#[test]
fn a_stray_partner_byte_on_an_old_launch_would_strand_its_fees() {
    let mut w = World::new();
    let sol = w.sol;
    let Some(o) = made_by_the_deployed_program(&mut w) else {
        return;
    };
    let new = program_bytes("bordrless_launch");
    use_program(&mut w, &new);
    let (c, m) = (&o.from_plain.0, o.from_plain.1);
    let key = launch::launch_address(&m);
    let mut acc = w.env.account(&key).unwrap();
    let mut l: Launch = w.env.read(&key);
    l.reserved[14] = 1;
    let mut data = Vec::new();
    l.try_serialize(&mut data).unwrap();
    acc.data[..data.len()].copy_from_slice(&data);
    w.env.svm.set_account(key, acc).unwrap();
    w.env
        .send_paid_by(&[launch::claim_creator_fees(c.pubkey(), m, sol)], c, &[])
        .expect_code(code(LaunchError::PartnerAccountsMissing));
    let ix = launch::distribute_fees(m, sol, o.plain, c.pubkey(), c.pubkey(), None);
    let tx = w.env.send(&[ix], &[]);
    println!("distribute_fees with a stray byte: {:?}", tx.err());
    tx.expect_fail();
}

// =================================================================================================
// 4. Compute and packets.
// =================================================================================================

fn partner_config(
    w: &mut World,
    author: &Keypair,
    args: CreateConfigArgs,
    author_share: u16,
    cut: QuoteCut,
    partner: Pubkey,
    share: u16,
) -> Pubkey {
    let k = Keypair::new();
    let ix = launch::create_partner_config(
        author.pubkey(),
        k.pubkey(),
        args,
        author_share,
        cut,
        partner,
        share,
    );
    send_config(w, author, ix, &k).ok();
    k.pubkey()
}

fn partner_wallet(w: &mut World, payer: &Keypair) -> Pubkey {
    let p = Keypair::new().pubkey();
    w.env
        .send_paid_by(
            &[token::create_holding(payer.pubkey(), w.sol, p)],
            payer,
            &[],
        )
        .ok();
    p
}

#[test]
fn partner_claims_fit_a_packet_and_the_budget() {
    let mut w = World::new();
    let sol = w.sol;
    let author = w.wallet_with_sol(50 * SOL);
    let partner = partner_wallet(&mut w, &author);
    let both = partner_config(
        &mut w,
        &author,
        cargs(None, 100, "Both"),
        3_000,
        NO_CUT,
        partner,
        2_000,
    );
    let k = Keypair::new();
    send_config(
        &mut w,
        &author,
        launch::create_listed_config(
            author.pubkey(),
            k.pubkey(),
            cargs(None, 100, "Listed"),
            3_000,
        ),
        &k,
    )
    .ok();
    let listed = k.pubkey();
    let c = w.wallet_with_sol(20 * SOL);
    let (m, tx) = w.create_launch_from_config(&c, "BTH", VQ, &both);
    tx.ok();
    let (ml, tx) = w.create_launch_from_config(&c, "LST", VQ, &listed);
    tx.ok();
    w.env.warp(31);
    println!("| path | keys | bytes | trace | height | CU |");
    trade(&mut w, &ml, 2 * SOL);
    measured(
        &mut w,
        "claim_creator_fees, author (baseline, no partner)",
        &[launch::claim_creator_fees_shared(
            c.pubkey(),
            ml,
            sol,
            listed,
            author.pubkey(),
        )],
        &c,
        &[],
    );
    trade(&mut w, &m, 2 * SOL);
    measured(
        &mut w,
        "claim_creator_fees, author + partner",
        &[launch::claim_creator_fees_with(
            c.pubkey(),
            m,
            sol,
            Some((both, author.pubkey())),
            Some((both, partner)),
        )],
        &c,
        &[],
    );
    trade(&mut w, &m, 2 * SOL);
    measured(
        &mut w,
        "claim_author_fees + partner",
        &[launch::claim_author_fees_with_partner(
            author.pubkey(),
            m,
            sol,
            both,
            c.pubkey(),
            partner,
        )],
        &author,
        &[],
    );
    trade(&mut w, &m, 2 * SOL);
    let cranker = w.wallet_with_sol(SOL);
    measured(
        &mut w,
        "distribute_fees + author",
        &[launch::distribute_fees(
            m,
            sol,
            both,
            c.pubkey(),
            partner,
            Some(author.pubkey()),
        )],
        &cranker,
        &[],
    );
    // All three parties' holdings made in the same transaction (the worst case a site sends).
    trade(&mut w, &m, 2 * SOL);
    measured(
        &mut w,
        "create_holding x3 + distribute_fees + author",
        &[
            token::create_holding(cranker.pubkey(), sol, partner),
            token::create_holding(cranker.pubkey(), sol, c.pubkey()),
            token::create_holding(cranker.pubkey(), sol, author.pubkey()),
            launch::distribute_fees(m, sol, both, c.pubkey(), partner, Some(author.pubkey())),
        ],
        &cranker,
        &[],
    );
}

#[test]
fn create_launch_from_a_partner_config_costs_about_the_same() {
    let mut w = World::new();
    let _sol = w.sol;
    let creator = w.wallet_with_sol(50 * SOL);
    let partner = partner_wallet(&mut w, &creator);
    println!("| path | keys | bytes | trace | height | CU |");
    // Every kit module, plain config vs partner config.
    let p = bordrless_core::policy::presets::DIAMOND_HANDS;
    let a = CreateConfigArgs {
        rules: presets::of(&p),
        creator_fee_bps: p.creator_fee_bps,
        custom_hook: None,
        custom_hook_flags: 0,
        label: p.name.to_string(),
    };
    let (plain, tx) = w.create_config(&creator, a.clone());
    tx.ok();
    let part = partner_config(&mut w, &creator, a, 0, NO_CUT, partner, 2_500);
    let mut cus = Vec::new();
    for (name, config) in [("kit, plain config", plain), ("kit, partner config", part)] {
        let mint = Keypair::new();
        let ix =
            w.create_launch_from_config_ix(&creator.pubkey(), &mint.pubkey(), "DIAM", VQ, &config);
        cus.push(measured(&mut w, name, &[ix], &creator, &[&mint]).cu());
    }
    println!("kit: +{} CU for the partner", cus[1] as i64 - cus[0] as i64);
    // A custom hook with a SOL cut, plain vs partner.
    let mut cus = Vec::new();
    let flags = token_flags::BEFORE_TRANSFER | token_flags::BEFORE_MINT;
    let mut args = cargs(Some(hook_tester::ID), 100, "Own hook");
    args.custom_hook_flags = flags;
    let cut = QuoteCut {
        max_buy_bps: 500,
        max_sell_bps: 500,
    };
    let k = Keypair::new();
    send_config(
        &mut w,
        &creator,
        launch::create_quote_cut_config(creator.pubkey(), k.pubkey(), args.clone(), 0, cut),
        &k,
    )
    .ok();
    let plain = k.pubkey();
    let part = partner_config(&mut w, &creator, args, 0, cut, partner, 2_500);
    for (name, config) in [
        ("hook + cut, plain config", plain),
        ("hook + cut, partner config", part),
    ] {
        let mint = Keypair::new();
        w.env
            .send_paid_by(
                &[tester::init_script(creator.pubkey(), mint.pubkey(), vec![])],
                &creator,
                &[],
            )
            .ok();
        let ix =
            w.create_launch_from_config_ix(&creator.pubkey(), &mint.pubkey(), "HOOK", VQ, &config);
        let tx = measured(&mut w, name, &[ix], &creator, &[&mint]);
        assert!(tx.max_height() <= 4);
        cus.push(tx.cu());
    }
    println!(
        "hook: +{} CU for the partner",
        cus[1] as i64 - cus[0] as i64
    );
}

// =================================================================================================
// 4/5. Companions from partner configs.
// =================================================================================================

// The companion-with-partner measurements that stood here were dropped once the launchpad refused a
// partner config for a companion coin (`PartnerOnCompanion`); partner_fee.rs and audit_pf_adversarial.rs
// assert the refusal.
