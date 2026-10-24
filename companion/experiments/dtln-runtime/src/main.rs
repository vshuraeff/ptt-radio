use std::{path::Path, time::Instant};

use anyhow::{Context, Result, ensure};
use cpu_time::ProcessTime;
use dtln_runtime_lab::{BLOCK_48, Dtln, HOP, Stream48, read_f32};

fn wav(path: &Path) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    ensure!(
        spec.channels == 1 && spec.sample_rate == 48000,
        "expected mono 48 khz WAV"
    );
    match spec.sample_format {
        hound::SampleFormat::Float => Ok(reader.samples::<f32>().collect::<Result<_, _>>()?),
        hound::SampleFormat::Int => Ok(reader
            .samples::<i32>()
            .map(|s| s.map(|s| s as f32 / 2f32.powi(spec.bits_per_sample as i32 - 1)))
            .collect::<Result<_, _>>()?),
    }
}

fn write(path: &Path, samples: &[f32]) -> Result<()> {
    let mut writer = hound::WavWriter::create(
        path,
        hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )?;
    for &sample in samples {
        writer.write_sample(sample)?;
    }
    writer.finalize()?;
    Ok(())
}

fn energy(samples: &[f32]) -> f64 {
    samples.iter().map(|&s| (s as f64).powi(2)).sum()
}

fn parity(directory: &Path, units: usize) -> Result<()> {
    let near = read_f32(&directory.join("parity-near.f32"))?;
    let far = read_f32(&directory.join("parity-far.f32"))?;
    let expected = read_f32(&directory.join("parity-expected.f32"))?;
    ensure!(
        near.len() == far.len() && expected.len() == near.len(),
        "unequal parity streams"
    );
    let mut model = Dtln::load(directory, units)?;
    let mut actual = Vec::new();
    for (near, far) in near
        .as_chunks::<HOP>()
        .0
        .iter()
        .zip(far.as_chunks::<HOP>().0)
    {
        actual.extend(model.process(near, far)?);
    }
    let errors: Vec<_> = expected.iter().zip(&actual).map(|(a, b)| a - b).collect();
    let peak = errors.iter().map(|s| s.abs()).fold(0.0, f32::max);
    let snr = 10.0 * (energy(&expected) / energy(&errors).max(1e-30)).log10();
    println!(
        "TFLite/Rust parity: samples={} max_abs={peak:.8} snr={snr:.2}dB",
        actual.len()
    );
    ensure!(
        actual.len() == expected.len() && peak < 0.001 && snr > 50.0,
        "streaming parity failed"
    );
    Ok(())
}

fn latency(directory: &Path, units: usize) -> Result<()> {
    for phase in [0, 41, 127] {
        let mut model = Dtln::load(directory, units)?;
        let impulse = 16000 + phase;
        let mut output = Vec::new();
        for block in 0..200 {
            let near = std::array::from_fn(|i| if block * HOP + i == impulse { 0.5 } else { 0.0 });
            output.extend(model.process(&near, &[0.0; HOP])?);
        }
        let (peak, value) = output
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
            .context("no 16k impulse output")?;
        ensure!(
            value.abs() > 1e-8 && peak >= impulse,
            "no causal 16k response"
        );
        println!(
            "16k impulse phase={phase}: peak_delay={}samples ({:.3}ms), peak={value:.6}",
            peak - impulse,
            (peak - impulse) as f64 / 16.0
        );
    }
    for phase in [0, 123, 479] {
        let mut model = Stream48::load(directory, units)?;
        let impulse = 48000 + phase;
        let mut output = Vec::new();
        for block in 0..150 {
            let near = std::array::from_fn(|i| {
                if block * BLOCK_48 + i == impulse {
                    0.5
                } else {
                    0.0
                }
            });
            output.extend(model.process(&near, &[0.0; BLOCK_48])?);
        }
        let (peak, value) = output
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
            .context("no impulse output")?;
        ensure!(
            value.abs() > 1e-8 && peak >= impulse,
            "no causal impulse response"
        );
        println!(
            "48k impulse phase={phase}: peak_delay={}samples ({:.3}ms), peak={value:.6}",
            peak - impulse,
            (peak - impulse) as f64 / 48.0
        );
    }
    Ok(())
}

fn smoke(
    directory: &Path,
    units: usize,
    far_path: &Path,
    near_path: &Path,
    output_dir: &Path,
) -> Result<()> {
    let far_voice = wav(far_path)?;
    let near_voice = wav(near_path)?;
    ensure!(
        !far_voice.is_empty() && !near_voice.is_empty(),
        "empty voice fixture"
    );
    let count = 16 * 48000;
    let far: Vec<_> = (0..count).map(|i| far_voice[i % far_voice.len()]).collect();
    let near: Vec<_> = (0..count)
        .map(|i| {
            if i < count / 2 {
                0.0
            } else {
                near_voice[(i - count / 2) % near_voice.len()]
            }
        })
        .collect();
    // fixed small room, -6 db direct path; echo-only first, then double talk
    let taps = [
        (960, 1.0),
        (1109, 0.5),
        (1234, -0.35),
        (1402, 0.25),
        (1632, -0.18),
        (1968, 0.12),
        (2544, 0.08),
        (3216, -0.05),
    ];
    let input: Vec<_> = (0..count)
        .map(|i| {
            near[i]
                + taps
                    .iter()
                    .map(|&(delay, gain)| {
                        i.checked_sub(delay)
                            .map_or(0.0, |j| far[j] * gain * 10f32.powf(-6.0 / 20.0))
                    })
                    .sum::<f32>()
        })
        .collect();
    let mut stream = Stream48::load(directory, units)?;
    let mut output = Vec::new();
    let mut costs = Vec::new();
    let cpu = ProcessTime::now();
    let began = Instant::now();
    for (near, far) in input
        .as_chunks::<BLOCK_48>()
        .0
        .iter()
        .zip(far.as_chunks::<BLOCK_48>().0)
    {
        let start = Instant::now();
        output.extend(stream.process(near, far)?);
        costs.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let cpu_ms = cpu.elapsed().as_secs_f64() * 1000.0 / costs.len() as f64;
    let wall = began.elapsed().as_secs_f64();
    costs.sort_by(f64::total_cmp);
    let percentile = |p: usize| costs[(costs.len() - 1) * p / 100];
    let warm = 4 * 48000..7 * 48000;
    let reduction = 10.0 * (energy(&input[warm.clone()]) / energy(&output[warm])).log10();
    println!(
        "{units} units: 16s audio, wall={wall:.3}s; CPU={cpu_ms:.3}ms/10ms; call p50={:.3}ms p95={:.3}ms max={:.3}ms",
        percentile(50),
        percentile(95),
        percentile(100)
    );
    println!(
        "echo-only 4-7s: input={:.2}dBFS output={:.2}dBFS reduction={reduction:.2}dB",
        10.0 * (energy(&input[4 * 48000..7 * 48000]) / 144000.0).log10(),
        10.0 * (energy(&output[4 * 48000..7 * 48000]) / 144000.0).log10()
    );
    ensure!(output.iter().all(|s| s.is_finite()), "non-finite output");
    std::fs::create_dir_all(output_dir)?;
    for (name, samples) in [
        ("far", far),
        ("near", near),
        ("input", input),
        ("output", output),
    ] {
        write(&output_dir.join(format!("{name}.wav")), &samples)?;
    }
    ensure!(reduction > 6.0, "smoke echo reduction below 6 dB");
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let command = args.first().context(
        "usage: parity|latency|smoke MODEL_DIR [UNITS=128] [FAR48.wav NEAR48.wav OUTPUT_DIR]",
    )?;
    let directory = Path::new(args.get(1).context("missing model directory")?);
    let units = args.get(2).map_or(Ok(128), |s| s.parse::<usize>())?;
    match command.as_str() {
        "parity" => parity(directory, units),
        "latency" => latency(directory, units),
        "smoke" => smoke(
            directory,
            units,
            Path::new(args.get(3).context("missing far WAV")?),
            Path::new(args.get(4).context("missing near WAV")?),
            Path::new(args.get(5).context("missing output directory")?),
        ),
        _ => anyhow::bail!("unknown command {command}"),
    }
}
