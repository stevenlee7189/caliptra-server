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

Expected firmware inputs for the QEMU co-simulation flow:

```text
fw/mcu_rom.bin
fw/mcu_runtime.bin
fw/caliptra_rom.bin
fw/caliptra_fw.bin
fw/soc_manifest.bin
fw/vendor_pk_hash.txt
```

## QEMU AST1040 Co-Simulation

Start `caliptra-server` first, then start QEMU with a matching socket path.

### Terminal 1: Start caliptra-server

```bash
./target/debug/caliptra-server \
  --mbox-socket /tmp/mcu_mbox.sock \
  --rom fw/mcu_rom.bin \
  --firmware fw/mcu_runtime.bin \
  --caliptra-rom fw/caliptra_rom.bin \
  --caliptra-firmware fw/caliptra_fw.bin \
  --soc-manifest fw/soc_manifest.bin \
  --vendor-pk-hash \
    b17ca877666657ccd100e6926c7206b60c995cb68992c6c9baefce728af05441dee1ff415adfc187e1e4edb4d3b2d909 \
  --hw-revision 2.0.0 \
  --rom-offset 0x80000000 --rom-size 0x10000 \
  --dccm-offset 0x50000000 --dccm-size 0x4000 \
  --sram-offset 0x40000000 --sram-size 0x80000 \
  --pic-offset 0x60000000 \
  --i3c-offset 0x20004000 --i3c-size 0x1000 \
  --mci-offset 0x21000000 --mci-size 0xe00000 \
  --mbox-offset 0x30020000 --mbox-size 0x28 \
  --soc-offset 0x30030000 --soc-size 0x5e0 \
  --otp-offset 0x70000000 --otp-size 0x140 \
  --lc-offset 0x70000400 --lc-size 0x8c \
  --device-security-state 3 \
  --test-feature test-mcu-mbox-driver \
  --no-stdin-uart
```

The socket path passed to `--mbox-socket` must match QEMU's chardev path.

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
