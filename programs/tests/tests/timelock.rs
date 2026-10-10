//! `hook_timelock` (`docs/phase3a.md` §3, §13): a program's upgrade authority behind a public delay.
//!
//! The program under test is `tax_hook` (loaded at its own id, so its code checks its id and runs),
//! upgradeable by an author's key until registered. Proposals carry real code: `half_life.so`
//! (another Anchor program, which at `tax_hook`'s id refuses every instruction with
//! `DeclaredProgramIdMismatch`, so the upgraded program visibly runs the new code) and
//! `tax_hook.so` itself (whose executable hash is the one on mainnet, `solana-verify`'s).

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::{AccountDeserialize, InstructionData};
use bordrless_hook::authority::{
    classify, programdata_address, trimmed_len, AuthorityClass, BPF_LOADER_UPGRADEABLE_ID, MAX_DELAY,
};
use bordrless_program_tests::env::{Env, Tx, SYSTEM_PROGRAM_ID};
use bordrless_program_tests::program_bytes;
use bordrless_program_tests::timelock::*;
use hook_timelock::client as tl;
use hook_timelock::loader;
use hook_timelock::{Timelock, TimelockError, EXECUTE_WINDOW, MAX_CODE_LEN, MIN_DELAY};
use solana_keypair::Keypair;
use solana_signer::Signer;

const PROGRAM: Pubkey = tax_hook::ID;
const DAY: i64 = 86_400;
/// `tax_hook.so`'s executable hash on mainnet (the README's table).
const TAX_HOOK_HASH: &str = "a73935a42bf200b6a9a73490d8fa0ea661d1b89963ff2387c5318f6563835bc8";
/// Anchor's `DeclaredProgramIdMismatch`.
const DECLARED_ID_MISMATCH: u32 = 4100;

fn code(e: TimelockError) -> u32 {
    u32::from(e)
}

struct T {
    env: Env,
    author: Keypair,
}

/// tax_hook upgradeable by a fresh author.
fn world() -> T {
    let mut env = Env::new();
    let author = env.funded(10_000_000_000);
    env.set_upgrade_authority(PROGRAM, Some(author.pubkey()));
    T { env, author }
}

/// tax_hook registered under a timelock of `delay` with its author.
fn registered(delay: u32) -> T {
    let mut t = world();
    let a = t.author.insecure_clone();
    register(&mut t.env, &a, PROGRAM, delay, a.pubkey()).ok();
    t
}

fn timelock(env: &Env) -> Timelock {
    env.read::<Timelock>(&timelock_of(&PROGRAM))
}

/// A tax_hook instruction anyone can send (prepare for a fresh mint): with tax_hook's code it gets
/// past Anchor's id check; with another program's code it is refused at once.
fn poke(env: &mut Env) -> Tx {
    let payer = env.payer.insecure_clone();
    let mint = Pubkey::new_unique();
    let tax = Pubkey::find_program_address(&[tax_hook::TAX_SEED, mint.as_ref()], &PROGRAM).0;
    let ix = Instruction {
        program_id: PROGRAM,
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new(tax, false),
            AccountMeta::new(
                bordrless_hook::hook_accounts_address(&PROGRAM, &mint).0,
                false,
            ),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data: tax_hook::instruction::Prepare {
            fee_bps: 100,
            max_wallet_bps: 0,
        }
        .data(),
    };
    env.send(&[ix], &[])
}

/// A buffer of `code` owned by the author, then handed to the timelock (the loader's
/// `SetAuthority`, the author signing): what a proposal starts from.
fn buffer_for_timelock(t: &mut T, code: &[u8], extra_zeros: usize) -> Pubkey {
    let buffer = Pubkey::new_unique();
    put_buffer(&mut t.env, buffer, t.author.pubkey(), code, extra_zeros);
    let a = t.author.insecure_clone();
    t.env
        .send(
            &[loader::set_authority(
                buffer,
                a.pubkey(),
                Some(timelock_of(&PROGRAM)),
            )],
            &[&a],
        )
        .ok();
    assert_eq!(buffer_authority(&t.env, &buffer), Some(timelock_of(&PROGRAM)));
    buffer
}

fn propose(t: &mut T, buffer: Pubkey, len: usize) -> Tx {
    let a = t.author.insecure_clone();
    t.env
        .send(&[tl::propose(a.pubkey(), PROGRAM, buffer, len as u32)], &[&a])
}

fn execute(t: &mut T, buffer: Pubkey) -> Tx {
    let sender = t.env.funded(1_000_000_000);
    let author = timelock(&t.env).author;
    t.env
        .send(&[tl::execute(sender.pubkey(), PROGRAM, buffer, author)], &[&sender])
}

fn grow_programdata(t: &mut T, bytes: u32) -> Tx {
    let payer = t.env.funded(10_000_000_000);
    t.env.send(
        &[loader::extend_program(
            programdata_address(&PROGRAM),
            PROGRAM,
            payer.pubkey(),
            bytes,
        )],
        &[&payer],
    )
}

// ---- register ---------------------------------------------------------------------------------

#[test]
fn register_hands_the_authority_to_the_timelock() {
    let mut t = world();
    let a = t.author.insecure_clone();
    poke(&mut t.env).ok();
    let tx = register(&mut t.env, &a, PROGRAM, MIN_DELAY, a.pubkey());
    tx.ok();
    assert!(tx.size < 700, "register is {} bytes", tx.size);
    let pda = timelock_of(&PROGRAM);
    assert_eq!(upgrade_authority(&t.env, &PROGRAM), Some(pda));
    let lock = timelock(&t.env);
    assert_eq!(
        (lock.program, lock.programdata, lock.author, lock.delay_secs),
        (PROGRAM, programdata_address(&PROGRAM), a.pubkey(), MIN_DELAY)
    );
    assert!(!lock.has_pending() && !lock.finalized);
    let ev = tx.event::<hook_timelock::TimelockRegistered>();
    assert_eq!(ev.previous_authority, a.pubkey());
    // The program still runs as before.
    poke(&mut t.env).ok();
    // A second registration can't happen (the account exists, and the authority moved).
    let tx = register(&mut t.env, &a, PROGRAM, MIN_DELAY, a.pubkey());
    tx.expect_fail();
}

#[test]
fn register_takes_another_author_and_checks_its_bounds() {
    let mut t = world();
    let a = t.author.insecure_clone();
    for (delay, err) in [
        (MIN_DELAY - 1, TimelockError::DelayTooShort),
        (MAX_DELAY + 1, TimelockError::DelayTooLong),
    ] {
        register(&mut t.env, &a, PROGRAM, delay, a.pubkey()).expect_code(code(err));
    }
    register(&mut t.env, &a, PROGRAM, MIN_DELAY, Pubkey::default())
        .expect_code(code(TimelockError::BadAuthor));
    // Only the current upgrade authority.
    let stranger = t.env.funded(1_000_000_000);
    register(&mut t.env, &stranger, PROGRAM, MIN_DELAY, stranger.pubkey())
        .expect_code(code(TimelockError::NotUpgradeable));
    // Studio's key hands a program straight to its creator's timelock (owner decision 3).
    let creator = Keypair::new();
    register(&mut t.env, &a, PROGRAM, MAX_DELAY, creator.pubkey()).ok();
    let lock = timelock(&t.env);
    assert_eq!((lock.author, lock.delay_secs), (creator.pubkey(), MAX_DELAY));
}

#[test]
fn an_immutable_program_or_a_non_program_cant_register() {
    let mut t = world();
    let a = t.author.insecure_clone();
    t.env.set_upgrade_authority(PROGRAM, None);
    register(&mut t.env, &a, PROGRAM, MIN_DELAY, a.pubkey())
        .expect_code(code(TimelockError::NotUpgradeable));
    // A wallet is no program.
    let payer = t.env.payer.insecure_clone();
    let fake = Keypair::new().pubkey();
    let ix = tl::register(payer.pubkey(), a.pubkey(), fake, MIN_DELAY, a.pubkey());
    t.env.send(&[ix], &[&a]).expect_fail();
}

// ---- propose -----------------------------------------------------------------------------------

#[test]
fn a_proposal_hashes_the_code_as_solana_verify_does() {
    let mut t = registered(MIN_DELAY);
    let code_bytes = program_bytes("tax_hook");
    // Extra zeros after the code don't change the hash; the length is the code's.
    let buffer = buffer_for_timelock(&mut t, &code_bytes, 4_096);
    let len = trimmed_len(&code_bytes);
    assert!(len < code_bytes.len(), "a .so ends with zero bytes");
    let tx = propose(&mut t, buffer, len);
    tx.ok();
    let lock = timelock(&t.env);
    assert_eq!(hex(&lock.pending_hash), TAX_HOOK_HASH);
    assert_eq!(hex(&executable_hash(&code_bytes)), TAX_HOOK_HASH);
    assert_eq!(lock.pending_len as usize, len);
    assert_eq!(lock.eta, t.env.now + i64::from(MIN_DELAY));
    println!(
        "propose of {len} bytes: {} CU, {} bytes",
        tx.cu(),
        tx.size
    );
    assert!(tx.cu() < 150_000, "propose {} CU", tx.cu());
    assert!(tx.size < 500);
    // One proposal at a time.
    let other = buffer_for_timelock(&mut t, &code_bytes, 0);
    propose(&mut t, other, len).expect_code(code(TimelockError::ProposalPending));
}

#[test]
fn a_proposal_needs_the_timelocks_buffer_and_the_exact_length() {
    let mut t = registered(MIN_DELAY);
    let code_bytes = program_bytes("half_life");
    let len = trimmed_len(&code_bytes);
    // A buffer still the author's.
    let mine = Pubkey::new_unique();
    put_buffer(&mut t.env, mine, t.author.pubkey(), &code_bytes, 0);
    propose(&mut t, mine, len).expect_code(code(TimelockError::BufferAuthority));
    // An account that is no buffer (the program's own ProgramData, owned by the loader).
    propose(&mut t, programdata_address(&PROGRAM), len)
        .expect_code(code(TimelockError::BufferAuthority));
    // A look-alike not owned by the loader.
    let fake = Pubkey::new_unique();
    put_buffer(&mut t.env, fake, timelock_of(&PROGRAM), &code_bytes, 0);
    let mut acc = t.env.account(&fake).unwrap();
    acc.owner = Pubkey::new_unique();
    t.env.put(fake, acc);
    propose(&mut t, fake, len).expect_code(code(TimelockError::BufferAuthority));
    // The trailing-zero rule: the length is exactly the code's.
    let buffer = buffer_for_timelock(&mut t, &code_bytes, 100);
    for bad in [len - 1, len + 1, len + 100, 1] {
        propose(&mut t, buffer, bad).expect_code(code(TimelockError::BadLength));
    }
    propose(&mut t, buffer, 0).expect_code(code(TimelockError::BadLength));
    propose(&mut t, buffer, MAX_CODE_LEN as usize + 1)
        .expect_code(code(TimelockError::BufferTooLarge));
    // Only the author.
    let stranger = t.env.funded(1_000_000_000);
    t.env
        .send(
            &[tl::propose(stranger.pubkey(), PROGRAM, buffer, len as u32)],
            &[&stranger],
        )
        .expect_code(code(TimelockError::NotAuthor));
    propose(&mut t, buffer, len).ok();
}

#[test]
fn a_proposal_of_the_largest_code_fits_the_compute_budget() {
    let mut t = registered(MIN_DELAY);
    // 2 MiB of code (non-zero), the cap.
    let big: Vec<u8> = (0..MAX_CODE_LEN as usize).map(|i| (i % 251) as u8 | 1).collect();
    let buffer = buffer_for_timelock(&mut t, &big, 0);
    let tx = propose(&mut t, buffer, big.len());
    tx.ok();
    println!("propose of 2 MiB: {} CU", tx.cu());
    assert!(tx.cu() < 1_400_000);
    assert_eq!(timelock(&t.env).pending_hash, executable_hash(&big));
}

// ---- execute -------------------------------------------------------------------------------------

#[test]
fn execute_waits_for_the_delay_and_runs_the_new_code() {
    let mut t = registered(MIN_DELAY);
    poke(&mut t.env).ok();
    let new_code = program_bytes("half_life");
    let buffer = buffer_for_timelock(&mut t, &new_code, 0);
    propose(&mut t, buffer, trimmed_len(&new_code)).ok();
    let buffer_lamports = t.env.lamports(&buffer);
    // half_life is larger than tax_hook: the ProgramData must grow first (anyone may).
    t.env.warp(i64::from(MIN_DELAY) - 1);
    execute(&mut t, buffer).expect_code(code(TimelockError::TooEarly));
    t.env.warp(1);
    let tx = execute(&mut t, buffer);
    tx.expect_fail();
    assert!(tx.logs().iter().any(|l| l.contains("not large enough")), "{:?}", tx.logs());
    let grow = (new_code.len() as u32).saturating_sub(
        (t.env.account(&programdata_address(&PROGRAM)).unwrap().data.len() - 45) as u32,
    );
    grow_programdata(&mut t, grow.max(10_240)).ok();
    // A same-slot extension blocks the upgrade for that slot only.
    let tx = execute(&mut t, buffer);
    tx.expect_fail();
    assert!(tx.logs().iter().any(|l| l.contains("deployed in this block")));
    t.env.warp(1);
    let author_before = t.env.lamports(&t.author.pubkey());
    let tx = execute(&mut t, buffer);
    tx.ok();
    println!("execute: {} CU, {} bytes", tx.cu(), tx.size);
    assert!(tx.size < 800, "execute is {} bytes", tx.size);
    // The code is the proposal's; the buffer's lamports went to the author (the spill).
    assert_eq!(programdata_hash(&t.env, &PROGRAM), executable_hash(&new_code));
    assert!(t.env.account(&buffer).is_none_or(|a| a.lamports == 0));
    assert!(t.env.lamports(&t.author.pubkey()) >= author_before + buffer_lamports / 2);
    assert_eq!(upgrade_authority(&t.env, &PROGRAM), Some(timelock_of(&PROGRAM)));
    let lock = timelock(&t.env);
    assert!(!lock.has_pending());
    assert_eq!((lock.upgrades, lock.last_upgraded_at), (1, t.env.now));
    // The program runs the new code: half_life's, which refuses tax_hook's id.
    t.env.warp(1);
    poke(&mut t.env).expect_code(DECLARED_ID_MISMATCH);
    // Back to tax_hook's code, through the timelock again.
    let old = program_bytes("tax_hook");
    let buffer = buffer_for_timelock(&mut t, &old, 0);
    propose(&mut t, buffer, trimmed_len(&old)).ok();
    t.env.warp(i64::from(MIN_DELAY));
    execute(&mut t, buffer).ok();
    t.env.warp(1);
    poke(&mut t.env).ok();
    assert_eq!(hex(&programdata_hash(&t.env, &PROGRAM)), TAX_HOOK_HASH);
}

#[test]
fn execute_is_refused_after_its_window_and_expire_refunds_the_author() {
    let mut t = registered(MIN_DELAY);
    let new_code = program_bytes("tax_hook");
    let buffer = buffer_for_timelock(&mut t, &new_code, 0);
    propose(&mut t, buffer, trimmed_len(&new_code)).ok();
    let sender = t.env.funded(1_000_000_000);
    let author = t.author.pubkey();
    // Not before the window has passed.
    t.env.warp(i64::from(MIN_DELAY) + EXECUTE_WINDOW - 1);
    t.env
        .send(&[tl::expire(sender.pubkey(), PROGRAM, buffer, author)], &[&sender])
        .expect_code(code(TimelockError::NotExpired));
    t.env.warp(1);
    execute(&mut t, buffer).expect_code(code(TimelockError::Expired));
    let before = t.env.lamports(&author);
    let lamports = t.env.lamports(&buffer);
    // Anyone expires it; the lamports go to the author, never to the sender.
    let sender_before = t.env.lamports(&sender.pubkey());
    t.env
        .send(&[tl::expire(sender.pubkey(), PROGRAM, buffer, author)], &[&sender])
        .ok();
    assert_eq!(t.env.lamports(&author), before + lamports);
    assert!(t.env.lamports(&sender.pubkey()) <= sender_before);
    assert!(!timelock(&t.env).has_pending());
    assert_eq!(t.env.lamports(&buffer), 0);
    // The expire can't name another recipient.
    let buffer = buffer_for_timelock(&mut t, &new_code, 0);
    propose(&mut t, buffer, trimmed_len(&new_code)).ok();
    t.env.warp(i64::from(MIN_DELAY) + EXECUTE_WINDOW);
    t.env
        .send(
            &[tl::expire(sender.pubkey(), PROGRAM, buffer, sender.pubkey())],
            &[&sender],
        )
        .expect_code(code(TimelockError::NotAuthor));
}

#[test]
fn cancel_and_reclaim_refund_the_author() {
    let mut t = registered(MIN_DELAY);
    let code_bytes = program_bytes("tax_hook");
    let buffer = buffer_for_timelock(&mut t, &code_bytes, 0);
    propose(&mut t, buffer, trimmed_len(&code_bytes)).ok();
    let a = t.author.insecure_clone();
    let stranger = t.env.funded(1_000_000_000);
    // Only the author cancels.
    t.env
        .send(&[tl::cancel(stranger.pubkey(), PROGRAM, buffer)], &[&stranger])
        .expect_fail();
    let before = t.env.lamports(&a.pubkey());
    let lamports = t.env.lamports(&buffer);
    let tx = t.env.send(&[tl::cancel(a.pubkey(), PROGRAM, buffer)], &[&a]);
    tx.ok();
    assert_eq!(t.env.lamports(&a.pubkey()), before + lamports);
    assert!(!timelock(&t.env).has_pending());
    t.env
        .send(&[tl::cancel(a.pubkey(), PROGRAM, buffer)], &[&a])
        .expect_fail();
    // A stray buffer handed to the timelock by mistake: the author reclaims it.
    let stray = buffer_for_timelock(&mut t, &code_bytes, 0);
    let pending = buffer_for_timelock(&mut t, &code_bytes, 0);
    propose(&mut t, pending, trimmed_len(&code_bytes)).ok();
    t.env
        .send(&[tl::reclaim_buffer(a.pubkey(), PROGRAM, pending)], &[&a])
        .expect_code(code(TimelockError::WrongBuffer));
    t.env
        .send(&[tl::reclaim_buffer(stranger.pubkey(), PROGRAM, stray)], &[&stranger])
        .expect_code(code(TimelockError::NotAuthor));
    let before = t.env.lamports(&a.pubkey());
    let lamports = t.env.lamports(&stray);
    t.env
        .send(&[tl::reclaim_buffer(a.pubkey(), PROGRAM, stray)], &[&a])
        .ok();
    assert_eq!(t.env.lamports(&a.pubkey()), before + lamports);
    // A buffer that is not the timelock's can't be reclaimed through it.
    let other = Pubkey::new_unique();
    put_buffer(&mut t.env, other, a.pubkey(), &code_bytes, 0);
    t.env
        .send(&[tl::reclaim_buffer(a.pubkey(), PROGRAM, other)], &[&a])
        .expect_code(code(TimelockError::BufferAuthority));
}

// ---- lengthen, hand over, finalize ------------------------------------------------------------------

#[test]
fn lengthen_moves_a_pending_eta_and_never_shortens() {
    let mut t = registered(MIN_DELAY);
    let a = t.author.insecure_clone();
    let code_bytes = program_bytes("tax_hook");
    let buffer = buffer_for_timelock(&mut t, &code_bytes, 0);
    propose(&mut t, buffer, trimmed_len(&code_bytes)).ok();
    let proposed_at = timelock(&t.env).proposed_at;
    t.env
        .send(&[tl::lengthen(a.pubkey(), PROGRAM, MIN_DELAY - 1)], &[&a])
        .expect_code(code(TimelockError::DelayShortened));
    t.env
        .send(&[tl::lengthen(a.pubkey(), PROGRAM, MAX_DELAY + 1)], &[&a])
        .expect_code(code(TimelockError::DelayTooLong));
    let week = 7 * 86_400u32;
    t.env
        .send(&[tl::lengthen(a.pubkey(), PROGRAM, week)], &[&a])
        .ok();
    let lock = timelock(&t.env);
    assert_eq!((lock.delay_secs, lock.eta), (week, proposed_at + i64::from(week)));
    // The old eta no longer executes.
    t.env.warp(i64::from(MIN_DELAY));
    execute(&mut t, buffer).expect_code(code(TimelockError::TooEarly));
    t.env
        .send(&[tl::lengthen(a.pubkey(), PROGRAM, MIN_DELAY)], &[&a])
        .expect_code(code(TimelockError::DelayShortened));
    // Equal is allowed (a no-op), and only the author lengthens.
    t.env
        .send(&[tl::lengthen(a.pubkey(), PROGRAM, week)], &[&a])
        .ok();
    let stranger = t.env.funded(1_000_000_000);
    t.env
        .send(&[tl::lengthen(stranger.pubkey(), PROGRAM, MAX_DELAY)], &[&stranger])
        .expect_code(code(TimelockError::NotAuthor));
    t.env.warp(i64::from(week) - i64::from(MIN_DELAY));
    execute(&mut t, buffer).ok();
}

#[test]
fn the_author_role_moves_in_two_steps() {
    let mut t = registered(MIN_DELAY);
    let a = t.author.insecure_clone();
    let b = t.env.funded(1_000_000_000);
    let c = t.env.funded(1_000_000_000);
    t.env
        .send(&[tl::accept_author(b.pubkey(), PROGRAM)], &[&b])
        .expect_code(code(TimelockError::NotPendingAuthor));
    t.env
        .send(&[tl::propose_author(a.pubkey(), PROGRAM, b.pubkey())], &[&a])
        .ok();
    t.env
        .send(&[tl::accept_author(c.pubkey(), PROGRAM)], &[&c])
        .expect_code(code(TimelockError::NotPendingAuthor));
    t.env
        .send(&[tl::accept_author(b.pubkey(), PROGRAM)], &[&b])
        .ok();
    let lock = timelock(&t.env);
    assert_eq!((lock.author, lock.pending_author), (b.pubkey(), Pubkey::default()));
    // The old author can do nothing more.
    t.env
        .send(&[tl::lengthen(a.pubkey(), PROGRAM, MAX_DELAY)], &[&a])
        .expect_code(code(TimelockError::NotAuthor));
    // Timing is unchanged; spills now go to the new author.
    assert_eq!(lock.delay_secs, MIN_DELAY);
}

#[test]
fn finalize_makes_the_program_immutable() {
    let mut t = registered(MIN_DELAY);
    let a = t.author.insecure_clone();
    let code_bytes = program_bytes("tax_hook");
    let buffer = buffer_for_timelock(&mut t, &code_bytes, 0);
    propose(&mut t, buffer, trimmed_len(&code_bytes)).ok();
    t.env
        .send(&[tl::finalize(a.pubkey(), PROGRAM)], &[&a])
        .expect_code(code(TimelockError::ProposalPending));
    t.env.send(&[tl::cancel(a.pubkey(), PROGRAM, buffer)], &[&a]).ok();
    let stranger = t.env.funded(1_000_000_000);
    t.env
        .send(&[tl::finalize(stranger.pubkey(), PROGRAM)], &[&stranger])
        .expect_code(code(TimelockError::NotAuthor));
    let tx = t.env.send(&[tl::finalize(a.pubkey(), PROGRAM)], &[&a]);
    tx.ok();
    assert_eq!(upgrade_authority(&t.env, &PROGRAM), None);
    assert!(timelock(&t.env).finalized);
    poke(&mut t.env).ok();
    // Nothing more: no proposal, no lengthening, no second finalize.
    let buffer = buffer_for_timelock(&mut t, &code_bytes, 0);
    propose(&mut t, buffer, trimmed_len(&code_bytes)).expect_code(code(TimelockError::Finalized));
    t.env
        .send(&[tl::lengthen(a.pubkey(), PROGRAM, MAX_DELAY)], &[&a])
        .expect_code(code(TimelockError::Finalized));
    t.env
        .send(&[tl::finalize(a.pubkey(), PROGRAM)], &[&a])
        .expect_code(code(TimelockError::Finalized));
    // The stray buffer can still be reclaimed.
    t.env
        .send(&[tl::reclaim_buffer(a.pubkey(), PROGRAM, buffer)], &[&a])
        .ok();
}

// ---- the class readers see ----------------------------------------------------------------------

#[test]
fn readers_class_a_registered_program_as_timelocked() {
    let mut t = registered(5 * 86_400);
    let pd = programdata_address(&PROGRAM);
    let lock_key = timelock_of(&PROGRAM);
    let mut prog = t.env.account(&PROGRAM).unwrap();
    let mut pdata = t.env.account(&pd).unwrap();
    let mut lock = t.env.account(&lock_key).unwrap();
    let (mut l1, mut l2, mut l3) = (prog.lamports, pdata.lamports, lock.lamports);
    let p = anchor_lang::prelude::AccountInfo::new(
        &PROGRAM,
        false,
        false,
        &mut l1,
        &mut prog.data,
        &BPF_LOADER_UPGRADEABLE_ID,
        true,
    );
    let d = anchor_lang::prelude::AccountInfo::new(
        &pd,
        false,
        false,
        &mut l2,
        &mut pdata.data,
        &BPF_LOADER_UPGRADEABLE_ID,
        false,
    );
    let l = anchor_lang::prelude::AccountInfo::new(
        &lock_key,
        false,
        false,
        &mut l3,
        &mut lock.data,
        &hook_timelock::ID,
        false,
    );
    assert_eq!(
        classify(&p, &d, Some(&l)),
        Ok(AuthorityClass::Timelocked {
            delay_secs: 5 * 86_400
        })
    );
    let _ = &mut t;
}

// ---- a multisig author, through CPI -----------------------------------------------------------

#[test]
fn an_author_that_is_a_program_acts_through_cpi() {
    let mut t = world();
    // hook_tester's ["hook-authority"] PDA stands for a multisig's vault: it signs only by CPI.
    let (vault, bump) = bordrless_hook::hook_authority(&hook_tester::ID);
    t.env.set_upgrade_authority(PROGRAM, Some(vault));
    let via = |ix: Instruction| -> Instruction {
        let mut accounts = vec![
            AccountMeta::new_readonly(vault, false),
            AccountMeta::new_readonly(ix.program_id, false),
        ];
        accounts.extend(ix.accounts.iter().map(|m| AccountMeta {
            pubkey: m.pubkey,
            is_signer: m.is_signer && m.pubkey != vault,
            is_writable: m.is_writable,
        }));
        Instruction {
            program_id: hook_tester::ID,
            accounts,
            data: hook_tester::instruction::InvokeAsHook { bump, data: ix.data }.data(),
        }
    };
    let payer = t.env.payer.insecure_clone();
    t.env
        .send(
            &[via(tl::register(payer.pubkey(), vault, PROGRAM, MIN_DELAY, vault))],
            &[],
        )
        .ok();
    assert_eq!(timelock(&t.env).author, vault);
    let code_bytes = program_bytes("tax_hook");
    let buffer = Pubkey::new_unique();
    put_buffer(&mut t.env, buffer, timelock_of(&PROGRAM), &code_bytes, 0);
    let tx = t.env.send(
        &[via(tl::propose(vault, PROGRAM, buffer, trimmed_len(&code_bytes) as u32))],
        &[],
    );
    tx.ok();
    println!("propose through a multisig: height {}", tx.max_height());
    t.env.warp(i64::from(MIN_DELAY));
    execute(&mut t, buffer).ok();
    t.env.send(&[via(tl::finalize(vault, PROGRAM))], &[]).ok();
    assert_eq!(upgrade_authority(&t.env, &PROGRAM), None);
}

// ---- the invariant, over random sequences ---------------------------------------------------------

/// A deterministic xorshift.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn random_sequences_never_leave_an_authority_but_the_timelock_or_none() {
    for seed in 1..=6u64 {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ seed.wrapping_mul(0x1234_5678_9abc));
        let mut t = registered(MIN_DELAY);
        let pda = timelock_of(&PROGRAM);
        let a = t.author.insecure_clone();
        let b = t.env.funded(1_000_000_000);
        let stranger = t.env.funded(1_000_000_000);
        let code_bytes = program_bytes("tax_hook");
        let mut buffers: Vec<Pubkey> = vec![];
        for _ in 0..40 {
            let lock = timelock(&t.env);
            let who = match rng.next() % 3 {
                0 => a.insecure_clone(),
                1 => b.insecure_clone(),
                _ => stranger.insecure_clone(),
            };
            let pending = lock.pending_buffer;
            let any_buffer = buffers
                .get((rng.next() as usize) % buffers.len().max(1))
                .copied()
                .unwrap_or_else(Pubkey::new_unique);
            let ix = match rng.next() % 10 {
                0 => {
                    let buf = Pubkey::new_unique();
                    put_buffer(&mut t.env, buf, pda, &code_bytes, (rng.next() % 64) as usize);
                    buffers.push(buf);
                    tl::propose(who.pubkey(), PROGRAM, buf, trimmed_len(&code_bytes) as u32)
                }
                1 => tl::cancel(who.pubkey(), PROGRAM, pending),
                2 => tl::execute(who.pubkey(), PROGRAM, pending, lock.author),
                3 => tl::expire(who.pubkey(), PROGRAM, pending, lock.author),
                4 => tl::reclaim_buffer(who.pubkey(), PROGRAM, any_buffer),
                5 => tl::lengthen(who.pubkey(), PROGRAM, MIN_DELAY + (rng.next() % 10) as u32 * 86_400),
                6 => tl::propose_author(who.pubkey(), PROGRAM, b.pubkey()),
                7 => tl::accept_author(who.pubkey(), PROGRAM),
                8 => tl::finalize(who.pubkey(), PROGRAM),
                _ => {
                    // A raw loader SetAuthority attempt by whoever, on the program or a buffer.
                    let target = if rng.next().is_multiple_of(2) {
                        programdata_address(&PROGRAM)
                    } else {
                        any_buffer
                    };
                    loader::set_authority(target, who.pubkey(), Some(who.pubkey()))
                }
            };
            let _ = t.env.send(&[ix], &[&who]);
            t.env.warp((rng.next() % (12 * DAY as u64)) as i64);
            let authority = upgrade_authority(&t.env, &PROGRAM);
            assert!(
                authority == Some(pda) || authority.is_none(),
                "seed {seed}: the program's authority is {authority:?}"
            );
            if authority.is_none() {
                assert!(timelock(&t.env).finalized);
            }
            for buf in &buffers {
                if let Some(acc) = t.env.account(buf) {
                    if acc.lamports > 0 {
                        assert_eq!(
                            buffer_authority(&t.env, buf),
                            Some(pda),
                            "seed {seed}: a buffer left the timelock"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn the_timelock_account_reads_as_its_layout() {
    let t = registered(MIN_DELAY);
    let raw = t.env.account(&timelock_of(&PROGRAM)).unwrap();
    let view = bordrless_hook::authority::parse_timelock(&raw.data).expect("parses");
    let lock = Timelock::try_deserialize(&mut &raw.data[..]).unwrap();
    assert_eq!(
        (view.program, view.author, view.delay_secs, view.bump),
        (lock.program, lock.author, lock.delay_secs, lock.bump)
    );
    assert_eq!(raw.data.len(), Timelock::LEN);
}
