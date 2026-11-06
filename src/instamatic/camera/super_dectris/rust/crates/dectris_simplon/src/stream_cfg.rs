//! Stream interface config endpoints: mode, format, header_detail, header_appendix
//! (docs/DESIGN.md §4.1, §4.2 step 6 [G2-M]; PROTOCOL §3.6, §6.4, §15 #33).

use std::sync::atomic::Ordering;

use dectris_common::{dlog, DriverError, Result};
use serde_json::{json, Value};

use crate::client::{is_status, Module, Task};
use crate::detector::Detector;

const HA_UNKNOWN: u8 = 0;
const HA_OBJECT: u8 = 1;
const HA_STRING: u8 = 2;

impl Detector {
    fn stream_url(&self, task: Task, key: &str) -> String {
        self.client.url(Module::Stream, task, key)
    }

    /// PUT one stream config key; 405 → GET (5 s) + compare (WARNING on mismatch, still success);
    /// other errors → `Err`.
    fn stream_put_405(&self, key: &str, value: &str) -> Result<()> {
        let url = self.stream_url(Task::Config, key);
        match self
            .client
            .put_config(&url, json!(value), self.budgets().put)
        {
            Ok(_) => Ok(()),
            Err(e) if is_status(&e, 405) => {
                match self.client.get_string(&url, self.budgets().get_slow) {
                    Ok(got) if got == value => {}
                    Ok(got) => dlog!(
                        Warn,
                        "stream {key}: PUT returned 405; DCU holds {got:?}, wanted {value:?}"
                    ),
                    Err(e2) => dlog!(
                        Warn,
                        "stream {key}: PUT returned 405 and the readback failed: {e2}"
                    ),
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// `enable_stream_v2() -> bool`: PUT `mode` (= `stream_mode`, default `enabled`), `format`
    /// (= `stream_format`, default `cbor`) and `header_detail` only if `stream_header_detail` is
    /// set; 405 → GET + compare. True iff every PUT returned 200 or 405. Never raises (PROTOCOL §3.6).
    pub fn enable_stream_v2(&self) -> bool {
        let s = self.settings();
        let mut ok = true;
        let mut steps: Vec<(&str, String)> = vec![
            ("mode", s.stream_mode.clone()),
            ("format", s.stream_format.clone()),
        ];
        if let Some(hd) = s.stream_header_detail.clone() {
            steps.push(("header_detail", hd));
        }
        for (key, value) in steps {
            if let Err(e) = self.stream_put_405(key, &value) {
                dlog!(
                    Warn,
                    "enable_stream_v2: setting stream {key}={value} failed: {e}"
                );
                ok = false;
            }
        }
        if ok {
            dlog!(
                Info,
                "Stream mode set to: {} ({})",
                s.stream_mode,
                s.stream_format
            );
            self.remember_stream_mode(Some(s.stream_mode));
        }
        ok
    }

    /// `disable_stream_v2()`: PUT `mode disabled` (5 s), WARNING on failure; never raises.
    pub fn disable_stream_v2(&self) {
        match self.client.put_config(
            &self.stream_url(Task::Config, "mode"),
            json!("disabled"),
            self.budgets().put,
        ) {
            Ok(_) => {
                dlog!(Info, "Stream mode set to: disabled");
                self.remember_stream_mode(Some("disabled".into()));
            }
            Err(e) => dlog!(Warn, "disable_stream_v2 failed: {e}"),
        }
    }

    /// `get_stream_mode()`: one of the four Tk-polled getters (1 s, back-off aware, initialize in
    /// flight → last good value); `'unknown'` on error; never raises.
    pub fn get_stream_mode(&self) -> String {
        if self.init_in_flight() {
            return self
                .remember_stream_mode(None)
                .unwrap_or_else(|| "unknown".into());
        }
        if self.client.in_backoff() {
            return "unknown".into();
        }
        match self.client.get_string(
            &self.stream_url(Task::Config, "mode"),
            self.budgets().poll_get,
        ) {
            Ok(m) => {
                self.remember_stream_mode(Some(m.clone()));
                m
            }
            Err(e) => {
                dlog!(Debug, "get_stream_mode failed: {e}");
                "unknown".into()
            }
        }
    }

    /// `set_config` step 6: PUT stream `header_appendix` (5 s, WARNING on failure). [G2-M] On a
    /// non-200 for the object form, retry once with `{"value": json.dumps(obj)}`; remember which
    /// form worked for the rest of the session. Returns true on success.
    pub fn put_header_appendix(&self, appendix: &Value) -> bool {
        let url = self.stream_url(Task::Config, "header_appendix");
        let budget = self.budgets().put;
        let as_string = match appendix {
            Value::String(s) => Value::String(s.clone()),
            other => Value::String(other.to_string()),
        };
        let form = self.header_appendix_form.load(Ordering::SeqCst);
        if appendix.is_string() || form == HA_STRING {
            return match self.client.put_config(&url, as_string, budget) {
                Ok(_) => true,
                Err(e) => {
                    dlog!(Warn, "header_appendix PUT failed: {e}");
                    false
                }
            };
        }
        match self.client.put_config(&url, appendix.clone(), budget) {
            Ok(_) => {
                self.header_appendix_form.store(HA_OBJECT, Ordering::SeqCst);
                true
            }
            Err(e @ DriverError::Http { .. }) if form == HA_UNKNOWN => {
                dlog!(
                    Debug,
                    "header_appendix object form rejected ({e}); retrying as a JSON string"
                );
                match self.client.put_config(&url, as_string, budget) {
                    Ok(_) => {
                        self.header_appendix_form.store(HA_STRING, Ordering::SeqCst);
                        dlog!(Info, "header_appendix accepted as a JSON string; using that form from now on");
                        true
                    }
                    Err(e2) => {
                        dlog!(
                            Warn,
                            "header_appendix PUT failed (object: {e}; string: {e2})"
                        );
                        false
                    }
                }
            }
            Err(e) => {
                dlog!(Warn, "header_appendix PUT failed: {e}");
                false
            }
        }
    }
}
