// caliptra-server
//
// Bridges QEMU MCU mailbox traffic (via Unix socket) to the caliptra-mcu-sw emulator.
//
// Socket mode:
//
//   Command-level MBOX (--mbox-socket):
//     aspeed-mcu-mbox device sends complete commands via MBOX protocol.
//     Use with -machine ...,mcu-mbox-socket=<path>
//
// Emulator args (--rom, --firmware, --caliptra-rom, --caliptra-firmware,
// --soc-manifest, …) are forwarded directly to caliptra-mcu-emulator.
//
// MCU bus mailbox addresses (MCI base = 0x2100_0000):
//   SRAM     0x2140_0000   (MCI + 0x40_0000)
//   LOCK     0x2160_0000   (MCI + 0x60_0000)
//   CMD      0x2160_0010
//   DLEN     0x2160_0014
//   EXECUTE  0x2160_0018
//   STATUS   0x2160_0020

mod mbox_server;

use caliptra_emu_bus::Bus;
use caliptra_emu_cpu::StepAction;
use caliptra_emu_types::RvSize;
use caliptra_mcu_emulator::{Emulator, EmulatorArgs};
use clap::Parser;
use mbox_server::{MboxCommand, MboxResponse};
use std::sync::mpsc::Receiver;

// MCU bus-view addresses for MCU Mailbox 0 (within the MCI peripheral).
const MCI_BASE: u32 = 0x2100_0000;
const MBOX_SRAM_BASE: u32 = MCI_BASE + 0x0040_0000;
const MBOX_LOCK: u32 = MCI_BASE + 0x0060_0000;
const MBOX_CMD: u32 = MCI_BASE + 0x0060_0010;
const MBOX_DLEN: u32 = MCI_BASE + 0x0060_0014;
const MBOX_EXECUTE: u32 = MCI_BASE + 0x0060_0018;
const MBOX_STATUS: u32 = MCI_BASE + 0x0060_0020;

// CMD_STATUS values (MboxCmdStatus::Status)
const STATUS_BUSY: u32 = 0;
const STATUS_DATA_READY: u32 = 1;
const STATUS_COMPLETE: u32 = 2;

// FW_FLOW_STATUS register (MCI base + 0x30). Userspace mbox responder calls
// mci.set_mailbox_ready() right before it starts listening; that ORs the
// FIRMWARE_MAILBOX_READY milestone (bit 9 of the 16-bit milestone field,
// stored in the upper 16 bits of the register, i.e. bit 25) into this reg.
// Polling this is far more accurate than waiting a fixed number of cycles
// after MCU_RUNTIME_STARTED.
const FW_FLOW_STATUS: u32 = MCI_BASE + 0x30;
const FIRMWARE_MAILBOX_READY_MASK: u32 = 1 << (16 + 9);

// Hard cap on emulator steps spent waiting for a single mbox response.
// Prevents an infinite hang if the MCU never sets CMD_STATUS.
const MBOX_DISPATCH_TIMEOUT_STEPS: u64 = 50_000_000;

#[derive(Parser)]
#[command(name = "caliptra-server", about = "Caliptra mailbox socket bridge + emulator")]
struct Args {
    /// Unix socket path for command-level MBOX protocol (mcu-mbox-socket QEMU property).
    #[arg(long, default_value = "")]
    mbox_socket: String,

    // All caliptra-mcu-emulator args are forwarded via flatten.
    #[command(flatten)]
    emu: EmulatorArgs,
}

fn bus_read(emu: &mut Emulator, addr: u32) -> u32 {
    emu.mcu_cpu.bus.read(RvSize::Word, addr).unwrap_or(0)
}

fn bus_write(emu: &mut Emulator, addr: u32, val: u32) {
    let _ = emu.mcu_cpu.bus.write(RvSize::Word, addr, val);
}

/// Send a command into the emulator mailbox and step until the MCU responds.
///
/// NOTE: This works only when the emulator is started with
///   --test-feature test-mcu-mbox-driver
/// which sets test_mcu_mbox_driver=true on the mailbox, bypassing the
/// SocAgent requester check so EXECUTE=1 fires the IRQ to the MCU.
fn dispatch_to_emulator(emu: &mut Emulator, cmd: u32, payload: Vec<u8>) -> MboxResponse {
    // Acquire lock by reading LOCK (returns 0 = acquired, 1 = busy).
    let lock_val = bus_read(emu, MBOX_LOCK);
    if lock_val != 0 {
        eprintln!("caliptra-server: mailbox locked (0x{:08x}), dropping command", lock_val);
        return MboxResponse { status: 3, data: vec![] }; // CmdFailure
    }

    // Write SRAM payload (word-aligned).
    for (i, chunk) in payload.chunks(4).enumerate() {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        bus_write(emu, MBOX_SRAM_BASE + (i as u32) * 4, u32::from_le_bytes(word));
    }
    bus_write(emu, MBOX_DLEN, payload.len() as u32);
    bus_write(emu, MBOX_CMD, cmd);
    // EXECUTE=1 fires MBOX0_CMD_AVAIL IRQ to MCU (only when test_mcu_mbox_driver=true).
    bus_write(emu, MBOX_EXECUTE, 1);

    // Step the emulator until CMD_STATUS is no longer Busy (or we time out).
    let mut steps = 0u64;
    loop {
        if steps >= MBOX_DISPATCH_TIMEOUT_STEPS {
            eprintln!(
                "caliptra-server: mbox response timeout after {} steps (cmd=0x{:08x})",
                steps, cmd
            );
            bus_write(emu, MBOX_EXECUTE, 0);
            return MboxResponse { status: 3, data: vec![] };
        }
        steps += 1;
        match emu.step() {
            StepAction::Continue => {}
            other => {
                eprintln!("caliptra-server: emulator halted ({:?}) while waiting for mbox response", other);
                bus_write(emu, MBOX_EXECUTE, 0);
                return MboxResponse { status: 3, data: vec![] };
            }
        }
        let status = bus_read(emu, MBOX_STATUS);
        if status != STATUS_BUSY {
            let dlen = bus_read(emu, MBOX_DLEN) as usize;
            let data = if status == STATUS_DATA_READY || status == STATUS_COMPLETE {
                (0..dlen.div_ceil(4))
                    .flat_map(|i| bus_read(emu, MBOX_SRAM_BASE + (i as u32) * 4).to_le_bytes())
                    .take(dlen)
                    .collect()
            } else {
                vec![]
            };
            bus_write(emu, MBOX_EXECUTE, 0);
            return MboxResponse { status, data };
        }
    }
}
fn run(mut emu: Emulator, mbox_rx: Receiver<MboxCommand>) {
    // Defer MBOX dispatching until the userspace responder signals
    // FIRMWARE_MAILBOX_READY via FW_FLOW_STATUS. Polling every step is fine;
    // a single bus read is cheap.
    let mut mbox_ready = false;

    loop {
        // Handle one pending MBOX command (blocks emulator during processing).
        if mbox_ready {
            if let Ok(req) = mbox_rx.try_recv() {
                println!(
                    "caliptra-server: mbox cmd=0x{:08x} payload_len={}",
                    req.cmd,
                    req.payload.len()
                );
                let rsp = dispatch_to_emulator(&mut emu, req.cmd, req.payload);
                let _ = req.resp_tx.send(rsp);
                continue;
            }
        }

        // Advance the emulator one CPU step.
        match emu.step() {
            StepAction::Continue => {}
            StepAction::Break => {
                println!("caliptra-server: emulator breakpoint");
            }
            other => {
                eprintln!("caliptra-server: emulator exited ({:?})", other);
                std::process::exit(1);
            }
        }

        if !mbox_ready
            && bus_read(&mut emu, FW_FLOW_STATUS) & FIRMWARE_MAILBOX_READY_MASK != 0
        {
            println!("caliptra-server: FIRMWARE_MAILBOX_READY observed, accepting commands");
            mbox_ready = true;
        }
    }
}

fn main() {
    let args = Args::parse();

    if args.mbox_socket.is_empty() {
        eprintln!("caliptra-server: specify --mbox-socket");
        std::process::exit(1);
    }

    let mbox_rx = mbox_server::start(&args.mbox_socket);

    println!("caliptra-server: initializing emulator…");
    let emu = Emulator::from_args(args.emu, false)
        .unwrap_or_else(|e| {
            eprintln!("caliptra-server: emulator init failed: {}", e);
            std::process::exit(1);
        });
    println!("caliptra-server: emulator ready, entering step loop");

    run(emu, mbox_rx);
}
