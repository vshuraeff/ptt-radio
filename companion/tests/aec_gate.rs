//! offline measurements of the gated path; no device or relay is involved.
//! PTT_LAB_A=a.wav PTT_LAB_B=b.wav cargo test --release --test aec_gate -- --ignored --nocapture

use std::path::Path;
use std::time::Instant;

use ptt_radio_companion::echo::{ALGORITHMIC_DELAY_MS, EchoPath};

#[path = "aec_gate/metric.rs"]
mod metric;

const RATE: usize = 48_000;
const BLOCK: usize = 480;

fn voice(var: &str) -> Vec<f32> {
    let path = std::env::var(var).unwrap_or_else(|_| panic!("set {var}"));
    let mut wav = hound::WavReader::open(path).unwrap();
    assert_eq!(wav.spec().channels, 1);
    assert_eq!(wav.spec().sample_rate as usize, RATE);
    let voice: Vec<f32> = wav
        .samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.0)
        .collect();
    assert!(!voice.is_empty());
    voice
}

fn write(path: &Path, samples: &[f32]) {
    let mut wav = hound::WavWriter::create(
        path,
        hound::WavSpec {
            channels: 1,
            sample_rate: RATE as u32,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )
    .unwrap();
    for &sample in samples {
        wav.write_sample((sample.clamp(-1.0, 1.0) * 32767.0) as i16)
            .unwrap();
    }
    wav.finalize().unwrap();
}

fn energy(x: &[f32]) -> f64 {
    x.iter().map(|&s| (s as f64).powi(2)).sum()
}

fn room(far: &[f32]) -> Vec<f32> {
    let mut seed = 0x2545_f491u32;
    let taps: Vec<_> = (0..150 * RATE / 1000)
        .map(|i| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let noise = seed as f32 / u32::MAX as f32 - 0.5;
            (
                20 * RATE / 1000 + i + 1,
                noise * 0.3 * (-(i as f32) / (0.03 * RATE as f32)).exp(),
            )
        })
        .step_by(8)
        .chain([(20 * RATE / 1000, 1.0)])
        .collect();
    (0..far.len())
        .map(|i| {
            taps.iter()
                .map(|&(delay, gain)| i.checked_sub(delay).map_or(0.0, |k| far[k] * gain))
                .sum()
        })
        .collect()
}

#[test]
#[ignore = "speech files required; run explicitly"]
fn gate_lab() {
    let seconds: usize = std::env::var("PTT_LAB_SECONDS")
        .unwrap_or_else(|_| "60".into())
        .parse()
        .unwrap();
    assert!(seconds >= 20);
    let (a, b) = (voice("PTT_LAB_A"), voice("PTT_LAB_B"));
    let far: Vec<f32> = (0..seconds * RATE).map(|i| b[i % b.len()]).collect();
    let first_far_ms = far
        .as_chunks::<BLOCK>()
        .0
        .iter()
        .position(|block| energy(block) / BLOCK as f64 > 1e-7)
        .unwrap()
        * 10;
    let room_echo = room(&far);
    let metric = metric::Metric::new();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("aec_gate");
    std::fs::create_dir_all(&dir).unwrap();
    let mut false_engagements = 0;
    let mut headphones_identical = true;
    for start in [0, 3 * RATE] {
        let near: Vec<f32> = (0..far.len())
            .map(|i| {
                if i < start {
                    0.0
                } else {
                    a[(i - start) % a.len()]
                }
            })
            .collect();
        write(&dir.join(format!("near-{}s.wav", start / RATE)), &near);
        for db in [None, Some(-6.0f32), Some(-20.0), Some(-30.0)] {
            let gain = db.map_or(0.0, |db| 10f32.powf(db / 20.0));
            let acoustic_echo: Vec<f32> = room_echo.iter().map(|e| e * gain).collect();
            let input: Vec<f32> = near
                .iter()
                .zip(&acoustic_echo)
                .map(|(n, e)| n + e)
                .collect();
            let echo = EchoPath::new(true);
            let mut output = input.clone();
            let (mut first, mut active, mut switches, mut previous, mut max_score) =
                (None, 0, 0, false, 0.0f64);
            let began = Instant::now();
            let mut delays = Vec::new();
            let mut last_switch = None;
            for (i, (reference, capture)) in far
                .as_chunks::<BLOCK>()
                .0
                .iter()
                .zip(output.as_chunks_mut::<BLOCK>().0.iter_mut())
                .enumerate()
            {
                echo.far_end(reference);
                echo.near_end(capture);
                let state = echo.gate_state();
                max_score = max_score.max(state.score);
                if state.engaged {
                    active += 1;
                    first.get_or_insert(i * 10);
                }
                if state.engaged != previous {
                    switches += 1;
                    last_switch = Some(i);
                }
                delays.push(if last_switch.is_none_or(|at| i >= at + 2) {
                    Some(if state.engaged {
                        (ALGORITHMIC_DELAY_MS * RATE as f64 / 1000.0).round() as usize
                    } else {
                        0
                    })
                } else {
                    None
                });
                previous = state.engaged;
                if db.is_none() {
                    false_engagements += usize::from(state.engaged);
                }
            }
            let label = db.map_or("headphones".into(), |v| format!("speakers-{}db", -v));
            if db.is_none() {
                headphones_identical &= output == input;
            }
            let total_loss = 10.0 * (energy(&input) / energy(&output).max(1e-20)).log10();
            println!(
                "warmup={} {label}: first={first:?}ms active={active}/{} switches={switches} max_score={max_score:.3} combined_energy_drop={total_loss:.2}dB compute={:.1}ms",
                start / RATE,
                far.len() / BLOCK,
                began.elapsed().as_secs_f64() * 1000.0
            );
            println!(
                "  first far speech={first_far_ms}ms (-70 dBFS block RMS); detection after speech={:?}ms",
                first.map(|ms| ms.saturating_sub(first_far_ms))
            );
            if db.is_some() {
                let end = first.map_or(input.len(), |ms| ms * RATE / 1000);
                let leaked = energy(&acoustic_echo[..end]);
                let voice = energy(&near[..end]);
                let relative = if voice > 1e-15 {
                    format!("{:.1}dB vs near", 10.0 * (leaked / voice).log10())
                } else {
                    "near silent".into()
                };
                println!(
                    "  pre-engagement echo {:.1}dBFS, {relative}",
                    10.0 * (leaked / end.max(1) as f64).max(1e-20).log10()
                );
            }
            for (from, to) in [(0, 5), (5, 10), (10, seconds)] {
                let samples = from * RATE..to * RATE;
                let bands = metric.measure(
                    &near[samples.clone()],
                    &acoustic_echo[samples.clone()],
                    &output[samples],
                    &delays[from * 100..to * 100],
                );
                for (name, band) in ["<1k", "1-4k", "4-8k"].iter().zip(&bands) {
                    println!(
                        "  {from:>2}-{to:<2}s {name:<4}: near_loss={:.2}dB holes={:.1}% echo_reduction={:.1}dB distortion={:.1}dB skipped={:.1}% windows={}",
                        band.loss_db(),
                        band.holes() * 100.0,
                        band.echo_db(),
                        band.distortion_db(),
                        band.skipped() * 100.0,
                        band.windows()
                    );
                }
            }
            write(
                &dir.join(format!("{label}-{}s-input.wav", start / RATE)),
                &input,
            );
            write(
                &dir.join(format!("{label}-{}s-output.wav", start / RATE)),
                &output,
            );
        }
    }
    println!("wavs: {}", dir.display());
    assert_eq!(
        false_engagements, 0,
        "headphone blocks incorrectly processed"
    );
    assert!(headphones_identical, "headphone samples changed");
}

#[test]
#[ignore = "60 s in each of eight speech/cadence combinations; run explicitly"]
fn headphones_matrix() {
    let (a, b) = (voice("PTT_LAB_A"), voice("PTT_LAB_B"));
    for (order, near_voice, far_voice) in [("a/b", &a, &b), ("b/a", &b, &a)] {
        for start in [0, 3 * RATE] {
            for burst in [1, 2] {
                let echo = EchoPath::new(true);
                let size = burst * BLOCK;
                let mut max_score = 0.0f64;
                for base in (0..60 * RATE).step_by(size) {
                    let far: Vec<f32> = (base..base + size)
                        .map(|i| far_voice[i % far_voice.len()])
                        .collect();
                    let mut near: Vec<f32> = (base..base + size)
                        .map(|i| {
                            if i < start {
                                0.0
                            } else {
                                near_voice[(i - start) % near_voice.len()]
                            }
                        })
                        .collect();
                    let dry = near.clone();
                    echo.far_end(&far);
                    echo.near_end(&mut near);
                    let state = echo.gate_state();
                    max_score = max_score.max(state.score);
                    assert!(
                        !state.engaged,
                        "{order} start={start} burst={burst} at {}ms: {state:?}",
                        base * 1000 / RATE
                    );
                    assert_eq!(
                        near, dry,
                        "{order} start={start} burst={burst} changed microphone samples"
                    );
                }
                println!(
                    "headphones {order} start={}s burst={burst}: 60s bit-exact, no engagement, max_score={max_score:.3}",
                    start / RATE
                );
            }
        }
    }
}

#[test]
#[ignore = "speech file required; simulated speaker removal; run explicitly"]
fn speaker_removal_returns_to_dry() {
    let b = voice("PTT_LAB_B");
    let far: Vec<f32> = (0..15 * RATE).map(|i| b[i % b.len()]).collect();
    let acoustic = room(&far);
    let path = EchoPath::new(true);
    let mut engaged = false;
    let mut released = None;
    for (i, reference) in far.as_chunks::<BLOCK>().0.iter().enumerate() {
        let base = i * BLOCK;
        let mut capture: Vec<f32> = if base < 5 * RATE {
            acoustic[base..base + BLOCK]
                .iter()
                .map(|s| s * 0.5)
                .collect()
        } else {
            vec![0.0; BLOCK]
        };
        path.far_end(reference);
        path.near_end(&mut capture);
        let state = path.gate_state();
        engaged |= state.engaged;
        if engaged && !state.engaged && released.is_none() {
            released = Some(i * 10);
        }
        if released.is_some_and(|ms| i * 10 >= ms + 20) {
            assert_eq!(capture, vec![0.0; BLOCK], "dry path did not resume");
        }
    }
    assert!(engaged);
    let released = released.expect("removed speaker path never released during quiet near speech");
    assert!(
        (5000..10_000).contains(&released),
        "release at {released} ms"
    );
    println!("speaker removed at 5000ms, dry state at {released}ms, bit-exact after crossfade");
}
