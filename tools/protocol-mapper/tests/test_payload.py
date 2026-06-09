from __future__ import annotations

from datetime import UTC, datetime
import unittest

from rce.tools.fuji_ble_gps.payload import (
    LocationPayload,
    decode_location,
    encode_location,
    parse_payload_hex,
    registration_ack_value,
)


CAPTURED_HEX = "a8848516684bffb62100000000000000ea070501001a0e"


class PayloadTests(unittest.TestCase):
    def test_decode_captured_location_payload(self) -> None:
        payload = decode_location(bytes.fromhex(CAPTURED_HEX))
        self.assertEqual(payload.latitude, 37.7849)
        self.assertEqual(payload.longitude, -122.4783)
        self.assertEqual(payload.altitude_m, 33)
        self.assertEqual(payload.speed_mps, 0)
        self.assertEqual(payload.utc, datetime(2026, 5, 1, 0, 26, 14, tzinfo=UTC))

    def test_encode_captured_location_payload(self) -> None:
        payload = LocationPayload(
            latitude=37.7849,
            longitude=-122.4783,
            altitude_m=33,
            speed_mps=0,
            utc=datetime(2026, 5, 1, 0, 26, 14, tzinfo=UTC),
        )
        self.assertEqual(payload.encode().hex(), CAPTURED_HEX)

    def test_parse_payload_hex_allows_spaces(self) -> None:
        payload = "a8 84 85 16 68 4b ff b6 21 00 00 00 00 00 00 00 ea 07 05 01 00 1a 0e"
        self.assertEqual(parse_payload_hex(payload).hex(), CAPTURED_HEX)

    def test_registration_ack_sets_app_bit(self) -> None:
        self.assertEqual(registration_ack_value(bytes.fromhex("70df0500")).hex(), "70df0520")

    def test_encode_location_helper(self) -> None:
        self.assertEqual(
            encode_location(
                37.7849,
                -122.4783,
                33,
                0,
                datetime(2026, 5, 1, 0, 26, 14, tzinfo=UTC),
            ).hex(),
            CAPTURED_HEX,
        )

    def test_invalid_lengths_raise(self) -> None:
        with self.assertRaisesRegex(ValueError, "GPS payload must be 23 bytes"):
            decode_location(b"\x00")
        with self.assertRaisesRegex(ValueError, "GPS payload must be 23 bytes"):
            parse_payload_hex("00")
        with self.assertRaisesRegex(ValueError, "registration id must be 4 bytes"):
            registration_ack_value(b"\x00")


if __name__ == "__main__":
    unittest.main()
