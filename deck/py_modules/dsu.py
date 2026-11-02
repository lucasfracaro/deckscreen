"""
DSU (cemuhook protocol) motion server for the Steam Deck.

Reads the Deck's built-in controller straight from hidraw (read-only, alongside
Steam, so Steam Input keeps working: trackpad mouse, layouts, etc.) and serves
accelerometer + gyro on UDP 26760. Emulators on the PC (Cemu, Dolphin, Ryujinx,
Citra/Lime3DS, ...) connect to the Deck's IP as a "DSU/cemuhook" motion source.

Report layout: SDL's SteamDeckStatePacket_t; axis conversion follows
kmicki/SteamDeckGyroDSU (MIT), which is the de facto reference for this.
"""

import os
import select
import socket
import struct
import threading
import time
import zlib

PORT = 26760
VID, PID, INTERFACE = 0x28DE, 0x1205, 2
REPORT_LEN = 64
DECK_STATE = 0x09           # ValveInReportHeader_t.ucType for Deck state
ACC_1G = 16384.0            # ±2 g over int16
GYRO_1DPS = 16.0            # counts per °/s (SteamDeckGyroDSU's calibration)
GYRO_DEADZONE = 8
SCAN_US = 4000              # Deck reports at 250 Hz
CLIENT_TIMEOUT = 5.0        # emulators re-request data about once per second

PROTO = 1001
T_VERSION, T_INFO, T_DATA = 0x100000, 0x100001, 0x100002

HEADER = struct.Struct("<4sHHII")          # magic, version, length, crc, id (16 bytes)
SHARED = struct.Struct("<BBBB6sBB")        # slot, state, model, connection, mac, battery, connected (12)
MOTION = struct.Struct("<Q6f")             # timestamp µs, accX/Y/Z (g), pitch/yaw/roll (°/s)


# ------------------------------------------------------------------ device

def find_hidraw(sys_root: str = "/sys/class/hidraw") -> str:
    """Path of the Deck controller's hidraw node for interface 2, or ''."""
    want = f"HID_ID=0003:{VID:08X}:{PID:08X}"
    try:
        names = sorted(os.listdir(sys_root))
    except OSError:
        return ""
    for name in names:
        dev = os.path.join(sys_root, name, "device")
        try:
            with open(os.path.join(dev, "uevent"), encoding="utf-8") as f:
                if want not in f.read().upper():
                    continue
            # .../1-3:1.2/0003:28DE:1205.0003 -> interface 2
            parent = os.path.basename(os.path.dirname(os.path.realpath(dev)))
        except OSError:
            continue
        if parent.rsplit(".", 1)[-1] == str(INTERFACE):
            return "/dev/" + name
    return ""


def parse_report(data: bytes):
    """64-byte Deck state report -> (counter, accel(3), gyro(3)) raw int16, or None."""
    if len(data) < 36 or data[2] != DECK_STATE:
        return None
    counter = struct.unpack_from("<I", data, 4)[0]
    # offsets 24..35: accel right→left, top→bottom, front→back; gyro in the same order
    a_rl, a_tb, a_fb, g_rl, g_tb, g_fb = struct.unpack_from("<6h", data, 24)
    return counter, (a_rl, a_tb, a_fb), (g_rl, g_tb, g_fb)


def to_motion(counter: int, accel, gyro):
    """Raw Deck axes -> DSU (timestamp µs, accX, accY, accZ, pitch, yaw, roll)."""
    a_rl, a_tb, a_fb = accel
    g_rl, g_tb, g_fb = (0 if -GYRO_DEADZONE < v < GYRO_DEADZONE else v for v in gyro)
    return (
        counter * SCAN_US,
        -a_rl / ACC_1G,
        -a_fb / ACC_1G,
        a_tb / ACC_1G,
        g_rl / GYRO_1DPS,
        -g_fb / GYRO_1DPS,
        g_tb / GYRO_1DPS,
    )


# ------------------------------------------------------------------ protocol

def _packet(server_id: int, event: int, payload: bytes) -> bytes:
    body = struct.pack("<I", event) + payload
    pkt = bytearray(HEADER.pack(b"DSUS", PROTO, len(body), 0, server_id) + body)
    struct.pack_into("<I", pkt, 8, zlib.crc32(pkt) & 0xFFFFFFFF)
    return bytes(pkt)


def _shared(slot: int, connected: bool, data: bool = False) -> bytes:
    if not connected:
        return SHARED.pack(slot, 0, 0, 0, b"\0" * 6, 0, 0)
    # state 2 = connected, model 2 = full gyro, connection 1 = USB, battery 5 = full
    return SHARED.pack(slot, 2, 2, 1, b"\0\0\0\0\0\x01", 0x05, 1 if data else 0)


def version_packet(server_id: int) -> bytes:
    return _packet(server_id, T_VERSION, struct.pack("<H", PROTO))


def info_packet(server_id: int, slot: int, connected: bool) -> bytes:
    return _packet(server_id, T_INFO, _shared(slot, connected))


def data_packet(server_id: int, number: int, motion) -> bytes:
    payload = (
        _shared(0, True, data=True)
        + struct.pack("<I", number)
        + bytes(20)                 # buttons/sticks: the Deck's controls go through Moonlight
        + bytes(12)                 # two touch points
        + MOTION.pack(*motion)
    )
    return _packet(server_id, T_DATA, payload)


def parse_request(pkt: bytes):
    """Client packet -> (event, payload) or None if malformed / bad CRC."""
    if len(pkt) < 20 or pkt[:4] != b"DSUC":
        return None
    _, _, length, crc, _ = HEADER.unpack_from(pkt, 0)
    if len(pkt) < 16 + length:
        return None
    pkt = pkt[: 16 + length]
    check = bytearray(pkt)
    struct.pack_into("<I", check, 8, 0)
    if zlib.crc32(check) & 0xFFFFFFFF != crc:
        return None
    return struct.unpack_from("<I", pkt, 16)[0], pkt[20:]


# ------------------------------------------------------------------ server

class Server:
    def __init__(self, port: int = PORT, device: str = "", log=print):
        self.port = port
        self.fixed_device = device  # tests pass a fake node; normally found via sysfs
        self.device = device
        self.log = log
        self.id = int.from_bytes(os.urandom(4), "little")
        self.clients = {}           # addr -> last request time
        self.lock = threading.Lock()
        self.stop_event = threading.Event()
        self.threads = []
        self.sock = None
        self.packets = 0
        self.last_motion = 0.0      # time of the last non-zero IMU sample
        self.error = ""

    # -- lifecycle
    def start(self):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("0.0.0.0", self.port))
        self.port = self.sock.getsockname()[1]
        self.sock.settimeout(0.5)
        for target in (self._serve, self._read):
            t = threading.Thread(target=target, daemon=True)
            t.start()
            self.threads.append(t)

    def stop(self):
        self.stop_event.set()
        for t in self.threads:
            t.join(timeout=2)
        if self.sock:
            self.sock.close()

    def status(self) -> dict:
        now = time.monotonic()
        with self.lock:
            clients = [a[0] for a, t in self.clients.items() if now - t < CLIENT_TIMEOUT]
        return {
            "running": not self.stop_event.is_set(),
            "port": self.port,
            "device": self.device,
            "clients": clients,
            # Steam stops the IMU stream when nothing in the active layout uses the gyro
            "imu_active": now - self.last_motion < 2.0,
            "error": self.error,
        }

    # -- UDP side
    def _serve(self):
        while not self.stop_event.is_set():
            try:
                pkt, addr = self.sock.recvfrom(1024)
            except (socket.timeout, OSError):
                continue
            req = parse_request(pkt)
            if not req:
                continue
            event, payload = req
            try:
                if event == T_VERSION:
                    self.sock.sendto(version_packet(self.id), addr)
                elif event == T_INFO and len(payload) >= 4:
                    count = max(0, min(4, struct.unpack_from("<i", payload, 0)[0]))
                    for slot in payload[4 : 4 + count]:
                        self.sock.sendto(info_packet(self.id, slot, slot == 0), addr)
                elif event == T_DATA:
                    with self.lock:
                        if addr not in self.clients:
                            self.log(f"DSU: cliente {addr[0]}:{addr[1]} conectado")
                        self.clients[addr] = time.monotonic()
            except OSError:
                pass

    def _targets(self):
        now = time.monotonic()
        with self.lock:
            for a in [a for a, t in self.clients.items() if now - t >= CLIENT_TIMEOUT]:
                del self.clients[a]
            return list(self.clients)

    # -- HID side
    def _read(self):
        while not self.stop_event.is_set():
            if not self._targets():         # nobody listening: don't touch the controller
                self.stop_event.wait(0.3)
                continue
            path = self.fixed_device or find_hidraw()
            if not path:
                self.error = "controle do Deck não encontrado"
                self.stop_event.wait(3)
                continue
            try:
                fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
            except OSError as e:
                self.error = f"sem acesso a {path}: {e.strerror}"
                self.stop_event.wait(3)
                continue
            self.device, self.error = path, ""
            try:
                self._pump(fd)
            finally:
                os.close(fd)

    def _pump(self, fd: int):
        while not self.stop_event.is_set():
            r, _, _ = select.select([fd], [], [], 0.5)
            if not r:
                continue
            try:
                data = os.read(fd, REPORT_LEN)
            except BlockingIOError:
                continue
            except OSError:
                return                      # device went away; rescan
            parsed = parse_report(data)
            if not parsed:
                continue
            counter, accel, gyro = parsed
            if any(accel) or any(gyro):
                self.last_motion = time.monotonic()
            targets = self._targets()
            if not targets:
                return                      # last client left; go idle
            self.packets += 1
            pkt = data_packet(self.id, self.packets, to_motion(counter, accel, gyro))
            for addr in targets:
                try:
                    self.sock.sendto(pkt, addr)
                except OSError:
                    pass
