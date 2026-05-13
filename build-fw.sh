#!/usr/bin/env bash
# Build all caliptra-mcu-sw firmware images and caliptra-server.
#
# Usage:
#   cd /home/steven_lee/work/caliptra/caliptra_2_x/caliptra-server
#   ./build-fw.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FW_DIR="$SCRIPT_DIR/fw"

# Locate caliptra-mcu-sw: check sibling directory first, then parent's sibling.
if [ -f "$SCRIPT_DIR/../caliptra-mcu-sw/Cargo.toml" ]; then
    MCU_SW_DIR="$(cd "$SCRIPT_DIR/../caliptra-mcu-sw" && pwd)"
elif [ -f "$SCRIPT_DIR/../../caliptra-mcu-sw/Cargo.toml" ]; then
    MCU_SW_DIR="$(cd "$SCRIPT_DIR/../../caliptra-mcu-sw" && pwd)"
else
    echo "ERROR: cannot find caliptra-mcu-sw relative to $SCRIPT_DIR" >&2
    exit 1
fi
ZIP="$MCU_SW_DIR/target/all-fw.zip"
BUILD_LOG="$MCU_SW_DIR/caliptra-server-build.log"

echo "=== [1/3] Building caliptra-mcu-sw firmware with --runtime-features debug,test-mcu-mbox-cmds (takes a few minutes) ==="
# Runtime features required:
#   debug              — Tock OS interactive boot
#   test-mcu-mbox-cmds — enables the userspace MCU mailbox service in
#                        platforms/emulator/runtime/userspace/apps/user/src/mcu_mbox/mod.rs,
#                        which registers NonCryptoCmdHandlerMock to handle
#                        MFWV / MDID / MDIN / MCAP. Without this, the runtime's
#                        mcu_mbox_task() returns immediately with no handler,
#                        so dispatch_to_emulator() spins forever on CMD_STATUS=BUSY.
# pipefail is temporarily disabled so tee doesn't hide cargo's exit code.
set +o pipefail
(cd "$MCU_SW_DIR" && cargo xtask all-build --platform emulator --runtime-features debug,test-mcu-mbox-cmds) 2>&1 | tee "$BUILD_LOG"
BUILD_EXIT=${PIPESTATUS[0]}
set -o pipefail
if [ "$BUILD_EXIT" -ne 0 ]; then
    echo "ERROR: cargo xtask all-build failed (exit $BUILD_EXIT)" >&2
    exit "$BUILD_EXIT"
fi

# Extract vendor PK hash from build log.
# all_build prints: Vendor PK hash: "aabb..."
VENDOR_PK_HASH="$(grep -oP '(?<=Vendor PK hash: ")[^"]+' "$BUILD_LOG" | tail -1 || true)"
mkdir -p "$FW_DIR"
if [ -z "$VENDOR_PK_HASH" ]; then
    echo "WARNING: Could not extract vendor_pk_hash from build log." >&2
else
    echo "Extracted Vendor PK hash: $VENDOR_PK_HASH"
    echo "$VENDOR_PK_HASH" > "$FW_DIR/vendor_pk_hash.txt"
fi

echo "=== [2/3] Extracting firmware binaries ==="
# Locate all-fw.zip: cargo places it in the workspace's actual target dir,
# which may differ from MCU_SW_DIR/target if there's a workspace root above.
# Search for the most-recently-modified all-fw.zip under the caliptra tree.
ZIP="$(find /home/steven_lee/work/caliptra -maxdepth 6 -name "all-fw.zip" \
    2>/dev/null | xargs ls -t 2>/dev/null | head -1)"
if [ -z "$ZIP" ]; then
    echo "ERROR: Could not find all-fw.zip anywhere under /home/steven_lee/work/caliptra" >&2
    exit 1
fi
echo "Using ZIP: $ZIP"
mkdir -p "$FW_DIR"
unzip -o "$ZIP" \
    caliptra_rom.bin \
    caliptra_fw.bin \
    mcu_rom.bin \
    mcu_runtime.bin \
    soc_manifest.bin \
    -d "$FW_DIR"

# Read back hash (in case it was set above)
if [ -f "$FW_DIR/vendor_pk_hash.txt" ]; then
    VENDOR_PK_HASH="$(cat "$FW_DIR/vendor_pk_hash.txt")"
fi

echo "=== [3/3] Building caliptra-server ==="
(cd "$SCRIPT_DIR" && cargo build)

echo ""
echo "======================================================="
echo " Build complete!"
echo "======================================================="
echo ""
echo "Terminal 1 — caliptra-server:"
echo ""
echo "cd $SCRIPT_DIR && ./target/debug/caliptra-server \\"
echo "  --mbox-socket /tmp/mcu_mbox.sock \\"
echo "  --rom            fw/mcu_rom.bin \\"
echo "  --firmware       fw/mcu_runtime.bin \\"
echo "  --caliptra-rom   fw/caliptra_rom.bin \\"
echo "  --caliptra-firmware fw/caliptra_fw.bin \\"
echo "  --soc-manifest   fw/soc_manifest.bin \\"
echo "  --vendor-pk-hash $VENDOR_PK_HASH \\"
echo "  --hw-revision    2.0.0 \\"
echo "  --rom-offset  0x80000000 --rom-size  0x10000  \\"
echo "  --dccm-offset 0x50000000 --dccm-size 0x4000   \\"
echo "  --sram-offset 0x40000000 --sram-size 0x80000  \\"
echo "  --pic-offset  0x60000000 \\"
echo "  --i3c-offset  0x20004000 --i3c-size  0x1000   \\"
echo "  --mci-offset  0x21000000 --mci-size  0xe00000 \\"
echo "  --mbox-offset 0x30020000 --mbox-size 0x28     \\"
echo "  --soc-offset  0x30030000 --soc-size  0x5e0    \\"
echo "  --otp-offset  0x70000000 --otp-size  0x140    \\"
echo "  --lc-offset   0x70000400 --lc-size   0x8c     \\"
echo "  --device-security-state 3 \\"
echo "  --test-feature test-mcu-mbox-driver \\"
echo "  --no-stdin-uart"
echo ""
echo "Terminal 1 (quick test, mailbox-responder ROM, no Caliptra images needed):"
echo ""
echo "cd $SCRIPT_DIR && ./target/debug/caliptra-server \\"
echo "  --mbox-socket /tmp/mcu_mbox.sock \\"
echo "  --rom fw/mcu-test-rom-caliptra-mcu-test-fw-mailbox-responder-caliptra-mcu-test-fw-mailbox-responder.bin \\"
echo "  --firmware /dev/null --caliptra-rom /dev/null \\"
echo "  --caliptra-firmware /dev/null --soc-manifest /dev/null \\"
echo "  --vendor-pk-hash $VENDOR_PK_HASH \\"
echo "  --rom-offset  0x80000000 --rom-size  0x10000  \\"
echo "  --dccm-offset 0x50000000 --dccm-size 0x4000   \\"
echo "  --sram-offset 0x40000000 --sram-size 0x80000  \\"
echo "  --pic-offset  0x60000000 \\"
echo "  --i3c-offset  0x20004000 --i3c-size  0x1000   \\"
echo "  --mci-offset  0x21000000 --mci-size  0xe00000 \\"
echo "  --mbox-offset 0x30020000 --mbox-size 0x28     \\"
echo "  --soc-offset  0x30030000 --soc-size  0x5e0    \\"
echo "  --otp-offset  0x70000000 --otp-size  0x140    \\"
echo "  --lc-offset   0x70000400 --lc-size   0x8c     \\"
echo "  --device-security-state 3 \\"
echo "  --test-feature test-mcu-mbox-driver \\"
echo "  --no-stdin-uart"
echo ""
echo "Terminal 2 — QEMU:"
echo ""
echo "/home/steven_lee/work/caliptra/caliptra_2_x/build/qemu-system-arm \\"
echo "  -machine ast1030-evb,mcu-mbox-socket=/tmp/mcu_mbox.sock \\"
echo "  -kernel /home/steven_lee/work/caliptra/caliptra_2_x/zephyr.elf \\"
echo "  -nographic"
