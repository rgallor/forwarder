extern crate prost_build;

use std::io::Result;

fn main() -> Result<()> {
    prost_build::Config::new()
        .type_attribute(".", "#[derive(Eq)]")
        .compile_protos(&["src/proto.proto"], &["src/"])?;

    prost_build::Config::new()
        .type_attribute(".", "#[derive(Eq)]")
        .compile_protos(
            &[
                "proto/edgehog_device_forwarder/message.proto",
                "proto/edgehog_device_forwarder/http.proto",
                "proto/edgehog_device_forwarder/ws.proto",
            ],
            &["proto/"],
        )?;

    Ok(())
}
