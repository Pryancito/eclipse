/* The image IS the file: one output section at the address the headers in
 * `i386-dyn.s` claim, written out raw with `--oformat binary`. */
SECTIONS
{
	. = 0x08048000;
	.all : { *(.text) }
}
