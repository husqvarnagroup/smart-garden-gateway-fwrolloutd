// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Concepts used in IpsoRegistry
//! - definition: XML file describing single IPSO object
//! - registry: All available IPSO definitions in flash storage (i.e root path: /var/lib/ipso_definitions).
//!   In the root path are following subdirectory:
//!   - base: IPSO definitions distributed with gateway image
//!   - fwrolloutd: IPSO definitions from downloaded IPSO archive which are not present in base subdirectory
//! - archive: Bundled IPSO definitions (tar-ball) downloaded from cloud. Archive name assumed to
//!   contain fingerprint to unique identify content.
//! - archive name: The name (including fingerprint) of a downloaded archive. That name is persisted on
//!   the filesystem (in the "archive-name-file") in order to identify the version of the current installed
//!   archive in fwrolloutd subdirectory.

use std::path::{Path, PathBuf};
use tokio::fs;
use url::Url;

use anyhow::{anyhow, bail, Context};
use async_compression::futures::bufread::GzipDecoder;
use async_tar::Archive;
use futures::TryStreamExt;
use std::process;
use tokio::io;
use tokio::io::AsyncWriteExt;

use futures::AsyncReadExt;
use futures_util::StreamExt;
use reqwest::{Error, Response};

#[derive(Debug)]
pub struct IpsoRegistry {
    registry_root_path: PathBuf,
}

impl IpsoRegistry {
    pub fn new(registry_path: &Path) -> Self {
        Self {
            registry_root_path: registry_path.to_path_buf(),
        }
    }

    #[tracing::instrument(name = "update-ipso", level = "info", skip_all)]
    pub async fn update_if_needed(&self, archive_url: &Url) -> anyhow::Result<()> {
        if self.is_update_needed(archive_url).await {
            match self.get_remote_archive(archive_url).await {
                Ok(response) => match response.status() {
                    reqwest::StatusCode::OK => {
                        self.update_registry(response).await?;
                        reload_lwm2m_server();
                        return Ok(());
                    }
                    other => anyhow::bail!("{}", other),
                },
                Err(error) => {
                    if error.is_connect() || error.is_timeout() {
                        // index.json just got successfully downloaded and now there are network
                        // issues. Often now new IPSOs will be available and this is not an issue
                        // at all. If it happens frequently, then code should be refactored to
                        // switch back to old index
                        tracing::warn!("Network-related problem updating IPSO definitions - accept rare occurrences");
                        return Ok(());
                    }
                    return Err(anyhow!(error));
                }
            }
        }
        Ok(())
    }

    /// Compare the archive file name from the URL (from the index file) with the
    /// name stored in the registry on the fs (if available).
    async fn is_update_needed(&self, update_url: &Url) -> bool {
        if let Some(archive_name_from_fs) = self.read_archive_name_file().await {
            if let Ok(archive_name_form_url) = Self::archive_name_from_url(update_url) {
                return archive_name_from_fs != archive_name_form_url;
            }
        }
        true
    }

    /// Fetch the archive from the web with HTTP(S).
    async fn get_remote_archive(&self, archive_url: &Url) -> Result<Response, Error> {
        let client = reqwest::Client::new();
        tracing::debug!("Getting {}", archive_url);
        client.get(archive_url.clone()).send().await
    }

    /// Unpack the downloaded archive in a "atomic" manner. So it is replaced later in case something
    /// went wrong.
    async fn update_registry(&self, response: Response) -> anyhow::Result<()> {
        let new_archive_filename = Self::archive_name_from_url(response.url())?;
        self.remove_archive_name_file().await?;
        self.remove_old_fwrolloutd_registry().await?;
        self.unpack_archive(response).await?;
        self.write_archive_name_file(new_archive_filename).await?;
        tracing::info!(
            "IPSO registry written to {:?}",
            self.registry_path_fwrolloutd()
        );
        Ok(())
    }

    /// Move the file (instead of removing). This is atomic on the filesystem.
    /// If there is a power-cut after the move the `fwrolloutd` will not find
    /// the file and act like there would not be a downloaded registry at all.
    /// The file will be removed with the whole old downloaded registry.
    async fn remove_archive_name_file(&self) -> anyhow::Result<()> {
        if self.archive_name_file_path().is_file() {
            /* move file atomically on fs if exists */
            fs::rename(
                &self.archive_name_file_path(),
                &self.archive_name_file_tmp_path(),
            )
            .await
            .context("Could not move registry name file")?;
        }

        Ok(())
    }

    /// This removes the complete 'fwrolloutd' subdirectory and recreates an
    /// empty one. This operation is not atomic on the filesystem.
    async fn remove_old_fwrolloutd_registry(&self) -> anyhow::Result<()> {
        /* remove whole directory if it's there */
        fs::remove_dir_all(self.registry_path_fwrolloutd())
            .await
            .ok();
        /* make sure the directory exists */
        fs::create_dir_all(self.registry_path_fwrolloutd()).await?;
        Ok(())
    }

    /// Unpack the definitions in the downloaded archive to the 'fwrolloutd' subdirectory in the
    /// repository. For each definition in the archive it checks if it is already available in the
    /// 'base' subdirectory and unpacks it only if it is not there or if the contents differ.
    async fn unpack_archive(&self, response: Response) -> anyhow::Result<()> {
        let url = response.url().to_string();
        /* stream from response */
        let reader = response
            .bytes_stream()
            .map_err(io::Error::other)
            .into_async_read();

        /* unzip */
        let gz = GzipDecoder::new(reader);

        /* un-tar */
        let archive = Archive::new(gz);

        /* handle each file in tar (we are not checking if it is a definition file) */
        let mut entries = archive
            .entries()
            .with_context(|| anyhow!("can't get entries from tar file {:?}", url))?;
        while let Some(entry) = entries.next().await {
            let mut file = entry?;
            /* read file content form tar */
            let mut content: String = String::new();
            file.read_to_string(&mut content)
                .await
                .with_context(|| anyhow!("Can't read content of {:?} in archive", file.path()))?;

            /* file path form tar */
            let file_path = file.path().context("Can't read path definition")?;
            let file_path = file_path.as_ref().as_os_str();
            let file_path = Path::new(file_path);
            /* write file if not in base registry */
            if self.definition_needs_update(file_path, &content).await {
                self.save_file_atomic(&self.registry_path_fwrolloutd().join(file_path), content)
                    .await
                    .context("Can't write file atomically")?
            }
        }

        Ok(())
    }

    /// The name (including fingerprint) is stored on disk into the "archive-name-file".
    /// This is used to identify which archive has been installed to ensure that we don't
    /// download an install the same archive over and over again.
    async fn write_archive_name_file(&self, archive_name: String) -> anyhow::Result<()> {
        let file_path = self.archive_name_file_path();
        self.save_file_atomic(&file_path, archive_name).await?;
        Ok(())
    }

    /// Write file content into a temporary file first and move it then to the final
    /// destination. This guarantees that the file content is complete in the final file because
    /// moving a file on the filesystem is atomic (in contrast to writing to a file).
    /// This step is important for embedded Linux.
    async fn save_file_atomic(&self, file_path: &PathBuf, content: String) -> anyhow::Result<()> {
        let temp_file_path = self.temp_file_path(file_path);

        let mut file = fs::File::create(&temp_file_path)
            .await
            .with_context(|| anyhow!("can't create file {:?}", temp_file_path))?;
        file.write_all(content.as_ref())
            .await
            .with_context(|| anyhow!("can't write file {:?}", temp_file_path))?;
        file.sync_all().await?;

        fs::rename(&temp_file_path, &file_path)
            .await
            .with_context(|| anyhow!("can't rename file {:?}", &temp_file_path))?;
        // file not yet durable for next few seconds. But if gateway reboots, download just restarts
        Ok(())
    }

    /// Extract the archive file name from the URL and check if it is not empty.
    fn archive_name_from_url(url: &Url) -> anyhow::Result<String> {
        let file_name = url
            .path_segments()
            .with_context(|| anyhow!("no segments in url {}", url))?
            .next_back()
            .unwrap_or_default()
            .to_string();

        if file_name.is_empty() {
            bail!("Could not get filename from URL {:?}", url);
        }
        Ok(file_name)
    }

    /// Read the name of the last updated registry from the "archive-name-file".
    async fn read_archive_name_file(&self) -> Option<String> {
        let archive_name_file_path = self.archive_name_file_path();
        if let Ok(str) = fs::read_to_string(archive_name_file_path).await {
            if !str.is_empty() {
                return Some(str);
            }
        }
        None
    }

    /// Check if a definition with the same name and content is not already available in the
    /// base subdirectory. Used to check it is necessary to unpack it.
    async fn definition_needs_update(&self, file_path: &Path, content_in_tar: &String) -> bool {
        let file_in_base = self.registry_path_base().join(file_path);
        if file_in_base.is_file() {
            if let Ok(content_from_base) = fs::read_to_string(file_in_base).await {
                if &content_from_base == content_in_tar {
                    return false;
                }
            }
        }
        true
    }

    /// Create name of temporary file when writing atomically to
    /// the filesystem.
    fn temp_file_path(&self, file_path: &Path) -> PathBuf {
        let mut temp_file_path = file_path.to_path_buf().into_os_string();
        temp_file_path.push(".tmp");
        let mut temp_file_path_buf = PathBuf::new();
        temp_file_path_buf.push(temp_file_path);
        temp_file_path_buf
    }

    /// The path to the "base" subdirectory in the registry.
    fn registry_path_base(&self) -> PathBuf {
        self.registry_root_path.join("base")
    }

    /// The path to the "fwrolloutd" subdirectory in the registry.
    fn registry_path_fwrolloutd(&self) -> PathBuf {
        self.registry_root_path.join("fwrolloutd")
    }

    /// The path to the "archive-name-file" where the name of the
    /// last unpacked archive is stored.
    fn archive_name_file_path(&self) -> PathBuf {
        self.registry_path_fwrolloutd().join("archive_name")
    }

    /// Temporary path for the "archive-name-file". Needed to enable atomic delete and write.
    fn archive_name_file_tmp_path(&self) -> PathBuf {
        self.temp_file_path(&self.archive_name_file_path())
    }
}

/// Find the PID of the lwm2mserver using `pgrep`.
fn get_lwm2m_pid() -> anyhow::Result<String> {
    let output = process::Command::new("/usr/bin/pgrep")
        .arg("-f")
        .arg(r#"(lwm2mserver_native|python.+lwm2mserver)"#)
        .output()
        .context("could not run pgrep")?
        .stdout;
    let pid = (*String::from_utf8(output)?
        .lines()
        .collect::<Vec<_>>()
        .first()
        .context("can't find PID of lwm2mserver")?)
    .to_string();
    Ok(pid)
}

/// Send the HUP signal to the given PID.
fn send_hup_signal(pid: &str) -> anyhow::Result<()> {
    process::Command::new("/bin/kill")
        .arg("-HUP")
        .arg(pid)
        .output()
        .context("could not run kill -HUP")?;
    Ok(())
}

/// Need to send a HUP signal to the lwm2mserver.
/// This is done in a manner that it also works on a developer machine.
fn reload_lwm2m_server() {
    match get_lwm2m_pid() {
        Ok(pid) => match send_hup_signal(pid.as_str()) {
            Ok(()) => {
                tracing::info!("lwm2mserver ({}) reloaded", pid.as_str());
            }
            Err(e) => {
                tracing::warn!("lwm2mserver not reloaded: {:?}", e);
            }
        },
        Err(e) => {
            tracing::warn!("lwm2mserver not reloaded: {:?}", e);
        }
    }
}
