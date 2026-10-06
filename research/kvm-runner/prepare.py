import pathlib,re,shutil
root=pathlib.Path(__file__).resolve().parent
src=root/'linux-source/linux-6.12.112'
p=src/'arch/x86/include/asm/msr.h'
s=p.read_text()
for name in ['rdtsc','rdtsc_ordered']:
 pattern=r'(static __always_inline unsigned long long '+name+r'\(void\)\n\{).*?\n\}'
 body=r'''\1
#ifdef __KERNEL__
 unsigned int low, high;
 asm volatile("inl %w1, %0" : "=a"(low) : "Nd"((unsigned short)0x520) : "memory");
 asm volatile("inl %w1, %0" : "=a"(high) : "Nd"((unsigned short)0x524) : "memory");
 return ((unsigned long long)high << 32) | low;
#else
 unsigned int low, high;
 asm volatile("rdtsc" : "=a"(low), "=d"(high));
 return ((unsigned long long)high << 32) | low;
#endif
}'''
 s,n=re.subn(pattern,body,s,flags=re.S);assert n==1
p.write_text(s)
# Pin frequency as an explicit profile assumption, bypassing native calibration.
p=src/'arch/x86/kernel/tsc.c';s=p.read_text();needle='static bool __init determine_cpu_tsc_frequencies(bool early)\n{'
start=s.index(needle); body=start+len(needle); depth=1;end=body
while depth:
 end+=1
 depth += (s[end]=='{') - (s[end]=='}')
s=s[:start]+needle+'\n cpu_khz = tsc_khz = 1000000;\n setup_force_cpu_cap(X86_FEATURE_TSC_KNOWN_FREQ);\n return true;\n}'+s[end+1:];p.write_text(s)
