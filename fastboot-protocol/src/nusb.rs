use nusb::transfer::{Buffer, In, Out};
use nusb::transfer::Bulk;
use std::{collections::HashMap, fmt::Display, io::Write};
pub use nusb::{transfer::TransferError, Device, DeviceInfo, Interface};
use nusb::descriptors::TransferType;
use nusb::Endpoint;
use nusb::transfer::Direction;
use thiserror::Error;
use tracing::{info, warn};
use tracing::{instrument, trace};

use crate::protocol::FastBootResponse;
use crate::protocol::{FastBootCommand, FastBootResponseParseError};

/// List fastboot devices
pub async fn devices() -> Result<impl Iterator<Item = DeviceInfo>, nusb::Error> {
    Ok(nusb::list_devices().await?.filter(|d| NusbFastBoot::find_fastboot_interface(d).is_some()))
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
    interface: Interface,
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
                    if end.transfer_type() == TransferType::Bulk
                        && end.direction() == Direction::In
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
        let ep_out = interface.endpoint::<Bulk, Out>(ep_out).unwrap();
        let ep_in = interface.endpoint::<Bulk, In>(ep_in).unwrap();
        Ok(Self {
            interface,
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
            .claim_interface(interface).await
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
        Ok(Self::from_device(device, interface).await?)
    }

    #[tracing::instrument(skip_all, err)]
    async fn send_data(&mut self, data: Vec<u8>) -> Result<(), NusbFastBootError> {
        self.ep_out.submit(data.into());
        self.ep_out.next_complete().await;
        Ok(())
    }

    async fn send_command<S: Display>(
        &mut self,
        cmd: FastBootCommand<S>
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

    /// Get the named variable
    ///
    /// The "all" variable is special; For that [Self::get_all_vars] should be used instead
    pub async fn get_var(&mut self, var: &str) -> Result<String, NusbFastBootError> {
        let cmd = FastBootCommand::GetVar(var);
        self.execute(cmd).await
    }

    /// Prepare a download of a given size
    ///
    /// When successfull the [DataDownload] helper should be used to actually send the data
    pub async fn download(&'_ mut self, size: u32) -> Result<DataDownload<'_>, NusbFastBootError> {
        let cmd = FastBootCommand::<&str>::Download(size);
        self.send_command(cmd).await?;
        loop {
            let resp = self.read_response().await?;
            match resp {
                FastBootResponse::Info(i) => info!("info: {i}"),
                FastBootResponse::Text(t) => info!("Text: {}", t),
                FastBootResponse::Data(size) => {
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
    current: Vec<u8>,
}

impl<'s> DataDownload<'s> {
    fn new(fastboot: &'s mut NusbFastBoot, size: u32) -> DataDownload<'s> {
        let current = Self::allocate_buffer(fastboot.max_out).to_vec();
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
        self.update_size(data.len() as u32)?;
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
        let size = left.min(max);
        self.update_size(size as u32)?;

        let len = self.current.len();
        self.current.resize(len + size, 0);
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

    fn allocate_buffer(max_out: usize) -> Vec<u8> {
        // Allocate about 1Mb of buffer ensuring it's always a multiple of the maximum out packet
        // size
        let size = (1024usize * 1024).next_multiple_of(max_out);
        Vec::with_capacity(size)
    }

    async fn next_buffer(&mut self) -> Result<(), DownloadError> {
        if self.fastboot.ep_out.pending() >= 3 {
            let completion = self.fastboot.ep_out.next_complete().await;
            completion.status.map_err(NusbFastBootError::from)?;
        }

        let data_to_send = std::mem::take(&mut self.current);
        if !data_to_send.is_empty() {
            self.fastboot.ep_out.submit(data_to_send.into());
        }

        let max_out = self.fastboot.max_out;
        self.current = Self::allocate_buffer(max_out);

        Ok(())
    }

    /// Finish all pending transfer
    ///
    /// This should only be called if all data has been queued up (matching the total size)
    #[instrument(skip_all, err)]
    pub async fn finish(mut self) -> Result<(), DownloadError> {
        if self.left != 0 {
            return Err(DownloadError::IncorrectDataLength {
                expected: self.size,
                actual: self.size - self.left,
            });
        }

        let final_data = std::mem::take(&mut self.current);
        if !final_data.is_empty() {
            self.fastboot.ep_out.submit(final_data.into());
        }
        while self.fastboot.ep_out.pending() > 0 {
            let completion = self.fastboot.ep_out.next_complete().await;
            completion.status.map_err(NusbFastBootError::from)?;
        }
        self.fastboot.handle_responses().await?;
        Ok(())
    }
}
