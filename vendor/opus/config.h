/* Minimal config for building the vendored libopus 1.5.2 (floating point, no SIMD, no DNN) from build.rs. */
#ifndef OPUS_CONFIG_H
#define OPUS_CONFIG_H
#define OPUS_BUILD 1
#define USE_ALLOCA 1
#define PACKAGE_VERSION "1.5.2"
#define HAVE_STDINT_H 1
#endif
