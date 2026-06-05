"""Tests for the PCSS/1.0 frame parser."""
from __future__ import annotations

import unittest

from protocol_mapper.pcss_frame import parse_pcss_frame


# Wire-captured DISCOVERY knock (host -> camera UDP:51562), 69 bytes incl. trailing NUL.
DISCOVERY = (
    b"DISCOVERY * HTTP/1.1\r\n"
    b"HOST: 192.168.7.49\r\n"
    b"MX: 5\r\n"
    b"SERVICE: PCSS/1.0\r\n\x00"
)

# Wire-captured NOTIFY (camera -> host on PC's TCP:51560), 103 bytes.
NOTIFY = (
    b"NOTIFY * HTTP/1.1\r\n"
    b"DSC: 192.168.4.27\r\n"
    b"CAMERANAME: GFX100 II\r\n"
    b"DSCPORT: 15740\r\n"
    b"MX: 7\r\n"
    b"SERVICE: PCSS/1.0\r\n"
)

OK_RESPONSE = b"HTTP/1.1 200 OK\r\n\x00"
FORBIDDEN = b"HTTP/1.1 403 Forbidden\r\n\x00"


class PCSSFrameTests(unittest.TestCase):

    def test_parse_discovery_knock(self):
        f = parse_pcss_frame(DISCOVERY)
        self.assertIsNotNone(f)
        assert f is not None  # for the type checker
        self.assertEqual(f.verb, "DISCOVERY")
        self.assertIsNone(f.status_code)
        self.assertEqual(f.host, "192.168.7.49")
        self.assertEqual(f.mx, "5")
        self.assertEqual(f.service, "PCSS/1.0")
        self.assertTrue(f.trailing_nul)
        self.assertFalse(f.is_response)

    def test_parse_notify(self):
        f = parse_pcss_frame(NOTIFY)
        self.assertIsNotNone(f)
        assert f is not None
        self.assertEqual(f.verb, "NOTIFY")
        self.assertEqual(f.dsc, "192.168.4.27")
        self.assertEqual(f.camera_name, "GFX100 II")
        self.assertEqual(f.dsc_port, 15740)
        self.assertEqual(f.mx, "7")
        self.assertFalse(f.trailing_nul)  # the captured NOTIFY has no NUL

    def test_parse_200_ok(self):
        f = parse_pcss_frame(OK_RESPONSE)
        self.assertIsNotNone(f)
        assert f is not None
        self.assertEqual(f.status_code, 200)
        self.assertTrue(f.is_response)
        self.assertTrue(f.trailing_nul)

    def test_parse_403(self):
        f = parse_pcss_frame(FORBIDDEN)
        self.assertIsNotNone(f)
        assert f is not None
        self.assertEqual(f.status_code, 403)

    def test_empty_returns_none(self):
        self.assertIsNone(parse_pcss_frame(b""))

    def test_non_pcss_garbage_returns_none(self):
        self.assertIsNone(parse_pcss_frame(b"not a pcss frame at all"))

    def test_to_dict_round_trips_hex(self):
        f = parse_pcss_frame(NOTIFY)
        assert f is not None
        d = f.to_dict()
        self.assertEqual(d["verb"], "NOTIFY")
        self.assertEqual(d["headers"]["CAMERANAME"], "GFX100 II")
        self.assertEqual(bytes.fromhex(d["frame_bytes_hex"]), NOTIFY)


if __name__ == "__main__":
    unittest.main()
