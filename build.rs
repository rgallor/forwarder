extern crate prost_build;

use std::io::Result;

fn main() -> Result<()> {
    prost_build::Config::new()
        .type_attribute(".", "#[derive(Eq)]")
        .compile_protos(&["src/items.proto"], &["src/"])?;

    Ok(())
}
