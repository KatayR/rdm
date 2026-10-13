"""Minimal minisign verifier (Ed25519, RFC 8032 reference arithmetic) for checking Zig tarballs.
Usage: python -I minisign_verify.py <public key base64> <file> <file.minisig>"""
import base64, hashlib, sys

p = 2**255 - 19
L = 2**252 + 27742317777372353535851937790883648493
d = -121665 * pow(121666, p - 2, p) % p
I = pow(2, (p - 1) // 4, p)


def xrecover(y):
    xx = (y * y - 1) * pow(d * y * y + 1, p - 2, p)
    x = pow(xx, (p + 3) // 8, p)
    if (x * x - xx) % p:
        x = x * I % p
    if (x * x - xx) % p:
        raise ValueError("not a point")
    return x


def add(P, Q):
    (x1, y1, z1, t1), (x2, y2, z2, t2) = P, Q
    A = (y1 - x1) * (y2 - x2) % p
    B = (y1 + x1) * (y2 + x2) % p
    C = t1 * 2 * d * t2 % p
    D = z1 * 2 * z2 % p
    E, F, G, H = B - A, D - C, D + C, B + A
    return (E * F % p, G * H % p, F * G % p, E * H % p)


def mul(s, P):
    Q = (0, 1, 1, 0)
    while s:
        if s & 1:
            Q = add(Q, P)
        P = add(P, P)
        s >>= 1
    return Q


def decode(b):
    y = int.from_bytes(b, "little")
    sign = y >> 255
    y &= (1 << 255) - 1
    x = xrecover(y)
    if (x & 1) != sign:
        x = p - x
    return (x, y, 1, x * y % p)


def equal(P, Q):
    return (P[0] * Q[2] - Q[0] * P[2]) % p == 0 and (P[1] * Q[2] - Q[1] * P[2]) % p == 0


By = 4 * pow(5, p - 2, p) % p
Bx = xrecover(By)
Bx = p - Bx if Bx & 1 else Bx
B = (Bx, By, 1, Bx * By % p)


def verify(pk, msg, sig):
    R, S = decode(sig[:32]), int.from_bytes(sig[32:], "little")
    if S >= L:
        return False
    h = int.from_bytes(hashlib.sha512(sig[:32] + pk + msg).digest(), "little") % L
    return equal(mul(S, B), add(R, mul(h, decode(pk))))


def main():
    pk_raw = base64.b64decode(sys.argv[1])
    assert pk_raw[:2] == b"Ed"
    key_id, pk = pk_raw[2:10], pk_raw[10:]
    lines = open(sys.argv[3], encoding="utf-8").read().splitlines()
    sig_raw = base64.b64decode(lines[1])
    alg, sig_key_id, sig = sig_raw[:2], sig_raw[2:10], sig_raw[10:]
    assert sig_key_id == key_id, "signed by another key"
    data = open(sys.argv[2], "rb").read()
    msg = hashlib.blake2b(data).digest() if alg == b"ED" else data
    ok = verify(pk, msg, sig)
    trusted = lines[2].split("trusted comment: ", 1)[1].encode()
    ok_global = verify(pk, sig + trusted, base64.b64decode(lines[3]))
    print("file signature:", "OK" if ok else "BAD")
    print("trusted comment signature:", "OK" if ok_global else "BAD", "|", trusted.decode())
    sys.exit(0 if ok and ok_global else 1)


main()
