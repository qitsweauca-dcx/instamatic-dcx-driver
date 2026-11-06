"""super_dectris — Dectris SINGLA driver in Rust.

``CameraDectris`` (interface ``dectris_simplon``) is a class of the PyO3
extension ``_super_dectris`` (sources in ``rust/``, design in ``docs/DESIGN.md``).
The module-level names below are kept for code that imports them from here (DESIGN §11.6).
"""

from __future__ import annotations

import logging

from instamatic.camera.camera_base import CameraBase
from instamatic.image_utils import autoscale, bin_ndarray

from ._super_dectris import (
    TYPED_ARRAY_DTYPES,
    CameraDectris,
    DectrisBusy,
    DectrisConnectionError,
    DectrisError,
    DectrisHTTPError,
    DectrisTimeout,
    DetectorStateError,
    StreamDecodeError,
    decode_message,
    decode_raw_header,
    decompress,
    decompress_image,
    tag_decoders,
)

# The exact logger name a log panel attaches to; the Rust records use the same name.
logger = logging.getLogger('instamatic.camera.camera_dectris')

# A #[pyclass] has no Python base, so isinstance(cam, CameraBase) needs the registration.
CameraBase.register(CameraDectris)

__all__ = [
    'CameraDectris',
    'DectrisBusy',
    'DectrisConnectionError',
    'DectrisError',
    'DectrisHTTPError',
    'DectrisTimeout',
    'DetectorStateError',
    'StreamDecodeError',
    'TYPED_ARRAY_DTYPES',
    'autoscale',
    'bin_ndarray',
    'decode_message',
    'decode_raw_header',
    'decompress',
    'decompress_image',
    'logger',
    'tag_decoders',
]
