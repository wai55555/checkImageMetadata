#!/usr/bin/env python3
"""Generate the §11.4 performance fixtures and measure the v1.2.0 CLI.

Uses only the Python standard library. Generated fixtures are written under
target/phase0-fixtures so they stay out of version control.

The routine JPEG fixture places its 5 MiB image scan after SOS. The original
APP2-before-SOS layout is retained separately as a segment-walk stress case.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import random
import statistics
import struct
import subprocess
import time
import zlib
from pathlib import Path


PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"
STEALTH_SIGNATURE = b"stealth_pnginfo"
IDAT_MARKERS = (b"tEXt", b"iTXt")


def png_chunk(kind: bytes, data: bytes) -> bytes:
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))


def ihdr(width: int, height: int, color_type: int) -> bytes:
    data = struct.pack(">IIBBBBB", width, height, 8, color_type, 0, 0, 0)
    return png_chunk(b"IHDR", data)


def text_chunk(keyword: str, text: str) -> bytes:
    return png_chunk(b"tEXt", keyword.encode("ascii") + b"\0" + text.encode("utf-8"))


def make_png(
    width: int,
    height: int,
    color_type: int,
    idat_chunks: list[bytes],
    after_idat: list[bytes] | None = None,
) -> bytes:
    result = bytearray(PNG_SIGNATURE)
    result.extend(ihdr(width, height, color_type))
    for data in idat_chunks:
        result.extend(png_chunk(b"IDAT", data))
    for chunk in after_idat or []:
        result.extend(chunk)
    result.extend(png_chunk(b"IEND", b""))
    return bytes(result)


def has_false_text_marker(data: bytes) -> bool:
    return any(marker in data[-131_072:] for marker in IDAT_MARKERS)


def noisy_rgb_stream(width: int, height: int, seed: int, level: int) -> bytes:
    rng = random.Random(seed)
    compressor = zlib.compressobj(level)
    output = bytearray()
    row_size = width * 3
    for _ in range(height):
        output.extend(compressor.compress(b"\0" + rng.randbytes(row_size)))
    output.extend(compressor.flush())
    return bytes(output)


def safe_noisy_rgb_stream(width: int, height: int, seed: int, level: int) -> bytes:
    while True:
        compressed = noisy_rgb_stream(width, height, seed, level)
        if not has_false_text_marker(compressed):
            return compressed
        seed += 1


def generate_comfy_png(path: Path) -> None:
    # 2048 × 1700 RGB scanlines produce a zlib IDAT of approximately 10 MB.
    compressed = safe_noisy_rgb_stream(2048, 1700, seed=20261126, level=1)
    first_idat = compressed[:65_536]
    if len(first_idat) != 65_536:
        raise RuntimeError("ComfyUI fixture must start with a 65,536-byte IDAT")
    png = make_png(
        2048,
        1700,
        2,
        [first_idat, compressed[65_536:]],
        [text_chunk("workflow", '{"fixture":"comfy-workflow"}')],
    )
    path.write_bytes(png)


def generate_stealth_png(path: Path) -> None:
    # Non-interlaced 2048 × 2048 RGBA, matching the current scanner's support.
    width = height = 2048
    bytes_per_pixel = 4
    row_bytes = width * bytes_per_pixel
    raw = bytearray(height * (row_bytes + 1))
    payload_size = 450 * 1024
    payload = b"Stealth performance fixture\n" + b"x" * (payload_size - 28)
    stream = STEALTH_SIGNATURE + struct.pack(">I", len(payload) * 8) + payload

    # Current Stealth PNG stores MSB-first bits in alpha LSBs using column-major
    # pixel order. Do not set IHDR's interlace field; Adam7 is out of scope.
    for byte_index, byte in enumerate(stream):
        first_bit = byte_index * 8
        for bit_in_byte in range(8):
            bit_index = first_bit + bit_in_byte
            pixel_x, pixel_y = divmod(bit_index, height)
            sample_offset = pixel_y * (row_bytes + 1) + 1 + pixel_x * bytes_per_pixel + 3
            bit = (byte >> (7 - bit_in_byte)) & 1
            raw[sample_offset] = (raw[sample_offset] & 0xFE) | bit

    compressed = zlib.compress(raw, level=1)
    path.write_bytes(make_png(width, height, 6, [compressed]))


def jpeg_segment(marker: int, data: bytes) -> bytes:
    if len(data) > 65_533:
        raise ValueError("JPEG segment data exceeds 65,533 bytes")
    return b"\xff" + bytes([marker]) + struct.pack(">H", len(data) + 2) + data


def minimal_jpeg_image() -> bytes:
    dqt = bytes([0]) + bytes([1] * 64)
    sof0 = bytes([8, 0, 8, 0, 8, 1, 1, 0x11, 0])
    dht = bytearray()
    for table_class in (0, 0x10):
        dht.append(table_class)
        dht.append(1)
        dht.extend(bytes(15))
        dht.append(0)
    sos = bytes([1, 1, 0, 0, 63, 0])
    return (
        jpeg_segment(0xDB, dqt)
        + jpeg_segment(0xC0, sof0)
        + jpeg_segment(0xC4, bytes(dht))
        + jpeg_segment(0xDA, sos)
        + b"\x3f\xff\xd9"
    )


def jpeg_exif_payload() -> bytes:
    comment = b"ASCII\0\0\0parameters=phase0 JPEG baseline"
    tiff = bytearray(b"II*\0" + struct.pack("<I", 8))
    tiff.extend(struct.pack("<H", 1))
    tiff.extend(b"\x86\x92" + struct.pack("<H", 7))
    tiff.extend(struct.pack("<I", len(comment)))
    tiff.extend(struct.pack("<I", 26))
    tiff.extend(struct.pack("<I", 0))
    tiff.extend(comment)
    return b"Exif\0\0" + bytes(tiff)


def generate_jpeg(path: Path) -> None:
    """Generate a 5 MiB JPEG whose image data follows SOS."""
    target_size = 5 * 1024 * 1024
    image = minimal_jpeg_image()
    prefix = b"\xff\xd8" + jpeg_segment(0xE1, jpeg_exif_payload()) + image[:-3]
    scan_data_size = target_size - len(prefix) - 2  # Leave room for EOI.
    if scan_data_size < 1:
        raise RuntimeError("JPEG headers exceed the 5 MiB performance fixture size")
    path.write_bytes(prefix + b"\x3f" + bytes(scan_data_size - 1) + b"\xff\xd9")


def generate_jpeg_app2_stress(path: Path) -> None:
    """Keep the original 5 MiB APP2-before-SOS fixture as a stress case."""
    target_size = 5 * 1024 * 1024
    image = minimal_jpeg_image()
    jpeg = bytearray(b"\xff\xd8")
    jpeg.extend(jpeg_segment(0xE1, jpeg_exif_payload()))
    while len(jpeg) + len(image) < target_size:
        remaining = target_size - len(jpeg) - len(image)
        if remaining < 4:
            break
        data_size = min(65_533, max(0, remaining - 4))
        jpeg.extend(jpeg_segment(0xE2, bytes(data_size)))
    jpeg.extend(image)
    path.write_bytes(jpeg)


def generate_absent_many_idat(path: Path) -> None:
    compressed = safe_noisy_rgb_stream(1024, 768, seed=102, level=1)
    chunks = [compressed[pos : pos + 4096] for pos in range(0, len(compressed), 4096)]
    path.write_bytes(make_png(1024, 768, 2, chunks))


def generate_absent_canvas(path: Path) -> None:
    width, height = 1200, 800
    compressor = zlib.compressobj(6)
    compressed = bytearray()
    for y in range(height):
        row = bytearray(1 + width * 4)
        row[0] = 0
        for x in range(width):
            offset = 1 + x * 4
            row[offset : offset + 4] = bytes((x * 255 // width, y * 255 // height, 128, 255))
        compressed.extend(compressor.compress(row))
    compressed.extend(compressor.flush())
    path.write_bytes(make_png(width, height, 6, [bytes(compressed)]))


def generate_absent_photo(path: Path) -> None:
    compressed = safe_noisy_rgb_stream(1024, 1024, seed=103, level=1)
    path.write_bytes(make_png(1024, 1024, 2, [compressed]))


def fixture_paths(directory: Path) -> dict[str, Path]:
    return {
        "PNG ComfyUI 10 MiB IDAT + workflow": directory / "comfy-10mb-workflow.png",
        "PNG Stealth 2048x2048 RGBA": directory / "stealth-2048-rgba.png",
        "JPEG 5 MiB with EXIF": directory / "jpeg-5mb-exif.jpg",
        "JPEG 5 MiB APP2-before-SOS stress": directory / "jpeg-5mb-app2-stress.jpg",
        "PNG absent, many small IDAT chunks": directory / "absent-many-idat.png",
        "PNG absent, browser canvas style": directory / "absent-canvas.png",
        "PNG absent, single large photo IDAT": directory / "absent-photo.png",
    }


def generate_fixtures(paths: dict[str, Path]) -> None:
    generate_comfy_png(paths["PNG ComfyUI 10 MiB IDAT + workflow"])
    generate_stealth_png(paths["PNG Stealth 2048x2048 RGBA"])
    generate_jpeg(paths["JPEG 5 MiB with EXIF"])
    generate_jpeg_app2_stress(paths["JPEG 5 MiB APP2-before-SOS stress"])
    generate_absent_many_idat(paths["PNG absent, many small IDAT chunks"])
    generate_absent_canvas(paths["PNG absent, browser canvas style"])
    generate_absent_photo(paths["PNG absent, single large photo IDAT"])
    validate_jpeg_fixture_layout(paths)


def validate_jpeg_fixture_layout(paths: dict[str, Path]) -> None:
    target_size = 5 * 1024 * 1024
    normal = paths["JPEG 5 MiB with EXIF"].read_bytes()
    stress = paths["JPEG 5 MiB APP2-before-SOS stress"].read_bytes()
    normal_sos = normal.find(b"\xff\xda")
    stress_sos = stress.find(b"\xff\xda")
    if len(normal) != target_size or not 0 <= normal_sos < 1024:
        raise RuntimeError("routine JPEG fixture must place SOS in the first 1 KiB")
    if len(normal) - normal_sos < 4 * 1024 * 1024:
        raise RuntimeError("routine JPEG fixture must put at least 4 MiB after SOS")
    if (
        len(stress) != target_size
        or stress_sos < 4 * 1024 * 1024
        or stress[:stress_sos].count(b"\xff\xe2") < 32
    ):
        raise RuntimeError("APP2 stress fixture must keep many segments before SOS")


def binary_model_name() -> str:
    cpuinfo = Path("/proc/cpuinfo")
    if cpuinfo.exists():
        for line in cpuinfo.read_text(errors="replace").splitlines():
            if line.lower().startswith("model name"):
                return line.split(":", 1)[1].strip()
    return platform.processor() or "unknown"


def measure(binary: Path, fixture: Path, warmups: int, iterations: int) -> dict[str, float]:
    command = [str(binary.resolve()), str(fixture.resolve())]
    for _ in range(warmups):
        completed = subprocess.run(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if completed.returncode != 0:
            raise RuntimeError(f"warmup failed for {fixture}: status {completed.returncode}")

    samples_ms = []
    for _ in range(iterations):
        start = time.perf_counter_ns()
        completed = subprocess.run(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        elapsed_ms = (time.perf_counter_ns() - start) / 1_000_000
        if completed.returncode != 0:
            raise RuntimeError(f"measurement failed for {fixture}: status {completed.returncode}")
        samples_ms.append(elapsed_ms)

    return {
        "median_ms": statistics.median(samples_ms),
        "min_ms": min(samples_ms),
        "max_ms": max(samples_ms),
    }


def validate_expected_output(binary: Path, fixture: Path, markers: tuple[str, ...]) -> None:
    command = [str(binary.resolve()), str(fixture.resolve())]
    completed = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if completed.returncode != 0:
        raise RuntimeError(f"validation failed for {fixture}: status {completed.returncode}")
    output = completed.stdout.decode("utf-8", errors="replace")
    missing = [marker for marker in markers if marker not in output]
    if missing:
        raise RuntimeError(f"validation failed for {fixture}: missing output markers {missing!r}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    default_binary = "target/release/fast_meta.exe" if os.name == "nt" else "target/release/fast_meta"
    parser.add_argument("--binary", type=Path, default=Path(default_binary))
    parser.add_argument("--output-dir", type=Path, default=Path("target/phase0-fixtures"))
    parser.add_argument("--iterations", type=int, default=15)
    parser.add_argument("--warmups", type=int, default=3)
    args = parser.parse_args()

    if not args.binary.is_file():
        raise SystemExit(f"release binary not found: {args.binary}; run cargo build --release first")
    if args.iterations < 1 or args.warmups < 0:
        raise SystemExit("iterations must be positive and warmups must be non-negative")

    args.output_dir.mkdir(parents=True, exist_ok=True)
    paths = fixture_paths(args.output_dir)
    generate_fixtures(paths)

    expected_output_markers = {
        "PNG ComfyUI 10 MiB IDAT + workflow": ("workflow", '"fixture":"comfy-workflow"'),
        "PNG Stealth 2048x2048 RGBA": ("Stealth PNG Info", "Stealth performance fixture"),
        "JPEG 5 MiB with EXIF": ("EXIF UserComment", "phase0 JPEG baseline"),
        "JPEG 5 MiB APP2-before-SOS stress": (
            "EXIF UserComment",
            "phase0 JPEG baseline",
        ),
        "PNG absent, many small IDAT chunks": ("(No metadata detected)",),
        "PNG absent, browser canvas style": ("(No metadata detected)",),
        "PNG absent, single large photo IDAT": ("(No metadata detected)",),
    }
    for label, path in paths.items():
        validate_expected_output(args.binary, path, expected_output_markers[label])

    results = {
        "environment": {
            "platform": platform.platform(),
            "cpu": binary_model_name(),
            "logical_cpus": os.cpu_count(),
            "python": platform.python_version(),
            "binary": str(args.binary.resolve()),
            "binary_size_bytes": args.binary.stat().st_size,
            "warmups": args.warmups,
            "iterations": args.iterations,
            "timing": "whole CLI process, stdout/stderr discarded, median of perf_counter_ns samples",
        },
        "fixtures": {},
    }

    for label, path in paths.items():
        timing = measure(args.binary, path, args.warmups, args.iterations)
        results["fixtures"][label] = {
            "path": str(path),
            "size_bytes": path.stat().st_size,
            **timing,
        }
        print(f"{label}: {path.stat().st_size:,} bytes, median {timing['median_ms']:.3f} ms")

    result_path = args.output_dir / "phase0-results.json"
    result_path.write_text(json.dumps(results, indent=2) + "\n")
    print(f"release binary: {args.binary.stat().st_size:,} bytes")
    print(f"results: {result_path}")


if __name__ == "__main__":
    main()
