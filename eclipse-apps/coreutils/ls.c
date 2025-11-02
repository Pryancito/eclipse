/*
 * ls - Listar archivos
 * Comando básico de Eclipse OS
 */

#define NULL ((void*)0)
#define SYS_EXIT 0
#define SYS_WRITE 1
#define STDOUT 1

static inline long syscall1(long n, long a1) {
    long ret;
    asm volatile("int $0x80" : "=a"(ret) : "a"(n), "D"(a1) : "rcx", "r11", "memory");
    return ret;
}

static inline long syscall3(long n, long a1, long a2, long a3) {
    long ret;
    asm volatile("int $0x80" : "=a"(ret) : "a"(n), "D"(a1), "S"(a2), "d"(a3) : "rcx", "r11", "memory");
    return ret;
}

static inline void sys_exit(int code) { syscall1(SYS_EXIT, code); }
static inline long sys_write(int fd, const char *buf, unsigned long count) { 
    return syscall3(SYS_WRITE, fd, (long)buf, count); 
}

static unsigned long strlen(const char *s) {
    unsigned long len = 0;
    while (s[len]) len++;
    return len;
}

static void write_str(const char *str) {
    sys_write(STDOUT, str, strlen(str));
}

void _start(int argc, char *argv[]) {
    // ls lista archivos del directorio actual
    
    // Por ahora, listar archivos simulados
    // TODO: Usar getdents() syscall cuando esté implementada
    
    write_str("boot/\n");
    write_str("dev/\n");
    write_str("etc/\n");
    write_str("home/\n");
    write_str("tmp/\n");
    write_str("usr/\n");
    write_str("var/\n");
    
    sys_exit(0);
}

