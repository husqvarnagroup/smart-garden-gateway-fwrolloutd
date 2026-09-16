// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

use anyhow::anyhow;
use anyhow::Context as _;
use fwrolloutd::IpsoRegistry;
use num_traits::FromPrimitive as _;
use rand::seq::SliceRandom as _;
use rand::Rng as _;
use reqwest::Client;
use std::path::PathBuf;

lazy_static::lazy_static! {
    static ref TENANT: std::sync::Arc<std::sync::Mutex<Option<String>>> = std::sync::Arc::new(std::sync::Mutex::new(None));
}

const IPC_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);
const UPLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60 * 60);

struct AltDisplay<T>(T);

impl<T: core::fmt::Display> core::fmt::Display for AltDisplay<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> Result<(), core::fmt::Error> {
        write!(f, "{:#}", self.0)
    }
}

trait DisplayEx {
    fn altdisplay(&self) -> AltDisplay<&Self>;
}

impl<T: core::fmt::Display> DisplayEx for T {
    fn altdisplay(&self) -> AltDisplay<&Self> {
        AltDisplay(self)
    }
}

// Source: https://github.com/rust-lang/rust/blob/26c9b0046f96403cdf959e4e1f874ec25f9dbf6f/src/librustdoc/html/format.rs#L1474
// License: see the linked repo
// PANIC: this function does panic if you try to call `fmt` twice.
pub(crate) fn display_fn(
    f: impl FnOnce(&mut std::fmt::Formatter<'_>) -> std::fmt::Result,
) -> impl std::fmt::Display {
    struct WithFormatter<F>(std::cell::Cell<Option<F>>);

    impl<F> std::fmt::Display for WithFormatter<F>
    where
        F: FnOnce(&mut std::fmt::Formatter<'_>) -> std::fmt::Result,
    {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            (self.0.take()).unwrap()(f)
        }
    }

    WithFormatter(std::cell::Cell::new(Some(f)))
}

fn ubootenv_get(name: &str) -> anyhow::Result<String> {
    let output = std::process::Command::new("/sbin/fw_printenv")
        .arg("-n")
        .arg(name)
        .output()
        .context("failed to execute fw_printenv")?;
    if !output.status.success() {
        return Err(anyhow!("fw_printenv failed. code={}", output.status));
    }

    String::from_utf8(output.stdout).context("value is not valid UTF-8")
}

#[derive(Debug, serde::Deserialize)]
#[serde(default)]
struct Config {
    fota_loop: bool,
    /// in seconds
    sleep_interval: u64,
    index_url: url::Url,
    /// in seconds
    ipc_connect_retry_interval: u64,
    device_servers: Vec<String>,
    ipso_registry_root_path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        let tenant = ubootenv_get("bnw_cloud_tenant");

        let indexfile_url = match TENANT
            .lock()
            .unwrap()
            .as_deref()
            .map_or_else(|| tenant.as_deref(), Ok)
            .map(|s| s.trim())
        {
            Ok("sg-live" | "sg-staging" | "") => {
                "https://gateway.iot.sg.dss.husqvarnagroup.net/firmware-repository/index_v2.json"
            }
            Ok("sg-qa") => {
                "https://gateway-dev.iot.sg.dss.husqvarnagroup.net/firmware-repository/qa/index_v2.json"
            }
            Ok(_) => {
                "https://gateway-dev.iot.sg.dss.husqvarnagroup.net/firmware-repository/dev/index_v2.json"
            }
            Err(e) => {
                tracing::info!(error=%e.altdisplay(), "Failed to read tenant from ubootenv");
                "https://gateway.iot.sg.dss.husqvarnagroup.net/firmware-repository/index_v2.json"
            }
        };

        Self {
            sleep_interval: 300,
            index_url: url::Url::parse(indexfile_url).unwrap(),
            fota_loop: false,
            ipc_connect_retry_interval: 10,
            device_servers: vec!["lemonbeatd".to_string(), "lwm2mserver".to_string()],
            ipso_registry_root_path: PathBuf::from("/var/lib/ipso_definitions/"),
        }
    }
}

impl Config {
    pub async fn load<P: AsRef<std::path::Path>>(path: P) -> anyhow::Result<Option<Self>> {
        let path = path.as_ref();
        let data = match tokio::fs::read(path).await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| anyhow!("can't read config from `{:?}`", path))
            }
            Ok(v) => v,
        };

        let config = serde_yaml::from_slice(&data)
            .with_context(|| anyhow!("can't parse config at `{:?}`", path))?;

        tracing::info!(?path, "Loaded config");

        Ok(Some(config))
    }
}

#[derive(Debug, serde::Deserialize)]
struct ImageIndexEntry {
    model_number: String,
    software_version: String,
    firmware_version: String,
    pkg_version: String,
    /// we can't use a Url because that requires us to know the base url
    url: String,
    update: bool,
}

impl ImageIndexEntry {
    pub async fn download(&self) -> anyhow::Result<bytes::Bytes> {
        tracing::info!(url=%self.url, "Download");

        let response = Client::builder()
            .timeout(DOWNLOAD_TIMEOUT)
            .build()?
            .get(&self.url)
            .send()
            .await
            .with_context(|| anyhow!("download timed out `{}`", self.url))?;

        let status = response.status();
        if status.is_success() {
            response
                .bytes()
                .await
                .with_context(|| anyhow!("can't download `{}`", self.url))
        } else {
            Err(anyhow!(
                "can't download `{}`, status `{}`",
                self.url,
                status
            ))
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct IndexFile {
    ipso_definitions: String,
    firmware_updates: Vec<ImageIndexEntry>,
}

impl IndexFile {
    fn new() -> Self {
        Self {
            ipso_definitions: "".to_string(),
            firmware_updates: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct ImageIndex<'a> {
    index_file: IndexFile,
    config: &'a Config,
    etag: Option<String>,
    next_update: Option<chrono::DateTime<chrono::offset::Local>>,
    ipso_registry: IpsoRegistry,
}

impl<'a> ImageIndex<'a> {
    pub fn new(config: &'a Config) -> Self {
        Self {
            index_file: IndexFile::new(),
            config,
            etag: None,
            next_update: None,
            ipso_registry: IpsoRegistry::new(&config.ipso_registry_root_path),
        }
    }

    pub async fn update(&mut self) -> anyhow::Result<()> {
        let client = Client::builder().timeout(DOWNLOAD_TIMEOUT).build()?;
        let mut builder = client.get(self.config.index_url.clone());

        if let Some(etag) = &self.etag {
            builder = builder.header(reqwest::header::IF_NONE_MATCH, etag);
        }

        let response = match builder.send().await {
            Ok(response) => match response.status() {
                reqwest::StatusCode::NOT_MODIFIED => {
                    tracing::info!("Index didn't change");
                    self.delay_next_check();
                    return Ok(());
                }
                reqwest::StatusCode::OK => {
                    self.delay_next_check();
                    response
                }
                other => anyhow::bail!("{}", other),
            },
            Err(error) => {
                if error.is_connect() || error.is_timeout() {
                    tracing::info!("Network-related problem updating index - will retry later");
                    return Ok(());
                }
                return Err(anyhow!(error));
            }
        };

        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .context("no etag in response")?
            .to_str()
            .context("etag is not a valid UTF-8 string")?
            .to_string();
        self.index_file = response.json().await?;

        let ipso_update_url = &self
            .config
            .index_url
            .join(&self.index_file.ipso_definitions)
            .with_context(|| anyhow!("invalid IPSO URL: {}", self.index_file.ipso_definitions))?;
        self.ipso_registry.update_if_needed(ipso_update_url).await?;

        for entry in &mut self.index_file.firmware_updates {
            let url = self
                .config
                .index_url
                .join(&entry.url)
                .with_context(|| anyhow!("invalid URL: {}", entry.url))?;
            entry.url = url.to_string();
        }

        tracing::info!(
            etag = etag.as_str(),
            entries = self.index_file.firmware_updates.len(),
            url = self.config.index_url.as_str(),
            "Index updated"
        );

        self.etag = Some(etag);

        Ok(())
    }

    fn delay_next_check(&mut self) {
        self.next_update = Some(
            chrono::offset::Local::today()
                .checked_add_signed(chrono::Duration::days(1))
                .unwrap()
                .and_time(chrono::naive::NaiveTime::from_hms(0, 0, 0))
                .unwrap()
                .checked_add_signed(chrono::Duration::minutes(
                    rand::thread_rng().gen_range(0..=55),
                ))
                .unwrap(),
        );
    }

    async fn update_if_needed(&mut self) {
        let next = match self.next_update {
            Some(next) => next,
            None => return,
        };

        let now = chrono::offset::Local::now();
        if next >= now {
            return;
        }

        if let Err(e) = self.update().await {
            if (now - next).num_hours() < 24 {
                tracing::info!(error=%e.altdisplay(), url=self.config.index_url.as_str(), "Failed to update index (first 24h)")
            } else {
                tracing::error!(error=%e.altdisplay(), url=self.config.index_url.as_str(), "Failed to update index")
            }
        }
    }

    fn find_update(&self, info: &DeviceInfo, fota_loop: bool) -> Option<&ImageIndexEntry> {
        self.index_file.firmware_updates.iter().find(|entry| {
            entry.model_number == info.model_number
                && entry.software_version == info.software_version
                && entry.firmware_version == info.firmware_version
                && (entry.update || self.config.fota_loop || fota_loop)
        })
    }

    fn has_rule(&self, info: &DeviceInfo) -> bool {
        self.index_file
            .firmware_updates
            .iter()
            .any(|entry| entry.model_number == info.model_number)
    }
}

#[derive(Clone, Debug)]
struct DeviceInfo {
    model_number: String,
    software_version: String,
    firmware_version: String,
    firmware_update_state: lwm2m::FirmwareUpdateState,
    firmware_update_result: lwm2m::FirmwareUpdateResult,
    pkg_version: String,
    online: bool,
}

#[tracing::instrument(skip_all)]
fn parse_device_info(mut device: lwm2m::raw::Device) -> anyhow::Result<DeviceInfo> {
    let firmware_object = device
        .get_mut(&lwm2m::raw::ObjectTypeMaybe::Parsed(
            lwm2m::ObjectType::FirmwareUpdate,
        ))
        .context("no firmware update object")?
        .instances
        .get_mut("0")
        .context("no firmware update instance")?;
    let pkg_version: String = firmware_object
        .remove("pkg_version")
        .context("no pkg_version")?
        .data
        .try_into()
        .context("pkg_version")?;
    let firmware_update_state: u64 = firmware_object
        .remove("state")
        .context("no state")?
        .data
        .try_into()
        .context("state")?;
    let firmware_update_state =
        lwm2m::FirmwareUpdateState::from_u64(firmware_update_state).context("unsupported state")?;
    let firmware_update_result: u64 = firmware_object
        .remove("update_result")
        .context("no update result")?
        .data
        .try_into()
        .context("update_result")?;
    let firmware_update_result = lwm2m::FirmwareUpdateResult::from_u64(firmware_update_result)
        .context("unsupported result")?;

    let device_object = device
        .get_mut(&lwm2m::raw::ObjectTypeMaybe::Parsed(
            lwm2m::ObjectType::Device,
        ))
        .context("no device object")?
        .instances
        .get_mut("0")
        .context("no device object instance")?;
    let software_version: String = device_object
        .remove("software_version")
        .context("no software_version")?
        .data
        .try_into()
        .context("software_version")?;
    let model_number: String = device_object
        .remove("model_number")
        .context("no model_number")?
        .data
        .try_into()
        .context("model_number")?;
    let firmware_version: String = device_object
        .remove("firmware_version")
        .context("no firmware_version")?
        .data
        .try_into()
        .context("firmware_version")?;

    let connstatus_object = device
        .get_mut(&lwm2m::raw::ObjectTypeMaybe::Parsed(
            lwm2m::ObjectType::ConnectionStatus,
        ))
        .context("no connection status object")?
        .instances
        .get_mut("0")
        .context("no connection status instance")?;
    let online: bool = connstatus_object
        .remove("online")
        .context("no online")?
        .data
        .try_into()
        .context("online")?;

    Ok(DeviceInfo {
        model_number,
        software_version,
        firmware_version,
        firmware_update_state,
        firmware_update_result,
        pkg_version,
        online,
    })
}

#[derive(Debug)]
enum NextCheck {
    Immediately,
    At(chrono::DateTime<chrono::offset::Local>),
}

#[derive(Debug, Default)]
struct SkipReason {
    line: u32,
    column: u32,
    /// additional data to compare
    ///
    /// This is highly inefficient and inexact but using PartialEq is
    /// non-trivial: [https://users.rust-lang.org/t/how-to-support-derive-partialeq-for-dyn-partialeq/68003]
    meta: Option<Box<dyn core::fmt::Debug>>,
}

impl PartialEq for SkipReason {
    fn eq(&self, other: &Self) -> bool {
        self.line == other.line
            && self.column == other.column
            && self.meta.as_ref().map(|meta| format!("{:?}", meta))
                == other.meta.as_ref().map(|meta| format!("{:?}", meta))
    }
}

macro_rules! changed_skip_reason_inner {
    ($device:expr, $meta:expr) => {{
        let reason = SkipReason {
            line: ::core::line!(),
            column: ::core::column!(),
            meta: $meta,
        };
        if $device.last_skip_reason != reason {
            $device.last_skip_reason = reason;
            true
        } else {
            false
        }
    }};
}

macro_rules! changed_skip_reason {
    ($device:expr, $meta:expr) => {
        changed_skip_reason_inner!($device, Some(Box::new($meta)))
    };
    ($device:expr) => {
        changed_skip_reason_inner!($device, None)
    };
}

#[derive(Debug, Clone)]
struct ReqServiceConfig {
    socket_path: String,
    retry_interval: u64,
}

#[derive(Debug)]
struct Device {
    id: String,
    next_check: NextCheck,
    force_check: bool,
    last_skip_reason: SkipReason,
    oneshot_fota_loop: bool,
    req_service_config: ReqServiceConfig,
}

impl Device {
    pub fn new(id: String, req_service_config: ReqServiceConfig) -> Self {
        Self {
            id,
            next_check: NextCheck::Immediately,
            force_check: false,
            last_skip_reason: SkipReason::default(),
            oneshot_fota_loop: false,
            req_service_config,
        }
    }

    pub fn check_delayed(&self) -> bool {
        match self.next_check {
            NextCheck::Immediately => false,
            NextCheck::At(next) => next > chrono::offset::Local::now(),
        }
    }

    pub fn delay_check_by_one_hour(&mut self) {
        self.next_check = NextCheck::At(
            chrono::offset::Local::now()
                .checked_add_signed(chrono::Duration::hours(1))
                .unwrap(),
        );
    }

    pub async fn upload_firmware(&mut self, data: &[u8]) -> anyhow::Result<()> {
        tokio::time::timeout(UPLOAD_TIMEOUT, async {
            let mut req_service = tokio::time::timeout(
                IPC_REQUEST_TIMEOUT,
                make_reqservice(self.req_service_config.clone()),
            )
            .await
            .context("timed out connecting to IPC req service")?;
            let response = tokio::time::timeout(IPC_REQUEST_TIMEOUT, async {
                lwm2m::make_request::<Option<serde_json::Value>>(
                    &mut req_service,
                    &lwm2m::Request {
                        op: lwm2m::Method::Write,
                        entity: lwm2m::Entity {
                            path: std::path::PathBuf::from("firmware_update/0/package"),
                            kind: lwm2m::EntityKind::Device {
                                device: self.id.to_string(),
                            },
                        },
                        payload: Some(lwm2m::Payload::Value(lwm2m::Value::new(
                            data.to_vec().into(),
                            None,
                        ))),
                    },
                )
                .await
            })
            .await
            .context("timed out waiting for IPC upload response")??;
            if !response.success {
                return Err(anyhow!("failed to upload firmware"));
            }

            Ok(())
        })
        .await
        .context("firmware upload timed out")?
    }

    pub async fn reset_upload_state(&mut self) -> anyhow::Result<()> {
        self.upload_firmware(&[0x00])
            .await
            .context("failed to reset upload state")
    }

    #[tracing::instrument(skip_all, name="device", fields(device = %self.id))]
    async fn process(
        &mut self,
        device_infos: &std::collections::HashMap<String, DeviceInfo>,
        index: &ImageIndex<'_>,
    ) -> ProcessResult {
        if self.check_delayed() {
            tracing::debug!("Skip device: next check delayed");
            return ProcessResult::DeviceDone;
        }

        let device_info = match device_infos.get(&self.id) {
            Some(v) => v,
            None => {
                if changed_skip_reason!(self) {
                    tracing::error!("Skip device: failed to get device info");
                }
                return ProcessResult::DeviceDone;
            }
        };
        tracing::debug!("{:#?}", device_info);

        if !index.has_rule(device_info) {
            if changed_skip_reason!(self) {
                tracing::info!("Skip device: device without update rules (CBT11?)");
            }
            return ProcessResult::DeviceDone;
        }

        if !device_info.online {
            if changed_skip_reason!(self) {
                tracing::info!("Skip device: offline");
            }
            return ProcessResult::DeviceDone;
        }

        let index_entry = match index.find_update(device_info, self.oneshot_fota_loop) {
            Some(v) => v,
            None => {
                let failed_upload = matches!(
                    device_info.firmware_update_state,
                    lwm2m::FirmwareUpdateState::Idle
                ) && !matches!(
                    device_info.firmware_update_result,
                    lwm2m::FirmwareUpdateResult::Initial
                        | lwm2m::FirmwareUpdateResult::Success
                        | lwm2m::FirmwareUpdateResult::UpdateFailed
                );

                let uploaded = matches!(
                    device_info.firmware_update_state,
                    lwm2m::FirmwareUpdateState::DownloadComplete
                );

                if failed_upload || uploaded {
                    // update got pulled back - reset to idle state

                    // note: LWM2M does not foresee a way to reset the "result" after a failed
                    // upload. Sending a \0 image however resets (by chance) the result for
                    // lemonbeatd. For lwm2m devices it likely has no effect.

                    tracing::info!(
                        "No update available and state={:?}/{:?}. Remove uploaded file",
                        device_info.firmware_update_state,
                        device_info.firmware_update_result
                    );

                    if let Err(e) = self.reset_upload_state().await {
                        tracing::warn!(error=%e.altdisplay(), "Failed to cancel update");
                        self.delay_check_by_one_hour();
                        return ProcessResult::DeviceDone;
                    }
                }

                if changed_skip_reason!(self, device_info.clone()) {
                    tracing::info!(
                        model_number = %device_info.model_number,
                        software_version = %device_info.software_version,
                        firmware_version = %device_info.firmware_version,
                        "Skip device: no update available"
                    );
                }
                self.delay_check_by_one_hour();
                return ProcessResult::DeviceDone;
            }
        };
        self.oneshot_fota_loop = false;

        match device_info.firmware_update_state {
            lwm2m::FirmwareUpdateState::Idle => match device_info.firmware_update_result {
                // nothing to do, the device is ready to receive an update
                lwm2m::FirmwareUpdateResult::Initial
                | lwm2m::FirmwareUpdateResult::Success
                | lwm2m::FirmwareUpdateResult::UpdateFailed => {}

                // a previous update failed.
                result => {
                    if self.force_check {
                        // User requested a one-time re-upload. So not stopping here but resetting
                        // force_check so that future failed uploads are not automatically retried.
                        self.force_check = false;
                    } else {
                        // User has yet to request re-upload. Skip device.
                        if changed_skip_reason!(self, result) {
                            tracing::info!(?result, "Skip device: previous upload failed");
                        }
                        return ProcessResult::DeviceDone;
                    }
                }
            },
            // the device is still downloading. Let's not start another one.
            // not even from another device since we want to prevent parallel downloads.
            lwm2m::FirmwareUpdateState::Downloading => {
                tracing::info!("Abort check: image being uploaded");
                return ProcessResult::EverythingDone;
            }
            lwm2m::FirmwareUpdateState::DownloadComplete => {
                if index_entry.pkg_version == device_info.pkg_version {
                    // image was uploaded already
                    if changed_skip_reason!(self, index_entry.pkg_version.clone()) {
                        tracing::info!(
                            pkg_version = index_entry.pkg_version.as_str(),
                            "Skip device: image already uploaded"
                        );
                    }
                    return ProcessResult::DeviceDone;
                } else if let Err(e) = self.reset_upload_state().await {
                    tracing::error!(error=%e.altdisplay(), "Skip device: failed to reset upload state");
                    return ProcessResult::DeviceDone;
                }
            }
            // we can't really know what firmware is currently running
            // we can continue processing another device though because no
            // other upload is running
            lwm2m::FirmwareUpdateState::Updating => {
                if changed_skip_reason!(self) {
                    tracing::info!("Skip device: device being updated");
                }
                return ProcessResult::DeviceDone;
            }
        }

        tracing::debug!("{:#?}", index_entry);

        let data = match index_entry.download().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error=%e.altdisplay(), "Skip device: failed to download image");
                self.delay_check_by_one_hour();
                return ProcessResult::DeviceDone;
            }
        };
        tracing::debug!(len = data.len(), "Downloaded image");

        match self.upload_firmware(&data).await {
            Ok(()) => tracing::info!("Image upload started"),
            Err(e) => {
                tracing::warn!(error=%e.altdisplay(), "Skip device: image upload start failed");
                self.delay_check_by_one_hour();
                return ProcessResult::DeviceDone;
            }
        }

        ProcessResult::EverythingDone
    }
}

#[derive(argh::FromArgs)]
/// firmware rollout service
struct Args {
    /// tenant override
    #[argh(option, short = 't')]
    tenant: Option<String>,
}

async fn make_reqservice(req_service_config: ReqServiceConfig) -> sg_ipc::ReqService {
    let mut attempt: u32 = 0;
    loop {
        match sg_ipc::ReqService::new(&req_service_config.socket_path).await {
            Ok(v) => break v,
            Err(e) => {
                if attempt < 20 {
                    attempt += 1;
                    tracing::info!(error=%e.altdisplay(), path=%req_service_config.socket_path, "Failed early connect to IPC rsp service");
                } else {
                    tracing::error!(error=%e.altdisplay(), path=%req_service_config.socket_path, "Failed to connect to IPC rsp service");
                }
                let duration = std::time::Duration::from_secs(req_service_config.retry_interval);
                tokio::time::sleep(duration).await;
            }
        }
    }
}

pub enum ProcessResult {
    DeviceDone,
    ServerDone,
    EverythingDone,
}

struct DeviceServer {
    devices: Vec<Device>,
    name: String,
    req_service_config: ReqServiceConfig,
}

impl DeviceServer {
    #[tracing::instrument(skip_all, name="device_server", fields(name = name))]
    pub async fn new(name: &str, req_service_config: ReqServiceConfig) -> Self {
        Self {
            devices: Vec::new(),
            name: name.to_string(),
            req_service_config,
        }
    }

    async fn load_device_infos(
        &mut self,
    ) -> anyhow::Result<std::collections::HashMap<String, DeviceInfo>> {
        let mut req_service = tokio::time::timeout(
            IPC_REQUEST_TIMEOUT,
            make_reqservice(self.req_service_config.clone()),
        )
        .await
        .context("timed out connecting to IPC req service")?;
        let mut response = tokio::time::timeout(IPC_REQUEST_TIMEOUT, async {
            lwm2m::make_request::<lwm2m::raw::DeviceMap>(
                &mut req_service,
                &lwm2m::Request {
                    op: lwm2m::Method::Read,
                    entity: lwm2m::Entity {
                        path: std::path::PathBuf::from("devices"),
                        kind: lwm2m::EntityKind::Gateway {
                            service: self.name.clone(),
                        },
                    },
                    payload: None,
                },
            )
            .await
        })
        .await
        .context("timed out waiting for device info IPC response")??;
        if !response.success {
            return Err(anyhow!("failed to load devices"));
        }

        let mut infos = std::collections::HashMap::new();
        for (id, device) in response.payload.drain() {
            match parse_device_info(device) {
                Err(e) => {
                    tracing::error!(device=%id, error=%e.altdisplay(), "Failed to parse device")
                }
                Ok(info) => {
                    infos.insert(id, info);
                }
            }
        }

        tracing::debug!("Loaded {} devices: {:#?}", infos.len(), infos);
        Ok(infos)
    }

    #[tracing::instrument(skip_all, name="device_server", fields(name = self.name.as_str()))]
    pub async fn process(
        &mut self,
        index: &mut ImageIndex<'_>,
        device_infos: &std::collections::HashMap<String, DeviceInfo>,
    ) -> ProcessResult {
        // remove old entries
        self.devices
            .retain(|device| device_infos.iter().any(|(id, _)| &device.id == id));

        // add missing entries
        for id in device_infos.keys() {
            if !self.devices.iter().any(|device| &device.id == id) {
                self.devices
                    .push(Device::new(id.clone(), self.req_service_config.clone()));
            }
        }

        // print single-line debug device-strings but one per line
        tracing::debug!(
            "Devices: [\n{}]",
            display_fn(|f| {
                for device in &self.devices {
                    writeln!(f, "\t{:?}", device)?;
                }
                Ok(())
            })
        );

        index.update_if_needed().await;

        self.devices.shuffle(&mut rand::thread_rng());

        for device in &mut self.devices {
            match device.process(device_infos, index).await {
                ProcessResult::DeviceDone => continue,
                ProcessResult::ServerDone => return ProcessResult::ServerDone,
                ProcessResult::EverythingDone => return ProcessResult::EverythingDone,
            }
        }

        ProcessResult::ServerDone
    }
}

async fn handle_ipc_all_devices_request() -> Result<serde_json::Value, anyhow::Error> {
    Ok(serde_json::json!(serde_json::Map::new()))
}

async fn handle_ipc_downgrade_request(
    request: lwm2m::Request,
    servers: &mut [DeviceServer],
) -> Result<serde_json::Value, anyhow::Error> {
    if !matches!(request.op, lwm2m::Method::Execute) {
        return Err(anyhow!("unsupported operation: {:?}", request.op));
    }

    let value = match request.payload {
        Some(lwm2m::Payload::Value(value)) => value,
        other => return Err(anyhow!("unsupported payload: {:?}", other)),
    };

    let id = match value.data {
        lwm2m::ValueData::String(Some(id)) => id,
        other => return Err(anyhow!("unsupported value data: {:?}", other)),
    };

    if let Some(device) = servers
        .iter_mut()
        .flat_map(|r| r.devices.iter_mut())
        .find(|device| device.id == id)
    {
        device.next_check = NextCheck::Immediately;
        device.force_check = true;
        device.oneshot_fota_loop = true;
    } else {
        anyhow::bail!("Can't find device '{id}'");
    }

    Ok(serde_json::json!(null))
}

async fn handle_ipc_retry_upload_request(
    request: lwm2m::Request,
    servers: &mut [DeviceServer],
) -> Result<serde_json::Value, anyhow::Error> {
    if !matches!(request.op, lwm2m::Method::Execute) {
        return Err(anyhow!("unsupported operation: {:?}", request.op));
    }

    let value = match request.payload {
        Some(lwm2m::Payload::Value(value)) => value,
        other => return Err(anyhow!("unsupported payload: {:?}", other)),
    };

    let ids = match value.data {
        lwm2m::ValueData::StringArray(mut ids) => ids.drain(..).flatten().collect::<Vec<_>>(),
        other => return Err(anyhow!("unsupported value data: {:?}", other)),
    };

    for id in ids {
        if let Some(device) = servers
            .iter_mut()
            .flat_map(|r| r.devices.iter_mut())
            .find(|d| d.id == id)
        {
            device.next_check = NextCheck::Immediately;
            device.force_check = true;
        } else {
            anyhow::bail!("Can't find device '{id}'");
        }
    }

    Ok(serde_json::json!(null))
}

async fn handle_ipc_request(
    request: lwm2m::Request,
    servers: &mut [DeviceServer],
) -> Result<serde_json::Value, anyhow::Error> {
    match request.entity.kind.clone() {
        lwm2m::EntityKind::Gateway { service } => {
            if service != "fwrolloutd" {
                return Err(anyhow!("unsupported service type: {}", service));
            }

            match request.entity.path.to_str() {
                // WORKAROUND: cloud adapter communicates with this service and
                //             produces error logs if we don't implement this.
                Some("devices") => handle_ipc_all_devices_request().await,
                Some("downgrade") => handle_ipc_downgrade_request(request, servers).await,
                Some("retry_upload") => handle_ipc_retry_upload_request(request, servers).await,
                _ => Err(anyhow!("unsupported path: {:?}", request.entity.path)),
            }
        }
        other => Err(anyhow!("unsupported entity kind: {:?}", other)),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    gardenalog::init_tracing();

    let span = tracing::span!(tracing::Level::INFO, "fwrolloutd", activity=%"fota");
    let _guard = span.enter();

    tracing::info!("Started");

    let args: Args = argh::from_env();
    *TENANT.lock().unwrap() = args.tenant;

    // NOTE: Due to the default impl of `Config` this is blocking.
    //       Technically this is against the async-rules but this is the main
    //       function, no other tasks are running yet, and we only do it this
    //       one time before doing anything else.
    let mut config = Config::load("config.yml").await?;

    if config.is_none() {
        config = Config::load("/etc/fwrolloutd.yml").await?;
    }

    let config = config.unwrap_or_default();
    tracing::debug!("{:#?}", config);

    let mut index = ImageIndex::new(&config);
    let start_index_load = tokio::time::Instant::now();

    let mut servers = Vec::<DeviceServer>::new();
    for name in &config.device_servers {
        let req_service_config = ReqServiceConfig {
            socket_path: format!("/tmp/{name}-command.ipc"),
            retry_interval: config.ipc_connect_retry_interval,
        };
        servers.push(DeviceServer::new(name, req_service_config).await);
    }

    struct IPCRequest {
        message: lwm2m::Request,
        resp_tx: tokio::sync::oneshot::Sender<Result<serde_json::Value, anyhow::Error>>,
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel(1);

    let rep_service = sg_ipc::RepService::new("/tmp/fwrolloutd-command.ipc");
    lwm2m::start_repservice(rep_service, "fwrolloutd".to_string(), move |message| {
        let tx = tx.clone();
        async move {
            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
            tx.send(IPCRequest { message, resp_tx })
                .await
                .map_err(|e| anyhow!("can't forward request to main task: {e}"))?;

            tokio::time::timeout(IPC_REQUEST_TIMEOUT, resp_rx)
                .await
                .context("timed out waiting for main task IPC answer")?
                .context("can't receive main task answer")?
        }
    })
    .context("can't start rep service")?;

    // Currently fwrolloutd does not publish any events. But this socket is required to have a
    // uniform API to other tools. See SG-19559.
    let (pub_service_builder, _pub_service) = sg_ipc::PubServiceBuilder::new();
    pub_service_builder
        .start("/tmp/fwrolloutd-event.ipc")
        .context("can't start IPC pub service")?;

    let mut duration = None;
    let mut sigusr1_stream =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
            .context("can't create SIGUSR1 listener")?;
    let mut last_aliveness_log = std::time::Instant::now();
    loop {
        // Sleep for x minutes
        // NOTE: We just don't want to do that on the first iteration
        if let Some(duration) = &duration {
            tracing::debug!("Entering sleep mode");
            let instant = tokio::time::Instant::now()
                .checked_add(*duration)
                .expect("can't represent sleep with instant");

            tokio::select! {
                // we're using an instant here so a loop-iteration doesn't
                // restart the timer. This can happen when receiving
                // invalid requests.
                _ = tokio::time::sleep_until(instant) => (),

                // We're doing it this way so we don't have to share all
                // state through a mutex.
                request = rx.recv() => {
                    match request {
                        None => anyhow::bail!("all IPC request senders are gone"),
                        Some(request) => if request.resp_tx.send(handle_ipc_request(request.message, &mut servers).await).is_err() {
                            tracing::info!("failed to send answer to IPC task");
                        }
                    };
                }

                _ = sigusr1_stream.recv() => {
                    tracing::info!("Interrupted by SIGUSR1");
                }
            }
        } else {
            duration = Some(std::time::Duration::from_secs(config.sleep_interval));
        }

        // check if initial index load happened
        if index.etag.is_none() {
            if let Err(e) = index.update().await {
                if start_index_load.elapsed() < std::time::Duration::from_secs(24 * 60 * 60) {
                    tracing::info!(error=%e.altdisplay(), url=config.index_url.as_str(), "Failed to load index (first 24h)");
                } else {
                    tracing::error!(error=%e.altdisplay(), url=config.index_url.as_str(), "Failed to load index");
                }
                // It doesn't make sense to keep going until we have some valid index.
                continue;
            }
        }

        // log once a day that service is active and which index is being used
        if last_aliveness_log.elapsed().as_secs() > 24 * 60 * 60 {
            last_aliveness_log = std::time::Instant::now();
            let index_etag = match index.etag {
                Some(ref etag) => etag.as_str(),
                None => "No index etag",
            };
            tracing::info!(remote = true, index_etag, "Service alive");
        }

        let mut server_device_infos = Vec::new();
        for server in &mut servers {
            match server.load_device_infos().await {
                Err(e) => {
                    tracing::error!(error=%e.altdisplay(), device_server=server.name.as_str(), "Failed to load device infos");
                    break;
                }
                Ok(v) => server_device_infos.push(v),
            }
        }

        if server_device_infos.len() != servers.len() {
            tracing::warn!("Missing device info - upload could be in progress. Abort checks.");
            continue;
        }

        if server_device_infos
            .iter()
            .flat_map(|device_infos| device_infos.iter())
            .any(|(_, info)| info.firmware_update_state == lwm2m::FirmwareUpdateState::Downloading)
        {
            tracing::info!("Image upload in progress. Abort checks.");
            continue;
        }

        for (server, device_infos) in servers.iter_mut().zip(server_device_infos.iter()) {
            match server.process(&mut index, device_infos).await {
                // that shouldn't happen but we don't care
                ProcessResult::DeviceDone => continue,
                ProcessResult::ServerDone => continue,
                ProcessResult::EverythingDone => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, ImageIndex};

    const JSON: &str = r#"{
  "ipso_definitions": "ipso_definitions_9630f6ea6a0608b03bfa279b340957fe68afcf4c.tar.gz",
  "firmware_updates": [
    {
      "model_number": "18869",
      "software_version": "ICD1.16_1.0.0.121",
      "firmware_version": "2.4.7-1.2.0-4349",
      "pkg_version": "4.0.0-1.5.3-2.4.1",
      "update": true,
      "url": "18869/4.0.0-1.5.3-2.4.1/watering_computer-2.4.1_fota_v2_bsa.container"
    },
    {
      "model_number": "18869",
      "software_version": "ICD1.16_1.0.1.128",
      "firmware_version": "2.5.0-1.2.1-4674",
      "pkg_version": "4.0.0-1.5.3-2.4.1",
      "update": true,
      "url": "18869/4.0.0-1.5.3-2.4.1/watering_computer-2.4.1_fota_v2_bsa.container"
    }
    ]
}"#;

    #[test]
    fn parse_index_v2_json() {
        let config = Config::default();
        let mut index = ImageIndex::new(&config);
        index.index_file = serde_json::from_str(JSON).unwrap();
        assert_eq!(
            index.index_file.ipso_definitions,
            "ipso_definitions_9630f6ea6a0608b03bfa279b340957fe68afcf4c.tar.gz"
        );

        let entry = &index.index_file.firmware_updates[0];
        assert_eq!(entry.model_number, "18869");
        assert_eq!(entry.software_version, "ICD1.16_1.0.0.121");
        assert_eq!(entry.firmware_version, "2.4.7-1.2.0-4349");
        assert_eq!(entry.pkg_version, "4.0.0-1.5.3-2.4.1");
        assert!(entry.update);
        assert_eq!(
            entry.url,
            "18869/4.0.0-1.5.3-2.4.1/watering_computer-2.4.1_fota_v2_bsa.container"
        );

        let entry = &index.index_file.firmware_updates[1];
        assert_eq!(entry.model_number, "18869");
        assert_eq!(entry.software_version, "ICD1.16_1.0.1.128");
        assert_eq!(entry.firmware_version, "2.5.0-1.2.1-4674");
        assert_eq!(entry.pkg_version, "4.0.0-1.5.3-2.4.1");
        assert!(entry.update);
        assert_eq!(
            entry.url,
            "18869/4.0.0-1.5.3-2.4.1/watering_computer-2.4.1_fota_v2_bsa.container"
        );
    }
}
