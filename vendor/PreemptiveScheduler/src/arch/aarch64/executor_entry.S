# executor的入口函数
#
#   void executor_entry()
#
# 该函数只是简单的获取栈底 executor 实例地址
# 然后再跳转到 runtime.rs::run_executor 函数。
#
	.text
	.globl executor_entry
	.balign 4
executor_entry:
	ldr x0, [sp]
	# One usize (`self_addr`) was pushed below a 16-byte-aligned top, so SP
	# is 8 (mod 16) here. AAPCS64 requires 16-byte alignment on entry to
	# `run_executor`. `add sp, sp, #8` is rejected: an immediate written to
	# SP must be a multiple of 16, and adding 16 would land inside the top
	# guard. The register form is the same adjustment riscv64's
	# `addi sp, sp, 8` makes, and x86_64's `pop` makes for free.
	add x9, sp, #8
	mov sp, x9
	b run_executor


