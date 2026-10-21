//! The arithmetic of the SOL switch (`docs/token-quotes.md` §7.5.2): prices as Q64.64 (`u128`, the
//! other side's raw units per raw unit of the token, times 2^64), a 256-bit product for the few
//! places a price meets a price, Raydium CLMM's tick math (ported from
//! `raydium-clmm/programs/amm/src/libraries/tick_math.rs` @ edfc5c0a, exact in `u128`), and the
//! minimum out and cap of a batch.

/// 2^64.
pub const Q64: u128 = 1 << 64;

/// `a * b` as a 256-bit number: `(high, low)` 128-bit halves.
pub fn mul_wide(a: u128, b: u128) -> (u128, u128) {
    let mask = u128::from(u64::MAX);
    let (a_hi, a_lo) = (a >> 64, a & mask);
    let (b_hi, b_lo) = (b >> 64, b & mask);
    let lo_lo = a_lo * b_lo;
    let hi_lo = a_hi * b_lo;
    let lo_hi = a_lo * b_hi;
    let hi_hi = a_hi * b_hi;
    let mid = (lo_lo >> 64) + (hi_lo & mask) + (lo_hi & mask);
    let low = (lo_lo & mask) | ((mid & mask) << 64);
    let high = hi_hi + (hi_lo >> 64) + (lo_hi >> 64) + (mid >> 64);
    (high, low)
}

/// `floor(a * b / d)`, `None` when `d` is 0 or the result does not fit in a `u128`.
pub fn mul_div(a: u128, b: u128, d: u128) -> Option<u128> {
    if d == 0 {
        return None;
    }
    let (high, low) = mul_wide(a, b);
    if high == 0 {
        return Some(low / d);
    }
    if high >= d {
        return None;
    }
    // Long division of the 256-bit (high, low) by d, one bit at a time: the remainder stays below d.
    let mut rem = high;
    let mut quotient: u128 = 0;
    for i in (0..128).rev() {
        let carry = rem >> 127;
        rem = (rem << 1) | ((low >> i) & 1);
        quotient <<= 1;
        if carry == 1 || rem >= d {
            rem = rem.wrapping_sub(d);
            quotient |= 1;
        }
    }
    Some(quotient)
}

/// `p * q` of two Q64.64 prices (`None` past `u128`).
pub fn mul_q64(p: u128, q: u128) -> Option<u128> {
    mul_div(p, q, Q64)
}

/// The value of `amount` raw units at `price` (Q64.64): `floor(amount * price / 2^64)`.
pub fn value_of(amount: u64, price: u128) -> u128 {
    let mask = u128::from(u64::MAX);
    let a = u128::from(amount);
    // amount * (price >> 64) fits (64 x 64 bits); the low half's product shifted down adds the rest.
    a.saturating_mul(price >> 64)
        .saturating_add((a * (price & mask)) >> 64)
}

/// The Q64.64 price of a pool side `other` per unit of `token` (raw reserves): `other / token`.
pub fn ratio_q64(other: u64, token: u64) -> Option<u128> {
    if token == 0 {
        return None;
    }
    Some((u128::from(other) << 64) / u128::from(token))
}

/// The inverse of a Q64.64 price.
pub fn invert_q64(price: u128) -> Option<u128> {
    if price == 0 {
        return None;
    }
    mul_div(Q64, Q64, price)
}

/// The token amount worth `value` at `price` (Q64.64): `floor(value * 2^64 / price)`.
pub fn amount_for(value: u128, price: u128) -> Option<u128> {
    mul_div(value, Q64, price)
}

// ---- Raydium CLMM's tick math -----------------------------------------------------------------------

/// The lowest tick.
pub const MIN_TICK: i32 = -443_636;
/// The highest tick.
pub const MAX_TICK: i32 = 443_636;

/// `get_sqrt_price_at_tick`: √(1.0001^tick) as Q64.64, exactly as Raydium CLMM computes it.
pub fn sqrt_price_at_tick(tick: i32) -> Option<u128> {
    if !(MIN_TICK..=MAX_TICK).contains(&tick) {
        return None;
    }
    let abs = tick.unsigned_abs();
    let mut ratio: u128 = if abs & 0x1 != 0 {
        0xfffcb933bd6fb800
    } else {
        Q64
    };
    const STEPS: [(u32, u128); 18] = [
        (0x2, 0xfff97272373d4000),
        (0x4, 0xfff2e50f5f657000),
        (0x8, 0xffe5caca7e10f000),
        (0x10, 0xffcb9843d60f7000),
        (0x20, 0xff973b41fa98e800),
        (0x40, 0xff2ea16466c9b000),
        (0x80, 0xfe5dee046a9a3800),
        (0x100, 0xfcbe86c7900bb000),
        (0x200, 0xf987a7253ac65800),
        (0x400, 0xf3392b0822bb6000),
        (0x800, 0xe7159475a2caf000),
        (0x1000, 0xd097f3bdfd2f2000),
        (0x2000, 0xa9f746462d9f8000),
        (0x4000, 0x70d869a156f31c00),
        (0x8000, 0x31be135f97ed3200),
        (0x10000, 0x9aa508b5b85a500),
        (0x20000, 0x5d6af8dedc582c),
        (0x40000, 0x2216e584f5fa),
    ];
    for (bit, factor) in STEPS {
        if abs & bit != 0 {
            ratio = (ratio * factor) >> 64;
        }
    }
    if tick > 0 {
        ratio = u128::MAX / ratio;
    }
    Some(ratio)
}

/// A CLMM pool's price from its √P (Q64.64, token1 per token0): token1 per token0 (`token_is_0`,
/// selling token0) or token0 per token1, as Q64.64.
pub fn price_from_sqrt(sqrt_price_x64: u128, token_is_0: bool) -> Option<u128> {
    if sqrt_price_x64 == 0 {
        return None;
    }
    if token_is_0 {
        mul_div(sqrt_price_x64, sqrt_price_x64, Q64)
    } else {
        // √(1/P) = 2^128 / √P, as Raydium inverts (`U128::MAX / ratio`).
        let inv = u128::MAX / sqrt_price_x64;
        mul_div(inv, inv, Q64)
    }
}

// ---- A batch's minimum and cap ----------------------------------------------------------------------

/// Basis points in one.
pub const BPS: u128 = 10_000;
/// Fee rates are in millionths (Raydium's `FEE_RATE_DENOMINATOR_VALUE`).
pub const PPM: u128 = 1_000_000;
/// The time-weighted price may sit at most this far (basis points) above the reference price used:
/// `P_ref = max(P_spot, P_twap × 97%)` (`docs/token-quotes.md` §7.5.2).
pub const TWAP_FLOOR_BPS: u128 = 9_700;
/// The slack the minimum out leaves for the price's own move under the batch, beyond the pool's fee
/// and its constant-product impact: 1%.
pub const MIN_OUT_SLACK_BPS: u128 = 9_900;

/// The reference price of a sale: the spot price, raised to 97% of the time-weighted one when the
/// venue keeps a history.
pub fn reference_price(spot: u128, twap: Option<u128>) -> u128 {
    match twap {
        Some(t) => spot.max(t / BPS * TWAP_FLOOR_BPS + t % BPS * TWAP_FLOOR_BPS / BPS),
        None => spot,
    }
}

/// The least a sale of `value` (other-side units at the reference price) may bring: less the pool's
/// fee (`fee_ppm`), less a constant-product pool's impact on a side of `depth` (`value / (depth +
/// value)` of it), less the 1% slack.
pub fn min_out(value: u128, fee_ppm: u128, depth: u128) -> u128 {
    let after_fee = value * (PPM - fee_ppm.min(PPM)) / PPM;
    let after_impact = if depth == 0 {
        0
    } else {
        mul_div(after_fee, depth, depth.saturating_add(value)).unwrap_or(0)
    };
    after_impact * MIN_OUT_SLACK_BPS / BPS
}

/// What a reference may sell per window: half its fee times its depth (both sides of a push pay the
/// fee; `docs/token-quotes.md` §6.3.3), in lamports.
pub fn window_cap(fee_ppm: u128, depth_lamports: u128) -> u128 {
    mul_div(fee_ppm, depth_lamports, 2 * PPM).unwrap_or(u128::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_products_and_division_are_exact() {
        assert_eq!(mul_wide(u128::MAX, u128::MAX), (u128::MAX - 1, 1));
        assert_eq!(mul_wide(1 << 100, 1 << 100), (1 << 72, 0));
        assert_eq!(mul_div(u128::MAX, u128::MAX, u128::MAX), Some(u128::MAX));
        assert_eq!(mul_div(1 << 100, 1 << 100, 1 << 80), Some(1 << 120));
        assert_eq!(mul_div(1 << 100, 1 << 100, 1 << 70), None);
        assert_eq!(mul_div(7, 9, 4), Some(15));
        assert_eq!(mul_div(3, 3, 0), None);
        // A 256-bit numerator divided back: (2^127 + 5) * 3 / 3.
        assert_eq!(mul_div((1 << 127) + 5, 3, 3), Some((1 << 127) + 5));
        assert_eq!(mul_q64(3 * Q64, Q64 / 2), Some(3 * Q64 / 2));
        assert_eq!(value_of(1_000, 5 * Q64 / 2), 2_500);
        assert_eq!(value_of(u64::MAX, Q64), u128::from(u64::MAX));
        assert_eq!(ratio_q64(10, 4), Some(5 * Q64 / 2));
        assert_eq!(invert_q64(4 * Q64), Some(Q64 / 4));
        assert_eq!(amount_for(2_500, 5 * Q64 / 2), Some(1_000));
    }

    #[test]
    fn the_tick_math_is_raydiums() {
        // The constants of tick_math.rs and the agreed points (MIN, MAX, tick 0).
        assert_eq!(sqrt_price_at_tick(0), Some(Q64));
        assert_eq!(sqrt_price_at_tick(MIN_TICK), Some(4_295_048_016));
        assert_eq!(
            sqrt_price_at_tick(MAX_TICK),
            Some(79_226_673_521_066_979_257_578_248_091)
        );
        assert_eq!(sqrt_price_at_tick(MAX_TICK + 1), None);
        // Monotone, and each tick about 1.00005 times the last.
        let a = sqrt_price_at_tick(13_335).unwrap();
        let b = sqrt_price_at_tick(13_336).unwrap();
        assert!(b > a && (b - a) * 100_000 / a == 4);
        // A live point (TSLAx/USDC, 21 Oct): tick 13335 holds √P 35931229615057125583.
        let live = 35_931_229_615_057_125_583u128;
        assert!(a <= live && live < b);
    }

    #[test]
    fn prices_from_a_square_root_both_ways() {
        // √P = 2 (Q64): token1 per token0 = 4, token0 per token1 = 1/4.
        let sp = 2 * Q64;
        assert_eq!(price_from_sqrt(sp, true), Some(4 * Q64));
        let inv = price_from_sqrt(sp, false).unwrap();
        assert!(inv.abs_diff(Q64 / 4) <= 1);
    }

    #[test]
    fn the_reference_the_minimum_and_the_cap() {
        assert_eq!(reference_price(100, None), 100);
        assert_eq!(reference_price(100, Some(100)), 100);
        assert_eq!(reference_price(90, Some(100 * Q64)), 97 * Q64);
        assert_eq!(reference_price(99 * Q64, Some(100 * Q64)), 99 * Q64);
        // 1,000,000 at a 0.25% fee into a side of 1e9: less 2,500, less 0.1% impact, less 1%.
        let m = min_out(1_000_000, 2_500, 1_000_000_000);
        assert_eq!(m, 996_503 * 9_900 / 10_000);
        assert_eq!(window_cap(2_500, 1_000_000_000_000), 1_250_000_000);
    }
}
