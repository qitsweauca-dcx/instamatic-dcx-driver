# Design of the Rust driver for the Dectris SINGLA detector

This document describes how the driver is built and why, for someone who has to read it or change it. Comments in the Rust source cite its section numbers, for example `DESIGN §4.3 Q2` or `DESIGN §7 [R36]`, so the numbers are kept even where a section is short. It describes the driver in this repository, put into Instamatic at commit `ded7d4b` (version 2.2.2) with `examples/instamatic-2.2.2.patch` applied. "Instamatic" and "the program" mean that version here.

**Callers named in the reasons.** Many reasons below name experiment and GUI code that drives the detector directly: a cRED, autoCRED or FAST-ADT that arms, triggers and receives through the driver's own methods, and a GUI that polls the detector's state every 2 s and sends raw SIMPLON commands. Instamatic's own experiments and GUI do not do this; what they call is in §11. The reasons are kept because they hold for any caller that does, such as a script of one's own.

**Labels.** Bracketed marks such as `[R12]`, `[G1-E]`, `[G2-H]` and `[#12 keep]` are only labels. They carry no meaning and no order. A source comment and a paragraph here that have the same label are about the same thing, so you can search for one from the other. Three other kinds of number do have a meaning:

- `R1`..`R7` and `R3a` written with §3.2, as in `§3.2 R7`, are the numbered locking rules of §3.2. They are numbered separately from the bracketed labels, even though `[R7]` and `R7` look alike.
- `Q1`..`Q30` are the rows of the table in §5.
- `§4.2 step 4` and similar name a numbered step in that section.

**Other documents cited in comments.** A comment that gives a path cites `docs/`, the folder of this file. `PROTOCOL §n` is `PROTOCOL.md`, beside this file: what the detector sends and accepts. `DESIGN §12 #n` is a decision listed in §12, with its reason. A phase number in a comment, such as `Phase 1`, and `review #n` in a comment or test are labels too, like the bracketed ones: they carry no meaning and no order.

**Marks.** **Open**, **Reported** and **Measured** are the marks of PROTOCOL's "How to read this document": **Open**, an open point to look at on a detector; **Reported**, reported from use with a detector; **Measured**, measured on the development machine with synthetic frames.

## Contents

- §1 Architecture: the crates and layers, and what `instamatic-2.2.2.patch` changes in Instamatic.
- §2 The Rust and Python boundary: classes, constructor, members, iterators, dict shapes, arrays, logging.
- §3 Concurrency: threads, the locking rules R1 to R7, the GIL, trigger, exceptions, shutdown.
- §4 Protocol layer: REST client, state policy, ZeroMQ, CBOR, bslz4 and lz4, decoding saved messages.
- §5 Behaviours worth knowing: the table Q1 to Q30.
- §6 Error model.
- §7 Build and packaging.
- §8 Test strategy, and what to check first on a detector.
- §9 Performance bounds.
- §10 Open points, and choices that may change on a detector.
- §11 What Instamatic expects of the driver.
- §12 Decisions cited in comments, with their reasons.

## Terms

| term | meaning |
|---|---|
| DCU | Detector control unit: the computer that belongs to the detector. The driver talks to it over the network. |
| SIMPLON | The DCU's REST interface (version 1.8 here), used for settings, status and commands. |
| Stream V2 | The DCU's data stream: CBOR-encoded messages sent over ZeroMQ, one ZeroMQ message for each Stream V2 message. |
| ZeroMQ, PUSH, PULL | A message library. The DCU has a PUSH socket and the driver a PULL socket. Several PULL clients would share the messages in turn (round-robin), so each receives only part of the frames. |
| CBOR | A compact binary format similar to JSON. |
| bslz4, lz4 | The two compression formats of the image data. bslz4 means bitshuffle followed by LZ4. |
| series | The messages of one acquisition: one `start`, the `image` messages, one `end`. |
| arm, trigger, disarm | SIMPLON commands. `arm` prepares a series and makes the DCU send the `start` message. `trigger` begins exposing. `disarm` ends the series. |
| ints, inte, exts, exte | Trigger modes. `int` is a software trigger, sent as a command; `ext` is an electrical signal. `s` (series): one trigger starts the whole series of `nimages` frames. `e` (enable): one trigger or enable pulse for each frame, and in inte the exposure is the `count_time` sent with the trigger. |
| GIL | The lock that a Python thread holds while running Python code. "Detach" means releasing it during slow work, so other Python threads can run. |
| Tk thread | The main thread of the program's GUI. Blocking it freezes the windows. |
| yaml | The camera's settings file, `config/camera/dectris-simplon.yaml`. |

## 1. Architecture overview

The driver is one Python extension module, `_super_dectris`, compiled from Rust. It owns everything that talks to the detector: REST, ZeroMQ, CBOR, decompression, the policy for the detector's states, and the `CameraDectris` class that the program uses. There is no Python glue beyond a small `__init__.py`.

```
src/instamatic/camera/super_dectris/
├── __init__.py          re-exports the names; calls CameraBase.register(CameraDectris)
├── _super_dectris.cp312-win_amd64.pyd     the compiled driver (built by build.ps1, not committed)
├── README.md
└── rust/                Cargo workspace; not a Python package (no __init__.py) [R28]
    ├── build.ps1  Cargo.toml  Cargo.lock  rust-toolchain.toml  clippy.toml  LAYOUT.md ...
    ├── crates/dectris_common/    logging macros, DriverError, lock helper, test helpers
    ├── crates/dectris_codec/     cbor, typed_array, framing, bitshuffle, lz4, image, message, encode
    ├── crates/dectris_simplon/   client, detector, stream_cfg, filewriter, monitor, tiff,
    │                             signals, session, worker
    ├── crates/dectris_stream/    zmq_link, receiver, attribution, message, guard, registry, stats
    ├── crates/dectris_core/      facade; driver.rs (single-frame and movie paths), preview.rs
    └── py/src/                   the Python layer: camera_dectris.rs, core.rs, iters.rs, convert.rs,
                                  errors.rs, logging.rs, session.rs, stats.rs, lib.rs
```

The crates depend on each other in one direction only:

```
dectris_common <- dectris_codec <- dectris_stream
dectris_common <- dectris_simplon
all of them    <- dectris_core  <- py (crate super_dectris_py, library _super_dectris)
```

Only `py/` uses pyo3, so everything below it can be tested with `cargo test` without Python. Only `py/src/camera_dectris.rs` imports `instamatic`. `rust/LAYOUT.md` has the crate map, the build and the `dectris_common` API.

**Threads and data flow:**

```
 Python threads of the program                                     DCU
 -----------------------------                                     ---
 Tk thread (a GUI's polls, buttons)     --+
 MediaGrabber thread (live view)          |
 experiment thread (cRED, FAST-ADT, ...)  +--> CameraDectris  -- REST (ureq) ----> SIMPLON
 trigger threads of a caller's own        |    (py/, holds the GIL only            DCU_host:DCU_port
 a GUI's command threads                --+     to make Python objects)                ^
                                                     |                                 |
                                                     | detach: GIL released            |
                                                     v                                 |
                                              Driver (crates/)                         |
                                               |            \                          |
                                               |             worker threads, one PUT each:
                                               |             dectris-trigger-<n>, dectris-init-<n>
                                               |             never touch Python
                                               v
                                       ZeroMQ PULL socket  <--- Stream V2 ---- ZMQ_host:ZMQ_port
                                       (libzmq's own thread queues up to zmq_rcvhwm messages)
```

A Python thread calls a method. The method detaches, does its HTTP, ZeroMQ or decompression work in Rust, returns to Python to build the result, and only then writes any log records or console lines that the work produced (§3.2 R6). The driver has no permanent thread of its own. Short-lived worker threads run the `trigger` and `initialize` PUT requests (§3.4, §4.2), and they never attach to Python (§3.1). libzmq runs its own internal I/O thread, which queues incoming messages.

**Changes to Instamatic.** `examples/instamatic-2.2.2.patch` changes only these three files, and shows every line:

1. `camera/camera.py`: the interface `dectris_simplon` imports `instamatic.camera.super_dectris.CameraDectris`. Instamatic's own `simplon` interface stays as it is.
2. `camera/videostream.py`: hands camera errors back to the caller, and uses a camera's own preview for the live view (§3.6).
3. `__init__.py` gets `__app_title__` and `__app_version__`, which the driver reads for its User-Agent.

No experiment and no GUI file is changed (§11).

## 2. Rust and Python boundary

### 2.1 Python classes and module contents

| name | what it is |
|---|---|
| `DectrisCore` | The driver's methods and properties. `#[pyclass(subclass, frozen)]` with its state in an `Arc`; `frozen` means no method takes `&mut self`, so no "already borrowed" error can arise between threads. |
| `CameraDectris` | `#[pyclass(extends=DectrisCore, subclass, frozen, dict, weakref)]`: the class the program uses (§2.2). |
| `FrameIter` | Backs `receive_frames`, `receive_frames_with_metadata`, `receive_raw_frames`. Stops silently (§2.6). |
| `MovieIter` | Backs `get_movie` and `stream`. Arms on its first `__next__`, disarms with its last frame or on `close()`. Raises on a short movie (§2.6). |
| `StreamStats` | The snapshot `cam.stats`. Fields: `received, delivered, stale, flushed, flushed_armed, timeouts, reconnects, trigger_unblocked, triggers_skipped, attribution_mismatch, log_errors, last_recv_monotonic`. |
| `DectrisSession`, `DectrisResponse` | `cam.session` and what its calls return (§2.5). |
| `CborTag` | A small frozen object with `.tag` and `.value`: how `decode_message` shows a CBOR tag it does not know (§4.4). |
| exceptions | `DectrisError(Exception)`, `DectrisHTTPError(DectrisError)` with `.status_code` and `.url`, `DectrisConnectionError(DectrisError, ConnectionError)`, `DectrisTimeout(DectrisError, TimeoutError)`, `DetectorStateError(DectrisError)`, `DectrisBusy(DectrisError)`, `StreamDecodeError(DectrisError, ValueError)`. See §6. |

All pyclasses declare `module="instamatic.camera.super_dectris._super_dectris"`. `CamServer.get_attrs` returns `type(getattr(cam, name))` for every public attribute, and with the pickle serializer a class whose `__module__` is `builtins` cannot be pickled by reference [G1-Q].

Module-level names:

- `_STREAM_END`: one `object()`, returned by `message_receiver` at the end of a series.
- `decode_message(raw: bytes) -> dict`, `decompress_image(message: dict) -> ndarray`, `decode_raw_header(raw: bytes) -> dict`.
- `decompress(algorithm: str, elem_size: int, data: bytes) -> bytes`, the hook that tests use to compare with other decoders.
- `TYPED_ARRAY_DTYPES` (tag to numpy dtype string, the §2.8 table) and `tag_decoders` (tag to a decoding callable, for tags 0, 40, 1040, 56500 and 64 to 87) [#12 keep].
- `OPTION_KEYS`, `reset_log_cache()`, `__version__`, and the test hooks `_fine_timer_counts` and `_test_attach_counts`.

### 2.2 Constructor and `CameraDectris`

`CameraDectris` is written in Rust, in `py/src/camera_dectris.rs`. It has no Python base class, so `__init__.py` registers it with `CameraBase.register`, and it carries itself what `CameraBase` would give: `load_defaults`, `get_name`, `get_camera_dimensions`, `__enter__`, `__exit__`. It also has `get_image_dimensions`, `get_preview_image(binning=None, **kw)`, `get_image(exposure=None, binning=None, **kwargs)` (accepts `binsize`), `get_movie(n_frames, ...)`, `stream`, `establish_connection`, `release_connection`, `close`, and the class attributes `streamable = True` and `ZMQ_RCVHWM = 500`.

`#[new]` takes `*_args, **_kwargs` and makes an unconnected object with no I/O, so both `cls()` (used by autoconfig) and `cls(name=...)` work. `__init__(name=None)` then does this, on the caller's thread:

1. **Name.** If `name` is None: `config.camera.name` when `config.camera.interface == 'dectris_simplon'`, else `'dectris-simplon'` [G1-K]. This reuses the live config without reloading it, and makes the zero-argument call load the right yaml.
2. **`load_defaults()`.** Reloads the config only if the name differs from `config.settings.camera`, then sets every yaml key as an attribute. A subclass's own `load_defaults` is still called.
3. **`zmq_rcvhwm`** becomes `ZMQ_RCVHWM`. A bad value gives a warning `ignored; using 500`.
4. **`connect()`** sets `DCU_endpoint` (`http://host:port`), `ZMQ_stream`, `convert_uint16 = False` (the driver never reads it), `session`, and the User-Agent `__app_title__/__app_version__`. A control character in it (below 0x20 except tab, or 0x7F) becomes a space, with one WARNING: the HTTP library would otherwise panic at every request. A yaml without `DCU_host` and the like raises `AttributeError`.
5. **`_connect(...)`**, below. The Stream header appendix is sent as `''`, so that a value left on the DCU by another client is overwritten.
6. **Exit hooks**, registered once per process over a `WeakSet` of live cameras: `threading._register_atexit` runs the first shutdown step, `atexit` the last (§3.7).

Every public `DectrisCore` descriptor is copied into the `CameraDectris` class dict, because `CamClient._init_dict` looks only at the class's own `__dict__` [R39]. `default_binning` is a property whose setter records that the yaml set it; `_connect` uses the passed `default_binsize` only if it was not set.

**Yaml options.** `OPTION_KEYS` lists these 13. Each is also a writable property, each has a default so that the yaml can stay as it is, and none contains the substring `name`, because `config_updater` treats a camera key containing `name` as old-style and rewrites the file.

| key | default | meaning |
|---|---|---|
| `auto_initialize` | True | May the driver initialize the detector when it finds it in a bad state (§4.2)? |
| `receive_timeout` | None | Seconds to wait for a frame; None means computed (§4.3 Window). |
| `trigger_blocking` | True | Does public `trigger()` wait until the series is done (§3.4)? |
| `frame_time_policy` | `'readout'` | `'readout'`: omitted frame time = exposure + readout time. `'detector'`: leave the DCU's value. |
| `stale_series_policy` | `'drop'` | `'drop'` or `'deliver'`; governs stale images only [R12][G2-D]. |
| `series_attribution` | `'sequence_id'` | `'sequence_id'`, `'fifo'` or `'off'` (§4.3). |
| `single_frame_fresh` | True | Does `acquire_single_frame` drop queued frames first (§4.2) [R24]? |
| `log_frames` | False | Per-frame log records. |
| `monitor_max_hz` | 10 | Rate limit of the live preview. |
| `zmq_tcp_keepalive` | False | TCP keepalive on the stream socket. |
| `zmq_rcvbuf` | 4194304 | TCP receive buffer in bytes; 0 lets the OS decide (§4.3 Socket). |
| `stream_header_detail` | None | Value for the DCU's `header_detail`; None means never written. |
| `stream_decode_start_arrays` | False | Decode the large arrays of a `start` message instead of skipping them (§4.4). |

Two more yaml keys are read but are not in `OPTION_KEYS`:

- `zmq_rcvhwm`: the receive queue length in messages, default 500. A full queue of 500 holds about 170 MB at 0.344 MB per compressed frame (measured on the development machine with synthetic frames). **Set it to 4** if a caller pairs each frame with the stage tilt when the frame arrives: a frame that waited in the queue would get a wrong angle. Instamatic's own experiments do not do this (§11.4). At 4, frames can be lost at high rates: on the development machine with synthetic frames, from about 700 frames per second.
- `windows_fine_timer`: default true. While a camera is connected, the driver holds the Windows timer at 1 ms with `timeBeginPeriod(1)`, and ends this on release. Reason: a thread returning to the GIL from a busy Python thread waits for a timer tick, by default 15.6 ms, for every frame. Measured on the development machine with synthetic frames at 100 frames per second with a CPU-bound Python thread: 42 frames per second with two detaches per frame, 61 with one on the default timer, 100 with the 1 ms timer. `sys.setswitchinterval(0.001)` does not help.

**Lenient setters [R33].** `load_defaults` sets every yaml key with no guard, so a bad value must not abort `get_camera`. Each option setter, and the setters of `trigger_mode`, `compression`, `stream_mode`, `stream_format` and `default_binning`, accepts any Python object and converts leniently: bool from a bool, an int or the strings `true/false/yes/no/1/0`; a number from an int, a float or a numeric string; a string from `str`. On failure it logs one warning (`<key>=<value!r> ignored; keeping <default>`) and keeps the default. The method `set_trigger_mode` still raises `ValueError`.

**`_connect()`** runs with the GIL released and also installs the interrupt hook when on the main thread:

1. Build the HTTP agent (connect timeout 1 s).
2. Startup probe: GET `status/state`, 3 s in total. On a transport failure: WARNING `DCU unreachable ...; skipping startup checks`, set `dcu_reachable = False`, start the offline back-off (§4.1 [R10]), skip the rest of the I/O below.
3. Otherwise: read `trigger_mode` (INFO if it matches the yaml, WARNING if not); `enable_stream_v2()`; read `x/y_pixels_in_detector` and warn if it differs from the yaml `dimensions`; send `disarm` **only if the probe read `ready` or `acquire`**; enable the Monitor. The constructor never writes the detector configuration and never initializes. Note what step 3 implies: constructing a second camera while a series runs disarms that series (§4.6).
4. ZeroMQ: take the single-PULL guard (§3.7). The holder connects now, not at arm, because the manual says streamed data is not buffered, so the PULL must be connected before `arm` makes the DCU send its `start` (the limit of this choice is in §3.7 [G1-L]). The console gets `[Dectris] ZMQ connected: {addr} (buffer HWM={n} messages)`.

Every step is best effort, so a camera can be constructed with the DCU off (CamServer, autoconfig, tests).

**Preview scaling.** `get_preview_image` scales in `dectris_core::preview::fit` to 640 pixels on the larger side, with the same mapping as `scipy.ndimage.zoom(order=1)`. It returns zeros `(640, 640)` of `uint16` when there is no frame, and logs one WARNING per failure episode, not per call [G1-J].

### 2.3 Which members exist [R40]

On `DectrisCore`:

- **Connection:** `establish_connection()`, `release_connection()`, `close()`. `CameraBase` declares the first two, and `with cam:` calls them. `establish_connection()` connects the ZeroMQ PULL again if this instance has none, for example after a `release_connection()`, taking the single-PULL guard again (§3.7); on `CameraDectris` it also holds the 1 ms Windows timer again (§2.2 `windows_fine_timer`). Otherwise, and on an object that was never connected, it does nothing. `release_connection()` and `close()` are the final release of §3.7 step 5: a guarded disarm, close the PULL, release the guard; on `CameraDectris` they also end the 1 ms timer.
- **Modes and stream:** `set_trigger_mode`, `enable_stream_v2() -> bool`, `disable_stream_v2`, `get_stream_mode`.
- **Status and high voltage:** `get/set_countrate_correction`, `get_detector_state`, `get_hv_state`, `hv_enabled(state)`, `gate(enable=None)`.
- **Commands:** `arm`, `disarm`, `trigger(timeout=None, count_time=None)`, `initialize`, `abort`, `cancel`.
- **Configuration:** `set_config(nimages, exposure, ntrigger=1, frame_time=None)`, `ensure_config(same)`.
- **FileWriter** (the DCU's own HDF5 writer): `configure_filewriter(mode='enabled', name_pattern='series_$id', nimages_per_file=1000, compression_enabled=True, image_nr_start=1)`, `disable_filewriter`, `download_filewriter_files(destination) -> list[pathlib.Path]`, `clear_filewriter`.
- **ZeroMQ:** `reconnect_zmq() -> bool`, `flush_zmq_buffer() -> int`.
- **Receiving:** `message_receiver(timeout=2000)`, `decompress_image(msg)`, `receive_frames(n_frames, binning=None)`, `receive_frames_with_metadata(n_frames, binning=None)`, `receive_raw_frames(n_frames)`, `acquire_single_frame(binning=None)`, `get_binning`.
- **Shutdown:** `request_stop()`: sets `shutting_down` and wakes waiters, with no HTTP and no logging. Its effect is permanent for the instance [G1-S].
- **Private:** `_connect`, `_acquire_image`, `_movie`, `_monitor_frame`, and hooks for exit and tests.
- **Properties** (setters local and lenient, getters never fail): `trigger_mode`, `compression`, `stream_mode`, `stream_format`, `default_binning`, `_stream_start_config` (setting None clears it), and all of `OPTION_KEYS`.
- **Read-only:** `last_stop_reason` (`None`, `'n_frames'`, `'end'`, `'timeout'`, `'cancelled'`, `'shutdown'`), `stats`, `dcu_reachable`.

**[#12 keep] Members that nothing in the program calls.** They stay public so the surface of a Dectris camera is complete, and so `CamClient` and scripts can reach them. Each is a thin wrapper over something the driver already needs:

- `set_compression(algorithm)`: `bslz4`, `lz4` or `none`, else `ValueError`. Clears the ensure cache; `none` is local only, with a WARNING.
- `get_bit_depth() -> int`: 0 on any error. `enable_monitor()`: WARNING on failure. `get_filewriter_files() -> list[str]`: `[]` on error. `wait_for_idle(timeout=10.0, poll_interval=0.2) -> str`: returns the last state seen. None of these raises.
- `arm_for_preview()`: `ensure_config(nimages=99999, exposure=default_exposure)`, `arm()`, one non-blocking trigger in ints or inte. `disarm_preview()`: disarms only if `arm_for_preview` armed. The live view does not use them: it reads the Monitor.
- `establish_zmq_stream_connection()`: connect if this instance holds no PULL, else nothing.
- `stream(n_frames, exposure=None, binning=None, **kwargs)` (on `CameraDectris`): the same as `get_movie`.

**Not connected** (constructed without `_connect`, as a bare `DectrisCore()`): methods that need I/O raise `DectrisError('driver not connected')`; the others do nothing, and the getters answer `'unknown'` or `None`.

**After `release_connection()`** the driver object is kept, so the REST side still works: getters, commands, `set_config`, the Monitor preview and `cam.session`. Only the ZeroMQ PULL is gone. A receive path finds no socket, logs one WARNING `ZMQ stream not connected ... receive paths time out until reconnect_zmq()`, and times out. `get_image` and `get_movie` still configure and arm the detector, and trigger it in ints and inte, before that: after one window with no frame they log the no-start WARNING of §10 [G1-R] and send the guarded disarm of the series tail (§4.2), and after a second window they raise `DectrisTimeout` (`get_movie`: `movie ended (timeout) after 0 of n frames`, after one more guarded disarm). `establish_connection()` or `reconnect_zmq()` connects the PULL again.

### 2.4 Dynamic attributes and isinstance

The class has an instance `__dict__`, so it holds the yaml keys and attributes the program writes (`get_attrs`, `convert_uint16`). Rust properties are data descriptors, so they take precedence over `__dict__`: a write such as `setattr(cam, 'trigger_mode', 'ints')` from `load_defaults` goes to the lenient setter.

`isinstance(cam, CameraBase)` is true through `register`. `dir(cam)` followed by `getattr` never raises, which `CamServer.get_attrs` needs. There is no attribute named `cam`, and no member named `start`, `run`, `join`, `is_alive`, `frame`, `lock`, `grabber`, `requested` or `frametime`, because the wrapper `LiveVideoStream` tells cameras apart by such names (§11.2).

### 2.5 `session` and `DCU_endpoint`

A caller can make its own REST calls through `cam.session` and `cam.DCU_endpoint`. `session` is a `DectrisSession`, written in Rust, over its own HTTP agent:

- `get(url, *, timeout=None)` and `put(url, *, json=None, timeout=None)`; default timeout 30 s; a timeout must be above 0.
- The response has `status_code`, `ok`, `url`, `content`, `text`, `json()`, `raise_for_status()`.
- It shares no connection with the driver's own client and goes through neither the back-off nor the initialize gate of §4.1. Body limit 256 MiB. Safe from any thread.

The driver does not limit what these calls do. For example a menu's raw `arm` command is adopted through the attribution table (§4.3 row 3).

### 2.6 Iterators and `message_receiver`

Every receive path classifies messages against the armed series (§4.3 [R12]), holds a registry entry while active (§3.2 [R5]), and checks `shutting_down` on each poll slice [R6].

**`FrameIter.__next__`** stops silently: it never raises for a timeout or an end of series, because a caller that loops over it, such as a cRED that drives the detector directly, expects a plain end of iteration.

1. **Stop check.** If done or no frames remain: StopIteration with `last_stop_reason = 'n_frames'`. The registry entry is released when the last frame is handed out, not on the next call. Stop reasons are sticky.
2. **Attached first part (microseconds).** Register on the first call, with the calling thread as holder. Allocate a fresh numpy array from the last frame's shape (`ShapeMemo`). No array is kept between calls [R9][G1-T]. The first frame, a shape change and raw mode go by the slow path of step 4.
3. **One detach [R9]:** repeat: lock the socket for one `poll(POLLIN, min(budget_left, 250 ms))` and `recv`; drop the lock; scan the header; classify (§4.3). A start of the armed series updates the start cache after the socket lock is dropped, never nested [R2]. The armed series' end ends the wait. An armed image is decompressed straight into the numpy buffer if shape and dtype match; otherwise its location goes back for an attached allocation. Stale messages are counted and skipped by policy. An unknown `type` gives one WARNING. Non-image messages do not extend the deadline.
4. **Attached last part.** Emit the log records held back during the work (§3.2 R6). If an array is still needed (first frame, or a changed shape or dtype), allocate it and detach again to decompress. Timeout: `'timeout'`, INFO, StopIteration. End: `'end'`, wakes trigger waiters (§3.4). Shutdown: `'shutdown'`. Image: return `ndarray`, `(ndarray, meta)` or `(bytes, meta)`, where `bytes` is the complete wire message with no decompression. `binning > 1` averages in Rust and gives float64, like `image_utils.apply_binning`.

`FrameIter` does not stop on `cancel`, because frames already buffered should still be delivered. `close()` wakes a blocked `__next__` within one slice.

**`MovieIter` [R7][R14].** First `__next__`:

0. Series ownership check (§3.2 R7): `DectrisBusy` with no HTTP if another thread owns a running series [G1-E].
1. State Idle to Arming, briefly under a lock.
2. Detached: `ensure_config(nimages=n, ntrigger=1, exposure)` for ints and exts, or `ensure_config(nimages=1, ntrigger=n, exposure)` for inte and exte, since the detector ignores `nimages` in enable modes [G2-B]; then `arm()`; then the trigger: ints one internal trigger for the series (§3.4), inte one serialised `trigger(count_time=exposure)` before each frame, exts and exte none.
3. If `close()` was requested meanwhile: Done, a guarded disarm, StopIteration. Otherwise Armed.
4. The state lock is never held across a detach.

Later: frames arrive by the `FrameIter` rules. The movie disarms with its n-th frame. If frames are missing when the window ends, the series tail rule applies (§4.2). **A movie never ends short without raising [G1-F]:** an end of the armed series (for example a `disarm()` from another thread), a timeout or a shutdown raises `DectrisTimeout('movie ended (<reason>) after k of n frames')` after a guarded disarm; on shutdown there is no HTTP. `close()` from another thread is safe. Dropping the iterator clears series ownership and does no I/O [R8]. Nothing in the program calls `close()` on the camera's iterator directly, so `MediaGrabber`, as changed by `instamatic-2.2.2.patch`, does it in a `finally` (§3.6) [R7].

**`message_receiver(timeout=2000)`** returns an image dict with `'_start_config'`, or `None`, or `_STREAM_END`. It returns `None` at once when the armed start has been cached, since its only caller throws the value away (Q3). It also returns `None` after `timeout` ms, shared with non-image messages. A stale start or end is skipped and counted [R12].

### 2.7 Metadata and dict shapes

`meta = {'series_id': int, 'image_id': int, 'count_time': float | None, 'real_time': [int, int] | None}`: exactly four keys.

- `count_time` comes from the start attributed to the armed series; `None`, with one WARNING per series, when that start is absent (§4.3).
- `decode_message(raw)` returns every top-level key: `real_time`, `start_time`, `stop_time` as `[num, den]`, `series_date` and `arm_date` as ISO-8601 `str` (Q17), `data` as `{channel: ndarray}` with the typed-tag table of §2.8.
- `decode_raw_header(raw)` returns only `type`, `series_id`, `series_unique_id`, `image_id`, `real_time`.
- `decompress_image(msg)` accepts four shapes: `msg['data']['threshold_1']` already an ndarray (returned as is); an object with `.tag` and `.value` for tag 40 or 1040 (decoded with the same checks); `bytes` (through `decode_message` first); a missing channel gives `StreamDecodeError` listing the keys.

### 2.8 numpy arrays

Arrays are allocated by numpy: C-order, `owndata == True`, writable. The dtype comes from the typed tag only. Every frame is a fresh array, so no two frames share memory.

**Checks before any allocation [R19]:** on the image path, two dimensions, none zero, and `rows * cols * itemsize` (checked multiplication) equal to the 12-byte header's `orig_size` (compressed) or to the payload length (raw). Every size must be at most 256 MiB. `decode_message` allows N-dimensional arrays with the same product checks. An overflow is a `StreamDecodeError`.

**Typed tags [R21]** (RFC 8746):

| tag | numpy dtype |
|---|---|
| 64, 68 | `u1` |
| 65 / 69 | `>u2` / `<u2` |
| 66 / 70 | `>u4` / `<u4` |
| 67 / 71 | `>u8` / `<u8` |
| 72 | `i1` |
| 73 / 77 | `>i2` / `<i2` |
| 74 / 78 | `>i4` / `<i4` |
| 75 / 79 | `>i8` / `<i8` |
| 80 / 84 | `>f2` / `<f2` |
| 81 / 85 | `>f4` / `<f4` |
| 82 / 86 | `>f8` / `<f8` |
| 76, 83, 87 | `StreamDecodeError` (unassigned or float128) |

These wire spellings are how the table names them; big-endian tags are byte-swapped at decode into the little-endian dtype. float16 is kept as raw `u16` bits and viewed as `<f2` in `convert.rs`. The image path accepts only 64, 68, 69 and 70; anything else there is a `StreamDecodeError`.

**Tag 1040 [R19]:** column-major data. The logical shape stays `dims`; the decoder decompresses into scratch memory, then copies into a C-order array of shape `dims`. A test compares this with `np.reshape(buf, dims, order='F').copy(order='C')`.

### 2.9 Logging [R1][R8][R31]

Logs reach Python's `logging` through pyo3-log. Records land on `instamatic.camera.camera_dectris` and `instamatic.camera.camera_dectris.stream`. A log panel can attach to these names, so they must not change. `dlog!` and `slog!` in `dectris_common` are the only logging entry points; `clippy.toml` forbids bare `log::*` macros [R31].

The logger is `GuardedLogger`, wrapping a pyo3-log logger that passes only `instamatic` targets. Its purpose is that no thread takes the GIL for a record that nobody wants, and that some threads never take it at all:

- `GuardedLogger::log` returns first if the thread-local `NO_PYTHON` is set (the first statement of every worker), or if the static `LOGGING_CLOSED` is set, or if the inner logger's cached `enabled()` says no. That last check is needed explicitly, because the `log` macros call `log(record)` directly once the global level passes [G1-A]. So the per-request debug records of `ureq` never take the GIL on any thread, even with the root logger at DEBUG, including one that `ureq` writes while holding its pool lock.
- Only then does it attach. It keeps any Python exception raised by a handler from leaking: without this, a raising handler (for example a GUI log handler during Tk teardown) would turn into `SystemError: ... returned a result with an exception set`. Such errors count in `stats.log_errors` [R8]. A `KeyboardInterrupt` raised in a handler is re-delivered through `_thread.interrupt_main()`.
- `LOGGING_CLOSED` is process-wide, so it is set only on the interpreter exit path. An explicit `close()`, `release_connection()` or `with cam:` never sets it, so closing one instance cannot silence others [G1-C].
- `reset_log_cache()` clears the cached levels.

**Why workers must not touch Python.** On CPython 3.12, pyo3 0.29 compiles its finalizing guard only for 3.13 and later. A thread attaching during interpreter finalization therefore calls `PyGILState_Ensure` on a finalizing interpreter, and after finalization `attach()` panics. `NO_PYTHON`, the target filter and `LOGGING_CLOSED` together make "workers never attach" a guarantee and not a habit.

**Volume.** Per-frame records, and every record from a `Drop`, are gated by `log_frames` (default off). Otherwise there is one INFO per series at stop: `received N frames (M stale, K timeouts) in X s`.

**stdout.** The `[Dectris] ZMQ connected/reconnected/reconnect skipped` lines go to Python's `sys.stdout`, because the GUI console captures Python's stdout only. The Python layer writes them from events returned by the crates, after all locks are dropped; they are not carried in `Deferred`.

## 3. Concurrency model

### 3.1 Threads

The driver is called from: the Tk main thread, where a GUI of one's own (see "Callers named in the reasons" at the top) may poll the status every 2 s and call flush, reconnect, trigger mode and high voltage; the MediaGrabber daemon thread; the experiment thread; a caller's daemon `trigger()` threads; a GUI's command daemon threads; the executor of `TEMController`; and exit hooks.

Rust's own threads are short-lived workers, `dectris-trigger-<n>` and `dectris-init-<n>`. Each sets `NO_PYTHON` first, runs exactly one PUT, holds only what that needs, is wrapped in `catch_unwind` (a panic is stored as an internal error), and never attaches to Python, not even through a dependency's log record (§2.9) [R1]. A failure to spawn becomes a `DectrisError` [R9]. Live trigger workers are counted against a process-wide ceiling of 8 (§3.4) [R3].

### 3.2 State and locks

The state is split between five holders, the first column of the table, so that each crate owns its part and there is no `Arc` cycle (`Detector` holds hooks to `Stream`, which reaches back through a `Weak<Detector>`). `Driver` in `dectris_core` holds the others.

| where | state | lock | held for |
|---|---|---|---|
| `Stream` | ZeroMQ socket (`zmq::Socket` is `Send` but not `Sync`) | Mutex | one poll slice and recv (at most 250 ms), flush, reconnect, close |
| `Stream` | `SeriesState`: armed id, `own_start_seen`, `driver_armed`, the [G2-H] pin, start cache, self-check flags | one Mutex | microseconds; never nested with the socket lock [R2]. One lock and not separate atomics, because these fields must change together |
| `Stream` | receive registry, single-PULL guard entry, stats | Mutex, atomics | microseconds [R5] |
| `Detector` | settings (modes, options), `last_applied`, ensure cache | Mutex | microseconds. `last_applied` holds the last successful `set_config` values [R16] |
| `Detector` | `TriggerBook` (epoch, issued count, in-flight outcome), late outcomes, initialize slot | Mutex | microseconds [R3] |
| `Detector` | last good getter values, `armed_by`, `preview_armed`, `MonitorCache` | Mutex | microseconds |
| `Client` | HTTP agent; back-off deadline; initialize counter | lock-free or brief Mutex | no request is made under a lock [R10] |
| `Detector` | epoch, bumped by `arm()` | atomic | none |
| `Signals` | `cancel_requested`, `end_seen`, `shutting_down` | atomics | none |
| `Signals` | `trig_cv`: a generation counter `gen` and a Condvar [G1-B] | Mutex | microseconds per release; the only lock held across a wait, and only inside `Condvar::wait*`, which releases it (see R3) |
| `Driver` | `setup` | Mutex, `try_lock` only | while a series is set up (R7) |

Rules:

- **R1.** The socket lock is taken only while detached.
- **R2. Busy detection for `flush_zmq_buffer` and `reconnect_zmq` [R5].** The call is busy if the registry has a live entry held by **another** thread. An entry lives from an iterator's first `__next__`, or from entry to `message_receiver`, `_acquire_image` or `acquire_single_frame`, until StopIteration, the last frame, `close()`, Drop, or 2 windows without a `__next__`. Then `try_lock` on the socket is a second guard. When busy: flush returns 0 with a WARNING; reconnect returns `False`, with a WARNING and the console line `[Dectris] ZMQ reconnect skipped: receiver busy` [R32]. A GUI of one's own that disables its buttons during some runs only (see the top) leaves the other runs to this check. It is scoped to the other thread and is not a plain count of receivers, because a caller that drives the detector directly may keep its frame iterator referenced when it flushes in cleanup on the same thread, and that flush must work.
- **R3.** No lock is held across I/O, a return to Python, a wait, or the taking of another lock. Locks do not nest. The exception is `trig_cv`, held only across the `Condvar::wait*` call [G1-B].
- **R3a. Release protocol for `trig_cv` [R3a][G1-B].** Every releaser (`cancel`, `disarm`, `abort`, `request_stop`, a worker storing its outcome, the armed series' end) first stores its flag or outcome, then takes the mutex, increments `gen`, drops the lock and calls `notify_all()`. Every waiter reads `seen = gen` under the mutex, then uses `wait_timeout_while(guard, slice, |s| s.gen == seen && !released())`, where `released()` re-reads the flags. A flag stored between a waiter's check and its wait always bumps `gen` and wakes it, so no wake-up is lost and "cancel releases trigger" is certain.
- **R4.** `py.check_signals()` runs only on the main thread, and only in waits that hold no lock [R9], so Ctrl-C works during a long wait.
- **R5.** A poisoned lock (a panic happened while it was held) is recovered with `into_inner()` (`lock_unpoisoned` in `dectris_common`).
- **R6 [R2].** Logging, writing to Python's stdout and any return to Python are forbidden while any lock guard is alive. Code under a lock pushes records into a `#[must_use] Deferred` buffer, emitted after the guard drops. A lock taken on an attached thread detaches if it has to wait, so even a log record slipping under a lock cannot close a cycle between the GIL and a lock, including one through a log handler that marshals to the Tk thread. The test hook `_test_hold_lock` covers this rule (§8).
- **R7. Series ownership [G1-E].** `arm()` records the arming thread (`armed_by`). A path that would configure or arm on its own behalf (`_acquire_image`, `MovieIter`'s first `__next__`, `arm_for_preview`) first checks: is the driver armed, and does either a live receive entry belong to another thread, or `armed_by` name another thread with no `disarm`, `abort`, `initialize` or end of that series since? If so it raises `DectrisBusy('detector owned by a running series on thread <name>')` **with no HTTP**, so it neither disarms the running series nor competes for its frames. A time limit would leave a gap during a caller's rotation wait between `arm()` and the first frame, so there is none. These same three paths hold the `setup` lock with `try_lock` for their whole setup; contention raises `DectrisBusy('detector series being set up on another thread')`. An error after arm disarms and releases ownership, and `_acquire_image` clears ownership when its image arrives. Public `arm()`, `set_config()` and `disarm()` are not checked, because their callers own them. A forgotten manual arm is cleared by `disarm()`, for example from a GUI's Disarm button. Late events of an earlier arm are recognised by their epoch (`on_armed_end(epoch, attributed)`).

### 3.3 GIL release

The GIL is released around every HTTP call, ZeroMQ `poll`, `recv`, `flush`, `connect` and `close`, decompression and TIFF decoding, idle-wait sleeps, the trigger and initialize waits, and the Monitor's rate-limit sleep. `core::call` in `py/` does the detach, runs the work inside `catch_unwind`, and emits the `Deferred` records after returning to Python.

Nothing tied to the GIL crosses a detach. The decompress target crosses as a raw pointer and length (`OutBuf`), everything else as an `Arc` clone. `FrameIter` does receive and decompress in one detach whenever the array was ready [R9], because each extra return to the GIL costs a timer tick when another Python thread is busy (§2.2 `windows_fine_timer`).

### 3.4 `trigger()` and releasing the caller [R3][R4][R23]

Why this is elaborate: a DCU may hold a `trigger` PUT open until the whole series ends (**Reported**, PROTOCOL §15 #10), which can be hours for a 99999-image series, and a caller that triggers from its own thread needs that thread back after a Stop. So the PUT runs on a worker and the caller waits on a condition that other events can also satisfy.

`trigger(timeout=None, count_time=None)`:

0. **Mode.** exts or exte: `DetectorStateError('software trigger not allowed in <mode>')` with no HTTP [R23].
1. **Entry check [R4].** Note the epoch. If `cancel_requested` or `shutting_down` is set, return at once without a worker: INFO `trigger suppressed: series cancelled/disarmed — re-arm first`, `stats.triggers_skipped += 1`. `cancel`, `disarm`, `abort` and `request_stop` set their flag and wake waiters **before** their own HTTP call. A trigger from an earlier arm is also suppressed if the detector was armed again meanwhile.
2. **One series, one trigger [R3].** Under `TriggerBook`:
   - ints, internal callers (`MovieIter`, `acquire_single_frame`, `_acquire_image`, `arm_for_preview`, and public `trigger()` when `trigger_blocking` is false; `TriggerCaller::Internal` and `InternalBlocking`): PUT only if nothing was issued for this epoch, or if `nimages == 1` and `issued < ntrigger`. Otherwise skip, DEBUG `series already triggered`. In ints one software trigger starts the whole series.
   - ints, public blocking `trigger()` (a caller's own trigger thread, a GUI button; `TriggerCaller::Public`): PUT unless `issued >= ntrigger`, which logs a WARNING and skips.
   - inte: serialise. If an unfinished outcome of this epoch is in flight, wait for it first, with the release conditions of step 4. So at most one PUT per epoch is in flight.
3. **Spawn.** With 8 or more live workers in the process, including leftovers of earlier arms: `DectrisBusy('too many outstanding trigger requests')`. Otherwise a worker PUTs `command/trigger` (body `{"value": ct}` only when `count_time` is given), connect timeout 5 s, total timeout `timeout` if given, else:

   | mode | total timeout |
   |---|---|
   | ints | `nimages × frame_time + 60 s` |
   | inte | `frame_time + 60 s` |

   clamped to 30 s .. 24 h. `frame_time` and `nimages` are the effective values from `last_applied`, never None [G1-M]. With no known configuration: ints 24 h, inte 60 s. The worker stores its outcome and wakes waiters (R3a). If a DCU never completes a pending PUT after a cancel, leftover workers last until this timeout; §8 Tier 3 step 1 checks this.
4. **Caller wait, GIL released.** On the main thread in 250 ms slices with `check_signals()`; on another thread one wait for the whole budget (budget + connect + 5 s; past that it raises `DectrisTimeout`), with the protocol of R3a [G1-B]. The caller is released by:
   - the stored outcome: Ok returns; Err raises the mapped error;
   - `cancel_requested` or `shutting_down`;
   - `end_seen` for the current epoch, set only by the armed series' end (§4.3 [R12]).

   A caller released early returns normally (`stats.trigger_unblocked += 1`). The leftover worker finishes by itself, and a later `arm`, `set_config`, `trigger` or `initialize` drains its late outcome (Ok DEBUG, Err WARNING); `disarm`, `abort` and `cancel` do not. So a caller that joins its trigger thread with a time limit, such as `trigger_thread.join(timeout=10)`, succeeds even if the DCU never completes a cancelled PUT.

`trigger_blocking: false` means spawn (subject to step 2) and return at once. `acquire_single_frame` uses the non-blocking path: for a caller that arms once with `nimages=99999` and calls it per frame, such as an autoCRED that drives the detector directly, 200 calls issue one PUT under step 2.

The ceiling is 8 and not per-instance, because a leftover PUT can legitimately outlive its series. A small per-instance limit would refuse the third run in a row.

### 3.5 Limits on the preview

`get_preview_image` costs at most a 2 s GET, about 5 ms of decoding and turning (measured on the development machine with synthetic frames), at most a 100 ms rate-limit sleep, and the scaling. The rate limit follows the vendor's advice to stay under 10 Hz; the GUI's 0.05 s frame time would otherwise mean 20 Hz. HTTP 408 returns the cached last frame, so the view does not flicker black. During the offline back-off (§4.1) it returns the cached frame or zeros with no I/O. Any other error gives zeros.

`grabber.stop()` joins with no time limit while the grabber may be inside `get_image`, so it is bounded by other means: at exit by the first shutdown hook (§3.7) [R6], and on the GUI's close path by `cam.request_stop()` in `MediaGrabber.stop()` [G1-S]. Without that, a grabber waiting in `get_image`, `ensure_idle`, `initialize` could hold the closing thread for about 160 s.

### 3.6 Exceptions through the grabber [R7][R34]

In `videostream.py` before `instamatic-2.2.2.patch`, an exception in `MediaGrabber.run` kills the daemon thread while `LiveVideoStream.get_image` waits forever for its answer, and the single-worker executor of `TEMController` is stuck too. The driver raises where it has no image to give (Q1), so `instamatic-2.2.2.patch` changes `videostream.py`:

- `MediaGrabber.run` skips `r is None`. It wraps each request in `try`/`except Exception` and passes the exception on: `self.callback(exc, request=r)`.
- For the live view, a camera with `get_preview_image` is read through it, at most once per frame time; other cameras still get `get_image`. While blocked, the grabber waits instead of looping.
- For a movie (`_run_movie`): `it = cam.get_movie(...)`; it stops early if `self.request is not r`; `finally: getattr(it, 'close', lambda: None)()` disarms [R7]; and `RuntimeError(f'get_movie yielded {k} of {n} frames')` if short, for any camera [G1-F].
- `MediaGrabber.stop` sets the stop event, calls `getattr(self.cam, 'request_stop', lambda: None)()`, then joins [G1-S].
- `LiveVideoStream` has a `request_lock`. `send_media` queues `(request, media)` only if that request is the current one, and sets `frame` only if `media` is not an exception. `_next_requested(request)` skips stale items, raises handed-back exceptions, and raises `RuntimeError('the media grabber has stopped')` when the grabber is gone. `get_image` and `get_movie` use it.

The request check [R34] closes a further hazard: `TEMController` closes the wrapper after n frames while the grabber may still be appending, and a later `get_image` could otherwise take a stale movie frame or a stale exception.

### 3.7 Single-PULL guard and shutdown [R6][R8]

**Single-PULL guard.** ZeroMQ would share frames between two PULL clients. A process-wide `static GUARD: Mutex<HashMap<"host:port", (id, Weak<holder>)>>` allows one per endpoint. The first instance connects. A second logs WARNING `another CameraDectris already holds the PULL for ...` and stays without a socket until the holder releases, or until it calls `reconnect_zmq()`.

The guard works **only within one process** [G1-L]. Another process that opens its own PULL (the autoconfig command line, or a second copy of the program) still takes frames from a running collection in turn. Do not run those during a collection.

**Shutdown:**

1. `threading._shutdown` runs the hook registered with `threading._register_atexit`, before any `atexit` callback. It sets `shutting_down` and wakes waiters, with no HTTP and no logging. It may run repeatedly.
2. From then on every long wait returns soon: receive slices, idle-wait polls, `ensure_idle` waits, movie windows, trigger and initialize waits. Each gives `'shutdown'` (iterators) or `DectrisError('driver shutting down')`.
3. The flag cannot interrupt one HTTP request already out. That is at most 30 s by §4.1, or 120 s for the exit disarm with the FileWriter on (§4.2, PROTOCOL §3.3). Initialize does not count, because it runs on a worker. A FileWriter download, at most 120 s per file, can also be out, but only if a download thread is still running at exit.
4. `atexit` then runs in reverse order. `LiveVideoStream`'s `grabber.stop` joins promptly.
5. Then the final release, `release_connection()`. It is also what `close()` and `__exit__` call. It runs once and never raises:
   1. a guarded disarm, only if `dcu_reachable` and no initialize is in flight (§4.2);
   2. detached: close the socket (LINGER 0) inside its own `catch_unwind`, because `zmq::Socket::drop` panics if `zmq_close` fails; that panic is logged, not raised [R8];
   3. release the guard;
   4. INFO `Connection to camera '{name}' released`;
   5. only at interpreter exit: set `LOGGING_CLOSED`. An explicit `close()` stops at step 4 and leaves logging on [G1-C].

If `threading._register_atexit` is absent, step 1 is lost, and exit waits at most one receive window or HTTP budget.

The vendor calls `initialize` a blocking operation: "no other API operation may be performed until successful completion". So it is one long PUT on a worker, not a short PUT with polling, and the driver's own getters stay silent meanwhile (§4.1 [G1-H]). A caller's raw `session` calls cannot be held back by the driver, and whether a DCU tolerates them is **Open** (PROTOCOL §15 #42).

## 4. Protocol layer

### 4.1 SIMPLON REST client (`dectris_simplon::client`)

`ureq` with no TLS, plus `serde_json`. One agent: connect timeout 1 s, HTTP statuses are not errors at the transport level, a total timeout per request, at most 4 idle connections, headers `User-Agent` and `Accept: application/json`.

Wire conventions: config PUT sends `{"value": v}`; commands PUT without a body; GET answers `{"value": ...}`; `arm`, `disarm`, `abort` and `cancel` return the sequence id (§4.2); only HTTP 200 is success. A config PUT succeeds on HTTP 200 alone: the body, a JSON list of changed keys that may be empty or absent, is read only for a DEBUG log [G2-L].

Timeouts in seconds:

| budget | requests |
|---|---|
| connect 1 | every request except `trigger` |
| 1 | the four status getters, which a GUI of one's own may poll: `status/state`, `status/high_voltage/state`, stream `config/mode`, `countrate_correction_applied` [R10] |
| 3 | GET `trigger_mode`, `binning_mode`, `bit_depth_image`, `detector_readout_time`, `x/y_pixels_in_detector`, the startup probe |
| 5 | GET after a 405, FileWriter `status/files`, idle-wait polls (every 0.2 s) |
| 5 | PUT detector config keys, stream `mode/format/header_detail/header_appendix`, FileWriter `config/*`, `hv_enabled` |
| 3 | PUT Monitor `mode` |
| 30 | `arm`, `disarm` (with the FileWriter on, the download budget), FileWriter `clear` |
| 130 | `initialize`, on a worker, then an idle wait of 30 s (vendor: up to 2 minutes, Q15) |
| 10 | waiting out a transient `configure` or `initialize` state |
| 15 | `abort`, `cancel` |
| connect 5, then §3.4 | `trigger` |
| 2 | GET `images/monitor` |
| 120 | GET `/data/<file>` |

**Offline back-off [R10].** A transport error or timeout on any request opens a 5 s back-off; any HTTP response closes it. During it, the four public getters and the Monitor read answer with `'unknown'`, `'unknown'`, `'unknown'`, `None` or the cached preview, and do no I/O, so an unreachable DCU cannot make each poll from a GUI on the Tk thread cost a timeout. The Monitor still applies its rate-limit sleep so that the grabber does not spin; `None` becomes silent zeros [G1-J]. Back-off does not apply to internal GETs that decide policy (`ensure_idle`, idle wait, `disarm`'s state check), since a made-up `'unknown'` would drive `ensure_idle` into an initialize; nor to strict commands; nor to `cam.session`.

**Initialize in flight [G1-H].** While an initialize PUT is out, `Client::execute` refuses everything else with a `State{initialize}` error (a counter, not a lock). The public getters answer with no I/O: `get_detector_state()` gives `'initialize'`, the others their last good value or `'unknown'`/`None`, and the Monitor its cached frame or `None`. This keeps the timers of a GUI that polls silent during the vendor's blocking operation.

**Error policy.** Strict (raises): `arm`, `trigger`, the `set_config` core PUTs (§4.2), `trigger_mode` and `compression` errors other than 405, GETs after a 405, `configure_filewriter` errors other than 405, `hv_enabled`. `download_filewriter_files` logs each file's error and goes on. Everything else is best effort and logs.

### 4.2 State policy (`dectris_simplon::detector`)

**`get_detector_state()`**: one GET (1 s, back-off aware); failure gives DEBUG and `'unknown'`.

**Idle wait** (internal, 0.2 s poll): until `idle`, the timeout or `shutting_down`; WARNING on timeout; returns the last state.

**`ensure_idle()`** (internal; used by `arm` and `set_config`) returns whether it re-initialized [R16]:

- `idle`: no.
- `ready`, `acquire`: set cancel; from `acquire`, PUT `cancel`, and a failure is only logged; PUT `disarm`. If that fails, recover by initialize. Then read the state again and treat it by these same rules, up to depth 3 [G2-J].
- `initialize`: wait up to 130 s for `idle`, and **never** send a second initialize while one runs [G2-J]; then the steps after initialize; returns yes.
- `configure`: wait 10 s, then initialize if still not idle.
- `na`, `error`, `test`, `unknown`: initialize if `auto_initialize`, else `DetectorStateError`.

A failed initialize raises `DetectorStateError` with `needed: "idle (initialize failed)"`. All waits watch the flags.

**`disarm()`** sets `cancel_requested`, wakes waiters first [R4], and clears series ownership. It never raises. Then, by state:

- initialize in flight: wait at most 10 s, send nothing;
- `idle`: DEBUG, except with the FileWriter on, when it sends PUT `disarm` because that closes the FileWriter's files;
- `na`: DEBUG; `error`: WARNING;
- `initialize`, `configure`: wait at most 10 s;
- `ready`: PUT `disarm`;
- `acquire`: PUT `cancel` (15 s), then PUT `disarm`. A DCU may finish the running series before answering `disarm`: hours for 99999 images. §8 Tier 3 step 6 checks this.
- anything else, such as `unknown`: WARNING, nothing sent.

The PUT's budget is the download budget (120 s) with the FileWriter on, else 30 s. A failure is logged as an error, or as a WARNING for the FileWriter disarm from `idle`.

**`arm()`**, after draining late trigger outcomes:

1. `r = ensure_idle()`.
2. If `r` (an implicit initialize ran) [R16]: re-apply `last_applied` (core PUTs, trigger mode, compression, header appendix, then `enable_stream_v2()`), INFO `detector re-initialized; configuration re-applied`. If nothing was ever applied in this process: WARNING `detector re-initialized; DCU default configuration in effect`. Clear the ensure cache. Without this, a DCU restart between configuring and arming would arm with default settings.
3. Bump the epoch; clear the start cache, `cancel_requested`, `end_seen`, `own_start_seen` and the `TriggerBook`; armed id unknown, driver armed; record the arming thread.
4. PUT `arm` (30 s); failure logs ERROR and raises.
5. Parse the sequence id from the body: the key `"sequence_id"` or `"sequence id"` (with a space), whose value is a number or a nested `{"value": n}`; or a bare number as the whole body. If absent, DEBUG, and the id stays unknown. Store it as the armed id [R12].
6. For the time between the PUT and the parse: if a start was cached meanwhile and its `series_id` differs from the parsed id, clear the start cache and `own_start_seen`.

**`trigger()`**: §3.4. It does not read the state first (Q16).

**`initialize()`**: PUT (130 s) on a worker with no Python. The caller waits for only two things, the stored outcome or `shutting_down` [G2-A]. `cancel_requested` and `end_seen` do not release it: `cancel_requested` stays set from an earlier cancel until arm step 3, which comes after `ensure_idle`, so it would release the caller at once. Until the outcome is stored, the driver sends no other request. After Ok: idle wait 30 s, then `enable_stream_v2()` and Monitor enable, then clear the ensure cache. Initialize may reset both (**Open**, PROTOCOL §15 #32); re-enabling does no harm either way. `last_applied` stays, being what must be re-applied. After an error or timeout: ERROR, no idle wait, no re-enable. The worker does not count against the ceiling of §3.4: it has one slot of its own, and a second caller joins the first, so leftover trigger workers can never make initialize fail.

**`abort()`, `cancel()`**: set `cancel_requested` and wake waiters first [R4]. `abort`: skipped while an initialize is in flight and for `idle`, `na`, `error`, else PUT. `cancel`: PUT only from `acquire`, since RED calls it with nothing armed. Neither raises or re-initializes.

**`set_config(nimages, exposure, ntrigger=1, frame_time=None)`** first checks its arguments: exposure finite and above 0; `nimages` and `ntrigger` at least 1; `frame_time` at least 0.

0. Clear the ensure cache before the first PUT, whoever called [G1-D]. The DCU is about to hold a configuration the cache does not describe, and a failed call may leave it partial. Without this, a later `ensure_config` whose key equals an older cached one would skip, and arm the configuration another caller had set instead; if that was a 99999-frame series, `get_movie` and `_acquire_image` would then run a blocking trigger on it.
1. `ensure_idle()`; INFO summary.
2. If `frame_time` is None or 0 and the policy is `readout`: GET `detector_readout_time` (fallback 0.004 s with a WARNING), `frame_time = exposure + readout`. Policy `detector`: no `frame_time` PUT.
   - 2a. If the mode is inte or exte [G2-B]: when `nimages > 1` and `ntrigger == 1`, send `nimages=1, ntrigger=<nimages>` (INFO `enable mode: nimages→ntrigger`); otherwise send `nimages=1` and `ntrigger` as given. The detector ignores `nimages` in enable modes (SINGLA user manual §7.3.1). `last_applied` and the ensure key store the mapped values.
3. PUT **frame_time, count_time, frame_time**, then `nimages`, `ntrigger` (Q7). The first `frame_time` is best effort: on a refusal, WARNING `pre-set frame_time rejected (reject model); continuing` [G2-C]. The others are strict, so a failure can leave a partial configuration. The manual says the DCU rewrites the other value; a report says it refuses (**Open**, PROTOCOL §15 #31). This order works for both, including a change from a long to a short exposure, and the readback of step 7 shows which value took effect.
4. `trigger_mode` (on 405: GET and compare).
5. `compression`, only if `bslz4` or `lz4`; yaml `none` gives a WARNING and no PUT.
6. `header_appendix`, WARNING on failure. An object is tried first; on a refusal, once more as a string, and the form that worked is remembered [G2-M]. A string goes as it is. The value here is `''`.
7. GET back `count_time` and `frame_time`, WARNING if they differ by more than 1e-6.
8. On success, store in `last_applied` with the **effective** `frame_time` from step 2 or 7, never None, so the trigger budget and the receive window always have a number [G1-M].

**`ensure_config(...)`**: key `(nimages, ntrigger, exposure, frame_time or None, trigger_mode, compression)`, with mapped values. Equal key: DEBUG and skip. Otherwise `set_config` and `enable_stream_v2()`, cached only if both succeed. The cache is cleared by the `trigger_mode` and `compression` setters, `set_trigger_mode`, `set_compression`, `initialize`, `reconnect_zmq` and every `set_config` (step 0).

**`hv_enabled(state)`**: PUT `command/hv_enabled {"value": bool}`; raises, so a GUI can revert its checkbox.

**`gate(enable=None)`**: WARNING once `exte requires an electrical enable signal on EXT IN; gate() has no hardware effect`; no HTTP.

**Others.** `set_trigger_mode` validates `ints`, `inte`, `exts`, `exte` (`ValueError`), stores, clears the ensure cache, and does a best-effort PUT. `enable_stream_v2()` PUTs `mode`, `format`, and `header_detail` only if `stream_header_detail` is set; on 405, GET and compare; returns a bool and never raises. FileWriter PUT order: `mode, name_pattern, nimages_per_file, compression_enabled, image_nr_start`; 405 gives a WARNING and a skip, other errors raise.

**Series tail rule [R14]** (one procedure in `dectris_stream::receiver`), for paths that own their series, `_acquire_image` and `MovieIter`. SIMPLON says that the last acquired image "is available only after a disarm command has been issued". So:

1. When the window ends with frames still outstanding: a guarded disarm.
2. Wait one more window, taking images of the armed series until its end.
3. If frames are still missing: `DectrisTimeout`.

`acquire_single_frame` never disarms, because its caller owns that series; it raises `DectrisTimeout` directly.

**Single-frame paths.** Defaults for `_acquire_image`, `_movie`, `stream`, `acquire_single_frame` [G1-M]: `exposure=None` means `default_exposure`, and `binning=None` means `get_binning()`.

`_acquire_image` [R13]:

0. Series ownership check and setup lock (§3.2 R7) [G1-E].
1. `ensure_config(nimages=1, exposure)`, `arm()`, a blocking internal trigger (ints, inte).
2. Wait for one image of the armed series; stale starts and ends are skipped by attribution.
3. Series tail if needed.
4. On success, a detached drain of at most 50 ms reads at most one further message. If that is the armed series' end, it is consumed, so the end of a self-disarmed `nimages=1` series does not leak into the next call.
5. No disarm on success, since the detector goes back to idle by itself.

`acquire_single_frame`: an internal non-blocking trigger (§3.4 step 2), then one armed-series image or `DectrisTimeout`. With `single_frame_fresh: true`, queued armed images are first drained locally (counted in `stats.stale`), so the frame returned is at most about one period old. With `false`, in ints with a running series, it returns the **oldest** queued frame, which can be as many frame periods old as the queue is long [R24].

### 4.3 ZeroMQ (`dectris_stream::{zmq_link, receiver, attribution}`)

**Crate.** `zmq 0.10.0` from a pinned git revision of rust-zmq, which compiles libzmq 4.3.5 from source through `cc`. On MSVC libzmq needs `advapi32`, hence the link line in `build.rs`. A test asserts the version.

**Socket.** `socket(PULL)`; `set_rcvhwm(ZMQ_RCVHWM)` and, if not 0, `set_rcvbuf(zmq_rcvbuf)` before `connect`; `set_linger(0)`; default reconnect interval; TCP keepalive on with 60 s idle when `zmq_tcp_keepalive`. Receiving is `poll(POLLIN, slice)` followed by `recv_msg(0)`.

`zmq_rcvbuf` defaults to 4 MB. With the OS default, waits of about 30 ms inside Windows TCP made the sender drop frames; every value from 1 to 64 MB removed that. This was measured on the development machine with synthetic frames, over loopback; whether a DCU and its network show it is **Open**. A fixed `SO_RCVBUF` is believed to turn off Windows' own window tuning; 0 goes back to the OS.

**Series attribution [R12]** (`attribution.rs`; the pure function `classify`, tested by table). Each message's header is scanned in microseconds (`type`, `series_id`, `series_unique_id`, `image_id`) and classified by **equality, never by order**, because the sequence id resets on initialize (SINGLA user manual §7.3.1). Messages left from an earlier series must not be taken for the current one: fast_adt and autoCRED never flush.

| row | situation | start | image | end |
|---|---|---|---|---|
| 1 | armed id known, `series_id == armed` | cache once, `own_start_seen = true`, **pin `series_unique_id`** [G2-H]. A later start with the same id and a new unique id is a newer series, adopted as in row 3 | deliver if `own_start_seen` and the unique id matches; else stale as row 2 [G2-H] | if `own_start_seen` and the unique id matches: stop receiving, set `end_seen`, `_STREAM_END`; else stale as row 2 [G2-H] |
| 2 | armed id known, `≠ armed`, `own_start_seen` false | **stale**: not cached, `stats.stale`, one WARNING per series | **stale**: dropped (`stale_series_policy: drop`) or delivered with `count_time=None` (`deliver`) | **stale**: ignored; does not stop anything or set `end_seen`; `stats.stale` and one WARNING |
| 3 | armed id known, `≠ armed`, `own_start_seen` true | a newer series armed outside the driver, for example by a menu's raw command: **adopt** the id, pin again, WARNING `series armed outside the driver`. Only a start can cause adoption [G2-H] | before such a start: stale; after adoption: as row 1 | before such a start: stale; after adoption: as row 1 |
| 4 | armed id unknown, driver armed this epoch (arm body had no id) | first start of the epoch: adopt its id | before it: stale, because one connection keeps order; after it: deliver | before it: ignored; after it: end |
| 5 | no driver arm in this process | cache, deliver everything, one WARNING `series attribution unavailable` | deliver | end |

What the table gives:

- A stale end from series N, read during series N+1, does not end the next run. fast_adt would otherwise collect zero frames in its second run.
- A stale start is never cached, so stale images never match it.
- Images and ends of the armed id count only after the armed start [G2-H]. On one connection a series' start always precedes its images and end, so an armed-id image seen before the armed start must be old: since ids reset on initialize, unflushed messages of an earlier series can carry the same id as the next arm.
- Fallback: if armed-id images arrive and no armed start comes within one window, counted from the first such image, the receiver delivers them with `count_time=None` and a WARNING `armed start missing (dropped by DCU?)`, so that a start dropped on the DCU's side cannot empty a whole run (`DeliverStartMissing`).

**Attribution mode [G2-D]** (`series_attribution`):

- `'sequence_id'`: the table above.
- `'fifo'`: ignore the id in the arm body, so row 4 governs every epoch.
- `'off'`: row 5 governs.
- `stale_series_policy` affects only stale images. A stale start is never cached and a stale end is never final, in every mode except `'off'`.

**Self-check [G2-D].** The vendor's documents do not say that a start's `series_id` equals the `sequence_id` that `arm` returned. So the driver detects a mismatch and does not just drop every frame in silence: in `'sequence_id'` mode, if a window ends in an epoch that saw no armed-id message but at least one start classified stale after `arm()` returned, it logs ERROR `series_id never matched arm sequence_id (armed=X, seen=Y) — set series_attribution: fifo` and increments `stats.attribution_mismatch`. The vendor's documents leave this open; it is the first thing to check on a detector (§8 Tier 3 step 0).

**`flush_zmq_buffer()`** [R22]: the busy check (§3.2 R2), then `try_lock`, then `recv` without blocking until nothing is left and, once an image was drained, until the socket stays quiet for 50 ms. The images behind a small PULL queue arrive only as it empties, and left there they can keep the next series' start out of a full sender queue; whether a DCU's sender behaves so is **Open** (PROTOCOL §15 #40). Caps: 10000 messages and 2 s, so a flush during a running series takes the full 2 s. Each drained message gets the header scan.

- A start of the armed series not yet seen is cached and **not** counted. It is held in a local variable during the drain and written after the socket lock is dropped, so the two locks never nest [G1-I].
- Everything else is discarded and counted, including current-series images, which a caller's flush after a warm-up means to discard. Stale ones also count in `stats.stale`.
- It returns the count (INFO if above 0). It never clears the start cache (Q28).
- Armed-series images discarded also count in `stats.flushed_armed`. If the armed series' end has not been seen: WARNING `flush discarded N image(s) of the current series` [G2-I]. The last image may arrive only after a disarm, and a flush after a disarm can discard it, so the warning makes that loss visible.

**`reconnect_zmq() -> bool`** [R32]: the busy check, close, a new PULL with the same queue length, connect, INFO and the console line `[Dectris] ZMQ reconnected: fresh buffer`, `True`. Busy gives `False` (§3.2 R2). Otherwise it raises only on programming errors, so a GUI button can call it on the Tk thread with no `try`.

**Window (Q2).** `window_ms = clamp(2 × frame_time × 1000 + 1000, 2000, 60000)`. `frame_time` comes from the attributed start, else `last_applied`; with neither, the window is the floor, 2000 ms. It is worked out once, when a receive begins, so a `start` message that arrives later does not change it. `receive_timeout` overrides it, and `message_receiver(timeout)` uses its argument. These are deadlines, with 250 ms slices that check `shutting_down`. Other values in `StreamTuning`: 50 ms drain after a single frame. `recv_freshest` has the same caps as flush.

### 4.4 CBOR decoder (`dectris_codec::cbor`, written by hand) [R20][R37]

**Reader.** `Reader<'a>{buf, pos, depth}` handles major types 0 to 7, additional info 24 to 27, float16/32/64, 64-bit integers, indefinite-length arrays and maps, and byte and text strings. Strings are borrowed when of definite length and joined into a scratch `Vec` when chunked; each chunk must be definite and of the same major type. Simple values 20 to 23 are `False`, `True`, `None`, `None`, and additional info 28 to 30 is a `StreamDecodeError` [G2-P]. Scalar and header-scan parsers skip any tag outside the set {0, 40, 1040, 56500, 64 to 87} that precedes a scalar, as the vendor's parser does [G2-P].

A message must be exactly one top-level map. Trailing bytes are an error.

**Safety bounds.** Each `pos + len` uses checked arithmetic, and each read is bounds-checked (`StreamDecodeError{offset}`). Nesting is limited to 32 for both decode and `skip()`; `skip()` uses an explicit stack, so a depth bomb cannot overflow the native stack, which `catch_unwind` cannot recover. Other limits: 32 dimensions, 256 MiB for one decoded buffer, 512 MiB of tag 56500 output in one message. `codec` and `stream` forbid `unwrap`, `expect`, indexing and `panic` through clippy lints. The codec never logs: a `modifier_mismatch` comes back as data.

**Tags.**

- 0: ISO text.
- 40, 1040: `[dims, elements]` (checks in §2.8).
- 64 to 87: typed arrays (76, 83, 87 are errors).
- 56500: `[algorithm, modifier, bstr]`, wherever a byte string is expected, including inside a typed array (`40 → 70 → 56500`). The content must be exactly a 3-element array `[tstr, uint, bstr]` [G2-K]. The algorithm is compared byte for byte with `bslz4` and `lz4`, so `"LZ4"` and `"lz4 "` are refused. Anything else gives `StreamDecodeError('unsupported compression algorithm <x>')`.
- MultiDimArray elements given as a plain CBOR array decode in `decode_message` to an object `ndarray` reshaped to `dims`; on the image path they are a `StreamDecodeError` [G2-P].
- Unknown tags pass through. In Python they appear as `CborTag`, with `.tag` and `.value`, the shape `decompress_image` also accepts (§2.7) [G2-P].

**Messages.** A start message has its scalars parsed. At `header_detail=all` it carries the full-frame arrays `flatfield` and `pixel_mask`, about 4.4 MB each, and the smaller `countrate_correction_lookup_table`; all three are skipped by length unless `stream_decode_start_arrays`. The raw path scans `type, series_id, series_unique_id, image_id, real_time` at the top level and skips `data` (microseconds, no decompression; Q9).

**Why written by hand [R37].** It allows decoding into the numpy buffer without a copy, control over tag 56500 inside typed arrays, and validation with depth limits that cannot panic. `ciborium` is a test dependency that generates test maps.

### 4.5 bslz4 and lz4 (`dectris_codec::{framing, bitshuffle, lz4}`) [R15][R17][R18]

`lz4.rs` is the driver's own safe LZ4 block decoder. The reason is speed: `lz4_flex`'s safe decoder copies overlapping matches byte by byte, and bitshuffled frames are long runs of zeros (0.42 ms against 1.56 ms on one bitshuffled frame, measured on the development machine with synthetic frames). `lz4_flex` stays as the encoder and the reference in differential tests, with one swap point, `framing::lz4_exact`. Offset 0 is refused. Any decoder error maps to `Decode{offset}`, and so does any output length different from the expected block length [R15].

| check | bslz4 | lz4 |
|---|---|---|
| header | 12 bytes: u64 BE `orig_size`, u32 BE `block_size` in bytes | same |
| `orig_size` | `== rows × cols × itemsize` on the image path; at most 256 MiB | same |
| `block_size` | not 0 if `orig` is not 0; at most `i32::MAX` [G2-F]; `% (8 × elem) == 0` | not 0 if `orig` is not 0; at most `i32::MAX` [G2-F] |
| scratch | `min(block_size, orig_size)` bytes, twice; the `orig_size` cap limits the allocation [G2-F] | output only, sized by `orig_size` |
| element size | `elem = modifier` (the wire value), `1 ≤ elem ≤ 0x1FFF_FFFF`, checked **before** the header is parsed, so it applies even when `orig_size = 0` [G2-E] | modifier ignored (a placeholder 0) |
| blocks | `full = orig / block`; `leftover = orig % (8 × elem)`; `last = (orig % block) − leftover` | `full = orig / block`; `last = orig % block`, no rounding to 8 elements |
| per block | u32 BE length present and within what remains; LZ4 must give exactly the block length; then inverse bitshuffle of `block / elem` elements | a raw copy when `compressed_size` equals that block's own size, including the last partial block [R17]; else LZ4 to exactly that size |
| tail | `leftover` bytes raw, must equal what remains | none |
| trailing bytes after the last block or leftover | **refused** (`StreamDecodeError('trailing bytes')`) | **refused** |

**Modifier and itemsize [R18].** The decoder always decompresses with the wire modifier, which is correct by construction, since bitshuffle inverts with the encoder's element size. When it differs from the typed tag's itemsize, one WARNING per series; the size check stays strict. Tests assert that the modifier is used. §8 Tier 3 step 0 checks whether a DCU sends `modifier = itemsize`.

**Block-size cap [G2-F].** The cap on `block_size` is the vendor's, not lower, because the default block of the HDF5 LZ4 framing is 1 GiB, and the vendor's decoder accepts an lz4 stream with `block_size` of 1 GiB and `orig_size` of 3. A lower cap would refuse every frame from a DCU that writes that default. Allocation is bounded by `orig_size` alone.

**Trailing bytes.** The driver is deliberately stricter than the vendor's decoder, which checks the bslz4 tail only when leftover bytes exist and never for lz4. Tests assert this difference. If a DCU pads its frames, this has to relax to a WARNING (**Open**).

**Bitshuffle.** Safe Rust: a lane-parallel kernel for the main body, and the plain per-group form of `PROTOCOL §9.5` for tails and odd element sizes. No AVX2: use it only if a need is measured on real data. `compression='none'` is a raw copy with the same length check.

### 4.6 Decoding saved messages

`receive_raw_frames` gives each Stream V2 message as its raw bytes, which a caller can write to a file and decode later. The module functions `decode_message(raw)` and `decompress_image(message)` do that without a camera and without a connection. A corrupt message gives a `StreamDecodeError`, which the caller can count and skip [R15][R20]. It cannot panic or overflow the stack.

**Do not construct a second camera to decode.** A second `CameraDectris` in the process gets no socket from the single-PULL guard (§3.7), so it takes no frames. But it runs the connect checks of §2.2, may reload the config if the names differ, and **would disarm a running series**, because the probe reads `ready` or `acquire`. Use the module functions instead.

## 5. Behaviours worth knowing

Each row is what the driver does, and why. Source comments cite these numbers.

| # | behaviour | reason, and who could notice |
|---|---|---|
| Q1 | `get_image` and `acquire_single_frame` raise `DectrisTimeout` (also a `TimeoutError`) on timeout or end, and never return `None` | The controller would fail on `None` at `arr.shape`. A typed error reaches the caller through §3.6 |
| Q2 | Receive window `clamp(2·ft + 1 s, 2 s, 60 s)`, or `receive_timeout`, as a deadline | A fixed short window breaks at long frame times. The same as 2 s for frame times up to 0.5 s |
| Q3 | `message_receiver` returns `None` as soon as the armed start is cached | A caller such as a cRED that drives the detector directly calls it only to consume the start, and throws away the value. No 5 s stall after arm |
| Q4 | `binsize=` is accepted as `binning` | The program always calls with `binsize`. Matters only above 1, and the yaml allows only 1 |
| Q5 | `get_movie` returns a `MovieIter` with `close()`, applies the series tail, raises on a short movie | The wrapper and the controller iterate; a silent short movie would spin the wrapper |
| Q6 | `header_appendix` PUT has a 5 s timeout, WARNING on failure | Without one it could hang a caller's unguarded `set_config` |
| Q7 | PUT order frame_time, count_time, frame_time with a readback; the first is best effort [G2-C]; omitted frame time = exposure + readout (`frame_time_policy`) | So that a frame time from an earlier run cannot stay in force. autoCRED and FAST-ADT do not pass a frame time, so this sets their period |
| Q8 | `ensure_config` skips when the key equals the last one it applied; cleared on initialize, reconnect and every `set_config` [G1-D] | An `ensure_config` after another caller's `set_config`, such as a GUI's Apply, applies again and does not arm the wrong settings |
| Q9 | The raw path classifies by header scan, with no decompression | Speed; the bytes handed out are identical |
| Q10 | `arm` and `set_config` may initialize, bounded (130 + 30 s on a worker), re-enable stream and Monitor, re-apply the last settings before arming [R16]; `auto_initialize: false` raises instead | A DCU restart between configuring and arming does not arm with defaults |
| Q11 | The default name is `'dectris-simplon'` | The zero-argument path loads the right yaml |
| Q12 | `gate` and `hv_enabled` exist (§4.2) | exte flows reach `arm()`; a GUI's high-voltage box can work |
| Q13 | Frames are owned arrays; a start is logged as a scalar summary at DEBUG; the User-Agent carries the program's version | No aliasing; no huge log lines |
| Q14 | ZeroMQ is closed with LINGER 0 in `release_connection`; `[Dectris]` console lines are printed | Exit cannot hang on the socket; the GUI console shows connection state |
| Q15 | Initialize has 130 s plus a 30 s wait | Vendor: up to 2 minutes. A cold DCU does not fail the first arm |
| Q16 | `trigger` does not read the state first; `disarm` from `unknown` sends nothing; `convert_uint16` has no effect | One round trip less per inte frame. A GUI's `u16` buttons would do nothing. TIFF files are uint32 and MRC files uint16 |
| Q17 | `series_date` and `arm_date` come back as ISO `str`, not `datetime` | Nothing reads them |
| Q18 | `trigger()` can be released from the client side; bounded PUT; at most 1 in flight per epoch; ceiling 8 | A caller that joins its trigger thread with a time limit does not time out after a cancel |
| Q19 | An inte movie uses `nimages=1, ntrigger=n` and n serialised triggers | It yields n frames |
| Q20 | Monitor read at most 10 Hz (`monitor_max_hz`); cached frame on HTTP 408; offline back-off | Vendor advice; smooth live view; no 2 s stalls when offline |
| Q21 | Attribution by the arm `sequence_id`: stale images dropped by default, stale end ignored, stale start never cached; unknown id delivers with a WARNING [R12] | fast_adt and autoCRED, which do not flush, do not see the previous series |
| Q22 | `trigger()` in exts or exte raises `DetectorStateError`, no HTTP [R23] | The vendor forbids it. A GUI button can show the error, for example as `Trigger failed: ...` |
| Q23 | A trigger after cancel or disarm is suppressed until the next `arm()`, INFO [R4] | An inte trigger that a caller sends after its Stop does not expose a frame |
| Q24 | One trigger PUT per armed series in ints for internal callers; public ints `trigger()` is skipped with a WARNING once `issued >= ntrigger` [R3] | A caller of `acquire_single_frame` per frame, in one long series, sends 1 PUT and not one per frame |
| Q25 | `reconnect_zmq` returns a bool; busy gives `False` and a message [R32] | It runs on the Tk thread during possible collections |
| Q26 | Series tail: disarm and one more window for owned series [R14] | The vendor may hold the last image until a disarm |
| Q27 | At most a 50 ms drain and attribution take care of the end of a self-disarmed `get_image` series [R13] | Consecutive `get_image` calls all succeed |
| Q28 | Flush keeps the armed series' start. `arm()` and `_stream_start_config = None` clear it [R22] | A flush between `arm()` and the first frame keeps `count_time` |
| Q29 | In inte and exte, `set_config` maps `nimages=n, ntrigger=1` to `nimages=1, ntrigger=n` [G2-B] | The vendor ignores `nimages` there. A caller that calls `trigger()` once in inte still gets one frame: one trigger exposes one frame |
| Q30 | `get_image` or `get_movie` during another thread's running series raises `DectrisBusy`, no HTTP [G1-E] | A click on acquire during a collection gives an error and does not end the collection |

## 6. Error model

`DriverError` in `dectris_common::error` is the only error type of the crates. `py/src/errors.rs` maps it:

| `DriverError` | Python |
|---|---|
| `Http{status, url, body}` | `DectrisHTTPError` (`.status_code`, `.url`) |
| `Transport(String)` | `DectrisConnectionError` |
| `Timeout(String)` | `DectrisTimeout` |
| `State{state, needed}` | `DetectorStateError` |
| `Busy(String)` | `DectrisBusy` |
| `Decode{offset, msg}` | `StreamDecodeError` |
| `InvalidArg(String)` | `ValueError` |
| `Zmq`, `NotConnected`, `Shutdown`, `Internal(String)` | `DectrisError` |

**Exception classes [R30].** They are built with builtins `type(name, bases, {'__module__': 'instamatic.camera.super_dectris._super_dectris', '__qualname__': name, ...})` at module init, because `create_exception!` allows one base only, and they are kept in a `PyOnceLock`. Without `__module__` a class would report `_frozen_importlib.DectrisError`. `DectrisConnectionError` and `DectrisTimeout` derive from `OSError` classes, so they are constructed with exactly one message argument, and OSError's errno unpacking never runs. All derive from `Exception`, so each `except Exception` catches them.

**Panics.** `panic = "unwind"`. Each exported body runs inside `catch_unwind` and gives `DectrisError("internal error: ...")`, never a `PanicException`. Workers store `Internal`. `release_connection` drops the socket inside its own `catch_unwind` and logs, so "never raises" holds [R8]. There is no `unwrap`, `expect` or indexing on wire data, and recursion is depth-limited (§4.4), because a stack overflow aborts the process and cannot be caught.

**Timeouts.**

- `_acquire_image`, `acquire_single_frame`, `MovieIter`: `DectrisTimeout`.
- `receive_*`: StopIteration with `last_stop_reason='timeout'`.
- `message_receiver`: `None`.
- HTTP timeouts raise from strict methods and are logged from best-effort ones (§4.1).

## 7. Build and packaging

**Workspace [R28].** `rust/Cargo.toml` is a workspace of the five crates and `py`. `rust/` has no `__init__.py`, so no packager treats it as Python. Profiles: `release` with `opt-level = 3`, fat LTO, one codegen unit, `panic = "unwind"`, debug info level 1; `test-opt` inherits release without LTO, for the perf tests of §9. `rust-toolchain.toml` pins Rust 1.91.1 with clippy and rustfmt [R41], so rustup fetches the right version. `clippy.toml` forbids bare `log::*` macros [R31]. `.cargo/config.toml` also puts the target folder outside the package [R36].

Main dependencies: `ureq` 3.4 without TLS, `serde_json`, `zmq` at a pinned git revision (§4.3), `lz4_flex`, `tiff` (with deflate and LZW), `thiserror`, `log`; in `py/`: `pyo3 0.29.3`, `numpy 0.29.0`, `pyo3-log 0.13.4`, which build a `.pyd` for Python 3.12. `Cargo.lock` holds exact versions.

**You need:** Windows x64, Python 3.12, rustup, the Visual Studio C++ build tools (libzmq is compiled as C++), and internet on the first build (cargo downloads crates and the rust-zmq git dependency). Any Python 3.12 environment works: conda, a python.org install, or a plain venv such as one made by `uv`. `instamatic` must be installed in it editable: `pip install -e .`, or `uv pip install -e .`.

**Keep the path short.** `build.ps1` stops with a message when the repository's root path is too long, because MSVC's linker fails when that path is beyond about 150 characters. Put the repository under a short path such as `C:\src`.

**Build [R26][R27][R41].**

```
powershell -ExecutionPolicy Bypass -File src\instamatic\camera\super_dectris\rust\build.ps1 -Python C:\path\to\python.exe
```

The script finds the repository from its own place, `..\..\..\..\..` from `rust\`.

- `-Python <path>`: the Python to build against and to test the import with. It never uses `python` on PATH, since that may be the Windows Store stub. Default: the active conda environment; with none, the script stops. Pass it explicitly.
- `-Quick`: skip the extras after the build (below). The copy and the import check always run.
- `-NoChecks`: skip the checks before the build.

Steps:

1. Without `-NoChecks`: `cargo test --workspace`; `cargo test -p dectris_codec --profile test-opt --test perf`, which asserts the bounds of §9 (the perf tests assert them only in an optimised build); `cargo clippy --workspace --all-targets -- -D warnings`; `cargo deny` if installed, else a warning.
2. `cargo build --release -p super_dectris_py`.
3. Copy `_super_dectris.dll` to `super_dectris\_super_dectris.cp312-win_amd64.pyd`.
4. Import check: `python -I -c` imports the module, asserts that its parent folder is named `super_dectris`, and prints the version. `-I` leaves the current folder out of the import path, so `instamatic` must be installed in that Python, and only an editable install (`pip install -e .` or `uv pip install -e .`) is supported. The check uses whichever copy that Python imports, so with several copies, look at where it came from.
5. Without `-Quick`: `cargo about` if installed, else a warning; then `build\_build_info.json` (the driver's version, which is the Cargo workspace version; rustc version; `git_rev` and `git_dirty`, which describe the repository the driver sits in; SHA-256 of the `.pyd`; `dumpbin /DEPENDENTS`) [R29], and the PDB copied to `build\` for crash analysis.

The build and `-Quick` need no git, and the source need not be in a repository. Step 5 calls the `git` command for its two fields. Outside a repository git prints an error and the fields stay empty, and the script goes on. On a machine with **no git installed** the full form stops with an error at that point, after the `.pyd` is already in place. Use `-Quick` there.

What the script sets, and why:

- `PYO3_PYTHON`: the same value for each cargo call, so pyo3 does not rebuild for each [R27].
- `CARGO_TARGET_DIR = <repo>\build\cargo-target`: gigabytes of build output must stay out of the importable package, which packagers sweep [R36].
- `RUSTFLAGS` with `--remap-path-prefix` and `/PDBALTPATH:%_PDB%`, and `CFLAGS`/`CXXFLAGS` with `/d1trimfile` when the profile path has no space: no build machine's folder names stay in the binary [R41]. A cargo call with different `RUSTFLAGS`, such as from a plain shell, rebuilds everything in that target folder. To run the tests alone, use `rust\test.ps1`: it runs `cargo test --workspace` with the same `PYO3_PYTHON`, `CARGO_TARGET_DIR` and `RUSTFLAGS` as `build.ps1`, so it reuses the build.

A wheel-building tool is not used: cargo, a copy and an import check have no wheel layout to get wrong, and nothing can be pip-installed under a shadowing name [R26].

**Runtime DLLs [R29].** pyo3 plus statically linked C++ libzmq adds load-time imports of `MSVCP140.dll` and `VCRUNTIME140_1.dll`. A conda environment provides them; otherwise install the Microsoft Visual C++ Redistributable (x64).

**Optional dependency tools [R38].** `deny.toml`, `about.toml`, `about.hbs` and `notices_addendum.md` belong to `cargo-deny` and `cargo-about`, which check the third-party dependencies and generate `THIRD_PARTY_NOTICES.md`, a list of the third-party components built into the extension. Each is skipped with a warning if not installed.

**EXE.** A program bundled into an executable (PyInstaller, Nuitka) must list the `.pyd` as a binary and the hidden imports `instamatic.camera.super_dectris` and `instamatic.camera.super_dectris._super_dectris`. The extension's own runtime imports are invisible to a bundler, so if an EXE fails on import, look there first: `numpy._core.multiarray`, `pathlib`, `logging`, `json`.

## 8. Test strategy

Rust and Python tests use fakes, so they run without a DCU. A label in the regressions list below can be searched for in `rust/` and `tests/super_dectris/`.

**Tier 1: `cargo test --workspace`** (no Python needed) [R28]. `rust\test.ps1` runs it with the environment of `build.ps1` (§7), so it does not rebuild everything:

- **CBOR:** RFC 8949 Appendix A; generated maps; truncation at every offset gives a typed error, never a panic; indefinite lengths and chunked strings; a depth bomb [R20]; the typed-tag table including errors and float16 [R21]; `40→70` and `40→70→56500`; tag 1040 [R19].
- **Framing and bitshuffle:** the vendor's test vectors; vectors from the vendor's encoders, kept with checksums; property tests forward and back; the table of §4.5, including lz4's last partial raw block and lz4 with a non-zero modifier [R17]; trailing-junk vectors (we refuse, the vendor's decoder accepts: asserted); wire modifier used when it differs from itemsize [R18]; corruption of the LZ4 payload itself [R15]; dimension checks before allocation [R19].
- **Optional inputs.** The tests that need the vendor's packages read `DECTRIS_TEST_PYTHON`, which names a Python with `dectris-compression` 0.3.1 and `hdf5plugin`. Without it they print SKIP and still report ok. `DECTRIS_MONITOR_SAMPLE` names a Monitor TIFF saved from a DCU for one more test, which is skipped without it.
- **Attribution:** every row of §4.3 [R12]; the window table.
- **Driver end to end** (`driver_e2e.rs`): a fake SIMPLON (`dectris_simplon::testing`, feature `test-util`) and a PUSH server (`dectris_stream::PushServer`): start, image and end with series ids, delays, no end, last image only after a disarm.
- **Timing** (`cargo test -p dectris_codec --profile test-opt --test perf`, §7 step 1): the bounds of §9. They are asserted only in an optimised build.

**Tier 2: `python -m pytest tests/super_dectris`**, against the built `.pyd`. `conftest.py` starts with `pytest.importorskip` on the extension. The tests need `pytest`, `pyzmq`, `cbor2<6`, `h5py`, `hdf5plugin`, `tifffile` and `dectris-compression`, whose source is included in this repository under `third_party/`; the README lists them too. Without `dectris-compression` the module-function tests skip. `fake_dcu.FakeDCU` is a scriptable SIMPLON state machine and PUSH socket written from `PROTOCOL.md`. It can hold a `trigger` PUT until a cancel, inject errors, and answer `arm` with either spelling or none. The test files include `test_camera_class.py`, `test_concurrency.py`, `test_grabber.py`, `test_module_functions.py`, `test_offline.py` and `test_receive.py`. `test_review.py` holds the regressions labelled `review #n`: series ownership across threads, the video grabber, Ctrl+C and initialize, stop reasons. Fixtures snapshot and restore `config.camera`, since a by-name construction changes the process-wide config [G1-K].

Regressions that the tiers check. Not every one is a separate test:

- **[R1][G1-A]** With a DEBUG handler on the root logger, 100 HTTP calls and 10 non-blocking triggers: no record from `ureq` or `zmq`, and the attach counter (`_test_attach_counts`) is 0 for each target outside `instamatic`, on all threads.
- **[R2]** One thread holds each lock via `_test_hold_lock(name, 2.0, log=True)` while another calls `arm()`, the getters, `set_trigger_mode` and a setter: all finish in the hold plus 1 s, no deadlock.
- **[R3]** `nimages=99999`, `arm()`, 200 × `acquire_single_frame()`: exactly one trigger PUT. inte: two overlapping triggers are never in flight together.
- **[R4]** `cancel()`, then `trigger(count_time=0.1)` with no arm: no PUT, returns in under 50 ms. **Q18:** `cancel` frees a blocked `trigger()` in under 0.5 s, and the next `arm()` and `trigger()` work.
- **[R5]** A `FrameIter` on thread A with 20 flushes from thread B: no frame lost, each flush returns 0 with a WARNING. A flush from A after `break` drains normally. `reconnect_zmq()` while busy returns `False`.
- **[R6]** Interpreter exit with a movie waiting on a silent stream and a hanging initialize completes within 2 s plus one HTTP budget.
- **[R7]** The wrapper's `get_movie` closed early: exactly one disarm. **[R8]** A raising logging handler: no `SystemError`, `stats.log_errors` grows.
- **[R12]** A stale end before a new start is not final; a stale start and images before a new start: images dropped, start not cached; two fast_adt-style runs with no flush each collect n frames; external re-arm adoption; arm body with no id. **[R13]** 10 consecutive `get_image()` calls succeed. **[R14]** The last image only after a disarm: `get_movie(5)` and `get_image()` succeed.
- **[R16]** `na` during an ensure-cache hit: initialize, config PUTs, then arm. **[R22]** A flush between `arm()` and the first frame keeps `count_time`. **[R23]** exts `trigger()` raises with no HTTP. **[R24]** `single_frame_fresh` returns the newest frame.
- **[R30]** Exception `__module__` and MRO. **[R33]** A yaml with bad types constructs with WARNINGs. **[R34]** A grabber exception and a stale movie item: a later `get_image` gets neither; the exception reaches the caller and the grabber survives. **[R39]** The class `__dict__` holds each public member.
- **[G2-A]** `cancel()`, then `initialize()` against a held PUT: no other request until it completes. **[G2-B]** inte and exte `set_config(nimages=5)` PUT `nimages=1, ntrigger=5`. **[G2-C]** A DCU refusing the first frame time: settings still applied, no exception. **[G2-D]** A start's `series_id` = arm id + 1: the self-check fires once; `fifo` gives n frames. **[G2-H]** Stale image and end with the same id as the new arm are neither delivered nor final. **[G2-E][G2-F]** The element-size overflow vector must fail; lz4 with `block_size` of 1 GiB and `orig_size` of 3 must decode. **[G2-K]** `"LZ4"`, `"lz4 "` and a 2-element 56500 array must fail. **[G2-P]** Simple values, skipped tags, plain-array elements, `CborTag`.
- **[G1-B]** 10000 races of a blocking `trigger()` against `cancel()`, with random jitter: each returns in under 0.5 s. **[G1-C]** Closing one instance leaves the other logging. **[G1-D]** `ensure_config(A)`, `set_config(B)`, `ensure_config(A)`: A is re-applied. **[G1-E]** A cRED-style receiver on thread A; `get_image()` and `get_movie(3)` on B give `DectrisBusy`, no request, A unaffected. **[G1-F]** `get_movie(5)` with an end after 2 images: `DectrisTimeout`; through the wrapper the caller gets it, and the grabber survives. **[G1-H]** Held initialize: polled getters make no request, the state reads `'initialize'`. **[G1-J]** Offline: 50 previews give zeros, one WARNING, and the rate limit holds. **[G1-K]** Zero-argument construction with a live Dectris config does not reload. **[G1-Q]** `pickle.dumps` of a `get_attrs`-style dict works. **[G1-R]** The warning when no start comes (§10). **[G1-S]** `MediaGrabber.stop()` during a held initialize returns in under 1 s.
- **[#12 keep]** Each member of §2.3's list exists and follows its never-raise rule.
- **Soak:** frames with GUI-style polls plus a CPU-bound Python thread [R9]; some are `slow` and run only with `SUPER_DECTRIS_SLOW=1`.

**Tier 3: on a detector.** The checks, in order:

| step | content |
|---|---|
| 0 | Capture real messages with `receive_raw_frames`, at `header_detail` basic and all. **Confirm `series_id == arm sequence_id`** [R12], else set `series_attribution: fifo` [G2-D]. Tag 56500 modifier = itemsize [R18]. No trailing bytes [R17]. Are strings chunked? |
| 1 | ints `trigger` PUT: does it block? Repeated triggers. What does a pending PUT do after cancel or disarm (this sets the budget of §3.4) [R3]. Cancel gives `end`? Initialize from `na`. Does the DCU tolerate raw `session` polls during an initialize (§3.7)? |
| 2 | inte `count_time`. |
| 3 | Compression and bit depth, each combination. |
| 4 | Queue length, flush, stale policy. Does a flush after a disarm discard a final image (`stats.flushed_armed`) [G2-I]? Does the sender need `zmq_rcvbuf`? |
| 5 | Monitor and FileWriter. Preview orientation against a calibrated `get_image` [G1-U]; rotation axis against a dataset of a known sample (`camera_rotation_vs_stage_xy`). |
| 6 | Movie timing: the frame period of `get_movie` against the requested 0.900 s at frame time 0.9; the time per `get_image` call in cRED. Does `disarm` from `acquire` need `cancel` first (§4.2)? |
| 7 | FAST-ADT continuous scans: the first frame against the start of the rotation, and the frame count. |

## 9. Performance bounds

The tests assert these. The time bounds are asserted by the perf tests, `cargo test -p dectris_codec --profile test-opt --test perf` (§7 step 1), and only in an optimised build. Comments that say `§9 bound`, or `§9` with a phase label such as `§9 Phase 1`, mean this list. The figures are measured on the development machine with synthetic frames; other frames may differ.

| what | bound | measured |
|---|---|---|
| decode and decompress one 1028 × 1062 uint32 bslz4 frame | at most 3 ms | about 1.2 ms into a reused buffer, 1.8 ms into a fresh one |
| raw classify of a 4.4 MB message (Q9) | under 50 µs | about 0.5 µs |
| skip the arrays of a 4.4 MB start message | under 1 ms | about 1.1 µs |
| property tests | 10000 cases, forward and back | |

## 10. Open points, and choices that may change on a detector

- **Geometry.** Frame shape and dtype always come from the stream. The yaml `dimensions` stays the source of `get_camera_dimensions()` and `get_image_dimensions()`. The constructor and the first start message warn on a mismatch. Tests cover 512², 1028×1062 and 1028×514. Edit the yaml if the capture of Tier 3 step 0 disagrees.
- **`series_id` and `sequence_id`** (§4.3 self-check): if they differ on a DCU, set `series_attribution: fifo`.
- **`gate`** has no hardware effect and warns; **`hv_enabled`** is the SIMPLON command, which the vendor says exists only on some detector types, and raises if refused.
- **Preview orientation.** The Monitor preview is turned `fliplr → flipud → rot90(k=-1)`, a net `rot90(k=+1)`. The driver's stream and `get_image` frames stay in the detector's layout. The GUI's `save_frame` writes the 640-pixel preview. The pixels and their orientation have not been compared with a dataset of a known sample, so do that first [G1-U].
- **ZMQ endpoint errors.** After `arm()`, if no armed start arrives within one window, the first receive path logs one WARNING `no Stream V2 start within <w> s of arm — check ZMQ_host/ZMQ_port (<ZMQ_stream>) and stream mode`, and `stats.timeouts` counts it. It does not raise, so waits in exts and exte are not affected [G1-R].
- **Second PULL in another process** (§3.7) cannot be detected by the driver.
- **Trigger.** `trigger_blocking` is True, so a caller of its own that triggers by hand before it starts a rotation must call `trigger()` from a thread. Instamatic's experiments never call `trigger()` (§11.4). `single_frame_fresh` is True. Revisit the budget of §3.4 if pending PUTs never complete after a cancel on a DCU; the design works either way.
- **Tag 51192 of the Monitor TIFF** (`dectris_simplon::monitor`): text or a private DECTRIS IFD of strings; which form a DCU writes is **Open** (a saved Monitor TIFF can be tested with `DECTRIS_MONITOR_SAMPLE`, §8 Tier 1). It is parsed and not exposed to Python, so a failure can cost at most that metadata.
- **Queue length.** 500 by default. Set 4 only for a caller that pairs a frame with the stage tilt when the frame arrives (§2.2). A caller that places each frame by its `image_id` can keep the larger queue.

## 11. What Instamatic expects of the driver

This section is about Instamatic's own callers at `ded7d4b` with `instamatic-2.2.2.patch`: `controller.py` (`TEMController`), `camera/videostream.py`, the experiments and `gui/`. None of them knows the driver by name. They use the members every camera has, and only the live view looks for `get_preview_image` (§3.6). The other members of §2.3, such as `arm`, `trigger`, `receive_frames` and the FileWriter calls, are for scripts and for code of one's own.

**Aliases.** A comment that calls a name an alias means a second public name for the same member, kept for code that uses that name.

### 11.1 Rules every path relies on

- Exceptions derive from `Exception`.
- Construction succeeds with the DCU off. `get_preview_image` never raises, returns in bounded time, and does not disturb a running series.
- `get_image(exposure=, binsize=)` returns a 2-D `np.ndarray` or raises; it never returns `None` (Q1).
- `get_movie(n_frames=, exposure=, binsize=)` returns an iterator that arms on its first `next`, yields one array per frame, raises on a short movie, and disarms on `close()` (Q5). The controller calls `next` on its single-worker executor and reads the microscope while it waits.
- Frames are real `np.ndarray` objects that no later frame changes, so a caller may keep them without copying.
- `cancel()` and `disarm()` are safe when nothing is armed, and are thread-safe. `trigger()` returns after `cancel()` or `disarm()`.
- `get_binning()`, `get_camera_dimensions()`, `name`, `streamable`, `default_exposure`, `default_binsize`, `dimensions` and `dynamic_range` exist on the raw camera, since the controller, the wrapper and the GUI read them.
- `camera/camera.py` selects the driver by the interface name `dectris_simplon`, so that name must not change.

### 11.2 How the camera is reached

`camera.get_camera` loads the camera config, then calls `cam_cls(name=name)` and `VideoStream.from_any(cam)`. As `streamable` is True, the program's `ctrl.cam` is a `LiveVideoStream` wrapping the raw camera, and it forwards unknown attributes to it. The GUI's video frame uses the raw camera, `self.stream.cam`.

The wrapper shadows names: `cam`, `lock`, `name`, `dimensions`, `default_exposure`, `default_binsize`, `frame`, `frametime`, `grabber`, `requested`, `request_lock`, `close`, `block`, `unblock`, `blocked`, `unblocked`, `get_image`, `get_movie`. A write through `ctrl.cam` lands on the wrapper and not the driver, so set driver options on `ctrl.cam.cam`. `get_image` and `get_movie` through `ctrl.cam` run on the grabber thread (§3.6).

`CamServer` (off by default, `use_cam_server: False`) needs `dir()` followed by `getattr` to be safe, and its results to be picklable; its client reads the class's own `__dict__` [R39]. `instamatic.autoconfig` may construct the camera with no arguments, `cls()`.

### 11.3 Yaml keys

Needed, with no default: `DCU_host`, `DCU_port`, `ZMQ_host`, `ZMQ_port` (31001 for Stream V2), `dimensions`, `default_exposure`, `default_binsize`. `dynamic_range` is read with no guard in `videostream_frame.__init__`. `physical_pixelsize`, `stretch_amplitude`, `stretch_azimuth` and `camera_rotation_vs_stage_xy` are read by the experiments from `config.camera`. Optional: `trigger_mode`, `compression`, `stream_mode`, `stream_format`, and all of §2.2. `dectris-simplon.yaml` in this repository has the needed keys; the optional ones it leaves out take their defaults.

**A new key must not contain the substring `name`**, or `config_updater` treats the file as old-style and rewrites it. It must also have a code default, because a deployed copy of the yaml is never synchronised. `instamatic -l config` shows the config folder in use.

### 11.4 Experiments

**cRED** (`experiments/cred/experiment.py`): inside `ctrl.cam.block()`, one `ctrl.get_image(exposure)` per frame while the stage rotates. Each call is a series of its own (§4.2 `_acquire_image`): a cached `ensure_config`, `arm`, a trigger, one frame. The frame rate is therefore bounded by these round trips, not by the detector, and the receive queue holds at most one series' messages. Consecutive calls all succeed (Q27, [R13]).

**FAST-ADT** (`experiments/fast_adt/experiment.py`):

- Continuous scans: `ctrl.get_movie(n_frames=len(run) - 1, exposure=run.exposure)`, then the rotation is started with `stage.set(a=..., wait=False)`, then `zip(run.steps, movie)`. The movie arms on its first `next`, so after the rotation has started. One series of `n_frames` frames with one internal trigger. The frame period follows `frame_time_policy` (§12 #4). Frames are paired with steps in order, so a lost frame shifts the ones after it.
- Stills and tracking: `ctrl.get_image` per step, as cRED.
- `ctrl.cam.dead_time`: the driver has no such attribute, so FAST-ADT falls back to its calibrated estimate, or 0.

**autoCRED, RED, serialED:** `ctrl.get_image` per image; autoCRED and RED inside `ctrl.cam.block()` and `unblock()`.

No experiment calls `arm`, `trigger`, `receive_*`, `flush_zmq_buffer` or the FileWriter. A caller of its own that uses them, for instance a long ints series read with `receive_frames_with_metadata`, should read §3.4, §4.3 and the queue length in §2.2.

### 11.5 Live view and GUI

**`LiveVideoStream` and `MediaGrabber`:** §3.6. The grabber calls `get_preview_image()` at most once per frame time (0.05 s by default, so the driver's own limit of 10 per second applies, §3.5), and waits while blocked. The live view therefore never arms the detector.

**`gui/videostream_frame.py`:** `dynamic_range` is read with no guard in `__init__`. "Save frame" writes the frame shown, which is the 640-pixel preview, turned as in §10. "Save image" writes the displayed PNG.

Instamatic has no GUI of its own for the detector's state, high voltage, initialize or the FileWriter. Use the methods of §2.3 from a script or the console, on `ctrl.cam.cam`.

### 11.6 Members with no caller, and defaults

Instamatic calls only the members of §11.1 and `get_preview_image`. The rest of §2.3 has no caller in Instamatic, and neither have the module names `TYPED_ARRAY_DTYPES`, `autoscale`, `bin_ndarray`, `decompress`, `logger` and `tag_decoders`, re-exported by `super_dectris/__init__.py`. The parsed Monitor metadata (`_last_monitor_metadata`) stays inside the Monitor code, with no reader. Any public method is reachable through `CamServer` whether or not the program calls it.

`configure_filewriter`'s defaults are `mode='enabled'`, `name_pattern='series_$id'`, `nimages_per_file=1000`, `compression_enabled=True`, `image_nr_start=1`. `$id` expands to the series id. Each key is one PUT with a 5 s timeout. Call it before `arm()`.

## 12. Decisions cited in comments

Source comments cite these as `DESIGN §12 #n`. The numbers stay fixed for those citations, so some numbers are missing.

- **#1 libzmq.** libzmq 4.3.5 under MPL-2.0, built from a pinned rust-zmq git revision, was chosen over libzmq 4.3.4 under LGPL-3.0 and over the pure-Rust `zeromq` crate. A test asserts the version (§7).
- **#4 `frame_time` policy.** When a caller gives no `frame_time`, the driver sends exposure + readout time (`frame_time_policy: readout`). This changes the frame period of autoCRED and FAST-ADT (§2.2).
- **#6 Three trigger changes.** They were accepted with no `force=True` way around them: `trigger()` raises in exts and exte; after a cancel or disarm, triggers are suppressed until the next `arm()`; in ints a trigger is sent once per series (§3.4).
- **#7 Implicit initialize.** If `arm()` finds that an initialize ran, it re-applies the last configuration before arming instead of raising (§4.2 step 2).
- **#8 Defaults.** `single_frame_fresh` is True by default. With False, `acquire_single_frame` returns the oldest queued frame of a running ints series. Measured on the development machine with synthetic frames at 100 fps with 0.05 s between calls, False moves one stale frame per call and True about four, that is, to the current frame. `trigger_blocking` is True. These defaults come from the callers, the manual and those measurements.
- **#18 Monitor rate.** The Monitor preview is read at most 10 times a second, as the manufacturer advises (Q20).
- **#19 Licence exception.** `deny.toml` allows `target-lexicon` (Apache-2.0 WITH LLVM-exception). It is used only while building and is not in the `.pyd`.
- **#20 Own LZ4 decoder.** The driver decodes LZ4 with its own safe Rust code instead of `lz4_flex`: about 3.7 times faster on one bitshuffled frame (measured on the development machine with synthetic frames), and checked against `lz4_flex` and the manufacturer's Python package (§4.5).
- **#21 TIFF metadata.** The Monitor TIFF metadata (tag 51192) stays a dict of strings.
