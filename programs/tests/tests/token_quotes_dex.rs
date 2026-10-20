//! Quote tokens on the DEX (`docs/token-quotes.md` §2.1, §2.3): pools quoted in a real SPL Token
//! or Token-2022 mint (quote kind 2). Here the pool's hook is the test-only `hook_tester`, which
//! creates its own curve (`create_spl_pool` with its hook authority signing, through
//! `invoke_as_hook`) and answers scripted quote-side cuts, so the DEX's rules are exercised apart
//! from the launchpad (`token_quotes_launch.rs` runs the launchpad's legs). After every
//! instruction that moves a kind-2 pool's quote the invariant is asserted, exactly:
//!
//! quote vault (the pool's associated token account) == quote_reserve + protocol_fees_quote
//!
//! (plus what a test donated, where one does).

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_core::{policy, swap_amounts, Reserves};
use bordrless_hook::{pool_flags, AccountSource, Delta, ExtraAccount, HookReturn};
use bordrless_launch::client as launch;
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::quotes::{
    ext, pack_mint, xstock_extensions, QUOTE_FUNDS, TOKEN, TOKEN_2022,
};
use bordrless_swap::client as swap;
use bordrless_swap::error::SwapError;
use bordrless_swap::events::{PoolCreated, ProtocolFeesCollected, SplPoolCreated, Swapped};
use bordrless_swap::instructions::{AddLiquidityArgs, CreatePoolArgs, SwapArgs};
use bordrless_swap::spl;
use bordrless_swap::state::Pool;
use bordrless_token::client as token;
use hook_tester::callback;
use hook_tester::client as tester;
use solana_keypair::Keypair;
use solana_signer::Signer;

const SUPPLY: u64 = 1_000_000_000_000_000;
const DEPOSIT: u64 = 100_000_000_000_000;
const VQ: u64 = 5_000_000_000;
const LP_FEE: u16 = 100;
const SWAP_FLAGS: u16 = pool_flags::BEFORE_SWAP
    | pool_flags::AFTER_SWAP
    | pool_flags::BEFORE_SWAP_RETURNS_DELTA
    | pool_flags::AFTER_SWAP_RETURNS_DELTA;
/// The first registry extra of a hook_tester callback (after the prefix and the script).
const X: u8 = 6;

fn code(e: SwapError) -> u32 {
    u32::from(e)
}

fn hook_authority() -> Pubkey {
    tester::hook_authority().0
}

/// A curve's arguments with `hook_tester` as its hook.
fn curve_args(flags: u16, virtual_quote: u64) -> CreatePoolArgs {
    CreatePoolArgs {
        lp_fee_bps: LP_FEE,
        hook_program: hook_tester::ID,
        hook_flags: flags,
        virtual_base: 0,
        virtual_quote,
        base_amount: DEPOSIT,
        quote_amount: 0,
        base_hook_accounts: 0,
        quote_hook_accounts: 0,
        hook_data: vec![],
    }
}

/// A pool quoted in an SPL token, its hook `hook_tester`, and who trades it.
struct Setup {
    w: World,
    lp: Keypair,
    base: Pubkey,
    quote: Pubkey,
    program: Pubkey,
    pool: Pubkey,
    /// A third party's token account of the quote: registry extra `X`, a cut's destination.
    third: Pubkey,
}

impl Setup {
    /// `create_spl_pool` by `hook_tester` as its own hook, `lp` depositing.
    fn create_ix(&self, quote: &Pubkey, program: &Pubkey, args: CreatePoolArgs) -> Instruction {
        tester::as_hook(swap::create_spl_pool(
            &swap::CreateSplPoolKeys {
                payer: self.lp.pubkey(),
                authority: self.lp.pubkey(),
                treasury: self.w.env.treasury.pubkey(),
                base_mint: self.base,
                quote_mint: *quote,
                quote_program: *program,
                hook_caller: hook_authority(),
            },
            args,
            vec![],
        ))
    }

    fn send(&mut self, ixs: &[Instruction]) -> Tx {
        let lp = self.lp.insecure_clone();
        self.w.env.send_paid_by(ixs, &lp, &[])
    }

    fn pool(&self) -> Pool {
        self.w.env.read(&self.pool)
    }

    /// A trader holding `amount` of the quote and a holding of the base.
    fn trader(&mut self, amount: u64) -> Keypair {
        let t = self.w.env.funded(10_000_000_000);
        self.w
            .env
            .token_account(&t.pubkey(), &self.quote, &self.program, amount);
        self.w
            .env
            .send_paid_by(
                &[token::create_holding(t.pubkey(), self.base, t.pubkey())],
                &t,
                &[],
            )
            .ok();
        t
    }

    fn quote_account(&self, owner: &Pubkey) -> Pubkey {
        spl::associated_token_address(owner, &self.quote, &self.program)
    }

    /// `swap_spl` of `trader`, delivered to itself.
    fn swap_ix(&self, trader: &Pubkey, direction: u8, amount_in: u64, min_out: u64) -> Instruction {
        let extras = self.w.env.pool_hook_extras(
            &hook_tester::ID,
            &self.pool,
            &self.base,
            &self.quote,
            trader,
        );
        swap::swap_spl(
            &swap::SwapSplKeys {
                trader: *trader,
                pool: self.pool,
                base_mint: self.base,
                quote_mint: self.quote,
                quote_program: self.program,
                trader_base: token::holding_address(&self.base, trader),
                trader_quote: self.quote_account(trader),
                hook_program: Some(hook_tester::ID),
                base_mint_writable: false,
            },
            SwapArgs {
                direction,
                amount_in,
                min_amount_out: min_out,
                in_hook_accounts: 0,
                out_hook_accounts: 0,
                hook_data: vec![],
            },
            extras,
        )
    }

    /// Scripts the pool hook's `which` callback to answer `answer`.
    fn script(&mut self, which: u8, answer: &HookReturn) {
        let ix = tester::answer(self.lp.pubkey(), self.pool, which, answer);
        self.send(&[ix]).ok();
    }
}

/// A world with a base mint held by `lp`, a quote mint of `program` (`extensions` as given; its
/// key passed to `extensions`), and a kind-2 pool between them whose hook is `hook_tester` with
/// `flags`, its registry naming a third party's token account of the quote.
fn setup(program: Pubkey, extensions: impl Fn(Pubkey) -> Vec<ext::Ext>, flags: u16) -> Setup {
    let mut w = World::new();
    let lp = w.env.funded(100_000_000_000);
    let base = w.mint_to_owner(&lp, 6, SUPPLY, "BASE");
    let quote = Pubkey::new_unique();
    let issuer = w.env.deployer.pubkey();
    w.env.put_mint(
        quote,
        program,
        pack_mint(
            Some(issuer),
            Some(issuer),
            6,
            u64::MAX / 2,
            &extensions(quote),
        ),
    );
    let pool = swap::pool_address(&base, &quote, LP_FEE, Some(hook_tester::ID));
    let third = w
        .env
        .token_account(&Pubkey::new_unique(), &quote, &program, 0);
    let mut s = Setup {
        w,
        lp,
        base,
        quote,
        program,
        pool,
        third,
    };
    let script = tester::init_script(
        s.lp.pubkey(),
        pool,
        vec![ExtraAccount {
            writable: true,
            source: AccountSource::Key(third),
        }],
    );
    s.send(&[script]).ok();
    let ix = s.create_ix(&quote, &program, curve_args(flags, VQ));
    let tx = s.send(&[ix]);
    tx.ok();
    let created: PoolCreated = tx.event();
    let spl_created: SplPoolCreated = tx.event();
    let vault = swap::spl_vault_address(&pool, &quote, &program);
    assert_eq!(
        (
            created.pool,
            created.lp_mint,
            created.quote_vault,
            created.curve
        ),
        (pool, Pubkey::default(), vault, true)
    );
    assert_eq!(
        (
            created.base_reserve,
            created.quote_reserve,
            created.lp_supply
        ),
        (DEPOSIT, 0, 0)
    );
    assert_eq!(
        (
            spl_created.pool,
            spl_created.quote_mint,
            spl_created.quote_program,
            spl_created.quote_decimals,
            spl_created.quote_vault
        ),
        (pool, quote, program, 6, vault)
    );
    s.w.env.assert_spl_invariant(&pool);
    s
}

/// A plain mint of either program.
fn plain(_: Pubkey) -> Vec<ext::Ext> {
    vec![]
}

/// A mint's extensions, given its key.
type Extensions = fn(Pubkey) -> Vec<ext::Ext>;

/// The two quote programs, each with its example mint: a plain SPL Token mint and an xStocks-like
/// Token-2022 mint (a permanent delegate, pausable, a hook with no program, scaled amounts,
/// metadata, a freeze authority).
fn both() -> Vec<(Pubkey, Extensions)> {
    fn xstock(mint: Pubkey) -> Vec<ext::Ext> {
        xstock_extensions(mint, Pubkey::new_from_array([7; 32]))
    }
    vec![(TOKEN, plain), (TOKEN_2022, xstock)]
}

#[test]
fn a_kind_2_pool_is_created_as_its_hooks_curve() {
    for (program, exts) in both() {
        let s = setup(program, exts, SWAP_FLAGS);
        let p = s.pool();
        assert!(p.quote_spl() && !p.quote_native());
        assert_eq!(p.reserved[0], 2);
        assert_eq!(p.reserved[17], u8::from(program == TOKEN_2022));
        assert_eq!((p.spl_program(), p.spl_decimals()), (program, 6));
        assert_eq!(p.reserved[19..], [0u8; 41]);
        assert_eq!(
            p.quote_vault,
            spl::associated_token_address(&s.pool, &s.quote, &program)
        );
        assert_eq!(
            (p.lp_mint, p.lp_supply, p.curve),
            (Pubkey::default(), 0, true)
        );
        assert_eq!((p.virtual_base, p.virtual_quote), (0, VQ));
        assert_eq!((p.base_reserve, p.quote_reserve), (DEPOSIT, 0));
        assert_eq!(p.hook_program, Some(hook_tester::ID));
        // Another hook's curve pays the flat rate (only the launchpad's gets the share model).
        assert_eq!(
            (p.fee_model, p.protocol_fee_bps, p.protocol_share_bps),
            (0, policy::PROTOCOL_FEE_BPS, 0)
        );
        // The vault is a token account of the quote owned by the pool, empty.
        let vault = s.w.env.account(&p.quote_vault).unwrap();
        let key = |at: usize| Pubkey::new_from_array(vault.data[at..at + 32].try_into().unwrap());
        assert_eq!((key(0), key(32)), (s.quote, s.pool));
        assert_eq!(s.w.env.token_amount(&p.quote_vault), 0);
        assert_eq!(vault.owner, program, "the vault is the quote program's");
        assert_eq!(s.w.env.holding(&s.base, &s.pool), DEPOSIT);
    }
}

/// What a swap on `p` gives, under the flat model with no hook answer.
fn flat(p: &Pool, buy: bool, received: u64) -> bordrless_core::SwapAmounts {
    swap_amounts(
        buy,
        received,
        p.lp_fee_bps,
        p.protocol_fee_bps,
        &Reserves {
            base_reserve: p.base_reserve,
            quote_reserve: p.quote_reserve,
            virtual_base: p.virtual_base,
            virtual_quote: p.virtual_quote,
        },
    )
    .expect("a swap")
}

#[test]
fn buys_and_sells_move_the_real_token_with_quote_side_cuts() {
    for (program, exts) in both() {
        let mut s = setup(program, exts, SWAP_FLAGS);
        let t = s.trader(1_000_000_000_000);
        let tq = s.quote_account(&t.pubkey());
        // A buy: the hook takes 7,000 of the input to a third party's token account.
        let cut_in = 7_000;
        s.script(
            callback::BEFORE_SWAP,
            &HookReturn {
                deltas: vec![Delta {
                    amount: cut_in,
                    account: X,
                }],
                ..HookReturn::default()
            },
        );
        let amount_in = 1_000_000_000;
        let before = s.pool();
        let expect = flat(&before, true, amount_in - cut_in);
        let ix = s.swap_ix(&t.pubkey(), 1, amount_in, 1);
        let tx = s.w.env.send_paid_by(&[ix], &t, &[]);
        let ev: Swapped = tx.event();
        assert_eq!(
            (ev.direction, ev.amount_in, ev.received_in, ev.cuts_in),
            (1, amount_in, amount_in - cut_in, cut_in)
        );
        assert_eq!(ev.deltas_in.len(), 1);
        assert_eq!(
            (ev.deltas_in[0].holding, ev.deltas_in[0].amount),
            (s.third, cut_in)
        );
        assert_eq!(
            (ev.amount_out, ev.delivered_out, ev.protocol_fee, ev.lp_fee),
            (
                expect.out_gross,
                expect.amount_out,
                expect.protocol_fee,
                expect.lp_fee
            )
        );
        assert_eq!(ev.recipient, t.pubkey());
        assert_eq!(s.w.env.token_amount(&tq), 1_000_000_000_000 - amount_in);
        assert_eq!(s.w.env.token_amount(&s.third), cut_in);
        assert_eq!(s.w.env.holding(&s.base, &t.pubkey()), expect.amount_out);
        let p = s.pool();
        assert_eq!(p.quote_reserve, expect.to_reserve_in);
        assert_eq!(p.protocol_fees_quote, expect.protocol_fee);
        assert_eq!(p.base_reserve, DEPOSIT - expect.out_gross);
        s.w.env.assert_spl_invariant(&s.pool);
        // A sell of half: the hook takes 3,000 of the quote output to the third party.
        s.script(callback::BEFORE_SWAP, &HookReturn::default());
        let cut_out = 3_000;
        s.script(
            callback::AFTER_SWAP,
            &HookReturn {
                deltas: vec![Delta {
                    amount: cut_out,
                    account: X,
                }],
                ..HookReturn::default()
            },
        );
        let sell = expect.amount_out / 2;
        let before = s.pool();
        let expect = flat(&before, false, sell);
        let quote_before = s.w.env.token_amount(&tq);
        let ix = s.swap_ix(&t.pubkey(), 0, sell, 1);
        let tx = s.w.env.send_paid_by(&[ix], &t, &[]);
        let ev: Swapped = tx.event();
        assert_eq!(
            (ev.direction, ev.received_in, ev.cuts_out),
            (0, sell, cut_out)
        );
        assert_eq!(
            (ev.deltas_out[0].holding, ev.deltas_out[0].amount),
            (s.third, cut_out)
        );
        assert_eq!(ev.delivered_out, expect.amount_out - cut_out);
        assert_eq!(
            s.w.env.token_amount(&tq),
            quote_before + expect.amount_out - cut_out
        );
        assert_eq!(s.w.env.token_amount(&s.third), cut_in + cut_out);
        let p = s.pool();
        assert_eq!(p.quote_reserve, before.quote_reserve - expect.out_gross);
        assert_eq!(
            p.protocol_fees_quote,
            before.protocol_fees_quote + expect.protocol_fee
        );
        s.w.env.assert_spl_invariant(&s.pool);
        // Selling back what the pool never sold (the quote it would owe is not in it) is refused:
        // a leg's price never falls below where it opened.
        let more = s.w.env.holding(&s.base, &t.pubkey());
        s.w.env
            .send_paid_by(
                &[token::transfer(
                    s.lp.pubkey(),
                    token::holding_address(&s.base, &s.lp.pubkey()),
                    token::holding_address(&s.base, &t.pubkey()),
                    s.base,
                    None,
                    vec![],
                    DEPOSIT,
                )],
                &s.lp.insecure_clone(),
                &[],
            )
            .ok();
        let ix = s.swap_ix(&t.pubkey(), 0, more + DEPOSIT, 1);
        s.w.env
            .send_paid_by(&[ix], &t, &[])
            .expect_code(code(SwapError::InsufficientLiquidity));
        s.w.env.assert_spl_invariant(&s.pool);
        // The protocol fees go to the fee collector's token account of the quote, made by the
        // sender (anyone) when it does not exist.
        let fees = s.pool().protocol_fees_quote;
        assert!(fees > 0);
        let collector = s.w.env.deployer.pubkey();
        let collector_ata = spl::associated_token_address(&collector, &s.quote, &program);
        assert!(s.w.env.account(&collector_ata).is_none());
        let cranker = s.w.env.funded(1_000_000_000);
        let ix =
            swap::collect_protocol_fees_spl(cranker.pubkey(), s.pool, s.quote, program, collector);
        let tx =
            s.w.env
                .send_paid_by(std::slice::from_ref(&ix), &cranker, &[]);
        let ev: ProtocolFeesCollected = tx.event();
        assert_eq!(
            (ev.pool, ev.quote_amount, ev.collector),
            (s.pool, fees, collector)
        );
        assert_eq!(s.w.env.token_amount(&collector_ata), fees);
        assert_eq!(s.pool().protocol_fees_quote, 0);
        s.w.env.assert_spl_invariant(&s.pool);
        // Again, the account existing and nothing accrued: a no-op that lands.
        s.w.env.warp(1);
        let tx = s.w.env.send_paid_by(&[ix], &cranker, &[]);
        assert_eq!(tx.event::<ProtocolFeesCollected>().quote_amount, 0);
        assert_eq!(s.w.env.token_amount(&collector_ata), fees);
        s.w.env.assert_spl_invariant(&s.pool);
    }
}

#[test]
fn the_extension_policy_refuses_and_allows_as_section_2_3_says() {
    let mut s = setup(TOKEN, plain, 0);
    let issuer = s.w.env.deployer.pubkey();
    let other = Pubkey::new_unique();
    let refused: Vec<(Vec<ext::Ext>, SwapError)> = vec![
        (vec![ext::transfer_fee(50)], SwapError::QuoteMintTransferFee),
        (
            vec![ext::confidential_fee()],
            SwapError::QuoteMintTransferFee,
        ),
        (
            vec![ext::non_transferable()],
            SwapError::QuoteMintNonTransferable,
        ),
        (
            vec![ext::default_state(2)],
            SwapError::QuoteMintFrozenByDefault,
        ),
        (
            vec![ext::transfer_hook(Some(issuer), Some(other))],
            SwapError::QuoteMintTransferHook,
        ),
        (
            vec![ext::pausable(issuer, true)],
            SwapError::QuoteMintPaused,
        ),
        (
            vec![ext::confidential_mint_burn()],
            SwapError::QuoteMintConfidential,
        ),
        // Account extensions on a mint, one newer than the policy (PermissionedBurn, 28), any.
        (vec![ext::raw(7, 0)], SwapError::QuoteMintUnknownExtension),
        (vec![ext::raw(28, 32)], SwapError::QuoteMintUnknownExtension),
        (vec![ext::raw(999, 4)], SwapError::QuoteMintUnknownExtension),
        // A refused one among allowed ones.
        (
            vec![ext::permanent_delegate(issuer), ext::transfer_fee(1)],
            SwapError::QuoteMintTransferFee,
        ),
    ];
    for (exts, why) in refused {
        let quote = s.w.env.new_quote_mint(TOKEN_2022, 6, &exts);
        let ix = s.create_ix(&quote, &TOKEN_2022, small());
        s.send(&[ix]).expect_code(code(why));
    }
    let allowed: Vec<Vec<ext::Ext>> = vec![
        vec![],
        vec![ext::mint_close(issuer)],
        vec![ext::confidential_mint()],
        vec![ext::default_state(1)],
        vec![ext::interest_bearing(issuer)],
        vec![ext::permanent_delegate(issuer)],
        vec![ext::transfer_hook(Some(issuer), None)],
        vec![ext::transfer_hook(None, None)],
        vec![ext::pausable(issuer, false)],
        vec![ext::scaled_ui_amount(issuer)],
        vec![
            ext::metadata_pointer(issuer, other),
            ext::token_metadata(issuer, other, "Name", "SYM"),
        ],
        vec![
            ext::group_pointer(issuer, other),
            ext::token_group(issuer, other),
        ],
        vec![
            ext::group_member_pointer(issuer, other),
            ext::token_group_member(other, other),
        ],
    ];
    for exts in allowed {
        let quote = s.w.env.new_quote_mint(TOKEN_2022, 9, &exts);
        let ix = s.create_ix(&quote, &TOKEN_2022, small());
        s.send(&[ix]).ok();
        let pool = swap::pool_address(&s.base, &quote, LP_FEE, Some(hook_tester::ID));
        assert_eq!(s.w.env.read::<Pool>(&pool).spl_decimals(), 9);
        s.w.env.assert_spl_invariant(&pool);
    }
    // The xStocks-like mint, with a freeze authority too.
    let quote = Pubkey::new_unique();
    s.w.env.put_mint(
        quote,
        TOKEN_2022,
        pack_mint(
            Some(issuer),
            Some(issuer),
            8,
            1,
            &xstock_extensions(quote, issuer),
        ),
    );
    let ix = s.create_ix(&quote, &TOKEN_2022, small());
    s.send(&[ix]).ok();
    // Not a mint: uninitialized; a token account; a Bordrless mint; an SPL Token mint longer than
    // a plain mint.
    let mut uninit = pack_mint(None, None, 6, 0, &[]);
    uninit[45] = 0;
    let quote = Pubkey::new_unique();
    s.w.env.put_mint(quote, TOKEN, uninit);
    let ix = s.create_ix(&quote, &TOKEN, small());
    s.send(&[ix]).expect_code(code(SwapError::QuoteMintInvalid));
    let not_mint = s.third;
    let ix = s.create_ix(&not_mint, &TOKEN, small());
    s.send(&[ix]).expect_code(code(SwapError::QuoteMintInvalid));
    let quote = Pubkey::new_unique();
    s.w.env.put_mint(
        quote,
        TOKEN,
        pack_mint(None, None, 6, 0, &[ext::permanent_delegate(issuer)]),
    );
    let ix = s.create_ix(&quote, &TOKEN, small());
    s.send(&[ix]).expect_code(code(SwapError::QuoteMintInvalid));
    let bordrless = s.w.sol;
    let ix = s.create_ix(&bordrless, &bordrless_token::ID, small());
    s.send(&[ix]).expect_code(code(SwapError::QuoteMintInvalid));
    // The quote program passed must be the mint's.
    let quote = s.w.env.new_quote_mint(TOKEN, 6, &[]);
    let ix = s.create_ix(&quote, &TOKEN_2022, small());
    s.send(&[ix])
        .expect_code(code(SwapError::WrongTokenProgram));
}

#[test]
fn create_spl_pool_is_a_curve_its_hook_creates() {
    let mut s = setup(TOKEN, plain, 0);
    let quote = s.w.env.new_quote_mint(TOKEN, 6, &[]);
    // No virtual quote, a quote deposit, quote hook accounts: refused.
    for args in [
        curve_args(0, 0),
        CreatePoolArgs {
            quote_amount: 1,
            ..curve_args(0, VQ)
        },
        CreatePoolArgs {
            quote_hook_accounts: 1,
            ..curve_args(0, VQ)
        },
    ] {
        let ix = s.create_ix(&quote, &TOKEN, args);
        s.send(&[ix]).expect_code(code(SwapError::SplPoolCurveOnly));
    }
    // The base slice's count must be exact.
    let mut ix = s.create_ix(&quote, &TOKEN, curve_args(0, VQ));
    ix.accounts
        .push(AccountMeta::new_readonly(Pubkey::new_unique(), false));
    s.send(&[ix]).expect_code(code(SwapError::AccountCounts));
    // Signed by anyone but the hook's authority: refused.
    let fake = Keypair::new();
    let ix = swap::create_spl_pool(
        &swap::CreateSplPoolKeys {
            payer: s.lp.pubkey(),
            authority: s.lp.pubkey(),
            treasury: s.w.env.treasury.pubkey(),
            base_mint: s.base,
            quote_mint: quote,
            quote_program: TOKEN,
            hook_caller: fake.pubkey(),
        },
        curve_args(0, VQ),
        vec![],
    );
    let lp = s.lp.insecure_clone();
    s.w.env
        .send_paid_by(&[ix], &lp, &[&fake])
        .expect_code(code(SwapError::BadHookCaller));
    // `hook_tester` naming another program (the launchpad) as the hook: its authority is not
    // that program's.
    let ix = s.create_ix(
        &quote,
        &TOKEN,
        CreatePoolArgs {
            hook_program: bordrless_launch::ID,
            ..curve_args(0, VQ)
        },
    );
    s.send(&[ix]).expect_code(code(SwapError::BadHookCaller));
    // Paused.
    let pause = s.w.dex_pause_ix(true);
    s.w.as_admin(pause).ok();
    let ix = s.create_ix(&quote, &TOKEN, curve_args(0, VQ));
    s.send(&[ix]).expect_code(code(SwapError::Paused));
    let pause = s.w.dex_pause_ix(false);
    s.w.as_admin(pause).ok();
    // Someone created the pool's token account of the quote first (the ATA is anyone's to make,
    // and sent it tokens): the pool is still created, the donation outside its books.
    let pool = swap::pool_address(&s.base, &quote, LP_FEE, Some(hook_tester::ID));
    let vault = s.w.env.token_account(&pool, &quote, &TOKEN, 777);
    let ix = s.create_ix(&quote, &TOKEN, curve_args(0, VQ));
    s.send(&[ix]).ok();
    let p: Pool = s.w.env.read(&pool);
    assert_eq!((p.quote_vault, p.quote_reserve), (vault, 0));
    assert_eq!(s.w.env.token_amount(&vault), 777);
}

#[test]
fn a_kind_2_pool_refuses_every_other_path() {
    let mut s = setup(TOKEN, plain, SWAP_FLAGS);
    let t = s.trader(1_000_000_000_000);
    let ix = s.swap_ix(&t.pubkey(), 1, 1_000_000, 1);
    s.w.env.send_paid_by(&[ix], &t, &[]).ok();
    let p = s.pool();
    // The classic swap (its quote vault set to the pool's): `SplPool`.
    let extras =
        s.w.env
            .pool_hook_extras(&hook_tester::ID, &s.pool, &s.base, &s.quote, &t.pubkey());
    let mut ix = swap::swap(
        &swap::SwapKeys {
            trader: t.pubkey(),
            pool: s.pool,
            base_mint: s.base,
            quote_mint: s.quote,
            trader_base: token::holding_address(&s.base, &t.pubkey()),
            trader_quote: s.quote_account(&t.pubkey()),
            hook_program: Some(hook_tester::ID),
            base_mint_writable: false,
            quote_mint_writable: false,
        },
        SwapArgs {
            direction: 1,
            amount_in: 1_000,
            min_amount_out: 0,
            in_hook_accounts: 0,
            out_hook_accounts: 0,
            hook_data: vec![],
        },
        extras.clone(),
    );
    ix.accounts[6].pubkey = p.quote_vault;
    s.w.env
        .send_paid_by(&[ix], &t, &[])
        .expect_code(code(SwapError::SplPool));
    // `swap_v2`: not a native pool.
    let ix = swap::swap_v2(
        &swap::SwapV2Keys {
            trader: t.pubkey(),
            pool: s.pool,
            base_mint: s.base,
            trader_base: token::holding_address(&s.base, &t.pubkey()),
            trader_quote: t.pubkey(),
            hook_program: Some(hook_tester::ID),
            base_mint_writable: false,
            with_bridge: false,
        },
        SwapArgs {
            direction: 1,
            amount_in: 1_000,
            min_amount_out: 0,
            in_hook_accounts: 0,
            out_hook_accounts: 0,
            hook_data: vec![],
        },
        extras,
    );
    s.w.env
        .send_paid_by(&[ix], &t, &[])
        .expect_code(code(SwapError::NotNativePool));
    // `finalize_curve`, by the pool's own hook (its vault and LP mint as the pool names them).
    let (authority, bump) = tester::hook_authority();
    let mut fin = swap::finalize_curve(
        authority,
        bump,
        s.pool,
        s.base,
        s.quote,
        Pubkey::new_unique(),
    );
    fin.accounts[3].pubkey = p.quote_vault;
    fin.accounts[4].pubkey = p.lp_mint;
    s.send(&[tester::as_hook(fin)])
        .expect_code(code(SwapError::SplPool));
    // Liquidity in or out.
    let keys = swap::LiquidityKeys {
        provider: s.lp.pubkey(),
        pool: s.pool,
        base_mint: s.base,
        quote_mint: s.quote,
        hook_program: Some(hook_tester::ID),
    };
    let mut add = swap::add_liquidity(
        &keys,
        AddLiquidityArgs {
            base_desired: 1_000,
            quote_desired: 1_000,
            min_lp: 0,
            base_hook_accounts: 0,
            quote_hook_accounts: 0,
            hook_data: vec![],
        },
        vec![],
    );
    add.accounts[5].pubkey = p.lp_mint;
    add.accounts[7].pubkey = p.quote_vault;
    s.send(&[add]).expect_code(code(SwapError::SplPool));
    // The admin's collect: its token-standard transfer can't move the quote.
    let admin = s.w.admin();
    let mut collect =
        swap::collect_protocol_fees(admin.pubkey(), s.pool, s.quote, admin.pubkey(), vec![]);
    collect.accounts[4].pubkey = p.quote_vault;
    s.w.as_admin(collect).expect_code(code(SwapError::SplPool));
    // The SOL family: not bridged SOL.
    let cranker = s.w.env.funded(1_000_000_000);
    s.w.env
        .send_paid_by(
            &[swap::collect_protocol_fees_sol(
                cranker.pubkey(),
                s.pool,
                admin.pubkey(),
            )],
            &cranker,
            &[],
        )
        .expect_code(code(SwapError::NotBridgedSol));
    let pause = s.w.dex_pause_ix(true);
    s.w.as_admin(pause).ok();
    s.w.as_admin(swap::migrate_pool_to_native(admin.pubkey(), s.pool, 0))
        .expect_code(code(SwapError::NotBridgedSol));
    let pause = s.w.dex_pause_ix(false);
    s.w.as_admin(pause).ok();
    // And `swap_spl` on a pool that is not kind 2.
    let other =
        s.w.mint_to_owner(&s.lp.insecure_clone(), 6, SUPPLY, "OTHER");
    let (bridged_pool, _) = s.w.new_pool(
        &s.lp.insecure_clone(),
        &bordrless_program_tests::hooks::NewPool {
            base: s.base,
            quote: other,
            lp_fee_bps: LP_FEE,
            base_amount: 1_000_000_000,
            quote_amount: 1_000_000_000,
            tester_flags: None,
            extras: vec![],
        },
    );
    let mut ix = s.swap_ix(&t.pubkey(), 1, 1_000, 0);
    ix.accounts[2].pubkey = bridged_pool;
    s.w.env
        .send_paid_by(&[ix], &t, &[])
        .expect_code(code(SwapError::NotSplPool));
    s.w.env.assert_spl_invariant(&s.pool);
}

#[test]
fn swap_spl_refuses_bad_accounts_and_cuts() {
    let mut s = setup(TOKEN, plain, SWAP_FLAGS);
    let t = s.trader(1_000_000_000_000);
    // The quote program must be the pool's.
    let mut ix = s.swap_ix(&t.pubkey(), 1, 1_000_000, 0);
    ix.accounts[13].pubkey = TOKEN_2022;
    s.w.env
        .send_paid_by(&[ix], &t, &[])
        .expect_code(code(SwapError::WrongTokenProgram));
    // The trader's quote account must be of the quote mint.
    let wrong_mint = s.w.env.new_quote_mint(TOKEN, 6, &[]);
    let wrong =
        s.w.env
            .token_account(&t.pubkey(), &wrong_mint, &TOKEN, 1_000_000);
    let mut ix = s.swap_ix(&t.pubkey(), 1, 1_000, 0);
    ix.accounts[8].pubkey = wrong;
    s.w.env
        .send_paid_by(&[ix], &t, &[])
        .expect_code(code(SwapError::WrongTokenAccount));
    // No quote-side token-hook slice.
    let mut ix = s.swap_ix(&t.pubkey(), 1, 1_000, 0);
    let args = SwapArgs {
        direction: 1,
        amount_in: 1_000,
        min_amount_out: 0,
        in_hook_accounts: 1,
        out_hook_accounts: 0,
        hook_data: vec![],
    };
    ix.data = bordrless_swap::instruction::SwapSpl { args }.data_vec();
    s.w.env
        .send_paid_by(&[ix], &t, &[])
        .expect_code(code(SwapError::AccountCounts));
    // Slippage.
    let ix = s.swap_ix(&t.pubkey(), 1, 1_000_000, u64::MAX);
    s.w.env
        .send_paid_by(&[ix], &t, &[])
        .expect_code(code(SwapError::Slippage));
    // A quote-side cut to the script (not a token account), to the pool's own vault, a burn of
    // the quote: refused before anything moves.
    let vault = s.pool().quote_vault;
    let bad: Vec<(HookReturn, SwapError)> = vec![
        (
            HookReturn {
                deltas: vec![Delta {
                    amount: 1,
                    account: X - 1,
                }],
                ..HookReturn::default()
            },
            SwapError::InvalidDeltaAccount,
        ),
        (
            HookReturn {
                burn: 5,
                ..HookReturn::default()
            },
            SwapError::SplBurn,
        ),
        (
            HookReturn {
                deltas: vec![Delta {
                    amount: 2_000_000,
                    account: X,
                }],
                ..HookReturn::default()
            },
            SwapError::DeltaTooLarge,
        ),
    ];
    for (answer, why) in bad {
        s.script(callback::BEFORE_SWAP, &answer);
        let ix = s.swap_ix(&t.pubkey(), 1, 1_000_000, 0);
        s.w.env.send_paid_by(&[ix], &t, &[]).expect_code(code(why));
    }
    // A cut to a token account of another mint, and to the vault itself, named as an extra.
    for target in [wrong, vault] {
        s.script(
            callback::BEFORE_SWAP,
            &HookReturn {
                deltas: vec![Delta {
                    amount: 1,
                    account: X + 1,
                }],
                ..HookReturn::default()
            },
        );
        let mut ix = s.swap_ix(&t.pubkey(), 1, 1_000_000, 0);
        ix.accounts.push(AccountMeta::new(target, false));
        s.w.env
            .send_paid_by(&[ix], &t, &[])
            .expect_code(code(SwapError::InvalidDeltaAccount));
    }
    // A delta to a read-only token account of the quote.
    s.script(
        callback::BEFORE_SWAP,
        &HookReturn {
            deltas: vec![Delta {
                amount: 1,
                account: X,
            }],
            ..HookReturn::default()
        },
    );
    let mut ix = s.swap_ix(&t.pubkey(), 1, 1_000_000, 0);
    let n = ix.accounts.len();
    ix.accounts[n - 1].is_writable = false;
    s.w.env
        .send_paid_by(&[ix], &t, &[])
        .expect_code(code(SwapError::InvalidDeltaAccount));
    s.w.env.assert_spl_invariant(&s.pool);
}

#[test]
fn a_donation_to_the_vault_stays_outside_the_books() {
    for (program, exts) in both() {
        let mut s = setup(program, exts, 0);
        let t = s.trader(1_000_000_000_000);
        let vault = s.pool().quote_vault;
        s.w.env.set_token_amount(&vault, 5_000);
        let ix = s.swap_ix(&t.pubkey(), 1, 10_000_000, 0);
        s.w.env.send_paid_by(&[ix], &t, &[]).ok();
        let p = s.pool();
        assert_eq!(
            s.w.env.token_amount(&vault),
            p.quote_reserve + p.protocol_fees_quote + 5_000
        );
        let all = s.w.env.holding(&s.base, &t.pubkey());
        let ix = s.swap_ix(&t.pubkey(), 0, all, 0);
        s.w.env.send_paid_by(&[ix], &t, &[]).ok();
        let p = s.pool();
        assert_eq!(
            s.w.env.token_amount(&vault),
            p.quote_reserve + p.protocol_fees_quote + 5_000
        );
        // A vault drained below its books (the issuer's permanent delegate could): every swap is
        // refused until it is made whole.
        s.w.env.set_token_amount(&vault, 0);
        let ix = s.swap_ix(&t.pubkey(), 1, 10_000_000, 0);
        s.w.env
            .send_paid_by(&[ix], &t, &[])
            .expect_code(code(SwapError::SplInvariant));
    }
}

#[test]
fn a_pool_in_the_launchpads_name_is_the_launchpads_to_create() {
    let mut w = World::new();
    let lp = w.env.funded(100_000_000_000);
    let a = w.mint_to_owner(&lp, 6, SUPPLY, "AAA");
    let b = w.mint_to_owner(&lp, 6, SUPPLY, "BBB");
    let keys = swap::CreatePoolKeys {
        payer: lp.pubkey(),
        authority: lp.pubkey(),
        treasury: w.env.treasury.pubkey(),
        base_mint: a,
        quote_mint: b,
        hook_caller: None,
    };
    let args = CreatePoolArgs {
        lp_fee_bps: policy::LP_FEE_BPS,
        hook_program: bordrless_launch::ID,
        hook_flags: 0,
        virtual_base: 0,
        virtual_quote: 0,
        base_amount: 1_000_000,
        quote_amount: 1_000_000,
        base_hook_accounts: 0,
        quote_hook_accounts: 0,
        hook_data: vec![],
    };
    // An ordinary pool naming the launchpad, without asking it (no `before_initialize`): it went
    // through before, taking an address the launchpad may need (a leg's, a reserve's).
    w.env
        .send_paid_by(&[swap::create_pool(&keys, args.clone(), vec![])], &lp, &[])
        .expect_code(code(SwapError::LaunchpadPoolNeedsLaunchpad));
    // Asking it: the launchpad still refuses it itself.
    let asked = CreatePoolArgs {
        hook_flags: pool_flags::BEFORE_INITIALIZE,
        ..args.clone()
    };
    w.env
        .send_paid_by(&[swap::create_pool(&keys, asked, vec![])], &lp, &[])
        .expect_code(u32::from(
            bordrless_launch::error::LaunchError::OnlyLaunchpadCreatesPools,
        ));
    // Any other hook, or none, as before.
    let none = CreatePoolArgs {
        hook_program: Pubkey::default(),
        ..args
    };
    w.env
        .send_paid_by(&[swap::create_pool(&keys, none, vec![])], &lp, &[])
        .ok();
}

trait DataVec {
    fn data_vec(self) -> Vec<u8>;
}

impl<T: anchor_lang::InstructionData> DataVec for T {
    fn data_vec(self) -> Vec<u8> {
        self.data()
    }
}

/// A curve with a small deposit, for the suites that open many pools.
fn small() -> CreatePoolArgs {
    CreatePoolArgs {
        base_amount: 1_000_000,
        ..curve_args(0, VQ)
    }
}

/// A token account of `quote` owned by `owner` that is not its associated token account: an ATA of
/// a stranger (so Token-2022 sized it for the mint) copied to a fresh key with `owner` written in.
fn token_account_of(w: &mut World, owner: &Pubkey, quote: &Pubkey, program: &Pubkey) -> Pubkey {
    let ata = w
        .env
        .token_account(&Pubkey::new_unique(), quote, program, 0);
    let mut account = w.env.account(&ata).expect("the stranger's account");
    account.data[32..64].copy_from_slice(owner.as_ref());
    let key = Pubkey::new_unique();
    w.env.put(key, account);
    key
}

/// A sell never delivers into the pool's own quote vault, nor into any other token account the
/// pool owns (the independent review of 19 Oct, its reproduced finding): the token programs take a
/// transfer to the same account as a no-op, so the sell would land with nothing delivered, the
/// seller's quote stranded in the vault outside the books, and the share model would count the
/// whole undelivered output as a hook's cut, moving Bordrless's share of it out of
/// `quote_reserve`. Refused before anything moves (`WrongTokenAccount`); the seller's own account
/// takes the same sell.
#[test]
fn a_sell_cannot_deliver_into_the_pools_own_vault() {
    for (program, exts) in both() {
        let mut w = World::new();
        let creator = w.env.funded(10_000_000_000);
        let issuer = w.env.deployer.pubkey();
        let quote = Pubkey::new_unique();
        w.env.put_mint(
            quote,
            program,
            pack_mint(Some(issuer), Some(issuer), 6, u64::MAX / 2, &exts(quote)),
        );
        let (mint, tx) = w.create_token_launch(&creator, 100, &[(quote, VQ)]);
        tx.ok();
        w.open_all_legs(&mint);
        let k = w.leg_keys(&mint, 0);
        let b = w.quote_wallet(&quote, &program, QUOTE_FUNDS);
        w.leg_buy(&b, &mint, 0, 1_000_000_000).ok();
        let coins = w.env.holding(&mint, &b.pubkey());
        let half = coins / 2;
        let before: Pool = w.env.read(&k.pool());
        let vault_before = w.env.token_amount(&k.quote_vault());
        let sell_to = |to: Pubkey| {
            launch::leg_swap_with(
                &k,
                b.pubkey(),
                token::holding_address(&mint, &b.pubkey()),
                to,
                0,
                half,
                0,
            )
        };
        // Into the vault itself, and into another token account of the quote the pool owns.
        let pool_owned = token_account_of(&mut w, &k.pool(), &quote, &program);
        for to in [k.quote_vault(), pool_owned] {
            w.env
                .send_paid_by(&[sell_to(to)], &b, &[])
                .expect_code(code(SwapError::WrongTokenAccount));
        }
        // Nothing moved.
        assert_eq!(w.env.holding(&mint, &b.pubkey()), coins);
        assert_eq!(w.env.token_amount(&k.quote_vault()), vault_before);
        let after: Pool = w.env.read(&k.pool());
        assert_eq!(
            (
                after.quote_reserve,
                after.protocol_fees_quote,
                after.base_reserve
            ),
            (
                before.quote_reserve,
                before.protocol_fees_quote,
                before.base_reserve
            )
        );
        w.env.assert_spl_invariant(&k.pool());
        // To the seller's own account: the sell lands and delivers.
        let own = k.quote_account(&b.pubkey());
        let had = w.env.token_amount(&own);
        let tx = w.env.send_paid_by(&[sell_to(own)], &b, &[]);
        let ev: Swapped = tx.event();
        assert!(ev.delivered_out > 0);
        assert_eq!(w.env.token_amount(&own), had + ev.delivered_out);
        w.env.assert_spl_invariant(&k.pool());
    }
}
