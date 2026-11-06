//! FileWriter config and file listing / download endpoints (docs/DESIGN.md §2.3, §4.1
//! [#12 keep]; PROTOCOL §6.6, §12).

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use dectris_common::{dlog, DriverError, Result};
use serde_json::{json, Value};

use crate::client::{is_status, Module, Task};
use crate::detector::Detector;

/// Arguments of `configure_filewriter` (SIMPLON key names; defaults per DESIGN §11.6).
#[derive(Debug, Clone, PartialEq)]
pub struct FilewriterConfig {
    pub mode: String,
    pub name_pattern: String,
    pub nimages_per_file: u64,
    pub compression_enabled: bool,
    pub image_nr_start: u64,
}

impl Default for FilewriterConfig {
    fn default() -> Self {
        FilewriterConfig {
            mode: "enabled".into(),
            name_pattern: "series_$id".into(),
            nimages_per_file: 1000,
            compression_enabled: true,
            image_nr_start: 1,
        }
    }
}

/// A DCU file name is used as a local file name only if it is a plain name (no separators, no
/// `..`, no drive prefix).
pub fn safe_file_name(name: &str) -> Option<&str> {
    let ok = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', ':', '\0'])
        && !name.starts_with("..");
    ok.then_some(name)
}

impl Detector {
    fn fw_url(&self, task: Task, key: &str) -> String {
        self.client.url(Module::Filewriter, task, key)
    }

    /// `configure_filewriter(...)`: PUT order `mode, name_pattern, nimages_per_file,
    /// compression_enabled, image_nr_start` (5 s each); 405 → WARNING + skip the key; other errors
    /// raise (ERROR).
    pub fn configure_filewriter(&self, cfg: &FilewriterConfig) -> Result<()> {
        dlog!(
            Info,
            "Configuring FileWriter: mode={}, name_pattern={}, nimages_per_file={}, compression_enabled={}, image_nr_start={}",
            cfg.mode, cfg.name_pattern, cfg.nimages_per_file, cfg.compression_enabled, cfg.image_nr_start
        );
        let steps: [(&str, Value); 5] = [
            ("mode", json!(cfg.mode)),
            ("name_pattern", json!(cfg.name_pattern)),
            ("nimages_per_file", json!(cfg.nimages_per_file)),
            ("compression_enabled", json!(cfg.compression_enabled)),
            ("image_nr_start", json!(cfg.image_nr_start)),
        ];
        for (key, value) in steps {
            match self.client.put_config(
                &self.fw_url(Task::Config, key),
                value.clone(),
                self.budgets().put,
            ) {
                Ok(_) => {}
                Err(e) if is_status(&e, 405) => {
                    dlog!(Warn, "FileWriter {key}: PUT returned 405; skipped")
                }
                Err(e) => {
                    dlog!(Error, "Failed to set FileWriter {key}={value}: {e}");
                    return Err(e);
                }
            }
        }
        let on = cfg.mode == "enabled";
        self.filewriter_on.store(on, Ordering::SeqCst);
        self.filewriter_expect_data
            .store(on && cfg.nimages_per_file > 0, Ordering::SeqCst);
        Ok(())
    }

    /// `disable_filewriter()`: PUT `mode disabled` (5 s), WARNING on failure; never raises.
    pub fn disable_filewriter(&self) {
        self.filewriter_on.store(false, Ordering::SeqCst);
        match self.client.put_config(
            &self.fw_url(Task::Config, "mode"),
            json!("disabled"),
            self.budgets().put,
        ) {
            Ok(_) => dlog!(Info, "FileWriter disabled"),
            Err(e) => dlog!(Warn, "disable_filewriter failed: {e}"),
        }
    }

    /// `get_filewriter_files()` [#12 keep]: GET `status/files` (5 s); `[]` + WARNING on error;
    /// never raises.
    pub fn get_filewriter_files(&self) -> Vec<String> {
        match self
            .client
            .get_value(&self.fw_url(Task::Status, "files"), self.budgets().get_slow)
        {
            Ok(Value::Array(items)) => items
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            Ok(Value::Null) => Vec::new(),
            Ok(other) => {
                dlog!(Warn, "get_filewriter_files: unexpected value {other}");
                Vec::new()
            }
            Err(e) => {
                dlog!(Warn, "get_filewriter_files failed: {e}");
                Vec::new()
            }
        }
    }

    /// Wait (≤ the download budget) while FileWriter `status/state` is `acquire`: data files are
    /// listed only when complete. A failed GET ends the wait; a timeout is a WARNING.
    fn wait_filewriter_finished(&self) {
        let url = self.fw_url(Task::Status, "state");
        let b = self.budgets();
        let started = Instant::now();
        loop {
            match self.client.get_value(&url, b.get_slow) {
                Ok(Value::String(s)) if s == "acquire" => {}
                Ok(_) => return,
                Err(e) => {
                    dlog!(Debug, "FileWriter status/state failed: {e}");
                    return;
                }
            }
            if self.signals.shutting_down() {
                return;
            }
            if started.elapsed() >= b.download {
                dlog!(
                    Warn,
                    "FileWriter still writing after {:.0} s; downloading what is listed",
                    b.download.as_secs_f64()
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// `download_filewriter_files(destination) -> list[Path]`: mkdir; wait for the FileWriter to
    /// finish; for each listed file `GET /data/<name>` (120 s, 64 KiB chunks) into
    /// `destination/<name>`. Per-file failures are logged (ERROR) and skipped; no data file is a
    /// WARNING when one was expected. Raises only if the destination cannot be created.
    pub fn download_filewriter_files(&self, destination: &Path) -> Result<Vec<PathBuf>> {
        std::fs::create_dir_all(destination).map_err(|e| {
            DriverError::Internal(format!(
                "cannot create download directory {}: {e}",
                destination.display()
            ))
        })?;
        self.wait_filewriter_finished();
        let names = self.get_filewriter_files();
        if self.filewriter_expect_data.load(Ordering::SeqCst)
            && !names.iter().any(|n| n.contains("_data_"))
        {
            dlog!(Warn, "FileWriter lists no data file: {names:?}");
        }
        let mut out = Vec::new();
        for name in names {
            let Some(safe) = safe_file_name(&name) else {
                dlog!(
                    Error,
                    "FileWriter file name {name:?} is not a plain file name; skipped"
                );
                continue;
            };
            let dest = destination.join(safe);
            let url = self.client.data_url(safe);
            match self
                .client
                .download_to(&url, self.budgets().download, &dest)
            {
                Ok(n) => {
                    dlog!(Info, "Downloaded {name} ({n} bytes)");
                    out.push(dest);
                }
                Err(e) => dlog!(Error, "Failed to download {name}: {e}"),
            }
        }
        Ok(out)
    }

    /// `clear_filewriter()`: PUT `command/clear` (30 s), WARNING on failure; never raises.
    pub fn clear_filewriter(&self) {
        self.filewriter_expect_data.store(false, Ordering::SeqCst);
        match self.client.put_command(
            &self.fw_url(Task::Command, "clear"),
            None,
            self.budgets().arm,
        ) {
            Ok(_) => dlog!(Info, "FileWriter data cleared on the DCU"),
            Err(e) => dlog!(Warn, "clear_filewriter failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_safety() {
        assert_eq!(
            safe_file_name("series_1_master.h5"),
            Some("series_1_master.h5")
        );
        for bad in ["", ".", "..", "../x", "a/b", "a\\b", "C:x", "..h5"] {
            assert!(safe_file_name(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn defaults() {
        let c = FilewriterConfig::default();
        assert_eq!(c.name_pattern, "series_$id");
        assert_eq!(c.nimages_per_file, 1000);
        assert!(c.compression_enabled);
        assert_eq!(c.image_nr_start, 1);
    }
}
