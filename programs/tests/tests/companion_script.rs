//! Script companions (2026-10-12): a builder's own program, the script, decides what a companion's
//! pot does, once Bordrless has approved its code (`approve`, bound to the code's hash). The
//! companion keeps custody and lets the script act with the creator address signing, then checks
//! every step: the pot spent and nothing else set aside, the dev bag and the creator address's
//! lamports kept, custody unchanged (no delegate, no closed holding, no reassigned creator address,
//! no fee claim inside a step).
//!
//! The script is `fixtures/scripts/example`, whose steps do what its state's mode says: a buy or a
//! sell on the launch pool, nothing, or a misdeed the companion must refuse. It refuses any call in
//! which an account other than the creator (and, at its init, the payer) signs, so every passing
//! step also shows that neither the mint's nor the sender's signature reached it.
//!
//! Only Bordrless controls an approved script's code (Bordrless builds and deploys every approved
//! script, with its own upgrade key): here the script is upgradeable by Bordrless's key, and its
//! upgrades are sent as that key (sigverify off). A script's author earns half of what every fee
//! claim of a companion made with it takes from its launch, unless they launched it themselves.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{AccountDeserialize, AccountSerialize};
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{
    CreateArgs, CreateGameArgs, CreateScriptArgs, CreateSnowballArgs,
};
use bordrless_companion::state::{Approval, Companion, GameKind, Script, Split};
use bordrless_core::policy;
use bordrless_hook::authority::{programdata_address, MIN_ACCEPTED_DELAY};
use bordrless_launch::client::{self as launch, LaunchKeys};
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::program_bytes;
use bordrless_program_tests::timelock::{
    programdata_hash, programdata_slot, put_buffer, register, send_as, timelock_of,
    upgrade_authority,
};
use bordrless_token::client as token;
use bordrless_token::state::Holding;
use hook_timelock::loader;
use lottery_hook::client as lottery;
use script_example as se;
use solana_keypair::Keypair;
use solana_signer::Signer;

const SCRIPT: Pubkey = se::ID;
const CREATOR_FEE: u16 = 200;
/// What the example script's buys spend.
const BUY: u64 = SOL / 20;
/// The size of the state the companion makes for the example script.
const STATE_LEN: u32 = 64;
/// Bordrless's upgrade key (the protocol's, one of those the launchpad takes for a custom hook):
/// approved scripts are deployed under it (or are immutable, or timelocked).
const BORDRLESS_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[1];

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

/// A world with the example script loaded, deployed by Bordrless (upgradeable by Bordrless's key),
/// and its author: a wallet of their own.
fn script_world() -> (World, Keypair) {
    let mut w = World::new();
    w.env
        .svm
        .add_program(SCRIPT, &program_bytes("script_example"))
        .expect("load the script");
    w.env.set_upgrade_authority(SCRIPT, Some(BORDRLESS_KEY));
    let author = w.env.funded(SOL);
    (w, author)
}

/// Bordrless approves the script's code as it is now, as `kind`, naming `author`.
fn approve_by(w: &mut World, kind: u8, author: Pubkey) -> Tx {
    let deployer = w.env.deployer.insecure_clone();
    let hash = programdata_hash(&w.env, &SCRIPT);
    w.env.send(
        &[companion::approve(
            deployer.pubkey(),
            SCRIPT,
            kind,
            hash,
            author,
        )],
        &[&deployer],
    )
}

/// Bordrless approves the script's code as it is now, as `kind`, naming no author (nobody is paid
/// a share of the fees).
fn approve(w: &mut World, kind: u8) -> Tx {
    approve_by(w, kind, Pubkey::default())
}

fn revoke(w: &mut World) -> Tx {
    let deployer = w.env.deployer.insecure_clone();
    w.env.send(
        &[companion::revoke_approval(deployer.pubkey(), SCRIPT)],
        &[&deployer],
    )
}

fn create_args() -> CreateArgs {
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
    }
}

/// 70% of every fee claim to the pot, 30% to the launcher; a step a minute at most.
fn terms(init: Vec<u8>) -> CreateScriptArgs {
    CreateScriptArgs {
        split: Split {
            buyback_bps: 0,
            holders_bps: 0,
            beneficiary_bps: 3_000,
        },
        pot_bps: 7_000,
        program: SCRIPT,
        interval: 60,
        state_len: STATE_LEN,
        init_data: init,
    }
}

/// The accounts the script's init takes after its four (`[creator, mint, state, system program]`):
/// none (the companion makes its state and hands it over in the prefix).
fn init_accounts(_mint: &Pubkey) -> Vec<AccountMeta> {
    vec![]
}

/// `create` then `create_script` for a fresh mint (`launcher` paying), with `extra` as the init's
/// accounts. Answers the mint and the transaction.
fn make_with(
    w: &mut World,
    launcher: &Keypair,
    args: CreateScriptArgs,
    extra: impl FnOnce(&Pubkey) -> Vec<AccountMeta>,
) -> (Keypair, Tx) {
    let mint = Keypair::new();
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            mint.pubkey(),
            create_args(),
        ),
        companion::create_script(
            launcher.pubkey(),
            mint.pubkey(),
            args,
            extra(&mint.pubkey()),
        ),
    ];
    let tx = w.env.send_paid_by(&ixs, launcher, &[&mint]);
    (mint, tx)
}

fn make(w: &mut World, launcher: &Keypair, args: CreateScriptArgs) -> (Keypair, Tx) {
    make_with(w, launcher, args, init_accounts)
}

/// The launch of `mint` through its companion: no kit rules, no custom hook.
fn launch_it(w: &mut World, launcher: &Keypair, mint: &Keypair) -> Tx {
    let creator = companion::creator_address(&mint.pubkey());
    let launch_args = World::launch_args("SCRP", CREATOR_FEE, VQ, LaunchRules::NONE);
    let inner = launch::create_launch_with(
        creator,
        mint.pubkey(),
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        launch_args.clone(),
        None,
        None,
    );
    let ix = companion::launch_script(launcher.pubkey(), mint.pubkey(), &inner, launch_args);
    w.env.send_paid_by(&[ix], launcher, &[mint])
}

/// A script companion made and launched (its script approved before), its init storing `init`.
fn launched(w: &mut World, launcher: &Keypair, init: Vec<u8>) -> Pubkey {
    let (mint, tx) = make(w, launcher, terms(init));
    tx.ok();
    launch_it(w, launcher, &mint).ok();
    mint.pubkey()
}

fn keys(w: &World, mint: &Pubkey) -> LaunchKeys {
    LaunchKeys::of(&w.launch(mint))
}

fn companion_of(w: &World, mint: &Pubkey) -> Companion {
    w.env.read(&companion::companion_address(mint))
}

fn script_of(w: &World, mint: &Pubkey) -> Script {
    w.env.read(&companion::script_address(mint))
}

/// `ix`'s accounts and program as remaining accounts, nobody a signer.
fn metas(ix: &Instruction) -> Vec<AccountMeta> {
    let mut m: Vec<AccountMeta> = ix
        .accounts
        .iter()
        .map(|a| AccountMeta {
            pubkey: a.pubkey,
            is_signer: false,
            is_writable: a.is_writable,
        })
        .collect();
    m.push(AccountMeta::new_readonly(ix.program_id, false));
    m
}

/// What a step of the example script takes after its six (`[creator, mint, launch, companion, pool,
/// state]`): the attacker's account (`aux`), then what its mode's call needs.
fn step_accounts(w: &World, mint: &Pubkey, mode: u8, aux: &Pubkey) -> Vec<AccountMeta> {
    let creator = companion::creator_address(mint);
    let sol = w.sol;
    let call = match mode {
        se::MODE_BUY | se::MODE_SELL => {
            Some(launch::swap(&keys(w, mint), creator, creator, 1, 0, 0))
        }
        se::MODE_TAKE_SOL => Some(token::transfer_with(
            creator,
            token::holding_address(&sol, &creator),
            *aux,
            sol,
            None,
            vec![],
            0,
        )),
        se::MODE_TAKE_TOKENS => Some(token::transfer_with(
            creator,
            token::holding_address(mint, &creator),
            *aux,
            *mint,
            None,
            vec![],
            0,
        )),
        se::MODE_DELEGATE => Some(token::approve(
            creator,
            token::holding_address(&sol, &creator),
            *aux,
            0,
        )),
        se::MODE_CLAIM => Some(launch::claim_creator_fees(creator, *mint, sol)),
        _ => None,
    };
    let mut extra = vec![AccountMeta::new(*aux, false)];
    if let Some(ix) = call {
        extra.extend(metas(&ix));
    }
    extra
}

/// Sets the script's mode for `mint` (the first bytes of its state rewritten).
fn set_mode(w: &mut World, mint: &Pubkey, mode: u8, param: u64) {
    let key = se::state_address(mint);
    let mut a = w.env.account(&key).expect("the script's state");
    let bytes = se::state_bytes(mode, param);
    a.data[..bytes.len()].copy_from_slice(&bytes);
    w.env.put(key, a);
}

/// A step of `mint`'s script in `mode` (with `param`), `aux` the attacker's account, sent (and
/// paid) by `cranker`.
fn step(w: &mut World, cranker: &Keypair, mint: &Pubkey, mode: u8, param: u64, aux: &Pubkey) -> Tx {
    set_mode(w, mint, mode, param);
    let ix = companion::script_step(
        cranker.pubkey(),
        &keys(w, mint),
        SCRIPT,
        step_accounts(w, mint, mode, aux),
    );
    w.env.send_paid_by(&[ix], cranker, &[])
}

/// Trades enough to accrue creator fees: buys and sells by a few wallets.
fn trade(w: &mut World, mint: &Pubkey, wallets: usize, sol: u64) {
    for _ in 0..wallets {
        let t = w.wallet_with_sol(sol + SOL);
        w.buy(&t, mint, sol).ok();
        let held = w.env.holding(mint, &t.pubkey());
        w.sell(&t, mint, held / 4).ok();
    }
}

/// Fees into the pot: trades, then a claim (its pot's part credited whole). Answers a cranker.
fn fill_pot(w: &mut World, mint: &Pubkey) -> Keypair {
    w.env.warp(31);
    trade(w, mint, 4, 2 * SOL);
    let cranker = w.wallet_with_sol(SOL);
    let tx = w.env.send_paid_by(
        &[companion::claim_fees_script(cranker.pubkey(), *mint, None)],
        &cranker,
        &[],
    );
    tx.ok();
    let funded: PotFunded = tx.event();
    let claimed: FeesClaimed = tx.event();
    assert!(funded.to_pot > BUY + SCRIPT_BOUNTY, "{}", funded.to_pot);
    assert_eq!(funded.to_buyback, 0);
    assert!(claimed.to_beneficiary > 0);
    let c = companion_of(w, mint);
    assert_eq!(c.pending_pot, funded.to_pot);
    assert_eq!(c.pending_beneficiary, claimed.to_beneficiary);
    cranker
}

/// The script upgraded by Bordrless to `code` (the ProgramData grown first when it must be), sent as
/// Bordrless's upgrade key, whose secret the suite does not hold (sigverify off:
/// `Env::without_sigverify`).
fn upgrade(w: &mut World, code: &[u8]) {
    let pd = programdata_address(&SCRIPT);
    let have = w.env.account(&pd).expect("programdata").data.len() - 45;
    if code.len() > have {
        let payer = w.env.payer.pubkey();
        w.env
            .send(
                &[loader::extend_program(
                    pd,
                    SCRIPT,
                    payer,
                    ((code.len() - have) as u32).max(10_240),
                )],
                &[],
            )
            .ok();
        w.env.warp(1);
    }
    let buffer = Pubkey::new_unique();
    put_buffer(&mut w.env, buffer, BORDRLESS_KEY, code, 0);
    let spill = w.env.payer.pubkey();
    w.env.svm.expire_blockhash();
    send_as(
        &mut w.env,
        &[loader::upgrade(pd, SCRIPT, buffer, spill, BORDRLESS_KEY)],
        &[],
        &[BORDRLESS_KEY],
    )
    .ok();
    w.env.warp(1);
}

#[test]
fn approvals_are_bound_to_code_and_written_only_by_the_protocol() {
    let (mut w, _author) = script_world();
    let deployer = w.env.deployer.insecure_clone();
    let hash = programdata_hash(&w.env, &SCRIPT);
    // A hash that is not the code's.
    let mut wrong = hash;
    wrong[0] ^= 1;
    refused(
        &w.env.send(
            &[companion::approve(
                deployer.pubkey(),
                SCRIPT,
                APPROVAL_KIND_SCRIPT,
                wrong,
                Pubkey::default(),
            )],
            &[&deployer],
        ),
        CompanionError::BadApproval,
    );
    // A kind that is neither a token hook nor a script.
    refused(
        &w.env.send(
            &[companion::approve(
                deployer.pubkey(),
                SCRIPT,
                2,
                hash,
                Pubkey::default(),
            )],
            &[&deployer],
        ),
        CompanionError::BadApproval,
    );
    // None of the protocol's programs is ever a script (it would run with a creator address
    // signing).
    let token_hash = programdata_hash(&w.env, &bordrless_token::ID);
    refused(
        &w.env.send(
            &[companion::approve(
                deployer.pubkey(),
                bordrless_token::ID,
                APPROVAL_KIND_SCRIPT,
                token_hash,
                Pubkey::default(),
            )],
            &[&deployer],
        ),
        CompanionError::BadApproval,
    );
    // Only the companion program's upgrade authority approves.
    let stranger = w.env.funded(SOL);
    refused(
        &w.env.send(
            &[companion::approve(
                stranger.pubkey(),
                SCRIPT,
                APPROVAL_KIND_SCRIPT,
                hash,
                Pubkey::default(),
            )],
            &[&stranger],
        ),
        CompanionError::NotProtocolAuthority,
    );
    assert!(w
        .env
        .account(&companion::approval_address(&SCRIPT))
        .is_none());

    // The approval: the code's hash and the ProgramData's slot, recomputed on chain.
    let tx = approve(&mut w, APPROVAL_KIND_SCRIPT);
    tx.ok();
    let ev: ProgramApproved = tx.event();
    let a: Approval = w.env.read(&companion::approval_address(&SCRIPT));
    assert_eq!(
        (a.version, a.program, a.kind, a.code_hash, a.revoked),
        (APPROVAL_VERSION, SCRIPT, APPROVAL_KIND_SCRIPT, hash, false)
    );
    assert_eq!(
        (a.updated_by, a.approved_at, a.programdata_slot),
        (
            deployer.pubkey(),
            w.env.now,
            programdata_slot(&w.env, &SCRIPT)
        )
    );
    assert_eq!(
        (
            ev.program,
            ev.kind,
            ev.code_hash,
            ev.programdata_slot,
            ev.authority
        ),
        (
            SCRIPT,
            APPROVAL_KIND_SCRIPT,
            hash,
            a.programdata_slot,
            deployer.pubkey()
        )
    );
    assert_eq!(
        w.env
            .account(&companion::approval_address(&SCRIPT))
            .unwrap()
            .data
            .len(),
        Approval::LEN
    );
    // The program named must be the program passed (and the approval its own), both ways.
    let mut other = companion::approve(
        deployer.pubkey(),
        bordrless_token::ID,
        APPROVAL_KIND_SCRIPT,
        hash,
        Pubkey::default(),
    );
    other.data = companion::approve(
        deployer.pubkey(),
        SCRIPT,
        APPROVAL_KIND_SCRIPT,
        hash,
        Pubkey::default(),
    )
    .data;
    refused(
        &w.env.send(&[other], &[&deployer]),
        CompanionError::ProgramAccounts,
    );
    let mut other = companion::revoke_approval(deployer.pubkey(), SCRIPT);
    other.data = companion::revoke_approval(deployer.pubkey(), bordrless_token::ID).data;
    refused(
        &w.env.send(&[other], &[&deployer]),
        CompanionError::ProgramAccounts,
    );

    // Only the upgrade authority revokes.
    refused(
        &w.env.send(
            &[companion::revoke_approval(stranger.pubkey(), SCRIPT)],
            &[&stranger],
        ),
        CompanionError::NotProtocolAuthority,
    );
    let tx = revoke(&mut w);
    tx.ok();
    let ev: ApprovalRevoked = tx.event();
    assert_eq!(
        (ev.program, ev.kind, ev.code_hash, ev.authority),
        (SCRIPT, APPROVAL_KIND_SCRIPT, hash, deployer.pubkey())
    );
    assert!(
        w.env
            .read::<Approval>(&companion::approval_address(&SCRIPT))
            .revoked
    );
    // Approved again (here as a token hook, for the site): rewritten, no longer revoked.
    w.env.warp(1);
    approve(&mut w, APPROVAL_KIND_TOKEN_HOOK).ok();
    let a: Approval = w.env.read(&companion::approval_address(&SCRIPT));
    assert_eq!((a.kind, a.revoked), (APPROVAL_KIND_TOKEN_HOOK, false));
}

/// The script approver (`SCRIPT_APPROVER`, a hot key of Bordrless's backend the suites never hold:
/// sent with sigverify off and its signature left empty, `send_as`) writes and revokes approvals as
/// the protocol's upgrade authority does, under the same rules, and each rewrites the other's. Any
/// other key sent the same way is refused: it is the address that is checked.
#[test]
fn the_script_approver_writes_and_revokes_approvals_too() {
    fn as_approver(w: &mut World, ix: Instruction) -> Tx {
        send_as(&mut w.env, &[ix], &[], &[SCRIPT_APPROVER])
    }
    let (mut w, author) = script_world();
    w.env.without_sigverify();
    w.env.fund(SCRIPT_APPROVER, SOL);
    let deployer = w.env.deployer.insecure_clone();
    let hash = programdata_hash(&w.env, &SCRIPT);
    let approval = companion::approval_address(&SCRIPT);
    // The same rules: the hash must be the code's.
    let mut wrong = hash;
    wrong[0] ^= 1;
    refused(
        &as_approver(
            &mut w,
            companion::approve(
                SCRIPT_APPROVER,
                SCRIPT,
                APPROVAL_KIND_SCRIPT,
                wrong,
                author.pubkey(),
            ),
        ),
        CompanionError::BadApproval,
    );
    assert!(w.env.account(&approval).is_none());
    // Approved by the approver: recorded as the authority's is, naming the approver.
    let tx = as_approver(
        &mut w,
        companion::approve(
            SCRIPT_APPROVER,
            SCRIPT,
            APPROVAL_KIND_SCRIPT,
            hash,
            author.pubkey(),
        ),
    );
    tx.ok();
    let a: Approval = w.env.read(&approval);
    assert_eq!(
        (
            a.version,
            a.program,
            a.kind,
            a.code_hash,
            a.revoked,
            a.author
        ),
        (
            APPROVAL_VERSION,
            SCRIPT,
            APPROVAL_KIND_SCRIPT,
            hash,
            false,
            author.pubkey()
        )
    );
    assert_eq!(
        (a.updated_by, a.approved_at, a.programdata_slot),
        (
            SCRIPT_APPROVER,
            w.env.now,
            programdata_slot(&w.env, &SCRIPT)
        )
    );
    assert_eq!(tx.event::<ProgramApproved>().authority, SCRIPT_APPROVER);
    // It counts like the authority's: a script companion is made with it, its author recorded.
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = launched(&mut w, &launcher, se::state_bytes(se::MODE_NOTHING, 0));
    assert_eq!(script_of(&w, &mint).author, author.pubkey());
    // Revoked by the approver.
    let tx = as_approver(&mut w, companion::revoke_approval(SCRIPT_APPROVER, SCRIPT));
    tx.ok();
    assert_eq!(tx.event::<ApprovalRevoked>().authority, SCRIPT_APPROVER);
    let a: Approval = w.env.read(&approval);
    assert_eq!((a.revoked, a.updated_by), (true, SCRIPT_APPROVER));
    // Each rewrites the other's.
    w.env.warp(1);
    approve(&mut w, APPROVAL_KIND_SCRIPT).ok();
    let a: Approval = w.env.read(&approval);
    assert_eq!((a.revoked, a.updated_by), (false, deployer.pubkey()));
    as_approver(&mut w, companion::revoke_approval(SCRIPT_APPROVER, SCRIPT)).ok();
    assert!(w.env.read::<Approval>(&approval).revoked);
    w.env.warp(1);
    as_approver(
        &mut w,
        companion::approve(
            SCRIPT_APPROVER,
            SCRIPT,
            APPROVAL_KIND_TOKEN_HOOK,
            hash,
            Pubkey::default(),
        ),
    )
    .ok();
    let a: Approval = w.env.read(&approval);
    assert_eq!(
        (a.kind, a.revoked, a.updated_by),
        (APPROVAL_KIND_TOKEN_HOOK, false, SCRIPT_APPROVER)
    );
    revoke(&mut w).ok();
    let a: Approval = w.env.read(&approval);
    assert_eq!((a.revoked, a.updated_by), (true, deployer.pubkey()));
    // Any other key sent the same way (its signature left empty too) is refused, Studio's
    // attester included: it is the address that is checked.
    for key in [Pubkey::new_unique(), STUDIO_ATTESTER] {
        w.env.fund(key, SOL);
        refused(
            &send_as(
                &mut w.env,
                &[companion::approve(
                    key,
                    SCRIPT,
                    APPROVAL_KIND_SCRIPT,
                    hash,
                    Pubkey::default(),
                )],
                &[],
                &[key],
            ),
            CompanionError::NotProtocolAuthority,
        );
        refused(
            &send_as(
                &mut w.env,
                &[companion::revoke_approval(key, SCRIPT)],
                &[],
                &[key],
            ),
            CompanionError::NotProtocolAuthority,
        );
    }
    assert!(w.env.read::<Approval>(&approval).revoked);
}

/// An immutable script (its upgrade authority given up: nobody can change its code, the rule the
/// launchpad applies to hooks) runs with no approval record, the owner's rule of 2026-10-12: made,
/// launched and stepped with its approval account an empty one, which is never read (a revoked
/// approval of it has no say either); it has no author, so a claim pays no author share and the
/// whole claim is split as ever. Every program someone can upgrade keeps the approval's rules:
/// without a current approval it is refused.
#[test]
fn an_immutable_script_runs_with_no_approval() {
    let (mut w, _author) = script_world();
    let launcher = w.wallet_with_sol(10 * SOL);
    let init = se::state_bytes(se::MODE_BUY, BUY);
    // Upgradeable (by Bordrless's key) and unapproved: refused.
    let (_, tx) = make(&mut w, &launcher, terms(init.clone()));
    refused(&tx, CompanionError::NotApproved);
    // Immutable: made, its approval account an empty one.
    w.env.set_upgrade_authority(SCRIPT, None);
    let approval = companion::approval_address(&SCRIPT);
    assert!(w.env.account(&approval).is_none());
    let (mint, tx) = make(&mut w, &launcher, terms(init));
    tx.ok();
    assert_eq!(tx.event::<ScriptCreated>().author, Pubkey::default());
    let mint_key = mint.pubkey();
    let s = script_of(&w, &mint_key);
    assert_eq!(
        (s.program, s.author, s.author_paid),
        (SCRIPT, Pubkey::default(), 0)
    );
    launch_it(&mut w, &launcher, &mint).ok();
    let mint = mint_key;
    // A claim: no author to pay, the whole claim split as ever.
    w.env.warp(31);
    trade(&mut w, &mint, 4, 2 * SOL);
    let fees = w.env.holding(&w.sol, &launch::launch_address(&mint));
    assert!(fees > 0);
    let cranker = w.wallet_with_sol(SOL);
    let tx = w.env.send_paid_by(
        &[companion::claim_fees_script(cranker.pubkey(), mint, None)],
        &cranker,
        &[],
    );
    tx.ok();
    assert!(tx.events::<AuthorPaid>().is_empty());
    let claimed: FeesClaimed = tx.event();
    let funded: PotFunded = tx.event();
    assert_eq!(claimed.claimed, fees);
    let c = companion_of(&w, &mint);
    assert_eq!(
        (c.claimed_total, c.pending_pot, c.pending_beneficiary),
        (fees, funded.to_pot, claimed.to_beneficiary)
    );
    assert_eq!(script_of(&w, &mint).author_paid, 0);
    // Steps, the approval account still an empty one.
    w.env.warp(30);
    let nobody = Pubkey::new_unique();
    let tx = step(&mut w, &cranker, &mint, se::MODE_BUY, BUY, &nobody);
    tx.ok();
    assert!(tx.event::<ScriptStepped>().tokens_after > 0);
    assert!(w.env.account(&approval).is_none());
    // An approval of it, revoked: not read; nothing to say over code nobody can change.
    approve(&mut w, APPROVAL_KIND_SCRIPT).ok();
    revoke(&mut w).ok();
    w.env.warp(60);
    step(&mut w, &cranker, &mint, se::MODE_NOTHING, 0, &nobody).ok();
    // Upgradeable again (an authority written into its ProgramData, which no loader does; it is
    // what the rule turns on): the approval's rules are back, and the revoked approval refuses it
    // until it is approved again.
    w.env.set_upgrade_authority(SCRIPT, Some(BORDRLESS_KEY));
    w.env.warp(60);
    refused(
        &step(&mut w, &cranker, &mint, se::MODE_NOTHING, 0, &nobody),
        CompanionError::NotApproved,
    );
    w.env.warp(1);
    approve(&mut w, APPROVAL_KIND_SCRIPT).ok();
    step(&mut w, &cranker, &mint, se::MODE_NOTHING, 0, &nobody).ok();
}

/// Only a program nobody but Bordrless can change is approved (Bordrless builds and deploys every
/// approved script itself): upgradeable only by Bordrless's keys, immutable, or behind a hook
/// timelock (its `Timelock` passed); never one its author, or anyone else, can upgrade. The
/// approval records the author it names.
#[test]
fn only_a_program_bordrless_controls_is_approved() {
    let (mut w, author) = script_world();
    // Bordrless's keys: Studio's and the protocol's.
    for key in bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES {
        w.env.set_upgrade_authority(SCRIPT, Some(key));
        w.env.warp(1);
        let tx = approve_by(&mut w, APPROVAL_KIND_SCRIPT, author.pubkey());
        tx.ok();
        let ev: ProgramApproved = tx.event();
        assert_eq!(ev.author, author.pubkey());
        let a: Approval = w.env.read(&companion::approval_address(&SCRIPT));
        assert_eq!(a.author, author.pubkey());
    }
    // Its author's own key (or anyone else's): refused, as a script and as a token hook.
    w.env.set_upgrade_authority(SCRIPT, Some(author.pubkey()));
    w.env.warp(1);
    refused(
        &approve(&mut w, APPROVAL_KIND_SCRIPT),
        CompanionError::ProgramNotBounded,
    );
    refused(
        &approve(&mut w, APPROVAL_KIND_TOKEN_HOOK),
        CompanionError::ProgramNotBounded,
    );
    // Behind a hook timelock (its author proposes code, everyone gets the delay's notice):
    // approved, the `Timelock` passed; without it the program's class can't be read.
    register(
        &mut w.env,
        &author,
        SCRIPT,
        MIN_ACCEPTED_DELAY,
        author.pubkey(),
    )
    .ok();
    assert_eq!(
        upgrade_authority(&w.env, &SCRIPT),
        Some(timelock_of(&SCRIPT))
    );
    w.env.warp(1);
    let deployer = w.env.deployer.insecure_clone();
    let mut bare = companion::approve(
        deployer.pubkey(),
        SCRIPT,
        APPROVAL_KIND_SCRIPT,
        programdata_hash(&w.env, &SCRIPT),
        author.pubkey(),
    );
    assert_eq!(bare.accounts.pop().unwrap().pubkey, timelock_of(&SCRIPT));
    refused(
        &w.env.send(&[bare], &[&deployer]),
        CompanionError::ProgramAccounts,
    );
    approve(&mut w, APPROVAL_KIND_SCRIPT).ok();
    // Immutable: approved.
    w.env.set_upgrade_authority(SCRIPT, None);
    w.env.warp(1);
    approve(&mut w, APPROVAL_KIND_SCRIPT).ok();
    assert_eq!(
        w.env
            .read::<Approval>(&companion::approval_address(&SCRIPT))
            .author,
        Pubkey::default()
    );
}

/// A script's author earns half of what every fee claim of a companion made with it takes from its
/// launch, paid as SOL to their wallet before the split; the rest is split as ever. A claim that
/// takes nothing from the launch (bridged SOL that reached the creator's holding another way) pays
/// the author nothing. The script and the author's wallet must be passed.
#[test]
fn the_author_earns_half_of_what_each_claim_takes() {
    let (mut w, author) = script_world();
    approve_by(&mut w, APPROVAL_KIND_SCRIPT, author.pubkey()).ok();
    let launcher = w.wallet_with_sol(10 * SOL);
    let (mint, tx) = make(
        &mut w,
        &launcher,
        terms(se::state_bytes(se::MODE_NOTHING, 0)),
    );
    tx.ok();
    let ev: ScriptCreated = tx.event();
    assert_eq!(ev.author, author.pubkey());
    let mint_key = mint.pubkey();
    assert_eq!(script_of(&w, &mint_key).author, author.pubkey());
    launch_it(&mut w, &launcher, &mint).ok();
    let mint = mint_key;
    let creator = companion::creator_address(&mint);
    w.env.warp(31);
    trade(&mut w, &mint, 4, 2 * SOL);
    let fees = w.env.holding(&w.sol, &launch::launch_address(&mint));
    assert!(fees > 0);
    let cranker = w.wallet_with_sol(SOL);
    // Neither the script nor the author's wallet may be left out (nothing moves).
    refused(
        &w.env.send_paid_by(
            &[companion::claim_fees(cranker.pubkey(), mint, None)],
            &cranker,
            &[],
        ),
        CompanionError::MissingAccount,
    );
    refused(
        &w.env.send_paid_by(
            &[companion::claim_fees_script(cranker.pubkey(), mint, None)],
            &cranker,
            &[],
        ),
        CompanionError::MissingAccount,
    );
    let before = w.env.lamports(&author.pubkey());
    let tx = w.env.send_paid_by(
        &[companion::claim_fees_script(
            cranker.pubkey(),
            mint,
            Some(author.pubkey()),
        )],
        &cranker,
        &[],
    );
    tx.ok();
    println!(
        "claim_fees of a script companion, its author paid: CU {} size {}",
        tx.cu(),
        tx.size
    );
    let paid: AuthorPaid = tx.event();
    let half = fees * u64::from(AUTHOR_SHARE_BPS) / 10_000;
    assert_eq!(
        (paid.companion, paid.author, paid.amount),
        (companion::companion_address(&mint), author.pubkey(), half)
    );
    assert_eq!(w.env.lamports(&author.pubkey()), before + half);
    assert_eq!(script_of(&w, &mint).author_paid, half);
    // The rest, split as ever: the bounty, then 30% to the launcher and 70% to the pot.
    let claimed: FeesClaimed = tx.event();
    let funded: PotFunded = tx.event();
    let rest = fees - half;
    assert_eq!(claimed.claimed, rest);
    assert_eq!(claimed.bounty, rest * 50 / 10_000);
    let split = rest - claimed.bounty;
    assert_eq!(claimed.to_beneficiary, split * 3_000 / 10_000);
    assert_eq!(funded.to_pot, split - claimed.to_beneficiary);
    let c = companion_of(&w, &mint);
    assert_eq!(
        (c.pending_pot, c.pending_beneficiary, c.claimed_total),
        (funded.to_pot, claimed.to_beneficiary, rest)
    );
    assert_eq!(w.env.holding(&w.sol, &creator), c.set_aside().unwrap());

    // Bridged SOL given to the creator's holding: split, the author paid nothing of it.
    let donor = w.wallet_with_sol(SOL);
    let gift = SOL / 10;
    let sol = w.sol;
    w.env
        .send_paid_by(
            &[token::transfer_with(
                donor.pubkey(),
                token::holding_address(&sol, &donor.pubkey()),
                token::holding_address(&sol, &creator),
                sol,
                None,
                vec![],
                gift,
            )],
            &donor,
            &[],
        )
        .ok();
    assert_eq!(w.env.holding(&w.sol, &launch::launch_address(&mint)), 0);
    let tx = w.env.send_paid_by(
        &[companion::claim_fees_script(
            cranker.pubkey(),
            mint,
            Some(author.pubkey()),
        )],
        &cranker,
        &[],
    );
    tx.ok();
    assert!(tx.events::<AuthorPaid>().is_empty());
    let claimed: FeesClaimed = tx.event();
    assert_eq!(claimed.claimed, gift);
    assert_eq!(script_of(&w, &mint).author_paid, half);
    assert_eq!(w.env.lamports(&author.pubkey()), before + half);

    // The next fees: half again.
    trade(&mut w, &mint, 1, SOL);
    let fees = w.env.holding(&w.sol, &launch::launch_address(&mint));
    let tx = w.env.send_paid_by(
        &[companion::claim_fees_script(
            cranker.pubkey(),
            mint,
            Some(author.pubkey()),
        )],
        &cranker,
        &[],
    );
    tx.ok();
    let paid: AuthorPaid = tx.event();
    assert_eq!(paid.amount, fees / 2);
    assert_eq!(script_of(&w, &mint).author_paid, half + fees / 2);
}

/// The hook's SOL cut in a claim (2026-10-15, `Companion.quote_cut_claimed`): the author's share
/// and the bounty come from the whole claim, the cut's part of the rest goes whole to the pot, and
/// only the rest is split, so a launcher at 50% takes half of the creator fee only. PumpTek's
/// proportions: 45.42 of a 106.96 claim the cut. The cut is simulated by raising the launch's
/// counter (the launchpad keeps the cut's SOL in the vault with the fee).
#[test]
fn the_hook_cut_goes_to_the_pot_after_the_authors_share_and_the_bounty() {
    let (mut w, author) = script_world();
    approve_by(&mut w, APPROVAL_KIND_SCRIPT, author.pubkey()).ok();
    let launcher = w.wallet_with_sol(10 * SOL);
    let mut args = terms(se::state_bytes(se::MODE_NOTHING, 0));
    args.split.beneficiary_bps = 5_000;
    args.pot_bps = 5_000;
    let (mint, tx) = make(&mut w, &launcher, args);
    tx.ok();
    launch_it(&mut w, &launcher, &mint).ok();
    let mint = mint.pubkey();
    w.env.warp(31);
    trade(&mut w, &mint, 4, 2 * SOL);
    let fees = w.env.holding(&w.sol, &launch::launch_address(&mint));
    let cut = fees * 4_542 / 10_696;
    let key = launch::launch_address(&mint);
    let mut account = w.env.account(&key).unwrap();
    let mut l = bordrless_launch::state::Launch::try_deserialize(&mut &account.data[..]).unwrap();
    l.add_quote_cut(cut);
    let mut data = Vec::new();
    l.try_serialize(&mut data).unwrap();
    account.data[..data.len()].copy_from_slice(&data);
    w.env.put(key, account);

    let cranker = w.wallet_with_sol(SOL);
    let tx = w.env.send_paid_by(
        &[companion::claim_fees_script(
            cranker.pubkey(),
            mint,
            Some(author.pubkey()),
        )],
        &cranker,
        &[],
    );
    tx.ok();
    let paid: AuthorPaid = tx.event();
    let claimed: FeesClaimed = tx.event();
    let funded: PotFunded = tx.event();
    let to_author = fees * u64::from(AUTHOR_SHARE_BPS) / 10_000;
    assert_eq!(paid.amount, to_author);
    let got = fees - to_author;
    assert_eq!(claimed.claimed, got);
    assert_eq!(claimed.bounty, got * 50 / 10_000);
    let rest = got - claimed.bounty;
    let from_hook = (u128::from(rest) * u128::from(cut) / u128::from(fees)) as u64;
    let base = rest - from_hook;
    assert_eq!(claimed.from_hook, from_hook);
    assert_eq!(claimed.to_beneficiary, base / 2);
    assert_eq!(funded.to_pot, base - base / 2 + from_hook);
    // Every lamport claimed lands in exactly one place.
    assert_eq!(
        paid.amount
            + claimed.bounty
            + claimed.to_beneficiary
            + claimed.to_buyback
            + claimed.to_holders
            + funded.to_pot,
        fees
    );
    let c = companion_of(&w, &mint);
    assert_eq!(c.quote_cut_claimed.amount, cut);
    assert_eq!(
        (c.pending_pot, c.pending_beneficiary),
        (funded.to_pot, claimed.to_beneficiary)
    );
    assert_eq!(
        w.env.holding(&w.sol, &companion::creator_address(&mint)),
        c.set_aside().unwrap()
    );
}

/// A launcher who wrote the script they launch with is its beneficiary already: no extra share,
/// the whole claim split as ever (no author's wallet needed). An approval naming no author pays
/// nobody either, nor one naming an account no transfer can pay (a program): its claims are never
/// stuck on it.
#[test]
fn a_launcher_who_wrote_the_script_takes_no_extra_share() {
    for case in 0..3 {
        let (mut w, _author) = script_world();
        let launcher = w.wallet_with_sol(10 * SOL);
        let author = match case {
            0 => launcher.pubkey(),
            1 => Pubkey::default(),
            _ => SCRIPT,
        };
        approve_by(&mut w, APPROVAL_KIND_SCRIPT, author).ok();
        let mint = launched(&mut w, &launcher, se::state_bytes(se::MODE_NOTHING, 0));
        assert_eq!(script_of(&w, &mint).author, author);
        w.env.warp(31);
        trade(&mut w, &mint, 4, 2 * SOL);
        let fees = w.env.holding(&w.sol, &launch::launch_address(&mint));
        let cranker = w.wallet_with_sol(SOL);
        let before = w.env.lamports(&launcher.pubkey());
        // A program named as the author is passed as it is (read-only: no transaction can write it).
        let mut ix =
            companion::claim_fees_script(cranker.pubkey(), mint, (case == 2).then_some(author));
        if case == 2 {
            ix.accounts.last_mut().unwrap().is_writable = false;
        }
        let tx = w.env.send_paid_by(&[ix], &cranker, &[]);
        tx.ok();
        assert!(tx.events::<AuthorPaid>().is_empty());
        let claimed: FeesClaimed = tx.event();
        assert_eq!(claimed.claimed, fees);
        assert_eq!(script_of(&w, &mint).author_paid, 0);
        assert_eq!(w.env.lamports(&launcher.pubkey()), before);
        let c = companion_of(&w, &mint);
        assert_eq!(c.claimed_total, fees);
        assert_eq!(c.pending_beneficiary, claimed.to_beneficiary);
    }
}

#[test]
fn a_script_needs_a_current_approval_of_its_kind() {
    let (mut w, _author) = script_world();
    let launcher = w.wallet_with_sol(10 * SOL);
    let init = se::state_bytes(se::MODE_BUY, BUY);
    // No approval at all.
    let (_, tx) = make(&mut w, &launcher, terms(init.clone()));
    refused(&tx, CompanionError::NotApproved);
    // Approved as a token hook only.
    approve(&mut w, APPROVAL_KIND_TOKEN_HOOK).ok();
    let (_, tx) = make(&mut w, &launcher, terms(init.clone()));
    refused(&tx, CompanionError::NotApproved);
    // Approved as a script: made, its init run.
    w.env.warp(1);
    approve(&mut w, APPROVAL_KIND_SCRIPT).ok();
    let (mint, tx) = make(&mut w, &launcher, terms(init.clone()));
    tx.ok();
    let mint = mint.pubkey();
    let ev: ScriptCreated = tx.event();
    assert_eq!(
        (ev.mint, ev.program, ev.pot_bps, ev.interval),
        (mint, SCRIPT, 7_000, 60)
    );
    assert_eq!(ev.companion, companion::companion_address(&mint));
    let c = companion_of(&w, &mint);
    assert!(c.is_script() && !c.is_classic() && !c.is_game() && !c.launched);
    assert_eq!(
        (c.pot_bps, c.split.beneficiary_bps, c.mode),
        (7_000, 3_000, MODE_SCRIPT)
    );
    let s = script_of(&w, &mint);
    assert_eq!(
        (s.version, s.mint, s.program, s.interval, s.last_at, s.steps),
        (SCRIPT_VERSION, mint, SCRIPT, 60, 0, 0)
    );
    assert_eq!(s.created_at, w.env.now);
    assert_eq!(
        w.env
            .account(&companion::script_address(&mint))
            .unwrap()
            .data
            .len(),
        Script::LEN
    );
    assert_eq!(s.state_len, STATE_LEN);
    // The companion made the script's state (the launcher paying its rent), owned by the script,
    // which wrote its init data at its start: the rest is still zeros.
    let state = w
        .env
        .account(&companion::script_state_address(&mint))
        .unwrap();
    assert_eq!(
        companion::script_state_address(&mint),
        se::state_address(&mint)
    );
    assert_eq!(state.owner, SCRIPT);
    assert_eq!(state.data.len(), STATE_LEN as usize);
    assert_eq!(state.lamports, w.env.rent(STATE_LEN as usize));
    assert_eq!(&state.data[..init.len()], &init[..]);
    assert!(state.data[init.len()..].iter().all(|b| *b == 0));
    // Revoked: no new script.
    revoke(&mut w).ok();
    let (_, tx) = make(&mut w, &launcher, terms(init));
    refused(&tx, CompanionError::NotApproved);
}

#[test]
fn what_create_script_refuses() {
    let (mut w, _author) = script_world();
    approve(&mut w, APPROVAL_KIND_SCRIPT).ok();
    let launcher = w.wallet_with_sol(20 * SOL);
    let init = || se::state_bytes(se::MODE_NOTHING, 0);
    // The split with the pot must add up, with a pot.
    let mut a = terms(init());
    a.pot_bps = 6_000;
    refused(&make(&mut w, &launcher, a).1, CompanionError::BadSplit);
    let mut a = terms(init());
    a.pot_bps = 0;
    a.split.beneficiary_bps = 10_000;
    refused(&make(&mut w, &launcher, a).1, CompanionError::BadSplit);
    // Steps 10 s to a day apart; init data of 512 bytes at most; a state of 10,240 bytes at most;
    // never a protocol program.
    for bad in [
        CreateScriptArgs {
            interval: MIN_SCRIPT_INTERVAL - 1,
            ..terms(init())
        },
        CreateScriptArgs {
            interval: MAX_SCRIPT_INTERVAL + 1,
            ..terms(init())
        },
        CreateScriptArgs {
            init_data: vec![se::MODE_NOTHING; MAX_SCRIPT_INIT_LEN + 1],
            ..terms(init())
        },
        CreateScriptArgs {
            state_len: MAX_SCRIPT_STATE_LEN + 1,
            ..terms(init())
        },
        CreateScriptArgs {
            program: bordrless_token::ID,
            ..terms(init())
        },
    ] {
        refused(&make(&mut w, &launcher, bad).1, CompanionError::BadScript);
    }
    // The bounds themselves are taken: the largest state, made zeroed for the script.
    for good in [
        CreateScriptArgs {
            interval: MIN_SCRIPT_INTERVAL,
            ..terms(init())
        },
        CreateScriptArgs {
            interval: MAX_SCRIPT_INTERVAL,
            init_data: [init(), vec![0; MAX_SCRIPT_INIT_LEN - 9]].concat(),
            state_len: MAX_SCRIPT_STATE_LEN,
            ..terms(init())
        },
    ] {
        let state_len = good.state_len;
        let (mint, tx) = make(&mut w, &launcher, good);
        tx.ok();
        let state = w
            .env
            .account(&companion::script_state_address(&mint.pubkey()))
            .unwrap();
        assert_eq!(
            (state.owner, state.data.len()),
            (SCRIPT, state_len as usize)
        );
    }

    // No signature but the creator address's ever reaches the script: the mint's and the payer's,
    // passed again as signers among the init's accounts, arrive unsigned (the script refuses any
    // signer but the creator).
    let (_, tx) = make_with(&mut w, &launcher, terms(init()), |mint| {
        let mut extra = init_accounts(mint);
        extra.push(AccountMeta::new_readonly(*mint, true));
        extra.push(AccountMeta::new(launcher.pubkey(), true));
        extra
    });
    tx.ok();
    // So a script can't use the launcher's signature: here its init moving the launcher's lamports
    // fails (the runtime refuses the signature it does not have), and nothing moves.
    let before = w.env.lamports(&launcher.pubkey());
    let (_, tx) = make_with(
        &mut w,
        &launcher,
        terms(se::state_bytes(se::MODE_TAKE_PAYER, SOL)),
        |mint| {
            let mut extra = init_accounts(mint);
            extra.push(AccountMeta::new(launcher.pubkey(), true));
            extra
        },
    );
    assert!(
        matches!(
            tx.err(),
            solana_transaction::TransactionError::InstructionError(
                _,
                solana_transaction::InstructionError::PrivilegeEscalation
            )
        ),
        "{:?}\n{}",
        tx.err(),
        tx.logs().join("\n")
    );
    // At most the transaction's fee (two signatures) left the launcher.
    assert!(w.env.lamports(&launcher.pubkey()) + 2 * 5_000 >= before);

    // The init may take nothing: the creator address's lamports, or its system account.
    let thief = Pubkey::new_unique();
    let (_, tx) = make_with(
        &mut w,
        &launcher,
        terms(se::state_bytes(se::MODE_TAKE_LAMPORTS, 1)),
        |mint| {
            let mut extra = init_accounts(mint);
            extra.push(AccountMeta::new(thief, false));
            extra
        },
    );
    refused(&tx, CompanionError::ScriptTookLamports);
    let (_, tx) = make(
        &mut w,
        &launcher,
        terms(se::state_bytes(se::MODE_ASSIGN, 0)),
    );
    refused(&tx, CompanionError::ScriptBrokeCustody);

    // Not twice.
    let (mint, tx) = make(&mut w, &launcher, terms(init()));
    tx.ok();
    let again = companion::create_script(
        launcher.pubkey(),
        mint.pubkey(),
        terms(init()),
        init_accounts(&mint.pubkey()),
    );
    w.env
        .send_paid_by(&[again], &launcher, &[&mint])
        .expect_fail();
    // A script companion is neither a Snowball nor a game.
    let snowball = CreateSnowballArgs {
        split: Split {
            buyback_bps: 0,
            holders_bps: 0,
            beneficiary_bps: 3_000,
        },
        pot_bps: 7_000,
        buy_step_bps: 200,
        sell_step_bps: 200,
        buy_bps: 2_500,
        sell_bps: 2_500,
        interval: 60,
        max_trade: SOL,
        pool_bps: 500,
        flat_trades: false,
    };
    refused(
        &w.env.send_paid_by(
            &[companion::create_snowball(
                launcher.pubkey(),
                mint.pubkey(),
                snowball.clone(),
            )],
            &launcher,
            &[&mint],
        ),
        CompanionError::BadSnowball,
    );
    let game = CreateGameArgs {
        kind: GameKind::Lottery,
        hook: lottery_hook::ID,
        split: Split {
            buyback_bps: 3_000,
            holders_bps: 0,
            beneficiary_bps: 0,
        },
        pot_bps: 7_000,
        round_secs: 3_600,
        min_pot: 100_000_000,
        prize_bps: 10_000,
        claim_window_secs: 300,
        max_attempts: 6,
    };
    refused(
        &w.env.send_paid_by(
            &[
                lottery::prepare(launcher.pubkey(), mint.pubkey(), 3_600),
                companion::create_game(launcher.pubkey(), mint.pubkey(), game),
            ],
            &launcher,
            &[&mint],
        ),
        CompanionError::BadGame,
    );
    // A Snowball is no script.
    let other = Keypair::new();
    w.env
        .send_paid_by(
            &[
                companion::create(
                    launcher.pubkey(),
                    launcher.pubkey(),
                    other.pubkey(),
                    create_args(),
                ),
                companion::create_snowball(launcher.pubkey(), other.pubkey(), snowball),
            ],
            &launcher,
            &[&other],
        )
        .ok();
    refused(
        &w.env.send_paid_by(
            &[companion::create_script(
                launcher.pubkey(),
                other.pubkey(),
                terms(init()),
                init_accounts(&other.pubkey()),
            )],
            &launcher,
            &[&other],
        ),
        CompanionError::BadScript,
    );
    // Not after the launch.
    let classic = Keypair::new();
    w.env
        .send_paid_by(
            &[companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                classic.pubkey(),
                create_args(),
            )],
            &launcher,
            &[&classic],
        )
        .ok();
    launch_it(&mut w, &launcher, &classic).ok();
    refused(
        &w.env.send_paid_by(
            &[companion::create_script(
                launcher.pubkey(),
                classic.pubkey(),
                terms(init()),
                init_accounts(&classic.pubkey()),
            )],
            &launcher,
            &[&classic],
        ),
        CompanionError::BadScript,
    );
}

/// The script spends its pot once an interval, from a minute after the launch: a buy (the pot down
/// by what it spent, the tokens in the creator's holding), a sell (the pot up by what it fetched),
/// a step that does nothing; each pays the sender `SCRIPT_BOUNTY` from the pot.
#[test]
fn a_script_spends_its_pot_once_an_interval() {
    let (mut w, _author) = script_world();
    approve(&mut w, APPROVAL_KIND_SCRIPT).ok();
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = launched(&mut w, &launcher, se::state_bytes(se::MODE_BUY, BUY));
    let creator = companion::creator_address(&mint);
    let cranker = fill_pot(&mut w, &mint);
    let nobody = Pubkey::new_unique();
    // Not in the launch's first minute.
    refused(
        &step(&mut w, &cranker, &mint, se::MODE_BUY, BUY, &nobody),
        CompanionError::ScriptNotDue,
    );
    w.env.warp(30);

    // A buy.
    let pot = companion_of(&w, &mint).pending_pot;
    let bounties = companion_of(&w, &mint).bounties_total;
    let sol = w.env.holding(&w.sol, &creator);
    let lamports = w.env.lamports(&cranker.pubkey());
    let tx = step(&mut w, &cranker, &mint, se::MODE_BUY, BUY, &nobody);
    tx.ok();
    println!(
        "script step (a buy): CU {} size {} height {} trace {}",
        tx.cu(),
        tx.size,
        tx.max_height(),
        tx.trace_len()
    );
    assert!(tx.max_height() <= 5, "height {}", tx.max_height());
    let ev: ScriptStepped = tx.event();
    assert_eq!(
        (ev.companion, ev.program, ev.cranker),
        (
            companion::companion_address(&mint),
            SCRIPT,
            cranker.pubkey()
        )
    );
    assert_eq!((ev.sol_before, ev.sol_after), (sol, sol - BUY));
    assert_eq!(ev.tokens_before, 0);
    assert!(ev.tokens_after > 0);
    assert_eq!(w.env.holding(&mint, &creator), ev.tokens_after);
    assert_eq!(ev.bounty, SCRIPT_BOUNTY);
    assert_eq!(ev.pot_after, pot - BUY - SCRIPT_BOUNTY);
    let c = companion_of(&w, &mint);
    assert_eq!(c.pending_pot, ev.pot_after);
    // The holding holds exactly what is set aside: the pot spent, the launcher's part untouched.
    assert_eq!(w.env.holding(&w.sol, &creator), c.set_aside().unwrap());
    // The sender: the bounty, less its fee and the rent of the creator's token holding it made.
    let rent = w.env.rent(Holding::LEN);
    assert_eq!(
        w.env.lamports(&cranker.pubkey()),
        lamports - 5_000 - rent + SCRIPT_BOUNTY
    );
    let s = script_of(&w, &mint);
    assert_eq!((s.steps, s.last_at), (1, w.env.now));

    // Not again within the interval.
    refused(
        &step(&mut w, &cranker, &mint, se::MODE_NOTHING, 0, &nobody),
        CompanionError::ScriptNotDue,
    );
    w.env.warp(59);
    refused(
        &step(&mut w, &cranker, &mint, se::MODE_NOTHING, 0, &nobody),
        CompanionError::ScriptNotDue,
    );
    w.env.warp(1);

    // A sell of everything it bought: the pot grows by what the sell fetched.
    let pot = companion_of(&w, &mint).pending_pot;
    let tx = step(&mut w, &cranker, &mint, se::MODE_SELL, 0, &nobody);
    tx.ok();
    let ev: ScriptStepped = tx.event();
    assert!(ev.sol_after > ev.sol_before);
    assert_eq!(ev.tokens_after, 0);
    assert_eq!(
        ev.pot_after,
        pot + (ev.sol_after - ev.sol_before) - SCRIPT_BOUNTY
    );
    let c = companion_of(&w, &mint);
    assert_eq!(c.pending_pot, ev.pot_after);
    assert_eq!(w.env.holding(&w.sol, &creator), c.set_aside().unwrap());

    // A step that does nothing still pays its sender, from the pot.
    w.env.warp(60);
    let pot = c.pending_pot;
    let tx = step(&mut w, &cranker, &mint, se::MODE_NOTHING, 0, &nobody);
    tx.ok();
    let ev: ScriptStepped = tx.event();
    assert_eq!(ev.sol_after, ev.sol_before);
    assert_eq!(
        (ev.bounty, ev.pot_after),
        (SCRIPT_BOUNTY, pot - SCRIPT_BOUNTY)
    );
    assert_eq!(script_of(&w, &mint).steps, 3);
    assert_eq!(
        companion_of(&w, &mint).bounties_total,
        bounties + 3 * SCRIPT_BOUNTY
    );
}

/// A buy step fits mainnet's limits as a keeper sends it: v0 with the protocol's 22-address lookup
/// table, at stack height 5 (Solana's limit), within a modest compute budget.
#[test]
fn a_script_step_fits_a_packet_with_the_protocol_table() {
    use bordrless_program_tests::env::compute_unit_limit;
    let (mut w, _author) = script_world();
    approve(&mut w, APPROVAL_KIND_SCRIPT).ok();
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = launched(&mut w, &launcher, se::state_bytes(se::MODE_BUY, BUY));
    let cranker = fill_pot(&mut w, &mint);
    w.env.warp(30);
    let mut table = protocol_lookup_table(&w);
    table.extend([
        companion::event_authority(),
        bordrless_launch::ID,
        bordrless_swap::ID,
        bordrless_token::ID,
    ]);
    assert_eq!(table.len(), 22);
    let lut = w.env.put_lookup_table(Pubkey::new_unique(), &table);
    set_mode(&mut w, &mint, se::MODE_BUY, BUY);
    let ix = companion::script_step(
        cranker.pubkey(),
        &keys(&w, &mint),
        SCRIPT,
        step_accounts(&w, &mint, se::MODE_BUY, &Pubkey::new_unique()),
    );
    let tx = w.env.send_v0(
        &[compute_unit_limit(400_000), ix],
        &cranker,
        &[],
        std::slice::from_ref(&lut),
    );
    tx.ok();
    println!(
        "script step (a buy), v0 with the protocol table: {} bytes, {} CU, height {}",
        tx.size,
        tx.cu(),
        tx.max_height()
    );
    assert!(tx.size <= 1_232, "{} bytes", tx.size);
    assert!(tx.max_height() <= 5);
    let ev: ScriptStepped = tx.event();
    assert_eq!(ev.sol_before - ev.sol_after, BUY);
}

/// A script trades a token with kit rules (holder rewards and a burn on both sides: the kit's hook
/// runs on every transfer) within Solana's stack limit: the script's call is one level more than a
/// companion's own trade.
#[test]
fn a_script_trades_a_kit_token_within_the_stack_limit() {
    let (mut w, _author) = script_world();
    approve(&mut w, APPROVAL_KIND_SCRIPT).ok();
    let launcher = w.wallet_with_sol(10 * SOL);
    let (mint, tx) = make(&mut w, &launcher, terms(se::state_bytes(se::MODE_BUY, BUY)));
    tx.ok();
    let creator = companion::creator_address(&mint.pubkey());
    let launch_args = World::launch_args("SKIT", CREATOR_FEE, VQ, presets::burn());
    let inner = launch::create_launch_with(
        creator,
        mint.pubkey(),
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        launch_args.clone(),
        None,
        None,
    );
    let ix = companion::launch(launcher.pubkey(), mint.pubkey(), &inner, launch_args);
    w.env.send_paid_by(&[ix], &launcher, &[&mint]).ok();
    let mint = mint.pubkey();
    assert!(w.launch(&mint).modules != 0);
    let cranker = fill_pot(&mut w, &mint);
    w.env.warp(30);
    let nobody = Pubkey::new_unique();
    for mode in [se::MODE_BUY, se::MODE_SELL] {
        let tx = step(
            &mut w,
            &cranker,
            &mint,
            mode,
            if mode == se::MODE_BUY { BUY } else { 0 },
            &nobody,
        );
        tx.ok();
        println!(
            "script step on a kit token (mode {mode}): CU {} height {} trace {}",
            tx.cu(),
            tx.max_height(),
            tx.trace_len()
        );
        assert!(tx.max_height() <= 5, "height {}", tx.max_height());
        let ev: ScriptStepped = tx.event();
        assert_ne!(ev.tokens_after, ev.tokens_before);
        w.env.warp(60);
    }
}

/// The script may spend the pot, all of it, and nothing else set aside: the launcher's part stays.
#[test]
fn the_script_spends_the_pot_and_nothing_else() {
    let (mut w, _author) = script_world();
    approve(&mut w, APPROVAL_KIND_SCRIPT).ok();
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = launched(&mut w, &launcher, se::state_bytes(se::MODE_NOTHING, 0));
    let creator = companion::creator_address(&mint);
    let cranker = fill_pot(&mut w, &mint);
    w.env.warp(30);
    let thief = w.wallet_with_sol(SOL);
    let to = token::holding_address(&w.sol, &thief.pubkey());
    let c = companion_of(&w, &mint);
    let (pot, set_aside) = (c.pending_pot, c.set_aside().unwrap());
    assert!(c.pending_beneficiary > 0);
    // One lamport more than the pot, or everything.
    refused(
        &step(&mut w, &cranker, &mint, se::MODE_TAKE_SOL, pot + 1, &to),
        CompanionError::ScriptOverspent,
    );
    refused(
        &step(&mut w, &cranker, &mint, se::MODE_TAKE_SOL, 0, &to),
        CompanionError::ScriptOverspent,
    );
    // The pot, exactly: the script's to spend (here to anyone: Bordrless's approval vouches for
    // what a script does with its pot; the companion for what it may never touch).
    let held = w.env.holding(&w.sol, &thief.pubkey());
    let tx = step(&mut w, &cranker, &mint, se::MODE_TAKE_SOL, pot, &to);
    tx.ok();
    assert_eq!(w.env.holding(&w.sol, &thief.pubkey()), held + pot);
    let ev: ScriptStepped = tx.event();
    // An empty pot pays no bounty.
    assert_eq!((ev.bounty, ev.pot_after), (0, 0));
    let c = companion_of(&w, &mint);
    assert_eq!(c.pending_pot, 0);
    assert_eq!(w.env.holding(&w.sol, &creator), set_aside - pot);
    assert_eq!(c.set_aside().unwrap(), set_aside - pot);
    // With the pot empty, not one lamport; nothing done, nothing paid.
    w.env.warp(60);
    refused(
        &step(&mut w, &cranker, &mint, se::MODE_TAKE_SOL, 1, &to),
        CompanionError::ScriptOverspent,
    );
    let tx = step(&mut w, &cranker, &mint, se::MODE_NOTHING, 0, &to);
    tx.ok();
    let ev: ScriptStepped = tx.event();
    assert_eq!((ev.bounty, ev.pot_after), (0, 0));
    // The launcher is paid their part as ever.
    let before = w.env.lamports(&launcher.pubkey());
    w.env
        .send_paid_by(
            &[companion::withdraw(
                cranker.pubkey(),
                mint,
                launcher.pubkey(),
            )],
            &cranker,
            &[],
        )
        .ok();
    assert!(w.env.lamports(&launcher.pubkey()) >= before + c.pending_beneficiary);
}

/// The dev bag stays in the creator's holding whatever the script does; what the script bought is
/// its own to move.
#[test]
fn the_dev_bag_stays() {
    let (mut w, _author) = script_world();
    approve(&mut w, APPROVAL_KIND_SCRIPT).ok();
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = launched(&mut w, &launcher, se::state_bytes(se::MODE_BUY, BUY));
    let creator = companion::creator_address(&mint);
    let k = keys(&w, &mint);
    w.env
        .send_paid_by(
            &[companion::dev_buy(launcher.pubkey(), &k, SOL / 4, 1)],
            &launcher,
            &[],
        )
        .ok();
    let bag = companion_of(&w, &mint).dev_tokens;
    assert!(bag > 0);
    let cranker = fill_pot(&mut w, &mint);
    w.env.warp(30);
    let nobody = Pubkey::new_unique();
    let tx = step(&mut w, &cranker, &mint, se::MODE_BUY, BUY, &nobody);
    tx.ok();
    let ev: ScriptStepped = tx.event();
    assert_eq!(ev.tokens_before, bag);
    let bought = ev.tokens_after - bag;
    assert!(bought > 0);
    let thief = w.wallet_with_sol(SOL);
    w.env
        .send_paid_by(
            &[token::create_holding(thief.pubkey(), mint, thief.pubkey())],
            &thief,
            &[],
        )
        .ok();
    let to = token::holding_address(&mint, &thief.pubkey());
    w.env.warp(60);
    // One token of the bag, or all of them.
    refused(
        &step(
            &mut w,
            &cranker,
            &mint,
            se::MODE_TAKE_TOKENS,
            bought + 1,
            &to,
        ),
        CompanionError::ScriptTookDevBag,
    );
    refused(
        &step(&mut w, &cranker, &mint, se::MODE_TAKE_TOKENS, 0, &to),
        CompanionError::ScriptTookDevBag,
    );
    // What it bought, exactly.
    step(&mut w, &cranker, &mint, se::MODE_TAKE_TOKENS, bought, &to).ok();
    assert_eq!(w.env.holding(&mint, &thief.pubkey()), bought);
    assert_eq!(w.env.holding(&mint, &creator), bag);
    // The bag vests to the launcher as ever.
    w.env
        .send_paid_by(
            &[companion::release(
                cranker.pubkey(),
                &k,
                false,
                launcher.pubkey(),
            )],
            &cranker,
            &[],
        )
        .ok();
    assert_eq!(w.env.holding(&mint, &launcher.pubkey()), bag);
    assert_eq!(w.env.holding(&mint, &creator), 0);
}

/// The creator address's lamports and everything the companion keeps custody of are not the
/// script's: no lamports out, no delegate, no reassigned creator address, no fee claim inside a
/// step (it would pass the launcher's part of the fees to the pot).
#[test]
fn lamports_and_custody_stay_the_companions() {
    let (mut w, _author) = script_world();
    approve(&mut w, APPROVAL_KIND_SCRIPT).ok();
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = launched(&mut w, &launcher, se::state_bytes(se::MODE_NOTHING, 0));
    let cranker = fill_pot(&mut w, &mint);
    w.env.warp(30);
    let thief = w.wallet_with_sol(SOL).pubkey();
    refused(
        &step(&mut w, &cranker, &mint, se::MODE_TAKE_LAMPORTS, 1, &thief),
        CompanionError::ScriptTookLamports,
    );
    refused(
        &step(&mut w, &cranker, &mint, se::MODE_DELEGATE, 0, &thief),
        CompanionError::ScriptBrokeCustody,
    );
    refused(
        &step(&mut w, &cranker, &mint, se::MODE_ASSIGN, 0, &thief),
        CompanionError::ScriptBrokeCustody,
    );
    trade(&mut w, &mint, 2, SOL);
    assert!(w.env.holding(&w.sol, &launch::launch_address(&mint)) > 0);
    refused(
        &step(&mut w, &cranker, &mint, se::MODE_CLAIM, 0, &thief),
        CompanionError::ScriptBrokeCustody,
    );
    // Nothing moved: the step goes on, and the fees are claimed (and split) by `claim_fees`.
    step(&mut w, &cranker, &mint, se::MODE_NOTHING, 0, &thief).ok();
    let tx = w.env.send_paid_by(
        &[companion::claim_fees_script(cranker.pubkey(), mint, None)],
        &cranker,
        &[],
    );
    tx.ok();
    let claimed: FeesClaimed = tx.event();
    assert!(claimed.to_beneficiary > 0);
}

/// An approval is bound to the code: anyone's `ExtendProgram` (the slot moves, the code does not)
/// keeps it; an upgrade to other code voids it until that code is approved; back on the approved
/// code it holds again. A revoked approval stops every step.
#[test]
fn an_upgrade_voids_the_approval_until_its_code_is_approved_again() {
    let (mut w, _author) = script_world();
    w.env.without_sigverify();
    approve(&mut w, APPROVAL_KIND_SCRIPT).ok();
    let approved_slot = programdata_slot(&w.env, &SCRIPT);
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = launched(&mut w, &launcher, se::state_bytes(se::MODE_NOTHING, 0));
    let cranker = fill_pot(&mut w, &mint);
    w.env.warp(30);
    let nobody = Pubkey::new_unique();
    step(&mut w, &cranker, &mint, se::MODE_NOTHING, 0, &nobody).ok();

    // Anyone grows the ProgramData: its slot moves, its code does not.
    let payer = w.env.payer.pubkey();
    w.env
        .send(
            &[loader::extend_program(
                programdata_address(&SCRIPT),
                SCRIPT,
                payer,
                10_240,
            )],
            &[],
        )
        .ok();
    assert_ne!(programdata_slot(&w.env, &SCRIPT), approved_slot);
    w.env.warp(60);
    let tx = step(&mut w, &cranker, &mint, se::MODE_NOTHING, 0, &nobody);
    tx.ok();
    println!("script step (code hashed again): CU {}", tx.cu());

    // An upgrade to other code: no step until that code is approved.
    let approved_code = program_bytes("script_example");
    upgrade(&mut w, &program_bytes("strategy_tester"));
    assert_ne!(
        programdata_hash(&w.env, &SCRIPT),
        w.env
            .read::<Approval>(&companion::approval_address(&SCRIPT))
            .code_hash
    );
    w.env.warp(60);
    refused(
        &step(&mut w, &cranker, &mint, se::MODE_NOTHING, 0, &nobody),
        CompanionError::NotApproved,
    );
    // Back on the approved code: the approval holds again (it is of the code, not of a deploy).
    upgrade(&mut w, &approved_code);
    w.env.warp(60);
    step(&mut w, &cranker, &mint, se::MODE_NOTHING, 0, &nobody).ok();
    // Revoked: no step.
    revoke(&mut w).ok();
    w.env.warp(60);
    refused(
        &step(&mut w, &cranker, &mint, se::MODE_NOTHING, 0, &nobody),
        CompanionError::NotApproved,
    );
    // Approved again: steps again.
    approve(&mut w, APPROVAL_KIND_SCRIPT).ok();
    step(&mut w, &cranker, &mint, se::MODE_NOTHING, 0, &nobody).ok();
    // A step needs the script's own ProgramData.
    w.env.warp(60);
    let mut ix = companion::script_step(
        cranker.pubkey(),
        &keys(&w, &mint),
        SCRIPT,
        step_accounts(&w, &mint, se::MODE_NOTHING, &nobody),
    );
    let own = programdata_address(&SCRIPT);
    let at = ix.accounts.iter().position(|m| m.pubkey == own).unwrap();
    ix.accounts[at].pubkey = programdata_address(&bordrless_token::ID);
    refused(
        &w.env.send_paid_by(&[ix], &cranker, &[]),
        CompanionError::ProgramAccounts,
    );
}

/// A script may ask for no state: the system program's id stands in its place in both calls, and
/// its steps run (the example script then does nothing). A signer among the remaining accounts
/// (here the sender, passed again as a signer) reaches the script unsigned. A state address someone
/// funded before the script is made is made all the same, the funding kept: nobody can block a
/// script that way.
#[test]
fn a_script_with_no_state_and_a_state_funded_beforehand() {
    let (mut w, _author) = script_world();
    approve(&mut w, APPROVAL_KIND_SCRIPT).ok();
    let launcher = w.wallet_with_sol(10 * SOL);
    let (mint, tx) = make(
        &mut w,
        &launcher,
        CreateScriptArgs {
            state_len: 0,
            ..terms(vec![])
        },
    );
    tx.ok();
    let m = mint.pubkey();
    assert_eq!(script_of(&w, &m).state_len, 0);
    assert!(w
        .env
        .account(&companion::script_state_address(&m))
        .is_none());
    launch_it(&mut w, &launcher, &mint).ok();
    let cranker = fill_pot(&mut w, &m);
    w.env.warp(30);
    let ix = companion::script_step(
        cranker.pubkey(),
        &keys(&w, &m),
        SCRIPT,
        vec![AccountMeta::new(cranker.pubkey(), true)],
    );
    let tx = w.env.send_paid_by(&[ix], &cranker, &[]);
    tx.ok();
    let ev: ScriptStepped = tx.event();
    assert_eq!(ev.sol_after, ev.sol_before);
    assert_eq!(ev.bounty, SCRIPT_BOUNTY);
    assert!(w
        .env
        .account(&companion::script_state_address(&m))
        .is_none());

    // A state address funded beforehand (a griefer's lamports, below any rent): made all the same,
    // topped up to its rent, zeroed and the script's.
    let mint = Keypair::new();
    let state = companion::script_state_address(&mint.pubkey());
    w.env.fund(state, 1_000);
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            mint.pubkey(),
            create_args(),
        ),
        companion::create_script(
            launcher.pubkey(),
            mint.pubkey(),
            CreateScriptArgs {
                state_len: 128,
                ..terms(vec![])
            },
            vec![],
        ),
    ];
    w.env.send_paid_by(&ixs, &launcher, &[&mint]).ok();
    let a = w.env.account(&state).unwrap();
    assert_eq!(
        (a.owner, a.data.len(), a.lamports),
        (SCRIPT, 128, w.env.rent(128))
    );
    assert!(a.data.iter().all(|b| *b == 0));
    assert_eq!(script_of(&w, &mint.pubkey()).state_len, 128);
}

/// Audit H-1/I-5: while native launches are on, a script companion launches native only with a
/// native-ready script; one running a script on `NOT_NATIVE_READY_SCRIPTS` is refused
/// (`ScriptNotNativeReady`), and the launch needs the script's address (the client passes it).
/// With native launches off it launches bridged as before.
#[test]
fn a_script_that_is_not_native_ready_cannot_launch_native() {
    let (mut w, _) = script_world();
    approve(&mut w, APPROVAL_KIND_SCRIPT).ok();
    let admin = w.admin();
    let ad = admin.pubkey();
    w.as_admin(launch::set_native_launches(ad, true)).ok();
    let launcher = w.wallet_with_sol(5 * SOL);
    // A native-ready script launches native.
    let (ok_mint, tx) = make(&mut w, &launcher, terms(vec![]));
    tx.ok();
    launch_it(&mut w, &launcher, &ok_mint).ok();
    assert!(w.launch(&ok_mint.pubkey()).is_native());
    assert!(companion_of(&w, &ok_mint.pubkey()).is_native());
    // The same companion with a held script (its Script account rewritten to name one).
    let (mint, tx) = make(&mut w, &launcher, terms(vec![]));
    tx.ok();
    let key = companion::script_address(&mint.pubkey());
    let mut s = script_of(&w, &mint.pubkey());
    s.program = NOT_NATIVE_READY_SCRIPTS[0];
    let mut account = w.env.account(&key).expect("the script");
    let mut data = vec![];
    s.try_serialize(&mut data).unwrap();
    account.data[..data.len()].copy_from_slice(&data);
    w.env.put(key, account);
    let tx = launch_it(&mut w, &launcher, &mint);
    refused(&tx, CompanionError::ScriptNotNativeReady);
    // Without the script's address the launch is refused too.
    let creator = companion::creator_address(&mint.pubkey());
    let launch_args = World::launch_args("SCRP", CREATOR_FEE, VQ, LaunchRules::NONE);
    let inner = launch::create_launch_with(
        creator,
        mint.pubkey(),
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        launch_args.clone(),
        None,
        None,
    );
    let ix = companion::launch(launcher.pubkey(), mint.pubkey(), &inner, launch_args);
    refused(
        &w.env.send_paid_by(&[ix], &launcher, &[&mint]),
        CompanionError::MissingAccount,
    );
    // Native launches off: it launches bridged.
    w.as_admin(launch::set_native_launches(ad, false)).ok();
    launch_it(&mut w, &launcher, &mint).ok();
    assert!(!w.launch(&mint.pubkey()).is_native());
}
