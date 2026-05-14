use crate::effects::{Distortion, MultibandComp};
use crate::params::Params;
use crossbeam_queue::ArrayQueue;
use std::f32::consts::PI;
use std::sync::Arc;

pub const NUM_VOICES: usize = 8;

#[derive(Copy, Clone, Debug)]
pub enum NoteEvent {
    On { note: u8, velocity: f32 },
    Off { note: u8 },
    AllOff,
}

#[inline]
fn midi_to_hz(n: f32) -> f32 {
    440.0 * 2f32.powf((n - 69.0) / 12.0)
}

#[inline]
fn fast_tanh(x: f32) -> f32 {
    let x = x.clamp(-3.5, 3.5);
    let x2 = x * x;
    x * (27.0 + x2) / (27.0 + 9.0 * x2)
}

#[inline]
fn poly_blep(t: f32, dt: f32) -> f32 {
    if t < dt {
        let t = t / dt;
        2.0 * t - t * t - 1.0
    } else if t > 1.0 - dt {
        let t = (t - 1.0) / dt;
        t * t + 2.0 * t + 1.0
    } else {
        0.0
    }
}

struct Osc {
    phase: f32,
    rng: u32,
}

impl Osc {
    fn new(seed: u32) -> Self {
        Self {
            phase: 0.0,
            rng: seed.wrapping_mul(2654435761).max(1),
        }
    }

    fn tick(&mut self, freq: f32, sr: f32, wave: i32) -> f32 {
        let dt = (freq / sr).clamp(0.0, 0.49);
        let t = self.phase;
        let out = match wave {
            0 => {
                let mut s = 2.0 * t - 1.0;
                s -= poly_blep(t, dt);
                s
            }
            1 => {
                let mut s = if t < 0.5 { 1.0 } else { -1.0 };
                s += poly_blep(t, dt);
                let t2 = if t + 0.5 >= 1.0 { t - 0.5 } else { t + 0.5 };
                s -= poly_blep(t2, dt);
                s
            }
            2 => {
                if t < 0.5 {
                    4.0 * t - 1.0
                } else {
                    3.0 - 4.0 * t
                }
            }
            3 => (2.0 * PI * t).sin(),
            _ => {
                self.rng ^= self.rng << 13;
                self.rng ^= self.rng >> 17;
                self.rng ^= self.rng << 5;
                (self.rng as f32 / 2147483648.0) - 1.0
            }
        };
        self.phase += dt;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }
        out
    }
}

#[derive(Copy, Clone, PartialEq)]
enum Stage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

struct Adsr {
    stage: Stage,
    value: f32,
    sr: f32,
}

impl Adsr {
    fn new(sr: f32) -> Self {
        Self {
            stage: Stage::Idle,
            value: 0.0,
            sr,
        }
    }

    fn gate_on(&mut self) {
        self.stage = Stage::Attack;
    }

    fn gate_off(&mut self) {
        if self.stage != Stage::Idle {
            self.stage = Stage::Release;
        }
    }

    fn active(&self) -> bool {
        self.stage != Stage::Idle
    }

    fn tick(&mut self, a: f32, d: f32, s: f32, r: f32) -> f32 {
        let coef = |time: f32| -> f32 { 1.0 - (-1.0 / (time.max(0.0005) * self.sr)).exp() };
        match self.stage {
            Stage::Idle => self.value = 0.0,
            Stage::Attack => {
                let c = coef(a);
                // Aim slightly past 1.0 so the curve actually reaches 1.0
                self.value += c * (1.2 - self.value);
                if self.value >= 1.0 {
                    self.value = 1.0;
                    self.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                let c = coef(d);
                self.value += c * (s - self.value);
                if (self.value - s).abs() < 0.001 {
                    self.value = s;
                    self.stage = Stage::Sustain;
                }
            }
            Stage::Sustain => self.value = s,
            Stage::Release => {
                let c = coef(r);
                self.value += c * (0.0 - self.value);
                if self.value < 0.0005 {
                    self.value = 0.0;
                    self.stage = Stage::Idle;
                }
            }
        }
        self.value
    }
}

struct LadderFilter {
    z: [f32; 4],
    sr: f32,
}

impl LadderFilter {
    fn new(sr: f32) -> Self {
        Self { z: [0.0; 4], sr }
    }

    fn process(&mut self, input: f32, cutoff_hz: f32, resonance: f32, drive: f32) -> f32 {
        let fc = cutoff_hz.clamp(20.0, self.sr * 0.45);
        let g = 1.0 - (-2.0 * PI * fc / self.sr).exp();
        let k = 4.0 * resonance.clamp(0.0, 0.98);
        // The signature chonky bit: tanh saturation in the feedback path.
        let pre = input * (1.0 + drive * 4.0);
        let x = fast_tanh(pre - k * self.z[3]);
        self.z[0] += g * (x - self.z[0]);
        self.z[1] += g * (self.z[0] - self.z[1]);
        self.z[2] += g * (self.z[1] - self.z[2]);
        self.z[3] += g * (self.z[2] - self.z[3]);
        self.z[3]
    }
}

struct Voice {
    note: u8,
    velocity: f32,
    freq: f32,
    target_freq: f32,
    osc1: Osc,
    osc2: Osc,
    sub: Osc,
    noise: Osc,
    amp_env: Adsr,
    filt_env: Adsr,
    filter: LadderFilter,
    gate: bool,
    sr: f32,
    age: u64,
}

impl Voice {
    fn new(sr: f32, seed: u32) -> Self {
        Self {
            note: 0,
            velocity: 0.0,
            freq: 0.0,
            target_freq: 0.0,
            osc1: Osc::new(seed),
            osc2: Osc::new(seed ^ 0xa5a5),
            sub: Osc::new(seed ^ 0x5a5a),
            noise: Osc::new(seed ^ 0xdead),
            amp_env: Adsr::new(sr),
            filt_env: Adsr::new(sr),
            filter: LadderFilter::new(sr),
            gate: false,
            sr,
            age: 0,
        }
    }

    fn note_on(&mut self, note: u8, velocity: f32, tick: u64) {
        self.note = note;
        self.velocity = velocity.max(0.05);
        let new_freq = midi_to_hz(note as f32);
        if !self.amp_env.active() {
            self.freq = new_freq;
        }
        self.target_freq = new_freq;
        self.amp_env.gate_on();
        self.filt_env.gate_on();
        self.gate = true;
        self.age = tick;
    }

    fn note_off(&mut self) {
        self.amp_env.gate_off();
        self.filt_env.gate_off();
        self.gate = false;
    }

    fn active(&self) -> bool {
        self.amp_env.active()
    }

    fn tick(&mut self, p: &Params, lfo: f32) -> f32 {
        let glide = p.glide.load();
        if glide > 0.0005 {
            let c = 1.0 - (-1.0 / (glide * self.sr)).exp();
            self.freq += c * (self.target_freq - self.freq);
        } else {
            self.freq = self.target_freq;
        }

        let pitch_lfo_cents = lfo * p.lfo_to_pitch.load() * 100.0;
        let cents_to_mult = |c: f32| 2f32.powf(c / 1200.0);

        let osc2_octave_mult = 2f32.powf(p.osc2_octave.load().round());

        let f1 = self.freq * cents_to_mult(p.osc1_detune.load() + pitch_lfo_cents);
        let f2 = self.freq
            * osc2_octave_mult
            * cents_to_mult(p.osc2_detune.load() + pitch_lfo_cents);
        let fsub = self.freq * 0.5;

        let w1 = p.osc1_wave.load() as i32;
        let w2 = p.osc2_wave.load() as i32;

        let s1 = self.osc1.tick(f1, self.sr, w1) * p.osc1_level.load();
        let s2 = self.osc2.tick(f2, self.sr, w2) * p.osc2_level.load();
        let ssub = self.sub.tick(fsub, self.sr, 1) * p.sub_level.load();
        let snoise = self.noise.tick(0.0, self.sr, 4) * p.noise_level.load();

        let mix = (s1 + s2 + ssub + snoise) * 0.5;

        let fenv = self.filt_env.tick(
            p.fenv_a.load(),
            p.fenv_d.load(),
            p.fenv_s.load(),
            p.fenv_r.load(),
        );
        let amp = self.amp_env.tick(
            p.amp_a.load(),
            p.amp_d.load(),
            p.amp_s.load(),
            p.amp_r.load(),
        );

        let base = p.filter_cutoff.load();
        let env_amt = p.filter_env_amount.load();
        let keytrack = p.filter_keytrack.load();
        let keytrack_offset = (self.note as f32 - 60.0) / 60.0 * keytrack;
        let lfo_amt = lfo * p.lfo_to_cutoff.load();
        let cutoff_norm =
            (base + env_amt * fenv * self.velocity + keytrack_offset + lfo_amt * 0.5).clamp(0.0, 1.0);
        // 20 Hz .. 20 kHz exponential
        let cutoff_hz = 20.0 * 1000f32.powf(cutoff_norm);

        let filtered = self.filter.process(
            mix,
            cutoff_hz,
            p.filter_resonance.load(),
            p.filter_drive.load(),
        );

        filtered * amp * self.velocity
    }
}

pub struct Engine {
    voices: Vec<Voice>,
    queue: Arc<ArrayQueue<NoteEvent>>,
    params: Arc<Params>,
    lfo_phase: f32,
    sr: f32,
    tick_counter: u64,
    dist: Distortion,
    comp: MultibandComp,
}

impl Engine {
    pub fn new(sr: f32, params: Arc<Params>, queue: Arc<ArrayQueue<NoteEvent>>) -> Self {
        Self {
            voices: (0..NUM_VOICES)
                .map(|i| Voice::new(sr, 0xdeadbeef ^ (i as u32 * 7919)))
                .collect(),
            queue,
            params,
            lfo_phase: 0.0,
            sr,
            tick_counter: 0,
            dist: Distortion::new(),
            comp: MultibandComp::new(),
        }
    }

    fn handle(&mut self, ev: NoteEvent) {
        match ev {
            NoteEvent::On { note, velocity } => {
                let mono = self.params.mono.load() > 0.5;
                let tick = self.tick_counter;
                if mono {
                    let still_on = self.voices.iter().any(|v| v.gate);
                    let voice = &mut self.voices[0];
                    voice.note = note;
                    voice.velocity = velocity.max(0.05);
                    let new_freq = midi_to_hz(note as f32);
                    voice.target_freq = new_freq;
                    if !still_on {
                        voice.freq = new_freq;
                        voice.amp_env.gate_on();
                        voice.filt_env.gate_on();
                    }
                    voice.gate = true;
                    voice.age = tick;
                } else {
                    // Reuse a voice already playing the same note, else find idle, else steal oldest.
                    let idx = if let Some(i) =
                        self.voices.iter().position(|v| v.note == note && v.gate)
                    {
                        i
                    } else if let Some(i) = self.voices.iter().position(|v| !v.active()) {
                        i
                    } else {
                        let mut oldest = 0usize;
                        let mut oldest_age = u64::MAX;
                        for (i, v) in self.voices.iter().enumerate() {
                            if v.age < oldest_age {
                                oldest_age = v.age;
                                oldest = i;
                            }
                        }
                        oldest
                    };
                    self.voices[idx].note_on(note, velocity, tick);
                }
            }
            NoteEvent::Off { note } => {
                let mono = self.params.mono.load() > 0.5;
                if mono {
                    if self.voices[0].note == note {
                        self.voices[0].note_off();
                    }
                } else {
                    for v in &mut self.voices {
                        if v.note == note && v.gate {
                            v.note_off();
                        }
                    }
                }
            }
            NoteEvent::AllOff => {
                for v in &mut self.voices {
                    v.note_off();
                }
            }
        }
    }

    pub fn render(&mut self, buf: &mut [f32], channels: usize) {
        while let Some(ev) = self.queue.pop() {
            self.handle(ev);
        }

        let p = &self.params;
        let lfo_rate = p.lfo_rate.load();
        let master = p.master_volume.load();
        let mdrive = p.master_drive.load();

        // Pull effect params once per buffer; they don't change at audio rate.
        let dist_on = p.dist_enabled.load() > 0.5;
        let dist_type = p.dist_type.load().round() as i32;
        let dist_drive = p.dist_drive.load();
        let dist_tone = p.dist_tone.load();
        let dist_mix = p.dist_mix.load();

        let comp_on = p.comp_enabled.load() > 0.5;
        let comp_xl = p.comp_xover_low.load();
        let comp_xh = p.comp_xover_high.load();
        let comp_thr = p.comp_threshold.load();
        let comp_ratio = p.comp_ratio.load();
        let comp_a = p.comp_attack.load();
        let comp_r = p.comp_release.load();
        let comp_gl = p.comp_gain_low.load();
        let comp_gm = p.comp_gain_mid.load();
        let comp_gh = p.comp_gain_high.load();

        for frame in buf.chunks_mut(channels.max(1)) {
            self.lfo_phase += lfo_rate / self.sr;
            if self.lfo_phase >= 1.0 {
                self.lfo_phase -= 1.0;
            }
            let lfo = (2.0 * PI * self.lfo_phase).sin();

            let mut sum = 0.0;
            for v in &mut self.voices {
                if v.active() {
                    sum += v.tick(p, lfo);
                }
            }

            if dist_on {
                sum = self
                    .dist
                    .process(sum, dist_drive, dist_type, dist_tone, dist_mix, self.sr);
            }
            if comp_on {
                sum = self.comp.process(
                    sum, comp_xl, comp_xh, comp_thr, comp_ratio, comp_a, comp_r, comp_gl,
                    comp_gm, comp_gh, self.sr,
                );
            }

            sum *= 1.0 + mdrive * 4.0;
            let out = fast_tanh(sum * 0.3) * master;

            for s in frame.iter_mut() {
                *s = out;
            }
            self.tick_counter = self.tick_counter.wrapping_add(1);
        }
    }
}
