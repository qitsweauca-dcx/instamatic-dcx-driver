"""Receive paths and REST policy against the fake DCU (DESIGN §8 Tier 2)."""

from __future__ import annotations

import logging
import time

import numpy as np
import pytest

from .conftest import make_camera
from .fake_dcu import tiff_bytes


def ids(frame) -> tuple[int, int]:
    """(series_id, image_id) encoded in a ``FakeDCU.default_frame``."""
    v = int(frame.flat[0])
    return v // 1_000_000, v % 1_000_000


def puts(fake, prefix='detector/config/'):
    return [(r['path'][len(prefix) :], r['body']['value']) for r in fake.requests() if r['method'] == 'PUT' and r['path'].startswith(prefix)]


def test_get_image_round_trip(fake, cam):
    img = cam.get_image(0.01)
    assert img.dtype == np.uint32 and img.shape == (fake.rows, fake.cols)
    np.testing.assert_array_equal(img, fake.default_frame(1, 1))
    assert fake.count('PUT detector/command/arm') == 1 and fake.count('PUT detector/command/trigger') == 1


def test_get_image_binning(fake, cam):
    img = cam.get_image(0.01, binning=2)
    ref = fake.default_frame(1, 1).astype(np.float64).reshape(fake.rows // 2, 2, fake.cols // 2, 2).mean((1, 3))
    np.testing.assert_array_equal(img, ref)


def test_r13_consecutive_get_image(fake, cam):  # [R13]
    for i in range(1, 11):
        assert ids(cam.get_image(0.01)) == (i, 1)
    assert cam.stats.timeouts == 0


def test_movie_ints(fake, cam):
    frames = list(cam.get_movie(5, 0.01))
    assert [ids(f) for f in frames] == [(1, i) for i in range(1, 6)]
    assert ('nimages', 5) in puts(fake) and ('ntrigger', 1) in puts(fake)
    assert fake.count('PUT detector/command/trigger') == 1


def test_movie_inte_g2b(fake, cam):  # [G2-B] nimages=1, ntrigger=n, one count_time body per trigger
    cam.set_trigger_mode('inte')
    frames = list(cam.get_movie(5, 0.01))
    assert [ids(f) for f in frames] == [(1, i) for i in range(1, 6)]
    assert ('nimages', 1) in puts(fake) and ('ntrigger', 5) in puts(fake)
    assert [r['body'] for r in fake.requests('PUT detector/command/trigger')] == [{'value': 0.01}] * 5


def test_frames_with_metadata_and_start(fake, cam):
    cam.set_config(3, 0.01)
    cam.arm()
    cam.trigger()
    out = list(cam.receive_frames_with_metadata(3))
    assert [m['image_id'] for _, m in out] == [1, 2, 3]
    assert out[0][1] == {'series_id': 1, 'image_id': 1, 'count_time': 0.01, 'real_time': [100000, 10000000]}
    start = cam._stream_start_config
    assert start['type'] == 'start' and start['series_id'] == 1 and start['number_of_images'] == 3
    assert start['arm_date'] == '2026-10-21T01:02:03.000000+00:00'  # tag 0 -> its text


def test_raw_frames_identity(fake, cam):
    cam.set_config(2, 0.01)
    cam.arm()
    cam.trigger()
    raw = list(cam.receive_raw_frames(2))
    sent = [b for k, b in fake.sent if k == 'image']
    assert [b for b, _ in raw] == sent
    assert [m['image_id'] for _, m in raw] == [1, 2]
    assert all(type(b) is bytes for b, _ in raw)


def test_no_aliasing(fake, cam):
    cam.set_config(4, 0.01)
    cam.arm()
    cam.trigger()
    frames = list(cam.receive_frames(4))
    for i, a in enumerate(frames):
        assert a.flags.owndata or a.base is None or not any(np.shares_memory(a, b) for b in frames[i + 1 :])
    copy = [f.copy() for f in frames]
    frames[0][:] = 0
    for a, b in zip(frames[1:], copy[1:]):
        np.testing.assert_array_equal(a, b)
    assert [ids(f)[1] for f in copy] == [1, 2, 3, 4]


def shape_changing_frames(fake) -> list[np.ndarray]:
    """Each is a different output from the array allocated for it from the frame before [R9]."""
    frames = [
        np.full((8, 6), 1, np.uint16),
        np.full((8, 6), 2, np.uint16),  # the same shape: decoded in one detach
        np.full((16, 12), 3, np.uint16),  # bigger than the array allocated for it
        np.full((16, 12), 4, np.uint32),  # the same dimensions, wider elements
        np.full((4, 3), 5, np.uint32),  # smaller
        np.full((6, 4), 6, np.uint16),  # the same byte length as the one before
        np.full((6, 4), 7, np.uint16),
    ]
    fake.frame_fn = lambda series_id, image_id: frames[image_id - 1]
    return frames


def assert_same_frames(got, frames):
    assert len(got) == len(frames)
    for a, b in zip(got, frames):
        assert a.dtype == b.dtype
        np.testing.assert_array_equal(a, b)


def test_shape_change_within_a_series_frames(fake, cam):
    frames = shape_changing_frames(fake)
    cam.set_config(len(frames), 0.01)
    cam.arm()
    cam.trigger()
    assert_same_frames(list(cam.receive_frames(len(frames))), frames)


def test_shape_change_within_a_series_metadata(fake, cam):
    frames = shape_changing_frames(fake)
    cam.set_config(len(frames), 0.01)
    cam.arm()
    cam.trigger()
    out = list(cam.receive_frames_with_metadata(len(frames)))
    assert_same_frames([a for a, _ in out], frames)
    assert [meta['image_id'] for _, meta in out] == list(range(1, len(frames) + 1))


def test_shape_change_within_a_movie(fake, cam):
    frames = shape_changing_frames(fake)
    assert_same_frames(list(cam.get_movie(len(frames), 0.01)), frames)


def test_binning_in_the_one_detach_path(fake, cam):
    cam.set_config(3, 0.01)
    cam.arm()
    cam.trigger()
    got = list(cam.receive_frames(3, binning=2))
    for i, a in enumerate(got, start=1):
        ref = fake.default_frame(1, i).astype(np.float64).reshape(fake.rows // 2, 2, fake.cols // 2, 2).mean((1, 3))
        np.testing.assert_array_equal(a, ref)


def test_bad_image_after_good_ones_raises_and_the_next_decodes(m, fake, cam):
    good = fake.image_message

    def message(series_id, unique, image_id):
        msg = good(series_id, unique, image_id)
        if image_id == 2:  # one pixel short for its dimensions
            arr = msg['data']['threshold_1']
            arr.value[1] = type(arr.value[1])(arr.value[1].tag, arr.value[1].value[:-4])
        return msg

    fake.image_message = message
    cam.set_config(3, 0.01)
    cam.arm()
    cam.trigger()
    it = cam.receive_frames(3)
    np.testing.assert_array_equal(next(it), fake.default_frame(1, 1))
    with pytest.raises(m.StreamDecodeError):
        next(it)
    np.testing.assert_array_equal(next(it), fake.default_frame(1, 3))


def test_arm_body_variants(fake, cam):  # 'sequence id', 'sequence_id', {} (FIFO fallback)
    for body in ['sequence id', 'sequence_id', None]:
        fake.arm_body = body
        a = cam.get_image(0.01)
        assert ids(a)[1] == 1 and ids(a)[0] == fake.seq


def test_r22_flush_keeps_armed_start(fake, cam):  # [R22]
    cam.set_config(1, 0.01)
    cam.arm()
    assert fake.wait_for(lambda: any(k == 'start' for k, _ in fake.sent))
    time.sleep(0.1)
    cam.flush_zmq_buffer()
    cam.trigger()
    frames = list(cam.receive_frames_with_metadata(1))
    assert len(frames) == 1 and frames[0][1]['count_time'] == 0.01
    assert cam._stream_start_config['count_time'] == 0.01


def test_r12_stale_series_dropped(fake, cam):  # [R12]
    real = fake.frame_fn
    fake.frame_fn = lambda s, i: np.zeros((fake.rows, fake.cols), fake.dtype)
    fake.send_end(41, 'old')  # stale end before the new start: not terminal
    fake.send_start(42, 'old')  # stale start + image: dropped, start not cached
    fake.send_image(42, 'old', 1)
    fake.frame_fn = real
    img = cam.get_image(0.02)
    assert ids(img) == (1, 1)
    assert cam._stream_start_config['series_id'] == 1 and cam._stream_start_config['count_time'] == 0.02
    assert cam.stats.stale >= 3


def test_g2h_stale_same_id_before_start(fake, cam):  # [G2-H]
    real = fake.frame_fn
    fake.frame_fn = lambda s, i: np.zeros((fake.rows, fake.cols), fake.dtype)
    fake.send_image(1, 'old', 1)
    fake.send_end(1, 'old')
    fake.frame_fn = real
    assert ids(cam.get_image(0.01)) == (1, 1)


def test_g2d_attribution_self_check(m, fake, cam, records):  # [G2-D]
    fake.start_id_offset = 1
    cam.receive_timeout = 0.5
    with pytest.raises(m.DectrisTimeout):
        cam.get_image(0.01)
    errors = [m for m in records.messages(logging.ERROR) if 'series_attribution: fifo' in m]
    assert len(errors) == 1 and cam.stats.attribution_mismatch == 1
    cam.series_attribution = 'fifo'
    assert len(list(cam.get_movie(3, 0.01))) == 3


def test_r14_last_image_after_disarm(fake, cam):  # [R14]
    fake.defer_last = True
    assert [ids(f)[1] for f in cam.get_movie(5, 0.01)] == [1, 2, 3, 4, 5]
    assert ids(cam.get_image(0.01)) == (2, 1)


def test_r24_single_frame_fresh(fake, cam):  # [R24] newest queued image, older ones stale
    cam.single_frame_fresh = True
    cam.set_config(99999, 0.01)
    cam.arm()
    a = cam.acquire_single_frame()
    time.sleep(0.3)
    b = cam.acquire_single_frame()
    assert ids(b)[1] - ids(a)[1] >= 3 and cam.stats.stale >= 2
    cam.disarm()


def test_r3_one_trigger_for_200_single_frames(fake, cam):  # [R3]
    cam.set_trigger_mode('ints')
    cam.set_config(nimages=99999, exposure=0.01)
    cam.arm()
    got = [ids(cam.acquire_single_frame())[1] for _ in range(200)]
    assert got == sorted(got) and len(set(got)) == 200
    assert fake.count('PUT detector/command/trigger') == 1
    cam.disarm()


def test_g2c_frame_time_reject_model(fake, cam, records):  # [G2-C]
    fake.ft_reject = True
    cam.get_image(1.0)
    records.records.clear()
    cam.get_image(0.1)
    warnings = records.messages(logging.WARNING)
    assert any('frame_time rejected' in w for w in warnings)
    assert fake.detector['count_time'] == 0.1 and abs(fake.detector['frame_time'] - 0.104) < 1e-9


def test_g1d_ensure_config_cache(fake, cam):  # [G1-D]
    cam.ensure_config(1, 0.01)
    n = len(puts(fake))
    cam.ensure_config(1, 0.01)
    assert len(puts(fake)) == n  # cache hit
    cam.set_config(2, 0.02)
    fake.log.clear()
    cam.ensure_config(1, 0.01)
    assert ('nimages', 1) in puts(fake) and ('count_time', 0.01) in puts(fake)
    fake.fail['detector/config/nimages'] = 500  # a failing set_config also invalidates the cache
    with pytest.raises(Exception):
        cam.set_config(3, 0.01)
    del fake.fail['detector/config/nimages']
    fake.log.clear()
    cam.ensure_config(1, 0.01)
    assert ('nimages', 1) in puts(fake)


def test_r16_na_initializes_before_config(fake, cam):  # [R16]
    cam.auto_initialize = True
    cam.get_image(0.01)
    fake.state = 'na'
    fake.log.clear()
    assert ids(cam.get_image(0.01))[1] == 1
    paths = [f"{r['method']} {r['path']}" for r in fake.requests()]
    i_init = paths.index('PUT detector/command/initialize')
    i_cfg = paths.index('PUT detector/config/count_time')
    i_arm = paths.index('PUT detector/command/arm')
    assert i_init < i_cfg < i_arm


def test_r23_exts_trigger_no_http(m, fake, cam):  # [R23]
    cam.trigger_mode = 'exts'
    fake.log.clear()
    with pytest.raises(m.DetectorStateError):
        cam.trigger()
    assert fake.requests() == []


def test_r4_trigger_after_cancel_is_skipped(fake, cam):  # [R4]
    cam.set_trigger_mode('inte')
    cam.set_config(5, 0.01)
    cam.arm()
    cam.cancel()
    fake.log.clear()
    t = time.monotonic()
    assert cam.trigger(count_time=0.1) is None
    assert time.monotonic() - t < 0.25  # skipped, not a 0.1 s+ trigger round trip
    assert fake.count('PUT detector/command/trigger') == 0 and cam.stats.triggers_skipped == 1


def test_g1f_short_movie_times_out(m, fake, cam):  # [G1-F] GUI Disarm: end after 2 of 5 images
    fake.end_after = 2
    with pytest.raises(m.DectrisTimeout, match='2 of 5'):
        list(cam.get_movie(5, 0.01))
    fake.end_after = None
    assert ids(cam.get_image(0.01))[1] == 1


def test_monitor_preview(fake, cam):
    from .conftest import orient

    img = (np.arange(48 * 64, dtype=np.uint16) * 3).reshape(48, 64)
    fake.monitor_tiff = tiff_bytes(img)
    frame = cam._monitor_frame()
    np.testing.assert_array_equal(frame, orient(img))
    prev = cam.get_preview_image()
    assert max(prev.shape) == 640
    fake.monitor_tiff = None  # 408: the cached frame is served
    time.sleep(1.0 / cam.monitor_max_hz + 0.05)
    np.testing.assert_array_equal(cam._monitor_frame(), orient(img))


def test_filewriter_download(fake, cam, tmp_path):
    fake.files = {'series_1_master.h5': b'master', 'series_1_data_000001.h5': bytes(range(256)) * 40}
    assert sorted(cam.get_filewriter_files()) == sorted(fake.files)
    paths = cam.download_filewriter_files(tmp_path)
    assert sorted(p.name for p in paths) == sorted(fake.files)
    for p in paths:
        assert p.read_bytes() == fake.files[p.name]


def test_g1c_two_instances(fake, cam, records):  # [G1-C]
    from .fake_dcu import FakeDCU

    other = FakeDCU()
    try:
        cam2 = make_camera(other)
        cam.close()
        records.records.clear()
        assert ids(cam2.get_image(0.01)) == (1, 1)
        assert records.records, 'the second instance must still log after the first closed'
        cam2.release_connection()
    finally:
        other.close()
