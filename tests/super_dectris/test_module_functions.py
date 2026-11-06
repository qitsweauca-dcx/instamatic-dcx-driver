"""Module-level functions vs a spec-derived oracle written fresh here:
cbor2 + ``dectris.compression`` + numpy (DESIGN §8 differential decode)."""

from __future__ import annotations

import numpy as np
import pytest

from .conftest import VECTORS

cbor2 = pytest.importorskip('cbor2')
dc = pytest.importorskip('dectris.compression').decompress

DT = {
    64: 'u1', 65: '>u2', 66: '>u4', 67: '>u8', 68: 'u1', 69: '<u2', 70: '<u4', 71: '<u8', 72: 'i1',
    73: '>i2', 74: '>i4', 75: '>i8', 77: '<i2', 78: '<i4', 79: '<i8', 80: '>f2', 81: '>f4', 82: '>f8',
    84: '<f2', 85: '<f4', 86: '<f8',
}  # fmt: skip


def _hook(dec, tag):
    if tag.tag == 56500:
        alg, mod, data = tag.value
        return dc(data, alg, elem_size=mod) if alg == 'bslz4' else dc(data, alg)
    if tag.tag in DT:
        return np.frombuffer(tag.value, DT[tag.tag])
    if tag.tag in (40, 1040):
        dims, arr = tag.value
        return np.asarray(arr).reshape(dims, order='F' if tag.tag == 1040 else 'C')
    return tag


def _same(a, b, path='$'):
    if isinstance(a, np.ndarray) or isinstance(b, np.ndarray):
        a2, b2 = np.asarray(a), np.asarray(b)
        assert a2.shape == b2.shape, (path, a2.shape, b2.shape)
        # [G2-P]: plain-array elements decode to an object ndarray (the oracle infers int64)
        assert a2.dtype == object or a2.dtype.newbyteorder('=') == b2.dtype.newbyteorder('='), path
        assert np.array_equal(a2, b2), path
    elif isinstance(a, dict):
        assert set(a) == set(b), (path, set(a) ^ set(b))
        for k in a:
            _same(a[k], b[k], f'{path}.{k}')
    elif isinstance(a, list):
        assert len(a) == len(b), path
        for i, (x, y) in enumerate(zip(a, b)):
            _same(x, y, f'{path}[{i}]')
    elif hasattr(b, 'tag'):  # a tag the oracle leaves undecoded
        assert hasattr(a, 'tag') and a.tag == b.tag, path
    elif hasattr(b, 'isoformat'):  # tag 0: cbor2 gives datetime, we give the ISO string
        assert isinstance(a, str), path
    else:
        assert a == b, (path, a, b)


MESSAGES = sorted(VECTORS.glob('msg_*.cbor'))


@pytest.mark.parametrize('path', MESSAGES, ids=lambda p: p.name)
def test_decode_message_matches_oracle(m, path):
    raw = path.read_bytes()
    ours = m.decode_message(raw)
    _same(ours, cbor2.loads(raw, tag_hook=_hook), path.name)


@pytest.mark.parametrize('path', MESSAGES, ids=lambda p: p.name)
def test_decompress_image_all_input_shapes(m, path):
    raw = path.read_bytes()
    ref = cbor2.loads(raw, tag_hook=_hook)
    if ref.get('type') != 'image':
        pytest.skip('not an image message')
    img = m.decompress_image(raw)
    assert img.flags['C_CONTIGUOUS'] and img.flags['OWNDATA'] and img.flags['WRITEABLE']
    assert np.array_equal(img, ref['data']['threshold_1'])
    # cbor2 CBORTag objects (lists, not tuples) through the duck-typed path
    assert np.array_equal(m.decompress_image(cbor2.loads(raw)), img)
    # an ndarray already in place is returned as is
    assert m.decompress_image({'data': {'threshold_1': img}}) is img
    h = m.decode_raw_header(raw)
    assert (h['type'], h['series_id'], h['image_id']) == ('image', ref['series_id'], ref['image_id'])


def test_decode_errors_are_stream_decode_errors(m):
    with pytest.raises(m.StreamDecodeError):
        m.decode_message(b'\xa1\x61')
    with pytest.raises(m.StreamDecodeError, match="'threshold_1'"):
        m.decompress_image({'data': {'x': 1}})
    with pytest.raises(ValueError):  # StreamDecodeError is also a ValueError
        m.decode_message(b'\xff')


def test_exception_classes(m):  # [R30]
    mod = 'instamatic.camera.super_dectris._super_dectris'
    for name in ['DectrisError', 'DectrisHTTPError', 'DectrisConnectionError', 'DectrisTimeout',
                 'DetectorStateError', 'DectrisBusy', 'StreamDecodeError']:  # fmt: skip
        cls = getattr(m, name)
        assert cls.__module__ == mod and cls.__qualname__ == name
        assert issubclass(cls, m.DectrisError) and issubclass(cls, Exception)
    assert issubclass(m.DectrisTimeout, TimeoutError)
    assert issubclass(m.DectrisConnectionError, ConnectionError)
    assert issubclass(m.StreamDecodeError, ValueError)
    e = m.DectrisTimeout('x')
    assert e.args == ('x',) and str(e) == 'x'


def test_module_names(m):
    assert len(m.TYPED_ARRAY_DTYPES) == 21 and m.TYPED_ARRAY_DTYPES[69] == '<u2'
    assert {76, 83, 87}.isdisjoint(m.TYPED_ARRAY_DTYPES)
    assert set(m.tag_decoders) == {0, 40, 1040, 56500} | set(m.TYPED_ARRAY_DTYPES)
    assert len(m.OPTION_KEYS) == 13 and not any('name' in k for k in m.OPTION_KEYS)
    allowed, foreign = m._test_attach_counts()
    assert foreign == 0
