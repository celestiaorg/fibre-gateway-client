"""Runs fibre_verify against clients/testdata/commitment_vectors.json.

    python3 -m unittest
"""

import hashlib
import json
import pathlib
import unittest

from fibre_verify import VerifyError, commitment_roots, row_size, verify

VECTORS = pathlib.Path(__file__).resolve().parent.parent / "testdata" / "commitment_vectors.json"


def vector_data(size):
    """Byte i is the top byte of the wrapping uint32 product i * 2654435761."""
    a, mask32 = 2654435761, (1 << 32) - 1
    block = min(size, 1 << 20)
    ones = int.from_bytes((b"\x01" + bytes(7)) * block, "little")
    lane_mask = ones * mask32
    lanes = int.from_bytes(b"".join(((i * a) & mask32).to_bytes(8, "little") for i in range(block)), "little")
    step = ones * ((block * a) & mask32)
    out = bytearray()
    while len(out) < size:
        out += lanes.to_bytes(8 * block, "little")[3::8]
        lanes = (lanes + step) & lane_mask
    return bytes(out[:size])


def load_vectors():
    return json.loads(VECTORS.read_text())["vectors"]


class VectorTest(unittest.TestCase):
    def check(self, v):
        data = vector_data(v["size"])
        self.assertEqual(hashlib.sha256(data).hexdigest(), v["data_sha256"])
        self.assertEqual(row_size(len(data)), v["row_size"])
        siblings = [bytes.fromhex(s) for s in v["row_root_siblings"]]
        row_root, rlc_root = commitment_roots(data, siblings)
        self.assertEqual(row_root.hex(), v["row_root"])
        self.assertEqual(rlc_root.hex(), v["rlc_root"])
        verify(data, v["blob_id"], v["row_root_siblings"])

    def test_vectors(self):
        for v in load_vectors():
            with self.subTest(size=v["size"]):
                self.check(v)

    def test_tampering_fails(self):
        v = load_vectors()[1]
        data = bytearray(vector_data(v["size"]))
        data[0] ^= 1
        with self.assertRaisesRegex(VerifyError, "does not match"):
            verify(bytes(data), v["blob_id"], v["row_root_siblings"])
        siblings = list(v["row_root_siblings"])
        siblings[0] = "00" * 32
        with self.assertRaisesRegex(VerifyError, "does not match"):
            verify(vector_data(v["size"]), v["blob_id"], siblings)

    def test_malformed_input_fails(self):
        v = load_vectors()[0]
        data = vector_data(v["size"])
        cases = [
            (data, "zz", v["row_root_siblings"]),
            (data, v["blob_id"][:-2], v["row_root_siblings"]),
            (data, "01" + v["blob_id"][2:], v["row_root_siblings"]),
            (b"", v["blob_id"], v["row_root_siblings"]),
            (data, v["blob_id"], v["row_root_siblings"][:1]),
            (data, v["blob_id"], [v["row_root_siblings"][0], "00"]),
            (data, v["blob_id"], [v["row_root_siblings"][0], "zz"]),
        ]
        for args in cases:
            with self.assertRaises(VerifyError):
                verify(*args)


if __name__ == "__main__":
    unittest.main()
