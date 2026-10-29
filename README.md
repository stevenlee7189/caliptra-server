# caliptra-server

`caliptra-server` is an external Caliptra mailbox peer used for AST1040 QEMU
co-simulation. It listens on a Unix socket for mailbox requests from QEMU's
`cptra-mbox-peer-extern` device, dispatches each request into the
`caliptra-mcu-sw` emulator, and returns the mailbox response to QEMU.

This is the current bring-up backend for the QEMU AST1040 Caliptra mailbox
support. The QEMU qtest does not depend on this repository; it uses an internal
socket-backed peer to validate the QEMU-side protocol and mailbox flow.

## Design

The implementation is split into two layers:

- `src/mbox_server.rs` owns the Unix socket protocol and connection handling.
- `src/main.rs` owns the emulator, mailbox register access, and CPU stepping.

The socket layer does not mutate emulator state directly. Each socket request is
converted into an `MboxCommand` and sent to the main emulator loop through an
`mpsc` channel. The main loop is the only owner of the emulator, which avoids
concurrent mutation of CPU, bus, and peripheral state.

Request flow:

```text
QEMU cptra-mbox-peer-extern
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

- `MBOX_EXECUTE = 1`: QEMU to server. Payload is `u32 cmd`, `u32 dlen`, then
  `ROUND_UP(dlen, 4)` bytes copied from mailbox SRAM.
- `MBOX_RESPONSE = 2`: server to QEMU. Payload is `u32 status`, `u32 dlen`,
  then `ROUND_UP(dlen, 4)` response bytes to copy back into mailbox SRAM.

The `status` value is the Caliptra mailbox command status returned to QEMU.

## Runtime Behavior

The server starts the socket listener first, then initializes the emulator and
enters a step loop. Mailbox requests are accepted only after firmware sets the
`FIRMWARE_MAILBOX_READY` milestone in `FW_FLOW_STATUS`.

Dispatching a command is synchronous from the QEMU mailbox point of view. While
one command is being processed, the main loop steps the emulator until the
mailbox status changes from `BUSY` or until the dispatch timeout is hit.

The server always runs the emulator with `--test-feature test-mcu-mbox-driver`,
which lets `EXECUTE=1` raise the MCU mailbox interrupt, plus
`--device-security-state 3` and `--no-stdin-uart`.

## Usage

```text
caliptra-server [-m emulator|ast1040] [--mbox-socket PATH] [machine options] [-- EMULATOR ARGS]
```

| Option | Machine | Default |
| --- | --- | --- |
| `-m`, `--machine` | | `emulator` |
| `--mbox-socket` | both | `/tmp/mcu_mbox.sock` |
| `--fw-dir` | `emulator` | `fw` |
| `--flash`, `--xip-rom`, `--caliptra-rom` | `ast1040` | required |
| `--trace` | `ast1040` | off |
| `--heartbeat N`, `--pc-history` | both | off |

Arguments after `--` go to `caliptra-mcu-emulator` and override the ones the
server derives, e.g. `-- --hw-revision 2.1.0` or `-- --vendor-pk-hash <hash>`.

## Emulator Platform

Build firmware images and the server:

```bash
./build-fw.sh
```

`build-fw.sh` writes `fw/mcu_rom.bin`, `fw/mcu_runtime.bin`,
`fw/caliptra_rom.bin`, `fw/caliptra_fw.bin`, `fw/soc_manifest.bin` and
`fw/vendor_pk_hash.txt`. Run with:

```bash
./target/debug/caliptra-server --fw-dir fw
```

The emulator memory map (ROM at `0x8000_0000`, MCI at `0x2100_0000`, ...) and
HW revision 2.0.0 are built in. The images in `fw/` must be built from the same
caliptra-mcu-sw revision the server links against.

## AST1040 Flash Boot

`-m ast1040` runs real AST1040 firmware. It uses
`caliptra-mcu-emulator-ast1040` (`caliptra-mcu-sw/platforms/ast1040/emulator`),
which wraps the unmodified emulator with the AST1040 memory map, IO-die
peripherals, SS 2.1 flash boot and the A0 I3C / OTP differences. The SS-MCU XIP
ROM boots from the flash image and loads Caliptra FW, SoC manifest and MCU
runtime from flash. The vendor PK hash and PQC key type are read from the
Caliptra FW manifest in flash.

| Option | Source |
| --- | --- |
| `--flash` | aspeed-mcu-fw-hub `bazel build //ast1040/ast1040a0/evb:image` → `bazel-bin/ast1040/ast1040a0/evb/ast1040.bin` |
| `--xip-rom` | caliptra-mcu-sw `aspeed-dev-2.1`: `cargo xtask-xip rom-build --platform ast1040 --features aspeed-xip` |
| `--caliptra-rom` (A0) | caliptra-sw tag `ast1040a0-rom-rc1`: `cargo run --release -p caliptra-builder --bin image -- --rom-with-log caliptra_rom_a0rc1.bin` |

```bash
./target/debug/caliptra-server -m ast1040 \
  --flash        aspeed-mcu-fw-hub/bazel-bin/ast1040/ast1040a0/evb/ast1040.bin \
  --xip-rom      caliptra-mcu-rom-ast1040-aspeed-xip,aspeed-xip-<commit>.bin \
  --caliptra-rom caliptra_rom_a0rc1.bin
```

Start QEMU once the server prints `FIRMWARE_MAILBOX_READY observed` (about one
minute). The MFWV smoke test below then returns the Caliptra core firmware
version from the production MCU runtime (for example `2.1.0`, DLEN `0x11`)
instead of the emulator mock string.

Debug aids: `--trace` logs the first access to each modeled IO-die register;
`--heartbeat <steps>` prints MCU / Caliptra PCs periodically; `--pc-history`
dumps the last Caliptra PCs at exit. Caliptra `FW_ERROR_FATAL` /
`FW_ERROR_NON_FATAL` changes are always logged.

## QEMU AST1040 Co-Simulation

Start `caliptra-server` first, then start QEMU with a matching socket path.

### Terminal 1: Start caliptra-server

Use either machine from the sections above, e.g.:

```bash
./target/debug/caliptra-server -m ast1040 --flash ... --xip-rom ... --caliptra-rom ...
```

The socket path (`--mbox-socket`, default `/tmp/mcu_mbox.sock`) must match
QEMU's chardev path.

### Terminal 2: Start QEMU

```bash
qemu-system-arm \
  -machine ast1040-evb,cptra-peer=peer0 \
  -chardev socket,id=cptra0,path=/tmp/mcu_mbox.sock \
  -device cptra-mbox-peer-extern,id=peer0,chardev=cptra0 \
  -kernel zephyr.elf \
  -nographic
```

## Guest-Side Mailbox Smoke Test

The following command sequence exercises the AST1040 MCI aperture from the guest
side. It sends `MC_FIRMWARE_VERSION` (`"MFWV"`, `0x4d465756`) with an 8-byte
request payload and reads the returned firmware version response.

Address map used by this flow:

```text
SCU_CPTRA_PAGE_REG0 = 0x74c02120
MCI aperture        = 0x74200000
Mailbox SRAM page   = 0x21400000
Mailbox CSR page    = 0x21600000
```

Run these commands in the guest monitor or firmware shell that provides `mw`,
`md`, and `devmem`:

```text
# 1. Select the mailbox CSR page through the MCI aperture.
mw 74c02120 0x21600000

# 2. Read LOCK. A return value of 0 means the lock was acquired.
md 74200000 1

# 3. Select the mailbox SRAM page.
mw 74c02120 0x21400000

# 4. Write FirmwareVersionReq.checksum.
#    calc_checksum("MFWV", index = 0) = -(0x56 + 0x57 + 0x46 + 0x4d)
#                                      = 0xfffffec0
#    SRAM[1] is left at 0, selecting CaliptraCore.
mw 74200000 0xfffffec0

# 5. Select the mailbox CSR page again.
mw 74c02120 0x21600000

# 6. CMD = "MFWV" = MC_FIRMWARE_VERSION.
mw 74200010 0x4d465756

# 7. DLEN = 8 bytes: 4-byte checksum + 4-byte index.
mw 74200014 0x00000008

# 8. EXECUTE = 1.
mw 74200018 0x00000001

# 9. Poll CMD_STATUS until it is not BUSY.
#    0 = BUSY, 1 = DATA_READY, 2 = COMPLETE, 3 = CMD_FAILURE.
md 74200020 1

# 10. Read response DLEN.
md 74200014 1

# 11. Select the mailbox SRAM page.
mw 74c02120 0x21400000

# 12. Dump FirmwareVersionResp.
devmem dump -a 74200000 -s 48
```

Expected result:

- `LOCK` returns `0` when the guest acquires the mailbox.
- `CMD_STATUS` eventually returns `2` (`COMPLETE`).
- The SRAM dump contains the variable-size mailbox response header followed by
  the Caliptra firmware version string.

## Manual Test Client

`mbox_send.py` can send mailbox commands directly to the server without QEMU:

```bash
python3 mbox_send.py --mbox-socket /tmp/mcu_mbox.sock fw-version
python3 mbox_send.py --mbox-socket /tmp/mcu_mbox.sock device-caps
python3 mbox_send.py --mbox-socket /tmp/mcu_mbox.sock random-generate --length 32
```
