//! Plain HTTP for `cam.session`: the SIMPLON parameters the driver has no method for (the GUI's
//! Dectris menu, the readout time cRED asks for).
//!
//! Separate from [`crate::Client`], like a plain `requests.Session`: no initialize gate, no
//! offline back-off, and any status is a [`Reply`], for the caller to judge.

use std::time::Duration;

use dectris_common::Result;

use crate::client::{header_safe_user_agent, map_ureq_error, Method, Reply};

/// Largest response read: a `flatfield` GET returns the whole array as JSON.
const BODY_LIMIT: u64 = 256 * 1024 * 1024;

#[derive(Debug)]
pub struct PlainSession {
    agent: ureq::Agent,
}

impl PlainSession {
    pub fn new(user_agent: &str) -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_idle_connections_per_host(4)
            .user_agent(header_safe_user_agent(user_agent))
            .accept("application/json")
            .build();
        PlainSession {
            agent: ureq::Agent::new_with_config(config),
        }
    }

    pub fn get(&self, url: &str, budget: Duration) -> Result<Reply> {
        let map = |e| map_ureq_error(e, Method::Get, url, budget);
        let resp = self
            .agent
            .get(url)
            .config()
            .timeout_global(Some(budget))
            .build()
            .call()
            .map_err(map)?;
        Self::reply(resp, map)
    }

    /// `json`: the body as JSON text; `None` sends no body.
    pub fn put(&self, url: &str, json: Option<&str>, budget: Duration) -> Result<Reply> {
        let map = |e| map_ureq_error(e, Method::Put, url, budget);
        let builder = self
            .agent
            .put(url)
            .config()
            .timeout_global(Some(budget))
            .build();
        let resp = match json {
            Some(body) => builder
                .header("Content-Type", "application/json")
                .send(body)
                .map_err(map)?,
            None => builder.send_empty().map_err(map)?,
        };
        Self::reply(resp, map)
    }

    fn reply(
        mut resp: ureq::http::Response<ureq::Body>,
        map: impl Fn(ureq::Error) -> dectris_common::DriverError,
    ) -> Result<Reply> {
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_vec()
            .map_err(map)?;
        Ok(Reply { status, body })
    }
}
