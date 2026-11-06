"""Self-check of the driver's integration into an Instamatic-based program (INTEGRATION.md step 10).

Run it in the program's Python environment:

    python -m instamatic.camera.super_dectris.selfcheck [--config DIR] [--dcu] [--json]

If that fails while importing the driver, run this file directly instead; it then reports which
part is missing:

    python src/instamatic/camera/super_dectris/selfcheck.py

`python -m` loads the program's default configuration before `--config` takes effect. To leave the
default configuration folder alone, set the `instamatic` variable instead, or run this file
directly with `--config`.

It is read-only. It never arms, triggers or configures the detector. With `--dcu` it sends two
GET requests to the control unit and opens and closes one TCP connection to the stream port.
Exit code 0 when no check failed.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib
import inspect
import json
import os
import platform
import re
import socket
import struct
import subprocess
import sys
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
PYD = '_super_dectris.cp312-win_amd64.pyd'
INTERFACE = 'dectris_simplon'
CAMERA_KEYS = ('DCU_host', 'DCU_port', 'ZMQ_host', 'ZMQ_port', 'dimensions')

# A synthetic Stream V2 image message from the driver's test vectors (series 42, image 1,
# 37 x 53 uint32, bslz4) and the SHA-256 of its decoded pixels.
VECTOR = 'selfcheck_image.cbor'
VECTOR_SHA256 = '5ea44e4d3c3a0b22b80f9a2e10d3fe0d279fb633809b072a2204672b922613dc'

PASS, WARN, FAIL, SKIP = 'PASS', 'WARN', 'FAIL', 'SKIP'


class Checks:
    def __init__(self):
        self.results = []

    def add(self, cid, status, text, step=None):
        self.results.append({'id': cid, 'status': status, 'text': text, 'step': step})

    def failed(self):
        return any(r['status'] == FAIL for r in self.results)


def _short(e: BaseException) -> str:
    return f'{type(e).__name__}: {e}'.strip()


def check_platform(c: Checks):
    problems = []
    if sys.platform != 'win32':
        problems.append(f'platform {sys.platform}, not Windows')
    if struct.calcsize('P') != 8:
        problems.append('32-bit Python')
    if sys.version_info[:2] != (3, 12):
        problems.append(f'Python {platform.python_version()}, not 3.12')
    if platform.python_implementation() != 'CPython':
        problems.append(platform.python_implementation())
    if problems:
        c.add('C1', FAIL, 'the .pyd is for CPython 3.12 on Windows x64: ' + '; '.join(problems), 2)
    else:
        c.add('C1', PASS, f'CPython {platform.python_version()} on Windows x64')


def check_app(c: Checks):
    try:
        app = importlib.import_module('instamatic')
    except Exception as e:
        c.add('C2', FAIL, f'import instamatic: {_short(e)}', 1)
        return None
    missing = [n for n in ('__app_title__', '__app_version__') if not isinstance(getattr(app, n, None), str)
               or not getattr(app, n)]
    if missing:
        c.add('C2', FAIL, f'instamatic/__init__.py lacks {", ".join(missing)} (strings; the driver\'s User-Agent)', 4)
        return app
    ua = f'{app.__app_title__}/{app.__app_version__}'
    if any((ch < ' ' and ch != '\t') or ch == '\x7f' for ch in ua):
        # The driver sends each control character as a space, with a WARNING
        c.add('C2', WARN, f'User-Agent {ua!r} has control characters; the driver sends them as spaces', 4)
    else:
        c.add('C2', PASS, f'User-Agent {ua}')
    return app


def check_pyd(c: Checks):
    pyd = HERE / PYD
    if not pyd.is_file():
        c.add('C3', FAIL, f'{PYD} is not in {HERE}', 2)
        return None
    digest = hashlib.sha256(pyd.read_bytes()).hexdigest()
    try:
        ext = importlib.import_module('instamatic.camera.super_dectris._super_dectris')
        pkg = importlib.import_module('instamatic.camera.super_dectris')
    except (ImportError, OSError) as e:
        c.add('C3', FAIL, f'importing the driver: {_short(e)} (.pyd sha256 {digest[:16]}…)', 2)
        return None
    except Exception as e:  # raised by the program while importing, e.g. a broken configuration file
        c.add('C3', FAIL, f'importing the driver raised {_short(e)}; this comes from the program or its '
                          'configuration, not from the .pyd')
        return None
    where = Path(pkg.__file__).resolve().parent
    if where != HERE:
        c.add('C3', FAIL, f'Python imports the driver from {where}, not from {HERE}; '
                          'install the program editable (pip install -e .) or copy the driver there', 1)
        return None
    c.add('C3', PASS, f'driver {getattr(ext, "__version__", "?")}, .pyd sha256 {digest}')
    return pkg


def check_registered(c: Checks, pkg):
    try:
        from instamatic.camera.camera_base import CameraBase
    except Exception as e:
        c.add('C4', FAIL, f'import CameraBase: {_short(e)}', 1)
        return
    if issubclass(pkg.CameraDectris, CameraBase):
        c.add('C4', PASS, 'CameraDectris is registered as a CameraBase')
    else:
        c.add('C4', FAIL, 'CameraDectris is not a CameraBase; the package __init__.py was changed or not run', 1)


def check_factory(c: Checks, pkg):
    try:
        camera = importlib.import_module('instamatic.camera.camera')
    except Exception as e:
        c.add('C5', FAIL, f'import instamatic.camera.camera: {_short(e)}', 3)
        return None
    get_class = getattr(camera, 'get_camera_class', None)
    if get_class is None:
        c.add('C5', WARN, 'instamatic.camera.camera has no get_camera_class; check by hand that the interface '
                          f'{INTERFACE!r} gives instamatic.camera.super_dectris.CameraDectris', 3)
        return camera
    try:
        cls = get_class(INTERFACE)
    except Exception as e:
        c.add('C5', FAIL, f'get_camera_class({INTERFACE!r}): {_short(e)}', 3)
        return camera
    try:
        simulate = bool(importlib.import_module('instamatic.config').settings.simulate)
    except Exception:
        simulate = False
    if cls is pkg.CameraDectris:
        c.add('C5', PASS, f'interface {INTERFACE!r} gives CameraDectris')
    elif simulate:
        c.add('C5', WARN, f'settings.yaml has simulate: True, so the factory gives {cls.__name__}; '
                          'set it False to check the interface', 3)
    else:
        c.add('C5', FAIL, f'interface {INTERFACE!r} gives {cls.__module__}.{cls.__name__}, not CameraDectris', 3)
    return camera


def check_decode(c: Checks, pkg):
    try:
        raw = (HERE / VECTOR).read_bytes()
    except OSError as e:
        c.add('C6', FAIL, f'{VECTOR}: {_short(e)} (copy the whole driver folder)', 1)
        return
    try:
        msg = pkg.decode_message(raw)
        arr = msg['data']['threshold_1']
    except Exception as e:
        c.add('C6', FAIL, f'decode_message: {_short(e)}', 2)
        return
    import numpy as np

    digest = hashlib.sha256(np.ascontiguousarray(arr).tobytes()).hexdigest()
    ok = digest == VECTOR_SHA256 and msg.get('series_id') == 42 and msg.get('image_id') == 1 \
        and arr.shape == (37, 53) and str(arr.dtype) == 'uint32'
    if ok:
        c.add('C6', PASS, 'decoded the test message (bslz4, 37 x 53 uint32) exactly')
    else:
        c.add('C6', FAIL, f'the test message decoded wrongly ({arr.dtype} {arr.shape}, sha256 {digest[:16]}…); '
                          'rebuild or re-download the .pyd', 2)


def camera_config(c: Checks):
    try:
        config = importlib.import_module('instamatic.config')
        cam = config.camera
        interface = getattr(cam, 'interface', None)
    except Exception as e:
        c.add('C7', WARN, f'could not read the camera configuration: {_short(e)}', 5)
        return None
    name = getattr(config.settings, 'camera', '?')
    if interface != INTERFACE:
        c.add('C7', WARN, f'settings.yaml names camera {name!r} with interface {interface!r}, not {INTERFACE!r}', 5)
        return None
    missing = [k for k in CAMERA_KEYS if getattr(cam, k, None) in (None, '')]
    hwm = getattr(cam, 'zmq_rcvhwm', None)
    queue = f'zmq_rcvhwm {hwm}' if hwm is not None else 'zmq_rcvhwm not set (500)'
    if missing:
        c.add('C7', WARN, f'camera {name!r} lacks {", ".join(missing)}', 5)
    else:
        c.add('C7', PASS, f'camera {name!r}: DCU {cam.DCU_host}:{cam.DCU_port}, stream {cam.ZMQ_host}:{cam.ZMQ_port}, '
                          f'{queue}')
    return cam


def check_live_view(c: Checks):
    try:
        src = inspect.getsource(importlib.import_module('instamatic.camera.videostream'))
    except Exception as e:
        c.add('C8', WARN, f'could not read instamatic.camera.videostream: {_short(e)}', 6)
        return
    if 'get_preview_image' in src:
        c.add('C8', PASS, 'the live view uses a camera\'s get_preview_image')
    else:
        c.add('C8', WARN, 'the live view does not use get_preview_image, so it arms the detector for every frame', 6)


def check_old_imports(c: Checks, camera):
    if camera is None:
        c.add('C9', SKIP, 'instamatic.camera.camera not importable')
        return
    try:
        src = inspect.getsource(camera)
    except Exception as e:
        c.add('C9', WARN, f'could not read instamatic.camera.camera: {_short(e)}', 8)
        return
    # This driver is imported as the package itself (`from instamatic.camera.super_dectris import ...`);
    # anything else naming Dectris, a submodule of an older super_dectris folder included, is reported
    this = re.compile(r'instamatic\.camera\.super_dectris(\._super_dectris)?\s+import\b')
    other = [ln.strip() for ln in src.splitlines()
             if re.search(r'\bimport\b', ln) and re.search(r'dectris', ln, re.I) and not this.search(ln)]
    if other:
        c.add('C9', WARN, 'other Dectris imports in camera.py: ' + ' | '.join(other), 8)
    else:
        c.add('C9', PASS, 'no other Dectris driver imported in camera.py')


def check_dcu(c: Checks, cam):
    if cam is None:
        c.add('C10', SKIP, 'no dectris_simplon camera configured')
        return
    base = f'http://{cam.DCU_host}:{cam.DCU_port}/detector/api/1.8.0'
    try:
        values = {}
        for key in ('status/state', 'config/x_pixels_in_detector', 'config/y_pixels_in_detector'):
            with urllib.request.urlopen(f'{base}/{key}', timeout=3) as r:
                values[key] = json.loads(r.read().decode('utf-8')).get('value')
        c.add('C10', PASS, f'DCU answers: state {values["status/state"]}, '
                           f'{values["config/x_pixels_in_detector"]} x {values["config/y_pixels_in_detector"]} pixels')
    except Exception as e:
        c.add('C10', FAIL, f'DCU {cam.DCU_host}:{cam.DCU_port}: {_short(e)}', 5)
        return
    try:
        socket.create_connection((cam.ZMQ_host, int(cam.ZMQ_port)), timeout=3).close()
        c.add('C10', PASS, f'stream port {cam.ZMQ_host}:{cam.ZMQ_port} accepts a connection')
    except Exception as e:
        c.add('C10', WARN, f'stream port {cam.ZMQ_host}:{cam.ZMQ_port}: {_short(e)} '
                           '(the stream may be disabled until the driver enables it)', 5)


def run(dcu: bool = False) -> Checks:
    c = Checks()
    check_platform(c)
    app = check_app(c)
    pkg = check_pyd(c) if app is not None else None
    if pkg is None:
        for cid in ('C4', 'C5', 'C6'):
            c.add(cid, SKIP, 'the driver did not import')
        camera = None
    else:
        check_registered(c, pkg)
        camera = check_factory(c, pkg)
        check_decode(c, pkg)
    cam = camera_config(c) if app is not None else None
    if app is not None:
        check_live_view(c)
    check_old_imports(c, camera)
    if dcu:
        check_dcu(c, cam)
    return c


def main(argv=None) -> int:
    p = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    p.add_argument('--config', help='Instamatic configuration folder to use (sets the instamatic variable)')
    p.add_argument('--dcu', action='store_true', help='also ask the control unit (read-only)')
    p.add_argument('--json', action='store_true', help='print the results as JSON')
    a = p.parse_args(argv)
    if a.config and not (Path(a.config) / 'config').is_dir():
        # Instamatic falls back to its default folder without a word, so say it here
        print(f'--config {a.config}: no folder "config" in it', file=sys.stderr)
        return 2
    if a.config and os.environ.get('instamatic') != a.config:
        # Instamatic reads its configuration folder when it is first imported, which `python -m` has
        # already done; so run again in a child process that has the folder from the start
        env = dict(os.environ, instamatic=a.config)
        args = sys.argv[1:] if argv is None else list(argv)
        return subprocess.run([sys.executable, str(Path(__file__).resolve()), *args], env=env).returncode
    c = run(dcu=a.dcu)
    if a.json:
        print(json.dumps({'ok': not c.failed(), 'results': c.results}, indent=1))
    else:
        for r in c.results:
            step = f'  [INTEGRATION.md step {r["step"]}]' if r['step'] and r['status'] in (FAIL, WARN) else ''
            print(f'{r["status"]:4}  {r["id"]:3}  {r["text"]}{step}')
        print('OK' if not c.failed() else 'FAILED: fix the FAIL lines, then run this again')
    return 1 if c.failed() else 0


if __name__ == '__main__':
    sys.exit(main())
