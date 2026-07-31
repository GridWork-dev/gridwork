# Conformance fixtures

Every vector in `stream-initial.hex` is **third-party and copied byte for
byte**. Nothing here was written for this repository, and nothing here should
be edited to make a test pass — a fixture adjusted to agree with our output has
stopped being evidence about anything.

They are stored hex-encoded rather than as the original files. The originals
are dense with `ESC` and `NUL`, and `tools/leak-scan.sh` refuses to pass a
repository holding tracked binaries it cannot read — correctly, and it is not a
gate worth weakening for test data. Hex is also the encoding
`docs/derivation/CAPTURES.md` already asks in-repo fixtures to use. Each line
carries `<name> <sha256-of-decoded-bytes> <hex>`, the runner checks the digest
after decoding, and the same digest appears beside that vector's golden frame,
so the encoding is verified rather than trusted.

## Where these came from

| | |
|---|---|
| Project | [ghostty](https://github.com/ghostty-org/ghostty) |
| Revision | `a887df42c56f6de86c0fe6da9c4eeca37931e083` (`GHOSTTY_COMMIT` in `../pins.env`) |
| Path | `test/fuzz-libghostty/corpus/stream-initial/` |
| Files | 24, unmodified (hex-encoded for storage, see above) |
| License | MIT — notice retained verbatim in [`LICENSE-ghostty`](LICENSE-ghostty) |

MIT requires its copyright and permission notice to travel with "all copies or
substantial portions of the Software", and 24 files copied byte for byte is not
obviously the small side of that line. Naming the license in a table is not the
same as discharging the condition it attaches, so the notice is vendored beside
the files it covers rather than cited from a distance. The repository's own
`LICENSE` at the root governs this crate; `LICENSE-ghostty` governs these
fixtures, and neither is a statement about the other.

These are the **seed** inputs for ghostty's own VT stream fuzzer, not fuzzer
output. The distinction is the whole reason this directory holds these and not
one of the neighbouring corpora: the `-cmin` directories are machine-minimised
mutations, while these were chosen by hand and named for what they exercise —
`03-csi-cursor-sgr`, `12-malformed-utf8`, `20-csi-subparams`, and so on.

The revision is the same one `pins.env` builds, so the fixtures and the parser
under test come from one tree rather than two that happen to be near each other.

## The first byte is not terminal input

`test/fuzz-libghostty/src/fuzz_stream.zig` reads byte 0 as a mode selector and
feeds only `input[1..]` to the parser:

```zig
// Use the first byte to decide between the scalar and slice paths
// so both code paths get exercised by the fuzzer.
const mode = input[0];
const data = input[1..];
if (mode & 1 == 0) { stream.nextSlice(data); }
else { for (data) |byte| stream.next(byte); }
```

So the corpus runner here strips byte 0 as well. Skipping that step would feed
a stray `NUL` or `SOH` to the parser ahead of every vector and pin frames for a
stream that upstream never tests.

It also means `01-plain-text-slice` and `02-plain-text-scalar` carry identical
terminal input and differ only in that selector byte. Both are kept rather than
deduplicated: they are upstream's files, and this directory is a copy, not a
curation.

## The geometry is upstream's too

The same harness builds its terminal at **80 × 24 with `max_scrollback = 100`**.
The runner uses those numbers so a frame captured here describes the same
terminal ghostty's own fuzzer drives. They are not ours to tune.

## What the golden frames are

`golden-frames.txt` holds, for each fixture, the plain-text screen after the
input has been parsed, plus the cursor position and a digest of the input that
produced it. They were captured once against the pinned toolchain and are
expected to stay byte-identical. A diff means the parser changed behaviour —
which is a fact worth knowing at the moment a pin moves, whether or not the new
behaviour is more correct.

Regenerate deliberately, never casually:

```
UPDATE_GOLDEN=1 cargo test -p gwk-pty --test conformance
```

Then read the diff. Committing a regenerated file without reading it converts
this gate into a rubber stamp.
