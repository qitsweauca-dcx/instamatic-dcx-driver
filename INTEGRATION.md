# Putting the driver into a program

This guide puts the driver into Instamatic, or into a program built on Instamatic's source tree.
It is written so that a person or an AI coding agent can follow it step by step. Each step says
what to change, how to find the place, and how to check it. Step 10 runs a self-check that tests
most of the steps.

`examples/instamatic-2.2.2.patch` shows steps 3, 4 and 6 on upstream Instamatic 2.2.2 (commit
`ded7d4b`). On another version, make the same changes at the equivalent places.

## Before you start

- Windows x64 and CPython 3.12. The `.pyd` is built for exactly that.
- Work on a copy or a branch of the program.
- Keep the source tree on a short path, such as `C:\src\instamatic`. This matters only if you
  build the driver yourself (MSVC's linker fails on long paths).
- Install the program editable (`pip install -e .`), so that Python imports the files you change.

## Steps

### 1. Copy the driver (required)

If the program already has a folder `src/instamatic/camera/super_dectris/`, it belongs to an older
driver. Move it out of the source tree (keep it as a backup), so that no old file is left beside the
new ones.

Copy `src/instamatic/camera/super_dectris/` from this repository into the program's
`src/instamatic/camera/`, the folder that holds `camera_base.py`. Copy the whole folder:
`selfcheck.py` and `selfcheck_image.cbor` are part of it.

For the Python tests, also copy `tests/super_dectris/` into the program's `tests/`.

Check: `src/instamatic/camera/super_dectris/__init__.py` exists in the program.

### 2. Add the compiled driver (required)

Put `_super_dectris.cp312-win_amd64.pyd` next to that `__init__.py`. Take it from this
repository's release of the same version, and compare its SHA-256 with `SHA256SUMS.txt`. Or build
it (`src/instamatic/camera/super_dectris/README.md`, Building).

Check: self-check C3.

### 3. Camera factory (required)

Find the function that maps a camera's `interface` to its class. In Instamatic it is
`get_camera_class` in `src/instamatic/camera/camera.py`; otherwise search for `'simplon'` or
`'gatansocket'`. Add, or change, the branch for `dectris_simplon`:

```python
    elif interface == 'dectris_simplon':
        from instamatic.camera.super_dectris import CameraDectris as cam
```

Instamatic's own `simplon` interface is a different driver. Leave it as it is.

Check: self-check C5.

### 4. Program name and version (required)

The driver sends `__app_title__/__app_version__` as its HTTP User-Agent, and it reads them from
`instamatic/__init__.py`. Without them, connecting raises `AttributeError`. Add two strings with
your program's name and version. For plain Instamatic:

```python
__app_title__ = 'instamatic'
__app_version__ = __version__
```

A program with its own name and version strings uses those. The example patch shows another
program's values.

Check: self-check C2.

### 5. Camera configuration (required)

Copy `examples/dectris-simplon.yaml` into the `camera` folder of the configuration the program
uses (`instamatic -l config` shows where that is). Name it in `settings.yaml` with
`camera: dectris-simplon`. Then set:

| key | meaning |
|---|---|
| `interface` | `dectris_simplon` |
| `DCU_host`, `DCU_port` | the control unit's REST address (port 80 unless the unit is set otherwise) |
| `ZMQ_host`, `ZMQ_port` | the Stream V2 address (port 31001) |
| `dimensions` | `[1028, 1062]` for a SINGLA 1M; frames take their real shape from the stream |
| `zmq_rcvhwm` | the receive queue length, below |

Every key and its default is in `docs/DESIGN.md` §2.2 and §11.3.

If the program already has a `dectris-simplon.yaml` from an older driver, start from the example
and carry over only the addresses and the keys that DESIGN lists. Other keys of the old file can
still set properties of this driver (for example `stream_mode`), so leave them out.

**The receive queue length.**

- **Default 500.** Right for code that places each frame by its `image_id` and the detector's frame
  times, and for Instamatic's own experiments.
- **Set `zmq_rcvhwm: 4`** if the program pairs each frame with the stage tilt read when the frame
  arrives. A frame that waited in a long queue would get the wrong angle. At 4, frames can be lost
  at high frame rates. `docs/DESIGN.md` §2.2 has the figures.

Check: self-check C7.

### 6. Live view (strongly recommended)

Find the class that grabs frames for the live view. In Instamatic it is `MediaGrabber` in
`src/instamatic/camera/videostream.py`. The example patch changes it so that:

- **It uses a camera's own preview when it has one.** A camera with `get_preview_image()` is read
  at most once per frame time, in place of one `get_image()` per frame. For this driver that is
  the detector's Monitor, so the live view no longer arms and triggers the detector for every
  frame.
- **A camera error reaches the caller.** Without this, an exception in the grabber ends its thread
  and a waiting `get_image()` or `get_movie()` never returns.
- **A movie that ends short raises `RuntimeError`**, and a movie the caller stops reading is closed,
  which lets the camera end its series.
- **One requester at a time,** and the grabber waits while blocked instead of looping. Use a
  `threading.Lock` for this, not an `RLock`: Instamatic's `ctrl.get_movie()` steps the movie in a
  worker thread and closes it from the caller's thread, and an `RLock` cannot be released there.
- **`stop()` calls the camera's `request_stop()`** if it has one, so a long camera call does not hold
  up the program's exit.

The changes concern every camera, not only this one. Copy them from the example patch, or make the
same changes at the equivalent place.

Check: self-check C8 sees only the first point. For the others, run
`tests/super_dectris/test_grabber.py`, then take a movie with `ctrl.get_movie()` followed by one
`ctrl.get_image()`: both must return.

### 7. Stored stream messages (only if the program has such a feature)

If the program saves raw Stream V2 messages and decodes them later, decode them with the driver:

- `decode_message(raw)` returns a dict with `type`, `series_id`, `image_id`, `start_time`,
  `stop_time`, `real_time` and `data`, a dict of arrays by channel (`'threshold_1'`).
- `decompress_image`, `decompress` and `tag_decoders` serve code written for other decoders.
- `docs/DESIGN.md` §2.1 lists them, and §4.6 covers saved messages.
- They are module functions and need no camera. Do not make a second camera object just to decode:
  connecting one can disarm a series that is running.

Check: decode one saved message and compare it with what the program wrote at the time.

### 8. Older Dectris drivers (if present)

Remove the imports of any older Dectris driver from the camera factory, and from any code that
decodes stream messages, so that only this driver is loaded for `dectris_simplon`.

Check: self-check C9 lists every other Dectris import left in `camera.py`.

### 9. Executables (only for EXE builds)

A program bundled with PyInstaller or Nuitka must include the following, in every build script or
spec file the program uses:

- `_super_dectris.cp312-win_amd64.pyd` as a binary, at `instamatic/camera/super_dectris/`;
- the hidden imports `instamatic.camera.super_dectris` and
  `instamatic.camera.super_dectris._super_dectris`.

The extension's own imports are invisible to a bundler. If the EXE fails on import, add these
first: `numpy._core.multiarray`, `pathlib`, `logging`, `json`. See `docs/DESIGN.md` §7, EXE.

Check: start the EXE and connect to the camera.

### 10. Self-check and tests (required)

```
python -m instamatic.camera.super_dectris.selfcheck
```

What it does:

- One line per check: `PASS`, `WARN` or `FAIL`, with the step to fix.
- Exit code 0 when nothing failed.
- `--json` gives the same for an agent.
- `--dcu` also asks the control unit, read-only: two GET requests and one TCP connection.
- `--config DIR` uses another configuration folder (`DIR` holds `config/`). With `python -m`,
  the program reads its default configuration once before that. To leave the default
  configuration alone, set the `instamatic` variable to `DIR` instead, or run the file directly
  with `--config`.
- If the command fails while importing the driver, run the file itself instead:
  `python src/instamatic/camera/super_dectris/selfcheck.py`.

| check | tests |
|---|---|
| C1 | Windows x64, CPython 3.12 |
| C2 | step 4 |
| C3 | step 2, and that Python imports the driver from the program's tree |
| C4 | the driver's registration as a `CameraBase` |
| C5 | step 3 |
| C6 | decodes a test message exactly |
| C7 | step 5 |
| C8 | step 6 |
| C9 | step 8 |
| C10 | with `--dcu`: the control unit and the stream port answer |

Then run the Python tests, `python -m pytest tests/super_dectris`. They need `pytest`, `pyzmq`,
`cbor2<6`, `h5py`, `hdf5plugin`, `tifffile`, and `dectris-compression` from `third_party/` (a
source package: installing it needs a C compiler, such as the Visual Studio Build Tools). A
skipped test checked nothing.

## After the integration

The integration comes first: finish steps 1 to 10 before any checks on the detector. Then, for the
first runs on the detector, go through `docs/DESIGN.md` §8 Tier 3 in order. `docs/PROTOCOL.md`
§15 lists the open points and what to do about each.

## Do not change

- The files in `super_dectris/`. To change the driver, change its Rust source and rebuild it.
- The `.pyd` of one version together with the Python files of another.

## When something fails

| symptom | cause | fix |
|---|---|---|
| `ImportError: DLL load failed` for `_super_dectris` | not CPython 3.12 x64, or the Visual C++ Redistributable is missing (plain venv without conda) | use CPython 3.12 x64; install the Redistributable (x64) |
| C3 says the driver is imported from another folder | the program is installed non-editable, or another copy is on `sys.path` | `pip install -e .` in the program's tree |
| `AttributeError: __app_title__` on connect | step 4 missing | add the two strings |
| live view slow, detector arms for every live frame | step 6 missing | apply the live view changes |
| `DectrisTimeout: movie ended ... after k of n frames` | the series ended early: wrong stream address, another client disarmed it, or frames lost | check `ZMQ_host`/`ZMQ_port`, that no other program holds the stream, and the queue length (step 5) |
| frames missing at high rates | `zmq_rcvhwm` too small for the rate | keep 500, and place frames by `image_id` |
| WARNING `no Stream V2 start within ... of arm` | stream address or stream mode | check `ZMQ_host`/`ZMQ_port`; the driver enables the stream itself |

## Licence when you distribute

The driver is under the Apache License, Version 2.0 (`LICENSE`):

- Using it in your own lab needs nothing more.
- If you distribute it, on its own or inside a program (for example, an executable given to
  others), do the following:
  - include `LICENSE`, `NOTICE` and `src/instamatic/camera/super_dectris/THIRD_PARTY_NOTICES.md`;
  - mark the files you changed.

  The rest of the program may be under any licence, closed source included.
- The compiled driver contains libzmq, which is under MPL-2.0. `THIRD_PARTY_NOTICES.md` says where
  its source is. Keep that with the program.
- The licence grants no right to use the author's name, or DECTRIS's names, to promote a product.

`examples/instamatic-2.2.2.patch` contains lines of Instamatic's files as context. Those lines stay
under Instamatic's licence: BSD 3-Clause, Copyright (c) 2021, Stef Smeets
(https://github.com/instamatic-dev/instamatic). Its full text is `examples/LICENCE-instamatic`.
