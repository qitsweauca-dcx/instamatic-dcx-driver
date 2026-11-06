
## Bundled C/C++ sources (manual addendum, appended by build.ps1)

cargo-about reads only crate licence fields. The `zeromq-src` crate (MIT OR Apache-2.0, build
dependency of `zmq-sys`) vendors and statically compiles the following C/C++ sources into
`_super_dectris.cp312-win_amd64.pyd`:

| component | version | licence | source |
|---|---|---|---|
| libzmq | 4.3.5 (via `zeromq-src 0.3.6+4.3.5`) | MPL-2.0 | https://github.com/zeromq/libzmq/tree/v4.3.5 ; also the `zeromq-src-0.3.6+4.3.5` crate on crates.io |
| wepoll (bundled in libzmq) | 1.5.8, as vendored by libzmq 4.3.5 | BSD-2-Clause (Copyright Bert Belder) | https://github.com/piscisaureus/wepoll |
| sha1 (`external/sha1/sha1.c`, bundled in libzmq) | as vendored by libzmq 4.3.5 (`ZMQ_USE_BUILTIN_SHA1`) | BSD-3-Clause (Copyright WIDE Project) | https://github.com/zeromq/libzmq/tree/v4.3.5/external/sha1 |

libzmq is used unmodified. Under MPL-2.0 its source code is available at the locations above. The full
licence texts are in the vendored sources (`zeromq-src-0.3.6+4.3.5/vendor/` in the cargo registry).
CURVE/libsodium is not enabled.
