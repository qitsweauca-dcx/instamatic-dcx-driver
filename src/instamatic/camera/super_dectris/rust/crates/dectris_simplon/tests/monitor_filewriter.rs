//! Monitor (decode + orientation vs numpy oracles, 408 → cached frame, ≤ 10 Hz, back-off [R10],
//! [G1-J]) and FileWriter (PUT order, 405 skip, listing, download) against the fake.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test crate: helpers outside #[test] fns

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{detector, detector_with, Action, FakeSimplon};
use dectris_common::DriverError;
use dectris_simplon::{
    DecodedImage, Detector, FilewriterConfig, MonitorDecoder, Pixels, Settings, TiffCrateDecoder,
};
use serde_json::json;

fn vector(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/vectors/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn sidecar(name: &str) -> serde_json::Value {
    serde_json::from_slice(&vector(&format!("{name}.json"))).unwrap()
}

#[test]
fn tiff_vectors_decode_and_orient_like_numpy() {
    for name in ["mono8", "mono16", "mono32", "float32"] {
        let img = TiffCrateDecoder
            .decode(&vector(&format!("{name}.tif")))
            .unwrap();
        let meta = sidecar(name);
        assert_eq!(
            img.height as u64,
            meta["src_rows"].as_u64().unwrap(),
            "{name}"
        );
        assert_eq!(
            img.width as u64,
            meta["src_cols"].as_u64().unwrap(),
            "{name}"
        );
        assert_eq!(
            img.pixels.dtype(),
            meta["dtype"].as_str().unwrap(),
            "{name}"
        );
        let kv: std::collections::HashMap<_, _> = img.metadata.iter().cloned().collect();
        assert_eq!(
            kv.get("ExposureTime").map(String::as_str),
            Some("0.01"),
            "{name}"
        );
        assert_eq!(kv.get("SeriesNumber").map(String::as_str), Some("7"));
        let f = dectris_simplon::monitor::orient(&img);
        assert_eq!(f.rows as u64, meta["rows"].as_u64().unwrap());
        assert_eq!(f.cols as u64, meta["cols"].as_u64().unwrap());
        assert_eq!(
            f.pixels.to_le_bytes(),
            vector(&format!("{name}.oriented.bin")),
            "{name}"
        );
    }
}

#[test]
fn deflate_tiff_decodes() {
    let fake = FakeSimplon::start();
    let tif = vector("mono16_deflate.tif");
    fake.with(|s| s.monitor_tiff = Some(tif.clone()));
    let d = detector(&fake);
    let f = d.monitor_frame().unwrap().unwrap();
    assert_eq!((f.rows, f.cols), (64, 48));
    assert_eq!(f.pixels.to_le_bytes(), vector("mono16.oriented.bin"));
    assert_eq!(d.last_monitor_tiff().as_deref(), Some(&tif));
}

#[test]
fn undecodable_tiff_is_a_decode_error_and_bytes_are_kept() {
    let fake = FakeSimplon::start();
    let tif = b"not a tiff".to_vec();
    fake.with(|s| s.monitor_tiff = Some(tif.clone()));
    let d = detector(&fake);
    let e = d.monitor_frame().unwrap_err();
    assert!(matches!(e, DriverError::Decode { .. }), "{e:?}");
    assert_eq!(d.last_monitor_tiff().as_deref(), Some(&tif));
}

#[test]
fn monitor_frame_fetch_408_cache_and_metadata() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    // nothing yet: 408 and no cached frame → None
    assert!(d.monitor_frame().unwrap().is_none());
    fake.with(|s| s.monitor_tiff = Some(vector("mono16.tif")));
    let f = d.monitor_frame().unwrap().unwrap();
    assert_eq!((f.rows, f.cols), (64, 48));
    assert_eq!(f.pixels.to_le_bytes(), vector("mono16.oriented.bin"));
    assert!(d
        .last_monitor_metadata()
        .contains(&("IncidentEnergy".to_owned(), "200000".to_owned())));
    // 408 → the cached oriented frame (no black flicker)
    fake.with(|s| s.monitor_tiff = None);
    let g = d.monitor_frame().unwrap().unwrap();
    assert!(Arc::ptr_eq(&f, &g));
    // other HTTP errors raise (the adapter turns them into zeros + one WARNING)
    fake.rule(
        None,
        "images/monitor",
        Action::Status(500, String::new()),
        Some(1),
    );
    assert!(matches!(
        d.monitor_frame(),
        Err(DriverError::Http { status: 500, .. })
    ));
}

const IFD_VECTORS: [&str; 3] = ["dectris_ifd_le", "dectris_ifd_be", "dectris_ifd_bigtiff"];

fn expected_metadata(name: &str) -> Vec<(String, String)> {
    sidecar(name)["metadata"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p[0].as_str().unwrap().to_owned(),
                p[1].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// Tag 51192 as LONG (classic LE), IFD (classic BE) and IFD8 (BigTIFF) offset to the DECTRIS private
/// IFD (vectors from `tests/vectors/gen_dectris_ifd_tiffs.py`).
#[test]
fn dectris_private_ifd_vectors_give_string_metadata() {
    for name in IFD_VECTORS {
        let img = TiffCrateDecoder
            .decode(&vector(&format!("{name}.tif")))
            .unwrap();
        let meta = sidecar(name);
        assert_eq!(img.width as u64, meta["width"].as_u64().unwrap(), "{name}");
        assert_eq!(
            img.height as u64,
            meta["height"].as_u64().unwrap(),
            "{name}"
        );
        assert_eq!(img.metadata, expected_metadata(name), "{name}");
    }
}

#[test]
fn dectris_private_ifd_through_monitor_frame() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    fake.with(|s| s.monitor_tiff = Some(vector("dectris_ifd_be.tif")));
    assert!(d.monitor_frame().unwrap().is_some());
    let kv = d.last_monitor_metadata();
    assert!(kv.contains(&("ExposureTime".to_owned(), "0.01".to_owned())));
    assert!(kv.contains(&("IncidentEnergy".to_owned(), "200000".to_owned())));
    assert_eq!(kv, expected_metadata("dectris_ifd_be"));
}

/// A 51192 offset outside the file (or inside the header) → the image still decodes, metadata empty.
#[test]
fn dectris_private_ifd_bad_offset_gives_empty_metadata() {
    for name in IFD_VECTORS {
        let good = vector(&format!("{name}.tif"));
        let pos = sidecar(name)["dectris_value_pos"].as_u64().unwrap() as usize;
        let width = if name.contains("bigtiff") { 8 } else { 4 };
        let be = name.ends_with("_be");
        for bad in [0u64, 2, good.len() as u64 + 100, u64::from(u32::MAX)] {
            let mut b = good.clone();
            let field = if width == 8 {
                bad.to_le_bytes().to_vec()
            } else if be {
                (bad as u32).to_be_bytes().to_vec()
            } else {
                (bad as u32).to_le_bytes().to_vec()
            };
            b[pos..pos + width].copy_from_slice(&field);
            let img = TiffCrateDecoder.decode(&b).unwrap();
            assert!(img.metadata.is_empty(), "{name} offset {bad}");
        }
    }
}

#[test]
fn dectris_private_ifd_truncated_never_panics() {
    for name in IFD_VECTORS {
        let data = vector(&format!("{name}.tif"));
        for cut in 0..data.len() {
            let _ = TiffCrateDecoder.decode(&data[..cut]);
        }
    }
}

/// A Monitor TIFF saved from a DCU, named by `DECTRIS_MONITOR_SAMPLE` (read at test time, never
/// copied into the repo). Prints SKIP and passes when the variable is not set.
#[test]
fn monitor_sample_private_ifd() {
    let Ok(path) = std::env::var("DECTRIS_MONITOR_SAMPLE") else {
        eprintln!("SKIP monitor_sample_private_ifd: DECTRIS_MONITOR_SAMPLE not set");
        return;
    };
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("SKIP monitor_sample_private_ifd: {path} not found");
        return;
    };
    let img = TiffCrateDecoder.decode(&bytes).unwrap();
    eprintln!("Monitor sample metadata: {:?}", img.metadata);
    assert_eq!((img.width, img.height), (1028, 1062));
    let kv: std::collections::HashMap<_, _> = img.metadata.iter().cloned().collect();
    // every code resolves to a vendor name, in vendor-table order (IncidentWavelength and
    // DetectorDistance are [OPTIONAL] in the vendor table, so presence is not pinned)
    let vendor_order = [
        "IfdVersion",
        "SeriesUniqueId",
        "SeriesNumber",
        "ImageNumber",
        "ImageDateTime",
        "ThresholdId",
        "ThresholdEnergy",
        "ExposureTime",
        "IncidentEnergy",
        "IncidentWavelength",
        "LostPixelCount",
        "BeamCenter",
        "DetectorDistance",
    ];
    let names: Vec<_> = img.metadata.iter().map(|(k, _)| k.as_str()).collect();
    assert!(names.iter().all(|k| !k.starts_with("Tag")), "{names:?}");
    let present: Vec<_> = vendor_order
        .iter()
        .copied()
        .filter(|n| names.contains(n))
        .collect();
    assert_eq!(names, present);
    for key in ["SeriesUniqueId", "ImageDateTime", "BeamCenter"] {
        assert!(kv.contains_key(key), "{key} missing: {names:?}");
    }
    for key in ["ExposureTime", "IncidentEnergy"] {
        let v = kv.get(key).unwrap();
        assert!(v.parse::<f64>().unwrap() > 0.0, "{key}={v}");
    }
    assert!(!kv["SeriesUniqueId"].contains('\0'));
    assert!(kv["ImageDateTime"].ends_with('Z'));
    assert_eq!(kv["BeamCenter"].split(',').count(), 2);
}

#[test]
fn monitor_rate_limit_is_ten_hz() {
    let fake = FakeSimplon::start();
    fake.with(|s| s.monitor_tiff = Some(vector("mono8.tif")));
    let d = detector(&fake);
    let t0 = Instant::now();
    for _ in 0..11 {
        d.monitor_frame().unwrap();
    }
    let el = t0.elapsed();
    assert!(el >= Duration::from_millis(990), "{el:?}");
    assert!(el < Duration::from_millis(2500), "{el:?}");
    assert_eq!(fake.count("GET monitor/images/monitor"), 11);
    // option: 20 Hz
    let fake = FakeSimplon::start();
    let d = detector_with(
        &fake,
        Settings {
            monitor_max_hz: 20.0,
            ..Settings::default()
        },
    );
    let t0 = Instant::now();
    for _ in 0..11 {
        let _ = d.monitor_frame();
    }
    let el = t0.elapsed();
    assert!(
        el >= Duration::from_millis(490) && el < Duration::from_millis(990),
        "{el:?}"
    );
}

/// [G1-J] offline: zeros every time (adapter), one error then `None`, and the rate limit holds.
#[test]
fn g1j_offline_monitor_keeps_rate_limit_and_goes_silent() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let mut b = common::fast_budgets();
    b.backoff = Duration::from_secs(5);
    let d = Detector::builder(format!("http://127.0.0.1:{port}"))
        .budgets(b)
        .build()
        .unwrap();
    let t0 = Instant::now();
    let mut errors = 0;
    let mut nones = 0;
    for _ in 0..50 {
        match d.monitor_frame() {
            Err(_) => errors += 1,
            Ok(None) => nones += 1,
            Ok(Some(_)) => panic!("no frame offline"),
        }
    }
    let el = t0.elapsed();
    assert_eq!(
        errors, 1,
        "one failure, then the back-off answers without I/O"
    );
    assert_eq!(nones, 49);
    assert!(el >= Duration::from_millis(4850), "{el:?}");
}

#[derive(Debug)]
struct FixedDecoder;
impl MonitorDecoder for FixedDecoder {
    fn decode(&self, _bytes: &[u8]) -> dectris_common::Result<DecodedImage> {
        Ok(DecodedImage {
            height: 1,
            width: 2,
            pixels: Pixels::U32(vec![7, 9]),
            metadata: vec![],
        })
    }
}

#[test]
fn decoder_is_swappable() {
    let fake = FakeSimplon::start();
    fake.with(|s| s.monitor_tiff = Some(b"anything".to_vec()));
    let d = Detector::builder(fake.endpoint())
        .budgets(common::fast_budgets())
        .decoder(Arc::new(FixedDecoder))
        .build()
        .unwrap();
    let f = d.monitor_frame().unwrap().unwrap();
    assert_eq!((f.rows, f.cols), (2, 1));
    assert_eq!(f.pixels, Pixels::U32(vec![9, 7]));
}

#[test]
fn monitor_shutdown_interrupts_rate_limit_sleep() {
    let fake = FakeSimplon::start();
    let d = Arc::new(detector_with(
        &fake,
        Settings {
            monitor_max_hz: 0.5,
            ..Settings::default()
        },
    ));
    let _ = d.monitor_frame();
    let d2 = Arc::clone(&d);
    let h = std::thread::spawn(move || {
        let t0 = Instant::now();
        (d2.monitor_frame(), t0.elapsed())
    });
    std::thread::sleep(Duration::from_millis(100));
    d.request_stop();
    let (r, el) = h.join().unwrap();
    assert!(matches!(r, Err(DriverError::Shutdown)));
    assert!(el < Duration::from_millis(600));
}

// ---------------------------------------------------------------------------------------------
// FileWriter
// ---------------------------------------------------------------------------------------------

#[test]
fn configure_filewriter_order_and_405_skip() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    fake.rule(
        Some("PUT"),
        "config/image_nr_start",
        Action::Status(405, String::new()),
        None,
    );
    let cfg = FilewriterConfig {
        name_pattern: "manual_$id".into(),
        ..FilewriterConfig::default()
    };
    d.configure_filewriter(&cfg).unwrap();
    let keys: Vec<_> = fake
        .keys()
        .into_iter()
        .filter(|k| k.starts_with("PUT filewriter/config/"))
        .collect();
    assert_eq!(
        keys,
        vec![
            "PUT filewriter/config/mode",
            "PUT filewriter/config/name_pattern",
            "PUT filewriter/config/nimages_per_file",
            "PUT filewriter/config/compression_enabled",
            "PUT filewriter/config/image_nr_start",
        ]
    );
    let fw = fake.with(|s| s.filewriter.clone());
    assert_eq!(fw.get("name_pattern"), Some(&json!("manual_$id")));
    assert_eq!(fw.get("nimages_per_file"), Some(&json!(1000)));
    assert_eq!(fw.get("compression_enabled"), Some(&json!(true)));
    // other errors raise
    fake.rule(
        Some("PUT"),
        "config/nimages_per_file",
        Action::Status(500, String::new()),
        None,
    );
    assert!(matches!(
        d.configure_filewriter(&cfg),
        Err(DriverError::Http { status: 500, .. })
    ));
    d.disable_filewriter();
    assert_eq!(
        fake.with(|s| s.filewriter.get("mode").cloned()),
        Some(json!("disabled"))
    );
}

#[test]
fn download_waits_for_the_filewriter_to_finish() {
    // Data files are listed only when complete: a listing during `acquire` can miss them.
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    fake.with(|s| {
        s.filewriter_state = "acquire".into();
        s.files.push(("s_master.h5".into(), vec![1]));
    });
    let dir = std::env::temp_dir().join(format!("simplon-fw-wait-{}", std::process::id()));
    let got = std::thread::scope(|sc| {
        sc.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            fake.with(|s| {
                s.files.push(("s_data_000001.h5".into(), vec![2]));
                s.filewriter_state = "ready".into();
            });
        });
        d.download_filewriter_files(&dir).unwrap()
    });
    assert_eq!(got.len(), 2, "{got:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn filewriter_files_download_and_clear() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    assert!(d.get_filewriter_files().is_empty());
    fake.with(|s| {
        s.files.push(("s_master.h5".into(), vec![1, 2, 3]));
        s.files.push(("s_data_000001.h5".into(), vec![9; 200_000]));
        s.files.push(("../evil.h5".into(), vec![0]));
        s.files.push(("gone.h5".into(), vec![0]));
    });
    fake.rule(
        Some("GET"),
        "/data/gone.h5",
        Action::Status(404, String::new()),
        None,
    );
    assert_eq!(d.get_filewriter_files().len(), 4);
    let dir = std::env::temp_dir().join(format!("simplon-fw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let got = d.download_filewriter_files(&dir.join("sub")).unwrap();
    let names: Vec<_> = got
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["s_master.h5", "s_data_000001.h5"]);
    assert_eq!(std::fs::read(&got[1]).unwrap().len(), 200_000);
    assert!(!dir.join("evil.h5").exists());
    assert_eq!(
        fake.count("GET data/../evil.h5"),
        0,
        "unsafe name never requested"
    );
    d.clear_filewriter();
    assert!(d.get_filewriter_files().is_empty());
    // listing errors → [] (never raises)
    fake.rule(
        Some("GET"),
        "status/files",
        Action::Status(500, String::new()),
        None,
    );
    assert!(d.get_filewriter_files().is_empty());
    assert!(d.download_filewriter_files(&dir).unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
