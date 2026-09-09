use nusb::descriptors::TransferType;
use nusb::transfer::Bulk;
use nusb::transfer::Direction;
use nusb::transfer::{Buffer, In, Out};
use nusb::Endpoint;
pub use nusb::{transfer::TransferError, Device, DeviceInfo, Interface};
use std::{collections::HashMap, fmt::Display, io::Write};
use thiserror::Error;
use tracing::{info, warn};
use tracing::{instrument, trace};

use crate::protocol::FastBootResponse;
use crate::protocol::{FastBootCommand, FastBootResponseParseError};

/// List fastboot devices
pub async fn devices() -> Result<impl Iterator<Item = DeviceInfo>, nusb::Error> {
    Ok(nusb::list_devices()
        .await?
        .filter(|d| NusbFastBoot::find_fastboot_interface(d).is_some()))
}

/// Fastboot communication errors
#[derive(Debug, Error)]
pub enum NusbFastBootError {
    #[error("Transfer error: {0}")]
    Transfer(#[from] TransferError),
    #[error("Fastboot client failure: {0}")]
    FastbootFailed(String),
    #[error("Unexpected fastboot response")]
    FastbootUnexpectedReply,
    #[error("Unknown fastboot response: {0}")]
    FastbootParseError(#[from] FastBootResponseParseError),
    #[error("Invalid fastboot command")]
    InvalidCommand,
    #[error("Incorrect transfer length: expected {expected}, got {actual}")]
    IncorrectLength { expected: usize, actual: usize },
}

/// Errors when opening the fastboot device
#[derive(Debug, Error)]
pub enum NusbFastBootOpenError {
    #[error("Failed to open device: {0}")]
    Device(nusb::Error),
    #[error("Failed to claim interface: {0}")]
    Interface(nusb::Error),
    #[error("Failed to find interface for fastboot")]
    MissingInterface,
    #[error("Failed to find required endpoints for fastboot")]
    MissingEndpoints,
    #[error("Unknown fastboot response: {0}")]
    FastbootParseError(#[from] FastBootResponseParseError),
}

/// Nusb fastboot client
pub struct NusbFastBoot {
    ep_out: Endpoint<Bulk, Out>,
    max_out: usize,
    ep_in: Endpoint<Bulk, In>,
    max_in: usize,
}

impl NusbFastBoot {
    /// Find fastboot interface within a USB device
    pub fn find_fastboot_interface(info: &DeviceInfo) -> Option<u8> {
        info.interfaces().find_map(|i| {
            if i.class() == 0xff && i.subclass() == 0x42 && i.protocol() == 0x3 {
                Some(i.interface_number())
            } else {
                None
            }
        })
    }

    /// Create a fastboot client based on a USB interface. Interface is assumed to be a fastboot
    /// interface
    #[tracing::instrument(skip_all, err)]
    pub fn from_interface(interface: Interface) -> Result<Self, NusbFastBootOpenError> {
        let (ep_out, max_out, ep_in, max_in) = interface
            .descriptors()
            .find_map(|alt| {
                // Requires one bulk IN and one bulk OUT
                let (ep_out, max_out) = alt.endpoints().find_map(|end| {
                    if end.transfer_type() == TransferType::Bulk
                        && end.direction() == Direction::Out
                    {
                        Some((end.address(), end.max_packet_size()))
                    } else {
                        None
                    }
                })?;

                let (ep_in, max_in) = alt.endpoints().find_map(|end| {
                    if end.transfer_type() == TransferType::Bulk && end.direction() == Direction::In
                    {
                        Some((end.address(), end.max_packet_size()))
                    } else {
                        None
                    }
                })?;
                Some((ep_out, max_out, ep_in, max_in))
            })
            .ok_or(NusbFastBootOpenError::MissingEndpoints)?;
        trace!(
            "Fastboot endpoints: OUT: {} (max: {}), IN: {} (max: {})",
            ep_out,
            max_out,
            ep_in,
            max_in
        );
        let ep_out = interface
            .endpoint::<Bulk, Out>(ep_out)
            .map_err(NusbFastBootOpenError::Interface)?;
        let ep_in = interface
            .endpoint::<Bulk, In>(ep_in)
            .map_err(NusbFastBootOpenError::Interface)?;
        Ok(Self {
            ep_out,
            max_out,
            ep_in,
            max_in,
        })
    }

    /// Create a fastboot client based on a USB device. Interface number must be the fastboot
    /// interface
    #[tracing::instrument(skip_all, err)]
    pub async fn from_device(device: Device, interface: u8) -> Result<Self, NusbFastBootOpenError> {
        let interface = device
            .claim_interface(interface)
            .await
            .map_err(NusbFastBootOpenError::Interface)?;
        Self::from_interface(interface)
    }

    /// Create a fastboot client based on device info. The correct interface will automatically be
    /// determined
    #[tracing::instrument(skip_all, err)]
    pub async fn from_info(info: &DeviceInfo) -> Result<Self, NusbFastBootOpenError> {
        let interface =
            Self::find_fastboot_interface(info).ok_or(NusbFastBootOpenError::MissingInterface)?;
        let device = info.open().await.map_err(NusbFastBootOpenError::Device)?;
        Self::from_device(device, interface).await
    }

    #[tracing::instrument(skip_all, err)]
    async fn send_data(&mut self, data: Vec<u8>) -> Result<(), NusbFastBootError> {
        let expected = data.len();
        self.ep_out.submit(data.into());
        let completion = self.ep_out.next_complete().await;
        completion.status?;
        check_length(expected, completion.actual_len)?;
        Ok(())
    }

    async fn send_command<S: Display>(
        &mut self,
        cmd: FastBootCommand<S>,
    ) -> Result<(), NusbFastBootError> {
        let mut out = vec![];
        // Only fails if memory allocation fails
        out.write_fmt(format_args!("{}", cmd)).unwrap();
        trace!(
            "Sending command: {}",
            std::str::from_utf8(&out).unwrap_or("Invalid utf-8")
        );
        self.send_data(out).await
    }

    #[tracing::instrument(skip_all, err)]
    async fn read_response(&mut self) -> Result<FastBootResponse, NusbFastBootError> {
        self.ep_in.submit(Buffer::new(self.max_in));
        let resp = self
            .ep_in
            .next_complete()
            .await
            .into_result()
            .map_err(NusbFastBootError::Transfer)?;
        Ok(FastBootResponse::from_bytes(&resp)?)
    }

    #[tracing::instrument(skip_all, err)]
    async fn handle_responses(&mut self) -> Result<String, NusbFastBootError> {
        loop {
            let resp = self.read_response().await?;
            trace!("Response: {:?}", resp);
            match resp {
                FastBootResponse::Info(_) => (),
                FastBootResponse::Text(_) => (),
                FastBootResponse::Data(_) => {
                    return Err(NusbFastBootError::FastbootUnexpectedReply)
                }
                FastBootResponse::Okay(value) => return Ok(value),
                FastBootResponse::Fail(fail) => {
                    return Err(NusbFastBootError::FastbootFailed(fail))
                }
            }
        }
    }

    #[tracing::instrument(skip_all, err)]
    async fn execute<S: Display>(
        &mut self,
        cmd: FastBootCommand<S>,
    ) -> Result<String, NusbFastBootError> {
        self.send_command(cmd).await?;
        self.handle_responses().await
    }

    fn allocate(&self) -> Buffer {
        // Allocate about 1Mb of buffer ensuring it's always a multiple of the maximum out packet
        // size
        let size = (1024usize * 1024).next_multiple_of(self.max_out);
        self.ep_out.allocate(size)
    }

    /// Execute an implementation-specific command with INFO/TEXT/OKAY/FAIL
    /// responses. Commands with a data phase require a separate method.
    pub async fn command(&mut self, command: &str) -> Result<String, NusbFastBootError> {
        validate_command(command)?;
        self.send_data(command.as_bytes().to_vec()).await?;
        self.handle_responses().await
    }

    /// Fetch a bounded range from a partition. Requires device fetch support.
    /// The returned bytes are accepted only after the complete DATA payload and
    /// final OKAY. Callers should chunk large partitions to bound memory use.
    pub async fn fetch(
        &mut self,
        partition: &str,
        offset: u64,
        length: u32,
    ) -> Result<Vec<u8>, NusbFastBootError> {
        if partition.contains(':') || length == 0 || offset.checked_add(u64::from(length)).is_none()
        {
            return Err(NusbFastBootError::InvalidCommand);
        }
        let command = format!("fetch:{partition}:{offset:x}:{length:x}");
        validate_command(&command)?;
        self.send_data(command.into_bytes()).await?;
        loop {
            match self.read_response().await? {
                FastBootResponse::Info(_) | FastBootResponse::Text(_) => (),
                FastBootResponse::Data(actual) => {
                    check_length(length as usize, actual as usize)?;
                    break;
                }
                FastBootResponse::Fail(reason) => {
                    return Err(NusbFastBootError::FastbootFailed(reason))
                }
                _ => return Err(NusbFastBootError::FastbootUnexpectedReply),
            }
        }
        let mut data = Vec::with_capacity(length as usize);
        while data.len() < length as usize {
            let remaining = length as usize - data.len();
            let requested = remaining.min(1024 * 1024).next_multiple_of(self.max_in);
            self.ep_in.submit(Buffer::new(requested));
            let completion = self.ep_in.next_complete().await;
            completion.status?;
            if completion.actual_len == 0 || completion.actual_len > remaining {
                return Err(NusbFastBootError::IncorrectLength {
                    expected: remaining,
                    actual: completion.actual_len,
                });
            }
            data.extend_from_slice(&completion.buffer);
        }
        self.handle_responses().await?;
        Ok(data)
    }

    /// Get the named variable
    ///
    /// The "all" variable is special; For that [Self::get_all_vars] should be used instead
    pub async fn get_var(&mut self, var: &str) -> Result<String, NusbFastBootError> {
        let cmd = FastBootCommand::GetVar(var);
        self.execute(cmd).await
    }

    /// Prepare a download of a given size
    ///
    /// When successful the [DataDownload] helper should be used to actually send the data
    pub async fn download(&'_ mut self, size: u32) -> Result<DataDownload<'_>, NusbFastBootError> {
        let cmd = FastBootCommand::<&str>::Download(size);
        self.send_command(cmd).await?;
        loop {
            let resp = self.read_response().await?;
            match resp {
                FastBootResponse::Info(i) => info!("info: {i}"),
                FastBootResponse::Text(t) => info!("Text: {}", t),
                FastBootResponse::Data(actual) => {
                    check_length(size as usize, actual as usize)?;
                    return Ok(DataDownload::new(self, size));
                }
                FastBootResponse::Okay(_) => {
                    return Err(NusbFastBootError::FastbootUnexpectedReply)
                }
                FastBootResponse::Fail(fail) => {
                    return Err(NusbFastBootError::FastbootFailed(fail))
                }
            }
        }
    }

    /// Flash downloaded data to a given target partition
    pub async fn flash(&mut self, target: &str) -> Result<(), NusbFastBootError> {
        let cmd = FastBootCommand::Flash(target);
        self.execute(cmd).await.map(|v| {
            trace!("Flash ok: {v}");
        })
    }

    /// Continue booting
    pub async fn continue_boot(&mut self) -> Result<(), NusbFastBootError> {
        let cmd = FastBootCommand::<&str>::Continue;
        self.execute(cmd).await.map(|v| {
            trace!("Continue ok: {v}");
        })
    }

    /// Erasing the given target partition
    pub async fn erase(&mut self, target: &str) -> Result<(), NusbFastBootError> {
        let cmd = FastBootCommand::Erase(target);
        self.execute(cmd).await.map(|v| {
            trace!("Erase ok: {v}");
        })
    }

    /// Reboot the device
    pub async fn reboot(&mut self) -> Result<(), NusbFastBootError> {
        let cmd = FastBootCommand::<&str>::Reboot;
        self.execute(cmd).await.map(|v| {
            trace!("Reboot ok: {v}");
        })
    }

    /// Reboot the device to the bootloader
    pub async fn reboot_to(&mut self, mode: &str) -> Result<(), NusbFastBootError> {
        let cmd = FastBootCommand::<&str>::RebootTo(mode);
        self.execute(cmd).await.map(|v| {
            trace!("Reboot ok: {v}");
        })
    }

    /// Retrieve all variables
    pub async fn get_all_vars(&mut self) -> Result<HashMap<String, String>, NusbFastBootError> {
        let cmd = FastBootCommand::GetVar("all");
        self.send_command(cmd).await?;
        let mut vars = HashMap::new();
        loop {
            let resp = self.read_response().await?;
            trace!("Response: {:?}", resp);
            match resp {
                FastBootResponse::Info(i) => {
                    let Some((key, value)) = i.rsplit_once(':') else {
                        warn!("Failed to parse variable: {i}");
                        continue;
                    };
                    vars.insert(key.trim().to_string(), value.trim().to_string());
                }
                FastBootResponse::Text(t) => info!("Text: {}", t),
                FastBootResponse::Data(_) => {
                    return Err(NusbFastBootError::FastbootUnexpectedReply)
                }
                FastBootResponse::Okay(_) => {
                    return Ok(vars);
                }
                FastBootResponse::Fail(fail) => {
                    return Err(NusbFastBootError::FastbootFailed(fail))
                }
            }
        }
    }
}

/// Error during data download
#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("Trying to complete while nothing was Queued")]
    NothingQueued,
    #[error("Incorrect data length: expected {expected}, got {actual}")]
    IncorrectDataLength { actual: u32, expected: u32 },
    #[error(transparent)]
    Nusb(#[from] NusbFastBootError),
}

/// Data download helper
///
/// To success stream data over usb it needs to be sent in blocks that are multiple of the max
/// endpoint size, otherwise the receiver may complain. It also should only send as much data as
/// was indicate in the DATA command.
///
/// This helper ensures both invariants are met. To do this data needs to be sent by using
/// [DataDownload::extend_from_slice] or [DataDownload::get_mut_data], after sending the data [DataDownload::finish] should be called to
/// validate and finalize.
pub struct DataDownload<'s> {
    fastboot: &'s mut NusbFastBoot,
    size: u32,
    left: u32,
    current: Buffer,
}

impl<'s> DataDownload<'s> {
    fn new(fastboot: &'s mut NusbFastBoot, size: u32) -> DataDownload<'s> {
        let current = fastboot.allocate();
        Self {
            fastboot,
            size,
            left: size,
            current,
        }
    }
}

impl DataDownload<'_> {
    /// Total size of the data transfer
    pub fn size(&self) -> u32 {
        self.size
    }

    /// Data left to be sent/queued
    pub fn left(&self) -> u32 {
        self.left
    }

    /// Extend the streaming from a slice
    ///
    /// This will copy all provided data and send it out if enough is collected. The total amount
    /// of data being sent should not exceed the download size
    pub async fn extend_from_slice(&mut self, mut data: &[u8]) -> Result<(), DownloadError> {
        let size = u32::try_from(data.len()).map_err(|_| NusbFastBootError::IncorrectLength {
            expected: self.left as usize,
            actual: data.len(),
        })?;
        self.update_size(size)?;
        loop {
            let left = self.current.capacity() - self.current.len();
            if left >= data.len() {
                self.current.extend_from_slice(data);
                break;
            } else {
                self.current.extend_from_slice(&data[0..left]);
                self.next_buffer().await?;
                data = &data[left..];
            }
        }
        Ok(())
    }

    /// This will provide a mutable reference to a [u8] of at most `max` size. The returned slice
    /// should be completely filled with data to be downloaded to the device
    ///
    /// The total amount of data should not exceed the download size
    pub async fn get_mut_data(&mut self, max: usize) -> Result<&mut [u8], DownloadError> {
        if self.current.capacity() == self.current.len() {
            self.next_buffer().await?;
        }

        let left = self.current.capacity() - self.current.len();
        let size = left.min(max).min(self.left as usize);
        self.update_size(size as u32)?;

        let len = self.current.len();
        self.current.extend_fill(size, 0);
        Ok(&mut self.current[len..])
    }

    fn update_size(&mut self, size: u32) -> Result<(), DownloadError> {
        if size > self.left {
            return Err(DownloadError::IncorrectDataLength {
                expected: self.size,
                actual: size - self.left + self.size,
            });
        }
        self.left -= size;
        Ok(())
    }

    async fn next_buffer(&mut self) -> Result<(), DownloadError> {
        let mut next = if self.fastboot.ep_out.pending() < 3 {
            self.fastboot.allocate()
        } else {
            let mut completion = self.fastboot.ep_out.next_complete().await;
            completion.status.map_err(NusbFastBootError::from)?;
            check_length(completion.buffer.len(), completion.actual_len)?;
            completion.buffer.clear();
            completion.buffer
        };

        std::mem::swap(&mut next, &mut self.current);
        self.fastboot.ep_out.submit(next);

        Ok(())
    }

    /// Finish all pending transfer
    ///
    /// This should only be called if all data has been queued up (matching the total size)
    #[instrument(skip_all, err)]
    pub async fn finish(self) -> Result<(), DownloadError> {
        if self.left != 0 {
            return Err(DownloadError::IncorrectDataLength {
                expected: self.size,
                actual: self.size - self.left,
            });
        }

        if !self.current.is_empty() {
            self.fastboot.ep_out.submit(self.current);
        }

        while self.fastboot.ep_out.pending() > 0 {
            let completion = self.fastboot.ep_out.next_complete().await;
            completion.status.map_err(NusbFastBootError::from)?;
            check_length(completion.buffer.len(), completion.actual_len)?;
        }

        self.fastboot.handle_responses().await?;
        Ok(())
    }
}

fn validate_command(command: &str) -> Result<(), NusbFastBootError> {
    if command.is_empty()
        || command.len() > 64
        || !command.bytes().all(|b| (0x20..=0x7e).contains(&b))
    {
        Err(NusbFastBootError::InvalidCommand)
    } else {
        Ok(())
    }
}

fn check_length(expected: usize, actual: usize) -> Result<(), NusbFastBootError> {
    if expected == actual {
        Ok(())
    } else {
        Err(NusbFastBootError::IncorrectLength { expected, actual })
    }
}

#[cfg(test)]
mod transfer_checks {
    use super::*;
    #[test]
    fn rejects_short_or_excess_completions_and_changed_download_size() {
        assert!(check_length(4096, 2048).is_err());
        assert!(check_length(4096, 4097).is_err());
        assert!(check_length(4096, 4096).is_ok());
    }
    #[test]
    fn bounded_generic_commands() {
        assert!(validate_command("oem custom-command").is_ok());
        for bad in ["", "getvar:version\0", "reboot\n", &"x".repeat(65)] {
            assert!(validate_command(bad).is_err());
        }
    }
}
