// caliptra-server
//
// Bridges QEMU MCU mailbox traffic (via Unix socket) to the caliptra-mcu-sw emulator.
//
// Machines (-m):
//   ast1040   AST1040 flash boot (caliptra-mcu-emulator-ast1040), default
//   emulator  caliptra-mcu-sw emulator platform, firmware from --fw-dir
//
// Arguments after `--` are passed to caliptra-mcu-emulator and override the
// values derived here, e.g. `-- --hw-revision 2.1.0`.
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
use caliptra_mcu_emulator_ast1040::{Ast1040Config, Ast1040Emulator};
use clap::{CommandFactory, FromArgMatches, Parser, ValueEnum};
use mbox_server::{MboxCommand, MboxResponse};
use std::cell::RefCell;
use std::ffi::OsString;
use std::path::PathBuf;
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

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum MachineKind {
    /// AST1040 flash boot
    Ast1040,
    /// caliptra-mcu-sw emulator platform
    Emulator,
}

#[derive(Parser)]
#[command(
    name = "caliptra-server",
    about = "Caliptra mailbox socket bridge + emulator"
)]
struct Args {
    /// Machine to emulate.
    #[arg(short, long, value_enum, default_value = "ast1040")]
    machine: MachineKind,

    /// Unix socket path QEMU's cptra-mbox-peer-extern chardev connects to.
    #[arg(long, default_value = "/tmp/mcu_mbox.sock")]
    mbox_socket: String,

    /// Directory with mcu_rom.bin, mcu_runtime.bin, caliptra_rom.bin,
    /// caliptra_fw.bin, soc_manifest.bin and vendor_pk_hash.txt (build-fw.sh).
    #[arg(long, default_value = "fw", help_heading = "Emulator")]
    fw_dir: PathBuf,

    /// Flash image (fw-hub ast1040.bin).
    #[arg(long, help_heading = "AST1040")]
    flash: Option<PathBuf>,

    /// SS-MCU XIP ROM, placed at the start of the flash image.
    #[arg(long, help_heading = "AST1040")]
    xip_rom: Option<PathBuf>,

    /// Caliptra core ROM (A0: caliptra-sw tag ast1040a0-rom-rc1).
    #[arg(long, help_heading = "AST1040")]
    caliptra_rom: Option<PathBuf>,

    /// Log the first access to each modeled IO-die register.
    #[arg(long, help_heading = "AST1040")]
    trace: bool,

    /// Print MCU / Caliptra PCs every N steps.
    #[arg(long, value_name = "N", help_heading = "Debug")]
    heartbeat: Option<u64>,

    /// Dump the last Caliptra PCs at exit.
    #[arg(long, help_heading = "Debug")]
    pc_history: bool,

    /// caliptra-mcu-emulator arguments; override the derived ones.
    #[arg(last = true, value_name = "EMULATOR ARGS")]
    emu_args: Vec<OsString>,
}

// The emulator platform's memory map, as the AST1040 QEMU machine expects it.
const EMULATOR_MEMORY_MAP: &[(&str, &str)] = &[
    ("--hw-revision", "2.0.0"),
    ("--rom-offset", "0x80000000"),
    ("--rom-size", "0x10000"),
    ("--dccm-offset", "0x50000000"),
    ("--dccm-size", "0x4000"),
    ("--sram-offset", "0x40000000"),
    ("--sram-size", "0x80000"),
    ("--pic-offset", "0x60000000"),
    ("--i3c-offset", "0x20004000"),
    ("--i3c-size", "0x1000"),
    ("--mci-offset", "0x21000000"),
    ("--mci-size", "0xe00000"),
    ("--mbox-offset", "0x30020000"),
    ("--mbox-size", "0x28"),
    ("--soc-offset", "0x30030000"),
    ("--soc-size", "0x5e0"),
    ("--otp-offset", "0x70000000"),
    ("--otp-size", "0x140"),
    ("--lc-offset", "0x70000400"),
    ("--lc-size", "0x8c"),
];

impl Args {
    /// caliptra-mcu-emulator arguments for the selected machine, followed
    /// by the user's `--` arguments, which take precedence.
    fn emulator_args(&self) -> Result<EmulatorArgs, String> {
        let ast1040_only = [
            ("--flash", self.flash.is_some()),
            ("--xip-rom", self.xip_rom.is_some()),
            ("--caliptra-rom", self.caliptra_rom.is_some()),
            ("--trace", self.trace),
        ];
        // clap's required_if_eq ignores the default -m, so check here.
        match self.machine {
            MachineKind::Ast1040 => {
                let missing: Vec<&str> = ast1040_only[..3]
                    .iter()
                    .filter(|(_, given)| !given)
                    .map(|(name, _)| *name)
                    .collect();
                if !missing.is_empty() {
                    return Err(format!("-m ast1040 requires {}", missing.join(", ")));
                }
            }
            MachineKind::Emulator => {
                if let Some((name, _)) = ast1040_only.iter().find(|(_, given)| *given) {
                    return Err(format!("{name} is not used by -m emulator"));
                }
            }
        }
        let mut argv: Vec<OsString> = ["caliptra-mcu-emulator", "--no-stdin-uart"]
            .into_iter()
            .map(Into::into)
            .collect();
        let mut arg = |name: &str, value: OsString| argv.extend([name.into(), value]);
        // Production lifecycle; the mailbox driver feature lets EXECUTE=1
        // interrupt the MCU (see dispatch_to_emulator).
        arg("--device-security-state", "3".into());
        arg("--test-feature", "test-mcu-mbox-driver".into());
        match self.machine {
            MachineKind::Emulator => {
                let fw = |name: &str| self.fw_dir.join(name).into_os_string();
                arg("--rom", fw("mcu_rom.bin"));
                arg("--firmware", fw("mcu_runtime.bin"));
                arg("--caliptra-rom", fw("caliptra_rom.bin"));
                arg("--caliptra-firmware", fw("caliptra_fw.bin"));
                arg("--soc-manifest", fw("soc_manifest.bin"));
                let hash_file = self.fw_dir.join("vendor_pk_hash.txt");
                let hash = std::fs::read_to_string(&hash_file)
                    .map_err(|e| format!("{}: {e}", hash_file.display()))?;
                arg("--vendor-pk-hash", hash.trim().into());
                for (name, value) in EMULATOR_MEMORY_MAP {
                    arg(name, value.into());
                }
            }
            MachineKind::Ast1040 => {
                // Ast1040Emulator loads everything else from flash.
                let caliptra_rom = self.caliptra_rom.clone().unwrap_or_default();
                arg("--caliptra-rom", caliptra_rom.into_os_string());
                for name in [
                    "--rom",
                    "--firmware",
                    "--caliptra-firmware",
                    "--soc-manifest",
                ] {
                    arg(name, "/dev/null".into());
                }
            }
        }
        argv.extend(self.emu_args.iter().cloned());
        let matches = EmulatorArgs::command()
            .args_override_self(true)
            .try_get_matches_from(argv)
            .map_err(|e| e.to_string())?;
        EmulatorArgs::from_arg_matches(&matches).map_err(|e| e.to_string())
    }
}

// Caliptra firmware reports fatal errors through CPTRA_FW_ERROR_{FATAL,NON_FATAL}
// and may then end the emulator soon after (TB services exit), so watch the
// registers often and log any change as it happens.
const CALIPTRA_ERROR_CHECK_INTERVAL: u64 = 64;

struct CaliptraErrorWatch {
    soc: SocToCaliptraBus,
    last: [u32; 2],
}

impl CaliptraErrorWatch {
    const FW_ERROR_REGS: [(&'static str, u32); 2] = [
        ("FW_ERROR_FATAL", 0x3003_0008),
        ("FW_ERROR_NON_FATAL", 0x3003_000c),
    ];

    fn new(emu: &Emulator) -> Self {
        Self {
            soc: emu
                .caliptra_cpu
                .bus
                .soc_to_caliptra_bus(MailboxRequester::SocUser(1)),
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
        eprintln!(
            "caliptra-server: last Caliptra PCs (oldest first):\n{}",
            pcs.join(" ")
        );
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
    eprintln!(
        "caliptra-server: Caliptra at FMC entry; FHT marker found at [{}]",
        found.join(", ")
    );
}

/// The emulator, optionally with AST1040 platform support.
enum Machine {
    Emulator(Box<Emulator>),
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
        eprintln!(
            "caliptra-server: mailbox locked (0x{:08x}), dropping command",
            lock_val
        );
        return MboxResponse {
            status: 3,
            data: vec![],
        }; // CmdFailure
    }

    // Write SRAM payload (word-aligned).
    for (i, chunk) in payload.chunks(4).enumerate() {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        bus_write(
            emu,
            MBOX_SRAM_BASE + (i as u32) * 4,
            u32::from_le_bytes(word),
        );
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
            return MboxResponse {
                status: 3,
                data: vec![],
            };
        }
        steps += 1;
        match machine.step() {
            StepAction::Continue => {}
            other => {
                eprintln!(
                    "caliptra-server: emulator halted ({:?}) while waiting for mbox response",
                    other
                );
                bus_write(machine.emu(), MBOX_EXECUTE, 0);
                return MboxResponse {
                    status: 3,
                    data: vec![],
                };
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
fn run(
    mut machine: Machine,
    mbox_rx: Receiver<MboxCommand>,
    heartbeat: u64,
    trace_caliptra_pc: bool,
) {
    let mut caliptra_errors = CaliptraErrorWatch::new(machine.emu());
    let mut fht_reported = false;
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

        let emu = machine.emu();
        steps += 1;
        if steps.is_multiple_of(CALIPTRA_ERROR_CHECK_INTERVAL) {
            caliptra_errors.check();
        }
        if heartbeat != 0 && steps.is_multiple_of(heartbeat) {
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

        if !mbox_ready && bus_read(emu, FW_FLOW_STATUS) & FIRMWARE_MAILBOX_READY_MASK != 0 {
            println!("caliptra-server: FIRMWARE_MAILBOX_READY observed, accepting commands");
            mbox_ready = true;
        }
    }
}

fn main() {
    let args = Args::parse();
    let emu_args = args.emulator_args().unwrap_or_else(|e| {
        eprintln!("caliptra-server: {e}");
        std::process::exit(2);
    });

    let mbox_rx = mbox_server::start(&args.mbox_socket);

    println!("caliptra-server: initializing emulator…");
    let machine = match args.machine {
        MachineKind::Emulator => {
            Emulator::from_args(emu_args, false).map(|emu| Machine::Emulator(Box::new(emu)))
        }
        MachineKind::Ast1040 => {
            let config = Ast1040Config {
                flash: args.flash.clone().unwrap_or_default(),
                xip_rom: args.xip_rom.clone().unwrap_or_default(),
                trace: args.trace,
            };
            Ast1040Emulator::new(emu_args, &config).map(|ast| Machine::Ast1040(Box::new(ast)))
        }
    }
    .unwrap_or_else(|e| {
        eprintln!("caliptra-server: emulator init failed: {}", e);
        std::process::exit(1);
    });
    println!("caliptra-server: emulator ready, entering step loop");

    run(
        machine,
        mbox_rx,
        args.heartbeat.unwrap_or(0),
        args.pc_history,
    );
}
