use std::{
    io::SeekFrom,
    path::{Path, PathBuf},
};

use android_sparse_image::{
    split::split_image, ChunkHeader, FileHeader, FileHeaderBytes, CHUNK_HEADER_BYTES_LEN,
};
use anyhow::{bail, Context};
use clap::Parser;
use fastboot_protocol::nusb::NusbFastBoot;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt};

#[derive(Parser)]
enum Opts {
    GetVar { var: String },
    GetAllVars {},
    Flash { target: String, file: PathBuf },
    Reboot,
}

async fn flash_raw<R>(
    fb: &mut NusbFastBoot,
    target: &str,
    mut file: R,
    file_size: u32,
) -> anyhow::Result<()>
where
    R: AsyncRead + AsyncSeek + Unpin,
{
    println!("Uploading raw image directly");
    let mut sender = fb.download(file_size).await?;
    loop {
        let left = sender.left();
        if left == 0 {
            break;
        }
        let buf = sender.get_mut_data(left as usize).await?;
        file.read_exact(buf)
            .await
            .context("Failed to read from file")?;
    }

    sender.finish().await?;
    println!("Flashing data");
    fb.flash(target).await?;

    Ok(())
}

async fn flash(fb: &mut NusbFastBoot, target: &str, file: &Path) -> anyhow::Result<()> {
    let max_download = fb.get_var("max-download-size").await?;
    let max_download = fastboot_protocol::protocol::parse_u32_hex(&max_download)
        .with_context(|| anyhow::anyhow!("Failed to parse max download size: {max_download}"))?;
    println!("Max download size: {max_download}");

    let mut f = tokio::fs::File::open(file).await?;
    let mut header_bytes = FileHeaderBytes::default();
    f.read_exact(&mut header_bytes).await?;
    let splits = match FileHeader::from_bytes(&header_bytes) {
        Ok(header) => {
            println!("Preparing to flash android sparse image");
            let mut chunks = vec![];
            for _ in 0..header.chunks {
                let mut chunk_bytes = [0; CHUNK_HEADER_BYTES_LEN];
                f.read_exact(&mut chunk_bytes).await?;
                let chunk = ChunkHeader::from_bytes(&chunk_bytes)?;

                f.seek(SeekFrom::Current(chunk.data_size() as i64)).await?;
                chunks.push(chunk);
            }
            split_image(&header, &chunks, max_download)?
        }
        Err(android_sparse_image::ParseError::UnknownMagic) => {
            f.seek(SeekFrom::Start(0))
                .await
                .context("Seeking back to the start")?;
            let file_size = f
                .seek(SeekFrom::End(0))
                .await
                .context("Seek for determining file size")?;
            if file_size < max_download.into() {
                f.seek(SeekFrom::Start(0))
                    .await
                    .context("Seeking back to the start")?;
                return flash_raw(fb, target, f, file_size as u32).await;
            }
            android_sparse_image::split::split_raw(file_size as usize, max_download)?
        }
        Err(e) => bail!("Failed to parse sparse image: {e}"),
    };

    println!("Flashing in {} parts", splits.len());
    for (i, split) in splits.iter().enumerate() {
        println!("Downloading part {i}");
        let mut sender = fb.download(split.sparse_size() as u32).await?;

        sender.extend_from_slice(&split.header.to_bytes()).await?;
        for chunk in &split.chunks {
            sender.extend_from_slice(&chunk.header.to_bytes()).await?;
            f.seek(SeekFrom::Start(chunk.offset as u64))
                .await
                .context("Failed to seek input file")?;
            let mut left = chunk.size;
            while left > 0 {
                let buf = sender.get_mut_data(left).await?;
                left -= f
                    .read_exact(buf)
                    .await
                    .context("Failed to read from file")?;
            }
        }
        sender.finish().await?;
        println!("Flashing Part {i}");
        fb.flash(target).await?;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let opts = Opts::parse();

    let mut devices = fastboot_protocol::nusb::devices().await?;
    let info = devices
        .next()
        .ok_or_else(|| anyhow::anyhow!("No Device found"))?;

    println!(
        "Using Fastboot device: {}:{} M: {} P: {}",
        info.busnum(),
        info.device_address(),
        info.manufacturer_string().unwrap_or_default(),
        info.product_string().unwrap_or_default()
    );

    let mut fb = NusbFastBoot::from_info(&info).await?;

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
        Opts::Flash { target, file } => flash(&mut fb, &target, &file).await?,
        Opts::Reboot => fb.reboot().await?,
    }

    Ok(())
}
