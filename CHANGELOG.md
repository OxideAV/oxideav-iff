# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `ilbm` module: read/round-trip support for FORM/ILBM (Amiga
  InterLeaved BitMap, 1986 Jerry Morrison spec). BMHD / CMAP / CAMG /
  BODY chunks; uncompressed and ByteRun1 (PackBits) BODY; 1..=8
  colour bitplanes; EHB (32→64-entry palette mirroring); HAM6 and HAM8
  with running-state channel modify ops; `Masking::HasMask` plane and
  `Masking::HasTransparentColor` alpha. Public API:
  `parse_ilbm` / `encode_ilbm` / `IlbmImage` / `Bmhd` / `Camg` /
  `byterun1_{decode,encode}_row` / `expand_ham_row` /
  `expand_ehb_palette`.
- New container registration `"iff_ilbm"` with `.ilbm` / `.lbm`
  extensions and a `FORM....ILBM` probe; demuxer emits one keyframe
  packet of RGBA pixels through a `rawvideo` stream.

## [0.0.5](https://github.com/OxideAV/oxideav-iff/compare/v0.0.4...v0.0.5) - 2026-04-25

### Other

- drop oxideav-codec/oxideav-container shims, import from oxideav-core

## [0.0.4](https://github.com/OxideAV/oxideav-iff/compare/v0.0.3...v0.0.4) - 2026-04-19

### Other

- bump oxideav-container dep to "0.1"
- drop Cargo.lock — this crate is a library
- bump oxideav-core / oxideav-codec dep examples to "0.1"
- bump to oxideav-core 0.1.1 + codec 0.1.1
- bump oxideav-core + oxideav-codec deps to "0.1"
- thread &dyn CodecResolver through open()
- iff / 8svx: round-trip (c) copyright chunk, rewrite README
