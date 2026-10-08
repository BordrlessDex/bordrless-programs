//! Pure math and policy of the Bordrless programs. No dependencies, `no_std`, integer only, so the
//! programs, their LiteSVM tests and the TypeScript mirror (`packages/shared/src/policy.ts`) compute
//! the same numbers bit for bit. Every function answers `None` on overflow or an impossible input
//! ([`swap_amounts`] answers why, as a [`SwapFailure`]); the programs turn that into an error.

#![no_std]

/// Basis points in one.
pub const BPS: u64 = 10_000;

/// Policy of a launch. Numbers shown on the site come from the same constants in `packages/shared`.
pub mod policy {
    /// Decimals of a launched token.
    pub const TOKEN_DECIMALS: u8 = 6;
    /// Supply of a launched token, in base units: one billion whole tokens.
    pub const TOKEN_SUPPLY: u64 = 1_000_000_000 * 1_000_000;
    /// Share of the supply sold on the curve, in basis points; the rest is reserved for graduation.
    pub const CURVE_BPS: u64 = 7_500;
    /// LP fee of a launch pool: stays in the pool for ever.
    pub const LP_FEE_BPS: u16 = 30;
    /// Protocol fee of an ordinary DEX pool (anyone's pool), collected by the admin: a flat rate
    /// of the quote.
    pub const PROTOCOL_FEE_BPS: u16 = 100;
    /// Bordrless's share of what a launch's rules collect on each swap of a launch pool (its curve
    /// and the same pool after graduation): a quarter of the hooks' cuts, taken in the quote. A
    /// launch whose rules collect nothing pays nothing.
    pub const LAUNCH_PROTOCOL_SHARE_BPS: u16 = 2_500;
    /// Largest creator fee a launch may choose.
    pub const MAX_CREATOR_FEE_BPS: u16 = 200;
    /// Seconds after creation during which the LP fee is elevated against snipers.
    pub const SNIPER_WINDOW_SECS: i64 = 30;
    /// LP fee at the first second of the sniper window, falling linearly to [`LP_FEE_BPS`].
    pub const SNIPER_START_BPS: u16 = 8_000;
    /// Paid by the creator to the treasury at launch.
    pub const LAUNCH_FEE_LAMPORTS: u64 = 10_000_000;
    /// Bounds of the virtual quote reserve a launch may open with, in quote base units (the quote
    /// is bridged SOL, 9 decimals): 1 SOL to 10,000 SOL.
    pub const MIN_VIRTUAL_QUOTE: u64 = 1_000_000_000;
    /// See [`MIN_VIRTUAL_QUOTE`].
    pub const MAX_VIRTUAL_QUOTE: u64 = 10_000_000_000_000;
    /// LP units kept by the pool on the first deposit, never held by anyone, so a tiny first
    /// deposit cannot be used to make later shares unfair.
    pub const MINIMUM_LIQUIDITY: u64 = 1_000;
    /// Decimals of an LP mint.
    pub const LP_DECIMALS: u8 = 9;

    // Token rules (`docs/hooks-v2.md` §5.2): the bounds every launch config is set to. A config
    // may set others, up to the launch program's hard ceilings.

    /// Largest holder fee per side.
    pub const MAX_HOLDER_FEE_BPS: u16 = 200;
    /// Largest burn per side.
    pub const MAX_BURN_BPS: u16 = 100;
    /// Largest creator fee + holder fee + burn on one side.
    pub const MAX_RULES_FEE_BPS: u16 = 300;
    /// Smallest max wallet (when on).
    pub const MIN_MAX_WALLET_BPS: u16 = 100;
    /// Largest max wallet.
    pub const MAX_MAX_WALLET_BPS: u16 = 500;
    /// Longest creator wallet lock: 90 days.
    pub const MAX_CREATOR_LOCK_SECS: u32 = 90 * 86_400;
    /// Longest early-buyer window.
    pub const MAX_EARLY_WINDOW_SECS: u32 = 300;
    /// Latest early-buyer unlock, counted from the launch: 7 days.
    pub const MAX_EARLY_LOCK_SECS: u32 = 7 * 86_400;

    /// The presets of the launch form (`docs/hooks-v2.md` §7.1): the four rule sets the site
    /// offers ready-made. Beside them the form offers "Custom" (the same rules, mixed by hand)
    /// and "Build your own" (a `LaunchConfig` made with the SDK, pasted by its key). The
    /// TypeScript `RULE_PRESETS` is pinned to this list through the fee vectors.
    pub mod presets {
        /// One preset: a name, a slug, the creator fee and the token rules.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct Preset {
            /// As shown on the site.
            pub name: &'static str,
            /// As used in URLs and the API.
            pub slug: &'static str,
            /// Creator fee, basis points of the quote.
            pub creator_fee_bps: u16,
            /// Holder fee on buys.
            pub holder_fee_buy_bps: u16,
            /// Holder fee on sells.
            pub holder_fee_sell_bps: u16,
            /// Burn on buys.
            pub burn_buy_bps: u16,
            /// Burn on sells.
            pub burn_sell_bps: u16,
            /// Max wallet, basis points of the supply; 0 = off.
            pub max_wallet_bps: u16,
            /// Creator wallet lock, seconds from the launch; 0 = off.
            pub creator_lock_secs: u32,
            /// Early-buyer window, seconds from the launch; 0 = off.
            pub early_window_secs: u32,
            /// Early-buyer unlock, seconds from the launch; 0 when the lock is off.
            pub early_lock_secs: u32,
        }

        const DAY: u32 = 86_400;

        /// No token rules; creator fee 1%. The default.
        pub const PLAIN: Preset = Preset {
            name: "Plain",
            slug: "plain",
            creator_fee_bps: 100,
            holder_fee_buy_bps: 0,
            holder_fee_sell_bps: 0,
            burn_buy_bps: 0,
            burn_sell_bps: 0,
            max_wallet_bps: 0,
            creator_lock_secs: 0,
            early_window_secs: 0,
            early_lock_secs: 0,
        };
        /// Early-buyer lock (the first 5 min, until 24 h after launch), max wallet 1%, creator
        /// wallet lock 90 days, holder rewards 1% both sides, creator fee 0.5%.
        pub const DIAMOND_HANDS: Preset = Preset {
            name: "Diamond hands",
            slug: "diamond-hands",
            creator_fee_bps: 50,
            holder_fee_buy_bps: 100,
            holder_fee_sell_bps: 100,
            burn_buy_bps: 0,
            burn_sell_bps: 0,
            max_wallet_bps: 100,
            creator_lock_secs: 90 * DAY,
            early_window_secs: 300,
            early_lock_secs: DAY,
        };
        /// Burn 0.5% both sides, holder rewards 0.5% both sides, creator fee 0.5%.
        pub const BURN: Preset = Preset {
            name: "Burn",
            slug: "burn",
            creator_fee_bps: 50,
            holder_fee_buy_bps: 50,
            holder_fee_sell_bps: 50,
            burn_buy_bps: 50,
            burn_sell_bps: 50,
            max_wallet_bps: 0,
            creator_lock_secs: 0,
            early_window_secs: 0,
            early_lock_secs: 0,
        };
        /// Holder rewards 2% on sells only, creator wallet lock 30 days, creator fee 0.5%.
        pub const PAID_TO_HOLD: Preset = Preset {
            name: "Paid to hold",
            slug: "paid-to-hold",
            creator_fee_bps: 50,
            holder_fee_buy_bps: 0,
            holder_fee_sell_bps: 200,
            burn_buy_bps: 0,
            burn_sell_bps: 0,
            max_wallet_bps: 0,
            creator_lock_secs: 30 * DAY,
            early_window_secs: 0,
            early_lock_secs: 0,
        };
        /// Every preset, in the order the site shows them.
        pub const ALL: [Preset; 4] = [PLAIN, DIAMOND_HANDS, BURN, PAID_TO_HOLD];
        /// The preset the launch form starts on.
        pub const DEFAULT: Preset = PLAIN;
    }
}

/// `a * b / d`, rounded down.
pub fn mul_div_floor(a: u128, b: u128, d: u128) -> Option<u128> {
    if d == 0 {
        return None;
    }
    a.checked_mul(b)?.checked_div(d)
}

/// `a * b / d`, rounded up.
pub fn mul_div_ceil(a: u128, b: u128, d: u128) -> Option<u128> {
    if d == 0 {
        return None;
    }
    let p = a.checked_mul(b)?;
    let q = p / d;
    Some(if p % d == 0 { q } else { q.checked_add(1)? })
}

/// A fee of `bps` on `amount`, rounded up (the pool never undercharges).
pub fn fee_amount(amount: u64, bps: u16) -> Option<u64> {
    let fee = mul_div_ceil(u128::from(amount), u128::from(bps), u128::from(BPS))?;
    u64::try_from(fee).ok()
}

/// Integer square root of `n` (the largest `r` with `r * r <= n`).
pub fn isqrt(n: u128) -> u128 {
    if n < 2 {
        return n;
    }
    // Newton's method from a power-of-two estimate above the root.
    let mut x = 1u128 << ((128 - n.leading_zeros()).div_ceil(2));
    loop {
        let y = (x + n / x) / 2;
        if y >= x {
            return x;
        }
        x = y;
    }
}

/// The constant-product output of `amount_in` (after fees) against effective reserves
/// `reserve_in + virtual_in` and `reserve_out + virtual_out`, rounded down. `None` if nothing comes
/// out, if the output would exceed the real `reserve_out`, or on overflow.
pub fn swap_out(
    amount_in: u64,
    reserve_in: u64,
    virtual_in: u64,
    reserve_out: u64,
    virtual_out: u64,
) -> Option<u64> {
    if amount_in == 0 {
        return None;
    }
    let x = u128::from(reserve_in).checked_add(u128::from(virtual_in))?;
    let y = u128::from(reserve_out).checked_add(u128::from(virtual_out))?;
    let dx = u128::from(amount_in);
    let out = mul_div_floor(dx, y, x.checked_add(dx)?)?;
    let out = u64::try_from(out).ok()?;
    if out == 0 || out > reserve_out {
        return None;
    }
    Some(out)
}

/// A pool's reserves as its curve sees them: the real reserves and the virtual offsets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reserves {
    /// Real base reserve.
    pub base_reserve: u64,
    /// Real quote reserve.
    pub quote_reserve: u64,
    /// Virtual base offset.
    pub virtual_base: u64,
    /// Virtual quote offset.
    pub virtual_quote: u64,
}

/// The fees and the curve of one swap, in the order of hook protocol v2 (`docs/hooks-v2.md` §3.1).
/// The LP fee is on the input in both directions and stays in the pool. The protocol fee is always
/// in the quote token and kept apart from the reserves: a buy's comes from what reached the vault,
/// before the curve; a sell's from the curve's output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapAmounts {
    /// The LP fee, in the input token: `fee_amount(received, lp_fee_bps)`.
    pub lp_fee: u64,
    /// The protocol fee, in the quote token: `fee_amount(received, protocol_fee_bps)` on a buy,
    /// `fee_amount(out_gross, protocol_fee_bps)` on a sell.
    pub protocol_fee: u64,
    /// What goes into the curve: what reached the vault less the LP fee and, on a buy, the
    /// protocol fee.
    pub net_in: u64,
    /// What the curve gives: it leaves the output reserve.
    pub out_gross: u64,
    /// What the output side hands on: `out_gross`, less a sell's protocol fee. The pool's
    /// `after_swap` hook is told this amount; its deltas and burn come out of it and the rest is
    /// delivered.
    pub amount_out: u64,
    /// What enters the input reserve: what reached the vault, less a buy's protocol fee.
    pub to_reserve_in: u64,
}

/// Why [`swap_amounts`] refuses a swap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwapFailure {
    /// The LP fee (and, on a buy, the protocol fee) take the whole input.
    FeesExceedInput,
    /// The curve gives nothing, or more than the real output reserve.
    InsufficientLiquidity,
    /// A sell's protocol fee takes the whole output.
    FeeExceedsOutput,
}

/// The [`SwapAmounts`] of a swap whose input vault received `received`, on a pool with `reserves`.
/// `buy` is quote in, base out; a sell is base in, quote out.
pub fn swap_amounts(
    buy: bool,
    received: u64,
    lp_fee_bps: u16,
    protocol_fee_bps: u16,
    reserves: &Reserves,
) -> Result<SwapAmounts, SwapFailure> {
    // A fee that does not fit a u64 is larger than the amount it is taken from.
    let lp_fee = fee_amount(received, lp_fee_bps).ok_or(SwapFailure::FeesExceedInput)?;
    let protocol_in = if buy {
        fee_amount(received, protocol_fee_bps).ok_or(SwapFailure::FeesExceedInput)?
    } else {
        0
    };
    let net_in = received
        .checked_sub(lp_fee)
        .and_then(|rest| rest.checked_sub(protocol_in))
        .filter(|net| *net > 0)
        .ok_or(SwapFailure::FeesExceedInput)?;
    let r = reserves;
    let out_gross = if buy {
        swap_out(
            net_in,
            r.quote_reserve,
            r.virtual_quote,
            r.base_reserve,
            r.virtual_base,
        )
    } else {
        swap_out(
            net_in,
            r.base_reserve,
            r.virtual_base,
            r.quote_reserve,
            r.virtual_quote,
        )
    }
    .ok_or(SwapFailure::InsufficientLiquidity)?;
    let (protocol_fee, amount_out) = if buy {
        (protocol_in, out_gross)
    } else {
        let fee = fee_amount(out_gross, protocol_fee_bps).ok_or(SwapFailure::FeeExceedsOutput)?;
        let rest = out_gross
            .checked_sub(fee)
            .filter(|rest| *rest > 0)
            .ok_or(SwapFailure::FeeExceedsOutput)?;
        (fee, rest)
    };
    Ok(SwapAmounts {
        lp_fee,
        protocol_fee,
        net_in,
        out_gross,
        amount_out,
        to_reserve_in: received - protocol_in,
    })
}

/// The quote value of `base` at a swap's own price, rounded up: the swap moved `quote_leg` of the
/// quote against `base_leg` of the base through the curve (a buy: `net_in` against `out_gross`; a
/// sell: `out_gross` against `net_in`). `None` on overflow, or when `base_leg` is zero and `base`
/// is not.
pub fn quote_value(base: u64, quote_leg: u64, base_leg: u64) -> Option<u64> {
    if base == 0 {
        return Some(0);
    }
    u64::try_from(mul_div_ceil(
        u128::from(base),
        u128::from(quote_leg),
        u128::from(base_leg),
    )?)
    .ok()
}

/// Bordrless's share of `value` (the quote value of what a launch pool's hooks cut from one side of
/// a swap): `share_bps` of it, rounded up, never more than `value` (a share is at most [`BPS`]).
pub fn protocol_share(value: u64, share_bps: u16) -> Option<u64> {
    if u64::from(share_bps) > BPS {
        return None;
    }
    fee_amount(value, share_bps)
}

/// The fees and the curve of one swap on a pool under the share model (`docs/hooks-v2.md` §3.1):
/// the DEX takes `share_bps` of what the hooks cut, in the quote, instead of a flat rate. `cuts_in`
/// is the input side's cut as the DEX measured it (`amount_in - burn_in - received`): on a buy it
/// is quote, and its share leaves what reached the vault before the curve; on a sell it is base,
/// valued at the swap's own price (`quote_value(cuts_in, out_gross, net_in)`) and its share leaves
/// the curve's output before the `after_swap` hook is told. `SwapAmounts.protocol_fee` is this
/// input-side share only; the output side's share is [`output_share`], known after the hook has
/// answered.
///
/// The LP fee is Bordrless's too under the share model, in the quote (the launch's LP tokens are
/// locked for ever, so a fee left in the pool would go to nobody): on a buy it leaves what reached
/// the vault before the curve, on a sell it leaves the curve's output (`fee_amount(out_gross,
/// lp_fee_bps)`), and the base a sell brings goes into the curve whole. Its rate is the pool's, or
/// the pool hook's override (a launch's sniper fee). It is in `protocol_fee`, and `lp_fee` is 0:
/// nothing stays in the pool for its LPs.
pub fn swap_amounts_shared(
    buy: bool,
    received: u64,
    lp_fee_bps: u16,
    share_bps: u16,
    cuts_in: u64,
    reserves: &Reserves,
) -> Result<SwapAmounts, SwapFailure> {
    let r = reserves;
    if buy {
        // The quote that reached the vault: the LP fee and the share of the input's cuts leave it
        // for Bordrless, the rest goes into the curve and the reserve.
        let lp_fee = fee_amount(received, lp_fee_bps).ok_or(SwapFailure::FeesExceedInput)?;
        let share = protocol_share(cuts_in, share_bps).ok_or(SwapFailure::FeesExceedInput)?;
        let net_in = received
            .checked_sub(lp_fee)
            .and_then(|rest| rest.checked_sub(share))
            .filter(|net| *net > 0)
            .ok_or(SwapFailure::FeesExceedInput)?;
        let out_gross = swap_out(
            net_in,
            r.quote_reserve,
            r.virtual_quote,
            r.base_reserve,
            r.virtual_base,
        )
        .ok_or(SwapFailure::InsufficientLiquidity)?;
        Ok(SwapAmounts {
            lp_fee: 0,
            protocol_fee: lp_fee + share,
            net_in,
            out_gross,
            amount_out: out_gross,
            to_reserve_in: net_in,
        })
    } else {
        // The base that reached the vault goes into the curve whole; the LP fee and the share of
        // the input's cuts (valued at the swap's own price) leave the quote it gives.
        let net_in = Some(received)
            .filter(|net| *net > 0)
            .ok_or(SwapFailure::FeesExceedInput)?;
        let out_gross = swap_out(
            net_in,
            r.base_reserve,
            r.virtual_base,
            r.quote_reserve,
            r.virtual_quote,
        )
        .ok_or(SwapFailure::InsufficientLiquidity)?;
        let lp_fee = fee_amount(out_gross, lp_fee_bps).ok_or(SwapFailure::FeeExceedsOutput)?;
        let value = quote_value(cuts_in, out_gross, net_in).ok_or(SwapFailure::FeeExceedsOutput)?;
        let share = protocol_share(value, share_bps).ok_or(SwapFailure::FeeExceedsOutput)?;
        let fee = lp_fee
            .checked_add(share)
            .ok_or(SwapFailure::FeeExceedsOutput)?;
        let amount_out = out_gross
            .checked_sub(fee)
            .filter(|rest| *rest > 0)
            .ok_or(SwapFailure::FeeExceedsOutput)?;
        Ok(SwapAmounts {
            lp_fee: 0,
            protocol_fee: fee,
            net_in,
            out_gross,
            amount_out,
            to_reserve_in: received,
        })
    }
}

/// The output side's share under the share model: `share_bps` of `cuts_out`, what the hooks cut
/// from the output. On a buy the output is base, so the cut is valued at the swap's own price
/// (`quote_value(cuts_out, net_in, out_gross)`) and the share is set aside from the quote reserve
/// after the swap; on a sell the output is quote and the share is held back from the delivery.
/// `None` on overflow.
pub fn output_share(
    buy: bool,
    share_bps: u16,
    cuts_out: u64,
    net_in: u64,
    out_gross: u64,
) -> Option<u64> {
    let value = if buy {
        quote_value(cuts_out, net_in, out_gross)?
    } else {
        cuts_out
    };
    protocol_share(value, share_bps)
}

/// The input (after fees) that buys exactly `amount_out`, rounded up. `None` if `amount_out` is not
/// below the real `reserve_out`.
pub fn swap_in_for_out(
    amount_out: u64,
    reserve_in: u64,
    virtual_in: u64,
    reserve_out: u64,
    virtual_out: u64,
) -> Option<u64> {
    if amount_out == 0 || amount_out > reserve_out {
        return None;
    }
    let x = u128::from(reserve_in).checked_add(u128::from(virtual_in))?;
    let y = u128::from(reserve_out).checked_add(u128::from(virtual_out))?;
    let dy = u128::from(amount_out);
    let dx = mul_div_ceil(dy, x, y.checked_sub(dy)?)?;
    u64::try_from(dx).ok()
}

/// The largest input (after fees) a pool can take before its real `reserve_out` is exhausted: the
/// `dx` at which the output equals `reserve_out` (a curve pool's remaining tokens). `None` when the
/// pool has nothing to sell.
pub fn max_swap_in(
    reserve_in: u64,
    virtual_in: u64,
    reserve_out: u64,
    virtual_out: u64,
) -> Option<u64> {
    swap_in_for_out(
        reserve_out,
        reserve_in,
        virtual_in,
        reserve_out,
        virtual_out,
    )
}

/// The spot price in quote base units per base base unit, scaled by `WAD` (1e18), from the
/// effective reserves.
pub const WAD: u128 = 1_000_000_000_000_000_000;

/// See [`WAD`].
pub fn spot_price_wad(
    base_reserve: u64,
    virtual_base: u64,
    quote_reserve: u64,
    virtual_quote: u64,
) -> Option<u128> {
    let x = u128::from(base_reserve).checked_add(u128::from(virtual_base))?;
    let y = u128::from(quote_reserve).checked_add(u128::from(virtual_quote))?;
    mul_div_floor(y, WAD, x)
}

/// LP minted by the first deposit: `sqrt(base * quote)`, of which [`policy::MINIMUM_LIQUIDITY`] stays
/// with the pool. Answers `(to_depositor, total_supply)`.
pub fn initial_lp(base: u64, quote: u64) -> Option<(u64, u64)> {
    let total = u64::try_from(isqrt(u128::from(base).checked_mul(u128::from(quote))?)).ok()?;
    let to_depositor = total.checked_sub(policy::MINIMUM_LIQUIDITY)?;
    if to_depositor == 0 {
        return None;
    }
    Some((to_depositor, total))
}

/// A later deposit: given what the depositor offers and the pool's reserves, the amounts actually
/// taken (at the pool's ratio) and the LP minted (rounded down). `None` when a side is zero.
pub fn lp_for_deposit(
    base_desired: u64,
    quote_desired: u64,
    base_reserve: u64,
    quote_reserve: u64,
    lp_supply: u64,
) -> Option<(u64, u64, u64)> {
    if base_desired == 0
        || quote_desired == 0
        || base_reserve == 0
        || quote_reserve == 0
        || lp_supply == 0
    {
        return None;
    }
    let (br, qr) = (u128::from(base_reserve), u128::from(quote_reserve));
    // The quote the offered base is worth at the pool's ratio.
    let quote_for_base = mul_div_ceil(u128::from(base_desired), qr, br)?;
    let (base_used, quote_used) = if quote_for_base <= u128::from(quote_desired) {
        (u128::from(base_desired), quote_for_base)
    } else {
        (
            mul_div_ceil(u128::from(quote_desired), br, qr)?,
            u128::from(quote_desired),
        )
    };
    if base_used > u128::from(base_desired) {
        return None;
    }
    let lp_b = mul_div_floor(base_used, u128::from(lp_supply), br)?;
    let lp_q = mul_div_floor(quote_used, u128::from(lp_supply), qr)?;
    let lp = u64::try_from(lp_b.min(lp_q)).ok()?;
    if lp == 0 {
        return None;
    }
    Some((
        u64::try_from(base_used).ok()?,
        u64::try_from(quote_used).ok()?,
        lp,
    ))
}

/// What `lp` shares withdraw: the proportional reserves, rounded down.
pub fn withdraw_for_lp(
    lp: u64,
    base_reserve: u64,
    quote_reserve: u64,
    lp_supply: u64,
) -> Option<(u64, u64)> {
    if lp == 0 || lp > lp_supply {
        return None;
    }
    let base = mul_div_floor(
        u128::from(lp),
        u128::from(base_reserve),
        u128::from(lp_supply),
    )?;
    let quote = mul_div_floor(
        u128::from(lp),
        u128::from(quote_reserve),
        u128::from(lp_supply),
    )?;
    Some((u64::try_from(base).ok()?, u64::try_from(quote).ok()?))
}

/// The curve of a launch, derived from the supply, the share sold on the curve and the virtual
/// quote reserve it opens with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CurveParams {
    /// Tokens sold on the curve (`C`).
    pub curve_tokens: u64,
    /// Tokens reserved for graduation (`R = S - C`).
    pub reserve_tokens: u64,
    /// Virtual quote reserve at opening (`Vq`).
    pub virtual_quote: u64,
    /// Virtual base reserve (`Vb = C · R / (C − R)`), so that dropping the offsets at graduation
    /// leaves the price unchanged.
    pub virtual_base: u64,
    /// Real quote reserve at which the launch graduates (`T = Vq · (C − R) / R`).
    pub graduation_quote: u64,
}

/// See [`CurveParams`]. `None` unless `C > R`, i.e. more than half the supply is on the curve.
pub fn curve_params(supply: u64, curve_bps: u64, virtual_quote: u64) -> Option<CurveParams> {
    if curve_bps > BPS || virtual_quote == 0 {
        return None;
    }
    let c = mul_div_floor(u128::from(supply), u128::from(curve_bps), u128::from(BPS))?;
    let r = u128::from(supply).checked_sub(c)?;
    if c <= r || r == 0 {
        return None;
    }
    let diff = c - r;
    let virtual_base = mul_div_floor(c, r, diff)?;
    let graduation_quote = mul_div_floor(u128::from(virtual_quote), diff, r)?;
    Some(CurveParams {
        curve_tokens: u64::try_from(c).ok()?,
        reserve_tokens: u64::try_from(r).ok()?,
        virtual_quote,
        virtual_base: u64::try_from(virtual_base).ok()?,
        graduation_quote: u64::try_from(graduation_quote).ok()?,
    })
}

/// The market cap a curve opens at, in quote base units: `S · Vq / (C + Vb)`.
pub fn opening_mcap(supply: u64, params: &CurveParams) -> Option<u64> {
    let x = u128::from(params.curve_tokens).checked_add(u128::from(params.virtual_base))?;
    u64::try_from(mul_div_floor(
        u128::from(supply),
        u128::from(params.virtual_quote),
        x,
    )?)
    .ok()
}

/// The virtual quote reserve that opens a curve at `mcap` (quote base units): the inverse of
/// [`opening_mcap`], rounded up.
pub fn virtual_quote_for_mcap(supply: u64, curve_bps: u64, mcap: u64) -> Option<u64> {
    // Vb does not depend on Vq, so derive it from any Vq.
    let shape = curve_params(supply, curve_bps, 1)?;
    let x = u128::from(shape.curve_tokens).checked_add(u128::from(shape.virtual_base))?;
    u64::try_from(mul_div_ceil(u128::from(mcap), x, u128::from(supply))?).ok()
}

/// The base tokens to add to a curve pool at graduation so that the price is the same before and
/// after the virtual offsets are dropped: `Rq · (Rb + Vb) / (Rq + Vq) − Rb`, rounded down, never
/// negative.
pub fn graduation_topup(
    base_reserve: u64,
    quote_reserve: u64,
    virtual_base: u64,
    virtual_quote: u64,
) -> Option<u64> {
    let x = u128::from(base_reserve).checked_add(u128::from(virtual_base))?;
    let y = u128::from(quote_reserve).checked_add(u128::from(virtual_quote))?;
    let target = mul_div_floor(u128::from(quote_reserve), x, y)?;
    u64::try_from(target.saturating_sub(u128::from(base_reserve))).ok()
}

/// The creator and holder fees a launch pool's hook takes from `amount` of the quote (a buy's
/// input, a sell's output after the DEX's protocol fee), `docs/hooks-v2.md` §5.4: each rounded up
/// ([`fee_amount`]); the holder fee only when `holder_fee_bps > 0` and holders hold at least the
/// threshold (`eligible >= min_eligible`, as the kit counts them); neither when together they are
/// not below `amount`. Answers `(creator_fee, holder_fee)`.
pub fn creator_and_holder_fees(
    amount: u64,
    creator_fee_bps: u16,
    holder_fee_bps: u16,
    eligible: u64,
    min_eligible: u64,
) -> (u64, u64) {
    // A fee that does not fit a u64 is above the amount, so the guard drops it.
    let creator = fee_amount(amount, creator_fee_bps).unwrap_or(u64::MAX);
    let holder = if holder_fee_bps > 0 && eligible >= min_eligible {
        fee_amount(amount, holder_fee_bps).unwrap_or(u64::MAX)
    } else {
        0
    };
    match creator.checked_add(holder) {
        Some(sum) if sum < amount => (creator, holder),
        _ => (0, 0),
    }
}

/// The burn a launch pool's hook takes from `amount` of the token (a buy's output, a sell's input),
/// §5.4: rounded down, and none unless it is below `amount`.
pub fn trade_burn(amount: u64, burn_bps: u16) -> u64 {
    mul_div_floor(u128::from(amount), u128::from(burn_bps), u128::from(BPS))
        .and_then(|burn| u64::try_from(burn).ok())
        .filter(|burn| *burn < amount)
        .unwrap_or(0)
}

/// A launch's fee rates, as its pool hook applies them (`docs/hooks-v2.md` §5.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LaunchFeeRates {
    /// Creator fee, both sides, from the quote.
    pub creator_fee_bps: u16,
    /// Holder fee on buys, from the quote input.
    pub holder_fee_buy_bps: u16,
    /// Holder fee on sells, from the quote output after the protocol fee.
    pub holder_fee_sell_bps: u16,
    /// Burn on buys, from the token output.
    pub burn_buy_bps: u16,
    /// Burn on sells, from the token input.
    pub burn_sell_bps: u16,
}

/// What a launch pool's hook takes in one callback: creator and holder fees in the quote, the burn
/// in the token.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HookCut {
    /// To the launch's quote holding.
    pub creator_fee: u64,
    /// To the holder vault.
    pub holder_fee: u64,
    /// Burned.
    pub burn: u64,
}

/// The hook's `before_swap` cut from the input (§5.4): a buy's creator and holder fees, a sell's
/// burn. `eligible` and `min_eligible` are the kit's counts as the hook reads them (before the
/// trade); they only matter for a holder fee.
pub fn launch_before_swap(
    buy: bool,
    amount_in: u64,
    rates: &LaunchFeeRates,
    eligible: u64,
    min_eligible: u64,
) -> HookCut {
    if buy {
        let (creator_fee, holder_fee) = creator_and_holder_fees(
            amount_in,
            rates.creator_fee_bps,
            rates.holder_fee_buy_bps,
            eligible,
            min_eligible,
        );
        HookCut {
            creator_fee,
            holder_fee,
            burn: 0,
        }
    } else {
        HookCut {
            burn: trade_burn(amount_in, rates.burn_sell_bps),
            ..HookCut::default()
        }
    }
}

/// The hook's `after_swap` cut from the output it is told (§5.4): a buy's burn from the curve's
/// output, a sell's creator and holder fees from the output after the protocol fee. For a sell,
/// `eligible` is the kit's count once the input has left the seller.
pub fn launch_after_swap(
    buy: bool,
    amount_out: u64,
    rates: &LaunchFeeRates,
    eligible: u64,
    min_eligible: u64,
) -> HookCut {
    if buy {
        HookCut {
            burn: trade_burn(amount_out, rates.burn_buy_bps),
            ..HookCut::default()
        }
    } else {
        let (creator_fee, holder_fee) = creator_and_holder_fees(
            amount_out,
            rates.creator_fee_bps,
            rates.holder_fee_sell_bps,
            eligible,
            min_eligible,
        );
        HookCut {
            creator_fee,
            holder_fee,
            burn: 0,
        }
    }
}

/// The max-wallet cap of a launch, fixed at launch: `supply * max_wallet_bps / 10_000`, rounded
/// down (the kit's `max_wallet_amount`); 0 when off.
pub fn max_wallet_cap(supply: u64, max_wallet_bps: u16) -> u64 {
    mul_div_floor(
        u128::from(supply),
        u128::from(max_wallet_bps),
        u128::from(BPS),
    )
    .and_then(|cap| u64::try_from(cap).ok())
    .unwrap_or(u64::MAX)
}

/// The LP fee during the sniper window: `start` at creation, falling linearly to `base` over
/// `window` seconds, `base` from then on.
pub fn sniper_lp_fee(
    now: i64,
    created_at: i64,
    window_secs: i64,
    start_bps: u16,
    base_bps: u16,
) -> u16 {
    if window_secs <= 0 || now >= created_at.saturating_add(window_secs) || start_bps <= base_bps {
        return base_bps;
    }
    let elapsed = now.saturating_sub(created_at).max(0) as u64;
    let span = u64::from(start_bps - base_bps);
    let fallen = span.saturating_mul(elapsed) / (window_secs as u64);
    start_bps - (fallen.min(span) as u16)
}

#[cfg(test)]
mod tests {
    use super::policy::*;
    use super::*;

    #[test]
    fn isqrt_is_exact() {
        for n in [
            0u128,
            1,
            2,
            3,
            4,
            15,
            16,
            17,
            1 << 64,
            (1 << 100) + 12345,
            u128::MAX,
        ] {
            let r = isqrt(n);
            assert!(r * r <= n);
            assert!(r
                .checked_add(1)
                .is_none_or(|s| s.checked_mul(s).is_none_or(|sq| sq > n)));
        }
    }

    #[test]
    fn swap_keeps_k() {
        let (rb, rq, vb, vq) = (
            750_000_000_000_000u64,
            0u64,
            375_000_000_000_000u64,
            28_125_000_000u64,
        );
        let out = swap_out(1_000_000_000, rq, vq, rb, vb).unwrap();
        let k0 = (u128::from(rb) + u128::from(vb)) * (u128::from(rq) + u128::from(vq));
        let k1 = (u128::from(rb - out) + u128::from(vb))
            * (u128::from(rq + 1_000_000_000) + u128::from(vq));
        assert!(k1 >= k0);
        assert!(out > 0 && out < rb);
    }

    #[test]
    fn curve_graduates_continuously() {
        let p = curve_params(TOKEN_SUPPLY, CURVE_BPS, 28_125_000_000).unwrap();
        assert_eq!(p.curve_tokens, 750_000_000_000_000);
        assert_eq!(p.reserve_tokens, 250_000_000_000_000);
        assert_eq!(p.virtual_base, 375_000_000_000_000);
        assert_eq!(p.graduation_quote, 56_250_000_000);
        // Buy the whole curve in one go: the input that empties the real base reserve.
        let dx = max_swap_in(0, p.virtual_quote, p.curve_tokens, p.virtual_base).unwrap();
        assert!(dx >= p.graduation_quote && dx - p.graduation_quote < 1_000);
        // With the reserve topped up for continuity, the post-graduation price equals the curve's.
        let rb = 0u64;
        let rq = p.graduation_quote;
        let topup = graduation_topup(rb, rq, p.virtual_base, p.virtual_quote).unwrap();
        assert!(topup <= p.reserve_tokens);
        let before = spot_price_wad(rb, p.virtual_base, rq, p.virtual_quote).unwrap();
        let after = spot_price_wad(rb + topup, 0, rq, 0).unwrap();
        let diff = before.abs_diff(after);
        assert!(
            diff * 1_000_000 <= before,
            "price moved by more than a millionth: {before} -> {after}"
        );
        // The opening market cap is what the policy says: S · Vq / (C + Vb).
        assert_eq!(opening_mcap(TOKEN_SUPPLY, &p).unwrap(), 25_000_000_000);
        assert_eq!(
            virtual_quote_for_mcap(TOKEN_SUPPLY, CURVE_BPS, 25_000_000_000).unwrap(),
            28_125_000_000
        );
        // Graduation market cap is (C/R)^2 = 9 times the opening one.
        let grad_mcap = mul_div_floor(
            u128::from(TOKEN_SUPPLY),
            u128::from(rq),
            u128::from(rb + topup),
        )
        .unwrap();
        assert!(grad_mcap.abs_diff(9 * 25_000_000_000) * 1_000_000 <= grad_mcap);
    }

    #[test]
    fn lp_math_round_trips() {
        let (to_depositor, total) = initial_lp(1_000_000_000, 4_000_000_000).unwrap();
        assert_eq!(total, 2_000_000_000);
        assert_eq!(to_depositor, total - MINIMUM_LIQUIDITY);
        let (b, q, lp) = lp_for_deposit(
            500_000_000,
            10_000_000_000,
            1_000_000_000,
            4_000_000_000,
            total,
        )
        .unwrap();
        assert_eq!((b, q), (500_000_000, 2_000_000_000));
        assert_eq!(lp, 1_000_000_000);
        let (wb, wq) = withdraw_for_lp(lp, 1_500_000_000, 6_000_000_000, total + lp).unwrap();
        assert_eq!((wb, wq), (500_000_000, 2_000_000_000));
    }

    #[test]
    fn sniper_fee_falls_to_base() {
        assert_eq!(sniper_lp_fee(0, 0, 30, 8_000, 30), 8_000);
        assert_eq!(sniper_lp_fee(15, 0, 30, 8_000, 30), 8_000 - 3_985);
        assert_eq!(sniper_lp_fee(30, 0, 30, 8_000, 30), 30);
        assert_eq!(sniper_lp_fee(100, 0, 30, 8_000, 30), 30);
        assert_eq!(sniper_lp_fee(-5, 0, 30, 8_000, 30), 8_000);
    }

    #[test]
    fn fees_round_up() {
        assert_eq!(fee_amount(10_000, 30), Some(30));
        assert_eq!(fee_amount(1, 30), Some(1));
        assert_eq!(fee_amount(0, 30), Some(0));
        assert_eq!(fee_amount(u64::MAX, 10_000), Some(u64::MAX));
    }

    /// The opening reserves of a policy launch.
    fn opening() -> Reserves {
        Reserves {
            base_reserve: 750_000_000_000_000,
            quote_reserve: 0,
            virtual_base: 375_000_000_000_000,
            virtual_quote: 28_125_000_000,
        }
    }

    #[test]
    fn a_buy_pays_the_protocol_fee_from_its_input() {
        let r = opening();
        let received = 1_000_000_000u64;
        let a = swap_amounts(true, received, LP_FEE_BPS, PROTOCOL_FEE_BPS, &r).unwrap();
        let lp_fee = fee_amount(received, LP_FEE_BPS).unwrap();
        let protocol_fee = fee_amount(received, PROTOCOL_FEE_BPS).unwrap();
        assert_eq!((lp_fee, protocol_fee), (3_000_000, 10_000_000));
        let net_in = received - lp_fee - protocol_fee;
        let out = swap_out(
            net_in,
            r.quote_reserve,
            r.virtual_quote,
            r.base_reserve,
            r.virtual_base,
        )
        .unwrap();
        assert_eq!(
            a,
            SwapAmounts {
                lp_fee,
                protocol_fee,
                net_in,
                out_gross: out,
                amount_out: out,
                to_reserve_in: received - protocol_fee,
            }
        );
    }

    #[test]
    fn a_sell_pays_the_protocol_fee_from_its_output_in_quote() {
        // After a 1 SOL buy.
        let buy = swap_amounts(true, 1_000_000_000, 30, 100, &opening()).unwrap();
        let r = Reserves {
            base_reserve: opening().base_reserve - buy.out_gross,
            quote_reserve: buy.to_reserve_in,
            ..opening()
        };
        let received = buy.out_gross / 2;
        let a = swap_amounts(false, received, 30, 100, &r).unwrap();
        let lp_fee = fee_amount(received, 30).unwrap();
        let out_gross = swap_out(
            received - lp_fee,
            r.base_reserve,
            r.virtual_base,
            r.quote_reserve,
            r.virtual_quote,
        )
        .unwrap();
        let protocol_fee = fee_amount(out_gross, 100).unwrap();
        assert_eq!(
            a,
            SwapAmounts {
                lp_fee,
                protocol_fee,
                net_in: received - lp_fee,
                out_gross,
                amount_out: out_gross - protocol_fee,
                // The whole input stays in the pool: no protocol fee in the base token any more.
                to_reserve_in: received,
            }
        );
        assert!(protocol_fee > 0 && a.amount_out + a.protocol_fee == a.out_gross);
    }

    #[test]
    fn creator_and_holder_fees_round_up_and_drop_at_dust() {
        assert_eq!(
            creator_and_holder_fees(1_000_000_000, 50, 100, 1, 1),
            (5_000_000, 10_000_000)
        );
        assert_eq!(creator_and_holder_fees(10_001, 50, 100, 1, 1), (51, 101));
        // creator + holder >= amount: neither is taken.
        assert_eq!(creator_and_holder_fees(0, 50, 100, 1, 1), (0, 0));
        assert_eq!(creator_and_holder_fees(1, 50, 0, 1, 1), (0, 0));
        assert_eq!(creator_and_holder_fees(2, 50, 100, 1, 1), (0, 0));
        assert_eq!(creator_and_holder_fees(3, 50, 100, 1, 1), (1, 1));
        // At the ceilings.
        assert_eq!(creator_and_holder_fees(20, 500, 500, 1, 1), (1, 1));
        assert_eq!(creator_and_holder_fees(2, 500, 500, 1, 1), (0, 0));
        // The holder fee only from the eligible threshold up; below it the creator fee alone
        // faces the guard.
        let min = 1_000_000_000_000u64;
        assert_eq!(
            creator_and_holder_fees(1_000_000_000, 50, 100, min - 1, min),
            (5_000_000, 0)
        );
        assert_eq!(
            creator_and_holder_fees(1_000_000_000, 50, 100, min, min),
            (5_000_000, 10_000_000)
        );
        assert_eq!(creator_and_holder_fees(3, 50, 100, 0, min), (1, 0));
        // Rates that do not fit are dropped by the guard, never wrapped.
        assert_eq!(
            creator_and_holder_fees(u64::MAX, u16::MAX, u16::MAX, 1, 1),
            (0, 0)
        );
    }

    #[test]
    fn trade_burns_round_down_and_never_take_everything() {
        assert_eq!(trade_burn(10_001, 50), 50);
        assert_eq!(trade_burn(199, 50), 0);
        assert_eq!(trade_burn(200, 50), 1);
        assert_eq!(trade_burn(1, 100), 0);
        assert_eq!(trade_burn(0, 100), 0);
        assert_eq!(trade_burn(5, 10_000), 0);
        assert_eq!(trade_burn(u64::MAX, u16::MAX), 0);
        let rates = LaunchFeeRates {
            creator_fee_bps: 50,
            holder_fee_buy_bps: 0,
            holder_fee_sell_bps: 200,
            burn_buy_bps: 25,
            burn_sell_bps: 100,
        };
        // Each side at its own rate, in its own callback.
        let cut = |creator_fee, holder_fee, burn| HookCut {
            creator_fee,
            holder_fee,
            burn,
        };
        assert_eq!(
            launch_before_swap(true, 1_000_000, &rates, 1, 1),
            cut(5_000, 0, 0)
        );
        assert_eq!(
            launch_after_swap(true, 1_000_000, &rates, 1, 1),
            cut(0, 0, 2_500)
        );
        assert_eq!(
            launch_before_swap(false, 1_000_000, &rates, 1, 1),
            cut(0, 0, 10_000)
        );
        assert_eq!(
            launch_after_swap(false, 1_000_000, &rates, 1, 1),
            cut(5_000, 20_000, 0)
        );
        assert_eq!(
            max_wallet_cap(policy::TOKEN_SUPPLY, 200),
            20_000_000_000_000
        );
        assert_eq!(max_wallet_cap(999, 100), 9);
        assert_eq!(max_wallet_cap(policy::TOKEN_SUPPLY, 0), 0);
    }

    #[test]
    fn swaps_that_deliver_nothing_are_refused() {
        let r = opening();
        // One lamport in: the LP fee rounds up to all of it.
        assert_eq!(
            swap_amounts(true, 1, 30, 100, &r),
            Err(SwapFailure::FeesExceedInput)
        );
        assert_eq!(
            swap_amounts(false, 1, 30, 0, &r),
            Err(SwapFailure::FeesExceedInput)
        );
        // A fee rate that does not fit is more than the input.
        assert_eq!(
            swap_amounts(true, u64::MAX, u16::MAX, 0, &r),
            Err(SwapFailure::FeesExceedInput)
        );
        // More than the real reserve, or nothing from the curve.
        let thin = Reserves {
            base_reserve: 10,
            ..r
        };
        assert_eq!(
            swap_amounts(true, 1_000_000_000_000, 30, 100, &thin),
            Err(SwapFailure::InsufficientLiquidity)
        );
        assert_eq!(
            swap_amounts(false, 1_000, 0, 100, &r),
            Err(SwapFailure::InsufficientLiquidity)
        );
        // A sell whose curve output is one lamport: the protocol fee (rounded up) takes it all.
        let flat = Reserves {
            base_reserve: 1_000_000,
            quote_reserve: 1_000_000,
            virtual_base: 0,
            virtual_quote: 0,
        };
        let a = swap_amounts(false, 2, 0, 0, &flat).unwrap();
        assert_eq!((a.out_gross, a.amount_out, a.protocol_fee), (1, 1, 0));
        assert_eq!(
            swap_amounts(false, 2, 0, 100, &flat),
            Err(SwapFailure::FeeExceedsOutput)
        );
    }

    #[test]
    fn the_share_model_takes_the_lp_fee_and_a_quarter_of_the_cuts_in_the_quote() {
        let r = opening();
        let share = LAUNCH_PROTOCOL_SHARE_BPS;
        // A buy of 1 SOL with a 1% creator fee: the hook cut 10,000,000 lamports from the input;
        // Bordrless takes 25% of that (0.25% of the trade) and the LP fee on what reached the
        // vault, both before the curve, and only the rest enters the reserve: nothing compounds.
        let creator_fee = fee_amount(1_000_000_000, 100).unwrap();
        let received = 1_000_000_000 - creator_fee;
        let a = swap_amounts_shared(true, received, LP_FEE_BPS, share, creator_fee, &r).unwrap();
        let lp_fee = fee_amount(received, LP_FEE_BPS).unwrap();
        assert_eq!(a.protocol_fee, 2_500_000 + lp_fee);
        assert_eq!(a.lp_fee, 0);
        assert_eq!(a.net_in, received - lp_fee - 2_500_000);
        assert_eq!(a.to_reserve_in, a.net_in);
        assert_eq!(a.amount_out, a.out_gross);
        // No cuts (a Plain launch with creator fee 0): the LP fee alone, still Bordrless's.
        let b = swap_amounts_shared(true, 1_000_000_000, LP_FEE_BPS, share, 0, &r).unwrap();
        let lp_b = fee_amount(1_000_000_000, LP_FEE_BPS).unwrap();
        assert_eq!((b.protocol_fee, b.lp_fee), (lp_b, 0));
        assert_eq!(
            (b.net_in, b.to_reserve_in),
            (1_000_000_000 - lp_b, 1_000_000_000 - lp_b)
        );
        // A sell whose input side cut nothing (the launch hook only burns there): the tokens go
        // into the curve whole, the LP fee leaves its output in the quote, and the hook is told
        // the rest; the output side's share comes from what the hook then cuts.
        let traded = Reserves {
            base_reserve: r.base_reserve - b.out_gross,
            quote_reserve: b.to_reserve_in,
            ..r
        };
        let s = swap_amounts_shared(false, b.out_gross / 2, LP_FEE_BPS, share, 0, &traded).unwrap();
        assert_eq!(
            (s.net_in, s.to_reserve_in, s.lp_fee),
            (b.out_gross / 2, b.out_gross / 2, 0)
        );
        let lp_s = fee_amount(s.out_gross, LP_FEE_BPS).unwrap();
        assert_eq!((s.protocol_fee, s.amount_out), (lp_s, s.out_gross - lp_s));
        let (c, h) = creator_and_holder_fees(s.amount_out, 100, 100, 1, 1);
        let p_out = output_share(false, share, c + h, s.net_in, s.out_gross).unwrap();
        assert_eq!(p_out, fee_amount(c + h, share).unwrap());
        assert!(p_out * 4 >= c + h && p_out * 4 < c + h + 4);
        // A base-side cut (a token hook of the creator's own) is valued at the swap's price,
        // rounded up: on a sell its share leaves the output, with the LP fee, before the hook is
        // told; on a buy it is set aside after the swap.
        let cut_in = 1_000_000u64;
        let s2 = swap_amounts_shared(false, b.out_gross / 2, LP_FEE_BPS, share, cut_in, &traded)
            .unwrap();
        let value = quote_value(cut_in, s2.out_gross, s2.net_in).unwrap();
        assert_eq!(
            s2.protocol_fee,
            fee_amount(value, share).unwrap() + fee_amount(s2.out_gross, LP_FEE_BPS).unwrap()
        );
        assert_eq!(s2.amount_out, s2.out_gross - s2.protocol_fee);
        assert_eq!(
            output_share(true, share, cut_in, a.net_in, a.out_gross).unwrap(),
            fee_amount(quote_value(cut_in, a.net_in, a.out_gross).unwrap(), share).unwrap()
        );
        // Rounding: value and share round up, and the share never exceeds the value.
        assert_eq!(quote_value(1, 1, 3), Some(1));
        assert_eq!(quote_value(0, 1, 0), Some(0));
        assert_eq!(quote_value(1, 1, 0), None);
        assert_eq!(protocol_share(1, 2_500), Some(1));
        assert_eq!(protocol_share(3, 2_500), Some(1));
        assert_eq!(protocol_share(4, 2_500), Some(1));
        assert_eq!(protocol_share(5, 2_500), Some(2));
        assert_eq!(protocol_share(7, 10_000), Some(7));
        assert_eq!(protocol_share(7, 10_001), None);
        assert_eq!(protocol_share(0, 2_500), Some(0));
        // A share that eats the whole output is refused like a flat fee that does.
        let flat = Reserves {
            base_reserve: 1_000_000,
            quote_reserve: 1_000_000,
            virtual_base: 0,
            virtual_quote: 0,
        };
        assert_eq!(
            swap_amounts_shared(false, 2, 0, 10_000, 5, &flat),
            Err(SwapFailure::FeeExceedsOutput)
        );
        assert_eq!(
            swap_amounts_shared(true, 2, 30, 10_000, 1, &flat),
            Err(SwapFailure::FeesExceedInput)
        );
    }

    #[test]
    fn the_presets_are_four_and_within_the_policy_bounds() {
        use policy::presets::*;
        assert_eq!(ALL.len(), 4);
        assert_eq!(DEFAULT, PLAIN);
        assert_eq!(
            ALL.map(|p| p.name),
            ["Plain", "Diamond hands", "Burn", "Paid to hold"]
        );
        for p in ALL {
            assert!(p.creator_fee_bps <= MAX_CREATOR_FEE_BPS, "{}", p.name);
            assert!(
                p.holder_fee_buy_bps <= MAX_HOLDER_FEE_BPS
                    && p.holder_fee_sell_bps <= MAX_HOLDER_FEE_BPS
            );
            assert!(p.burn_buy_bps <= MAX_BURN_BPS && p.burn_sell_bps <= MAX_BURN_BPS);
            for (holder, burn) in [
                (p.holder_fee_buy_bps, p.burn_buy_bps),
                (p.holder_fee_sell_bps, p.burn_sell_bps),
            ] {
                assert!(
                    p.creator_fee_bps + holder + burn <= MAX_RULES_FEE_BPS,
                    "{}",
                    p.name
                );
            }
            assert!(
                p.max_wallet_bps == 0
                    || (MIN_MAX_WALLET_BPS..=MAX_MAX_WALLET_BPS).contains(&p.max_wallet_bps)
            );
            assert!(p.creator_lock_secs <= MAX_CREATOR_LOCK_SECS);
            assert!(p.early_window_secs <= MAX_EARLY_WINDOW_SECS);
            if p.early_window_secs == 0 {
                assert_eq!(p.early_lock_secs, 0);
            } else {
                assert!(
                    p.early_lock_secs > p.early_window_secs
                        && p.early_lock_secs <= MAX_EARLY_LOCK_SECS
                );
            }
        }
        assert_eq!(PLAIN.creator_fee_bps, 100);
        assert_eq!(
            (DIAMOND_HANDS.max_wallet_bps, DIAMOND_HANDS.early_lock_secs),
            (100, 86_400)
        );
        assert_eq!(PAID_TO_HOLD.holder_fee_buy_bps, 0);
    }
}
