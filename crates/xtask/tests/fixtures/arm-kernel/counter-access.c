static void update_cntkctl_el1(struct task_struct *next)
{
	if (test_ti_thread_flag(task_thread_info(next), TIF_TSC_SIGSEGV))
		sysreg_clear_set(cntkctl_el1, ARCH_TIMER_USR_VCT_ACCESS_EN, 0);
	else
		sysreg_clear_set(cntkctl_el1, 0, ARCH_TIMER_USR_VCT_ACCESS_EN);
}
