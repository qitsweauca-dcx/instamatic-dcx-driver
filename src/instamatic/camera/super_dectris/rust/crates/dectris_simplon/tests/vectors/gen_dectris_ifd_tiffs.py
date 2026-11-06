"""Generate monitor-TIFF vectors whose tag 51192 points to a DECTRIS private IFD (run with a Python that has numpy).

The SIMPLON monitor TIFF stores tag 51192 (0xC7F8) as an offset (LONG / IFD, or LONG8 / IFD8 in BigTIFF) to a
private IFD whose entries use the vendor codes of the SIMPLON API reference, table "TIFF Header Data"
(0x0000 IfdVersion ... 0x0017 DetectorDistance). The files are assembled by hand with `struct` so that the byte
order, the TIFF flavour and the 51192 field type are controlled exactly. Each `<name>.tif` has a `<name>.json`
sidecar with:

- `metadata`: the expected `[name, value]` list (strings, in IFD order) that `dectris_simplon::tiff` produces.
  The values are written by hand (Rust `Display` formatting, e.g. `8041.0` -> "8041"), not derived from Python.
- `dectris_value_pos`: file position of the 51192 entry's value field (tests patch it to a bad offset).
- `width`, `height`, `dtype`.

Synthetic data, not a DCU capture.
"""
import json
import pathlib
import struct

import numpy as np
import tifffile

HERE = pathlib.Path(__file__).parent

ASCII, SHORT, LONG, RATIONAL, UNDEFINED, SSHORT, SLONG, FLOAT, DOUBLE, IFD, LONG8, IFD8 = (
    2, 3, 4, 5, 7, 8, 9, 11, 12, 13, 16, 18)
FMT = {SHORT: 'H', LONG: 'I', SSHORT: 'h', SLONG: 'i', FLOAT: 'f', DOUBLE: 'd', IFD: 'I', LONG8: 'Q', IFD8: 'Q'}

# (code, type, values, expected string or None if the entry must be skipped)
PRIVATE = [
    (0x0000, LONG, [0], "0"),                                                 # IfdVersion
    (0x0001, ASCII, b"01DC3XQ8ENM14PYCDHQ9XVZZJ8\0", "01DC3XQ8ENM14PYCDHQ9XVZZJ8"),  # SeriesUniqueId
    (0x0002, LONG, [7], "7"),                                                 # SeriesNumber
    (0x0003, LONG, [42], "42"),                                               # ImageNumber
    (0x0004, ASCII, b"2019-05-14T10:59:56.000000000Z\0", "2019-05-14T10:59:56.000000000Z"),  # ImageDateTime
    (0x0005, SHORT, [1, 2], "1,2"),                                           # ThresholdId (difference mode)
    (0x0006, DOUBLE, [4020.5, 8041.0], "4020.5,8041"),                        # ThresholdEnergy
    (0x0007, DOUBLE, [0.01], "0.01"),                                         # ExposureTime
    (0x0008, RATIONAL, [(1, 4)], "0.25"),                                     # not in the vendor table
    (0x0009, DOUBLE, [200000.0], "200000"),                                   # IncidentEnergy
    (0x000A, DOUBLE, [0.0251], "0.0251"),                                     # IncidentWavelength
    (0x000B, FLOAT, [0.5, -1.25], "0.5,-1.25"),                               # not in the vendor table
    (0x000C, SSHORT, [-3], "-3"),                                             # not in the vendor table
    (0x000D, SLONG, [-100000], "-100000"),                                    # not in the vendor table
    (0x000E, UNDEFINED, b"abc\0", "abc"),                                     # not in the vendor table
    (0x000F, 99, b"\x01\x02\x03\x04", None),                                  # unknown field type: skipped
    (0x0010, IFD, [1234], "1234"),                                            # nested IFD: offset only, no recursion
    (0x0012, LONG, [5], "5"),                                                 # LostPixelCount
    (0x0016, DOUBLE, [514.5, 531.25], "514.5,531.25"),                        # BeamCenter
    (0x0017, DOUBLE, [0.25], "0.25"),                                         # DetectorDistance
    (0x0042, LONG, [66], "66"),                                               # unknown code -> Tag66
]
NAMES = {0x0000: "IfdVersion", 0x0001: "SeriesUniqueId", 0x0002: "SeriesNumber", 0x0003: "ImageNumber",
         0x0004: "ImageDateTime", 0x0005: "ThresholdId", 0x0006: "ThresholdEnergy", 0x0007: "ExposureTime",
         0x0009: "IncidentEnergy", 0x000A: "IncidentWavelength", 0x0012: "LostPixelCount",
         0x0016: "BeamCenter", 0x0017: "DetectorDistance"}


def payload(bo, typ, values):
    """(count, raw bytes) of one entry's value."""
    if isinstance(values, bytes):
        return len(values), values
    if typ == RATIONAL:
        return len(values), b"".join(struct.pack(bo + "II", n, d) for n, d in values)
    return len(values), struct.pack(bo + FMT[typ] * len(values), *values)


def pack_ifd(bo, big, base, entries):
    """Bytes of an IFD placed at file offset `base` (entries: (tag, type, count, raw)), followed by its
    out-of-line values. Next-IFD offset 0."""
    entries = sorted(entries, key=lambda e: e[0])
    cnt_fmt, off_fmt, inline, esz = ("Q", "Q", 8, 20) if big else ("H", "I", 4, 12)
    table_len = struct.calcsize(cnt_fmt) + len(entries) * esz + struct.calcsize(off_fmt)
    table = bytearray(struct.pack(bo + cnt_fmt, len(entries)))
    extra = bytearray()
    for tag, typ, count, raw in entries:
        if len(raw) <= inline:
            val = raw.ljust(inline, b"\0")
        else:
            val = struct.pack(bo + off_fmt, base + table_len + len(extra))
            extra += raw
            if len(extra) % 2:
                extra += b"\0"
        table += struct.pack(bo + "HH" + off_fmt, tag, typ, count) + val
    table += struct.pack(bo + off_fmt, 0)
    return bytes(table + extra)


def build(name, bo, big, dectris_type):
    arr = (np.arange(6 * 8, dtype=np.uint16).reshape(6, 8) * 1000 + 7).astype(np.uint16)
    h, w = arr.shape
    pix = arr.astype(np.dtype(np.uint16).newbyteorder(bo)).tobytes()
    if big:
        header = (b"II" if bo == "<" else b"MM") + struct.pack(bo + "HHHQ", 43, 8, 0, 0)
    else:
        header = (b"II" if bo == "<" else b"MM") + struct.pack(bo + "HI", 42, 0)
    data_off = len(header)
    ifd0_off = data_off + len(pix)
    long_t = LONG8 if big else LONG

    def ifd0(dectris_off):
        e = [
            (256, long_t, *payload(bo, long_t, [w])),
            (257, long_t, *payload(bo, long_t, [h])),
            (258, SHORT, *payload(bo, SHORT, [16])),
            (259, SHORT, *payload(bo, SHORT, [1])),
            (262, SHORT, *payload(bo, SHORT, [1])),
            (273, long_t, *payload(bo, long_t, [data_off])),
            (277, SHORT, *payload(bo, SHORT, [1])),
            (278, long_t, *payload(bo, long_t, [h])),
            (279, long_t, *payload(bo, long_t, [len(pix)])),
            (51192, dectris_type, *payload(bo, dectris_type, [dectris_off])),
        ]
        return pack_ifd(bo, big, ifd0_off, e)

    priv_off = ifd0_off + len(ifd0(0))
    first = ifd0(priv_off)
    priv = pack_ifd(bo, big, priv_off, [(c, t, *payload(bo, t, v)) for c, t, v, _ in PRIVATE])
    out = bytearray(header + pix + first + priv)
    off_fmt = "Q" if big else "I"
    out[(8 if big else 4):(16 if big else 8)] = struct.pack(bo + off_fmt, ifd0_off)

    # position of the 51192 entry's value field: last of 10 entries (51192 sorts last)
    esz, cnt_sz = (20, 8) if big else (12, 2)
    pos = ifd0_off + cnt_sz + 9 * esz + 4 + (8 if big else 4)
    entry_start = ifd0_off + cnt_sz + 9 * esz
    assert struct.unpack(bo + "H", out[entry_start:entry_start + 2])[0] == 51192
    assert struct.unpack(bo + off_fmt, out[pos:pos + (8 if big else 4)])[0] == priv_off

    path = HERE / f"{name}.tif"
    path.write_bytes(bytes(out))
    back = tifffile.imread(path)
    assert back.dtype == np.uint16 and np.array_equal(back, arr), name
    expected = [[NAMES.get(c, f"Tag{c}"), s] for c, _, _, s in PRIVATE if s is not None]
    (HERE / f"{name}.json").write_text(json.dumps({
        "metadata": expected, "dectris_value_pos": pos, "width": w, "height": h, "dtype": "uint16",
    }, indent=1))
    assert len(out) < 200 * 1024


build("dectris_ifd_le", "<", False, LONG)
build("dectris_ifd_be", ">", False, IFD)
build("dectris_ifd_bigtiff", "<", True, IFD8)
print("ok")
