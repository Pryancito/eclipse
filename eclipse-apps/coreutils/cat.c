/*
 * cat - Concatenar y mostrar archivos
 * Comando básico de Eclipse OS
 */

#define NULL ((void*)0)
#define SYS_EXIT 0
#define SYS_WRITE 1
#define SYS_READ 4
#define STDIN 0
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
static inline long sys_read(int fd, char *buf, unsigned long count) { 
    return syscall3(SYS_READ, fd, (long)buf, count); 
}

void _start(int argc, char *argv[]) {
    char buffer[4096];
    long bytes_read;
    
    // cat lee desde stdin y escribe a stdout
    // TODO: Abrir archivos cuando open() esté implementado
    
    // Por ahora, leer desde stdin (como cat sin argumentos)
    while ((bytes_read = sys_read(STDIN, buffer, sizeof(buffer))) > 0) {
        sys_write(STDOUT, buffer, bytes_read);
    }
    
    sys_exit(0);
}

