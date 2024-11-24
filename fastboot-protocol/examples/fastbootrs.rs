use std::{io::SeekFrom, path::PathBuf};

use anyhow::Context;
use clap::Parser;
use fastboot_protocol::nusb::NusbFastBoot;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::trace;

#[derive(Parser)]
enum Opts {
    GetVar { var: String },
    GetAllVars {},
    Flash { target: String, file: PathBuf },
    Reboot,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let opts = Opts::parse();

    let mut devices = fastboot_protocol::nusb::devices()?;
    let info = devices
        .next()
        .ok_or_else(|| anyhow::anyhow!("No Device found"))?;

    println!(
        "Using Fastboot device: {}:{} M: {} P: {}",
        info.bus_number(),
        info.device_address(),
        info.manufacturer_string().unwrap_or_default(),
        info.product_string().unwrap_or_default()
    );

    let mut fb = NusbFastBoot::from_info(&info)?;

    match opts {
        Opts::GetVar { var } => {
            let r = fb.get_var(&var).await?;
            println!("{var}: {r:?}");
        }
        Opts::GetAllVars {} => {
            let r = fb.get_all_vars().await?;
            for (k, v) in r {
                println!("{k}: {v}");
            }
        }
        Opts::Flash { target, file } => {
            let mut f = tokio::fs::File::open(file).await?;
            let len = f
                .seek(SeekFrom::End(0))
                .await
                .context("Seek for determining file size")?;
            f.seek(SeekFrom::Start(0))
                .await
                .context("Seeking back to the start")?;
            let mut sender = fb.download(len as u32).await?;
            println!("Uploading data");
            loop {
                let left = sender.left();
                if left == 0 {
                    break;
                }
                let mut buf = sender.get_buffer().await?;

                buf.resize(buf.capacity().min(left as usize), 0);
                trace!("Reading {}, left: {}", buf.len(), sender.left());
                f.read_exact(&mut buf)
                    .await
                    .context("Failed to read from file")?;
                sender.queue(buf)?;
            }

            sender.finish().await?;
            println!("Flashing data");
            fb.flash(&target).await?;
        }
        Opts::Reboot => fb.reboot().await?,
    }

    Ok(())
}
