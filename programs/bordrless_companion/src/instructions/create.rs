//! `create` and `launch`: a companion made for a mint that does not exist yet, then the launch
//! created through it, with the companion's creator address as the launch's creator. A script's or
//! a preset's coin quoted in tokens is launched with `launch_token` (`docs/token-quotes.md` §7).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::{system_program, InstructionData};
use bordrless_launch::instructions::{CreateLaunchArgs, CreateTokenLaunchArgs};
use bordrless_launch::state::{Launch, Legs};
use bordrless_swap::state::Pool;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::error::CompanionError;
use crate::events::{CompanionCreated, CompanionLaunched};
use crate::instructions::CreatorSeeds;
use crate::invoke::invoke_built;
use crate::state::*;

/// Arguments of `create`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct CreateArgs {
    pub split: Split,
    pub bounty_bps: u16,
    /// The most one buyback spends, lamports (ignored without a buyback share).
    pub max_buyback: u64,
    /// The least time between buybacks, seconds.
    pub buyback_interval: i64,
    /// The dev bag vests over this many seconds from the launch.
    pub vest_secs: i64,
    /// Lamports for the creator address: the launch's fee and rent (it pays them as the launch's
    /// creator) and its own rent-exempt minimum, which it keeps.
    pub fund: u64,
}

/// Accounts of `create`.
#[event_cpi]
#[derive(Accounts)]
pub struct Create<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: any address: who receives the beneficiary's part and the vested dev bag.
    pub beneficiary: UncheckedAccount<'info>,
    /// The launch's mint, a fresh keypair: it signs here, so only whoever holds it can make its
    /// companion (nobody can make one first and become its beneficiary), and the launch later.
    pub mint: Signer<'info>,
    #[account(init, payer = payer, space = Companion::LEN, seeds = [COMPANION_SEED, mint.key().as_ref()], bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address, `PDA(["creator", mint])`, system-owned and without data.
    #[account(mut, seeds = [CREATOR_SEED, mint.key().as_ref()], bump)]
    pub creator: UncheckedAccount<'info>,
    /// CHECK: bridged SOL (address-checked).
    #[account(address = BRIDGED_SOL_MINT)]
    pub bridged_sol_mint: UncheckedAccount<'info>,
    /// CHECK: the creator's holding of bridged SOL, created here (the token program checks it).
    #[account(mut)]
    pub creator_quote: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = TOKEN_ID)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority (the token program checks it).
    pub token_event_authority: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

pub fn process_create(ctx: Context<Create>, args: CreateArgs) -> Result<()> {
    require!(args.split.valid(), CompanionError::BadSplit);
    require!(
        args.bounty_bps <= MAX_BOUNTY_BPS,
        CompanionError::BountyTooHigh
    );
    if args.split.buyback_bps > 0 {
        require!(
            args.max_buyback >= MIN_MAX_BUYBACK
                && (MIN_BUYBACK_INTERVAL..=MAX_BUYBACK_INTERVAL).contains(&args.buyback_interval),
            CompanionError::BadBuybackLimits
        );
    }
    require!(
        (0..=MAX_VEST_SECS).contains(&args.vest_secs),
        CompanionError::VestTooLong
    );
    let rent_min = Rent::get()?.minimum_balance(0);
    let creator_after = ctx
        .accounts
        .creator
        .lamports()
        .checked_add(args.fund)
        .ok_or(CompanionError::MathOverflow)?;
    require!(creator_after >= rent_min, CompanionError::Underfunded);
    if args.fund > 0 {
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                system_program::Transfer {
                    from: ctx.accounts.payer.to_account_info(),
                    to: ctx.accounts.creator.to_account_info(),
                },
            ),
            args.fund,
        )?;
    }
    // The creator's bridged-SOL holding: creator fees are claimed into it.
    let creator = ctx.accounts.creator.key();
    let ix = token_client::create_holding(ctx.accounts.payer.key(), BRIDGED_SOL_MINT, creator);
    let available = ctx.accounts.to_account_infos();
    invoke_built(&ix, &available, &[])?;

    let mint = ctx.accounts.mint.key();
    let c = &mut ctx.accounts.companion;
    c.version = VERSION;
    c.bump = ctx.bumps.companion;
    c.creator_bump = ctx.bumps.creator;
    c.mint = mint;
    c.beneficiary = ctx.accounts.beneficiary.key();
    c.split = args.split;
    c.bounty_bps = args.bounty_bps;
    c.max_buyback = args.max_buyback;
    c.buyback_interval = args.buyback_interval;
    c.vest_secs = args.vest_secs;
    // No game until `create_game` (the v2 fields, zero as the reserved bytes they replace were).
    c.game_hook = Pubkey::default();
    c.pot_bps = 0;
    c.pending_pot = 0;
    c.round_secs = 0;
    c.stranded_burned_at = 0;
    c.game_kind = GameKind::Lottery;
    c.pot_locked = 0;
    c.mode = MODE_CLASSIC;
    emit_cpi!(CompanionCreated {
        companion: c.key(),
        mint,
        creator,
        beneficiary: c.beneficiary,
        split: c.split,
        bounty_bps: c.bounty_bps,
        max_buyback: c.max_buyback,
        buyback_interval: c.buyback_interval,
        vest_secs: c.vest_secs,
    });
    Ok(())
}

/// Accounts of `launch`. The remaining accounts are exactly `create_launch`'s, in its order (the
/// launchpad's client builds them with the creator address as the creator); the launchpad checks
/// every one of them.
#[event_cpi]
#[derive(Accounts)]
pub struct LaunchIt<'info> {
    pub launcher: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked).
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    /// CHECK: the launchpad.
    #[account(address = LAUNCH_ID)]
    pub launch_program: UncheckedAccount<'info>,
}

/// `create_launch`'s first account is the creator, its fourth the mint, its fifth the launch.
const CREATOR_AT: usize = 0;
const MINT_AT: usize = 3;
const LAUNCH_AT: usize = 4;

pub fn process_launch<'info>(
    ctx: Context<'info, LaunchIt<'info>>,
    args: CreateLaunchArgs,
) -> Result<()> {
    let c = &ctx.accounts.companion;
    require!(!c.launched, CompanionError::AlreadyLaunched);
    // The companion's vesting replaces the kit's creator wallet lock, which would lock its creator
    // address (the dev bag) instead of a person; holders can only be paid with holder rewards on.
    require!(
        args.rules.creator_lock_secs == 0,
        CompanionError::CreatorLockUnsupported
    );
    if c.split.holders_bps > 0 {
        require!(args.rules.rewards_on(), CompanionError::HolderRewardsOff);
    }
    // With holder rewards the kit lets only wallets hold the token: the dev bag must have one to go to.
    if args.rules.rewards_on() {
        require!(
            c.beneficiary.is_on_curve(),
            CompanionError::BeneficiaryNotAWallet
        );
    }
    // The client appends the companion's script address after `create_launch`'s accounts; it is
    // read here (audit H-1) and never forwarded (the launchpad refuses accounts it doesn't expect).
    let script_key = Pubkey::find_program_address(&[SCRIPT_SEED, c.mint.as_ref()], &crate::ID).0;
    let (remaining, script_info) = match ctx.remaining_accounts.split_last() {
        Some((last, rest)) if *last.key == script_key => (rest, Some(last)),
        _ => (ctx.remaining_accounts, None),
    };
    require!(remaining.len() > LAUNCH_AT, CompanionError::MissingAccount);
    require_keys_eq!(
        *remaining[CREATOR_AT].key,
        ctx.accounts.creator.key(),
        CompanionError::WrongCreator
    );
    require_keys_eq!(*remaining[MINT_AT].key, c.mint, CompanionError::WrongMint);
    let creator = ctx.accounts.creator.key();
    invoke_launchpad(
        &ctx.accounts.creator.to_account_info(),
        &ctx.accounts.launch_program.to_account_info(),
        &CreatorSeeds::of(c),
        remaining,
        bordrless_launch::instruction::CreateLaunch { args }.data(),
    )?;

    // The launch as created: its creator this companion's, a game's hook the game's (whose state
    // must be the game's), no config author paid (their claims would pay the companion outside its
    // steps). Any other companion may launch with a custom hook (2026-10-12; the launchpad takes
    // only a hook that Bordrless or nobody can upgrade, and the site offers approved ones): every
    // step that moves the token carries it. A custom hook excludes kit rules, so a holders' share
    // stays impossible beside one (`HolderRewardsOff` above).
    let launch = Account::<Launch>::try_from(&remaining[LAUNCH_AT])?;
    require_keys_eq!(launch.creator, creator, CompanionError::WrongCreator);
    if c.is_game() {
        check_game_launch(c, &launch, remaining)?;
    }
    require!(
        launch.author_share_bps == 0,
        CompanionError::AuthorShareUnsupported
    );
    // The buyback's reference price: the opening price, which nobody can have moved yet.
    let pool_info = remaining
        .iter()
        .find(|a| *a.key == launch.pool && *a.owner == SWAP_ID)
        .ok_or(CompanionError::MissingAccount)?;
    let pool = Pool::try_deserialize(&mut &pool_info.try_borrow_data()?[..])?;
    let reference = spot_price(
        pool.quote_reserve,
        pool.virtual_quote,
        pool.base_reserve,
        pool.virtual_base,
    )
    .ok_or(CompanionError::NoQuote)?;
    let now = Clock::get()?.unix_timestamp;
    // A launch made native (native launches on): the companion is native from here. A script
    // that is not native-ready (it trades the classic swap with bridged SOL) would fail every
    // step, so such a launch is refused while native launches are on (audit H-1, I-5).
    if launch.is_native() && c.is_script() {
        let info = script_info.ok_or(CompanionError::MissingAccount)?;
        let script = Account::<Script>::try_from(info)?;
        require!(
            !NOT_NATIVE_READY_SCRIPTS.contains(&script.program),
            CompanionError::ScriptNotNativeReady
        );
    }
    if launch.is_native() {
        let system = remaining
            .iter()
            .find(|a| *a.key == anchor_lang::system_program::ID)
            .ok_or(CompanionError::MissingAccount)?
            .clone();
        let creator_info = ctx.accounts.creator.to_account_info();
        let spare = crate::instructions::native::native_at_launch(
            &mut ctx.accounts.companion,
            &creator_info,
            &system,
        )?;
        emit_cpi!(crate::events::CompanionNativeSet {
            companion: ctx.accounts.companion.key(),
            native: true,
            moved: 0,
            spare,
        });
    }
    let c = &mut ctx.accounts.companion;
    c.launched = true;
    c.launched_at = now;
    c.reference_price = reference;
    c.reference_at = now;
    emit_cpi!(CompanionLaunched {
        companion: c.key(),
        mint: c.mint,
        ts: now
    });
    Ok(())
}

/// Calls the launchpad with `data` (`create_launch`'s or `create_token_launch`'s) and `remaining` as
/// its accounts, in their order, the creator address signing (it is never a signer outside this
/// program).
fn invoke_launchpad<'info>(
    creator: &AccountInfo<'info>,
    launch_program: &AccountInfo<'info>,
    seeds: &CreatorSeeds,
    remaining: &[AccountInfo<'info>],
    data: Vec<u8>,
) -> Result<()> {
    let ix = Instruction {
        program_id: LAUNCH_ID,
        accounts: remaining
            .iter()
            .map(|a| AccountMeta {
                pubkey: *a.key,
                is_signer: a.is_signer || a.key == creator.key,
                is_writable: a.is_writable,
            })
            .collect(),
        data,
    };
    let mut available = remaining.to_vec();
    available.push(launch_program.clone());
    available.push(creator.clone());
    invoke_built(&ix, &available, &[&seeds.seeds()])
}

/// Accounts of `launch_token`: `launch`'s, and the companion's pots on the legs.
#[event_cpi]
#[derive(Accounts)]
pub struct LaunchToken<'info> {
    pub launcher: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked).
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    /// CHECK: `PDA(["companion-legs", mint])` (seeds-checked): made here for a preset companion
    /// (its pots on the legs); a script companion's is left alone (its pot is the script's).
    #[account(mut, seeds = [COMPANION_LEGS_SEED, companion.mint.as_ref()], bump)]
    pub companion_legs: UncheckedAccount<'info>,
    /// CHECK: the launchpad.
    #[account(address = LAUNCH_ID)]
    pub launch_program: UncheckedAccount<'info>,
}

/// `create_token_launch`'s sixth account: the launch's `Legs`.
const LEGS_AT: usize = 5;

/// A program-owned account at a PDA of this program (`seeds`, which sign), `space` bytes, its rent
/// paid by the creator address (`payer_seeds`). An address someone funded first is topped up,
/// allocated and assigned, so nobody can block it.
fn create_owned<'info>(
    payer: &AccountInfo<'info>,
    payer_seeds: &[&[u8]],
    account: &AccountInfo<'info>,
    system: &AccountInfo<'info>,
    space: usize,
    seeds: &[&[u8]],
) -> Result<()> {
    let rent = Rent::get()?.minimum_balance(space);
    let short = rent.saturating_sub(account.lamports());
    if short > 0 {
        system_program::transfer(
            CpiContext::new_with_signer(
                *system.key,
                system_program::Transfer {
                    from: payer.clone(),
                    to: account.clone(),
                },
                &[payer_seeds],
            ),
            short,
        )?;
    }
    system_program::allocate(
        CpiContext::new_with_signer(
            *system.key,
            system_program::Allocate {
                account_to_allocate: account.clone(),
            },
            &[seeds],
        ),
        space as u64,
    )?;
    system_program::assign(
        CpiContext::new_with_signer(
            *system.key,
            system_program::Assign {
                account_to_assign: account.clone(),
            },
            &[seeds],
        ),
        &crate::ID,
    )
}

/// A preset companion's `CompanionLegs` for `mint`, made at `info` (its bump `bump`), the creator
/// address paying the rent: a `LegPot` for every leg of the launch's `Legs` (`legs`, read by its
/// offsets, one leg at a time: the whole account would not fit the stack), each in the leg's token,
/// the buyback's and the Snowball's references (each its own, review3 F1) the leg's opening price,
/// its paid parts' currency `currency` (the launch's SOL switch, `docs/token-quotes.md` §7.5.2).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn open_pots<'info>(
    creator: &AccountInfo<'info>,
    seeds: &CreatorSeeds,
    info: &AccountInfo<'info>,
    bump: u8,
    system: &AccountInfo<'info>,
    legs: &AccountInfo<'info>,
    mint: Pubkey,
    now: i64,
    currency: u8,
) -> Result<()> {
    require_keys_eq!(*legs.owner, LAUNCH_ID, CompanionError::MissingAccount);
    let src = legs.try_borrow_data()?;
    require!(
        src.len() >= Legs::LEN
            && src[..8] == *Legs::DISCRIMINATOR
            && src[Legs::MINT_OFFSET..Legs::MINT_OFFSET + 32] == mint.to_bytes(),
        CompanionError::WrongMint
    );
    let count = src[Legs::COUNT_OFFSET];
    require!(
        (1..=MAX_COMPANION_LEGS as u8).contains(&count),
        CompanionError::MissingAccount
    );
    create_owned(
        creator,
        &seeds.seeds(),
        info,
        system,
        CompanionLegs::LEN,
        &[COMPANION_LEGS_SEED, mint.as_ref(), &[bump]],
    )?;
    let mut data = info.try_borrow_mut_data()?;
    let mut w: &mut [u8] = &mut data;
    // `CompanionLegs` as laid out: the discriminator, the header, the legs in use (the rest zeros,
    // as allocated).
    std::io::Write::write_all(&mut w, CompanionLegs::DISCRIMINATOR)?;
    (COMPANION_LEGS_VERSION, bump, mint, count).serialize(&mut w)?;
    for i in 0..usize::from(count) {
        let at = Legs::LEGS_OFFSET + i * Legs::LEG_LEN;
        let leg = bordrless_launch::state::Leg::deserialize(&mut &src[at..at + Legs::LEG_LEN])?;
        // The opening price, which nobody can have moved yet: no virtual base, the leg's virtual
        // quote over its share.
        let opening =
            spot_price(0, leg.virtual_quote, leg.share, 0).ok_or(CompanionError::NoQuote)?;
        LegPot {
            quote_mint: leg.quote_mint,
            quote_program: leg.quote_program,
            decimals: leg.decimals,
            currency,
            reference_price: opening,
            reference_at: now,
            snowball_reference: opening,
            ..LegPot::default()
        }
        .serialize(&mut w)?;
    }
    Ok(())
}

/// A preset companion's `PayeesLegs` for `mint` (`docs/token-quotes.md` §7.5), made at `info` (its
/// address checked by the caller), the creator address paying the rent: nothing owed yet.
#[inline(never)]
fn open_payees_legs<'info>(
    creator: &AccountInfo<'info>,
    seeds: &CreatorSeeds,
    info: &AccountInfo<'info>,
    system: &AccountInfo<'info>,
    mint: Pubkey,
) -> Result<()> {
    let bump = PayeesLegs::address(&mint).1;
    create_owned(
        creator,
        &seeds.seeds(),
        info,
        system,
        PayeesLegs::LEN,
        &[PAYEES_LEGS_SEED, mint.as_ref(), &[bump]],
    )?;
    let mut data = info.try_borrow_mut_data()?;
    let mut w: &mut [u8] = &mut data;
    std::io::Write::write_all(&mut w, PayeesLegs::DISCRIMINATOR)?;
    (PAYEES_LEGS_VERSION, bump, mint).serialize(&mut w)?;
    Ok(())
}

/// `launch_token` (`docs/token-quotes.md` §7, §7.4, §7.5, §7.6): a coin quoted in tokens, launched
/// through the launchpad's `create_token_launch` with the creator address as its creator. A script
/// companion taking the whole creator fee as its pot keeps it in the creator address's token
/// accounts (each leg's fee lands whole in its token account of the leg's token, for the script,
/// §7). Every other companion (a buyback, the launcher's part, the holders' part, a split of them,
/// a Snowball, a game, a script with a launcher's part: §7.6) keeps its parts per leg in the leg's
/// token: its `CompanionLegs` is made here, each leg's references its opening price, its paid
/// parts' currency the launch's SOL switch. A game is checked as `launch` checks it, its `Game`
/// appended after `create_token_launch`'s accounts: without the SOL switch its pot is kept in the
/// legs' tokens, and it must have its token terms (`set_game_token_terms`); with the switch it has
/// none and plays as a SOL coin's (`TokenGameTerms` otherwise); a lottery's funded spare is its
/// oracle's SOL float. A holders' part needs the coin's holder rewards (`HolderRewardsOff`, as
/// `launch`); payees are paid in the legs' tokens (their `PayeesLegs`, after `create_token_launch`'s
/// accounts, made here) or, with the switch, in SOL by `withdraw`. Refused: a strategy game
/// (`StrategyOnTokenCoin`: SOL coins only), the kit's creator wallet lock
/// (`CreatorLockUnsupported`, as `launch`). The launch as created: its creator this companion's, no
/// config author paid. The companion is native from here (as a companion launched while native
/// launches are on): SOL that reaches its creator address is its lamports.
pub fn process_launch_token<'info>(
    ctx: Context<'info, LaunchToken<'info>>,
    args: CreateTokenLaunchArgs,
) -> Result<()> {
    let c = &ctx.accounts.companion;
    require!(!c.launched, CompanionError::AlreadyLaunched);
    // Every game but a strategy (`docs/token-quotes.md` §7.6: its plans and budgets are lamports).
    if c.is_game() {
        require!(
            c.game_kind != GameKind::Strategy,
            CompanionError::StrategyOnTokenCoin
        );
    }
    // Holders can only be paid with holder rewards on (as `launch`).
    if c.split.holders_bps > 0 {
        require!(args.rules.rewards_on(), CompanionError::HolderRewardsOff);
    }
    require!(
        args.rules.creator_lock_secs == 0,
        CompanionError::CreatorLockUnsupported
    );
    // A companion whose parts are kept per leg (a preset, a game, a script with a launcher's part:
    // `docs/token-quotes.md` §7.6); a script taking the whole fee as its pot keeps none (its pot is
    // the creator address's token accounts, §7).
    let pots_on_legs = !c.is_script() || u64::from(c.pot_bps) < BPS;
    // Accounts the client appends after `create_token_launch`'s, read here and never forwarded:
    // with payees whose launcher's part stays in the legs' tokens, what it owes them per leg
    // (`PayeesLegs`); for a game, its `Game` (its token terms).
    let payees_legs_key = PayeesLegs::address(&c.mint).0;
    let game_key = Game::address(&c.mint).0;
    let mut remaining = ctx.remaining_accounts;
    let mut payees_legs = None;
    let mut game_info = None;
    while let Some((last, rest)) = remaining.split_last() {
        if *last.key == payees_legs_key
            && payees_legs.is_none()
            && c.version == VERSION_WITH_PAYEES
            && pots_on_legs
        {
            payees_legs = Some(last);
        } else if *last.key == game_key && game_info.is_none() && c.is_game() {
            game_info = Some(last);
        } else {
            break;
        }
        remaining = rest;
    }
    require!(remaining.len() > LEGS_AT, CompanionError::MissingAccount);
    require_keys_eq!(
        *remaining[CREATOR_AT].key,
        ctx.accounts.creator.key(),
        CompanionError::WrongCreator
    );
    require_keys_eq!(*remaining[MINT_AT].key, c.mint, CompanionError::WrongMint);
    let creator = ctx.accounts.creator.key();
    let seeds = CreatorSeeds::of(c);
    let creator_info = ctx.accounts.creator.to_account_info();
    invoke_launchpad(
        &creator_info,
        &ctx.accounts.launch_program.to_account_info(),
        &seeds,
        remaining,
        bordrless_launch::instruction::CreateTokenLaunch { args }.data(),
    )?;
    let launch = Account::<Launch>::try_from(&remaining[LAUNCH_AT])?;
    require_keys_eq!(launch.creator, creator, CompanionError::WrongCreator);
    require!(
        launch.author_share_bps == 0,
        CompanionError::AuthorShareUnsupported
    );
    // A game: its hook as `launch` checks it, and its pot's terms. With the SOL switch its pot is
    // sold to SOL and it plays as a SOL coin's game (no token terms); without, its pot is kept per
    // leg in the legs' tokens and its least pot is in basis points of the opening value
    // (`set_game_token_terms`).
    // A game with its terms is marked launched in tokens here (its `Game`, writable), the one way
    // a game's pot comes to be kept in the legs' tokens: terms set before a SOL coin's `launch`
    // never count (`Game::in_tokens`).
    let lottery_in_tokens = if c.is_game() {
        check_game_launch(c, &launch, remaining)?;
        let info = game_info.ok_or(CompanionError::MissingAccount)?;
        let mut game = Account::<Game>::try_from(info)?;
        let terms = game.min_pot_bps() > 0;
        require!(terms != launch.sol_switch(), CompanionError::TokenGameTerms);
        if terms {
            require!(info.is_writable, CompanionError::MissingAccount);
            game.set_launched_in_tokens();
            let mut data = info.try_borrow_mut_data()?;
            let mut writer: &mut [u8] = &mut data;
            game.try_serialize(&mut writer)?;
        }
        terms && c.game_kind == GameKind::Lottery
    } else {
        false
    };
    let now = Clock::get()?.unix_timestamp;
    let system = remaining
        .iter()
        .find(|a| *a.key == anchor_lang::system_program::ID)
        .ok_or(CompanionError::MissingAccount)?
        .clone();
    // A preset companion's pots on the legs, the creator address paying their rent; the launcher's
    // and the holders' parts in SOL with the launch's switch (§7.5.2).
    let currency = if launch.sol_switch() {
        LEG_CURRENCY_SOL
    } else {
        LEG_CURRENCY_TOKEN
    };
    if pots_on_legs {
        open_pots(
            &creator_info,
            &seeds,
            &ctx.accounts.companion_legs.to_account_info(),
            ctx.bumps.companion_legs,
            &system,
            &remaining[LEGS_AT],
            c.mint,
            now,
            currency,
        )?;
    }
    // Payees paid in the legs' tokens: what they are owed per leg (without the switch; with it
    // they are paid in SOL by `withdraw`, as on a SOL coin).
    if c.version == VERSION_WITH_PAYEES && pots_on_legs && !launch.sol_switch() {
        let info = payees_legs.ok_or(CompanionError::PayeesLegsMissing)?;
        open_payees_legs(&creator_info, &seeds, info, &system, c.mint)?;
    }
    let spare = crate::instructions::native::native_at_launch(
        &mut ctx.accounts.companion,
        &creator_info,
        &system,
    )?;
    emit_cpi!(crate::events::CompanionNativeSet {
        companion: ctx.accounts.companion.key(),
        native: true,
        moved: 0,
        spare,
    });
    let c = &mut ctx.accounts.companion;
    // A lottery in the legs' tokens pays its oracle in SOL: the spare the launcher funded is its
    // float (`pending_pot`), not the launcher's part.
    if lottery_in_tokens {
        c.pending_beneficiary -= spare;
        c.pending_pot = c
            .pending_pot
            .checked_add(spare)
            .ok_or(CompanionError::MathOverflow)?;
    }
    c.launched = true;
    c.launched_at = now;
    c.reference_price = 0;
    c.reference_at = now;
    emit_cpi!(CompanionLaunched {
        companion: c.key(),
        mint: c.mint,
        ts: now
    });
    Ok(())
}

/// A game companion's launch: the token's hook is the game's, with exactly the callbacks its kind
/// needs (`GameKind::hook_flags`: transfers and burns, writing hook data; no deltas, which could
/// skim the companion's buybacks, and no callback the hook may lack, which would fail every burn),
/// and the hook's state for the mint is a game ticket header for this mint with the game's round
/// length (and, for a jackpot or a streak, its kind's header after it). The state is the hook's own
/// extra account in `create_launch`'s, so the check needs no account of its own (nor the `Game`:
/// the companion keeps the hook, its kind and the round length).
fn check_game_launch(c: &Companion, launch: &Launch, remaining: &[AccountInfo]) -> Result<()> {
    require!(
        launch.custom_hook == Some(c.game_hook)
            && launch.custom_hook_flags == c.game_kind.hook_flags(),
        CompanionError::GameHookMismatch
    );
    let state = bordrless_game::state_address(&c.game_hook, &c.mint).0;
    let info = remaining
        .iter()
        .find(|a| *a.key == state)
        .ok_or(CompanionError::MissingAccount)?;
    let header = bordrless_game::read_state(info, &c.game_hook, &c.mint)
        .map_err(|_| error!(CompanionError::HookState))?;
    require!(header.round_secs == c.round_secs, CompanionError::HookState);
    let kind_header = match c.game_kind {
        GameKind::Lottery => true,
        // A strategy's tickets: a lottery-format header, never a jackpot's or a streak's.
        GameKind::Strategy => {
            let data = info.try_borrow_data()?;
            bordrless_game::JackpotHeader::parse(&data).is_none()
                && bordrless_game::StreakHeader::parse(&data).is_none()
        }
        GameKind::Jackpot => {
            bordrless_game::JackpotHeader::parse(&info.try_borrow_data()?).is_some()
        }
        GameKind::Streak => bordrless_game::StreakHeader::parse(&info.try_borrow_data()?).is_some(),
    };
    require!(kind_header, CompanionError::HookState);
    // A jackpot's or a streak's registry, as the launch carries it, still lists at most
    // `MAX_GAME_HOOK_EXTRAS_V2` extras (the hook may have rewritten it since `create_game_v2`).
    // A lottery's launch is phase 1's, unchanged.
    if c.game_kind != GameKind::Lottery {
        let registry = bordrless_hook::hook_accounts_address(&c.game_hook, &c.mint).0;
        let info = remaining
            .iter()
            .find(|a| *a.key == registry)
            .ok_or(CompanionError::MissingAccount)?;
        crate::instructions::game::check_hook_registry(
            info,
            &c.game_hook,
            &c.mint,
            MAX_GAME_HOOK_EXTRAS_V2,
        )?;
    }
    Ok(())
}
