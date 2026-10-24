# /// script
# requires-python = ">=3.11,<3.12"
# dependencies = ["tensorflow==2.16.2", "tf2onnx==1.16.1", "onnx==1.17.0", "numpy==1.26.4"]
# ///
"""Convert pinned weights and generate an untrimmed streaming parity fixture."""

import argparse
import hashlib
from pathlib import Path

import numpy as np
import tensorflow as tf
import tf2onnx

from fetch_models import HASHES


def reference(directory, units):
    interpreters = [tf.lite.Interpreter(model_path=str(directory / f"dtln_aec_{units}_{p}.tflite"), num_threads=1) for p in [1, 2]]
    for interpreter in interpreters:
        interpreter.allocate_tensors()
    states = [np.zeros(i.get_input_details()[1]["shape"], dtype=np.float32) for i in interpreters]
    count = 16000 * 2
    t = np.arange(count) / 16000
    rng = np.random.default_rng(71)
    far = (0.03 * rng.normal(size=count) + 0.1 * np.sin(2 * np.pi * 213 * t)).astype(np.float32)
    near = (0.05 * np.sin(2 * np.pi * 317 * t)).astype(np.float32)
    near[320:] += far[:-320] * 0.5
    near[:3200] = far[:3200] = 0
    mic, loopback, overlap = [np.zeros(512, dtype=np.float32) for _ in range(3)]
    output = []
    for start in range(0, count, 128):
        mic[:-128], mic[-128:] = mic[128:].copy(), near[start:start + 128]
        loopback[:-128], loopback[-128:] = loopback[128:].copy(), far[start:start + 128]
        spectrum = np.fft.rfft(mic).astype(np.complex64)
        values = [np.abs(spectrum).reshape(1, 1, 257), states[0], np.abs(np.fft.rfft(loopback).astype(np.complex64)).reshape(1, 1, 257)]
        for part, interpreter in enumerate(interpreters):
            details = interpreter.get_input_details()
            for detail, value in zip(details, values):
                interpreter.set_tensor(detail["index"], value.astype(np.float32))
            interpreter.invoke()
            outputs = interpreter.get_output_details()
            result = interpreter.get_tensor(outputs[0]["index"])
            states[part] = interpreter.get_tensor(outputs[1]["index"])
            if part == 0:
                estimated = np.fft.irfft(spectrum * result.reshape(257)).astype(np.float32)
                values = [estimated.reshape(1, 1, 512), states[1], loopback.reshape(1, 1, 512)]
        overlap[:-128], overlap[-128:] = overlap[128:].copy(), 0
        overlap += result.reshape(512)
        output.extend(overlap[:128])
    for name, samples in [("near", near), ("far", far), ("expected", output)]:
        np.asarray(samples, dtype="<f4").tofile(directory / f"parity-{name}.f32")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--units", choices=["128", "512"], default="128")
    parser.add_argument("--directory", type=Path, default=Path(__file__).parent / "models")
    args = parser.parse_args()
    for part in [1, 2]:
        source = args.directory / f"dtln_aec_{args.units}_{part}.tflite"
        if hashlib.sha256(source.read_bytes()).hexdigest() != HASHES[f"{args.units}_{part}"]:
            raise ValueError(f"unverified input: {source.name}")
        output = source.with_suffix(".onnx")
        graph, _ = tf2onnx.convert.from_tflite(str(source), opset=16, output_path=str(output))
        print(f"{output.name}: inputs={[i.name for i in graph.graph.input]} outputs={[o.name for o in graph.graph.output]}")
        print(f"sha256 {hashlib.sha256(output.read_bytes()).hexdigest()}")
    reference(args.directory, args.units)


if __name__ == "__main__":
    main()
