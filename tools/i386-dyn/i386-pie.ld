/* The image IS the file, as in `i386-dyn.ld`, but at zero: `i386-interp-pie.s`
 * is `ET_DYN` and carries no absolute address, so its link-time addresses are
 * its offsets from the start of the image and the kernel biases every one of
 * them. That is exactly what lets the `call`/`pop` in it work out the base:
 * the runtime address of a label minus its link-time value is the bias.
 *
 * `i386-pie.s`, the program that names it, is still `ET_EXEC` and still uses
 * `i386-dyn.ld`. Only the interpreter is relocatable here.
 */
SECTIONS
{
	. = 0;
	.all : { *(.text) }
}
