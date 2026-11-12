//! `eclipse-netstack-use PROGRAM [ARG...]`: run a program on the network
//! stack that runs as a process.
//!
//! Which stack a program's sockets are is not the program's business: it
//! is decided by whatever starts it. This asks the kernel that the
//! datagram sockets of this process be those of `eclipse-netstack`
//! (`eclipse_netstack_use`), which `fork` and `execve` carry on, and
//! becomes the program. The program opens its sockets as it always did.
//!
//! Exit status: 2 without a program, 1 when the kernel has no such call,
//! 127 when the program cannot be run.

use std::os::unix::process::CommandExt;
use std::process::{exit, Command};

use udrivers::zx::{self, nr, Status};

const NAME: &str = "eclipse-netstack-use";

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(program) = args.next() else {
        println!("usage: {NAME} PROGRAM [ARG...]");
        exit(2);
    };
    // SAFETY: no arguments, no pointers.
    if let Err(e) = unsafe { zx::call(nr::ECLIPSE_NETSTACK_USE, [0; 6]) } {
        match e {
            Status::NO_GATEWAY => println!("{NAME}: the gateway is off"),
            e => println!("{NAME}: eclipse_netstack_use: {e}"),
        }
        exit(1);
    }
    let why = Command::new(&program).args(args).exec();
    println!("{NAME}: {}: {why}", program.to_string_lossy());
    exit(127);
}
