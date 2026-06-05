"""Host-side PCSS FSM mirror tests — match the camera's 0..5 state model."""
from __future__ import annotations

import unittest

from protocol_mapper.pcss_fsm import (
    PTP_RESPONSE_DEVICE_BUSY,
    PCSSFSM,
    PCSSState,
)
from protocol_mapper.pcss_frame import parse_pcss_frame


NOTIFY = (
    b"NOTIFY * HTTP/1.1\r\n"
    b"DSC: 192.168.4.27\r\n"
    b"CAMERANAME: GFX100 II\r\n"
    b"DSCPORT: 15740\r\n"
    b"MX: 7\r\n"
    b"SERVICE: PCSS/1.0\r\n"
)


class PCSSFSMTests(unittest.TestCase):

    def test_initial_state_is_listening(self):
        self.assertEqual(PCSSFSM().current, PCSSState.LISTENING)

    def test_happy_path_listening_to_paired(self):
        fsm = PCSSFSM()
        fsm.step("knock_sent")
        self.assertEqual(fsm.current, PCSSState.LISTENING)
        fsm.step("callback_accepted")
        self.assertEqual(fsm.current, PCSSState.DISCOVERED)
        fsm.step("notify_received", parse_pcss_frame(NOTIFY))
        self.assertEqual(fsm.current, PCSSState.HANDSHAKING)
        fsm.step("ok_sent")
        self.assertEqual(fsm.current, PCSSState.HANDSHAKING)
        fsm.step("ptpip_connected")
        fsm.step("init_ack")
        self.assertEqual(fsm.current, PCSSState.PAIRED)

    def test_icmp_unreachable_before_session_goes_idle(self):
        fsm = PCSSFSM()
        fsm.step("icmp_unreachable")
        self.assertEqual(fsm.current, PCSSState.IDLE)

    def test_icmp_unreachable_mid_session_is_error(self):
        fsm = PCSSFSM()
        fsm.step("callback_accepted")
        fsm.step("notify_received", parse_pcss_frame(NOTIFY))
        fsm.step("icmp_unreachable")
        self.assertEqual(fsm.current, PCSSState.ERROR)

    def test_init_fail_device_busy_stays(self):
        fsm = PCSSFSM()
        fsm.step("callback_accepted")
        fsm.step("notify_received", parse_pcss_frame(NOTIFY))
        fsm.step("init_fail", reason=hex(PTP_RESPONSE_DEVICE_BUSY))
        self.assertEqual(fsm.current, PCSSState.HANDSHAKING)

    def test_init_fail_other_reason_is_error(self):
        fsm = PCSSFSM()
        fsm.step("callback_accepted")
        fsm.step("notify_received", parse_pcss_frame(NOTIFY))
        fsm.step("init_fail", reason="0x2002")
        self.assertEqual(fsm.current, PCSSState.ERROR)

    def test_rst_mid_session_goes_idle(self):
        fsm = PCSSFSM()
        fsm.step("callback_accepted")
        fsm.step("rst")
        self.assertEqual(fsm.current, PCSSState.IDLE)

    def test_history_records_transitions(self):
        fsm = PCSSFSM()
        fsm.step("callback_accepted")
        fsm.step("notify_received", parse_pcss_frame(NOTIFY))
        # at least one transition record per actual state change
        names = [(t.prev.name, t.new.name) for t in fsm.history]
        self.assertIn(("LISTENING", "DISCOVERED"), names)
        self.assertIn(("DISCOVERED", "HANDSHAKING"), names)


if __name__ == "__main__":
    unittest.main()
