# SINGLA, SIMPLON 1.8 and Stream V2: protocol and behaviour

This document describes what a DECTRIS SINGLA detector and its control unit speak and how they behave, and what the Rust driver does about it, as of the driver in this repository. It is written for someone who has to change or repair the driver. Comments in the Rust source cite its section numbers, in the form `PROTOCOL §3.2` or `PROTOCOL §15 #5`, so the numbers must stay as they are, and some numbers are skipped.

## Contents

- §1 REST conventions: URLs, verbs and bodies, the driver's HTTP client.
- §2 The detector's state machine: states, commands, transitions.
- §3 The driver's state policy: what it reads, and what it does in each state.
- §4 Trigger modes.
- §5 Timing parameters.
- §6 The endpoint catalogue.
- §7 Stream V2 transport: the socket, the queue, which series a message belongs to.
- §8 The CBOR data model and tag table.
- §9 Compression framing: bslz4 and lz4, byte-exact.
- §10 Frame and metadata semantics.
- §11 The Monitor preview path.
- §12 The FileWriter path.
- §13 The raw-frame path.
- §14 Logging conventions.
- §15 Contradictions and open questions.

## How to read this document

Each statement that could matter rests on one of these. Where they differ, the text says so. A paragraph headed **Driver:** describes the code.

| Mark | Meaning |
|---|---|
| **Manual** | The manufacturer's documents, listed below. |
| **Driver** | What the driver's code does. This is a fact about the code, not about the detector. |
| **Reported** | Reported from use with a detector; how it was measured is not recorded. Where the text says "a report", this is the source. |
| **Measured** | Measured on the development machine with synthetic frames. |
| **Open** | An open point, to look at on a detector. Section 15 collects the main ones. |

Manufacturer's documents, available from DECTRIS and not in this repository:

| Short name | Document |
|---|---|
| API | DECTRIS SIMPLON API Reference 1.8, `simplon_apireference_v1p8.pdf` |
| User Manual | SINGLA User Manual v1.8.4, `usermanual_singla.pdf` |
| Tech Spec | SINGLA Technical Specifications v1.8.4, `technicalspecifications_singla.pdf` |

Published by the manufacturer on GitHub: `dectris/documentation` (`stream_v2/README.md`, called the Stream V2 README here, `stream_v2/examples/`, `cbor/dectris-compression-tag.md`, called the tag 56500 note), not in this repository; and `dectris/compression`, whose release 0.3.1 is in this repository under `third_party/dectris-compression-0.3.1/` (see `third_party/README.md`). Its C test `compression_test.cc` and its Python tests are not in that release, so §9.6 and §9.7 cite them from GitHub. The bitshuffle format is defined by `kiyo-masui/bitshuffle`.

## Terms

| Term | Meaning |
|---|---|
| DCU | Detector control unit: the server that runs the detector. The driver talks to it over the network. |
| SIMPLON | The DCU's HTTP REST interface: URLs for reading and setting values and sending commands. |
| Stream V2 | The DCU's stream of image data: CBOR messages over ZeroMQ. |
| ZeroMQ PUSH / PULL | A message socket pair. The DCU pushes, the driver pulls. Each message arrives whole. |
| HWM | High-water mark: the most messages a ZeroMQ socket will hold in its queue. |
| CBOR | A binary encoding of maps, arrays, numbers and strings (RFC 8949). |
| Series | The images between one `arm` and the end of that acquisition. |
| Arm, trigger, disarm | Prepare the detector; start the exposures; end the series and finish the data. |
| count_time, frame_time | The exposure time of an image, and the time from the start of one image to the start of the next. |
| Monitor, FileWriter | Two other DCU outputs: single TIFF images on request, and HDF5 files stored on the DCU. |

---

## 1. REST conventions

### 1.1 URL scheme

A resource is `http://<dcu>/<module>/api/1.8.0/<task>/<parameter>` (API §2.2, §3.2.1). The modules are `detector`, `monitor`, `filewriter`, `stream`, `system`. The tasks are `config`, `status`, `command`; the Monitor also has `images`. In `1.8.0` the third number is "currently unused" (API §2.3). The driver has the version as one constant, `API_VERSION` in `client.rs`.

Files written by the FileWriter are outside this scheme: `http://<dcu>/data/<file>` (§6.6).

The host and port come from the camera yaml: `DCU_host`, `DCU_port` (HTTP, normally 80), `ZMQ_host`, `ZMQ_port` (31001 for Stream V2).

The DCU's network ports (Tech Spec §4.2.1, Table 4.3): `user1p1` 1 GBase-T with DHCP; `service` 1 GBase-T, a fallback with the static address `169.254.254.1` / `255.255.255.0`; `user2p1`, `user2p2` 10 GbE SFP+ with DHCP; `user3p1`, `user3p2` 100 Gb QSFP28. The manual warns: "The detector control unit does not provide authentication mechanisms and is not protected against malicious acts by unauthorized third parties." Keep it on a private network.

### 1.2 Verbs and bodies

GET reads, PUT sets. DELETE removes a resource where the API says so, for example a custom flatfield or pixel mask; the driver never uses it.

| Case | Format | Driver |
|---|---|---|
| GET config reply | JSON object with some of `value`, `value_type` (bool, float, int, string, list), `min`, `max`, `allowed_values` (an empty list means unrestricted), `unit`, `access_mode` (`rw` when absent). An invalid value is `null` or empty (API table). | Takes `value`. An object with no `value` key is used whole. |
| GET status reply | Adds `time` (ISO 8601), `state` (`invalid`, `normal`, `critical`, `disabled`), `critical_limits`, `critical_values`. | Takes `value`. |
| PUT config body | `{"value": <v>}`. "The data type in a put request must agree" with `value_type` (API §3.2). | `nimages` and `ntrigger` go as JSON integers. |
| PUT config reply | A JSON list: "Put will always return a list of all parameters implicitly and explicitly changed, or that could have been changed." For `count_time` the API's example lists `bit_depth_image`, `count_time`, `countrate_correction_count_cutoff`, `frame_count_time`, `frame_period`. | Used only for a DEBUG log line. Empty, absent or another shape is accepted. |
| Command | PUT on `/<module>/api/1.8.0/command/<name>`. Without inputs: "no JSON body should be sent. The only accepted empty body is `{}`." With inputs: `{"value": <v>}`. | Sends no body, or `{"value": <v>}`. |
| Command reply | `arm`, `disarm`, `cancel`, `abort` return `sequence_id`; `trigger`, `initialize`, `hv_enabled`, `hv_reset` nothing; `check_connections` an array. The API's `initialize` example answers 200 with an empty body. | Accepts an absent body, `{}` or an object. Reads the id from the key `sequence_id`, and also accepts the spelling `sequence id` (with a space). The value is a number or a nested `{"value": n}`; a bare number as the whole body is also read. The exact body is **Open**. |

**Success is HTTP 200 only** (API, Detector Command table): "If an error occurs a corresponding HTTP error code is returned instead." The driver treats any other status as an error, with the 405 exceptions stated in §6.1, §6.4 and §6.6. Before `initialize`, only the detector state exists: a config GET fails with "Parameter count_time does not exist" (API, worked example).

Undocumented keys may exist but "Using those keys is strongly discouraged." A module's keys can be listed at `<module config URI>/keys`. A GET of a data array may send `accept=image/tiff`.

### 1.3 The driver's HTTP client

**Driver** (`client.rs`):

- One `ureq` HTTP client, shared between threads. Connect timeout 1 s, up to 4 idle connections, and every request has its own time limit for the whole request (§6). `User-Agent` is built from the program's `__app_title__` and `__app_version__`; `Accept` is `application/json`.
- Response size limits: 32 MiB for JSON, 64 MiB for a Monitor TIFF.
- **Offline back-off.** A transport error or timeout on any request opens a 5 s window; any HTTP response, whatever its status, closes it. Only the four status getters (§6.1, §6.2, §6.4) and the Monitor look at the window: inside it the getters return `'unknown'` (or `None`) at once, and the Monitor returns its last frame, if it has one, so a missing DCU does not stall a thread that polls them.
- **Initialize gate.** While the driver's own `initialize` PUT is outstanding, every other request is refused without being sent. This follows the API's "Blocking operation, no other API operation may be performed" (§2.4). Getters answer from the last good value in that time.

`cam.session` and `cam.DCU_endpoint` remain for callers that build their own URLs, for example a detector menu or a read of `detector_readout_time`. That session has neither the gate nor the back-off.

### 1.4 darray (2-D array) JSON

`pixel_mask` and `flatfield` are exchanged over REST as:

```
"__darray__": <VERSION>, "type": <type>, "shape": [<width>,<height>], "filters": ["base64"], "data": <base64 encoded data>
```

`<VERSION>` is `[major, minor, patch]`; `<type>` is `"<u4"` or `"<f4"` (API table). The driver has no darray code: it never asks for these keys, since the stream carries them when wanted (§7.5).

---

## 2. Detector state machine

The states and commands of the `detector` module, what the manual says about them, and what is still open. The driver's response is in §3.

### 2.1 States

`status/state` is one of `na`, `ready`, `initialize`, `configure`, `acquire`, `idle`, `test`, `error`. "State is "na", when the DCU is booted or the acquisition service was restarted." `test` means `test_image_mode` is set to a non-empty string (API tables).

**Driver:** `get_detector_state()` also returns `'unknown'` if the GET fails, and `'initialize'` without a request while its own initialize runs.

### 2.2 Commands

The remarks are the API's words.

| Command | Returns | Remark |
|---|---|---|
| `abort` | `sequence_id` | "Aborts all operations and resets the system immediately. All data in the pipeline will be dropped." |
| `arm` | `sequence_id` | "Loads configuration to the detector and arms the trigger unit." |
| `cancel` | `sequence_id` | "Stops the data acquisition, but only after the next image is finished." |
| `check_connections` | array | "**Caution:** The detector needs to be re-initialized in order to resume operation! ..." The driver never sends it. |
| `disarm` | `sequence_id` | "Writes all data to file and disarms the trigger unit." |
| `hv_enabled` | none | "Turn high voltage on (true) or off (false). **Info:** Only available on specific detector types." |
| `hv_reset` | none | Resets the module high voltage for 1 to 600 s, 30 s by default. "Should only be used with CdTe detectors." The driver never sends it. |
| `initialize` | none | "Initializes the detector. **Caution:** Before initializing the detector only the detector state is available!" |
| `trigger` | none | "Starts data acquisition with the programmed trigger sequence. The trigger command can also accept an argument in the put request -- the count_time - if used in trigger_mode inte (internal enable)." |

### 2.3 Transitions

The manuals give no transition table. This one is put together, and the last column says from what.

| From | Command or event | To | Basis |
|---|---|---|---|
| `na` | `initialize` | `initialize`, then `idle` | **Manual** |
| `error` | `initialize` | `initialize`, then `idle` | **Manual**: no re-initialization needed "unless the detector entered an error status" |
| `idle` | `initialize` | `initialize`, then `idle` | **Open**. **Manual**: it resets `sequence_id`. |
| `idle` | config PUT | `configure`, then `idle` | **Reported**, **Open** |
| `idle` | `arm` | `ready` | **Open** for the state names; **Manual** for the meaning |
| `ready` | `trigger` | `acquire` | **Open** for the state names; **Manual** for the meaning |
| `ready` | `disarm`, `abort` | `idle` | **Open** |
| `ready` | `cancel` | `idle` | **Open**. The driver does not send `cancel` from `ready` (§3.3). |
| `acquire` | series done, more triggers allowed | `ready` | **Manual**: "stays armed until ntrigger are received" |
| `acquire` | series done, last trigger | `idle` | **Manual**: "The detector will disarm after any triggered (trigger_mode: ints, exts, ..) series has been completed." |
| `acquire` | `disarm`, `abort` | `idle` | **Open**. A `disarm` may wait for a running burst of images to finish, which is why the driver sends `cancel` first (§3.3). |
| `acquire` | `cancel` | `idle` | **Open**. The manual does not say what state follows. §15 #2. |

Effects on the other modules, from the API and User Manual §7.3.1: `arm` makes the FileWriter create the master file and the stream send `start`; `trigger` makes the stream send `image` messages; `disarm` finishes the files and the stream sends `end`.

### 2.4 Manufacturer's statements on operation

Every row is **Manual**. The last column says where in the manuals.

| Topic | Statement | Source |
|---|---|---|
| Initialize | "Mandatory only once after any of the following events: power-up of the detector; power-up of the detector control unit, restart of the DAQ service providing the SIMPLON API." "Depending on system configuration, this may take up to 2 minutes." "Blocking operation, no other API operation may be performed until successful completion." | API §3.1 |
| Re-initialize | "The detector does not need to be reinitialized unless the detector entered an error status." | User Manual §7.2 |
| Configure | Optional: "If nothing is configured, defaults will be used." | API §3.1 |
| Data interfaces | "If no interface is enabled (default) no data will be written. Enable at least one of" FileWriter, Stream, Monitor `mode`. They "do not preclude each other and can be activated simultaneously." | API §3.1, User Manual §7.1 |
| Arm | "uploads the configuration to the detector and prepares the system for data acquisition, but does not yet activate acquisition". "Depending on system configuration, this may take a few seconds. If the configuration is not changed the command will finish significantly faster on subsequent requests." | API §3.1 |
| Trigger | "Sending a "trigger" command is mandatory in software trigger mode (e.g ints, inte) and has to be omitted in external enabled modes (e.g exts, exte)." | API §3.1, Information #3 |
| Disarm | "Depending on trigger mode the last acquired image (or the image, if only one image was configured) is available only after a disarm command has been issued." | API §3.1, Information #4 |
| Repeating | Configure (optional), arm (mandatory), trigger ("mandatory for internal trigger, omit for external trigger/enable"), disarm ("optional as of firmware > 1.5.1"). | API §3.1 |
| Data lifespan | "Data not fetched within this lifespan is permanently lost." | User Manual §7.2, Information #3 |
| Stream buffering | "Streamed data is not buffered and will be lost if not fetched or incompletely fetched." | User Manual §7.1 |
| Monitor buffering | "a relatively small buffer. In case of buffer overflow, the oldest images will be overwritten." | User Manual §7.1 |
| Sequence id | "The sequence identification number is reset after initializing the detector." | User Manual §7.3.1 |
| Arm time | `data_collection_date` and the stream's `arm_date` are the time the arm command was issued. | API tables |
| Recording | 1 set up; 2 initialize if needed; 3 set parameters and enable interfaces; 4 arm; 5 trigger(s) and fetch data; 6 disarm "(to ensure files are finalized and closed)"; 7 repeat from 3 (different settings) or 4 (identical). | User Manual §7.2 |

What the driver takes from these: `initialize` gets a 130 s limit and blocks all other requests; a `disarm` is skipped when the detector is already `idle` (with one exception, §3.3); software triggers are refused in `exts` and `exte`; receivers stay open after `cancel` or `disarm`, because the last image and the `end` message may come afterwards.

### 2.5 High-voltage sub-state

`status/high_voltage/state` is `NA`, `OFF`, `RAMPING` or `READY`. "For optimal data quality acquisitions should only be started when state is READY." (API table.)

**Driver:** `get_hv_state()` returns that value or `'unknown'`, and does not hold back an `arm` for it. `hv_enabled(bool)` PUTs `command/hv_enabled` with `{"value": bool}` and raises on failure, so a caller can undo what it showed, such as a checkbox. Whether a SINGLA is one of the "specific detector types" is **Open** (§15 #22).

### 2.6 Configuration constraints that interact with the state machine

- "Always configure the ROI mode first before setting any acquisition specific configurations. For example count_time and frame_time are reset to default after setting a ROI mode!" `binning_mode` and `pixel_format` can change only "if the already configured frame rate is within the allowed bounds." (API table.) The driver never sets these three. A caller can, through `cam.session`.
- `bit_depth_image` and `bit_depth_readout` are read-only (API table). §5.4 explains them.
- `countrate_correction_applied` and the bit depth: a report says that `true` gives 32-bit frames and `false` 16-bit (**Reported**, not in the manual as such; **Open**). The driver PUTs the key and reads `bit_depth_image` for the log. It reads the frame type from each message, so nothing depends on this.
- Corrections are "enabled by default" and "disabling either correction will likely result in inferior data quality" (`countrate_correction_applied`, `flatfield_correction_applied`, `pixel_mask_applied`). "Changing photon_energy, threshold_energy or changing the counting_mode causes a reset of the flatfield." (User Manual §7.3.2, §7.4.1.) The driver writes only the first of these keys, and only when asked to.
- "If the settings or the external trigger/enable pulses applied are out of specification, acquisitions will not be performed and the measurement obtained with the detector might be incomplete." (User Manual §8.1.) So a 200 on `arm` or `trigger` does not promise frames; a receive timeout (§5.3) is the only sign the driver gets.
- Config PUTs in `idle` only: a project rule, not a sentence of the manual. `set_config` and `arm` go through ensure-idle first (§3.2). `set_trigger_mode`, `set_compression` and `set_countrate_correction` do not.
- `trigger_start_delay` "can be used in ints and exts mode." The driver does not use it.

### 2.7 Other modules' state

| Module | `status/state` | Manual | Notes |
|---|---|---|---|
| Stream | `disabled`, `ready`, `acquire`, `error` | "After the detector has been armed the state becomes acquire, after disarm, abort or cancel the state becomes ready. There are currently no error conditions." `status/dropped`: "Number of images that got dropped as not requested. After "arm" this number is reset to zero." `command/initialize`: "Resets the stream to its original state (i.e reset dropped images and errors, mode is set to disabled)." | Whether the *detector's* `initialize` also resets the stream is **Open** (§15 #32). |
| FileWriter | `disabled`, `ready`, `acquire`, `error` | `config/mode`: "During an active acquisition the interface remains enabled and will be disabled after receiving the end of series message." | The driver's download waits while this is `acquire` (§6.6). |
| Monitor | `normal`, `overflow` | "overflow if images were dropped due to a full buffer" | Runs apart from the detector state, according to a report (**Reported**). |

---

## 3. Driver state policy

This section is **Driver**: `crates/dectris_simplon/src/detector.rs`. The manual does not ask for any of it. It says what the driver does before and around each command, so that the program does not have to.

### 3.1 State reads

There are two. `get_detector_state()` is for a caller that polls: 1 s limit, consults the back-off (§1.3), never raises, returns `'unknown'` on failure. Inside the policy below, the driver reads `status/state` with a 5 s limit and ignores the back-off.

### 3.2 Ensure-idle recovery

Run before `arm` and before every `set_config`.

| State read | Action |
|---|---|
| `idle` | Nothing. |
| `ready`, `acquire` | From `acquire`, first PUT `cancel` (15 s; a failure is only logged), because a DCU may finish a running series before answering `disarm`. PUT `disarm` (30 s), read the state again. `idle`: done. `configure` or `initialize`: handle as in the rows below. Anything else, or a failed PUT: initialize. |
| `initialize` | An initialize not started by this driver is running. Wait for `idle`, up to 130 s, polling every 0.2 s, and never send a second one. If it ends `idle`, re-enable stream and Monitor and report "re-initialized". If still `initialize`: error. Another state: go through this table again, at most 3 times. |
| `configure` | Wait for `idle`, up to 10 s. If not: initialize. |
| `na`, `error`, `test`, `unknown`, anything else | Initialize. |

"Initialize" here needs the yaml option `auto_initialize` (default true); with it off, or if the initialize fails, ensure-idle raises `DetectorStateError`.

### 3.3 Each command

| Method | Sends | When | On failure |
|---|---|---|---|
| `disarm()` | `disarm`, limit 30 s (120 s when the FileWriter is on). From `acquire`, first `cancel` (15 s). | From `ready`, `acquire`. Also from `idle` when the FileWriter is on, because `disarm` "writes all data to file" and a collection stopped with `cancel` arrives here `idle`. Whether a DCU accepts `disarm` in `idle` is **Open**. Other states: nothing is sent. `initialize`, `configure`: waits up to 10 s, sends nothing. | Logs ERROR (WARNING for the FileWriter disarm from `idle`). Never raises. |
| `arm()` | Ensure-idle. If that re-initialized, the last configuration again. Then `arm` (30 s). | Always. | Logs ERROR and raises. |
| `trigger(timeout, count_time)` | `trigger` on a worker thread; body `{"value": count_time}` only when given. | `ints`, `inte` only. No state check: a wrong state shows up as the DCU's answer. See §4.4. | Blocking callers get the error. Non-blocking: logged at the next driver call. In `exts`, `exte`, or during an initialize: raises `DetectorStateError` with no request. |
| `cancel()` | `cancel` (15 s). | Only from `acquire`. No initialize afterwards. | Logs ERROR. Never raises. |
| `abort()` | `abort` (15 s). | Not from `idle`, `na`, `error`. The API allows it from any state; the driver skips those deliberately. | Logs ERROR. Never raises. |
| `initialize()` | `initialize` (130 s) on a worker thread, then waits for `idle` up to 30 s, then the steps of §3.6. | Any state. A second caller joins the one running. | Logs ERROR. Raises only on shutdown, an interrupt, or if the thread cannot start. |
| `set_config()` | Ensure-idle, then §6.1. | Always. | Core keys raise (§6.1). |

`disarm()`, `abort()` and `cancel()` set a cancel flag and wake waiting threads before any HTTP. From then until the next `arm`, no new trigger is sent.

### 3.4 Wait-for-idle timing

Polls `status/state` every 0.2 s until `idle`. A timeout is a WARNING and returns the last state seen; it is not an error. Limits: 10 s after `configure`, 30 s after an `initialize` whose PUT has returned.

### 3.5 Lifecycle

The usual path: `idle`, config PUTs, `arm`, `ready`, `trigger`, `acquire`, then `cancel` or the series' end, `disarm`, `idle`.

**At construction**, all best-effort, in this order: GET `status/state` (3 s), which on a transport failure gives a WARNING and skips the rest; GET `config/trigger_mode` and log `VERIFIED` or `MISMATCH` against the yaml, without writing it; `enable_stream_v2()`; GET `x_pixels_in_detector`, `y_pixels_in_detector` and WARN if they differ from the yaml's `dimensions`; a guarded `disarm()`, only if the state was `ready` or `acquire`; PUT Monitor `config/mode`. Then the ZeroMQ socket connects. The constructor never sends `initialize` and never PUTs detector config.

**At exit:** a guarded `disarm()`, unless the startup check found no DCU or an initialize is running.

### 3.6 Stream re-enable after initialize

After every `initialize` the driver sends, including inside ensure-idle, it runs `enable_stream_v2()`, PUTs Monitor `config/mode enabled` and clears the `ensure_config` key. The FileWriter mode is not re-sent. If the initialize happened inside the ensure-idle of an `arm`, that `arm` writes the last `set_config` values again before it arms. After an explicit `initialize()` nothing is re-written until the next `set_config` or `ensure_config`.

The manual says only that `stream/command/initialize` resets the mode. Whether the detector's `initialize` does too is **Open** (§15 #32). Re-enabling does no harm either way.

`enable_stream_v2()` returns a bool and never raises. It PUTs stream `config/mode` (yaml `stream_mode`, default `enabled`), `config/format` (yaml `stream_format`, default `cbor`), and `config/header_detail` only if yaml `stream_header_detail` is set. True if every PUT returned 200 or 405. On a 405 it reads the key back (5 s) and WARNs if the value differs, and that still counts as success.

### 3.7 Config cache and `frame_time`

`ensure_config(nimages, exposure, ntrigger=1, frame_time=None)` builds a key of `nimages`, `ntrigger` (after the mapping of §4.4-4), `exposure`, `frame_time`, `trigger_mode`, `compression`. If it equals the last key, nothing is written. Otherwise it runs `set_config` and `enable_stream_v2()`, and stores the key only if both succeed. `frame_time=0.0` counts the same as `None`. `set_trigger_mode`, `set_compression`, `set_config`, an initialize and a ZeroMQ reconnect all clear the key.

`set_config` always writes. A caller uses it, followed by `enable_stream_v2()`, to force a full write.

When the caller gives no `frame_time`, or 0, yaml `frame_time_policy` decides: `readout` (default) sends `exposure + detector_readout_time` (§5.3); `detector` sends no `frame_time` and leaves the DCU's value. The `readout` default exists because callers such as `get_image` and `get_movie` never pass a `frame_time`, and a longer exposure would otherwise conflict with the `frame_time` left by the last run.

---

## 4. Trigger modes

### 4.1 What all modes share

**Manual** (User Manual §7.3.1):

- "The detector always considers a trigger as the start of a series of n images."
- "Setting values greater than 1 for ntrigger allows several trigger commands or external trigger pulses per arm/disarm sequence. ... The resulting number of frames is product of ntrigger and nimages. In external enable modes the parameter nimages is ignored (i.e. always 1) and the number of frames therefore has to be configured using the detector configuration parameter ntrigger."
- `nimages` and `ntrigger` have `min` and `max` in their GET reply.

### 4.2 Each mode

| Mode | Manual | Use with this driver |
|---|---|---|
| `ints`, internal series | "An exposure (series) can be triggered by using a software trigger. This is the default mode of operation." The first exposure starts after the trigger is processed, with latency "hard to predict" since it comes over TCP/IP. The rest follow from `frame_time` and `count_time`. `nimages` frames per trigger, "stays armed until ntrigger are received." (§8.2) | A continuous collection: `nimages=99999`, one `trigger()` from a thread, stopped by `cancel()`, then `disarm()`. `get_image` and `get_movie` trigger by themselves. The yaml's default mode. |
| `inte`, internal enable | "a single image or series of images with varying exposure times can be started by issuing a number ntrigger of trigger commands." Each trigger may carry the `count_time` of the next frame, for example `{"value": 2.1}`. `nimages` is implied to be 1. "The configured count_time and frame_time should be close to the count time and frame time of the shortest expected exposure in the configured series." (§8.3) | `nimages=1`, `ntrigger=n`, one `trigger(count_time)` per frame from a worker thread. |
| `exts`, external series | One electrical pulse starts `nimages` frames, timed by `frame_time` and `count_time`. "stays armed until ntrigger are received." (§8.4) | No software trigger. |
| `exte`, external enable | "The count time as well as the period of individual frames of a series are defined by the duration of the high state of the external enable signal." `nimages` implied 1. "The detector records as many frames as valid (according to the specifications) enable pulses are received until the value set for ntrigger is reached." (§8.5) | No software trigger. `gate()` logs one WARNING and sends nothing, since no REST call can stand in for the signal. |

### 4.3 Electrical interface for exts and exte

**Manual** (Tech Spec Tables 3.2, 4.2). External trigger input: high 2.1 to 5.0 V, low 0.0 to 0.8 V, input impedance 47 kOhm. "Absolute maximum is 5 V. Applying a higher voltage will damage the detector." Trigger output: high 2.3 to 3.3 V, low 0.0 to 0.6 V, at most 24 mA. Connectors: `EXT IN`, Lemo Type 00 (NIM/CAMAC); `EN OUT`, "high when counting is enabled."

### 4.4 Driver rules

**Driver** (`detector.rs`, `trigger`). Item numbers are cited from the source.

1. `trigger()` takes an optional `count_time`, and sends `{"value": <float>}` only when it is given. Otherwise the PUT has no body.
2. The PUT runs on a worker thread that never touches Python, because a caller must be able to receive frames at the same time. A report says that on a detector the PUT blocks for the whole series in `ints` and about one `frame_time` in `inte` (**Reported**). The manual does not say so, and whether it blocks is **Open**. The driver works either way. A blocking caller (`trigger_blocking`, default true, is the yaml switch) waits for the PUT or for release by `cancel`, `disarm`, `abort`, the series' `end` or shutdown; once released, the PUT finishes by itself and its result is logged later. What a pending PUT does after `cancel` is **Open** (§15 #10).
3. `set_trigger_mode(mode)` raises `ValueError` unless `mode` is `ints`, `inte`, `exts` or `exte`. Otherwise it stores the mode, clears the config cache and tries the PUT (5 s; a failure is a WARNING, a 405 leads to a readback and comparison). Every `set_config` sends the current mode again.
4. In `inte` and `exte` the driver always sends `nimages=1`. If the caller passed `nimages > 1` with `ntrigger == 1`, it sends `ntrigger=<nimages>` instead, so the caller still gets that many frames. Other values of `ntrigger` are kept. (User Manual §7.3.1, §8.3, §8.5.)
5. Time limit of a trigger PUT, unless the caller gives one: `ints`, `nimages * frame_time + 60 s`; `inte`, `frame_time + 60 s`; both held between 30 s and 24 h. With no `set_config` in this process: 24 h in `ints`, 60 s in `inte`. Connect limit 5 s. At most 8 trigger workers at a time, beyond which `trigger` raises `DectrisBusy`.
6. How many PUTs go out, counted per `arm`: `inte` sends one at a time; another caller waits for the previous one. In `ints`, internal callers (the iterator of `get_movie` and `stream`, `acquire_single_frame`, the series of `get_image`, `arm_for_preview`, and a public `trigger()` when yaml `trigger_blocking` is false) send one per series, or up to `ntrigger` if `nimages == 1`. The Monitor live view never arms, so it sends no trigger. A public blocking `trigger()` is skipped with a WARNING once `ntrigger` have gone out.

Reported timings in `inte` (**Reported**): trigger round trip about 5 to 10 ms, gap between frames about 7 to 12 ms.

---

## 5. Timing parameters

### 5.1 Definitions

| Key | Access | API definition |
|---|---|---|
| `count_time` | rw | "Exposure time per image." |
| `frame_time` | rw | "Time interval between start of image acquisitions. This defines the speed of data collection and is the inverse of the frame rate" |
| `detector_readout_time` | r | "Readout dead time between consecutive detector frames." |
| `frame_count_time` | r | "Time interval between start of image acquisitions for sub-images when auto_summation is enabled." |
| `trigger_start_delay` | rw | "Delayed time from when the trigger comes until the configured series starts." |

### 5.2 Rules from the manual

- "The difference between the timing parameters frame_time and count_time has to be greater than the detector_readout_time." (User Manual §7.3.1)
- "detector_readout_time may change if photon_energy or threshold_energy is changed." (§7.4.2)
- "If frame_time conflicts with the current count_time, count_time is set to the difference of frame_time and detector_readout_time." "If count_time conflicts with frame_time, frame_time is set to the sum of count_time and detector_readout_time." `frame_count_time`, `frame_period` and `nframes_sum` may change with them. "To acquire images with a certain frame rate and best possible duty cycle, a simple procedure is to first set count_time to the inverse of the frame rate and subsequently frame_time to the inverse of the frame rate." (§7.4.2)
- Hardware (Tech Spec Table 3.1): 1028 x 1062 pixels (1 091 736), 75 um pixels, vertical gap between modules 38 pixels, image bit depth 8, 16, 32, readout bit depth 8, 16, maximum count rate 1e7 electrons per pixel per second, threshold 3.5 to 30 keV, electron energy 30 to 200 keV, one threshold, "continuous readout, with 100 ns dead time", maximum frame rate 2250 Hz at 16 bit and 4500 Hz at 8 bit.

The 100 ns is the counters' dead time and is a different thing from the API's `detector_readout_time`, which a report gives as typically about 0.004 s (**Reported**).

### 5.3 What the driver does with them

**Driver:**

- **Readout time.** `set_config` reads `detector_readout_time` (3 s) when it has to work out a `frame_time` (§3.7). If the read fails or the value is invalid, it uses `0.004` s (`FALLBACK_READOUT`) and WARNs.
- **Conflict between the two times.** The manual says the DCU rewrites the other value; a report says it refuses. **Open**, §15 #31. The write order of §6.1 is meant to end correctly under either, and the driver reads both values back afterwards, WARNing if either differs by more than 1e-6 s. The value read back becomes the effective `frame_time` used for the time limits below.
- **Receive window.** How long a receive waits for the next message: `2 * frame_time + 1 s`, held between 2 s and 60 s, with `frame_time` from the series' `start` message, else from the last `set_config`; with neither, the window is 2 s. The yaml's `receive_timeout` (seconds) replaces all of this. A timeout ends a frame iterator. An external trigger gap longer than the window also ends it, so set `receive_timeout` for slow `exts` or `exte` work.
- `message_receiver(timeout=2000)` takes its own window in ms.

Reported from use with a detector (**Reported**): first frame 0.6 to 0.9 s after trigger; `disarm` request about 50 to 200 ms; arm, trigger and disarm per frame about 500 ms; reconfiguring between series 0.5 to 2 s.

### 5.4 Readout, bit depth and auto-summation

**Manual** (User Manual §3.2.2, §3.2.3, §3.2.5):

- Two counters per pixel: "While one counter is being read out, data acquisition continues in the other counter."
- Auto-summation: "While a single frame is limited to the 8 bit, 16 bit of the digital counter, auto-summation extends the data depth up to 32 bit ..." `auto_summation` "Should always be enabled." (API table.) So `bit_depth_image` can read 32 while `bit_depth_readout` reads 16.
- 8-bit readout: "The 8-bit mode is automatically enabled as soon as the frame rate exceeds the maximum frame rate in 16-bit mode, as indicated in the Technical Specifications." "... there is no overflow protection and the counter will roll over if 255 counts are exceeded." The limit is 2250 Hz, that is a `frame_time` under about 0.44 ms.

**What the manual does not say:** that switch is of the *readout* (`bit_depth_readout`, "Bit depth of the internal readout"). `bit_depth_image` ("Bit depth of generated images") is a separate key, and the manual is silent on whether it follows above 2250 Hz. So whether frames then arrive as uint8 is **Open**.

**Driver:** takes the frame type from each message's tag (§10), never from a rate. It decodes uint8, and the decoder's tests include uint8 messages built for the purpose.

---

## 6. Endpoint catalogue

The requests the driver makes, with the time limit of each (`Budgets` in `client.rs`) and what it does on failure. "Best-effort" means log and carry on; "raise" means the error reaches the caller. Keys marked "not used" are listed because a maintainer may want them. The API has the complete list.

### 6.1 Detector config: `/detector/api/1.8.0/config/<key>`

| Key | Method | Limit | On failure | Notes |
|---|---|---|---|---|
| `frame_time` | PUT, first | 5 s | HTTP error: WARNING, carry on. Transport error: raise. | Only if a value is to be sent (§3.7). |
| `count_time` | PUT | 5 s | Raise | |
| `frame_time` | PUT, second | 5 s | Raise | Only if a value is to be sent. |
| `nimages`, `ntrigger` | PUT | 5 s | Raise | After the mapping of §4.4-4. |
| `trigger_mode` | PUT | 5 s | 405: GET (5 s) and compare, a failed GET raises. Other: raise. `set_trigger_mode` itself: best-effort. | |
| `compression` | PUT | 5 s | As `trigger_mode` | Only `bslz4`, `lz4`. "DECTRIS strongly advises using bslz4". `none` is not sent (§15 #5). |
| `count_time`, `frame_time` | GET | 3 s | WARNING | Readback at the end of `set_config`. |
| `detector_readout_time` | GET | 3 s | WARNING, uses 0.004 s | |
| `trigger_mode` | GET | 3 s | WARNING | At startup. |
| `x_pixels_in_detector`, `y_pixels_in_detector` | GET | 3 s | DEBUG | At startup, against the yaml. |
| `countrate_correction_applied` | GET | 1 s | `None` | For a caller that polls. |
| `countrate_correction_applied` | PUT | 5 s | WARNING | Then GET `bit_depth_image`. |
| `bit_depth_image` | GET | 3 s | 0 | Read-only. |

Order of `set_config`: ensure-idle, `frame_time` (best-effort), `count_time`, `frame_time`, `nimages`, `ntrigger`, `trigger_mode`, `compression`, stream `header_appendix`, the two readbacks. `frame_time` goes first as well as third so that the sequence ends right whether the DCU rewrites or refuses a conflict (§5.3). A failure part-way leaves the detector partly configured.

Not used by the driver: `photon_energy`, `threshold_energy` (changing either may change `detector_readout_time` and resets the flatfield); `auto_summation`; `counting_mode` (resets the flatfield); `flatfield_correction_applied`, `pixel_mask_applied`; `roi_mode`, `pixel_format`, `binning_mode`; `description`, `detector_number`, `eiger_fw_version`, `software_version`; `countrate_correction_count_cutoff`.

### 6.2 Detector status: `/detector/api/1.8.0/status/<key>`

| Key | Limit | On failure |
|---|---|---|
| `state`, for `get_detector_state()` | 1 s | `'unknown'` |
| `state`, inside the policy of §3 | 5 s (3 s at startup) | `'unknown'` |
| `high_voltage/state` | 1 s | `'unknown'` |

The four status getters are `get_detector_state` and `get_hv_state` (this section), `get_stream_mode` (§6.4) and `get_countrate_correction` (§6.1).

### 6.3 Detector commands: `/detector/api/1.8.0/command/<name>`

| Command | Body | Limit | On failure |
|---|---|---|---|
| `initialize` | none | 130 s, then wait for `idle` up to 30 s | Log |
| `arm` | none | 30 s | Log and raise |
| `trigger` | none, or `{"value": <count_time>}` | §4.4-5 | §3.3 |
| `disarm` | none | 30 s; 120 s with the FileWriter on | Log |
| `cancel`, `abort` | none | 15 s | Log |
| `hv_enabled` | `{"value": true or false}` | 5 s | Log and raise |

### 6.4 Stream: `/stream/api/1.8.0/...`

| Key | Method | Body | Limit | On failure | Notes |
|---|---|---|---|---|---|
| `config/mode` | PUT | `{"value": "enabled"}` | 5 s | 405: GET (5 s), WARNING if different, still success. Other: returns false, WARNING. | §3.6 |
| `config/format` | PUT | `{"value": "cbor"}` | 5 s | Same | `"legacy"` is Stream V1. |
| `config/header_detail` | PUT | `{"value": "all", "basic" or "none"}` | 5 s | Same | Only if yaml `stream_header_detail` is set. §7.5 |
| `config/mode` | GET | | 1 s | `'unknown'` | `get_stream_mode()` |
| `config/mode` | PUT | `{"value": "disabled"}` | 5 s | WARNING | `disable_stream_v2()` |
| `config/header_appendix` | PUT | `{"value": ""}` | 5 s | WARNING only | §15 #33 |

**`header_appendix`.** The API: "User data to be included in start event messages"; it surfaces as `user_data` in `start`. It is the last step of every `set_config`. The program always sends an empty string. It sends it, rather than nothing, because a DCU may keep the last value written, so a value left by another client would stay in every `start`; whether a DCU keeps it is **Open**. The code can also take a non-string value, sent as a JSON object first and, if the DCU answers with an HTTP error, once more as a JSON string, then remembered; the program does not use this path.

Not used: `config/image_appendix`; `status/state`; `status/dropped`, which is worth reading when frames go missing (§7.2); `command/initialize`. A combined PUT is possible, `{"mode": {"value": "enabled"}, "format": {"value": "cbor"}}` at `/stream/api/1.8.0/config` (Stream V2 README); the driver sends one key at a time. Stream V2 README defaults: `mode` disabled, `format` legacy, `header_detail` basic.

### 6.5 Monitor: `/monitor/api/1.8.0/...`

| Key | Method | Limit | On failure | Notes |
|---|---|---|---|---|
| `config/mode` | PUT `{"value": "enabled"}` | 3 s | WARNING | At startup and after an initialize. "The Monitor must be enabled before it can be used." |
| `images/monitor` | GET | 2 s | 408: the last frame, or nothing. Other: an error, which `get_preview_image` turns into a black frame (§11). | Returns TIFF. "Default waits for 500 ms for image. Timeout adjustable by adding query string ?timeout=<ms>". "Returns 408 if no image is available." |

Not used: `images/next` (the oldest image, removed from the buffer); `images/` and `images/<series>/<id>/<threshold_id>`; `config/buffer_size`, `config/discard_new`; `status/buffer_fill_level`, `buffer_free`, `dropped`, `state`; `command/clear`, `command/initialize`.

### 6.6 FileWriter: `/filewriter/api/1.8.0/...` and `/data/`

| Key | Method | Body | Limit | On failure | Notes |
|---|---|---|---|---|---|
| `config/mode` | PUT | `{"value": "enabled"}` | 5 s | 405: WARNING, skip the key. Other: raise. | First of five PUTs. Detector must be disarmed. |
| `config/name_pattern` | PUT | `{"value": <pattern>}` | 5 s | Same | `$id` becomes the sequence number. "The FileWriter will overwrite existing files with identical names". |
| `config/nimages_per_file` | PUT | `{"value": 1000}` | 5 s | Same | "Data files are only created when <nimages_per_file> is reached or when detector command disarm is sent". 0 puts all images in the master file. |
| `config/compression_enabled` | PUT | `{"value": true}` | 5 s | Same | "Compression is required for full detector performance" |
| `config/image_nr_start` | PUT | `{"value": 1}` | 5 s | Same | `image_nr_low` of the first data file. |
| `config/mode` | PUT | `{"value": "disabled"}` | 5 s | WARNING | |
| `status/state` | GET | | 5 s | Ends the wait | Polled every 100 ms before a download, while `acquire`, up to 120 s. |
| `status/files` | GET | | 5 s | `[]`, WARNING | File names on the DCU. |
| `/data/<filename>` | GET | | 120 s per file, 64 KiB chunks | ERROR for that file, next file. The partial file is removed. | No API prefix. The name is percent-encoded. A listed name that is not a plain file name (`/`, `\`, `:`, NUL, leading `..`) is skipped. |
| `command/clear` | PUT | none | 30 s | WARNING | "Drops all data (image data and directories) on the DCU." |

Not used: `command/initialize`; `config/format` (`"hdf5 nexus legacy nxmx"` or `"hdf5 nexus v2024.2 nxmx"`).

### 6.7 System: `/system/api/1.8.0/...`

The driver never calls this module. It has `datetime/*`, `network/<n>/*`, `system_info/service_tag` and the commands `create_log_tarball`, `delete_logs`, `reboot`, `restart`, `shutdown`. Note that `restart` and `reboot` put the detector into `na`.

---

## 7. Stream V2 transport

### 7.1 Socket

**Manual:** Stream V1 is ZeroMQ PUSH on port 9999, multipart, JSON and binary. Stream V2 is ZeroMQ PUSH on port 31001, one ZeroMQ message per Stream V2 message, CBOR, "available starting with release 2022.1". "Clients should connect to the server using a ZeroMQ PULL socket over TCP port 31001. Multiple clients may connect to the same server simultaneously. However, each message is only ever sent to a single client using a round-robin strategy."

So a second client connected to the DCU silently takes every other message. If frames go missing in a regular pattern, look for another program connected to port 31001.

**Driver** (`zmq_link.rs`): one PULL socket with its own context. Before `connect` it sets `RCVHWM` (§7.2), `RCVBUF` (§7.2), `LINGER 0`, and, if yaml `zmq_tcp_keepalive` is on, `TCP_KEEPALIVE 1` with idle 60 s. libzmq 4.3.5. `connect` is asynchronous: a wrong host or port does not fail, it shows up only as receive timeouts. Receives are a poll in slices of up to 250 ms followed by a non-blocking receive.

`guard.rs` keeps a second camera object in the same process from connecting to the same host and port: it stays disconnected until the first releases. Another process is not stopped. Host names are not resolved for this check, so a machine's name and its IP address count as different.

### 7.2 Receive queue and TCP buffer

| Setting | Driver default | Yaml key | Suggested |
|---|---|---|---|
| `ZMQ_RCVHWM`, messages the socket may queue | 500 | `zmq_rcvhwm` | The default; **4** for a caller that pairs frames with the tilt on arrival |
| `ZMQ_RCVBUF`, TCP receive buffer | 4 MB | `zmq_rcvbuf`; 0 leaves it to the operating system | The default |

**When 4.** A caller that reads the stage tilt when a frame arrives pairs a frame that waited in a queue with the wrong angle, so its queue must stay short. The driver's own default suits code that places a frame by its `image_id`, and `get_image`, whose series is one frame. Instamatic's own experiments need no change (DESIGN §11.4).

**Measured on the development machine with synthetic frames:**

- With 4, frames were lost from about 700 frames per second.
- At 500, with 0.344 MB per compressed frame, a full queue holds about 170 MB.
- With the operating system's TCP buffer, a fast transfer stalled for about 30 ms at a time on Windows. At 3000 frames per second for 5 s, 5 runs lost 8 to 151 frames; with 4 MB, 5 runs lost none. Sender and receiver were on one machine.

How much a DCU's sender queues, and whether it drops or waits, is **Open** (§15 #40). **Manual:** "Streamed data is not buffered and will be lost if not fetched", and `stream/status/dropped` counts what was dropped since `arm`. The driver does not read this key. When frames are lost, read it by hand and compare it with the gaps in `image_id`.

### 7.3 Stale messages, flush and reconnect

The PULL queue survives `arm` and `disarm`. Messages of a cancelled series, and its `end`, can still be waiting when the next series starts. A report also says the detector may send a few more frames after `disarm`, before `end` (**Reported**).

**Attribution.** **Driver** (`attribution.rs`): each message is matched to the series the driver armed, using the `sequence_id` in the `arm` reply against the message's `series_id`, by equality only, because the id restarts after `initialize`.

| Situation | `start` | `image` | `end` |
|---|---|---|---|
| Id known, same `series_id` | Cached once; its `series_unique_id` is remembered | Delivered, if the start was seen and the unique id matches | Ends the receive, if the start was seen and the unique id matches |
| Id known, other `series_id`, armed start not seen yet | Stale | Stale | Stale |
| Id known, other `series_id`, armed start seen | Adopted, with a WARNING: a newer series armed from outside the driver | Stale | Stale |
| Driver armed, no id in the reply | The first start is adopted | Stale | Stale |
| Driver has not armed in this process | Cached | Delivered | Ends the receive |

Stale images are dropped and counted; yaml `stale_series_policy: deliver` hands them over with `count_time` `None`. An image with the armed id that arrives before its `start` is stale, but if such images keep coming for one receive window they are delivered with `count_time` `None`.

**That `sequence_id` equals `series_id` is Open (§15 #38).** If a DCU's ids do not match, every frame would count as stale and receives would time out. Yaml `series_attribution: fifo` adopts the first start after each `arm` and ignores the id; `series_attribution: off` delivers everything.

**Flush.** `flush_zmq_buffer()` returns the number of messages drained. It takes what is queued and, once it has seen an image, keeps going until the stream has been quiet for 50 ms, since the rest of a backlog is still behind the queue. It stops at 10 000 messages or 2 s, because a running series can refill the queue. A drained `start` of the armed series is kept, and `arm` clears the cache instead. If a receiver is using the socket, the flush is skipped with a WARNING. The driver never flushes by itself, for example inside `arm`: the caller decides when.

A flush that stops at the first empty read can leave a backlog. A sender with a full queue then holds back the next `start`, and the next series times out with 0 frames. That is why the flush waits for quiet. Whether a DCU's sender behaves so is **Open** (§15 #40).

`reconnect_zmq()` closes the socket and opens a new one with the same settings, and clears the `ensure_config` key. If a receiver is busy, it is skipped.

### 7.4 Message order and the receivers

**Manual:** per series, `start` (sent at arm), then one `image` per exposure, then `end` (sent at disarm). The V1 text adds: "After passing "disarm", "cancel" or "abort", one message containing End of Series is sent".

**Driver:**

- `message_receiver(timeout=2000)`: returns `None` at once when the armed `start` has been received and cached, a dict for an armed image, the `_STREAM_END` object for the armed `end` (`None` means only "nothing yet"), and `None` after the timeout. So a call right after `arm` does not wait out its timeout.
- `receive_frames(n)`, `receive_frames_with_metadata(n)`, `receive_raw_frames(n)`: iterators that yield up to `n` frames and stop without an exception on the `end`, on `cancel`, or on a receive timeout. They never arm, trigger or disarm. `last_stop_reason` says why one stopped.
- `get_image` and `get_movie` run their own series. If the window expires with frames outstanding, they send a guarded `disarm` once and wait one more window, because the last image may come only after a `disarm` (§2.4); a second expiry raises `DectrisTimeout`. `get_image` then spends up to 50 ms draining what remains, and relies on the detector disarming itself after the series and on the next ensure-idle.
- `acquire_single_frame()` sends a trigger as §4.4-6 allows, and takes the newest queued frame when yaml `single_frame_fresh` is on, otherwise the oldest.
- A message whose `type` is missing or unknown gets one WARNING and is skipped.

### 7.5 `header_detail` and message sizes

**Manual:** `header_detail` selects the fields of `start`: `all`; `basic`, without the flatfield and pixel mask (the Stream V2 README adds: without the countrate tables); `none`, for which see §15 #6. The driver leaves it alone unless yaml `stream_header_detail` is set, so the DCU's value applies, `basic` by default according to the README.

With `all`, `start` carries `flatfield` (float32, tag 85), `pixel_mask` (uint32, tag 70) and `countrate_correction_lookup_table`. At 1028 x 1062 each full-frame array is 4 366 944 bytes before compression, so about 8.7 MB in one message. What a DCU sends with `none` is **Open** (§15 #6). The driver copes with a `start` that has no `count_time` or `frame_time`: `count_time` is then `None` for each frame, and the receive window takes `frame_time` from the last `set_config` (§5.3).

**Driver:** a `start` is kept whole, and its arrays are not decoded. They appear as `None` unless yaml `stream_decode_start_arrays` is on. Classifying a message reads only the top-level map and skips over `data` and those arrays.

**Sizes.** A compressed full frame from a detector is estimated at about 0.5 to 1 MB. The synthetic frames used on the development machine are 0.344 MB per compressed uint16 frame. A frame before compression is 4.4 MB at uint32, 2.2 MB at uint16. Rates over a site's network, for example 1 GbE, are **Open**.

---

## 8. CBOR data model and tag table

The structure of Stream V2 messages. Code: `crates/dectris_codec/src/cbor.rs`, `message.rs`, `typed_array.rs`, `image.rs`.

### 8.1 Encoding and top-level shape

**Manual** (API, Stream V2 README): "Each such top-level map begins with the key `type` that specifies the type of message." Data types: Any; Array(T); Boolean; DateTime, tag 0 with a text string (RFC 8949 §3.4.1); Float, IEEE 754 binary64; Integer, unsigned up to 64 bits; Map with text keys; Map(K to V); MultiDimArray, tag 40 with an array (RFC 8746 §3.1); Rational, "array of two unsigned integers ... numerator and denominator ... need not be in lowest terms", a plain array with no tag; TextString; TypedArray, a tag with a byte string (RFC 8746 §2).

**Driver:** accepts any valid CBOR head, because the manufacturer does not promise a canonical encoding: non-minimal integer widths (additional info 24 to 27), indefinite-length strings and containers, float16, float32 and float64 for a Float. Nesting of containers and tags is limited to 32 levels. Scalars are read leniently: a known key whose value has another type counts as absent. Structural errors are errors. A message is exactly one top-level map, with **trailing bytes an error**; leading tags outside the data model, such as 55799, are skipped. Whether a DCU sends any of these forms, or padding, is **Open** (§15 #21). The strict trailing-byte rule is the first thing to look at if a DCU's messages all fail to decode.

### 8.2 Message fields

**Manual** (API, Stream V2 message tables). Fields the driver's own logic uses are in bold; the others are only passed on.

**`start`**, once per series at arm:

| Field | Type | Notes |
|---|---|---|
| **`series_id`**, **`series_unique_id`** | Integer, TextString | §7.3 |
| `arm_date` | DateTime | Time of the arm command. |
| `channels` | Array(TextString) | "List of active channel names. Image channel data is serialized in this order." |
| **`count_time`**, **`frame_time`** | Float | s. `count_time` goes into per-frame metadata, `frame_time` sets the receive window. |
| `image_size_x`, `image_size_y` | Integer | Width, height. |
| `image_dtype` | TextString | `uint8`, `uint16`, `uint32`. In the Stream V2 README, not in the API's table. |
| `number_of_images` | Integer | |
| `beam_center_x`, `beam_center_y`, `pixel_size_x`, `pixel_size_y` | Float | |
| `countrate_correction_enabled`, `flatfield_enabled`, `pixel_mask_enabled`, `virtual_pixel_interpolation_enabled` | Boolean | |
| `countrate_correction_lookup_table` | TypedArray | §7.5 |
| `flatfield`, `pixel_mask` | Map(TextString to MultiDimArray) | §7.5. "32-bit pixel mask". |
| `detector_description`, `detector_serial_number`, `sensor_material` | TextString | |
| `detector_translation` | Array(Float) | m |
| `goniometer` | Map | Axis to `increment`, `start`. |
| `incident_energy`, `incident_wavelength` | Float | eV, Angstrom |
| `saturation_value` | Integer | "Maximum valid sample value (excluding the NODATA value)" |
| `sensor_thickness` | Float | |
| `threshold_energy` | Map(TextString to Float) | |
| `user_data` | Any | From `header_appendix`, §6.4. |

The driver keeps the start message's keys as `_stream_start_config` and attaches them to image dicts as `_start_config`.

**`image`**, one per exposure, with "all active channel (threshold) data of the exposure":

| Field | Type | Notes |
|---|---|---|
| `data` | Map(TextString to MultiDimArray) | §8.5 |
| **`image_id`** | Integer | "The image number." Whether the first is 0 or 1 is not stated (**Open**). |
| **`series_id`**, **`series_unique_id`** | | §7.3 |
| **`real_time`** | Rational | "Real exposure time of the image in seconds. The denominator is the time base frequency which is constant for the duration of the series." Example: `[4000000, 40000000]` is 0.1 s. |
| `start_time`, `stop_time` | Rational | "relative to series_date" |
| `series_date` | DateTime | |
| `user_data` | Any | |

**`end`**: `series_id`, `series_unique_id`.

### 8.3 Tag table

The "Driver" column is for `decode_message` and start-message arrays. The path for image frames takes only 64, 68, 69, 70.

| Tag | Meaning | Numpy type | Driver | Expected on this stream |
|---|---|---|---|---|
| 0 | DateTime, RFC 3339 text | | Kept as text | `arm_date`, `series_date` |
| 40 | MultiDimArray, row-major: `[dimensions, elements]` | | Decoded | Image channels, flatfield, pixel mask |
| 1040 | MultiDimArray, column-major | | Copied to row order | Not expected |
| 64 | uint8 | `u1` | Decoded | 8-bit frames, when they occur (§5.4) |
| 65, 66, 67 | uint16, 32, 64, big-endian | `>u2`, `>u4`, `>u8` | Byte-swapped | |
| 68 | uint8, clamped | `u1` | Decoded | |
| 69 | uint16 little-endian | `<u2` | Decoded | 16-bit frames |
| 70 | uint32 little-endian | `<u4` | Decoded | 32-bit frames, pixel mask |
| 71 | uint64 little-endian | `<u8` | Decoded | |
| 72 | sint8 | `i1` | Decoded | |
| 73, 74, 75 | sint16, 32, 64, big-endian | `>i2`, `>i4`, `>i8` | Byte-swapped | |
| 76 | Unassigned | | **Rejected** | |
| 77, 78, 79 | sint16, 32, 64, little-endian | `<i2`, `<i4`, `<i8` | Decoded | |
| 80, 81, 82 | float16, 32, 64, big-endian | `>f2`, `>f4`, `>f8` | Byte-swapped | |
| 83 | float128 big-endian | `>f16` | **Rejected** (no such numpy type on Windows) | |
| 84, 85, 86 | float16, 32, 64, little-endian | `<f2`, `<f4`, `<f8` | Decoded | 85: flatfield, countrate table |
| 87 | float128 little-endian | `<f16` | **Rejected** | |
| 56500 | DECTRIS compressed byte string: `[algorithm, modifier, data]` | | Decompressed in place | Inside typed-array tags |

The "Expected" column rests on the manufacturer's examples. Which tags a DCU sends is **Open** (§15 #21).

Tag number formula (RFC 8746 §2.1): `64 + 16*f + 8*s + 4*e + ll`, where `f` is 1 for float, `s` 1 for signed, `e` 1 for little-endian, and `ll` the length code. An element is `2^ll` bytes for an integer and `2^(ll+1)` for a float.

### 8.4 What each tag means to the decoder

- **TypedArray** (RFC 8746 §2): a byte string read as a flat sequence of elements in the tag's byte order. The driver hands out little-endian data.
- **Tag 56500.** The tag 56500 note: "This tag is equivalent to a byte string of the decompressed data. In particular, it MAY be used with tags that require data items of major type 2". So wherever a byte string is expected, in particular inside tags 64 to 87, the decoder takes either a byte string or `56500([algorithm, modifier, bytes])`. A bare tag 56500 where any value is allowed decodes to bytes. A payload may expand to 256 MiB at most, and all payloads of one message to 512 MiB, which guards against a corrupt size field.
- **MultiDimArray** (RFC 8746 §3.1): `[dimensions, elements]`. Tag 40 is row order, tag 1040 column order. `elements` is normally a typed array; a plain CBOR array is also accepted, as the manufacturer's example client does, and becomes an object array in Python.
- **Rational**: a plain `[numerator, denominator]`, passed on as a list. Callers divide.
- **Tag 0**: kept as text. Nothing downstream reads it.
- **Tags outside the data model** ({0, 40, 1040, 56500, 64 to 87}): passed through unchanged in a position that allows Any, such as `user_data`. In the element position of an image, anything but 64, 68, 69, 70 is an error naming the tag.

### 8.5 Nesting of an image payload

```
image map
 └─ "data" : map
     └─ "threshold_1" : tag 40 ([ [rows, cols], <elements> ])     ; rows = image_size_y, cols = image_size_x
          └─ <elements> : tag 70 (<u4) | tag 69 (<u2) | tag 64 (u1)
               └─ content : byte string (uncompressed)
                            | tag 56500 ([ "bslz4", elem_size, byte string ])   ; elem_size = bytes per element of the enclosing tag
                            | tag 56500 ([ "lz4", 0, byte string ])
```

This shape comes from the API and the tag 56500 note (§15 #21).

**Driver:** the decoded size must equal `rows * cols * itemsize`, with `itemsize` from the typed-array tag; this is checked before anything is allocated or decompressed, and the total is limited to 256 MiB. Decompression uses the 56500 **modifier**, since bitshuffle can be undone only with the element size that encoded it. If the modifier differs from `itemsize`, there is one WARNING per series and decoding goes on: a wrong element size gives a buffer of the right length with wrong contents and no error, so the mismatch has to be visible. Start-message `flatfield` and `pixel_mask` nest the same way with tags 85 and 70.

---

## 9. Compression framing: bslz4 and lz4, byte-exact

Both algorithms use the framing of the HDF5 bitshuffle and LZ4 filters. The tag 56500 note: "bslz4: Byte string of bitshuffle-transposed blocks each compressed using LZ4. The format is the same as the bitshuffle HDF5 filter." "lz4: Byte string of LZ4 compressed blocks. The LZ4 filter format for HDF5 is used for framing." The modifier is the bitshuffle element size for `bslz4`, and for `lz4` "Unused. ... The zero integer is serialized as a placeholder".

The FileWriter's HDF5 files use the same format: "By default the SINGLA data is compressed using the BSLZ4 algorithm" (User Manual §9.2). Bitshuffle is HDF5 filter 32008, so reading those files needs `hdf5plugin`. Stream V1 is different: its blobs are raw LZ4 with no header, and it gives times in integer ns where V2 uses rationals in seconds. Do not point this decoder at V1.

This section comes from the manufacturer's `dectris/compression` code and the bitshuffle source, not from a captured stream. **Driver:** `crates/dectris_codec/src/framing.rs`, `bitshuffle.rs`, `lz4.rs`. What to confirm on a detector is in §15 #21.

### 9.1 Container header

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 8 | `orig_size`, u64 **big-endian** | Total bytes after decompression. |
| 8 | 4 | `block_size`, u32 **big-endian**, **in bytes** | The bitshuffle writer stores `block_elems * elem_size`. |
| 12 | | Block stream | §9.2 |

Input shorter than 12 bytes is an error. **Driver:** also refuses `orig_size` above 256 MiB.

### 9.2 Block stream

Each block is a `u32` big-endian `compressed_size` followed by that many bytes holding one **LZ4 block**: the raw block format, not the LZ4 frame format. `compressed_size` must be at most `INT_MAX` and at most what remains of the input.

### 9.3 `bslz4` partition and decoding

With `E` the element size (the modifier; above 0 and at most `UINT32_MAX / 8`), `B` the `block_size` in bytes, and `N` the `orig_size`:

1. Require: not (`N != 0` and `B == 0`); `B mod (8*E) == 0`; `B <= INT_MAX`. `N == 0` gives empty output.
2. `leftover = N mod (8*E)` bytes; `full_blocks = N div B`; `last_block = (N mod B) - leftover` bytes.
3. Each of the `full_blocks` blocks: LZ4-decompress, which must give exactly `B` bytes, then bit-unshuffle `B / E` elements (§9.5).
4. If `last_block > 0`: one more block, exactly `last_block` bytes, unshuffled likewise.
5. If `leftover > 0`: the final `leftover` bytes of the input are copied **raw**, neither LZ4 nor shuffled.
6. The total written must equal `N`. The driver also requires `N = rows * cols * E` (§8.5).

Every block goes through LZ4; `bslz4` has no raw-block shortcut.

**Stricter than the manufacturer's code, on purpose:** bytes left over after the last block or the raw leftover are an error in the driver, while the manufacturer's C code accepts them. If a DCU pads its streams, this rule will fail (§15 #21).

### 9.4 `lz4` partition and decoding

1. Require: not (`N != 0` and `B == 0`); `B <= INT_MAX`. `N == 0` gives empty output.
2. `full_blocks = N div B`; `last_block = N mod B`. No rounding and no raw leftover.
3. Each block with expected size `S` (`B`, or `last_block` for the last): if `compressed_size == S`, the payload is stored **raw** and copied as it is. Otherwise LZ4-decompress, which must give exactly `S` bytes.

The modifier is ignored for `lz4`.

### 9.5 Bitshuffle transform: layout of one shuffled block

The scalar form in `kiyo-masui/bitshuffle`, `src/bitshuffle_core.c`, is the definition. For a block of `n` elements (`n` a multiple of 8) of `E` bytes each:

- **Shuffled layout.** `8*E` bit-rows of `n/8` bytes each. Row index `r = j*8 + k`, where `j` in `[0, E)` is the byte index within an element in **memory order** (for this little-endian data, `j = 0` is the least significant byte) and `k` in `[0, 8)` is the bit index. Byte `g` in `[0, n/8)` of row `r` packs bit `k` of byte `j` of elements `8g` to `8g+7`, with element `8g+m` at bit position `m`, lowest bit first.
- **Inverse, which the driver implements.** (1) Gather: `out[g*8*E + j*8 + k] = in[(j*8 + k)*(n/8) + g]`. (2) For every group `g` and plane `j`, load the 8 bytes `out[g*8*E + j*8 ...  + 7]` as one little-endian 64-bit word, apply `TRANS_BIT_8X8`, and scatter byte `k` of the result to output byte `g*8*E + j + k*E`, which is byte `j` of element `8g + k`.

```
TRANS_BIT_8X8(x):
  t = (x ^ (x >> 7))  & 0x00AA00AA00AA00AA;  x ^= t ^ (t << 7);
  t = (x ^ (x >> 14)) & 0x0000CCCC0000CCCC;  x ^= t ^ (t << 14);
  t = (x ^ (x >> 28)) & 0x00000000F0F0F0F0;  x ^= t ^ (t << 28);
```

**Driver:** this scalar form handles tails and unusual element sizes. For the usual sizes `bitshuffle.rs` has a faster kernel that gives the same result, working on 8 lanes of 64 elements. It is safe Rust that the compiler vectorises.

- **Default block size.** The driver's own encoder (`crates/dectris_codec/src/encode.rs`, used only by tests and fake streams) writes blocks of 8192 bytes by default. The decoder always takes `B` from the header. A DCU's value is **Open** (§15 #21).
- **Worked sizes for a full SINGLA frame, with 8192-byte blocks.** uint32, `N = 4 366 944`: 533 full blocks and `last_block` 608 bytes (608 mod 32 = 0, no leftover). uint16, `N = 2 183 472`: 266 full blocks and `last_block` 4400 bytes (4400 mod 16 = 0, no leftover).

### 9.6 Error vectors

All of these must fail (from `dectris/compression`, `compression_test.cc`): empty input; a 12-byte header with `N = 1, B = 0`; a truncated block header (`N = 1, B = 1`, then one byte); `bslz4` with `E = 0`; `bslz4` with `E = 0xFFFFFFFE`, which overflows `8*E`; `lz4` with `B = 0xFFFFFFFE` and `N = 0x00000000FFFFFFFE`; an LZ4 payload that does not decode to exactly the expected size; a leftover count that does not match what remains.

**Driver:** also rejects LZ4 match offset 0, which the LZ4 block format calls invalid. The liblz4 inside `dectris-compression` 0.3.1 accepts it, and its output then depends on uninitialised memory.

### 9.7 Test vectors

From `compression_test.cc` (hex: header, then blocks; `00...02` means an 8-byte big-endian value):

| # | Algorithm, E | Input | Output |
|---|---|---|---|
| 1 | bslz4, 1 | `00...01` `00 00 20 00` `89` | `89` (no blocks, 1 raw leftover) |
| 2 | bslz4, 2 | `00...02` `00 00 40 00` `89 00` | `89 00` |
| 3 | bslz4, 4 | `00...04` `00 00 80 00` `89 00 00 00` | `89 00 00 00` |
| 4 | lz4 | `00...01` `00 00 20 00` `00 00 00 01` `89` | `89` (a 1-byte last block stored raw) |
| 5 | lz4 | `00...1d` `00 00 00 10` `00 00 00 0a` `16 ff 01 00 50 ff ff ff ff ff` `00 00 00 0a` `13 ff 01 00 50 ff ff ff ff ff` | 29 x `ff` (16 + 13, both LZ4-coded) |
| 6 | lz4 | `00...03` `00 00 00 02` `00 00 00 02` `ff ff` `00 00 00 01` `ff` | 3 x `ff` (both blocks raw) |
| 7 | both | 12 zero bytes | Empty |

Other sources:

- **The algorithm name is compared byte-exactly.** `""`, `"LZ4"`, `"lz"`, `"lz5"`, `"lz4 "`, `" lz4"` are refused (from the Python tests of `dectris/compression` v0.3.1, which also have "turing" vectors decoding to one 72-byte sentence).
- **`dectris-compression` 0.3.1 as a reference** (in `third_party/`). `decompress(data, algorithm, *, elem_size=0)`. A wrong but valid `elem_size`, 8 instead of 4, returns a wrong buffer of the same length without an error (§8.5).
- **`hdf5plugin` as a generator.** Write a dataset with `hdf5plugin.Bitshuffle(nelems=0, cname='lz4')` or `hdf5plugin.LZ4(nbytes=0)`, `chunks=arr.shape`, and read the bytes with `dataset.id.read_direct_chunk((0, 0))[1]`. Tried with hdf5plugin 6.0.0, those chunks round-trip through `dectris-compression` for uint8, 16, 32.
- **In this repository:** `crates/dectris_codec/tests/gen_vectors.py` writes such vectors to `tests/vectors/`, and `oracle_vectors.rs` and `messages.rs` run them; `framing_vectors.rs` runs the vectors of §9.6 and §9.7. The Python-dependent tests skip themselves when that Python is missing. `DECTRIS_TEST_PYTHON` names it.
- **Raw frames (§13)** are exact wire captures, and so the best material for tests against a DCU's own output.

---

## 10. Frame and metadata semantics

**Driver:** what a frame from the stream is.

| Item | Rule |
|---|---|
| Type | From the channel payload's typed-array tag only: 64 or 68 uint8, 69 uint16, 70 uint32. `image_dtype` in `start` is not used for this. Another tag in that position is an error naming the tag. |
| Shape | From the tag's `dimensions`, `[rows, cols]` = `[image_size_y, image_size_x]`. Never from the yaml, which only gets a startup comparison (§3.5). A full frame is 1062 rows by 1028 columns. |
| Channel | `data["threshold_1"]`. Other channels are ignored. Missing: an error listing the keys found. |
| Size check | §8.5, for raw and compressed payloads alike. |
| Layout | As sent: no flip or rotation, no scaling to uint16. `convert_uint16` is an unused attribute. The program's file writers decide further. |
| Memory | Each frame is a new array that owns its memory. |
| Metadata | `receive_frames_with_metadata(n)` yields `(array, {"series_id", "image_id", "count_time", "real_time"})`. `count_time` comes from the attributed `start`, and is `None` if there was none. `real_time` is `[num, den]` as received, or `None`. |
| Start cache | §8.2. A caller may clear `_stream_start_config` before arming, and `arm` clears it anyway. |
| Binning | With `binning > 1`, the iterators return a mean-binned float64 array. The yaml allows only 1. |
| Stale frames | §7.3. |

---

## 11. Monitor preview path

**Manual** (API, Monitor tables): HTTP only. `GET /monitor/api/1.8.0/images/monitor` returns the newest exposure as TIFF. The server waits 500 ms by default, with 408 when there is nothing. "For frame rates above 10 Hz, use of either the FileWriter or Stream interface is advised."

TIFF private tag **51192** (0xC7F8) holds a DECTRIS IFD, a nested table of tags. Its entries are `IfdVersion`, `SeriesUniqueId`, `SeriesNumber`, `ImageNumber`, `ImageDateTime` (RFC 3339), `ThresholdId`, `ThresholdEnergy` (eV), `ExposureTime`, `IncidentEnergy` (eV), `IncidentWavelength` (Angstrom), `LostPixelCount`, `BeamCenter`, `DetectorDistance`; the codes are in the "TIFF Header Data" table of the API.

**Driver** (`monitor.rs`, `tiff.rs`, and `get_preview_image` in `py/src/camera_dectris.rs`):

- **Rate.** At most 10 requests per second, set by yaml `monitor_max_hz`, because a frame-grabbing thread may ask faster: the live view's 0.05 s frame time would mean 20 per second. A caller beyond the rate waits for its turn.
- **No fresh frame.** With a 408, during the back-off or during an initialize, the last frame is used. If there is none, a 640 x 640 uint16 array of zeros.
- **Errors.** One WARNING `Monitor preview failed: ...`, until a preview works again; then zeros as above. Never raises.
- **Decoding.** In Rust: grayscale 8, 16 or 32-bit integer and 32-bit float; uncompressed, deflate or LZW. Anything else is an error. The test TIFFs (`dectris_simplon/tests/vectors/`) were made for the purpose; a Monitor TIFF saved from a DCU can be tested the same way.
- **Tag 51192.** Read as an IFD or as `key=value` text. Which form a DCU writes is **Open**. The values are stored as strings and not exposed to Python.
- **Turn.** A preview gets `fliplr`, `flipud`, `rot90(k=-1)` in that order, which comes to `rot90(k=+1)`, one turn counter-clockwise. So before scaling a preview is 1028 rows by 1062 columns. Stream frames, and files written from them, are not turned. So the live view and saved images differ by that turn. Whether a DCU's Monitor and stream images differ in just that way is **Open** (§15 #41).
- **Scale.** To fit 640 pixels on the longer side, with linear interpolation, in the same pixel type.

---

## 12. FileWriter path

**Manual** (API, User Manual §7.1, §7.3.1): HDF5 files (NeXus NXmx) on the DCU: "The files are created locally on the detector server and have to be transferred to the user computer". Master file `/data/<name_pattern>_master.h5`, data files `/data/<name_pattern>_data_<filenr>.h5`. A master file appears after `arm`. After 1000 images one data file becomes available (with `nimages_per_file` at its default of 1000), and "No further files will made available until the series is finished either by completing the nth image (nimages) of the nth trigger (ntrigger) or by ending the series using the detector command disarm." Space is limited: "Buffered files have to be regularly fetched and subsequently deleted".

**Driver**, with endpoints in §6.6:

- `configure_filewriter(mode='enabled', name_pattern='series_$id', nimages_per_file=1000, compression_enabled=True, image_nr_start=1)`, before `arm`.
- `disarm()` from `idle` with the FileWriter on still sends the PUT, so that the files are closed (§3.3).
- `download_filewriter_files(destination)`: makes the directory, waits while FileWriter `status/state` is `acquire`, lists, downloads each file. If data files were expected and none is listed: a WARNING. Raises only if the directory cannot be made.
- `clear_filewriter()` deletes everything on the DCU. `disable_filewriter()` sets the mode.

A FileWriter that is slow to finish its files may list no data file right after `cancel`. The wait exists for this. What a DCU does is **Open**.

A `name_pattern` with no `$id` is the same for every series, so the FileWriter overwrites the files of the last one.

---

## 13. Raw-frame path

A caller can write each stream message to disk during collection, as received, and decode it later.

**Driver:**

1. `receive_raw_frames(n)` yields `(raw_bytes, meta)`. `raw_bytes` is the whole ZeroMQ message, byte for byte, still compressed inside; `meta` is as in §10. It stops without an exception on a timeout or on `end`.
2. It classifies by a scan of the top-level map (`type`, `series_id`, `series_unique_id`, `image_id`, `real_time`), skipping `data` without decoding or decompressing it. Measured on the development machine with synthetic frames: under 1 microsecond per message, against about 1.2 ms for a full decode of a 1028 x 1062 uint32 frame into a reused buffer, 1.8 ms into a fresh one (DESIGN §9).
3. `decode_message(raw)` and `decompress_image(message)` work on such bytes with no camera or DCU (DESIGN §4.6).

Raw files taken on a detector are the best test material for §8 and §9. Take one set with `header_detail` `basic` and one with `all`.

---

## 14. Logging conventions

**Driver** (`crates/dectris_common/src/log.rs`):

- Python logger names: `instamatic.camera.camera_dectris` for the driver, `instamatic.camera.camera_dectris.stream` for the receive path. A log panel can attach to these names, so they must not change.
- Levels: DEBUG for polling and per-frame detail, the latter only if yaml `log_frames` is on; INFO for state changes (`Arming detector...`, `Configuring: nimages=...`); WARNING for best-effort failures, 405 fallbacks and mismatches; ERROR for failed commands.
- Stdout, with the prefix `[Dectris]`: `[Dectris] ZMQ connected: <addr> (buffer HWM=<n> messages)`, `[Dectris] ZMQ reconnected: fresh buffer`, `[Dectris] ZMQ reconnect skipped: receiver busy`.
- Worker threads never log through Python. Late results of trigger workers are logged by the next driver call.

Log text is the first thing to read on a detector. `Trigger mode MISMATCH`, geometry differing from the yaml, `series armed outside the driver`, and `frame_time readback ... differs` all point at §15.

---

## 15. Contradictions and open questions

What the sources disagree on, and what a DCU settles. **Numbers are cited from the source, so they stay fixed, and some are skipped.**

| # | Topic | What is known | What the driver does | How to settle it |
|---|---|---|---|---|
| 2 | Does `cancel` need a re-initialize? | A report says so "per spec" (**Reported**). The API only says it "Stops the data acquisition, but only after the next image is finished." | No initialize after `cancel`. | Run `cancel` on a series; check `end` arrives and the state reaches `idle`, and that the next series works. |
| 5 | `compression: none` | The API allows only `lz4`, `bslz4`. "None" for files is the FileWriter's `compression_enabled`. Whether a DCU refuses `none` is **Open**. | `none` is kept locally with a WARNING and never sent. The decoder still reads uncompressed payloads. | Nothing is needed unless you want uncompressed frames. |
| 6 | `header_detail: none` | API: "No meta data". Stream V2 README: "Same as basic". What a DCU sends then is **Open**. | Never sets it unless the yaml says so. A `start` without `count_time` gives `None` for each frame (§7.5). | Avoid `none`. The API discourages it. |
| 9 | Last image and a flush after `disarm` | API: the last image may come "only after a disarm". A caller that flushes right after `disarm` may discard it. | `get_image` and `get_movie` wait (§7.4). The flush is the caller's. | Compare the frame count with `nimages` on a short series. |
| 10 | Does the trigger PUT block? | A report says for the whole series (`ints`) or about one `frame_time` (`inte`) (**Reported**). The manual only says the latency is "hard to predict". What a pending PUT does after `cancel` is unknown too. | Works for both (§4.4-2). A caller that triggers by hand should do it from a thread for this reason. | Time a `trigger` in `ints` on a 10-frame series, then cancel one during a long series. |
| 21 | What is on the wire | Which typed tags appear; whether 56500 wraps as in §8.5; the DCU's `block_size`; indefinite-length items, tag 1040, float16, extra channels; padding after a message or stream. | Accepts all the variants the format allows, except trailing bytes, which are errors (§8.1, §9.3). | Capture raw files (§13). |
| 22 | `hv_enabled` on a SINGLA | "Only available on specific detector types". Whether `READY` must be waited for before `arm` is also unknown. | Sends it when asked, and does not wait for `READY`. | Call `hv_enabled(True)`; read `status/high_voltage/state`. |
| 23 | Limits on `nimages`, `ntrigger`; `inte` with `count_time` above `frame_time` | A continuous collection uses 99999. | Sends what it is given. | GET `min` and `max` of those keys. |
| 31 | A `count_time` or `frame_time` that conflicts | Manual §7.4.2: the DCU rewrites the other value, and the PUT returns the changed keys. A report says "the detector rejects the configuration". Neither has been checked. | Written to end correctly under both (§6.1), and read back. The test fakes (`crates/dectris_simplon/src/testing.rs`, `tests/super_dectris/fake_dcu.py`) model both. | Set a long `count_time` with a short `frame_time`; read both back. |
| 32 | Does the detector's `initialize` reset the stream mode? | A report says yes (**Reported**); the API's sentence is about `stream/command/initialize`. | Re-enables always (§3.6). | GET `stream/config/mode` before and after a detector `initialize`. |
| 33 | Value type of `header_appendix` | API: `string`. Stream V2 README: `user_data` is Any. Whether a DCU accepts a JSON object is **Open**. | Sends the string `""`. The decoder accepts `user_data` of any CBOR type. If the PUT fails: a WARNING only. The object-then-string retry in `stream_cfg.rs` is for a non-string value. | Look at `user_data` in a captured `start`. |
| 38 | `sequence_id` and `series_id` | The driver assumes the id in the `arm` reply equals `series_id` in the messages, and that the reply's key is `sequence_id`. Both are **Open**. | §7.3. The driver also accepts the spelling `sequence id` (with a space). | If frames count as stale after `arm`, read the `arm` reply and `start`; use `series_attribution: fifo` meanwhile. |
| 39 | Bit depth | Manual: 8-bit readout is automatic above 2250 Hz, silent on `bit_depth_image`. Whether count-rate correction switches frames between 16 and 32 bit (§2.6) is only **Reported**. | Follows each message's tag. | GET `bit_depth_image` and look at the frame type, with the correction on and off, and at a `frame_time` under 0.44 ms. Watch for counter rollover at 255. |
| 40 | The DCU's sender | How much a DCU queues, and whether it drops or waits. | §7.2. | Compare `stream/status/dropped` with `image_id` gaps at high rates. |
| 41 | Orientation | The pixels of this driver have not been compared with a known sample. Whether Monitor and stream frames differ by exactly one turn is also unknown. | §10, §11. | Compare the frames with a dataset of a known sample, including the rotation axis. |
| 42 | Other things a DCU shows | Whether `disarm` is accepted in `idle`; whether polls may run during `initialize`; a Monitor TIFF from a DCU and its tag 51192; timing and rates on the site's network; the FileWriter's delay after `cancel`. | §3.3, §1.3, §11, §7.5, §12. | Watch the log during the first runs. |
