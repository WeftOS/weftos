//! Portable float helpers for `no_std` builds (no libm dependency).
//!
//! Host (`std`) uses the language builtins; bare targets use small Newton /
//! series approximations good enough for CEM noise and L2 distances.

/// Square root.
#[inline]
pub fn sqrtf(x: f32) -> f32 {
    #[cfg(feature = "std")]
    {
        x.sqrt()
    }
    #[cfg(not(feature = "std"))]
    {
        if x <= 0.0 {
            return 0.0;
        }
        // Quake-style inverse sqrt + one Newton step, then invert.
        let mut y = f32::from_bits(0x5f37_59df - (x.to_bits() >> 1));
        y = y * (1.5 - 0.5 * x * y * y);
        y = y * (1.5 - 0.5 * x * y * y);
        x * y
    }
}

/// Natural logarithm (domain `x > 0`).
#[inline]
pub fn lnf(x: f32) -> f32 {
    #[cfg(feature = "std")]
    {
        x.ln()
    }
    #[cfg(not(feature = "std"))]
    {
        // frexp-style: x = m * 2^e with m in [0.5, 1)
        if x <= 0.0 {
            return f32::NEG_INFINITY;
        }
        let bits = x.to_bits();
        let exp = ((bits >> 23) & 0xff) as i32 - 127;
        let mut m = f32::from_bits((bits & 0x807f_ffff) | 0x3f00_0000); // [0.5, 1)
        // AtanH series on (m-1)/(m+1) around 1.0 after shift to [√0.5, √2].
        // Simpler: range reduce to [2/3, 4/3] then padé-ish.
        let mut e = exp;
        if m < core::f32::consts::FRAC_1_SQRT_2 {
            m *= 2.0;
            e -= 1;
        }
        let y = (m - 1.0) / (m + 1.0);
        let y2 = y * y;
        // ln((1+y)/(1-y)) ≈ 2(y + y^3/3 + y^5/5 + y^7/7)
        let mut s = y;
        let mut t = y * y2;
        s += t / 3.0;
        t *= y2;
        s += t / 5.0;
        t *= y2;
        s += t / 7.0;
        2.0 * s + (e as f32) * core::f32::consts::LN_2
    }
}

/// Cosine (radians).
#[inline]
pub fn cosf(x: f32) -> f32 {
    #[cfg(feature = "std")]
    {
        x.cos()
    }
    #[cfg(not(feature = "std"))]
    {
        // Range reduce to [-π, π] then Taylor.
        const PI: f32 = core::f32::consts::PI;
        const TWO_PI: f32 = core::f32::consts::TAU;
        let mut t = x % TWO_PI;
        if t > PI {
            t -= TWO_PI;
        } else if t < -PI {
            t += TWO_PI;
        }
        let t2 = t * t;
        // 1 - t^2/2! + t^4/4! - t^6/6! + t^8/8!
        1.0 - t2 * (0.5 - t2 * (1.0 / 24.0 - t2 * (1.0 / 720.0 - t2 / 40320.0)))
    }
}

/// Arc cosine (radians, domain `[-1, 1]`, clamped).
#[inline]
pub fn acosf(x: f32) -> f32 {
    let x = x.clamp(-1.0, 1.0);
    #[cfg(feature = "std")]
    {
        x.acos()
    }
    #[cfg(not(feature = "std"))]
    {
        // Abramowitz & Stegun 4.4.46 on [0, 1] (|error| ≤ 2e-8), mirrored for x < 0.
        let a = x.abs();
        let p = core::f32::consts::FRAC_PI_2
            + a * (-0.214_598_8
                + a * (0.088_978_99
                    + a * (-0.050_174_3
                        + a * (0.030_891_88
                            + a * (-0.017_088_13 + a * (0.006_670_09 - a * 0.001_262_491))))));
        let r = sqrtf(1.0 - a) * p;
        if x < 0.0 {
            core::f32::consts::PI - r
        } else {
            r
        }
    }
}
