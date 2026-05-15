use crate::params::Params;
use crate::synth::NoteEvent;
use crossbeam_queue::ArrayQueue;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, Notify};

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Broadcast {
    Notes { sounding: Vec<u8>, held: Vec<u8> },
}

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
    broadcast: broadcast::Sender<Broadcast>,
}

impl Performer {
    pub fn new(
        params: Arc<Params>,
        queue: Arc<ArrayQueue<NoteEvent>>,
        broadcast: broadcast::Sender<Broadcast>,
    ) -> Arc<Self> {
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
            broadcast,
        })
    }

    fn broadcast_state(&self) {
        // `held` = the full set of notes the synth "wants on" right now (chord pool in arp,
        // chord-expanded notes when not arping). `sounding` = the actual currently-on note(s).
        // UI shows held as dim, sounding as bright. (Always identical when arp is off.)
        let (sounding, held) = {
            let st = self.state.lock().unwrap();
            let arp = self.params.arp_enabled.load() > 0.5;
            if arp {
                let pool = self.build_pool(&st.arp_keys);
                let sounding = st.arp_current.into_iter().collect();
                (sounding, pool)
            } else {
                let mut s: Vec<u8> = Vec::new();
                for v in st.chord_sounding.values() {
                    for &n in v {
                        if !s.contains(&n) {
                            s.push(n);
                        }
                    }
                }
                s.sort();
                (s.clone(), s)
            }
        };
        let _ = self.broadcast.send(Broadcast::Notes { sounding, held });
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
            {
                let mut st = self.state.lock().unwrap();
                if !st.arp_keys.contains(&note) {
                    st.arp_keys.push(note);
                }
            }
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
        self.broadcast_state();
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
        self.broadcast_state();
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
        self.broadcast_state();
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
        enum Step {
            Play {
                note: u8,
                period: Duration,
                gate_dur: Duration,
            },
            Idle {
                state_changed: bool,
            },
        }

        loop {
            let enabled = self.params.arp_enabled.load() > 0.5;

            let action = {
                let mut st = self.state.lock().unwrap();
                if !enabled || st.arp_keys.is_empty() {
                    let prev = st.arp_current.take();
                    if let Some(n) = prev {
                        let _ = self.queue.push(NoteEvent::Off { note: n });
                    }
                    Step::Idle {
                        state_changed: prev.is_some(),
                    }
                } else {
                    let played = self.build_pool(&st.arp_keys);
                    let mut sorted = played.clone();
                    sorted.sort();

                    let pattern = self.params.arp_pattern.load().round() as i32;
                    let octaves = (self.params.arp_octaves.load().round() as i32).clamp(1, 4);

                    let pool: &[u8] = if pattern == 4 { &played } else { &sorted };
                    let len = pool.len() as i64;
                    if len == 0 {
                        Step::Idle {
                            state_changed: false,
                        }
                    } else {
                        let total = len * octaves as i64;
                        let idx = match pattern {
                            1 => {
                                let i = total - 1 - st.arp_step.rem_euclid(total);
                                st.arp_step = st.arp_step.wrapping_add(1);
                                i
                            }
                            2 => {
                                let period = (total * 2 - 2).max(1);
                                let p = st.arp_step.rem_euclid(period);
                                st.arp_step = st.arp_step.wrapping_add(1);
                                if p < total { p } else { period - p }
                            }
                            3 => {
                                st.rng ^= st.rng << 13;
                                st.rng ^= st.rng >> 17;
                                st.rng ^= st.rng << 5;
                                st.arp_step = st.arp_step.wrapping_add(1);
                                (st.rng as i64).rem_euclid(total)
                            }
                            _ => {
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

                        // Release previous arp note before triggering the new one. (Sending Off
                        // again at gate-end is a no-op once the engine has already released it.)
                        if let Some(p) = st.arp_current {
                            let _ = self.queue.push(NoteEvent::Off { note: p });
                        }
                        let _ = self.queue.push(NoteEvent::On {
                            note,
                            velocity: 0.85,
                        });
                        st.arp_current = Some(note);

                        Step::Play {
                            note,
                            period,
                            gate_dur,
                        }
                    }
                }
            };

            match action {
                Step::Play {
                    note,
                    period,
                    gate_dur,
                } => {
                    self.broadcast_state();
                    tokio::time::sleep(gate_dur).await;
                    // Audible release; arp_current stays set so the UI keeps showing it
                    // for the remainder of the step. Cleared on the next step start.
                    let _ = self.queue.push(NoteEvent::Off { note });
                    tokio::time::sleep(period.saturating_sub(gate_dur)).await;
                }
                Step::Idle { state_changed } => {
                    if state_changed {
                        self.broadcast_state();
                    }
                    tokio::select! {
                        _ = self.wake.notified() => {}
                        _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                    }
                }
            }
        }
    }
}
