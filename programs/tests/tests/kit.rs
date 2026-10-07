//! The kit (`docs/hooks-v2.md` §4) on chain, set up directly as `create_launch` will leave it (see
//! `bordrless_program_tests::kit`): the binding of every account, each module alone and in every
//! combination, wallets only, max wallet, the creator wallet lock and the early-buyer lock, and
//! the compute of each path. The money (the reward accounting, shares, the seeded walk) is in
//! `kit_money.rs`.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::InstructionData;
use bordrless_hook::{token_flags, HookAccountList, Phase, TokenHookArgs, TokenOp};
use bordrless_kit::client as kit;
use bordrless_kit::constants::*;
use bordrless_kit::error::KitError;
use bordrless_kit::events::{RewardsClaimed, RewardsShared};
use bordrless_kit::{mint_flags, modules, HolderData, KitConfig, KitInitArgs};
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::hooks::{NewPool, SwapSpec};
use bordrless_program_tests::kit::{check_kit, kit_code, DirectKit, KitSpec, SOL};
use bordrless_swap::client as swap;
use bordrless_swap::instructions::AddLiquidityArgs;
use bordrless_token::client as token;
use bordrless_token::instructions::CreateMintArgs;
use bordrless_token::state::{AuthorityKind, Holding, Mint};
use solana_keypair::Keypair;
use solana_signer::Signer;

const SUPPLY: u64 = 1_000_000_000_000_000;
/// A whole token (6 decimals).
const TOKEN: u64 = 1_000_000;

fn k(e: KitError) -> u32 {
    kit_code(e)
}

/// Anchor's own errors.
mod anchor_err {
    pub const INSTRUCTION_FALLBACK_NOT_FOUND: u32 = 101;
    pub const CONSTRAINT_MUT: u32 = 2000;
    pub const CONSTRAINT_SIGNER: u32 = 2002;
    pub const ACCOUNT_OWNED_BY_WRONG_PROGRAM: u32 = 3007;
    pub const ACCOUNT_NOT_SIGNER: u32 = 3010;
    pub const ACCOUNT_NOT_INITIALIZED: u32 = 3012;
}

fn setup(modules: u8) -> (World, DirectKit) {
    let mut w = World::new();
    let d = w.direct_kit(&KitSpec::new(modules));
    (w, d)
}

/// A holder of `d`'s token with 10 bridged SOL.
fn holder(w: &mut World, d: &DirectKit) -> Keypair {
    w.kit_holder(&d.token, 10 * SOL)
}

/// A buy: the pool sends `amount` to `to`.
fn buy(w: &mut World, d: &DirectKit, to: &Pubkey, amount: u64) -> Tx {
    w.send_tokens(&d.pool, d.token.mint, to, amount)
}

/// A sell: `from` sends `amount` to the pool.
fn sell(w: &mut World, d: &DirectKit, from: &Keypair, amount: u64) -> Tx {
    w.send_tokens(from, d.token.mint, &d.token.pool, amount)
}

fn bal(w: &World, d: &DirectKit, owner: &Pubkey) -> u64 {
    w.env.holding(&d.token.mint, owner)
}

fn data(w: &World, d: &DirectKit, owner: &Pubkey) -> HolderData {
    HolderData::read(&w.env.holding_state(&d.token.mint, owner).1)
}

fn config(w: &World, d: &DirectKit) -> KitConfig {
    w.env.kit_config(&d.token.mint)
}

/// Rewrites the config to graduated (only the launch's kit-caller PDA can sign `graduate`).
fn graduate(w: &mut World, d: &DirectKit) {
    let mut c = config(w, d);
    c.graduated = true;
    w.env.put_kit_config(d.token.kit_config, &c);
}

/// A transfer of `d`'s token with the given extras instead of the registry's.
fn transfer_with(
    d: &DirectKit,
    from: &Keypair,
    to: &Pubkey,
    extras: Vec<AccountMeta>,
) -> Instruction {
    token::transfer(
        from.pubkey(),
        token::holding_address(&d.token.mint, &from.pubkey()),
        token::holding_address(&d.token.mint, to),
        d.token.mint,
        Some(bordrless_kit::ID),
        extras,
        1,
    )
}

#[test]
fn the_kit_knows_the_protocol_programs() {
    assert_eq!(LAUNCH_ID, bordrless_launch::ID);
    assert_eq!(SWAP_ID, bordrless_swap::ID);
    assert_eq!(BRIDGE_ID, bordrless_bridge::ID);
    assert_eq!(TOKEN_ID, bordrless_token::ID);
    assert_eq!(bordrless_kit::KIT_ID, bordrless_kit::ID);
    assert!(!kit::kit_caller_address(&Pubkey::new_unique())
        .0
        .is_on_curve());
}

#[test]
fn a_kit_token_is_set_up_as_create_launch_leaves_it() {
    let (w, d) = setup(modules::ALL);
    let t = &d.token;
    let m: Mint = w.env.read(&t.mint);
    assert_eq!(m.hook_program, Some(bordrless_kit::ID));
    assert_eq!(m.hook_flags, mint_flags(modules::ALL));
    assert_eq!(
        m.hook_flags,
        token_flags::BEFORE_TRANSFER | token_flags::BEFORE_BURN | token_flags::WRITES_HOOK_DATA
    );
    assert_eq!((m.hook_authority, m.mint_authority), (None, None));
    assert_eq!((m.supply, m.max_supply), (SUPPLY, SUPPLY));
    let c = config(&w, &d);
    assert_eq!(
        (c.modules, c.eligible, c.min_eligible, c.max_wallet_amount),
        (15, 0, SUPPLY / 1_000, SUPPLY / 50)
    );
    assert_eq!((c.launch, c.pool, c.creator), (t.launch, t.pool, t.creator));
    assert_eq!(c.reward_vault, kit::reward_vault_address(&t.mint, &w.sol));
    let vault: Holding = w.env.read(&c.reward_vault);
    assert_eq!(
        (vault.mint, vault.owner, vault.amount),
        (w.sol, t.kit_config, 0)
    );
    // The registry: the config (writable), then the reward vault (read-only).
    let list =
        HookAccountList::decode(&w.env.account(&kit::registry_address(&t.mint)).unwrap().data)
            .unwrap();
    let extras = list
        .resolve(
            &[Pubkey::default(); 5],
            &Pubkey::default(),
            &Pubkey::default(),
        )
        .unwrap();
    assert_eq!(extras, kit::hook_extras(&t.mint, Some(c.reward_vault)));
    // Three quarters went to the pool, excluded to excluded: nobody is eligible yet.
    assert_eq!(bal(&w, &d, &t.pool), SUPPLY / 4 * 3);
    assert_eq!(bal(&w, &d, &t.launch), SUPPLY / 4);
    assert_eq!(w.env.holding_state(&t.mint, &t.pool).1, [0; 64]);

    // Without holder rewards the registry names the kit itself for the absent vault.
    let (w, d) = setup(modules::MAX_WALLET | modules::CREATOR_WALLET_LOCK);
    let list = HookAccountList::decode(
        &w.env
            .account(&kit::registry_address(&d.token.mint))
            .unwrap()
            .data,
    )
    .unwrap();
    let extras = list
        .resolve(
            &[Pubkey::default(); 5],
            &Pubkey::default(),
            &Pubkey::default(),
        )
        .unwrap();
    assert_eq!(extras[1].pubkey, bordrless_kit::ID);
    assert_eq!(
        w.env.read::<Mint>(&d.token.mint).hook_flags,
        token_flags::BEFORE_TRANSFER
    );
    assert_eq!(config(&w, &d).reward_vault, Pubkey::default());
}

#[test]
fn the_curve_check_is_the_validate_point_syscall() {
    let mut w = World::new();
    let mut keys = vec![
        Pubkey::default(),
        bordrless_token::ID,
        bordrless_swap::ID,
        bordrless_bridge::ID,
        bordrless_launch::ID,
        bordrless_kit::ID,
        kit::kit_config_address(&Pubkey::new_unique()),
        kit::kit_caller_address(&Pubkey::new_unique()).0,
        token::hook_signer(&bordrless_kit::ID),
    ];
    for _ in 0..7 {
        keys.push(Keypair::new().pubkey());
    }
    let host: Vec<u8> = keys.iter().map(|k| u8::from(k.is_on_curve())).collect();
    // The default key (all zeros) and every program id are on the curve: the kit refuses them by
    // name. PDAs are off it.
    assert_eq!(&host[..9], &[1, 1, 1, 1, 1, 1, 0, 0, 0]);
    assert!(host[9..].iter().all(|b| *b == 1));
    let payer = w.env.funded(SOL);
    let checked = w.env.send_paid_by(
        &[hook_tester::client::probe_curve(keys.clone(), true)],
        &payer,
        &[],
    );
    assert_eq!(
        checked.return_data(),
        host,
        "the syscall agrees with the host"
    );
    let skipped = w.env.send_paid_by(
        &[hook_tester::client::probe_curve(keys.clone(), false)],
        &payer,
        &[],
    );
    skipped.ok();
    let per_key = (checked.cu() - skipped.cu()) as f64 / keys.len() as f64;
    println!(
        "curve25519 validate-point (Pubkey::is_on_curve): {per_key:.1} CU per key \
         ({} keys: {} CU checked, {} CU skipped)",
        keys.len(),
        checked.cu(),
        skipped.cu()
    );
    assert!((159.0..200.0).contains(&per_key), "{per_key}");
}

#[test]
fn callbacks_bind_every_account() {
    let mut w = World::new();
    let a = w.direct_kit(&KitSpec::new(modules::HOLDER_REWARDS));
    let b = w.direct_kit(&KitSpec::new(modules::HOLDER_REWARDS));
    let off = w.direct_kit(&KitSpec::new(modules::MAX_WALLET));
    let alice = holder(&mut w, &a);
    let bob = holder(&mut w, &a);
    buy(&mut w, &a, &alice.pubkey(), 10 * TOKEN).ok();
    w.env
        .send_paid_by(
            &[token::create_holding(
                alice.pubkey(),
                off.token.mint,
                alice.pubkey(),
            )],
            &alice,
            &[],
        )
        .ok();
    buy(&mut w, &off, &alice.pubkey(), 10 * TOKEN).ok();
    w.holdings(&alice, off.token.mint, &[bob.pubkey()]);
    let va = a.token.reward_vault.unwrap();
    let vb = b.token.reward_vault.unwrap();
    let send = |w: &mut World, ix: Instruction| w.env.send_paid_by(&[ix], &alice, &[]);

    // The registry's own accounts work.
    let ix = transfer_with(
        &a,
        &alice,
        &bob.pubkey(),
        kit::hook_extras(&a.token.mint, Some(va)),
    );
    send(&mut w, ix).ok();
    // Another token's config (and its vault): refused.
    let ix = transfer_with(
        &a,
        &alice,
        &bob.pubkey(),
        vec![
            AccountMeta::new(b.token.kit_config, false),
            AccountMeta::new_readonly(vb, false),
        ],
    );
    send(&mut w, ix).expect_code(k(KitError::WrongMint));
    // A copy of the config owned by another program: refused by its owner.
    let fake = Pubkey::new_unique();
    let mut account = w.env.account(&a.token.kit_config).unwrap();
    account.owner = hook_tester::ID;
    w.env.put(fake, account);
    let ix = transfer_with(
        &a,
        &alice,
        &bob.pubkey(),
        vec![
            AccountMeta::new(fake, false),
            AccountMeta::new_readonly(va, false),
        ],
    );
    send(&mut w, ix).expect_code(anchor_err::ACCOUNT_OWNED_BY_WRONG_PROGRAM);
    // The config passed read-only: refused.
    let ix = transfer_with(
        &a,
        &alice,
        &bob.pubkey(),
        vec![
            AccountMeta::new_readonly(a.token.kit_config, false),
            AccountMeta::new_readonly(va, false),
        ],
    );
    send(&mut w, ix).expect_code(anchor_err::CONSTRAINT_MUT);
    // A wrong reward vault (another token's), or none, with holder rewards on: refused.
    let ix = transfer_with(
        &a,
        &alice,
        &bob.pubkey(),
        vec![
            AccountMeta::new(a.token.kit_config, false),
            AccountMeta::new_readonly(vb, false),
        ],
    );
    send(&mut w, ix).expect_code(k(KitError::WrongRewardVault));
    let ix = transfer_with(
        &a,
        &alice,
        &bob.pubkey(),
        kit::hook_extras(&a.token.mint, None),
    );
    send(&mut w, ix).expect_code(k(KitError::MissingRewardVault));
    // A reward vault passed for a token without holder rewards: refused.
    let ix = transfer_with(
        &off,
        &alice,
        &bob.pubkey(),
        vec![
            AccountMeta::new(off.token.kit_config, false),
            AccountMeta::new_readonly(va, false),
        ],
    );
    send(&mut w, ix).expect_code(k(KitError::WrongRewardVault));
    let ix = transfer_with(
        &off,
        &alice,
        &bob.pubkey(),
        kit::hook_extras(&off.token.mint, None),
    );
    send(&mut w, ix).ok();

    // The callbacks answer the token program only.
    let args = TokenHookArgs {
        op: TokenOp::Transfer,
        phase: Phase::Before,
        mint: a.token.mint,
        source: token::holding_address(&a.token.mint, &alice.pubkey()),
        destination: token::holding_address(&a.token.mint, &bob.pubkey()),
        source_owner: alice.pubkey(),
        destination_owner: bob.pubkey(),
        authority: alice.pubkey(),
        authority_is_delegate: false,
        amount: 1,
        delta: 0,
        source_balance: 1_000,
        destination_balance: 0,
        decimals: 6,
        supply: SUPPLY,
        source_hook_data: [0; 64],
        destination_hook_data: [0; 64],
    };
    let direct = |signer: Pubkey, signs: bool| Instruction {
        program_id: bordrless_kit::ID,
        accounts: vec![
            AccountMeta::new_readonly(signer, signs),
            AccountMeta::new_readonly(a.token.mint, false),
            AccountMeta::new_readonly(args.source, false),
            AccountMeta::new_readonly(args.destination, false),
            AccountMeta::new_readonly(alice.pubkey(), false),
            AccountMeta::new(a.token.kit_config, false),
            AccountMeta::new_readonly(va, false),
        ],
        data: bordrless_kit::instruction::BeforeTransfer { args: args.clone() }.data(),
    };
    let forger = Keypair::new();
    let tx = w
        .env
        .send_paid_by(&[direct(forger.pubkey(), true)], &alice, &[&forger]);
    tx.expect_code(k(KitError::BadHookSigner));
    let tx = w
        .env
        .send_paid_by(&[direct(TOKEN_HOOK_AUTHORITY, false)], &alice, &[]);
    tx.expect_code(anchor_err::CONSTRAINT_SIGNER);
}

#[test]
fn a_mint_that_names_the_kit_without_a_config_never_moves() {
    let mut w = World::new();
    let a = w.direct_kit(&KitSpec::new(modules::HOLDER_REWARDS));
    let owner = w.env.funded(10 * SOL);
    let bob = Pubkey::new_unique();
    // Anyone can create a mint whose hook is the kit, with the kit's flags.
    let mint = Keypair::new();
    let f = mint.pubkey();
    let args = |flags: u16| CreateMintArgs {
        decimals: 6,
        name: "Look-alike".to_string(),
        symbol: "FAKE".to_string(),
        uri: String::new(),
        max_supply: 0,
        mint_authority: Some(owner.pubkey()),
        freeze_authority: None,
        hook_program: Some(bordrless_kit::ID),
        hook_flags: flags,
        hook_authority: None,
        metadata_authority: None,
    };
    let ixs = [
        token::create_mint(owner.pubkey(), f, args(mint_flags(modules::ALL))),
        token::create_holding(owner.pubkey(), f, owner.pubkey()),
        token::create_holding(owner.pubkey(), f, bob),
        token::mint_to(
            owner.pubkey(),
            f,
            token::holding_address(&f, &owner.pubkey()),
            Some(bordrless_kit::ID),
            vec![],
            1_000,
        ),
    ];
    w.env.send_paid_by(&ixs, &owner, &[&mint]).ok();
    let move_with = |w: &mut World, extras: Vec<AccountMeta>| {
        let ix = token::transfer(
            owner.pubkey(),
            token::holding_address(&f, &owner.pubkey()),
            token::holding_address(&f, &bob),
            f,
            Some(bordrless_kit::ID),
            extras,
            1,
        );
        w.env.send_paid_by(&[ix], &owner, &[])
    };
    // Its own config does not exist; any other token's config is not its own.
    move_with(&mut w, kit::hook_extras(&f, None)).expect_code(anchor_err::ACCOUNT_NOT_INITIALIZED);
    move_with(
        &mut w,
        vec![
            AccountMeta::new(a.token.kit_config, false),
            AccountMeta::new_readonly(a.token.reward_vault.unwrap(), false),
        ],
    )
    .expect_code(k(KitError::WrongMint));
    let burn = token::burn(
        owner.pubkey(),
        token::holding_address(&f, &owner.pubkey()),
        f,
        Some(bordrless_kit::ID),
        kit::hook_extras(&f, None),
        1,
    );
    w.env
        .send_paid_by(&[burn], &owner, &[])
        .expect_code(anchor_err::ACCOUNT_NOT_INITIALIZED);
    assert_eq!(w.env.holding(&f, &owner.pubkey()), 1_000);
    // The kit answers no mint: a mint that subscribes the kit to mints cannot even be minted.
    let mint2 = Keypair::new();
    let ixs = [
        token::create_mint(
            owner.pubkey(),
            mint2.pubkey(),
            args(token_flags::BEFORE_TRANSFER | token_flags::BEFORE_MINT),
        ),
        token::create_holding(owner.pubkey(), mint2.pubkey(), owner.pubkey()),
    ];
    w.env.send_paid_by(&ixs, &owner, &[&mint2]).ok();
    let ix = token::mint_to(
        owner.pubkey(),
        mint2.pubkey(),
        token::holding_address(&mint2.pubkey(), &owner.pubkey()),
        Some(bordrless_kit::ID),
        kit::hook_extras(&mint2.pubkey(), None),
        1,
    );
    w.env
        .send_paid_by(&[ix], &owner, &[])
        .expect_code(anchor_err::INSTRUCTION_FALLBACK_NOT_FOUND);
}

/// A mint ready for `init` (nothing of the kit written yet): the kit as hook with `flags`, no hook
/// authority, `supply` minted to `launch`, the mint authority revoked.
fn bare_mint(
    w: &mut World,
    launch: &Keypair,
    flags: u16,
    supply: u64,
    hook_authority: bool,
) -> Pubkey {
    let mint = Keypair::new();
    let m = mint.pubkey();
    let ixs = [
        token::create_mint(
            launch.pubkey(),
            m,
            CreateMintArgs {
                decimals: 6,
                name: "Bare".to_string(),
                symbol: "BARE".to_string(),
                uri: String::new(),
                max_supply: supply,
                mint_authority: Some(launch.pubkey()),
                freeze_authority: None,
                hook_program: Some(bordrless_kit::ID),
                hook_flags: flags,
                hook_authority: hook_authority.then(|| launch.pubkey()),
                metadata_authority: None,
            },
        ),
        token::create_holding(launch.pubkey(), m, launch.pubkey()),
        token::mint_to(
            launch.pubkey(),
            m,
            token::holding_address(&m, &launch.pubkey()),
            Some(bordrless_kit::ID),
            vec![],
            supply,
        ),
        token::set_authority(launch.pubkey(), m, AuthorityKind::Mint, None),
    ];
    w.env.send_paid_by(&ixs, launch, &[&mint]).ok();
    m
}

#[test]
fn init_is_refused_without_the_kit_caller_and_outside_its_bounds() {
    let mut w = World::new();
    let launch = w.env.funded(100 * SOL);
    let payer = w.env.funded(100 * SOL);
    let now = w.env.now;
    let all = modules::ALL;
    let args = |mint: &Pubkey, reward_mint: Pubkey| KitInitArgs {
        launch: launch.pubkey(),
        pool: Pubkey::new_unique(),
        creator: Pubkey::new_unique(),
        reward_mint,
        modules: all,
        max_wallet_bps: 200,
        creator_unlock_at: now + 86_400,
        early_window_end: now + 60,
        early_unlock_at: now + 3_600,
        kit_caller_bump: kit::kit_caller_address(mint).1,
    };
    let send = |w: &mut World, ix: Instruction, signers: &[&Keypair]| {
        w.env.send_paid_by(&[ix], &payer, signers)
    };
    let mint = bare_mint(&mut w, &launch, mint_flags(all), SUPPLY, false);
    let (caller, _) = kit::kit_caller_address(&mint);
    let sol = w.sol;

    // The real kit-caller PDA, but not signing (only the launch program can sign for it).
    let mut ix = kit::init(caller, payer.pubkey(), mint, args(&mint, sol));
    ix.accounts[0].is_signer = false;
    send(&mut w, ix, &[]).expect_code(anchor_err::ACCOUNT_NOT_SIGNER);
    // Any other signer.
    let impostor = Keypair::new();
    let ix = kit::init(impostor.pubkey(), payer.pubkey(), mint, args(&mint, sol));
    send(&mut w, ix, &[&impostor]).expect_code(k(KitError::NotKitCaller));
    // The right bump for another mint's caller does not help either.
    let other = bare_mint(&mut w, &launch, mint_flags(all), SUPPLY, false);
    let mut a = args(&mint, sol);
    a.kit_caller_bump = kit::kit_caller_address(&other).1.wrapping_add(1);
    let ix = kit::init(impostor.pubkey(), payer.pubkey(), mint, a);
    send(&mut w, ix, &[&impostor]).expect_code(k(KitError::NotKitCaller));
    assert!(w.env.account(&kit::kit_config_address(&mint)).is_none());

    // The bounds are checked before the caller, so each refusal shows here.
    let small = bare_mint(&mut w, &launch, mint_flags(all), 999, false);
    let ix = kit::init(impostor.pubkey(), payer.pubkey(), small, args(&small, sol));
    send(&mut w, ix, &[&impostor]).expect_code(k(KitError::SupplyOutOfBounds));
    let floor = bare_mint(&mut w, &launch, mint_flags(all), 1_000, false);
    let ix = kit::init(impostor.pubkey(), payer.pubkey(), floor, args(&floor, sol));
    send(&mut w, ix, &[&impostor]).expect_code(k(KitError::NotKitCaller));
    // A reward mint with a hook, or one that could get a hook or freeze the vault.
    let owner = w.env.funded(10 * SOL);
    let hooked = w.tax_mint(&owner, 9, 1_000, "TAX", owner.pubkey(), 100);
    let ix = kit::init(impostor.pubkey(), payer.pubkey(), mint, args(&mint, hooked));
    send(&mut w, ix, &[&impostor]).expect_code(k(KitError::BadRewardMint));
    let hookable = w.mint_to_owner(&owner, 9, 1_000, "HKA");
    let ix = kit::init(
        impostor.pubkey(),
        payer.pubkey(),
        mint,
        args(&mint, hookable),
    );
    send(&mut w, ix, &[&impostor]).expect_code(k(KitError::BadRewardMint));
    // A mint not set up for the kit: a hook authority, or flags for other modules.
    let loose = bare_mint(&mut w, &launch, mint_flags(all), SUPPLY, true);
    let ix = kit::init(impostor.pubkey(), payer.pubkey(), loose, args(&loose, sol));
    send(&mut w, ix, &[&impostor]).expect_code(k(KitError::WrongMintSetup));
    let flagged = bare_mint(&mut w, &launch, token_flags::BEFORE_TRANSFER, SUPPLY, false);
    let ix = kit::init(
        impostor.pubkey(),
        payer.pubkey(),
        flagged,
        args(&flagged, sol),
    );
    send(&mut w, ix, &[&impostor]).expect_code(k(KitError::WrongMintSetup));
    // The reserve must hold the whole supply.
    let moved = bare_mint(&mut w, &launch, mint_flags(all), SUPPLY, false);
    w.holdings(&launch, moved, &[owner.pubkey()]);
    let ix = token::transfer(
        launch.pubkey(),
        token::holding_address(&moved, &launch.pubkey()),
        token::holding_address(&moved, &owner.pubkey()),
        moved,
        Some(bordrless_kit::ID),
        vec![],
        1,
    );
    // (No config yet: the kit refuses the transfer, so build the reserve shortfall by burning.)
    w.env.send_paid_by(&[ix], &launch, &[]).expect_fail();
    let ix = kit::init(impostor.pubkey(), payer.pubkey(), moved, {
        let mut a = args(&moved, sol);
        a.launch = owner.pubkey();
        a
    });
    send(&mut w, ix, &[&impostor]).expect_code(k(KitError::WrongReserve));
    // Parameters outside the hard bounds.
    let mut a = args(&mint, sol);
    a.modules = 0;
    let ix = kit::init(impostor.pubkey(), payer.pubkey(), mint, a);
    send(&mut w, ix, &[&impostor]).expect_code(k(KitError::InvalidModules));
    let mut a = args(&mint, sol);
    a.max_wallet_bps = 10_000;
    let ix = kit::init(impostor.pubkey(), payer.pubkey(), mint, a);
    send(&mut w, ix, &[&impostor]).expect_code(k(KitError::InvalidMaxWallet));
    let mut a = args(&mint, sol);
    a.creator_unlock_at = now + MAX_CREATOR_LOCK_SECS + 1;
    let ix = kit::init(impostor.pubkey(), payer.pubkey(), mint, a);
    send(&mut w, ix, &[&impostor]).expect_code(k(KitError::InvalidCreatorLock));
    let mut a = args(&mint, sol);
    a.early_window_end = a.early_unlock_at;
    let ix = kit::init(impostor.pubkey(), payer.pubkey(), mint, a);
    send(&mut w, ix, &[&impostor]).expect_code(k(KitError::InvalidEarlyLock));
    // Holder rewards need the vault: without it, refused.
    let mut ix = kit::init(impostor.pubkey(), payer.pubkey(), mint, args(&mint, sol));
    ix.accounts[7] = AccountMeta::new_readonly(bordrless_kit::ID, false);
    send(&mut w, ix, &[&impostor]).expect_code(k(KitError::MissingRewardVault));
    assert!(w.env.account(&kit::kit_config_address(&mint)).is_none());
}

#[test]
fn graduate_claim_and_share_bind_their_accounts() {
    let mut w = World::new();
    let d = w.direct_kit(&KitSpec::new(modules::HOLDER_REWARDS));
    let other = w.direct_kit(&KitSpec::new(modules::HOLDER_REWARDS));
    let off = w.direct_kit(&KitSpec::new(modules::MAX_WALLET));
    let t = d.token.clone();
    let alice = holder(&mut w, &d);
    let bob = holder(&mut w, &d);
    buy(&mut w, &d, &alice.pubkey(), SUPPLY / 500).ok();
    buy(&mut w, &d, &bob.pubkey(), SUPPLY / 500).ok();
    w.donate(&alice, &t, SOL).ok();

    // graduate: only the kit-caller PDA (which only the launch can sign for).
    let impostor = Keypair::new();
    let tx = w.env.send_paid_by(
        &[kit::graduate(impostor.pubkey(), t.mint)],
        &alice,
        &[&impostor],
    );
    tx.expect_code(k(KitError::NotKitCaller));
    let mut ix = kit::graduate(kit::kit_caller_address(&t.mint).0, t.mint);
    ix.accounts[0].is_signer = false;
    w.env
        .send_paid_by(&[ix], &alice, &[])
        .expect_code(anchor_err::ACCOUNT_NOT_SIGNER);
    assert!(!config(&w, &d).graduated);

    // Claims: the pool and the launch are not holders.
    w.holdings(&alice, w.sol, &[t.pool, t.launch]);
    for excluded in [&d.pool, &d.launch] {
        w.kit_claim(excluded, &t)
            .expect_code(k(KitError::NotAHolder));
    }
    let claim = |w: &mut World, signer: &Keypair, edit: &dyn Fn(&mut Instruction)| {
        let mut ix = kit::claim(signer.pubkey(), t.mint, t.reward_mint);
        edit(&mut ix);
        w.env.send_paid_by(&[ix], signer, &[])
    };
    // A holding of another mint, another owner's holding, a delegate signing for the owner's.
    w.env
        .send_paid_by(
            &[token::create_holding(
                alice.pubkey(),
                other.token.mint,
                alice.pubkey(),
            )],
            &alice,
            &[],
        )
        .ok();
    let alice_other = token::holding_address(&other.token.mint, &alice.pubkey());
    claim(&mut w, &alice, &|ix| ix.accounts[3].pubkey = alice_other)
        .expect_code(k(KitError::WrongHolding));
    let bob_holding = token::holding_address(&t.mint, &bob.pubkey());
    claim(&mut w, &alice, &|ix| ix.accounts[3].pubkey = bob_holding)
        .expect_code(k(KitError::WrongHolding));
    let delegate = w.wallet_with_sol(SOL);
    w.env
        .send_paid_by(
            &[token::approve(
                bob.pubkey(),
                bob_holding,
                delegate.pubkey(),
                u64::MAX,
            )],
            &bob,
            &[],
        )
        .ok();
    claim(&mut w, &delegate, &|ix| ix.accounts[3].pubkey = bob_holding)
        .expect_code(k(KitError::WrongHolding));
    // A destination owned by someone else, or of another mint.
    let bob_sol = token::holding_address(&w.sol, &bob.pubkey());
    claim(&mut w, &alice, &|ix| ix.accounts[6].pubkey = bob_sol)
        .expect_code(k(KitError::WrongDestination));
    let alice_tokens = token::holding_address(&other.token.mint, &alice.pubkey());
    claim(&mut w, &alice, &|ix| ix.accounts[6].pubkey = alice_tokens)
        .expect_code(k(KitError::WrongDestination));
    // Another token's config, mint, vault or reward mint; another hook authority.
    let other_vault = other.token.reward_vault.unwrap();
    claim(&mut w, &alice, &|ix| {
        ix.accounts[1].pubkey = other.token.kit_config
    })
    .expect_code(k(KitError::WrongMint));
    claim(&mut w, &alice, &|ix| {
        ix.accounts[2].pubkey = other.token.mint
    })
    .expect_code(k(KitError::WrongMint));
    claim(&mut w, &alice, &|ix| ix.accounts[5].pubkey = other_vault)
        .expect_code(k(KitError::WrongRewardVault));
    claim(&mut w, &alice, &|ix| {
        ix.accounts[4].pubkey = other.token.mint
    })
    .expect_code(k(KitError::WrongRewardMint));
    let fake_authority = hook_tester::client::hook_authority().0;
    claim(&mut w, &alice, &|ix| ix.accounts[7].pubkey = fake_authority)
        .expect_code(k(KitError::WrongProgram));
    // Holder rewards off.
    let carol = holder(&mut w, &off);
    w.kit_claim(&carol, &off.token)
        .expect_code(k(KitError::RewardsOff));
    // The real claim, then nothing left.
    let tx = w.kit_claim(&alice, &t);
    tx.ok();
    let ev: RewardsClaimed = tx.event();
    assert_eq!(
        (ev.owner, ev.amount, ev.owed_left),
        (alice.pubkey(), SOL / 2, 0)
    );
    assert_eq!(
        w.env.holding_state(&t.mint, &alice.pubkey()).0,
        SUPPLY / 500
    );
    w.kit_claim(&alice, &t)
        .expect_code(k(KitError::NothingToClaim));

    // Shares: at least 0.001 SOL, holder rewards on, someone eligible.
    w.kit_share(&alice, &t, MIN_SHARE_LAMPORTS - 1)
        .expect_code(k(KitError::ShareTooSmall));
    w.kit_share(&carol, &off.token, SOL)
        .expect_code(k(KitError::RewardsOff));
    let lonely = holder(&mut w, &other);
    w.kit_share(&lonely, &other.token, SOL)
        .expect_code(k(KitError::NoEligibleHolders));
    let tx = w.kit_share(&alice, &t, MIN_SHARE_LAMPORTS);
    tx.ok();
    let ev: RewardsShared = tx.event();
    assert_eq!((ev.from, ev.amount), (alice.pubkey(), MIN_SHARE_LAMPORTS));
    // Into another token's vault: refused.
    let mut ix = kit::share(
        alice.pubkey(),
        t.mint,
        token::holding_address(&w.sol, &alice.pubkey()),
        w.sol,
        MIN_SHARE_LAMPORTS,
    );
    ix.accounts[4].pubkey = other_vault;
    w.env
        .send_paid_by(&[ix], &alice, &[])
        .expect_code(k(KitError::WrongRewardVault));
    // From someone else's holding: the token program refuses.
    let ix = kit::share(alice.pubkey(), t.mint, bob_sol, w.sol, MIN_SHARE_LAMPORTS);
    w.env.send_paid_by(&[ix], &alice, &[]).expect_fail();
    check_kit(&w.env, &t, &[alice.pubkey(), bob.pubkey()]);
}

/// What each module does, checked for one module set: each rule bites exactly when its module is
/// on.
fn run_module_set(m: u8) {
    let (mut w, d) = setup(m);
    let t = d.token.clone();
    let on = |module: u8| m & module != 0;
    let c0 = config(&w, &d);
    assert_eq!(w.env.read::<Mint>(&t.mint).hook_flags, mint_flags(m));
    let (e, f, g) = (holder(&mut w, &d), holder(&mut w, &d), holder(&mut w, &d));
    let creator = d.creator.insecure_clone();
    w.holdings(&creator, w.sol, &[creator.pubkey()]);
    let mut owners = vec![e.pubkey(), f.pubkey(), g.pubkey(), creator.pubkey()];

    // In the early window: e buys 1% of the supply.
    buy(&mut w, &d, &e.pubkey(), SUPPLY / 100).ok();
    let locked = data(&w, &d, &e.pubkey()).early_locked;
    assert_eq!(
        locked,
        if on(modules::EARLY_BUYER_LOCK) {
            SUPPLY / 100
        } else {
            0
        }
    );
    let tx = w.send_tokens(&e, t.mint, &f.pubkey(), TOKEN);
    if on(modules::EARLY_BUYER_LOCK) {
        tx.expect_code(k(KitError::EarlyLocked));
    } else {
        tx.ok();
    }

    // After the window: the creator buys, then tries to send.
    w.env.warp(61);
    buy(&mut w, &d, &creator.pubkey(), SUPPLY / 200).ok();
    let tx = w.send_tokens(&creator, t.mint, &f.pubkey(), TOKEN);
    if on(modules::CREATOR_WALLET_LOCK) {
        tx.expect_code(k(KitError::CreatorLocked));
    } else {
        tx.ok();
    }

    // Max wallet: g fills up to the cap; one unit more only when the module is off.
    let cap = SUPPLY / 50;
    buy(&mut w, &d, &g.pubkey(), cap).ok();
    let tx = buy(&mut w, &d, &g.pubkey(), 1);
    if on(modules::MAX_WALLET) {
        tx.expect_code(k(KitError::MaxWalletExceeded));
    } else {
        tx.ok();
    }

    // Wallets only: a PDA may hold the token only without holder rewards.
    let pda = kit::kit_caller_address(&t.mint).0;
    w.holdings(&g, t.mint, &[pda]);
    owners.push(pda);
    let tx = w.send_tokens(&g, t.mint, &pda, TOKEN);
    if on(modules::HOLDER_REWARDS) {
        tx.expect_code(k(KitError::DestinationNotAllowed));
    } else {
        tx.ok();
    }
    // The refused destinations, whatever the modules.
    w.holdings(
        &g,
        t.mint,
        &[t.launch, t.kit_config, Pubkey::default(), bordrless_kit::ID],
    );
    for to in [t.launch, t.kit_config, Pubkey::default(), bordrless_kit::ID] {
        w.send_tokens(&g, t.mint, &to, 1)
            .expect_code(k(KitError::DestinationNotAllowed));
    }

    // Holder rewards: a donation reaches g, the only eligible holder... or claims are off.
    if on(modules::HOLDER_REWARDS) {
        w.donate(&g, &t, 1_000_000).ok();
        let tx = w.kit_claim(&g, &t);
        tx.ok();
        assert!(tx.event::<RewardsClaimed>().amount > 0);
    } else {
        w.kit_claim(&g, &t).expect_code(k(KitError::RewardsOff));
    }

    // After every lock: everything moves; at graduation max wallet lifts.
    w.env.warp(31 * 86_400);
    w.send_tokens(&e, t.mint, &f.pubkey(), TOKEN).ok();
    w.send_tokens(&creator, t.mint, &f.pubkey(), TOKEN).ok();
    assert_eq!(data(&w, &d, &e.pubkey()).early_locked, 0);
    graduate(&mut w, &d);
    buy(&mut w, &d, &g.pubkey(), cap).ok();
    assert!(bal(&w, &d, &g.pubkey()) > cap);
    let all = bal(&w, &d, &g.pubkey());
    sell(&mut w, &d, &g, all).ok();
    assert_eq!(config(&w, &d).max_wallet_amount, c0.max_wallet_amount);
    check_kit(&w.env, &t, &owners);
}

#[test]
fn every_module_set_runs_exactly_its_rules() {
    // Each module alone, each pair, each triple and all four.
    for m in 1..=modules::ALL {
        run_module_set(m);
    }
}

#[test]
fn a_wallet_to_wallet_transfer_with_every_rule_pays_nothing() {
    let (mut w, d) = setup(modules::ALL);
    let t = d.token.clone();
    let (a, b) = (holder(&mut w, &d), holder(&mut w, &d));
    w.env.warp(61);
    buy(&mut w, &d, &a.pubkey(), SUPPLY / 100).ok();
    buy(&mut w, &d, &b.pubkey(), SUPPLY / 100).ok();
    w.donate(&a, &t, SOL).ok();
    let before = (
        w.env.vault_amount(&t),
        bal(&w, &d, &t.pool),
        bal(&w, &d, &t.launch),
        config(&w, &d).eligible,
    );
    let ix = w.kit_transfer_ix(t.mint, a.pubkey(), &a.pubkey(), &b.pubkey(), 12_345);
    let tx = w.env.send_paid_by(&[ix], &a, &[]);
    tx.ok();
    println!(
        "wallet-to-wallet transfer, every rule: CU {} size {} trace {} height {}",
        tx.cu(),
        tx.size,
        tx.trace_len(),
        tx.max_height()
    );
    assert!(tx.cu() < 60_000, "{}", tx.cu());
    assert_eq!(bal(&w, &d, &b.pubkey()), SUPPLY / 100 + 12_345);
    assert_eq!(bal(&w, &d, &a.pubkey()), SUPPLY / 100 - 12_345);
    assert_eq!(
        (
            w.env.vault_amount(&t),
            bal(&w, &d, &t.pool),
            bal(&w, &d, &t.launch),
            config(&w, &d).eligible
        ),
        before
    );
    assert_eq!(tx.max_height(), 2);
    let ev: bordrless_token::events::Transferred = tx.event();
    assert!(ev.deltas.is_empty());
    check_kit(&w.env, &t, &[a.pubkey(), b.pubkey()]);
}

#[test]
fn with_holder_rewards_no_program_can_hold_the_token() {
    let (mut w, d) = setup(modules::HOLDER_REWARDS);
    let t = d.token.clone();
    let a = holder(&mut w, &d);
    buy(&mut w, &d, &a.pubkey(), SUPPLY / 100).ok();
    let somewhere = Pubkey::find_program_address(&[b"vault"], &hook_tester::ID).0;
    let refused = [
        somewhere,
        t.kit_config,
        t.launch,
        Pubkey::default(),
        bordrless_token::ID,
        bordrless_swap::ID,
        bordrless_bridge::ID,
        bordrless_launch::ID,
        bordrless_kit::ID,
    ];
    w.holdings(&a, t.mint, &refused);
    for to in refused {
        let before = bal(&w, &d, &to);
        w.send_tokens(&a, t.mint, &to, 1)
            .expect_code(k(KitError::DestinationNotAllowed));
        assert_eq!(bal(&w, &d, &to), before);
    }
    // A wallet and the pool are fine.
    let b = Keypair::new().pubkey();
    w.holdings(&a, t.mint, &[b]);
    w.send_tokens(&a, t.mint, &b, 1).ok();
    sell(&mut w, &d, &a, 1).ok();

    // A second pool cannot be created: its vault belongs to a PDA.
    let spec = NewPool {
        base: t.mint,
        quote: w.sol,
        lp_fee_bps: 25,
        tester_flags: None,
        extras: vec![],
        base_amount: TOKEN,
        quote_amount: SOL / 10,
    };
    let (pool2, tx) = w.new_pool(&a, &spec);
    tx.expect_code(k(KitError::DestinationNotAllowed));
    assert!(w.env.account(&pool2).is_none());
    check_kit(&w.env, &t, &[a.pubkey(), b]);
}

#[test]
fn max_wallet_caps_holders_and_never_a_sell_until_graduation() {
    let (mut w, d) = setup(modules::MAX_WALLET | modules::HOLDER_REWARDS);
    let t = d.token.clone();
    let cap = config(&w, &d).max_wallet_amount;
    assert_eq!(cap, SUPPLY / 50);
    let (m, n) = (holder(&mut w, &d), holder(&mut w, &d));
    // A buy to exactly the cap lands; one unit over is refused, from the pool or a wallet.
    buy(&mut w, &d, &m.pubkey(), cap).ok();
    buy(&mut w, &d, &m.pubkey(), 1).expect_code(k(KitError::MaxWalletExceeded));
    buy(&mut w, &d, &n.pubkey(), 10).ok();
    w.send_tokens(&n, t.mint, &m.pubkey(), 1)
        .expect_code(k(KitError::MaxWalletExceeded));
    // A sell into the pool is never blocked, however much the pool holds.
    sell(&mut w, &d, &m, cap).ok();
    assert!(bal(&w, &d, &t.pool) > cap * 30);
    w.send_tokens(&d.launch, t.mint, &t.pool, SUPPLY / 4).ok();
    // The cap is fixed at init: burns never tighten it.
    buy(&mut w, &d, &m.pubkey(), cap).ok();
    w.kit_burn(&m, t.mint, cap / 2).ok();
    buy(&mut w, &d, &m.pubkey(), cap / 2).ok();
    assert_eq!(config(&w, &d).max_wallet_amount, cap);
    buy(&mut w, &d, &m.pubkey(), 1).expect_code(k(KitError::MaxWalletExceeded));
    // It lifts at graduation.
    graduate(&mut w, &d);
    buy(&mut w, &d, &m.pubkey(), cap * 3).ok();
    assert_eq!(bal(&w, &d, &m.pubkey()), cap * 4);
    check_kit(&w.env, &t, &[m.pubkey(), n.pubkey()]);
}

#[test]
fn without_holder_rewards_a_second_pool_is_capped_like_a_wallet() {
    let (mut w, d) = setup(modules::MAX_WALLET);
    let t = d.token.clone();
    let cap = config(&w, &d).max_wallet_amount;
    let maker = holder(&mut w, &d);
    let trader = holder(&mut w, &d);
    buy(&mut w, &d, &maker.pubkey(), cap).ok();
    buy(&mut w, &d, &trader.pubkey(), cap).ok();
    // Another pool for the token, its vault filled to just under the cap.
    let spec = NewPool {
        base: t.mint,
        quote: w.sol,
        lp_fee_bps: 25,
        tester_flags: None,
        extras: vec![],
        base_amount: cap - 10 * TOKEN,
        quote_amount: SOL,
    };
    let (pool2, tx) = w.new_pool(&maker, &spec);
    tx.ok();
    assert_eq!(bal(&w, &d, &pool2), cap - 10 * TOKEN);
    // A sell into it that would put its vault over the cap is refused; a smaller one lands.
    let sell2 = |w: &mut World, amount: u64| {
        let ix = w
            .env
            .swap_ix(&SwapSpec::new(trader.pubkey(), pool2, 0, amount));
        w.env.send_paid_by(&[ix], &trader, &[])
    };
    sell2(&mut w, 10 * TOKEN + 1).expect_code(k(KitError::MaxWalletExceeded));
    sell2(&mut w, 10 * TOKEN).ok();
    assert_eq!(bal(&w, &d, &pool2), cap);
    // After graduation the cap is gone for that pool too.
    graduate(&mut w, &d);
    sell2(&mut w, 5 * TOKEN).ok();
    assert_eq!(bal(&w, &d, &pool2), cap + 5 * TOKEN);
}

#[test]
fn the_creator_wallet_lock() {
    let (mut w, d) = setup(modules::CREATOR_WALLET_LOCK | modules::HOLDER_REWARDS);
    let t = d.token.clone();
    let creator = d.creator.insecure_clone();
    w.wrap_sol(&creator, 5 * SOL).ok();
    let friend = holder(&mut w, &d);
    // The creator can receive (buy); it cannot sell, send, or let a delegate send.
    buy(&mut w, &d, &creator.pubkey(), SUPPLY / 100).ok();
    buy(&mut w, &d, &friend.pubkey(), SUPPLY / 100).ok();
    sell(&mut w, &d, &creator, TOKEN).expect_code(k(KitError::CreatorLocked));
    w.send_tokens(&creator, t.mint, &friend.pubkey(), TOKEN)
        .expect_code(k(KitError::CreatorLocked));
    let creator_holding = token::holding_address(&t.mint, &creator.pubkey());
    w.env
        .send_paid_by(
            &[token::approve(
                creator.pubkey(),
                creator_holding,
                friend.pubkey(),
                u64::MAX,
            )],
            &creator,
            &[],
        )
        .ok();
    let ix = w.kit_transfer_ix(
        t.mint,
        friend.pubkey(),
        &creator.pubkey(),
        &friend.pubkey(),
        1,
    );
    w.env
        .send_paid_by(&[ix], &friend, &[])
        .expect_code(k(KitError::CreatorLocked));
    // Claims and shares still work.
    w.donate(&friend, &t, SOL).ok();
    let tx = w.kit_claim(&creator, &t);
    tx.ok();
    assert_eq!(tx.event::<RewardsClaimed>().amount, SOL / 2);
    w.kit_share(&creator, &t, 10 * MIN_SHARE_LAMPORTS).ok();
    // Burning extracts nothing: allowed.
    w.kit_burn(&creator, t.mint, TOKEN).ok();
    // One second before the unlock: still locked; at the unlock: everything works.
    let unlock = config(&w, &d).creator_unlock_at;
    w.env.warp(unlock - w.env.now - 1);
    sell(&mut w, &d, &creator, TOKEN).expect_code(k(KitError::CreatorLocked));
    w.env.warp(1);
    sell(&mut w, &d, &creator, TOKEN).ok();
    w.send_tokens(&creator, t.mint, &friend.pubkey(), TOKEN)
        .ok();
    check_kit(&w.env, &t, &[creator.pubkey(), friend.pubkey()]);

    // Liquidity: a token without holder rewards can have another pool, but the creator cannot
    // add to it before the unlock.
    let (mut w, d) = setup(modules::CREATOR_WALLET_LOCK);
    let t = d.token.clone();
    let creator = d.creator.insecure_clone();
    w.wrap_sol(&creator, 5 * SOL).ok();
    let maker = holder(&mut w, &d);
    buy(&mut w, &d, &maker.pubkey(), SUPPLY / 100).ok();
    buy(&mut w, &d, &creator.pubkey(), SUPPLY / 100).ok();
    let spec = NewPool {
        base: t.mint,
        quote: w.sol,
        lp_fee_bps: 25,
        tester_flags: None,
        extras: vec![],
        base_amount: SUPPLY / 1_000,
        quote_amount: SOL,
    };
    let (pool2, tx) = w.new_pool(&maker, &spec);
    tx.ok();
    let lp_mint = swap::lp_mint_address(&pool2);
    w.holdings(&creator, lp_mint, &[creator.pubkey()]);
    let add = |w: &World| {
        let c = creator.pubkey();
        let slice = w.env.token_hook_slice(
            &t.mint,
            &token::holding_address(&t.mint, &c),
            &swap::vault_address(&pool2, &t.mint),
            &c,
            &c,
            &pool2,
        );
        swap::add_liquidity(
            &swap::LiquidityKeys {
                provider: c,
                pool: pool2,
                base_mint: t.mint,
                quote_mint: w.sol,
                hook_program: None,
            },
            AddLiquidityArgs {
                base_desired: SUPPLY / 10_000,
                quote_desired: SOL / 10,
                min_lp: 1,
                base_hook_accounts: slice.len() as u8,
                quote_hook_accounts: 0,
                hook_data: vec![],
            },
            slice,
        )
    };
    let ix = add(&w);
    w.env
        .send_paid_by(&[ix], &creator, &[])
        .expect_code(k(KitError::CreatorLocked));
    w.env.warp(30 * 86_400);
    let ix = add(&w);
    w.env.send_paid_by(&[ix], &creator, &[]).ok();
    assert!(w.env.holding(&lp_mint, &creator.pubkey()) > 0);
}

#[test]
fn the_early_buyer_lock() {
    let (mut w, d) = setup(modules::EARLY_BUYER_LOCK | modules::HOLDER_REWARDS);
    let t = d.token.clone();
    let (e, f, g) = (holder(&mut w, &d), holder(&mut w, &d), holder(&mut w, &d));
    // Bought in the window: locked; it cannot be sold, sent or burned.
    buy(&mut w, &d, &e.pubkey(), SUPPLY / 200).ok();
    assert_eq!(data(&w, &d, &e.pubkey()).early_locked, SUPPLY / 200);
    sell(&mut w, &d, &e, 1).expect_code(k(KitError::EarlyLocked));
    w.send_tokens(&e, t.mint, &g.pubkey(), 1)
        .expect_code(k(KitError::EarlyLocked));
    w.kit_burn(&e, t.mint, 1)
        .expect_code(k(KitError::EarlyLocked));
    // Claiming during the lock pays, and still unlocks nothing.
    w.donate(&g, &t, SOL).ok();
    let tx = w.kit_claim(&e, &t);
    tx.ok();
    assert_eq!(tx.event::<RewardsClaimed>().amount, SOL);
    let after_claim = data(&w, &d, &e.pubkey());
    assert_eq!(after_claim.early_locked, SUPPLY / 200);
    assert_eq!(after_claim.owed, 0);
    sell(&mut w, &d, &e, 1).expect_code(k(KitError::EarlyLocked));
    // Tokens received later from a wallet can move: f buys after the window and sends to e.
    w.env.warp(60);
    buy(&mut w, &d, &f.pubkey(), SUPPLY / 200).ok();
    assert_eq!(data(&w, &d, &f.pubkey()).early_locked, 0);
    w.send_tokens(&f, t.mint, &e.pubkey(), 1_000).ok();
    sell(&mut w, &d, &e, 600).ok();
    w.kit_burn(&e, t.mint, 400).ok();
    sell(&mut w, &d, &e, 1).expect_code(k(KitError::EarlyLocked));
    // After the unlock everything moves; the data clears and the holding closes.
    let unlock = config(&w, &d).early_unlock_at;
    w.env.warp(unlock - w.env.now);
    w.donate(&g, &t, SOL).ok();
    sell(&mut w, &d, &e, SUPPLY / 200).ok();
    let left = data(&w, &d, &e.pubkey());
    assert_eq!((left.early_locked, left.snapshot), (0, 0));
    assert!(left.owed > 0);
    let e_holding = token::holding_address(&t.mint, &e.pubkey());
    let close = token::close_holding(e.pubkey(), t.mint, e_holding, e.pubkey());
    w.env
        .send_paid_by(std::slice::from_ref(&close), &e, &[])
        .expect_code(u32::from(
            bordrless_token::error::TokenError::HookDataNotEmpty,
        ));
    w.kit_claim(&e, &t).ok();
    assert_eq!(w.env.holding_state(&t.mint, &e.pubkey()), (0, [0; 64]));
    w.env.send_paid_by(&[close], &e, &[]).ok();
    assert!(w.env.account(&e_holding).is_none());
    check_kit(&w.env, &t, &[e.pubkey(), f.pubkey(), g.pubkey()]);
}

#[test]
fn compute_and_size_of_each_kit_path() {
    let (mut w, d) = setup(modules::ALL);
    let t = d.token.clone();
    let (a, b) = (holder(&mut w, &d), holder(&mut w, &d));
    let print = |name: &str, tx: &Tx| {
        tx.ok();
        println!(
            "{name}: CU {} size {} trace {} height {}",
            tx.cu(),
            tx.size,
            tx.trace_len(),
            tx.max_height()
        );
    };
    // A callback makes no CPI and emits no event: a transfer or a burn is the budget
    // instruction, the token instruction, the kit's callback and the token's own event, all at
    // height 2 at most.
    let callback_only = |tx: &Tx| {
        assert_eq!((tx.max_height(), tx.trace_len()), (2, 4));
    };
    let tx = buy(&mut w, &d, &a.pubkey(), SUPPLY / 100);
    print(
        "buy-side transfer (pool to holder), every rule, in the early window",
        &tx,
    );
    assert!(tx.cu() < 60_000);
    callback_only(&tx);
    w.env.warp(3_600);
    buy(&mut w, &d, &b.pubkey(), SUPPLY / 100).ok();
    w.donate(&a, &t, SOL).ok();
    let tx = buy(&mut w, &d, &a.pubkey(), TOKEN);
    print(
        "buy-side transfer after a donation (sync divides), every rule",
        &tx,
    );
    assert!(tx.cu() < 60_000);
    callback_only(&tx);
    let tx = sell(&mut w, &d, &a, TOKEN);
    print("sell-side transfer (holder to pool), every rule", &tx);
    assert!(tx.cu() < 60_000);
    callback_only(&tx);
    let tx = w.kit_burn(&a, t.mint, TOKEN);
    print("burn by a holder, every rule", &tx);
    assert!(tx.cu() < 60_000);
    callback_only(&tx);
    let tx = w.kit_claim(&a, &t);
    print("claim", &tx);
    assert!(tx.cu() < 100_000);
    assert_eq!(tx.max_height(), 3);
    let tx = w.kit_share(&b, &t, SOL);
    print("share", &tx);
    assert!(tx.cu() < 80_000);
    assert_eq!(tx.max_height(), 3);
    let tx = w.send_tokens(&d.launch, t.mint, &t.pool, TOKEN);
    print("excluded to excluded (launch to pool)", &tx);
    let program = w.env.account(&bordrless_kit::ID).unwrap();
    println!("kit program account: {} bytes", program.data.len());
}

#[test]
fn the_dex_fee_collector_is_an_ordinary_holder() {
    // The DEX takes its protocol fee in the quote token (§3.1), so the kit excludes only the pool
    // and the launch: the fee collector holds, counts and claims like anyone.
    let (mut w, d) = setup(modules::HOLDER_REWARDS);
    let t = d.token.clone();
    let collector = w.env.deployer.insecure_clone();
    let dex: bordrless_swap::state::Config = w.env.read(&swap::config_address());
    assert_eq!(dex.fee_collector, collector.pubkey());
    w.wrap_sol(&collector, 2 * SOL).ok();
    w.holdings(&collector, t.mint, &[collector.pubkey()]);
    buy(&mut w, &d, &collector.pubkey(), SUPPLY / 100).ok();
    assert_eq!(config(&w, &d).eligible, SUPPLY / 100);
    w.donate(&collector, &t, SOL).ok();
    let tx = w.kit_claim(&collector, &t);
    tx.ok();
    assert_eq!(tx.event::<RewardsClaimed>().amount, SOL);
    sell(&mut w, &d, &collector, SUPPLY / 100).ok();
    assert_eq!(config(&w, &d).eligible, 0);
}

#[test]
fn the_flags_fix_the_callbacks() {
    // Max wallet and the creator lock only need transfers: burns never reach the kit, and the
    // token program never reads an answer from it.
    let (mut w, d) = setup(modules::MAX_WALLET | modules::CREATOR_WALLET_LOCK);
    let a = holder(&mut w, &d);
    buy(&mut w, &d, &a.pubkey(), TOKEN).ok();
    let tx = w.kit_burn(&a, d.token.mint, TOKEN / 2);
    tx.ok();
    assert_eq!(tx.max_height(), 2, "the kit was not called on the burn");
    assert_eq!(data(&w, &d, &a.pubkey()), HolderData::default());
    let c = config(&w, &d);
    assert_eq!(c.eligible, TOKEN, "burns are not seen without the flag");
}
