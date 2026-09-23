# fwrolloutd

`fwrolloutd` is one of the services running on the GARDENA smart Gateway. It
periodically checks a web server for firmware updates. If a new firmware version
is available, it downloads the file and sends it to the corresponding device
service. The device service then install the firmware update on the connected
devices (FOTA).
`fwrolloutd` does not update the Linux system running on the gateway itself.

## Running the Application

The application is meant to run as a service on a GARDENA smart Gateway.
Running on a PC is not supported and can cause errors.

```sh
RUST_LOG=info cargo run
```

## Running Unit Tests

```sh
cargo test
```

## Linting and Formatting

```sh
cargo clippy --all-targets --all-features -- -Dwarnings
```

```sh
cargo fmt
```
