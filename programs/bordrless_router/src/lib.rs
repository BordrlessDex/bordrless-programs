//! `bordrless_router`: an app's integrator fee on any Bordrless DEX swap (`docs/router.md`).
//!
//! An app (a terminal, a wallet, a bot) routes its users' swaps through `swap` and names its fee,
//! up to `MAX_FEE_BPS` of the trade's quote side, and its own holding of the quote. The router:
//!
//! - **a buy** (quote in): takes the fee from the trader's quote input first, then runs the DEX's
//!   `swap` with the rest, its accounts and arguments otherwise exactly as the app built them;
//! - **a sell** (quote out): runs the DEX's `swap`, measures what reached the trader's quote
//!   holding, takes the fee from that, and holds the rest to the app's own floor
//!   (`min_quote_after_fee`).
//!
//! Of every fee, `BORDRLESS_SHARE_BPS` (20%) goes to the DEX config's treasury (its holding of the
//! quote), the rest to the integrator. The DEX and the pool's hooks run exactly as they would
//! without the router: its fees, the launchpad's, every hook's. The fee is in the quote, whose mint
//! must have no token hook (bridged SOL), and a sell must deliver to the trader itself, the only
//! holding whose owner signed. One `IntegratorFeePaid` per swap.

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::program::invoke;
use anchor_lang::InstructionData;
use bordrless_swap::instructions::SwapArgs;
use bordrless_token::client as token_client;

declare_id!("7M5GTuzhByzBbrJURzLtbBrFjcsC8aHu816qWTXAYD85");

#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Bordrless router",
    project_url: "https://github.com/BordrlessDex/bordrless-programs",
    contacts: "link:https://github.com/BordrlessDex/bordrless-programs/security/advisories/new",
    policy: "https://github.com/BordrlessDex/bordrless-programs/blob/main/SECURITY.md",
    source_code: "https://github.com/BordrlessDex/bordrless-programs"
}

/// The most an integrator's fee may be: 1% of the trade's quote side.
pub const MAX_FEE_BPS: u16 = 100;
/// Bordrless's share of every integrator fee: 20% of it, to the DEX config's treasury.
pub const BORDRLESS_SHARE_BPS: u16 = 2_000;
/// Basis points.
pub const BPS: u64 = 10_000;
/// The DEX swap's accounts, as `bordrless_swap::client::swap` lists them: the trader (0), the
/// config (1), the pool (2), the base mint (3), the quote mint (4), and the trader's quote holding
/// (8); at least 11 before the token program's and the event accounts.
pub const SWAP_TRADER: usize = 0;
pub const SWAP_CONFIG: usize = 1;
pub const SWAP_POOL: usize = 2;
pub const SWAP_QUOTE_MINT: usize = 4;
pub const SWAP_TRADER_QUOTE: usize = 8;
pub const SWAP_MIN_ACCOUNTS: usize = 11;
/// The DEX `swap_v2`'s accounts, as `bordrless_swap::client::swap_v2` lists them: the trader (0),
/// the config (1), the pool (2), the quote mint (4) and the trader's quote account (7, its wallet);
/// at least 13 before the bridge's and the event accounts.
pub const SWAP_V2_TRADER_QUOTE: usize = 7;
pub const SWAP_V2_MIN_ACCOUNTS: usize = 13;

/// What a fee of `fee_bps` on `amount` comes to: the fee (rounded down) and Bordrless's share of it
/// (rounded down); the integrator gets the fee less the share.
pub fn fee_split(amount: u64, fee_bps: u16) -> (u64, u64) {
    let fee = u64::try_from(u128::from(amount) * u128::from(fee_bps) / u128::from(BPS))
        .unwrap_or(u64::MAX);
    let share = u64::try_from(u128::from(fee) * u128::from(BORDRLESS_SHARE_BPS) / u128::from(BPS))
        .unwrap_or(u64::MAX);
    (fee, share)
}

/// Arguments of `swap`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct RouterSwapArgs {
    /// The DEX swap's own arguments; a buy's `amount_in` is the trader's whole input, the fee
    /// included (the DEX swaps it less the fee).
    pub swap: SwapArgs,
    /// The integrator's fee: 1 to `MAX_FEE_BPS` basis points of the trade's quote side.
    pub fee_bps: u16,
    /// A sell's floor on what the trader keeps after the fee (the DEX's `min_amount_out` bounds
    /// what arrives before it); unused on a buy.
    pub min_quote_after_fee: u64,
}

#[program]
pub mod bordrless_router {
    use super::*;

    /// A DEX swap with an integrator's fee. Remaining accounts: the DEX swap's own, exactly as
    /// `bordrless_swap::client::swap` lists them (the trader signing).
    pub fn swap<'info>(ctx: Context<'info, RouterSwap<'info>>, args: RouterSwapArgs) -> Result<()> {
        process_swap(ctx, args)
    }

    /// `swap` on a native pool, through the DEX's `swap_v2`: the trader pays a buy's fee from its
    /// wallet, and a sell, delivered to the trader's wallet, pays it from what arrived; the
    /// integrator and the DEX treasury are paid SOL to their wallets. Remaining accounts: the DEX
    /// `swap_v2`'s own, exactly as `bordrless_swap::client::swap_v2` lists them.
    pub fn swap_v2<'info>(
        ctx: Context<'info, RouterSwapV2<'info>>,
        args: RouterSwapArgs,
    ) -> Result<()> {
        process_swap_v2(ctx, args)
    }
}

/// Accounts of `swap_v2`.
#[event_cpi]
#[derive(Accounts)]
pub struct RouterSwapV2<'info> {
    /// The trader: signs the DEX swap and pays the fee from its wallet.
    #[account(mut)]
    pub trader: Signer<'info>,
    /// CHECK: the integrator's wallet (any account but the trader and the pool; it must end at or
    /// above the rent-exempt minimum).
    #[account(mut)]
    pub integrator: UncheckedAccount<'info>,
    /// CHECK: the DEX config's treasury (address-checked in the handler).
    #[account(mut)]
    pub treasury: UncheckedAccount<'info>,
    /// CHECK: the DEX (address-checked).
    #[account(address = bordrless_swap::ID @ RouterError::WrongProgram)]
    pub dex_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// `amount` lamports from the trader's wallet to `to`, which must end at or above the rent-exempt
/// minimum of its size.
fn pay_sol<'info>(
    ctx: &Context<'info, RouterSwapV2<'info>>,
    to: &AccountInfo<'info>,
    amount: u64,
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    require!(
        to.lamports().saturating_add(amount) >= Rent::get()?.minimum_balance(to.data_len()),
        RouterError::RecipientBelowRent
    );
    anchor_lang::system_program::transfer(
        CpiContext::new(
            ctx.accounts.system_program.key(),
            anchor_lang::system_program::Transfer {
                from: ctx.accounts.trader.to_account_info(),
                to: to.clone(),
            },
        ),
        amount,
    )
}

fn process_swap_v2<'info>(
    ctx: Context<'info, RouterSwapV2<'info>>,
    args: RouterSwapArgs,
) -> Result<()> {
    require!(
        (1..=MAX_FEE_BPS).contains(&args.fee_bps),
        RouterError::BadFee
    );
    let swap = ctx.remaining_accounts;
    require!(
        swap.len() >= SWAP_V2_MIN_ACCOUNTS,
        RouterError::SwapAccounts
    );
    let trader = ctx.accounts.trader.key();
    require!(
        *swap[SWAP_TRADER].key == trader && swap[SWAP_TRADER].is_signer,
        RouterError::SwapAccounts
    );
    require_keys_eq!(
        *swap[SWAP_CONFIG].key,
        bordrless_swap::client::config_address(),
        RouterError::SwapAccounts
    );
    let config = {
        let info = &swap[SWAP_CONFIG];
        require_keys_eq!(*info.owner, bordrless_swap::ID, RouterError::SwapAccounts);
        let data = info.try_borrow_data()?;
        bordrless_swap::state::Config::try_deserialize(&mut &data[..])?
    };
    // A sell delivers to the trader's own wallet (the fee moves from it).
    require_keys_eq!(
        *swap[SWAP_V2_TRADER_QUOTE].key,
        trader,
        RouterError::NotTradersHolding
    );
    require_keys_eq!(
        ctx.accounts.treasury.key(),
        config.treasury,
        RouterError::WrongTreasury
    );
    require!(
        ctx.accounts.integrator.key() != trader
            && ctx.accounts.integrator.key() != swap[SWAP_POOL].key(),
        RouterError::WrongIntegratorHolding
    );

    let buy = args.swap.direction == 1;
    let mut dex_args = args.swap.clone();
    let integrator = ctx.accounts.integrator.to_account_info();
    let treasury = ctx.accounts.treasury.to_account_info();
    let (charged_on, fee, share) = if buy {
        let (fee, share) = fee_split(args.swap.amount_in, args.fee_bps);
        dex_args.amount_in = args
            .swap
            .amount_in
            .checked_sub(fee)
            .filter(|a| *a > 0)
            .ok_or(RouterError::BadFee)?;
        pay_sol(&ctx, &integrator, fee - share)?;
        pay_sol(&ctx, &treasury, share)?;
        dex_v2(&ctx, swap, dex_args)?;
        (args.swap.amount_in, fee, share)
    } else {
        let before = ctx.accounts.trader.lamports();
        dex_v2(&ctx, swap, dex_args)?;
        let delivered = ctx
            .accounts
            .trader
            .lamports()
            .checked_sub(before)
            .ok_or(RouterError::SwapAccounts)?;
        let (fee, share) = fee_split(delivered, args.fee_bps);
        require!(
            delivered - fee >= args.min_quote_after_fee,
            RouterError::Slippage
        );
        pay_sol(&ctx, &integrator, fee - share)?;
        pay_sol(&ctx, &treasury, share)?;
        (delivered, fee, share)
    };
    emit_cpi!(IntegratorFeePaid {
        pool: swap[SWAP_POOL].key(),
        trader,
        integrator: integrator.key(),
        direction: args.swap.direction,
        charged_on,
        fee,
        bordrless_share: share,
    });
    Ok(())
}

/// The DEX's `swap_v2` with `args`, its accounts exactly as passed.
fn dex_v2<'info>(
    ctx: &Context<'info, RouterSwapV2<'info>>,
    swap: &[AccountInfo<'info>],
    args: SwapArgs,
) -> Result<()> {
    let ix = Instruction {
        program_id: bordrless_swap::ID,
        accounts: swap
            .iter()
            .map(|a| AccountMeta {
                pubkey: *a.key,
                is_signer: a.is_signer,
                is_writable: a.is_writable,
            })
            .collect(),
        data: bordrless_swap::instruction::SwapV2 { args }.data(),
    };
    let mut infos = swap.to_vec();
    infos.push(ctx.accounts.dex_program.to_account_info());
    invoke(&ix, &infos)?;
    Ok(())
}

/// Accounts of `swap`.
#[event_cpi]
#[derive(Accounts)]
pub struct RouterSwap<'info> {
    /// The trader: signs the DEX swap and the fee's transfer.
    pub trader: Signer<'info>,
    /// CHECK: the integrator's holding of the pool's quote (checked: a holding of that mint).
    #[account(mut)]
    pub integrator_quote: UncheckedAccount<'info>,
    /// CHECK: the DEX config's treasury's holding of the quote (address-checked).
    #[account(mut)]
    pub treasury_quote: UncheckedAccount<'info>,
    /// CHECK: the DEX (address-checked).
    #[account(address = bordrless_swap::ID @ RouterError::WrongProgram)]
    pub dex_program: UncheckedAccount<'info>,
    /// CHECK: the token program (address-checked).
    #[account(address = bordrless_token::ID @ RouterError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority (address-checked).
    #[account(address = token_client::event_authority() @ RouterError::WrongProgram)]
    pub token_event_authority: UncheckedAccount<'info>,
}

fn balance(info: &AccountInfo) -> Result<u64> {
    Ok(token_client::read_holding(info)?.amount)
}

/// `amount` of the quote from `from` (the trader's holding, the trader signing) to `to`.
fn pay<'info>(
    ctx: &Context<'info, RouterSwap<'info>>,
    quote_mint: &AccountInfo<'info>,
    from: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    amount: u64,
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    let ix = token_client::transfer(
        ctx.accounts.trader.key(),
        from.key(),
        to.key(),
        quote_mint.key(),
        None,
        vec![],
        amount,
    );
    let token = ctx.accounts.token_program.to_account_info();
    invoke(
        &ix,
        &[
            ctx.accounts.trader.to_account_info(),
            from.clone(),
            to.clone(),
            quote_mint.clone(),
            token.clone(),
            ctx.accounts.token_event_authority.to_account_info(),
            token,
        ],
    )?;
    Ok(())
}

fn process_swap<'info>(ctx: Context<'info, RouterSwap<'info>>, args: RouterSwapArgs) -> Result<()> {
    require!(
        (1..=MAX_FEE_BPS).contains(&args.fee_bps),
        RouterError::BadFee
    );
    let swap = ctx.remaining_accounts;
    require!(swap.len() >= SWAP_MIN_ACCOUNTS, RouterError::SwapAccounts);
    let trader = ctx.accounts.trader.key();
    // The DEX's trader is the router's, signing; its config, its pool, the pool's quote mint.
    require!(
        *swap[SWAP_TRADER].key == trader && swap[SWAP_TRADER].is_signer,
        RouterError::SwapAccounts
    );
    require_keys_eq!(
        *swap[SWAP_CONFIG].key,
        bordrless_swap::client::config_address(),
        RouterError::SwapAccounts
    );
    let config = {
        let info = &swap[SWAP_CONFIG];
        require_keys_eq!(*info.owner, bordrless_swap::ID, RouterError::SwapAccounts);
        let data = info.try_borrow_data()?;
        bordrless_swap::state::Config::try_deserialize(&mut &data[..])?
    };
    let pool = bordrless_swap::client::read_pool(&swap[SWAP_POOL])?;
    let quote_mint = &swap[SWAP_QUOTE_MINT];
    require_keys_eq!(*quote_mint.key, pool.quote_mint, RouterError::SwapAccounts);
    require!(
        token_client::read_mint(quote_mint)?.hook_program.is_none(),
        RouterError::QuoteHasHook
    );
    // The fee moves through the trader's own quote holding: a buy's input, a sell's delivery (so a
    // sell delivers to the trader, whose signature moves the fee).
    let trader_quote = &swap[SWAP_TRADER_QUOTE];
    require_keys_eq!(
        *trader_quote.key,
        token_client::holding_address(quote_mint.key, &trader),
        RouterError::NotTradersHolding
    );
    require_keys_eq!(
        ctx.accounts.treasury_quote.key(),
        token_client::holding_address(quote_mint.key, &config.treasury),
        RouterError::WrongTreasury
    );
    let integrator = token_client::read_holding(&ctx.accounts.integrator_quote)?;
    require_keys_eq!(
        integrator.mint,
        pool.quote_mint,
        RouterError::WrongIntegratorHolding
    );
    require!(
        ctx.accounts.integrator_quote.key() != trader_quote.key()
            && integrator.owner != swap[SWAP_POOL].key(),
        RouterError::WrongIntegratorHolding
    );

    let buy = args.swap.direction == 1;
    let mut dex_args = args.swap.clone();
    let integrator_quote = ctx.accounts.integrator_quote.to_account_info();
    let treasury_quote = ctx.accounts.treasury_quote.to_account_info();
    let (charged_on, fee, share) = if buy {
        let (fee, share) = fee_split(args.swap.amount_in, args.fee_bps);
        dex_args.amount_in = args
            .swap
            .amount_in
            .checked_sub(fee)
            .filter(|a| *a > 0)
            .ok_or(RouterError::BadFee)?;
        pay(
            &ctx,
            quote_mint,
            trader_quote,
            &integrator_quote,
            fee - share,
        )?;
        pay(&ctx, quote_mint, trader_quote, &treasury_quote, share)?;
        dex(&ctx, swap, dex_args)?;
        (args.swap.amount_in, fee, share)
    } else {
        let before = balance(trader_quote)?;
        dex(&ctx, swap, dex_args)?;
        let delivered = balance(trader_quote)?
            .checked_sub(before)
            .ok_or(RouterError::SwapAccounts)?;
        let (fee, share) = fee_split(delivered, args.fee_bps);
        require!(
            delivered - fee >= args.min_quote_after_fee,
            RouterError::Slippage
        );
        pay(
            &ctx,
            quote_mint,
            trader_quote,
            &integrator_quote,
            fee - share,
        )?;
        pay(&ctx, quote_mint, trader_quote, &treasury_quote, share)?;
        (delivered, fee, share)
    };
    emit_cpi!(IntegratorFeePaid {
        pool: swap[SWAP_POOL].key(),
        trader,
        integrator: integrator.owner,
        direction: args.swap.direction,
        charged_on,
        fee,
        bordrless_share: share,
    });
    Ok(())
}

/// The DEX's `swap` with `args`, its accounts exactly as passed (the trader's signature with them).
fn dex<'info>(
    ctx: &Context<'info, RouterSwap<'info>>,
    swap: &[AccountInfo<'info>],
    args: SwapArgs,
) -> Result<()> {
    let ix = Instruction {
        program_id: bordrless_swap::ID,
        accounts: swap
            .iter()
            .map(|a| AccountMeta {
                pubkey: *a.key,
                is_signer: a.is_signer,
                is_writable: a.is_writable,
            })
            .collect(),
        data: bordrless_swap::instruction::Swap { args }.data(),
    };
    let mut infos = swap.to_vec();
    infos.push(ctx.accounts.dex_program.to_account_info());
    invoke(&ix, &infos)?;
    Ok(())
}

/// Instruction builders.
pub mod client {
    use super::*;
    use anchor_lang::ToAccountMetas;

    /// `swap_v2`: `dex_swap_v2` (built by `bordrless_swap::client::swap_v2`, the trader's wallet its
    /// quote account) routed with an integrator's fee of `fee_bps`, paid in SOL to `integrator`
    /// (a wallet) and `treasury` (the DEX config's treasury).
    pub fn swap_v2(
        trader: Pubkey,
        integrator: Pubkey,
        treasury: Pubkey,
        dex_swap_v2: &Instruction,
        fee_bps: u16,
        min_quote_after_fee: u64,
    ) -> Instruction {
        let args = bordrless_swap::instruction::SwapV2::try_from_slice(&dex_swap_v2.data[8..])
            .expect("a DEX swap_v2's data")
            .args;
        let mut accounts = crate::accounts::RouterSwapV2 {
            trader,
            integrator,
            treasury,
            dex_program: bordrless_swap::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: event_authority(),
            program: crate::ID,
        }
        .to_account_metas(None);
        accounts.extend(dex_swap_v2.accounts.iter().cloned());
        Instruction {
            program_id: crate::ID,
            accounts,
            data: crate::instruction::SwapV2 {
                args: RouterSwapArgs {
                    swap: args,
                    fee_bps,
                    min_quote_after_fee,
                },
            }
            .data(),
        }
    }

    /// This program's event authority.
    pub fn event_authority() -> Pubkey {
        Pubkey::find_program_address(&[b"__event_authority"], &crate::ID).0
    }

    /// `swap`: `dex_swap` (built by `bordrless_swap::client::swap`, or any client that builds the
    /// same accounts) routed with an integrator's fee of `fee_bps`, paid to `integrator_quote` (the
    /// integrator's holding of the pool's quote) and `treasury_quote` (the DEX treasury's).
    pub fn swap(
        trader: Pubkey,
        integrator_quote: Pubkey,
        treasury_quote: Pubkey,
        dex_swap: &Instruction,
        fee_bps: u16,
        min_quote_after_fee: u64,
    ) -> Instruction {
        let args = bordrless_swap::instruction::Swap::try_from_slice(&dex_swap.data[8..])
            .expect("a DEX swap's data")
            .args;
        let mut accounts = crate::accounts::RouterSwap {
            trader,
            integrator_quote,
            treasury_quote,
            dex_program: bordrless_swap::ID,
            token_program: bordrless_token::ID,
            token_event_authority: token_client::event_authority(),
            event_authority: event_authority(),
            program: crate::ID,
        }
        .to_account_metas(None);
        accounts.extend(dex_swap.accounts.iter().cloned());
        Instruction {
            program_id: crate::ID,
            accounts,
            data: crate::instruction::Swap {
                args: RouterSwapArgs {
                    swap: args,
                    fee_bps,
                    min_quote_after_fee,
                },
            }
            .data(),
        }
    }
}

/// A routed swap paid an integrator's fee.
#[event]
pub struct IntegratorFeePaid {
    pub pool: Pubkey,
    pub trader: Pubkey,
    /// The owner of the integrator's holding paid.
    pub integrator: Pubkey,
    /// 0 a sell, 1 a buy.
    pub direction: u8,
    /// What the fee was taken of: a buy's whole input, a sell's delivery.
    pub charged_on: u64,
    /// The whole fee, and Bordrless's share of it (the integrator got the rest).
    pub fee: u64,
    pub bordrless_share: u64,
}

#[error_code]
pub enum RouterError {
    #[msg("an integrator fee is 1 to 100 basis points, and a buy must keep some input to swap")]
    BadFee,
    #[msg("the remaining accounts must be the DEX swap's own, the router's trader signing first")]
    SwapAccounts,
    #[msg("the pool's quote mint has a token hook: the router takes fees in a quote without one")]
    QuoteHasHook,
    #[msg(
        "the swap's quote holding must be the trader's own: a routed sell delivers to the trader"
    )]
    NotTradersHolding,
    #[msg("the treasury holding must be the DEX config's treasury's holding of the quote")]
    WrongTreasury,
    #[msg("the integrator's holding must be a holding of the pool's quote, not the trader's nor the pool's")]
    WrongIntegratorHolding,
    #[msg("what the trader keeps after the fee is under its floor")]
    Slippage,
    #[msg("a program account is not the expected program")]
    WrongProgram,
    #[msg("a fee recipient would be left below the rent-exempt minimum")]
    RecipientBelowRent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fee_and_the_share_round_down() {
        assert_eq!(fee_split(1_000_000_000, 100), (10_000_000, 2_000_000));
        assert_eq!(fee_split(1_000_000_000, 50), (5_000_000, 1_000_000));
        assert_eq!(fee_split(99, 100), (0, 0));
        assert_eq!(fee_split(100, 100), (1, 0));
        assert_eq!(
            fee_split(u64::MAX, 100),
            (u64::MAX / 100, u64::MAX / 100 / 5)
        );
    }
}
