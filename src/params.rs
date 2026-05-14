use std::sync::atomic::{AtomicU32, Ordering};

pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub const fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    #[inline]
    pub fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    #[inline]
    pub fn store(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
}

macro_rules! define_params {
    ($($name:ident : $default:expr),* $(,)?) => {
        pub struct Params {
            $(pub $name: AtomicF32,)*
        }

        impl Default for Params {
            fn default() -> Self {
                Self {
                    $($name: AtomicF32::new($default),)*
                }
            }
        }

        impl Params {
            pub fn set(&self, name: &str, value: f32) -> bool {
                match name {
                    $(stringify!($name) => { self.$name.store(value); true },)*
                    _ => false,
                }
            }

            pub fn to_patch(&self) -> Patch {
                Patch {
                    $($name: self.$name.load(),)*
                }
            }

            pub fn apply_patch(&self, patch: &Patch) {
                $(self.$name.store(patch.$name);)*
            }
        }

        #[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
        pub struct Patch {
            $(#[serde(default)] pub $name: f32,)*
        }

        impl Default for Patch {
            fn default() -> Self {
                Self {
                    $($name: $default,)*
                }
            }
        }
    };
}

define_params! {
    osc1_wave: 0.0,
    osc1_detune: -7.0,
    osc1_level: 0.7,
    osc2_wave: 0.0,
    osc2_detune: 7.0,
    osc2_level: 0.7,
    osc2_octave: 0.0,
    sub_level: 0.5,
    noise_level: 0.0,
    filter_cutoff: 0.45,
    filter_resonance: 0.35,
    filter_env_amount: 0.55,
    filter_drive: 0.4,
    filter_keytrack: 0.3,
    amp_a: 0.005,
    amp_d: 0.2,
    amp_s: 0.7,
    amp_r: 0.3,
    fenv_a: 0.005,
    fenv_d: 0.25,
    fenv_s: 0.2,
    fenv_r: 0.3,
    lfo_rate: 2.0,
    lfo_to_cutoff: 0.0,
    lfo_to_pitch: 0.0,
    master_volume: 0.55,
    master_drive: 0.2,
    glide: 0.0,
    mono: 0.0,
    chord_type: 0.0,
    arp_enabled: 0.0,
    arp_pattern: 0.0,
    arp_rate: 6.0,
    arp_gate: 0.5,
    arp_octaves: 1.0,
}
