# A Rust driver for DECTRIS SINGLA detectors in Instamatic

AI agents and new sessions: start with `AGENTS.md`.

A camera interface for [Instamatic](https://github.com/instamatic-dev/instamatic), and for programs
built on it. It drives a DECTRIS SINGLA through its detector control unit:

- the SIMPLON 1.8 REST API for control;
- Stream V2 over ZeroMQ for the frames;
- the Monitor for the live view.

It is written in Rust and loaded by Python as one extension module. Decoding and decompressing a
frame takes about a millisecond and does not hold Python's GIL.

What it does:

- Single images, movies and continuous series, in the trigger modes `ints`, `inte`, `exts` and
  `exte`.
- Frames from the stream, decoded and decompressed (bslz4, lz4) in Rust, each placed in its series
  by the identifiers the detector sends.
- The live view from the Monitor, at most 10 times a second, without arming the detector.
- The FileWriter and raw stream messages, for those who keep them.
- Typed exceptions (`DectrisTimeout`, `DectrisBusy`, `DetectorStateError`, ...) in place of hangs,
  a time limit on every request, and recovery when the control unit restarts.

This repository holds only the driver. It changes no file of the program it goes into; the program's
own changes are listed in `INTEGRATION.md`.

## Quick start

1. Download `_super_dectris.cp312-win_amd64.pyd` from this repository's release, and check it
   against `SHA256SUMS.txt`.
2. Follow `INTEGRATION.md`, or ask a coding agent to: "Read AGENTS.md in this repository and put
   the driver into the program in <folder>."
3. Run `python -m instamatic.camera.super_dectris.selfcheck` in the program's environment until it
   prints `OK`.
4. Set the control unit's address in the camera's yaml and connect.

## Contents

```
INTEGRATION.md                         how to put the driver into a program, and check it
AGENTS.md, CLAUDE.md                   where an AI agent starts
src/instamatic/camera/super_dectris/   the driver: __init__.py, selfcheck.py, rust/ (the Cargo workspace)
tests/super_dectris/                   Python tests against a scripted fake control unit
examples/                              a camera yaml, the integration into Instamatic 2.2.2 as a patch, and
                                       Instamatic's licence
docs/DESIGN.md                         how the driver is built, and why
docs/PROTOCOL.md                       what the detector sends and accepts
third_party/                           dectris-compression, the reference decoder of the tests
CHANGELOG.md                           versions
LICENSE, NOTICE                        the Apache License 2.0, and the attribution notice
CITATION.cff, .zenodo.json             how to cite the driver
```

The compiled driver is not in the repository; it is a release asset.

## Requirements

- Windows x64 and CPython 3.12: conda, a python.org install or a plain venv (for example from
  `uv`). Without conda, also the Microsoft Visual C++ Redistributable (x64).
- An Instamatic-based program, installed editable, with the changes of `INTEGRATION.md`.
- Only to build the driver: the Rust toolchain (`rustup`; the workspace pins Rust 1.91.1) and the
  Visual Studio C++ build tools. See `src/instamatic/camera/super_dectris/README.md`.

## Documents

- `INTEGRATION.md`: the steps, the self-check, and what to do when something fails.
- `docs/DESIGN.md`: explains the driver for someone who has to read or change it. Start with its
  section 1; section 2 is the Python API and section 11 what a program can expect of the driver.
- `docs/PROTOCOL.md`: the detector's side. It covers the state machine, the trigger modes, the
  endpoints, the Stream V2 messages and the compression format, byte by byte.

Comments in the source cite both documents by section, as `DESIGN §4.3` or `PROTOCOL §8`.

## Safety and liability

The driver controls a detector on an electron microscope. It does not replace any interlock,
protection or safety procedure of the detector or of the microscope. Operate both within the
limits and procedures their manufacturers set. You are responsible for using the driver safely on
your instruments.

As sections 7 and 8 of `LICENSE` set out, the driver is provided "as is", without warranty of any
kind. To the extent permitted by applicable law, no author or contributor is liable for damages of
any character arising from its use. That includes:

- damage to instruments, detectors, samples or other property;
- lost or corrupted data;
- lost instrument time;
- any result derived from data acquired with the driver.

This section explains those terms and does not change them.

Nothing here excludes, restricts or modifies any right or remedy, or any guarantee, warranty or
other term or condition, implied or imposed by legislation that cannot lawfully be excluded or
limited, including the Australian Consumer Law.

## Citing

Cite the driver by its DOI, [10.5281/zenodo.23993707](https://doi.org/10.5281/zenodo.23993707).
`CITATION.cff` has the full reference.

## Licence

Copyright 2026 Chung-Han Tsai. Released under the Apache License, Version 2.0 (the "License"); you
may not use the driver except in compliance with the License. See `LICENSE` and `NOTICE`.

- What the licence asks when the driver, or a program containing it, is distributed:
  `INTEGRATION.md`, the end.
- The licences of the Rust packages and C libraries compiled into the driver:
  `src/instamatic/camera/super_dectris/THIRD_PARTY_NOTICES.md`.
- `third_party/` keeps its own licences (see its README).

DECTRIS, SINGLA and SIMPLON are names of DECTRIS Ltd. This project is not made, endorsed or
supported by DECTRIS. Instamatic is a separate project under its own licence.

Developed by Chung-Han Tsai with the assistance of Claude (Anthropic).
