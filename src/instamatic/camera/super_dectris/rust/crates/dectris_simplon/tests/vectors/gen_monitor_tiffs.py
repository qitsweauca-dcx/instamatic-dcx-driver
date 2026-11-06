"""Generate monitor-TIFF test vectors for dectris_simplon (run with a Python that has numpy and tifffile).

Each vector is a small TIFF plus the expected *oriented* frame (fliplr -> flipud -> rot90(k=-1), PROTOCOL §11)
as raw little-endian bytes, and a JSON sidecar with shape/dtype. Tag 51192 carries key=value lines.
"""
import json
import pathlib

import numpy as np
import tifffile

HERE = pathlib.Path(__file__).parent
META = "IfdVersion=1\nSeriesNumber=7\nImageNumber=3\nExposureTime=0.01\nIncidentEnergy=200000\n"


def orient(a):
    return np.rot90(np.flipud(np.fliplr(a)), k=-1)


def write(name, arr, compression=None):
    path = HERE / f"{name}.tif"
    tifffile.imwrite(path, arr, compression=compression,
                     extratags=[(51192, 's', 0, META, True)])
    o = np.ascontiguousarray(orient(arr))
    (HERE / f"{name}.oriented.bin").write_bytes(o.astype(o.dtype.newbyteorder('<')).tobytes())
    (HERE / f"{name}.json").write_text(json.dumps({
        "rows": int(o.shape[0]), "cols": int(o.shape[1]), "dtype": str(arr.dtype),
        "src_rows": int(arr.shape[0]), "src_cols": int(arr.shape[1]),
    }))
    assert np.array_equal(o, np.rot90(arr, k=1))


r, c = np.mgrid[0:48, 0:64]
write("mono16", (r * 64 + c).astype(np.uint16))
write("mono32", (r * 100000 + c).astype(np.uint32))
write("mono8", ((r * 5 + c) % 256).astype(np.uint8))
write("float32", (r + c / 100.0).astype(np.float32))
write("mono16_deflate", (r * 64 + c).astype(np.uint16), compression="zlib")
print("ok")
