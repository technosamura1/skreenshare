libopus 1.5.2 (https://opus-codec.org/, BSD-3-Clause, see COPYING), trimmed: floating-point build without SIMD,
DNN, multistream and projection code. Compiled by ../../build.rs with the `cc` crate into the exe, so no extra DLL.
Update: replace celt/, silk/, src/, include/ from a new release tarball (file lists: celt_sources.mk, silk_sources.mk).
