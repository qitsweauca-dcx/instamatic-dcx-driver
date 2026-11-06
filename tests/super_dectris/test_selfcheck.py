"""Tests of the integration self-check (super_dectris/selfcheck.py)."""

from __future__ import annotations

import importlib
import json
import types

import pytest

from instamatic.camera.super_dectris import selfcheck as sc


def by_id(checks):
    return {r['id']: r for r in checks.results}


def fake_imports(monkeypatch, **fakes):
    """Make selfcheck's importlib.import_module return the given modules for those names."""
    real = importlib.import_module

    def imp(name, *a, **kw):
        return fakes[name] if name in fakes else real(name, *a, **kw)

    monkeypatch.setattr(sc.importlib, 'import_module', imp)


def test_this_tree_passes_the_driver_checks():
    r = by_id(sc.run())
    for cid in ('C1', 'C2', 'C3', 'C4', 'C6'):
        assert r[cid]['status'] == sc.PASS, r[cid]
    assert r['C5']['status'] in (sc.PASS, sc.WARN), r['C5']  # WARN when settings.yaml has simulate: True
    assert 'C10' not in r  # only with --dcu


def test_missing_app_attributes_fail_with_step_4(monkeypatch):
    fake_imports(monkeypatch, instamatic=types.ModuleType('instamatic'))
    c = sc.Checks()
    sc.check_app(c)
    assert c.results[0]['status'] == sc.FAIL
    assert c.results[0]['step'] == 4


def test_control_character_in_app_title_warns_with_step_4(monkeypatch):
    app = types.ModuleType('instamatic')
    app.__app_title__, app.__app_version__ = 'my-app\n', '1.0'
    fake_imports(monkeypatch, instamatic=app)
    c = sc.Checks()
    sc.check_app(c)
    assert c.results[0]['status'] == sc.WARN
    assert c.results[0]['step'] == 4


def test_factory_giving_another_class_fails(monkeypatch):
    pkg = importlib.import_module('instamatic.camera.super_dectris')
    cam = types.ModuleType('instamatic.camera.camera')
    cam.get_camera_class = lambda interface: object
    fake_imports(monkeypatch, **{'instamatic.camera.camera': cam})
    c = sc.Checks()
    sc.check_factory(c, pkg)
    assert c.results[0]['status'] in (sc.FAIL, sc.WARN)
    assert c.results[0]['step'] == 3


def test_factory_without_get_camera_class_warns(monkeypatch):
    pkg = importlib.import_module('instamatic.camera.super_dectris')
    fake_imports(monkeypatch, **{'instamatic.camera.camera': types.ModuleType('instamatic.camera.camera')})
    c = sc.Checks()
    sc.check_factory(c, pkg)
    assert c.results[0]['status'] == sc.WARN


def test_wrong_decode_fails(monkeypatch):
    pkg = importlib.import_module('instamatic.camera.super_dectris')
    monkeypatch.setattr(sc, 'VECTOR_SHA256', '0' * 64)
    c = sc.Checks()
    sc.check_decode(c, pkg)
    assert c.results[0]['status'] == sc.FAIL


def test_other_dectris_import_warns(tmp_path, monkeypatch):
    mod = tmp_path / 'fake_camera_mod.py'
    mod.write_text(
        "def get_camera_class(interface):\n"
        "    if interface == 'x':\n"
        "        from instamatic.camera.some_dectris_driver import Cam as cam\n"
        "    elif interface == 'y':\n"
        "        from instamatic.camera.super_dectris.older_module import Old as cam\n"
        "    elif interface == 'dectris_simplon':\n"
        "        from instamatic.camera.super_dectris import CameraDectris as cam\n"
        "    return cam\n",
        encoding='utf-8',
    )
    monkeypatch.syspath_prepend(str(tmp_path))
    c = sc.Checks()
    sc.check_old_imports(c, importlib.import_module('fake_camera_mod'))
    assert c.results[0]['status'] == sc.WARN
    assert 'some_dectris_driver' in c.results[0]['text']
    assert 'older_module' in c.results[0]['text']
    assert 'import CameraDectris' not in c.results[0]['text']


def test_live_view_without_preview_warns(tmp_path, monkeypatch):
    mod = tmp_path / 'fake_videostream_mod.py'
    mod.write_text('class MediaGrabber:\n    pass\n', encoding='utf-8')
    monkeypatch.syspath_prepend(str(tmp_path))
    fake_imports(monkeypatch, **{'instamatic.camera.videostream': importlib.import_module('fake_videostream_mod')})
    c = sc.Checks()
    sc.check_live_view(c)
    assert c.results[0]['status'] == sc.WARN
    assert c.results[0]['step'] == 6


def test_json_output_and_exit_code(capsys):
    code = sc.main(['--json'])
    out = json.loads(capsys.readouterr().out)
    assert out['ok'] is (code == 0)
    assert {r['id'] for r in out['results']} >= {'C1', 'C2', 'C3', 'C4', 'C5', 'C6', 'C7', 'C8', 'C9'}


def test_config_without_config_folder_stops(tmp_path, capsys):
    assert sc.main(['--config', str(tmp_path)]) == 2
    assert 'no folder "config"' in capsys.readouterr().err


@pytest.mark.parametrize('status', [sc.FAIL])
def test_failed_is_true_only_with_a_fail(status):
    c = sc.Checks()
    c.add('X', sc.WARN, 'w')
    assert not c.failed()
    c.add('Y', status, 'f')
    assert c.failed()
