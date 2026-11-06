# super_dectris — a Rust driver for DECTRIS SINGLA detectors

`CameraDectris` (yaml `interface: dectris_simplon`) talks to the detector control unit over the
SIMPLON 1.8 REST API and Stream V2 (ZeroMQ). The whole driver is the PyO3 extension
`_super_dectris`, the camera class included: `CameraDectris` adds what Instamatic needs (the yaml,
`get_image`, `get_movie`, the Monitor preview) to the driver type `DectrisCore`.

```
__init__.py         re-exports; registers CameraDectris as a virtual CameraBase
selfcheck.py        the integration self-check (INTEGRATION.md step 10), with selfcheck_image.cbor
_super_dectris…pyd  built from rust/ by build.ps1, not committed
rust/               Cargo workspace (common, codec, simplon, stream, core, py), build.ps1, test.ps1, LAYOUT.md
```

## Documents

They are in `docs/` at the root of the repository. Comments in the source cite them:

- `DESIGN §n` or `docs/DESIGN.md §n`: the design of the driver.
- `PROTOCOL §n` or `docs/PROTOCOL.md §n`: what the detector sends and accepts.
- `DESIGN §12 #n`: decision n, with its reason.
- Tags in brackets, such as `[R7]` or `[G1-F]`, are only labels: a comment and a paragraph of
  `docs/DESIGN.md` with the same label are about the same thing.

## Building

You need Windows x64, the Rust toolchain (`rustup`; the workspace pins Rust 1.91.1), the Visual
Studio C++ build tools, and a Python 3.12 environment with `instamatic` installed editable from the
source tree this folder is in (see the README at the root of the repository). Any environment
works: conda, a python.org install, or a plain venv such as one made by `uv`. Without conda, the
driver also needs the Microsoft Visual C++ Redistributable (x64) at run time.

Keep the source tree on a short path, such as `C:\src\instamatic`: MSVC's linker fails on long
paths, and `build.ps1` stops with a message when the path is over 150 characters.

```powershell
powershell -ExecutionPolicy Bypass -File src\instamatic\camera\super_dectris\rust\build.ps1 -Python C:\path\to\python.exe
```

This runs the Rust tests, the timing bounds (an optimised build of `dectris_codec`'s `perf` test),
clippy and cargo-deny, builds the release `.pyd` into `build/cargo-target/` (outside the package),
copies it here as `_super_dectris.cp312-win_amd64.pyd`, checks that it imports from this folder, and
regenerates `THIRD_PARTY_NOTICES.md` and `build/_build_info.json`. `-Quick` skips the post-build
extras; `-NoChecks` skips the tests.

## Tests

- **Rust:** `rust\test.ps1 -Python C:\path\to\python.exe` runs `cargo test --workspace` with the
  same environment as `build.ps1` (`PYO3_PYTHON`, `CARGO_TARGET_DIR`, `RUSTFLAGS`). A plain
  `cargo test` with other values rebuilds every crate. Extra arguments go to `cargo test`, in
  their long form (`--package dectris_codec`, not `-p`), since PowerShell takes a single-dash option
  for one of the script's own.
  - Set `DECTRIS_TEST_PYTHON` to a Python that has `dectris-compression` 0.3.1 and `hdf5plugin`,
    to compare the decoders with DECTRIS's own. Without it, those tests print SKIP and pass.
  - Set `DECTRIS_MONITOR_SAMPLE` to a Monitor TIFF saved from a DCU, for one more TIFF test.
    Without it, that test prints SKIP and passes.
- **Python:** `python -m pytest tests/super_dectris`, against a scripted fake control unit (SIMPLON
  over HTTP and a Stream V2 PUSH socket). They need the built `.pyd`, and the packages `pytest`,
  `pyzmq`, `cbor2<6`, `h5py`, `hdf5plugin`, `tifffile` and `dectris-compression` (built from
  `third_party/`, see its README). Without `dectris-compression` the tests of
  `test_module_functions.py` are skipped. A skipped test checked nothing.

  With all of them, 126 tests pass and 5 are skipped: four cases of an image test whose vector is
  not an image message, and the soak test, which runs when `SUPER_DECTRIS_SLOW=1` is set.
