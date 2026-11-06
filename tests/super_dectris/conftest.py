"""Tier-2 tests of the Rust Dectris driver (docs/DESIGN.md §8).

They need the built extension (``rust/build.ps1``); without it they are
skipped.
"""

from __future__ import annotations

import logging
from pathlib import Path

import numpy as np
import pytest

core = pytest.importorskip('instamatic.camera.super_dectris._super_dectris')

REPO = Path(__file__).resolve().parents[2]
VECTORS = REPO / 'src/instamatic/camera/super_dectris/rust/crates/dectris_codec/tests/vectors'

# An address nothing listens on: connect fails at once (offline constructor paths).
OFFLINE_DCU = 'http://127.0.0.1:1'
OFFLINE_ZMQ = 'tcp://127.0.0.1:5'


def pytest_configure(config):
    config.addinivalue_line('markers', 'slow: soak test, runs only with SUPER_DECTRIS_SLOW=1')


@pytest.fixture
def m():
    return core


@pytest.fixture
def dectris_config(monkeypatch):
    """The loaded camera config says Dectris, whichever camera the settings name."""
    from instamatic import config

    monkeypatch.setattr(config.camera, 'interface', 'dectris_simplon', raising=False)
    monkeypatch.delattr(config.camera, 'saved_frame_rot90', raising=False)
    return config.camera


@pytest.fixture
def offline_yaml():
    return dict(
        DCU_host='127.0.0.1',
        DCU_port=1,
        ZMQ_host='127.0.0.1',
        ZMQ_port=5,
        dimensions=[1028, 1062],
        default_binsize=1,
        default_exposure=0.05,
        trigger_mode='ints',
        interface='dectris_simplon',
    )


@pytest.fixture
def offline_camera(offline_yaml):
    """A ``CameraDectris`` whose yaml is ``offline_yaml`` (no config files read)."""
    from instamatic.camera.super_dectris import CameraDectris

    class OfflineCamera(CameraDectris):
        def load_defaults(self):
            for k, v in offline_yaml.items():
                setattr(self, k, v)

    cam = OfflineCamera('offline')
    yield cam
    cam.close()


def orient(img):
    """The live-view orientation: fliplr -> flipud -> rot90(k=-1) (PROTOCOL §11)."""
    return np.rot90(np.flipud(np.fliplr(img)), k=-1)


def fake_yaml(fake, **extra) -> dict:
    yaml = dict(
        DCU_host=fake.host,
        DCU_port=fake.port,
        ZMQ_host='127.0.0.1',
        ZMQ_port=fake.zmq_port,
        dimensions=[1028, 1062],
        default_binsize=1,
        default_exposure=0.01,
        trigger_mode='ints',
        interface='dectris_simplon',
        receive_timeout=1.0,
    )
    yaml.update(extra)
    return yaml


def make_camera(fake, **extra):
    """A ``CameraDectris`` on ``fake``; no config files are read or re-pointed [G1-K]."""
    from instamatic.camera.super_dectris import CameraDectris

    yaml = fake_yaml(fake, **extra)

    class FakeCamera(CameraDectris):
        def load_defaults(self):
            for k, v in yaml.items():
                setattr(self, k, v)

    return FakeCamera('fake')


@pytest.fixture
def fake():
    from .fake_dcu import FakeDCU

    f = FakeDCU()
    yield f
    f.close()


@pytest.fixture
def cam(fake):
    c = make_camera(fake)
    fake.log.clear()
    yield c
    c.release_connection()  # never closes logging (DESIGN §8) [G1-C]


class Records(logging.Handler):
    """Collects records of the driver logger (and its children)."""

    def __init__(self):
        super().__init__(logging.DEBUG)
        self.records: list[logging.LogRecord] = []

    def emit(self, record):
        self.records.append(record)

    def messages(self, level=logging.DEBUG) -> list[str]:
        return [r.getMessage() for r in self.records if r.levelno >= level]


@pytest.fixture
def records():
    lg = logging.getLogger('instamatic.camera.camera_dectris')
    h = Records()
    old = lg.level
    lg.addHandler(h)
    lg.setLevel(logging.DEBUG)
    core.reset_log_cache()  # pyo3-log caches levels (DESIGN §2.9)
    yield h
    lg.removeHandler(h)
    lg.setLevel(old)
    core.reset_log_cache()
