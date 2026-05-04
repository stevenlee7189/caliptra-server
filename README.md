# caliptra-server

`caliptra-server` is a small Rust service that bridges the Caliptra software emulator and QEMU.

## Build

Build a debug binary:

```bash
cargo build
```

Build an optimized binary:

```bash
cargo build --release
```

The crate pulls its Caliptra dependencies from the upstream `caliptra-sw` Git repository.

## Run

The server needs three inputs:

- `--socket <path>`: Unix socket path to listen on
- `--rom <path>`: Caliptra ROM image
- `--firmware <path>`: Caliptra firmware bundle

Example:

```bash
cargo run -- --socket /tmp/caliptra.sock \
  --rom images/caliptra-rom.bin \
  --firmware images/caliptra-fw.bundle
```

If you built the release binary, run it directly instead:

```bash
./target/release/caliptra-server --socket /tmp/caliptra.sock \
  --rom images/caliptra-rom.bin \
  --firmware images/caliptra-fw.bundle
```

The server boots the emulator, waits until runtime is ready, and then serves APB requests over the Unix socket.
