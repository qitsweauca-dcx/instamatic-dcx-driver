"""DectrisCore and the CameraDectris adapter with no DCU reachable (DESIGN §2.2:
construction is best-effort and must succeed offline)."""

from __future__ import annotations

import gc
import logging
import pickle
import sys
import time
import types

import numpy as np
import pytest

from .conftest import OFFLINE_DCU, OFFLINE_ZMQ


@pytest.fixture
def core(m):
    c = m.DectrisCore()
    yield c
    c.close()


windows_only = pytest.mark.skipif(sys.platform != 'win32', reason='the 1 ms timer exists on Windows only')


@pytest.fixture
def timer_calls(m):
    """``() -> (begins, ends)`` of the Windows timer since the test began."""
    gc.collect()
    b0, e0 = m._fine_timer_counts()

    def calls():
        b, e = m._fine_timer_counts()
        return b - b0, e - e0

    return calls


def fake_camera_class(offline_yaml):
    from instamatic.camera.super_dectris import CameraDectris

    class OfflineCamera(CameraDectris):
        def load_defaults(self):
            for k, v in offline_yaml.items():
                setattr(self, k, v)

    return OfflineCamera


@windows_only
def test_windows_timer_follows_the_connection(timer_calls, offline_yaml):  # [R9]
    cam = fake_camera_class(offline_yaml)('offline')
    assert timer_calls() == (1, 0)
    cam.establish_connection()  # already held
    assert timer_calls() == (1, 0)
    cam.release_connection()
    cam.release_connection()
    assert timer_calls() == (1, 1)
    cam.establish_connection()
    assert timer_calls() == (2, 1)
    cam.close()
    cam.close()
    assert timer_calls() == (2, 2)


@windows_only
def test_windows_timer_released_when_the_camera_goes(timer_calls, offline_yaml):
    cam = fake_camera_class(offline_yaml)('offline')
    del cam
    gc.collect()
    assert timer_calls() == (1, 1)


@windows_only
def test_windows_timer_opt_out(timer_calls, offline_yaml):
    cam = fake_camera_class(dict(offline_yaml, windows_fine_timer=False))('offline')
    cam.close()
    assert timer_calls() == (0, 0)


def test_camera_base_behaviour(offline_yaml):
    from instamatic.camera.camera_base import CameraBase

    with fake_camera_class(offline_yaml)('offline') as cam:
        assert isinstance(cam, CameraBase)
        assert cam.get_name() == 'offline'
        assert cam.get_camera_dimensions() == [1028, 1062]
    assert cam.get_detector_state() == 'unknown'


def test_load_defaults_reads_the_camera_config(monkeypatch, offline_yaml):
    from instamatic import config
    from instamatic.camera.super_dectris import CameraDectris

    live = types.SimpleNamespace(name='live', interface='dectris_simplon', mapping=offline_yaml)
    monkeypatch.setattr(config, 'camera', live)
    monkeypatch.setattr(config.settings, 'camera', 'live')

    def load(camera_name):
        loaded.append(camera_name)

    loaded = []
    monkeypatch.setattr(config, 'load_camera_config', load)
    cam = CameraDectris()  # [G1-K] the live config, not reloaded
    assert cam.name == 'live' and cam.DCU_host == '127.0.0.1' and loaded == []
    cam.close()
    cam = CameraDectris(name='other')
    assert cam.name == 'other' and loaded == ['other']
    cam.close()


def test_zmq_rcvhwm_key(offline_yaml, caplog):
    with caplog.at_level(logging.WARNING, logger='instamatic.camera.camera_dectris'):
        cam = fake_camera_class(dict(offline_yaml, zmq_rcvhwm='2'))('offline')
        assert cam.ZMQ_RCVHWM == 2
        cam.close()
        cam = fake_camera_class(dict(offline_yaml, zmq_rcvhwm='x'))('offline')
        assert cam.ZMQ_RCVHWM == 500
        cam.close()
    assert "zmq_rcvhwm='x' ignored; using 500" in caplog.text


def test_zmq_rcvbuf_key(offline_yaml, caplog):
    cam = fake_camera_class(offline_yaml)('offline')
    assert cam.zmq_rcvbuf == 4 * 1024 * 1024
    cam.close()
    cam = fake_camera_class(dict(offline_yaml, zmq_rcvbuf='1048576'))('offline')
    assert cam.zmq_rcvbuf == 1048576
    cam.close()
    cam = fake_camera_class(dict(offline_yaml, zmq_rcvbuf=0))('offline')
    assert cam.zmq_rcvbuf == 0
    cam.close()
    with caplog.at_level(logging.WARNING):
        for bad in ('x', -1, 1.5):
            cam = fake_camera_class(dict(offline_yaml, zmq_rcvbuf=bad))('offline')
            assert cam.zmq_rcvbuf == 4 * 1024 * 1024
            cam.close()
    assert "zmq_rcvbuf='x' ignored; keeping 4194304" in caplog.text


def test_session_offline(m, offline_camera):
    # refused at once, or (Windows retries a refused connect) out of time
    with pytest.raises((m.DectrisConnectionError, m.DectrisTimeout)):
        offline_camera.session.get(f'{offline_camera.DCU_endpoint}/detector/api/1.8.0/status/state', timeout=2)
    with pytest.raises(ValueError):
        offline_camera.session.get(offline_camera.DCU_endpoint, timeout=0)


def test_new_ignores_arguments_and_does_no_io(m):
    m.DectrisCore()
    m.DectrisCore('name', name='x')


def test_unconnected_core(m, core):
    assert core.get_detector_state() == 'unknown'
    assert core.get_countrate_correction() is None
    assert core.flush_zmq_buffer() == 0 and core.reconnect_zmq() is False
    assert core.stats.received == 0 and core.last_stop_reason is None
    with pytest.raises(m.DectrisError, match='not connected'):
        core.arm()
    core.disarm()  # no-op
    core.release_connection()
    core.close()


def test_lenient_setters(m, core, caplog):  # [R33]
    with caplog.at_level(logging.WARNING, logger='instamatic.camera.camera_dectris'):
        core.receive_timeout = '5x'
        core.log_frames = 'maybe'
        core.frame_time_policy = 'bogus'
        core.auto_initialize = 'no'
        core.series_attribution = 'fifo'
        core.monitor_max_hz = '5'
        core.default_binning = 2
    assert core.receive_timeout is None and core.log_frames is False
    assert core.frame_time_policy == 'readout' and core.auto_initialize is False
    assert core.series_attribution == 'fifo' and core.monitor_max_hz == 5.0
    assert core.get_binning() == 2
    text = caplog.text
    assert "receive_timeout='5x' ignored; keeping None" in text
    assert "log_frames='maybe' ignored; keeping False" in text
    with pytest.raises(ValueError):
        core.set_trigger_mode('foo')


def test_offline_connect(m, core):
    t = time.monotonic()
    core._connect(OFFLINE_DCU, OFFLINE_ZMQ, 4, 1, [1028, 1062])
    assert time.monotonic() - t <= 3.5  # offline constructor <= 3 s
    assert core.dcu_reachable is False
    assert core.get_detector_state() == 'unknown' and core.get_hv_state() == 'unknown'
    assert core._monitor_frame() is None
    core.trigger_mode = 'exts'
    with pytest.raises(m.DetectorStateError):  # [R23] no HTTP
        core.trigger()
    core.trigger_mode = 'ints'
    core.receive_timeout = 0.2
    it = core.receive_frames(n_frames=2)
    assert type(it).__module__ == m.__name__  # [G1-Q]
    assert list(it) == [] and core.last_stop_reason == 'timeout'
    assert core.message_receiver(100) is None


def test_adapter_shape(m, offline_camera):
    from instamatic.camera.camera_base import CameraBase
    from instamatic.camera.super_dectris import CameraDectris

    cam = offline_camera
    assert all(hasattr(CameraDectris, k) for k in CameraBase.__abstractmethods__)
    assert isinstance(cam, CameraBase) and isinstance(cam, m.DectrisCore)
    assert issubclass(CameraDectris, CameraBase)
    # [R39] every public Rust member is in the subclass dict (CamClient._init_dict)
    missing = [k for k in dir(m.DectrisCore) if not k.startswith('_') and k not in CameraDectris.__dict__]
    assert missing == []
    # dir() + getattr never raise; [G1-Q] the CamServer.get_attrs dict pickles
    values = {k: getattr(cam, k) for k in dir(cam) if not k.startswith('_')}
    pickle.dumps({k: type(v) for k, v in values.items() if not callable(v)})
    assert cam.get_image_dimensions() == (1028, 1062)
    assert cam.ZMQ_RCVHWM == 500 and cam.streamable is True


def test_adapter_preview_offline_is_zeros(offline_camera):  # [G1-J]
    frames = [offline_camera.get_preview_image() for _ in range(3)]
    assert all(f.shape == (640, 640) and f.dtype == np.uint16 and not f.any() for f in frames)


def test_kept_module_names():  # [#12 keep]
    import instamatic.camera.super_dectris as sd

    for n in ['TYPED_ARRAY_DTYPES', 'autoscale', 'bin_ndarray', 'decompress', 'logger',
              'tag_decoders', 'CameraDectris']:  # fmt: skip
        assert hasattr(sd, n), n
    assert sd.logger.name == 'instamatic.camera.camera_dectris'
