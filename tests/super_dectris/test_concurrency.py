"""Locking, logging and cross-thread regressions against the fake DCU (DESIGN §8)."""

from __future__ import annotations

import logging
import os
import queue
import random
import re
import threading
import time

import pytest

from .test_receive import ids


def run(fn, *args):
    """Start ``fn`` on a thread; ``.result``/``.error`` hold its outcome."""
    t = threading.Thread(target=lambda: _capture(t, fn, *args), daemon=True)
    t.result = t.error = None
    t.start()
    return t


def _capture(t, fn, *args):
    try:
        t.result = fn(*args)
    except BaseException as e:  # noqa: BLE001
        t.error = e


def test_r1_g1a_no_foreign_records_or_attaches(m, fake, cam):  # [R1][G1-A]
    seen = []

    class Root(logging.Handler):
        def emit(self, r):
            seen.append((r.name, r.threadName))

    root = logging.getLogger()
    h, old = Root(logging.DEBUG), root.level
    root.addHandler(h)
    root.setLevel(logging.DEBUG)
    m.reset_log_cache()
    allowed0, foreign0 = m._test_attach_counts()
    try:
        for _ in range(100):
            cam.get_detector_state()
        cam.trigger_blocking = False
        cam.set_config(10, 0.01)
        cam.arm()
        for _ in range(10):
            cam.trigger()
        cam.disarm()
    finally:
        root.removeHandler(h)
        root.setLevel(old)
        m.reset_log_cache()
    assert not [n for n, _ in seen if n.startswith(('ureq', 'zmq', 'rustls', 'hyper'))]
    assert {t for n, t in seen if n.startswith('instamatic.camera.camera_dectris')} == {'MainThread'}
    assert m._test_attach_counts()[1] == foreign0 == 0
    assert m._test_attach_counts()[0] > allowed0


def lock_names(cam) -> list[str]:
    with pytest.raises(ValueError) as e:
        cam._test_hold_lock('no-such-lock', 0.0)
    names = re.findall(r'[A-Za-z_]+', str(e.value).split('known:', 1)[1])
    assert 'pending' in names and 'settings' in names and 'zmq' in names
    return names


def test_r2_no_deadlock_while_a_lock_is_held(fake, cam, records):  # [R2]
    hold = 0.3
    for name in lock_names(cam):
        a = run(cam._test_hold_lock, name, hold, True)
        time.sleep(0.05)
        t = time.monotonic()
        cam.get_detector_state()
        cam.get_hv_state()
        cam.get_countrate_correction()
        cam.get_stream_mode()
        cam.set_trigger_mode('ints')
        cam.receive_timeout = 1.0
        cam.arm()
        cam.disarm()
        assert time.monotonic() - t < hold + 0.5, name  # waits at most once for the held lock
        a.join(5)
        assert not a.is_alive() and a.error is None, (name, a.error)


class TkQueueHandler(logging.Handler):
    """Log-panel-style handler whose widget calls are marshalled to a fake Tk
    thread: a record from any other thread waits until that thread has inserted it, like a
    tkinter call made off the main loop."""

    def __init__(self):
        super().__init__(logging.DEBUG)
        self.q = queue.Queue()
        self.lines: list[str] = []
        self.tk_ident = None
        self.stuck: list[str] = []

    def emit(self, record):
        msg = record.getMessage()
        if threading.get_ident() == self.tk_ident:
            self.lines.append(msg)
            return
        done = threading.Event()
        self.q.put((msg, done))
        if not done.wait(5):
            self.stuck.append(msg)


def test_r2_tk_queue_handler(m, fake, cam, records):  # [R2] variant: marshalling handler, Tk-polled getters
    tk = TkQueueHandler()
    lg = logging.getLogger('instamatic.camera.camera_dectris')
    lg.addHandler(tk)
    m.reset_log_cache()
    stop = threading.Event()
    slow = []

    def mainloop():  # the Tk thread: service queued widget calls, poll the getters like the GUI's after()
        tk.tk_ident = threading.get_ident()
        while not stop.is_set():
            while True:
                try:
                    msg, done = tk.q.get_nowait()
                except queue.Empty:
                    break
                tk.lines.append(msg)
                done.set()
            t = time.monotonic()
            cam.get_detector_state()
            cam.get_hv_state()
            cam.get_countrate_correction()
            cam.get_stream_mode()
            if time.monotonic() - t > 0.3 + 0.5:
                slow.append(time.monotonic() - t)
            time.sleep(0.02)

    loop = run(mainloop)
    hold = 0.3
    try:
        assert fake.wait_for(lambda: tk.tk_ident is not None)
        for name in lock_names(cam):
            n = len(tk.lines)
            a = run(cam._test_hold_lock, name, hold, True)
            time.sleep(0.05)
            t = time.monotonic()
            cam.set_trigger_mode('ints')
            cam.receive_timeout = 1.0
            cam.arm()
            cam.disarm()
            assert time.monotonic() - t < hold + 0.5 + 0.5, name
            a.join(5)
            assert not a.is_alive() and a.error is None, (name, a.error)
            assert fake.wait_for(lambda: len(tk.lines) > n), name  # records reached the Tk thread
    finally:
        stop.set()
        loop.join(5)
        lg.removeHandler(tk)
    assert not loop.is_alive() and loop.error is None, loop.error
    assert tk.stuck == [] and slow == []


def test_r5_flush_while_busy(fake, cam, records):  # [R5]
    fake.frame_period = 0.01
    cam.set_config(50, 0.01)
    cam.arm()
    trig = run(cam.trigger)
    reader = run(lambda: list(cam.receive_frames(50)))
    assert fake.wait_for(lambda: sum(k == 'image' for k, _ in fake.sent) >= 3)
    flushes = [cam.flush_zmq_buffer() for _ in range(20)]
    assert cam.reconnect_zmq() is False
    reader.join(10)
    trig.join(10)
    assert reader.error is None and [ids(f)[1] for f in reader.result] == list(range(1, 51))
    assert flushes == [0] * 20
    assert any('flush' in w.lower() for w in records.messages(logging.WARNING))


def test_r5_flush_after_break_drains(fake, cam):  # cRED: flush on the same thread, iterator still referenced
    cam.set_config(5, 0.01)
    cam.arm()
    cam.trigger()
    frame_iter = cam.receive_frames(5)
    next(frame_iter)
    n = cam.flush_zmq_buffer()
    assert n >= 0 and cam.stats.flushed >= n
    cam.disarm()
    assert ids(cam.get_image(0.01))[1] == 1
    del frame_iter


def test_g1e_busy_from_other_thread(m, fake, cam):  # [G1-E]
    ready, done = threading.Event(), threading.Event()
    got = []

    def cred():
        cam.set_config(100, 0.01)
        cam.arm()
        tr = run(cam.trigger)
        it = cam.receive_frames(100)
        for _ in range(3):
            got.append(next(it))
        ready.set()
        done.wait(10)
        for f in it:
            got.append(f)
        tr.join(10)
        cam.disarm()

    fake.frame_period = 0.005
    a = run(cred)
    assert ready.wait(10)
    n = len(fake.requests())
    with pytest.raises(m.DectrisBusy):
        cam.get_image(0.01)
    with pytest.raises(m.DectrisBusy):
        list(cam.get_movie(3, 0.01))
    assert len(fake.requests()) == n
    done.set()
    a.join(20)
    assert a.error is None and [ids(f)[1] for f in got] == list(range(1, 101))
    assert ids(cam.get_image(0.01))[1] == 1


def test_r8_raising_handler(m, fake, cam):  # [R8]
    class Boom(logging.Handler):
        def emit(self, record):
            raise RuntimeError('TclError-style failure')

    lg = logging.getLogger('instamatic.camera.camera_dectris')
    h, old = Boom(logging.DEBUG), lg.level
    lg.addHandler(h)
    lg.setLevel(logging.DEBUG)
    m.reset_log_cache()
    try:
        assert ids(cam.get_image(0.01)) == (1, 1)  # no SystemError
    finally:
        lg.removeHandler(h)
        lg.setLevel(old)
        m.reset_log_cache()
    assert cam.stats.log_errors > 0


def test_q18_cancel_unblocks_trigger(fake, cam):  # [R4]/Q18
    fake.trigger_hold = 'release'
    cam.set_config(1, 0.01)
    cam.arm()
    tr = run(cam.trigger)
    assert fake.wait_for(lambda: fake.count('PUT detector/command/trigger') == 1)
    t = time.monotonic()
    cam.cancel()
    tr.join(2)
    assert not tr.is_alive() and time.monotonic() - t < 1.0 and tr.error is None
    fake.trigger_hold = None
    cam.disarm()
    assert ids(cam.get_image(0.01))[1] == 1


def g1b_trigger_cancel_race(fake, cam, n: int) -> list[int]:
    fake.trigger_hold = 'release'
    cam.set_config(1, 0.01)
    rng = random.Random(7)
    slow = []
    for i in range(n):
        cam.arm()
        t = time.monotonic()
        tr = run(cam.trigger)
        end = time.perf_counter() + rng.uniform(0, 100e-6)
        while time.perf_counter() < end:
            pass
        cam.cancel()
        tr.join(2)
        if tr.is_alive() or time.monotonic() - t >= 1.0:
            slow.append(i)
        cam.disarm()
        cam.flush_zmq_buffer()  # nobody reads the series messages: a full PUSH queue stalls the fake 2 s per send
    return slow


def test_g1b_trigger_cancel_race(fake, cam):  # [G1-B] 200 iterations; the design's 10 000 is the slow soak below
    assert g1b_trigger_cancel_race(fake, cam, 200) == []


@pytest.mark.slow
@pytest.mark.skipif(not os.environ.get('SUPER_DECTRIS_SLOW'), reason='soak: set SUPER_DECTRIS_SLOW=1')
def test_g1b_trigger_cancel_race_soak(fake, cam):  # [G1-B] at the design's 10 000 iterations
    assert g1b_trigger_cancel_race(fake, cam, 10_000) == []


def test_g1h_g2a_initialize_held(fake, cam):  # [G1-H][G2-A]
    fake.init_delay = 1.5
    cam.cancel()
    init = run(cam.initialize)
    assert fake.wait_for(lambda: fake.state == 'initialize')
    time.sleep(0.05)
    n = len(fake.requests())
    t = time.monotonic()
    states = []
    while time.monotonic() - t < 0.5:  # Tk-style polls of the four getters
        states.append(cam.get_detector_state())
        cam.get_hv_state()
        cam.get_countrate_correction()
        cam.get_stream_mode()
        time.sleep(0.05)
    assert set(states) == {'initialize'} and len(fake.requests()) == n
    init.join(5)
    assert init.error is None
    after = [f"{r['method']} {r['path']}" for r in fake.requests()[n:]]
    assert 'PUT stream/config/mode' in after  # enable_stream_v2 after the PUT completes
    assert ids(cam.get_image(0.01))[1] == 1
