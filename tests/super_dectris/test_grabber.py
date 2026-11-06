"""``LiveVideoStream``/``MediaGrabber`` with the Rust driver (DESIGN §3.6 [R7][R34][G1-F][G1-S])."""

from __future__ import annotations

import threading
import time

import numpy as np
import pytest

from instamatic.camera.videostream import LiveVideoStream

from .test_concurrency import run
from .test_receive import ids


@pytest.fixture
def stream(cam):
    s = LiveVideoStream(cam)
    yield s
    if s.grabber.thread.is_alive():
        s.close()


def test_get_image_and_movie(fake, cam, stream):
    assert ids(stream.get_image(0.01)) == (1, 1)
    assert [ids(f) for f in stream.get_movie(3, 0.01)] == [(2, 1), (2, 2), (2, 3)]


def test_r7_early_close_disarms_once(fake, cam, stream):  # [R7] controller pattern
    fake.frame_period = 0.2  # 5 frames take ~1 s: the close lands mid-series
    gen = stream.get_movie(5, 0.01)
    for i, _ in enumerate(gen):
        if i == 1:
            break
    gen.close()
    assert fake.wait_for(lambda: fake.state == 'idle')
    assert fake.wait_for(lambda: fake.count('PUT detector/command/disarm') == 1)
    time.sleep(0.3)  # the grabber's close() / the n-th frame never disarm a second time
    assert fake.count('PUT detector/command/disarm') == 1
    assert cam.last_stop_reason == 'cancelled'  # the grabber stopped the camera movie early
    fake.frame_period = 0.002
    img = stream.get_image(0.01)  # [R34] never a stale movie item
    assert ids(img) == (2, 1)


def test_movie_stepped_in_a_worker_thread(fake, cam, stream):  # ctrl.get_movie's pattern
    # the controller calls next() in its executor and close() from the caller's thread, so the
    # requester lock must be one that another thread can release
    from concurrent.futures import ThreadPoolExecutor

    gen = stream.get_movie(3, 0.01)
    with ThreadPoolExecutor(max_workers=1) as ex:
        frames = [ex.submit(lambda: next(gen)).result() for _ in range(3)]
    gen.close()
    assert [ids(f) for f in frames] == [(1, 1), (1, 2), (1, 3)]
    t = run(stream.get_image, 0.01)  # on a thread: with the lock still held it would hang
    t.join(5)
    assert not t.is_alive() and t.error is None, t.error
    assert ids(t.result) == (2, 1)


def test_r34_g1f_exception_reaches_caller(m, fake, cam, stream):  # [R34][G1-F]
    fake.end_after = 2
    with pytest.raises(m.DectrisTimeout):
        list(stream.get_movie(5, 0.01))
    fake.end_after = None
    fake.fail['detector/command/arm'] = 500
    with pytest.raises(m.DectrisError):
        stream.get_image(0.01)
    del fake.fail['detector/command/arm']
    assert stream.grabber.thread.is_alive()
    assert ids(stream.get_image(0.01))[1] == 1


class ShortMovieCamera:
    """A non-Dectris camera whose ``get_movie`` stops early."""

    streamable = True
    default_exposure = 0.01
    default_binsize = 1
    dimensions = (4, 4)
    name = 'short'

    def get_image(self, exposure=None, binsize=None, **kw):
        return np.zeros((4, 4))

    def get_movie(self, n_frames, exposure=None, binsize=None, **kw):
        yield from (np.ones((4, 4)) for _ in range(2))


def test_g1f_short_movie_non_dectris():  # [G1-F] RuntimeError reaches the caller
    s = LiveVideoStream(ShortMovieCamera())
    try:
        with pytest.raises(RuntimeError, match='2 of 5'):
            list(s.get_movie(5))
        assert s.get_image().shape == (4, 4)
    finally:
        s.close()


def test_g1s_stop_during_held_initialize(fake, cam, stream):  # [G1-S]
    cam.auto_initialize = True
    fake.init_delay = 3.0
    fake.state = 'na'
    req = run(stream.get_image, 0.01)
    assert fake.wait_for(lambda: fake.state == 'initialize')
    t = time.monotonic()
    stream.close()
    assert time.monotonic() - t < 1.0
    req.join(2)
    assert not req.is_alive() and isinstance(req.error, Exception)
    assert threading.active_count() < 50
