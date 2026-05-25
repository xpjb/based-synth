use std::f32::consts::PI;

#[inline]
fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[inline]
fn lin_to_db(x: f32) -> f32 {
    20.0 * x.max(1e-9).log10()
}

#[inline]
fn soft_clip(x: f32) -> f32 {
    let x = x.clamp(-3.5, 3.5);
    let x2 = x * x;
    x * (27.0 + x2) / (27.0 + 9.0 * x2)
}

#[inline]
fn hard_clip(x: f32) -> f32 {
    x.clamp(-1.0, 1.0)
}

#[inline]
fn fold(x: f32) -> f32 {
    let mut y = x;
    for _ in 0..8 {
        if y > 1.0 {
            y = 2.0 - y;
        } else if y < -1.0 {
            y = -2.0 - y;
        } else {
            break;
        }
    }
    y
}

#[inline]
fn bit_crush(x: f32, bits: f32) -> f32 {
    let levels = (2f32.powf(bits.max(1.0)) * 0.5) - 1.0;
    if levels <= 0.0 {
        return x.signum();
    }
    (x.clamp(-1.0, 1.0) * levels).round() / levels
}

pub struct OnePoleLp {
    z: f32,
}
impl OnePoleLp {
    pub fn new() -> Self {
        Self { z: 0.0 }
    }
    #[inline]
    pub fn process(&mut self, x: f32, fc: f32, sr: f32) -> f32 {
        let fc = fc.clamp(1.0, sr * 0.49);
        let g = 1.0 - (-2.0 * PI * fc / sr).exp();
        self.z += g * (x - self.z);
        self.z
    }
}

pub struct Distortion {
    tone: OnePoleLp,
}

impl Distortion {
    pub fn new() -> Self {
        Self {
            tone: OnePoleLp::new(),
        }
    }

    /// dist_type: 0=Soft, 1=Hard, 2=Fold, 3=Crush
    pub fn process(
        &mut self,
        x: f32,
        drive: f32,
        dist_type: i32,
        tone: f32,
        mix: f32,
        sr: f32,
    ) -> f32 {
        let drive = drive.clamp(0.0, 1.0);
        let pre = 1.0 + drive * 31.0; // up to ~30 dB
        let driven = x * pre;
        let shaped = match dist_type {
            0 => soft_clip(driven),
            1 => hard_clip(driven * 0.5) * 2.0,
            2 => fold(driven * 0.7) * 1.4,
            _ => {
                let bits = (16.0 - drive * 14.0).max(2.0);
                bit_crush(driven * 0.5, bits) * 2.0
            }
        };
        // Rough output level compensation so increasing drive doesn't blow up.
        let normalized = shaped / (1.0 + drive * 3.0);
        let fc = 500.0 * 36f32.powf(tone.clamp(0.0, 1.0));
        let filtered = self.tone.process(normalized, fc, sr);
        let m = mix.clamp(0.0, 1.0);
        m * filtered + (1.0 - m) * x
    }
}

pub struct EnvFollower {
    y: f32,
}
impl EnvFollower {
    pub fn new() -> Self {
        Self { y: 0.0 }
    }
    #[inline]
    pub fn process(&mut self, x: f32, attack: f32, release: f32, sr: f32) -> f32 {
        let xa = x.abs();
        let coef = if xa > self.y {
            (-1.0 / (attack.max(1e-4) * sr)).exp()
        } else {
            (-1.0 / (release.max(1e-3) * sr)).exp()
        };
        self.y = xa + (self.y - xa) * coef;
        self.y
    }
}

pub struct CompBand {
    env: EnvFollower,
}
impl CompBand {
    pub fn new() -> Self {
        Self {
            env: EnvFollower::new(),
        }
    }
    #[inline]
    pub fn process(
        &mut self,
        x: f32,
        threshold_db: f32,
        ratio: f32,
        upward_ratio: f32,
        upward_range_db: f32,
        attack: f32,
        release: f32,
        makeup_db: f32,
        sr: f32,
    ) -> f32 {
        let env = self.env.process(x, attack, release, sr);
        let env_db = lin_to_db(env);
        let over = env_db - threshold_db;
        let gr_db = if over > 0.0 {
            -over * (1.0 - 1.0 / ratio.max(1.0))
        } else {
            let boost = -over * (1.0 - 1.0 / upward_ratio.max(1.0));
            boost.min(upward_range_db.max(0.0))
        };
        x * db_to_lin(gr_db + makeup_db)
    }
}

/// 3-band compressor with complementary 2-pole subtractive crossovers.
/// Bands sum back to input exactly when compression is disabled.
pub struct MultibandComp {
    lp_low_a: OnePoleLp,
    lp_low_b: OnePoleLp,
    lp_mid_a: OnePoleLp,
    lp_mid_b: OnePoleLp,
    low: CompBand,
    mid: CompBand,
    high: CompBand,
}

impl MultibandComp {
    pub fn new() -> Self {
        Self {
            lp_low_a: OnePoleLp::new(),
            lp_low_b: OnePoleLp::new(),
            lp_mid_a: OnePoleLp::new(),
            lp_mid_b: OnePoleLp::new(),
            low: CompBand::new(),
            mid: CompBand::new(),
            high: CompBand::new(),
        }
    }

    pub fn process(
        &mut self,
        x: f32,
        xover_low: f32,
        xover_high: f32,
        threshold_db: f32,
        ratio: f32,
        upward_ratio: f32,
        upward_range_db: f32,
        attack: f32,
        release: f32,
        gain_low_db: f32,
        gain_mid_db: f32,
        gain_high_db: f32,
        sr: f32,
    ) -> f32 {
        // Make sure xover_high is comfortably above xover_low.
        let xl = xover_low.clamp(20.0, sr * 0.45);
        let xh = xover_high.clamp(xl * 1.5, sr * 0.45);

        let low = self
            .lp_low_b
            .process(self.lp_low_a.process(x, xl, sr), xl, sr);
        let above_low = x - low;
        let mid = self
            .lp_mid_b
            .process(self.lp_mid_a.process(above_low, xh, sr), xh, sr);
        let high = above_low - mid;

        let l = self.low.process(low, threshold_db, ratio, upward_ratio, upward_range_db, attack, release, gain_low_db, sr);
        let m = self.mid.process(mid, threshold_db, ratio, upward_ratio, upward_range_db, attack, release, gain_mid_db, sr);
        let h = self.high.process(high, threshold_db, ratio, upward_ratio, upward_range_db, attack, release, gain_high_db, sr);

        l + m + h
    }
}
