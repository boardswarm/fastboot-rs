use std::{collections::HashMap, fmt::Display, io::Write};

use nusb::transfer::RequestBuffer;
use nusb::DeviceInfo;
use thiserror::Error;
use tracing::{debug, trace};
use tracing::{info, warn};

use crate::protocol::FastBootResponse;
use crate::protocol::{FastBootCommand, FastBootResponseParseError};

/// List fastboot devices
pub fn devices() -> std::result::Result<impl Iterator<Item = DeviceInfo>, nusb::Error> {
    Ok(nusb::list_devices()?.filter(|d| NusbFastBoot::find_fastboot_interface(d).is_some()))
}

/// Fastboot communication errors
#[derive(Debug, Error)]
pub enum NusbFastBootError {
    #[error("Transfer error: {0}")]
    Transfer(#[from] nusb::transfer::TransferError),
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
    Device(std::io::Error),
    #[error("Failed to claim interface: {0}")]
    Interface(std::io::Error),
    #[error("Failed to find interface for fastboot")]
    MissingInterface,
    #[error("Failed to find required endpoints for fastboot")]
    MissingEndpoints,
    #[error("Unknown fastboot response: {0}")]
    FastbootParseError(#[from] FastBootResponseParseError),
}

/// Nusb fastboot client
pub struct NusbFastBoot {
    interface: nusb::Interface,
    ep_out: u8,
    ep_in: u8,
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
    pub fn from_interface(interface: nusb::Interface) -> Result<Self, NusbFastBootOpenError> {
        let (ep_out, max_out, ep_in, max_in) = interface
            .descriptors()
            .find_map(|alt| {
                // Requires one bulk IN and one bulk OUT
                let (ep_out, max_out) = alt.endpoints().find_map(|end| {
                    if end.transfer_type() == nusb::transfer::EndpointType::Bulk
                        && end.direction() == nusb::transfer::Direction::Out
                    {
                        Some((end.address(), end.max_packet_size()))
                    } else {
                        None
                    }
                })?;
                let (ep_in, max_in) = alt.endpoints().find_map(|end| {
                    if end.transfer_type() == nusb::transfer::EndpointType::Bulk
                        && end.direction() == nusb::transfer::Direction::In
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
        Ok(Self {
            interface,
            ep_out,
            ep_in,
            max_in,
        })
    }

    /// Create a fastboot client based on a USB device. Interface number must be the fastboot
    /// interface
    #[tracing::instrument(skip_all, err)]
    pub fn from_device(device: nusb::Device, interface: u8) -> Result<Self, NusbFastBootOpenError> {
        let interface = device
            .claim_interface(interface)
            .map_err(NusbFastBootOpenError::Interface)?;
        Self::from_interface(interface)
    }

    /// Create a fastboot client based on device info. The correct interface will automatically be
    /// determined
    #[tracing::instrument(skip_all, err)]
    pub fn from_info(info: &DeviceInfo) -> Result<Self, NusbFastBootOpenError> {
        let interface =
            Self::find_fastboot_interface(info).ok_or(NusbFastBootOpenError::MissingInterface)?;
        let device = info.open().map_err(NusbFastBootOpenError::Device)?;
        Self::from_device(device, interface)
    }

    #[tracing::instrument(skip_all, err)]
    async fn send_data(&mut self, data: Vec<u8>) -> Result<(), NusbFastBootError> {
        self.interface.bulk_out(self.ep_out, data).await.status?;
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
    async fn read_response(&mut self) -> Result<FastBootResponse, FastBootResponseParseError> {
        let req = RequestBuffer::new(self.max_in);
        let resp = self.interface.bulk_in(self.ep_in, req).await;
        FastBootResponse::from_bytes(&resp.data)
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
    /// When successfull the [SendData] helper should be used to actually send the data
    pub async fn download(&mut self, size: u32) -> Result<SendData, NusbFastBootError> {
        let cmd = FastBootCommand::<&str>::Download(size);
        self.send_command(cmd).await?;
        loop {
            let resp = self.read_response().await?;
            match resp {
                FastBootResponse::Info(i) => println!("info: {i}"),
                FastBootResponse::Text(t) => info!("Text: {}", t),
                FastBootResponse::Data(size) => {
                    return Ok(SendData::new(self, size));
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
    pub async fn reboot_bootloader(&mut self) -> Result<(), NusbFastBootError> {
        let cmd = FastBootCommand::<&str>::RebootBootloader;
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

#[derive(Debug, Error)]
pub enum SendError {
    #[error("Trying to complete while nothing was Queued")]
    NothingQueued,
    #[error("Incorrect data length: expected {expected}, got {actual}")]
    IncorrectDataLength { actual: u32, expected: u32 },
    #[error(transparent)]
    Nusb(#[from] NusbFastBootError),
}

/// Data download helper
pub struct SendData<'s> {
    fastboot: &'s mut NusbFastBoot,
    queue: nusb::transfer::Queue<Vec<u8>>,
    size: u32,
    left: u32,
}

impl<'s> SendData<'s> {
    pub fn new(fastboot: &'s mut NusbFastBoot, size: u32) -> SendData<'s> {
        let queue = fastboot.interface.bulk_out_queue(fastboot.ep_out);
        Self {
            fastboot,
            queue,
            size,
            left: size,
        }
    }
}

impl SendData<'_> {
    /// Total size of the data transfer
    pub fn size(&self) -> u32 {
        self.size
    }

    /// Data left to be sent/queued
    pub fn left(&self) -> u32 {
        self.left
    }

    /// Pending transfers
    pub fn pending(&self) -> usize {
        self.queue.pending()
    }

    /// Queue a data transfer
    ///
    /// The total amount of data queued up should not be more then the total data size
    pub fn queue(&mut self, data: Vec<u8>) -> Result<(), SendError> {
        let len = data.len() as u32;
        if len > self.left {
            return Err(SendError::IncorrectDataLength {
                expected: self.size,
                actual: len - self.left + self.size,
            });
        }
        self.left -= len;
        self.queue.submit(data);
        Ok(())
    }

    /// Complete the next transfer.
    ///
    /// On success this returns the allocation of the finished transfer. This can be re-used to
    /// limit allocations.
    pub async fn complete_next(&mut self) -> Result<Vec<u8>, SendError> {
        if self.pending() == 0 {
            Err(SendError::NothingQueued)
        } else {
            let r = self.queue.next_complete().await;
            debug!("=> D: {:?}", r.status);
            r.status.map_err(NusbFastBootError::from)?;
            let mut data = r.data.reuse();
            data.truncate(0);
            Ok(data)
        }
    }

    /// Get a buffer to be queued up
    ///
    /// Helper to create or re-use a buffer to be queued up. By default this creates
    /// new vectors with a 1Mb capacity unless 3 or more buffers are queued, in which case it
    /// returns a buffer to re-use.
    ///
    /// Buffers are always returned with no data (len == 0)
    pub async fn get_buffer(&mut self) -> Result<Vec<u8>, SendError> {
        if self.pending() < 3 {
            Ok(Vec::with_capacity(1024 * 1024))
        } else {
            self.complete_next().await
        }
    }

    /// Finish all pending transfer
    ///
    /// This should only be called if all data has been queued up (matching the total size)
    pub async fn finish(mut self) -> Result<(), SendError> {
        if self.left != 0 {
            return Err(SendError::IncorrectDataLength {
                expected: self.size,
                actual: self.size - self.left,
            });
        }
        while self.pending() > 0 {
            self.complete_next().await?;
        }
        self.fastboot.handle_responses().await?;
        Ok(())
    }
}
