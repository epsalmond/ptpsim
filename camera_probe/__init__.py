"""camera-probe — initiator-side camera protocol probe (promoted from fuji-remote).

Probes cameras across transports (PCSS/PTP-IP, reference app AP, http/XLV, USB) and emits an
**observation bundle**: a JSONL stream of distinct facts ("interface A, in state B, prop/op C
returned D"). The bundle is the only seam to downstream consumers (manifest generation, the
simulator) — this package builds the bundle and stops there.
"""
__all__ = ["bundle", "risk", "transports", "plans"]
