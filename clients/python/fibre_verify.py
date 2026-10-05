"""Checks that a Fibre gateway /v1/put receipt commits to the caller's own bytes.

Pure Python (stdlib only). It recomputes the blob version 0 commitment from the
data and the two row tree siblings that /v1/put returns, like the Rust
client. Use the Rust client in production; this one is much slower.

    from fibre_verify import verify
    verify(data, put["blob_id"], put["commitment_proof"]["row_root_siblings"])
"""

import hashlib

ORIGINAL_ROWS = 4096  # K
PARITY_ROWS = 12288  # N
ROW_ROOT_SIBLINGS = 2
BLOB_HEADER_LEN = 5
MAX_DATA_SIZE = (1 << 31) - BLOB_HEADER_LEN
BLOB_ID_LEN = 33
MIN_ROW_SIZE = 64

# GF(2^16) as used by Leopard / reed-solomon-simd.
_GF_ORDER = 65536
_GF_MODULUS = 65535
_GF_POLYNOMIAL = 0x1002D
_CANTOR_BASIS = (
    0x0001, 0xACCA, 0x3C0E, 0x163E, 0xC582, 0xED2E, 0x914C, 0x4012,
    0x6C98, 0x10D8, 0x6A72, 0xB900, 0xFDB8, 0xFB34, 0xFF38, 0x991E,
)


class VerifyError(Exception):
    """The commitment did not verify."""


def verify(data, blob_id, row_root_siblings):
    """Raises VerifyError unless the hex blob_id commits to data.

    row_root_siblings is the list of hex strings in the put response's
    commitment_proof.
    """
    try:
        bid = bytes.fromhex(blob_id)
    except (TypeError, ValueError):
        raise VerifyError("invalid blob id") from None
    if len(bid) != BLOB_ID_LEN:
        raise VerifyError("invalid blob id")
    if bid[0] != 0:
        raise VerifyError(f"unsupported blob version {bid[0]}")
    if not 0 < len(data) <= MAX_DATA_SIZE:
        raise VerifyError(f"data size {len(data)} out of range")
    try:
        siblings = [bytes.fromhex(s) for s in row_root_siblings]
    except (TypeError, ValueError):
        raise VerifyError("invalid row root siblings") from None
    if len(siblings) != ROW_ROOT_SIBLINGS or any(len(s) != 32 for s in siblings):
        raise VerifyError(f"expected {ROW_ROOT_SIBLINGS} row root siblings of 32 bytes")
    row_root, rlc_root = commitment_roots(data, siblings)
    if hashlib.sha256(row_root + rlc_root).digest() != bid[1:]:
        raise VerifyError("commitment does not match data")


def row_size(data_len):
    """Row size for data_len bytes of blob version 0 data."""
    rows = -(-(data_len + BLOB_HEADER_LEN) // ORIGINAL_ROWS)
    return -(-rows // MIN_ROW_SIZE) * MIN_ROW_SIZE


def commitment_roots(data, siblings):
    """Returns (row_root, rlc_root) for data and the 32-byte row root siblings."""
    size = row_size(len(data))
    header = b"\x00" + len(data).to_bytes(4, "big")
    matrix = header + bytes(data) + bytes(ORIGINAL_ROWS * size - BLOB_HEADER_LEN - len(data))

    leaves = [_hash_leaf(matrix[i * size:(i + 1) * size]) for i in range(ORIGINAL_ROWS)]
    row_root = _merkle_root(leaves)
    for sibling in siblings:
        row_root = _hash_internal(row_root, sibling)

    coeffs = _derive_coefficients(row_root, ORIGINAL_ROWS, PARITY_ROWS, size)
    rlcs = _rlcs(matrix, size, coeffs)
    rlc_root = _merkle_root([_hash_leaf(rlcs[i * 16:(i + 1) * 16]) for i in range(ORIGINAL_ROWS)])
    return row_root, rlc_root


def _hash_leaf(data):
    return hashlib.sha256(b"\x00" + data).digest()


def _hash_internal(left, right):
    return hashlib.sha256(b"\x01" + left + right).digest()


def _merkle_root(nodes):
    while len(nodes) > 1:
        nodes = [_hash_internal(nodes[i], nodes[i + 1]) for i in range(0, len(nodes), 2)]
    return nodes[0]


def _gf_tables():
    """Exp and log tables in the Cantor basis, as in reed-solomon-simd."""
    exp = [0] * _GF_ORDER
    log = [0] * _GF_ORDER
    state = 1
    for i in range(_GF_MODULUS):
        exp[state] = i
        state <<= 1
        if state >= _GF_ORDER:
            state ^= _GF_POLYNOMIAL
    exp[0] = _GF_MODULUS
    for i, basis in enumerate(_CANTOR_BASIS):
        width = 1 << i
        for j in range(width):
            log[j + width] = log[j] ^ basis
    log = [exp[v] for v in log]
    for i in range(_GF_ORDER):
        exp[log[i]] = i
    exp[_GF_MODULUS] = exp[0]
    return exp, log


def _derive_coefficients(row_root, k, n, size):
    """RLC coefficients, each as 8 GF(2^16) limbs."""
    params = k.to_bytes(4, "little") + n.to_bytes(4, "little") + size.to_bytes(4, "little")
    seed = hashlib.sha256(row_root + params).digest()
    coeffs = []
    for i in range(size // 2):
        h = hashlib.sha256(seed + i.to_bytes(4, "little")).digest()
        folded = bytes(a ^ b for a, b in zip(h[:16], h[16:]))
        coeffs.append([int.from_bytes(folded[2 * l:2 * l + 2], "little") for l in range(8)])
    return coeffs


def _rlcs(matrix, size, coeffs):
    """The 16-byte RLC of each original row, concatenated.

    Bit-sliced over rows: each row is one byte lane of a big int, so one int
    operation handles all K rows at once.
    """
    exp, log = _gf_tables()
    k = ORIGINAL_ROWS

    # basis[j][b]: coefficient j times the symbol 1 << b, packed as a 128-bit int.
    log_bit = [log[1 << b] for b in range(16)]
    basis = []
    for limbs in coeffs:
        logs = [log[c] if c else None for c in limbs]
        packed = []
        for lb in log_bit:
            v = 0
            for l, lc in enumerate(logs):
                if lc is not None:
                    s = lb + lc
                    v |= exp[(s + (s >> 16)) & 0xFFFF] << (16 * l)
            packed.append(v)
        basis.append(packed)

    ones = int.from_bytes(b"\x01" * k, "little")
    lanes = [ones * m for m in range(256)]  # lanes[m]: byte m in every row's lane
    bits = [bytes((v >> i) & 1 for i in range(8)) for v in range(256)]

    # acc[t]: per row lane, bit u holds the parity contribution of byte bit u to output bit t.
    acc = [0] * 128
    for col in range(size):
        chunk, pos = divmod(col, 64)
        j = chunk * 32 + pos % 32
        b0 = 0 if pos < 32 else 8
        # masks[t]: which bits u of this byte column feed output bit t.
        masks = 0
        for u in range(8):
            spread = b"".join(bits[x] for x in basis[j][b0 + u].to_bytes(16, "little"))
            masks |= int.from_bytes(spread, "little") << u
        column = int.from_bytes(matrix[col::size], "little")
        if not column:
            continue
        for t, m in enumerate(masks.to_bytes(128, "little")):
            if m:
                acc[t] ^= column & lanes[m]

    parity = bytes(bin(v).count("1") & 1 for v in range(256))
    out = bytearray(16 * k)
    for q in range(16):
        y = 0
        for w in range(8):
            plane = acc[8 * q + w].to_bytes(k, "little").translate(parity)
            y |= int.from_bytes(plane, "little") << w
        out[q::16] = y.to_bytes(k, "little")
    return bytes(out)
