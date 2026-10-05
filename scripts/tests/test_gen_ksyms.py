import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "gen_ksyms", Path(__file__).parents[2] / "tools" / "gen_ksyms.py"
)
ksyms = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ksyms)

UART_LOCK = "lock::ticket::TicketMutex<Uart16550Inner<Pmio<u8>>>::lock"
ITIMER_LOCK = "lock::ticket::TicketMutex<[linux_object::time::ItimerSlot; 3]>::lock"


class FoldedSymbolNames(unittest.TestCase):
    """What a crash report may claim about an address several symbols share.

    63 addresses in the x86_64 image share one, and the worst holds 35 names.
    Printing whichever `nm` listed first, with no hedge, hands a reader a
    concrete name for something the address does not determine -- and a
    concrete name on a stop screen is what gets believed.
    """

    def test_an_address_with_one_symbol_keeps_its_exact_name(self):
        self.assertEqual(ksyms.fold_name([ITIMER_LOCK]), ITIMER_LOCK)

    def test_a_repeated_name_is_still_one_name(self):
        self.assertEqual(ksyms.fold_name([UART_LOCK, UART_LOCK]), UART_LOCK)

    def test_sibling_monomorphizations_lose_the_type_they_disagree_on(self):
        folded = ksyms.fold_name([UART_LOCK, ITIMER_LOCK])
        self.assertEqual(folded, "lock::ticket::TicketMutex<?>::lock +1")
        # The two things a reader needs: the method, and that the type is not
        # knowable from the address.
        self.assertIn("::lock", folded)
        self.assertNotIn("ItimerSlot", folded)
        self.assertNotIn("Uart16550", folded)

    def test_the_count_says_how_many_other_names_were_there(self):
        names = [ITIMER_LOCK, UART_LOCK, "lock::ticket::TicketMutex<()>::lock"]
        self.assertTrue(ksyms.fold_name(names).endswith("+2"))

    def test_plain_aliases_keep_a_concrete_name_and_gain_the_count(self):
        # `memcpy` / `__memcpy` are the same function under two names, not two
        # instantiations of one generic: there is no middle to blank out, and
        # `?memcpy` would be less use than either name.
        self.assertEqual(ksyms.fold_name(["memcpy", "__memcpy"]), "memcpy +1")

    def test_names_that_share_nothing_fall_back_to_the_first(self):
        self.assertEqual(ksyms.fold_name(["alpha", "beta"]), "alpha +1")

    def test_a_shared_end_must_not_reach_back_into_the_shared_start(self):
        # "ab" and "aXb" share "a" at the front and "b" at the back, but the
        # shorter name has only two characters: a fold claiming both would
        # report more shared text than exists.
        folded = ksyms.fold_name(["ab", "aXb"])
        self.assertEqual(folded, "ab +1")

    def test_drop_glue_folds_the_same_way_as_a_lock(self):
        folded = ksyms.fold_name(
            ["core::ptr::drop_in_place<A>", "core::ptr::drop_in_place<B>"]
        )
        self.assertEqual(folded, "core::ptr::drop_in_place<?> +1")


class CommonPrefix(unittest.TestCase):
    def test_the_prefix_of_one_name_is_the_whole_name(self):
        self.assertEqual(ksyms.common_prefix(["abc"]), "abc")

    def test_a_name_that_is_a_prefix_of_another_bounds_the_answer(self):
        self.assertEqual(ksyms.common_prefix(["abc", "abcdef"]), "abc")
        self.assertEqual(ksyms.common_prefix(["abcdef", "abc"]), "abc")

    def test_no_shared_first_character_is_an_empty_prefix(self):
        self.assertEqual(ksyms.common_prefix(["abc", "xyz"]), "")

    def test_an_empty_name_leaves_nothing_shared(self):
        self.assertEqual(ksyms.common_prefix(["", "abc"]), "")


if __name__ == "__main__":
    unittest.main()
