use std::ops::{Add, Mul, Sub};

const N: usize = 480;
const BINS: usize = 80;
const FIT: usize = 10;

#[derive(Clone, Copy, Default)]
struct C {
    re: f64,
    im: f64,
}

impl C {
    fn conj(self) -> Self {
        Self {
            re: self.re,
            im: -self.im,
        }
    }
    fn power(self) -> f64 {
        self.re * self.re + self.im * self.im
    }
}

impl Add for C {
    type Output = Self;
    fn add(self, b: Self) -> Self {
        Self {
            re: self.re + b.re,
            im: self.im + b.im,
        }
    }
}
impl Sub for C {
    type Output = Self;
    fn sub(self, b: Self) -> Self {
        Self {
            re: self.re - b.re,
            im: self.im - b.im,
        }
    }
}
impl Mul for C {
    type Output = Self;
    fn mul(self, b: Self) -> Self {
        Self {
            re: self.re * b.re - self.im * b.im,
            im: self.re * b.im + self.im * b.re,
        }
    }
}
impl Mul<f64> for C {
    type Output = Self;
    fn mul(self, b: f64) -> Self {
        Self {
            re: self.re * b,
            im: self.im * b,
        }
    }
}

#[derive(Default)]
pub struct Band {
    weight: f64,
    loss: f64,
    echo_weight: f64,
    echo_loss: f64,
    residual: f64,
    desired: f64,
    windows: usize,
    holes: usize,
    skipped: usize,
    eligible: usize,
}

impl Band {
    pub fn loss_db(&self) -> f64 {
        self.loss / self.weight.max(1e-20)
    }
    pub fn echo_db(&self) -> f64 {
        self.echo_loss / self.echo_weight.max(1e-20)
    }
    pub fn holes(&self) -> f64 {
        self.holes as f64 / self.windows.max(1) as f64
    }
    pub fn skipped(&self) -> f64 {
        self.skipped as f64 / self.eligible.max(1) as f64
    }
    pub fn distortion_db(&self) -> f64 {
        10.0 * (self.residual.max(1e-20) / self.desired.max(1e-20)).log10()
    }
    pub fn windows(&self) -> usize {
        self.windows
    }
}

pub struct Metric {
    basis: Vec<Vec<C>>,
}

impl Metric {
    pub fn new() -> Self {
        let basis = (1..=BINS)
            .map(|k| {
                (0..N)
                    .map(|i| {
                        let angle = -std::f64::consts::TAU * k as f64 * i as f64 / N as f64;
                        let hann = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / N as f64).cos();
                        C {
                            re: angle.cos() * hann,
                            im: angle.sin() * hann,
                        }
                    })
                    .collect()
            })
            .collect();
        Self { basis }
    }

    fn spectrum(&self, x: &[f32]) -> Vec<C> {
        self.basis
            .iter()
            .map(|bin| {
                bin.iter()
                    .zip(x)
                    .fold(C::default(), |sum, (&basis, &sample)| {
                        sum + basis * sample as f64
                    })
            })
            .collect()
    }

    /// 100 ms fits, stepped every 10 ms; switched/transition frames use None.
    /// the standardized two-column gram matrix must have condition number <=100.
    pub fn measure(
        &self,
        near: &[f32],
        echo: &[f32],
        output: &[f32],
        delays: &[Option<usize>],
    ) -> [Band; 3] {
        let frames = near.len().min(echo.len()) / N;
        let ns: Vec<_> = near
            .as_chunks::<N>()
            .0
            .iter()
            .map(|x| self.spectrum(x))
            .collect();
        let es: Vec<_> = echo
            .as_chunks::<N>()
            .0
            .iter()
            .map(|x| self.spectrum(x))
            .collect();
        let ys: Vec<_> = (0..frames)
            .map(|i| {
                delays
                    .get(i)
                    .copied()
                    .flatten()
                    .and_then(|d| output.get(i * N + d..(i + 1) * N + d))
                    .map(|x| self.spectrum(x))
            })
            .collect();
        let near_peak = ns.iter().flatten().map(|c| c.power()).fold(0.0, f64::max);
        let echo_peak = es.iter().flatten().map(|c| c.power()).fold(0.0, f64::max);
        let mut bands = std::array::from_fn::<_, 3, _>(|_| Band::default());
        for start in 0..frames.saturating_sub(FIT + 1) {
            let Some(delay) = delays[start] else { continue };
            if delays[start..start + FIT + 1]
                .iter()
                .any(|&d| d != Some(delay))
                || ys[start..start + FIT].iter().any(Option::is_none)
            {
                continue;
            }
            let mut window_in = [0.0; 3];
            let mut window_out = [0.0; 3];
            for k in 0..BINS {
                let band = if k < 9 {
                    0
                } else if k < 39 {
                    1
                } else {
                    2
                };
                let (mut nn, mut ee, mut ne, mut ny, mut ey) =
                    (0.0, 0.0, C::default(), C::default(), C::default());
                for i in start..start + FIT {
                    let (n, e, y) = (ns[i][k], es[i][k], ys[i].as_ref().unwrap()[k]);
                    nn += n.power();
                    ee += e.power();
                    ne = ne + n.conj() * e;
                    ny = ny + n.conj() * y;
                    ey = ey + e.conj() * y;
                }
                let near_active = nn > FIT as f64 * near_peak * 1e-6 && nn > 1e-16;
                let echo_active = ee > FIT as f64 * echo_peak * 1e-6 && ee > 1e-16;
                if !near_active && !echo_active {
                    continue;
                }
                let b = &mut bands[band];
                b.eligible += 1;
                let (a_gain, e_gain) = if near_active && echo_active {
                    let rho = (ne.power() / (nn * ee)).sqrt().min(1.0);
                    if (1.0 + rho) / (1.0 - rho).max(1e-15) > 100.0 {
                        b.skipped += 1;
                        continue;
                    }
                    let det = nn * ee - ne.power();
                    (
                        (ny * ee - ne * ey) * (1.0 / det),
                        (ey * nn - ne.conj() * ny) * (1.0 / det),
                    )
                } else if near_active {
                    (ny * (1.0 / nn), C::default())
                } else {
                    (C::default(), ey * (1.0 / ee))
                };
                if near_active {
                    b.weight += nn;
                    b.loss += -10.0 * a_gain.power().max(1e-18).log10() * nn;
                    b.desired += a_gain.power() * nn;
                    window_in[band] += nn;
                    window_out[band] += a_gain.power() * nn;
                }
                if echo_active {
                    b.echo_weight += ee;
                    b.echo_loss += -10.0 * e_gain.power().max(1e-18).log10() * ee;
                }
                for i in start..start + FIT {
                    let residual =
                        ys[i].as_ref().unwrap()[k] - a_gain * ns[i][k] - e_gain * es[i][k];
                    b.residual += residual.power();
                }
            }
            for k in 0..3 {
                if window_in[k] > 0.0 {
                    bands[k].windows += 1;
                    bands[k].holes += usize::from(window_out[k] < window_in[k] * 0.25);
                }
            }
        }
        bands
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separates_identity_echo_removal_and_band_stop() {
        let mut seed = 71u32;
        let mut random = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            0.02 + (seed as f64 / u32::MAX as f64) * 0.1
        };
        let (mut near, mut echo, mut low_near) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..100 {
            let na = [random(), random(), random()];
            let ea = [random(), random(), random()];
            for i in 0..N {
                let tones = [400.0, 2000.0, 6000.0]
                    .map(|f| std::f64::consts::TAU * f * i as f64 / 48_000.0);
                near.push(tones.iter().zip(na).map(|(p, a)| p.sin() * a).sum::<f64>() as f32);
                echo.push(tones.iter().zip(ea).map(|(p, a)| p.cos() * a).sum::<f64>() as f32);
                low_near.push((tones[0].sin() * na[0]) as f32);
            }
        }
        let metric = Metric::new();
        let delays = vec![Some(0); near.len() / N];
        for factor in [1.0, 0.1, 0.0] {
            let out: Vec<f32> = near
                .iter()
                .zip(&echo)
                .map(|(n, e)| n + e * factor)
                .collect();
            for band in metric.measure(&near, &echo, &out, &delays) {
                assert!(band.windows > 0);
                assert!(band.loss_db().abs() < 0.01, "near loss {}", band.loss_db());
                if factor == 0.0 {
                    assert!(band.echo_db() > 100.0);
                } else {
                    assert!((band.echo_db() + 20.0 * (factor as f64).log10()).abs() < 0.01);
                }
                assert!(band.distortion_db() < -80.0);
            }
        }
        let out: Vec<f32> = low_near.iter().zip(&echo).map(|(n, e)| n + e).collect();
        let bands = metric.measure(&near, &echo, &out, &delays);
        assert!(bands[0].loss_db().abs() < 0.01);
        assert!(bands[1].loss_db() > 100.0 && bands[2].loss_db() > 100.0);
        assert!(bands[1].holes() > 0.99 && bands[2].holes() > 0.99);
        for band in metric.measure(&near, &near, &near, &delays) {
            assert_eq!(band.windows(), 0);
            assert!(
                band.skipped() > 0.99,
                "collinear references were treated as separable"
            );
        }
        let mixed: Vec<f32> = near.iter().zip(&echo).map(|(n, e)| n + e).collect();
        let shifted: Vec<f32> = std::iter::repeat_n(0.0, 436).chain(mixed).collect();
        for band in metric.measure(&near, &echo, &shifted, &vec![Some(436); delays.len()]) {
            assert!(band.loss_db().abs() < 0.01 && band.echo_db().abs() < 0.01);
        }
    }
}
