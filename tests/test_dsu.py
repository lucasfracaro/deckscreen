"""Tests for deck/py_modules/dsu.py (DSU/cemuhook motion server). Run: python3 -m unittest discover -s tests"""
import os
import socket
import struct
import sys
import tempfile
import time
import unittest
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "deck", "py_modules"))
import dsu  # noqa: E402


def deck_report(counter=7, accel=(0, -16384, 0), gyro=(160, 0, -320)) -> bytes:
    r = bytearray(64)
    struct.pack_into("<HBB", r, 0, 1, dsu.DECK_STATE, 64)
    struct.pack_into("<I", r, 4, counter)
    struct.pack_into("<6h", r, 24, *accel, *gyro)
    return bytes(r)


def client_packet(event: int, payload: bytes = b"") -> bytes:
    body = struct.pack("<I", event) + payload
    pkt = bytearray(struct.pack("<4sHHII", b"DSUC", 1001, len(body), 0, 42) + body)
    struct.pack_into("<I", pkt, 8, zlib.crc32(pkt) & 0xFFFFFFFF)
    return bytes(pkt)


def check_crc(pkt: bytes) -> bool:
    c = bytearray(pkt)
    crc = struct.unpack_from("<I", c, 8)[0]
    struct.pack_into("<I", c, 8, 0)
    return zlib.crc32(c) & 0xFFFFFFFF == crc


class ConversionTests(unittest.TestCase):
    def test_parse_and_convert(self):
        counter, accel, gyro = dsu.parse_report(deck_report())
        self.assertEqual((counter, accel, gyro), (7, (0, -16384, 0), (160, 0, -320)))
        ts, ax, ay, az, pitch, yaw, roll = dsu.to_motion(counter, accel, gyro)
        self.assertEqual(ts, 7 * dsu.SCAN_US)
        self.assertEqual((ax, ay, az), (0.0, 0.0, -1.0))  # flat on a table: gravity on the top→bottom axis
        self.assertEqual((pitch, yaw, roll), (10.0, 20.0, 0.0))  # pitch=RtL, yaw=-FtB, roll=TtB

    def test_deadzone_and_other_reports(self):
        self.assertEqual(dsu.to_motion(0, (0, 0, 0), (5, -7, 3))[4:], (0.0, 0.0, 0.0))
        bad = bytearray(deck_report())
        bad[2] = 0x04
        self.assertIsNone(dsu.parse_report(bytes(bad)))
        self.assertIsNone(dsu.parse_report(b"\x01\x00"))

    def test_packets(self):
        d = dsu.data_packet(1, 5, (1, 0, 0, 1, 0, 0, 0))
        self.assertEqual(len(d), 100)
        self.assertEqual(d[:4], b"DSUS")
        self.assertTrue(check_crc(d))
        self.assertEqual(struct.unpack_from("<H", d, 6)[0], 100 - 16)
        self.assertEqual(len(dsu.info_packet(1, 0, True)), 32)
        self.assertEqual(len(dsu.version_packet(1)), 22)

    def test_parse_request_rejects_bad_crc(self):
        good = client_packet(dsu.T_VERSION)
        self.assertEqual(dsu.parse_request(good), (dsu.T_VERSION, b""))
        bad = bytearray(good)
        bad[-1] ^= 1
        self.assertIsNone(dsu.parse_request(bytes(bad)))
        self.assertIsNone(dsu.parse_request(b"DSUS" + good[4:]))


class FindDeviceTests(unittest.TestCase):
    def test_picks_interface_2(self):
        with tempfile.TemporaryDirectory() as root:
            for name, iface, hid in (("hidraw1", 0, "28DE:1205"), ("hidraw2", 2, "28DE:1205"), ("hidraw3", 2, "046D:C52B")):
                dev = os.path.join(root, "devices", f"1-3:1.{iface}", f"0003:{hid}.000{name[-1]}")
                os.makedirs(dev)
                v, p = hid.split(":")
                with open(os.path.join(dev, "uevent"), "w") as f:
                    f.write(f"HID_ID=0003:0000{v}:0000{p}\nHID_NAME=Valve\n")
                os.makedirs(os.path.join(root, "class", name))
                os.symlink(dev, os.path.join(root, "class", name, "device"))
            self.assertEqual(dsu.find_hidraw(os.path.join(root, "class")), "/dev/hidraw2")
            self.assertEqual(dsu.find_hidraw(os.path.join(root, "missing")), "")


class ServerTests(unittest.TestCase):
    def test_end_to_end(self):
        tmp = tempfile.mkdtemp()
        fifo = os.path.join(tmp, "hidraw")
        os.mkfifo(fifo)
        srv = dsu.Server(port=0, device=fifo, log=lambda m: None)
        srv.start()
        cl = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        cl.settimeout(2)
        addr = ("127.0.0.1", srv.port)
        writer = None
        try:
            cl.sendto(client_packet(dsu.T_VERSION), addr)
            v = cl.recv(256)
            self.assertTrue(check_crc(v))
            self.assertEqual(struct.unpack_from("<IH", v, 16), (dsu.T_VERSION, 1001))

            cl.sendto(client_packet(dsu.T_INFO, struct.pack("<i4B", 2, 0, 1, 0, 0)), addr)
            states = sorted((p[20], p[21]) for p in (cl.recv(256), cl.recv(256)))
            self.assertEqual(states, [(0, 2), (1, 0)])  # slot 0 connected, slot 1 empty

            cl.sendto(client_packet(dsu.T_DATA, bytes(8)), addr)
            writer = os.open(fifo, os.O_WRONLY)   # blocks until the server opens the "device"
            os.write(writer, deck_report(counter=3))
            pkt = cl.recv(256)
            self.assertEqual(len(pkt), 100)
            self.assertEqual(struct.unpack_from("<I", pkt, 16)[0], dsu.T_DATA)
            ts, *vals = struct.unpack_from("<Q6f", pkt, 68)
            self.assertEqual(ts, 3 * dsu.SCAN_US)
            self.assertAlmostEqual(vals[3], 10.0)
            st = srv.status()
            self.assertEqual(st["clients"], ["127.0.0.1"])
            self.assertTrue(st["imu_active"])
        finally:
            srv.stop()
            cl.close()
            if writer is not None:
                os.close(writer)


if __name__ == "__main__":
    unittest.main()
