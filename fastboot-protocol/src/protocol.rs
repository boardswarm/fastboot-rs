use std::{fmt::Display, num::ParseIntError};
use thiserror::Error;
use tracing::trace;

/// Parse a hexadecimal 0x prefixed string e.g. 0x1234 into a u32
pub fn parse_u32_hex(hex: &str) -> Result<u32, ParseIntError> {
    // Can't create a custom ParseIntError; so if there is no 0x prefix, work around it providing
    // an invalid hex string
    let hex = hex.strip_prefix("0x").unwrap_or("invalid");
    u32::from_str_radix(hex, 16)
}

/// Fastboot commands
#[derive(Debug)]
pub enum FastBootCommand<S> {
    /// Get a variable value
    GetVar(S),
    /// Download a given length of data to the devices
    Download(u32),
    /// Verify
    Verify(u32),
    /// Flash downloaded to a partition
    Flash(S),
    /// Erase a partition
    Erase(S),
    /// Boot the downloaded data
    Boot,
    /// Continue booting
    Continue,
    /// Reboot the devices
    Reboot,
    /// Reboot into the bootloader
    RebootBootloader,
    /// Power off the device
    Powerdown,
}

impl<S: Display> Display for FastBootCommand<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FastBootCommand::GetVar(var) => write!(f, "getvar:{var}"),
            FastBootCommand::Download(size) => write!(f, "download:{size:08x}"),
            FastBootCommand::Verify(part) => write!(f, "verity:{part}"),
            FastBootCommand::Flash(part) => write!(f, "flash:{part}"),
            FastBootCommand::Erase(part) => write!(f, "erase:{part}"),
            FastBootCommand::Boot => write!(f, "boot"),
            FastBootCommand::Continue => write!(f, "continue"),
            FastBootCommand::Reboot => write!(f, "reboot"),
            FastBootCommand::RebootBootloader => write!(f, "reboot-bootloader"),
            FastBootCommand::Powerdown => write!(f, "powerdown"),
        }
    }
}

/// Parse errors for fastboot responses
#[derive(Error, Debug, PartialEq, Eq)]
pub enum FastBootResponseParseError {
    /// Unknown response type
    #[error("Unknown response type")]
    UnknownReply,
    /// Couldn't parse DATA length
    #[error("Couldn't parse DATA length")]
    DataLength,
}

/// Fastboot response
#[derive(Debug, PartialEq, Eq)]
pub enum FastBootResponse {
    /// Command succeeded with value (depending on command)
    Okay(String),
    /// Information from the device
    Info(String),
    /// Text data from the device
    Text(String),
    /// Command failed with provided reason
    Fail(String),
    /// Device expected the amount of data to be sent
    Data(u32),
}

impl<'a> FastBootResponse {
    fn from_parts(resp: &str, data: &'a str) -> Result<Self, FastBootResponseParseError> {
        trace!("Parsing Response: {} {}", resp, data);
        match resp {
            "OKAY" => Ok(Self::Okay(data.into())),
            "INFO" => Ok(Self::Info(data.into())),
            "TEXT" => Ok(Self::Text(data.into())),
            "FAIL" => Ok(Self::Fail(data.into())),
            "DATA" => {
                let offset = u32::from_str_radix(data, 16)
                    .or(Err(FastBootResponseParseError::DataLength))?;
                Ok(Self::Data(offset))
            }
            _ => Err(FastBootResponseParseError::UnknownReply),
        }
    }

    /// Parse a fastboot response from provided data
    pub fn from_bytes(bytes: &'a [u8]) -> Result<Self, FastBootResponseParseError> {
        if bytes.len() < 4 {
            Err(FastBootResponseParseError::UnknownReply)
        } else {
            let resp = std::str::from_utf8(&bytes[0..4]).unwrap();
            let data = std::str::from_utf8(&bytes[4..]).unwrap();

            Self::from_parts(resp, data)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn parse_valid_u32_hex() {
        let hex = parse_u32_hex("0x123456").unwrap();
        assert_eq!(0x123456, hex);
    }
    #[test]
    fn parse_invalid_u32_hex() {
        parse_u32_hex("123456").unwrap_err();
    }

    #[test]
    fn response_parse_ok() {
        let r = FastBootResponse::from_bytes(b"OKAYtest").unwrap();
        assert_eq!(r, FastBootResponse::Okay("test".to_string()));
    }

    #[test]
    fn response_parse_fail() {
        let r = FastBootResponse::from_bytes(b"FAILtest").unwrap();
        assert_eq!(r, FastBootResponse::Fail("test".to_string()));
    }

    #[test]
    fn response_parse_info() {
        let r = FastBootResponse::from_bytes(b"INFOtest").unwrap();
        assert_eq!(r, FastBootResponse::Info("test".to_string()));
    }

    #[test]
    fn response_parse_text() {
        let r = FastBootResponse::from_bytes(b"TEXTtest").unwrap();
        assert_eq!(r, FastBootResponse::Text("test".to_string()));
    }
    #[test]
    fn response_parse_data() {
        let r = FastBootResponse::from_bytes(b"DATA00123456").unwrap();
        assert_eq!(r, FastBootResponse::Data(0x123456));
    }

    #[test]
    fn response_parse_invalid() {
        let e = FastBootResponse::from_bytes(b"UNKN").unwrap_err();
        assert_eq!(e, FastBootResponseParseError::UnknownReply);
    }

    #[test]
    fn response_parse_too_short() {
        let e = FastBootResponse::from_bytes(b"UN").unwrap_err();
        assert_eq!(e, FastBootResponseParseError::UnknownReply);
    }
}
