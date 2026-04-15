/* setjmp.h for Eclipse OS */
#pragma once
#ifndef _SETJMP_H
#define _SETJMP_H

/* jmp_buf layout (x86-64): rbx, rbp, r12, r13, r14, r15, rsp, rip */
typedef unsigned long jmp_buf[8];
typedef unsigned long sigjmp_buf[8+1];  /* +1 for saved sigmask flag */

int  setjmp(jmp_buf env);
void longjmp(jmp_buf env, int val) __attribute__((noreturn));
int  sigsetjmp(sigjmp_buf env, int savesigs);
void siglongjmp(sigjmp_buf env, int val) __attribute__((noreturn));

#define _setjmp  setjmp
#define _longjmp longjmp

#endif /* _SETJMP_H */
