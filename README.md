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

Example server invocation (full runtime with Caliptra images):

```bash
./target/debug/caliptra-server \
  --mbox-socket /tmp/mcu_mbox.sock \
  --rom            fw/mcu_rom.bin \
  --firmware       fw/mcu_runtime.bin \
  --caliptra-rom   fw/caliptra_rom.bin \
  --caliptra-firmware fw/caliptra_fw.bin \
  --soc-manifest   fw/soc_manifest.bin \
  --vendor-pk-hash "$(cat fw/vendor_pk_hash.txt)" \
  --hw-revision    2.0.0 \
  --rom-offset  0x80000000 --rom-size  0x10000  \
  --dccm-offset 0x50000000 --dccm-size 0x4000   \
  --sram-offset 0x40000000 --sram-size 0x80000  \
  --pic-offset  0x60000000 \
  --i3c-offset  0x20004000 --i3c-size  0x1000   \
  --mci-offset  0x21000000 --mci-size  0xe00000 \
  --mbox-offset 0x30020000 --mbox-size 0x28     \
  --soc-offset  0x30030000 --soc-size  0x5e0    \
  --otp-offset  0x70000000 --otp-size  0x140    \
  --lc-offset   0x70000400 --lc-size   0x8c     \
  --device-security-state 3 \
  --test-feature test-mcu-mbox-driver \
  --no-stdin-uart
```

QEMU should use the same socket path:

```bash
qemu-system-arm \
    -machine ast1040-evb,cptra-peer=peer0 \
    -chardev socket,id=cptra0,path=/tmp/mcu_mbox.sock \
    -device cptra-mbox-peer-extern,id=peer0,chardev=cptra0 \
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


## Test Step in AST1040 QEMU
``` bash

  # 1. Switch to CSR view — the window register at 0x74c02120 controls whether
  #    0x74200000 maps to the mailbox CSRs or the SRAM data buffer.
  mw 74c02120 0x21600000

  # 2. Read the LOCK register. A return value of 0 means the lock was acquired
  #    successfully; the mailbox is now owned by this SoC agent.
  md 74200000 1

  # 3. Switch to SRAM view so we can write the request payload into the
  #    mailbox data buffer.
  mw 74c02120 0x21400000

  # 4. Write SRAM[0] = request checksum.
  #    FirmwareVersionReq payload = { chksum (i32), index (u32) }.
  #    Checksum covers the command ID bytes and all fields after chksum:
  #      sum("MFWV" bytes) = 0x56+0x57+0x46+0x4D = 0x140
  #      chksum = -0x140 = 0xFFFFFEC0
  #    SRAM[1] (index field) is left as 0, requesting CaliptraCore firmware version.
  mw 74200000 0xfffffec0

  # 5. Switch back to CSR view to write the command registers.
  mw 74c02120 0x21600000

  # 6. Write the command ID: 0x4D465756 = ASCII "MFWV" = MC_FIRMWARE_VERSION.
  mw 74200010 0x4d465756

  # 7. Write DLEN = 8: the request payload is 8 bytes
  #    (4-byte chksum + 4-byte index).
  mw 74200014 0x00000008

  # 8. Write EXECUTE = 1 to trigger the command. The frontend sets CMD_STATUS
  #    to BUSY and forwards the request to the Caliptra MCU peer.
  mw 74200018 0x00000001

  # 9. Read CMD_STATUS to check for completion.
  #    0 = BUSY, 1 = DATA_READY, 2 = COMPLETE, 3 = CMD_FAILURE.
  #    Poll until the value is no longer BUSY.
  md 74200020 1

  # 10. Read DLEN to find out how many bytes the MCU returned in the response.
  md 74200014 1

  # 11. Switch to SRAM view to read the response payload.
  mw 74c02120 0x21400000

  # 12. Dump 48 bytes of response data starting from SRAM base.
  #     FirmwareVersionResp layout: MailboxRespHeaderVarSize (8 bytes) +
  #     version string (up to 32 bytes), so 48 bytes covers the full response.
  devmem dump -a 74200000 -s 48
```
