"""What moved into Rust with ``CameraDectris``: the preview scaling (against scipy), the Monitor
TIFF variants, ``cam.session`` and the class ``CamClient`` reads."""

from __future__ import annotations

import io
import logging
import time

import numpy as np
import pytest
import tifffile
from PIL import Image
from scipy import ndimage

from .conftest import orient


def wait_for_monitor(cam):
    time.sleep(1.0 / cam.monitor_max_hz + 0.05)


def tiff_of(img: np.ndarray, **save) -> bytes:
    out = io.BytesIO()
    Image.fromarray(img).save(out, format='TIFF', **save)
    return out.getvalue()


def as_scipy(img: np.ndarray) -> np.ndarray:
    return ndimage.zoom(orient(img), 640 / max(img.shape), order=1)


def noisy_frame(rows: int, cols: int, dtype: str) -> np.ndarray:
    rng = np.random.default_rng(7)
    top = {'uint8': 250, 'uint16': 60000, 'uint32': 4_000_000_000, 'float32': 1.0}[dtype]
    return (rng.random((rows, cols)) * top).astype(dtype)


@pytest.mark.parametrize(
    'rows, cols, dtype',
    [(1062, 1028, 'uint16'), (1062, 1028, 'uint32'), (531, 514, 'uint8'), (48, 64, 'uint16'), (100, 100, 'float32')],
)
def test_preview_scales_like_scipy(fake, cam, rows, cols, dtype):
    img = noisy_frame(rows, cols, dtype)
    out = io.BytesIO()
    tifffile.imwrite(out, img)
    fake.monitor_tiff = out.getvalue()
    got = cam.get_preview_image()
    want = as_scipy(img)
    assert got.shape == want.shape and got.dtype == want.dtype
    # the outermost rows and columns are compared apart: scipy can read its constant 0 there
    inner_diff = np.abs(got[1:-1, 1:-1].astype(float) - want[1:-1, 1:-1].astype(float))
    if dtype == 'float32':
        assert inner_diff.max() < 1e-5
    else:
        assert inner_diff.max() <= 1
        assert (inner_diff == 0).mean() > 0.999


def test_preview_keeps_the_corners(fake, cam):
    img = noisy_frame(48, 64, 'uint16')
    fake.monitor_tiff = tiff_of(img)
    got, turned = cam.get_preview_image(), orient(img)
    for corner in [(0, 0), (0, -1), (-1, 0), (-1, -1)]:
        assert got[corner] == turned[corner]


@pytest.mark.parametrize('compression', ['tiff_lzw', 'tiff_adobe_deflate'])
def test_compressed_monitor_tiff(fake, cam, compression):
    img = noisy_frame(48, 64, 'uint16')
    fake.monitor_tiff = tiff_of(img, compression=compression)
    np.testing.assert_array_equal(cam._monitor_frame(), orient(img))


def test_undecodable_monitor_warns_once_and_recovers(fake, cam, records):
    fake.monitor_tiff = b'not a tiff'
    for _ in range(2):
        prev = cam.get_preview_image()
        assert prev.shape == (640, 640) and prev.dtype == np.uint16 and not prev.any()
        wait_for_monitor(cam)
    warned = [t for t in records.messages(logging.WARNING) if 'Monitor preview failed' in t]
    assert len(warned) == 1

    fake.monitor_tiff = tiff_of(noisy_frame(48, 64, 'uint16'))
    assert cam.get_preview_image().any()
    wait_for_monitor(cam)
    fake.monitor_tiff = b'not a tiff'
    cam.get_preview_image()
    warned = [t for t in records.messages(logging.WARNING) if 'Monitor preview failed' in t]
    assert len(warned) == 2  # a new episode


def test_session_get_and_put(fake, cam):
    state = f'{cam.DCU_endpoint}/detector/api/1.8.0/status/state'
    r = cam.session.get(state, timeout=2)
    r.raise_for_status()
    assert r.status_code == 200 and r.ok and r.url == state
    assert r.json()['value'] == cam.get_detector_state()
    assert '"value"' in r.text and b'"value"' in r.content

    fake.log.clear()
    r = cam.session.put(f'{cam.DCU_endpoint}/detector/api/1.8.0/config/count_time', json={'value': 0.25}, timeout=2)
    r.raise_for_status()
    assert [q['body'] for q in fake.requests('PUT detector/config/count_time')] == [{'value': 0.25}]
    assert cam.session.get(f'{cam.DCU_endpoint}/detector/api/1.8.0/config/count_time', timeout=2).json()['value'] == 0.25

    # a command without a body, as dectris_menu sends it
    cam.session.put(f'{cam.DCU_endpoint}/detector/api/1.8.0/command/cancel', timeout=2).raise_for_status()


def test_session_http_error(m, fake, cam):
    url = f'{cam.DCU_endpoint}/nothing/here'
    r = cam.session.get(url, timeout=2)
    assert r.status_code == 404 and not r.ok
    with pytest.raises(m.DectrisHTTPError) as e:
        r.raise_for_status()
    assert e.value.status_code == 404 and e.value.url == url


def test_session_sends_the_driver_user_agent(fake, cam):
    from instamatic import __app_title__, __app_version__

    fake.log.clear()
    cam.session.get(f'{cam.DCU_endpoint}/detector/api/1.8.0/status/state', timeout=2)
    assert fake.log[-1]['headers']['user-agent'] == f'{__app_title__}/{__app_version__}'
    assert fake.log[-1]['headers']['accept'] == 'application/json'


def test_interface_loads_the_rust_class(m, monkeypatch):
    from instamatic import config
    from instamatic.camera.camera import get_camera_class

    monkeypatch.setattr(config.settings, 'simulate', False)
    cls = get_camera_class('dectris_simplon')
    assert cls is m.CameraDectris
    assert cls.__module__ == m.__name__
    # [R39] what CamClient._init_dict reads: every public method, both ours and the core's
    for k in ['get_image', 'get_movie', 'establish_connection', 'load_defaults', 'arm', 'set_config', 'stats']:
        assert k in cls.__dict__, k
