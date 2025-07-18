// Copyright 2024 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::archive_public::{ArchiveAddress, PublicArchive};
use super::{DownloadError, FileCostError, Metadata, UploadError};
use crate::client::high_level::data::DataAddress;
use crate::client::payment::PaymentOption;
use crate::client::Client;
use crate::data::CombinedChunks;
use crate::AttoTokens;
use bytes::Bytes;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

impl Client {
    /// Download file from network to local file system
    pub async fn file_download_public(
        &self,
        data_addr: &DataAddress,
        to_dest: PathBuf,
    ) -> Result<(), DownloadError> {
        let data = self.data_get_public(data_addr).await?;
        if let Some(parent) = to_dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
            debug!("Created parent directories {parent:?} for {to_dest:?}");
        }
        tokio::fs::write(to_dest.clone(), data).await?;
        debug!("Downloaded file to {to_dest:?} from the network address {data_addr:?}");
        Ok(())
    }

    /// Download directory from network to local file system
    pub async fn dir_download_public(
        &self,
        archive_addr: &ArchiveAddress,
        to_dest: PathBuf,
    ) -> Result<(), DownloadError> {
        let archive = self.archive_get_public(archive_addr).await?;
        debug!("Downloaded archive for the directory from the network at {archive_addr:?}");
        for (path, addr, _meta) in archive.iter() {
            self.file_download_public(addr, to_dest.join(path)).await?;
        }
        debug!(
            "All files in the directory downloaded to {:?} from the network address {:?}",
            to_dest.parent(),
            archive_addr
        );
        Ok(())
    }

    /// Upload the content of all files in a directory to the network.
    /// The directory is recursively walked and each file is uploaded to the network.
    ///
    /// The data maps of these files are uploaded on the network, making the individual files publicly available.
    ///
    /// This returns, but does not upload (!),the [`PublicArchive`] containing the data maps of the uploaded files.
    pub async fn dir_content_upload_public(
        &self,
        dir_path: PathBuf,
        payment_option: PaymentOption,
    ) -> Result<(AttoTokens, PublicArchive), UploadError> {
        info!("Uploading directory: {dir_path:?}");

        let encryption_results = self.encrypt_directory_files_public(dir_path).await?;

        let mut combined_chunks: CombinedChunks = vec![];
        let mut public_archive = PublicArchive::new();

        for encryption_result in encryption_results {
            match encryption_result {
                Ok((file_path, chunks, file_data)) => {
                    info!("Successfully encrypted file: {file_path:?}");
                    #[cfg(feature = "loud")]
                    println!("Successfully encrypted file: {file_path:?}");

                    let (relative_path, data_address, file_metadata) = file_data;
                    combined_chunks.push(((file_path, Some(data_address)), chunks));
                    public_archive.add_file(relative_path, data_address, file_metadata);
                }
                Err(err_msg) => {
                    error!("Error during file encryption: {err_msg}");
                    #[cfg(feature = "loud")]
                    println!("Error during file encryption: {err_msg}");
                }
            }
        }

        let total_cost = self.pay_and_upload(payment_option, combined_chunks).await?;

        for (file_path, data_addr, _meta) in public_archive.iter() {
            info!("Uploaded file: {file_path:?} to: {data_addr}");
            #[cfg(feature = "loud")]
            println!("Uploaded file: {file_path:?} to: {data_addr}");
        }

        Ok((total_cost, public_archive))
    }

    /// Same as [`Client::dir_content_upload_public`] but also uploads the archive to the network.
    ///
    /// Returns the [`ArchiveAddress`] of the uploaded archive.
    pub async fn dir_upload_public(
        &self,
        dir_path: PathBuf,
        payment_option: PaymentOption,
    ) -> Result<(AttoTokens, ArchiveAddress), UploadError> {
        let (cost1, archive) = self
            .dir_content_upload_public(dir_path, payment_option.clone())
            .await?;
        let (cost2, archive_addr) = self.archive_put_public(&archive, payment_option).await?;
        let total_cost = cost1.checked_add(cost2).unwrap_or_else(|| {
            error!("Total cost overflowed: {cost1:?} + {cost2:?}");
            cost1
        });
        Ok((total_cost, archive_addr))
    }

    /// Upload the content of a file to the network.
    /// Reads file, splits into chunks, uploads chunks, uploads datamap, returns DataAddr (pointing to the datamap)
    pub async fn file_content_upload_public(
        &self,
        path: PathBuf,
        payment_option: PaymentOption,
    ) -> Result<(AttoTokens, DataAddress), UploadError> {
        info!("Uploading file: {path:?}");
        #[cfg(feature = "loud")]
        println!("Uploading file: {path:?}");

        let data = tokio::fs::read(path.clone()).await?;
        let data = Bytes::from(data);
        let (cost, addr) = self.data_put_public(data, payment_option).await?;
        debug!("File {path:?} uploaded to the network at {addr:?}");
        Ok((cost, addr))
    }

    /// Get the cost to upload a file/dir to the network.
    /// quick and dirty implementation, please refactor once files are cleanly implemented
    pub async fn file_cost(&self, path: &PathBuf) -> Result<AttoTokens, FileCostError> {
        let mut archive = PublicArchive::new();
        let mut content_addrs = vec![];

        for entry in walkdir::WalkDir::new(path) {
            let entry = entry?;

            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path().to_path_buf();
            tracing::info!("Cost for file: {path:?}");

            let data = tokio::fs::read(&path).await?;
            let file_bytes = Bytes::from(data);

            let addrs = self.get_content_addrs(file_bytes.clone())?;

            // The first addr is always the chunk_map_name
            let map_xor_name = addrs[0].0;

            content_addrs.extend(addrs);

            let metadata = metadata_from_entry(&entry);

            archive.add_file(path, DataAddress::new(map_xor_name), metadata);
        }

        let serialized = archive.to_bytes()?;
        content_addrs.extend(self.get_content_addrs(serialized)?);

        let total_cost = self.get_cost_estimation(content_addrs).await?;
        debug!("Total cost for the directory: {total_cost:?}");
        Ok(total_cost)
    }
}

// Get metadata from directory entry. Defaults to `0` for creation and modification times if
// any error is encountered. Logs errors upon error.
pub(crate) fn metadata_from_entry(entry: &walkdir::DirEntry) -> Metadata {
    let fs_metadata = match entry.metadata() {
        Ok(metadata) => metadata,
        Err(err) => {
            tracing::warn!(
                "Failed to get metadata for `{}`: {err}",
                entry.path().display()
            );
            return Metadata {
                created: 0,
                modified: 0,
                size: 0,
                extra: None,
            };
        }
    };

    let unix_time = |property: &'static str, time: std::io::Result<SystemTime>| {
        time.inspect_err(|err| {
            tracing::warn!(
                "Failed to get '{property}' metadata for `{}`: {err}",
                entry.path().display()
            );
        })
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(SystemTime::UNIX_EPOCH)
        .inspect_err(|err| {
            tracing::warn!(
                "'{property}' metadata of `{}` is before UNIX epoch: {err}",
                entry.path().display()
            );
        })
        .unwrap_or(Duration::from_secs(0))
        .as_secs()
    };
    let created = unix_time("created", fs_metadata.created());
    let modified = unix_time("modified", fs_metadata.modified());

    Metadata {
        created,
        modified,
        size: fs_metadata.len(),
        extra: None,
    }
}
