"""Deterministic source images for the oxiarc-jpeg corrupt-input fixtures."""
import sys

W, H = 24, 20


def pixel(x, y):
    return ((x * 7 + y * 13) % 256,
            255 if (x // 5 + y // 3) % 2 == 0 else 31,
            128)


rgb = bytearray()
gray = bytearray()
gray12 = bytearray()
for y in range(H):
    for x in range(W):
        r, g, b = pixel(x, y)
        rgb += bytes([r, g, b])
        gray.append(r)
        gray12 += ((r * 13) % 4096).to_bytes(2, 'big')

open(sys.argv[1] + '/src_rgb.ppm', 'wb').write(b'P6\n%d %d\n255\n' % (W, H) + bytes(rgb))
open(sys.argv[1] + '/src_gray.pgm', 'wb').write(b'P5\n%d %d\n255\n' % (W, H) + bytes(gray))
open(sys.argv[1] + '/src_gray12.pgm', 'wb').write(b'P5\n%d %d\n4095\n' % (W, H) + bytes(gray12))
