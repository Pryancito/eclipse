//! Whose the machine's network is, as its boot says.
//!
//! The first virtio network card is left to userspace, by the kernel
//! itself or because the boot says so (`udriver=virtio-net@0`:
//! `zcore_drivers::user`), and goes to the stack that runs as a process,
//! which is the card's driver too. That stack is the machine's network:
//! every program's sockets are its own, asked for or not. That is what a
//! machine with such a card gets by saying nothing, and these are the
//! words that change it:
//!
//! * `unetstack=kernel`: the card goes to the kernel's own stack, with a
//!   driver process to carry its frames, and no stack runs as a process.
//! * `unetstack=none`: the card is not left to userspace unless the boot
//!   names it, and no stack takes it either way.
//! * `unetuse=asked`: the stack process has the card, and only the
//!   programs started by `eclipse-netstack-use` have its sockets.
//! * `unetuse=all`: every program's sockets are a stack process's whatever
//!   else the boot says, even while none runs.
//!
//! On x86_64 a machine with no such card whose first card is an Intel
//! 82574 (QEMU's `e1000e`) leaves that one to userspace the same way
//! ([`E1000E_CARD`]), and the stack process is its driver;
//! `unetcard=kernel` keeps its driver in the kernel, which then lends it
//! as below, and `unetstack=kernel` keeps card and network for the
//! kernel's own stack.
//!
//! A machine with neither, whose first card the kernel drives itself
//! (an I219, or any card on a boot that says `unetcard=kernel`), lends that card to the stack process
//! ([`lends`]): the kernel goes on driving it, its frames go to the stack
//! process and not to the kernel's stack, and it counts as the stack
//! process's card in all of the above. That is the machine's network too
//! when the kernel has no other card. `unetstack=kernel` keeps the card,
//! and the network, for the kernel's own stack, as `unetstack=none` does;
//! `unetcard=kernel` lends the card even when a virtio card is there, which
//! is then not left to userspace unasked.
//!
//! Every driver of the kernel's can lend its card (`NetScheme::can_lend`),
//! and an ordinary boot lends whichever is first ([`lends_any`]). A small
//! echo costs about 1.6 times as much lent as with the kernel's own stack
//! (against a host that takes its time to answer; connecting, sending and
//! receiving are faster lent), which is the price that was accepted for
//! one stack instead of two. `unetlend=asked` goes back to lending only
//! the cards whose drivers lend unasked (`NetScheme::lends_unasked`: the
//! Intel ones with MSI), and the others keep the kernel's own stack. On
//! x86_64 a card with no MSI needs the legacy interrupt line of its slot
//! to be lent, which a boot has under a hypervisor or with `pci.intx=1`
//! (`kernel_hal::drivers::pci_intx_wanted`); without the line such a card
//! is lent only to a boot that says `unetlend=any`.
//!
//! The kernel's other cards are lent too ([`lends_cards`]), to the same
//! stack process, whichever its first card is: it has a stack for each
//! (`tools/eclipse-netstack`, `cards`), and is the machine's network by
//! whichever has a cable. That is when every one of them can be lent. A
//! card that cannot (its driver lends only when asked and the boot says
//! `unetlend=asked`, or it has no interrupt) is the kernel's own stack's,
//! which is the only one that reaches it: on a machine with any, the
//! stack process has its first card and no other, the programs that do
//! not ask keep the kernel's stack, and the stack process is for the ones
//! that do, unless the boot says `unetuse=all`. A boot that says
//! `unetuse=asked` is the same: the first only, and the kernel's stack
//! keeps the rest.
//!
//! A machine with no card at all, the kernel's or anybody's, has no stack
//! process and needs none: what its programs say to each other never
//! reaches a stack ([`Whose::AllNoCard`]). Their sockets are the served
//! ones all the same, which join two programs of the machine by
//! themselves, and the kernel's own stack carries nothing there;
//! `unetstack=kernel` gives them the kernel's stack and its loopback back.

/// The card the stack process takes, as `udriver=` names it and as
/// `zcore_drivers::user::reserved` lists it: the one the `unet` boot
/// service is started for.
pub const CARD: &str = "virtio-net@0";

/// The first Intel 82574 card, when the boot leaves it to userspace
/// (unasked on x86_64, or `udriver=e1000e@0`): the stack process claims it and is its driver,
/// as with [`CARD`], and it counts as that card in all of the above. The
/// `enet` boot service is started for it. Only the stack process drives
/// it from userspace: with `unetstack=kernel` nothing takes it.
pub const E1000E_CARD: &str = "e1000e@0";

/// Whether what the buses left to userspace (`zcore_drivers::user::reserved`)
/// has a card for the stack process to take and drive: [`CARD`] or
/// [`E1000E_CARD`].
pub fn has_card<S: AsRef<str>>(left_to_userspace: &[S]) -> bool {
    left_to_userspace
        .iter()
        .any(|left| [CARD, E1000E_CARD].contains(&left.as_ref()))
}

/// The kernel's first card when it is lent to the stack process, as
/// `/proc/udrivers` lists it: the one the `knet` boot service is started
/// for.
pub const LENT_CARD: &str = "kernel-net@0";

static LENT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// How many of its cards the kernel lends on this boot, and whether the
/// ones whose drivers lend only when asked are among them.
static CARDS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static ANY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether this boot lends the kernel's first card to a stack that runs as
/// a process: `unetstack` is the value of that word, `card` is whether
/// [`CARD`] is there and left to userspace, and `lendable` whether the
/// kernel drives a first card and its driver can lend it. It does unless
/// one of those says no: a machine with [`CARD`] has that one for the stack
/// process, and a boot that wants no stack process (`unetstack=kernel`,
/// `unetstack=none`) lends nothing.
pub fn lends(unetstack: Option<&str>, card: bool, lendable: bool) -> bool {
    lendable && !card && !matches!(unetstack, Some("kernel") | Some("none"))
}

/// How many of the kernel's cards this boot lends to the stack that runs
/// as a process: `lendable` is how many it drives whose drivers can lend
/// them on this boot ([`lends_any`]), `card` whether [`CARD`] is there and
/// left to userspace, and the words are the command line's. All of them,
/// whether the stack process starts with one of them or with a card of
/// its own, unless the boot wants no stack process (`unetstack=kernel`,
/// `unetstack=none`) or keeps the kernel's stack for the programs that do
/// not ask (`unetuse=asked`): then the stack process has one card, its
/// own or the kernel's first.
pub fn lends_cards(
    unetstack: Option<&str>,
    unetuse: Option<&str>,
    card: bool,
    lendable: usize,
) -> usize {
    match (unetstack, unetuse) {
        (Some("kernel") | Some("none"), _) => 0,
        (_, Some("asked")) => lendable.min(!card as usize),
        _ => lendable,
    }
}

/// Says what [`lends_cards`] answered on this boot, once it is known, and
/// what [`lends_any`] did.
pub fn set_lends(cards: usize, any: bool) {
    CARDS.store(cards, core::sync::atomic::Ordering::Release);
    ANY.store(any, core::sync::atomic::Ordering::Release);
}

/// Whether this boot lends any card of the kernel's.
pub fn lends_some() -> bool {
    CARDS.load(core::sync::atomic::Ordering::Acquire) != 0
}

/// Whether a card of the kernel's may be borrowed now: `loans` is how
/// many are lent at the moment, and `unasked` whether the card's driver
/// lends it without the boot's word (`NetScheme::lends_unasked`).
pub fn may_borrow(loans: usize, unasked: bool) -> bool {
    loans < CARDS.load(core::sync::atomic::Ordering::Acquire)
        && (unasked || ANY.load(core::sync::atomic::Ordering::Acquire))
}

/// Whether this boot lends a card whose driver lends only when asked
/// (`NetScheme::lends_unasked`). `unetlend` is that word's value: `any`
/// says yes and `asked` says no. A boot that says neither lends it where
/// the card can be told of its frames: `interrupts` is whether a card with
/// no MSI has an interrupt on this boot (always, off x86_64; on x86_64,
/// whether it is given the legacy line of its slot).
pub fn lends_any(unetlend: Option<&str>, interrupts: bool) -> bool {
    match unetlend {
        Some("any") => true,
        Some("asked") => false,
        _ => interrupts,
    }
}

/// Says what [`lends`] answered on this boot, once it is known.
pub fn set_lent(lent: bool) {
    LENT.store(lent, core::sync::atomic::Ordering::Release);
}

/// The card the kernel lends on this boot, by its name ([`LENT_CARD`]), if
/// it lends one.
pub fn lent() -> Option<&'static str> {
    LENT.load(core::sync::atomic::Ordering::Acquire)
        .then_some(LENT_CARD)
}

/// Whose the sockets are of a program that did not ask for the stack that
/// runs as a process, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Whose {
    /// The stack process's: the boot says so (`unetuse=all`).
    AllSaid,
    /// The stack process's: it has the only card of the machine.
    AllOnlyCard,
    /// The served ones, with no stack process behind them: the machine
    /// has no card, and what its programs say to each other needs none.
    AllNoCard,
    /// The kernel's: the boot says so (`unetuse=asked`).
    AskedSaid,
    /// The kernel's: this boot gives no card to a stack process.
    NoStack,
    /// The kernel's: it drives this many cards of its own.
    KernelCards(usize),
}

impl Whose {
    /// Whether every program's sockets are the served ones, the stack
    /// process's where there is one.
    pub fn all(self) -> bool {
        matches!(self, Whose::AllSaid | Whose::AllOnlyCard | Whose::AllNoCard)
    }

    /// Whether no stack process is expected behind those sockets: what
    /// would leave the machine has no way out, and never will have.
    pub fn alone(self) -> bool {
        self == Whose::AllNoCard
    }
}

/// Whether a stack that runs as a process has a card on this boot: `card`
/// is whether [`CARD`] is there and left to userspace, and `unetstack` the
/// value of that word on the command line.
pub fn has_stack(card: bool, unetstack: Option<&str>) -> bool {
    card && !matches!(unetstack, Some("kernel") | Some("none"))
}

/// What `unetuse=` can say. Any other word is as good as none.
pub fn understood(unetuse: &str) -> bool {
    matches!(unetuse, "all" | "asked")
}

/// Whose the sockets are on a boot where [`CARD`] is left to userspace or
/// not (`card`), where the kernel lends `lent` of its cards
/// ([`lends_cards`]), whose command line has these values of `unetstack=`
/// and `unetuse=` (`None` for a word that is not there), on a machine where
/// the kernel drives `kernel_cards` network cards itself, the loopback not
/// counted and the lent ones counted. A card left to userspace for the
/// kernel's own stack (`unetstack=kernel`) is brought to it by a driver
/// process and is not one of those.
pub fn whose(
    card: bool,
    lent: usize,
    unetstack: Option<&str>,
    unetuse: Option<&str>,
    kernel_cards: usize,
) -> Whose {
    let stack = lent != 0 || has_stack(card, unetstack);
    match unetuse {
        Some("all") => Whose::AllSaid,
        Some("asked") => Whose::AskedSaid,
        // No card anywhere, and no word that keeps the kernel's stack: a
        // card left to userspace that no stack takes (`unetstack=none`)
        // is nobody's network either.
        _ if !stack && kernel_cards == 0 => match unetstack {
            Some("kernel") => Whose::NoStack,
            _ => Whose::AllNoCard,
        },
        _ if !stack => Whose::NoStack,
        _ => match kernel_cards.saturating_sub(lent) {
            0 => Whose::AllOnlyCard,
            others => Whose::KernelCards(others),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_card_the_kernel_can_lend_is_lent_unless_the_boot_keeps_its_stack() {
        // With nothing said: all of them, to a stack process that starts
        // with one of them or with a card of its own.
        assert_eq!(lends_cards(None, None, false, 2), 2);
        assert_eq!(lends_cards(None, None, true, 2), 2);
        assert_eq!(lends_cards(Some("dhcp"), Some("all"), true, 1), 1);
        assert_eq!(lends_cards(None, None, false, 0), 0);
        // No stack process: nobody to lend to.
        assert_eq!(lends_cards(Some("kernel"), None, false, 2), 0);
        assert_eq!(lends_cards(Some("none"), None, true, 2), 0);
        // The kernel's stack for who does not ask: the stack process has
        // one card, and the kernel's stack the rest.
        assert_eq!(lends_cards(None, Some("asked"), false, 2), 1);
        assert_eq!(lends_cards(None, Some("asked"), true, 2), 0);
        assert_eq!(lends_cards(None, Some("asked"), false, 0), 0);

        // What a borrower is answered by, once the boot has decided.
        assert!(!lends_some());
        assert!(!may_borrow(0, true));
        set_lends(2, false);
        assert!(lends_some());
        assert!(may_borrow(0, true) && may_borrow(1, true));
        assert!(!may_borrow(2, true), "as many as the boot lends");
        assert!(!may_borrow(0, false), "a driver that lends only when asked");
        set_lends(1, true);
        assert!(may_borrow(0, false));
        assert!(!may_borrow(1, false));
        set_lends(0, false);
    }

    #[test]
    fn an_ordinary_boot_lends_any_card_that_has_an_interrupt() {
        assert!(lends_any(None, true));
        assert!(!lends_any(None, false), "nothing to tell the borrower with");
        assert!(
            lends_any(Some("yes"), true),
            "a word it does not know is none"
        );
    }

    #[test]
    fn the_boot_can_say_which_cards_are_lent() {
        for interrupts in [false, true] {
            assert!(lends_any(Some("any"), interrupts));
            assert!(!lends_any(Some("asked"), interrupts));
        }
    }

    #[test]
    fn a_virtio_card_or_the_first_e1000e_left_to_userspace_is_the_stacks() {
        assert!(has_card(&["virtio-net@0"]));
        assert!(has_card(&["virtio-blk@1", "e1000e@0"]));
        assert!(!has_card(&["virtio-blk@1"]));
        assert!(!has_card(&["virtio-net@1", "e1000e@1"]));
        assert!(!has_card::<&str>(&[]));
    }

    #[test]
    fn the_stack_process_is_the_network_of_a_machine_whose_only_card_it_has() {
        // Nothing but the card left to userspace: the stack process takes
        // it, and every program has that stack.
        assert_eq!(whose(true, 0, None, None, 0), Whose::AllOnlyCard);
        // With an address of its own or a server's, the same.
        assert_eq!(whose(true, 0, Some("dhcp"), None, 0), Whose::AllOnlyCard);
        assert_eq!(
            whose(true, 0, Some("10.0.9.15/24,10.0.9.2"), None, 0),
            Whose::AllOnlyCard
        );
        assert!(Whose::AllOnlyCard.all());
    }

    #[test]
    fn a_boot_that_leaves_no_card_to_a_stack_process_keeps_the_kernels() {
        // A machine whose card the kernel drives and does not lend.
        assert_eq!(whose(false, 0, None, None, 1), Whose::NoStack);
        assert_eq!(whose(false, 0, Some("dhcp"), None, 2), Whose::NoStack);
        // The card left to userspace for the kernel's own stack.
        assert_eq!(whose(true, 0, Some("kernel"), None, 0), Whose::NoStack);
        assert_eq!(whose(true, 0, Some("none"), None, 1), Whose::NoStack);
        assert!(!Whose::NoStack.all());
        assert!(!Whose::NoStack.alone());
    }

    #[test]
    fn a_machine_with_no_card_has_the_served_sockets_and_no_stack() {
        // No card of the kernel's, none left to userspace.
        assert_eq!(whose(false, 0, None, None, 0), Whose::AllNoCard);
        // An address there does not make a stack of it.
        assert_eq!(whose(false, 0, Some("dhcp"), None, 0), Whose::AllNoCard);
        // A card left to userspace that no stack takes is nobody's
        // network.
        assert_eq!(whose(true, 0, Some("none"), None, 0), Whose::AllNoCard);
        assert_eq!(whose(false, 0, Some("none"), None, 0), Whose::AllNoCard);
        assert!(Whose::AllNoCard.all());
        assert!(Whose::AllNoCard.alone());
        // No other answer is a machine alone.
        assert!(!Whose::AllSaid.alone());
        assert!(!Whose::AllOnlyCard.alone());
        // The kernel's stack and its loopback, for the boot that says so.
        assert_eq!(whose(false, 0, Some("kernel"), None, 0), Whose::NoStack);
        // And what the boot says of every program still counts first.
        assert_eq!(whose(false, 0, None, Some("asked"), 0), Whose::AskedSaid);
        assert_eq!(whose(false, 0, None, Some("all"), 0), Whose::AllSaid);
    }

    #[test]
    fn a_card_of_the_kernels_keeps_its_stack_for_who_does_not_ask() {
        assert_eq!(whose(true, 0, None, None, 1), Whose::KernelCards(1));
        assert_eq!(whose(true, 0, Some("dhcp"), None, 2), Whose::KernelCards(2));
        assert!(!Whose::KernelCards(1).all());
    }

    #[test]
    fn what_the_boot_says_of_every_program_is_what_counts() {
        // `unetuse=all`: with a card of the kernel's, and even with no
        // stack process of the boot's (one can be run by hand).
        assert_eq!(whose(true, 0, None, Some("all"), 1), Whose::AllSaid);
        assert_eq!(whose(false, 0, None, Some("all"), 1), Whose::AllSaid);
        assert_eq!(
            whose(true, 0, Some("kernel"), Some("all"), 0),
            Whose::AllSaid
        );
        assert!(Whose::AllSaid.all());
        // `unetuse=asked`: the stack has the only card and still only the
        // programs that ask have it.
        assert_eq!(whose(true, 0, None, Some("asked"), 0), Whose::AskedSaid);
        assert_eq!(
            whose(true, 0, Some("dhcp"), Some("asked"), 0),
            Whose::AskedSaid
        );
        assert!(!Whose::AskedSaid.all());
        // A word that is neither is as good as none.
        assert!(understood("all") && understood("asked"));
        assert!(!understood("") && !understood("ALL") && !understood("every"));
        assert_eq!(whose(true, 0, None, Some("every"), 0), Whose::AllOnlyCard);
        assert_eq!(
            whose(true, 0, None, Some("every"), 1),
            Whose::KernelCards(1)
        );
        assert_eq!(whose(false, 0, None, Some(""), 1), Whose::NoStack);
    }

    #[test]
    fn a_card_the_kernel_lends_is_the_stack_processs_and_not_the_kernels() {
        // The kernel's first card can be lent and no virtio card is left
        // to userspace: lent, with nothing said or with an address.
        assert!(lends(None, false, true));
        assert!(lends(Some("dhcp"), false, true));
        assert!(lends(Some("10.0.2.15/24,10.0.2.2"), false, true));
        // No card of the kernel's, or one whose driver cannot lend it.
        assert!(!lends(None, false, false));
        // The virtio card is the stack process's first.
        assert!(!lends(None, true, true));
        // A boot with no stack process has nobody to lend to.
        assert!(!lends(Some("kernel"), false, true));
        assert!(!lends(Some("none"), false, true));

        // The only card of the machine, lent: the stack process is the
        // network of every program, as with a card of its own.
        assert_eq!(whose(false, 1, None, None, 1), Whose::AllOnlyCard);
        // Another card the kernel keeps: its stack for who does not ask.
        assert_eq!(whose(false, 1, None, None, 2), Whose::KernelCards(1));
        // What the boot says of every program still counts.
        assert_eq!(whose(false, 1, None, Some("asked"), 1), Whose::AskedSaid);
        assert_eq!(whose(false, 1, None, Some("all"), 2), Whose::AllSaid);

        // Every card of the kernel's lent: the stack process has them
        // all, with a card of its own or without.
        assert_eq!(whose(false, 2, None, None, 2), Whose::AllOnlyCard);
        assert_eq!(whose(true, 1, None, None, 1), Whose::AllOnlyCard);
        // One the kernel cannot lend keeps its stack for who does not ask.
        assert_eq!(whose(true, 1, None, None, 2), Whose::KernelCards(1));

        // What `/proc/udrivers` lists it as, once the boot has decided.
        assert_eq!(lent(), None);
        set_lent(true);
        assert_eq!(lent(), Some(LENT_CARD));
        set_lent(false);
    }
}
