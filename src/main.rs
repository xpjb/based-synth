mod params;
mod synth;
mod web;

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_queue::ArrayQueue;
use params::{Params, Patch};
use std::path::PathBuf;
use std::sync::Arc;
use synth::{Engine, NoteEvent};
use web::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    let params = Arc::new(Params::default());
    let queue: Arc<ArrayQueue<NoteEvent>> = Arc::new(ArrayQueue::new(512));

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("No default audio output device"))?;
    let supported = device.default_output_config()?;
    let sample_format = supported.sample_format();
    let stream_config: cpal::StreamConfig = supported.into();
    let channels = stream_config.channels as usize;
    let sample_rate = stream_config.sample_rate.0 as f32;

    println!(
        "[chonk] audio device: {}",
        device.name().unwrap_or_else(|_| "<unknown>".into())
    );
    println!(
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
        other => return Err(anyhow::anyhow!("unsupported sample format: {:?}", other)),
    };

    stream.play()?;

    let patches_dir = PathBuf::from("patches");
    std::fs::create_dir_all(&patches_dir).ok();
    write_factory_patches(&patches_dir, &params)?;

    let state = AppState {
        params: params.clone(),
        queue: queue.clone(),
        patches_dir,
    };

    let app = web::router(state);

    let addr = "127.0.0.1:3030";
    println!("[chonk] open http://{} to play", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn write_factory_patches(dir: &PathBuf, params: &Arc<Params>) -> Result<()> {
    let factory: Vec<(&str, Patch)> = vec![
        (
            "init",
            Patch {
                osc1_wave: 0.0,
                osc1_detune: 0.0,
                osc1_level: 0.8,
                osc2_wave: 0.0,
                osc2_detune: 0.0,
                osc2_level: 0.0,
                osc2_octave: 0.0,
                sub_level: 0.0,
                noise_level: 0.0,
                filter_cutoff: 0.5,
                filter_resonance: 0.1,
                filter_env_amount: 0.0,
                filter_drive: 0.0,
                filter_keytrack: 0.0,
                amp_a: 0.005,
                amp_d: 0.2,
                amp_s: 0.8,
                amp_r: 0.2,
                fenv_a: 0.005,
                fenv_d: 0.2,
                fenv_s: 0.0,
                fenv_r: 0.2,
                lfo_rate: 2.0,
                lfo_to_cutoff: 0.0,
                lfo_to_pitch: 0.0,
                master_volume: 0.5,
                master_drive: 0.0,
                glide: 0.0,
                mono: 0.0,
            },
        ),
        (
            "fat_bass",
            Patch {
                osc1_wave: 0.0,
                osc1_detune: -9.0,
                osc1_level: 0.7,
                osc2_wave: 0.0,
                osc2_detune: 9.0,
                osc2_level: 0.7,
                osc2_octave: 0.0,
                sub_level: 0.8,
                noise_level: 0.0,
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
                lfo_rate: 2.0,
                lfo_to_cutoff: 0.0,
                lfo_to_pitch: 0.0,
                master_volume: 0.6,
                master_drive: 0.35,
                glide: 0.0,
                mono: 1.0,
            },
        ),
        (
            "acid_lead",
            Patch {
                osc1_wave: 0.0,
                osc1_detune: 0.0,
                osc1_level: 1.0,
                osc2_wave: 1.0,
                osc2_detune: 0.0,
                osc2_level: 0.0,
                osc2_octave: 0.0,
                sub_level: 0.0,
                noise_level: 0.0,
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
                lfo_to_cutoff: 0.0,
                lfo_to_pitch: 0.0,
                master_volume: 0.55,
                master_drive: 0.5,
                glide: 0.06,
                mono: 1.0,
            },
        ),
        (
            "super_pad",
            Patch {
                osc1_wave: 0.0,
                osc1_detune: -11.0,
                osc1_level: 0.55,
                osc2_wave: 0.0,
                osc2_detune: 12.0,
                osc2_level: 0.55,
                osc2_octave: 0.0,
                sub_level: 0.25,
                noise_level: 0.0,
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
                lfo_to_pitch: 0.0,
                master_volume: 0.45,
                master_drive: 0.1,
                glide: 0.0,
                mono: 0.0,
            },
        ),
        (
            "pluck",
            Patch {
                osc1_wave: 0.0,
                osc1_detune: 0.0,
                osc1_level: 0.7,
                osc2_wave: 2.0,
                osc2_detune: 0.0,
                osc2_level: 0.4,
                osc2_octave: 1.0,
                sub_level: 0.3,
                noise_level: 0.0,
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
                lfo_rate: 2.0,
                lfo_to_cutoff: 0.0,
                lfo_to_pitch: 0.0,
                master_volume: 0.55,
                master_drive: 0.2,
                glide: 0.0,
                mono: 0.0,
            },
        ),
    ];

    let mut wrote_init = false;
    for (name, patch) in &factory {
        let path = dir.join(format!("{}.json", name));
        if !path.exists() {
            let json = serde_json::to_string_pretty(&patch)?;
            std::fs::write(&path, json)?;
        }
        if *name == "fat_bass" {
            // Set the running params to "fat_bass" as the welcome sound.
            params.apply_patch(patch);
            wrote_init = true;
        }
    }
    if !wrote_init {
        // fallback so we at least have something
    }
    Ok(())
}
