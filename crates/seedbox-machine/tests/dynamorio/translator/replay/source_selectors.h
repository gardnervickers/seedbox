/* Public A64 selector names from DR 356c1821; see provenance.txt. */
static const char *const source_register_names[] = {
    "DR_REG_NULL",
    "DR_REG_INVALID",
    "DR_REG_X0",
    "DR_REG_X1",
    "DR_REG_X2",
    "DR_REG_X3",
    "DR_REG_X4",
    "DR_REG_X5",
    "DR_REG_X6",
    "DR_REG_X7",
    "DR_REG_X8",
    "DR_REG_X9",
    "DR_REG_X10",
    "DR_REG_X11",
    "DR_REG_X12",
    "DR_REG_X13",
    "DR_REG_X14",
    "DR_REG_X15",
    "DR_REG_X16",
    "DR_REG_X17",
    "DR_REG_X18",
    "DR_REG_X19",
    "DR_REG_X20",
    "DR_REG_X21",
    "DR_REG_X22",
    "DR_REG_X23",
    "DR_REG_X24",
    "DR_REG_X25",
    "DR_REG_X26",
    "DR_REG_X27",
    "DR_REG_X28",
    "DR_REG_X29",
    "DR_REG_X30",
    "DR_REG_XSP",
    "DR_REG_XZR",
    "DR_REG_W0",
    "DR_REG_W1",
    "DR_REG_W2",
    "DR_REG_W3",
    "DR_REG_W4",
    "DR_REG_W5",
    "DR_REG_W6",
    "DR_REG_W7",
    "DR_REG_W8",
    "DR_REG_W9",
    "DR_REG_W10",
    "DR_REG_W11",
    "DR_REG_W12",
    "DR_REG_W13",
    "DR_REG_W14",
    "DR_REG_W15",
    "DR_REG_W16",
    "DR_REG_W17",
    "DR_REG_W18",
    "DR_REG_W19",
    "DR_REG_W20",
    "DR_REG_W21",
    "DR_REG_W22",
    "DR_REG_W23",
    "DR_REG_W24",
    "DR_REG_W25",
    "DR_REG_W26",
    "DR_REG_W27",
    "DR_REG_W28",
    "DR_REG_W29",
    "DR_REG_W30",
    "DR_REG_WSP",
    "DR_REG_WZR",
    "DR_REG_Z0",
    "DR_REG_Z1",
    "DR_REG_Z2",
    "DR_REG_Z3",
    "DR_REG_Z4",
    "DR_REG_Z5",
    "DR_REG_Z6",
    "DR_REG_Z7",
    "DR_REG_Z8",
    "DR_REG_Z9",
    "DR_REG_Z10",
    "DR_REG_Z11",
    "DR_REG_Z12",
    "DR_REG_Z13",
    "DR_REG_Z14",
    "DR_REG_Z15",
    "DR_REG_Z16",
    "DR_REG_Z17",
    "DR_REG_Z18",
    "DR_REG_Z19",
    "DR_REG_Z20",
    "DR_REG_Z21",
    "DR_REG_Z22",
    "DR_REG_Z23",
    "DR_REG_Z24",
    "DR_REG_Z25",
    "DR_REG_Z26",
    "DR_REG_Z27",
    "DR_REG_Z28",
    "DR_REG_Z29",
    "DR_REG_Z30",
    "DR_REG_Z31",
    "DR_REG_Q0",
    "DR_REG_Q1",
    "DR_REG_Q2",
    "DR_REG_Q3",
    "DR_REG_Q4",
    "DR_REG_Q5",
    "DR_REG_Q6",
    "DR_REG_Q7",
    "DR_REG_Q8",
    "DR_REG_Q9",
    "DR_REG_Q10",
    "DR_REG_Q11",
    "DR_REG_Q12",
    "DR_REG_Q13",
    "DR_REG_Q14",
    "DR_REG_Q15",
    "DR_REG_Q16",
    "DR_REG_Q17",
    "DR_REG_Q18",
    "DR_REG_Q19",
    "DR_REG_Q20",
    "DR_REG_Q21",
    "DR_REG_Q22",
    "DR_REG_Q23",
    "DR_REG_Q24",
    "DR_REG_Q25",
    "DR_REG_Q26",
    "DR_REG_Q27",
    "DR_REG_Q28",
    "DR_REG_Q29",
    "DR_REG_Q30",
    "DR_REG_Q31",
    "DR_REG_D0",
    "DR_REG_D1",
    "DR_REG_D2",
    "DR_REG_D3",
    "DR_REG_D4",
    "DR_REG_D5",
    "DR_REG_D6",
    "DR_REG_D7",
    "DR_REG_D8",
    "DR_REG_D9",
    "DR_REG_D10",
    "DR_REG_D11",
    "DR_REG_D12",
    "DR_REG_D13",
    "DR_REG_D14",
    "DR_REG_D15",
    "DR_REG_D16",
    "DR_REG_D17",
    "DR_REG_D18",
    "DR_REG_D19",
    "DR_REG_D20",
    "DR_REG_D21",
    "DR_REG_D22",
    "DR_REG_D23",
    "DR_REG_D24",
    "DR_REG_D25",
    "DR_REG_D26",
    "DR_REG_D27",
    "DR_REG_D28",
    "DR_REG_D29",
    "DR_REG_D30",
    "DR_REG_D31",
    "DR_REG_S0",
    "DR_REG_S1",
    "DR_REG_S2",
    "DR_REG_S3",
    "DR_REG_S4",
    "DR_REG_S5",
    "DR_REG_S6",
    "DR_REG_S7",
    "DR_REG_S8",
    "DR_REG_S9",
    "DR_REG_S10",
    "DR_REG_S11",
    "DR_REG_S12",
    "DR_REG_S13",
    "DR_REG_S14",
    "DR_REG_S15",
    "DR_REG_S16",
    "DR_REG_S17",
    "DR_REG_S18",
    "DR_REG_S19",
    "DR_REG_S20",
    "DR_REG_S21",
    "DR_REG_S22",
    "DR_REG_S23",
    "DR_REG_S24",
    "DR_REG_S25",
    "DR_REG_S26",
    "DR_REG_S27",
    "DR_REG_S28",
    "DR_REG_S29",
    "DR_REG_S30",
    "DR_REG_S31",
    "DR_REG_H0",
    "DR_REG_H1",
    "DR_REG_H2",
    "DR_REG_H3",
    "DR_REG_H4",
    "DR_REG_H5",
    "DR_REG_H6",
    "DR_REG_H7",
    "DR_REG_H8",
    "DR_REG_H9",
    "DR_REG_H10",
    "DR_REG_H11",
    "DR_REG_H12",
    "DR_REG_H13",
    "DR_REG_H14",
    "DR_REG_H15",
    "DR_REG_H16",
    "DR_REG_H17",
    "DR_REG_H18",
    "DR_REG_H19",
    "DR_REG_H20",
    "DR_REG_H21",
    "DR_REG_H22",
    "DR_REG_H23",
    "DR_REG_H24",
    "DR_REG_H25",
    "DR_REG_H26",
    "DR_REG_H27",
    "DR_REG_H28",
    "DR_REG_H29",
    "DR_REG_H30",
    "DR_REG_H31",
    "DR_REG_B0",
    "DR_REG_B1",
    "DR_REG_B2",
    "DR_REG_B3",
    "DR_REG_B4",
    "DR_REG_B5",
    "DR_REG_B6",
    "DR_REG_B7",
    "DR_REG_B8",
    "DR_REG_B9",
    "DR_REG_B10",
    "DR_REG_B11",
    "DR_REG_B12",
    "DR_REG_B13",
    "DR_REG_B14",
    "DR_REG_B15",
    "DR_REG_B16",
    "DR_REG_B17",
    "DR_REG_B18",
    "DR_REG_B19",
    "DR_REG_B20",
    "DR_REG_B21",
    "DR_REG_B22",
    "DR_REG_B23",
    "DR_REG_B24",
    "DR_REG_B25",
    "DR_REG_B26",
    "DR_REG_B27",
    "DR_REG_B28",
    "DR_REG_B29",
    "DR_REG_B30",
    "DR_REG_B31",
    "DR_REG_NZCV",
    "DR_REG_FPCR",
    "DR_REG_FPSR",
    "DR_REG_MDCCSR_EL0",
    "DR_REG_DBGDTR_EL0",
    "DR_REG_DBGDTRRX_EL0",
    "DR_REG_SP_EL0",
    "DR_REG_SPSEL",
    "DR_REG_DAIFSET",
    "DR_REG_DAIFCLR",
    "DR_REG_CURRENTEL",
    "DR_REG_PAN",
    "DR_REG_UAO",
    "DR_REG_CTR_EL0",
    "DR_REG_DCZID_EL0",
    "DR_REG_RNDR",
    "DR_REG_RNDRRS",
    "DR_REG_DAIF",
    "DR_REG_DIT",
    "DR_REG_SSBS",
    "DR_REG_TCO",
    "DR_REG_DSPSR_EL0",
    "DR_REG_DLR_EL0",
    "DR_REG_PMCR_EL0",
    "DR_REG_PMCNTENSET_EL0",
    "DR_REG_PMCNTENCLR_EL0",
    "DR_REG_PMOVSCLR_EL0",
    "DR_REG_PMSWINC_EL0",
    "DR_REG_PMSELR_EL0",
    "DR_REG_PMCEID0_EL0",
    "DR_REG_PMCEID1_EL0",
    "DR_REG_PMCCNTR_EL0",
    "DR_REG_PMXEVTYPER_EL0",
    "DR_REG_PMXEVCNTR_EL0",
    "DR_REG_PMUSERENR_EL0",
    "DR_REG_PMOVSSET_EL0",
    "DR_REG_SCXTNUM_EL0",
    "DR_REG_CNTFRQ_EL0",
    "DR_REG_CNTPCT_EL0",
    "DR_REG_CNTP_TVAL_EL0",
    "DR_REG_CNTP_CTL_EL0",
    "DR_REG_CNTP_CVAL_EL0",
    "DR_REG_CNTV_TVAL_EL0",
    "DR_REG_CNTV_CTL_EL0",
    "DR_REG_CNTV_CVAL_EL0",
    "DR_REG_PMEVCNTR0_EL0",
    "DR_REG_PMEVCNTR1_EL0",
    "DR_REG_PMEVCNTR2_EL0",
    "DR_REG_PMEVCNTR3_EL0",
    "DR_REG_PMEVCNTR4_EL0",
    "DR_REG_PMEVCNTR5_EL0",
    "DR_REG_PMEVCNTR6_EL0",
    "DR_REG_PMEVCNTR7_EL0",
    "DR_REG_PMEVCNTR8_EL0",
    "DR_REG_PMEVCNTR9_EL0",
    "DR_REG_PMEVCNTR10_EL0",
    "DR_REG_PMEVCNTR11_EL0",
    "DR_REG_PMEVCNTR12_EL0",
    "DR_REG_PMEVCNTR13_EL0",
    "DR_REG_PMEVCNTR14_EL0",
    "DR_REG_PMEVCNTR15_EL0",
    "DR_REG_PMEVCNTR16_EL0",
    "DR_REG_PMEVCNTR17_EL0",
    "DR_REG_PMEVCNTR18_EL0",
    "DR_REG_PMEVCNTR19_EL0",
    "DR_REG_PMEVCNTR20_EL0",
    "DR_REG_PMEVCNTR21_EL0",
    "DR_REG_PMEVCNTR22_EL0",
    "DR_REG_PMEVCNTR23_EL0",
    "DR_REG_PMEVCNTR24_EL0",
    "DR_REG_PMEVCNTR25_EL0",
    "DR_REG_PMEVCNTR26_EL0",
    "DR_REG_PMEVCNTR27_EL0",
    "DR_REG_PMEVCNTR28_EL0",
    "DR_REG_PMEVCNTR29_EL0",
    "DR_REG_PMEVCNTR30_EL0",
    "DR_REG_PMEVTYPER0_EL0",
    "DR_REG_PMEVTYPER1_EL0",
    "DR_REG_PMEVTYPER2_EL0",
    "DR_REG_PMEVTYPER3_EL0",
    "DR_REG_PMEVTYPER4_EL0",
    "DR_REG_PMEVTYPER5_EL0",
    "DR_REG_PMEVTYPER6_EL0",
    "DR_REG_PMEVTYPER7_EL0",
    "DR_REG_PMEVTYPER8_EL0",
    "DR_REG_PMEVTYPER9_EL0",
    "DR_REG_PMEVTYPER10_EL0",
    "DR_REG_PMEVTYPER11_EL0",
    "DR_REG_PMEVTYPER12_EL0",
    "DR_REG_PMEVTYPER13_EL0",
    "DR_REG_PMEVTYPER14_EL0",
    "DR_REG_PMEVTYPER15_EL0",
    "DR_REG_PMEVTYPER16_EL0",
    "DR_REG_PMEVTYPER17_EL0",
    "DR_REG_PMEVTYPER18_EL0",
    "DR_REG_PMEVTYPER19_EL0",
    "DR_REG_PMEVTYPER20_EL0",
    "DR_REG_PMEVTYPER21_EL0",
    "DR_REG_PMEVTYPER22_EL0",
    "DR_REG_PMEVTYPER23_EL0",
    "DR_REG_PMEVTYPER24_EL0",
    "DR_REG_PMEVTYPER25_EL0",
    "DR_REG_PMEVTYPER26_EL0",
    "DR_REG_PMEVTYPER27_EL0",
    "DR_REG_PMEVTYPER28_EL0",
    "DR_REG_PMEVTYPER29_EL0",
    "DR_REG_PMEVTYPER30_EL0",
    "DR_REG_PMCCFILTR_EL0",
    "DR_REG_SPSR_IRQ",
    "DR_REG_SPSR_ABT",
    "DR_REG_SPSR_UND",
    "DR_REG_SPSR_FIQ",
    "DR_REG_TPIDRURW",
    "DR_REG_TPIDRURO",
    "DR_REG_P0",
    "DR_REG_P1",
    "DR_REG_P2",
    "DR_REG_P3",
    "DR_REG_P4",
    "DR_REG_P5",
    "DR_REG_P6",
    "DR_REG_P7",
    "DR_REG_P8",
    "DR_REG_P9",
    "DR_REG_P10",
    "DR_REG_P11",
    "DR_REG_P12",
    "DR_REG_P13",
    "DR_REG_P14",
    "DR_REG_P15",
    "DR_REG_FFR",
    "DR_REG_CNTVCT_EL0",
    "DR_REG_ID_AA64ISAR0_EL1",
    "DR_REG_ID_AA64ISAR1_EL1",
    "DR_REG_ID_AA64ISAR2_EL1",
    "DR_REG_ID_AA64PFR0_EL1",
    "DR_REG_ID_AA64MMFR1_EL1",
    "DR_REG_ID_AA64DFR0_EL1",
    "DR_REG_ID_AA64ZFR0_EL1",
    "DR_REG_ID_AA64PFR1_EL1",
    "DR_REG_ID_AA64MMFR2_EL1",
    "DR_REG_MIDR_EL1",
    "DR_REG_MPIDR_EL1",
    "DR_REG_REVIDR_EL1",
    "DR_REG_FPMR",
    "DR_REG_CONTEXTIDR_EL1",
    "DR_REG_ELR_EL1",
    "DR_REG_SPSR_EL1",
    "DR_REG_TPIDR_EL1",
    "DR_REG_ACCDATA_EL1",
};
static const int source_register_ids[] = {
    DR_REG_NULL, /* 0: DR_REG_NULL */
    DR_REG_INVALID, /* 1: DR_REG_INVALID */
    DR_REG_X0, /* 2: DR_REG_X0 */
    DR_REG_X1, /* 3: DR_REG_X1 */
    DR_REG_X2, /* 4: DR_REG_X2 */
    DR_REG_X3, /* 5: DR_REG_X3 */
    DR_REG_X4, /* 6: DR_REG_X4 */
    DR_REG_X5, /* 7: DR_REG_X5 */
    DR_REG_X6, /* 8: DR_REG_X6 */
    DR_REG_X7, /* 9: DR_REG_X7 */
    DR_REG_X8, /* 10: DR_REG_X8 */
    DR_REG_X9, /* 11: DR_REG_X9 */
    DR_REG_X10, /* 12: DR_REG_X10 */
    DR_REG_X11, /* 13: DR_REG_X11 */
    DR_REG_X12, /* 14: DR_REG_X12 */
    DR_REG_X13, /* 15: DR_REG_X13 */
    DR_REG_X14, /* 16: DR_REG_X14 */
    DR_REG_X15, /* 17: DR_REG_X15 */
    DR_REG_X16, /* 18: DR_REG_X16 */
    DR_REG_X17, /* 19: DR_REG_X17 */
    DR_REG_X18, /* 20: DR_REG_X18 */
    DR_REG_X19, /* 21: DR_REG_X19 */
    DR_REG_X20, /* 22: DR_REG_X20 */
    DR_REG_X21, /* 23: DR_REG_X21 */
    DR_REG_X22, /* 24: DR_REG_X22 */
    DR_REG_X23, /* 25: DR_REG_X23 */
    DR_REG_X24, /* 26: DR_REG_X24 */
    DR_REG_X25, /* 27: DR_REG_X25 */
    DR_REG_X26, /* 28: DR_REG_X26 */
    DR_REG_X27, /* 29: DR_REG_X27 */
    DR_REG_X28, /* 30: DR_REG_X28 */
    DR_REG_X29, /* 31: DR_REG_X29 */
    DR_REG_X30, /* 32: DR_REG_X30 */
    DR_REG_XSP, /* 33: DR_REG_XSP */
    DR_REG_XZR, /* 34: DR_REG_XZR */
    DR_REG_W0, /* 35: DR_REG_W0 */
    DR_REG_W1, /* 36: DR_REG_W1 */
    DR_REG_W2, /* 37: DR_REG_W2 */
    DR_REG_W3, /* 38: DR_REG_W3 */
    DR_REG_W4, /* 39: DR_REG_W4 */
    DR_REG_W5, /* 40: DR_REG_W5 */
    DR_REG_W6, /* 41: DR_REG_W6 */
    DR_REG_W7, /* 42: DR_REG_W7 */
    DR_REG_W8, /* 43: DR_REG_W8 */
    DR_REG_W9, /* 44: DR_REG_W9 */
    DR_REG_W10, /* 45: DR_REG_W10 */
    DR_REG_W11, /* 46: DR_REG_W11 */
    DR_REG_W12, /* 47: DR_REG_W12 */
    DR_REG_W13, /* 48: DR_REG_W13 */
    DR_REG_W14, /* 49: DR_REG_W14 */
    DR_REG_W15, /* 50: DR_REG_W15 */
    DR_REG_W16, /* 51: DR_REG_W16 */
    DR_REG_W17, /* 52: DR_REG_W17 */
    DR_REG_W18, /* 53: DR_REG_W18 */
    DR_REG_W19, /* 54: DR_REG_W19 */
    DR_REG_W20, /* 55: DR_REG_W20 */
    DR_REG_W21, /* 56: DR_REG_W21 */
    DR_REG_W22, /* 57: DR_REG_W22 */
    DR_REG_W23, /* 58: DR_REG_W23 */
    DR_REG_W24, /* 59: DR_REG_W24 */
    DR_REG_W25, /* 60: DR_REG_W25 */
    DR_REG_W26, /* 61: DR_REG_W26 */
    DR_REG_W27, /* 62: DR_REG_W27 */
    DR_REG_W28, /* 63: DR_REG_W28 */
    DR_REG_W29, /* 64: DR_REG_W29 */
    DR_REG_W30, /* 65: DR_REG_W30 */
    DR_REG_WSP, /* 66: DR_REG_WSP */
    DR_REG_WZR, /* 67: DR_REG_WZR */
    DR_REG_Z0, /* 68: DR_REG_Z0 */
    DR_REG_Z1, /* 69: DR_REG_Z1 */
    DR_REG_Z2, /* 70: DR_REG_Z2 */
    DR_REG_Z3, /* 71: DR_REG_Z3 */
    DR_REG_Z4, /* 72: DR_REG_Z4 */
    DR_REG_Z5, /* 73: DR_REG_Z5 */
    DR_REG_Z6, /* 74: DR_REG_Z6 */
    DR_REG_Z7, /* 75: DR_REG_Z7 */
    DR_REG_Z8, /* 76: DR_REG_Z8 */
    DR_REG_Z9, /* 77: DR_REG_Z9 */
    DR_REG_Z10, /* 78: DR_REG_Z10 */
    DR_REG_Z11, /* 79: DR_REG_Z11 */
    DR_REG_Z12, /* 80: DR_REG_Z12 */
    DR_REG_Z13, /* 81: DR_REG_Z13 */
    DR_REG_Z14, /* 82: DR_REG_Z14 */
    DR_REG_Z15, /* 83: DR_REG_Z15 */
    DR_REG_Z16, /* 84: DR_REG_Z16 */
    DR_REG_Z17, /* 85: DR_REG_Z17 */
    DR_REG_Z18, /* 86: DR_REG_Z18 */
    DR_REG_Z19, /* 87: DR_REG_Z19 */
    DR_REG_Z20, /* 88: DR_REG_Z20 */
    DR_REG_Z21, /* 89: DR_REG_Z21 */
    DR_REG_Z22, /* 90: DR_REG_Z22 */
    DR_REG_Z23, /* 91: DR_REG_Z23 */
    DR_REG_Z24, /* 92: DR_REG_Z24 */
    DR_REG_Z25, /* 93: DR_REG_Z25 */
    DR_REG_Z26, /* 94: DR_REG_Z26 */
    DR_REG_Z27, /* 95: DR_REG_Z27 */
    DR_REG_Z28, /* 96: DR_REG_Z28 */
    DR_REG_Z29, /* 97: DR_REG_Z29 */
    DR_REG_Z30, /* 98: DR_REG_Z30 */
    DR_REG_Z31, /* 99: DR_REG_Z31 */
    DR_REG_Q0, /* 100: DR_REG_Q0 */
    DR_REG_Q1, /* 101: DR_REG_Q1 */
    DR_REG_Q2, /* 102: DR_REG_Q2 */
    DR_REG_Q3, /* 103: DR_REG_Q3 */
    DR_REG_Q4, /* 104: DR_REG_Q4 */
    DR_REG_Q5, /* 105: DR_REG_Q5 */
    DR_REG_Q6, /* 106: DR_REG_Q6 */
    DR_REG_Q7, /* 107: DR_REG_Q7 */
    DR_REG_Q8, /* 108: DR_REG_Q8 */
    DR_REG_Q9, /* 109: DR_REG_Q9 */
    DR_REG_Q10, /* 110: DR_REG_Q10 */
    DR_REG_Q11, /* 111: DR_REG_Q11 */
    DR_REG_Q12, /* 112: DR_REG_Q12 */
    DR_REG_Q13, /* 113: DR_REG_Q13 */
    DR_REG_Q14, /* 114: DR_REG_Q14 */
    DR_REG_Q15, /* 115: DR_REG_Q15 */
    DR_REG_Q16, /* 116: DR_REG_Q16 */
    DR_REG_Q17, /* 117: DR_REG_Q17 */
    DR_REG_Q18, /* 118: DR_REG_Q18 */
    DR_REG_Q19, /* 119: DR_REG_Q19 */
    DR_REG_Q20, /* 120: DR_REG_Q20 */
    DR_REG_Q21, /* 121: DR_REG_Q21 */
    DR_REG_Q22, /* 122: DR_REG_Q22 */
    DR_REG_Q23, /* 123: DR_REG_Q23 */
    DR_REG_Q24, /* 124: DR_REG_Q24 */
    DR_REG_Q25, /* 125: DR_REG_Q25 */
    DR_REG_Q26, /* 126: DR_REG_Q26 */
    DR_REG_Q27, /* 127: DR_REG_Q27 */
    DR_REG_Q28, /* 128: DR_REG_Q28 */
    DR_REG_Q29, /* 129: DR_REG_Q29 */
    DR_REG_Q30, /* 130: DR_REG_Q30 */
    DR_REG_Q31, /* 131: DR_REG_Q31 */
    DR_REG_D0, /* 132: DR_REG_D0 */
    DR_REG_D1, /* 133: DR_REG_D1 */
    DR_REG_D2, /* 134: DR_REG_D2 */
    DR_REG_D3, /* 135: DR_REG_D3 */
    DR_REG_D4, /* 136: DR_REG_D4 */
    DR_REG_D5, /* 137: DR_REG_D5 */
    DR_REG_D6, /* 138: DR_REG_D6 */
    DR_REG_D7, /* 139: DR_REG_D7 */
    DR_REG_D8, /* 140: DR_REG_D8 */
    DR_REG_D9, /* 141: DR_REG_D9 */
    DR_REG_D10, /* 142: DR_REG_D10 */
    DR_REG_D11, /* 143: DR_REG_D11 */
    DR_REG_D12, /* 144: DR_REG_D12 */
    DR_REG_D13, /* 145: DR_REG_D13 */
    DR_REG_D14, /* 146: DR_REG_D14 */
    DR_REG_D15, /* 147: DR_REG_D15 */
    DR_REG_D16, /* 148: DR_REG_D16 */
    DR_REG_D17, /* 149: DR_REG_D17 */
    DR_REG_D18, /* 150: DR_REG_D18 */
    DR_REG_D19, /* 151: DR_REG_D19 */
    DR_REG_D20, /* 152: DR_REG_D20 */
    DR_REG_D21, /* 153: DR_REG_D21 */
    DR_REG_D22, /* 154: DR_REG_D22 */
    DR_REG_D23, /* 155: DR_REG_D23 */
    DR_REG_D24, /* 156: DR_REG_D24 */
    DR_REG_D25, /* 157: DR_REG_D25 */
    DR_REG_D26, /* 158: DR_REG_D26 */
    DR_REG_D27, /* 159: DR_REG_D27 */
    DR_REG_D28, /* 160: DR_REG_D28 */
    DR_REG_D29, /* 161: DR_REG_D29 */
    DR_REG_D30, /* 162: DR_REG_D30 */
    DR_REG_D31, /* 163: DR_REG_D31 */
    DR_REG_S0, /* 164: DR_REG_S0 */
    DR_REG_S1, /* 165: DR_REG_S1 */
    DR_REG_S2, /* 166: DR_REG_S2 */
    DR_REG_S3, /* 167: DR_REG_S3 */
    DR_REG_S4, /* 168: DR_REG_S4 */
    DR_REG_S5, /* 169: DR_REG_S5 */
    DR_REG_S6, /* 170: DR_REG_S6 */
    DR_REG_S7, /* 171: DR_REG_S7 */
    DR_REG_S8, /* 172: DR_REG_S8 */
    DR_REG_S9, /* 173: DR_REG_S9 */
    DR_REG_S10, /* 174: DR_REG_S10 */
    DR_REG_S11, /* 175: DR_REG_S11 */
    DR_REG_S12, /* 176: DR_REG_S12 */
    DR_REG_S13, /* 177: DR_REG_S13 */
    DR_REG_S14, /* 178: DR_REG_S14 */
    DR_REG_S15, /* 179: DR_REG_S15 */
    DR_REG_S16, /* 180: DR_REG_S16 */
    DR_REG_S17, /* 181: DR_REG_S17 */
    DR_REG_S18, /* 182: DR_REG_S18 */
    DR_REG_S19, /* 183: DR_REG_S19 */
    DR_REG_S20, /* 184: DR_REG_S20 */
    DR_REG_S21, /* 185: DR_REG_S21 */
    DR_REG_S22, /* 186: DR_REG_S22 */
    DR_REG_S23, /* 187: DR_REG_S23 */
    DR_REG_S24, /* 188: DR_REG_S24 */
    DR_REG_S25, /* 189: DR_REG_S25 */
    DR_REG_S26, /* 190: DR_REG_S26 */
    DR_REG_S27, /* 191: DR_REG_S27 */
    DR_REG_S28, /* 192: DR_REG_S28 */
    DR_REG_S29, /* 193: DR_REG_S29 */
    DR_REG_S30, /* 194: DR_REG_S30 */
    DR_REG_S31, /* 195: DR_REG_S31 */
    DR_REG_H0, /* 196: DR_REG_H0 */
    DR_REG_H1, /* 197: DR_REG_H1 */
    DR_REG_H2, /* 198: DR_REG_H2 */
    DR_REG_H3, /* 199: DR_REG_H3 */
    DR_REG_H4, /* 200: DR_REG_H4 */
    DR_REG_H5, /* 201: DR_REG_H5 */
    DR_REG_H6, /* 202: DR_REG_H6 */
    DR_REG_H7, /* 203: DR_REG_H7 */
    DR_REG_H8, /* 204: DR_REG_H8 */
    DR_REG_H9, /* 205: DR_REG_H9 */
    DR_REG_H10, /* 206: DR_REG_H10 */
    DR_REG_H11, /* 207: DR_REG_H11 */
    DR_REG_H12, /* 208: DR_REG_H12 */
    DR_REG_H13, /* 209: DR_REG_H13 */
    DR_REG_H14, /* 210: DR_REG_H14 */
    DR_REG_H15, /* 211: DR_REG_H15 */
    DR_REG_H16, /* 212: DR_REG_H16 */
    DR_REG_H17, /* 213: DR_REG_H17 */
    DR_REG_H18, /* 214: DR_REG_H18 */
    DR_REG_H19, /* 215: DR_REG_H19 */
    DR_REG_H20, /* 216: DR_REG_H20 */
    DR_REG_H21, /* 217: DR_REG_H21 */
    DR_REG_H22, /* 218: DR_REG_H22 */
    DR_REG_H23, /* 219: DR_REG_H23 */
    DR_REG_H24, /* 220: DR_REG_H24 */
    DR_REG_H25, /* 221: DR_REG_H25 */
    DR_REG_H26, /* 222: DR_REG_H26 */
    DR_REG_H27, /* 223: DR_REG_H27 */
    DR_REG_H28, /* 224: DR_REG_H28 */
    DR_REG_H29, /* 225: DR_REG_H29 */
    DR_REG_H30, /* 226: DR_REG_H30 */
    DR_REG_H31, /* 227: DR_REG_H31 */
    DR_REG_B0, /* 228: DR_REG_B0 */
    DR_REG_B1, /* 229: DR_REG_B1 */
    DR_REG_B2, /* 230: DR_REG_B2 */
    DR_REG_B3, /* 231: DR_REG_B3 */
    DR_REG_B4, /* 232: DR_REG_B4 */
    DR_REG_B5, /* 233: DR_REG_B5 */
    DR_REG_B6, /* 234: DR_REG_B6 */
    DR_REG_B7, /* 235: DR_REG_B7 */
    DR_REG_B8, /* 236: DR_REG_B8 */
    DR_REG_B9, /* 237: DR_REG_B9 */
    DR_REG_B10, /* 238: DR_REG_B10 */
    DR_REG_B11, /* 239: DR_REG_B11 */
    DR_REG_B12, /* 240: DR_REG_B12 */
    DR_REG_B13, /* 241: DR_REG_B13 */
    DR_REG_B14, /* 242: DR_REG_B14 */
    DR_REG_B15, /* 243: DR_REG_B15 */
    DR_REG_B16, /* 244: DR_REG_B16 */
    DR_REG_B17, /* 245: DR_REG_B17 */
    DR_REG_B18, /* 246: DR_REG_B18 */
    DR_REG_B19, /* 247: DR_REG_B19 */
    DR_REG_B20, /* 248: DR_REG_B20 */
    DR_REG_B21, /* 249: DR_REG_B21 */
    DR_REG_B22, /* 250: DR_REG_B22 */
    DR_REG_B23, /* 251: DR_REG_B23 */
    DR_REG_B24, /* 252: DR_REG_B24 */
    DR_REG_B25, /* 253: DR_REG_B25 */
    DR_REG_B26, /* 254: DR_REG_B26 */
    DR_REG_B27, /* 255: DR_REG_B27 */
    DR_REG_B28, /* 256: DR_REG_B28 */
    DR_REG_B29, /* 257: DR_REG_B29 */
    DR_REG_B30, /* 258: DR_REG_B30 */
    DR_REG_B31, /* 259: DR_REG_B31 */
    DR_REG_NZCV, /* 260: DR_REG_NZCV */
    DR_REG_FPCR, /* 261: DR_REG_FPCR */
    DR_REG_FPSR, /* 262: DR_REG_FPSR */
    DR_REG_MDCCSR_EL0, /* 263: DR_REG_MDCCSR_EL0 */
    DR_REG_DBGDTR_EL0, /* 264: DR_REG_DBGDTR_EL0 */
    DR_REG_DBGDTRRX_EL0, /* 265: DR_REG_DBGDTRRX_EL0 */
    DR_REG_SP_EL0, /* 266: DR_REG_SP_EL0 */
    DR_REG_SPSEL, /* 267: DR_REG_SPSEL */
    DR_REG_DAIFSET, /* 268: DR_REG_DAIFSET */
    DR_REG_DAIFCLR, /* 269: DR_REG_DAIFCLR */
    DR_REG_CURRENTEL, /* 270: DR_REG_CURRENTEL */
    DR_REG_PAN, /* 271: DR_REG_PAN */
    DR_REG_UAO, /* 272: DR_REG_UAO */
    DR_REG_CTR_EL0, /* 273: DR_REG_CTR_EL0 */
    DR_REG_DCZID_EL0, /* 274: DR_REG_DCZID_EL0 */
    DR_REG_RNDR, /* 275: DR_REG_RNDR */
    DR_REG_RNDRRS, /* 276: DR_REG_RNDRRS */
    DR_REG_DAIF, /* 277: DR_REG_DAIF */
    DR_REG_DIT, /* 278: DR_REG_DIT */
    DR_REG_SSBS, /* 279: DR_REG_SSBS */
    DR_REG_TCO, /* 280: DR_REG_TCO */
    DR_REG_DSPSR_EL0, /* 281: DR_REG_DSPSR_EL0 */
    DR_REG_DLR_EL0, /* 282: DR_REG_DLR_EL0 */
    DR_REG_PMCR_EL0, /* 283: DR_REG_PMCR_EL0 */
    DR_REG_PMCNTENSET_EL0, /* 284: DR_REG_PMCNTENSET_EL0 */
    DR_REG_PMCNTENCLR_EL0, /* 285: DR_REG_PMCNTENCLR_EL0 */
    DR_REG_PMOVSCLR_EL0, /* 286: DR_REG_PMOVSCLR_EL0 */
    DR_REG_PMSWINC_EL0, /* 287: DR_REG_PMSWINC_EL0 */
    DR_REG_PMSELR_EL0, /* 288: DR_REG_PMSELR_EL0 */
    DR_REG_PMCEID0_EL0, /* 289: DR_REG_PMCEID0_EL0 */
    DR_REG_PMCEID1_EL0, /* 290: DR_REG_PMCEID1_EL0 */
    DR_REG_PMCCNTR_EL0, /* 291: DR_REG_PMCCNTR_EL0 */
    DR_REG_PMXEVTYPER_EL0, /* 292: DR_REG_PMXEVTYPER_EL0 */
    DR_REG_PMXEVCNTR_EL0, /* 293: DR_REG_PMXEVCNTR_EL0 */
    DR_REG_PMUSERENR_EL0, /* 294: DR_REG_PMUSERENR_EL0 */
    DR_REG_PMOVSSET_EL0, /* 295: DR_REG_PMOVSSET_EL0 */
    DR_REG_SCXTNUM_EL0, /* 296: DR_REG_SCXTNUM_EL0 */
    DR_REG_CNTFRQ_EL0, /* 297: DR_REG_CNTFRQ_EL0 */
    DR_REG_CNTPCT_EL0, /* 298: DR_REG_CNTPCT_EL0 */
    DR_REG_CNTP_TVAL_EL0, /* 299: DR_REG_CNTP_TVAL_EL0 */
    DR_REG_CNTP_CTL_EL0, /* 300: DR_REG_CNTP_CTL_EL0 */
    DR_REG_CNTP_CVAL_EL0, /* 301: DR_REG_CNTP_CVAL_EL0 */
    DR_REG_CNTV_TVAL_EL0, /* 302: DR_REG_CNTV_TVAL_EL0 */
    DR_REG_CNTV_CTL_EL0, /* 303: DR_REG_CNTV_CTL_EL0 */
    DR_REG_CNTV_CVAL_EL0, /* 304: DR_REG_CNTV_CVAL_EL0 */
    DR_REG_PMEVCNTR0_EL0, /* 305: DR_REG_PMEVCNTR0_EL0 */
    DR_REG_PMEVCNTR1_EL0, /* 306: DR_REG_PMEVCNTR1_EL0 */
    DR_REG_PMEVCNTR2_EL0, /* 307: DR_REG_PMEVCNTR2_EL0 */
    DR_REG_PMEVCNTR3_EL0, /* 308: DR_REG_PMEVCNTR3_EL0 */
    DR_REG_PMEVCNTR4_EL0, /* 309: DR_REG_PMEVCNTR4_EL0 */
    DR_REG_PMEVCNTR5_EL0, /* 310: DR_REG_PMEVCNTR5_EL0 */
    DR_REG_PMEVCNTR6_EL0, /* 311: DR_REG_PMEVCNTR6_EL0 */
    DR_REG_PMEVCNTR7_EL0, /* 312: DR_REG_PMEVCNTR7_EL0 */
    DR_REG_PMEVCNTR8_EL0, /* 313: DR_REG_PMEVCNTR8_EL0 */
    DR_REG_PMEVCNTR9_EL0, /* 314: DR_REG_PMEVCNTR9_EL0 */
    DR_REG_PMEVCNTR10_EL0, /* 315: DR_REG_PMEVCNTR10_EL0 */
    DR_REG_PMEVCNTR11_EL0, /* 316: DR_REG_PMEVCNTR11_EL0 */
    DR_REG_PMEVCNTR12_EL0, /* 317: DR_REG_PMEVCNTR12_EL0 */
    DR_REG_PMEVCNTR13_EL0, /* 318: DR_REG_PMEVCNTR13_EL0 */
    DR_REG_PMEVCNTR14_EL0, /* 319: DR_REG_PMEVCNTR14_EL0 */
    DR_REG_PMEVCNTR15_EL0, /* 320: DR_REG_PMEVCNTR15_EL0 */
    DR_REG_PMEVCNTR16_EL0, /* 321: DR_REG_PMEVCNTR16_EL0 */
    DR_REG_PMEVCNTR17_EL0, /* 322: DR_REG_PMEVCNTR17_EL0 */
    DR_REG_PMEVCNTR18_EL0, /* 323: DR_REG_PMEVCNTR18_EL0 */
    DR_REG_PMEVCNTR19_EL0, /* 324: DR_REG_PMEVCNTR19_EL0 */
    DR_REG_PMEVCNTR20_EL0, /* 325: DR_REG_PMEVCNTR20_EL0 */
    DR_REG_PMEVCNTR21_EL0, /* 326: DR_REG_PMEVCNTR21_EL0 */
    DR_REG_PMEVCNTR22_EL0, /* 327: DR_REG_PMEVCNTR22_EL0 */
    DR_REG_PMEVCNTR23_EL0, /* 328: DR_REG_PMEVCNTR23_EL0 */
    DR_REG_PMEVCNTR24_EL0, /* 329: DR_REG_PMEVCNTR24_EL0 */
    DR_REG_PMEVCNTR25_EL0, /* 330: DR_REG_PMEVCNTR25_EL0 */
    DR_REG_PMEVCNTR26_EL0, /* 331: DR_REG_PMEVCNTR26_EL0 */
    DR_REG_PMEVCNTR27_EL0, /* 332: DR_REG_PMEVCNTR27_EL0 */
    DR_REG_PMEVCNTR28_EL0, /* 333: DR_REG_PMEVCNTR28_EL0 */
    DR_REG_PMEVCNTR29_EL0, /* 334: DR_REG_PMEVCNTR29_EL0 */
    DR_REG_PMEVCNTR30_EL0, /* 335: DR_REG_PMEVCNTR30_EL0 */
    DR_REG_PMEVTYPER0_EL0, /* 336: DR_REG_PMEVTYPER0_EL0 */
    DR_REG_PMEVTYPER1_EL0, /* 337: DR_REG_PMEVTYPER1_EL0 */
    DR_REG_PMEVTYPER2_EL0, /* 338: DR_REG_PMEVTYPER2_EL0 */
    DR_REG_PMEVTYPER3_EL0, /* 339: DR_REG_PMEVTYPER3_EL0 */
    DR_REG_PMEVTYPER4_EL0, /* 340: DR_REG_PMEVTYPER4_EL0 */
    DR_REG_PMEVTYPER5_EL0, /* 341: DR_REG_PMEVTYPER5_EL0 */
    DR_REG_PMEVTYPER6_EL0, /* 342: DR_REG_PMEVTYPER6_EL0 */
    DR_REG_PMEVTYPER7_EL0, /* 343: DR_REG_PMEVTYPER7_EL0 */
    DR_REG_PMEVTYPER8_EL0, /* 344: DR_REG_PMEVTYPER8_EL0 */
    DR_REG_PMEVTYPER9_EL0, /* 345: DR_REG_PMEVTYPER9_EL0 */
    DR_REG_PMEVTYPER10_EL0, /* 346: DR_REG_PMEVTYPER10_EL0 */
    DR_REG_PMEVTYPER11_EL0, /* 347: DR_REG_PMEVTYPER11_EL0 */
    DR_REG_PMEVTYPER12_EL0, /* 348: DR_REG_PMEVTYPER12_EL0 */
    DR_REG_PMEVTYPER13_EL0, /* 349: DR_REG_PMEVTYPER13_EL0 */
    DR_REG_PMEVTYPER14_EL0, /* 350: DR_REG_PMEVTYPER14_EL0 */
    DR_REG_PMEVTYPER15_EL0, /* 351: DR_REG_PMEVTYPER15_EL0 */
    DR_REG_PMEVTYPER16_EL0, /* 352: DR_REG_PMEVTYPER16_EL0 */
    DR_REG_PMEVTYPER17_EL0, /* 353: DR_REG_PMEVTYPER17_EL0 */
    DR_REG_PMEVTYPER18_EL0, /* 354: DR_REG_PMEVTYPER18_EL0 */
    DR_REG_PMEVTYPER19_EL0, /* 355: DR_REG_PMEVTYPER19_EL0 */
    DR_REG_PMEVTYPER20_EL0, /* 356: DR_REG_PMEVTYPER20_EL0 */
    DR_REG_PMEVTYPER21_EL0, /* 357: DR_REG_PMEVTYPER21_EL0 */
    DR_REG_PMEVTYPER22_EL0, /* 358: DR_REG_PMEVTYPER22_EL0 */
    DR_REG_PMEVTYPER23_EL0, /* 359: DR_REG_PMEVTYPER23_EL0 */
    DR_REG_PMEVTYPER24_EL0, /* 360: DR_REG_PMEVTYPER24_EL0 */
    DR_REG_PMEVTYPER25_EL0, /* 361: DR_REG_PMEVTYPER25_EL0 */
    DR_REG_PMEVTYPER26_EL0, /* 362: DR_REG_PMEVTYPER26_EL0 */
    DR_REG_PMEVTYPER27_EL0, /* 363: DR_REG_PMEVTYPER27_EL0 */
    DR_REG_PMEVTYPER28_EL0, /* 364: DR_REG_PMEVTYPER28_EL0 */
    DR_REG_PMEVTYPER29_EL0, /* 365: DR_REG_PMEVTYPER29_EL0 */
    DR_REG_PMEVTYPER30_EL0, /* 366: DR_REG_PMEVTYPER30_EL0 */
    DR_REG_PMCCFILTR_EL0, /* 367: DR_REG_PMCCFILTR_EL0 */
    DR_REG_SPSR_IRQ, /* 368: DR_REG_SPSR_IRQ */
    DR_REG_SPSR_ABT, /* 369: DR_REG_SPSR_ABT */
    DR_REG_SPSR_UND, /* 370: DR_REG_SPSR_UND */
    DR_REG_SPSR_FIQ, /* 371: DR_REG_SPSR_FIQ */
    DR_REG_TPIDRURW, /* 372: DR_REG_TPIDRURW */
    DR_REG_TPIDRURO, /* 373: DR_REG_TPIDRURO */
    DR_REG_P0, /* 374: DR_REG_P0 */
    DR_REG_P1, /* 375: DR_REG_P1 */
    DR_REG_P2, /* 376: DR_REG_P2 */
    DR_REG_P3, /* 377: DR_REG_P3 */
    DR_REG_P4, /* 378: DR_REG_P4 */
    DR_REG_P5, /* 379: DR_REG_P5 */
    DR_REG_P6, /* 380: DR_REG_P6 */
    DR_REG_P7, /* 381: DR_REG_P7 */
    DR_REG_P8, /* 382: DR_REG_P8 */
    DR_REG_P9, /* 383: DR_REG_P9 */
    DR_REG_P10, /* 384: DR_REG_P10 */
    DR_REG_P11, /* 385: DR_REG_P11 */
    DR_REG_P12, /* 386: DR_REG_P12 */
    DR_REG_P13, /* 387: DR_REG_P13 */
    DR_REG_P14, /* 388: DR_REG_P14 */
    DR_REG_P15, /* 389: DR_REG_P15 */
    DR_REG_FFR, /* 390: DR_REG_FFR */
    DR_REG_CNTVCT_EL0, /* 391: DR_REG_CNTVCT_EL0 */
    DR_REG_ID_AA64ISAR0_EL1, /* 392: DR_REG_ID_AA64ISAR0_EL1 */
    DR_REG_ID_AA64ISAR1_EL1, /* 393: DR_REG_ID_AA64ISAR1_EL1 */
    DR_REG_ID_AA64ISAR2_EL1, /* 394: DR_REG_ID_AA64ISAR2_EL1 */
    DR_REG_ID_AA64PFR0_EL1, /* 395: DR_REG_ID_AA64PFR0_EL1 */
    DR_REG_ID_AA64MMFR1_EL1, /* 396: DR_REG_ID_AA64MMFR1_EL1 */
    DR_REG_ID_AA64DFR0_EL1, /* 397: DR_REG_ID_AA64DFR0_EL1 */
    DR_REG_ID_AA64ZFR0_EL1, /* 398: DR_REG_ID_AA64ZFR0_EL1 */
    DR_REG_ID_AA64PFR1_EL1, /* 399: DR_REG_ID_AA64PFR1_EL1 */
    DR_REG_ID_AA64MMFR2_EL1, /* 400: DR_REG_ID_AA64MMFR2_EL1 */
    DR_REG_MIDR_EL1, /* 401: DR_REG_MIDR_EL1 */
    DR_REG_MPIDR_EL1, /* 402: DR_REG_MPIDR_EL1 */
    DR_REG_REVIDR_EL1, /* 403: DR_REG_REVIDR_EL1 */
    DR_REG_FPMR, /* 404: DR_REG_FPMR */
    -1, /* 405: DR_REG_CONTEXTIDR_EL1 */
    -1, /* 406: DR_REG_ELR_EL1 */
    -1, /* 407: DR_REG_SPSR_EL1 */
    -1, /* 408: DR_REG_TPIDR_EL1 */
    -1, /* 409: DR_REG_ACCDATA_EL1 */
};
#define SOURCE_REG_COUNT (sizeof(source_register_ids) / sizeof(source_register_ids[0]))
static inline reg_id_t source_reg(unsigned id) {
    REPLAY_CHECK(id < SOURCE_REG_COUNT, "source register id %u", id);
    if (source_register_ids[id] >= 0)
        return (reg_id_t)source_register_ids[id];
    /* Missing original symbols remain explicit. Resolve their semantic names
     * if a later matching SDK supplies them, without inventing enum values. */
    char name[64];
    const char *symbol = source_register_names[id] + 7; /* DR_REG_ */
    size_t n = strlen(symbol);
    REPLAY_CHECK(n < sizeof(name), "register name too long");
    for (size_t i = 0; i <= n; ++i)
        name[i] = symbol[i] >= 'A' && symbol[i] <= 'Z' ? symbol[i] + ('a' - 'A') : symbol[i];
    for (int reg = DR_REG_START_GPR; reg <= DR_REG_LAST_ENUM; ++reg)
        if (strcmp(get_register_name((reg_id_t)reg), name) == 0)
            return (reg_id_t)reg;
    REPLAY_CHECK(false, "required source register unavailable: %s", source_register_names[id]);
    return DR_REG_NULL;
}
static inline unsigned source_reg_id(reg_id_t reg) {
    for (unsigned i = 0; i < SOURCE_REG_COUNT; ++i)
        /* Use the same resolved map in both directions. In particular, the
         * restored source entries must not remain invisible here merely
         * because their build-time fallback entry was -1. Aliases share the
         * canonical source ordinal and do not add another selector. */
        if (source_reg(i) == reg) return i;
    REPLAY_CHECK(false, "register %s has no source selector", get_register_name(reg));
    return 0;
}
static const char *const source_opcode_names[] = {
    "abs", /* 6 */
    "adc", /* 7 */
    "adcs", /* 8 */
    "add", /* 9 */
    "addhn", /* 10 */
    "addhn2", /* 11 */
    "addp", /* 12 */
    "adds", /* 13 */
    "addv", /* 14 */
    "adr", /* 15 */
    "adrp", /* 16 */
    "aesd", /* 17 */
    "aese", /* 18 */
    "aesimc", /* 19 */
    "aesmc", /* 20 */
    "and", /* 21 */
    "ands", /* 22 */
    "asrv", /* 23 */
    "autia1716", /* 24 */
    "autib1716", /* 25 */
    "b", /* 26 */
    "bcond", /* 27 */
    "bfm", /* 28 */
    "bic", /* 29 */
    "bics", /* 30 */
    "bif", /* 31 */
    "bit", /* 32 */
    "bl", /* 33 */
    "blr", /* 34 */
    "br", /* 35 */
    "brk", /* 36 */
    "bsl", /* 37 */
    "cas", /* 38 */
    "casa", /* 39 */
    "casab", /* 40 */
    "casah", /* 41 */
    "casal", /* 42 */
    "casalb", /* 43 */
    "casalh", /* 44 */
    "casb", /* 45 */
    "cash", /* 46 */
    "casl", /* 47 */
    "caslb", /* 48 */
    "caslh", /* 49 */
    "casp", /* 50 */
    "caspa", /* 51 */
    "caspal", /* 52 */
    "caspl", /* 53 */
    "cbnz", /* 54 */
    "cbz", /* 55 */
    "ccmn", /* 56 */
    "ccmp", /* 57 */
    "clrex", /* 58 */
    "cls", /* 59 */
    "clz", /* 60 */
    "cmeq", /* 61 */
    "cmge", /* 62 */
    "cmgt", /* 63 */
    "cmhi", /* 64 */
    "cmhs", /* 65 */
    "cmle", /* 66 */
    "cmlt", /* 67 */
    "cmtst", /* 68 */
    "cnt", /* 69 */
    "crc32b", /* 70 */
    "crc32cb", /* 71 */
    "crc32ch", /* 72 */
    "crc32cw", /* 73 */
    "crc32cx", /* 74 */
    "crc32h", /* 75 */
    "crc32w", /* 76 */
    "crc32x", /* 77 */
    "csel", /* 78 */
    "csinc", /* 79 */
    "csinv", /* 80 */
    "csneg", /* 81 */
    "dcps1", /* 82 */
    "dcps2", /* 83 */
    "dcps3", /* 84 */
    "dmb", /* 85 */
    "drps", /* 86 */
    "dsb", /* 87 */
    "dup", /* 88 */
    "eon", /* 89 */
    "eor", /* 90 */
    "eret", /* 91 */
    "ext", /* 92 */
    "extr", /* 93 */
    "fabd", /* 94 */
    "fabs", /* 95 */
    "facge", /* 96 */
    "facgt", /* 97 */
    "fadd", /* 98 */
    "faddp", /* 99 */
    "fccmp", /* 100 */
    "fccmpe", /* 101 */
    "fcmeq", /* 102 */
    "fcmge", /* 103 */
    "fcmgt", /* 104 */
    "fcmle", /* 105 */
    "fcmlt", /* 106 */
    "fcmp", /* 107 */
    "fcmpe", /* 108 */
    "fcsel", /* 109 */
    "fcvt", /* 110 */
    "fcvtas", /* 111 */
    "fcvtau", /* 112 */
    "fcvtl", /* 113 */
    "fcvtl2", /* 114 */
    "fcvtms", /* 115 */
    "fcvtmu", /* 116 */
    "fcvtn", /* 117 */
    "fcvtn2", /* 118 */
    "fcvtns", /* 119 */
    "fcvtnu", /* 120 */
    "fcvtps", /* 121 */
    "fcvtpu", /* 122 */
    "fcvtxn", /* 123 */
    "fcvtxn2", /* 124 */
    "fcvtzs", /* 125 */
    "fcvtzu", /* 126 */
    "fdiv", /* 127 */
    "fmadd", /* 128 */
    "fmax", /* 129 */
    "fmaxnm", /* 130 */
    "fmaxnmp", /* 131 */
    "fmaxnmv", /* 132 */
    "fmaxp", /* 133 */
    "fmaxv", /* 134 */
    "fmin", /* 135 */
    "fminnm", /* 136 */
    "fminnmp", /* 137 */
    "fminnmv", /* 138 */
    "fminp", /* 139 */
    "fminv", /* 140 */
    "fmla", /* 141 */
    "fmlal", /* 142 */
    "fmlal2", /* 143 */
    "fmls", /* 144 */
    "fmlsl", /* 145 */
    "fmlsl2", /* 146 */
    "fmov", /* 147 */
    "fmsub", /* 148 */
    "fmul", /* 149 */
    "fmulx", /* 150 */
    "fneg", /* 151 */
    "fnmadd", /* 152 */
    "fnmsub", /* 153 */
    "fnmul", /* 154 */
    "frecpe", /* 155 */
    "frecps", /* 156 */
    "frecpx", /* 157 */
    "frinta", /* 158 */
    "frinti", /* 159 */
    "frintm", /* 160 */
    "frintn", /* 161 */
    "frintp", /* 162 */
    "frintx", /* 163 */
    "frintz", /* 164 */
    "frsqrte", /* 165 */
    "frsqrts", /* 166 */
    "fsqrt", /* 167 */
    "fsub", /* 168 */
    "hlt", /* 169 */
    "hvc", /* 170 */
    "ins", /* 171 */
    "isb", /* 172 */
    "ld1", /* 173 */
    "ld1r", /* 174 */
    "ld2", /* 175 */
    "ld2r", /* 176 */
    "ld3", /* 177 */
    "ld3r", /* 178 */
    "ld4", /* 179 */
    "ld4r", /* 180 */
    "ldadd", /* 181 */
    "ldadda", /* 182 */
    "ldaddab", /* 183 */
    "ldaddah", /* 184 */
    "ldaddal", /* 185 */
    "ldaddalb", /* 186 */
    "ldaddalh", /* 187 */
    "ldaddb", /* 188 */
    "ldaddh", /* 189 */
    "ldaddl", /* 190 */
    "ldaddlb", /* 191 */
    "ldaddlh", /* 192 */
    "ldar", /* 193 */
    "ldarb", /* 194 */
    "ldarh", /* 195 */
    "ldaxp", /* 196 */
    "ldaxr", /* 197 */
    "ldaxrb", /* 198 */
    "ldaxrh", /* 199 */
    "ldclr", /* 200 */
    "ldclra", /* 201 */
    "ldclrab", /* 202 */
    "ldclrah", /* 203 */
    "ldclral", /* 204 */
    "ldclralb", /* 205 */
    "ldclralh", /* 206 */
    "ldclrb", /* 207 */
    "ldclrh", /* 208 */
    "ldclrl", /* 209 */
    "ldclrlb", /* 210 */
    "ldclrlh", /* 211 */
    "ldeor", /* 212 */
    "ldeora", /* 213 */
    "ldeorab", /* 214 */
    "ldeorah", /* 215 */
    "ldeoral", /* 216 */
    "ldeoralb", /* 217 */
    "ldeoralh", /* 218 */
    "ldeorb", /* 219 */
    "ldeorh", /* 220 */
    "ldeorl", /* 221 */
    "ldeorlb", /* 222 */
    "ldeorlh", /* 223 */
    "ldnp", /* 224 */
    "ldp", /* 225 */
    "ldpsw", /* 226 */
    "ldr", /* 227 */
    "ldrb", /* 228 */
    "ldrh", /* 229 */
    "ldrsb", /* 230 */
    "ldrsh", /* 231 */
    "ldrsw", /* 232 */
    "ldset", /* 233 */
    "ldseta", /* 234 */
    "ldsetab", /* 235 */
    "ldsetah", /* 236 */
    "ldsetal", /* 237 */
    "ldsetalb", /* 238 */
    "ldsetalh", /* 239 */
    "ldsetb", /* 240 */
    "ldseth", /* 241 */
    "ldsetl", /* 242 */
    "ldsetlb", /* 243 */
    "ldsetlh", /* 244 */
    "ldsmax", /* 245 */
    "ldsmaxa", /* 246 */
    "ldsmaxab", /* 247 */
    "ldsmaxah", /* 248 */
    "ldsmaxal", /* 249 */
    "ldsmaxalb", /* 250 */
    "ldsmaxalh", /* 251 */
    "ldsmaxb", /* 252 */
    "ldsmaxh", /* 253 */
    "ldsmaxl", /* 254 */
    "ldsmaxlb", /* 255 */
    "ldsmaxlh", /* 256 */
    "ldsmin", /* 257 */
    "ldsmina", /* 258 */
    "ldsminab", /* 259 */
    "ldsminah", /* 260 */
    "ldsminal", /* 261 */
    "ldsminalb", /* 262 */
    "ldsminalh", /* 263 */
    "ldsminb", /* 264 */
    "ldsminh", /* 265 */
    "ldsminl", /* 266 */
    "ldsminlb", /* 267 */
    "ldsminlh", /* 268 */
    "ldtr", /* 269 */
    "ldtrb", /* 270 */
    "ldtrh", /* 271 */
    "ldtrsb", /* 272 */
    "ldtrsh", /* 273 */
    "ldtrsw", /* 274 */
    "ldumax", /* 275 */
    "ldumaxa", /* 276 */
    "ldumaxab", /* 277 */
    "ldumaxah", /* 278 */
    "ldumaxal", /* 279 */
    "ldumaxalb", /* 280 */
    "ldumaxalh", /* 281 */
    "ldumaxb", /* 282 */
    "ldumaxh", /* 283 */
    "ldumaxl", /* 284 */
    "ldumaxlb", /* 285 */
    "ldumaxlh", /* 286 */
    "ldumin", /* 287 */
    "ldumina", /* 288 */
    "lduminab", /* 289 */
    "lduminah", /* 290 */
    "lduminal", /* 291 */
    "lduminalb", /* 292 */
    "lduminalh", /* 293 */
    "lduminb", /* 294 */
    "lduminh", /* 295 */
    "lduminl", /* 296 */
    "lduminlb", /* 297 */
    "lduminlh", /* 298 */
    "ldur", /* 299 */
    "ldurb", /* 300 */
    "ldurh", /* 301 */
    "ldursb", /* 302 */
    "ldursh", /* 303 */
    "ldursw", /* 304 */
    "ldxp", /* 305 */
    "ldxr", /* 306 */
    "ldxrb", /* 307 */
    "ldxrh", /* 308 */
    "lslv", /* 309 */
    "lsrv", /* 310 */
    "madd", /* 311 */
    "mla", /* 312 */
    "mls", /* 313 */
    "movi", /* 314 */
    "movk", /* 315 */
    "movn", /* 316 */
    "movz", /* 317 */
    "mrs", /* 318 */
    "msr", /* 319 */
    "msub", /* 320 */
    "mul", /* 321 */
    "mvni", /* 322 */
    "neg", /* 323 */
    "nop", /* 324 */
    "not", /* 325 */
    "orn", /* 326 */
    "orr", /* 327 */
    "pmul", /* 328 */
    "pmull", /* 329 */
    "pmull2", /* 330 */
    "prfm", /* 331 */
    "prfum", /* 332 */
    "raddhn", /* 333 */
    "raddhn2", /* 334 */
    "rbit", /* 335 */
    "ret", /* 336 */
    "rev", /* 337 */
    "rev16", /* 338 */
    "rev32", /* 339 */
    "rev64", /* 340 */
    "rorv", /* 341 */
    "rshrn", /* 342 */
    "rshrn2", /* 343 */
    "rsubhn", /* 344 */
    "rsubhn2", /* 345 */
    "saba", /* 346 */
    "sabal", /* 347 */
    "sabal2", /* 348 */
    "sabd", /* 349 */
    "sabdl", /* 350 */
    "sabdl2", /* 351 */
    "sadalp", /* 352 */
    "saddl", /* 353 */
    "saddl2", /* 354 */
    "saddlp", /* 355 */
    "saddlv", /* 356 */
    "saddw", /* 357 */
    "saddw2", /* 358 */
    "sbc", /* 359 */
    "sbcs", /* 360 */
    "sbfm", /* 361 */
    "scvtf", /* 362 */
    "sdiv", /* 363 */
    "sdot", /* 364 */
    "sev", /* 365 */
    "sevl", /* 366 */
    "sha1c", /* 367 */
    "sha1h", /* 368 */
    "sha1m", /* 369 */
    "sha1p", /* 370 */
    "sha1su0", /* 371 */
    "sha1su1", /* 372 */
    "sha256h", /* 373 */
    "sha256h2", /* 374 */
    "sha256su0", /* 375 */
    "sha256su1", /* 376 */
    "shadd", /* 377 */
    "shl", /* 378 */
    "shll", /* 379 */
    "shll2", /* 380 */
    "shrn", /* 381 */
    "shrn2", /* 382 */
    "shsub", /* 383 */
    "sli", /* 384 */
    "smaddl", /* 385 */
    "smax", /* 386 */
    "smaxp", /* 387 */
    "smaxv", /* 388 */
    "smc", /* 389 */
    "smin", /* 390 */
    "sminp", /* 391 */
    "sminv", /* 392 */
    "smlal", /* 393 */
    "smlal2", /* 394 */
    "smlsl", /* 395 */
    "smlsl2", /* 396 */
    "smov", /* 397 */
    "smsubl", /* 398 */
    "smulh", /* 399 */
    "smull", /* 400 */
    "smull2", /* 401 */
    "sqabs", /* 402 */
    "sqadd", /* 403 */
    "sqdmlal", /* 404 */
    "sqdmlal2", /* 405 */
    "sqdmlsl", /* 406 */
    "sqdmlsl2", /* 407 */
    "sqdmulh", /* 408 */
    "sqdmull", /* 409 */
    "sqdmull2", /* 410 */
    "sqneg", /* 411 */
    "sqrdmlah", /* 412 */
    "sqrdmulh", /* 413 */
    "sqrshl", /* 414 */
    "sqrshrn", /* 415 */
    "sqrshrn2", /* 416 */
    "sqrshrun", /* 417 */
    "sqrshrun2", /* 418 */
    "sqshl", /* 419 */
    "sqshlu", /* 420 */
    "sqshrn", /* 421 */
    "sqshrn2", /* 422 */
    "sqshrun", /* 423 */
    "sqshrun2", /* 424 */
    "sqsub", /* 425 */
    "sqxtn", /* 426 */
    "sqxtn2", /* 427 */
    "sqxtun", /* 428 */
    "sqxtun2", /* 429 */
    "srhadd", /* 430 */
    "sri", /* 431 */
    "srshl", /* 432 */
    "srshr", /* 433 */
    "srsra", /* 434 */
    "sshl", /* 435 */
    "sshll", /* 436 */
    "sshll2", /* 437 */
    "sshr", /* 438 */
    "ssra", /* 439 */
    "ssubl", /* 440 */
    "ssubl2", /* 441 */
    "ssubw", /* 442 */
    "ssubw2", /* 443 */
    "st1", /* 444 */
    "st2", /* 445 */
    "st3", /* 446 */
    "st4", /* 447 */
    "stlr", /* 448 */
    "stlrb", /* 449 */
    "stlrh", /* 450 */
    "stlxp", /* 451 */
    "stlxr", /* 452 */
    "stlxrb", /* 453 */
    "stlxrh", /* 454 */
    "stnp", /* 455 */
    "stp", /* 456 */
    "str", /* 457 */
    "strb", /* 458 */
    "strh", /* 459 */
    "sttr", /* 460 */
    "sttrb", /* 461 */
    "sttrh", /* 462 */
    "stur", /* 463 */
    "sturb", /* 464 */
    "sturh", /* 465 */
    "stxp", /* 466 */
    "stxr", /* 467 */
    "stxrb", /* 468 */
    "stxrh", /* 469 */
    "sub", /* 470 */
    "subhn", /* 471 */
    "subhn2", /* 472 */
    "subs", /* 473 */
    "suqadd", /* 474 */
    "svc", /* 475 */
    "swp", /* 476 */
    "swpa", /* 477 */
    "swpab", /* 478 */
    "swpah", /* 479 */
    "swpal", /* 480 */
    "swpalb", /* 481 */
    "swpalh", /* 482 */
    "swpb", /* 483 */
    "swph", /* 484 */
    "swpl", /* 485 */
    "swplb", /* 486 */
    "swplh", /* 487 */
    "sys", /* 488 */
    "sysl", /* 489 */
    "tbl", /* 490 */
    "tbnz", /* 491 */
    "tbx", /* 492 */
    "tbz", /* 493 */
    "trn1", /* 494 */
    "trn2", /* 495 */
    "uaba", /* 496 */
    "uabal", /* 497 */
    "uabal2", /* 498 */
    "uabd", /* 499 */
    "uabdl", /* 500 */
    "uabdl2", /* 501 */
    "uadalp", /* 502 */
    "uaddl", /* 503 */
    "uaddl2", /* 504 */
    "uaddlp", /* 505 */
    "uaddlv", /* 506 */
    "uaddw", /* 507 */
    "uaddw2", /* 508 */
    "ubfm", /* 509 */
    "ucvtf", /* 510 */
    "udiv", /* 511 */
    "udot", /* 512 */
    "uhadd", /* 513 */
    "uhsub", /* 514 */
    "umaddl", /* 515 */
    "umax", /* 516 */
    "umaxp", /* 517 */
    "umaxv", /* 518 */
    "umin", /* 519 */
    "uminp", /* 520 */
    "uminv", /* 521 */
    "umlal", /* 522 */
    "umlal2", /* 523 */
    "umlsl", /* 524 */
    "umlsl2", /* 525 */
    "umov", /* 526 */
    "umsubl", /* 527 */
    "umulh", /* 528 */
    "umull", /* 529 */
    "umull2", /* 530 */
    "uqadd", /* 531 */
    "uqrshl", /* 532 */
    "uqrshrn", /* 533 */
    "uqrshrn2", /* 534 */
    "uqshl", /* 535 */
    "uqshrn", /* 536 */
    "uqshrn2", /* 537 */
    "uqsub", /* 538 */
    "uqxtn", /* 539 */
    "uqxtn2", /* 540 */
    "urecpe", /* 541 */
    "urhadd", /* 542 */
    "urshl", /* 543 */
    "urshr", /* 544 */
    "ursqrte", /* 545 */
    "ursra", /* 546 */
    "ushl", /* 547 */
    "ushll", /* 548 */
    "ushll2", /* 549 */
    "ushr", /* 550 */
    "usqadd", /* 551 */
    "usra", /* 552 */
    "usubl", /* 553 */
    "usubl2", /* 554 */
    "usubw", /* 555 */
    "usubw2", /* 556 */
    "uzp1", /* 557 */
    "uzp2", /* 558 */
    "wfe", /* 559 */
    "wfi", /* 560 */
    "xpaclri", /* 561 */
    "xtn", /* 562 */
    "xtn2", /* 563 */
    "yield", /* 564 */
    "zip1", /* 565 */
    "zip2", /* 566 */
    "udf", /* 567 */
    "dc_zva", /* 568 */
    "dc_cisw", /* 569 */
    "dc_civac", /* 570 */
    "dc_csw", /* 571 */
    "dc_cvac", /* 572 */
    "dc_cvau", /* 573 */
    "dc_isw", /* 574 */
    "dc_ivac", /* 575 */
    "ic_iallu", /* 576 */
    "ic_ialluis", /* 577 */
    "ic_ivau", /* 578 */
    "sqrdmlsh", /* 579 */
    "ldlar", /* 580 */
    "ldlarb", /* 581 */
    "ldlarh", /* 582 */
    "stllr", /* 583 */
    "stllrb", /* 584 */
    "stllrh", /* 585 */
    "sm3partw1", /* 586 */
    "sm3partw2", /* 587 */
    "sm3ss1", /* 588 */
    "sm3tt1a", /* 589 */
    "sm3tt1b", /* 590 */
    "sm3tt2a", /* 591 */
    "sm3tt2b", /* 592 */
    "sm4e", /* 593 */
    "sm4ekey", /* 594 */
    "sha512h", /* 595 */
    "sha512h2", /* 596 */
    "sha512su0", /* 597 */
    "sha512su1", /* 598 */
    "bcax", /* 599 */
    "eor3", /* 600 */
    "esb", /* 601 */
    "psb", /* 602 */
    "rax1", /* 603 */
    "xar", /* 604 */
    "<invalid>", /* 605 */
    "<invalid>", /* 606 */
    "<invalid>", /* 607 */
    "<invalid>", /* 608 */
    "<invalid>", /* 609 */
    "<invalid>", /* 610 */
    "<invalid>", /* 611 */
    "<invalid>", /* 612 */
    "<invalid>", /* 613 */
    "<invalid>", /* 614 */
    "<invalid>", /* 615 */
    "<invalid>", /* 616 */
    "<invalid>", /* 617 */
    "<invalid>", /* 618 */
    "<invalid>", /* 619 */
    "<invalid>", /* 620 */
    "<invalid>", /* 621 */
    "<invalid>", /* 622 */
    "<invalid>", /* 623 */
    "<invalid>", /* 624 */
    "<invalid>", /* 625 */
    "<invalid>", /* 626 */
    "<invalid>", /* 627 */
    "<invalid>", /* 628 */
    "<invalid>", /* 629 */
    "<invalid>", /* 630 */
    "<invalid>", /* 631 */
    "<invalid>", /* 632 */
    "<invalid>", /* 633 */
    "<invalid>", /* 634 */
    "<invalid>", /* 635 */
    "<invalid>", /* 636 */
    "<invalid>", /* 637 */
    "<invalid>", /* 638 */
    "<invalid>", /* 639 */
    "<invalid>", /* 640 */
    "<invalid>", /* 641 */
    "<invalid>", /* 642 */
    "<invalid>", /* 643 */
    "<invalid>", /* 644 */
    "<invalid>", /* 645 */
    "<invalid>", /* 646 */
    "<invalid>", /* 647 */
    "<invalid>", /* 648 */
    "<invalid>", /* 649 */
    "<invalid>", /* 650 */
    "<invalid>", /* 651 */
    "<invalid>", /* 652 */
    "<invalid>", /* 653 */
    "<invalid>", /* 654 */
    "<invalid>", /* 655 */
    "<invalid>", /* 656 */
    "<invalid>", /* 657 */
    "<invalid>", /* 658 */
    "<invalid>", /* 659 */
    "<invalid>", /* 660 */
    "<invalid>", /* 661 */
    "<invalid>", /* 662 */
    "<invalid>", /* 663 */
    "<invalid>", /* 664 */
    "<invalid>", /* 665 */
    "<invalid>", /* 666 */
    "<invalid>", /* 667 */
    "<invalid>", /* 668 */
    "<invalid>", /* 669 */
    "<invalid>", /* 670 */
    "<invalid>", /* 671 */
    "<invalid>", /* 672 */
    "<invalid>", /* 673 */
    "<invalid>", /* 674 */
    "<invalid>", /* 675 */
    "<invalid>", /* 676 */
    "<invalid>", /* 677 */
    "<invalid>", /* 678 */
    "retaa", /* 679 */
    "pacibsp", /* 680 */
    "autibsp", /* 681 */
    "blraaz", /* 682 */
    "braa", /* 683 */
    "paciza", /* 684 */
    "xpaci", /* 685 */
    "xpacd", /* 686 */
    "pacda", /* 687 */
    "pacia", /* 688 */
    "pacib", /* 689 */
    "autdb", /* 690 */
    "<invalid>", /* 691 */
    "<invalid>", /* 692 */
    "<invalid>", /* 693 */
    "<invalid>", /* 694 */
    "<invalid>", /* 695 */
    "<invalid>", /* 696 */
    "<invalid>", /* 697 */
    "<invalid>", /* 698 */
    "<invalid>", /* 699 */
    "<invalid>", /* 700 */
    "<invalid>", /* 701 */
    "<invalid>", /* 702 */
    "<invalid>", /* 703 */
    "<invalid>", /* 704 */
    "<invalid>", /* 705 */
    "<invalid>", /* 706 */
    "<invalid>", /* 707 */
    "<invalid>", /* 708 */
    "<invalid>", /* 709 */
    "<invalid>", /* 710 */
    "<invalid>", /* 711 */
    "<invalid>", /* 712 */
    "<invalid>", /* 713 */
    "<invalid>", /* 714 */
    "<invalid>", /* 715 */
    "<invalid>", /* 716 */
    "<invalid>", /* 717 */
    "<invalid>", /* 718 */
    "<invalid>", /* 719 */
    "<invalid>", /* 720 */
    "<invalid>", /* 721 */
    "<invalid>", /* 722 */
    "<invalid>", /* 723 */
    "<invalid>", /* 724 */
    "<invalid>", /* 725 */
    "<invalid>", /* 726 */
    "<invalid>", /* 727 */
    "<invalid>", /* 728 */
    "<invalid>", /* 729 */
    "<invalid>", /* 730 */
    "<invalid>", /* 731 */
    "<invalid>", /* 732 */
    "<invalid>", /* 733 */
    "<invalid>", /* 734 */
    "<invalid>", /* 735 */
    "<invalid>", /* 736 */
    "<invalid>", /* 737 */
    "<invalid>", /* 738 */
    "<invalid>", /* 739 */
    "<invalid>", /* 740 */
    "<invalid>", /* 741 */
    "<invalid>", /* 742 */
    "<invalid>", /* 743 */
    "<invalid>", /* 744 */
    "<invalid>", /* 745 */
    "<invalid>", /* 746 */
    "<invalid>", /* 747 */
    "<invalid>", /* 748 */
    "<invalid>", /* 749 */
    "<invalid>", /* 750 */
    "<invalid>", /* 751 */
    "<invalid>", /* 752 */
    "<invalid>", /* 753 */
    "<invalid>", /* 754 */
    "<invalid>", /* 755 */
    "<invalid>", /* 756 */
    "<invalid>", /* 757 */
    "<invalid>", /* 758 */
    "<invalid>", /* 759 */
    "<invalid>", /* 760 */
    "<invalid>", /* 761 */
    "<invalid>", /* 762 */
    "<invalid>", /* 763 */
    "<invalid>", /* 764 */
    "<invalid>", /* 765 */
    "<invalid>", /* 766 */
    "<invalid>", /* 767 */
    "<invalid>", /* 768 */
    "<invalid>", /* 769 */
    "<invalid>", /* 770 */
    "<invalid>", /* 771 */
    "<invalid>", /* 772 */
    "<invalid>", /* 773 */
    "<invalid>", /* 774 */
    "<invalid>", /* 775 */
    "<invalid>", /* 776 */
    "<invalid>", /* 777 */
    "<invalid>", /* 778 */
    "<invalid>", /* 779 */
    "<invalid>", /* 780 */
    "<invalid>", /* 781 */
    "blraa", /* 782 */
    "movprfx", /* 783 */
    "subr", /* 784 */
    "cpy", /* 785 */
    "ptest", /* 786 */
    "mad", /* 787 */
    "msb", /* 788 */
    "fexpa", /* 789 */
    "ftmad", /* 790 */
    "ftsmul", /* 791 */
    "ftssel", /* 792 */
    "cnot", /* 793 */
    "sdivr", /* 794 */
    "udivr", /* 795 */
    "ldapr", /* 796 */
    "ldaprb", /* 797 */
    "ldaprh", /* 798 */
    "sxtb", /* 799 */
    "sxth", /* 800 */
    "sxtw", /* 801 */
    "uxtb", /* 802 */
    "uxth", /* 803 */
    "uxtw", /* 804 */
    "fcmne", /* 805 */
    "fcmuo", /* 806 */
    "cmpeq", /* 807 */
    "cmpge", /* 808 */
    "cmpgt", /* 809 */
    "cmphi", /* 810 */
    "cmphs", /* 811 */
    "cmple", /* 812 */
    "cmplo", /* 813 */
    "cmpls", /* 814 */
    "cmplt", /* 815 */
    "cmpne", /* 816 */
    "rdffr", /* 817 */
    "rdffrs", /* 818 */
    "setffr", /* 819 */
    "wrffr", /* 820 */
    "cntp", /* 821 */
    "decp", /* 822 */
    "incp", /* 823 */
    "sqdecp", /* 824 */
    "sqincp", /* 825 */
    "uqdecp", /* 826 */
    "uqincp", /* 827 */
    "eors", /* 828 */
    "nand", /* 829 */
    "nands", /* 830 */
    "nor", /* 831 */
    "nors", /* 832 */
    "orns", /* 833 */
    "orrs", /* 834 */
    "clasta", /* 835 */
    "clastb", /* 836 */
    "lasta", /* 837 */
    "lastb", /* 838 */
    "cntb", /* 839 */
    "decb", /* 840 */
    "decd", /* 841 */
    "dech", /* 842 */
    "sqdech", /* 843 */
    "cntd", /* 844 */
    "cnth", /* 845 */
    "cntw", /* 846 */
    "decw", /* 847 */
    "incb", /* 848 */
    "incd", /* 849 */
    "inch", /* 850 */
    "incw", /* 851 */
    "sqdecb", /* 852 */
    "sqdecd", /* 853 */
    "sqdecw", /* 854 */
    "sqincb", /* 855 */
    "sqincd", /* 856 */
    "sqinch", /* 857 */
    "sqincw", /* 858 */
    "uqdecb", /* 859 */
    "uqdecd", /* 860 */
    "uqdech", /* 861 */
    "uqdecw", /* 862 */
    "uqincb", /* 863 */
    "uqincd", /* 864 */
    "uqinch", /* 865 */
    "uqincw", /* 866 */
    "brka", /* 867 */
    "brkas", /* 868 */
    "brkb", /* 869 */
    "brkbs", /* 870 */
    "brkn", /* 871 */
    "brkns", /* 872 */
    "brkpa", /* 873 */
    "brkpas", /* 874 */
    "brkpb", /* 875 */
    "brkpbs", /* 876 */
    "whilele", /* 877 */
    "whilelo", /* 878 */
    "whilels", /* 879 */
    "whilelt", /* 880 */
    "insr", /* 881 */
    "splice", /* 882 */
    "revb", /* 883 */
    "revh", /* 884 */
    "revw", /* 885 */
    "compact", /* 886 */
    "punpkhi", /* 887 */
    "punpklo", /* 888 */
    "sunpkhi", /* 889 */
    "sunpklo", /* 890 */
    "uunpkhi", /* 891 */
    "uunpklo", /* 892 */
    "dupm", /* 893 */
    "pfalse", /* 894 */
    "pfirst", /* 895 */
    "sel", /* 896 */
    "ptrue", /* 897 */
    "ptrues", /* 898 */
    "asr", /* 899 */
    "asrd", /* 900 */
    "asrr", /* 901 */
    "lsl", /* 902 */
    "lslr", /* 903 */
    "lsr", /* 904 */
    "lsrr", /* 905 */
    "fcpy", /* 906 */
    "fdup", /* 907 */
    "ld1rb", /* 908 */
    "ld1rd", /* 909 */
    "ld1rh", /* 910 */
    "ld1rsb", /* 911 */
    "ld1rsh", /* 912 */
    "ld1rsw", /* 913 */
    "ld1rw", /* 914 */
    "andv", /* 915 */
    "eorv", /* 916 */
    "fadda", /* 917 */
    "faddv", /* 918 */
    "orv", /* 919 */
    "saddv", /* 920 */
    "uaddv", /* 921 */
    "index", /* 922 */
    "ctermeq", /* 923 */
    "ctermne", /* 924 */
    "pnext", /* 925 */
    "fdivr", /* 926 */
    "fmad", /* 927 */
    "fnmad", /* 928 */
    "fnmla", /* 929 */
    "fnmls", /* 930 */
    "fscale", /* 931 */
    "fsubr", /* 932 */
    "fmsb", /* 933 */
    "addpl", /* 934 */
    "addvl", /* 935 */
    "rdvl", /* 936 */
    "ldff1b", /* 937 */
    "ldff1d", /* 938 */
    "ldff1h", /* 939 */
    "ldff1sb", /* 940 */
    "ldff1sh", /* 941 */
    "ldff1sw", /* 942 */
    "ldff1w", /* 943 */
    "fcadd", /* 944 */
    "fcmla", /* 945 */
    "ld1b", /* 946 */
    "ld1rob", /* 947 */
    "ld1rqb", /* 948 */
    "ld1sb", /* 949 */
    "ldnt1b", /* 950 */
    "st1b", /* 951 */
    "stnt1b", /* 952 */
    "bfcvt", /* 953 */
    "bfdot", /* 954 */
    "bfmlalb", /* 955 */
    "bfmlalt", /* 956 */
    "bfmmla", /* 957 */
    "smmla", /* 958 */
    "sudot", /* 959 */
    "ummla", /* 960 */
    "usdot", /* 961 */
    "usmmla", /* 962 */
    "prfb", /* 963 */
    "prfd", /* 964 */
    "prfh", /* 965 */
    "prfw", /* 966 */
    "ld2b", /* 967 */
    "ld3b", /* 968 */
    "ld4b", /* 969 */
    "st2b", /* 970 */
    "st3b", /* 971 */
    "st4b", /* 972 */
    "bfcvtn", /* 973 */
    "bfcvtn2", /* 974 */
    "ld1d", /* 975 */
    "ld1h", /* 976 */
    "ld1sh", /* 977 */
    "ld1sw", /* 978 */
    "ld1w", /* 979 */
    "st1h", /* 980 */
    "st1d", /* 981 */
    "st1w", /* 982 */
    "ld2d", /* 983 */
    "ld2h", /* 984 */
    "ld2w", /* 985 */
    "ld3d", /* 986 */
    "ld3h", /* 987 */
    "ld3w", /* 988 */
    "ld4d", /* 989 */
    "ld4h", /* 990 */
    "ld4w", /* 991 */
    "ldnt1d", /* 992 */
    "ldnt1h", /* 993 */
    "ldnt1w", /* 994 */
    "st2d", /* 995 */
    "st2h", /* 996 */
    "st2w", /* 997 */
    "st3d", /* 998 */
    "st3h", /* 999 */
    "st3w", /* 1000 */
    "st4d", /* 1001 */
    "st4h", /* 1002 */
    "st4w", /* 1003 */
    "stnt1d", /* 1004 */
    "stnt1h", /* 1005 */
    "stnt1w", /* 1006 */
    "ldnf1b", /* 1007 */
    "ldnf1d", /* 1008 */
    "ldnf1h", /* 1009 */
    "ldnf1sb", /* 1010 */
    "ldnf1sh", /* 1011 */
    "ldnf1sw", /* 1012 */
    "ldnf1w", /* 1013 */
    "ldapur", /* 1014 */
    "ldapurb", /* 1015 */
    "ldapurh", /* 1016 */
    "ldapursb", /* 1017 */
    "ldapursh", /* 1018 */
    "ldapursw", /* 1019 */
    "stlur", /* 1020 */
    "stlurb", /* 1021 */
    "stlurh", /* 1022 */
    "cfinv", /* 1023 */
    "rmif", /* 1024 */
    "setf16", /* 1025 */
    "setf8", /* 1026 */
    "autda", /* 1027 */
    "autdza", /* 1028 */
    "autdzb", /* 1029 */
    "autia", /* 1030 */
    "autiasp", /* 1031 */
    "autiaz", /* 1032 */
    "autib", /* 1033 */
    "autibz", /* 1034 */
    "autiza", /* 1035 */
    "autizb", /* 1036 */
    "blrab", /* 1037 */
    "blrabz", /* 1038 */
    "braaz", /* 1039 */
    "brab", /* 1040 */
    "brabz", /* 1041 */
    "pacdb", /* 1042 */
    "pacdza", /* 1043 */
    "pacdzb", /* 1044 */
    "pacga", /* 1045 */
    "pacia1716", /* 1046 */
    "paciasp", /* 1047 */
    "paciaz", /* 1048 */
    "pacib1716", /* 1049 */
    "pacibz", /* 1050 */
    "pacizb", /* 1051 */
    "ldraa", /* 1052 */
    "ldrab", /* 1053 */
    "eretaa", /* 1054 */
    "eretab", /* 1055 */
    "retab", /* 1056 */
    "fjcvtzs", /* 1057 */
    "dc_cvadp", /* 1058 */
    "dc_cvap", /* 1059 */
    "ld1rqd", /* 1060 */
    "ld1rqh", /* 1061 */
    "ld1rqw", /* 1062 */
    "fnmsb", /* 1063 */
    "bfcvtnt", /* 1064 */
    "bsl1n", /* 1065 */
    "bsl2n", /* 1066 */
    "fmlalb", /* 1067 */
    "fmlalt", /* 1068 */
    "fmlslb", /* 1069 */
    "fmlslt", /* 1070 */
    "histseg", /* 1071 */
    "nbsl", /* 1072 */
    "adclb", /* 1073 */
    "adclt", /* 1074 */
    "bdep", /* 1075 */
    "bext", /* 1076 */
    "bgrp", /* 1077 */
    "eorbt", /* 1078 */
    "eortb", /* 1079 */
    "sbclb", /* 1080 */
    "sbclt", /* 1081 */
    "addhnb", /* 1082 */
    "addhnt", /* 1083 */
    "pmullb", /* 1084 */
    "pmullt", /* 1085 */
    "raddhnb", /* 1086 */
    "raddhnt", /* 1087 */
    "rsubhnb", /* 1088 */
    "rsubhnt", /* 1089 */
    "sabalb", /* 1090 */
    "sabalt", /* 1091 */
    "sabdlb", /* 1092 */
    "sabdlt", /* 1093 */
    "saddlb", /* 1094 */
    "saddlbt", /* 1095 */
    "saddlt", /* 1096 */
    "saddwb", /* 1097 */
    "saddwt", /* 1098 */
    "smlalb", /* 1099 */
    "smlalt", /* 1100 */
    "smlslb", /* 1101 */
    "smlslt", /* 1102 */
    "smullb", /* 1103 */
    "smullt", /* 1104 */
    "sqdmlalb", /* 1105 */
    "sqdmlalbt", /* 1106 */
    "sqdmlalt", /* 1107 */
    "sqdmlslb", /* 1108 */
    "sqdmlslbt", /* 1109 */
    "sqdmlslt", /* 1110 */
    "sqdmullb", /* 1111 */
    "sqdmullt", /* 1112 */
    "ssublb", /* 1113 */
    "ssublbt", /* 1114 */
    "ssublt", /* 1115 */
    "ssubltb", /* 1116 */
    "ssubwb", /* 1117 */
    "ssubwt", /* 1118 */
    "subhnb", /* 1119 */
    "subhnt", /* 1120 */
    "uabalb", /* 1121 */
    "uabalt", /* 1122 */
    "uabdlb", /* 1123 */
    "uabdlt", /* 1124 */
    "uaddlb", /* 1125 */
    "uaddlt", /* 1126 */
    "uaddwb", /* 1127 */
    "uaddwt", /* 1128 */
    "umlalb", /* 1129 */
    "umlalt", /* 1130 */
    "umlslb", /* 1131 */
    "umlslt", /* 1132 */
    "umullb", /* 1133 */
    "umullt", /* 1134 */
    "usublb", /* 1135 */
    "usublt", /* 1136 */
    "usubwb", /* 1137 */
    "usubwt", /* 1138 */
    "sqxtnb", /* 1139 */
    "sqxtnt", /* 1140 */
    "sqxtunb", /* 1141 */
    "sqxtunt", /* 1142 */
    "uqxtnb", /* 1143 */
    "uqxtnt", /* 1144 */
    "histcnt", /* 1145 */
    "shsubr", /* 1146 */
    "sqrshlr", /* 1147 */
    "sqshlr", /* 1148 */
    "sqsubr", /* 1149 */
    "srshlr", /* 1150 */
    "uhsubr", /* 1151 */
    "uqrshlr", /* 1152 */
    "uqshlr", /* 1153 */
    "uqsubr", /* 1154 */
    "urshlr", /* 1155 */
    "fcvtlt", /* 1156 */
    "fcvtnt", /* 1157 */
    "fcvtx", /* 1158 */
    "fcvtxnt", /* 1159 */
    "flogb", /* 1160 */
    "cadd", /* 1161 */
    "cdot", /* 1162 */
    "cmla", /* 1163 */
    "rshrnb", /* 1164 */
    "rshrnt", /* 1165 */
    "shrnb", /* 1166 */
    "shrnt", /* 1167 */
    "sqcadd", /* 1168 */
    "sqrdcmlah", /* 1169 */
    "sqrshrnb", /* 1170 */
    "sqrshrnt", /* 1171 */
    "sqrshrunb", /* 1172 */
    "sqrshrunt", /* 1173 */
    "sqshrnb", /* 1174 */
    "sqshrnt", /* 1175 */
    "sqshrunb", /* 1176 */
    "sqshrunt", /* 1177 */
    "sshllb", /* 1178 */
    "sshllt", /* 1179 */
    "uqrshrnb", /* 1180 */
    "uqrshrnt", /* 1181 */
    "uqshrnb", /* 1182 */
    "uqshrnt", /* 1183 */
    "ushllb", /* 1184 */
    "ushllt", /* 1185 */
    "ldnt1sb", /* 1186 */
    "ldnt1sh", /* 1187 */
    "ldnt1sw", /* 1188 */
    "match", /* 1189 */
    "nmatch", /* 1190 */
    "whilege", /* 1191 */
    "whilegt", /* 1192 */
    "whilehi", /* 1193 */
    "whilehs", /* 1194 */
    "whilerw", /* 1195 */
    "whilewr", /* 1196 */
    "st2g", /* 1197 */
    "stg", /* 1198 */
    "stz2g", /* 1199 */
    "stzg", /* 1200 */
    "ldg", /* 1201 */
    "stgp", /* 1202 */
    "gmi", /* 1203 */
    "irg", /* 1204 */
    "subp", /* 1205 */
    "subps", /* 1206 */
    "addg", /* 1207 */
    "subg", /* 1208 */
    "dc_gva", /* 1209 */
    "dc_gzva", /* 1210 */
    "bti", /* 1211 */
    "frint32x", /* 1212 */
    "frint32z", /* 1213 */
    "frint64x", /* 1214 */
    "frint64z", /* 1215 */
    "stgm", /* 1216 */
    "stzgm", /* 1217 */
    "ldgm", /* 1218 */
    "axflag", /* 1219 */
    "xaflag", /* 1220 */
    "wfet", /* 1221 */
    "wfit", /* 1222 */
    "ld64b", /* 1223 */
    "st64b", /* 1224 */
    "st64bv", /* 1225 */
    "st64bv0", /* 1226 */
    "fmmla", /* 1227 */
    "ld1roh", /* 1228 */
    "ld1row", /* 1229 */
    "ld1rod", /* 1230 */
    "sb", /* 1231 */
    "dsbnxs", /* 1232 */
};
static inline int source_opcode(uint32_t word) {
    const char *name = source_opcode_names[word % (sizeof(source_opcode_names) / sizeof(source_opcode_names[0]))];
    for (int op = OP_ldstex + 1; op <= OP_LAST; ++op)
        if (strcmp(decode_opcode_name(op), name) == 0) return op;
    REPLAY_CHECK(false, "required source opcode unavailable: %s", name);
    return OP_INVALID;
}
