
.section .text
.global fork
.type fork, @function
fork:
    mov $-1, %rax
    ret
.size fork, .-fork

.global vfork
.type vfork, @function
vfork:
    mov $-1, %rax
    ret
.size vfork, .-vfork

.global forced_fork
.type forced_fork, @function
forced_fork:
    mov $-1, %rax
    ret
.size forced_fork, .-forced_fork

.global execl
.type execl, @function
execl:
    mov $-1, %rax
    ret
.size execl, .-execl

.global execv
.type execv, @function
execv:
    mov $-1, %rax
    ret
.size execv, .-execv

.global execvp
.type execvp, @function
execvp:
    mov $-1, %rax
    ret
.size execvp, .-execvp

.global pipe
.type pipe, @function
pipe:
    test %rdi, %rdi
    jz .Lpipe_err
    movl $-1, (%rdi)
    movl $-1, 4(%rdi)
.Lpipe_err:
    mov $-1, %rax
    ret
.size pipe, .-pipe
