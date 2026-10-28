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
use caliptra_emu_periph::{MailboxRequester, SocToCaliptraBus};
use caliptra_emu_types::RvSize;
use caliptra_mcu_emulator::{Emulator, EmulatorArgs};
use caliptra_mcu_emulator_ast1040::{Ast1040Args, Ast1040Emulator};
use clap::Parser;
use mbox_server::{MboxCommand, MboxResponse};
use std::cell::RefCell;
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

    #[command(flatten)]
    ast1040: Ast1040Args,

    // All caliptra-mcu-emulator args are forwarded via flatten.
    #[command(flatten)]
    emu: EmulatorArgs,
}

// Caliptra firmware reports fatal errors through CPTRA_FW_ERROR_{FATAL,NON_FATAL}
// and may then end the emulator immediately (TB services exit), so watch the
// registers every step and log any change as it happens.
struct CaliptraErrorWatch {
    soc: SocToCaliptraBus,
    last: [u32; 2],
}

impl CaliptraErrorWatch {
    const FW_ERROR_REGS: [(&'static str, u32); 2] =
        [("FW_ERROR_FATAL", 0x3003_0008), ("FW_ERROR_NON_FATAL", 0x3003_000c)];

    fn new(emu: &Emulator) -> Self {
        Self {
            soc: emu.caliptra_cpu.bus.soc_to_caliptra_bus(MailboxRequester::SocUser(1)),
            last: [0; 2],
        }
    }

    fn check(&mut self) {
        for (i, (name, addr)) in Self::FW_ERROR_REGS.iter().enumerate() {
            let val = self.soc.read(RvSize::Word, *addr).unwrap_or(0);
            if val != self.last[i] {
                eprintln!("caliptra-server: Caliptra {name} = 0x{val:08x}");
                self.last[i] = val;
            }
        }
    }
}

// The emulator can end the process from inside a CPU step (e.g. Caliptra's
// TB services exit), so keep the last Caliptra PCs and dump them at exit.
const PC_HISTORY_LEN: usize = 256;

thread_local! {
    static CALIPTRA_PCS: RefCell<(usize, [u32; PC_HISTORY_LEN])> =
        const { RefCell::new((0, [0; PC_HISTORY_LEN])) };
}

extern "C" {
    fn atexit(cb: extern "C" fn()) -> i32;
}

fn record_caliptra_pc(pc: u32) {
    CALIPTRA_PCS.with(|h| {
        let mut h = h.borrow_mut();
        let (next, ring) = &mut *h;
        if ring[(*next + PC_HISTORY_LEN - 1) % PC_HISTORY_LEN] != pc {
            ring[*next] = pc;
            *next = (*next + 1) % PC_HISTORY_LEN;
        }
    });
}

extern "C" fn dump_caliptra_pcs() {
    let _ = CALIPTRA_PCS.try_with(|h| {
        let h = h.borrow();
        let (next, ring) = &*h;
        let pcs: Vec<String> = (0..PC_HISTORY_LEN)
            .map(|i| ring[(next + i) % PC_HISTORY_LEN])
            .filter(|pc| *pc != 0)
            .map(|pc| format!("{pc:08x}"))
            .collect();
        eprintln!("caliptra-server: last Caliptra PCs (oldest first):\n{}", pcs.join(" "));
    });
}

const CALIPTRA_FMC_ENTRY: u32 = 0x4000_0130;

// Where did Caliptra ROM leave the firmware handoff table ("CFHT")?
fn report_fht_location(emu: &mut Emulator) {
    const DCCM: std::ops::Range<u32> = 0x5000_0000..0x5004_0000;
    const FHT_MARKER: u32 = 0x5448_4643;
    let found: Vec<String> = DCCM
        .step_by(4)
        .filter(|a| emu.caliptra_cpu.bus.read(RvSize::Word, *a) == Ok(FHT_MARKER))
        .map(|a| format!("0x{a:08x}"))
        .collect();
    eprintln!("caliptra-server: Caliptra at FMC entry; FHT marker found at [{}]", found.join(", "));
}

/// The emulator, optionally with AST1040 platform support.
enum Machine {
    Emulator(Emulator),
    Ast1040(Box<Ast1040Emulator>),
}

impl Machine {
    fn emu(&mut self) -> &mut Emulator {
        match self {
            Self::Emulator(emu) => emu,
            Self::Ast1040(ast) => &mut ast.emu,
        }
    }

    fn step(&mut self) -> StepAction {
        match self {
            Self::Emulator(emu) => emu.step(),
            Self::Ast1040(ast) => ast.step(),
        }
    }
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
fn dispatch_to_emulator(machine: &mut Machine, cmd: u32, payload: Vec<u8>) -> MboxResponse {
    let emu = machine.emu();
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
            bus_write(machine.emu(), MBOX_EXECUTE, 0);
            return MboxResponse { status: 3, data: vec![] };
        }
        steps += 1;
        match machine.step() {
            StepAction::Continue => {}
            other => {
                eprintln!("caliptra-server: emulator halted ({:?}) while waiting for mbox response", other);
                bus_write(machine.emu(), MBOX_EXECUTE, 0);
                return MboxResponse { status: 3, data: vec![] };
            }
        }
        let emu = machine.emu();
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
fn run(mut machine: Machine, mbox_rx: Receiver<MboxCommand>) {
    let mut caliptra_errors = CaliptraErrorWatch::new(machine.emu());
    let trace_caliptra_pc = std::env::var_os("CALIPTRA_SERVER_PC_HISTORY").is_some();
    let mut fht_reported = false;
    // Periodically log where both CPUs are, to locate stalls.
    let heartbeat: u64 = std::env::var("CALIPTRA_SERVER_HEARTBEAT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut steps: u64 = 0;
    if trace_caliptra_pc {
        // SAFETY: registers a plain extern "C" fn with libc.
        unsafe {
            atexit(dump_caliptra_pcs);
        }
    }

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
                let rsp = dispatch_to_emulator(&mut machine, req.cmd, req.payload);
                let _ = req.resp_tx.send(rsp);
                continue;
            }
        }

        // Advance the emulator one CPU step.
        match machine.step() {
            StepAction::Continue => {}
            StepAction::Break => {
                println!("caliptra-server: emulator breakpoint");
            }
            other => {
                eprintln!("caliptra-server: emulator exited ({:?})", other);
                std::process::exit(1);
            }
        }

        caliptra_errors.check();
        let emu = machine.emu();
        steps += 1;
        if heartbeat != 0 && steps % heartbeat == 0 {
            eprintln!(
                "caliptra-server: step {steps}: MCU pc=0x{:08x} Caliptra pc=0x{:08x}",
                emu.mcu_cpu.read_pc(),
                emu.caliptra_cpu.read_pc()
            );
        }
        if trace_caliptra_pc {
            let pc = emu.caliptra_cpu.read_pc();
            record_caliptra_pc(pc);
            if pc == CALIPTRA_FMC_ENTRY && !fht_reported {
                fht_reported = true;
                report_fht_location(emu);
            }
        }

        if !mbox_ready
            && bus_read(emu, FW_FLOW_STATUS) & FIRMWARE_MAILBOX_READY_MASK != 0
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
    let machine = if args.ast1040.flash.is_some() {
        Ast1040Emulator::new(args.emu, &args.ast1040).map(|ast| Machine::Ast1040(Box::new(ast)))
    } else {
        Emulator::from_args(args.emu, false).map(Machine::Emulator)
    }
    .unwrap_or_else(|e| {
        eprintln!("caliptra-server: emulator init failed: {}", e);
        std::process::exit(1);
    });
    println!("caliptra-server: emulator ready, entering step loop");

    run(machine, mbox_rx);
}
