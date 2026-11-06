# Rust workspace layout — `_super_dectris`

The crate map, the build, and the shared API for code in this workspace. The design is in
`docs/DESIGN.md` and the protocol in `docs/PROTOCOL.md`, at the root of the repository.

This directory is a Cargo workspace, **not** a Python package (no `__init__.py`, DESIGN §7 [R28]).

## Layout

Each layer is its own crate, and `dectris_core` is a thin facade that re-exports them as `codec`,
`simplon` and `stream` (DESIGN §1).

| path | crate | contents | DESIGN section |
|---|---|---|---|
| `Cargo.toml` | (workspace) | members, `[workspace.dependencies]` pins, lints, profiles `release` (fat LTO, cgu 1, unwind, debug 1) / `test-opt` | §7 |
| `rust-toolchain.toml` | — | channel 1.91.1 + clippy, rustfmt | §7 [R41] |
| `.cargo/config.toml` | — | default target dir `<repo>/build/cargo-target` | §7 [R36] |
| `clippy.toml` | — | `disallowed-macros` for bare `log::*`; unwrap/expect/panic allowed in tests | §2.9 [R31] |
| `deny.toml` | — | licence allow-list | §7 [R38] |
| `about.toml`, `about.hbs`, `notices_addendum.md` | — | cargo-about config, template, libzmq/wepoll addendum | §7 [R38] |
| `build.ps1` | — | test → clippy → deny → release build → copy `.pyd` → import assertion → notices / build info | §7 [R26][R27][R29][R41] |
| `crates/dectris_common/` | `dectris_common` | `log.rs` (`dlog!`/`slog!`, `NO_PYTHON`, `LOGGING_CLOSED`, `LOG_FRAMES`, `Deferred`), `error.rs` (`DriverError`), `sync.rs` (poison-recovering lock), `testing.rs` (feature `test-util`: capture logger) | §2.9, §3.2 R5/R6, §6 |
| `crates/dectris_codec/` | `dectris_codec` | `cbor.rs typed_array.rs framing.rs bitshuffle.rs lz4.rs image.rs message.rs encode.rs` — pure, no I/O | §4.4–§4.6 |
| `crates/dectris_simplon/` | `dectris_simplon` | `client.rs detector.rs session.rs signals.rs stream_cfg.rs filewriter.rs monitor.rs worker.rs tiff.rs testing.rs` | §4.1–§4.2, §3.4 |
| `crates/dectris_stream/` | `dectris_stream` | `zmq_link.rs receiver.rs attribution.rs message.rs guard.rs registry.rs stats.rs` + `build.rs` (advapi32) | §4.3, §3.7, §3.2 R2 |
| `crates/dectris_core/` | `dectris_core` | facade (`pub use dectris_codec as codec; …simplon; …stream; pub use dectris_common::*;`) + `driver.rs` (`Arc<Inner>`, epochs, `trig_cv`, R7 ownership), `preview.rs` | §1, §3.2, §3.4 |
| `py/` | `super_dectris_py`, lib `_super_dectris` (cdylib) | `lib.rs core.rs camera_dectris.rs session.rs iters.rs convert.rs errors.rs logging.rs stats.rs` | §2 |

Dependency direction: `dectris_common` ← `dectris_codec` ← `dectris_stream`; `dectris_common` ← `dectris_simplon`;
all four ← `dectris_core` ← `py`. Only `py` depends on pyo3.

Output: `build.ps1` copies `<target>/release/_super_dectris.dll` to
`src/instamatic/camera/super_dectris/_super_dectris.cp312-win_amd64.pyd`.

## Building

Use `build.ps1`: it sets `PYO3_PYTHON`, builds the release `.pyd` and copies it into the package (DESIGN §7):

```powershell
powershell -ExecutionPolicy Bypass -File src\instamatic\camera\super_dectris\rust\build.ps1 -Python C:\path\to\python.exe [-Quick] [-NoChecks]
```

`-Quick` skips the post-build extras (cargo about, `build\_build_info.json`, PDB copy); `-NoChecks` skips test/clippy/deny.
The copy to the `.pyd` and the import assertion always run. In this folder, `cargo test -p <crate>` runs a crate's
tests without Python, for every crate but `super_dectris_py`. A plain `cargo build` makes a debug DLL that nothing loads.

## `dectris_common` API (use these; do not invent parallel ones)

- Macros: `dlog!(Info, "fmt {}", x)` → target `instamatic::camera::camera_dectris`; `slog!(Warn, …)` → `…::stream`;
  `dlog!(level: lvl_expr, …)`. Arguments are evaluated only when the level is enabled. Never under a `MutexGuard` (R6).
- `log::{TARGET_DRIVER, TARGET_STREAM, PY_LOGGER_DRIVER, PY_LOGGER_STREAM, ALLOWED_TARGET_ROOT, Level}`.
- `log::set_no_python()` (first statement of every worker thread), `log::no_python()`.
- `log::close_logging()` (atexit `_final_release` only), `log::logging_closed()`, `log::python_logging_blocked()`, `log::target_allowed(t)`.
- `log::LOG_FRAMES`, `log::log_frames()`, `log::set_log_frames(bool)` — gate per-frame and `Drop` records.
- `log::Deferred`: `new`, `push(level, msg)`, `push_stream`, `push_target`, `warn`/`info`/`debug`, `append`, `len`, `is_empty`,
  `records`, `emit(self)`, `into_records(self)`, `discard(self)`. `#[must_use]`; debug builds panic if dropped non-empty.
- `DriverError` (DESIGN §6 exactly), `DriverError::{kind, from_panic, decode}`, `ExceptionKind::python_name`, `Result<T>`.
- `sync::lock_unpoisoned(&Mutex<T>)` (detached threads; attached threads use pyo3 `lock_py_attached`).
- Tests: `[dev-dependencies] dectris_common = { workspace = true, features = ["test-util"] }`, then
  `dectris_common::testing::{install, take_matching}`; tag messages uniquely (tests run in parallel).
