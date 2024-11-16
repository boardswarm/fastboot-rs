# Fastboot protocol implementation

Currently only supports USB client side (via nusb)

# Example client

Printing fastboot using the nusb:
```rust,no_run
#[tokio::main]
async fn main() -> anyhow::Result<()> {
  let mut devices = fastboot_protocol::nusb::devices()?;
  let info = devices.next()
    .ok_or_else(|| anyhow::anyhow!("No Device found"))?;
  let mut fb = fastboot_protocol::nusb::NusbFastBoot::from_info(&info)?;

  println!("Fastboot version: {}", fb.get_var("version").await?);
  Ok(())
}
```
