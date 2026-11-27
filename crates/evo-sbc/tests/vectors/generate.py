#!/usr/bin/env python3
# Copyright (c) 2026 Just a Nerd
# SPDX-License-Identifier: Apache-2.0
"""Regenerate the conformance vectors in this directory.

Needs `libsbc` (the reference SBC library from BlueZ) installed. It is used as the
test oracle only: it encodes a deterministic synthetic signal, then decodes the
frames again, and the pair is stored. Nothing here is built into, or shipped
with, the decoder.

    python3 generate.py [output-directory]

The vectors fall in three families: frames the reference *encoder* wrote from a synthetic
signal (every layout, the bitpool extremes, the shapes real phones send); and, prefixed
`crafted_`, well-formed frames no encoder wrote (random scale factors and audio under a
correct CRC), which reach allocation cases and clipping that real streams rarely do.

A vector file is: the magic `SBCV`, `u16` frame count, `u32` length of the
frames, `u32` length of the expected PCM (all little-endian), the frames, then
the expected 16-bit little-endian interleaved PCM the reference decoded them to.
"""
import ctypes, ctypes.util, math, os, random, struct, sys

L = ctypes.CDLL(ctypes.util.find_library('sbc') or 'libsbc.so.1')


class SBC(ctypes.Structure):
    _fields_ = [('flags', ctypes.c_ulong), ('frequency', ctypes.c_uint8), ('blocks', ctypes.c_uint8),
                ('subbands', ctypes.c_uint8), ('mode', ctypes.c_uint8), ('allocation', ctypes.c_uint8),
                ('bitpool', ctypes.c_uint8), ('endian', ctypes.c_uint8), ('priv', ctypes.c_void_p),
                ('priv_alloc_base', ctypes.c_void_p)]


P = ctypes.POINTER
L.sbc_init.argtypes = [P(SBC), ctypes.c_ulong]
L.sbc_encode.argtypes = [P(SBC), ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p, ctypes.c_size_t, P(ctypes.c_ssize_t)]
L.sbc_encode.restype = ctypes.c_ssize_t
L.sbc_decode.argtypes = [P(SBC), ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p, ctypes.c_size_t, P(ctypes.c_size_t)]
L.sbc_decode.restype = ctypes.c_ssize_t
L.sbc_get_codesize.argtypes = [P(SBC)]
L.sbc_get_codesize.restype = ctypes.c_size_t
L.sbc_finish.argtypes = [P(SBC)]
FREQ = {16000: 0, 32000: 1, 44100: 2, 48000: 3}
BLK = {4: 0, 8: 1, 12: 2, 16: 3}
MODE = {'mono': 0, 'dual': 1, 'stereo': 2, 'joint': 3}
ALLOC = {'loudness': 0, 'snr': 1}
SUB = {4: 0, 8: 1}


def new(freq, blocks, subbands, mode, alloc, bitpool):
    s = SBC()
    assert L.sbc_init(ctypes.byref(s), 0) == 0
    s.frequency, s.blocks, s.subbands = FREQ[freq], BLK[blocks], SUB[subbands]
    s.mode, s.allocation, s.bitpool, s.endian = MODE[mode], ALLOC[alloc], bitpool, 0
    return s


def encode(pcm, **cfg):
    s = new(**cfg)
    code = L.sbc_get_codesize(ctypes.byref(s))
    out, pos, buf, w = [], 0, ctypes.create_string_buffer(4096), ctypes.c_ssize_t()
    while pos + code <= len(pcm):
        assert L.sbc_encode(ctypes.byref(s), pcm[pos:pos + code], code, buf, 4096, ctypes.byref(w)) == code
        out.append(bytes(buf[:w.value]))
        pos += code
    L.sbc_finish(ctypes.byref(s))
    return out


def decode(frames):
    s = SBC()
    assert L.sbc_init(ctypes.byref(s), 0) == 0
    out, pos, buf, w = bytearray(), 0, ctypes.create_string_buffer(8192), ctypes.c_size_t()
    while pos < len(frames):
        n = L.sbc_decode(ctypes.byref(s), frames[pos:], len(frames) - pos, buf, 8192, ctypes.byref(w))
        assert n > 0
        out += buf[:w.value]
        pos += n
    L.sbc_finish(ctypes.byref(s))
    return bytes(out)


def signal(nsamples, chn, seed):
    """Three tones, light noise, a near-silent stretch and a loud, clipping one."""
    r = random.Random(seed)
    out = bytearray()
    f1, f2, f3 = r.uniform(80, 900), r.uniform(900, 4000), r.uniform(4000, 14000)
    a1, a2, a3 = r.uniform(3000, 9000), r.uniform(500, 4000), r.uniform(50, 1500)
    for n in range(nsamples):
        t = n / 44100.0
        for ch in range(chn):
            v = a1 * math.sin(2 * math.pi * f1 * t + ch) + a2 * math.sin(2 * math.pi * f2 * t * (1 + 0.01 * ch)) + a3 * math.sin(2 * math.pi * f3 * t)
            v += r.uniform(-60, 60)
            seg = (n // 512) % 8
            if seg == 3:
                v = r.uniform(-2, 2)
            if seg == 6:
                v = max(-32768, min(32767, v * 4))
            out += struct.pack('<h', int(max(-32768, min(32767, v))))
    return bytes(out)


def make(out_dir, name, freq, blocks, sub, mode, alloc, bitpool, nframes, seed):
    chn = 1 if mode == 'mono' else 2
    s = new(freq, blocks, sub, mode, alloc, bitpool)
    code = L.sbc_get_codesize(ctypes.byref(s))
    frames = encode(signal(nframes * code // (2 * chn), chn, seed), freq=freq, blocks=blocks, subbands=sub,
                    mode=mode, alloc=alloc, bitpool=bitpool)
    assert len(frames) == nframes
    sbc = b''.join(frames)
    pcm = decode(sbc)
    with open(os.path.join(out_dir, name + '.sbcv'), 'wb') as f:
        f.write(struct.pack('<4sHII', b'SBCV', nframes, len(sbc), len(pcm)) + sbc + pcm)


def crc8(bits):
    crc = 0x0F
    for bit in bits:
        top = ((crc >> 7) & 1) ^ bit
        crc = (crc << 1) & 0xFF
        if top:
            crc ^= 0x1D
    return crc


def frame_len(mode, sub, blocks, bitpool):
    scale = 4 * sub * (1 if mode == 'mono' else 2)
    joint = sub if mode == 'joint' else 0
    audio = blocks * bitpool * (2 if mode == 'dual' else 1)
    return 4 + (joint + scale + audio + 7) // 8      # flags, scale factors and audio: padded once


def crafted_frame(r, freq, blocks, sub, mode, alloc, bitpool, gentle):
    """A well-formed frame no encoder wrote: random joint flags, scale factors and audio bits
    under a correct CRC. It reaches allocation cases and clipping that real streams rarely do."""
    n = frame_len(mode, sub, blocks, bitpool)
    chn = 1 if mode == 'mono' else 2
    bits = []
    if mode == 'joint':
        bits += [r.randrange(2) for _ in range(sub)]
    for _ in range(sub * chn):
        v = r.randrange(0, 9) if gentle else r.randrange(0, 16)
        bits += [(v >> i) & 1 for i in (3, 2, 1, 0)]
    head_bits = len(bits)
    bits += [r.randrange(2) for _ in range(8 * (n - 4) - head_bits)]
    body = bytearray()
    for i in range(0, len(bits), 8):
        chunk = bits[i:i + 8] + [0] * (8 - len(bits[i:i + 8]))
        body.append(int(''.join(map(str, chunk)), 2))
    params = (FREQ[freq] << 6) | (BLK[blocks] << 4) | (MODE[mode] << 2) | (ALLOC[alloc] << 1) | SUB[sub]
    frame = bytearray([0x9C, params, bitpool, 0]) + body
    crc_bits = [(params >> i) & 1 for i in range(7, -1, -1)] + [(bitpool >> i) & 1 for i in range(7, -1, -1)] + bits[:head_bits]
    frame[3] = crc8(crc_bits)
    assert len(frame) == n
    return bytes(frame)


def make_crafted(out_dir, name, freq, blocks, sub, mode, alloc, bitpool, nframes, seed, gentle):
    r = random.Random(seed)
    sbc = b''.join(crafted_frame(r, freq, blocks, sub, mode, alloc, bitpool, gentle) for _ in range(nframes))
    pcm = decode(sbc)
    with open(os.path.join(out_dir, name + '.sbcv'), 'wb') as f:
        f.write(struct.pack('<4sHII', b'SBCV', nframes, len(sbc), len(pcm)) + sbc + pcm)


def main(out_dir):
    os.makedirs(out_dir, exist_ok=True)
    i, n = 0, 0
    for mode in ('mono', 'dual', 'stereo', 'joint'):          # every layout at 44.1 kHz
        for sub in (4, 8):
            for blocks in (4, 8, 12, 16):
                for alloc in ('loudness', 'snr'):
                    bp = (8, 20, 35, 53)[i % 4]
                    i += 1
                    make(out_dir, f'f44_{mode}_{sub}sb_{blocks}blk_{alloc}_bp{bp}', 44100, blocks, sub, mode, alloc, bp, 4, 1000 + i)
                    n += 1
    for freq in (16000, 32000, 48000):                         # the loudness offsets are per frequency
        for sub in (4, 8):
            for mode in ('mono', 'joint'):
                make(out_dir, f'f{freq // 1000}_{mode}_{sub}sb_16blk_loudness_bp32', freq, 16, sub, mode, 'loudness', 32, 4, 2000 + n)
                n += 1
    for mode in ('mono', 'dual', 'stereo', 'joint'):          # bitpool extremes
        for sub in (4, 8):
            make(out_dir, f'bp_min_{mode}_{sub}sb', 44100, 16, sub, mode, 'loudness', 2, 4, 3000 + n)
            n += 1
            top = min(250, 16 * sub * (2 if mode in ('stereo', 'joint') else 1))
            make(out_dir, f'bp_max_{mode}_{sub}sb', 44100, 16, sub, mode, 'loudness', top, 4, 3100 + n)
            n += 1
    for name, kw in (('a2dp_hq_48k_joint', dict(freq=48000, blocks=16, sub=8, mode='joint', alloc='loudness', bitpool=53)),
                     ('a2dp_xq_44k_joint', dict(freq=44100, blocks=16, sub=8, mode='joint', alloc='loudness', bitpool=76)),
                     ('a2dp_mq_44k_stereo', dict(freq=44100, blocks=16, sub=8, mode='stereo', alloc='loudness', bitpool=35))):
        make(out_dir, name, nframes=24, seed=4000 + n, **kw)
        n += 1
    # Frames no encoder wrote: every layout, a bitpool anywhere in its range, gentle and loud.
    r = random.Random(20261128)
    k = 0
    for mode in ('mono', 'dual', 'stereo', 'joint'):
        for sub in (4, 8):
            for blocks in (4, 8, 12, 16):
                for alloc in ('loudness', 'snr'):
                    top = min(250, 16 * sub * (2 if mode in ('stereo', 'joint') else 1))
                    bp = r.randrange(2, top + 1)
                    freq = (16000, 32000, 44100, 48000)[k % 4]
                    make_crafted(out_dir, f'crafted_{mode}_{sub}sb_{blocks}blk_{alloc}_bp{bp}', freq, blocks, sub, mode, alloc, bp,
                                 4, 5000 + k, gentle=(k % 2 == 0))
                    k += 1
                    n += 1
    print(n, 'vectors in', out_dir)


if __name__ == '__main__':
    main(sys.argv[1] if len(sys.argv) > 1 else os.path.dirname(os.path.abspath(__file__)))
