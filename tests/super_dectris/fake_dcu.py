"""A fake Dectris DCU for Tier-2 tests: SIMPLON 1.8 REST (http.server) plus a
Stream V2 ZeroMQ PUSH publisher (pyzmq + cbor2), written from the protocol
spec (docs/PROTOCOL.md) only.

State machine: ``idle --arm--> ready --trigger--> acquire --(series done)-->
ready | idle``; ``disarm``/``abort``/``cancel`` -> ``idle``; ``initialize``
-> ``initialize`` for ``init_delay`` -> ``idle`` (sequence id reset, stream
mode ``disabled``). ``arm`` publishes the start, every trigger publishes its
images (ints: ``nimages``; inte/exte: 1), the end of the series (all
``ntrigger`` triggers done, or a disarm/abort/cancel of an armed series)
publishes the end.
"""

from __future__ import annotations

import io
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Callable, Optional

import cbor2
import numpy as np
import zmq

API = '/api/1.8.0/'


def short(path: str) -> str:
    """``/detector/api/1.8.0/command/arm`` -> ``detector/command/arm``."""
    return path.lstrip('/').replace(API.lstrip('/'), '', 1)


class FakeDCU:
    def __init__(self, rows: int = 16, cols: int = 24, dtype: str = '<u4', frame_period: float = 0.002, sndhwm: int = 1000):
        self.rows, self.cols, self.dtype = rows, cols, np.dtype(dtype)
        self.frame_period = frame_period
        self.cv = threading.Condition()
        self.log: list[dict[str, Any]] = []
        self.state = 'idle'
        self.hv = 'READY'
        self.detector: dict[str, Any] = {
            'count_time': 0.5, 'frame_time': 0.504, 'detector_readout_time': 0.004, 'nimages': 1,
            'ntrigger': 1, 'trigger_mode': 'ints', 'compression': 'bslz4', 'x_pixels_in_detector': 1028,
            'y_pixels_in_detector': 1062, 'bit_depth_image': 32, 'countrate_correction_applied': True,
        }  # fmt: skip
        self.stream = {'mode': 'enabled', 'format': 'cbor', 'header_detail': 'basic'}
        self.monitor = {'mode': 'disabled'}
        self.filewriter: dict[str, Any] = {'mode': 'disabled'}
        self.files: dict[str, bytes] = {}
        self.monitor_tiff: Optional[bytes] = None
        self.seq = 0
        self.unique = 0
        self.series: Optional[dict[str, Any]] = None
        self.release_gen = 0
        self.init_delay = 0.05
        self.trigger_hold: Optional[float | str] = None  # seconds, or 'release' (until disarm/cancel/abort)
        self.end_after: Optional[int] = None  # publish the end after this many images (GUI Disarm)
        self.start_id_offset = 0  # start series_id = arm id + offset ([G2-D] mismatch)
        self.arm_body = 'sequence id'  # or 'sequence_id', or None for an empty body (FIFO fallback)
        self.ft_reject = False  # reject (400) a frame_time below count_time + readout instead of rewriting
        self.defer_last = False  # the last image of a series goes out only on disarm [R14]
        self.deferred: Optional[tuple] = None
        self.fail: dict[str, int] = {}  # 'detector/config/x' -> HTTP status (persistent)
        self.frame_fn: Callable[[int, int], np.ndarray] = self.default_frame
        self.sent: list[tuple[str, bytes]] = []  # (type, raw) of every published message

        self.ctx = zmq.Context()
        self.push = self.ctx.socket(zmq.PUSH)
        self.push.setsockopt(zmq.LINGER, 0)
        self.push.setsockopt(zmq.SNDTIMEO, 2000)
        self.push.setsockopt(zmq.SNDHWM, sndhwm)  # before bind: applies to the connection
        zport = self.push.bind_to_random_port('tcp://127.0.0.1')
        self.push_lock = threading.Lock()
        self.zmq_endpoint = f'tcp://127.0.0.1:{zport}'

        fake = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def log_message(self, *a):
                pass

            def _body(self):
                n = int(self.headers.get('Content-Length') or 0)
                raw = self.rfile.read(n) if n else b''
                return json.loads(raw) if raw else None

            def do_GET(self):
                fake._dispatch(self, 'GET', None)

            def do_PUT(self):
                fake._dispatch(self, 'PUT', self._body())

        self.http = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.http.daemon_threads = True
        self.dcu_endpoint = f'http://127.0.0.1:{self.http.server_address[1]}'
        self.host, self.port = '127.0.0.1', self.http.server_address[1]
        self.zmq_port = zport
        threading.Thread(target=self.http.serve_forever, daemon=True).start()

    # ----- helpers for tests -------------------------------------------------------------------

    def default_frame(self, series_id: int, image_id: int) -> np.ndarray:
        """A ramp whose first pixel is ``series_id * 1_000_000 + image_id``."""
        n = self.rows * self.cols
        a = (np.arange(n, dtype=np.uint64) * 7 + image_id) % min(np.iinfo(self.dtype).max, 65_000)
        a[0] = (series_id * 1_000_000 + image_id) % np.iinfo(self.dtype).max
        return a.astype(self.dtype).reshape(self.rows, self.cols)

    def requests(self, key: Optional[str] = None) -> list[dict[str, Any]]:
        with self.cv:
            return [r for r in self.log if key is None or f"{r['method']} {r['path']}" == key]

    def count(self, key: str) -> int:
        return len(self.requests(key))

    def wait_for(self, pred: Callable[[], bool], timeout: float = 5.0) -> bool:
        end = time.monotonic() + timeout
        with self.cv:
            while not pred():
                left = end - time.monotonic()
                if left <= 0:
                    return False
                self.cv.wait(min(left, 0.05))
        return True

    def close(self):
        with self.cv:
            self.release_gen += 1
            self.cv.notify_all()
        self.http.shutdown()
        self.http.server_close()
        with self.push_lock:
            self.push.close(0)
        self.ctx.term()

    # ----- Stream V2 messages --------------------------------------------------------------------

    def _publish(self, kind: str, msg: dict) -> None:
        raw = cbor2.dumps(msg)
        with self.push_lock:
            try:
                self.push.send(raw)
            except zmq.ZMQError:
                return
        self.sent.append((kind, raw))

    def send_start(self, series_id: int, unique: str, **extra) -> None:
        msg = {
            'type': 'start', 'series_id': series_id, 'series_unique_id': unique,
            'arm_date': cbor2.CBORTag(0, '2026-10-21T01:02:03.000000+00:00'),
            'count_time': float(self.detector['count_time']), 'frame_time': float(self.detector['frame_time']),
            'image_size_x': self.cols, 'image_size_y': self.rows, 'image_dtype': self.dtype.name,
            'number_of_images': int(self.detector['nimages']) * int(self.detector['ntrigger']),
            'channels': ['threshold_1'],
        }  # fmt: skip
        msg.update(extra)
        self._publish('start', msg)

    def image_message(self, series_id: int, unique: str, image_id: int) -> dict:
        a = self.frame_fn(series_id, image_id)
        elem = {1: 68, 2: 69, 4: 70}[a.dtype.itemsize]
        return {
            'type': 'image', 'series_id': series_id, 'series_unique_id': unique, 'image_id': image_id,
            'real_time': [int(self.detector['count_time'] * 1e7), 10_000_000],
            'data': {'threshold_1': cbor2.CBORTag(40, [list(a.shape), cbor2.CBORTag(elem, a.tobytes())])},
        }  # fmt: skip

    def send_image(self, series_id: int, unique: str, image_id: int) -> None:
        self._publish('image', self.image_message(series_id, unique, image_id))

    def send_end(self, series_id: int, unique: str) -> None:
        self._publish('end', {'type': 'end', 'series_id': series_id, 'series_unique_id': unique})

    def _end_series(self) -> None:
        s = self.series
        if s is not None and not s['ended']:
            s['ended'] = True
            self.send_end(s['id'] + self.start_id_offset, s['unique'])

    # ----- HTTP ----------------------------------------------------------------------------------

    def _reply(self, h, status: int, body: Any = None, content_type: str = 'application/json') -> None:
        if isinstance(body, (bytes, bytearray)):
            data = bytes(body)
        elif body is None:
            data = b''
        else:
            data = json.dumps(body).encode()
        h.send_response(status)
        h.send_header('Content-Type', content_type)
        h.send_header('Content-Length', str(len(data)))
        h.end_headers()
        h.wfile.write(data)

    def _dispatch(self, h, method: str, body: Any) -> None:
        path = short(h.path)
        value = body.get('value') if isinstance(body, dict) else None
        with self.cv:
            self.log.append({'method': method, 'path': path, 'body': body, 't': time.monotonic(), 'headers': {k.lower(): v for k, v in h.headers.items()}})
            self.cv.notify_all()
            status = self.fail.get(path)
        if status is not None:
            return self._reply(h, status, b'injected', 'text/plain')
        parts = path.split('/')
        try:
            if method == 'PUT' and parts[:2] == ['detector', 'command']:
                return self._command(h, parts[2], value)
            with self.cv:
                r = self._resource(method, parts, value)
                self.cv.notify_all()
            self._reply(h, *r)
        except (ConnectionError, OSError):
            pass

    def _resource(self, method: str, parts: list[str], value: Any) -> tuple:
        key = '/'.join(parts[2:])
        if parts[0] == 'data' and method == 'GET':
            name = '/'.join(parts[1:])
            return (200, self.files[name], 'application/octet-stream') if name in self.files else (404, b'', 'text/plain')
        mod, task = parts[0], parts[1] if len(parts) > 1 else ''
        if (mod, task, method) == ('detector', 'status', 'GET'):
            if key == 'state':
                return (200, {'value': self.state, 'value_type': 'string'})
            if key == 'high_voltage/state':
                return (200, {'value': self.hv})
        if (mod, task) == ('detector', 'config'):
            if self.state == 'na':
                return (400, b'Parameter does not exist', 'text/plain')
            if method == 'GET':
                return (200, {'value': self.detector[key]}) if key in self.detector else (404, b'', 'text/plain')
            return self._detector_put(key, value)
        if (mod, task) == ('stream', 'config'):
            if method == 'GET':
                return (200, {'value': self.stream.get(key)}) if key in self.stream else (404, b'', 'text/plain')
            self.stream[key] = value
            return (200, [key])
        if (mod, task, key) == ('monitor', 'images', 'monitor'):
            return (200, self.monitor_tiff, 'image/tiff') if self.monitor_tiff else (408, b'', 'text/plain')
        if (mod, task) == ('monitor', 'config'):
            self.monitor[key] = value
            return (200, [key])
        if (mod, task) == ('filewriter', 'config'):
            self.filewriter[key] = value
            return (200, [key])
        if (mod, task, key) == ('filewriter', 'status', 'state'):
            return (200, {'value': 'ready'})
        if (mod, task, key) == ('filewriter', 'status', 'files'):
            return (200, {'value': sorted(self.files)})
        if (mod, task, key) == ('filewriter', 'command', 'clear'):
            self.files.clear()
            return (200, None)
        return (404, b'unknown resource', 'text/plain')

    def _detector_put(self, key: str, v: Any) -> tuple:
        readout = self.detector['detector_readout_time']
        if key == 'count_time':
            if self.detector['frame_time'] - v < readout - 1e-12:
                self.detector['frame_time'] = v + readout
        elif key == 'frame_time':
            if self.ft_reject and v - self.detector['count_time'] < readout - 1e-12:
                return (400, b'frame_time out of range', 'text/plain')
            if v - self.detector['count_time'] < readout - 1e-12:
                self.detector['count_time'] = v - readout
        elif key in ('nimages', 'ntrigger') and not (isinstance(v, int) and not isinstance(v, bool)):
            return (400, b'value_type int expected', 'text/plain')
        self.detector[key] = v
        return (200, [key])

    def _seq_body(self) -> dict:
        return {self.arm_body: self.seq} if self.arm_body else {}

    def _command(self, h, cmd: str, value: Any) -> None:
        if cmd == 'initialize':
            with self.cv:
                self.state = 'initialize'
                self.cv.notify_all()
            time.sleep(self.init_delay)
            with self.cv:
                self.state = 'idle'
                self.seq = 0
                self.series = None
                self.stream['mode'] = 'disabled'
                self.monitor['mode'] = 'disabled'
                self.cv.notify_all()
            return self._reply(h, 200)
        if cmd == 'trigger':
            return self._trigger(h, value)
        with self.cv:
            if cmd == 'arm':
                if self.state != 'idle':
                    r = (400, b'not idle', 'text/plain')
                else:
                    self.seq += 1
                    self.unique += 1
                    self.state = 'ready'
                    self.series = {'id': self.seq, 'unique': f'u-{self.unique}', 'image_id': 0, 'triggers': 0,
                                   'ended': False}  # fmt: skip
                    self.send_start(self.seq + self.start_id_offset, self.series['unique'])
                    r = (200, self._seq_body())
            elif cmd in ('disarm', 'abort', 'cancel'):
                if cmd != 'cancel' or self.state == 'acquire':
                    if self.deferred is not None:
                        self.send_image(*self.deferred)
                        self.send_end(*self.deferred[:2])
                        self.deferred = None
                    if self.state in ('ready', 'acquire'):
                        self._end_series()
                    self.state = 'idle'
                self.release_gen += 1
                r = (200, self._seq_body())
            elif cmd == 'hv_enabled':
                self.hv = 'READY' if value else 'OFF'
                r = (200, None)
            else:
                r = (404, b'unknown command', 'text/plain')
            self.cv.notify_all()
        self._reply(h, *r)

    def _released(self, gen: int) -> bool:
        return self.release_gen != gen

    def _trigger(self, h, value: Any) -> None:
        with self.cv:
            if self.state not in ('ready', 'acquire') or self.series is None:
                r = (400, b'not armed', 'text/plain')
                self.cv.notify_all()
                return self._reply(h, *r)
            self.state = 'acquire'
            gen = self.release_gen
            s = self.series
            mode = self.detector['trigger_mode']
            n = 1 if mode in ('inte', 'exte') else int(self.detector['nimages'])
            hold = self.trigger_hold
            self.cv.notify_all()
            if hold == 'release':
                while not self._released(gen):
                    self.cv.wait(0.05)
            elif hold:
                self.cv.wait_for(lambda: self._released(gen), timeout=float(hold))
        for _ in range(n):
            with self.cv:
                if self._released(gen) or s['ended']:
                    break
                s['image_id'] += 1
                image_id = s['image_id']
                last = image_id == int(self.detector['nimages']) * int(self.detector['ntrigger'])
                if self.defer_last and last:
                    self.deferred = (s['id'] + self.start_id_offset, s['unique'], image_id)
                    s['ended'] = True  # the end follows the deferred image on disarm
                    break
            self.send_image(s['id'] + self.start_id_offset, s['unique'], image_id)
            if self.end_after is not None and image_id >= self.end_after:
                with self.cv:
                    self._end_series()
                    self.state = 'idle'
                    self.cv.notify_all()
                break
            time.sleep(self.frame_period)
        with self.cv:
            if not self._released(gen) and self.state == 'acquire' and self.series is s:
                s['triggers'] += 1
                if self.deferred is not None:
                    self.state = 'acquire'  # stays busy until the disarm flushes the last image
                elif s['triggers'] >= int(self.detector['ntrigger']):
                    self._end_series()
                    self.state = 'idle'  # auto-disarm after the last trigger (SIMPLON footnote 1)
                else:
                    self.state = 'ready'
            self.cv.notify_all()
        self._reply(h, 200)


def tiff_bytes(img: np.ndarray) -> bytes:
    import tifffile

    buf = io.BytesIO()
    tifffile.imwrite(buf, img)
    return buf.getvalue()
