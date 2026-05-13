// Command-level MCU Mailbox socket server.
//
// Counterpart to the QEMU aspeed-mcu-mbox device (aspeed_mcu_mbox.c).
// QEMU keeps register state locally; only EXECUTE=1 triggers a roundtrip.
//
// Wire protocol (magic 0x4D424F58 "MBOX", little-endian):
//
//   Header (12 bytes):
//     u32 magic    = 0x4D424F58
//     u16 version  = 1
//     u16 command
//     u32 payload_len
//
//   MBOX_EXECUTE (1) QEMU → server
//     u32 cmd
//     u32 dlen
//     u8  sram[ROUND_UP(dlen, 4)]
//
//   MBOX_RESPONSE (2) server → QEMU
//     u32 status
//     u32 dlen
//     u8  sram[ROUND_UP(dlen, 4)]

use std::io::{self, Read, Write};
use std::os::unix::net::UnixListener;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

const MAGIC: u32 = 0x4D424F58;
const VERSION: u16 = 1;
const CMD_EXECUTE: u16 = 1;
const CMD_RESPONSE: u16 = 2;

pub struct MboxCommand {
    pub cmd: u32,
    pub payload: Vec<u8>,
    pub resp_tx: Sender<MboxResponse>,
}

pub struct MboxResponse {
    pub status: u32,
    pub data: Vec<u8>,
}

/// Start the MCU Mailbox command server in a background thread.
/// Returns a Receiver the main loop must drain between CPU steps.
pub fn start(socket_path: &str) -> Receiver<MboxCommand> {
    let (tx, rx) = channel::<MboxCommand>();
    let path = socket_path.to_string();

    thread::Builder::new()
        .name("mbox-listener".into())
        .spawn(move || run_listener(path, tx))
        .expect("failed to spawn mbox-listener thread");

    rx
}

fn run_listener(path: String, tx: Sender<MboxCommand>) {
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap_or_else(|e| {
        panic!("mbox-server: failed to bind {}: {}", path, e)
    });
    println!("mbox-server: listening on {}", path);

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let tx2 = tx.clone();
                thread::Builder::new()
                    .name("mbox-conn".into())
                    .spawn(move || run_connection(s, tx2))
                    .ok();
            }
            Err(e) => eprintln!("mbox-server: accept error: {}", e),
        }
    }
}

fn run_connection(mut stream: std::os::unix::net::UnixStream, tx: Sender<MboxCommand>) {
    loop {
        // Read 12-byte header
        let mut hdr = [0u8; 12];
        if let Err(e) = stream.read_exact(&mut hdr) {
            if e.kind() != io::ErrorKind::UnexpectedEof {
                eprintln!("mbox-server: read header: {}", e);
            }
            break;
        }

        let magic = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let _ver = u16::from_le_bytes(hdr[4..6].try_into().unwrap());
        let cmd = u16::from_le_bytes(hdr[6..8].try_into().unwrap());
        let plen = u32::from_le_bytes(hdr[8..12].try_into().unwrap()) as usize;

        if magic != MAGIC {
            eprintln!("mbox-server: bad magic 0x{:08x}", magic);
            break;
        }

        // Read payload
        let mut payload = vec![0u8; plen];
        if plen > 0 && stream.read_exact(&mut payload).is_err() {
            break;
        }

        match cmd {
            CMD_EXECUTE => {
                if payload.len() < 8 {
                    eprintln!("mbox-server: MBOX_EXECUTE payload too short");
                    break;
                }
                let mbox_cmd = u32::from_le_bytes(payload[0..4].try_into().unwrap());
                let dlen = u32::from_le_bytes(payload[4..8].try_into().unwrap()) as usize;
                let data_bytes = dlen.min(payload.len().saturating_sub(8));
                let sram_payload = payload[8..8 + data_bytes].to_vec();

                println!(
                    "mbox-server: EXECUTE cmd=0x{:08x} dlen={} payload_bytes={}",
                    mbox_cmd,
                    dlen,
                    sram_payload.len()
                );

                let (resp_tx, resp_rx) = channel::<MboxResponse>();
                if tx
                    .send(MboxCommand {
                        cmd: mbox_cmd,
                        payload: sram_payload,
                        resp_tx,
                    })
                    .is_err()
                {
                    break;
                }

                // Block until the main loop processes the command
                let rsp = resp_rx.recv().unwrap_or(MboxResponse {
                    status: 3, // CMD_FAILURE
                    data: vec![],
                });

                if send_response(&mut stream, &rsp).is_err() {
                    break;
                }
            }
            other => {
                eprintln!("mbox-server: unknown command {}", other);
                break;
            }
        }
    }
}

fn send_response(
    stream: &mut std::os::unix::net::UnixStream,
    rsp: &MboxResponse,
) -> io::Result<()> {
    let dlen_words = rsp.data.len().div_ceil(4);
    let payload_len = 4 + 4 + dlen_words * 4; // status + dlen + sram

    let mut buf = vec![0u8; 12 + payload_len];
    // header
    buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    buf[4..6].copy_from_slice(&VERSION.to_le_bytes());
    buf[6..8].copy_from_slice(&CMD_RESPONSE.to_le_bytes());
    buf[8..12].copy_from_slice(&(payload_len as u32).to_le_bytes());
    // payload
    buf[12..16].copy_from_slice(&rsp.status.to_le_bytes());
    buf[16..20].copy_from_slice(&(rsp.data.len() as u32).to_le_bytes());
    // copy response bytes (word-aligned)
    let data_len = rsp.data.len().min(dlen_words * 4);
    buf[20..20 + data_len].copy_from_slice(&rsp.data[..data_len]);

    stream.write_all(&buf)
}
