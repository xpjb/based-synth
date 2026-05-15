mod commands;
mod effects;
mod ipc;
mod params;
mod performance;
mod synth;

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_queue::ArrayQueue;
use ipc::AppState;
use params::{Params, Patch};
use performance::Performer;
use std::path::Path;
use std::sync::Arc;
use synth::{Engine, NoteEvent};
use tauri::{Emitter, Manager};
use tokio::sync::broadcast;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    if let Err(e) = run_inner() {
        eprintln!("[chonk] fatal: {:#}", e);
        std::process::exit(1);
    }
}

fn run_inner() -> Result<()> {
    tauri::Builder::default()
        .setup(|app| {
            let patches_dir = std::env::current_dir()
                .map_err(|e| anyhow::anyhow!("could not get working directory: {e}"))?
                .join("patches");
            std::fs::create_dir_all(&patches_dir)?;
            eprintln!("[chonk] patches dir: {}", patches_dir.display());

            let params = Arc::new(Params::default());
            write_factory_patches(&patches_dir, &params)?;

            let queue: Arc<ArrayQueue<NoteEvent>> = Arc::new(ArrayQueue::new(512));

            let host = cpal::default_host();
            let device = host
                .default_output_device()
                .ok_or_else(|| anyhow::anyhow!("no default audio output device"))?;
            let supported = device.default_output_config()?;
            let sample_format = supported.sample_format();
            let stream_config: cpal::StreamConfig = supported.into();
            let channels = stream_config.channels as usize;
            let sample_rate = stream_config.sample_rate.0 as f32;

            eprintln!(
                "[chonk] audio device: {}",
                device.name().unwrap_or_else(|_| "<unknown>".into())
            );
            eprintln!(
                "[chonk] {} Hz, {} channels, fmt {:?}",
                sample_rate, channels, sample_format
            );

            let engine = Engine::new(sample_rate, params.clone(), queue.clone());
            let err_fn = |err| eprintln!("[chonk] stream error: {}", err);

            let stream = match sample_format {
                cpal::SampleFormat::F32 => {
                    let mut engine = engine;
                    device.build_output_stream(
                        &stream_config,
                        move |data: &mut [f32], _| engine.render(data, channels),
                        err_fn,
                        None,
                    )?
                }
                cpal::SampleFormat::I16 => {
                    let mut engine = engine;
                    let mut scratch: Vec<f32> = Vec::new();
                    device.build_output_stream(
                        &stream_config,
                        move |data: &mut [i16], _| {
                            scratch.resize(data.len(), 0.0);
                            engine.render(&mut scratch, channels);
                            for (d, s) in data.iter_mut().zip(scratch.iter()) {
                                let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                                *d = v;
                            }
                        },
                        err_fn,
                        None,
                    )?
                }
                cpal::SampleFormat::U16 => {
                    let mut engine = engine;
                    let mut scratch: Vec<f32> = Vec::new();
                    device.build_output_stream(
                        &stream_config,
                        move |data: &mut [u16], _| {
                            scratch.resize(data.len(), 0.0);
                            engine.render(&mut scratch, channels);
                            for (d, s) in data.iter_mut().zip(scratch.iter()) {
                                let n = (s.clamp(-1.0, 1.0) * 0.5 + 0.5) * u16::MAX as f32;
                                *d = n as u16;
                            }
                        },
                        err_fn,
                        None,
                    )?
                }
                other => return Err(anyhow::anyhow!("unsupported sample format: {:?}", other).into()),
            };

            stream.play()?;
            std::mem::forget(stream);

            let (broadcast_tx, _) = broadcast::channel(128);
            let performer = Performer::new(params.clone(), queue.clone(), broadcast_tx.clone());
            tauri::async_runtime::spawn(performer.clone().run_arp());

            let app_handle = app.handle().clone();
            let mut bridge_rx = broadcast_tx.subscribe();
            tauri::async_runtime::spawn(async move {
                loop {
                    match bridge_rx.recv().await {
                        Ok(b) => {
                            let _ = app_handle.emit("notes", &b);
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });

            let state = AppState {
                params,
                performer,
                patches_dir,
            };
            app.manage(Arc::new(state));

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![commands::dispatch])
        .run(tauri::generate_context!())
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    Ok(())
}

fn write_factory_patches(dir: &Path, params: &Arc<Params>) -> Result<()> {
    let factory: Vec<(&str, Patch)> = vec![
        (
            "init",
            Patch {
                osc1_level: 0.8,
                osc2_level: 0.0,
                sub_level: 0.0,
                filter_cutoff: 0.5,
                filter_resonance: 0.1,
                filter_env_amount: 0.0,
                filter_drive: 0.0,
                filter_keytrack: 0.0,
                amp_s: 0.8,
                amp_r: 0.2,
                fenv_s: 0.0,
                fenv_r: 0.2,
                master_volume: 0.5,
                master_drive: 0.0,
                ..Default::default()
            },
        ),
        (
            "fat_bass",
            Patch {
                osc1_detune: -9.0,
                osc2_detune: 9.0,
                sub_level: 0.8,
                filter_cutoff: 0.28,
                filter_resonance: 0.45,
                filter_env_amount: 0.55,
                filter_drive: 0.6,
                filter_keytrack: 0.25,
                amp_a: 0.002,
                amp_d: 0.4,
                amp_s: 0.55,
                amp_r: 0.15,
                fenv_a: 0.002,
                fenv_d: 0.18,
                fenv_s: 0.1,
                fenv_r: 0.15,
                master_volume: 0.6,
                master_drive: 0.35,
                mono: 1.0,
                ..Default::default()
            },
        ),
        (
            "acid_lead",
            Patch {
                osc1_detune: 0.0,
                osc1_level: 1.0,
                osc2_wave: 1.0,
                osc2_detune: 0.0,
                osc2_level: 0.0,
                sub_level: 0.0,
                filter_cutoff: 0.32,
                filter_resonance: 0.82,
                filter_env_amount: 0.65,
                filter_drive: 0.55,
                filter_keytrack: 0.4,
                amp_a: 0.002,
                amp_d: 0.35,
                amp_s: 0.0,
                amp_r: 0.08,
                fenv_a: 0.002,
                fenv_d: 0.28,
                fenv_s: 0.0,
                fenv_r: 0.18,
                lfo_rate: 4.0,
                master_volume: 0.55,
                master_drive: 0.5,
                glide: 0.06,
                mono: 1.0,
                ..Default::default()
            },
        ),
        (
            "super_pad",
            Patch {
                osc1_detune: -11.0,
                osc1_level: 0.55,
                osc2_detune: 12.0,
                osc2_level: 0.55,
                sub_level: 0.25,
                filter_cutoff: 0.55,
                filter_resonance: 0.25,
                filter_env_amount: 0.2,
                filter_drive: 0.2,
                filter_keytrack: 0.4,
                amp_a: 0.6,
                amp_d: 0.5,
                amp_s: 0.75,
                amp_r: 1.2,
                fenv_a: 0.7,
                fenv_d: 0.6,
                fenv_s: 0.4,
                fenv_r: 1.0,
                lfo_rate: 0.3,
                lfo_to_cutoff: 0.15,
                master_volume: 0.45,
                master_drive: 0.1,
                ..Default::default()
            },
        ),
        (
            "pluck",
            Patch {
                osc1_level: 0.7,
                osc1_detune: 0.0,
                osc2_wave: 2.0,
                osc2_detune: 0.0,
                osc2_level: 0.4,
                osc2_octave: 1.0,
                sub_level: 0.3,
                filter_cutoff: 0.4,
                filter_resonance: 0.4,
                filter_env_amount: 0.55,
                filter_drive: 0.3,
                filter_keytrack: 0.5,
                amp_a: 0.002,
                amp_d: 0.18,
                amp_s: 0.0,
                amp_r: 0.18,
                fenv_a: 0.002,
                fenv_d: 0.12,
                fenv_s: 0.0,
                fenv_r: 0.18,
                master_volume: 0.55,
                master_drive: 0.2,
                ..Default::default()
            },
        ),
        (
            "crunch_lead",
            Patch {
                osc1_detune: -8.0,
                osc2_detune: 8.0,
                sub_level: 0.4,
                filter_cutoff: 0.55,
                filter_resonance: 0.3,
                filter_env_amount: 0.4,
                filter_drive: 0.3,
                filter_keytrack: 0.4,
                amp_a: 0.003,
                amp_d: 0.3,
                amp_s: 0.7,
                amp_r: 0.2,
                fenv_a: 0.003,
                fenv_d: 0.2,
                fenv_s: 0.2,
                fenv_r: 0.2,
                master_volume: 0.45,
                master_drive: 0.1,
                dist_enabled: 1.0,
                dist_type: 0.0,
                dist_drive: 0.55,
                dist_tone: 0.55,
                dist_mix: 0.85,
                comp_enabled: 1.0,
                comp_xover_low: 220.0,
                comp_xover_high: 2200.0,
                comp_threshold: -22.0,
                comp_ratio: 5.0,
                comp_attack: 0.004,
                comp_release: 0.12,
                comp_gain_low: 2.0,
                comp_gain_mid: 0.0,
                comp_gain_high: 1.5,
                ..Default::default()
            },
        ),
        (
            "arp_house",
            Patch {
                osc1_detune: -5.0,
                osc2_detune: 5.0,
                sub_level: 0.3,
                filter_cutoff: 0.45,
                filter_resonance: 0.4,
                filter_env_amount: 0.45,
                filter_drive: 0.3,
                filter_keytrack: 0.5,
                amp_a: 0.002,
                amp_d: 0.15,
                amp_s: 0.0,
                amp_r: 0.1,
                fenv_a: 0.002,
                fenv_d: 0.12,
                fenv_s: 0.0,
                fenv_r: 0.1,
                master_volume: 0.5,
                master_drive: 0.2,
                chord_type: 3.0,
                arp_enabled: 1.0,
                arp_pattern: 0.0,
                arp_rate: 8.0,
                arp_gate: 0.45,
                arp_octaves: 2.0,
                ..Default::default()
            },
        ),
    ];

    for (name, patch) in &factory {
        let path = dir.join(format!("{}.json", name));
        if !path.exists() {
            let json = serde_json::to_string_pretty(&patch)?;
            std::fs::write(&path, json)?;
        }
        if *name == "fat_bass" {
            params.apply_patch(patch);
        }
    }
    Ok(())
}
