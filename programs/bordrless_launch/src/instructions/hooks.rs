//! The pool hook callbacks (`docs/hooks-v2.md` §5.4): the DEX calls these, signing with its
//! signer for this program, `["hook-authority", LAUNCH_ID]` under the DEX ([`DEX_HOOK_AUTHORITY`]).
//! Only the DEX can sign for it, and only when it calls this program as a pool's hook: the signer
//! it gives any other pool hook is that hook's own, so a hook that passes its signer on in a CPI
//! here is refused (`BadHookSigner`), and the arguments a callback trusts are the DEX's.
//! `before_swap` sets the LP fee (the sniper schedule) and cuts the input: a buy's creator and
//! holder fees, a sell's burn. `after_swap` cuts the output: a buy's burn, a sell's creator and
//! holder fees. The math is `bordrless_core`'s ([`launch_before_swap`], [`launch_after_swap`]):
//! fees round up, burns round down, creator + holder is dropped when it is not below the amount,
//! a burn when it is not below the amount, and the holder fee is taken only while holders hold at
//! least the kit's threshold.
//!
//! A launch whose custom hook takes a SOL cut (`docs/quote-cut.md`) also asks the hook's
//! `quote_cut` on each trade (a buy before the swap, a sell after it) and takes its answer, capped
//! by the config, from the same SOL as the creator fee, into the same holding. The creator's
//! trades to itself (a companion's, for a companion coin) and the token's hook-vault slots' skip
//! it: it would pay the creator's side from the creator's side.
//!
//! A buy past the end of the curve takes what is left (§3.1's fill): `before_swap` finds the
//! smallest input that sells the curve out ([`launch_fill`]: the fees above, the DEX's LP fee and
//! share, and the SOL cut's rate, which the hook is asked once, for the trade as requested), takes
//! its cuts of that input and ends its answer with it; the DEX uses that much, sells the last token
//! and leaves the rest of the input with the trader, and `graduate` can follow in the same
//! transaction. A buy short of the end is unchanged.
//!
//! A launch quoted in tokens (`docs/token-quotes.md` §2.2, §6.1) answers on each of its leg pools
//! with its creator fee, on the quote side, into the leg's fee account, and its rules' burn on the
//! coin's side ([`leg_answer`]); it counts the legs' sales for max wallet ([`count_leg_sales`]) and
//! refuses every trade until all its legs are open.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::program::{get_return_data, invoke_signed};
use bordrless_core::{
    curve_out, launch_after_swap, launch_before_swap, launch_fill, quote_cut_fee, sniper_lp_fee,
    HookCut, Reserves,
};
use bordrless_hook::{clear_return_data, set_answer, Delta, HookReturn, PoolHookArgs};
use bordrless_swap::state::Pool;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::error::LaunchError;
use crate::events::{LegsMaxWalletLifted, QuoteCutTaken};
use crate::state::*;

/// Accounts of `before_initialize`: the prefix only. There is never a launch to find, so the
/// handler refuses.
#[derive(Accounts)]
pub struct RejectInitialize<'info> {
    /// CHECK: the DEX's signer for this program (address- and signer-checked).
    #[account(signer, address = DEX_HOOK_AUTHORITY @ LaunchError::BadHookSigner)]
    pub hook_signer: UncheckedAccount<'info>,
    /// CHECK: unused.
    pub pool: UncheckedAccount<'info>,
    /// CHECK: unused.
    pub base_mint: UncheckedAccount<'info>,
    /// CHECK: unused.
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: unused.
    pub actor: UncheckedAccount<'info>,
}

/// `before_initialize`.
pub fn process_before_initialize(
    _ctx: Context<RejectInitialize>,
    _args: PoolHookArgs,
) -> Result<HookReturn> {
    err!(LaunchError::OnlyLaunchpadCreatesPools)
}

/// Accounts of `before_swap` and `after_swap`: the prefix, then the registry's extras at 5 to 8:
/// the launch, its quote holding, the holder vault and the kit config. A leg pool of a launch
/// quoted in tokens (`docs/token-quotes.md` §2.2) passes at 6 the leg's fee account and at 7 the
/// launch's `Legs` instead, both checked in the handler ([`leg_answer`]).
#[derive(Accounts)]
pub struct HookCallback<'info> {
    /// CHECK: the DEX's signer for this program (address- and signer-checked).
    #[account(signer, address = DEX_HOOK_AUTHORITY @ LaunchError::BadHookSigner)]
    pub hook_signer: UncheckedAccount<'info>,
    /// CHECK: the pool (must be the launch's).
    pub pool: UncheckedAccount<'info>,
    /// CHECK: the base mint (must be the launch's).
    pub base_mint: UncheckedAccount<'info>,
    /// CHECK: the quote mint.
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: the trader.
    pub actor: UncheckedAccount<'info>,
    /// The launch of the base mint. Only `create_launch` (and `create_token_launch`) creates a
    /// `Launch`, at `["launch", mint]` with that mint, so the owner, the discriminator and the mint
    /// bind it without a derivation. A launch quoted in tokens has one pool per leg: the handler
    /// finds the pool among its legs.
    #[account(
        mut,
        constraint = launch.mint == base_mint.key() @ LaunchError::WrongPool,
        constraint = launch.is_token_quoted() || launch.pool == pool.key() @ LaunchError::WrongPool
    )]
    pub launch: Box<Account<'info, Launch>>,
    /// CHECK: the launch's quote holding (address-checked); receives creator fees. For a launch
    /// quoted in tokens: the leg's fee account (checked in the handler).
    #[account(
        mut,
        constraint = launch.is_token_quoted() || launch_quote.key() == launch.quote_holding @ LaunchError::WrongHolding
    )]
    pub launch_quote: UncheckedAccount<'info>,
    /// CHECK: the holder vault (address-checked); receives holder fees (the DEX checks it is a
    /// writable holding of the quote before paying it). For a launch quoted in tokens: its `Legs`
    /// (writable; checked in the handler).
    #[account(
        constraint = launch.is_token_quoted() || holder_vault.key() == launch.holder_vault @ LaunchError::WrongHolderVault
    )]
    pub holder_vault: UncheckedAccount<'info>,
    /// CHECK: the kit config (address-checked); read, through the kit crate, only when holder
    /// rewards are on.
    #[account(address = launch.kit_config @ LaunchError::WrongKitAccount)]
    pub kit_config: UncheckedAccount<'info>,
}

/// The kit's eligible supply and threshold, read when this side takes a holder fee (which holder
/// rewards being on implies); zeros otherwise, as nothing reads them then.
fn eligibility(ctx: &Context<'_, HookCallback<'_>>, holder_fee_bps: u16) -> Result<(u64, u64)> {
    if holder_fee_bps == 0 || !ctx.accounts.launch.rewards_on() {
        return Ok((0, 0));
    }
    let kit = bordrless_kit::client::read_kit_config(&ctx.accounts.kit_config)?;
    require_keys_eq!(
        kit.mint,
        ctx.accounts.launch.mint,
        LaunchError::WrongKitAccount
    );
    Ok((kit.eligible, kit.min_eligible))
}

/// The answer that takes `cut`: the creator fee to the launch's quote holding, the holder fee to
/// the holder vault, each only when above zero, and the burn; the launch counts each. The
/// counters are running totals for display (`claim_creator_fees` pays the quote holding's
/// balance, not a counter), so they saturate: a counter never blocks a trade.
fn answer(launch: &mut Launch, cut: &HookCut, lp_fee_bps: Option<u16>) -> HookReturn {
    let mut ret = HookReturn {
        lp_fee_bps,
        burn: cut.burn,
        ..HookReturn::default()
    };
    // A native launch is paid lamports: its creator fees on the launch account itself, its holder
    // fees on the kit config (which the kit's own slice passes writable in every launch-pool
    // swap); the DEX pays them in SOL whichever swap trades the pool.
    let (creator_index, holder_index) = if launch.is_native() {
        (LAUNCH_INDEX, KIT_CONFIG_INDEX)
    } else {
        (QUOTE_HOLDING_INDEX, HOLDER_VAULT_INDEX)
    };
    if cut.creator_fee > 0 {
        ret.deltas.push(Delta {
            amount: cut.creator_fee,
            account: creator_index,
        });
        launch.creator_fees_accrued = launch.creator_fees_accrued.saturating_add(cut.creator_fee);
    }
    if cut.holder_fee > 0 {
        ret.deltas.push(Delta {
            amount: cut.holder_fee,
            account: holder_index,
        });
        launch.holder_fees_accrued = launch.holder_fees_accrued.saturating_add(cut.holder_fee);
    }
    launch.burned_on_trades = launch.burned_on_trades.saturating_add(cut.burn);
    ret
}

/// Whether `actor` is one of `mint`'s `hook_vault` slots, which sell and buy the hook's token cut
/// on the token's own pool through `hook_vault`, whose swaps pass the pool's four extras only.
fn is_vault_slot(actor: &Pubkey, mint: &Pubkey) -> bool {
    (0..HOOK_VAULT_MAX_SLOTS).any(|i| {
        Pubkey::find_program_address(&[HOOK_VAULT_SLOT_SEED, mint.as_ref(), &[i]], &HOOK_VAULT_ID).0
            == *actor
    })
}

/// The rate of the custom hook's SOL cut on this trade (`docs/quote-cut.md`): `(answered, bps)`,
/// the hook's `quote_cut` answer (a `u16` of basis points in its return data; none is 0) and that
/// answer capped by the launch; `(0, 0)` without a cut on this side, for the creator's trades to
/// itself and for the token's hook-vault slots'. The trade fails when the accounts are missing or
/// wrong, or the hook fails or answers anything else. The hook is asked once, for the trade as the
/// DEX describes it (a filled buy's input is the whole input asked).
fn quote_cut_bps<'info>(
    ctx: &Context<'info, HookCallback<'info>>,
    args: &PoolHookArgs,
    buy: bool,
) -> Result<(u16, u16)> {
    let launch = &ctx.accounts.launch;
    let Some((hook, cut)) = launch.quote_cut() else {
        return Ok((0, 0));
    };
    let max_bps = cut.max_bps(buy);
    // The creator trading for itself (a companion's buys, sells and scripts all trade creator to
    // creator): the cut would pay the creator's side from the creator's side. Not when it delivers
    // to someone else, or it could relay cut-free buys for anyone.
    if max_bps == 0 || (args.actor == launch.creator && args.recipient == launch.creator) {
        return Ok((0, 0));
    }
    let extras = ctx.remaining_accounts;
    if extras.len() < QUOTE_CUT_EXTRAS {
        require!(
            is_vault_slot(&args.actor, &launch.mint),
            LaunchError::QuoteCutAccountsMissing
        );
        return Ok((0, 0));
    }
    let (state_bump, signer_bump) = launch.quote_cut_bumps();
    let [program, holding, state, signer] = [&extras[0], &extras[1], &extras[2], &extras[3]];
    let state_key = Pubkey::create_program_address(
        &[HOOK_STATE_SEED, launch.mint.as_ref(), &[state_bump]],
        &hook,
    )
    .map_err(|_| error!(LaunchError::QuoteCutWrongAccount))?;
    let signer_seeds: &[&[u8]] = &[QUOTE_CUT_SIGNER_SEED, hook.as_ref(), &[signer_bump]];
    let signer_key = Pubkey::create_program_address(signer_seeds, &crate::ID)
        .map_err(|_| error!(LaunchError::QuoteCutWrongAccount))?;
    require!(
        *program.key == hook
            && *holding.key == token_client::holding_address(&launch.mint, &args.actor)
            && *state.key == state_key
            && state.is_writable
            && *signer.key == signer_key,
        LaunchError::QuoteCutWrongAccount
    );

    let mut data = Vec::with_capacity(8 + 256 + args.hook_data.len());
    data.extend_from_slice(&QUOTE_CUT_DISCRIMINATOR);
    args.serialize(&mut data)?;
    let ix = Instruction {
        program_id: hook,
        accounts: vec![
            AccountMeta::new_readonly(signer_key, true),
            AccountMeta::new_readonly(launch.key(), false),
            AccountMeta::new_readonly(ctx.accounts.pool.key(), false),
            AccountMeta::new_readonly(ctx.accounts.base_mint.key(), false),
            AccountMeta::new_readonly(ctx.accounts.quote_mint.key(), false),
            AccountMeta::new_readonly(ctx.accounts.actor.key(), false),
            AccountMeta::new_readonly(*holding.key, false),
            AccountMeta::new(state_key, false),
        ],
        data,
    };
    clear_return_data();
    invoke_signed(
        &ix,
        &[
            signer.clone(),
            launch.to_account_info(),
            ctx.accounts.pool.to_account_info(),
            ctx.accounts.base_mint.to_account_info(),
            ctx.accounts.quote_mint.to_account_info(),
            ctx.accounts.actor.to_account_info(),
            holding.clone(),
            state.clone(),
            program.clone(),
        ],
        &[signer_seeds],
    )?;
    let answered = match get_return_data() {
        Some((from, bytes)) if from == hook => {
            let raw: [u8; 2] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| error!(LaunchError::QuoteCutBadAnswer))?;
            u16::from_le_bytes(raw)
        }
        _ => 0,
    };
    Ok((answered, answered.min(max_bps)))
}

/// The custom hook's SOL cut of `amount` (a buy's input, a sell's output) at the rate
/// [`quote_cut_bps`] answered (`answered`, capped to `bps`): rounded down, dropped when it and
/// `taken` (the creator and holder fees) would not stay below `amount`
/// (`bordrless_core::quote_cut_fee`); said in `QuoteCutTaken` when above zero.
fn take_quote_cut(
    launch: Pubkey,
    args: &PoolHookArgs,
    amount: u64,
    taken: u64,
    answered: u16,
    bps: u16,
) -> u64 {
    let fee = quote_cut_fee(amount, bps, taken);
    if fee > 0 {
        emit!(QuoteCutTaken {
            launch,
            actor: args.actor,
            direction: args.direction,
            answered_bps: answered,
            bps,
            amount: fee,
        });
    }
    fee
}

/// `cut` with the hook's SOL cut `fee` added to the creator fee (same holding, one delta), counted.
fn with_quote_cut(launch: &mut Launch, mut cut: HookCut, fee: u64) -> HookCut {
    if fee > 0 {
        cut.creator_fee = cut.creator_fee.saturating_add(fee);
        launch.add_quote_cut(fee);
    }
    cut
}

/// Bordrless's share of the cuts on the launch's pool (§3.1), read from the pool as the DEX keeps
/// it; a pool under the flat model (none the launchpad opens) shares nothing.
fn share_bps_of(pool: &AccountInfo) -> Result<u16> {
    let data = pool.try_borrow_data()?;
    let p = Pool::try_deserialize(&mut &data[..])?;
    Ok(if p.shares_cuts() {
        p.protocol_share_bps
    } else {
        0
    })
}

/// The answer on a leg pool of a launch quoted in tokens (`docs/token-quotes.md` §2.2, §6.1), in
/// `before_swap` (`before`) or `after_swap`: the pool is one of the launch's legs (its `Legs`, at
/// index 7, names it with this quote mint and its fee account at index 6); every leg must be open;
/// the creator fee is taken from the quote side (a buy's input before, a sell's output after) into
/// the leg's fee account, and counted on the leg; the rules' burn from the coin's side (a sell's
/// input before, a buy's output after), as on a SOL coin's pool. No LP fee override (the leg's pool
/// keeps its own), no holder fee, no sniper window, no creator first-buy rule. `after_swap` also
/// counts the leg's sales while max wallet waits to lift ([`count_leg_sales`]).
fn leg_answer(
    ctx: &Context<HookCallback>,
    args: &PoolHookArgs,
    before: bool,
) -> Result<HookReturn> {
    let a = &ctx.accounts;
    require_keys_eq!(args.pool, a.pool.key(), LaunchError::WrongPool);
    let legs_info = a.holder_vault.to_account_info();
    require!(
        *legs_info.owner == crate::ID && legs_info.is_writable,
        LaunchError::LegsAccountInvalid
    );
    // The leg's offset in the `Legs` account, read in place (no full decode on every trade).
    let (at, count) = {
        use anchor_lang::Discriminator;
        let data = legs_info.try_borrow_data()?;
        require!(
            data.len() >= Legs::LEN && &data[..8] == Legs::DISCRIMINATOR,
            LaunchError::LegsAccountInvalid
        );
        let key = |at: usize| Pubkey::new_from_array(data[at..at + 32].try_into().expect("32"));
        require_keys_eq!(
            key(Legs::MINT_OFFSET),
            a.launch.mint,
            LaunchError::LegsAccountInvalid
        );
        let count = usize::from(data[Legs::COUNT_OFFSET]).min(MAX_LEGS);
        require!(
            data[Legs::OPENED_OFFSET] == data[Legs::COUNT_OFFSET],
            LaunchError::LegsNotOpen
        );
        let at = (0..count)
            .map(|i| Legs::LEGS_OFFSET + i * Legs::LEG_LEN)
            .find(|at| key(*at) == a.quote_mint.key() && key(at + Legs::LEG_POOL) == args.pool)
            .ok_or(LaunchError::WrongPool)?;
        require_keys_eq!(
            key(at + Legs::LEG_FEE_ACCOUNT),
            a.launch_quote.key(),
            LaunchError::WrongLegAccount
        );
        (at, count)
    };
    let buy = args.direction == 1;
    // The launch's rules: its creator fee, its holder fee (the kit's holder rewards, §7.5.1, taken
    // only while holders hold the kit's threshold, as on a SOL coin) and its burn.
    let rates = a.launch.rules.fee_rates(a.launch.creator_fee_bps);
    let side_holder_bps = match (before, buy) {
        (true, true) => rates.holder_fee_buy_bps,
        (false, false) => rates.holder_fee_sell_bps,
        _ => 0,
    };
    let (eligible, min_eligible) = eligibility(ctx, side_holder_bps)?;
    let cut = if before {
        launch_before_swap(buy, args.amount_in, &rates, eligible, min_eligible)
    } else {
        launch_after_swap(buy, args.amount_out, &rates, eligible, min_eligible)
    };
    let mut ret = HookReturn {
        burn: cut.burn,
        ..HookReturn::default()
    };
    if cut.creator_fee > 0 {
        ret.deltas.push(Delta {
            amount: cut.creator_fee,
            account: LEG_FEES_INDEX,
        });
        // A running total for display (the claim pays the fee account's balance): saturating.
        let mut data = legs_info.try_borrow_mut_data()?;
        let field = at + Legs::LEG_ACCRUED;
        let accrued = u64::from_le_bytes(data[field..field + 8].try_into().expect("8"));
        data[field..field + 8]
            .copy_from_slice(&accrued.saturating_add(cut.creator_fee).to_le_bytes());
    }
    if cut.holder_fee > 0 {
        // Into the kit's vault of this leg's token (index 9): the kit config's associated token
        // account of it, derived here so no trader can name an account of its own.
        let vault = ctx
            .remaining_accounts
            .first()
            .ok_or(LaunchError::WrongKitVault)?;
        let program = legs_info.try_borrow_data()?[at + Legs::LEG_QUOTE_PROGRAM];
        let expected = bordrless_swap::spl::associated_token_address(
            &a.launch.kit_config,
            &a.quote_mint.key(),
            &bordrless_swap::spl::program_id(program),
        );
        require!(
            *vault.key == expected && vault.is_writable,
            LaunchError::WrongKitVault
        );
        ret.deltas.push(Delta {
            amount: cut.holder_fee,
            account: KIT_VAULT_INDEX,
        });
    }
    if !before && a.launch.modules & bordrless_kit::modules::MAX_WALLET != 0 {
        count_leg_sales(&legs_info, a.launch.key(), a.launch.mint, args, at, count)?;
    }
    Ok(ret)
}

/// Max wallet on a launch quoted in tokens (`docs/token-quotes.md` §6.1), in `after_swap` (the pool
/// as the trade left it): while it waits to lift (`Legs::lift_at` above 0, not yet lifted), the leg
/// records its coins sold (its share less its pool's base reserve), and when the legs together had
/// sold `lift_at` before this trade, it lifts: `Legs::lifted`, which the kit reads before this
/// trade's output reaches the trader, and `LegsMaxWalletLifted`. The trade that crosses is capped
/// like the buy that sells a SOL coin's curve out (graduation, which lifts it there, follows it);
/// the first trade after it, on any leg, lifts. Nothing is read or written for a launch without
/// max wallet.
fn count_leg_sales(
    legs_info: &AccountInfo,
    launch: Pubkey,
    mint: Pubkey,
    args: &PoolHookArgs,
    at: usize,
    count: usize,
) -> Result<()> {
    let mut data = legs_info.try_borrow_mut_data()?;
    let u64_at =
        |data: &[u8], at: usize| u64::from_le_bytes(data[at..at + 8].try_into().expect("8"));
    let lift_at = u64_at(&data[..], Legs::LIFT_AT_OFFSET);
    if lift_at == 0 || data[Legs::LIFTED_OFFSET] == 1 {
        return Ok(());
    }
    let sold_at = |i: usize| Legs::SOLD_OFFSET + 8 * i;
    let before: i128 = (0..count)
        .map(|i| i128::from(u64_at(&data[..], sold_at(i)) as i64))
        .sum();
    let i = (at - Legs::LEGS_OFFSET) / Legs::LEG_LEN;
    let share = u64_at(&data[..], at + Legs::LEG_SHARE);
    let sold = i128::from(share) - i128::from(args.base_reserve);
    let sold = i64::try_from(sold).unwrap_or(if sold < 0 { i64::MIN } else { i64::MAX });
    data[sold_at(i)..sold_at(i) + 8].copy_from_slice(&sold.to_le_bytes());
    if before >= i128::from(lift_at) {
        data[Legs::LIFTED_OFFSET] = 1;
        emit!(LegsMaxWalletLifted {
            launch,
            mint,
            pool: args.pool,
            sold: u64::try_from(before).unwrap_or(u64::MAX),
            lift_at,
            ts: Clock::get()?.unix_timestamp,
        });
    }
    Ok(())
}

/// `before_swap`: the LP fee (the sniper schedule; the creator's own first buy into its own
/// wallet inside the window pays the normal fee, once), then the cut from the input; a buy past
/// the end of the curve is filled (the module doc), its cuts of the part used, and the answer
/// ends with that part.
pub fn process_before_swap<'info>(
    ctx: Context<'info, HookCallback<'info>>,
    args: PoolHookArgs,
) -> Result<()> {
    if ctx.accounts.launch.is_token_quoted() {
        let ret = leg_answer(&ctx, &args, true)?;
        let launch = &mut ctx.accounts.launch;
        launch.burned_on_trades = launch.burned_on_trades.saturating_add(ret.burn);
        set_answer(&ret, None);
        return Ok(());
    }
    require_keys_eq!(args.pool, ctx.accounts.launch.pool, LaunchError::WrongPool);
    let now = Clock::get()?.unix_timestamp;
    let buy = args.direction == 1;
    let rates = {
        let l = &ctx.accounts.launch;
        l.rules.fee_rates(l.creator_fee_bps)
    };
    let (eligible, min_eligible) =
        eligibility(&ctx, if buy { rates.holder_fee_buy_bps } else { 0 })?;
    let launch = &mut ctx.accounts.launch;
    let own_first = buy
        && args.actor == launch.creator
        && args.recipient == launch.creator
        && !launch.creator_bought
        && now < launch.created_at.saturating_add(launch.sniper_window_secs);
    let lp_fee = if own_first {
        launch.creator_bought = true;
        launch.lp_fee_bps
    } else {
        sniper_lp_fee(
            now,
            launch.created_at,
            launch.sniper_window_secs,
            launch.sniper_start_bps,
            launch.lp_fee_bps,
        )
    };
    let on_curve = launch.status == STATUS_CURVE;
    // A buy's SOL cut rate, asked once for the trade as requested.
    let (answered, cut_bps) = if buy {
        quote_cut_bps(&ctx, &args, true)?
    } else {
        (0, 0)
    };
    // A buy past the end of the curve: the smallest input that sells it out, on which every fee
    // below is then taken; the DEX leaves the rest with the trader.
    // Only for a swap the transaction sends the DEX itself (the DEX at the top, this callback one
    // below): a program that buys through the DEX (the router, a companion, the hook vault)
    // measures nothing past what it asked, so its buys are answered exactly as before the fill.
    let direct = anchor_lang::solana_program::instruction::get_stack_height()
        == anchor_lang::solana_program::instruction::TRANSACTION_LEVEL_STACK_HEIGHT + 1;
    // `launch_fill`'s own first test, made here so a buy short of the end never reads the pool:
    // the whole input into the curve, fees aside, short of every token left is no fill.
    let reaches_end = args.base_reserve > 0
        && curve_out(
            args.amount_in,
            args.quote_reserve,
            args.virtual_quote,
            args.base_reserve,
            args.virtual_base,
        )
        .is_some_and(|out| out >= u128::from(args.base_reserve));
    let fill = if buy && on_curve && direct && reaches_end {
        let reserves = Reserves {
            base_reserve: args.base_reserve,
            quote_reserve: args.quote_reserve,
            virtual_base: args.virtual_base,
            virtual_quote: args.virtual_quote,
        };
        launch_fill(
            &reserves,
            args.amount_in,
            lp_fee,
            share_bps_of(&ctx.accounts.pool)?,
            &rates,
            eligible,
            min_eligible,
            cut_bps,
        )
    } else {
        None
    };
    let amount_in = fill.unwrap_or(args.amount_in);
    let cut = launch_before_swap(buy, amount_in, &rates, eligible, min_eligible);
    // A buy's SOL cut, from the input with the creator fee.
    let fee = if buy {
        take_quote_cut(
            ctx.accounts.launch.key(),
            &args,
            amount_in,
            cut.creator_fee.saturating_add(cut.holder_fee),
            answered,
            cut_bps,
        )
    } else {
        0
    };
    let launch = &mut ctx.accounts.launch;
    let cut = with_quote_cut(launch, cut, fee);
    let ret = answer(launch, &cut, Some(lp_fee));
    set_answer(&ret, fill);
    Ok(())
}

/// `after_swap`: the cut from the output the DEX hands on (a buy: the curve's output; a sell: the
/// curve's output less the DEX's protocol fee, which is always in the quote).
pub fn process_after_swap<'info>(
    ctx: Context<'info, HookCallback<'info>>,
    args: PoolHookArgs,
) -> Result<HookReturn> {
    if ctx.accounts.launch.is_token_quoted() {
        let ret = leg_answer(&ctx, &args, false)?;
        let launch = &mut ctx.accounts.launch;
        launch.burned_on_trades = launch.burned_on_trades.saturating_add(ret.burn);
        return Ok(ret);
    }
    require_keys_eq!(args.pool, ctx.accounts.launch.pool, LaunchError::WrongPool);
    let buy = args.direction == 1;
    let rates = {
        let l = &ctx.accounts.launch;
        l.rules.fee_rates(l.creator_fee_bps)
    };
    // A sell's input has left the seller by now: the kit counts it out of the eligible supply.
    let (eligible, min_eligible) =
        eligibility(&ctx, if buy { 0 } else { rates.holder_fee_sell_bps })?;
    let cut = launch_after_swap(buy, args.amount_out, &rates, eligible, min_eligible);
    // A sell's SOL cut, from the output with the creator fee.
    let fee = if buy {
        0
    } else {
        let (answered, bps) = quote_cut_bps(&ctx, &args, false)?;
        take_quote_cut(
            ctx.accounts.launch.key(),
            &args,
            args.amount_out,
            cut.creator_fee.saturating_add(cut.holder_fee),
            answered,
            bps,
        )
    };
    let launch = &mut ctx.accounts.launch;
    let cut = with_quote_cut(launch, cut, fee);
    Ok(answer(launch, &cut, None))
}
