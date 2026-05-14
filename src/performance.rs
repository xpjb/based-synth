use crate::params::Params;
use crate::synth::NoteEvent;
use crossbeam_queue::ArrayQueue;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

// Chord interval tables. Index 0 is "Off" (single note, no expansion).
pub const CHORDS: &[(&str, &[i8])] = &[
    ("Off",   &[0]),
    ("Oct",   &[0, 12]),
    ("5th",   &[0, 7]),
    ("Maj",   &[0, 4, 7]),
    ("Min",   &[0, 3, 7]),
    ("Sus2",  &[0, 2, 7]),
    ("Sus4",  &[0, 5, 7]),
    ("Maj7",  &[0, 4, 7, 11]),
    ("Min7",  &[0, 3, 7, 10]),
    ("Dom7",  &[0, 4, 7, 10]),
    ("Dim",   &[0, 3, 6]),
    ("Aug",   &[0, 4, 8]),
    ("Add9",  &[0, 4, 7, 14]),
    ("Min9",  &[0, 3, 7, 10, 14]),
];

// Arp pattern indices: 0=Up, 1=Down, 2=UpDown, 3=Random, 4=AsPlayed.

struct PerfState {
    // arp OFF: user_key -> notes currently sounding (so we can release them on key up).
    chord_sounding: HashMap<u8, Vec<u8>>,
    // arp ON: user_keys held in press order (used for the AsPlayed pattern).
    arp_keys: Vec<u8>,
    arp_step: i64,
    arp_current: Option<u8>,
    rng: u32,
}

pub struct Performer {
    state: Mutex<PerfState>,
    params: Arc<Params>,
    queue: Arc<ArrayQueue<NoteEvent>>,
    wake: Notify,
}

impl Performer {
    pub fn new(params: Arc<Params>, queue: Arc<ArrayQueue<NoteEvent>>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PerfState {
                chord_sounding: HashMap::new(),
                arp_keys: Vec::new(),
                arp_step: 0,
                arp_current: None,
                rng: 0x1357bdf,
            }),
            params,
            queue,
            wake: Notify::new(),
        })
    }

    fn chord_intervals(&self) -> &'static [i8] {
        let idx = self.params.chord_type.load().round() as usize;
        let idx = idx.min(CHORDS.len() - 1);
        CHORDS[idx].1
    }

    fn expand_chord(&self, root: u8) -> Vec<u8> {
        self.chord_intervals()
            .iter()
            .filter_map(|&i| {
                let n = root as i16 + i as i16;
                if (0..=127).contains(&n) {
                    Some(n as u8)
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn note_on(&self, note: u8, velocity: f32) {
        let arp = self.params.arp_enabled.load() > 0.5;
        if arp {
            let mut st = self.state.lock().unwrap();
            if !st.arp_keys.contains(&note) {
                st.arp_keys.push(note);
            }
            drop(st);
            self.wake.notify_one();
        } else {
            let notes = self.expand_chord(note);
            for n in &notes {
                let _ = self.queue.push(NoteEvent::On {
                    note: *n,
                    velocity,
                });
            }
            self.state
                .lock()
                .unwrap()
                .chord_sounding
                .insert(note, notes);
        }
    }

    pub fn note_off(&self, note: u8) {
        let arp = self.params.arp_enabled.load() > 0.5;
        if arp {
            let mut st = self.state.lock().unwrap();
            st.arp_keys.retain(|&n| n != note);
            if st.arp_keys.is_empty() {
                if let Some(c) = st.arp_current.take() {
                    let _ = self.queue.push(NoteEvent::Off { note: c });
                }
            }
        } else {
            let mut st = self.state.lock().unwrap();
            if let Some(notes) = st.chord_sounding.remove(&note) {
                for n in notes {
                    let _ = self.queue.push(NoteEvent::Off { note: n });
                }
            }
        }
    }

    pub fn all_off(&self) {
        {
            let mut st = self.state.lock().unwrap();
            st.chord_sounding.clear();
            st.arp_keys.clear();
            st.arp_current = None;
            st.arp_step = 0;
        }
        let _ = self.queue.push(NoteEvent::AllOff);
    }

    fn build_pool(&self, keys: &[u8]) -> Vec<u8> {
        let mut pool: Vec<u8> = Vec::new();
        for &k in keys {
            for n in self.expand_chord(k) {
                if !pool.contains(&n) {
                    pool.push(n);
                }
            }
        }
        pool
    }

    pub async fn run_arp(self: Arc<Self>) {
        loop {
            let enabled = self.params.arp_enabled.load() > 0.5;

            // Compute action under one short lock; do timing-related awaits outside.
            let action = {
                let mut st = self.state.lock().unwrap();
                if !enabled || st.arp_keys.is_empty() {
                    if let Some(n) = st.arp_current.take() {
                        let _ = self.queue.push(NoteEvent::Off { note: n });
                    }
                    None
                } else {
                    let played = self.build_pool(&st.arp_keys);
                    let mut sorted = played.clone();
                    sorted.sort();

                    let pattern = self.params.arp_pattern.load().round() as i32;
                    let octaves = (self.params.arp_octaves.load().round() as i32).clamp(1, 4);

                    let pool: &[u8] = if pattern == 4 { &played } else { &sorted };
                    let len = pool.len() as i64;
                    if len == 0 {
                        None
                    } else {
                        let total = len * octaves as i64;
                        let idx = match pattern {
                            1 => {
                                // Down
                                let i = total - 1 - st.arp_step.rem_euclid(total);
                                st.arp_step = st.arp_step.wrapping_add(1);
                                i
                            }
                            2 => {
                                // UpDown — no repeated endpoints
                                let period = (total * 2 - 2).max(1);
                                let p = st.arp_step.rem_euclid(period);
                                st.arp_step = st.arp_step.wrapping_add(1);
                                if p < total { p } else { period - p }
                            }
                            3 => {
                                // Random
                                st.rng ^= st.rng << 13;
                                st.rng ^= st.rng >> 17;
                                st.rng ^= st.rng << 5;
                                st.arp_step = st.arp_step.wrapping_add(1);
                                (st.rng as i64).rem_euclid(total)
                            }
                            _ => {
                                // Up / AsPlayed share Up indexing logic; pool selection differs above.
                                let i = st.arp_step.rem_euclid(total);
                                st.arp_step = st.arp_step.wrapping_add(1);
                                i
                            }
                        };
                        let oct = (idx / len) as i16;
                        let base = pool[(idx % len) as usize] as i16;
                        let note = (base + oct * 12).clamp(0, 127) as u8;

                        let rate = self.params.arp_rate.load().max(0.1);
                        let gate = self.params.arp_gate.load().clamp(0.05, 0.95);
                        let period = Duration::from_secs_f32(1.0 / rate);
                        let gate_dur = Duration::from_secs_f32(gate / rate);

                        if let Some(p) = st.arp_current.take() {
                            let _ = self.queue.push(NoteEvent::Off { note: p });
                        }
                        let _ = self.queue.push(NoteEvent::On {
                            note,
                            velocity: 0.85,
                        });
                        st.arp_current = Some(note);

                        Some((note, period, gate_dur))
                    }
                }
            };

            match action {
                Some((_, period, gate_dur)) => {
                    tokio::time::sleep(gate_dur).await;
                    {
                        let mut st = self.state.lock().unwrap();
                        if let Some(n) = st.arp_current.take() {
                            let _ = self.queue.push(NoteEvent::Off { note: n });
                        }
                    }
                    let rest = period.saturating_sub(gate_dur);
                    if !rest.is_zero() {
                        tokio::time::sleep(rest).await;
                    }
                }
                None => {
                    tokio::select! {
                        _ = self.wake.notified() => {}
                        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                    }
                }
            }
        }
    }
}
