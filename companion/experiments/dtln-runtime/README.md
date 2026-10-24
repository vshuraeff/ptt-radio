# DTLN-aec runtime experiment

Standalone experiment, also a companion dev-dependency for offline comparisons, outside the production dependency graph. It does not replace the shipped canceller. `Stream48` is a callable Rust library for the multi-party evaluation; the binary provides parity, latency and coupled-fixture checks. Runtime inference uses `tract-onnx` 0.23.9 and RustFFT, with explicit recurrent state for each stream. TensorFlow/Python are used only to convert and validate weights offline.

## Models and conversion

Source: [breizhn/DTLN-aec at 9d24e128](https://github.com/breizhn/DTLN-aec/tree/9d24e128b4f409db18227b8babb343016625921f). The upstream repository contains the pretrained 128/256/512-unit models and an MIT licence, reproduced in `MODEL-LICENSE`; no separate weight-specific licence file/model card was present. This experiment covers 128 and 512 units. Upstream's 512-unit model was the challenge submission; the smaller model is a useful CPU comparison, not assumed equivalent in quality.

`fetch_models.py` downloads only from that immutable commit and checks SHA-256 before writing. `convert_models.py` pins TensorFlow 2.16.2, tf2onnx 1.16.1, ONNX 1.17.0 and NumPy 1.26.4 on Python 3.11. It converts both stages to ONNX opset 16 without custom operators or continue-on-error, then generates a 2-second streaming TFLite reference with state feedback. Generated weights/reference files are ignored by Git.

| model | upstream TFLite SHA-256 | converted ONNX SHA-256 in measured run |
|---|---|---|
| 128 stage 1 | `8d241b3a732af8ca140b2e30043e56a6c3c7800c46e22c26f4eea2f70974ad1e` | `9e3e2e34f8f33403dacbe436fe659bc4ab31d1975ad9c3da1950d081895f6de0` |
| 128 stage 2 | `350bb01a1152ae3cabe09fe5e868ef2f7d8b988a9f22aae44f140195f6493126` | `40064eb56cc6073dc5940a2ed8e877c45a36efd8c7aae81d6335773d2ebcb781` |
| 512 stage 1 | `569f7c3cfac96b1e093229c3ca10b5d892f5b1906105b644e457f8245b4f7383` | `761c6ff22445e9c61d438ea2e6010a1f9d847ffd41df95bdeab6ea59bb9655fa` |
| 512 stage 2 | `fb423d867ab25d5f4716bd369c7126b6c84926175c019b832e71bf21de0e9907` | `1957afa9e68e3d15f38b50c3c35ee317e6f581d88a99c82a231997e1c881989a` |

Direct `tract-tflite` 0.23.9 loading was tried and panicked in its fully-connected importer (`nn.rs:154`, index -1). Source inspection also found missing PACK, UNPACK, SPLIT and SQUARED_DIFFERENCE import registrations and no public registry extension API. ONNX conversion avoids a fork of that runtime. `ort` with its default ONNX Runtime backend would introduce a C/C++ inference runtime; it was not used. The measured macOS binary links only libSystem and libiconv, with no TensorFlow, ONNX Runtime or system BLAS library.

From this directory:

```sh
uv run -q fetch_models.py
uv run -q --python 3.11 convert_models.py
cargo run --release -- parity models
cargo run --release -- latency models
cargo run --release -- smoke models 128 far48.wav near48.wav output/128

uv run -q fetch_models.py --units 512 --directory models/512
uv run -q --python 3.11 convert_models.py --units 512 --directory models/512
cargo run --release -- parity models/512 512
cargo run --release -- latency models/512 512
cargo run --release -- smoke models/512 512 far48.wav near48.wav output/512

cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --release
```

Supply two independent mono 48 kHz WAVs; integer PCM or float input is supported. The smoke command repeats them to 16 seconds. Far speech plays throughout; the desired near voice starts at 8 seconds. Capture contains a -6 dB direct speaker path delayed 20 ms plus seven room reflections. Output WAVs are float, without clipping or normalization: `far.wav`, `near.wav`, `input.wav`, `output.wav`.

The smoke assertion requires >6 dB attenuation in the echo-only interval at 4–7 seconds and finite output. It does not estimate near-voice quality from total mixed energy. Double-talk spectral loss, holes and perceptual quality belong to the multi-party comparison. This is an open-loop microphone-path smoke test, without Opus, relay transport, device callbacks or recirculating echo.

## Streaming contract and delay

The [upstream runner](https://github.com/breizhn/DTLN-aec/blob/9d24e128b4f409db18227b8babb343016625921f/run_aec.py) uses mono 16 kHz, a 512-sample frame and 128-sample hop. Stage 1 takes mic magnitude, state and loopback magnitude; its mask multiplies the original complex mic spectrum. Stage 2 takes the inverse FFT waveform, its own state and the raw loopback frame. There is no extra analysis window; predicted frames are overlap-added. Rust inverse FFT explicitly divides by 512. Each stage retains its entire `[1,2,units,2]` state tensor between hops. A new `Dtln` or `Stream48` resets a stream; do not share it across players.

`Dtln::process` accepts and returns 128 samples at 16 kHz without trimming or future access. `Stream48::process` accepts simultaneous near/far 480-sample blocks and returns 480 samples at 48 kHz. Two 127-tap Blackman-windowed sinc FIRs filter the mic's down/up rate conversion; far uses the same down filter. Cutoff is 7.2 kHz. This deliberately rejects aliasing, unlike the companion's device-rate linear interpolator. Its transition band also affects near spectra around 7–8 kHz; quality comparisons must include that effect rather than attribute it all to the neural model.

An output reserve of one 16 kHz hop absorbs the mismatch between 10 ms caller cadence and 8 ms model cadence. Measured impulse peaks at three phases, both sizes:

| component | added signal delay |
|---|---|
| model + overlap-add at 16 kHz | 384 samples = 24 ms |
| cadence reserve | 128 samples at 16 kHz = 8 ms |
| two FIR filters | 126 samples at 48 kHz = 2.625 ms |
| complete Stream48 | 1662 samples at 48 kHz = 34.625 ms |

The FIR test checks peak delay, unity summed impulse gain, 1 kHz passband gain within 1%, and at least 60 dB rejection of a 12 kHz input. The measured neural impulse peak is not a claim of linear phase or unchanged waveform. The upstream offline padding/crop is not applied to hide latency. Audio capture block readiness, device queues, Opus and transport add their own latency outside these signal-delay numbers.

## Measured smoke and parity

2026-10-24, macOS 15.8, Intel i9-9900K @3.60 GHz, Rust 1.99.1, release build. Fixture voices were independent Milena/Eddy speech WAVs. Model loading, fixture generation and WAV writes are excluded from the processing measurement. Process CPU includes both inference stages, FFT, FIR resampling and cadence queues. The wall-clock per-call percentiles cover the 1600 calls; these are offline timings, not a scheduling guarantee.

| units | CPU ms per 10 ms audio | call p50 / p95 / max, ms | wall time for 16 s | echo-only mic input / output, dBFS | reduction |
|---|---|---|---|---|---|
| 128 | 0.737 | 0.633 / 1.182 / 2.428 | 1.182 s | -23.32 / -55.40 | 32.08 dB |
| 512 | 2.622 | 2.148 / 4.206 / 7.124 | 4.203 s | -23.32 / -52.59 | 29.28 dB |

Streaming parity against the original TFLite graphs, 250 recurrent hops including silence, near tone and delayed independent echo: 128 units max absolute error 0.00000015, SNR 97.73 dB; 512 units 0.00000009, 111.20 dB. The parity command requires max error <0.001 and SNR >50 dB. This verifies converted streaming inference for that control, not equivalence for every possible input.

The larger model's lower echo-only score in this fixture is not a general quality ranking. No model is recommended for adoption here; the next evaluation must compare desired voice preservation and closed-loop behavior alongside echo attenuation. Windows/Linux builds, actual device deadlines and Factorio integration were not exercised.
