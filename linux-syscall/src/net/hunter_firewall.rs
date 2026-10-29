//! The kernel's half of hunter's per-application firewall
//! (`hunter::firewall`): who is asking, where to, and -- under `Enforce` --
//! holding the call while the person at the desktop answers.
//!
//! Split out of `net.rs` for the size ratchet, and because none of it is
//! socket plumbing: `connect`, `bind`, `sendto` and `sendmsg` each make one
//! call to [`Syscall::hunter_net_gate`] before they touch the socket.

use super::*;
use alloc::string::String;
use core::time::Duration;
use hunter::firewall::{self, Direction, Verdict};

/// How often a waiting call looks for the answer. The answer is a write to
/// `/proc/hunter` from a person clicking a button; a quarter of a second is
/// well under what they notice and costs nothing while nobody has answered.
const POLL: Duration = Duration::from_millis(250);

/// What the firewall needs to know about an address: how to print it, and
/// whether it stays on this machine. `None` for what is not IP at all (unix,
/// netlink, packet sockets): the firewall has nothing to say about those.
///
/// Local, and so never asked about:
/// * loopback, either way;
/// * the unspecified address as a *destination*, which Linux routes to this
///   host;
/// * a bind to port 0, which is a client picking its own source port, not a
///   server opening one.
pub(super) fn classify(dir: Direction, endpoint: &Endpoint) -> Option<(String, bool)> {
    let ip = match endpoint {
        Endpoint::Ip(ip) => ip,
        _ => return None,
    };
    let local = ip.addr.is_loopback()
        || match dir {
            Direction::Out => ip.addr.is_unspecified(),
            Direction::In => ip.port == 0,
        };
    Some((format!("{}", ip), local))
}

impl Syscall<'_> {
    /// Asks hunter's firewall whether this process may reach `endpoint` in
    /// `dir`. `EACCES` when a rule denies it, or when `Enforce` waited for an
    /// answer and got a `deny` or none; `EINTR` when a signal arrives while it
    /// waits, so `Ctrl-C` on a program held by the question still works.
    pub(super) async fn hunter_net_gate(
        &self,
        dir: Direction,
        endpoint: &Endpoint,
    ) -> LxResult<()> {
        let (peer, local) = match classify(dir, endpoint) {
            Some(c) => c,
            None => return Ok(()),
        };
        let pid = self.zircon_process().id();
        // Cheap enough for every datagram: under `Off` or for local traffic
        // `check` answers before it looks at the program, and the path is
        // only read when there is something to decide.
        if local || firewall::net_mode() == hunter::Mode::Off {
            return Ok(());
        }
        let program = self.linux_process().execute_path();
        match firewall::check(pid, &program, dir, &peer, local) {
            Verdict::Allow => return Ok(()),
            Verdict::Deny => return Err(LxError::EACCES),
            Verdict::Ask => {}
        }
        let deadline = kernel_hal::timer::deadline_after(Duration::from_secs(
            firewall::ask_timeout_secs() as u64,
        ));
        loop {
            match firewall::decided(&program, dir) {
                Some(true) => return Ok(()),
                Some(false) => return Err(LxError::EACCES),
                None => {}
            }
            let now = kernel_hal::timer::timer_now();
            if now >= deadline {
                firewall::expire(pid, &program, dir, &peer);
                return Err(LxError::EACCES);
            }
            let wake = core::cmp::min(deadline, now + POLL);
            linux_object::process::interruptible_sleep_until(self.thread, wake).await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(a: [u8; 4], port: u16) -> Endpoint {
        Endpoint::Ip((IpAddress::v4(a[0], a[1], a[2], a[3]), port).into())
    }

    #[test]
    fn remote_addresses_are_asked_about() {
        assert_eq!(
            classify(Direction::Out, &ip([93, 184, 216, 34], 443)),
            Some((String::from("93.184.216.34:443"), false))
        );
        assert_eq!(
            classify(Direction::In, &ip([0, 0, 0, 0], 8080)),
            Some((String::from("0.0.0.0:8080"), false)),
            "a server on every interface is the one worth asking about"
        );
    }

    #[test]
    fn local_traffic_is_not() {
        assert!(
            classify(Direction::Out, &ip([127, 0, 0, 1], 6000))
                .unwrap()
                .1
        );
        assert!(
            classify(Direction::In, &ip([127, 0, 0, 1], 8080))
                .unwrap()
                .1
        );
        assert!(classify(Direction::Out, &ip([0, 0, 0, 0], 80)).unwrap().1);
        assert!(
            classify(Direction::In, &ip([0, 0, 0, 0], 0)).unwrap().1,
            "a client choosing its source port is not a server"
        );
        assert_eq!(
            classify(
                Direction::Out,
                &Endpoint::Unix(String::from("/tmp/.X11-unix/X0"))
            ),
            None
        );
    }
}
