# caliptra-server

`caliptra-server` bridges QEMU MCU mailbox traffic to the `caliptra-mcu-sw`
emulator. QEMU sends complete mailbox commands over a Unix socket, this server
dispatches them into the emulator through the MCU mailbox registers, then returns
the mailbox response to QEMU.

## Design

The implementation is split into two layers:

- `src/mbox_server.rs` owns the Unix socket protocol and connection handling.
- `src/main.rs` owns the emulator, mailbox register access, and CPU stepping.

The socket layer never touches emulator state directly. Each socket request is
converted into an `MboxCommand` and sent to the main emulator loop over an
`mpsc` channel. The main loop is the only owner of the emulator, which avoids
concurrent mutation of CPU, bus, and peripheral state.

Request flow:

```text
QEMU aspeed-mcu-mbox
  -> Unix socket MBOX_EXECUTE
  -> mbox_server connection thread
  -> mpsc MboxCommand
  -> emulator main loop
  -> mailbox SRAM / CMD / DLEN / EXECUTE registers
  -> emulator steps until STATUS != BUSY
  -> MBOX_RESPONSE over Unix socket
```

## Socket Protocol

The protocol is little-endian and framed by a 12-byte header:

```text
u32 magic       = 0x4d424f58  // "MBOX"
u16 version     = 1
u16 command
u32 payload_len
```

Supported commands:

- `MBOX_EXECUTE = 1`: QEMU to server, payload is `u32 cmd`, `u32 dlen`, then
  word-padded mailbox SRAM bytes.
- `MBOX_RESPONSE = 2`: server to QEMU, payload is `u32 status`, `u32 dlen`, then
  word-padded response bytes.

## Runtime Behavior

The server starts the socket listener first, then initializes the emulator and
enters a step loop. Mailbox requests are accepted only after firmware sets the
`FIRMWARE_MAILBOX_READY` milestone in `FW_FLOW_STATUS`.

Dispatching a command is synchronous. While one mailbox command is being
processed, the main loop steps the emulator until the mailbox status changes
from `BUSY` or until the dispatch timeout is hit.

This server expects the emulator to run with:

```text
--test-feature test-mcu-mbox-driver
```

That feature lets `EXECUTE=1` raise the MCU mailbox interrupt in this emulator
setup.

## Build

Build the server:

```bash
cargo build
```

Build firmware images and the server:

```bash
./build-fw.sh
```

`build-fw.sh` generates firmware binaries under `fw/` and prints example launch
commands.

## Run

Example server invocation:

```bash
./target/debug/caliptra-server \
  --mbox-socket /tmp/mcu_mbox.sock \
  --rom fw/mcu_rom.bin \
  --firmware fw/mcu_runtime.bin \
  --caliptra-rom fw/caliptra_rom.bin \
  --caliptra-firmware fw/caliptra_fw.bin \
  --soc-manifest fw/soc_manifest.bin \
  --vendor-pk-hash "$(cat fw/vendor_pk_hash.txt)" \
  --hw-revision 2.0.0 \
  --test-feature test-mcu-mbox-driver \
  --no-stdin-uart
```

QEMU should use the same socket path:

```bash
qemu-system-arm \
  -machine ast1030-evb,mcu-mbox-socket=/tmp/mcu_mbox.sock \
  -kernel zephyr.elf \
  -nographic
```

## Manual Test Client

`mbox_send.py` can send mailbox commands directly to the server without QEMU:

```bash
python3 mbox_send.py --mbox-socket /tmp/mcu_mbox.sock fw-version
python3 mbox_send.py --mbox-socket /tmp/mcu_mbox.sock device-caps
python3 mbox_send.py --mbox-socket /tmp/mcu_mbox.sock random-generate --length 32
```

## Generated Files

The following are generated or local runtime artifacts and should not be
committed:

- `target/`
- `fw/*.bin`
- `fw/vendor_pk_hash.txt`
- `primary_flash`
- `secondary_flash`
- `.agents/`
- `.codex/`
