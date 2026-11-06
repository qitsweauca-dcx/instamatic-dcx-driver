"""Oracle test-vector generator for the dectris_codec crate (docs/DESIGN.md §8 Tier 1, §9 Phase 1).

Run with a Python that has the oracle packages (dectris-compression 0.3.1, hdf5plugin 6.0.0,
h5py, cbor2, numpy):

    python tests/gen_vectors.py all

Sub-commands
------------
``all``                     regenerate every committed file under ``tests/vectors/`` (small files,
                            each < 200 KB) and ``tests/vectors/regen.manifest``.
``regen NAME OUTDIR``       regenerate one large case listed in ``regen.manifest`` into OUTDIR
                            (``NAME.bin`` = compressed stream); prints ``sha256(compressed)
                            sha256(oracle output)``. Used by the Rust tests at test time, never
                            committed.
``oracle-batch IN OUT``     decode every record of IN with ``dectris.compression.decompress``
                            (record: u8 algo [0 bslz4, 1 lz4], u32 LE elem_size, u32 LE len,
                            bytes) and write per record: u8 status (0 ok, 1 error), u32 LE len,
                            bytes (output or the error text). Used by the differential tests.

Data
----
Every array is ``pattern_bytes(pattern, seed, n, elem)``, mirrored exactly by
``tests/common/mod.rs`` (splitmix64 of the element index), so committed vectors store only the
compressed bytes: the Rust side regenerates the expected output. Before writing a vector this
script asserts ``dectris.compression.decompress(stream) == pattern bytes``, so "equals the
regenerated data" in Rust means byte-exact agreement with the oracle.

Compressed streams come from hdf5plugin (``Bitshuffle(nelems, cname='lz4')`` / ``LZ4(nbytes)``,
one chunk = the whole array, bytes via ``read_direct_chunk``), i.e. the vendor C bitshuffle and
liblz4 encoders, not this crate's encoder. Stream V2 messages are encoded with cbor2.
"""

from __future__ import annotations

import hashlib
import io
import os
import struct
import sys
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
VEC = HERE / 'vectors'
MAX_FILE = 200 * 1024

M64 = (1 << 64) - 1


# ---------------------------------------------------------------- data (mirrors common/mod.rs)
def splitmix(seed: int, idx: np.ndarray) -> np.ndarray:
    with np.errstate(over='ignore'):
        z = np.uint64(seed) + (idx.astype(np.uint64) + np.uint64(1)) * np.uint64(0x9E3779B97F4A7C15)
        z = (z ^ (z >> np.uint64(30))) * np.uint64(0xBF58476D1CE4E5B9)
        z = (z ^ (z >> np.uint64(27))) * np.uint64(0x94D049BB133111EB)
        return z ^ (z >> np.uint64(31))


def pattern_values(pattern: str, seed: int, n: int, elem: int) -> np.ndarray:
    mask = np.uint64(M64 if elem >= 8 else (1 << (8 * elem)) - 1)
    i = np.arange(n, dtype=np.uint64)
    r = splitmix(seed, i)
    u = np.uint64
    if pattern == 'low':
        v = np.where((r & u(63)) != 0, (r >> u(61)) & mask, (r >> u(8)) & mask)
    elif pattern == 'random':
        v = r & mask
    elif pattern == 'zeros':
        v = np.zeros(n, dtype=np.uint64)
    elif pattern == 'ramp':
        v = i & mask
    elif pattern == 'sat':
        v = np.where((r & u(15)) == 0, mask, (r >> u(62)) & mask)
    else:
        raise ValueError(pattern)
    return v.astype({1: '<u1', 2: '<u2', 4: '<u4', 8: '<u8'}[elem])


def pattern_bytes(pattern: str, seed: int, n: int, elem: int) -> bytes:
    return pattern_values(pattern, seed, n, elem).tobytes()


# ---------------------------------------------------------------- encoders (vendor C via hdf5plugin)
def h5_chunk(arr: np.ndarray, filt) -> bytes:
    import h5py

    with h5py.File(io.BytesIO(), 'w') as f:
        d = f.create_dataset('x', data=arr, chunks=arr.shape, **filt)
        return bytes(d.id.read_direct_chunk((0,) * arr.ndim)[1])


def encode(algo: str, arr: np.ndarray, param: int) -> bytes:
    import hdf5plugin

    if algo == 'bslz4':
        return h5_chunk(arr, hdf5plugin.Bitshuffle(nelems=param, cname='lz4'))
    if algo == 'lz4':
        return h5_chunk(arr, hdf5plugin.LZ4(nbytes=param))
    raise ValueError(algo)


def oracle(stream: bytes, algo: str, elem: int) -> bytes:
    from dectris.compression import decompress

    if algo == 'bslz4':
        return decompress(stream, 'bslz4', elem_size=elem)
    return decompress(stream, 'lz4')


def sha(b: bytes) -> str:
    return hashlib.sha256(b).hexdigest()


# ---------------------------------------------------------------- committed oracle vectors
SHAPES = [(1,), (7,), (8,), (9,), (63,), (64,), (65,), (13, 17), (31, 33), (64, 64), (100, 37), (96, 97), (257, 3)]
PATTERNS = ['low', 'random', 'zeros', 'ramp', 'sat']
BS_NELEMS = [0, 8, 64, 256, 1024, 16]
LZ4_NBYTES = [0, 7, 64, 1000, 4096, 1]
DTYPES = {1: 'u8', 2: 'u16', 4: 'u32'}


def gen_oracle_vectors() -> int:
    """Write oracle_<algo>_<dtype>.bin containers + oracle.manifest; returns the count."""
    lines = ['# name algo elem pattern seed n_elems block_param container offset length']
    count = 0
    for algo in ('bslz4', 'lz4'):
        for elem, dname in DTYPES.items():
            k = 0
            for pattern in PATTERNS:
                container = f'oracle_{algo}_{dname}_{pattern}.bin'
                blob = bytearray()
                for shape in SHAPES:
                    n = int(np.prod(shape))
                    seed = 1000 + 37 * k + elem
                    params = BS_NELEMS if algo == 'bslz4' else LZ4_NBYTES
                    param = params[k % len(params)]
                    if algo == 'lz4' and 0 < param < 64 and n * elem > 1024:
                        param += 64  # 1- and 7-byte blocks only on small arrays (file size)
                    k += 1
                    data = pattern_values(pattern, seed, n, elem).reshape(shape)
                    stream = encode(algo, data, param)
                    assert oracle(stream, algo, elem) == data.tobytes(), (algo, elem, shape, pattern)
                    name = f'{algo}_{dname}_{"x".join(map(str, shape))}_{pattern}_p{param}'
                    lines.append(f'{name} {algo} {elem} {pattern} {seed} {n} {param} {container} {len(blob)} {len(stream)}')
                    blob += stream
                    count += 1
                assert len(blob) < MAX_FILE, (container, len(blob))
                (VEC / container).write_bytes(bytes(blob))
    (VEC / 'oracle.manifest').write_text('\n'.join(lines) + '\n', encoding='utf-8')
    return count


def gen_junk_vectors() -> int:
    """Trailing-junk streams with the oracle's verdict (DESIGN §4.5: we reject; the oracle may not)."""
    from dectris.compression import decompress  # noqa: F401

    lines = ['# name algo elem pattern seed n_elems junk_hex oracle_verdict container offset length']
    blob = bytearray()
    count = 0
    cases = [
        ('bslz4', 4, (64,), 'low', 0),      # no leftover: oracle ignores the junk
        ('bslz4', 4, (65,), 'low', 0),      # leftover 4 bytes: oracle checks the tail
        ('bslz4', 2, (13, 17), 'ramp', 8),
        ('bslz4', 1, (96, 97), 'sat', 64),
        ('lz4', 4, (64,), 'random', 0),
        ('lz4', 2, (13, 17), 'low', 7),
        ('lz4', 1, (257, 3), 'zeros', 64),
    ]
    for i, (algo, elem, shape, pattern, param) in enumerate(cases):
        n = int(np.prod(shape))
        seed = 5000 + i
        data = pattern_values(pattern, seed, n, elem).reshape(shape)
        stream = encode(algo, data, param)
        for junk in (b'\x00', b'JUNK', b'\x00\x00\x00\x00'):
            s = stream + junk
            try:
                out = oracle(s, algo, elem)
                verdict = 'ok' if out == data.tobytes() else 'wrong'
            except RuntimeError:
                verdict = 'err'
            lines.append(f'junk{i}_{junk.hex()} {algo} {elem} {pattern} {seed} {n} {junk.hex()} {verdict} oracle_junk.bin {len(blob)} {len(s)}')
            blob += s
            count += 1
    (VEC / 'oracle_junk.bin').write_bytes(bytes(blob))
    (VEC / 'oracle_junk.manifest').write_text('\n'.join(lines) + '\n', encoding='utf-8')
    return count


# ---------------------------------------------------------------- large cases (regenerated at test time)
REGEN = [
    # name, algo, elem, pattern, seed, shape, param
    ('full_u32_bslz4_low', 'bslz4', 4, 'low', 7, (1028, 1062), 0),
    ('full_u16_bslz4_sat', 'bslz4', 2, 'sat', 8, (1028, 1062), 0),
    ('full_u8_bslz4_random', 'bslz4', 1, 'random', 9, (1028, 1062), 0),
    ('full_u32_lz4_ramp', 'lz4', 4, 'ramp', 10, (1028, 1062), 0),
    ('half_u32_bslz4_low', 'bslz4', 4, 'low', 11, (1028, 514), 0),
    ('odd_u32_bslz4_low_multi', 'bslz4', 4, 'low', 12, (1021, 1063), 1024),
]


def regen_case(name: str):
    for c in REGEN:
        if c[0] == name:
            _, algo, elem, pattern, seed, shape, param = c
            data = pattern_values(pattern, seed, int(np.prod(shape)), elem).reshape(shape)
            stream = encode(algo, data, param)
            out = oracle(stream, algo, elem)
            assert out == data.tobytes(), name
            return c, stream, out
    raise SystemExit(f'unknown regen case {name}')


def gen_regen_manifest() -> None:
    lines = ['# name algo elem pattern seed n_elems block_param sha256_compressed sha256_output']
    for c in REGEN:
        name, algo, elem, pattern, seed, shape, param = c
        _, stream, out = regen_case(name)
        n = int(np.prod(shape))
        lines.append(f'{name} {algo} {elem} {pattern} {seed} {n} {param} {sha(stream)} {sha(out)}')
    (VEC / 'regen.manifest').write_text('\n'.join(lines) + '\n', encoding='utf-8')


# ---------------------------------------------------------------- Stream V2 messages (cbor2)
UNIQUE = '01HV2SERIES0000000000000042'


def tag(n, v):
    import cbor2

    return cbor2.CBORTag(n, v)


def typed(elem_tag: int, arr: np.ndarray, payload: str):
    """typed-array tag over `arr` (already in the tag's byte order)."""
    raw = arr.tobytes()
    if payload == 'raw':
        return tag(elem_tag, raw)
    flat = np.frombuffer(raw, dtype=f'u{arr.dtype.itemsize}')
    if payload == 'bslz4':
        stream = encode('bslz4', flat, 0)
        assert oracle(stream, 'bslz4', arr.dtype.itemsize) == raw
        return tag(elem_tag, tag(56500, ['bslz4', arr.dtype.itemsize, stream]))
    if payload == 'lz4':
        stream = encode('lz4', flat, 0)
        assert oracle(stream, 'lz4', 0) == raw
        return tag(elem_tag, tag(56500, ['lz4', 0, stream]))
    raise ValueError(payload)


def flatfield(rows, cols):
    return (1.0 + (np.arange(rows * cols) % 7) * 0.01).astype('<f4').reshape(rows, cols)


def pixel_mask(rows, cols):
    return np.where(np.arange(rows * cols) % 97 == 0, 1, 0).astype('<u4').reshape(rows, cols)


def gen_messages() -> int:
    import cbor2

    lines = ['# file kind series_id image_id elem_tag rows cols order pattern seed payload']
    count = 0

    def write(fname, obj, desc, canonical=False):
        nonlocal count
        b = cbor2.dumps(obj, canonical=canonical)
        assert len(b) < MAX_FILE, fname
        (VEC / fname).write_bytes(b)
        lines.append(f'{fname} {desc}')
        count += 1

    rows, cols = 32, 48
    for payload in ('raw', 'bslz4'):
        start = {
            'type': 'start',
            'series_id': 42,
            'series_unique_id': UNIQUE,
            'arm_date': tag(0, '2026-10-21T01:02:03.000000+00:00'),
            'beam_center_x': 24.5,
            'beam_center_y': 16.25,
            'channels': ['threshold_1'],
            'count_time': 0.0999,
            'frame_time': 0.1,
            'countrate_correction_enabled': True,
            'countrate_correction_lookup_table': typed(70, np.arange(1024, dtype='<u4') * 3, payload),
            'detector_description': 'Dectris SINGLA 1M',
            'detector_serial_number': 'E-01-0000',
            'detector_translation': [0.0, 0.0, 0.5],
            'flatfield': {'threshold_1': tag(40, [[rows, cols], typed(85, flatfield(rows, cols), payload)])},
            'flatfield_enabled': True,
            'goniometer': {'chi': {'increment': 0.0, 'start': 0.0}, 'omega': {'increment': 0.5, 'start': -30.0}},
            'image_dtype': 'uint32',
            'image_size_x': cols,
            'image_size_y': rows,
            'incident_energy': 200000.0,
            'incident_wavelength': 0.0251,
            'number_of_images': 5,
            'pixel_mask': {'threshold_1': tag(40, [[rows, cols], typed(70, pixel_mask(rows, cols), payload)])},
            'pixel_mask_enabled': True,
            'pixel_size_x': 7.5e-05,
            'pixel_size_y': 7.5e-05,
            'saturation_value': 1048575,
            'sensor_material': 'Si',
            'sensor_thickness': 0.00045,
            'threshold_energy': {'threshold_1': 4000.0},
            'user_data': {'note': 'gen_vectors', 'n': -3, 'f': None},
            'virtual_pixel_interpolation_enabled': False,
        }
        write(f'msg_start_all_{payload}.cbor', start, f'start 42 - - {rows} {cols} - - - {payload}')

    # images: (elem_tag, elem, payload, pattern, order)
    rows, cols = 37, 53
    specs = [
        (70, 4, 'bslz4', 'low', 'C'), (69, 2, 'bslz4', 'sat', 'C'), (64, 1, 'bslz4', 'random', 'C'),
        (70, 4, 'lz4', 'low', 'C'), (70, 4, 'raw', 'ramp', 'C'), (69, 2, 'raw', 'low', 'C'),
        (68, 1, 'raw', 'sat', 'C'), (70, 4, 'raw', 'low', 'F'), (70, 4, 'bslz4', 'sat', 'F'),
        (69, 2, 'lz4', 'random', 'F'),
    ]
    for i, (et, elem, payload, pattern, order) in enumerate(specs):
        seed = 700 + i
        a = pattern_values(pattern, seed, rows * cols, elem).reshape(rows, cols)  # logical C array
        wire = a if order == 'C' else np.asfortranarray(a)
        flat = np.frombuffer(wire.tobytes(order='K' if order == 'C' else 'F'), dtype=a.dtype)
        if order == 'F':
            assert np.array_equal(np.reshape(flat, (rows, cols), order='F'), a)
        img = {
            'type': 'image',
            'series_id': 42,
            'series_unique_id': UNIQUE,
            'image_id': i + 1,
            'real_time': [4000000, 40000000],
            'series_date': tag(0, '2026-10-21T01:02:03.456789+00:00'),
            'start_time': [i * 40000000, 40000000],
            'stop_time': [i * 40000000 + 4000000, 40000000],
            'user_data': None,
            'data': {'threshold_1': tag(40 if order == 'C' else 1040, [[rows, cols], typed(et, flat, payload)])},
        }
        write(f'msg_image_{i}.cbor', img, f'image 42 {i + 1} {et} {rows} {cols} {order} {pattern} {seed} {payload}',
              canonical=(i % 2 == 1))

    write('msg_end.cbor', {'type': 'end', 'series_id': 42, 'series_unique_id': UNIQUE}, 'end 42 - - - - - - - -')

    # every typed-array tag, decode_message path (expected LE values in msg_typed_expected.bin)
    tags = {64: 'u1', 65: '>u2', 66: '>u4', 67: '>u8', 68: 'u1', 69: '<u2', 70: '<u4', 71: '<u8', 72: 'i1', 73: '>i2',
            74: '>i4', 75: '>i8', 77: '<i2', 78: '<i4', 79: '<i8', 80: '>f2', 81: '>f4', 82: '>f8', 84: '<f2',
            85: '<f4', 86: '<f8'}
    msg = {'type': 'typed'}
    expected = bytearray()
    for t, dt in tags.items():
        d = np.dtype(dt)
        if d.kind == 'f':
            vals = np.array([0.0, -1.5, 65504.0 if d.itemsize == 2 else 1e30, 2.0 ** -24, np.inf, -0.0, 3.25, 7.0])
        elif d.kind == 'i':
            vals = np.array([0, -1, 1, -(2 ** (8 * d.itemsize - 1)), 2 ** (8 * d.itemsize - 1) - 1, 5, -7, 9])
        else:
            vals = np.array([0, 1, 2 ** (8 * d.itemsize) - 1, 7, 8, 9, 10, 255], dtype=np.uint64)
        arr = vals.astype(dt)
        msg[f't{t}'] = tag(t, arr.tobytes())
        msg[f'm{t}'] = tag(40, [[2, 4], tag(t, arr.tobytes())])
        expected += arr.astype(d.newbyteorder('<')).tobytes()
    msg['bstr56500'] = tag(56500, ['lz4', 0, encode('lz4', np.frombuffer(b'hello world, hello world', dtype='u1'), 0)])
    msg['object_md'] = tag(1040, [[2, 3], [1, 2, 3, 4, 5, 6]])
    msg['unknown'] = tag(1234, {'a': [1, 2]})
    msg['date'] = tag(0, '2026-10-21T00:00:00Z')
    msg['big'] = 2 ** 64 - 1
    msg['neg'] = -(2 ** 64)
    msg['f16'] = 1.5
    write('msg_typed.cbor', msg, 'typed - - - - - - - - -', canonical=True)
    (VEC / 'msg_typed_expected.bin').write_bytes(bytes(expected))

    (VEC / 'messages.manifest').write_text('\n'.join(lines) + '\n', encoding='utf-8')
    return count


# ---------------------------------------------------------------- oracle batch (differential tests)
def oracle_batch(inp: Path, outp: Path) -> None:
    from dectris.compression import decompress

    data = inp.read_bytes()
    pos = 0
    out = bytearray()
    while pos < len(data):
        algo, elem, n = struct.unpack_from('<BII', data, pos)
        pos += 9
        stream = data[pos:pos + n]
        pos += n
        try:
            if algo == 0:
                r = decompress(stream, 'bslz4', elem_size=elem)
            else:
                r = decompress(stream, 'lz4')
            out += struct.pack('<BI', 0, len(r)) + r
        except Exception as e:  # RuntimeError / ValueError from the binding
            msg = f'{type(e).__name__}: {e}'.encode()
            out += struct.pack('<BI', 1, len(msg)) + msg
    outp.write_bytes(bytes(out))


def main(argv):
    if not argv or argv[0] in ('-h', '--help'):
        print(__doc__)
        return 0
    cmd = argv[0]
    if cmd == 'all':
        VEC.mkdir(exist_ok=True)
        n1 = gen_oracle_vectors()
        n2 = gen_junk_vectors()
        n3 = gen_messages()
        gen_regen_manifest()
        sizes = {p.name: p.stat().st_size for p in VEC.iterdir()}
        big = {k: v for k, v in sizes.items() if v >= MAX_FILE}
        assert not big, big
        print(f'oracle vectors {n1}, junk vectors {n2}, messages {n3}, regen cases {len(REGEN)}; '
              f'{len(sizes)} files, {sum(sizes.values())} bytes, largest {max(sizes.values())}')
        return 0
    if cmd == 'regen':
        name, outdir = argv[1], Path(argv[2])
        outdir.mkdir(parents=True, exist_ok=True)
        _, stream, out = regen_case(name)
        (outdir / f'{name}.bin').write_bytes(stream)
        print(sha(stream), sha(out))
        return 0
    if cmd == 'oracle-batch':
        oracle_batch(Path(argv[1]), Path(argv[2]))
        return 0
    print(f'unknown command {cmd}', file=sys.stderr)
    return 2


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
