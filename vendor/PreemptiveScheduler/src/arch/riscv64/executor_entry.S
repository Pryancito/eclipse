# executor的入口函数
#
#   void executor_entry()
#
# 该函数只是简单的获取栈底的 executor 实例地址
# 然后再跳转到 runtime.rs::run_executor 函数。
#
	.text
	.globl executor_entry
	.balign 4
executor_entry:
	ld a0, 0(sp)
	addi sp, sp, 16
	# `tail` (auipc+jalr, +-2 GiB), no `j`: `j` se ensambla como JAL, que
	# alcanza +-1 MiB, y `run_executor` vive en el kernel, no en este
	# crate, asi que la distancia crece con el .text del kernel y en cuanto
	# paso de 1 MiB el enlace revento con `R_RISCV_JAL out of range`. Es el
	# mismo arreglo que `trap.S` necesito antes. `tail` clobbea t1, que es
	# de llamada y aqui no lleva nada: a0 ya esta cargado.
	tail run_executor

