#!/usr/bin/env python3
"""Patch the fork chainspec's Sudo.Key to dev //Alice, in place (byte-level).

Sudo.Key storage key = twox_128("Sudo") || twox_128("Key").
Value is the 32-byte AccountId; //Alice and any mainnet sudo key are both
32 bytes (66 hex chars incl 0x), so this is a same-length in-place edit — no
11 GB rewrite. Idempotent: re-running when it's already Alice is a no-op.

Usage:  python3 set-sudo-alice.py [chainspec.json]   (default: cc3-mainnet-fork.json)
"""
import mmap
import sys

F = sys.argv[1] if len(sys.argv) > 1 else "cc3-mainnet-fork.json"
KEY = b'"0x5c0d1176a568c1f92944340dbfed9e9c530ebca703c85910e7164cb7d1c9e47b": "'
ALICE = b"0xd43593c715fdd31c61141abd04a99fd6822c8558854ccde39a5684e7a56da27d"
assert len(ALICE) == 66

with open(F, "r+b") as f:
    mm = mmap.mmap(f.fileno(), 0)
    k = mm.find(KEY)
    if k == -1:
        sys.exit("ERROR: Sudo.Key entry not found in " + F)
    if mm.find(KEY, k + 1) != -1:
        sys.exit("ERROR: Sudo.Key appears more than once")
    v = k + len(KEY)
    old = mm[v:v + 66]
    if old[:2] != b"0x" or len(old) != 66:
        sys.exit("ERROR: unexpected value shape at Sudo.Key: %r" % old)
    if old == ALICE:
        print("Sudo.Key already = //Alice, no change")
    else:
        mm[v:v + 66] = ALICE
        mm.flush()
        print("Sudo.Key %s -> //Alice (%s)" % (old.decode(), ALICE.decode()))
    mm.close()
