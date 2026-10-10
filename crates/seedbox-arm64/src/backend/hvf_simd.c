// Hypervisor's SIMD setter takes a C vector by value. A Rust byte-array struct
// has a different arm64 calling convention; keep that ABI conversion in C.
#include <Hypervisor/hv_vcpu.h>
#include <stdint.h>
#include <string.h>

hv_return_t seedbox_hvf_set_simd(hv_vcpu_t cpu, hv_simd_fp_reg_t reg,
                               const uint8_t *bytes) {
    hv_simd_fp_uchar16_t value;
    _Static_assert(sizeof(value) == 16, "unexpected SIMD register width");
    memcpy(&value, bytes, sizeof(value));
    return hv_vcpu_set_simd_fp_reg(cpu, reg, value);
}
