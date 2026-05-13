#!/usr/bin/env python3
"""
mbox_send.py — Send a MCU Mailbox command to caliptra-server via Unix socket.

MCU Mailbox 的合法 command 全部是 ASCII 4字元 code。
每個 request 的 payload 第一個 u32 是 checksum：
  checksum = -(cmd_id + sum(payload[4:] as u32 words)) mod 2^32

常用 command:
  0x4D465756 (MFWV) = MC_FIRMWARE_VERSION  — 查詢 firmware 版本
  0x4D434150 (MCAP) = MC_DEVICE_CAPABILITIES — 查詢設備能力
  0x4D44_4944 (MDID) = MC_DEVICE_ID
  0x4D444E49 (MDIN) = MC_DEVICE_INFO
  0x4D47_4C47 (MGLG) = MC_GET_LOG
  0x4D46_5354 (MFST) = MC_FIPS_SELF_TEST_START
  0x4D43_5247 (MCRG) = MC_RANDOM_GENERATE

Usage:
    # 查詢 firmware version (index=0)
    python3 mbox_send.py fw-version
    python3 mbox_send.py fw-version --index 1

    # 查詢 device capabilities
    python3 mbox_send.py device-caps

    # 查詢 device ID
    python3 mbox_send.py device-id

    # 取得 log (type=0=debug)
    python3 mbox_send.py get-log
    python3 mbox_send.py get-log --log-type 1

    # Random generate
    python3 mbox_send.py random-generate --length 32

    # 送自訂 raw command (你負責填 checksum)
    python3 mbox_send.py raw --cmd 0x4D465756 --hex CHECKSUM_HEX_DATA

    # 送自訂 command，自動算 checksum
    python3 mbox_send.py raw --cmd 0x4D465756 --data-hex 00000000 --auto-checksum
"""

import argparse
import socket
import struct
import sys

MBOX_MAGIC   = 0x4D424F58   # "MBOX"
VERSION      = 1
CMD_EXECUTE  = 1
CMD_RESPONSE = 2

STATUS_NAMES = {0: "BUSY", 1: "DATA_READY", 2: "COMPLETE", 3: "CMD_FAILURE"}

# Known command codes
COMMANDS = {
    "MFWV": 0x4D465756,  # MC_FIRMWARE_VERSION
    "MCAP": 0x4D434150,  # MC_DEVICE_CAPABILITIES
    "MDID": 0x4D444944,  # MC_DEVICE_ID
    "MDIN": 0x4D44494E,  # MC_DEVICE_INFO
    "MGLG": 0x4D474C47,  # MC_GET_LOG
    "MCLG": 0x4D434C47,  # MC_CLEAR_LOG
    "MFST": 0x4D465354,  # MC_FIPS_SELF_TEST_START
    "MFGR": 0x4D464752,  # MC_FIPS_SELF_TEST_GET_RESULTS
    "MFPE": 0x4D465045,  # MC_FIPS_PERIODIC_ENABLE
    "MFPS": 0x4D465053,  # MC_FIPS_PERIODIC_STATUS
    "MCRG": 0x4D435247,  # MC_RANDOM_GENERATE
    "MCRS": 0x4D435253,  # MC_RANDOM_STIR
    "MCSI": 0x4D435349,  # MC_SHA_INIT
    "MCSU": 0x4D435355,  # MC_SHA_UPDATE
    "MCSF": 0x4D435346,  # MC_SHA_FINAL
}


def calc_checksum(cmd_id: int, data_bytes: bytes) -> int:
    """Caliptra MCU mailbox checksum, matching the Rust implementation in
    runtime/userspace/api/caliptra-api/src/checksum.rs:
        checksum = 0u32 - (sum(cmd_id.to_le_bytes()) + sum(data_bytes))
    Both cmd_id and data are summed **byte by byte** (not as u32 words).
    """
    total = 0
    for b in cmd_id.to_bytes(4, 'little'):
        total = (total + b) & 0xFFFFFFFF
    for b in data_bytes:
        total = (total + b) & 0xFFFFFFFF
    return (-total) & 0xFFFFFFFF


def build_request(cmd_id: int, data_after_checksum: bytes) -> bytes:
    """Build a complete MCU mbox request: checksum(u32) + data"""
    chksum = calc_checksum(cmd_id, data_after_checksum)
    return struct.pack('<I', chksum) + data_after_checksum


def pack_mbox_header(payload_len: int) -> bytes:
    return (struct.pack('<I', MBOX_MAGIC)
          + struct.pack('<H', VERSION)
          + struct.pack('<H', CMD_EXECUTE)
          + struct.pack('<I', payload_len))


def recvall(s: socket.socket, n: int) -> bytes:
    buf = b''
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            raise ConnectionError(f"Socket closed after {len(buf)}/{n} bytes")
        buf += chunk
    return buf


def send_mbox(sock_path: str, cmd_id: int, req_payload: bytes,
              timeout: float = 120.0) -> tuple:
    """Send MBOX_EXECUTE with pre-built payload (including checksum), return (status, rdata)."""
    dlen = len(req_payload)
    padded = req_payload + b'\x00' * ((-dlen) % 4)

    exec_payload = struct.pack('<II', cmd_id, dlen) + padded
    header = pack_mbox_header(len(exec_payload))

    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        # Use a finite timeout so blocking recv() can be interrupted by
        # Ctrl+C / SIGTERM (Python signals fire between syscalls).
        s.settimeout(timeout)
        s.connect(sock_path)
        s.sendall(header + exec_payload)

        rsp_hdr = recvall(s, 12)
        r_magic = struct.unpack('<I', rsp_hdr[0:4])[0]
        r_cmd   = struct.unpack('<H', rsp_hdr[6:8])[0]
        r_plen  = struct.unpack('<I', rsp_hdr[8:12])[0]

        if r_magic != MBOX_MAGIC:
            raise ValueError(f"Bad response magic: 0x{r_magic:08x}")
        if r_cmd != CMD_RESPONSE:
            raise ValueError(f"Unexpected response command: {r_cmd}")

        rsp_payload = recvall(s, r_plen)
        status = struct.unpack('<I', rsp_payload[0:4])[0]
        rdlen  = struct.unpack('<I', rsp_payload[4:8])[0]
        rdata  = rsp_payload[8:8 + rdlen]

    return status, rdata


def print_result(status: int, rdata: bytes):
    status_name = STATUS_NAMES.get(status, f"UNKNOWN({status})")
    print(f"status = {status} ({status_name})")
    if rdata:
        print(f"data   = {rdata.hex()}  ({len(rdata)} bytes)")
        # print as u32 words
        words = [struct.unpack_from('<I', rdata, i)[0]
                 for i in range(0, len(rdata) - 3, 4)]
        if words:
            print(f"data(u32): {' '.join(f'0x{w:08x}' for w in words)}")
        # try print as ASCII
        printable = ''.join(chr(b) if 32 <= b < 127 else '.' for b in rdata)
        print(f"data(str): {printable!r}")
    else:
        print("data   = (empty)")


def connect_and_send(args, cmd_id: int, payload_data: bytes):
    """Build request with checksum, send, and print result."""
    req = build_request(cmd_id, payload_data)
    cmd_ascii = cmd_id.to_bytes(4, 'big').decode('ascii', errors='?')
    print(f"cmd=0x{cmd_id:08x} ({cmd_ascii})  req({len(req)}B)={req.hex()}")
    try:
        status, rdata = send_mbox(args.mbox_socket, cmd_id, req, timeout=args.timeout)
    except (ConnectionRefusedError, FileNotFoundError) as e:
        print(f"ERROR: {e} — is caliptra-server running?", file=sys.stderr)
        sys.exit(1)
    except socket.timeout:
        print(f"ERROR: no response within {args.timeout}s — MCU may not be ready "
              f"or never set CMD_STATUS", file=sys.stderr)
        sys.exit(2)
    except KeyboardInterrupt:
        print("\nInterrupted.", file=sys.stderr)
        sys.exit(130)
    print_result(status, rdata)
    return status


def main():
    ap = argparse.ArgumentParser(
        description="Send MCU Mailbox commands to caliptra-server",
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--mbox-socket', default='/tmp/mcu_mbox.sock')
    ap.add_argument('--timeout', type=float, default=120.0,
                    help='Seconds to wait for response (default: 120)')
    sub = ap.add_subparsers(dest='subcmd', required=True)

    # fw-version
    p = sub.add_parser('fw-version', help='MC_FIRMWARE_VERSION (MFWV)')
    p.add_argument('--index', type=int, default=0,
                   help='0=CaliptraCore 1=McuRuntime 2=SoC')

    # device-caps
    sub.add_parser('device-caps', help='MC_DEVICE_CAPABILITIES (MCAP)')

    # device-id
    sub.add_parser('device-id', help='MC_DEVICE_ID (MDID)')

    # device-info
    p = sub.add_parser('device-info', help='MC_DEVICE_INFO (MDIN)')
    p.add_argument('--index', type=int, default=0)

    # get-log
    p = sub.add_parser('get-log', help='MC_GET_LOG (MGLG)')
    p.add_argument('--log-type', type=int, default=0, help='0=debug 1=attestation')

    # fips-self-test
    sub.add_parser('fips-self-test', help='MC_FIPS_SELF_TEST_START (MFST)')
    sub.add_parser('fips-self-test-results', help='MC_FIPS_SELF_TEST_GET_RESULTS (MFGR)')

    # random-generate
    p = sub.add_parser('random-generate', help='MC_RANDOM_GENERATE (MCRG)')
    p.add_argument('--length', type=int, default=32, help='bytes to generate')

    # raw
    p = sub.add_parser('raw', help='Send raw command (auto-checksum added)')
    p.add_argument('--cmd', required=True, help='Command code (hex, e.g. 0x4D465756)')
    p.add_argument('--data-hex', default='', help='Payload bytes AFTER checksum, as hex')
    p.add_argument('--raw-hex', default='',
                   help='Full payload hex including checksum (no auto-calc)')

    args = ap.parse_args()

    if args.subcmd == 'fw-version':
        cmd_id = COMMANDS["MFWV"]
        payload = struct.pack('<I', args.index)  # index field after checksum
        connect_and_send(args, cmd_id, payload)

    elif args.subcmd == 'device-caps':
        connect_and_send(args, COMMANDS["MCAP"], b'')

    elif args.subcmd == 'device-id':
        connect_and_send(args, COMMANDS["MDID"], b'')

    elif args.subcmd == 'device-info':
        payload = struct.pack('<I', args.index)
        connect_and_send(args, COMMANDS["MDIN"], payload)

    elif args.subcmd == 'get-log':
        payload = struct.pack('<I', args.log_type)
        connect_and_send(args, COMMANDS["MGLG"], payload)

    elif args.subcmd == 'fips-self-test':
        connect_and_send(args, COMMANDS["MFST"], b'')

    elif args.subcmd == 'fips-self-test-results':
        connect_and_send(args, COMMANDS["MFGR"], b'')

    elif args.subcmd == 'random-generate':
        payload = struct.pack('<I', args.length)
        connect_and_send(args, COMMANDS["MCRG"], payload)

    elif args.subcmd == 'raw':
        cmd_id = int(args.cmd.replace('_', ''), 0)
        if args.raw_hex:
            # send as-is, no auto-checksum
            req = bytes.fromhex(args.raw_hex.replace('_', '').replace(' ', ''))
            cmd_ascii = cmd_id.to_bytes(4, 'big').decode('ascii', errors='?')
            print(f"cmd=0x{cmd_id:08x} ({cmd_ascii})  req({len(req)}B)={req.hex()}")
            try:
                status, rdata = send_mbox(args.mbox_socket, cmd_id, req, timeout=args.timeout)
            except (ConnectionRefusedError, FileNotFoundError) as e:
                print(f"ERROR: {e}", file=sys.stderr)
                sys.exit(1)
            except socket.timeout:
                print(f"ERROR: no response within {args.timeout}s", file=sys.stderr)
                sys.exit(2)
            except KeyboardInterrupt:
                print("\nInterrupted.", file=sys.stderr)
                sys.exit(130)
            print_result(status, rdata)
        else:
            data = bytes.fromhex(args.data_hex.replace('_', '').replace(' ', '')) if args.data_hex else b''
            connect_and_send(args, cmd_id, data)


if __name__ == '__main__':
    main()
