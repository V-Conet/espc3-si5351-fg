//! Pure PLL / MultiSynth divider planning for the Si5351A.
//!
//! No I2C is touched here; `Si5351::configure` uses these plans to write the
//! register tree.

const XTAL_HZ: u64 = 25_000_000;
const VCO_DEFAULT_HZ: u64 = 900_000_000;
const VCO_MIN_HZ: u64 = 600_000_000;
const VCO_MAX_HZ: u64 = 900_000_000;
const FRAC_DENOM: u64 = 1_000_000; // 20-bit field, max 1_048_575

/// A MultiSynth + R divider plan for one output.
#[derive(Clone, Copy, Debug)]
pub(super) struct MsPlan {
    /// Use the MultiSynth integer mode (MSx_INT = 1).
    pub(super) integer: bool,
    pub(super) a: u32,
    pub(super) b: u64,
    pub(super) c: u64,
    /// R divider exponent: 0 -> /1, 1 -> /2, ... 7 -> /128.
    pub(super) r_exp: u8,
    pub(super) pll: usize,
}

/// PLL feedback divider plan: fVCO = fXTAL * (a + b/c).
#[derive(Clone, Copy, Debug)]
pub(super) struct PllPlan {
    pub(super) a: u32,
    pub(super) b: u64,
    pub(super) c: u64,
}

pub(super) struct GroupPlan {
    pub(super) pll: PllPlan,
    pub(super) outputs: Vec<(usize, MsPlan)>,
}

pub(super) fn plan_group(
    channels: &[usize],
    freqs: &[u64; 3],
    pll: usize,
) -> Result<GroupPlan, String> {
    let group_freqs: Vec<u64> = channels.iter().map(|&ch| freqs[ch]).collect();
    let vco = group_vco(&group_freqs)?;
    let pll_plan = pll_plan(vco)?;

    let mut outputs = Vec::with_capacity(channels.len());
    for &channel in channels {
        let plan = plan_output(vco, freqs[channel], pll)?;
        outputs.push((channel, plan));
    }

    Ok(GroupPlan {
        pll: pll_plan,
        outputs,
    })
}

/// Choose one VCO frequency that can serve every channel in the group.
fn group_vco(freqs: &[u64]) -> Result<u64, String> {
    let mut high_vco: Option<u64> = None;

    for &freq in freqs {
        if freq > 112_500_000 {
            let divider = smallest_even_divider(freq)?;
            let vco = freq * divider;
            match high_vco {
                Some(existing) if existing != vco => {
                    return Err(format!(
                        "high frequency {freq} Hz needs a different VCO than another output in the same PLL group"
                    ));
                }
                None => high_vco = Some(vco),
                _ => {}
            }
        }
    }

    Ok(high_vco.unwrap_or(VCO_DEFAULT_HZ))
}

/// Smallest even MultiSynth divider that keeps fVCO inside 600..900 MHz.
fn smallest_even_divider(freq: u64) -> Result<u64, String> {
    let mut divider = 4u64;
    loop {
        let vco = freq
            .checked_mul(divider)
            .ok_or_else(|| format!("frequency {freq} Hz is too high"))?;
        if vco >= VCO_MIN_HZ {
            if vco <= VCO_MAX_HZ {
                return Ok(divider);
            }
            return Err(format!("frequency {freq} Hz is too high"));
        }
        divider += 2;
        if divider > 900 {
            return Err(format!("frequency {freq} Hz is too high"));
        }
    }
}

fn pll_plan(vco: u64) -> Result<PllPlan, String> {
    let a = vco / XTAL_HZ;
    let remainder = vco % XTAL_HZ;
    if !(15..=90).contains(&a) {
        return Err(format!("VCO {vco} Hz is outside the PLL range"));
    }

    if remainder == 0 {
        return Ok(PllPlan {
            a: a as u32,
            b: 0,
            c: 1,
        });
    }

    let c = FRAC_DENOM;
    let b = ((remainder as u128 * c as u128 + XTAL_HZ as u128 / 2) / XTAL_HZ as u128) as u64;
    if b >= c {
        Ok(PllPlan {
            a: a as u32 + 1,
            b: 0,
            c: 1,
        })
    } else {
        Ok(PllPlan { a: a as u32, b, c })
    }
}

/// Pick the R divider and MultiSynth divider for `freq` from a given VCO.
fn plan_output(vco: u64, freq: u64, pll: usize) -> Result<MsPlan, String> {
    for r_exp in 0..=7u8 {
        let r = 1u64 << r_exp;
        let scaled = freq
            .checked_mul(r)
            .ok_or_else(|| format!("frequency {freq} Hz is too high"))?;
        let a = vco / scaled;
        let remainder = vco % scaled;

        if a < 4 {
            // Larger R only makes the divider smaller.
            break;
        }
        if a > 900 {
            // Need a larger R divider.
            continue;
        }

        if remainder == 0 && a % 2 == 0 {
            return Ok(MsPlan {
                integer: true,
                a: a as u32,
                b: 0,
                c: 1,
                r_exp,
                pll,
            });
        }

        if a >= 8 {
            let c = FRAC_DENOM;
            let b = ((remainder as u128 * c as u128 + scaled as u128 / 2) / scaled as u128) as u64;
            let (a, b) = if b >= c { (a + 1, 0) } else { (a, b) };
            if a > 900 {
                continue;
            }
            if b == 0 && a % 2 == 0 {
                return Ok(MsPlan {
                    integer: true,
                    a: a as u32,
                    b: 0,
                    c: 1,
                    r_exp,
                    pll,
                });
            }
            return Ok(MsPlan {
                integer: false,
                a: a as u32,
                b,
                c,
                r_exp,
                pll,
            });
        }

        // 4..=7 and not an even integer: this frequency cannot be reached from
        // this VCO. A larger R divider only lowers the divider further.
        break;
    }

    Err(format!("cannot synthesize {freq} Hz from a {vco} Hz VCO"))
}

pub(super) fn divider_params(a: u32, b: u64, c: u64) -> (u32, u32, u32) {
    let c = c.max(1);
    let floor_128 = (b as u128 * 128 / c as u128) as u32;
    let p1 = 128 * a + floor_128 - 512;
    let p2 = (128 * b as u128 - c as u128 * floor_128 as u128) as u32;
    let p3 = c as u32;
    (p1, p2, p3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dividers_for_common_frequencies() {
        let ms = plan_output(VCO_DEFAULT_HZ, 10_000_000, 0).unwrap();
        assert!(ms.integer);
        assert_eq!((ms.a, ms.b, ms.c, ms.r_exp), (90, 0, 1, 0));

        let ms = plan_output(VCO_DEFAULT_HZ, 1_000_000, 0).unwrap();
        assert!(ms.integer);
        assert_eq!(ms.a, 900);

        let ms = plan_output(VCO_DEFAULT_HZ, 8_000, 0).unwrap();
        assert_eq!(ms.r_exp, 7);
        assert!(ms.a >= 8 && ms.a <= 900);

        let ms = plan_output(640_000_000, 160_000_000, 0).unwrap();
        assert!(ms.integer);
        assert_eq!(ms.a, 4);
    }

    #[test]
    fn high_frequency_vco_selection() {
        assert_eq!(group_vco(&[160_000_000]).unwrap(), 640_000_000);
        assert_eq!(group_vco(&[120_000_000]).unwrap(), 720_000_000);
        assert_eq!(group_vco(&[10_000_000, 120_000_000]).unwrap(), 720_000_000);
        assert!(group_vco(&[120_000_000, 160_000_000]).is_err());
    }

    #[test]
    fn register_parameters_match_datasheet_examples() {
        // 10 MHz from 900 MHz: MultiSynth divider 90, P1 = 128*90 - 512.
        let (p1, p2, p3) = divider_params(90, 0, 1);
        assert_eq!((p1, p2, p3), (11008, 0, 1));
    }
}
