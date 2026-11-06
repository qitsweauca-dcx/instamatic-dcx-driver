# Third-party sources

## `dectris-compression-0.3.1/`

DECTRIS's Python package `dectris-compression` 0.3.1, the source distribution from PyPI, unchanged
except that `python/dectris_compression.egg-info/` is left out, since setuptools writes it again at
each install (`dectris-compression-0.3.1.tar.gz`, SHA-256
`554a2196d650f287cb5f8e77284b038e6933ec877d38c5b503e2ea4883c703da`; upstream:
<https://github.com/dectris/compression>). The driver does not use it. The tests use it as the
reference decoder that the driver's own bslz4 and lz4 decoders are compared with.

PyPI has no Windows wheel of it, so `pip install dectris-compression` builds it from source anyway.
To build it from this copy (it needs the Visual Studio C++ build tools, which the driver needs too):

```powershell
pip install third_party\dectris-compression-0.3.1
```

Licences, as in the folder:

- `LICENSE`: MIT, Copyright (c) 2020 DECTRIS Ltd.
- `third_party/bitshuffle/`: MIT or Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`).
- `third_party/lz4/lib/`: BSD 2-Clause (stated at the top of each file; `third_party/lz4/LICENSE`).
