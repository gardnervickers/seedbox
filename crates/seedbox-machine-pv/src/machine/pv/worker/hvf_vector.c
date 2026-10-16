#include <Hypervisor/hv_vcpu.h>
#include <stdint.h>
#include <string.h>
// The SDK setter takes an arm64 C vector by value, not a Rust array struct.
hv_return_t seedbox_pv_set_vector(hv_vcpu_t cpu, hv_simd_fp_reg_t reg, const uint8_t *bytes) {
    hv_simd_fp_uchar16_t value;
    _Static_assert(sizeof(value) == 16, "HVF SIMD width");
    memcpy(&value, bytes, sizeof(value));
    return hv_vcpu_set_simd_fp_reg(cpu, reg, value);
}
