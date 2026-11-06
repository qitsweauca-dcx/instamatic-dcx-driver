"""Regressions from a review of the driver before publication: series ownership across threads, the video grabber, Ctrl+C and initialize, stop reasons (DESIGN §8)."""

from __future__ import annotations

import _thread
import gc
import logging
import threading
import time
import weakref

import pytest

from instamatic.camera.videostream import LiveVideoStream, MovieRequest

from .conftest import make_camera
from .test_concurrency import run
from .test_receive import ids


@pytest.fixture
def stream(cam):
    s = LiveVideoStream(cam)
    yield s
    if s.grabber.thread.is_alive():
        s.close()


def test_dropped_movie_iter_releases_the_owner(fake, cam):  # review #1 (py)
    def worker():
        it = cam.get_movie(5, 0.01)
        next(it)  # armed by this thread, then dropped mid-series without close()

    w = run(worker)
    w.join(5)
    assert not w.is_alive() and w.error is None
    gc.collect()
    assert ids(cam.get_image(0.01))[1] == 1  # no DectrisBusy on the main thread


def test_ctrl_c_ends_a_blocked_receive(fake, cam):  # review #2
    fake.frame_period = 5.0  # the first image comes at once, the second 5 s later
    cam.receive_timeout = 10.0
    it = cam.get_movie(2, 0.01)
    next(it)
    timer = threading.Timer(0.3, _thread.interrupt_main)
    timer.start()
    t = time.monotonic()
    with pytest.raises(KeyboardInterrupt):
        next(it)
    assert time.monotonic() - t < 1.5
    it.close()
    fake.frame_period = 0.002
    cam.receive_timeout = 1.0
    assert ids(cam.get_image(0.01))[1] == 1


def test_ctrl_c_in_a_logging_handler_is_not_swallowed(fake, cam, records):  # review #2
    class Interrupting:
        level = 0
        fired = False

        def handle(self, record):
            if not self.fired:
                self.fired = True
                raise KeyboardInterrupt

    lg = logging.getLogger('instamatic.camera.camera_dectris')
    h = Interrupting()
    errors = cam.stats.log_errors  # process-wide counter
    lg.addHandler(h)
    try:
        with pytest.raises(KeyboardInterrupt):
            cam.compression = 'gzip'  # one WARNING record
            for _ in range(200):
                time.sleep(0.005)
    finally:
        lg.removeHandler(h)
    assert h.fired and cam.stats.log_errors == errors  # not counted as a handler error


def test_grabber_survives_a_request_withdrawn_before_it_ran(fake, cam, stream):  # review #3
    stream.grabber.request = None
    stream.grabber.acquireInitiateEvent.set()
    time.sleep(0.3)
    assert stream.grabber.thread.is_alive()
    assert ids(stream.get_image(0.01))[1] == 1


def test_requester_raises_when_the_grabber_is_gone(fake, cam, stream):  # review #3
    stream.close()
    with pytest.raises(RuntimeError, match='grabber has stopped'):
        stream.get_image(0.01)


def test_abandoned_requests_are_not_queued(fake, cam, stream):  # review #4
    stale = MovieRequest(3)
    stream.send_media(object(), request=stale)  # nobody waits for it
    assert len(stream.requested) == 0


def test_concurrent_requesters_do_not_hang(fake, cam, stream):  # review #5
    def many():
        return [ids(stream.get_image(0.01))[1] for _ in range(5)]

    a, b = run(many), run(many)
    a.join(20)
    b.join(20)
    assert not a.is_alive() and not b.is_alive()
    assert a.error is None and b.error is None, (a.error, b.error)
    assert a.result == b.result == [1] * 5


def test_close_before_the_first_frame_is_cancelled(fake, cam):  # review #6
    it = cam.get_movie(5, 0.01)
    it.close()
    assert cam.last_stop_reason == 'cancelled'
    assert list(it) == []
    assert fake.count('PUT detector/command/arm') == 0


def test_close_after_two_of_five_is_cancelled(fake, cam):  # review #6
    it = cam.get_movie(5, 0.01)
    next(it)
    next(it)
    it.close()
    assert cam.last_stop_reason == 'cancelled'
    assert fake.wait_for(lambda: fake.state == 'idle')


def test_full_movie_then_close_keeps_n_frames(fake, cam):  # review #6
    it = cam.get_movie(3, 0.01)
    assert len(list(it)) == 3
    it.close()
    assert cam.last_stop_reason == 'n_frames'


def test_frame_iter_close_wakes_a_blocked_next(fake, cam):  # review #6
    cam.receive_timeout = 5.0
    it = cam.receive_frames(3)
    r = run(lambda: list(it))
    time.sleep(0.3)
    t = time.monotonic()
    it.close()
    r.join(2)
    assert not r.is_alive() and r.error is None and r.result == []
    assert time.monotonic() - t < 1.0
    assert cam.last_stop_reason == 'cancelled'


def test_with_cam_twice(fake, cam):  # review #7
    with cam:
        assert ids(cam.get_image(0.01))[1] == 1
    with cam:
        assert ids(cam.get_image(0.01))[1] == 1


def test_exit_hooks_keep_no_camera_alive(fake):  # review #8
    c = make_camera(fake)
    ref = weakref.ref(c)
    c.release_connection()
    del c
    gc.collect()
    assert ref() is None


def test_mode_setters_normalise_or_warn(fake, cam, records):  # review #9
    cam.trigger_mode = ' INTE '
    assert cam.trigger_mode == 'inte'
    cam.trigger_mode = 'bogus'
    assert cam.trigger_mode == 'inte'
    cam.compression = 'LZ4'
    assert cam.compression == 'lz4'
    cam.compression = 'gzip'
    assert cam.compression == 'lz4'
    warnings = records.messages(30)
    assert any('trigger_mode=' in w and 'bogus' in w for w in warnings), warnings
    assert any('compression=' in w and 'gzip' in w for w in warnings), warnings
    cam.trigger_mode = 'ints'
    cam.compression = 'bslz4'
