// Licensed under the MIT license
//
// caliptra-server: Bridge between the caliptra-sw software emulator and QEMU.
//
// caliptra-server is launched independently of QEMU.  At startup it loads the
// Caliptra ROM and firmware bundle from the paths passed on the command line,
// boots the caliptra-hw-model emulator, and steps it until the runtime is
// ready.  It then listens on a Unix socket and waits for QEMU to connect.
//
// QEMU's Caliptra backend device (TYPE_ASPEED_CALIPTRA_EMU) connects to that
// socket from realize() and forwards every CA35 access to the Caliptra APB
// window as a request on the socket.  There is no boot handshake: the server
// is already runtime-ready by the time QEMU connects, and QEMU lets the CA35
// boot freely.
//
// Socket protocol (little-endian):
//
//   Header (12 bytes):
//     u32 magic       -- 0x43505452 ("CPTR")
//     u16 version     -- 1
//     u16 command
//     u32 payload_len
//
//   Commands:
//     3  APB_READ       QEMU→server  {u32 apb_addr}
//     4  APB_RDATA      server→QEMU  {u32 data}
//     5  APB_WRITE      QEMU→server  {u32 apb_addr, u32 data}
//     6  APB_WACK       server→QEMU  {} (empty payload)
//
//   APB address space (same as caliptra-hw-model apb_bus() addresses):
//     0x30020000..0x3002FFFF  mailbox CSR + SHA512 accelerator
//     0x30030000..0x3003FFFF  SOC IFC (generic + fuse registers)

use caliptra_api::SocManager;
use caliptra_emu_bus::Bus;
use caliptra_emu_types::RvSize;
use caliptra_hw_model::{BootParams, HwModel, InitParams};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

const SOCKET_MAGIC: u32 = 0x43505452; // "CPTR"
const SOCKET_VERSION: u16 = 1;
const CMD_APB_READ: u16 = 3;
const CMD_APB_RDATA: u16 = 4;
const CMD_APB_WRITE: u16 = 5;
const CMD_APB_WACK: u16 = 6;

const RUNTIME_READY_BOOT_STATUS: u32 = 0x600;
const MAX_BOOT_CYCLES: u32 = 60_000_000;


fn read_u32_le(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u16_le(r: &mut impl Read) -> io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn write_header(w: &mut impl Write, command: u16, payload_len: u32) -> io::Result<()> {
    w.write_all(&SOCKET_MAGIC.to_le_bytes())?;
    w.write_all(&SOCKET_VERSION.to_le_bytes())?;
    w.write_all(&command.to_le_bytes())?;
    w.write_all(&payload_len.to_le_bytes())
}

fn write_apb_rdata(w: &mut impl Write, data: u32) -> io::Result<()> {
    write_header(w, CMD_APB_RDATA, 4)?;
    w.write_all(&data.to_le_bytes())?;
    w.flush()
}

fn write_apb_wack(w: &mut impl Write) -> io::Result<()> {
    write_header(w, CMD_APB_WACK, 0)?;
    w.flush()
}

enum ApbReq {
    Read(u32),
    Write(u32, u32),
}

fn socket_has_data(stream: &std::os::unix::net::UnixStream) -> io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    if pfd.revents & libc::POLLHUP != 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "caliptra-server: QEMU closed the connection",
        ));
    }
    Ok(pfd.revents & libc::POLLIN != 0)
}

fn try_read_apb_request(
    stream: &mut std::os::unix::net::UnixStream,
) -> io::Result<Option<ApbReq>> {
    if !socket_has_data(stream)? {
        return Ok(None);
    }

    let magic = read_u32_le(stream)?;
    if magic != SOCKET_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("APB request bad magic: 0x{magic:08x}"),
        ));
    }
    let version = read_u16_le(stream)?;
    if version != SOCKET_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("APB request bad version: {version}"),
        ));
    }
    let command = read_u16_le(stream)?;
    let payload_len = read_u32_le(stream)? as usize;

    match command {
        CMD_APB_READ => {
            if payload_len < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "APB_READ payload too short",
                ));
            }
            let mut buf = vec![0u8; payload_len];
            stream.read_exact(&mut buf)?;
            let addr = u32::from_le_bytes(buf[0..4].try_into().unwrap());
            Ok(Some(ApbReq::Read(addr)))
        }
        CMD_APB_WRITE => {
            if payload_len < 8 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "APB_WRITE payload too short",
                ));
            }
            let mut buf = vec![0u8; payload_len];
            stream.read_exact(&mut buf)?;
            let addr = u32::from_le_bytes(buf[0..4].try_into().unwrap());
            let data = u32::from_le_bytes(buf[4..8].try_into().unwrap());
            Ok(Some(ApbReq::Write(addr, data)))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Unexpected command in APB loop: {command}"),
        )),
    }
}

fn serve_apb<M: HwModel>(
    hw: &mut M,
    mut stream: std::os::unix::net::UnixStream,
) -> io::Result<()> {
    loop {
        hw.step();
        let _ = hw.output().take(usize::MAX);

        match try_read_apb_request(&mut stream)? {
            None => {}
            Some(ApbReq::Read(addr)) => {
                let data = hw
                    .apb_bus()
                    .read(RvSize::Word, addr)
                    .unwrap_or(0xdead_beef);
                write_apb_rdata(&mut stream, data)?;
            }
            Some(ApbReq::Write(addr, data)) => {
                let _ = hw.apb_bus().write(RvSize::Word, addr, data);
                write_apb_wack(&mut stream)?;
            }
        }
    }
}

fn usage(argv0: &str) -> ! {
    eprintln!(
        "Usage: {argv0} --socket <path> --rom <path> --firmware <path>"
    );
    eprintln!();
    eprintln!("  --socket   PATH   Unix socket path to listen on");
    eprintln!("  --rom      PATH   Caliptra ROM image");
    eprintln!("  --firmware PATH   Caliptra firmware bundle");
    eprintln!();
    eprintln!("Example:");
    eprintln!(
        "  {argv0} --socket /tmp/caliptra.sock \\\n      \
         --rom images/caliptra-rom.bin --firmware images/caliptra-fw.bundle"
    );
    std::process::exit(1);
}

fn parse_arg(argv: &[String], name: &str, argv0: &str) -> PathBuf {
    let pos = argv
        .iter()
        .position(|a| a == name)
        .unwrap_or_else(|| usage(argv0));
    argv.get(pos + 1)
        .map(PathBuf::from)
        .unwrap_or_else(|| usage(argv0))
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let argv0 = argv.first().map(String::as_str).unwrap_or("caliptra-server");

    let socket_path = parse_arg(&argv, "--socket", argv0);
    let rom_path = parse_arg(&argv, "--rom", argv0);
    let fw_path = parse_arg(&argv, "--firmware", argv0);

    let rom = fs::read(&rom_path).unwrap_or_else(|e| {
        eprintln!("[caliptra-server] failed to read ROM {rom_path:?}: {e}");
        std::process::exit(1);
    });
    let fw = fs::read(&fw_path).unwrap_or_else(|e| {
        eprintln!("[caliptra-server] failed to read firmware {fw_path:?}: {e}");
        std::process::exit(1);
    });
    eprintln!(
        "[caliptra-server] loaded ROM {} bytes, firmware {} bytes",
        rom.len(),
        fw.len()
    );

    let mut hw = caliptra_hw_model::new(
        InitParams {
            rom: &rom,
            log_writer: Box::new(io::stderr()),
            ..Default::default()
        },
        BootParams {
            fw_image: Some(&fw),
            ..Default::default()
        },
    )
    .unwrap_or_else(|e| {
        eprintln!("[caliptra-server] hw-model init failed: {e}");
        std::process::exit(1);
    });

    eprintln!("[caliptra-server] firmware uploaded, stepping until runtime ready...");
    let mut cycles: u32 = 0;
    loop {
        let _ = hw.output().take(usize::MAX);
        let status = hw.soc_ifc().cptra_boot_status().read();
        if status == RUNTIME_READY_BOOT_STATUS {
            break;
        }
        if cycles >= MAX_BOOT_CYCLES {
            eprintln!(
                "[caliptra-server] timeout after {cycles} cycles: \
                 boot_status=0x{status:x} (waiting for 0x{RUNTIME_READY_BOOT_STATUS:x})"
            );
            std::process::exit(1);
        }
        hw.step();
        cycles += 1;
    }
    let _ = hw.output().take(usize::MAX);
    eprintln!(
        "[caliptra-server] boot_status=0x{RUNTIME_READY_BOOT_STATUS:x} reached after {cycles} cycles"
    );

    if socket_path.exists() {
        if let Err(e) = fs::remove_file(&socket_path) {
            eprintln!(
                "[caliptra-server] WARNING: failed to remove stale socket {socket_path:?}: {e}"
            );
        }
    }
    let listener = UnixListener::bind(&socket_path).unwrap_or_else(|e| {
        eprintln!("[caliptra-server] failed to bind socket {socket_path:?}: {e}");
        std::process::exit(1);
    });
    eprintln!("[caliptra-server] listening on {socket_path:?}");
    eprintln!("[caliptra-server] waiting for QEMU to connect...");

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                eprintln!("[caliptra-server] connection accepted");
                if let Err(e) = serve_apb(&mut hw, s) {
                    eprintln!("[caliptra-server] connection error: {e}");
                }
            }
            Err(e) => {
                eprintln!("[caliptra-server] accept error: {e}");
            }
        }
    }
}
